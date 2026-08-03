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

/// The one message that is delivered outside the bridge's queue, and the
/// invariant that it is delivered at most once.
///
/// The text is an error already in the model at connect — the parting message
/// of a session that failed. It is held here rather than in the model because
/// the pump only diffs on a revision *change*, and the default theme's first
/// `authenticate()` wipes it before the page has finished loading.
///
/// One-shot is the whole reason this is not a plain clone: a theme reload
/// re-fires load-finished, and resurrecting "your session crashed" on every
/// reload is a lie about the current boot. But taking it on *send* would
/// destroy it if the evaluation then fails — which is the state the queue
/// exists for — so a failed send [`restore`](Self::restore)s it for the next
/// load-finished — or pump tick — to retry.
#[derive(Default)]
struct StartupError {
    text: RefCell<Option<String>>,
    /// Whether a page has finished loading. Before that there is no theme to
    /// call `show_message` on, so the pump must not try.
    armed: std::cell::Cell<bool>,
}

impl StartupError {
    fn new(text: Option<String>) -> Self {
        Self {
            text: RefCell::new(text),
            armed: std::cell::Cell::new(false),
        }
    }

    /// Note that a page has loaded, so the message may now be sent.
    fn arm(&self) {
        self.armed.set(true);
    }

    /// The message, if a page is loaded and it has not already been delivered.
    fn take(&self) -> Option<String> {
        if !self.armed.get() {
            return None;
        }
        self.text.borrow_mut().take()
    }

    /// Put a message back after its evaluation failed.
    fn restore(&self, text: String) {
        *self.text.borrow_mut() = Some(text);
    }
}

/// Whether a URI resolves inside the canonical theme directory.
///
/// The navigation policy's one question. Written as a pure function so the
/// security boundary is unit-testable instead of living in a webview callback.
///
/// It refuses anything that is not a local `file://` URI, then checks the
/// decoded filesystem path — not the literal URI string — against the canonical
/// theme directory. Comparing strings was how a `..` or percent-encoded
/// (`%2e%2e`) traversal escaped to an arbitrary readable file.
fn within_theme(uri: &str, theme: &Path) -> bool {
    // g_filename_from_uri alone both rejects non-file schemes and refuses a
    // remote hostname; a `file://` URI with a host is not ours.
    let Ok((path, host)) = glib::filename_from_uri(uri) else {
        return false;
    };
    if host.as_deref().is_some_and(|h| !h.is_empty()) {
        return false;
    }

    // Normalise against the same canonical theme the greeter is serving. A
    // `..` or `%2e%2e` in the URI is resolved here, by the filesystem, not
    // matched as text. A path that cannot be canonicalised is refused rather
    // than compared raw: `starts_with` on an uncanonicalised path treats `..`
    // as an ordinary component, which is exactly the comparison this function
    // exists to not make.
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.starts_with(theme)
}

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
            if let Err(e) = activate(app, &theme, failed.clone()) {
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
    let name = theme_argument(std::env::args().skip(1))?;
    let name = name.unwrap_or_else(|| "default".to_owned());
    resolve_theme(&name, Path::new(THEME_ROOT))
}

/// Pull `--theme` out of the argument list.
///
/// A trailing `--theme` with no value and a repeated `--theme` are both errors
/// for the same reason a misspelled name is: falling back to the default, or
/// quietly taking the later value, shows a login screen other than the one that
/// was asked for.
fn theme_argument(mut args: impl Iterator<Item = String>) -> Result<Option<String>, String> {
    let mut name: Option<String> = None;

    while let Some(arg) = args.next() {
        let value = match arg.as_str() {
            "--theme" => args
                .next()
                .ok_or_else(|| "--theme needs a value".to_owned())?,
            other => match other.strip_prefix("--theme=") {
                Some(value) => value.to_owned(),
                None => return Err(format!("unrecognised argument: {other}")),
            },
        };
        if name.replace(value).is_some() {
            return Err("--theme given more than once".to_owned());
        }
    }

    Ok(name)
}

/// Turn a theme name — or a path, distinguished by containing a slash — into
/// its canonical directory.
fn resolve_theme(name: &str, root: &Path) -> Result<PathBuf, String> {
    let dir = if name.contains('/') {
        PathBuf::from(name)
    } else {
        root.join(name)
    };

    // Canonical, because it is also the boundary navigation is checked against
    // and a relative path would not compare against a URI's.
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("theme {name}: {} : {e}", dir.display()))?;

    if !dir.join("index.html").is_file() {
        return Err(format!(
            "theme {name} has no index.html in {}",
            dir.display()
        ));
    }

    Ok(dir)
}

fn activate(
    app: &Application,
    theme: &Path,
    failed: Rc<std::cell::Cell<bool>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let display = gtk4::gdk::Display::default().ok_or("no display")?;
    let (link, model) = Link::connect(&display)?;
    log::info!(
        "connected: {} user(s), {} session(s)",
        model.users.len(),
        model.sessions.len()
    );

    // Snapshot the parting error now, before the page can post anything, and
    // deliver it on load-finished — the first moment the theme's show_message
    // is guaranteed to exist. See StartupError for why it is held there.
    let startup_error = Rc::new(StartupError::new(model.error.clone()));

    let webview = build_webview(&model, theme)?;
    let window = build_window(app, &webview);

    let model: wdm_greeter_client::Shared = Rc::new(RefCell::new(model));
    let link: SharedLink = Rc::new(RefCell::new(link));
    let bridge = Rc::new(RefCell::new(Bridge::default()));

    connect_requests(&webview, model.clone(), link.clone(), bridge.clone());

    webview.connect_load_changed({
        let model = model.clone();
        let bridge = bridge.clone();
        let startup_error = startup_error.clone();
        move |webview, event| {
            if event != webkit6::LoadEvent::Finished {
                return;
            }

            // Directly, not through the queue: the theme's own authenticate()
            // triggers bridge.restart(), which would drop a queued message, and
            // whether that message arrives before or after this signal is a
            // race. Taken once, so a later reload does not resurrect it — but
            // only kept taken if it actually arrived, because this is the
            // compositor's account of why the last session died and a web
            // process mid-crash is exactly when it is worth having.
            startup_error.arm();
            if let Some(text) = startup_error.take() {
                let startup_error = startup_error.clone();
                webview.evaluate_javascript(
                    &bridge::error_script(&text),
                    None,
                    None,
                    None::<&gtk4::gio::Cancellable>,
                    move |result| {
                        if let Err(e) = result {
                            log::warn!("startup error message: {e}");
                            startup_error.restore(text);
                        }
                    },
                );
            }

            // And any other state the model held before the pump timer saw its
            // first change goes through the normal queue.
            let model = model.borrow();
            bridge.borrow_mut().diff(&model);
        }
    });

    window.present();

    let app = app.clone();
    glib::timeout_add_local(PUMP_INTERVAL, move || {
        // A page that has answered nothing for about a second is not coming
        // back on its own, and a greeter process that stays alive in front of
        // a dead login screen is invisible to wdm's supervisor. Exiting with
        // failure is what turns it into a respawn, or into wdm's give-up
        // screen.
        if bridge.borrow().is_wedged() {
            log::error!("the theme has stopped responding; exiting so wdm can respawn the greeter");
            failed.set(true);
            app.quit();
            return glib::ControlFlow::Break;
        }

        // A startup error whose evaluation failed is still owed to the user.
        if let Some(text) = startup_error.take() {
            let startup_error = startup_error.clone();
            webview.evaluate_javascript(
                &bridge::error_script(&text),
                None,
                None,
                None::<&gtk4::gio::Cancellable>,
                move |result| {
                    if result.is_err() {
                        startup_error.restore(text);
                    }
                },
            );
        }

        let changed = {
            let mut model = model.borrow_mut();
            link.borrow_mut().pump(&mut model)
        };

        if changed {
            let model = model.borrow();
            bridge.borrow_mut().diff(&model);
        }

        // Flushed every tick, not only on change: a script the page failed to
        // run stays queued, and this is where it is retransmitted.
        //
        // Borrows are released before evaluate_javascript: the page's reply
        // arrives through the message handler, which borrows model and bridge
        // again.
        let outbound = bridge.borrow_mut().flush();
        if let Some(outbound) = outbound {
            evaluate(&webview, &bridge, &outbound);
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

fn build_webview(model: &Model, theme: &Path) -> Result<WebView, Box<dyn std::error::Error>> {
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

    // The navigation policy stops the page *going* anywhere, but subresources —
    // <script src>, <img>, CSS url(), fetch, WebSockets — never reach it. This
    // is what stops those: nothing loads except local files and data: URIs, so
    // a theme cannot pull code off the network or exfiltrate what the user
    // typed. Inline script and style stay allowed; a theme is one directory of
    // hand-written files, and everything reaching the page from outside is
    // escaped into string literals by the bridge.
    //
    // `form-action`, `base-uri` and `frame-ancestors` are spelled out because
    // they are the directives that do *not* fall back to `default-src`. Without
    // them a `<form action="https://…">` is caught only by the navigation
    // policy — which stops mattering the moment WebKit routes a submission
    // through a decision type outside that callback's `matches!` — and a
    // `<base href>` is not constrained at all. `object-src` inherits, but is
    // named too: plugins have no business on a login screen either way.
    let webview = WebView::builder()
        .user_content_manager(&content)
        .network_session(&network)
        .default_content_security_policy(
            "default-src file: data: 'unsafe-inline'; \
             form-action 'none'; base-uri 'none'; frame-ancestors 'none'; object-src 'none'",
        )
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

    // g_filename_to_uri, the exact inverse of the g_filename_from_uri that
    // within_theme decodes with. Formatting the path into a string was wrong
    // for any theme directory containing a space, `#` or `?` — the resulting
    // URI failed its own navigation check and the login screen went blank.
    let uri = glib::filename_to_uri(theme.join("index.html"), None)?;
    webview.load_uri(&uri);
    Ok(webview)
}

/// Keep the page inside the theme directory.
///
/// A theme is meant to be self-contained. One that navigates elsewhere — a
/// stray link, a redirect, a mistake — would otherwise replace the login screen
/// with whatever it reached, on a machine at a login prompt.
fn refuse_navigation_outside(webview: &WebView, theme: &Path) {
    let theme = theme.to_owned();
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

        if within_theme(&uri, &theme) {
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

/// Run script in the page, telling the bridge whether it succeeded.
///
/// The verdict is what keeps events alive: a failed evaluation leaves the
/// statements queued in the bridge and the next pump sends them again, instead
/// of a wedged web process silently eating an `authentication_complete`.
///
/// The epoch travels with the script so a verdict that arrives after the theme
/// started another attempt is recognised as belonging to the conversation it
/// was sent for, and ignored.
fn evaluate(webview: &WebView, bridge: &Rc<RefCell<Bridge>>, outbound: &bridge::Outbound) {
    let bridge = bridge.clone();
    let epoch = outbound.epoch;
    webview.evaluate_javascript(
        &outbound.script,
        None,
        None,
        None::<&gtk4::gio::Cancellable>,
        move |result| {
            let error = result.as_ref().err().map(|e| e.to_string());
            let mut bridge = bridge.borrow_mut();
            bridge.delivered(epoch, result.is_ok());
            // Once, not every tick: a page that never recovers is retried
            // sixty times a second, and sixty warnings a second in the journal
            // is how a symptom becomes the whole log.
            if let Some(e) = error
                && bridge.consecutive_failures() == 1
            {
                log::warn!("theme script: {e}");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A theme directory with an asset in it, plus a secret outside it that
    /// every traversal below is trying to reach.
    fn theme() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let theme = dir.path().join("theme");
        std::fs::create_dir(&theme).unwrap();
        std::fs::write(theme.join("index.html"), "<html></html>").unwrap();
        std::fs::write(dir.path().join("secret"), "hunter2").unwrap();
        // Canonical, as theme_directory() guarantees for the real one.
        let canonical = theme.canonicalize().unwrap();
        (dir, canonical)
    }

    fn uri(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    #[test]
    fn stays_inside_the_theme() {
        let (_dir, theme) = theme();
        assert!(within_theme(&uri(&theme.join("index.html")), &theme));
    }

    #[test]
    fn refuses_dot_dot_traversal() {
        let (_dir, theme) = theme();
        assert!(!within_theme(&uri(&theme.join("../secret")), &theme));
    }

    #[test]
    fn refuses_percent_encoded_traversal() {
        // The regression this module exists for: the check used to be
        // starts_with on the URI string, which %2e%2e walked straight past.
        let (_dir, theme) = theme();
        let uri = format!("file://{}/%2e%2e/secret", theme.display());
        assert!(!within_theme(&uri, &theme));
    }

    #[test]
    fn refuses_a_symlink_pointing_out() {
        let (_dir, theme) = theme();
        std::os::unix::fs::symlink(theme.parent().unwrap().join("secret"), theme.join("link"))
            .unwrap();
        assert!(!within_theme(&uri(&theme.join("link")), &theme));
    }

    #[test]
    fn refuses_other_schemes_and_remote_hosts() {
        let (_dir, theme) = theme();
        assert!(!within_theme("https://example.com/", &theme));
        assert!(!within_theme("data:text/html,hi", &theme));
        assert!(!within_theme("about:blank", &theme));

        // The host branch, asserted as a precondition first. Without this the
        // case below passes whether the host check rejected it or
        // filename_from_uri errored out before the check was ever reached —
        // which would leave that branch uncovered while looking covered.
        let remote = format!("file://evil.example{}/index.html", theme.display());
        let (_, host) = glib::filename_from_uri(&remote).expect("a file: URI with a host parses");
        assert_eq!(host.as_deref(), Some("evil.example"));
        assert!(!within_theme(&remote, &theme));
    }

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn theme_argument_parses_both_forms() {
        assert_eq!(theme_argument(args(&[])).unwrap(), None);
        assert_eq!(
            theme_argument(args(&["--theme=x"])).unwrap(),
            Some("x".to_owned())
        );
        assert_eq!(
            theme_argument(args(&["--theme", "x"])).unwrap(),
            Some("x".to_owned())
        );
    }

    #[test]
    fn theme_argument_refuses_a_trailing_flag() {
        // A bare --theme silently meaning "the default" is the same bug as a
        // misspelled name silently meaning something else.
        assert!(theme_argument(args(&["--theme"])).is_err());
    }

    #[test]
    fn theme_argument_refuses_repetition() {
        assert!(theme_argument(args(&["--theme", "a", "--theme", "b"])).is_err());
        assert!(theme_argument(args(&["--theme=a", "--theme=b"])).is_err());
    }

    #[test]
    fn theme_argument_refuses_what_it_does_not_know() {
        assert!(theme_argument(args(&["--frobnicate"])).is_err());
    }

    #[test]
    fn resolves_a_path_and_a_name() {
        let (dir, theme) = theme();

        // A slash means a path, taken as-is; the root is not consulted.
        let by_path = resolve_theme(theme.to_str().unwrap(), Path::new("/nonexistent")).unwrap();
        assert_eq!(by_path, theme);

        // No slash means a name under the root.
        let by_name = resolve_theme("theme", dir.path()).unwrap();
        assert_eq!(by_name, theme);
    }

    #[test]
    fn refuses_a_theme_without_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(resolve_theme(empty.to_str().unwrap(), dir.path()).is_err());
        assert!(resolve_theme("missing", dir.path()).is_err());
    }

    #[test]
    fn the_initial_uri_survives_a_space_in_the_path() {
        // The URI handed to load_uri must pass the same within_theme check
        // navigation is held to, or the greeter refuses its own index.html and
        // shows a blank screen. format!("file://{}") failed exactly that for a
        // path containing a space; filename_to_uri is the inverse of the
        // filename_from_uri that within_theme decodes with.
        let dir = tempfile::tempdir().unwrap();
        let theme = dir.path().join("the theme");
        std::fs::create_dir(&theme).unwrap();
        std::fs::write(theme.join("index.html"), "<html></html>").unwrap();
        let theme = theme.canonicalize().unwrap();

        let uri = glib::filename_to_uri(theme.join("index.html"), None).unwrap();
        assert!(within_theme(&uri, &theme), "{uri}");
    }

    #[test]
    fn the_startup_error_is_delivered_exactly_once() {
        // The invariant the RefCell exists for. Making take() a clone would
        // silently re-show "your session crashed" on every theme reload, and
        // without this test nothing would notice.
        let error = StartupError::new(Some("session exited unexpectedly".to_owned()));

        // Nothing before a page has loaded: there is no show_message to call.
        assert_eq!(error.take(), None);

        error.arm();
        assert_eq!(error.take().as_deref(), Some("session exited unexpectedly"));
        assert_eq!(error.take(), None, "a reload resurrected it");
    }

    #[test]
    fn a_failed_startup_error_is_retried() {
        // Taking on *send* rather than on success destroyed the message when
        // the evaluation failed — a web process mid-crash, which is exactly
        // when the compositor's account of why the last session died matters.
        let error = StartupError::new(Some("session exited unexpectedly".to_owned()));
        error.arm();

        let text = error.take().expect("armed and undelivered");
        error.restore(text); // the evaluation came back Err
        assert_eq!(
            error.take().as_deref(),
            Some("session exited unexpectedly"),
            "a failed evaluation lost the message"
        );

        // And the retry that succeeds is still the last one.
        assert_eq!(error.take(), None);
    }

    #[test]
    fn the_default_theme_uses_the_api_it_is_shipped_with() {
        // A drift check, not a JS test. The theme is part of the contract, so
        // renaming a field of the injected API without updating it must fail
        // the build rather than the login screen.
        let theme = include_str!("../themes/default/theme.js");
        for field in ["wdm.users", "wdm.sessions", "wdm.default_session"] {
            assert!(theme.contains(field), "the default theme lost {field}");
        }
    }

    #[test]
    fn refuses_what_does_not_exist() {
        // Nothing to canonicalise means nothing to compare honestly; refusing
        // is also what keeps an unresolvable `..` from being matched as text.
        let (_dir, theme) = theme();
        assert!(!within_theme(&uri(&theme.join("missing.html")), &theme));
        assert!(!within_theme(
            &uri(&theme.join("../does-not-exist/../secret")),
            &theme
        ));
    }
}
