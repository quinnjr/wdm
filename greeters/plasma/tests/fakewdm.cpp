#include "fakewdm.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <sys/socket.h>
#include <unistd.h>

#include <wayland-server-core.h>

#include "wdm-greeter-v1-server-protocol.h"

namespace wdm::test {

namespace {

[[noreturn]] void fatal(const char *what) {
    std::fprintf(stderr, "fakewdm: %s\n", what);
    std::abort();
}

bool expectsAnswer(std::uint32_t style) {
    return style == WDM_GREETER_V1_PROMPT_STYLE_SECRET
           || style == WDM_GREETER_V1_PROMPT_STYLE_VISIBLE;
}

} // namespace

/// The server-side implementation, gathered so that fakewdm.h does not have to
/// name the generated interface struct.
struct FakeWdm::Bindings {
    static void destroy(wl_client *, wl_resource *resource) { wl_resource_destroy(resource); }

    static void createSession(wl_client *, wl_resource *resource, const char *username) {
        auto *self = static_cast<FakeWdm *>(wl_resource_get_user_data(resource));
        const std::lock_guard<std::mutex> lock(self->mutex_);
        self->createSessionCalls_.emplace_back(username);
    }

    static void respond(wl_client *, wl_resource *resource, std::uint32_t id,
                        const char *response) {
        auto *self = static_cast<FakeWdm *>(wl_resource_get_user_data(resource));
        const std::lock_guard<std::mutex> lock(self->mutex_);
        self->respondCalls_.emplace_back(id, std::string(response));
        // wdm raises stale_prompt for an id that is not the pending one; the
        // fake records the request and clears only on a match, so a test can
        // see the difference rather than being killed for it.
        if (self->pendingPrompt_.has_value() && *self->pendingPrompt_ == id) {
            self->pendingPrompt_.reset();
        }
    }

    static void cancel(wl_client *, wl_resource *resource) {
        auto *self = static_cast<FakeWdm *>(wl_resource_get_user_data(resource));
        const std::lock_guard<std::mutex> lock(self->mutex_);
        ++self->cancelCalls_;
        self->pendingPrompt_.reset();
    }

    static void startSession(wl_client *, wl_resource *resource, const char *sessionId,
                             wl_array *env) {
        auto *self = static_cast<FakeWdm *>(wl_resource_get_user_data(resource));
        const std::lock_guard<std::mutex> lock(self->mutex_);
        self->startSessionCall_ = StartSessionCall{std::string(sessionId), env->size};
    }

    static void resourceDestroyed(wl_resource *resource) {
        auto *self = static_cast<FakeWdm *>(wl_resource_get_user_data(resource));
        const std::lock_guard<std::mutex> lock(self->mutex_);
        if (self->resource_ == resource) {
            self->resource_ = nullptr;
        }
    }

    static void clientDestroyed(wl_listener *listener, void *) {
        auto *watch = reinterpret_cast<FakeWdm::ClientWatch *>(listener);
        const std::lock_guard<std::mutex> lock(watch->fake->mutex_);
        watch->fake->client_ = nullptr;
    }

    static void bind(wl_client *client, void *data, std::uint32_t version, std::uint32_t id) {
        auto *self = static_cast<FakeWdm *>(data);
        wl_resource *resource = wl_resource_create(client, &wdm_greeter_v1_interface,
                                                   static_cast<int>(version), id);
        if (resource == nullptr) {
            wl_client_post_no_memory(client);
            return;
        }
        wl_resource_set_implementation(resource, &kImplementation, self, &resourceDestroyed);
        {
            const std::lock_guard<std::mutex> lock(self->mutex_);
            self->resource_ = resource;
            self->boundVersion_ = version;
        }
        self->sendEnumerate(resource);
    }

    // `struct` is not optional here, and this is the one place in the greeter
    // where that is true. wayland-scanner emits both a `struct
    // wdm_greeter_v1_interface` (the request table an implementation fills in)
    // and a `const struct wl_interface wdm_greeter_v1_interface` (the
    // marshalling description), and in C++ the variable hides the type. C never
    // notices because the two live in different namespaces there, which is why
    // every example of this API omits the keyword.
    static const struct wdm_greeter_v1_interface kImplementation;
};

const struct wdm_greeter_v1_interface FakeWdm::Bindings::kImplementation = {
    .destroy = &FakeWdm::Bindings::destroy,
    .create_session = &FakeWdm::Bindings::createSession,
    .respond = &FakeWdm::Bindings::respond,
    .cancel = &FakeWdm::Bindings::cancel,
    .start_session = &FakeWdm::Bindings::startSession,
};

FakeWdm::FakeWdm(std::uint32_t advertisedVersion, bool advertiseGreeterGlobal)
    : advertisedVersion_(advertisedVersion) {
    display_ = wl_display_create();
    if (display_ == nullptr) {
        fatal("could not create a wl_display");
    }
    loop_ = wl_display_get_event_loop(display_);

    // A socketpair rather than a listening socket in XDG_RUNTIME_DIR: nothing
    // here should depend on a filesystem path, and a test that leaves a socket
    // behind when it fails is a test that fails differently the second time.
    int fds[2] = {-1, -1};
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, fds) != 0) {
        fatal("socketpair failed");
    }
    client_ = wl_client_create(display_, fds[0]);
    if (client_ == nullptr) {
        fatal("could not create a wl_client");
    }
    clientFd_ = fds[1];
    clientWatch_.fake = this;
    clientWatch_.listener.notify = &Bindings::clientDestroyed;
    wl_client_add_destroy_listener(client_, &clientWatch_.listener);

    if (advertiseGreeterGlobal) {
        global_ = wl_global_create(display_, &wdm_greeter_v1_interface,
                                   static_cast<int>(advertisedVersion_), this, &Bindings::bind);
        if (global_ == nullptr) {
            fatal("could not create the wdm_greeter_v1 global");
        }
    }
    // With no global the display still serves the registry, so the client's
    // roundtrip completes normally and simply learns nothing. Omitting the
    // global rather than refusing the connection is the difference the greeter
    // has to be able to report.

    thread_ = std::thread([this] { run(); });
}

FakeWdm::~FakeWdm() {
    {
        const std::lock_guard<std::mutex> lock(mutex_);
        running_ = false;
    }
    thread_.join();
    // Destroys the remaining clients and resources, which runs
    // resourceDestroyed on this thread with no lock held.
    wl_display_destroy(display_);
    if (clientFd_ >= 0) {
        close(clientFd_);
    }
}

int FakeWdm::takeClientFd() {
    // Also the point at which the enumerate configuration is published to the
    // server thread. The test writes `users` and friends without the lock, and
    // this is the last thing it does before the client connects; taking the
    // mutex here pairs with the bind handler taking it, which is what makes
    // those writes visible there rather than merely likely to be.
    const std::lock_guard<std::mutex> lock(mutex_);
    const int fd = clientFd_;
    clientFd_ = -1;
    return fd;
}

void FakeWdm::run() {
    for (;;) {
        // A short timeout rather than a wakeup pipe: the commands a test queues
        // are not latency sensitive, and an extra fd in the loop is an extra
        // thing to get wrong in a harness whose whole job is to be obviously
        // right.
        // The return is checked rather than discarded. A loop that fails
        // persistently would otherwise spin here at 100% CPU sending nothing,
        // and the test that was waiting would report a bare ctest timeout with
        // no indication that the server had stopped working.
        if (wl_event_loop_dispatch(loop_, 5) < 0) {
            fatal("the event loop failed; the fake server can no longer answer anything");
        }
        wl_display_flush_clients(display_);

        std::unique_lock<std::mutex> lock(mutex_);
        while (!commands_.empty()) {
            const Command command = commands_.front();
            commands_.pop_front();
            // Run it with the lock released: the command is server-thread work
            // that may itself want the mutex.
            lock.unlock();
            (*command.fn)();
            lock.lock();
            *command.done = true;
        }
        commandsDone_.notify_all();
        const bool stop = !running_;
        lock.unlock();

        // Flush again: a command that sent an event did so after the flush
        // above, and a test that then waits for it would wait for the next
        // trip round this loop.
        wl_display_flush_clients(display_);
        if (stop) {
            return;
        }
    }
}

void FakeWdm::onServerThread(const std::function<void()> &fn) {
    bool done = false;
    std::unique_lock<std::mutex> lock(mutex_);
    commands_.push_back(Command{&fn, &done});
    // A deadline, not a bare wait. A command pushed after run()'s final drain —
    // which is reachable by queuing work from a destructor, or from a test that
    // outlives its harness — would never be picked up, and an unbounded wait
    // turns that into a ctest timeout with no line number and no name. Failing
    // here names the harness and the call. Ten seconds because every command
    // this fake runs is answered within one trip round a 5 ms event loop; the
    // margin is for a loaded CI runner, not for anything that legitimately
    // takes time.
    if (!commandsDone_.wait_for(lock, std::chrono::seconds(10), [&done] { return done; })) {
        fatal("a command queued to the server thread was never run; the fake is not running");
    }
}

void FakeWdm::sendEnumerate(wl_resource *resource) {
    const std::uint32_t version = wl_resource_get_version(resource);

    std::vector<UserSpec> userSpecs;
    std::vector<SessionSpec> sessionSpecs;
    std::string configuredDefault;
    std::string error;
    {
        const std::lock_guard<std::mutex> lock(mutex_);
        userSpecs = users;
        sessionSpecs = sessions;
        configuredDefault = defaultSession;
        error = lastError;
    }

    for (const UserSpec &user : userSpecs) {
        // The version 1 substitution, exactly as wdm performs it: a version 1
        // peer cannot learn the configured default through any other event, so
        // it arrives in last_session for a user who has no history. A version 2
        // peer gets the user's own history and nothing else, and learns the
        // default from default_session.
        const std::string lastSession =
            (version < 2 && user.lastSession.empty()) ? configuredDefault : user.lastSession;
        wdm_greeter_v1_send_user(resource, user.name.c_str(), user.displayName.c_str(),
                                 user.avatarPath.c_str(), lastSession.c_str());
    }
    for (const SessionSpec &session : sessionSpecs) {
        wdm_greeter_v1_send_session(resource, session.id.c_str(), session.name.c_str(),
                                    session.exec.c_str(), session.type);
    }
    if (!error.empty()) {
        wdm_greeter_v1_send_last_error(resource, error.c_str());
    }
    if (version >= WDM_GREETER_V1_DEFAULT_SESSION_SINCE_VERSION) {
        wdm_greeter_v1_send_default_session(resource, configuredDefault.c_str());
    }
    wdm_greeter_v1_send_done(resource);

    // output_rank is not sent. It carries a wl_output object argument, and this
    // fake advertises no wl_output global — a greeter under wdm gets its
    // outputs from the compositor it is already drawing on. Link ignores the
    // event by design, so what is untested here is a branch that does nothing.
}

std::uint32_t FakeWdm::boundVersion() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return boundVersion_;
}

std::vector<std::string> FakeWdm::createSessionCalls() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return createSessionCalls_;
}

std::vector<std::pair<std::uint32_t, std::string>> FakeWdm::respondCalls() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return respondCalls_;
}

int FakeWdm::cancelCalls() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return cancelCalls_;
}

std::optional<StartSessionCall> FakeWdm::startSessionCall() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return startSessionCall_;
}

std::optional<std::uint32_t> FakeWdm::pendingPrompt() const {
    const std::lock_guard<std::mutex> lock(mutex_);
    return pendingPrompt_;
}

std::uint32_t FakeWdm::sendPrompt(const std::string &text, std::uint32_t style) {
    std::uint32_t id = 0;
    const std::function<void()> send = [this, &id, &text, style] {
        const std::lock_guard<std::mutex> lock(mutex_);
        if (resource_ == nullptr) {
            return;
        }
        id = nextPromptId_++;
        if (expectsAnswer(style)) {
            pendingPrompt_ = id;
        }
        wdm_greeter_v1_send_prompt(resource_, id, text.c_str(), style);
    };
    onServerThread(send);
    return id;
}

void FakeWdm::sendAuthOk() {
    const std::function<void()> send = [this] {
        const std::lock_guard<std::mutex> lock(mutex_);
        if (resource_ != nullptr) {
            pendingPrompt_.reset();
            wdm_greeter_v1_send_auth_ok(resource_);
        }
    };
    onServerThread(send);
}

void FakeWdm::sendAuthFailed(const std::string &reason) {
    const std::function<void()> send = [this, &reason] {
        const std::lock_guard<std::mutex> lock(mutex_);
        if (resource_ != nullptr) {
            pendingPrompt_.reset();
            wdm_greeter_v1_send_auth_failed(resource_, reason.c_str());
        }
    };
    onServerThread(send);
}

void FakeWdm::hangUp() {
    const std::function<void()> kill = [this] {
        wl_client *client = nullptr;
        {
            const std::lock_guard<std::mutex> lock(mutex_);
            client = client_;
            client_ = nullptr;
        }
        if (client != nullptr) {
            // Destroying the client runs resourceDestroyed, which takes the
            // mutex; hence taking it above and releasing it before this.
            wl_client_destroy(client);
        }
    };
    onServerThread(kill);
}

} // namespace wdm::test
