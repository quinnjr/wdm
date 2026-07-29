//! Parsing and validation of `/etc/wdm/wdm.toml`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

/// Where wdm looks for its configuration when `--config` is not given.
pub const DEFAULT_PATH: &str = "/etc/wdm/wdm.toml";

/// VT 7 is free on a default systemd install: `getty@tty1.service` is the unit
/// enabled by default and logind autospawns getty on tty2 through tty6 on
/// demand. Running here means wdm needs no `Conflicts=getty@…` and leaves tty1
/// as a working text console, which is the recovery path when a greeter wedges.
pub const DEFAULT_VT: u32 = 7;

/// Failure to load a configuration file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// A parsed, validated configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Virtual terminal to run on.
    #[serde(default = "default_vt")]
    pub vt: u32,

    /// Session id offered when a user has no recorded last session.
    ///
    /// wdm never preselects a session itself; this is advertised to the greeter
    /// as a fallback and applying it is the greeter's policy.
    #[serde(default)]
    pub default_session: Option<String>,

    #[serde(default)]
    pub greeter: Greeter,

    #[serde(default)]
    pub keyboard: Keyboard,

    /// Outputs in priority order. Array position is the rank: the first entry
    /// is rank 0, the primary. TOML arrays are ordered, so there is no
    /// `priority` field to validate and no duplicate-rank error case.
    #[serde(default)]
    pub output: Vec<Output>,
}

fn default_vt() -> u32 {
    DEFAULT_VT
}

/// How to launch the greeter, and as whom.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Greeter {
    /// Command line, split on whitespace. Not run through a shell.
    pub command: String,
    /// Unprivileged user the greeter runs as. Must not be root.
    pub user: String,
}

impl Default for Greeter {
    fn default() -> Self {
        Self {
            command: "/usr/lib/wdm/wdm-greeter".to_owned(),
            user: "wdm".to_owned(),
        }
    }
}

/// xkb keyboard configuration for the greeter.
///
/// This has to be configured here because there is no user whose preferences
/// could be consulted yet, and a user who cannot type their password on their
/// own layout cannot log in.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keyboard {
    #[serde(default)]
    pub rules: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub options: Option<String>,
}

/// Per-connector output configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// Connector name as the kernel reports it, e.g. `DP-1`, `eDP-1`.
    pub connector: String,

    /// Mode to set. When absent wdm uses the connector's preferred mode.
    #[serde(default)]
    pub mode: Option<Mode>,

    /// Fractional scales are permitted; wdm renders the greeter at this scale.
    #[serde(default)]
    pub scale: Option<f64>,

    #[serde(default)]
    pub transform: Transform,

    /// When false the output is left blank and receives no rank, so no login
    /// prompt can appear on it.
    #[serde(default = "enabled")]
    pub enable: bool,
}

fn enabled() -> bool {
    true
}

/// Output rotation and reflection, named as in `wl_output`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transform {
    #[default]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

/// A display mode, written `WIDTHxHEIGHT` or `WIDTHxHEIGHT@REFRESH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u16,
    pub height: u16,
    /// Refresh rate in millihertz, or `None` to accept any rate at this size.
    ///
    /// Millihertz because real modes are not integral: 59.94Hz is 59940, and
    /// rounding it to 60 fails to match the mode the kernel reports.
    pub refresh_mhz: Option<u32>,
}

/// Why a mode string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    #[error("expected WIDTHxHEIGHT or WIDTHxHEIGHT@REFRESH, got {0:?}")]
    Shape(String),
    #[error("{0:?} is not a valid dimension")]
    Dimension(String),
    #[error("{0:?} is not a valid refresh rate")]
    Refresh(String),
    #[error("dimensions must be non-zero")]
    Zero,
}

impl std::str::FromStr for Mode {
    type Err = ModeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (size, refresh) = match s.split_once('@') {
            Some((size, refresh)) => (size, Some(refresh)),
            None => (s, None),
        };

        let (w, h) = size
            .split_once(['x', 'X'])
            .ok_or_else(|| ModeError::Shape(s.to_owned()))?;

        let width: u16 = w
            .trim()
            .parse()
            .map_err(|_| ModeError::Dimension(w.to_owned()))?;
        let height: u16 = h
            .trim()
            .parse()
            .map_err(|_| ModeError::Dimension(h.to_owned()))?;

        if width == 0 || height == 0 {
            return Err(ModeError::Zero);
        }

        let refresh_mhz = match refresh {
            None => None,
            Some(r) => {
                let hz: f64 = r
                    .trim()
                    .trim_end_matches("Hz")
                    .trim_end_matches("hz")
                    .trim()
                    .parse()
                    .map_err(|_| ModeError::Refresh(r.to_owned()))?;
                if !hz.is_finite() || hz <= 0.0 {
                    return Err(ModeError::Refresh(r.to_owned()));
                }
                Some((hz * 1000.0).round() as u32)
            }
        };

        Ok(Self {
            width,
            height,
            refresh_mhz,
        })
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)?;
        if let Some(mhz) = self.refresh_mhz {
            write!(f, "@{}", f64::from(mhz) / 1000.0)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            vt: DEFAULT_VT,
            default_session: None,
            greeter: Greeter::default(),
            keyboard: Keyboard::default(),
            output: Vec::new(),
        }
    }
}

impl Config {
    /// Load and validate a configuration file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;

        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;

        config.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_owned(),
            message,
        })?;

        Ok(config)
    }

    /// Load the default path, falling back to defaults if it does not exist.
    ///
    /// A missing config is not an error: wdm's defaults are a working
    /// configuration, and refusing to start would leave a machine with no way
    /// to log in graphically over an absent optional file. A malformed config
    /// *is* an error, because it means someone tried to configure something and
    /// wdm would otherwise silently ignore it.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Err(ConfigError::Read { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config at {}, using defaults", path.display());
                Ok(Self::default())
            }
            other => other,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.vt == 0 {
            return Err("vt must be at least 1".to_owned());
        }
        if self.greeter.command.trim().is_empty() {
            return Err("greeter.command must not be empty".to_owned());
        }
        if self.greeter.user == "root" {
            // The greeter is untrusted third-party code hosted by a process
            // that owns DRM master. Running it as root defeats the entire
            // privilege split.
            return Err("greeter.user must not be root".to_owned());
        }
        if self.greeter.user.trim().is_empty() {
            return Err("greeter.user must not be empty".to_owned());
        }

        for output in &self.output {
            if output.connector.trim().is_empty() {
                return Err("output.connector must not be empty".to_owned());
            }
            if let Some(scale) = output.scale
                && (!scale.is_finite() || scale <= 0.0)
            {
                return Err(format!(
                    "output {}: scale must be a positive number",
                    output.connector
                ));
            }
        }

        let mut seen = std::collections::HashSet::new();
        for output in &self.output {
            if !seen.insert(&output.connector) {
                return Err(format!(
                    "output {} is configured more than once",
                    output.connector
                ));
            }
        }

        Ok(())
    }

    /// Look up the configuration for a connector, if any.
    pub fn output_for(&self, connector: &str) -> Option<&Output> {
        self.output.iter().find(|o| o.connector == connector)
    }

    /// Assign ranks to the currently connected connectors.
    ///
    /// Configured connectors come first in array order, then connectors with no
    /// configuration sorted by name — deterministic, rather than following
    /// udev probe order, so the primary output does not move between boots.
    /// Disabled outputs are excluded entirely and receive no rank, which is what
    /// makes `enable = false` mean "no password prompt here".
    ///
    /// Returns connector names in rank order; the index is the rank.
    pub fn rank_outputs<'a>(&self, connected: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
        let mut connected: Vec<&str> = connected.into_iter().collect();
        connected.retain(|name| self.output_for(name).is_none_or(|o| o.enable));

        // Rank by configured position, and by name among the unconfigured. The
        // sort is stable, so equal keys keep their relative order, but the name
        // tiebreak makes that irrelevant.
        connected.sort_by_key(|name| {
            let configured = self.output.iter().position(|o| o.connector == *name);
            (configured.unwrap_or(usize::MAX), *name)
        });

        connected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        assert_eq!(
            "2560x1440@144".parse::<Mode>().unwrap(),
            Mode {
                width: 2560,
                height: 1440,
                refresh_mhz: Some(144_000)
            }
        );
        assert_eq!(
            "1920x1080".parse::<Mode>().unwrap(),
            Mode {
                width: 1920,
                height: 1080,
                refresh_mhz: None
            }
        );
        // Real modes are not integral; 59.94 must not round to 60 or it will
        // fail to match what the kernel reports.
        assert_eq!(
            "1920x1080@59.94".parse::<Mode>().unwrap().refresh_mhz,
            Some(59_940)
        );
        assert_eq!(
            "800x600@60Hz".parse::<Mode>().unwrap().refresh_mhz,
            Some(60_000)
        );
    }

    #[test]
    fn rejects_bad_modes() {
        assert!("1920".parse::<Mode>().is_err());
        assert!("0x1080".parse::<Mode>().is_err());
        assert!("1920x1080@0".parse::<Mode>().is_err());
        assert!("1920x1080@abc".parse::<Mode>().is_err());
        assert!("99999x1080".parse::<Mode>().is_err()); // exceeds u16
    }

    #[test]
    fn mode_round_trips_through_display() {
        for s in ["2560x1440@144", "1920x1080", "1920x1080@59.94"] {
            let mode: Mode = s.parse().unwrap();
            assert_eq!(mode.to_string().parse::<Mode>().unwrap(), mode);
        }
    }

    fn config_with_outputs(toml: &str) -> Config {
        let c: Config = toml::from_str(toml).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn array_order_is_rank_order() {
        let c = config_with_outputs(
            r#"
            [[output]]
            connector = "DP-1"
            [[output]]
            connector = "DP-2"
            "#,
        );
        // Probe order deliberately reversed: config wins, not udev.
        assert_eq!(c.rank_outputs(["DP-2", "DP-1"]), ["DP-1", "DP-2"]);
    }

    #[test]
    fn unconfigured_outputs_rank_last_by_name() {
        let c = config_with_outputs(
            r#"
            [[output]]
            connector = "DP-2"
            "#,
        );
        assert_eq!(
            c.rank_outputs(["HDMI-A-1", "eDP-1", "DP-2"]),
            ["DP-2", "HDMI-A-1", "eDP-1"]
        );
    }

    #[test]
    fn disabled_outputs_get_no_rank() {
        let c = config_with_outputs(
            r#"
            [[output]]
            connector = "DP-1"
            [[output]]
            connector = "HDMI-A-1"
            enable = false
            "#,
        );
        assert_eq!(c.rank_outputs(["DP-1", "HDMI-A-1"]), ["DP-1"]);
    }

    #[test]
    fn unplugging_primary_promotes_the_next() {
        let c = config_with_outputs(
            r#"
            [[output]]
            connector = "DP-1"
            [[output]]
            connector = "DP-2"
            "#,
        );
        assert_eq!(c.rank_outputs(["DP-2"]), ["DP-2"]);
    }

    #[test]
    fn ranks_are_empty_when_nothing_is_connected() {
        let c = Config::default();
        assert!(c.rank_outputs([]).is_empty());
    }

    #[test]
    fn full_example_parses() {
        let c = config_with_outputs(
            r#"
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

            [[output]]
            connector = "HDMI-A-1"
            enable = false
            "#,
        );
        assert_eq!(c.vt, 7);
        assert_eq!(c.default_session.as_deref(), Some("hyprland.desktop"));
        assert_eq!(c.output_for("DP-1").unwrap().scale, Some(1.5));
        assert!(!c.output_for("HDMI-A-1").unwrap().enable);
    }

    #[test]
    fn defaults_are_usable() {
        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.vt, DEFAULT_VT);
    }

    #[test]
    fn rejects_root_greeter() {
        let c: Config = toml::from_str(
            r#"
            [greeter]
            command = "/bin/true"
            user = "root"
            "#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_connector() {
        let c: Config = toml::from_str(
            r#"
            [[output]]
            connector = "DP-1"
            [[output]]
            connector = "DP-1"
            "#,
        )
        .unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        // deny_unknown_fields: a typo'd key must fail loudly rather than be
        // silently ignored, since the user believed they configured something.
        assert!(toml::from_str::<Config>("vtt = 7").is_err());
    }
}
