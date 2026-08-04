# Changelog

Notable changes to wdm. Format follows [Keep a Changelog]; versions follow
[Semantic Versioning].

## [Unreleased]

### Fixed

- **PAM now runs in its own process, and the user's session is forked from it.**
  0.4.0 unblocked login by removing `pam_kwallet5` from the shipped PAM stacks.
  That treated one module; the hazard was structural, and **any** module that
  forks and exits during `pam_open_session` reproduced it. It is now closed, and
  `pam_kwallet5` is started again on all three distributions.

  `wdm --pam-helper` is a freshly `exec`'d copy of wdm's own binary that talks to
  wdm over a `SOCK_SEQPACKET` socket on fd 3. It has never loaded a graphics
  driver, so there are no `atexit` handlers and no EGL state for a forking module
  to corrupt. And wdm releases the DRM device, the renderer, libinput and the
  libseat session **before** it tells the helper to open the session at all, so
  there is no display state anywhere in the process that runs PAM.

  Three further defects fall out of the same change, none of which was fixable by
  configuration:

  - `pam_loginuid: set_loginuid failed`. The kernel refuses the write unless the
    writing task is the one `/proc/self` names — the thread-group leader — and
    PAM ran on a spawned thread. The helper is single-threaded, and the session
    it forks inherits the value.
  - `pam_systemd: Failed to set ambient capabilities`. `prctl` is per-thread, so
    the capabilities never reached the session even when the call succeeded.
  - `pam_keyinit`, `pam_selinux` and `pam_namespace` never reached the session,
    because `fork()` from `main` copies only the calling thread's state. The
    session is now forked from the process that ran `pam_open_session`.

  A session that fails to start after the display has been released can no longer
  be reported immediately — there is no greeter on screen. It is shown by the
  next greeter as `last_error`, which is the price of not opening a PAM session
  while holding the GPU.

## [0.4.0] — 2026-08-04

A security and conformance release. **Upgrade if you run 0.3.0**: an
untouched login screen locked the first user in the list out of their account
in about three minutes, and the GTK greeter showed passwords in plaintext.

Both were found by running it, not by reading it. Everything below the first
two entries came from a project-wide audit of 292 units against their stated
intent, which found 43 gaps — including a secret-misdirection bug introduced
by the lockout fix itself.

### Fixed

- **`pam_kwallet5` crashed the compositor mid-login, on NVIDIA.** The user
  authenticated successfully and then lost the display manager. It is no longer
  started from the shipped PAM stacks.

  `pam_kwallet5` forks inside `pam_sm_open_session` and, on its error path,
  calls `exit()` in the child rather than exec'ing. wdm still holds DRM and a
  live EGL context at that point — `pam_open_session` runs before the display is
  released, deliberately — so the child ran the graphics driver's `atexit`
  handlers against GPU state it shares with the parent. On NVIDIA that faulted
  wdm's own channel (`NVRM: Xid 31 ... name=wdm`), after which every frame was
  rejected with "Device is currently paused" and wdm died. `pam_gnome_keyring`
  forks too but execs, which resets the inherited state, so it is safe and
  stays.

  Commenting the module out is a workaround, not the fix: **any** PAM module
  that forks and exits during `pam_open_session` can do this. The fix is for wdm
  to run the PAM session somewhere that holds no graphics state, and until that
  lands the shipped stacks do not start one that is known to.

- **An empty password cost a login attempt.** Pressing Enter on an empty field
  ran the whole PAM stack, failed, and was recorded by `pam_faillock` — so three
  stray presses of Enter locked the account. All three greeters now refuse to
  open a conversation for an empty first answer and say "Enter your password"
  instead. Only the first answer is guarded: once a conversation is underway an
  empty answer is a real choice, and is still sent.

- **The GTK greeter showed passwords in plaintext.** Its entry was masked by
  `refresh` when a prompt arrived, and with nothing arming PAM until the user
  submits there is no longer a prompt at startup — which is exactly the state
  the password is typed into. The entry is now built masked and re-masked after
  every attempt. The reference greeter (`is_none_or(|p| p.secret)`) and the
  webkit theme (`<input type="password">`) already defaulted to masked.

- **The GTK greeter showed an empty red error box.** Its error and notice labels
  are hidden by `refresh`, which only runs when the model's revision changes —
  so on a greeter nobody had touched it never ran, and both labels sat visible
  and empty. Their CSS gives them a background, so this was a blank alarm under
  the password field. Both are now built hidden.

- Prompts are logged at debug level (`WDM_LOG=wdm=debug`), which is what makes
  "the greeter is showing something odd" answerable. The prompt text only —
  responses are the one thing in that file that must never reach a log.

- **An untouched login screen locked the account out.** This is the serious one:
  it needed no interaction, no attacker, and no misconfiguration — only a
  machine left sitting at its greeter.

  Every greeter opened a PAM conversation as soon as a user was selected, which
  at startup meant before anyone had touched the keyboard. `pam_authenticate`
  then blocked in the conversation callback, and wdm abandoned the attempt after
  60 seconds. There is no way to end a `pam_authenticate` that is mid-prompt
  without failing it, so that abandonment reached `pam_faillock`'s `authfail`
  arm as a failed login. wdm reported it to the greeter as a bare `auth_failed`,
  which is indistinguishable from a mistyped password, so the greeter retried —
  re-arming the timeout. The two spun at roughly one attempt a minute, and on
  Arch's default `deny=3` the first user in the list was locked out about three
  minutes after boot.

  Three changes, each of which breaks the loop on its own:

  - **No greeter arms PAM until the user asks to log in.** Selecting a user now
    ends any conversation rather than starting one, and only submitting the form
    opens one. What the user typed before PAM asked for it is carried across
    `create_session` and spent on the first prompt, so the password is still
    typed once. This is the fix that takes the unattended case to zero attempts
    rather than merely bounding it.
  - **A timed-out prompt explains itself before failing**, as a `prompt` event
    with style `error`. That reaches `Model::push_notice`, which sets `blocked`,
    which suppresses the automatic retry in every greeter sharing
    `wdm-greeter-client` — rather than in whichever ones remember to
    special-case a reason string.
  - **`RESPONSE_TIMEOUT` is 30 minutes rather than 60 seconds.** It is a guard
    against a wedged greeter pinning a thread, not a limit on how long a human
    may take to read a screen. Being wrong in this direction costs one pinned
    thread; being wrong in the other locked people out of their machines.

  Third-party greeters and webkit themes that call `authenticate()` at load time
  keep working — the retry loop is closed compositor-side — but they still spend
  one attempt per unattended greeter. See the theme guide.

- **The greeter recompiled every shader on every boot.** Its home is
  `/var/empty`, which is root-owned and must stay empty, so GTK could not create
  `$HOME/.cache` and logged "Failed to create pipeline cache directory". wdm now
  creates `/var/cache/wdm`, owned by the greeter account and mode `0700`, and
  points `XDG_CACHE_HOME` at it. The directory is opened `O_NOFOLLOW` before its
  mode and owner are asserted, because unlike the runtime directory it persists
  across boots under an unprivileged account — a greeter compromised once could
  otherwise leave a symlink for root to follow on the next boot. Failing to
  create it is a warning, not a fatal error: a slow login screen beats none.

### Changed

- **The Arch packaging is three AUR packages rather than one split package**:
  `wdm` (the compositor and the toolkit-free reference greeter),
  `wdm-gtk-greeter` and `wdm-webkit-greeter`. A split package has a single
  `build()`, so every greeter was compiled whatever you asked for — installing
  the GTK greeter required WebKitGTK in the build chroot, and installing `wdm`
  alone required both toolkits. Each package now builds only its own crates, and
  `wdm` builds against nothing but the display stack. The three are git
  submodules of `aur/` in this repository, each tracking its own AUR repository.

  No installed file moves and no package changes name, so an existing
  installation is unaffected; only what you must have present to *build* one
  does.

## [0.3.0] — 2026-08-04

A minor rather than a patch release: `wdm-greeter-client` removes a public field
and `show_message` changes what a theme has to do with what it is handed. The
protocol is untouched and `wdm_greeter_v1` stays at version 2, so a greeter
binary built against 0.2.0 keeps working — but source built against
`wdm-greeter-client`, and any webkit theme, can need edits, which is not
something a patch number is allowed to say.

> **Themes must append, not assign.** `show_message` now fires once per PAM
> message instead of once per attempt, so a theme whose handler *assigns* to an
> element — `el.textContent = text` — shows only the last message it was sent
> and drops everything before it, including the explanation the verdict is
> meaningless without. The shipped default theme's own handler was assign-style
> until this release, so that is the shape a third-party theme most likely
> copied. Append instead, and clear on `authenticate()` rather than on each
> message.
>
> **And a callback may now arrive twice, or not at all.** The theme API's
> obligations grew stricter than 0.2.0's: a theme must tolerate the same
> callback being delivered more than once *and* a callback it was owed never
> arriving. A theme that counts `show_message` calls, or toggles state once per
> call, is wrong in a way it was not before. Write handlers that are idempotent
> and that do not treat silence as proof PAM said nothing.

### Added

- **`Model::link_dead`** in `wdm-greeter-client`, so a greeter can tell "the
  compositor went away" from "the attempt failed" and stop offering a retry that
  cannot reach anything. The field is deliberately the only spelling: a greeter
  asks `model.link_dead`, a theme asks `wdm.link_dead`, and there is no inverted
  accessor alongside it for either to disagree with.
- **`window.wdm.link_dead`** in the webkit greeter, the same fact for a theme:
  true once the connection to wdm is gone, and it never becomes false again. A
  theme should stop offering a retry and point at a text console, because
  nothing sent after that point reaches wdm and the retry typically clears the
  message explaining the silence. A theme predating the field sees `undefined`,
  so read it defensively.
- **`wdm_protocol::GREETER_GAVE_UP_EXIT`**, exit status **69**, now part of the
  contract for *any* greeter rather than an internal detail of the shipped one.
  A greeter exiting with it tells wdm it will not recover by being restarted, so
  wdm counts the exit as a rapid failure however long the process had been up,
  and the give-up screen is reached instead of a login screen that reloads for
  ever. The value is `EX_UNAVAILABLE` from `sysexits.h`; a third-party greeter
  already exiting 69 to mean something else now changes meaning silently and
  should pick another status.
- **The webkit greeter heartbeats an idle page.** With nothing queued for it,
  the greeter injects a statement that does nothing — `void 0;` — into the
  theme's page roughly every half second, purely so silence has something to be
  silent about: a theme whose top-level script never returns, or a web process
  that hangs before the load finishes, queues nothing at all and so previously
  left the greeter alive in front of a frozen login screen with nothing to time
  out. A theme author instrumenting `window`, or watching evaluations, will see
  the greeter originating traffic against an otherwise idle page; it has no
  observable effect of its own.

### Changed

- **A theme is told PAM's message styles apart.** `show_message` now fires once
  per message PAM sent, carrying that message's own kind — `"info"` for
  `PAM_TEXT_INFO`, `"error"` for `PAM_ERROR_MSG` — instead of joining them into
  one line reported as `"info"` whatever they were. A locked account can be
  shown in red with the minutes remaining beside it in grey, and the WebKit
  greeter stops disagreeing with the reference greeter about the same input.
  The default theme accumulates messages rather than replacing, because PAM
  routinely splits one explanation in two.
- **`Model::notice` is now `Model::notices`**, a list of `Notice { kind, text }`
  rather than one joined string, with `Model::notice_text()` for a greeter that
  has a single label to put them in. **Breaking** for an out-of-tree greeter:
  the field is removed, not deprecated, so one reading `notice` needs the
  one-line change to `notices` or `notice_text()`. **`Model::push_notice` gained
  a `NoticeKind` first argument** for the same reason — there is now a kind to
  carry — so a greeter calling it gets an arity error rather than a silent
  change of behaviour.
- **`wdm.start_session()` throws when no session resolves.** Called with no
  argument on a machine where `wdm.sessions` is empty — nothing installed under
  `wayland-sessions` or `xsessions` — it used to return having done nothing, so
  a theme reported a login in progress that was never going to happen. It now
  throws, like every other out-of-order call, and the theme can say so.
- **The intermediate copy of a PAM answer is no longer zeroized** on the way to
  the compositor. It never narrowed the window it appeared to: the answer is
  already in the page, in the protocol buffer and in libwayland's, none of which
  the greeter can scrub, so wiping one copy of several bought nothing and read
  as a guarantee that was not being kept.
- **`pam_keyinit` moved** in the Fedora stack, to after `pam_selinux open` and
  `pam_namespace`, which is where Fedora's own `login` and `sshd` stacks put it.
- The AUR `build()` no longer passes `--all-targets`. Nothing installed is a
  test, example or bench, `check()` builds the test targets itself, and building
  them twice in release mode on a workspace that links GTK4, WebKitGTK and
  smithay/EGL is a real cost — as well as making a broken test fail in `build()`
  rather than in `check()`.

### Fixed

- **Every process the udev backend started ran with `SIGTERM` and `SIGINT`
  blocked** — which is every process on a real machine. Registering the signal
  source blocks those signals on the calling thread, and a signal mask survives
  both `fork` and `exec`, so the greeter and the user's entire session inherited
  it. `loginctl terminate-session`, `systemctl stop` and systemd's shutdown all
  degraded to `SIGKILL` after their stop timeouts, Ctrl-C was inert in every
  terminal in the session, and each login handoff sat out the greeter's full
  two-second grace period before killing it, because the greeter could not act
  on the `SIGTERM` asking it to stop drawing. The nested backend registered no
  signal source at all and so was never affected — it gains one below, and both
  spawn paths now clear the mask before exec regardless of which registered it.
- **A greeter that gave up on its own page was restarted for ever.** The webkit
  greeter takes about half a minute to conclude a silent web process will never
  answer, which is well past the window in which an exit counts as a failure to
  start — so each one looked like a healthy greeter that happened to stop, the
  failure budget reset every time, and the login screen reloaded every thirty
  seconds instead of reaching the give-up screen.
- **A `wl_output` global leaked** on every failed connector rescan. The global
  was created before the fallible part of bringing an output up, so a failure
  left one advertised that nothing could withdraw — and a greeter that bound it
  saw defaults rather than the real mode, scale and transform.
- **A session's failure reason could be lost.** If the greeter died before it
  bound the protocol, the reason recorded for `last_error` was overwritten with
  nothing, and the greeter that finally came up showed the unexplained bounce
  that event exists to prevent.
- **The reference greeter swallowed `j` and `k` from passwords** while the
  session drop-down was open: both were bound as vi-style motion with no
  modifier, and the key router consulted the menu first.
- **A hung WebKit web process is now detected**, not just a crashed one. An
  evaluation that never comes back at all counts as a failure after a deadline,
  so a page that stops answering escalates to the same give-up the greeter
  already had for one that answers with errors.
- **The GTK greeter's "nothing to log into" message survived** the next model
  event. It was written to the widget rather than the model, so anything that
  triggered a repaint blanked it and left an insensitive form with no text.
- **An unwaitable greeter is no longer reported as having exited cleanly** on
  the give-up screen, next to a log line saying `waitpid` failed.
- **The greeter account's home is actually moved on upgrade.** 0.2.0 changed
  where the home points for a *new* account and shipped nothing to apply it to
  an existing one — `systemd-sysusers` never modifies an account that already
  exists — so every machine that installed 0.1.x kept the greeter's home on
  `/var/lib/wdm`, the root-owned directory the change exists to move it off. The
  deb, the rpm and the AUR package now all run a guarded
  `usermod -d /var/empty wdm`, taking effect only when the home is still the old
  value and only when `/var/empty` exists. A machine set up by hand should run
  it too.
- **The reference greeter says why it is not there.** A compositor advertising
  neither `wl_compositor` nor `wl_shm` left it running with nothing drawn and
  nothing logged — a blank screen indistinguishable from a wedge. It now reports
  the missing global and exits.
- The nested backend now handles `SIGTERM`/`SIGINT`, so a development wdm no
  longer orphans its greeter and that greeter's process group.
- Fedora upgrades no longer revert a customised `/etc/wdm/wdm.toml` or PAM
  stack: both are marked `%config(noreplace)`, matching what the deb and the
  Arch package already did.
- The deb's `postinst` reports a failing `systemd-sysusers` or
  `systemd-tmpfiles` instead of discarding it — the two failures that decide
  whether wdm can start at all.
- The AUR `wdm` package no longer advertises the `wdm-greeter-implementation`
  virtual it depends on, which defeated the greeter-choice prompt for anything
  resolving from `.SRCINFO`.
- Documentation the audit found stale: the greeter-author dependency snippet
  named a version that cannot resolve now that the crates are unpublished, the
  man page still identified as 0.1.3, and `/run/wdm`'s ownership was described
  by its end state rather than the two phases the code actually goes through.

### Known limitations

- **SELinux confinement is not delivered**, despite `pam.d-wdm.fedora`
  bracketing its session stack with `pam_selinux.so` close/open and joining a
  session keyring with `pam_keyinit`. Both set **per-thread** state, and wdm runs
  `pam_open_session` on its PAM thread while forking the session from the main
  one — `fork()` copies only the calling thread, so neither the armed exec
  context nor the joined keyring reaches the session. A confined Fedora login
  (staff_u, guest_u, xguest_u) is therefore still unconfined, and MCS separation
  does not apply. The modules stay in the file, because they are correct for the
  day the fork moves; the bracket is marked `ponytail:` with the upgrade path.

## [0.2.0] — 2026-08-03

A minor rather than a patch release: `wdm_greeter_v1` gains an event and a
version, `user.last_session` changes meaning, and the greeter client's public
types become `#[non_exhaustive]`. Version 1 greeters keep working — the new
event is gated and the old `last_session` behaviour is preserved for them — but
rebuilding against this release can require changes, which is not something a
patch number is allowed to say.

### Added

- **`default_session`, a new protocol event**, carrying the session id the
  configuration names as the machine-wide default, or an empty string when none
  is set. `wdm_greeter_v1` is now **version 2** and the event is gated
  `since="2"`, so a greeter built against version 1 binds at 1 and never sees
  it. It exists because preselection needs two facts and previously had one:
  what this user last logged into, and what this machine defaults to.
- **`window.wdm.default_session`** in the webkit greeter, and a default theme
  that walks history → `default_session` → first entry, assigning only an id it
  has actually found in `wdm.sessions`. A recorded id can name a session that
  was uninstalled since, and a `<select>` set to a value no `<option>` carries
  shows nothing at all.
- **`already_bound`**, a protocol error of its own for a second
  `wdm_greeter_v1` object bound while one is still alive. It was reported as
  `auth_in_progress`, which described neither the mistake nor the fix.
- **Per-distribution PAM stacks.** `packaging/pam.d-wdm` is Arch's and needs
  `system-login`; `pam.d-wdm.debian` uses the pam-auth-update `common-*`
  fragments and lists `pam_env` and `pam_limits` itself, because Debian's
  common stack contains neither, and without them `/etc/environment` and
  `/etc/security/limits.conf` applied to every way into the machine except the
  graphical one; `pam.d-wdm.fedora` uses authselect's `system-auth` plus
  `postlogin` and brackets its session stack with `pam_selinux.so`
  close/open, without which the session wdm forks stays in wdm's own domain and
  confined logins are silently unconfined.

### Changed

- **`user.last_session` is now honestly empty** for a user who has never logged
  in. It previously stood in for the machine default, which made "this user's
  history" and "this machine's configuration" indistinguishable to a greeter
  that wanted to treat them differently. On a version 1 bind the old behaviour
  is kept.
- **`authentication_user` is nulled when a conversation fails**, so a theme
  cannot read it as the user who is logged in.
- **`--theme` is strict.** A trailing `--theme` with no value, and `--theme`
  given twice, are startup errors rather than a fall back to the default —
  the same reasoning as a misspelled theme name already followed.
- **The greeter account's home is `/var/empty`**, not `/var/lib/wdm`. wdm sets
  the greeter's working directory to it, so it must exist: Arch and Fedora ship
  it in their base filesystem, and `packaging/wdm.tmpfiles` now declares it for
  Debian, which does not.
- **`wayland-protocols` is no longer a dependency of `wdm-protocol`.** Nothing
  in the generated bindings referred to it. Greeter authors depending on the
  crate no longer pull it in.

### Fixed

- The deb's `postinst` no longer discards `systemctl daemon-reload`'s
  diagnostic. It is skipped when there is no running systemd — a chroot or a
  container — and otherwise allowed to say what went wrong, instead of leaving
  an administrator with a `systemctl enable wdm` that fails for no visible
  reason.
- The rpm now runs `systemd-sysusers` and `systemd-tmpfiles --create` from a
  post-install scriptlet. Fedora's file triggers fire on the files being
  installed, not on their contents changing, so an upgrade otherwise kept the
  old directory ownership until the next boot.
- **Every crate sets `publish = false`.** wdm ships as distribution packages,
  and a greeter author takes `wdm-protocol` as a git or path dependency — which
  is also what keeps the protocol and the compositor implementing it on one
  version. A greeter resolving `wdm-protocol` independently from a registry is
  the mismatch `since` gating exists to survive, not one worth inviting.

### Security

- **`/var/lib/wdm` is root-owned.** It was owned by the unprivileged greeter
  account, which handed that account a directory root creates and renames files
  in — a symlink-attack surface, for a state directory the greeter never needs
  to touch. **On upgrade**, the deb and the rpm both re-run
  `systemd-tmpfiles --create`, which corrects the ownership in place; a machine
  set up by hand should run `chown root:root /var/lib/wdm` itself.
- **The greeter is killed by process group**, not by pid. It is made a session
  leader at spawn, so anything it started — a helper, a shell wrapper — dies
  with it rather than surviving into the user's session holding the greeter's
  end of the protocol socket.

## [0.1.3] — 2026-07-31

### Fixed

- **The greeter could not connect on a real machine.** It exited immediately
  with "Failed to open display", three times, until wdm gave up and switched to
  tty1 — a boot ending at a text console with no login screen. The Wayland
  socket was created by wdm, which is root, and chmodded to `0600`; but `0600`
  describes the *owner*, and `connect(2)` needs write permission, so the kernel
  refused the unprivileged greeter before it could send a byte. The socket is
  now chowned to the greeter account, and the runtime directory is prepared
  before the socket is created rather than after. Development never saw this,
  because there wdm and the greeter are the same user.
- **wdm never claimed its VT, and every frame failed** with "Page flip commit
  failed (Permission denied)". seatd binds a client's session to whichever VT is
  foreground when the seat is opened rather than allocating one, so wdm — which
  opened the seat on tty1 and then asked to switch to VT 7 — left its own
  session behind on a VT that was no longer in front. An inactive session holds
  no DRM master. The switch now happens with `VT_ACTIVATE` and `VT_WAITACTIVE`
  before the seat is opened. **Not yet verified on hardware.**

### Added

- **Debian and Fedora packages**, built from metadata in the crate manifests
  with `cargo-deb` and `cargo-generate-rpm`. Both produce one package per
  greeter, as the Arch packaging does, and neither enables `wdm.service` on
  install.
- **`wdm(1)`**, the man page `wdm.service` has advertised since it was written.

## [0.1.2] — 2026-07-30

### Added

- **`wdm-webkit-greeter`**, a WebKitGTK greeter whose themes are written in
  HTML, CSS and JavaScript — the same idea as `lightdm-webkit2-greeter`. A theme
  is a directory with an `index.html` in it, selected with
  `--theme <name-or-path>`, and drives the login through a `window.wdm` API.
  The greeter holds no policy of its own: preselecting a session, retrying a
  failed attempt and deciding what a failure looks like all belong to the theme,
  so the default theme is part of the contract rather than a demo.
- **`wdm-greeter-client`**, the `wdm_greeter_v1` client both toolkit greeters
  share: the connection, the event queue and the model events write into.
  Extracted from `wdm-gtk-greeter` rather than copied.

### Security

- Everything the webkit greeter puts into the page is a JSON literal. PAM's
  prompt and message text reaches JavaScript verbatim, so building script by
  concatenation would hand whoever writes a PAM message the run of the login
  screen.
- The webkit greeter refuses navigation outside its theme directory, keeps no
  persistent storage, and enables developer tools only when `WDM_GREETER_DEBUG`
  is set.

## [0.1.1] — 2026-07-30

### Fixed

- **The GTK greeter no longer retries past PAM's explanation.** `pam_faillock`
  reports a locked account as a text-info message and then fails the attempt.
  The greeter restarted the conversation on any failure, which cleared that
  message, reset the form, and left the user with a bare "Authentication
  failure" and no way to learn why — and on a faillock stack the extra attempts
  fed the lock. Info and error messages are now sticky and suppress the
  automatic retry; the form says "Press Enter to try again" and waits. A plain
  wrong password still retries immediately.
- **The GTK greeter opened two conversations at startup**, so every launch made
  PAM authenticate twice and spent two of the rate limiter's attempts.

## [0.1.0] — 2026-07-30

First release. wdm is a Wayland display manager that is its own compositor: it
binds DRM/KMS through libseat, hosts the greeter as an ordinary Wayland client,
and hands the display to the user's session at login. No kiosk compositor, no
nested session, no X server.

### Added

- **Compositor** on smithay 0.7, with `udev` (DRM/KMS, libinput, libseat) and
  `winit` (nested, for development) backends. Greeter surfaces are
  `wlr-layer-shell`; `xdg_wm_base` is advertised only for popups, so wdm needs
  no window management, focus policy or stacking.
- **`wdm_greeter_v1`**, a Wayland protocol for logging in: an enumerate phase
  for users, sessions and output ranks; an authenticate phase mirroring PAM's
  conversation; and a launch phase. Bindings ship as the `wdm-protocol` crate so
  greeter authors depend on the protocol rather than on wdm.
- **`wdm-greeter`**, the default greeter — software-rendered into `wl_shm` with
  no toolkit, so the shipped default carries no GUI dependency.
- **`wdm-gtk-greeter`**, a GTK4 greeter that shares GDK's Wayland connection
  rather than opening a second one.
- **Session launch** with the display released in the parent before forking, so
  the user's compositor acquires real DRM master on the same VT.
- Configuration at `/etc/wdm/wdm.toml`: VT, greeter command and account, xkb
  layout, and per-connector output priority, mode, scale and transform.
- PAM configuration and a systemd unit under `packaging/`.
- Documentation at <https://quinnjr.github.io/wdm/>.

### Security

The greeter is treated as untrusted throughout. Its socket is mode `0600` and
every connection's `SO_PEERCRED` uid is checked against the greeter account.
Environment it supplies is filtered by key *and* value. Rate limiting lives in
the compositor and survives a greeter destroying its protocol object or exiting
to be respawned. Prompt ids are never reused, so a late response from a
cancelled conversation cannot answer a later one's question. Accounts outside
the loginable uid range are refused at launch even when PAM authenticates them.

### Known limitations

- **The DRM path has never run on real hardware.** Development and testing use
  the nested backend, which cannot exercise DRM, the seat, the handoff, VT
  switching, or anything that depends on output scale.
- Authentication fails until `packaging/pam.d-wdm` is installed as
  `/etc/pam.d/wdm`.
- There is no CI. Tests and lints are run locally.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[Semantic Versioning]: https://semver.org/spec/v2.0.0.html
[Unreleased]: https://github.com/quinnjr/wdm/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/quinnjr/wdm/releases/tag/v0.4.0
[0.3.0]: https://github.com/quinnjr/wdm/releases/tag/v0.3.0
[0.2.0]: https://github.com/quinnjr/wdm/releases/tag/v0.2.0
[0.1.3]: https://github.com/quinnjr/wdm/releases/tag/v0.1.3
[0.1.2]: https://github.com/quinnjr/wdm/releases/tag/v0.1.2
[0.1.1]: https://github.com/quinnjr/wdm/releases/tag/v0.1.1
[0.1.0]: https://github.com/quinnjr/wdm/releases/tag/v0.1.0
