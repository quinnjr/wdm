---
layout: ../layouts/Base.astro
title: Configuration
description: /etc/wdm/wdm.toml — VT, greeter, keyboard and output configuration.
---

# Configuration

`/etc/wdm/wdm.toml`. The file is optional: wdm's defaults are a working
configuration. A file that exists but is malformed **is** fatal, because
silently ignoring something you deliberately configured is worse than refusing
to start.

```toml
vt = 7
default_session = "hyprland.desktop"

[greeter]
command = "/usr/lib/wdm/wdm-greeter"
user = "wdm"

[keyboard]
layout = "us"

[[output]]
connector = "DP-1"
mode = "2560x1440@144"
scale = 1.5
```

## Top level

| Key | Default | Meaning |
| --- | --- | --- |
| `vt` | `7` | Virtual terminal to run on. Must be at least 1. |
| `default_session` | unset | Session offered to a user with no recorded history. wdm reports it; preselecting is the greeter's policy. |

VT 7 is free on a default systemd install, which is why `wdm.service` ships no
`Conflicts=getty@…`. **Moving `vt` onto a terminal a getty uses needs a
`wdm.service` drop-in**, or wdm and the getty contend for the same VT with
nothing arbitrating and the recovery console goes with it. `wdm(1)` gives the
drop-in and is the authoritative statement of the rule.

## `[greeter]`

| Key | Default | Meaning |
| --- | --- | --- |
| `command` | `/usr/lib/wdm/wdm-greeter` | Split on whitespace, **not** run through a shell. |
| `user` | `wdm` | Unprivileged account. Must not be root. |

The command is never passed to a shell, so a config file cannot inject shell
into a root process.

### The greeters' own files

How a greeter *looks* is not wdm's business, so it is not configured here.
Each shipped greeter reads its own file in `/etc/wdm` — wdm spawns greeters
with a cleared environment, so a file is the only channel to them besides
`command`'s arguments:

| Greeter | File | Keys |
| --- | --- | --- |
| `wdm-greeter` | `/etc/wdm/greeter.toml` | `color-scheme`, `background` |
| `wdm-gtk-greeter` | `/etc/wdm/gtk-greeter.toml` | `color-scheme`, `background`, `css` |
| `wdm-webkit-greeter` | `/etc/wdm/webkit-greeter.toml` | `theme`, `color-scheme`, `background` |
| `wdm-plasma-greeter` | `/etc/wdm/plasma-greeter.ini` | `theme`, `colorScheme`, `background` |

Every file is optional, and every file that exists but cannot be understood —
an unknown key, a malformed value — is a startup error rather than a
fallback, for the same reason a malformed `wdm.toml` is: continuing would
silently ignore a deliberate choice. `color-scheme` is `"dark"` or `"light"`;
`background` is a `#rrggbb` colour or an absolute path to an image
(`wdm-greeter` decodes it itself and takes PNG only; the toolkit greeters
take anything their toolkit loads); `theme`
names a theme exactly as `--theme` does, and `--theme` in `command` outranks
it. The packages install each file fully commented out, so the defaults are
also the documentation.

## `[keyboard]`

`rules`, `model`, `layout` (default `us`), `variant`, `options` — the usual xkb
fields. Configured here because there is no user whose preferences could be
consulted yet, and someone who cannot type their password on their own layout
cannot log in.

## `[[output]]`

**Array order is the priority.** The first entry is rank 0, the primary output,
which is where a greeter puts its login form. TOML arrays are ordered, so there
is no `priority` field to validate and no duplicate-rank error case.

| Key | Meaning |
| --- | --- |
| `connector` | Connector name as the kernel reports it, e.g. `DP-1`, `eDP-1`. |
| `mode` | `WIDTHxHEIGHT` or `WIDTHxHEIGHT@REFRESH`. Falls back to the preferred mode if unsupported. |
| `scale` | Fractional scales permitted. |
| `transform` | `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`, … |
| `enable` | `false` leaves the output unprogrammed and gives it no rank, so no login prompt can appear on it. |

Connectors not listed rank after every listed one, sorted by name — deterministic
rather than udev probe order, so the primary output does not move between boots.
Ranks are recomputed on hotplug, so unplugging the primary promotes the next
entry and the greeter moves its form.

`enable = false` does not actively blank the output. Whatever the previous DRM
master left in that connector's scanout stays there until the mode times out.

Output configuration exists at all because a monitor cannot be configured before
anyone has logged in.
