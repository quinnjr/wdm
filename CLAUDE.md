# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What wdm is

A Wayland display manager that **is** the compositor. Every other Wayland DM
(SDDM's Wayland mode, greetd, lightdm) spawns a separate kiosk compositor — cage,
weston-kiosk, gamescope — purely to host the greeter window. wdm binds DRM/KMS
itself via libseat, hosts the greeter as an ordinary Wayland client, and hands
the display to the user's session at login. There is no nesting and no X server.

Built on smithay 0.7 with a hand-rolled Wayland protocol, `wdm_greeter_v1`.

## Commands

```bash
cargo build                       # whole workspace
cargo test --workspace            # all in-file #[cfg(test)] modules
cargo test -p wdm login::         # one module
cargo test -p wdm the_test_name   # one test
cargo clippy --workspace --all-targets   # must be clean; CI does not exist yet

cargo deb -p wdm --no-build              # target/debian/*.deb
cargo generate-rpm -p crates/wdm         # target/generate-rpm/*.rpm
```

Packaging for all three distributions installs the same files from
`packaging/`, except the PAM stack, which is per-distribution: `pam.d-wdm`
(Arch), `pam.d-wdm.debian`, and `pam.d-wdm.fedora`, because each distro spells
its common PAM stack differently. The deb and rpm metadata lives in each crate's `Cargo.toml`
rather than in a `debian/` directory and a `.spec`, because neither
`dpkg-buildpackage` nor `rpmbuild` runs on Arch, and packaging that cannot be
built here is packaging that never gets checked.

Running it for real needs root and a spare VT. For development, use the nested
backend — it runs wdm as an ordinary window inside your existing session:

```bash
cargo run -p wdm -- --backend winit --config /path/to/wdm.toml
WDM_LOG=debug cargo run -p wdm -- --backend winit --config …   # WDM_LOG, not RUST_LOG
```

A working dev config points `greeter.command` at a built binary and
`greeter.user` at your own account (so no privilege drop is attempted):

```toml
vt = 7
[greeter]
command = "/abs/path/target/debug/wdm-greeter"   # or wdm-gtk-greeter
user = "yourusername"
[keyboard]
layout = "us"
```

**Authentication will always fail** until `packaging/pam.d-wdm` is installed as
`/etc/pam.d/wdm`. An "Authentication failure" on the greeter is expected
otherwise and is not a bug.

Greeter logs go through `WDM_GREETER_LOG`. To see a greeter's protocol traffic,
point `greeter.command` at a wrapper script that exports `WAYLAND_DEBUG=1` —
wdm calls `env_clear()` when spawning, so the variable cannot be inherited.

## Crates

- **`wdm-protocol`** — the `wdm_greeter_v1` XML plus generated bindings, with
  `client` and `server` features. Greeter authors depend on this, never on wdm.
- **`wdm`** — the display manager.
- **`wdm-greeter`** — reference greeter. Software-rendered into `wl_shm`, no
  toolkit. The shipped default, and the proof the protocol is implementable
  from scratch.
- **`wdm-gtk-greeter`** — GTK4 greeter, for deployments that want theming.
- **`wdm-webkit-greeter`** — WebKitGTK greeter; themes are HTML/CSS/JS driving a
  `window.wdm` API. Holds no policy — retrying, session preselection and what a
  failure looks like are the theme's, which is why the default theme is part of
  the contract and not a demo.
- **`wdm-greeter-client`** — the protocol client both toolkit greeters share:
  the connection, the queue, and the `Model` events write into.

## Architecture

### The handoff is the whole design

On `start_session`, **in the parent, before forking**: PAM opens the session,
the account and environment are resolved while still privileged, the greeter is
killed, and then *everything holding the display* is dropped — DRM device,
renderer, libinput, and the libseat session. Only then does it fork, drop
privileges in the child, and exec.

Steps 3–4 are deliberately not in the child: closing the seat there would not
release the fds the parent still holds. The child does only async-signal-safe
work, because PAM threads are alive at fork time — **do not allocate in a
`pre_exec` closure**, use `Error::from_raw_os_error` rather than `Error::other`.

`backend/udev.rs`'s outer `loop` is one login generation each pass. Anything
registered on the long-lived `LoopHandle` (timers, event sources, Wayland
globals) must be torn down before the handoff or it fires in the *next*
generation.

### Layer shell only

Greeter toplevels are `wlr-layer-shell` surfaces. `xdg_wm_base` is advertised
**only** for popups; `XdgShellHandler::new_toplevel` closes anything that tries
to create a window. This is why wdm has no window management, focus policy, or
stacking — and why a GTK greeter that fails to initialise layer shell shows the
user a blank screen.

An `xdg_popup` is its own surface, not a subsurface, so it is not reached by
walking the parent's tree; it needs its own render pass and its **initial
configure**, sent from `commit()`. Omitting that configure means every GTK
drop-down silently does nothing.

### PAM is the only threading

`pam_authenticate` blocks and its conversation callback is a C callback, so each
attempt gets a thread. The conversation sends prompts to the event loop over a
`calloop::channel` and blocks on `mpsc` for the answer. Cancellation is
expressed by *dropping* `AuthHandle`: the receivers close, `recv` fails, PAM
unwinds by itself. There are no locks.

The thread also owns the PAM handle for the session's lifetime, because
`pam_open_session`/`pam_close_session` must be paired on one handle.

### Queues between layers

Wayland dispatch and event-source closures receive the compositor state, not the
backend that owns DRM and the seat. So they queue rather than act:

- `Wdm::requests` (`backend::Request`) — things only the backend can do: VT
  switch, session activation, device add/remove, connector rescan, vblank.
- `Wdm::pending_actions` (`login::Action`) — launch a session, restart the
  greeter. Drained by `handle_action`, which returns `HandOff` so the backend
  can release the display before forking.

### The greeter is untrusted

It runs as an unprivileged user, and every protocol request is validated against
the phase the conversation is actually in. Things that look like belt-and-braces
but are load-bearing:

- The socket is mode `0600` and every connection's `SO_PEERCRED` uid is checked
  against the greeter account.
- The rate limit is a **deadline that survives `Login::reset()`**, because a
  greeter can reach reset by destroying its object or by exiting to be
  respawned. Phase alone would be resettable at will.
- Prompt ids come from a process-global counter and are never reused: per-attempt
  counters restart at 0, so a late `respond` from a cancelled conversation would
  match the next one's first prompt.
- Environment the greeter supplies is filtered by **key and value** — `LANG`,
  `LANGUAGE`, `LC_*` and `XKB_*` only, rejecting values with `/` (glibc loads a
  locale from a path;
  libxkbcommon honours `XKB_CONFIG_ROOT`) — and applied *before* wdm's own
  variables so it cannot contradict a fact about the seat.
- Rate-limited `create_session` is answered **late**, not refused immediately: a
  greeter that retries on failure would otherwise spin for the whole cooldown.

## Traps that have already cost time

- **Logical vs physical coordinates.** `render_elements_from_surface_tree` and
  `MemoryRenderBufferRenderElement::from_buffer` take `Point<_, Physical>`;
  `PopupManager` offsets and pointer locations are `Logical`. They coincide at
  scale 1 — which the winit backend hardcodes — so mistakes here are invisible in
  development and wrong on any scaled output. Use `comp::to_physical`.
- **gtk4-layer-shell link order.** It interposes libwayland symbols and only
  works if loaded first. The `#[link]` in `wdm-gtk-greeter`'s crate root is what
  achieves that; rustc emits the root crate's native libs before its
  dependencies', while build-script link args land at the end of the link line
  and are too late. Verify with `readelf -d`.
- **GTK re-entrancy.** `set_model` emits `notify::selected` *synchronously*, and
  that handler re-enters and borrows the model. Never hold a `RefCell` borrow
  across a widget call — read what you need, drop the borrow, then touch widgets.
- **`--backend winit` cannot exercise** DRM, the seat, the handoff, VT switching,
  or anything scale-dependent. Those paths have no test coverage and have never
  run on hardware.

## Conventions specific to this repo

Tests live in `#[cfg(test)]` modules beside the code; there are no `tests/`
directories. Non-trivial logic is expected to leave one runnable check behind.

Deliberate simplifications are marked `ponytail:` and name the ceiling and the
upgrade path, e.g. dmabuf imports are accepted without a trial import because the
renderer is not reachable from the handler.

The design spec lives at `docs/superpowers/specs/` and is **gitignored** —
local-only by user policy. It is the authoritative statement of intent; when
code and spec disagree, one of them is a bug and the audit workflow decides
which.

## Branching

git-flow, configured in `.git/config` for `main`/`develop` (not `master`).
`develop` is the default branch; `main` is protected and takes PRs only.

```bash
git flow feature start <name>     # or: git worktree add .worktrees/<name> -b feature/<name> develop
```

Feature branches squash-merge into `develop`; `develop` merges into `main` via
PR with a merge commit, preserving history. `.worktrees/` is gitignored.
