---
layout: ../layouts/Base.astro
title: Writing a webkit theme
description: The JavaScript API wdm-webkit-greeter exposes to themes, and what a theme is responsible for.
---

# Writing a webkit theme

`wdm-webkit-greeter` renders the login screen as a web page. A theme is a
directory with an `index.html` in it — everything else is optional.

```toml
[greeter]
command = "/usr/lib/wdm/wdm-webkit-greeter --theme default"
```

`--theme` takes a name under `/usr/share/wdm/webkit-greeter/themes`, or a path
if it contains a `/`. A theme that cannot be found is a startup failure, not a
fallback to the default: a misspelled name that silently shows something else is
a configuration bug nobody notices until they are looking at the wrong login
screen. For the same reason a trailing `--theme` with no value, and `--theme`
given twice, are errors as well: falling back to the default, or quietly taking
the later of two values, shows a login screen other than the one that was asked
for.

The default theme is installed at
`/usr/share/wdm/webkit-greeter/themes/default` and is the worked example of
everything below. Copy it and start editing.

## The API

`window.wdm` exists before your own scripts run, already populated — it is
injected at document-start, so reading `wdm.users` from a top-level script
works and there is no ready callback to wait for.

| Property | |
|---|---|
| `wdm.users` | `[{ name, display_name, last_session }]` |
| `wdm.sessions` | `[{ id, name }]` |
| `wdm.default_session` | the machine's configured default session id, `""` when unset |
| `wdm.authentication_user` | who the current conversation is for, or `null` |
| `wdm.is_authenticated` | whether the last conversation succeeded |
| `wdm.in_authentication` | whether one is in progress |
| `wdm.link_dead` | true once the connection to wdm is gone; it never becomes false again |
| `wdm._prompt` | the pending prompt's text, or `null` — the underscore marks it as owned by the bridge rather than part of the API's shape, but a theme may read it |

`wdm._prompt` is what lets a theme tell "answer the question on screen" from
"start again": the default theme reads it to decide whether Enter should submit
a response or restart the conversation. It is asserted by a drift test, so it is
contract even though it is spelled as private.

`wdm.link_dead` latches. When it is true, nothing a theme sends reaches wdm —
`respond` and `start_session` post into a dead socket and are silently lost —
so a theme should stop offering a retry and say where to go instead: the shipped
theme's wording is "Connection to wdm lost — switch to a text console", matching
the GTK greeter. Retrying is worse than useless here, because the retry
typically clears the very message that explains the silence. A theme written
before the field existed sees `undefined`, so read it defensively:

```js
if (typeof wdm.link_dead !== "undefined" && wdm.link_dead) { /* … */ }
```

| Method | |
|---|---|
| `wdm.authenticate(username)` | start a conversation, ending any live one |
| `wdm.respond(text)` | answer the pending prompt |
| `wdm.cancel()` | abandon the conversation |
| `wdm.start_session(id?)` | log in; only valid once authenticated |

Calling these out of order throws, so a mistake shows up in your theme as an
exception rather than as a protocol error that gets the greeter killed.

> **Do not call `authenticate()` at load time.** Call it when the user submits
> the form, not when the page loads or when the user drop-down changes.
>
> A conversation is a login attempt as far as `pam_faillock` is concerned, for
> its whole duration — including the part where wdm is waiting for an answer.
> There is no way to end a `pam_authenticate` that is sitting on a prompt
> without failing it, so a conversation opened on behalf of someone who is not
> at the keyboard is eventually charged to them as a failed login. A theme that
> authenticates on load spends one of those every time the greeter is left
> alone, and enough of them lock the account.
>
> The default theme keeps what the user typed in a `pendingAnswer` variable
> across `authenticate()` and spends it in `show_prompt`, so the password is
> still typed once and checked immediately. Copy that shape.
>
> wdm will not let this spin: a timed-out prompt arrives as a `show_message`
> with kind `"error"` before the conversation ends, so a theme that retries on
> failure has something to distinguish a timeout from a wrong password. But the
> retry loop being closed does not make the first attempt free.

`start_session`'s argument is optional: omitted, it falls back to
`wdm.sessions[0]`. If neither resolves — no id given and no session installed on
the machine — it **throws** rather than returning quietly, so a theme cannot
show a login in progress that was never going to happen. Catch it and say the
machine has nothing to log into.

## The callbacks

Define these as globals. Any you do not define are simply not called, which is
what makes a one-file theme possible.

```js
window.show_prompt = (text, kind) => {};        // kind: "password" | "text"
window.show_message = (text, kind) => {};       // kind: "info" | "error"
window.authentication_complete = () => {};      // check wdm.is_authenticated
```

`show_prompt` gives you PAM's own words. Display them rather than hardcoding
"Password:" — the stack decides what it asks, and an expiry or two-factor prompt
is not a password prompt. Mask when `kind` is `"password"`.

`show_message` fires once per message PAM sent, carrying that message's own
style: `"info"` for `PAM_TEXT_INFO`, `"error"` for `PAM_ERROR_MSG`. PAM sends
them one at a time and often splits one explanation across two — "the account is
locked" as an error, "10 minutes left to unlock" as info — so they arrive as
separate calls rather than one joined line. A theme that wants to style them
differently can; one that does not can ignore `kind` and append them all to the
same element.

The verdict of the attempt arrives through **the same callback**, as one more
`show_message` with kind `"error"`, after every message belonging to the
attempt. There is no separate channel for it. This is why a handler that
*assigns* — `el.textContent = text` — rather than appending loses the
explanation: the verdict is the last message sent, it overwrites everything
before it, and "Authentication failure" on its own tells the user nothing they
can act on.

A theme must tolerate a callback arriving **twice**, and a callback it was owed
**never arriving at all**. The two have different causes:

- **A repeat** when an evaluation fails outright — the page refused the script,
  so the greeter retransmits everything that evaluation carried.
- **A drop** when the page merely takes too long. A callback the greeter cannot
  confirm the page ran is thrown away rather than sent again, because silence is
  not a refusal: the statements may already have run, and re-sending them would
  render "The account is locked." once per retransmission.
- **A drop** again once the queue is past its limit — 256 unacknowledged
  statements, reachable only by a page that has stopped acknowledging anything.
  Assignments are evicted first because a newer one supersedes them, but past
  that callbacks go too.

So write handlers that are idempotent, and do not read the absence of a
`show_message` as PAM having sent none.

The shipped default theme also caps what it *displays* at **six** messages. Past
that it drops the second-oldest, never the first: the oldest is the one carrying
the lockout reason, and there is no scrolling on this screen by design. A theme
copying it inherits that cap, and it means the shipped theme is not lossless —
a stack that emits more than six messages in one attempt shows the first and the
five most recent.

## What a theme is responsible for

The greeter has no policy of its own. It does not preselect a session, does not
retry, and does not decide what a failure looks like — a greeter that decided
those would be fighting every theme that disagreed. So a theme must:

- **Preselect a session.** wdm reports facts, not a choice: `user.last_session`
  is what that user logged into last and nothing else — empty for somebody who
  has never logged in, and never silently filled in with the machine's default —
  while `wdm.default_session` is what the configuration names for everyone. The
  default theme walks the chain history → `wdm.default_session` → first entry,
  and assigns only an id it has found in `wdm.sessions`: a recorded id can name
  a session that has since been uninstalled, and setting a `<select>` to a value
  no `<option>` carries leaves the dropdown showing nothing at all.
- **Restart the conversation when the user changes.** PAM's is per user, and a
  half-answered one for somebody else cannot be reused.
- **Keep `show_message` on screen past `authentication_complete`.** This is
  where a locked account explains itself, and the `error` that follows the
  messages is only the verdict — "Authentication failure" says nothing the user
  can act on. A theme that puts the messages and the verdict in the same
  element, so the verdict overwrites them, shows the user only the half that
  says nothing.
- **Not retry on its own.** Restarting immediately clears that explanation, and
  against a `pam_faillock` stack each attempt can extend the lock. Wait for the
  user.

## Constraints

The page is a login screen on a machine nobody has logged into yet, so it is not
trusted with the process:

- **Navigation is refused outside the theme directory.** A theme is
  self-contained; one that links out fails visibly instead of turning the login
  screen into a browser.
- **No persistent storage**, no cookies, no cache surviving the greeter.
- **No developer tools** unless `WDM_GREETER_DEBUG` is set in the greeter's
  environment.
- **No file access from `file://` scripts.** Assets loaded as subresources —
  `<img>`, `<link>`, `<script>` — work; `fetch()` of a local file does not.
- Everything crossing into the page is a JSON literal, so PAM's text cannot
  close a string and run as code.
