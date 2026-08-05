---
layout: ../layouts/Base.astro
title: Writing a plasma theme
description: The QML API wdm-plasma-greeter exposes to themes, and what a theme is responsible for.
---

# Writing a plasma theme

`wdm-plasma-greeter` renders the login screen with QtQuick. A theme is a
directory with a `Main.qml` in it — everything else is optional.

```toml
[greeter]
command = "/usr/lib/wdm/wdm-plasma-greeter --theme default"
```

`--theme` takes a name under `/usr/share/wdm/plasma-greeter/themes`, or a path
if it contains a `/`. A theme that cannot be found is a startup failure, not a
fallback to the default: a misspelled name that silently shows something else is
a configuration bug nobody notices until they are looking at the wrong login
screen. For the same reason a trailing `--theme` with no value, `--theme=` with
nothing after it, and `--theme` given twice are all errors, and so is any other
argument — an argument that is quietly ignored in a `greeter.command` line is a
setting an administrator believes is in effect.

The default theme is installed at
`/usr/share/wdm/plasma-greeter/themes/default` and is the worked example of
everything below. Copy it and start editing.

## The shape of a theme

`Main.qml`'s root object must be an **`Item`**, not a `Window`. The greeter owns
the window, because the window is a `wlr-layer-shell` surface and getting that
wrong produces a blank screen with no error on either side. Your `Item` is
resized to fill it.

```qml
import QtQuick
import QtQuick.Controls

Item {
    // wdm is already here, and its models are already populated.
}
```

A theme that will not load — a missing directory, no `Main.qml`, a QML syntax
error, or a root that is not an `Item` — makes the greeter exit non-zero with
the reason on stderr. wdm's supervisor restarts it, and after three rapid
failures shows its give-up screen carrying that text. A blank screen is never
the result.

Run `qmllint` over your theme. The shipped theme is linted as part of the
project's own test suite precisely because a misspelled property is not an error
anywhere until a user cannot log in:

```bash
qmllint --unqualified disable -W 0 Main.qml
```

`--unqualified disable` because `wdm` is a context property and every reference
to it is unqualified by construction; `-W 0` because qmllint prints its warnings
and exits 0 without it.

## The object

One object, `wdm`, is a context property on the engine's root context. It exists
before your bindings are evaluated and its models are already populated — the
greeter does not create the QML engine until wdm's enumerate phase has ended, so
there is no loading state to render and no ready callback to wait for.

### Models

`wdm.users` and `wdm.sessions` are `QAbstractListModel`s, so a view binds to
them directly.

```qml
ComboBox { model: wdm.users;    textRole: "displayName"; valueRole: "name" }
ComboBox { model: wdm.sessions; textRole: "name";        valueRole: "id" }
```

| `wdm.users` role | |
|---|---|
| `name` | the login name — what `authenticate()` takes |
| `displayName` | the GECOS name, or `name` when the account has none |
| `avatarPath` | absolute path, `""` when there is none |
| `lastSession` | the session this user logged in with last, `""` for a user who never has |

| `wdm.sessions` role | |
|---|---|
| `id` | what `startSession()` takes |
| `name` | what to show |
| `type` | `"wayland"` or `"x11"` |

`displayName` never needs a fallback of your own — the greeter substitutes
`name`, once, so a theme binding it can never render a blank row. `avatarPath`
does need one: wdm reports the path recorded for the account rather than probing
it, so the file may not exist.

Both models also carry three methods a theme needs and `QAbstractListModel` does
not otherwise offer:

| | |
|---|---|
| `indexOf(value)` | the row for a `name` or an `id`, or `-1` |
| `get(row)` | that row as an object of its roles, or `{}` for a row that does not exist |
| `contains(value)` | whether the model carries that `name` or `id` |

`contains()` is what the greeter itself validates against before sending
`create_session` or `start_session`, and it is exposed rather than kept private
because a theme that has a name from somewhere other than the model — a
`--theme` that reads a saved user, a keyboard shortcut — needs the same check.
`indexOf(value) >= 0` answers the same question; `contains` is the spelling for
when the row number is not wanted.

Both models are populated once and never change afterwards. A user added while
the greeter is up is not shown; wdm sends the lists once and the protocol has no
way to say otherwise.

### Properties

Every one of these has a change notification, so an ordinary binding tracks it.

| Property | Type | Meaning |
|---|---|---|
| `hasPrompt` | bool | a question is pending a response |
| `promptText` | string | its text |
| `promptSecret` | bool | the response must not be echoed |
| `authenticating` | bool | a conversation is live |
| `authenticated` | bool | it succeeded |
| `conversationOver` | bool | one has ended, either way |
| `defaultSession` | string | the machine's configured default, `""` when unset |
| `lastError` | string | why the previous session attempt failed, `""` if none |
| `linkDead` | bool | the connection to wdm is gone; latches |

`hasPrompt` is its own boolean rather than `promptText !== ""`. A PAM module
that legitimately sends an empty prompt is otherwise indistinguishable from no
prompt at all, and that is the sort of thing that only turns up on somebody's
smartcard stack.

`promptSecret` is **true before any prompt has arrived**, and true again
whenever a prompt is cleared. That is deliberate: nothing arms PAM until the
user submits, so the field they type their password into exists before there is
any prompt to ask about it. Bind `echoMode` to it and the field is masked on the
first frame with no special case of your own:

```qml
TextField { echoMode: wdm.promptSecret ? TextInput.Password : TextInput.Normal }
```

`linkDead` latches: once true it never clears. There is no reconnection — wdm
accepts the greeter's connection exactly once.

### Signals

```qml
Connections {
    target: wdm
    function onMessage(text, kind) { }          // kind: "info" | "error"
    function onAuthenticationComplete() { }     // check wdm.authenticated

    // Every property above has a NOTIFY signal and is connectable here by
    // name. The name is `onXChanged` for a property `x` — *except* for
    // `hasPrompt`, `promptText` and `promptSecret`, which share one signal,
    // `promptChanged`, and are therefore all three handled by
    // `onPromptChanged`. These two are where the shipped theme's control flow
    // actually lives.
    function onPromptChanged() { }              // PAM asked something; put it on screen
    function onLinkDeadChanged() { }            // the link died; disable everything
}
```

There is no `onHasPromptChanged`, no `onPromptTextChanged` and no
`onPromptSecretChanged`, and writing one is not an error you will be shown at
load: QML reports "no signal of the target matches the name" as a warning and
the handler then never runs, which is a login screen that stays blank when PAM
asks its question. The three share `promptChanged` because a prompt arriving or
being answered moves all three at once — there is no state in which one has
changed and the others have not — so one signal is one thing for a theme to
connect rather than three chances to connect the one that does not fire.

A binding tracks a property, but reacting to one arriving needs a handler. The
two that a theme almost certainly wants are `onPromptChanged` and
`onLinkDeadChanged`:

- **`onPromptChanged`** is how PAM's question reaches the screen. Guard on
  `hasPrompt` — the signal fires when a prompt is *cleared* as well as when one
  arrives — then show `promptText` and re-enable the field. This is also where
  an echo-on prompt must clear whatever is already in the field: it was typed
  while masked, so it is a password, and a visible question would put it on
  screen. The greeter refuses the buffered answer on exactly the same condition,
  one field along.
- **`onLinkDeadChanged`** is the whole of the `linkDead` guard described below.
  `linkDead` only ever goes true, so the handler needs no condition.

`cancel()` is the one method that ends a conversation without announcing it:
it emits **no** `authenticationComplete` and does not set `conversationOver`,
because nothing was decided. A theme that cancels — which is what selecting a
different user must do — resets its own form on that path rather than waiting
for a signal that is not coming, and a "Press Enter to try again" driven off
`conversationOver` correctly stays quiet for a user who has not tried anything
yet.

`onMessage` fires **once per PAM message, carrying that message's own kind**.
PAM sends them one at a time and routinely splits one explanation across two —
"the account is locked" as an error, "10 minutes left to unlock" as info — so
they arrive as separate calls rather than one joined line. A theme that keeps
the distinction shows the first in red with the second beside it in grey; one
that ignores `kind` is merely plain.

The **verdict** of a failed conversation arrives through the same signal, as one
more message with kind `"error"`, immediately before `authenticationComplete`.
There is no separate channel for it. This is why a handler that *assigns* the
text rather than appending it loses the explanation: the verdict is last, it
overwrites everything before it, and "Authentication failure" on its own says
nothing the user can act on.

`onMessage` also carries the reason the link died, so a theme has something to
show beside its own "switch to a text console" line.

wdm now flattens every authenticate failure to a single string and suppresses
module text before authentication succeeds, so that the login screen cannot be
used to learn whether an account exists. **A theme must not depend on the detail
older wdm versions sent.** It still gets an `error`-kind message when wdm times
an attempt out, which is what distinguishes an explained failure from a bare
one.

### Methods

| Method | Precondition |
|---|---|
| `authenticate(username, answer)` | no conversation in progress, none has succeeded, `username` is in `users` |
| `respond(text)` | `hasPrompt` |
| `cancel()` | `authenticating` |
| `startSession(id)` | `authenticated`, and `id` is in `sessions` |

Calling one out of order **throws a QML error**, so a theme bug shows up in the
journal with a file and a line rather than as a protocol violation that gets the
greeter killed. The protocol defines `auth_in_progress`, `no_auth`,
`stale_prompt` and `invalid_session` precisely because these are fatal at the
protocol layer, and a theme should never be able to reach one.

`authenticate` returns `false`, having sent nothing and thrown nothing, when
`answer` is empty or when the link is already dead. An empty field is a user
doing nothing, not a theme with a bug — see the next section.

`startSession` sends no environment. Locale and keyboard come from wdm's own
configuration; the protocol's `env` argument exists for greeters that let the
user change them, and this one does not.

## What a theme is responsible for

The greeter has no policy of its own. It does not preselect a session, does not
retry, and does not decide what a failure looks like — a greeter that decided
those would be fighting every theme that disagreed. So a theme must:

### Call `authenticate()` from the submit handler and from nowhere else

Not on load. Not from the user drop-down's `currentIndexChanged` — which fires
while the ComboBox is being populated, before anyone has touched the machine.
Use `onActivated`, which is emitted only for a choice the user made.

> A conversation is a login attempt as far as `pam_faillock` is concerned, for
> its whole duration — including the part where wdm is waiting for an answer.
> There is no way to end a `pam_authenticate` that is sitting on a prompt
> without failing it, so a conversation opened on behalf of someone who is not
> at the keyboard is eventually charged to them as a failed login. A theme that
> authenticates on load spends one of those every time the greeter is left
> alone, and enough of them lock the account. This is not hypothetical: it
> locked a real account out of a real machine.

Selecting a different user **cancels** the conversation rather than starting
one. PAM's conversation is per user and a half-answered one for somebody else
cannot be reused.

### Hand the typed answer to `authenticate()`, and never keep a copy

`authenticate(username, answer)` takes what the user typed as its second
argument because nothing arms PAM until they submit, so the first thing they
type arrives before PAM has asked for it. The greeter holds it, spends it on
PAM's first question, and clears it on every path that ends a conversation.

The rule that makes this safe is **the greeter's, not the theme's**: the
buffered answer is spent only on a prompt PAM says is `secret`. The user typed
it into a masked field; if the stack's first answerable question is echo-on — a
token, a username re-prompt — sending the buffer would answer a visible question
with a password, and the stack logs it in the clear. On a non-secret first
prompt the greeter drops the buffer and shows the prompt.

QML never sees that buffer. Do not build one of your own.

### Mask the input from the first frame

Bind `echoMode` to `promptSecret` **alone**, and never gate it on `hasPrompt`:

```qml
TextField { echoMode: wdm.promptSecret ? TextInput.Password : TextInput.Normal }
```

`echoMode: hasPrompt && promptSecret ? …` — and any form that defaults to
`TextInput.Normal` until a prompt arrives — puts the password on screen, because
there is no prompt at the moment it is typed. Nothing arms PAM until the user
submits, so the field exists, has focus, and is being typed into for the whole
of the state before any prompt. `promptSecret` starts true precisely so that the
one-line binding above is masked on the first frame with no special case of your
own.

Unmasking is the only direction that can be corrected on screen: a visible field
that should have been masked has already leaked, while a masked field that a
prompt says is echo-on unmasks the moment the prompt arrives and the user sees
what they are typing.

### Refuse an empty first answer

Enter on an empty field is not a login attempt and must not cost one: submitting
nothing runs the whole PAM stack against an empty password, fails, and is
charged by `pam_faillock`, so three stray presses of Enter at an unattended
screen can lock the account. `authenticate()` enforces this by returning `false`
— say so on screen and stop:

```qml
if (!wdm.authenticate(userBox.currentValue, typed))
    promptLine = qsTr("Enter your password");
```

Guard only the **first** answer. Once a conversation is underway an empty answer
can be a legitimate choice — a stack asking for an optional token is entitled to
be answered with nothing — and `respond("")` sends it without complaint.

### Preselect a session

wdm reports facts, not a choice. `lastSession` is what that user logged into
last and nothing else — empty for somebody who has never logged in, and never
silently filled in with the machine's default — while `defaultSession` is what
the configuration names for everyone. Walk history → `defaultSession` → first
entry, and check each candidate against the installed sessions with `indexOf`: a
recorded id can name a session that has been uninstalled since, and a ComboBox
pointed at an id no row carries shows nothing at all.

### Not retry on its own

After a failure, show the verdict and wait for a keypress. Restarting
immediately clears the very message that explains the failure, and against a
`pam_faillock` stack each attempt can extend the lock. `authenticationComplete`
fires; what happens next is the user's.

### Stop when `linkDead` latches

After that, all four methods refuse: nothing is sent, each logs a warning, and
`authenticate` returns `false` rather than throwing — a dead link is a fact
about the machine, not a theme bug, and unwinding the submit handler would
abandon whatever the theme was doing to explain the silence. Disable the
controls anyway, clear the input, and say where to go instead: a warning in the
journal is not on screen, and a field the user can still type into is an
invitation to an attempt that will be refused. The shipped theme's wording is "Connection to wdm
lost — switch to a text console", matching the GTK and webkit greeters. (The
reference greeter has no lost-link wording of its own, so there is no third to
match.)

This is the bug that bit two of them: pressing Enter cleared the only
explanation on screen and posted into a dead socket. A theme that omits the
guard reproduces it.

### Say when the machine has nothing to log into

An empty `users` or `sessions` model is a fact about the machine, not about the
attempt, and it outlives every attempt. The shipped theme says "No users
available to log in" or "No sessions installed" — in that order of preference,
because a machine with neither is first of all a machine with nobody to be — and
disables the form. The other three greeters use the same two sentences, and a
test checks that they still agree.

## Logging

`WDM_GREETER_LOG` selects verbosity, taking the same words as the Rust greeters:
`error`, `warn`, `info`, `debug`, `trace`, `off`. QML's own runtime warnings are
routed through it, so a binding loop or a reference to a property that does not
exist is reported with the theme's file and line.

`console.error()` from a theme, and any `qCritical` from Qt, mark the greeter as
having **given up**: it exits with status 69, which wdm counts against the
restart budget whatever the greeter's uptime was, so a theme that is broken at
runtime reaches wdm's give-up screen rather than reloading forever.

## Styling

The greeter sets `QT_QUICK_CONTROLS_STYLE=org.kde.desktop` when it is not
already set, so `QtQuick.Controls` looks like Plasma rather than like the Basic
style. Install `qqc2-desktop-style` for that to resolve; without it Qt falls back
to Basic with a warning, which is the one place in this greeter where a fallback
is right — a login screen that looks wrong is still a login screen.

A theme is free to import anything installed on the machine, including Kirigami.
The shipped theme deliberately imports only `QtQuick`, `QtQuick.Controls` and
`QtQuick.Layouts`: an import that is not installed is a theme that will not load,
and a theme that will not load is a login screen that does not appear.
