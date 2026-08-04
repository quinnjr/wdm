#include "link.h"

#include <cerrno>
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
        self->pending_.reset();
        self->observer_->onAuthOk();
    }

    static void authFailed(void *data, wdm_greeter_v1 *, const char *reason) {
        auto *self = static_cast<Link *>(data);
        self->pending_.reset();
        self->observer_->onAuthFailed(reason);
    }

    static void defaultSession(void *data, wdm_greeter_v1 *, const char *id) {
        static_cast<Link *>(data)->observer_->onDefaultSession(id);
    }

    static const wl_registry_listener kRegistry;
    static const wdm_greeter_v1_listener kGreeter;
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

void Link::handlePrompt(std::uint32_t id, const char *text, std::uint32_t style) {
    Prompt prompt{id, text, styleFromProtocol(style)};
    if (prompt.expectsAnswer()) {
        pending_ = prompt;
    }
    observer_->onPrompt(prompt);
}

void Link::die(const std::string &why) {
    if (dead_) {
        return;
    }
    dead_ = true;
    pending_.reset();
    observer_->onLinkDead(why);
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
    if (wl_display_roundtrip_queue(display_, queue_) < 0) {
        *error = "lost the connection to the compositor: " + reasonFromErrno();
        die(*error);
        return false;
    }
    if (greeter_ == nullptr) {
        *error = "the compositor does not offer wdm_greeter_v1; this greeter only runs under wdm";
        return false;
    }
    if (wl_display_roundtrip_queue(display_, queue_) < 0) {
        *error = "lost the connection to the compositor: " + reasonFromErrno();
        die(*error);
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
