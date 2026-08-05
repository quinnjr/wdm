// The object a QML theme binds against, and the whole of the theme contract.
//
// One QObject, set as the context property `wdm` on the engine's root context.
// Everything a theme can read is a Q_PROPERTY with a NOTIFY, everything it can
// do is a Q_INVOKABLE, and everything it is told is a signal. There is no other
// channel — no global functions, no injected script, no second object — because
// the contract is versioned and documented on the same terms as the webkit
// greeter's `window.wdm`, and a second way in is a second thing to version.
//
// This is the Qt half of the split described in the design: Link owns the
// Wayland connection and knows no Qt, this owns the Qt objects and never sees a
// wl_proxy. They meet at LinkObserver, which is plain structs in one direction
// and five request methods in the other. That is what lets tst_wdm drive the
// whole state machine with a QCoreApplication and a socketpair, with no
// display, no window and no QPA platform plugin.
//
// The state machine here is not a translation of the protocol. It is the same
// set of decisions wdm-gtk-greeter's ui.rs and the default webkit theme's
// theme.js already make, arrived at the hard way, and three of them are
// security requirements rather than preferences:
//
//   - nothing arms PAM until the user submits (see authenticate);
//   - an empty first answer opens no conversation (see authenticate);
//   - the buffered answer is spent only on a secret prompt (see onPrompt).
//
// The third of those is why the buffer lives in this class and is never visible
// to QML. A theme cannot get a rule wrong that it cannot reach.

#pragma once

#include <cstdint>
#include <optional>

#include <QObject>
#include <QString>

#include "link.h"
#include "models.h"

class QQmlEngine;
class QTimer;

namespace wdm {

class Wdm : public QObject, public LinkObserver {
    Q_OBJECT

    // CONSTANT, and it is not laziness: the models are populated during the
    // enumerate phase, which Link::connect() completes before the engine is
    // created, and wdm never sends another user or session. The pointer a
    // theme binds is the pointer for the life of the process; the models
    // themselves still notify their own row insertions.
    Q_PROPERTY(QAbstractListModel *users READ users CONSTANT)
    Q_PROPERTY(QAbstractListModel *sessions READ sessions CONSTANT)

    // One notify for the three prompt properties, because a prompt arriving or
    // being answered changes all three at once and there is no state in which
    // one of them has moved and the others have not. Three separate signals
    // would be three chances for a theme to bind to the one that does not fire.
    Q_PROPERTY(bool hasPrompt READ hasPrompt NOTIFY promptChanged)
    Q_PROPERTY(QString promptText READ promptText NOTIFY promptChanged)
    Q_PROPERTY(bool promptSecret READ promptSecret NOTIFY promptChanged)

    Q_PROPERTY(bool authenticating READ authenticating NOTIFY authenticatingChanged)
    Q_PROPERTY(bool authenticated READ authenticated NOTIFY authenticatedChanged)
    Q_PROPERTY(bool conversationOver READ conversationOver NOTIFY conversationOverChanged)
    Q_PROPERTY(QString defaultSession READ defaultSession NOTIFY defaultSessionChanged)
    Q_PROPERTY(QString lastError READ lastError NOTIFY lastErrorChanged)
    Q_PROPERTY(bool linkDead READ linkDead NOTIFY linkDeadChanged)

public:
    /// How often dispatch() is called once startPumping() has been called.
    ///
    /// ponytail: polled rather than read. Ceiling: up to 16 ms between wdm
    /// sending a prompt and the theme showing it, which is not observable by a
    /// human typing a password. Upgrade: a QSocketNotifier on the connection fd
    /// cooperating with Qt through wl_display_prepare_read — rejected because
    /// two readers that get that handshake wrong live-lock the login screen.
    /// The reasoning is Link::dispatch()'s and is not repeated here; this
    /// constant is the number that reasoning produced.
    static constexpr int kPumpIntervalMs = 16;

    explicit Wdm(QObject *parent = nullptr);
    ~Wdm() override;

    /// Point this at the Link it will drive, before Link::connect().
    ///
    /// Separate from the constructor because the two are circular: Link needs
    /// its observer at construction and this needs the Link to send requests
    /// through. One of them has to be attached afterwards, and this is the one
    /// that can be — Link delivers nothing until connect() is called.
    void attach(Link *link);

    /// The engine whose QML errors this raises. Set before the theme loads.
    ///
    /// Held rather than found with qmlEngine(this), because this object is a
    /// context property: QML did not create it, it has no QQmlData, and
    /// qmlEngine() on it returns null. Without an engine, a precondition
    /// violation is reported with qCritical instead — which the message
    /// handler in main.cpp also treats as the greeter having given up.
    void setQmlEngine(QQmlEngine *engine);

    /// Start the dispatch timer. Called once, after the theme has loaded.
    void startPumping();

    /// Deliver whatever wdm has sent and flush what we have queued.
    ///
    /// What the timer calls. Public because the tests drive it directly: they
    /// have no event loop running, and a state machine that could only be
    /// advanced by a running QTimer could not be tested without one.
    void pump();

    QAbstractListModel *users() { return &users_; }
    QAbstractListModel *sessions() { return &sessions_; }

    bool hasPrompt() const { return hasPrompt_; }
    QString promptText() const { return promptText_; }
    bool promptSecret() const { return promptSecret_; }
    bool authenticating() const { return authenticating_; }
    bool authenticated() const { return authenticated_; }
    bool conversationOver() const { return conversationOver_; }
    QString defaultSession() const { return defaultSession_; }
    QString lastError() const { return lastError_; }
    bool linkDead() const { return linkDead_; }

    /// Open a PAM conversation for `username`, with the answer already typed.
    ///
    /// **Call this from the theme's submit handler and from nowhere else.** Not
    /// on load, not when the user drop-down changes. A conversation is a login
    /// attempt as far as pam_faillock is concerned for its whole duration,
    /// including the part where wdm is waiting for an answer nobody is there to
    /// give; there is no way to end a pam_authenticate sitting on a prompt
    /// without failing it. A greeter that armed PAM by itself charged an
    /// unattended login screen roughly one failed attempt a minute, and locked
    /// a real account out of a real machine.
    ///
    /// `answer` is the second argument for one reason, and it is the whole
    /// reason: it is what the user typed into a masked field before there was a
    /// prompt to put it in, and it must be spent only on a prompt PAM says is
    /// secret. Held here, dropped here, and never handed back to QML — see
    /// onPrompt. A theme that kept the buffer itself would be a theme that
    /// could answer an echo-on question with a password, and no amount of
    /// documentation prevents that as reliably as not offering it.
    ///
    /// Returns false, having sent nothing, when `answer` is empty. That is not
    /// a theme bug and does not raise: submitting an empty field would run the
    /// whole PAM stack against an empty password, fail, and be charged, so
    /// three stray presses of Enter would lock the account. Only the *first*
    /// answer is guarded this way — once a conversation is underway an empty
    /// answer is a legitimate choice and respond() sends it.
    ///
    /// Raises a QML error, having sent nothing, when a conversation is already
    /// in progress, when one has already succeeded, or when `username` names no
    /// account in `users`. Each of those is a request wdm answers with a
    /// protocol error, and a protocol error kills the greeter mid-login.
    ///
    /// `answer` has **no default**, and that is a decision rather than an
    /// omission. moc emits one QMetaMethod per arity, so a defaulted second
    /// argument would make `wdm.authenticate(username)` legal QML — and that is
    /// exactly the call a theme author ports from the webkit greeter, whose
    /// method genuinely takes one argument. It would take the empty-answer
    /// branch below and return false with no raise, no journal line and nothing
    /// on the wire: Enter does nothing, forever, silently. Without the default
    /// the same call fails QML overload resolution with a file and a line, which
    /// is the failure mode this whole class is built around.
    Q_INVOKABLE bool authenticate(const QString &username, const QString &answer);

    /// Answer the pending prompt. Raises unless `hasPrompt`.
    Q_INVOKABLE void respond(const QString &text);

    /// Abandon the conversation. Raises unless `authenticating`.
    ///
    /// This is what a theme calls when the user picks a different account: PAM's
    /// conversation is per user and a half-answered one for somebody else
    /// cannot be reused. It ends the conversation rather than starting another,
    /// which is the difference between a user drop-down that costs nothing and
    /// one that spends a PAM attempt every time it is touched.
    ///
    /// **No `authenticationComplete` is emitted for a cancel.** That is contract
    /// and not an implementation detail: a theme that waits for the signal
    /// before re-enabling its controls waits forever, and one that counts
    /// completions to decide whether an attempt happened must not count this.
    /// The theme asked for the cancel and knows it happened — `authenticating`
    /// is false by the time cancel() returns — and wdm sends neither auth_ok nor
    /// auth_failed after one, so there is no verdict to report. `conversationOver`
    /// stays false for the same reason: nothing was decided.
    ///
    /// A prompt already in flight when the cancel goes out is dropped rather
    /// than shown. See onPrompt: the request that would answer it belongs to a
    /// conversation wdm has already forgotten, and sending one is a protocol
    /// error that kills the greeter mid-login.
    Q_INVOKABLE void cancel();

    /// Log in. Raises unless `authenticated`, and unless `id` is in `sessions`.
    ///
    /// Sends no environment: locale and keyboard come from wdm's own
    /// configuration, and everything this greeter could put in the array wdm
    /// filters anyway.
    Q_INVOKABLE void startSession(const QString &id);

Q_SIGNALS:
    /// PAM said something. `kind` is "info" or "error".
    ///
    /// Once per message, carrying that message's own kind — never joined, never
    /// flattened to one severity. PAM routinely splits one explanation in two:
    /// "the account is locked" as an error and "10 minutes left to unlock" as
    /// info. A theme that keeps the distinction shows the first in red with the
    /// second beside it in grey; collapsing it here would take that choice away
    /// from every theme at once.
    ///
    /// The verdict of a failed conversation arrives through this same signal,
    /// as one more message with kind "error", immediately before
    /// authenticationComplete. There is no separate channel for it — which is
    /// why a theme that *assigns* the text rather than appending it loses the
    /// explanation and shows the user only "Authentication failure", which says
    /// nothing they can act on.
    ///
    /// wdm now flattens every authenticate failure to a single string and
    /// suppresses module text before authentication succeeds, so that the login
    /// screen cannot be used to learn whether an account exists. A theme must
    /// not depend on the detail older wdm versions sent.
    void message(const QString &text, const QString &kind);

    /// The conversation ended. `authenticated` says which way.
    ///
    /// Not emitted for cancel(), which the theme asked for. Emitted exactly
    /// once per conversation otherwise, including when the conversation ends
    /// because the link died.
    void authenticationComplete();

    void promptChanged();
    void authenticatingChanged();
    void authenticatedChanged();
    void conversationOverChanged();
    void defaultSessionChanged();
    void lastErrorChanged();
    void linkDeadChanged();

private:
    // LinkObserver. Everything above `done` runs during Link::connect(), before
    // the engine exists, which is why the models are populated by the time a
    // theme's first binding is evaluated.
    void onUser(const User &user) override;
    void onSession(const Session &session) override;
    void onDefaultSession(const std::string &id) override;
    void onLastError(const std::string &text) override;
    void onPrompt(const Prompt &prompt) override;
    void onAuthOk() override;
    void onAuthFailed(const std::string &reason) override;
    void onLinkDead(const std::string &why) override;

    /// Report a theme bug as a QML error, so it carries a file and a line.
    ///
    /// qmlWarning would not be enough. A warning lets the handler run on, so
    /// the theme carries on as though the call had worked — showing a login in
    /// progress that was never going to happen — and the next thing it does is
    /// usually the protocol violation this refusal just prevented. Throwing
    /// unwinds the handler at the call, which is the behaviour the webkit
    /// greeter's JavaScript API already has and the reason a theme bug there
    /// shows up as an exception rather than as a dead login screen.
    void raise(const QString &why);

    /// End whatever was in flight, whatever ended it.
    ///
    /// One function so that cancel, failure, success, link death and the
    /// unusable bail cannot disagree about what "ended" means — and, in
    /// particular, so that every one of them drops the buffered answer. A
    /// buffer that survived into the next conversation would be spent on a
    /// prompt the user never saw.
    void endConversation();

    void setPrompt(bool has, const QString &text, bool secret);
    void setAuthenticating(bool value);
    void setAuthenticated(bool value);
    void setConversationOver(bool value);

    Link *link_ = nullptr;
    QQmlEngine *engine_ = nullptr;
    QTimer *pump_ = nullptr;

    UserModel users_;
    SessionModel sessions_;

    bool hasPrompt_ = false;
    QString promptText_;
    /// True before any prompt has arrived, and true again whenever the prompt
    /// is cleared.
    ///
    /// Masked is the initial state and not merely the safe one. There is no
    /// prompt until the user submits, so the field they type their password
    /// into is this one; a theme binding echoMode to this gets a masked field
    /// on the first frame with no special case of its own. A stack whose first
    /// question is echo-on unmasks it when the prompt arrives, which is the
    /// direction that can be corrected on screen — the other cannot.
    bool promptSecret_ = true;
    bool authenticating_ = false;
    bool authenticated_ = false;
    bool conversationOver_ = false;
    QString defaultSession_;
    QString lastError_;
    bool linkDead_ = false;

    /// What the user typed before there was a prompt to put it in.
    ///
    /// An optional rather than a QString, so that "nothing buffered" and "the
    /// empty string" are different states. They cannot both occur today —
    /// authenticate() refuses an empty answer — but the distinction is what
    /// makes the refusal a separate rule from the spending rule rather than an
    /// emergent property of one string being falsy.
    ///
    /// ponytail: not zeroized. `reset()` frees the buffer without wiping it and
    /// `toStdString()` makes a second copy that is freed the same way, so a
    /// password can survive in freed heap until something reuses it. Ceiling:
    /// this greeter is no worse than the other three — none of wdm-greeter,
    /// wdm-gtk-greeter or wdm-webkit-greeter scrubs its answer buffer either —
    /// and no better, while wdm's own Rust side `zeroize`s every password it
    /// holds.
    ///
    /// A `scrub(QString &)` here would be theatre rather than an upgrade, and
    /// that is the reason it is not written. QString is implicitly shared: the
    /// value arriving from QML is a copy of the theme's `TextField.text`, so
    /// overwriting it either detaches — wiping a private copy while the
    /// original stays live in the item — or wipes a buffer the item is still
    /// displaying. The QML engine has its own JavaScript string for it as well,
    /// and neither of those is reachable from here. Upgrade, and it has to be
    /// the whole path or none of it: take the answer from QML as a
    /// `QJSValue`-free `QByteArray` the theme constructs and hands over, keep it
    /// in a container that wipes on destruction, marshal from it directly, and
    /// have the default theme clear its own field on submit — at which point the
    /// scrub here is the last link in a chain rather than the only one.
    std::optional<QString> buffered_;
};

} // namespace wdm
