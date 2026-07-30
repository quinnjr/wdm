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
| `vt` | `7` | Virtual terminal to run on. |
| `default_session` | unset | Session offered to a user with no recorded history. wdm reports it; preselecting is the greeter's policy. |

## `[greeter]`

| Key | Default | Meaning |
| --- | --- | --- |
| `command` | `/usr/lib/wdm/wdm-greeter` | Split on whitespace, **not** run through a shell. |
| `user` | `wdm` | Unprivileged account. Must not be root. |

The command is never passed to a shell, so a config file cannot inject shell
into a root process.

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
