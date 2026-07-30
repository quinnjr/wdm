//! A WebKitGTK greeter for wdm, themed in HTML, CSS and JavaScript.
//!
//! The same idea as `lightdm-webkit2-greeter`: the login screen is a web page,
//! and `window.wdm` is the API it drives the display manager through. A theme
//! is a directory with an `index.html` in it and nothing else required.
//!
//! WebKitGTK 6.0 is a GTK4 widget, so this is the GTK greeter's window with a
//! `WebView` where its form was — layer shell, GDK's Wayland connection, and
//! `wdm-greeter-client` for the protocol are all shared with it. What is not
//! shared is the policy: this greeter has no opinion about retrying, about
//! which session to preselect, or about what a locked account should look like.
//! Those are the theme's to decide, and a greeter that decided them anyway
//! would be fighting every theme that disagreed.
//!
//! ## The page is not trusted with the process
//!
//! A theme is a file an administrator installs, but it is still a web page, and
//! the failure modes of one are not hypothetical: it can be given text by PAM,
//! it can contain a mistake, and it runs on a machine nobody has logged into
//! yet. So:
//!
//! - Every value crossing into the page is a `serde_json` literal, never a
//!   concatenation. PAM's prompt text reaches JavaScript verbatim otherwise.
//! - Navigation is refused unless it stays inside the theme directory, which
//!   makes a theme that links to the internet fail visibly instead of turning
//!   the login screen into a browser.
//! - The web process gets no persistent storage, no context menu, and no
//!   developer tools unless `WDM_GREETER_DEBUG` is set.

mod bridge;

// See wdm-gtk-greeter's crate root: gtk4-layer-shell interposes libwayland
// symbols and only takes effect if it is loaded first, which is what declaring
// the link here — rather than from a build script — achieves.
#[link(name = "gtk4-layer-shell")]
unsafe extern "C" {}

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use webkit6::prelude::*;
use webkit6::{NavigationPolicyDecision, PolicyDecisionType, WebView};

use bridge::{Bridge, Request};
use wdm_greeter_client::{Link, Model, SharedLink};

const APP_ID: &str = "ai.lexmata.wdm.WebkitGreeter";

/// Where themes live when one is named rather than given as a path.
const THEME_ROOT: &str = "/usr/share/wdm/webkit-greeter/themes";

/// See `wdm-gtk-greeter`: GDK owns the connection's fd and reads from it, so
/// the queue is polled rather than watched.
const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::new().filter("WDM_GREETER_LOG")).init();

    let theme = match theme_directory() {
        Ok(theme) => theme,
        Err(e) => {
            log::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    log::info!("theme: {}", theme.display());

    let app = Application::builder().application_id(APP_ID).build();
    let failed = Rc::new(std::cell::Cell::new(false));

    app.connect_activate({
        let failed = failed.clone();
        let theme = theme.clone();
        move |app| {
            if let Err(e) = activate(app, &theme) {
                log::error!("{e}");
                failed.set(true);
                app.quit();
            }
        }
    });

    // Arguments are ours, not GTK's — it would try to interpret --theme.
    let status = app.run_with_args::<&str>(&[]);

    if failed.get() || status != glib::ExitCode::SUCCESS {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Resolve `--theme`, which is a name under [`THEME_ROOT`] or a path.
///
/// Failing here rather than falling back to a built-in is deliberate: a
/// misspelled theme that silently shows something else is a configuration bug
/// nobody notices until they are looking at the wrong login screen.
fn theme_directory() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut name = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--theme" => name = args.next(),
            other => match other.strip_prefix("--theme=") {
                Some(value) => name = Some(value.to_owned()),
                None => return Err(format!("unrecognised argument: {other}")),
            },
        }
    }

    let name = name.unwrap_or_else(|| "default".to_owned());
    let dir = if name.contains('/') {
        PathBuf::from(&name)
    } else {
        Path::new(THEME_ROOT).join(&name)
    };

    // Canonical, because it is also the boundary navigation is checked against
    // and a relative path would not compare against a URI's.
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("theme {name}: {} : {e}", dir.display()))?;

    if !dir.join("index.html").is_file() {
        return Err(format!("theme {name} has no index.html in {}", dir.display()));
    }

    Ok(dir)
}

fn activate(app: &Application, theme: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let display = gtk4::gdk::Display::default().ok_or("no display")?;
    let (link, model) = Link::connect(&display)?;
    log::info!(
        "connected: {} user(s), {} session(s)",
        model.users.len(),
        model.sessions.len()
    );

    let webview = build_webview(&model, theme);
    let window = build_window(app, &webview);

    let model: wdm_greeter_client::Shared = Rc::new(RefCell::new(model));
    let link: SharedLink = Rc::new(RefCell::new(link));
    let bridge = Rc::new(RefCell::new(Bridge::default()));

    connect_requests(&webview, model.clone(), link.clone(), bridge.clone());
    window.present();

    glib::timeout_add_local(PUMP_INTERVAL, move || {
        let changed = {
            let mut model = model.borrow_mut();
            link.borrow_mut().pump(&mut model)
        };

        if changed {
            // Borrows are released before evaluate_javascript: the page's reply
            // arrives through the message handler, which borrows both again.
            let script = {
                let model = model.borrow();
                bridge.borrow_mut().diff(&model).join("\n")
            };
            if !script.is_empty() {
                evaluate(&webview, &script);
            }
        }

        glib::ControlFlow::Continue
    });

    Ok(())
}

fn build_window(app: &Application, webview: &WebView) -> ApplicationWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .child(webview)
        .build();

    // Layer shell is not optional — wdm exposes no xdg_toplevel, so a plain
    // window is closed the moment it is created.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }

    window
}

fn build_webview(model: &Model, theme: &Path) -> WebView {
    let content = webkit6::UserContentManager::new();

    // At document-start, so a theme's own top-level script can read wdm.users
    // instead of having to wait for a callback.
    content.add_script(&webkit6::UserScript::new(
        &bridge::api_script(model),
        webkit6::UserContentInjectedFrames::TopFrame,
        webkit6::UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content.register_script_message_handler("wdm", None);

    // Ephemeral: a login screen has nothing worth persisting, and a cache that
    // outlives the greeter is a cache that can serve a stale theme.
    let network = webkit6::NetworkSession::new_ephemeral();
    let webview = WebView::builder()
        .user_content_manager(&content)
        .network_session(&network)
        .build();

    let settings = webkit6::prelude::WebViewExt::settings(&webview).unwrap_or_default();
    settings.set_enable_developer_extras(std::env::var_os("WDM_GREETER_DEBUG").is_some());
    // A theme's assets are loaded as subresources, which do not need this; what
    // does need it is a theme reading files through fetch(), and "any file the
    // greeter user can read" is not a capability a login page should have.
    settings.set_allow_file_access_from_file_urls(false);
    settings.set_allow_universal_access_from_file_urls(false);
    webkit6::prelude::WebViewExt::set_settings(&webview, &settings);

    // Nothing behind the page but the compositor's own background.
    webview.set_background_color(&gtk4::gdk::RGBA::new(0.07, 0.07, 0.10, 1.0));

    // No context menu: right-clicking a login screen should not offer to
    // reload, inspect, or open anything.
    webview.connect_context_menu(|_, _, _| true);

    refuse_navigation_outside(&webview, theme);

    webview.load_uri(&format!("file://{}/index.html", theme.display()));
    webview
}

/// Keep the page inside the theme directory.
///
/// A theme is meant to be self-contained. One that navigates elsewhere — a
/// stray link, a redirect, a mistake — would otherwise replace the login screen
/// with whatever it reached, on a machine at a login prompt.
fn refuse_navigation_outside(webview: &WebView, theme: &Path) {
    let root = format!("file://{}/", theme.display());
    webview.connect_decide_policy(move |_, decision, kind| {
        if !matches!(
            kind,
            PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction
        ) {
            return false;
        }

        let Some(decision) = decision.downcast_ref::<NavigationPolicyDecision>() else {
            return false;
        };
        let Some(uri) = decision
            .navigation_action()
            .and_then(|a| a.request())
            .and_then(|r| r.uri())
        else {
            return false;
        };

        if uri.starts_with(&root) {
            return false;
        }

        log::warn!("refusing navigation to {uri}");
        decision.ignore();
        true
    });
}

/// Apply what the page asked for.
fn connect_requests(
    webview: &WebView,
    model: wdm_greeter_client::Shared,
    link: SharedLink,
    bridge: Rc<RefCell<Bridge>>,
) {
    let Some(content) = webkit6::prelude::WebViewExt::user_content_manager(webview) else {
        log::error!("no user content manager; the theme cannot talk back");
        return;
    };

    content.connect_script_message_received(Some("wdm"), move |_, value| {
        let Some(request) = bridge::parse(&value.to_str()) else {
            log::warn!("ignoring unrecognised message from the theme");
            return;
        };

        let mut model = model.borrow_mut();
        let Some(greeter) = model.greeter.clone() else {
            return;
        };

        match request {
            Request::Authenticate(user) => {
                // Cancel first: wdm refuses create_session while a conversation
                // is live, and a theme is allowed to change its mind.
                greeter.cancel();
                greeter.create_session(user);
                model.begin_attempt();
                bridge.borrow_mut().restart();
            }
            Request::Respond(text) => match model.prompt.take() {
                Some(prompt) => greeter.respond(prompt.id, text),
                // The page's own check should have caught this; a stale answer
                // would be matched against a prompt id that no longer exists.
                None => log::warn!("theme answered a prompt that is not pending"),
            },
            Request::Cancel => {
                greeter.cancel();
                model.begin_attempt();
                bridge.borrow_mut().restart();
            }
            Request::StartSession(id) => {
                log::info!("starting session {id}");
                greeter.start_session(id, Vec::new());
            }
        }

        link.borrow_mut().pump(&mut model);
    });
}

/// Run script in the page, logging what it says if it goes wrong.
fn evaluate(webview: &WebView, script: &str) {
    webview.evaluate_javascript(
        script,
        None,
        None,
        None::<&gtk4::gio::Cancellable>,
        |result| {
            if let Err(e) = result {
                log::warn!("theme script: {e}");
            }
        },
    );
}
