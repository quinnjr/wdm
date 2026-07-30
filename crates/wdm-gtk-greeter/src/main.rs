//! A GTK4 greeter for wdm.
//!
//! Talks `wdm_greeter_v1` over the connection GDK already owns (see [`proto`]),
//! and puts its window on screen with `gtk4-layer-shell`. Layer shell is not
//! optional: wdm exposes no `xdg_toplevel` at all, so an ordinary GTK window is
//! closed as soon as it is created.
//!
//! The reference greeter in `wdm-greeter` stays the shipped default — it has no
//! toolkit dependency and proves the protocol is implementable from scratch.
//! This one exists because a real deployment usually wants a themeable greeter,
//! and because it demonstrates the protocol works from a toolkit that owns its
//! own event loop and its own Wayland connection.

mod proto;
mod ui;

// gtk4-layer-shell works by interposing a handful of libwayland-client symbols,
// which only takes effect if it is loaded first. Declaring the link here rather
// than in build.rs is what puts it ahead of libwayland-client in DT_NEEDED:
// rustc emits the root crate's native libraries before its dependencies', while
// build-script link args land at the very end of the link line.
//
// Get this wrong and the library silently does nothing, every window comes up as
// an ordinary toplevel, and against wdm — which exposes no xdg_toplevel — the
// greeter is closed the moment it appears, showing the user nothing at all.
#[link(name = "gtk4-layer-shell")]
unsafe extern "C" {}

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, glib};

use proto::Link;

const APP_ID: &str = "ai.lexmata.wdm.Greeter";

/// How often queued Wayland events are delivered to the model.
///
/// A timeout rather than a watch on the connection's fd: GDK owns that fd and
/// reads from it, so by the time our watch would run the fd is often no longer
/// readable even though libwayland has queued events for us. Polling sidesteps
/// the question entirely, and at 16ms on a login screen it costs nothing worth
/// measuring.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::new().filter("WDM_GREETER_LOG")).init();

    let app = Application::builder().application_id(APP_ID).build();

    // Exit code is decided by whether the connection came up, which is only
    // known inside activate; a cell carries it back out.
    let failed = Rc::new(std::cell::Cell::new(false));

    app.connect_activate({
        let failed = failed.clone();
        move |app| {
            if let Err(e) = activate(app) {
                log::error!("{e}");
                failed.set(true);
                app.quit();
            }
        }
    });

    // No command line handling: wdm launches this with a fixed command, and GTK
    // would otherwise try to interpret arguments meant for it.
    let status = app.run_with_args::<&str>(&[]);

    if failed.get() || status != glib::ExitCode::SUCCESS {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn activate(app: &Application) -> Result<(), Box<dyn std::error::Error>> {
    let display = gtk4::gdk::Display::default().ok_or("no display")?;
    let wayland = display
        .downcast_ref::<gdk4_wayland::WaylandDisplay>()
        .ok_or("wdm-gtk-greeter requires a Wayland session")?;

    let (link, model) = Link::connect(wayland)?;

    log::info!(
        "connected: {} user(s), {} session(s)",
        model.users.len(),
        model.sessions.len()
    );

    let model: proto::Shared = Rc::new(RefCell::new(model));
    let link: proto::SharedLink = Rc::new(RefCell::new(link));

    let (window, ui) = ui::build(app, model.clone(), link.clone());
    window.present();

    // Wayland events arrive independently of GTK's own event sources, so the
    // model is pumped on a timer and the UI refreshed only when it changed.
    glib::timeout_add_local(PUMP_INTERVAL, move || {
        let changed = {
            let mut model = model.borrow_mut();
            link.borrow_mut().pump(&mut model)
        };

        if changed {
            ui.refresh();
        }

        glib::ControlFlow::Continue
    });

    Ok(())
}
