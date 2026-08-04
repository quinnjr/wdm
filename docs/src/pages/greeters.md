---
layout: ../layouts/Base.astro
title: Writing a greeter
description: How to build a greeter against wdm_greeter_v1, and the constraints wdm imposes.
---

# Writing a greeter

A greeter is an ordinary Wayland client. Depend on the `wdm-protocol` crate for
bindings — not on wdm itself.

```toml
[dependencies]
wdm-protocol = { git = "https://github.com/quinnjr/wdm", tag = "v0.3.0", features = ["client"] }
```

Every crate here sets `publish = false`, so there is no registry version to
resolve: wdm ships as distribution packages, and taking the protocol as a git or
path dependency is what keeps it and the compositor implementing it on one
version. A greeter resolving `wdm-protocol` independently from a registry is the
mismatch `since` gating exists to survive, not one worth inviting. Pin the tag
you built against.

## Layer shell is mandatory

wdm exposes **no `xdg_toplevel` at all**. A greeter's window must be a
`zwlr_layer_shell_v1` surface; anything that tries to create a toplevel is
closed immediately, and the user sees a blank screen.

Anchor to all four edges and take exclusive keyboard focus. Either way of
choosing an output is legal — policy is the greeter's — and the two shipped
greeters make different choices:

- **Leave the output unset.** wdm places an output-less layer surface on the
  rank 0 output and moves it when ranks change, so a greeter that has no opinion
  gets the rank-0 policy for free and never has to see an `output_rank` event.
- **Bind the rank 0 output explicitly**, as `wdm-greeter` does: wait for the
  `output_rank` event naming rank 0, create the surface on that `wl_output`, and
  rebuild it when a later event names a different one. This costs a surface
  rebuild on hotplug, and buys a greeter whose stated policy is its own rather
  than the compositor's — it is the same placement today, but it does not
  silently change if the compositor's default ever does.

Choose the first if "wherever wdm puts it" is what you mean; the second if the
greeter documents where it draws and intends to keep that promise.

`xdg_wm_base` *is* advertised, but only so popups have somewhere to live —
menus and drop-downs need it for grabs.

## The shape of a greeter

1. Bind `wdm_greeter_v1` and collect the enumerate phase up to `done`.
2. Create the layer surface and draw.
3. `create_session(username)` and answer each `prompt` with `respond(id, …)`,
   using the id from the prompt rather than one you counted yourself.
4. On `auth_ok`, `start_session(session_id, env)`.

Show PAM's prompt text verbatim rather than hardcoding "Password:". The stack
decides what it asks, and a two-factor or expiry prompt is not a password
prompt.

Mask by prompt `style`, defaulting to masked when unsure: an unmasked password
is a worse failure than an unnecessarily masked token.

## Toolkit greeters

A toolkit owns its own Wayland connection, and GDK or Qt knows nothing about
`wdm_greeter_v1`. Opening a *second* connection does not work either — wdm
accepts the greeter once, and the protocol objects would belong to a client with
no surfaces.

The approach `wdm-gtk-greeter` uses is to share the toolkit's connection:
`gdk4-wayland`'s `wayland_crate` feature hands back GDK's `wl_display` as a
`wayland-client` proxy, from which a **separate event queue** is created on the
same connection. libwayland routes each object's events to the queue it was
created on, so the two do not interfere.

Two things that will cost you an afternoon otherwise:

- **`gtk4-layer-shell` must be loaded before `libwayland-client`**, because it
  interposes libwayland symbols. Declaring the link in the crate root achieves
  this; a build script does not, because its link arguments land at the end of
  the link line. Check with `readelf -d`.
- **GTK emits some notify handlers synchronously**, `set_model` among them, so a
  handler can re-enter your state while you still hold a borrow of it.

## What wdm will not let you do

These are enforced, not advisory:

- Escape the rate limit by destroying and rebinding the global. The limit is a
  deadline that survives reset.
- Set arbitrary environment on the session. See [the protocol](/wdm/protocol).
- Bind the global twice and drive two conversations.
- Answer a superseded prompt. Ids are checked.
- Keep the keyboard from Ctrl+Alt+F-key. VT switching is intercepted before the
  greeter sees it, so a wedged greeter can always be escaped from.
