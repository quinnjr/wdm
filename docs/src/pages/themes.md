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

| Method | |
|---|---|
| `wdm.authenticate(username)` | start a conversation, ending any live one |
| `wdm.respond(text)` | answer the pending prompt |
| `wdm.cancel()` | abandon the conversation |
| `wdm.start_session(id)` | log in; only valid once authenticated |

Calling these out of order throws, so a mistake shows up in your theme as an
exception rather than as a protocol error that gets the greeter killed.

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
same element. The verdict of the attempt is reported separately, as `"error"`,
after every message belonging to it.

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
