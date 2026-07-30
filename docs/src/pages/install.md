---
layout: ../layouts/Base.astro
title: Install
description: Building wdm, installing its PAM and systemd units, and running it for development.
---

# Install

## Build

wdm needs a Rust toolchain and the development headers for libinput, libseat,
libudev, gbm, libdrm, xkbcommon and PAM. The GTK greeter additionally needs
`gtk4` and `gtk4-layer-shell`.

```bash
cargo build --release
```

`gtk4-layer-shell` is not optional for the GTK greeter. wdm exposes no
`xdg_toplevel` at all, so a window that is not a layer surface is closed as soon
as it is created.

## Install

| From | To |
| --- | --- |
| `target/release/wdm` | `/usr/bin/wdm` |
| `target/release/wdm-greeter` | `/usr/lib/wdm/wdm-greeter` |
| `packaging/pam.d-wdm` | `/etc/pam.d/wdm` |
| `packaging/wdm.service` | `/etc/systemd/system/wdm.service` |
| `packaging/wdm.toml.example` | `/etc/wdm/wdm.toml` (optional) |

Create the unprivileged account the greeter runs as:

```bash
useradd --system --shell /usr/sbin/nologin --no-create-home wdm
```

Then `systemctl enable wdm.service`. The unit aliases itself to
`display-manager.service`.

> **`/etc/pam.d/wdm` is required.** Without it every login attempt fails with
> "Authentication failure", because PAM has no configuration for the `wdm`
> service. This is the single most common reason a fresh install appears broken.

## VT 7

wdm runs on VT 7 by default. logind autospawns a getty on tty2–6 on demand and
`getty@tty1.service` is the unit enabled by default, so VT 7 is free: wdm needs
no `Conflicts=getty@…` and masks nothing.

That leaves **tty1 as a working text console**, which is the recovery path when
a greeter wedges — Ctrl+Alt+F1, log in, `systemctl restart wdm`. Since wdm hosts
arbitrary third-party greeters, a guaranteed-good VT is load-bearing rather than
a nicety.

## Development

The nested backend runs wdm as an ordinary window inside an existing Wayland
session, with no root and no VT switching:

```bash
cargo run -p wdm -- --backend winit --config ./wdm.toml
WDM_LOG=debug cargo run -p wdm -- --backend winit --config ./wdm.toml
```

Logging is read from `WDM_LOG`, not `RUST_LOG`, so a session's own `RUST_LOG`
cannot reconfigure the display manager. Greeters log through `WDM_GREETER_LOG`.

A development config points at built binaries and at your own account, so no
privilege drop is attempted:

```toml
vt = 7

[greeter]
command = "/abs/path/target/debug/wdm-greeter"
user = "yourusername"
```

The nested backend cannot exercise DRM, the seat, the handoff, VT switching, or
anything that depends on output scale — it hardcodes scale 1.
