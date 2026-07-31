# Changelog

Notable changes to wdm. Format follows [Keep a Changelog]; versions follow
[Semantic Versioning].

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
[0.1.3]: https://github.com/quinnjr/wdm/releases/tag/v0.1.3
[0.1.2]: https://github.com/quinnjr/wdm/releases/tag/v0.1.2
[0.1.1]: https://github.com/quinnjr/wdm/releases/tag/v0.1.1
[0.1.0]: https://github.com/quinnjr/wdm/releases/tag/v0.1.0
