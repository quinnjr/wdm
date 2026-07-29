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
    --backend <NAME>    udev (real hardware) or winit (nested, for development)
                        [default: udev when running as root, otherwise winit]
    -h, --help          Print this help
    -V, --version       Print version
";

fn parse_args() -> Result<Args, String> {
    // Hand-rolled rather than pulling in an argument parser: there are two
    // options and neither is likely to grow.
    let mut config = None;
    let mut backend = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("wdm {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
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

    Ok(Args {
        config: config.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH)),
        // Defaulting on privilege rather than on the environment: running as
        // root inside a session should still drive real hardware, and running
        // unprivileged cannot drive it at all.
        backend: backend.unwrap_or(if is_root() {
            Backend::Udev
        } else {
            Backend::Winit
        }),
    })
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
        Ok(args) => args,
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
