//! Parsing and validation of `/etc/wdm/webkit-greeter.toml`.
//!
//! wdm spawns greeters with a cleared environment, so a file in `/etc/wdm` is
//! the only channel an administrator has to this process besides argv. The
//! file is optional — absent means the built-in defaults — but a file that
//! exists and cannot be understood is a startup error, never a fallback, for
//! the same reason a misspelled `--theme` is one: silently showing the
//! default look hides the administrator's mistake until they are looking at
//! the wrong login screen. The error goes to stderr, where wdm's supervisor
//! turns it into the give-up screen's text.
//!
//! `--theme` on the command line still wins over the file's `theme`, because
//! argv is written per-deployment in `greeter.command` and the file is the
//! distribution-wide layer under it.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

pub const DEFAULT_PATH: &str = "/etc/wdm/webkit-greeter.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// The `background` key: a solid colour or an image.
///
/// Neither is painted by this process — the page owns every pixel. Both are
/// handed to the theme as `window.wdm.config.background`; the colour is also
/// set behind the page, so a theme that leaves its body transparent shows it
/// rather than the compositor's black.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Background {
    /// `#rrggbb`, kept as spelled: everything downstream speaks CSS.
    Color(String),
    Image(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// Theme name or path, same semantics as `--theme`. `None` means the
    /// command line decides, or "default" if it is silent too.
    pub theme: Option<String>,
    pub color_scheme: Option<ColorScheme>,
    pub background: Option<Background>,
}

/// What the TOML actually contains, before the strings grow meaning.
///
/// `deny_unknown_fields` because a key this parser does not know is most
/// likely a typo of one it does, and accepting it means the administrator's
/// intent is ignored without a word said.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Raw {
    theme: Option<String>,
    color_scheme: Option<String>,
    background: Option<String>,
}

/// Read `path`, absent file meaning defaults.
///
/// Every other failure — unreadable, malformed, unknown key, bad value — is
/// an error carrying the path, because the person reading it is looking at a
/// give-up screen and needs to know which file to fix.
pub fn load(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    parse(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn parse(text: &str) -> Result<Config, String> {
    let raw: Raw = toml::from_str(text).map_err(|err| err.to_string())?;

    let mut config = Config::default();
    if let Some(theme) = raw.theme {
        if theme.is_empty() {
            return Err("theme must not be empty".to_owned());
        }
        config.theme = Some(theme);
    }
    if let Some(scheme) = raw.color_scheme.as_deref() {
        config.color_scheme = Some(match scheme {
            "dark" => ColorScheme::Dark,
            "light" => ColorScheme::Light,
            other => {
                return Err(format!(
                    "color-scheme must be \"dark\" or \"light\", not {other:?}"
                ));
            }
        });
    }
    if let Some(background) = raw.background.as_deref() {
        config.background = Some(parse_background(background)?);
    }
    Ok(config)
}

/// `#rrggbb` is a colour, an absolute path is an image, anything else is an
/// error. A relative path could only be relative to wdm's working directory,
/// which no administrator controls or should have to know.
fn parse_background(value: &str) -> Result<Background, String> {
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("background colour must be #rrggbb, not {value:?}"));
        }
        return Ok(Background::Color(value.to_ascii_lowercase()));
    }
    if value.starts_with('/') {
        return Ok(Background::Image(PathBuf::from(value)));
    }
    Err(format!(
        "background must be #rrggbb or an absolute path, not {value:?}"
    ))
}

/// The theme the greeter will show: `--theme` over the file over "default".
pub fn pick_theme(cli: Option<String>, config: &Config) -> String {
    cli.or_else(|| config.theme.clone())
        .unwrap_or_else(|| "default".to_owned())
}

/// A `<script>` that hangs the administrator's choices off `window.wdm`.
///
/// Injected at document start *after* the API script, so themes read
/// `wdm.config` the same way they read `wdm.users`: synchronously, from their
/// own top-level code. Built with serde_json so a hostile-looking path cannot
/// break out of the string literal — the file is root-owned, but the page is
/// the one context in this process where escaping mistakes become script.
pub fn script(config: &Config) -> String {
    let color_scheme = match config.color_scheme {
        // The shipped themes are dark-first, so absent means dark — but the
        // page can tell "chosen" from "defaulted" by whether it got null.
        None => json!(null),
        Some(ColorScheme::Dark) => json!("dark"),
        Some(ColorScheme::Light) => json!("light"),
    };
    let background = match &config.background {
        None => json!(null),
        Some(Background::Color(color)) => json!(color),
        // A URL rather than a path: the page can only use it as one, in an
        // <img> or a CSS url(), both of which the CSP's `file:` allows.
        // filename_to_uri and not `format!("file://{path}")` — percent
        // encoding is what keeps a space or `#` in the path from resolving
        // to the wrong file, the exact bug
        // `the_initial_uri_survives_a_space_in_the_path` pins for the theme
        // URI. The parser guaranteed the path is absolute, which is the only
        // way this conversion fails.
        Some(Background::Image(path)) => json!(
            gtk4::glib::filename_to_uri(path, None)
                .expect("parse_background only accepts absolute paths")
                .as_str()
        ),
    };
    format!(
        "window.wdm.config = {};\n",
        json!({
            "color_scheme": color_scheme,
            "background": background,
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_file_is_the_defaults() {
        let config = load(Path::new("/nonexistent/wdm-webkit-greeter-test.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn every_key_parses() {
        let config =
            parse("theme = \"arch\"\ncolor-scheme = \"light\"\nbackground = \"#AABBCC\"").unwrap();
        assert_eq!(config.theme.as_deref(), Some("arch"));
        assert_eq!(config.color_scheme, Some(ColorScheme::Light));
        assert_eq!(
            config.background,
            Some(Background::Color("#aabbcc".to_owned()))
        );
    }

    #[test]
    fn bad_values_are_errors_not_fallbacks() {
        assert!(parse("theme = \"\"").is_err());
        assert!(parse("color-scheme = \"Dark\"").is_err());
        assert!(parse("background = \"wall.png\"").is_err());
        assert!(parse("background = \"#abc\"").is_err());
        assert!(parse("them = \"default\"").is_err(), "typo must not pass");
    }

    #[test]
    fn the_command_line_beats_the_file_beats_the_default() {
        let file = Config {
            theme: Some("arch".to_owned()),
            ..Config::default()
        };
        assert_eq!(pick_theme(Some("react".to_owned()), &file), "react");
        assert_eq!(pick_theme(None, &file), "arch");
        assert_eq!(pick_theme(None, &Config::default()), "default");
    }

    #[test]
    fn the_script_exposes_choices_and_defaults_as_null() {
        let generated = script(&Config::default());
        assert!(
            generated.contains("\"color_scheme\":null")
                && generated.contains("\"background\":null"),
            "{generated}"
        );

        let generated = script(&Config {
            theme: None,
            color_scheme: Some(ColorScheme::Light),
            background: Some(Background::Image("/usr/share/wall.jpg".into())),
        });
        assert!(
            generated.contains("\"color_scheme\":\"light\""),
            "{generated}"
        );
        assert!(
            generated.contains("\"background\":\"file:///usr/share/wall.jpg\""),
            "{generated}"
        );
    }

    #[test]
    fn a_hostile_path_cannot_break_out_of_the_script() {
        let generated = script(&Config {
            theme: None,
            color_scheme: None,
            background: Some(Background::Image("/tmp/\"};alert(1);//wall.png".into())),
        });
        // filename_to_uri percent-encodes the quote, so nothing can end the
        // string literal and run as script — and serde_json would escape it
        // even if one slipped through.
        assert!(!generated.contains("/tmp/\"}"), "{generated}");
        assert!(generated.contains("%22"), "{generated}");
    }

    #[test]
    fn a_background_path_with_a_space_survives_as_a_uri() {
        // The same regression `the_initial_uri_survives_a_space_in_the_path`
        // pins for the theme's own URI: format!("file://{path}") loses the
        // file the moment the path has a space or a `#` in it.
        let generated = script(&Config {
            theme: None,
            color_scheme: None,
            background: Some(Background::Image("/srv/wall #2.png".into())),
        });
        assert!(
            generated.contains("file:///srv/wall%20%232.png"),
            "{generated}"
        );
    }
}
