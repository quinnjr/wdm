# Changelog

Notable changes to wdm. Format follows [Keep a Changelog]; versions follow
[Semantic Versioning].

## [Unreleased]

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
[Unreleased]: https://github.com/quinnjr/wdm/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/quinnjr/wdm/releases/tag/v0.1.3
[0.1.2]: https://github.com/quinnjr/wdm/releases/tag/v0.1.2
[0.1.1]: https://github.com/quinnjr/wdm/releases/tag/v0.1.1
[0.1.0]: https://github.com/quinnjr/wdm/releases/tag/v0.1.0
