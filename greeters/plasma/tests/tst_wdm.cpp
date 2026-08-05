// The QML-facing object's models and state machine, with no window and no
// display.
//
// Driven against the same fake compositor tst_link uses, over a socketpair,
// because the interesting assertions are about what actually goes on the wire:
// that a buffered password reaches wdm when PAM asks a masked question and does
// not when it asks a visible one is a claim about a `respond` request, and a
// test that only inspected this object's properties would pass with the request
// missing entirely.
//
// No QPA platform plugin is involved, not even offscreen. `Wdm` links QtCore
// and QtQml and nothing else — that is what the split between it and `Link` is
// for — so a QCoreApplication is the whole of the Qt this file needs.
//
// What is not tested here and is not tested anywhere:
//
//   - the window, the layer surface and the theme's rendering. They need a
//     compositor and a seat, the same acknowledged gap as wdm's own DRM path;
//   - main.cpp's own sequencing, other than the argument snapshot, which the
//     `plasma-greeter-refuses-qt-arguments` ctest entry drives by running the
//     binary;
//   - logging.h's messageHandler. The two decisions inside it are covered
//     below — severityRank's ordering and every word WDM_GREETER_LOG takes —
//     but the printing itself needs a process-wide handler installed and
//     stderr captured, and the QtFatalMsg arm ends the process by construction,
//     which is the behaviour under test. A case that exercised it would take
//     the test binary with it.

#include <chrono>
#include <cstdint>
#include <fstream>
#include <functional>
#include <memory>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include <wayland-client.h>

#include <QCoreApplication>
#include <QMetaMethod>
#include <QMetaObject>
#include <QQmlEngine>
#include <QString>
#include <QStringList>
#include <QVariantMap>

#include <catch2/catch_test_macros.hpp>

#include "fakewdm.h"
#include "link.h"
#include "logging.h"
#include "models.h"
#include "wdm-greeter-v1-client-protocol.h"
#include "wdm.h"

using namespace std::chrono_literals;
using wdm::Link;
using wdm::SessionModel;
using wdm::UserModel;
using wdm::test::FakeWdm;
using wdm::test::SessionSpec;
using wdm::test::UserSpec;

namespace wdm::test {

/// Link::readAndDispatch is private, and this named friend is the only thing
/// that can reach it. Declared identically in tst_link.cpp, for the same
/// reason: in the greeter Qt owns the connection fd, and the tests have no Qt
/// event loop reading it, so they must play Qt's part.
struct LinkPump {
    static bool readAndDispatch(Link &link, int timeoutMs) {
        return link.readAndDispatch(timeoutMs);
    }
};

} // namespace wdm::test

namespace {

using wdm::test::LinkPump;

/// The one QCoreApplication these tests share.
///
/// QQmlEngine wants an application instance to exist, and Catch2 supplies the
/// main() — so it is created on first use and never destroyed, which is the
/// ordinary shape for a process-lifetime singleton and avoids ordering an
/// application object's destruction against static QObjects.
QCoreApplication &application() {
    static int argc = 1;
    static char program[] = "tst_wdm";
    static char *argv[] = {program, nullptr};
    static QCoreApplication instance(argc, argv);
    return instance;
}

/// Everything the object emitted, in order.
struct Spy {
    std::vector<std::pair<QString, QString>> messages;
    int completions = 0;

    void watch(wdm::Wdm *bridge) {
        QObject::connect(bridge, &wdm::Wdm::message, bridge,
                         [this](const QString &text, const QString &kind) {
                             messages.emplace_back(text, kind);
                         });
        QObject::connect(bridge, &wdm::Wdm::authenticationComplete, bridge,
                         [this] { ++completions; });
    }

    /// The messages of one kind, joined, for an assertion that wants to say
    /// what was said rather than count it.
    QStringList of(const QString &kind) const {
        QStringList out;
        for (const auto &entry : messages) {
            if (entry.second == kind) {
                out.append(entry.first);
            }
        }
        return out;
    }
};

/// A fake wdm, a Link on a socketpair to it, and the Wdm object under test.
class Harness {
public:
    explicit Harness(std::uint32_t advertisedVersion = 2)
        : app(application()), server(advertisedVersion) {
        spy.watch(&bridge);
        // An engine from the first moment, because a raise with no engine is
        // reported with qCritical and cannot be observed — and every one of the
        // precondition cases below is an assertion that the engine was thrown
        // into.
        bridge.setQmlEngine(&engine);
    }

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

    /// Two users, two sessions and a configured default: enough for the model
    /// assertions, for the preselection facts a theme reads, and for every
    /// conversation case.
    void withOrdinaryEnumerate() {
        server.users = {
            UserSpec{"alice", "Alice Liddell", "/icons/alice", "plasmax11.desktop"},
            // No GECOS and no history, which is the account the displayName
            // fallback exists for.
            UserSpec{"bob", std::string(), std::string(), std::string()},
        };
        server.sessions = {
            SessionSpec{"plasma.desktop", "Plasma (Wayland)", "startplasma-wayland",
                        WDM_GREETER_V1_SESSION_TYPE_WAYLAND},
            SessionSpec{"plasmax11.desktop", "Plasma (X11)", "startplasma-x11",
                        WDM_GREETER_V1_SESSION_TYPE_X11},
        };
        server.defaultSession = "plasma.desktop";
    }

    void connect() {
        display = wl_display_connect_to_fd(server.takeClientFd());
        REQUIRE(display != nullptr);
        link = std::make_unique<Link>(display, &bridge);
        bridge.attach(link.get());
        std::string error;
        if (!link->connect(&error)) {
            FAIL("connect failed: " << error);
        }
    }

    /// First member, and that is the whole reason it is a member at all: the
    /// QQmlEngine below asserts that an application object exists, and members
    /// are constructed before the constructor's body runs. Creating the
    /// application in the body aborted every case in this file.
    QCoreApplication &app;
    FakeWdm server;
    wdm::Wdm bridge;
    QQmlEngine engine;
    Spy spy;
    wl_display *display = nullptr;
    std::unique_ptr<Link> link;
};

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

/// Read the socket until the client has seen what the test is waiting for.
void pumpUntil(Harness &harness, const char *what, const std::function<bool()> &done,
               std::chrono::milliseconds timeout = 2s) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (!done()) {
        if (std::chrono::steady_clock::now() > deadline) {
            FAIL("timed out waiting for " << what);
        }
        if (!LinkPump::readAndDispatch(*harness.link, 20) && harness.link->dead()) {
            std::this_thread::sleep_for(1ms);
        }
    }
}

/// Pump for a fixed period expecting nothing, so that "nothing arrived" is a
/// claim the test made rather than a moment it did not look.
void drain(Harness &harness, std::chrono::milliseconds period = 60ms) {
    const auto deadline = std::chrono::steady_clock::now() + period;
    while (std::chrono::steady_clock::now() < deadline) {
        if (!LinkPump::readAndDispatch(*harness.link, 10) && harness.link->dead()) {
            std::this_thread::sleep_for(1ms);
        }
    }
}

/// Whether the engine is holding an error, clearing it either way.
///
/// This is how a raise is observed. `QQmlEngine::throwError` from inside a
/// Q_INVOKABLE is what gives a theme's mistake a file and a line; called from
/// C++, as these tests do, it leaves the same pending error on the engine,
/// which is the fact being asserted — that the call refused and reported rather
/// than proceeding.
bool tookError(Harness &harness) {
    if (!harness.engine.hasError()) {
        return false;
    }
    harness.engine.catchError();
    return true;
}

/// Get as far as a pending secret prompt with the buffer already spent, which
/// is where the cases about answering start.
void promptAfterBuffer(Harness &harness, const QString &user, const QString &typed) {
    REQUIRE(harness.bridge.authenticate(user, typed));
    waitFor("create_session to reach the compositor",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });
    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the buffered answer to reach the compositor",
              [&harness] { return harness.server.respondCalls().size() == 1; });
}

/// How many times `needle` appears in `haystack` outside a `//` comment.
///
/// Comment-aware because the theme's own comments name the calls they are
/// about — the file explains at length that authenticate() is called from the
/// submit handler and nowhere else, and a plain count would be counting the
/// explanation as well as the thing explained. Line comments only: the theme is
/// QML and QML's block comments are not used in it, and a scanner that tried to
/// track them would be a second parser to get wrong.
std::size_t countOutsideComments(const std::string &haystack, const std::string &needle) {
    std::size_t count = 0;
    std::istringstream lines(haystack);
    std::string line;
    while (std::getline(lines, line)) {
        const std::size_t comment = line.find("//");
        const std::string code = comment == std::string::npos ? line : line.substr(0, comment);
        for (std::size_t at = code.find(needle); at != std::string::npos;
             at = code.find(needle, at + 1)) {
            ++count;
        }
    }
    return count;
}

std::string readFile(const char *path) {
    std::ifstream file(path);
    if (!file.is_open()) {
        FAIL("cannot read " << path);
    }
    std::ostringstream contents;
    contents << file.rdbuf();
    return contents.str();
}

} // namespace

// --------------------------------------------------------------------------
// Models
// --------------------------------------------------------------------------

TEST_CASE("the models carry what was enumerated, under the documented role names", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    auto *users = qobject_cast<UserModel *>(harness.bridge.users());
    auto *sessions = qobject_cast<SessionModel *>(harness.bridge.sessions());
    REQUIRE(users != nullptr);
    REQUIRE(sessions != nullptr);

    SECTION("the role names are the ones the theme contract documents") {
        // Spelled out rather than compared against the enum, because these
        // strings are the public contract: every theme writes textRole:
        // "displayName", and renaming one here breaks every theme silently —
        // QML resolves a role name at runtime and a missing one is an empty
        // delegate, not an error.
        const QHash<int, QByteArray> userRoles = users->roleNames();
        CHECK(userRoles.values().contains(QByteArrayLiteral("name")));
        CHECK(userRoles.values().contains(QByteArrayLiteral("displayName")));
        CHECK(userRoles.values().contains(QByteArrayLiteral("avatarPath")));
        CHECK(userRoles.values().contains(QByteArrayLiteral("lastSession")));
        CHECK(userRoles.size() == 4);

        const QHash<int, QByteArray> sessionRoles = sessions->roleNames();
        CHECK(sessionRoles.values().contains(QByteArrayLiteral("id")));
        CHECK(sessionRoles.values().contains(QByteArrayLiteral("name")));
        CHECK(sessionRoles.values().contains(QByteArrayLiteral("type")));
        CHECK(sessionRoles.size() == 3);
    }

    SECTION("the row counts are what wdm enumerated") {
        CHECK(users->rowCount() == 2);
        CHECK(sessions->rowCount() == 2);
        // Populated by the time connect() returned, which is before the engine
        // is created in main.cpp. A theme therefore never renders an empty list
        // and then flickers into a populated one.
        CHECK(users->get(0).value(QStringLiteral("name")).toString() == QStringLiteral("alice"));
    }

    SECTION("displayName falls back to the login name") {
        // The fallback is the greeter's, once, so that a theme binding
        // displayName cannot render a blank row for an account with no GECOS
        // entry — and so that no theme has to write the rule itself.
        CHECK(users->get(0).value(QStringLiteral("displayName")).toString()
              == QStringLiteral("Alice Liddell"));
        CHECK(users->get(1).value(QStringLiteral("displayName")).toString()
              == QStringLiteral("bob"));
        // And the raw name is still there, because it is what authenticate()
        // takes and what wdm knows the account by.
        CHECK(users->get(1).value(QStringLiteral("name")).toString() == QStringLiteral("bob"));
    }

    SECTION("a session's type is the string the contract names") {
        CHECK(sessions->get(0).value(QStringLiteral("type")).toString()
              == QStringLiteral("wayland"));
        CHECK(sessions->get(1).value(QStringLiteral("type")).toString() == QStringLiteral("x11"));
    }

    SECTION("indexOf is what a theme preselects a session with") {
        // A recorded lastSession can name a session that has been uninstalled
        // since, and a ComboBox told to select an id no row carries shows
        // nothing at all — so the theme asks, and -1 is the answer it acts on.
        CHECK(sessions->indexOf(QStringLiteral("plasmax11.desktop")) == 1);
        CHECK(sessions->indexOf(QStringLiteral("gone.desktop")) == -1);
        CHECK(users->indexOf(QStringLiteral("bob")) == 1);
    }

    SECTION("a row that does not exist is an empty map rather than an error") {
        // What a theme's binding reads for the one frame between a ComboBox
        // having no selection and having one.
        CHECK(users->get(-1).isEmpty());
        CHECK(users->get(99).isEmpty());
    }

    SECTION("the machine default arrives as a fact of its own") {
        CHECK(harness.bridge.defaultSession() == QStringLiteral("plasma.desktop"));
        // And a user who has never logged in is reported as exactly that,
        // rather than as someone whose history happens to be the default.
        CHECK(users->get(1).value(QStringLiteral("lastSession")).toString().isEmpty());
    }
}

TEST_CASE("the previous launch failure reaches the theme as lastError", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.server.lastError = "the session exited immediately";
    harness.connect();

    // Without it a user whose session crashed is bounced back to this exact
    // screen with no explanation of why.
    CHECK(harness.bridge.lastError() == QStringLiteral("the session exited immediately"));
}

// --------------------------------------------------------------------------
// Prompts
// --------------------------------------------------------------------------

TEST_CASE("the password field is masked from the first frame and re-armed at every end",
          "[wdm]") {
    // Requirement 5, and until this case existed it was enforced by a single
    // character — the `= true` on promptSecret_'s declaration — with nothing
    // that could notice it changing. Every other assertion in this file is taken
    // *after* a prompt, where setPrompt writes promptSecret explicitly; flipping
    // the initialiser to false put the user's password on screen in the clear on
    // the first frame of the login screen and left all fifteen cases passing.
    //
    // It is masked-first and not merely masked-when-known because of the order
    // the greeter works in: nothing arms PAM until the user submits, so there is
    // no prompt when they type their password, and the field they type it into
    // is bound to this. A theme gets a masked field with no special case of its
    // own. A stack whose first question is echo-on unmasks it when the prompt
    // arrives — that is the direction that can be corrected on screen. The other
    // cannot: the password has already been read by whoever was looking.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    SECTION("before anything has been submitted") {
        // The first frame: enumerate is complete, the theme's bindings are being
        // evaluated, and no prompt has ever arrived.
        CHECK(!harness.bridge.hasPrompt());
        CHECK(harness.bridge.promptText().isEmpty());
        CHECK(harness.bridge.promptSecret());
    }

    SECTION("after a failed attempt, ready for the next one") {
        // The state a user is in after mistyping a password, which is the most
        // common moment for the field to be typed into with no prompt pending.
        promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
        harness.server.sendAuthFailed("Authentication failure");
        pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });

        CHECK(!harness.bridge.hasPrompt());
        CHECK(harness.bridge.promptSecret());
    }

    SECTION("after a visible prompt was answered and the attempt ended") {
        // The one path that can leave promptSecret false: a visible prompt sets
        // it, and endConversation has to put it back. Without the re-arm the
        // next attempt's field is unmasked — a stack that asks an echo-on
        // question once would unmask the password field for the rest of the
        // login screen's life.
        REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        waitFor("create_session",
                [&harness] { return harness.server.createSessionCalls().size() == 1; });
        harness.server.sendPrompt("One-time code: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
        pumpUntil(harness, "the visible prompt", [&harness] { return harness.bridge.hasPrompt(); });
        REQUIRE(!harness.bridge.promptSecret());

        harness.server.sendAuthFailed("Authentication failure");
        pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });
        CHECK(!harness.bridge.hasPrompt());
        CHECK(harness.bridge.promptSecret());
    }

    SECTION("after a cancel") {
        // cancel() reaches the same endConversation, and it is the path a user
        // takes by picking a different account mid-attempt — after which they
        // type a password into that same field.
        REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        waitFor("create_session",
                [&harness] { return harness.server.createSessionCalls().size() == 1; });
        harness.server.sendPrompt("One-time code: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
        pumpUntil(harness, "the visible prompt", [&harness] { return harness.bridge.hasPrompt(); });
        REQUIRE(!harness.bridge.promptSecret());

        harness.bridge.cancel();
        waitFor("cancel", [&harness] { return harness.server.cancelCalls() == 1; });
        CHECK(!harness.bridge.hasPrompt());
        CHECK(harness.bridge.promptSecret());
    }

    SECTION("after the link died mid-attempt") {
        REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        waitFor("create_session",
                [&harness] { return harness.server.createSessionCalls().size() == 1; });
        harness.server.sendPrompt("One-time code: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
        pumpUntil(harness, "the visible prompt", [&harness] { return harness.bridge.hasPrompt(); });
        REQUIRE(!harness.bridge.promptSecret());

        harness.server.hangUp();
        pumpUntil(harness, "the link death", [&harness] { return harness.bridge.linkDead(); });
        CHECK(!harness.bridge.hasPrompt());
        CHECK(harness.bridge.promptSecret());
    }
}

TEST_CASE("a question becomes the pending prompt and a message does not", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    // Through authenticate() and not through the link directly, and the buffered
    // first prompt is spent to get here: onPrompt drops a prompt that arrives
    // while `authenticating` is false, so a conversation opened behind the
    // object's back is a state the greeter cannot actually be in and every
    // prompt below would be dropped. promptAfterBuffer leaves exactly what these
    // sections need — a live conversation, an empty buffer, and no pending
    // prompt — so the next prompt is shown rather than answered.
    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
    harness.spy.messages.clear();

    SECTION("a secret prompt sets all three prompt properties") {
        harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
        pumpUntil(harness, "the prompt", [&harness] { return harness.bridge.hasPrompt(); });
        CHECK(harness.bridge.promptText() == QStringLiteral("Password: "));
        CHECK(harness.bridge.promptSecret());
        CHECK(harness.spy.messages.empty());
    }

    SECTION("an empty prompt is still a prompt") {
        // hasPrompt is its own boolean and not promptText !== "". A PAM module
        // that legitimately sends an empty prompt is otherwise
        // indistinguishable from no prompt at all, and a theme deriving one
        // from the other would show the user a field with nothing above it and
        // no way to tell whether anything is waiting.
        harness.server.sendPrompt("", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
        pumpUntil(harness, "the empty prompt", [&harness] { return harness.bridge.hasPrompt(); });
        CHECK(harness.bridge.promptText().isEmpty());
        CHECK(harness.bridge.hasPrompt());
    }

    SECTION("a visible prompt is a question that must not be masked") {
        harness.server.sendPrompt("One-time code: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
        pumpUntil(harness, "the prompt", [&harness] { return harness.bridge.hasPrompt(); });
        CHECK(harness.bridge.promptText() == QStringLiteral("One-time code: "));
        CHECK(!harness.bridge.promptSecret());
    }

    SECTION("an info prompt is a message with its own kind and no question") {
        harness.server.sendPrompt("Login attempts are logged.",
                                  WDM_GREETER_V1_PROMPT_STYLE_INFO);
        pumpUntil(harness, "the info message",
                  [&harness] { return !harness.spy.messages.empty(); });
        CHECK(harness.spy.messages[0].first == QStringLiteral("Login attempts are logged."));
        CHECK(harness.spy.messages[0].second == QStringLiteral("info"));
        // A greeter that treated this as pending would hold an id wdm rejects,
        // and would wait forever for an answer to a statement.
        CHECK(!harness.bridge.hasPrompt());
    }

    SECTION("an error prompt keeps its own kind rather than being flattened") {
        harness.server.sendPrompt("Account locked for 10 minutes.",
                                  WDM_GREETER_V1_PROMPT_STYLE_ERROR);
        pumpUntil(harness, "the error message",
                  [&harness] { return !harness.spy.messages.empty(); });
        CHECK(harness.spy.messages[0].second == QStringLiteral("error"));
        CHECK(!harness.bridge.hasPrompt());
        // The two severities stay apart all the way to the theme: that is what
        // lets a theme show a lockout in red with the minutes beside it in
        // grey, and collapsing them here would take that choice away from every
        // theme at once.
        CHECK(harness.spy.of(QStringLiteral("error")).size() == 1);
        CHECK(harness.spy.of(QStringLiteral("info")).isEmpty());
    }
}

TEST_CASE("each PAM message arrives as its own signal rather than joined", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    // A live conversation with the buffer already spent — see the case above for
    // why the conversation is opened through the object rather than the link.
    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
    harness.spy.messages.clear();

    // The pair PAM actually sends: the reason as an error, the detail as info.
    harness.server.sendPrompt("The account is locked.", WDM_GREETER_V1_PROMPT_STYLE_ERROR);
    harness.server.sendPrompt("10 minutes left to unlock.", WDM_GREETER_V1_PROMPT_STYLE_INFO);
    pumpUntil(harness, "both messages", [&harness] { return harness.spy.messages.size() == 2; });

    CHECK(harness.spy.of(QStringLiteral("error"))
          == QStringList{QStringLiteral("The account is locked.")});
    CHECK(harness.spy.of(QStringLiteral("info"))
          == QStringList{QStringLiteral("10 minutes left to unlock.")});
}

// --------------------------------------------------------------------------
// The buffered answer
// --------------------------------------------------------------------------

TEST_CASE("the buffered answer is spent on a secret prompt", "[wdm]") {
    // The reason the buffer exists at all: nothing arms PAM until the user
    // submits, so the first thing they type arrives before PAM has asked for
    // it. Without this they would type a password, watch the field clear, and
    // have to type it again.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    CHECK(harness.bridge.authenticating());
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });
    CHECK(harness.server.createSessionCalls()[0] == "alice");

    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the buffered answer to reach the compositor",
              [&harness] { return !harness.server.respondCalls().empty(); });

    CHECK(harness.server.respondCalls()[0].second == "hunter2");
    // Answered without ever being shown: from the user's side they typed a
    // password, pressed Enter, and it was checked.
    CHECK(!harness.bridge.hasPrompt());
}

TEST_CASE("the buffered answer is dropped on a visible prompt", "[wdm]") {
    // The single most important assertion in this file.
    //
    // The buffer holds what was typed into a field the theme renders masked,
    // because promptSecret starts true — so it is a password. wdm forwards
    // PAM_PROMPT_ECHO_ON as a non-secret prompt, and a stack whose first
    // answerable question is echo-on — pam_oath's token, a username re-prompt —
    // would otherwise be handed that password as the answer to a question it
    // was never typed for, where the stack's own failure logging records it in
    // the clear.
    //
    // One linear run and no sections: the visible prompt has to come first and
    // the secret one after it, and a section would restart the case.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });

    harness.server.sendPrompt("Username: ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
    pumpUntil(harness, "the visible prompt", [&harness] { return harness.bridge.hasPrompt(); });

    // Shown, not answered.
    CHECK(harness.bridge.promptText() == QStringLiteral("Username: "));
    CHECK(!harness.bridge.promptSecret());
    drain(harness);
    CHECK(harness.server.respondCalls().empty());

    // And dropped rather than held: it was typed for the question that did not
    // arrive, so a masked question that comes later must be answered by the
    // user. A buffer kept across this would be the same password sent somewhere
    // it was never typed for, one prompt further on.
    harness.bridge.respond(QStringLiteral("alice"));
    waitFor("the visible prompt's answer",
            [&harness] { return harness.server.respondCalls().size() == 1; });
    CHECK(harness.server.respondCalls()[0].second == "alice");

    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the secret prompt", [&harness] { return harness.bridge.hasPrompt(); });
    drain(harness);
    CHECK(harness.bridge.hasPrompt());
    CHECK(harness.bridge.promptSecret());
    CHECK(harness.server.respondCalls().size() == 1);
    // Nothing beginning with the buffered password was ever sent.
    for (const auto &call : harness.server.respondCalls()) {
        CHECK(call.second != "hunter2");
    }
}

TEST_CASE("the buffer does not survive the conversation it was typed for", "[wdm]") {
    // Cleared on every path that ends a conversation. This checks the failure
    // path, which is the one a user reaches by mistyping a password: a buffer
    // that survived would be spent on the *next* conversation's first prompt,
    // which the user never saw and never typed for.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });
    harness.server.sendAuthFailed("Authentication failure");
    pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter3")));
    waitFor("the second create_session",
            [&harness] { return harness.server.createSessionCalls().size() == 2; });
    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the second attempt's answer",
              [&harness] { return !harness.server.respondCalls().empty(); });

    // The second attempt's own answer, not the first attempt's.
    CHECK(harness.server.respondCalls().size() == 1);
    CHECK(harness.server.respondCalls()[0].second == "hunter3");
}

TEST_CASE("an empty first answer opens no conversation", "[wdm]") {
    // Submitting an empty field would run the whole PAM stack against an empty
    // password, fail, and be charged to the account by pam_faillock — so three
    // stray presses of Enter at an unattended screen would lock it.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    CHECK(!harness.bridge.authenticate(QStringLiteral("alice"), QString()));
    CHECK(!harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("")));

    drain(harness);
    CHECK(harness.server.createSessionCalls().empty());
    CHECK(!harness.bridge.authenticating());
    // And it is not a theme bug: a user brushing the keyboard must not produce
    // a QML error, only a refusal the theme turns into "Enter your password".
    CHECK(!tookError(harness));

    // Only the *first* answer is guarded. Once a conversation is underway an
    // empty answer is a legitimate choice — a stack asking for an optional
    // token is entitled to be answered with nothing — and respond() sends it.
    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });
    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "the buffered answer",
              [&harness] { return harness.server.respondCalls().size() == 1; });
    harness.server.sendPrompt("Token (optional): ", WDM_GREETER_V1_PROMPT_STYLE_VISIBLE);
    pumpUntil(harness, "the optional token prompt",
              [&harness] { return harness.bridge.hasPrompt(); });

    harness.bridge.respond(QString());
    waitFor("the empty answer to reach the compositor",
            [&harness] { return harness.server.respondCalls().size() == 2; });
    CHECK(harness.server.respondCalls()[1].second.empty());
    CHECK(!tookError(harness));
}

// --------------------------------------------------------------------------
// The end of a conversation
// --------------------------------------------------------------------------

TEST_CASE("auth_ok ends the conversation exactly once", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
    harness.server.sendAuthOk();
    pumpUntil(harness, "auth_ok", [&harness] { return harness.bridge.authenticated(); });
    drain(harness);

    CHECK(harness.bridge.authenticated());
    CHECK(harness.bridge.conversationOver());
    CHECK(!harness.bridge.authenticating());
    CHECK(!harness.bridge.hasPrompt());
    // Exactly once: a theme starts the session from this signal, and a second
    // one would send a second start_session.
    CHECK(harness.spy.completions == 1);

    // And the session can now be launched, which is the whole point of the
    // property the theme guards on.
    harness.bridge.startSession(QStringLiteral("plasma.desktop"));
    waitFor("start_session", [&harness] { return harness.server.startSessionCall().has_value(); });
    CHECK(harness.server.startSessionCall()->sessionId == "plasma.desktop");
    // No environment: locale and keyboard come from wdm's own configuration.
    CHECK(harness.server.startSessionCall()->envBytes == 0u);
    CHECK(!tookError(harness));
}

TEST_CASE("a failure is reported and nothing retries by itself", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
    harness.server.sendAuthFailed("Authentication failure");
    pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });
    drain(harness, 120ms);

    CHECK(!harness.bridge.authenticated());
    CHECK(harness.bridge.conversationOver());
    CHECK(!harness.bridge.authenticating());
    // The verdict arrives through `message`, with kind "error", after every
    // message belonging to the attempt — there is no separate channel for it.
    CHECK(harness.spy.of(QStringLiteral("error"))
          == QStringList{QStringLiteral("Authentication failure")});

    // Nothing retried. A greeter that restarted here would re-arm the timeout
    // it may have just been ended by, one pam_faillock entry per turn, until
    // the account locked with nobody at the keyboard.
    CHECK(harness.server.createSessionCalls().size() == 1u);
    CHECK(harness.spy.completions == 1);
}

TEST_CASE("cancel ends the conversation without pretending it was decided", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });

    harness.bridge.cancel();
    waitFor("cancel", [&harness] { return harness.server.cancelCalls() == 1; });

    CHECK(!harness.bridge.authenticating());
    CHECK(!harness.bridge.hasPrompt());
    // Nothing was decided, so a theme that shows "Press Enter to try again" off
    // conversationOver must not say it to a user who has just picked a
    // different account and tried nothing.
    CHECK(!harness.bridge.conversationOver());
    // The theme asked for this and knows it happened; wdm sends neither auth_ok
    // nor auth_failed after a cancel, so there is nothing to complete.
    CHECK(harness.spy.completions == 0);

    // The buffer went with it. A prompt arriving late — the request crossed the
    // cancel on the wire — must not be answered with a password typed for a
    // conversation the user abandoned.
    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    drain(harness, 120ms);
    CHECK(harness.server.respondCalls().empty());

    // And it must not be *shown* either, which is the half nothing was asserting
    // and which the object used to get wrong. Without the authenticating_ guard
    // in onPrompt the late prompt set hasPrompt true with authenticating false;
    // respond() checks only hasPrompt, so the theme's next submit would send a
    // respond for a conversation wdm has already cancelled — no_auth or
    // stale_prompt, and a protocol error kills the greeter in the middle of a
    // login. Nothing on the wire was never the whole requirement; nothing on
    // screen offering to put something there is the other half.
    CHECK(!harness.bridge.hasPrompt());
    CHECK(harness.bridge.promptText().isEmpty());
    // Re-armed masked by endConversation, so the field the user types into next
    // is masked from the first frame of the next attempt too.
    CHECK(harness.bridge.promptSecret());
    // Still not a completion: dropping a stale prompt is not a verdict.
    CHECK(harness.spy.completions == 0);
    CHECK(!harness.bridge.authenticating());
    CHECK(!tookError(harness));
}

TEST_CASE("a prompt arriving after the verdict is dropped rather than re-arming the UI",
          "[wdm]") {
    // The same defect as the cancel case, reached the other way: wdm decided the
    // attempt and the theme was told, and then one more prompt belonging to that
    // attempt arrives. There is nothing left to answer it with — wdm has ended
    // the conversation at its end — so a greeter that showed it would be
    // offering the user a field whose submit is a protocol error.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
    harness.server.sendAuthFailed("Authentication failure");
    pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });
    REQUIRE(!harness.bridge.authenticating());

    const std::size_t answered = harness.server.respondCalls().size();
    harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    drain(harness, 120ms);

    CHECK(!harness.bridge.hasPrompt());
    CHECK(harness.bridge.promptText().isEmpty());
    CHECK(harness.bridge.promptSecret());
    // Nothing was sent for it, and no second verdict was invented for it.
    CHECK(harness.server.respondCalls().size() == answered);
    CHECK(harness.spy.completions == 1);
    CHECK(!tookError(harness));
}

TEST_CASE("a verdict arriving after the conversation was decided is dropped", "[wdm]") {
    // The prompt case above, for the other two conversation events. A second
    // verdict is not hypothetical — wdm can emit one for an attempt it was
    // already tearing down — and the object used to take it: onAuthOk and
    // onAuthFailed ran endConversation() unconditionally, which clears
    // `authenticating` and the buffered answer *while wdm may still have a live
    // conversation at its end*. The next authenticate() then passes every local
    // guard and sends create_session into auth_in_progress, which is a protocol
    // error and a greeter killed mid-login.
    //
    // It also emitted authenticationComplete a second time, which wdm.h makes
    // contract: exactly once per conversation.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));

    SECTION("an auth_ok after an auth_failed does not turn a refusal into a login") {
        harness.server.sendAuthFailed("Authentication failure");
        pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });
        REQUIRE(!harness.bridge.authenticated());

        harness.server.sendAuthOk();
        drain(harness, 120ms);

        // The one that would be a security defect rather than a nuisance: a
        // theme reads `authenticated` to decide whether to call startSession,
        // and this is a login granted by an event that arrived after the
        // password was refused.
        CHECK(!harness.bridge.authenticated());
        CHECK(harness.spy.completions == 1);
    }

    SECTION("a second auth_failed after an auth_ok neither un-authenticates nor speaks") {
        harness.server.sendAuthOk();
        pumpUntil(harness, "the verdict", [&harness] { return harness.spy.completions == 1; });
        REQUIRE(harness.bridge.authenticated());
        const int errors = harness.spy.of(QStringLiteral("error")).size();

        harness.server.sendAuthFailed("Authentication failure");
        drain(harness, 120ms);

        CHECK(harness.bridge.authenticated());
        CHECK(harness.spy.completions == 1);
        // And no "Authentication failure" printed under a theme that is already
        // showing "Starting session…".
        CHECK(harness.spy.of(QStringLiteral("error")).size() == errors);
    }

    CHECK(!harness.bridge.authenticating());
    CHECK(!tookError(harness));
}

TEST_CASE("a verdict crossing a cancel belongs to the conversation that was cancelled",
          "[wdm]") {
    // The same two events, reached the way a user reaches them: the verdict was
    // already on the wire when the theme cancelled, and the user has since
    // started a second conversation. `authenticating` is true again by the time
    // the verdict is dispatched, so no guard phrased as "is a conversation
    // live" can catch it — the question is *which* conversation, and only the
    // barrier Link::cancel installs can answer that.
    //
    // Without it, an auth_ok for the account the user backed out of is applied
    // to the account they switched to: `authenticated` becomes true for a
    // password wdm never checked, and a theme's onAuthenticationComplete calls
    // startSession.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session for alice",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });

    SECTION("auth_ok") {
        harness.server.sendAuthOk();
    }
    SECTION("auth_failed") {
        harness.server.sendAuthFailed("Authentication failure");
    }

    // The verdict is on the wire and nothing has dispatched it: the pump has
    // not run, and neither cancel() nor authenticate() reads the socket.
    harness.bridge.cancel();
    CHECK(harness.link->discardingUntilBarrier());
    REQUIRE(harness.bridge.authenticate(QStringLiteral("bob"), QStringLiteral("hunter2")));
    waitFor("create_session for bob",
            [&harness] { return harness.server.createSessionCalls().size() == 2; });

    drain(harness, 120ms);

    CHECK(!harness.link->discardingUntilBarrier());
    CHECK(!harness.bridge.authenticated());
    CHECK(!harness.bridge.conversationOver());
    CHECK(harness.spy.completions == 0);
    // Bob's conversation is untouched: still live, still waiting for its own
    // first prompt.
    CHECK(harness.bridge.authenticating());
    CHECK(!harness.bridge.hasPrompt());
    CHECK(!tookError(harness));
}

TEST_CASE("an answer buffered for one conversation cannot be sent under another's prompt id",
          "[wdm]") {
    // The race the barrier in Link::cancel exists for, driven exactly as it
    // happens: the default theme's user drop-down calls cancel() from
    // onActivated, and the user presses Enter before the next 16 ms pump tick.
    // Alice's prompt is on the wire the whole time.
    //
    // Without the barrier, Link::handlePrompt puts alice's prompt back into
    // `pending_` after the cancel dropped it, Wdm::onPrompt sees a live
    // conversation — bob's — and spends bob's buffered password on it, and
    // Link accepts the respond because the id matches what it is holding. wdm
    // answers that with stale_prompt, which kills the greeter; and the request
    // that killed it carried bob's password under alice's conversation.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"),
                                        QStringLiteral("alices-password")));
    waitFor("create_session for alice",
            [&harness] { return harness.server.createSessionCalls().size() == 1; });

    const std::uint32_t alicesPrompt =
        harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    REQUIRE(alicesPrompt != 0u);

    harness.bridge.cancel();
    REQUIRE(harness.bridge.authenticate(QStringLiteral("bob"), QStringLiteral("bobs-password")));
    waitFor("create_session for bob",
            [&harness] { return harness.server.createSessionCalls().size() == 2; });

    drain(harness, 120ms);

    // The whole assertion: nothing at all went out under alice's id, and in
    // particular nothing carrying bob's password.
    CHECK(harness.server.respondCalls().empty());
    // And bob's answer was not spent on it either — it is still held for the
    // prompt bob's own conversation has yet to send.
    CHECK(!harness.bridge.hasPrompt());
    CHECK(harness.bridge.authenticating());

    // Which it then is, under bob's id.
    const std::uint32_t bobsPrompt =
        harness.server.sendPrompt("Password: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
    pumpUntil(harness, "bob's buffered answer",
              [&harness] { return harness.server.respondCalls().size() == 1; });
    REQUIRE(harness.server.respondCalls().size() == 1u);
    CHECK(bobsPrompt != alicesPrompt);
    CHECK(harness.server.respondCalls().front().first == bobsPrompt);
    CHECK(harness.server.respondCalls().front().second == "bobs-password");
    CHECK(!tookError(harness));
}

// --------------------------------------------------------------------------
// Preconditions
// --------------------------------------------------------------------------

TEST_CASE("a method called out of order raises rather than reaching wdm", "[wdm]") {
    // Each of these is a request wdm answers with a protocol error —
    // auth_in_progress, no_auth, stale_prompt, invalid_session — and a protocol
    // error kills the greeter mid-login and leaves the user looking at whatever
    // the compositor draws when nothing is on screen. A theme should never be
    // able to reach one, so the refusal happens here and is reported as a QML
    // error with a file and a line.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    SECTION("respond with no prompt pending") {
        harness.bridge.respond(QStringLiteral("hunter2"));
        CHECK(tookError(harness));
        drain(harness);
        CHECK(harness.server.respondCalls().empty());
    }

    SECTION("cancel with no conversation in progress") {
        harness.bridge.cancel();
        CHECK(tookError(harness));
        drain(harness);
        CHECK(harness.server.cancelCalls() == 0);
    }

    SECTION("startSession before authenticating") {
        harness.bridge.startSession(QStringLiteral("plasma.desktop"));
        CHECK(tookError(harness));
        drain(harness);
        CHECK(!harness.server.startSessionCall().has_value());
    }

    SECTION("authenticate naming an account wdm never advertised") {
        CHECK(!harness.bridge.authenticate(QStringLiteral("mallory"), QStringLiteral("hunter2")));
        CHECK(tookError(harness));
        drain(harness);
        CHECK(harness.server.createSessionCalls().empty());
    }

    SECTION("authenticate while a conversation is already in progress") {
        REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        waitFor("create_session",
                [&harness] { return harness.server.createSessionCalls().size() == 1; });
        CHECK(!tookError(harness));

        CHECK(!harness.bridge.authenticate(QStringLiteral("bob"), QStringLiteral("hunter2")));
        CHECK(tookError(harness));
        drain(harness);
        // The live conversation is untouched: a second attempt would burn a
        // rate-limit slot and make PAM do every attempt twice.
        CHECK(harness.server.createSessionCalls().size() == 1u);
        CHECK(harness.bridge.authenticating());
    }

    SECTION("authenticate after authentication has already succeeded") {
        promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
        harness.server.sendAuthOk();
        pumpUntil(harness, "auth_ok", [&harness] { return harness.bridge.authenticated(); });

        CHECK(!harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        CHECK(tookError(harness));
        drain(harness);
        CHECK(harness.server.createSessionCalls().size() == 1u);
    }

    SECTION("startSession naming a session that is not installed") {
        promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
        harness.server.sendAuthOk();
        pumpUntil(harness, "auth_ok", [&harness] { return harness.bridge.authenticated(); });

        harness.bridge.startSession(QStringLiteral("gone.desktop"));
        CHECK(tookError(harness));
        drain(harness);
        // The last possible moment for this to be caught: the password has
        // already been accepted, and there would be nothing left on screen to
        // say what happened.
        CHECK(!harness.server.startSessionCall().has_value());
    }
}

// --------------------------------------------------------------------------
// The link dying
// --------------------------------------------------------------------------

TEST_CASE("linkDead latches and never clears", "[wdm]") {
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
    waitFor("create_session", [&harness] { return harness.server.createSessionCalls().size() == 1; });

    // wdm dropping the greeter, which it does on a protocol error and which is
    // indistinguishable here from the compositor dying.
    harness.server.hangUp();
    pumpUntil(harness, "the link death", [&harness] { return harness.bridge.linkDead(); });

    SECTION("the conversation it interrupted is ended, once") {
        CHECK(!harness.bridge.authenticating());
        CHECK(harness.bridge.conversationOver());
        CHECK(!harness.bridge.hasPrompt());
        // Without this a theme sits on `authenticating` forever and never
        // reaches the branch where it disables its controls.
        CHECK(harness.spy.completions == 1);
        // With a reason, so the theme has something to show beside its own
        // "switch to a text console" line.
        CHECK(!harness.spy.of(QStringLiteral("error")).isEmpty());
    }

    SECTION("no amount of pumping un-latches it") {
        for (int i = 0; i < 5; ++i) {
            harness.bridge.pump();
            LinkPump::readAndDispatch(*harness.link, 5);
        }
        CHECK(harness.bridge.linkDead());
        // Reported once. A greeter that reported on every tick would push the
        // one explanation the user has off the screen sixty times a second.
        CHECK(harness.spy.of(QStringLiteral("error")).size() == 1);
        CHECK(harness.spy.completions == 1);
    }

    SECTION("authenticate and cancel afterwards send nothing and do not raise") {
        // Not raises: the link dying is a fact about the machine rather than a
        // bug in the theme, and unwinding the theme's handler would abandon
        // whatever it was doing to explain the silence.
        //
        // These two are the pair this state actually arms. A conversation was
        // live when the link died, so cancel()'s precondition was met a moment
        // ago and authenticate()'s is met now; both would reach wdm if the
        // linkDead guard were the only thing missing. respond() and
        // startSession() are not armed here at all, which is why they have a
        // case of their own below rather than a line in this one.
        CHECK(!harness.bridge.authenticate(QStringLiteral("alice"), QStringLiteral("hunter2")));
        harness.bridge.cancel();
        CHECK(!tookError(harness));
        CHECK(harness.bridge.linkDead());
        CHECK(harness.server.createSessionCalls().size() == 1u);
    }
}

TEST_CASE("respond and startSession refuse from the state that would have sent", "[wdm]") {
    // The other half of the case above, and the reason it is a case rather than
    // two more lines in that one: asserting that respond() and startSession()
    // send nothing after a hang-up proves nothing at all unless they were in a
    // position to send. With no prompt ever delivered and authentication never
    // granted, both refuse on their *ordinary* preconditions and the assertion
    // holds with the linkDead guards deleted.
    //
    // The two states cannot coexist — endConversation clears the prompt when
    // auth_ok arrives, and a prompt after a verdict is dropped — so they are
    // reached separately.
    Harness harness;
    harness.withOrdinaryEnumerate();
    harness.connect();

    SECTION("respond, with a question actually on screen") {
        promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
        // A second question: the buffer is spent, so this one reaches the
        // theme and hasPrompt is true.
        harness.server.sendPrompt("Second factor: ", WDM_GREETER_V1_PROMPT_STYLE_SECRET);
        pumpUntil(harness, "the second prompt", [&harness] { return harness.bridge.hasPrompt(); });
        const std::size_t answered = harness.server.respondCalls().size();

        harness.server.hangUp();
        pumpUntil(harness, "the link death", [&harness] { return harness.bridge.linkDead(); });

        harness.bridge.respond(QStringLiteral("123456"));
        drain(harness);
        CHECK(harness.server.respondCalls().size() == answered);
        // The distinguishing assertion, and the only one that separates the
        // linkDead guard from Link's own dead_ check underneath it: with the
        // guard gone this falls through to the hasPrompt test — which
        // endConversation cleared on the way past — and reports a theme bug for
        // a machine failure, unwinding a submit handler that was about to
        // explain the silence.
        CHECK(!tookError(harness));
    }

    SECTION("startSession, with authentication actually granted") {
        promptAfterBuffer(harness, QStringLiteral("alice"), QStringLiteral("hunter2"));
        harness.server.sendAuthOk();
        pumpUntil(harness, "auth_ok", [&harness] { return harness.bridge.authenticated(); });
        REQUIRE(harness.bridge.authenticated());

        harness.server.hangUp();
        pumpUntil(harness, "the link death", [&harness] { return harness.bridge.linkDead(); });
        // Nothing clears it: a theme reading `authenticated` after the link
        // died still sees true, which is exactly why this call has to be
        // stopped by something other than its own precondition.
        REQUIRE(harness.bridge.authenticated());

        harness.bridge.startSession(QStringLiteral("plasma.desktop"));
        drain(harness);
        CHECK(!harness.server.startSessionCall().has_value());
        CHECK(!tookError(harness));
    }
}

// --------------------------------------------------------------------------
// The default theme
// --------------------------------------------------------------------------

TEST_CASE("the default theme spells the shared messages the way the other greeters do",
          "[wdm][theme]") {
    // A drift check across a language boundary, on the model of tst_exitcode
    // and of wdm-gtk-greeter's own
    // `the_other_greeters_spell_these_messages_the_same_way`. Nothing compiles
    // QML against Rust, so the only thing that can notice one of these being
    // reworded alone is a test that reads both files.
    //
    // A user meeting two wdm machines with two greeters on them must not read
    // two different sentences for one condition. What belongs here is
    // everything the greeter says in words of its own rather than PAM's.
    const std::string theme = readFile(WDM_DEFAULT_THEME_QML_PATH);
    const std::string gtk = readFile(WDM_GTK_GREETER_UI_RS_PATH);

    for (const char *line : {
             "No users available to log in",
             "No sessions installed",
             "Enter your password",
             // The idle line, and the most-seen string on the login screen:
             // what stands above the field between the greeter appearing and
             // the user typing. Nothing else pinned it, so the one word a user
             // reads every single day was the one word the two greeters were
             // free to disagree about.
             "Password",
             "Waiting…",
             "Checking…",
             "Starting session…",
             "Press Enter to try again",
             "Connection to wdm lost — switch to a text console",
         }) {
        INFO("shared wording: " << line);
        CHECK(theme.find(line) != std::string::npos);
        CHECK(gtk.find(line) != std::string::npos);
    }
}

TEST_CASE("the default theme arms PAM from the submit handler and from nowhere else",
          "[wdm][theme]") {
    // The requirement that a real account was locked out for. A theme that
    // calls authenticate() from Component.onCompleted, or from the user
    // drop-down's currentIndexChanged, spends a pam_faillock attempt every time
    // the login screen is left alone — and a conversation cannot be ended
    // without failing it.
    //
    // Checked by reading the theme, because the alternative is a QML engine, a
    // window and a compositor. It is a coarse check and it is worth having: it
    // fails the moment a second authenticate() call appears anywhere in the
    // file, which is exactly the edit that reintroduces the defect.
    const std::string theme = readFile(WDM_DEFAULT_THEME_QML_PATH);

    CHECK(countOutsideComments(theme, "wdm.authenticate(") == 1);

    // And the user drop-down reacts to a choice the user made rather than to
    // the model being populated, which happens before anyone has touched the
    // machine. onCurrentIndexChanged would fire during that population, on a
    // login screen nobody is at.
    CHECK(countOutsideComments(theme, "onActivated") == 1);
    CHECK(countOutsideComments(theme, "onCurrentIndexChanged") == 0);
}

// --------------------------------------------------------------------------
// A version 1 wdm
// --------------------------------------------------------------------------

TEST_CASE("under a version 1 wdm there is no defaultSession, only each user's history",
          "[wdm]") {
    // The Harness has taken an advertised version since it was written and
    // every case passed 2 by omission, so the v1 path — no default_session
    // event, `defaultSession` empty, the machine-wide default arriving inside
    // last_session instead — was never driven. That is the branch a theme meets
    // on a machine running an older wdm, and the one where a theme that trusts
    // `defaultSession` alone shows no preselected session at all.
    Harness harness(1);
    harness.withOrdinaryEnumerate();
    harness.connect();

    REQUIRE(harness.server.boundVersion() == 1u);
    // Bound as min(advertised, kInterfaceVersion), so a version 2 greeter under
    // a version 1 wdm speaks version 1 rather than failing to connect.
    CHECK(harness.link->version() == 1u);

    // Never sent, so never set — and a theme reading only this would preselect
    // nothing.
    CHECK(harness.bridge.defaultSession().isEmpty());

    // What it has instead: the compositor substitutes the configured default
    // into the last_session of a user with no history, which is a substitution
    // the theme cannot detect and does not need to. alice has her own history
    // and keeps it; bob has none and gets the default.
    auto *users = qobject_cast<UserModel *>(harness.bridge.users());
    REQUIRE(users != nullptr);
    REQUIRE(users->rowCount() == 2);
    CHECK(users->get(1).value(QStringLiteral("name")).toString() == QStringLiteral("bob"));
    CHECK(users->get(0).value(QStringLiteral("lastSession")).toString()
          == QStringLiteral("plasmax11.desktop"));
    CHECK(users->get(1).value(QStringLiteral("lastSession")).toString()
          == QStringLiteral("plasma.desktop"));
}

// --------------------------------------------------------------------------
// Logging
// --------------------------------------------------------------------------
//
// WDM_GREETER_LOG and the exit-69 rule are contract — the words are the three
// Rust greeters' words and `plasma-themes.md` publishes the rule to theme
// authors — and until logging.h existed they were an anonymous namespace in the
// executable's only translation unit, which nothing could reach.

TEST_CASE("severity ordering is not QtMsgType's numeric order", "[wdm][logging]") {
    // The reason severityRank exists at all. QtInfoMsg was added to the enum
    // last and has the highest value of the four ordinary levels, so a handler
    // that compared QtMsgType directly would make WDM_GREETER_LOG=info the
    // quietest setting there is — an administrator asking for more output and
    // getting silence.
    STATIC_REQUIRE(static_cast<int>(QtInfoMsg) > static_cast<int>(QtWarningMsg));

    using wdm::logging::severityRank;
    CHECK(severityRank(QtDebugMsg) < severityRank(QtInfoMsg));
    CHECK(severityRank(QtInfoMsg) < severityRank(QtWarningMsg));
    CHECK(severityRank(QtWarningMsg) < severityRank(QtCriticalMsg));
    CHECK(severityRank(QtCriticalMsg) < severityRank(QtFatalMsg));
}

TEST_CASE("WDM_GREETER_LOG takes env_logger's words", "[wdm][logging]") {
    // An administrator copies this setting between the four greeters, so a Qt
    // greeter that spelled it differently would be one they have to look up
    // separately at the moment they are debugging a login screen.
    struct Case {
        const char *value;
        QtMsgType threshold;
    };

    const auto restore = qgetenv("WDM_GREETER_LOG");

    for (const Case &entry : {
             Case{"trace", QtDebugMsg},
             Case{"debug", QtDebugMsg},
             Case{"info", QtInfoMsg},
             Case{"warn", QtWarningMsg},
             Case{"error", QtCriticalMsg},
             // Above every real severity, so nothing is printed. A fatal still
             // terminates: "off" is a request for quiet, not a request to keep
             // running after an unrecoverable error.
             Case{"off", QtFatalMsg},
             // Neither a word this greeter knows nor an error: warnings are
             // what a login screen's failures arrive as, and refusing to start
             // over a misspelled log level would be a blank screen for a typo.
             Case{"verbose", QtWarningMsg},
             Case{"", QtWarningMsg},
             // Case-insensitive, because env_logger is.
             Case{"DEBUG", QtDebugMsg},
         }) {
        INFO("WDM_GREETER_LOG=" << entry.value);
        qputenv("WDM_GREETER_LOG", entry.value);
        CHECK(wdm::logging::thresholdFromEnv() == entry.threshold);
    }

    // Unset is not the same input as empty, and both must land on warnings.
    qunsetenv("WDM_GREETER_LOG");
    CHECK(wdm::logging::thresholdFromEnv() == QtWarningMsg);

    qputenv("WDM_GREETER_LOG", restore);
}

// --------------------------------------------------------------------------
// The shape QML sees
// --------------------------------------------------------------------------

TEST_CASE("authenticate is offered to QML in one arity only", "[wdm]") {
    // moc emits one QMetaMethod per arity, so a defaulted second argument would
    // make `wdm.authenticate(username)` legal QML — and that is exactly the call
    // a theme author ports from the webkit greeter, whose method genuinely takes
    // one argument. It would land on authenticate()'s empty-answer branch and
    // return false with no raise, no journal line and nothing on the wire:
    // Enter does nothing, forever, silently, and the theme has no way to find
    // out why. Without the default the same call fails QML overload resolution
    // with a file and a line, which is how every other misuse of this object is
    // reported.
    //
    // Asserted against the metaobject because that is where the defect lives.
    // The signature in the header is a C++ fact; what QML can call is a moc
    // fact, and the two differ by exactly the default argument.
    const QMetaObject &meta = wdm::Wdm::staticMetaObject;
    CHECK(meta.indexOfMethod("authenticate(QString,QString)") != -1);
    CHECK(meta.indexOfMethod("authenticate(QString)") == -1);

    int arities = 0;
    for (int i = meta.methodOffset(); i < meta.methodCount(); ++i) {
        if (meta.method(i).name() == QByteArrayLiteral("authenticate")) {
            ++arities;
        }
    }
    CHECK(arities == 1);
}
