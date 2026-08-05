// A real compositor-side wdm_greeter_v1, for testing Link against the protocol
// rather than against a mock of it.
//
// This is libwayland-server speaking the same XML the client half was generated
// from, over a socketpair, in the same process. A mock would agree with
// whatever Link did — the interesting failures are the ones libwayland reports:
// a request marshalled with the wrong signature, an event sent to a resource
// bound at a version that does not have it, an argument that is not nullable.
// Only a real server implementation fails for those.
//
// It runs on its own thread, because it has to. Link::connect() roundtrips, and
// a roundtrip blocks until the compositor answers; in the real greeter the
// compositor is another process, and here it has to be another thread or the
// first connect deadlocks against itself. Everything that touches libwayland
// therefore happens on that thread: the request handlers because the event loop
// calls them there, and the send* methods because they are queued to it. The
// members a test reads are guarded by the mutex.

#pragma once

#include <condition_variable>
#include <cstdint>
#include <deque>
#include <functional>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

// For wl_listener, which has to be stored by value: the fake has to hear about
// the greeter disconnecting on its own, or a later hangUp() would call
// wl_client_destroy on a client libwayland has already freed.
#include <wayland-server-core.h>

struct wl_client;
struct wl_display;
struct wl_event_loop;
struct wl_global;
struct wl_resource;

namespace wdm::test {

/// A user the fake will advertise during the enumerate phase.
struct UserSpec {
    std::string name;
    std::string displayName;
    std::string avatarPath;
    /// The user's own history. Left empty for a user who has never logged in,
    /// which is the case the version 1 substitution exists for.
    std::string lastSession;
};

/// A session the fake will advertise during the enumerate phase.
struct SessionSpec {
    std::string id;
    std::string name;
    std::string exec;
    std::uint32_t type = 0;
};

/// One start_session request, as it arrived.
struct StartSessionCall {
    std::string sessionId;
    /// The raw bytes of the env array. The greeter sends none, and this is how
    /// a test can say so rather than assume it.
    std::size_t envBytes = 0;
};

class FakeWdm {
public:
    /// `advertisedVersion` is the version of the global, which is what decides
    /// what the client can bind at. Pass 1 to be an older wdm.
    ///
    /// `advertiseGreeterGlobal` false makes this a compositor that is not wdm:
    /// a real Wayland server with a working registry that has no
    /// wdm_greeter_v1 on it. That is what the greeter meets when someone runs
    /// the binary from a terminal, and it is not the same as a connection that
    /// failed — the connection is fine, there is simply nothing to bind.
    explicit FakeWdm(std::uint32_t advertisedVersion, bool advertiseGreeterGlobal = true);
    ~FakeWdm();

    FakeWdm(const FakeWdm &) = delete;
    FakeWdm &operator=(const FakeWdm &) = delete;

    /// The client end of the socketpair, for wl_display_connect_to_fd, which
    /// takes ownership of it. Callable once.
    int takeClientFd();

    // What the enumerate phase will contain. Set these before the client
    // connects; they are read on the server thread when the client binds.
    std::vector<UserSpec> users;
    std::vector<SessionSpec> sessions;
    std::string defaultSession;
    std::string lastError;

    /// The version the client actually bound at, 0 before it binds.
    std::uint32_t boundVersion() const;

    // What the client asked for.
    std::vector<std::string> createSessionCalls() const;
    std::vector<std::pair<std::uint32_t, std::string>> respondCalls() const;
    int cancelCalls() const;
    std::optional<StartSessionCall> startSessionCall() const;

    /// The id of the prompt this side is waiting on an answer to.
    ///
    /// The server's view, which is the authoritative one: it is what wdm would
    /// compare a `respond` id against before raising stale_prompt.
    std::optional<std::uint32_t> pendingPrompt() const;

    /// Send a prompt, returning the id it was given. Ids come from a counter
    /// that is never reused, exactly as wdm's do.
    std::uint32_t sendPrompt(const std::string &text, std::uint32_t style);
    void sendAuthOk();
    void sendAuthFailed(const std::string &reason);

    /// Drop the client, the way wdm does when it hands the display over or when
    /// it kills a greeter. The client's next read sees EOF.
    void hangUp();

    /// Run `fn` on the server thread and wait for it to finish. Public because
    /// a test that wants to do something to the server that has no method here
    /// still must not do it from the test thread.
    void onServerThread(const std::function<void()> &fn);

private:
    struct Bindings;
    friend struct Bindings;

    /// wl_listener first, so that the notify callback can recover the fake from
    /// the listener pointer libwayland hands it without any offsetof
    /// arithmetic: the two are the same address in a standard-layout struct.
    struct ClientWatch {
        wl_listener listener;
        FakeWdm *fake;
    };

    void run();
    void sendEnumerate(wl_resource *resource);

    mutable std::mutex mutex_;
    std::condition_variable commandsDone_;
    struct Command {
        const std::function<void()> *fn;
        bool *done;
    };
    std::deque<Command> commands_;
    bool running_ = true;

    wl_display *display_ = nullptr;
    wl_event_loop *loop_ = nullptr;
    wl_global *global_ = nullptr;
    wl_client *client_ = nullptr;
    ClientWatch clientWatch_{};
    wl_resource *resource_ = nullptr;
    int clientFd_ = -1;
    std::uint32_t advertisedVersion_ = 0;
    std::uint32_t boundVersion_ = 0;
    std::uint32_t nextPromptId_ = 1;
    std::optional<std::uint32_t> pendingPrompt_;

    std::vector<std::string> createSessionCalls_;
    std::vector<std::pair<std::uint32_t, std::string>> respondCalls_;
    int cancelCalls_ = 0;
    std::optional<StartSessionCall> startSessionCall_;

    std::thread thread_;
};

} // namespace wdm::test
