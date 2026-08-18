//! Parsing and validation of `/etc/wdm/gtk-greeter.toml`.
//!
//! wdm spawns greeters with a cleared environment, so a file in `/etc/wdm` is
//! the only channel an administrator has to this process besides argv. The
//! file is optional — absent means the built-in defaults — but a file that
//! exists and cannot be understood is a startup error, never a fallback,
//! for the same reason a misspelled theme name is one in the webkit greeter:
//! silently showing the default look hides the administrator's mistake until
//! they are looking at the wrong login screen. The error goes to stderr,
//! where wdm's supervisor turns it into the give-up screen's text.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/wdm/gtk-greeter.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// The `background` key: a solid colour or an image to fill the screen with.
///
/// The colour keeps its `#rrggbb` spelling because everything downstream of
/// it is CSS, which speaks that already.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Background {
    Color(String),
    Image(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub color_scheme: ColorScheme,
    pub background: Option<Background>,
    /// Extra CSS loaded after the built-in stylesheet, so it overrides.
    pub css: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        // Dark is what this greeter has always looked like; the file only
        // ever moves away from the status quo, never silently redefines it.
        Config {
            color_scheme: ColorScheme::Dark,
            background: None,
            css: None,
        }
    }
}

/// What the TOML actually contains, before the strings grow meaning.
///
/// `deny_unknown_fields` because a key this parser does not know is most
/// likely a typo of one it does, and accepting it means the administrator's
/// intent is ignored without a word said.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Raw {
    color_scheme: Option<String>,
    background: Option<String>,
    css: Option<String>,
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
    if let Some(scheme) = raw.color_scheme.as_deref() {
        config.color_scheme = match scheme {
            "dark" => ColorScheme::Dark,
            "light" => ColorScheme::Light,
            other => {
                return Err(format!(
                    "color-scheme must be \"dark\" or \"light\", not {other:?}"
                ));
            }
        };
    }
    if let Some(background) = raw.background.as_deref() {
        config.background = Some(parse_background(background)?);
    }
    if let Some(css) = raw.css.as_deref() {
        if !css.starts_with('/') {
            // Relative would resolve against wdm's working directory, which
            // no administrator controls or should have to know.
            return Err(format!("css must be an absolute path, not {css:?}"));
        }
        config.css = Some(PathBuf::from(css));
    }
    Ok(config)
}

/// `#rrggbb` is a colour, an absolute path is an image, anything else is an
/// error.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_file_is_the_defaults() {
        let config = load(Path::new("/nonexistent/wdm-gtk-greeter-test.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn color_scheme_parses_and_refuses() {
        assert_eq!(
            parse("color-scheme = \"light\"").unwrap().color_scheme,
            ColorScheme::Light
        );
        assert_eq!(parse("").unwrap().color_scheme, ColorScheme::Dark);
        // "Dark" silently meaning dark-by-default is the misspelled-theme bug.
        assert!(parse("color-scheme = \"Dark\"").is_err());
    }

    #[test]
    fn background_color_is_kept_as_css_and_normalised() {
        assert_eq!(
            parse("background = \"#AB12cd\"").unwrap().background,
            Some(Background::Color("#ab12cd".to_owned()))
        );
    }

    #[test]
    fn background_image_must_be_absolute() {
        assert_eq!(
            parse("background = \"/usr/share/wall.jpg\"")
                .unwrap()
                .background,
            Some(Background::Image(PathBuf::from("/usr/share/wall.jpg")))
        );
        assert!(parse("background = \"wall.jpg\"").is_err());
        assert!(parse("background = \"#abcd\"").is_err());
    }

    #[test]
    fn css_must_be_absolute() {
        assert_eq!(
            parse("css = \"/etc/wdm/my.css\"").unwrap().css,
            Some(PathBuf::from("/etc/wdm/my.css"))
        );
        assert!(parse("css = \"my.css\"").is_err());
    }

    #[test]
    fn unknown_keys_and_malformed_toml_are_errors() {
        assert!(
            parse("theme = \"x\"").is_err(),
            "this greeter has no themes"
        );
        assert!(parse("css = ").is_err());
    }
}
