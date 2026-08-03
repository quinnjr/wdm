//! wdm — a Wayland-native display manager.
//!
//! Unlike every other Wayland display manager, wdm does not spawn a kiosk
//! compositor to host its greeter. It binds DRM/KMS itself, hosts the greeter as
//! an ordinary Wayland client, and hands the display to the user's session at
//! login. No nesting, no cage, no X server.

mod auth;
mod backend;
mod comp;
mod config;
mod errscreen;
mod input;
mod login;
mod render;
mod session;
mod sessions;
mod supervise;
mod users;

use std::path::PathBuf;
use std::process::ExitCode;

/// Which backend to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Real hardware: DRM/KMS, libinput, libseat.
    Udev,
    /// Nested inside an existing Wayland session, as a window.
    Winit,
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    config: PathBuf,
    backend: Backend,
}

const USAGE: &str = "\
wdm — Wayland-native display manager

USAGE:
    wdm [OPTIONS]

OPTIONS:
    --config <PATH>     Configuration file [default: /etc/wdm/wdm.toml]
    --backend <NAME>    udev (real hardware) or winit (nested, for development);
                        drm and nested are accepted as aliases
                        [default: udev when running as root, otherwise winit]
    -h, --help          Print this help
    -V, --version       Print version
";

/// What the command line asked for.
///
/// `-h` and `-V` are returned rather than handled where they are parsed: doing
/// the printing and exiting in the caller keeps `parse_args_from` a pure
/// function, so the usage text — which is user-facing contract — can be tested
/// without the test binary calling `exit` on itself.
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Run(Args),
    Help,
    Version,
}

fn parse_args() -> Result<Parsed, String> {
    parse_args_from(std::env::args().skip(1), is_root())
}

/// The parsing itself, separated from `std::env::args` and `geteuid` so tests
/// can feed it arguments and both privilege levels.
fn parse_args_from(
    mut args: impl Iterator<Item = String>,
    is_root: bool,
) -> Result<Parsed, String> {
    // Hand-rolled rather than pulling in an argument parser: there are two
    // options and neither is likely to grow.
    let mut config = None;
    let mut backend = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--config" => {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            "--backend" => match args.next().ok_or("--backend needs a name")?.as_str() {
                "udev" | "drm" => backend = Some(Backend::Udev),
                "winit" | "nested" => backend = Some(Backend::Winit),
                other => return Err(format!("unknown backend {other:?}")),
            },
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }

    Ok(Parsed::Run(Args {
        config: config.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH)),
        // Defaulting on privilege rather than on the environment: running as
        // root inside a session should still drive real hardware, and running
        // unprivileged cannot drive it at all.
        backend: backend.unwrap_or(if is_root {
            Backend::Udev
        } else {
            Backend::Winit
        }),
    }))
}

fn is_root() -> bool {
    // SAFETY: geteuid cannot fail and touches no memory.
    unsafe { libc::geteuid() == 0 }
}

fn main() -> ExitCode {
    // WDM_LOG rather than RUST_LOG so a session's own RUST_LOG cannot
    // reconfigure the display manager's logging.
    env_logger::Builder::from_env(
        env_logger::Env::new()
            .filter("WDM_LOG")
            .write_style("WDM_LOG_STYLE"),
    )
    .format_timestamp_millis()
    .init();

    let args = match parse_args() {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("wdm {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("wdm: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let config = match config::Config::load_or_default(&args.config) {
        Ok(config) => config,
        Err(e) => {
            // A malformed config is fatal: continuing with defaults would
            // silently ignore what someone deliberately configured, including
            // which VT to take and which greeter to trust.
            eprintln!("wdm: {e}");
            return ExitCode::FAILURE;
        }
    };

    log::info!(
        "wdm {} starting on vt {} with the {:?} backend",
        env!("CARGO_PKG_VERSION"),
        config.vt,
        args.backend
    );

    let result = match args.backend {
        Backend::Udev => backend::udev::run(config),
        Backend::Winit => backend::winit::run(config),
    };

    match result {
        Ok(()) => {
            log::info!("wdm exiting");
            ExitCode::SUCCESS
        }
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], is_root: bool) -> Result<Args, String> {
        match parse_args_from(args.iter().map(|s| s.to_string()), is_root)? {
            Parsed::Run(args) => Ok(args),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn parse_raw(args: &[&str]) -> Result<Parsed, String> {
        parse_args_from(args.iter().map(|s| s.to_string()), false)
    }

    #[test]
    fn help_and_version_are_returned_not_executed() {
        // They used to call process::exit here, which no test could reach
        // without taking the test binary down with it.
        assert_eq!(parse_raw(&["-h"]).unwrap(), Parsed::Help);
        assert_eq!(parse_raw(&["--help"]).unwrap(), Parsed::Help);
        assert_eq!(parse_raw(&["-V"]).unwrap(), Parsed::Version);
        assert_eq!(parse_raw(&["--version"]).unwrap(), Parsed::Version);
    }

    #[test]
    fn usage_mentions_everything_the_parser_accepts() {
        // USAGE is the only documentation of the aliases. A flag the parser
        // takes but the help omits is a flag nobody can find.
        for token in [
            "--config",
            "--backend",
            "udev",
            "drm",
            "winit",
            "nested",
            "-h",
            "--help",
            "-V",
            "--version",
        ] {
            assert!(USAGE.contains(token), "USAGE does not mention {token}");
        }
    }

    #[test]
    fn backend_names_and_aliases_parse() {
        for (name, expected) in [
            ("udev", Backend::Udev),
            ("drm", Backend::Udev),
            ("winit", Backend::Winit),
            ("nested", Backend::Winit),
        ] {
            let args = parse(&["--backend", name], false).unwrap();
            assert_eq!(args.backend, expected, "--backend {name}");
        }
    }

    #[test]
    fn unknown_backend_is_an_error() {
        assert!(parse(&["--backend", "x11"], false).is_err());
    }

    #[test]
    fn config_without_a_path_is_an_error() {
        assert!(parse_raw(&["--config"]).is_err());
    }

    #[test]
    fn backend_without_a_name_is_an_error() {
        assert!(parse_raw(&["--backend"]).is_err());
    }

    #[test]
    fn config_path_is_taken_verbatim() {
        let args = parse(&["--config", "/tmp/wdm.toml"], false).unwrap();
        assert_eq!(args.config, PathBuf::from("/tmp/wdm.toml"));
    }

    #[test]
    fn config_defaults_when_not_given() {
        let args = parse(&[], false).unwrap();
        assert_eq!(args.config, PathBuf::from(config::DEFAULT_PATH));
    }

    #[test]
    fn positional_arguments_are_an_error() {
        assert!(parse(&["frobnicate"], false).is_err());
    }

    #[test]
    fn backend_defaults_on_privilege() {
        assert_eq!(parse(&[], true).unwrap().backend, Backend::Udev);
        assert_eq!(parse(&[], false).unwrap().backend, Backend::Winit);
        // An explicit choice wins over the privilege default in both directions.
        assert_eq!(
            parse(&["--backend", "winit"], true).unwrap().backend,
            Backend::Winit
        );
        assert_eq!(
            parse(&["--backend", "udev"], false).unwrap().backend,
            Backend::Udev
        );
    }
}
