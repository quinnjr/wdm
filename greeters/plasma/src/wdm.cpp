#include "wdm.h"

#include <utility>

#include <QQmlEngine>
#include <QTimer>

namespace wdm {

namespace {

/// What the theme contract calls the two message severities.
///
/// Only the two: Secret and Visible never reach here, because a prompt that
/// expects an answer is a question and becomes `hasPrompt` rather than a
/// message. The switch names every PromptStyle so that a style added to the
/// enum later is a diagnostic here — `-Werror=switch`, which this greeter's
/// CMakeLists sets, makes it a build failure — rather than a message silently
/// filed as information.
///
/// The fall-through below is what the switch cannot cover: a value that is not
/// one of the enumerators at all, which C++ permits an enum to hold and which a
/// cast from the wire could produce. It says so out loud rather than returning
/// quietly, on the same grounds as styleFromProtocol's default arm in link.cpp
/// — the failure it would otherwise produce is an error message rendered as a
/// grey aside with nothing anywhere explaining why.
QString kindOf(PromptStyle style) {
    switch (style) {
    case PromptStyle::Error:
        return QStringLiteral("error");
    case PromptStyle::Info:
    case PromptStyle::Secret:
    case PromptStyle::Visible:
        return QStringLiteral("info");
    }
    qWarning("wdm: prompt style %d is not one this greeter knows; shown as information",
             static_cast<int>(style));
    return QStringLiteral("info");
}

} // namespace

Wdm::Wdm(QObject *parent) : QObject(parent) {}

// Out of line, and not `= default` in the header, because QTimer is only
// forward-declared there: the implicit destructor would have to see the
// complete type to destroy a child it does not own anyway.
Wdm::~Wdm() = default;

void Wdm::attach(Link *link) { link_ = link; }

void Wdm::setQmlEngine(QQmlEngine *engine) { engine_ = engine; }

void Wdm::startPumping() {
    if (pump_ != nullptr || linkDead_) {
        // linkDead_ as well as pump_, because main.cpp reaches this after the
        // theme has loaded and the link can have died before then — wdm hangs
        // up on a greeter it is replacing, and loading a theme is not instant.
        // onLinkDead stops a timer that exists; nothing stopped one created
        // afterwards, and a 16 ms wakeup that nothing will ever cancel runs for
        // the rest of the machine's uptime, because this screen is taken down
        // only by a reboot.
        return;
    }
    pump_ = new QTimer(this);
    pump_->setInterval(kPumpIntervalMs);
    // Coarse, deliberately. This is a poll for a socket that is usually empty,
    // and a coarse timer lets the kernel align it with whatever else is waking
    // the process — on a login screen that is nothing, which is the point: a
    // precise 16 ms timer on an idle machine is a wakeup every 16 ms forever.
    pump_->setTimerType(Qt::CoarseTimer);
    connect(pump_, &QTimer::timeout, this, &Wdm::pump);
    pump_->start();
}

void Wdm::pump() {
    if (link_ == nullptr || linkDead_) {
        return;
    }
    // The return value is deliberately ignored: a failure has already been
    // reported through onLinkDead by the time dispatch returns, and there is
    // nothing this could do with it that has not been done.
    link_->dispatch();
}

// --------------------------------------------------------------------------
// The methods a theme calls
// --------------------------------------------------------------------------

bool Wdm::authenticate(const QString &username, const QString &answer) {
    if (link_ == nullptr || linkDead_) {
        // Not a raise: the link dying is a fact about the machine rather than a
        // bug in the theme, and unwinding the submit handler would abandon
        // whatever the theme was doing to explain the silence. A theme is
        // required to check `linkDead` and stop offering a retry; this is the
        // backstop for one that does not, and it sends nothing.
        qWarning("wdm: authenticate() with no live connection to wdm; ignored");
        return false;
    }
    if (authenticating_) {
        raise(QStringLiteral(
            "wdm.authenticate(): a conversation is already in progress. Call cancel() first, "
            "or wait for authenticationComplete()."));
        return false;
    }
    if (authenticated_) {
        raise(QStringLiteral(
            "wdm.authenticate(): authentication has already succeeded. Call startSession(id)."));
        return false;
    }
    if (!users_.contains(username)) {
        // Refused here rather than forwarded, because wdm answers a
        // create_session naming an account it never advertised with a protocol
        // error, and a protocol error kills the greeter at the moment the user
        // is trying to log in.
        raise(QStringLiteral("wdm.authenticate(): '%1' is not one of wdm.users.").arg(username));
        return false;
    }
    if (answer.isEmpty()) {
        // Requirement, not politeness. Opening a conversation with nothing to
        // answer it with runs the whole PAM stack against an empty password,
        // fails, and is charged to the account by pam_faillock — so three stray
        // presses of Enter at an unattended screen lock it. Only the first
        // answer is guarded; respond() sends an empty one without complaint.
        return false;
    }

    // Buffered before the request goes out, because the prompt it is for can
    // arrive inside createSession()'s own dispatch.
    buffered_ = answer;
    setPrompt(false, QString(), true);
    setConversationOver(false);
    setAuthenticating(true);
    link_->createSession(username.toStdString());
    return true;
}

void Wdm::respond(const QString &text) {
    if (link_ == nullptr || linkDead_) {
        qWarning("wdm: respond() with no live connection to wdm; ignored");
        return;
    }
    if (!hasPrompt_) {
        raise(QStringLiteral(
            "wdm.respond(): nothing is waiting for an answer. Check wdm.hasPrompt first."));
        return;
    }

    const std::optional<Prompt> &pending = link_->pendingPrompt();
    if (!pending.has_value()) {
        // Reachable in principle and unreached in practice, which is a weaker
        // claim than this comment used to make. The two are no longer written
        // strictly together: Link drops a prompt that arrives inside a cancel's
        // barrier without telling anyone (see Link::cancel), and onPrompt drops
        // one that arrives with no conversation live. Both of those leave
        // hasPrompt_ false as well, so the pair still cannot disagree in the
        // direction that matters — hasPrompt_ true with nothing on the link —
        // but "they move together" is now a property of every path rather than
        // of one assignment, and a path added later could break it.
        //
        // Handled rather than asserted because the alternative to handling it
        // is answering with an id that is not the live one, which wdm meets
        // with stale_prompt and a dead greeter. No test reaches it and none can
        // without breaking the invariant it defends; it is defence in depth,
        // not tested code, and not to be deleted on the grounds that no test
        // names it.
        qWarning("wdm: hasPrompt with no prompt on the link; dropping the answer");
        setPrompt(false, QString(), true);
        return;
    }

    const std::uint32_t id = pending->id;
    const QString question = promptText_;
    const bool secret = promptSecret_;
    // Cleared before the answer is sent: from the theme's side the question has
    // been answered the moment it hands the text over, and leaving hasPrompt
    // true would let a second Enter answer the same id — which wdm refuses with
    // stale_prompt.
    setPrompt(false, QString(), true);
    if (!link_->respond(id, text.toStdString())) {
        // Unreachable while hasPrompt_ and Link's pending prompt move together,
        // which is the whole of the invariant above. Handled anyway, and handled
        // by putting the question *back*, because the alternative is the only
        // state in this object with no way out: the answer has been exchanged
        // out of the caller's hands, hasPrompt is false so no theme will offer
        // the field again, and nothing went on the wire — so the theme sits on
        // "Waiting…" until wdm times the attempt out and the user is looking at
        // a login screen that stopped responding for no stated reason. Restoring
        // the prompt costs one redundant assignment on a path that does not
        // happen and turns that into a question the user can answer again.
        qWarning("wdm: the link refused the answer to prompt %u; the question stays on screen",
                 id);
        setPrompt(true, question, secret);
    }
}

void Wdm::cancel() {
    if (link_ == nullptr || linkDead_) {
        qWarning("wdm: cancel() with no live connection to wdm; ignored");
        return;
    }
    if (!authenticating_) {
        raise(QStringLiteral(
            "wdm.cancel(): no conversation is in progress. Check wdm.authenticating first."));
        return;
    }
    link_->cancel();
    // conversationOver is deliberately not set: nothing was decided, and a
    // theme that shows "Press Enter to try again" off conversationOver would
    // otherwise say it to a user who has just picked a different account and
    // has not tried anything yet.
    endConversation();
}

void Wdm::startSession(const QString &id) {
    if (link_ == nullptr || linkDead_) {
        qWarning("wdm: startSession() with no live connection to wdm; ignored");
        return;
    }
    if (!authenticated_) {
        raise(QStringLiteral("wdm.startSession(): not authenticated. Call it from "
                             "onAuthenticationComplete, guarded on wdm.authenticated."));
        return;
    }
    if (!sessions_.contains(id)) {
        // wdm answers an unknown id with invalid_session, which kills the
        // greeter at the last possible moment — after the password was accepted
        // and with nothing left on screen to say what happened.
        raise(QStringLiteral("wdm.startSession(): '%1' is not one of wdm.sessions.").arg(id));
        return;
    }
    link_->startSession(id.toStdString());
}

// --------------------------------------------------------------------------
// What Link reports
// --------------------------------------------------------------------------

void Wdm::onUser(const User &user) { users_.append(user); }

void Wdm::onSession(const Session &session) { sessions_.append(session); }

void Wdm::onDefaultSession(const std::string &id) {
    const QString value = QString::fromStdString(id);
    if (value == defaultSession_) {
        return;
    }
    defaultSession_ = value;
    Q_EMIT defaultSessionChanged();
}

void Wdm::onLastError(const std::string &text) {
    const QString value = QString::fromStdString(text);
    if (value == lastError_) {
        return;
    }
    lastError_ = value;
    Q_EMIT lastErrorChanged();
}

void Wdm::onPrompt(const Prompt &prompt) {
    if (!authenticating_) {
        // A prompt for a conversation that is already over. Two ways to get
        // here, and both are ordinary rather than exotic: the theme called
        // cancel() while a prompt was in flight on the wire, or wdm sent a
        // trailing prompt after auth_failed. Either way the request that would
        // answer it has already been cancelled or concluded at wdm's end.
        //
        // Dropped, and dropped *here*, because the alternative is a UI that
        // re-arms itself: setPrompt(true, …) with authenticating false leaves
        // hasPrompt true, respond() checks only hasPrompt_, and the theme's next
        // submit sends a respond for a conversation wdm has already forgotten —
        // which wdm answers with no_auth or stale_prompt, and a protocol error
        // kills the greeter in the middle of a login. That is exactly the class
        // of mistake the refuse-here-rather-than-forward design exists to
        // prevent; a theme cannot guard against it, because from QML the
        // property simply became true.
        //
        // Not a message() either. The text belongs to an attempt that has
        // already been reported on, and showing it now would put a stale
        // "Password: " under a form the user has just abandoned.
        qDebug("wdm: prompt for a conversation that is no longer live; dropped");
        return;
    }

    const QString text = QString::fromStdString(prompt.text);

    if (!prompt.expectsAnswer()) {
        // PAM telling rather than asking. It does not become `hasPrompt`: a
        // greeter that waited for an answer to a statement would hang the login
        // screen, and one that answered it would be holding an id wdm rejects.
        Q_EMIT message(text, kindOf(prompt.style));
        return;
    }

    // The answer the user gave before PAM had asked for it.
    //
    // Taken unconditionally — it is worth exactly one prompt whether or not it
    // is spent on this one. A stack that asks twice must get the second answer
    // from the user, and a buffer that outlived the prompt it was refused for
    // would be a password held for a question nobody asked.
    const std::optional<QString> buffered = std::exchange(buffered_, std::nullopt);

    // And spent only on a masked question. This is the rule the whole buffer
    // exists under: it was typed into a field the theme renders masked because
    // promptSecret starts true, so it is a password. wdm forwards
    // PAM_PROMPT_ECHO_ON as a non-secret prompt, and a stack whose first
    // answerable question is echo-on — pam_oath's token, a username re-prompt —
    // would otherwise be handed that password as the answer to a question it
    // was never typed for, which the stack's own logging then records in the
    // clear. On a visible prompt the buffer is dropped and the question is put
    // on screen for the user to answer.
    if (buffered.has_value() && prompt.style == PromptStyle::Secret) {
        setPrompt(false, QString(), true);
        if (link_->respond(prompt.id, buffered->toStdString())) {
            return;
        }
        // Unreachable: this id came from the prompt Link is holding, so it is by
        // construction the one Link will accept. Handled rather than ignored
        // because of what the unhandled version leaves behind — the buffer has
        // already been exchanged out, so the password is gone; hasPrompt is
        // false, so no theme puts the field back; and nothing reached wdm, so
        // the conversation stalls until wdm times the attempt out and the user
        // watches a login screen do nothing. Falling through puts the question
        // on screen instead, which the user can answer by typing it again.
        qWarning("wdm: the link refused the buffered answer to prompt %u; asking the user",
                 prompt.id);
    }

    setPrompt(true, text, prompt.style == PromptStyle::Secret);
}

void Wdm::onAuthOk() {
    if (!authenticating_) {
        // A verdict for a conversation that is no longer live — it crossed a
        // cancel on the wire, or wdm sent a second one. The same guard as
        // onPrompt's and for a worse reason: without it endConversation() runs,
        // clearing `authenticating` and the buffered answer *while wdm still
        // has a live conversation at its end*, and the next authenticate()
        // therefore passes every local guard and sends create_session into
        // auth_in_progress — a protocol error, and a greeter killed mid-login.
        //
        // Dropping it also keeps the contract wdm.h states about cancel:
        // authenticationComplete is not emitted for one. A theme that counts
        // completions to decide whether an attempt happened must not count a
        // verdict for an attempt the user abandoned.
        qDebug("wdm: auth_ok for a conversation that is no longer live; dropped");
        return;
    }
    setConversationOver(true);
    setAuthenticated(true);
    endConversation();
    // Last, so that a theme reading `authenticated` inside the handler — which
    // is where it decides whether to call startSession — sees the verdict that
    // this completion is about.
    Q_EMIT authenticationComplete();
}

void Wdm::onAuthFailed(const std::string &reason) {
    if (!authenticating_) {
        // onAuthOk's guard, for the other verdict and for the same reasons.
        // Neither the message nor the completion is emitted: the text belongs
        // to an attempt that has already been concluded or abandoned, and
        // showing "Authentication failure" under a form the user has just
        // pointed at a different account explains nothing about the account
        // they are now looking at.
        qDebug("wdm: auth_failed for a conversation that is no longer live; dropped");
        return;
    }
    setConversationOver(true);
    endConversation();
    // The verdict arrives as one more message with kind "error", after every
    // message belonging to the attempt and before the completion. There is no
    // separate channel for it, which is what makes a theme that appends
    // messages show "the account is locked" with "Authentication failure"
    // underneath rather than instead of it.
    Q_EMIT message(QString::fromStdString(reason), QStringLiteral("error"));
    Q_EMIT authenticationComplete();
}

void Wdm::onLinkDead(const std::string &why) {
    if (linkDead_) {
        return;
    }
    linkDead_ = true;
    if (pump_ != nullptr) {
        // Nothing will ever arrive again and every dispatch is a no-op; a timer
        // that keeps firing is a wakeup every 16 ms for the rest of the
        // machine's uptime, because this screen is never taken down by anything
        // but a reboot.
        pump_->stop();
    }

    const bool wasLive = authenticating_;
    if (wasLive) {
        setConversationOver(true);
    }
    endConversation();

    // The reason reaches the theme as an ordinary error message so that a theme
    // has something to show beside its own "switch to a text console" line. An
    // unexplained silence under a form that never changes again is the failure
    // this whole property exists to make visible.
    Q_EMIT message(QString::fromStdString(why), QStringLiteral("error"));
    Q_EMIT linkDeadChanged();

    if (wasLive) {
        // A conversation that was live has ended, whatever the theme was
        // waiting for. Without this a theme sits on `authenticating` forever
        // and never reaches the branch where it disables its controls.
        Q_EMIT authenticationComplete();
    }
}

// --------------------------------------------------------------------------
// Internals
// --------------------------------------------------------------------------

void Wdm::raise(const QString &why) {
    if (engine_ != nullptr) {
        // The QML engine attributes this to the call site, so a theme bug is a
        // message in the journal naming a file and a line rather than a login
        // screen that stopped working.
        engine_->throwError(why);
        return;
    }
    // No engine: the tests, and the window between construction and the theme
    // loading. qCritical rather than qWarning because main.cpp's message
    // handler treats a critical as the greeter having given up, which is the
    // right disposition for a contract violation with nowhere to report it.
    qCritical("%s", qUtf8Printable(why));
}

void Wdm::endConversation() {
    // The buffer first, and unconditionally. Every path that ends a
    // conversation comes through here precisely so that this cannot be
    // forgotten on one of them: cancel, failure, success, link death.
    buffered_.reset();
    setPrompt(false, QString(), true);
    setAuthenticating(false);
}

void Wdm::setPrompt(bool has, const QString &text, bool secret) {
    if (hasPrompt_ == has && promptText_ == text && promptSecret_ == secret) {
        return;
    }
    hasPrompt_ = has;
    promptText_ = text;
    promptSecret_ = secret;
    Q_EMIT promptChanged();
}

void Wdm::setAuthenticating(bool value) {
    if (authenticating_ == value) {
        return;
    }
    authenticating_ = value;
    Q_EMIT authenticatingChanged();
}

void Wdm::setAuthenticated(bool value) {
    if (authenticated_ == value) {
        return;
    }
    authenticated_ = value;
    Q_EMIT authenticatedChanged();
}

void Wdm::setConversationOver(bool value) {
    if (conversationOver_ == value) {
        return;
    }
    conversationOver_ = value;
    Q_EMIT conversationOverChanged();
}

} // namespace wdm
