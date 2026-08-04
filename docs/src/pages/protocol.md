---
layout: ../layouts/Base.astro
title: Protocol
description: wdm_greeter_v1 — the Wayland protocol a greeter uses to log a user in.
---

# `wdm_greeter_v1`

One global, bound by the greeter, three phases. Authentication travels over the
same connection the greeter renders on.

The compositor performs authentication itself; credentials never reach a process
that can read the shadow database. The greeter is untrusted, and rate limiting
is enforced compositor-side.

## Enumerate

wdm pushes state on bind. The greeter issues no requests.

| Event | Meaning |
| --- | --- |
| `user(name, display_name, avatar_path, last_session)` | A loginable account. Filtered to `uid >= UID_MIN` from `/etc/login.defs`, with a shell not in the nologin set. `display_name` comes from GECOS. `last_session` is the session id from this user's last successful login — on version 2 it is that history and nothing else, empty for a first-time user. |
| `session(id, name, exec, type)` | `type` is `wayland` or `x11`. Scanned from `wayland-sessions` and `xsessions` under `/usr/local/share` then `/usr/share`. Ids are unique across both types — `start_session` looks up by id alone — so a Wayland session shadows an X11 session with the same id, and a local entry shadows the distribution's. |
| `default_session(id)` | **Since version 2.** The session id the configuration names as the machine-wide default, empty when none is set. The fallback a greeter may preselect for a user with no `last_session`; preselection is the greeter's policy. |
| `output_rank(wl_output, rank)` | Rank 0 is primary. |
| `last_error(text)` | Only when the previous launch attempt failed. |
| `done` | Initial state complete. |

`last_error` exists so a user whose session failed to start is told why, instead
of being bounced back to a login prompt with no explanation.

## Versions

The interface is at **version 2**. Version 2 added `default_session` and with it
split two things version 1 conflated: `last_session` is now the user's own
history alone, where in version 1 it carried the configured default when the
user had no history.

A greeter bound at version 1 gets the version 1 meaning — wdm substitutes the
configured default into `last_session` for it, because it has no other way to
learn one — and never receives `default_session`. Bind
`min(2, advertised_version)`.

`default_session` is listed **last** in the XML, after `auth_failed`, rather
than in the position it is documented in above. Version negotiation is what
actually protects an older peer; the ordering is belt and braces, so that every
event a version 1 greeter knows keeps the opcode it was compiled against.

## Authenticate

Mirrors PAM's conversation, because PAM asks an arbitrary number of questions in
an arbitrary order — a password, then a token, then "your password expires in
three days".

```
greeter → create_session(username)
wdm     → prompt(id, text, style)      ×N
greeter → respond(id, answer)  |  cancel()
wdm     → auth_ok  |  auth_failed(reason)
```

`style` is `secret`, `visible`, `info` or `error`. The first two expect a
response; the last two do not, and wdm advances the conversation itself.

Prompts are **wdm-driven and id-tagged**. A `respond` carrying an id that is not
the pending prompt raises `stale_prompt`, so a slow greeter cannot answer a
question that has been replaced. Ids come from a process-global counter and are
never reused.

`create_session` is rate limited. The refusal is *deferred until the limit
expires* rather than sent immediately, so a greeter that retries on failure —
the natural thing to write — waits rather than spinning. A greeter must not
assume a prompt arrives promptly.

## Launch

```
greeter → start_session(session_id, env)
```

Valid only after `auth_ok`. There is no reply: a successful launch ends the
connection, because wdm tears the greeter down and hands over the display. If
the launch fails the greeter is relaunched and told why through `last_error`.

`env` is a `wl_array` of NUL-separated `NAME=VALUE` entries. It is **filtered**:
only `LANG`, `LANGUAGE`, `LC_*` and `XKB_*` are honoured, values containing `/`
or NUL are rejected, as are values longer than 64 bytes, and everything that
survives is applied *before* wdm's own session variables so the greeter cannot
contradict a fact about the seat. The NUL check cannot fire in practice —
entries are NUL-separated, so one inside a value does not survive decoding — but
the filter states what it accepts rather than relying on the framing above it.

Filtering is **silent, and not an error**. An entry that is well formed but does
not survive the filter is dropped with no event and no protocol error —
`invalid_env` is about the array being malformed, not about its contents being
unwelcome. A greeter cannot tell which of its entries were kept, so it should
not depend on any of them arriving.

A greeter is unprivileged but the session runs as the authenticated user through
a login shell, so an unfiltered environment would be arbitrary code execution as
that user.

## Errors

| Error | Raised when |
| --- | --- |
| `auth_in_progress` | `create_session` while a conversation is live. |
| `no_auth` | `respond` or `start_session` outside an authenticated state. |
| `stale_prompt` | `respond` carried an id that is not the pending prompt. |
| `invalid_session` | `start_session` named a session that was never advertised. |
| `invalid_env` | The `env` array is not NUL-separated `NAME=VALUE` entries with `NAME` matching `[A-Za-z0-9_]+`. A well-formed entry the filter drops is **not** an error. |
| `already_bound` | A second `wdm_greeter_v1` object was bound while one is still alive. |

## Output ranking

`wl_output` carries no concept of rank, hence `output_rank`. Rank 0 is primary
and ranks are contiguous from 0 across connected outputs.

It is **re-emitted on hotplug**, not initial-state-only: unplugging the primary
promotes the next output to rank 0, and the greeter is expected to move its
login form. Policy is the greeter's — draw only on rank 0, or draw a background
everywhere and put the form on rank 0. wdm forces neither.
