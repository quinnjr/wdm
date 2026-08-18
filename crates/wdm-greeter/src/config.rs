//! Parsing and validation of `/etc/wdm/greeter.toml`.
//!
//! wdm spawns greeters with a cleared environment, so a file in `/etc/wdm` is
//! the only channel an administrator has to this process besides argv. The
//! file is optional — absent means the built-in defaults — but a file that
//! exists and cannot be understood is a startup error, never a fallback: a
//! misread config that silently shows the default look is the same bug as a
//! misspelled theme name, and nobody notices until they are looking at the
//! wrong login screen. The error goes to stderr, where wdm's supervisor turns
//! it into the give-up screen's text.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where the file lives. Not overridable: the greeter runs from
/// `greeter.command`, and anyone wanting a different file can say so there
/// once this grows an argument for it — so far nothing has needed one.
pub const DEFAULT_PATH: &str = "/etc/wdm/greeter.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

/// The `background` key: a solid colour or a PNG to fill the screen with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Background {
    /// 0xAARRGGBB, alpha always opaque — what [`crate::text::Canvas`] holds.
    Color(u32),
    Image(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub color_scheme: ColorScheme,
    pub background: Option<Background>,
}

impl Default for Config {
    fn default() -> Self {
        // Dark is what this greeter has always painted; the file only ever
        // moves away from the status quo, never silently redefines it.
        Config {
            color_scheme: ColorScheme::Dark,
            background: None,
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
        let rgb = u32::from_str_radix(hex, 16).expect("checked hexdigits");
        return Ok(Background::Color(0xff00_0000 | rgb));
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
        let config = load(Path::new("/nonexistent/wdm-greeter-test.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn color_scheme_parses_both_values() {
        assert_eq!(
            parse("color-scheme = \"light\"").unwrap().color_scheme,
            ColorScheme::Light
        );
        assert_eq!(
            parse("color-scheme = \"dark\"").unwrap().color_scheme,
            ColorScheme::Dark
        );
    }

    #[test]
    fn color_scheme_refuses_anything_else() {
        // "Light" silently meaning dark is the misspelled-theme bug again.
        let err = parse("color-scheme = \"Light\"").unwrap_err();
        assert!(err.contains("Light"), "{err}");
    }

    #[test]
    fn background_hex_becomes_an_opaque_color() {
        assert_eq!(
            parse("background = \"#1a2b3c\"").unwrap().background,
            Some(Background::Color(0xff1a2b3c))
        );
    }

    #[test]
    fn background_absolute_path_becomes_an_image() {
        assert_eq!(
            parse("background = \"/usr/share/wall.png\"")
                .unwrap()
                .background,
            Some(Background::Image(PathBuf::from("/usr/share/wall.png")))
        );
    }

    #[test]
    fn background_refuses_short_hex_and_relative_paths() {
        assert!(parse("background = \"#abc\"").is_err());
        assert!(parse("background = \"#gggggg\"").is_err());
        assert!(parse("background = \"wall.png\"").is_err());
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = parse("colour-scheme = \"dark\"").unwrap_err();
        assert!(err.contains("colour-scheme"), "{err}");
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse("color-scheme = ").is_err());
    }

    #[test]
    fn errors_from_a_real_file_name_the_file() {
        let dir = std::env::temp_dir().join(format!("wdm-greeter-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("greeter.toml");
        std::fs::write(&path, "background = \"nope\"").unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("greeter.toml"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
