# Changelog

Notable changes to wdm. Format follows [Keep a Changelog]; versions follow
[Semantic Versioning].

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
[0.1.0]: https://github.com/quinnjr/wdm/releases/tag/v0.1.0
