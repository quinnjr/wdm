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
cargo fmt --all --check                  # must be clean
cargo clippy --workspace --all-targets    # must be clean

# All three run in CI (.github/workflows/rust.yml) on main and develop.

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

wdm never opens a PAM session, and never forks the user's session, while it
holds the display. The order, and every step of it is load-bearing:

1. The greeter sends `start_session`. wdm validates it — `Launch::validate`:
   the account exists, its uid is in range, it has a login shell, the session
   has a command. **Here**, because this is the last moment at which a greeter
   is still on screen to be told.
2. wdm kills the greeter and drops *everything holding the display* — DRM
   device, renderer, libinput, libseat session.
3. **Only then** `Login::launch` sends `Msg::Launch` to the PAM helper.
4. The helper runs `pam_open_session`, assembles the environment
   (`Launch::build` — `XDG_RUNTIME_DIR` comes from PAM and wdm must not invent
   it), forks, drops privileges in the child, and execs.
5. The helper `waitpid`s, reports `SessionEnded`, runs `pam_close_session` on
   the handle that opened the session, and exits.
6. wdm begins the next login generation.

Step 2 before step 3 is the whole point. A module that forks inside
`pam_sm_open_session` and `exit`s rather than `exec`ing — `pam_kwallet5` does,
on its error path — used to inherit wdm's live EGL context and run the driver's
`atexit` handlers against shared GPU state; on NVIDIA that faulted wdm's own
channel and killed the compositor mid-login. There is now no display state
anywhere in the process that runs PAM.

Step 4's fork must be the helper's, not wdm's: `loginuid`, the session keyring,
the mount namespace and ambient capabilities are set on the process that ran
`open_session`, and only its children inherit them. That is also why the helper
cannot just `exec` the session — it has to survive to pair
`pam_close_session`.

A failure after step 2 has no greeter to report to. It becomes the *next*
greeter's `last_error`, which is the price of not opening a PAM session while
holding the GPU.

The child does only async-signal-safe work — **do not allocate in a `pre_exec`
closure**, use `Error::from_raw_os_error` rather than `Error::other`. The
helper is single-threaded, so the specific hazard that rule was written for is
retired; the rule stays, because a `pre_exec` correct only under an assumption
about its caller does not announce itself when that assumption stops holding.

`backend/udev.rs`'s outer `loop` is one login generation each pass. Anything
registered on the long-lived `LoopHandle` (timers, event sources, Wayland
globals) must be torn down before the handoff or it fires in the *next*
generation. The loop does keep dispatching during the session —
`backend::wait_for_session` — because the helper's `SessionEnded` arrives on
the auth channel and there is no other way to hear it.

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

### PAM is a process, not a thread

PAM does not run in wdm. `wdm --pam-helper` (`pamhelper.rs`) is a freshly
`exec`'d copy of wdm's own binary — undocumented in `wdm(1)`, reachable only
with a `SOCK_SEQPACKET` socket on fd 3 — that runs the whole conversation, opens
the session, forks the user's session, and closes the session. `auth.rs` is
wdm's end: it spawns the helper, drives it, and turns what it says into
`AuthEvent`s.

`exec` and not merely `fork`, for three reasons that are all load-bearing:

- **Fresh address space.** No graphics driver was ever loaded into it, so there
  are no `atexit` handlers and no EGL state for a forking module to corrupt.
- **Single-threaded.** `pam_loginuid` writes `/proc/self/loginuid`, which the
  kernel refuses unless the writer is the thread-group leader. On a spawned
  thread it could never succeed. It also means the session is forked from a
  process with no other threads.
- **It is the session's parent.** `pam_open_session` sets `loginuid`, the
  session keyring, the mount namespace and ambient capabilities on the helper,
  and `fork` copies all of them to the child. `pam_open_session` and
  `pam_close_session` are paired on one handle, which the helper holds for the
  session's whole lifetime — which is why it forks rather than `exec`ing.

wdm still has *one* thread per attempt, in `auth.rs`: it blocks on `recv` and
forwards to the event loop. It holds no PAM state and it is not the defect —
removing it would mean making the socket a `calloop` source, which is a wider
change (see the `ponytail:` at the top of `auth.rs`). There are no locks.

Cancellation is expressed by *dropping* `AuthHandle`, exactly as before: wdm's
end of the socket is shut down, the helper's next read hits EOF, its
conversation returns `PAM_CONV_ERR`, and PAM unwinds by itself.

### Queues between layers

Wayland dispatch and event-source closures receive the compositor state, not the
backend that owns DRM and the seat. So they queue rather than act:

- `Wdm::requests` (`backend::Request`) — things only the backend can do: VT
  switch, session activation, device add/remove, connector rescan, vblank.
- `Wdm::pending_actions` (`login::Action`) — launch a session, restart the
  greeter. Drained by `handle_action`, which returns `HandOff` so the backend
  can release the display before anything opens a PAM session. `HandOff`
  carries only a username: what gets launched stays in `Login::chosen` and is
  only reachable through `Login::launch`, so there is no second copy of it that
  some other call site could start while the GPU is still held.

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
