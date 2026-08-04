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

## Packages

### Arch

The AUR packaging in `aur/` is a split package producing `wdm` and one package
per greeter. `wdm` depends on the virtual `wdm-greeter-implementation`, which
each greeter provides, so pacman asks which one to install.

```bash
cd aur && makepkg -si
```

### Debian and Fedora

Both are built from metadata in the crate manifests, with no `debian/`
directory and no `.spec` file, and neither needs `dpkg-buildpackage` or
`rpmbuild` — they can be produced on any machine with a Rust toolchain.

```bash
cargo install cargo-deb cargo-generate-rpm
cargo build --release --workspace

for p in wdm wdm-greeter wdm-gtk-greeter wdm-webkit-greeter; do
  cargo deb -p $p --no-build            # target/debian/*.deb
  cargo generate-rpm -p crates/$p       # target/generate-rpm/*.rpm
done
```

Both put the greeters in `/usr/lib/wdm` rather than `/usr/libexec`, matching
Arch. `greeter.command` defaults to `/usr/lib/wdm/wdm-greeter`, and a
per-distribution default would be a configuration file that is wrong on two
distributions out of three.

Neither enables `wdm.service` on install. A machine being installed onto
usually has a display manager already, and enabling a second one that claims
`display-manager.service` is how a boot ends with no login screen at all.

On Debian the greeter account and its directories are created from `postinst`,
because Debian has no file triggers for `sysusers.d` and `tmpfiles.d`; Fedora's
systemd runs both automatically on install, and the rpm runs them again from a
post-install scriptlet so that an *upgrade* whose tmpfiles entries changed
takes effect immediately rather than at the next boot.

## Install by hand

| From | To |
| --- | --- |
| `target/release/wdm` | `/usr/bin/wdm` |
| `target/release/wdm-greeter` | `/usr/lib/wdm/wdm-greeter` |
| `packaging/pam.d-wdm` | `/etc/pam.d/wdm` (Arch) |
| `packaging/pam.d-wdm.debian` | `/etc/pam.d/wdm` (Debian) |
| `packaging/pam.d-wdm.fedora` | `/etc/pam.d/wdm` (Fedora) |
| `packaging/wdm.service` | `/etc/systemd/system/wdm.service` |
| `packaging/wdm.toml.example` | `/etc/wdm/wdm.toml` (optional) |

Only Arch has `system-login`; Debian's common stack is the pam-auth-update
`common-*` fragments and Fedora's is authselect's `system-auth` plus
`postlogin`, so there are three files rather than one. Install the one for the
distribution you are on — the packages already pick correctly.

Create the unprivileged account the greeter runs as. The packages do this
declaratively with `packaging/wdm.sysusers`, which is what the equivalent
`useradd` looks like:

```bash
useradd --system --shell /usr/sbin/nologin --home-dir /var/empty \
        --no-create-home wdm
install -d -o root -g root -m 0755 /var/empty
install -d -o root -g root -m 0755 /var/lib/wdm
```

The home is `/var/empty` and it has to exist: wdm sets the greeter's working
directory to it before exec, so a missing directory is a greeter that never
spawns. Arch and Fedora ship it in their base filesystem package; Debian does
not, which is why `packaging/wdm.tmpfiles` declares it.

### Upgrading a machine set up by hand

The greeter account's home moved from `/var/lib/wdm` to `/var/empty` in 0.2.0,
but an account that already exists is never modified by `useradd` or
`systemd-sysusers` — the packages run the move from their install scripts, and a
machine set up by hand has to run it itself:

```bash
if [ "$(getent passwd wdm | cut -d: -f6)" = /var/lib/wdm ]; then
  if [ -d /var/empty ]; then
    usermod -d /var/empty wdm \
      || echo "wdm: could not move the wdm account's home to /var/empty" >&2
  else
    echo "wdm: /var/empty does not exist; leaving the wdm account's home at /var/lib/wdm" >&2
  fi
fi
chown root:root /var/lib/wdm
```

This is the same shape the deb's `postinst`, the rpm scriptlet and
`aur/wdm.install` use — nested rather than conjoined, so the two failures say
different things — and it can be diffed against them line for line. Both guards
matter: the first leaves an administrator who chose a different home alone, and
the second keeps `/etc/passwd` from naming a directory that is not there — wdm
`chdir`s into the greeter's home before exec, so a missing one is a greeter that
never spawns.

`/var/lib/wdm` — the per-user last-session record — is **root-owned**, not
owned by `wdm`. Only wdm itself writes there, and it does so as root; giving
the directory to the unprivileged greeter account would hand that account a
directory root creates and renames files in, which is a symlink-attack
surface.

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

That the unit ships no `Conflicts=` is a consequence of the default, not an
invariant. Setting `vt` to a terminal a getty uses needs a `wdm.service`
drop-in, or wdm and the getty contend for the same VT with nothing arbitrating
and the recovery console goes with it. See `wdm(1)`, which gives the drop-in.

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
