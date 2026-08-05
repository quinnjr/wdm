// wdm_greeter_v1 over the Wayland connection Qt already owns.
//
// Everything a Qt greeter needs from the protocol and nothing it needs from Qt.
// This half of the greeter links no Qt library at all, which is what lets
// tst_link drive it in a plain loop with no QGuiApplication and therefore on a
// machine with no display of any kind. The QObject that fans these events out
// to QML lives on the other side of the observer interface below and includes
// none of this.

#pragma once

#include <cstdint>
#include <optional>
#include <string>

struct wl_callback;
struct wl_display;
struct wl_event_queue;
struct wl_registry;
struct wl_output;
struct wdm_greeter_v1;

namespace wdm {

/// How a session expects to be displayed. Mirrors the protocol's session_type.
enum class SessionType {
    Wayland,
    X11,
};

/// How a prompt should be presented. Mirrors the protocol's prompt_style.
///
/// `Secret` and `Visible` expect an answer; `Info` and `Error` are PAM saying
/// something and the compositor advances the conversation by itself after them.
/// Link keeps that distinction rather than flattening it, because a theme that
/// can tell a lockout notice from a question can show the first in red and keep
/// the password box alive under it, and one that cannot is merely plain. A
/// greeter that mistook the two would either swallow the password box or wait
/// forever for an answer to a statement.
enum class PromptStyle {
    Secret,
    Visible,
    Info,
    Error,
};

/// A user wdm offered during the enumerate phase.
struct User {
    std::string name;
    /// From GECOS; empty when the account has none. Deliberately not
    /// substituted here — which of the two to show is the theme's, and the
    /// QObject layer is where the fallback belongs so that a theme binding
    /// `displayName` never sees an empty string.
    std::string displayName;
    /// Absolute path, empty when there is none. wdm reports the path recorded
    /// for the account rather than probing it, so it may not exist.
    std::string avatarPath;
    /// The session this user last logged in with. On a version 1 compositor
    /// this carries the machine-wide default for a user with no history,
    /// because a version 1 peer has no other way to learn it. See
    /// kInterfaceVersion.
    std::string lastSession;
};

/// A session wdm offered during the enumerate phase.
struct Session {
    std::string id;
    std::string name;
    /// The command line wdm would run. Reported because the protocol sends it;
    /// a greeter has no use for it beyond showing it, and must not act on it.
    std::string exec;
    SessionType type = SessionType::Wayland;
};

/// One prompt event, as it arrived.
struct Prompt {
    /// Echoed back in respond(). Prompt ids come from a counter that is never
    /// reused, so answering a stale one is a protocol error rather than a
    /// misdelivered answer — which is exactly why the id travels with the text
    /// instead of being tracked by position.
    std::uint32_t id = 0;
    std::string text;
    PromptStyle style = PromptStyle::Secret;

    /// Whether this prompt is asking for something rather than telling.
    bool expectsAnswer() const {
        return style == PromptStyle::Secret || style == PromptStyle::Visible;
    }
};

namespace test {
/// The tests' way in to Link::readAndDispatch, which is private.
///
/// Named here rather than befriending the whole test file, so that the one
/// method the greeter must never call has exactly one caller and that caller is
/// a type nothing in the greeter can construct a reason to use.
struct LinkPump;
} // namespace test

/// What Link tells its owner. Plain structs, no signals, no Qt.
///
/// Every method has an empty default so that a protocol event added later does
/// not break an implementation that does not care about it — the same reason
/// the Rust client's model structs are #[non_exhaustive].
class LinkObserver {
public:
    virtual ~LinkObserver();

    virtual void onUser(const User &) {}
    virtual void onSession(const Session &) {}
    /// The machine-wide default from wdm's configuration, empty when none is
    /// set. Never sent by a version 1 compositor.
    virtual void onDefaultSession(const std::string &) {}
    /// Why the previous session launch failed. Sent only when the greeter was
    /// relaunched after one; without showing it, a user sees an unexplained
    /// bounce back to the login screen.
    virtual void onLastError(const std::string &) {}
    /// The enumerate phase is complete and both lists are final.
    virtual void onEnumerateDone() {}
    virtual void onPrompt(const Prompt &) {}
    virtual void onAuthOk() {}
    /// The conversation is over and it did not succeed. The argument is the
    /// human-readable reason. Not an error condition: another attempt is
    /// possible, and whether to make one is the user's.
    virtual void onAuthFailed(const std::string &) {}
    /// The connection is gone and nothing will arrive on it again; the argument
    /// says why. Reported at most once — see Link::dead().
    virtual void onLinkDead(const std::string &) {}
};

/// The event queue, the binding, and the five requests.
class Link {
public:
    /// Highest wdm_greeter_v1 version this greeter understands.
    ///
    /// Bound as min(advertised, this). Version 2 adds default_session; under a
    /// version 1 wdm the greeter simply never sees it and the compositor
    /// substitutes the configured default into each user's last_session
    /// instead, which is a substitution the theme cannot detect and does not
    /// need to.
    static constexpr std::uint32_t kInterfaceVersion = 2;

    /// Takes the display; does not open one and does not close it.
    ///
    /// The greeter cannot open its own Wayland connection. wdm's socket accepts
    /// the greeter account once — SO_PEERCRED is checked per connection and the
    /// greeter is a single client — so objects created on a second connection
    /// would belong to a client with no surfaces, and nothing they said could
    /// ever be rendered. Qt owns the connection; this borrows it.
    ///
    /// The observer must outlive the Link and must be set before connect(),
    /// because the whole enumerate phase is delivered inside it.
    explicit Link(wl_display *display, LinkObserver *observer);
    ~Link();

    Link(const Link &) = delete;
    Link &operator=(const Link &) = delete;

    /// How long the enumerate phase is given before it is called a failure.
    ///
    /// Bounded because the unbounded version had exactly one failure mode and
    /// it was the worst one available: a wdm that accepts the connection and
    /// then stops answering — wedged rather than crashed, so no EOF and no
    /// error — leaves the greeter blocked inside connect() forever, with no
    /// window, nothing on stderr, and nothing for wdm's give-up screen to say.
    /// That is the blank login screen the top of main.cpp says this greeter
    /// exists to prevent, reached through the one path that had no deadline on
    /// it.
    ///
    /// Ten seconds, and the number is chosen from what is on the other side:
    /// wdm's enumerate phase is a scan of /etc/passwd and the session
    /// directories, work measured in milliseconds even on a machine that is
    /// swapping. Anything an order of magnitude past that is not slow, it is
    /// stopped. Long enough that no real machine trips it, short enough that
    /// the user gets a sentence instead of a black screen.
    static constexpr int kEnumerateTimeoutMs = 10000;

    /// Bind the global and receive the enumerate phase.
    ///
    /// Returns once `done` has arrived, so the QML engine loads against a
    /// populated model rather than flickering into one. False with `error` set
    /// when wdm_greeter_v1 is not advertised — the ordinary result of running
    /// the binary from a terminal, which deserves a sentence rather than a
    /// crash — when the connection failed outright, or when wdm accepted the
    /// connection and then stopped answering; see kEnumerateTimeoutMs.
    ///
    /// This is the one method that reads the connection's fd itself. It is
    /// called before QGuiApplication's event loop is running, so there is no
    /// second reader to race — which is why the two roundtrips it replaced were
    /// correct here and would not have been anywhere else.
    ///
    /// Callable once. A second call would overwrite the queue and the registry,
    /// leaking both and leaving the old registry listening into a destroyed
    /// queue; it returns false with `error` set rather than doing that. There is
    /// no reason to call it twice — wdm accepts the greeter's connection once,
    /// and a connect that failed did so for a reason that will not have changed
    /// — so this is a precondition made enforceable rather than a retry taken
    /// away.
    bool connect(std::string *error);

    /// The version actually bound, 0 before connect() succeeds.
    std::uint32_t version() const { return version_; }

    /// Set once a dispatch or flush has failed, and never cleared: a Wayland
    /// connection does not come back, and wdm accepts the greeter's exactly
    /// once. Everything after this point is a no-op, which is why the failure
    /// has to reach the screen — a greeter that silently stops sending leaves
    /// the user pressing Enter into a dead socket under a form that never
    /// changes again.
    bool dead() const { return dead_; }

    /// The question waiting for an answer, if any.
    ///
    /// Info and error prompts never land here: they expect no response, and a
    /// greeter that treated one as pending would hold a prompt id that
    /// `respond` would be rejected for. Cleared by respond(), cancel(), the end
    /// of the conversation, and link death.
    const std::optional<Prompt> &pendingPrompt() const { return pending_; }

    /// Whether a cancel's barrier is still outstanding — see cancel().
    ///
    /// For the tests, which have to be able to say "the barrier was up and then
    /// it came down" rather than infer it from what did not happen. The greeter
    /// never reads it: everything it changes has already been decided by the
    /// time an observer callback runs.
    bool discardingUntilBarrier() const { return discarding_; }

    /// Begin a PAM conversation. Only ever from a deliberate user action — a
    /// conversation is a login attempt as far as pam_faillock is concerned for
    /// its whole duration, including the part where it is waiting for an answer
    /// nobody is there to give, and enough of those lock the account.
    void createSession(const std::string &username);
    /// Answer the pending prompt. False, having sent nothing, when `id` is not
    /// the pending prompt's.
    ///
    /// Refused rather than forwarded because wdm raises stale_prompt for it and
    /// a protocol error kills the greeter mid-login — the user is left looking
    /// at whatever the compositor draws when nothing is on screen. A caller
    /// that gets here has a bug, and it should surface as a false and a message
    /// in the journal rather than as a dead login screen.
    ///
    /// `response` is taken by value, and that is load-bearing rather than
    /// stylistic. The natural call from the layer above is
    /// `respond(p->id, p->text)` off the reference pendingPrompt() hands out, so
    /// the argument routinely aliases the very Prompt that answering destroys.
    /// By value, the copy is made before the destruction; by const reference it
    /// was marshalled afterwards, and the freed bytes being marshalled are a
    /// password.
    bool respond(std::uint32_t id, std::string response);

    /// Abandon the conversation, and refuse everything wdm sent before it heard
    /// so.
    ///
    /// The second half is the load-bearing one, and it is why this is a barrier
    /// rather than a flag on the layer above. Dropping `pending_` here is not
    /// enough: a prompt already on the wire is dispatched *after* the cancel is
    /// sent, `handlePrompt` puts it back into `pending_`, and Link will then
    /// accept a `respond` quoting its id. If the theme has meanwhile started a
    /// second conversation — one 16 ms pump tick is all it takes: the user
    /// picks another account and presses Enter — the answer buffered for the
    /// *second* conversation is sent under the *first* conversation's prompt
    /// id. wdm meets that with stale_prompt, which kills the greeter, and the
    /// request that killed it carried one user's password under another user's
    /// conversation.
    ///
    /// So the cancel is followed by a `wl_display_sync` on our own queue, and
    /// every prompt and verdict arriving before its `done` is dropped without
    /// touching `pending_`. Wayland orders a client's requests and a
    /// compositor's events on one connection, so everything wdm emitted before
    /// it processed the cancel precedes that `done` and everything belonging to
    /// the next conversation follows it. The discard is therefore by
    /// construction rather than by timing — which is the difference between
    /// this and a guard that happens to win the race on the machines it was
    /// tried on.
    ///
    /// Not a `wl_display_roundtrip_queue`, which would have been three lines
    /// instead of thirty: it blocks the thread that draws the login screen on a
    /// compositor that may be wedged rather than gone, and a greeter frozen
    /// mid-cancel is the blank screen this greeter is built to avoid.
    void cancel();
    /// Launch a session. Sends no environment: locale and keyboard come from
    /// wdm's own configuration, and the env argument exists for greeters that
    /// let the user change them.
    void startSession(const std::string &sessionId);

    /// Deliver whatever libwayland has already queued for our objects, then
    /// flush our requests.
    ///
    /// This never reads the socket. Qt owns the fd and its event loop is what
    /// turns bytes on the wire into queued events; the Qt layer calls this from
    /// a timer.
    ///
    /// ponytail: polled rather than read. Ceiling: up to one timer period of
    /// latency between wdm sending a prompt and the theme showing it, which is
    /// not observable by a human typing a password. Upgrade: a QSocketNotifier
    /// on the connection fd cooperating with Qt through
    /// wl_display_prepare_read/read_events — rejected for now because two
    /// readers that get the handshake wrong live-lock the login screen, a
    /// hazard already identified and rejected in the GTK greeter.
    ///
    /// Returns false once the link is dead.
    bool dispatch();

private:
    // Every wayland listener trampoline, defined in link.cpp so that this
    // header does not have to name the generated listener structs.
    struct Callbacks;
    friend struct Callbacks;
    friend struct test::LinkPump;

    /// Read the socket ourselves, then dispatch.
    ///
    /// Private, and reachable only through test::LinkPump. In the greeter Qt
    /// owns the fd, and two readers racing for it through
    /// wl_display_prepare_read is the live-lock this design rejected a
    /// QSocketNotifier twice to avoid — so a comment saying "test only" on a
    /// public method is not enough. The Qt layer holds a Link* and would see
    /// this in autocomplete beside dispatch(), which is the whole mistake
    /// waiting to be made. The tests have no Qt, so they call it to play Qt's
    /// part, and by ending in dispatch() they exercise the path the greeter
    /// actually takes.
    ///
    /// `timeoutMs` bounds the wait for the socket to become readable.
    ///
    /// connect() is the one caller inside the greeter, and it is a caller for
    /// the same reason the tests are: at that point Qt's event loop has not
    /// started, so nothing else is reading the fd.
    bool readAndDispatch(int timeoutMs);

    /// wl_display_roundtrip_queue with a deadline.
    ///
    /// libwayland's own roundtrip has none, and a compositor that answers
    /// nothing is not distinguishable from one that is about to. Returns false
    /// with `error` set on the deadline and on the connection failing.
    bool roundtrip(int timeoutMs, std::string *error);

    void handleGlobal(wl_registry *registry, std::uint32_t name, const char *interface,
                      std::uint32_t version);
    void handlePrompt(std::uint32_t id, const char *text, std::uint32_t style);
    /// Whether this conversation event belongs to a conversation wdm has
    /// already been told to abandon. See cancel().
    bool discardConversationEvent(const char *what);
    /// Latch the failure and report it once.
    void die(const std::string &why);
    /// Destroy the outstanding cancel barrier, if there is one.
    void dropBarrier();

    wl_display *display_ = nullptr;
    LinkObserver *observer_ = nullptr;
    wl_event_queue *queue_ = nullptr;
    wl_registry *registry_ = nullptr;
    wdm_greeter_v1 *greeter_ = nullptr;
    std::uint32_t version_ = 0;
    bool dead_ = false;
    std::optional<Prompt> pending_;

    /// The outstanding cancel barrier, and whether it is still up. See
    /// cancel(). Two members rather than one because the callback has to be
    /// destroyed on the paths that never dispatch it — link death and this
    /// object's own destruction — and a null proxy is not the same fact as a
    /// barrier that has come down.
    wl_callback *barrier_ = nullptr;
    bool discarding_ = false;
};

} // namespace wdm
