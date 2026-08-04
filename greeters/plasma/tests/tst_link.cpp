// Link against a real compositor-side wdm_greeter_v1 over a socketpair.
//
// The mirror image of the harness on wdm's side of the same protocol: there,
// login.rs drives a fake greeter; here, a fake wdm drives the real client. What
// is being tested is the agreement between them, so both ends of every test are
// generated from the same XML and neither end is a stub.
//
// Catch2 and not QTest. Nothing in this layer links Qt — that is the property
// this file exists to preserve, and a test framework that pulled QtCore in
// would quietly destroy it while every test still passed. Catch2 links libstdc++
// and nothing else, so `ldd tst_link` naming no libQt* remains a fact this build
// can be checked against.

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <thread>
#include <vector>

#include <wayland-client.h>

#include <catch2/catch_test_macros.hpp>

#include "fakewdm.h"
#include "link.h"
#include "wdm-greeter-v1-client-protocol.h"

using namespace std::chrono_literals;
using wdm::Link;
using wdm::LinkObserver;
using wdm::Prompt;
using wdm::PromptStyle;
using wdm::Session;
using wdm::SessionType;
using wdm::User;
using wdm::test::FakeWdm;
using wdm::test::SessionSpec;
using wdm::test::UserSpec;

namespace wdm::test {

/// Link::readAndDispatch is private, and this is the only thing in the world
/// that can call it.
///
/// In the greeter Qt owns the connection fd; a second reader that gets
/// wl_display_prepare_read wrong live-locks the login screen, which is the
/// hazard a QSocketNotifier was rejected twice to avoid. The tests have no Qt
/// and so must play Qt's part, and this named friend is how they do it without
/// the method appearing in autocomplete next to dispatch() for the Qt layer.
struct LinkPump {
    static bool readAndDispatch(Link &link, int timeoutMs) {
        return link.readAndDispatch(timeoutMs);
    }
};

} // namespace wdm::test

namespace {

using wdm::test::LinkPump;

/// Everything Link reported, in the order it reported it.
class Recorder : public LinkObserver {
public:
    std::vector<User> users;
    std::vector<Session> sessions;
    std::string defaultSession;
    int defaultSessionEvents = 0;
    std::string lastError;
    int doneCount = 0;
    /// The list sizes at the moment `done` arrived. `done` promises the
    /// enumerate phase is complete, and a greeter builds its UI on that
    /// promise; recording the sizes here is how the test can tell "populated"
    /// from "populated eventually".
    std::size_t usersAtDone = 0;
    std::size_t sessionsAtDone = 0;
    std::vector<Prompt> prompts;
    int authOks = 0;
    int authFaileds = 0;
    std::string authFailure;
    int linkDeads = 0;
    std::string linkDeadWhy;

    void onUser(const User &user) override { users.push_back(user); }
    void onSession(const Session &session) override { sessions.push_back(session); }
    void onDefaultSession(const std::string &id) override {
        defaultSession = id;
        ++defaultSessionEvents;
    }
    void onLastError(const std::string &text) override { lastError = text; }
    void onEnumerateDone() override {
        ++doneCount;
        usersAtDone = users.size();
        sessionsAtDone = sessions.size();
    }
    void onPrompt(const Prompt &prompt) override { prompts.push_back(prompt); }
    void onAuthOk() override { ++authOks; }
    void onAuthFailed(const std::string &reason) override {
        ++authFaileds;
        authFailure = reason;
    }
    void onLinkDead(const std::string &why) override {
        ++linkDeads;
        linkDeadWhy = why;
    }
};

/// A fake wdm, a client connection to it, and a Link on that connection.
///
/// Constructed on the stack of every TEST_CASE, so a REQUIRE that throws out of
/// the middle of one still tears the server thread down through ~FakeWdm.
class Harness {
public:
    explicit Harness(std::uint32_t advertisedVersion, bool advertiseGreeterGlobal = true)
        : server(advertisedVersion, advertiseGreeterGlobal) {}

    ~Harness() {
        // The Link first: its destructor sends a destroy request, which needs
        // the connection to still be there.
        link.reset();
        if (display != nullptr) {
            wl_display_disconnect(display);
        }
    }

    Harness(const Harness &) = delete;
    Harness &operator=(const Harness &) = delete;

    /// One user, one Wayland session and a configured default, which is enough
    /// for every conversation test. The enumerate tests set their own.
    void withOrdinaryEnumerate() {
        server.users = {UserSpec{"alice", "Alice Liddell", "/var/lib/AccountsService/icons/alice",
                                 std::string()}};
        server.sessions = {SessionSpec{"plasma.desktop", "Plasma (Wayland)", "startplasma-wayland",
                                       WDM_GREETER_V1_SESSION_TYPE_WAYLAND}};
        server.defaultSession = "plasma.desktop";
    }

    void connect() {
        display = wl_display_connect_to_fd(server.takeClientFd());
        REQUIRE(display != nullptr);
        link = std::make_unique<Link>(display, &recorder);
        std::string error;
        if (!link->connect(&error)) {
            // The reason is the whole content of this failure: `connect` returns
            // a bare false, and "wdm_greeter_v1 not advertised" and "the
            // connection failed outright" are different bugs.
            FAIL("connect failed: " << error);
        }
    }

    /// Connect expecting connect() to refuse, returning what it said.
    ///
    /// Separate from connect() rather than a flag on it: the two have opposite
    /// success conditions, and a bool parameter at the call site would read as
    /// configuration rather than as the point of the test.
    std::string connectExpectingFailure() {
        display = wl_display_connect_to_fd(server.takeClientFd());
        REQUIRE(display != nullptr);
        link = std::make_unique<Link>(display, &recorder);
        std::string error;
        if (link->connect(&error)) {
            FAIL("connect succeeded, and it should not have");
        }
        return error;
    }

    FakeWdm server;
    Recorder recorder;
    wl_display *display = nullptr;
    std::unique_ptr<Link> link;
};

/// Wait for something the server thread will do, without touching the client.
///
/// A timeout is a hard failure carrying `what`, because Catch2 would otherwise
/// print `waitFor(...) == false` — which says that a wait ended, not what was
/// being waited for, and every one of these waits is on a different request
/// crossing a thread boundary.
void waitFor(const char *what, const std::function<bool()> &done,
             std::chrono::milliseconds timeout = 2s) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (!done()) {
        if (std::chrono::steady_clock::now() > deadline) {
            FAIL("timed out waiting for " << what);
        }
        std::this_thread::sleep_for(1ms);
    }
}

/// Play the part Qt plays in the real greeter — read the socket — until the
/// client has seen what the test is waiting for. Times out the same way.
void pumpUntil(Harness &harness, const char *what, const std::function<bool()> &done,
               std::chrono::milliseconds timeout = 2s) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (!done()) {
        if (std::chrono::steady_clock::now() > deadline) {
            FAIL("timed out waiting for " << what);
        }
        if (!LinkPump::readAndDispatch(*harness.link, 20) && harness.link->dead()) {
            // A dead link delivers nothing further; keep going only so the
            // caller's predicate gets a chance to be about the death itself.
            std::this_thread::sleep_for(1ms);
        }
    }
}

/// Pump for a fixed period expecting nothing, so that "nothing arrived" is a
/// claim the test made rather than a moment it did not look. Separate from
/// pumpUntil because here the timeout is the pass, not the failure.
void drain(Harness &harness, std::chrono::milliseconds period) {
    const auto deadline = std::chrono::steady_clock::now() + period;
    while (std::chrono::steady_clock::now() < deadline) {
        if (!LinkPump::readAndDispatch(*harness.link, 20) && harness.link->dead()) {
            std::this_thread::sleep_for(1ms);
        }
    }
}

/// Get as far as a pending secret prompt, which is where most of the
/// conversation tests start.
std::uint32_t promptFor(Harness &harness, const std::string &user) {
    harness.link->createSession(user);
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });
    const std::uint32_t id =
        harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the password prompt to reach the client",
              [&harness] { return !harness.recorder.prompts.empty(); });
    return id;
}

} // namespace

TEST_CASE("a version 2 binding receives the default session", "[link]") {
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    CHECK(harness.link->version() == 2u);
    CHECK(harness.server.boundVersion() == 2u);
    // The whole point of the event: the machine default arrives as a fact of
    // its own, distinct from any user's history.
    CHECK(harness.recorder.defaultSessionEvents == 1);
    CHECK(harness.recorder.defaultSession == "plasma.desktop");
    // And the user who has never logged in is reported as exactly that, rather
    // than as someone whose last session happened to be the default.
    REQUIRE(harness.recorder.users.size() == 1u);
    CHECK(harness.recorder.users[0].lastSession.empty());
}

TEST_CASE("a version 1 binding gets the default through last session instead", "[link]") {
    // The compositor advertises version 1, so min(2, advertised) binds 1. The
    // greeter is built against version 2 and must still work here: this is a
    // new greeter meeting an older wdm, which is the ordinary state of a
    // machine part way through an upgrade.
    Harness harness(1);
    harness.withOrdinaryEnumerate();
    harness.connect();

    CHECK(harness.link->version() == 1u);
    CHECK(harness.server.boundVersion() == 1u);
    // Never sent to a version 1 resource, and libwayland would abort the client
    // if it were: an event above the bound version has no opcode there.
    CHECK(harness.recorder.defaultSessionEvents == 0);
    CHECK(harness.recorder.defaultSession.empty());
    CHECK(!harness.link->dead());

    // The version 1 substitution — the configured default arriving in
    // last_session, because a version 1 peer has no other event to learn it
    // from — is performed entirely by the fake, in FakeWdm::sendEnumerate.
    // `Link` has no part in it: it copies last_session through whatever it is
    // told.
    //
    // So this is a check of harness fidelity and not of the client. It is worth
    // one line because a fake that stopped substituting would make the
    // assertions above pass for the wrong reason — the greeter would look
    // correct against a compositor that no wdm resembles — but it must not be
    // read as evidence that the greeter implements anything here.
    REQUIRE(harness.recorder.users.size() == 1u);
    CHECK(harness.recorder.users[0].lastSession == "plasma.desktop");
}

TEST_CASE("the enumerate phase ends at done with both lists populated", "[link]") {
    Harness harness(2);
    harness.server.users = {
        UserSpec{"alice", "Alice Liddell", "/icons/alice", "plasma.desktop"},
        UserSpec{"bob", std::string(), std::string(), std::string()},
    };
    harness.server.sessions = {
        SessionSpec{"plasma.desktop", "Plasma (Wayland)", "startplasma-wayland",
                    WDM_GREETER_V1_SESSION_TYPE_WAYLAND},
        SessionSpec{"plasmax11.desktop", "Plasma (X11)", "startplasma-x11",
                    WDM_GREETER_V1_SESSION_TYPE_X11},
    };
    harness.server.lastError = "the session exited immediately";
    harness.connect();

    // Sections rather than one run, because these four are independent claims
    // about one completed enumerate phase and each should be able to fail on
    // its own. Catch2 re-runs everything above from the top per section, which
    // costs another socketpair and another server thread and buys a report that
    // names which claim broke.

    SECTION("done arrives once, with both lists already complete") {
        CHECK(harness.recorder.doneCount == 1);
        // Both lists complete *at* done, not merely by the time the test looked.
        // connect() returns on done, and the QML engine is loaded against what is
        // there at that moment; anything arriving later would be a model that
        // flickers into shape in front of the user.
        CHECK(harness.recorder.usersAtDone == 2u);
        CHECK(harness.recorder.sessionsAtDone == 2u);
    }

    SECTION("each user carries the fields the account had") {
        REQUIRE(harness.recorder.users.size() == 2u);
        CHECK(harness.recorder.users[0].name == "alice");
        CHECK(harness.recorder.users[0].displayName == "Alice Liddell");
        CHECK(harness.recorder.users[0].avatarPath == "/icons/alice");
        // Empty GECOS stays empty here. Falling back to the login name is the
        // QObject layer's job, so that a theme binding displayName gets one rule
        // rather than writing the fallback itself.
        CHECK(harness.recorder.users[1].displayName.empty());
    }

    SECTION("each session carries its command and its type") {
        REQUIRE(harness.recorder.sessions.size() == 2u);
        CHECK(harness.recorder.sessions[0].name == "Plasma (Wayland)");
        CHECK(harness.recorder.sessions[0].exec == "startplasma-wayland");
        CHECK(harness.recorder.sessions[0].type == SessionType::Wayland);
        CHECK(harness.recorder.sessions[1].type == SessionType::X11);
    }

    SECTION("the previous launch failure is carried across") {
        // Why the last attempt bounced back to the login screen. Without it the
        // user sees an unexplained return to this exact screen.
        CHECK(harness.recorder.lastError == "the session exited immediately");
    }
}

TEST_CASE("a prompt and its answer carry the same id", "[link]") {
    // Deliberately one linear run and not sections: every step below depends on
    // the one before it, and a section restarts the case from the top, so
    // splitting it would test a conversation that never happened.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    harness.link->createSession("alice");
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });
    // Both sides of the comparison are read from an accessor that returns by
    // value — it reads state the server thread is writing, so it must — and
    // Catch2 evaluates the whole expression inside one full-expression, so the
    // temporary it indexes into is still alive when the failure is formatted.
    // Binding `const auto &` to `calls()[0]` outside the assertion would not be:
    // reference lifetime extension does not reach a subobject of a temporary.
    CHECK(harness.server.createSessionCalls()[0] == "alice");

    // A message first, because PAM sends them mixed in with questions and the
    // two must not be confused: an info prompt expects no answer, and a greeter
    // that treated it as pending would hold an id that respond is rejected for.
    harness.server.sendPrompt("Login attempts are logged.", WDM_GREETER_V1_PROMPT_STYLE_INFO);
    pumpUntil(harness, "the info prompt to reach the client",
              [&harness] { return !harness.recorder.prompts.empty(); });
    CHECK(harness.recorder.prompts[0].style == PromptStyle::Info);
    CHECK(!harness.recorder.prompts[0].expectsAnswer());
    CHECK(!harness.link->pendingPrompt().has_value());

    const std::uint32_t id =
        harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the password prompt to reach the client",
              [&harness] { return harness.recorder.prompts.size() == 2; });
    CHECK(harness.recorder.prompts[1].id == id);
    CHECK(harness.recorder.prompts[1].text == "Password: ");
    CHECK(harness.recorder.prompts[1].style == PromptStyle::Secret);
    REQUIRE(harness.link->pendingPrompt().has_value());
    CHECK(harness.link->pendingPrompt()->id == id);

    REQUIRE(harness.link->respond(id, "hunter2"));
    waitFor("respond to reach the compositor",
            [&harness] { return harness.server.respondCalls().size() == 1; });
    CHECK(harness.server.respondCalls()[0].first == id);
    CHECK(harness.server.respondCalls()[0].second == "hunter2");
    // Answered, so no longer pending on either side.
    CHECK(!harness.link->pendingPrompt().has_value());
    CHECK(!harness.server.pendingPrompt().has_value());

    // And answering it twice is refused here rather than forwarded, because
    // wdm's answer to a stale id is stale_prompt, which kills the greeter
    // mid-login.
    CHECK(!harness.link->respond(id, "hunter2 again"));
    CHECK(harness.server.respondCalls().size() == 1u);
}

TEST_CASE("an answer read from the pending prompt itself survives being sent", "[link]") {
    // The call every layer above this one will write. pendingPrompt() hands out
    // a `const std::optional<Prompt> &`, so `respond(p->id, p->text)` is the
    // obvious shape — and `respond` resets that optional before marshalling.
    // Taken by reference, the argument aliased the Prompt the reset destroyed
    // and libwayland marshalled freed memory that held a password. Nothing
    // observable went wrong: the bytes are usually still intact, which is
    // exactly why this needs a test and an ASan build rather than a reading.
    //
    // The answer is long on purpose, and that is the whole reason this case can
    // fail. libstdc++ keeps a string of 15 bytes or fewer inside the
    // std::string object itself, so destroying a short Prompt frees nothing and
    // ASan has no freed allocation to complain about — the bug is still there
    // and the test is green. Past the small-string buffer the characters live
    // on the heap, ~basic_string frees them, and reading c_str() afterwards is
    // a heap-use-after-free ASan reports. A password long enough to matter is
    // also the realistic case.
    const std::string answer = "correct horse battery staple";
    REQUIRE(answer.size() > 15u);

    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    harness.link->createSession("alice");
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });
    // The prompt text carries the answer, because the point is that the string
    // handed to respond lives inside the optional respond destroys, and the
    // pending Prompt is the only string there is.
    const std::uint32_t id = harness.server.sendPrompt(answer, WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the prompt to reach the client",
              [&harness] { return !harness.recorder.prompts.empty(); });

    const std::optional<Prompt> &pending = harness.link->pendingPrompt();
    REQUIRE(pending.has_value());
    REQUIRE(harness.link->respond(pending->id, pending->text));

    waitFor("respond to reach the compositor",
            [&harness] { return harness.server.respondCalls().size() == 1; });
    CHECK(harness.server.respondCalls()[0].first == id);
    // The whole assertion: the compositor received the password and not
    // whatever the allocator left behind.
    CHECK(harness.server.respondCalls()[0].second == answer);
}

TEST_CASE("answering a superseded prompt is refused and answering the live one is not", "[link]") {
    // The case the bool return exists for, and the one the pending-id half of
    // respond's guard is the only defence against. A greeter that kept the
    // first prompt's id — a stale widget, a queued keypress — would answer with
    // it, wdm would raise stale_prompt, and a protocol error kills the greeter
    // mid-login and leaves the user looking at nothing.
    //
    // One linear run and no sections: the second prompt has to supersede the
    // first, and a section would restart the case and test a conversation that
    // never happened.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    const std::uint32_t first = promptFor(harness, "alice");
    const std::uint32_t second =
        harness.server.sendPrompt("New password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the second prompt to reach the client",
              [&harness] { return harness.recorder.prompts.size() == 2; });
    REQUIRE(first != second);
    REQUIRE(harness.link->pendingPrompt().has_value());
    CHECK(harness.link->pendingPrompt()->id == second);

    // Refused, and — the part that matters — nothing goes on the wire. A
    // `false` returned after sending would still have killed the greeter.
    CHECK(!harness.link->respond(first, "hunter2"));
    CHECK(harness.server.respondCalls().empty());
    // Still pending afterwards: a rejected answer must not consume the question
    // the user is actually being asked.
    REQUIRE(harness.link->pendingPrompt().has_value());
    CHECK(harness.link->pendingPrompt()->id == second);

    CHECK(harness.link->respond(second, "correct horse"));
    waitFor("respond to reach the compositor",
            [&harness] { return harness.server.respondCalls().size() == 1; });
    CHECK(harness.server.respondCalls()[0].first == second);
    CHECK(harness.server.respondCalls()[0].second == "correct horse");
}

TEST_CASE("a visible prompt is a question and an error prompt is not", "[link]") {
    // The two styles the other cases never deliver, and they decide opposite
    // things. Visible is PAM asking for a token or a username rather than a
    // password: it must set pendingPrompt, or the greeter shows a question with
    // no way to answer it. Error is PAM saying something went wrong and
    // advancing the conversation itself: it must not, or the greeter waits
    // forever for an answer to a statement.
    //
    // Linear and not sectioned: the error arrives before the question, the way
    // PAM sends them.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    harness.link->createSession("alice");
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });

    harness.server.sendPrompt("Account locked for 10 minutes.",
                              WDM_GREETER_V1_PROMPT_STYLE_ERROR);
    pumpUntil(harness, "the error prompt to reach the client",
              [&harness] { return !harness.recorder.prompts.empty(); });
    CHECK(harness.recorder.prompts[0].style == PromptStyle::Error);
    CHECK(!harness.recorder.prompts[0].expectsAnswer());
    // Kept distinct from Info rather than flattened to "a notice": a theme that
    // can tell them apart shows the lockout in red and the minutes beside it in
    // grey, and one that cannot is merely plain.
    CHECK(harness.recorder.prompts[0].style != PromptStyle::Info);
    CHECK(!harness.link->pendingPrompt().has_value());

    const std::uint32_t id =
        harness.server.sendPrompt("One-time code: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
    pumpUntil(harness, "the visible prompt to reach the client",
              [&harness] { return harness.recorder.prompts.size() == 2; });
    CHECK(harness.recorder.prompts[1].style == PromptStyle::Visible);
    CHECK(harness.recorder.prompts[1].expectsAnswer());
    REQUIRE(harness.link->pendingPrompt().has_value());
    CHECK(harness.link->pendingPrompt()->id == id);
    // And it is answerable, which is the consequence the theme depends on.
    CHECK(harness.link->respond(id, "123456"));
}

TEST_CASE("a prompt style this greeter does not know is shown, not asked", "[link]") {
    // The default arm of styleFromProtocol, reached honestly: the fake puts a
    // value on the wire that the XML's prompt_style enum does not define, which
    // is exactly what a wdm built from a later protocol would do. libwayland
    // does not validate enum arguments, so this is the real encoding and not a
    // stub.
    //
    // Treated as a message and not as a question, deliberately: an unknown
    // style may well expect no answer, and a greeter waiting for a response PAM
    // is not asking for hangs the login screen, whereas one that shows the text
    // and moves on cannot.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    harness.link->createSession("alice");
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });

    constexpr std::uint32_t kUndefinedStyle = 99;
    harness.server.sendPrompt("Present your smartcard.", kUndefinedStyle);
    pumpUntil(harness, "the unknown-style prompt to reach the client",
              [&harness] { return !harness.recorder.prompts.empty(); });

    CHECK(harness.recorder.prompts[0].text == "Present your smartcard.");
    CHECK(harness.recorder.prompts[0].style == PromptStyle::Info);
    CHECK(!harness.recorder.prompts[0].expectsAnswer());
    CHECK(!harness.link->pendingPrompt().has_value());
    // Not a link failure: an unknown style is a newer compositor, not a broken
    // one, and a greeter that died on it would refuse to run against every wdm
    // released after it.
    CHECK(!harness.link->dead());
}

TEST_CASE("cancelling leaves no prompt pending on either side", "[link]") {
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptFor(harness, "alice");
    REQUIRE(harness.link->pendingPrompt().has_value());
    REQUIRE(harness.server.pendingPrompt().has_value());

    harness.link->cancel();
    waitFor("cancel to reach the compositor", [&harness] { return harness.server.cancelCalls() == 1; });

    // The client drops the prompt when it sends the cancel rather than when a
    // reply arrives, because there is no reply: the protocol says no auth_ok
    // and no auth_failed follows a cancel. A greeter waiting for confirmation
    // would hold a question that can never be answered again — which is the
    // state a user reaches by picking a different account halfway through
    // typing a password, and the state that has to leave PAM disarmed.
    CHECK(!harness.link->pendingPrompt().has_value());
    CHECK(!harness.server.pendingPrompt().has_value());

    // Nothing arrives afterwards either.
    drain(harness, 100ms);
    CHECK(harness.recorder.authOks == 0);
    CHECK(harness.recorder.authFaileds == 0);
    CHECK(harness.recorder.prompts.size() == 1u);
}

TEST_CASE("a failed conversation ends the pending prompt", "[link]") {
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptFor(harness, "alice");
    harness.server.sendAuthFailed("Authentication failure");
    pumpUntil(harness, "auth_failed to reach the client",
              [&harness] { return harness.recorder.authFaileds == 1; });

    CHECK(harness.recorder.authFailure == "Authentication failure");
    CHECK(!harness.link->pendingPrompt().has_value());
    // A failure is not a link failure: another attempt is possible, and whether
    // to make one is the user's, never the greeter's.
    CHECK(!harness.link->dead());
}

TEST_CASE("a successful conversation ends the pending prompt too", "[link]") {
    // The mirror of the auth_failed case, and it is not symmetry for its own
    // sake: a stack can succeed on a prompt the greeter never answered — a
    // module further down authenticated by another means, a smartcard was
    // presented — and auth_ok is then the end of the conversation with a
    // question still on screen. Without the reset the greeter carries a pending
    // prompt into a logged-in session and offers a password box under it, and
    // respond() on that id is a stale_prompt that kills the greeter during the
    // handoff.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptFor(harness, "alice");
    REQUIRE(harness.link->pendingPrompt().has_value());

    harness.server.sendAuthOk();
    pumpUntil(harness, "auth_ok to reach the client",
              [&harness] { return harness.recorder.authOks == 1; });

    CHECK(!harness.link->pendingPrompt().has_value());
    // And so the question cannot be answered afterwards either.
    CHECK(!harness.link->respond(1, "hunter2"));
    CHECK(harness.server.respondCalls().empty());
    CHECK(!harness.link->dead());
}

TEST_CASE("a compositor that is not wdm is refused with a sentence", "[link]") {
    // A real Wayland server with a working registry and no wdm_greeter_v1 on
    // it: the ordinary result of someone running the binary from a terminal.
    // The connection is fine, the roundtrip completes, there is simply nothing
    // to bind — and that deserves a sentence rather than a crash or a blank
    // window.
    Harness harness(2, /*advertiseGreeterGlobal=*/false);
    harness.withOrdinaryEnumerate();

    const std::string error = harness.connectExpectingFailure();

    // The text is what the user sees, so it has to say which of the two
    // failures this is. `connect` returns a bare false and "not under wdm" and
    // "the connection broke" want different things from the person reading it.
    CHECK(error.find("wdm_greeter_v1") != std::string::npos);
    CHECK(!error.empty());
    // Not dead: nothing failed. Latching linkDead here would make the greeter
    // report a lost connection it never had, and the Qt layer latches its own
    // linkDead off this.
    CHECK(!harness.link->dead());
    CHECK(harness.recorder.linkDeads == 0);
    // Nothing was enumerated, because nothing was bound.
    CHECK(harness.recorder.doneCount == 0);
    CHECK(harness.recorder.users.empty());
}

TEST_CASE("connecting twice is refused rather than leaking the first connection", "[link]") {
    // A second connect would overwrite queue_ and registry_, leaking both and
    // leaving the first registry listening into a destroyed queue — a
    // use-after-free that surfaces as a crash on some later dispatch, nowhere
    // near the call that caused it.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    std::string error;
    CHECK(!harness.link->connect(&error));
    CHECK(!error.empty());
    // The first connection is untouched and still works.
    CHECK(harness.link->version() == 2u);
    CHECK(!harness.link->dead());
    harness.link->createSession("alice");
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });
}

TEST_CASE("a launch carries the session id and no environment", "[link]") {
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    const std::uint32_t id = promptFor(harness, "alice");
    REQUIRE(harness.link->respond(id, "hunter2"));
    harness.server.sendAuthOk();
    pumpUntil(harness, "auth_ok to reach the client",
              [&harness] { return harness.recorder.authOks == 1; });

    harness.link->startSession("plasma.desktop");
    waitFor("start_session to reach the compositor",
            [&harness] { return harness.server.startSessionCall().has_value(); });
    CHECK(harness.server.startSessionCall()->sessionId == "plasma.desktop");
    // Empty, and empty deliberately: locale and keyboard come from wdm's own
    // configuration, and everything this greeter could put in the array would
    // be dropped by wdm's filter anyway.
    CHECK(harness.server.startSessionCall()->envBytes == 0u);
}

TEST_CASE("a flush failure latches the link dead as well", "[link]") {
    // The other limb of dispatch(), and the one the EAGAIN rule is about. A
    // flush that fails with EAGAIN has not lost anything — libwayland finishes
    // the write later — and dying on it would kill the login screen for a full
    // socket buffer. Every other errno means the request went on the floor: a
    // respond nobody will ever answer, a start_session that never starts. Both
    // have to be told apart here, because after this point Link is silent by
    // design and the user is typing into nothing.
    //
    // Reached without touching the dispatch limb, which is what makes this
    // about the flush. The socket is deliberately never read between the
    // hang-up and the request below, so libwayland cannot have seen the EOF:
    // wl_display_dispatch_queue_pending does not read the fd and has no error
    // to report, and the write is the first thing that meets the closed peer.
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    harness.server.hangUp();

    // createSession marshals the request and then dispatches, which is the
    // flush.
    harness.link->createSession("alice");

    CHECK(harness.link->dead());
    REQUIRE(harness.recorder.linkDeads == 1);
    // A real errno and not the errno==0 wording, which is what a protocol error
    // discovered while dispatching would have produced — so the reason itself
    // says which limb reported this.
    CHECK(harness.recorder.linkDeadWhy != "the compositor closed the connection");
    CHECK(!harness.recorder.linkDeadWhy.empty());
    CHECK(!harness.link->pendingPrompt().has_value());
}

TEST_CASE("a dispatch failure latches the link dead", "[link]") {
    Harness harness(2);
    harness.withOrdinaryEnumerate();
    harness.connect();

    // wdm dropping the greeter — which it does on a protocol error, and which
    // is indistinguishable here from the compositor dying.
    harness.server.hangUp();

    pumpUntil(harness, "the link death to be reported",
              [&harness] { return harness.recorder.linkDeads == 1; });

    // Three independent consequences of one death, so three sections: the
    // common part above reaches the same state each time, and a break in the
    // latching should not hide whether the requests still no-op.

    SECTION("the death is reported with a reason") {
        CHECK(harness.link->dead());
        // A greeter shows this string; an empty one leaves the user looking at a
        // form that stopped working for no stated reason.
        CHECK(!harness.recorder.linkDeadWhy.empty());
    }

    SECTION("it is latched, so it is never reported twice") {
        // Pumping again neither recovers nor reports twice. A greeter that
        // reported on every tick would push the one explanation the user has
        // off the screen sixty times a second.
        CHECK(!harness.link->dispatch());
        CHECK(!LinkPump::readAndDispatch(*harness.link, 0));
        CHECK(harness.recorder.linkDeads == 1);
    }

    SECTION("every request afterwards is a no-op") {
        // So that a theme that has not guarded its controls cannot post into a
        // dead socket and wait forever for an answer.
        CHECK(!harness.link->respond(1, "hunter2"));
        harness.link->createSession("alice");
        harness.link->startSession("plasma.desktop");
        CHECK(harness.link->dead());
    }
}
