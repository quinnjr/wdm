# wdm

A Wayland display manager that **is** the compositor — no kiosk, no nesting, no
X server.

[Documentation](https://quinnjr.github.io/wdm) ·
[Protocol](https://quinnjr.github.io/wdm/protocol) ·
[Writing a greeter](https://quinnjr.github.io/wdm/greeters) ·
[Changelog](CHANGELOG.md)

## Why

Every other Wayland display manager delegates the actual displaying. SDDM's
Wayland mode, greetd and lightdm's Wayland experiments all spawn a *separate*
kiosk compositor — cage, weston-kiosk, gamescope — whose only job is to host the
greeter window. That means two compositors on the login path, an extra package
dependency, and a nested session whose failure modes are opaque to the thing
supervising it.

wdm binds DRM/KMS directly through libseat, hosts the greeter as an ordinary
Wayland client, and hands the display to the user's session at login.

## How login works

Login is a **handoff, not a nesting**. When a session starts, wdm — in the
parent process, before forking — opens the PAM session, resolves the account
while still privileged, kills the greeter, and then drops *everything* holding
the display: the DRM device, the renderer, libinput, and the libseat session.
Only then does it fork, drop privileges, and exec.

The user's compositor therefore gets the same VT and real DRM master. Between
the release and the session coming up nothing owns the display — a black moment
of roughly 200ms, which is what every display manager does.

## Greeters are ordinary Wayland clients

A greeter speaks [`wdm_greeter_v1`](https://quinnjr.github.io/wdm/protocol) over
the same connection it renders on. It runs as an unprivileged user and never
sees the shadow database: wdm runs PAM itself and forwards each question the PAM
stack asks. Anything that can speak Wayland can be a greeter.

Three ship in this repository:

| Greeter | What it is |
|---|---|
| `wdm-greeter` | The default. Software-rendered into `wl_shm` with no toolkit, which keeps it dependency-free and makes it a readable example of the protocol. |
| `wdm-gtk-greeter` | GTK4, for deployments that want theming. |
| `wdm-webkit-greeter` | WebKitGTK — the login screen is a web page, driven by a small JavaScript API. See [writing a theme](https://quinnjr.github.io/wdm/themes). |

## Install

Building needs a Rust toolchain and development headers for libinput, libseat,
libudev, gbm, libdrm, xkbcommon and PAM; the toolkit greeters additionally need
`gtk4`, `gtk4-layer-shell` and WebKitGTK 6.

```bash
cargo build --release
```

Arch packaging lives in `aur/` as a split package (`cd aur && makepkg -si`).
Debian and Fedora packages are built from metadata in the crate manifests, so
neither `dpkg-buildpackage` nor `rpmbuild` is required:

```bash
cargo install cargo-deb cargo-generate-rpm
cargo deb -p wdm --no-build          # target/debian/*.deb
cargo generate-rpm -p crates/wdm     # target/generate-rpm/*.rpm
```

Everything from `packaging/` is installed the same way on all three
distributions **except the PAM stack**, which is per-distribution
(`pam.d-wdm`, `pam.d-wdm.debian`, `pam.d-wdm.fedora`) because each spells its
common stack differently. No package enables `wdm.service` on install: a
machine being installed onto usually has a display manager already, and
enabling a second one that claims `display-manager.service` is how a boot ends
with no login screen at all.

Full instructions, including the manual/from-source path, are in the
[install guide](https://quinnjr.github.io/wdm/install).

> **Authentication fails until `/etc/pam.d/wdm` exists.** An "Authentication
> failure" on the greeter without it is expected, not a bug.

## Development

Running for real needs root and a spare VT. For development use the nested
backend, which runs wdm as an ordinary window inside your existing session:

```bash
cargo run -p wdm -- --backend winit --config /path/to/wdm.toml
WDM_LOG=debug cargo run -p wdm -- --backend winit --config …
```

A working dev config points `greeter.command` at a built binary and
`greeter.user` at your own account, so no privilege drop is attempted:

```toml
vt = 7

[greeter]
command = "/abs/path/target/debug/wdm-greeter"
user = "yourusername"

[keyboard]
layout = "us"
```

```bash
cargo test --workspace                    # tests live beside the code
cargo clippy --workspace --all-targets    # must be clean
```

The nested backend **cannot** exercise DRM, the seat, the handoff, or VT
switching. Those paths have no test coverage.

## Layout

- **`wdm-protocol`** — the `wdm_greeter_v1` XML plus generated bindings, with
  `client` and `server` features. Greeter authors depend on this, never on wdm.
- **`wdm`** — the display manager.
- **`wdm-greeter`**, **`wdm-gtk-greeter`**, **`wdm-webkit-greeter`** — the three
  greeters above.
- **`wdm-greeter-client`** — the protocol client both toolkit greeters share.

Built on [smithay](https://github.com/Smithay/smithay) 0.7.

## Status

wdm is young. The compositor, protocol, PAM conversation, session launch and all
three greeters are implemented and tested, but **the DRM path has not yet run on
real hardware** — development uses a nested backend that cannot exercise DRM,
the seat, or the handoff. Treat it accordingly.

`wdm_greeter_v1` is at version 2. Greeters negotiate it, so a greeter built
against an older release keeps working, but the protocol is not yet frozen.

## License

MIT. See [LICENSE](LICENSE).
