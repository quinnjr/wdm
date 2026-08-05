#include "link.h"

#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstring>

#include <poll.h>
#include <wayland-client.h>

#include "wdm-greeter-v1-client-protocol.h"

namespace wdm {

LinkObserver::~LinkObserver() = default;

namespace {

/// The strerror of a failed libwayland call, or something honest when errno
/// says nothing. A dispatch that fails because the compositor sent a protocol
/// error leaves errno at 0, and "Success" is not a thing to show a user who has
/// just lost their login screen.
std::string reasonFromErrno() {
    const int saved = errno;
    if (saved == 0) {
        return "the compositor closed the connection";
    }
    return std::strerror(saved);
}

PromptStyle styleFromProtocol(std::uint32_t style) {
    switch (style) {
    case WDM_GREETER_V1_PROMPT_STYLE_SECRET:
        return PromptStyle::Secret;
    case WDM_GREETER_V1_PROMPT_STYLE_VISIBLE:
        return PromptStyle::Visible;
    case WDM_GREETER_V1_PROMPT_STYLE_INFO:
        return PromptStyle::Info;
    case WDM_GREETER_V1_PROMPT_STYLE_ERROR:
        return PromptStyle::Error;
    default:
        // A style this greeter's XML does not define. Shown as an
        // informational message and not as a question, deliberately: a greeter
        // that waits for an answer PAM is not asking for hangs the login
        // screen, and one that shows the text and moves on cannot. The same
        // choice the Rust client makes, for the same reason.
        //
        // Said out loud, because otherwise a style added to the protocol later
        // renders as a grey aside with nothing anywhere explaining why — the
        // reason wdm-greeter-client's equivalent arm carries a log::warn rather
        // than folding into its Info arm. stderr and not a logging framework:
        // this half links no Qt, so Qt's message handler is not reachable from
        // here, and wdm's greeters are journald children whose stderr is
        // already where their diagnostics go.
        std::fprintf(stderr, "wdm-plasma-greeter: unknown prompt style %u, shown as a message\n",
                     style);
        return PromptStyle::Info;
    }
}

SessionType typeFromProtocol(std::uint32_t type) {
    // Anything that is not x11 is treated as a Wayland session, because that is
    // what wdm does with it: an x11 session is the one that needs an X server
    // brought up, and guessing wrong in that direction is the one that fails
    // safely.
    return type == WDM_GREETER_V1_SESSION_TYPE_X11 ? SessionType::X11 : SessionType::Wayland;
}

} // namespace

/// The C callbacks, gathered so that link.h never has to name a generated
/// listener struct.
struct Link::Callbacks {
    static void global(void *data, wl_registry *registry, std::uint32_t name,
                       const char *interface, std::uint32_t version) {
        static_cast<Link *>(data)->handleGlobal(registry, name, interface, version);
    }

    static void globalRemove(void *, wl_registry *, std::uint32_t) {
        // wdm_greeter_v1 is a singleton that lives as long as the compositor
        // does; if it went away the connection is going with it, and dispatch()
        // is where that gets noticed.
    }

    static void user(void *data, wdm_greeter_v1 *, const char *name, const char *displayName,
                     const char *avatarPath, const char *lastSession) {
        auto *self = static_cast<Link *>(data);
        self->observer_->onUser(User{name, displayName, avatarPath, lastSession});
    }

    static void session(void *data, wdm_greeter_v1 *, const char *id, const char *name,
                        const char *exec, std::uint32_t type) {
        auto *self = static_cast<Link *>(data);
        self->observer_->onSession(Session{id, name, exec, typeFromProtocol(type)});
    }

    static void outputRank(void *, wdm_greeter_v1 *, wl_output *, std::uint32_t) {
        // ponytail: received and ignored. wdm places a layer surface with no
        // output on the rank 0 output itself and moves it when the ranks
        // change, so honouring the rank here would reimplement that. Ceiling:
        // no per-output layout — the same theme is shown on every output.
        // Upgrade: expose the rank on a screens model and let the theme place
        // itself, which needs a way back from a wl_output to a QScreen.
    }

    static void lastError(void *data, wdm_greeter_v1 *, const char *text) {
        static_cast<Link *>(data)->observer_->onLastError(text);
    }

    static void done(void *data, wdm_greeter_v1 *) {
        static_cast<Link *>(data)->observer_->onEnumerateDone();
    }

    static void prompt(void *data, wdm_greeter_v1 *, std::uint32_t id, const char *text,
                       std::uint32_t style) {
        static_cast<Link *>(data)->handlePrompt(id, text, style);
    }

    static void authOk(void *data, wdm_greeter_v1 *) {
        auto *self = static_cast<Link *>(data);
        if (self->discardConversationEvent("auth_ok")) {
            return;
        }
        self->pending_.reset();
        self->observer_->onAuthOk();
    }

    static void authFailed(void *data, wdm_greeter_v1 *, const char *reason) {
        auto *self = static_cast<Link *>(data);
        if (self->discardConversationEvent("auth_failed")) {
            return;
        }
        self->pending_.reset();
        self->observer_->onAuthFailed(reason);
    }

    /// A bounded roundtrip's sync coming back. `data` is the caller's flag on
    /// its own stack — the callback is destroyed before that stack unwinds on
    /// every path out of roundtrip(), including the deadline one.
    static void roundtripDone(void *data, wl_callback *, std::uint32_t) {
        *static_cast<bool *>(data) = true;
    }

    /// The cancel barrier coming down. See Link::cancel().
    static void barrierDone(void *data, wl_callback *callback, std::uint32_t) {
        auto *self = static_cast<Link *>(data);
        // Identity checked rather than assumed: cancel() destroys the previous
        // barrier when it installs a new one, so this should always be the
        // current one — and if it ever is not, clearing the flag for a barrier
        // that has been superseded would reopen the window the newer one is
        // holding shut.
        if (self->barrier_ != callback) {
            wl_callback_destroy(callback);
            return;
        }
        self->dropBarrier();
        self->discarding_ = false;
    }

    static void defaultSession(void *data, wdm_greeter_v1 *, const char *id) {
        static_cast<Link *>(data)->observer_->onDefaultSession(id);
    }

    static const wl_registry_listener kRegistry;
    static const wdm_greeter_v1_listener kGreeter;
    static const wl_callback_listener kBarrier;
    static const wl_callback_listener kRoundtrip;
};

const wl_callback_listener Link::Callbacks::kBarrier = {
    .done = &Link::Callbacks::barrierDone,
};

const wl_callback_listener Link::Callbacks::kRoundtrip = {
    .done = &Link::Callbacks::roundtripDone,
};

const wl_registry_listener Link::Callbacks::kRegistry = {
    .global = &Link::Callbacks::global,
    .global_remove = &Link::Callbacks::globalRemove,
};

const wdm_greeter_v1_listener Link::Callbacks::kGreeter = {
    .user = &Link::Callbacks::user,
    .session = &Link::Callbacks::session,
    .output_rank = &Link::Callbacks::outputRank,
    .last_error = &Link::Callbacks::lastError,
    .done = &Link::Callbacks::done,
    .prompt = &Link::Callbacks::prompt,
    .auth_ok = &Link::Callbacks::authOk,
    .auth_failed = &Link::Callbacks::authFailed,
    .default_session = &Link::Callbacks::defaultSession,
};

Link::Link(wl_display *display, LinkObserver *observer)
    : display_(display), observer_(observer) {}

Link::~Link() {
    // Destroying the greeter object cancels any conversation in progress, which
    // is the whole of the cleanup wdm needs from us. The display is Qt's and is
    // deliberately left alone.
    // Before the queue below, which it lives in.
    dropBarrier();
    if (greeter_ != nullptr) {
        wdm_greeter_v1_destroy(greeter_);
    }
    if (registry_ != nullptr) {
        wl_registry_destroy(registry_);
    }
    if (queue_ != nullptr) {
        wl_event_queue_destroy(queue_);
    }
}

void Link::handleGlobal(wl_registry *registry, std::uint32_t name, const char *interface,
                        std::uint32_t version) {
    if (std::strcmp(interface, wdm_greeter_v1_interface.name) != 0) {
        return;
    }
    if (greeter_ != nullptr) {
        // wdm advertises exactly one, and binding a second raises
        // already_bound.
        return;
    }
    version_ = version < kInterfaceVersion ? version : kInterfaceVersion;
    greeter_ = static_cast<wdm_greeter_v1 *>(
        wl_registry_bind(registry, name, &wdm_greeter_v1_interface, version_));
    wdm_greeter_v1_add_listener(greeter_, &Callbacks::kGreeter, this);
}

bool Link::discardConversationEvent(const char *what) {
    if (!discarding_) {
        return false;
    }
    // Said out loud, because this is where an event wdm really sent stops
    // without the layer above ever hearing of it: a login attempt that ends
    // with the screen simply going quiet has to leave a line behind saying
    // which event was dropped and why.
    //
    // stderr and not a logging framework, and unconditional rather than
    // thresholded — the same arrangement styleFromProtocol's default arm above
    // is under, and for the same reason. This half links no Qt, so
    // logging.h's handler and WDM_GREETER_LOG are not reachable from here, and
    // wdm's greeters are journald children whose stderr is already where their
    // diagnostics go.
    std::fprintf(stderr, "wdm-plasma-greeter: %s for a cancelled conversation; discarded\n", what);
    return true;
}

void Link::handlePrompt(std::uint32_t id, const char *text, std::uint32_t style) {
    // Before the Prompt is built, and in particular before `pending_` could be
    // written: re-arming pending_ for a conversation wdm has been told to
    // abandon is the whole of the defect cancel()'s barrier exists for. The
    // layer above drops the event too, but it cannot undo this — by the time
    // its guard runs, Link is already holding an id it will accept a respond
    // for.
    if (discardConversationEvent("prompt")) {
        return;
    }
    Prompt prompt{id, text, styleFromProtocol(style)};
    if (prompt.expectsAnswer()) {
        pending_ = prompt;
    }
    observer_->onPrompt(prompt);
}

void Link::dropBarrier() {
    if (barrier_ != nullptr) {
        wl_callback_destroy(barrier_);
        barrier_ = nullptr;
    }
}

void Link::die(const std::string &why) {
    if (dead_) {
        return;
    }
    dead_ = true;
    pending_.reset();
    // The barrier is destroyed but the flag is left standing. Nothing will ever
    // dispatch again, so the flag has no further effect; destroying the proxy
    // is what matters, because the queue it lives in is destroyed with this
    // object and a listener left pointing into it is a use-after-free waiting
    // for a dispatch that a reconnect would provide.
    dropBarrier();
    observer_->onLinkDead(why);
}

bool Link::roundtrip(int timeoutMs, std::string *error) {
    auto *wrapper = static_cast<wl_display *>(wl_proxy_create_wrapper(display_));
    if (wrapper == nullptr) {
        *error = "could not wrap the Wayland display";
        return false;
    }
    wl_proxy_set_queue(reinterpret_cast<wl_proxy *>(wrapper), queue_);
    wl_callback *callback = wl_display_sync(wrapper);
    wl_proxy_wrapper_destroy(wrapper);
    if (callback == nullptr) {
        *error = "could not ask the compositor for a roundtrip";
        return false;
    }

    bool done = false;
    wl_callback_add_listener(callback, &Callbacks::kRoundtrip, &done);

    // A clock and not a countdown of poll() timeouts: poll returns early on any
    // readable byte, including one belonging to another queue entirely, so a
    // loop that passed the full timeout each pass would have no deadline at
    // all on a busy connection.
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeoutMs);
    while (!done) {
        const auto now = std::chrono::steady_clock::now();
        if (now >= deadline) {
            wl_callback_destroy(callback);
            *error = "wdm accepted the connection but never finished enumerating";
            return false;
        }
        const auto remaining =
            std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count();
        if (!readAndDispatch(static_cast<int>(remaining))) {
            // Only a dead link gets here; readAndDispatch has already reported
            // it through onLinkDead, and this is the sentence the caller shows.
            wl_callback_destroy(callback);
            *error = "lost the connection to the compositor: " + reasonFromErrno();
            return false;
        }
    }
    wl_callback_destroy(callback);
    return true;
}

bool Link::connect(std::string *error) {
    if (queue_ != nullptr) {
        // Refused rather than repeated. A second call overwrites queue_ and
        // registry_, which leaks both and leaves the first registry listening
        // into an event queue that has been destroyed — a use-after-free that
        // surfaces as a crash on the next dispatch, nowhere near here.
        *error = "already connected";
        return false;
    }
    queue_ = wl_display_create_queue(display_);
    if (queue_ == nullptr) {
        *error = "could not create a Wayland event queue";
        return false;
    }

    // A proxy wrapper, not a bare wl_display_get_registry: the queue has to be
    // set on the registry before the request is sent, or events for it can be
    // dispatched into Qt's default queue in the window between creating the
    // proxy and reassigning it — where nothing understands them and they are
    // silently dropped. Every object the registry creates inherits its queue,
    // so this one call is what keeps the whole protocol out of Qt's dispatch
    // and Qt's protocols out of ours.
    auto *wrapper = static_cast<wl_display *>(wl_proxy_create_wrapper(display_));
    if (wrapper == nullptr) {
        *error = "could not wrap the Wayland display";
        return false;
    }
    wl_proxy_set_queue(reinterpret_cast<wl_proxy *>(wrapper), queue_);
    registry_ = wl_display_get_registry(wrapper);
    wl_proxy_wrapper_destroy(wrapper);
    if (registry_ == nullptr) {
        *error = "could not get the Wayland registry";
        return false;
    }
    wl_registry_add_listener(registry_, &Callbacks::kRegistry, this);

    // Two roundtrips, the same shape as the Rust client's connect. The first
    // collects the globals and binds; the second collects everything that
    // arrives as a result of binding, which is the entire enumerate phase up to
    // and including `done`.
    //
    // Both are deadlined, and both halves need it for different reasons: a
    // compositor that accepts a connection and never answers the registry is
    // as silent as one that binds and never sends `done`, and the deadline is
    // shared between them because what the user is waiting for is one thing —
    // a login screen — rather than two protocol phases.
    const auto deadline =
        std::chrono::steady_clock::now() + std::chrono::milliseconds(kEnumerateTimeoutMs);
    const auto remaining = [&deadline] {
        const auto left = std::chrono::duration_cast<std::chrono::milliseconds>(
                              deadline - std::chrono::steady_clock::now())
                              .count();
        return left > 0 ? static_cast<int>(left) : 0;
    };

    if (!roundtrip(remaining(), error)) {
        return false;
    }
    if (greeter_ == nullptr) {
        *error = "the compositor does not offer wdm_greeter_v1; this greeter only runs under wdm";
        return false;
    }
    if (!roundtrip(remaining(), error)) {
        return false;
    }
    return true;
}

void Link::createSession(const std::string &username) {
    if (dead_ || greeter_ == nullptr) {
        return;
    }
    wdm_greeter_v1_create_session(greeter_, username.c_str());
    dispatch();
}

bool Link::respond(std::uint32_t id, std::string response) {
    if (dead_ || greeter_ == nullptr) {
        return false;
    }
    if (!pending_.has_value() || pending_->id != id) {
        return false;
    }
    // `response` is a copy — see the header. The obvious call from the layer
    // above is respond(p->id, p->text) off the reference pendingPrompt()
    // returns, and this reset is what destroys that Prompt; by reference, the
    // c_str() below would be reading freed memory that held a password.
    pending_.reset();
    wdm_greeter_v1_respond(greeter_, id, response.c_str());
    dispatch();
    return true;
}

void Link::cancel() {
    if (dead_ || greeter_ == nullptr) {
        return;
    }
    // Dropped before the request is sent rather than when a reply arrives:
    // there is no reply. Cancelling is answered by silence — no auth_ok, no
    // auth_failed — so a greeter that waited for confirmation would hold a
    // prompt that is never going to be answerable again.
    pending_.reset();
    wdm_greeter_v1_cancel(greeter_);

    // The barrier, and it is sent *after* the cancel because that ordering is
    // the entire guarantee: its `done` cannot arrive until wdm has processed
    // the cancel, so nothing before it belongs to a conversation that still
    // exists. See the header.
    //
    // A wrapper for the same reason connect() uses one — the sync's callback
    // must be created on our queue and not on Qt's, or the `done` is dispatched
    // where nothing understands it and the barrier never comes down.
    dropBarrier();
    auto *wrapper = static_cast<wl_display *>(wl_proxy_create_wrapper(display_));
    if (wrapper != nullptr) {
        wl_proxy_set_queue(reinterpret_cast<wl_proxy *>(wrapper), queue_);
        barrier_ = wl_display_sync(wrapper);
        wl_proxy_wrapper_destroy(wrapper);
    }
    if (barrier_ != nullptr) {
        wl_callback_add_listener(barrier_, &Callbacks::kBarrier, this);
        discarding_ = true;
    } else {
        // Only reachable on an allocation failure, which libwayland does not
        // otherwise survive. Named rather than ignored, because without the
        // barrier a prompt crossing this cancel is delivered again and the
        // failure it causes — a respond under a dead conversation's id — shows
        // up as wdm killing the greeter with no local explanation at all.
        std::fprintf(stderr,
                     "wdm-plasma-greeter: could not create the cancel barrier; a prompt already "
                     "in flight may still be delivered\n");
    }
    dispatch();
}

void Link::startSession(const std::string &sessionId) {
    if (dead_ || greeter_ == nullptr) {
        return;
    }
    // An empty env array, not a null one: the argument is not nullable and
    // libwayland dereferences it while marshalling.
    wl_array env;
    wl_array_init(&env);
    wdm_greeter_v1_start_session(greeter_, sessionId.c_str(), &env);
    wl_array_release(&env);
    // Flushed here and not on the next timer tick, because there may not be a
    // next tick: on success wdm tears the greeter down, and a start_session
    // still sitting in the output buffer is a login that never happens.
    dispatch();
}

bool Link::dispatch() {
    if (dead_) {
        return false;
    }
    if (wl_display_dispatch_queue_pending(display_, queue_) < 0) {
        die(reasonFromErrno());
        return false;
    }
    // EAGAIN from flush means the socket's buffer is full and libwayland will
    // finish the write later; every other errno means the request has been
    // dropped on the floor — a respond nobody will ever answer, a start_session
    // that never starts — and that has to reach the screen rather than a log.
    if (wl_display_flush(display_) < 0 && errno != EAGAIN) {
        die(reasonFromErrno());
        return false;
    }
    return true;
}

bool Link::readAndDispatch(int timeoutMs) {
    if (dead_) {
        return false;
    }

    // Non-zero means libwayland already has events queued for us and reading
    // would block behind them; dispatching is what makes progress.
    if (wl_display_prepare_read_queue(display_, queue_) != 0) {
        return dispatch();
    }

    // Flush before blocking: the thing we are waiting for is usually the answer
    // to a request still sitting in the output buffer.
    if (wl_display_flush(display_) < 0 && errno != EAGAIN) {
        wl_display_cancel_read(display_);
        die(reasonFromErrno());
        return false;
    }

    pollfd fd{wl_display_get_fd(display_), POLLIN, 0};
    const int ready = poll(&fd, 1, timeoutMs);
    if (ready > 0) {
        // A failure here — including EOF, which arrives as POLLHUP and then a
        // read of zero bytes — puts the display into its error state, and the
        // dispatch below is where that becomes a reported death. Not handled
        // twice.
        wl_display_read_events(display_);
    } else {
        wl_display_cancel_read(display_);
    }
    return dispatch();
}

} // namespace wdm
