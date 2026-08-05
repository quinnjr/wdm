// The default theme, and part of the contract rather than a demo.
//
// This is the worked example of every decision a theme has to make, because
// wdm-plasma-greeter has no policy of its own: it does not preselect a session,
// does not retry after a failure, and does not decide what a failure looks
// like. A theme author is expected to diff against this file, so the rules that
// are requirements rather than taste are marked as such below and repeated in
// docs/src/pages/plasma-themes.md.
//
// The four that are requirements:
//
//   1. Nothing calls wdm.authenticate() except submit(). Not Component.onCompleted,
//      not the user drop-down. A conversation is a pam_faillock attempt for its
//      whole duration, including the part where wdm is waiting for an answer
//      nobody is there to give, and enough of them lock the account.
//   2. Nothing retries by itself. onAuthenticationComplete shows the verdict and
//      waits for a keypress.
//   3. Enter on an empty field opens no conversation.
//   4. The controls go dead when wdm.linkDead latches. Pressing Enter into a
//      socket nobody reads clears the one message that explains the silence.
//
// The buffered answer — what the user typed before PAM had asked for it — is
// not here at all. It lives in the greeter, is spent only on a prompt PAM says
// is secret, and QML never sees it. That is deliberate: it is the rule a theme
// is most likely to get wrong, so it is the one a theme cannot reach.

import QtQuick
import QtQuick.Controls
import QtQuick.Layouts

Item {
    id: root

    // QQuickView sizes this to the window, and the window is a layer surface
    // anchored to all four edges of the output. These are what the theme looks
    // like in a designer with no compositor attached.
    implicitWidth: 960
    implicitHeight: 640

    // What the login screen says to the user right now. One line, and the
    // greeter never writes it: PAM's own words when there is a prompt, and this
    // theme's words for the states PAM has nothing to say about.
    property string promptLine: ""

    // PAM's running commentary, split by the severity PAM gave each message.
    //
    // Accumulated rather than assigned, and that is a requirement rather than a
    // style: the verdict of a failed attempt arrives as one more `message` with
    // kind "error", after the messages explaining it. A theme that assigns
    // shows the user "Authentication failure" in place of "the account is
    // locked, 10 minutes left", which is the half that says nothing they can
    // act on.
    property string errorText: ""
    property string infoText: ""

    // Set once, when the link dies. Latching here as well as in the greeter so
    // that the "switch to a text console" line is appended exactly once however
    // many paths reach it.
    property bool gaveUp: false

    // Whether this machine can be logged into at all. A machine with no users
    // or no sessions is worth saying out loud rather than presenting a form
    // that cannot lead anywhere.
    readonly property bool usable: userBox.count > 0 && sessionBox.count > 0

    function appendError(text: string) {
        if (text === "")
            return;
        root.errorText = root.errorText === "" ? text : root.errorText + "\n" + text;
    }

    function appendInfo(text: string) {
        if (text === "")
            return;
        root.infoText = root.infoText === "" ? text : root.infoText + "\n" + text;
    }

    function clearMessages() {
        root.errorText = "";
        root.infoText = "";
    }

    // The session to preselect: this user's history, then the machine's
    // configured default, then the first installed session.
    //
    // The greeter reports those as facts and chooses between them for nobody —
    // `lastSession` is empty for an account that has never logged in and is
    // never quietly filled in with the default. Each candidate is checked
    // against the installed sessions, because a recorded id can name a session
    // that was uninstalled since, and a ComboBox pointed at an id no row
    // carries shows nothing at all.
    function selectPreferredSession() {
        if (sessionBox.count === 0)
            return;
        const user = wdm.users.get(userBox.currentIndex);
        let index = -1;
        if (user && user.lastSession)
            index = wdm.sessions.indexOf(user.lastSession);
        if (index < 0 && wdm.defaultSession !== "")
            index = wdm.sessions.indexOf(wdm.defaultSession);
        sessionBox.currentIndex = index < 0 ? 0 : index;
    }

    // The "your move" state: no attempt in flight, the field live and empty,
    // and a line saying what to do next. Reached on first paint, and after the
    // user picks a different account.
    function waitForUser() {
        root.promptLine = wdm.conversationOver ? qsTr("Press Enter to try again") : qsTr("Password");
        answer.text = "";
        answer.enabled = true;
        answer.forceActiveFocus();
    }

    // The link is gone for good and nothing sent from here reaches anyone.
    //
    // Everything is disabled rather than merely left alone: a field the user
    // can still type into is an invitation to an attempt that will be refused,
    // and the refusal would clear the only explanation on screen.
    function giveUp() {
        if (!root.gaveUp) {
            root.gaveUp = true;
            root.appendError(qsTr("Connection to wdm lost — switch to a text console"));
        }
        root.promptLine = "";
        // Cleared as well as disabled: a password typed just as the link
        // dropped would otherwise sit in the field until the machine is
        // rebooted, which is how long this screen stays up.
        answer.text = "";
        answer.enabled = false;
        userBox.enabled = false;
        sessionBox.enabled = false;
        submitButton.enabled = false;
    }

    function unusableReason(): string {
        // Missing users are reported ahead of missing sessions because a
        // machine with neither is first of all a machine with nobody to be.
        // The wording matches the other three greeters; a user meeting two wdm
        // machines should not read two different sentences for one condition.
        if (userBox.count === 0)
            return qsTr("No users available to log in");
        if (sessionBox.count === 0)
            return qsTr("No sessions installed");
        return "";
    }

    // The only thing that arms PAM, and the only place authenticate() is
    // called. See requirement 1 at the top of this file.
    function submit() {
        if (wdm.linkDead) {
            root.giveUp();
            return;
        }
        if (!root.usable) {
            // Defence in depth, and unreachable in *this* theme: `usable` is
            // readonly and bound to model counts that never change, so when it
            // is false Component.onCompleted has already disabled all four
            // controls and nothing can call submit(). It is here for the theme
            // copied from this one that re-enables a control — the drop-downs
            // are the easy mistake, and there is a paragraph in
            // Component.onCompleted about why they must go too.
            //
            // Says why rather than returning silently, because a bare `return`
            // reached that way is an Enter that does nothing at all with
            // nothing on screen to explain it, which reads as a wedged greeter
            // — the same defect the GTK greeter removed deliberately, and for
            // the same reason; see `unanswerable_prompt` in
            // crates/wdm-gtk-greeter/src/ui.rs. The reason is recomputed rather
            // than read from errorText because it is a fact about the machine
            // and outlives every attempt.
            root.promptLine = root.unusableReason();
            return;
        }

        if (wdm.hasPrompt) {
            // Read, clear, then send. An answer left in the field survives every
            // later state, and this is the clear that makes all of them empty
            // without any of them having to remember.
            const text = answer.text;
            answer.text = "";
            answer.enabled = false;
            root.promptLine = qsTr("Checking…");
            wdm.respond(text);
            return;
        }

        if (wdm.authenticating) {
            // A conversation is open and PAM has not asked anything yet. That
            // window is not brief — wdm answers a rate-limited create_session
            // late rather than refusing it, so it can last a whole cooldown —
            // and an Enter that does nothing at all reads as a wedged greeter.
            root.promptLine = qsTr("Waiting…");
            return;
        }

        if (answer.text === "") {
            // Requirement 3. Submitting nothing would run the whole PAM stack
            // against an empty password, fail, and be charged to the account,
            // so three stray presses of Enter would lock it. Only the *first*
            // answer is guarded: once a conversation is underway an empty
            // answer is a legitimate choice and is sent by the branch above.
            root.promptLine = qsTr("Enter your password");
            answer.forceActiveFocus();
            return;
        }

        const typed = answer.text;
        answer.text = "";
        answer.enabled = false;
        root.clearMessages();
        root.promptLine = qsTr("Waiting…");
        // The typed answer goes to the greeter with the request, and the
        // greeter decides whether PAM's first question is one it may be spent
        // on. This theme neither keeps a copy nor gets a say.
        if (!wdm.authenticate(userBox.currentValue, typed)) {
            answer.enabled = true;
            root.promptLine = qsTr("Enter your password");
            answer.forceActiveFocus();
        }
    }

    Connections {
        target: wdm

        // Once per PAM message, carrying that message's own kind. Two elements
        // and not one, so a locked account can explain itself in red with the
        // minutes remaining beside it in grey.
        function onMessage(text: string, kind: string): void {
            if (kind === "error")
                root.appendError(text);
            else
                root.appendInfo(text);
        }

        function onPromptChanged(): void {
            if (!wdm.hasPrompt)
                return;
            root.promptLine = wdm.promptText;
            // Whatever is in the field was typed while it was masked, so it is
            // a password; an echo-on question would put it on screen. The
            // greeter refuses the buffered answer for the same reason, one
            // field along.
            if (!wdm.promptSecret)
                answer.text = "";
            answer.enabled = true;
            answer.forceActiveFocus();
        }

        // The conversation ended, either way. Nothing here retries — see
        // requirement 2.
        function onAuthenticationComplete(): void {
            if (wdm.authenticated) {
                root.promptLine = qsTr("Starting session…");
                answer.enabled = false;
                userBox.enabled = false;
                sessionBox.enabled = false;
                submitButton.enabled = false;
                wdm.startSession(sessionBox.currentValue);
                return;
            }
            // A conversation can also end because the link did. Offering a
            // retry then would clear the message that explains the silence.
            if (wdm.linkDead) {
                root.giveUp();
                return;
            }
            root.waitForUser();
        }

        function onLinkDeadChanged(): void {
            root.giveUp();
        }
    }

    Rectangle {
        anchors.fill: parent
        color: "#12131a"
    }

    Rectangle {
        id: card

        anchors.centerIn: parent
        width: Math.min(460, root.width - 48)
        // The layout's margins counted rather than restated as a number. A
        // literal 56 here is 28 twice, and a copied theme that changes the
        // margin below gets a card that clips or gaps with nothing on screen
        // pointing at the cause. No `height:` line: QQuickItem's height already
        // defaults to implicitHeight, and restating it is one more place for
        // the two to disagree.
        implicitHeight: layout.implicitHeight + layout.anchors.margins * 2
        radius: 12
        color: "#1c1e28"
        border.color: "#2c2f3d"
        border.width: 1

        ColumnLayout {
            id: layout

            anchors.fill: parent
            anchors.margins: 28
            spacing: 12

            Label {
                text: qsTr("Sign in")
                color: "#e8e8ef"
                font.pointSize: 20
                font.bold: true
            }

            Label {
                text: qsTr("User")
                color: "#8b8fa3"
                font.pointSize: 9
            }

            ComboBox {
                id: userBox

                Layout.fillWidth: true
                model: wdm.users
                textRole: "displayName"
                valueRole: "name"

                // onActivated and never onCurrentIndexChanged. currentIndexChanged
                // fires while the ComboBox is being populated, before anyone has
                // touched the machine, and everything below would then run on a
                // login screen nobody is at. onActivated is emitted only for a
                // choice the user made.
                onActivated: {
                    // PAM's conversation is per user, and a half-answered one
                    // for somebody else cannot be reused. It is *ended* rather
                    // than restarted: starting one here would arm PAM for
                    // whoever the list happens to land on, which is exactly
                    // what submit() exists to be the only cause of.
                    if (wdm.authenticating)
                        wdm.cancel();
                    root.clearMessages();
                    root.selectPreferredSession();
                    root.waitForUser();
                }
            }

            Label {
                text: root.promptLine
                color: "#e8e8ef"
                visible: text !== ""
                Layout.fillWidth: true
                wrapMode: Text.Wrap
            }

            TextField {
                id: answer

                Layout.fillWidth: true
                // Masked from the first frame. There is no prompt until the
                // user submits, so this is the state the password is typed
                // into; promptSecret starts true and a prompt that says
                // otherwise unmasks it, which is the direction that can be
                // corrected on screen.
                echoMode: wdm.promptSecret ? TextInput.Password : TextInput.Normal
                color: "#e8e8ef"
                background: Rectangle {
                    color: "#0d0e13"
                    border.color: answer.activeFocus ? "#6f9dff" : "#2c2f3d"
                    border.width: 1
                    radius: 4
                }
                onAccepted: root.submit()
            }

            Label {
                text: root.errorText
                color: "#ff7b72"
                font.pointSize: 9
                visible: text !== ""
                Layout.fillWidth: true
                wrapMode: Text.Wrap
            }

            Label {
                text: root.infoText
                color: "#8b8fa3"
                font.pointSize: 9
                visible: text !== ""
                Layout.fillWidth: true
                wrapMode: Text.Wrap
            }

            Label {
                text: qsTr("Session")
                color: "#8b8fa3"
                font.pointSize: 9
            }

            ComboBox {
                id: sessionBox

                Layout.fillWidth: true
                model: wdm.sessions
                textRole: "name"
                valueRole: "id"
            }

            Button {
                id: submitButton

                text: qsTr("Log in")
                Layout.alignment: Qt.AlignRight
                onClicked: root.submit()
            }
        }
    }

    Component.onCompleted: {
        // The models are already populated: the greeter does not create the QML
        // engine until wdm's enumerate phase has ended, so there is no loading
        // state for a theme to render.
        //
        // Nothing here calls authenticate(). This is where a greeter that armed
        // PAM on its own used to lock out the first account in the list, at
        // roughly one failed attempt a minute, with nobody at the keyboard.
        //
        // linkDead is tested *first*, and the order is the whole point of the
        // test. A link that died during startup on a machine that also
        // enumerated nothing reaches both branches, and the unusable one
        // returns — so the user was told "No users available to log in" for
        // what is actually a dead socket, with the "switch to a text console"
        // line never said and gaveUp left false. The empty models are a
        // *consequence* of the link dying before wdm finished enumerating, so
        // reporting them is reporting the symptom and hiding the cause.
        if (wdm.linkDead) {
            root.giveUp();
            return;
        }
        if (!root.usable) {
            root.appendError(root.unusableReason());
            // All four, and the two drop-downs are the load-bearing half.
            //
            // Disabling only `answer` and `submitButton` left `userBox` live on
            // a machine with users but no sessions, and picking a user there
            // runs onActivated: clearMessages() erases "No sessions installed",
            // then waitForUser() re-enables the field and prints "Password". A
            // form that had explained itself is back to inviting an attempt
            // that cannot go anywhere, with the explanation gone. Enter then
            // reaches submit(), which has nothing to do.
            //
            // An empty users model has the mirror shape — sessionBox is the one
            // still live — so both go, not just the one that happens to be
            // populated.
            answer.enabled = false;
            userBox.enabled = false;
            sessionBox.enabled = false;
            submitButton.enabled = false;
            return;
        }
        // Why the last session ended, when there was one. Without it a user
        // whose session crashed is bounced back to this exact screen with no
        // explanation.
        if (wdm.lastError !== "")
            root.appendError(wdm.lastError);
        root.selectPreferredSession();
        root.waitForUser();
    }
}
