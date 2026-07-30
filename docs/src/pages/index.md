---
layout: ../layouts/Base.astro
title: wdm
description: A Wayland display manager that is its own compositor — no kiosk, no nesting, no X server.
---

# wdm

A Wayland display manager that **is** the compositor.

Every other Wayland display manager delegates the actual displaying. SDDM's
Wayland mode, greetd and lightdm's Wayland experiments all spawn a *separate*
kiosk compositor — cage, weston-kiosk, gamescope — whose only job is to host the
greeter window. That means two compositors on the login path, an extra package
dependency, and a nested session whose failure modes are opaque to the thing
supervising it.

wdm binds DRM/KMS directly through libseat, hosts the greeter as an ordinary
Wayland client, and hands the display to the user's session at login. No kiosk,
no nesting, no X server.

## How login works

Login is a **handoff, not a nesting**. When a session starts, wdm — in the
parent process, before forking — opens the PAM session, resolves the account
while still privileged, kills the greeter, and then drops *everything* holding
the display: the DRM device, the renderer, libinput, and the libseat session.
Only then does it fork, drop privileges, and exec.

The user's compositor therefore gets the same VT and real DRM master. Between
the release and the session coming up, nothing owns the display — a black moment
of roughly 200ms, which is what every display manager does.

## Greeters are ordinary Wayland clients

The greeter talks [`wdm_greeter_v1`](/wdm/protocol), a Wayland protocol, over the
same connection it renders on. It runs as an unprivileged user and never sees
the shadow database: wdm runs PAM itself and forwards each question the PAM
stack asks.

Two greeters ship in the repository:

- **`wdm-greeter`** — the default. Software-rendered into `wl_shm` with no
  toolkit at all, which keeps the shipped default dependency-free and makes it a
  readable example of the protocol.
- **`wdm-gtk-greeter`** — GTK4, for deployments that want theming.

Anything that can speak Wayland can be a greeter. See
[writing a greeter](/wdm/greeters).

## Status

wdm is young. The compositor, protocol, PAM conversation, session launch and
both greeters are implemented and tested, but **the DRM path has not yet run on
real hardware** — development uses a nested backend that cannot exercise DRM,
the seat, or the handoff. Treat it accordingly.
