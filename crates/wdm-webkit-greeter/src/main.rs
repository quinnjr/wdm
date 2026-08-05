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
//!   the login screen into a browser. Subresources are held to a looser line —
//!   local files, any local file — by a content security policy, which is also
//!   what stops anything at all being fetched from or sent to the network; the
//!   page is then denied the ability to *read* files it loads.
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
use wdm_protocol::GREETER_GAVE_UP_EXIT;

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
pub(crate) const PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

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
    // Zero is success; anything else is the status to exit with, which is how
    // the wedged case reports itself as something other than plain failure.
    let failure = Rc::new(std::cell::Cell::new(0u8));

    app.connect_activate({
        let failure = failure.clone();
        let theme = theme.clone();
        move |app| {
            if let Err(e) = activate(app, &theme, failure.clone()) {
                log::error!("{e}");
                failure.set(1);
                app.quit();
            }
        }
    });

    // Arguments are ours, not GTK's — it would try to interpret --theme.
    let status = app.run_with_args::<&str>(&[]);

    match failure.get() {
        0 if status == glib::ExitCode::SUCCESS => ExitCode::SUCCESS,
        0 => ExitCode::FAILURE,
        code => ExitCode::from(code),
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
    failure: Rc<std::cell::Cell<u8>>,
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
            //
            // Resynced first, because a reload rewinds one piece of page state
            // behind the bridge's back: `link_dead` is baked into the
            // document-start API script, which re-runs with the value frozen at
            // startup. After a link death both the page's copy and the bridge's
            // mirror read true, the diff sees no change, and the reloaded
            // document is left believing the socket is alive — offering a retry
            // that goes nowhere. See `Bridge::resync`.
            let model = model.borrow();
            let mut bridge = bridge.borrow_mut();
            bridge.resync();
            bridge.diff(&model);
        }
    });

    window.present();

    let app = app.clone();
    glib::timeout_add_local(PUMP_INTERVAL, move || {
        // Charge the tick against whatever evaluation is outstanding before
        // asking whether the page is dead. This is the only place a web process
        // that took the script and never answered gets noticed at all: it
        // produces no verdict for `evaluate`'s callback to report, so without
        // this the bridge would wait on it forever.
        //
        // A page that has answered nothing for long enough is not coming back
        // on its own, and a greeter process that stays alive in front of a dead
        // login screen is invisible to wdm's supervisor. Exiting is what turns
        // it into a respawn — with [`GREETER_GAVE_UP_EXIT`] rather than plain failure,
        // because establishing silence takes long enough that the supervisor
        // would otherwise read the exit as a healthy greeter that happened to
        // stop, reset its failure count, and reload the same broken theme
        // forever instead of reaching the give-up screen.
        let wedged = {
            let mut bridge = bridge.borrow_mut();
            bridge.tick();
            bridge.is_wedged()
        };
        if wedged {
            log::error!("the theme has stopped responding; exiting so wdm can respawn the greeter");
            failure.set(GREETER_GAVE_UP_EXIT);
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
    // The network half is the whole of what this directive buys. `file:` is a
    // *scheme*, not a directory, so `<img src="file:///var/…">` outside the
    // theme is loaded, and being a subresource it never passes through
    // within_theme. What keeps such a file from becoming something the theme
    // can read back — and therefore something it could put on screen or hand to
    // a caller — is the pair of file-access settings below, not this. So the
    // subresource boundary is "a local file", and confinement of the page is
    // the navigation policy, this policy and those settings together; no one of
    // the three is it.
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

/// Keep the page's *navigation* inside the theme directory.
///
/// A theme is meant to be self-contained. One that navigates elsewhere — a
/// stray link, a redirect, a mistake — would otherwise replace the login screen
/// with whatever it reached, on a machine at a login prompt.
///
/// It is one of three controls and not the confinement on its own. Only
/// `NavigationAction` and `NewWindowAction` are inspected here; `Response` and
/// any decision type WebKit grows later fall through as allowed, which is why
/// the CSP in [`build_webview`] spells out `form-action`, `base-uri` and
/// `frame-ancestors` rather than trusting this callback to catch a submission,
/// and why the file-access settings there are what actually bound what the page
/// can read. Widening the `matches!` would not fix that: a decision type this
/// code has never heard of is one it cannot judge, so allowing it and pinning
/// the guarantee on the CSP is the honest arrangement.
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
            // Once per distinct error, not every tick: a page that never
            // recovers is retried sixty times a second, and sixty warnings a
            // second in the journal is how a symptom becomes the whole log.
            // The bridge is asked rather than its failure count consulted,
            // because a missed deadline is a failure with no error text to
            // print and would otherwise spend the budget before this line —
            // the only one naming the statement that broke — ever ran.
            if let Some(e) = error
                && bridge.claim_error_report(&e)
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

    /// A theme's script with its whole-line comments removed.
    ///
    /// The drift checks below search for names that also appear in the theme's
    /// prose — it explains `wdm.default_session` two lines above using it — and
    /// a contract check a comment can satisfy checks nothing.
    fn strip_comments(source: &str) -> String {
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn theme_code() -> String {
        strip_comments(include_str!("../themes/default/theme.js"))
    }

    fn arch_theme_code() -> String {
        strip_comments(include_str!("../themes/arch/theme.js"))
    }

    /// How a theme's rules are held to account.
    ///
    /// The two hand-written themes have no test runner, so their guards can
    /// only be matched in their source. The React theme has one, and its rules
    /// are *executed* — see themes/react/src/machine.test.js. Both are
    /// legitimate; a theme with neither is not, which is what
    /// `every_shipped_theme_is_covered_somehow` refuses.
    enum Coverage {
        /// Pattern-matched here, against the named script.
        Grep,
        /// Executed by a Vitest suite at this path, relative to themes/.
        Suite(&'static str),
    }

    struct Theme {
        name: &'static str,
        html: &'static str,
        /// Every script the theme ships, concatenated, comments stripped.
        code: String,
        /// What the theme calls the injected API. The hand-written ones read
        /// the global directly; the React one takes it as a prop, so its
        /// members are reached through a different name and a check hardcoding
        /// `wdm.` would silently match nothing there.
        api: &'static str,
        coverage: Coverage,
    }

    /// Every shipped theme.
    ///
    /// Adding a theme here is what subjects it to the checks below. A theme
    /// that is installed but not listed is one nothing covers, and the failure
    /// that produces is a login screen — the one place nobody can read a
    /// backtrace from. `every_shipped_theme_is_installed` is what stops the
    /// list and the tree drifting apart.
    fn shipped_themes() -> Vec<Theme> {
        vec![
            Theme {
                name: "default",
                html: include_str!("../themes/default/index.html"),
                code: theme_code(),
                api: "wdm",
                coverage: Coverage::Grep,
            },
            Theme {
                name: "arch",
                html: include_str!("../themes/arch/index.html"),
                code: arch_theme_code(),
                api: "wdm",
                coverage: Coverage::Grep,
            },
            Theme {
                name: "react",
                html: include_str!("../themes/react/index.html"),
                // Three files rather than one: the decisions are in machine.js,
                // the bridge to wdm's globals in useWdm.js, and the markup in
                // App.jsx. Concatenated so a check for a name finds it wherever
                // the theme chose to put it.
                code: strip_comments(&format!(
                    "{}\n{}\n{}\n{}",
                    include_str!("../themes/react/src/machine.js"),
                    include_str!("../themes/react/src/useWdm.js"),
                    include_str!("../themes/react/src/App.jsx"),
                    include_str!("../themes/react/src/main.jsx"),
                )),
                api: "api",
                coverage: Coverage::Suite("react/src/machine.test.js"),
            },
        ]
    }

    #[test]
    fn the_default_theme_uses_the_api_it_is_shipped_with() {
        // A drift check, not a JS test. The injected API and the default theme
        // are both part of the contract themes.md documents, so renaming a
        // field of one without the other must fail the build rather than the
        // login screen.
        //
        // Asserted against the *generated script* as well as the theme, which
        // is the half that was missing: searching the theme alone left renaming
        // `default_session` in api_script — and nowhere else — green, which is
        // the exact drift this is supposed to catch.
        let script = bridge::api_script(&Model::default());
        let theme = theme_code();
        // The documentation is the third end of the same contract, and the one
        // that used to be claimed rather than checked: the list below said it
        // enumerated themes.md's tables while being hardcoded, so renaming a
        // field in the prose alone passed. Matched on the names themselves, not
        // on surrounding wording, which is free to change.
        let doc = include_str!("../../../docs/src/pages/themes.md");

        // Every row of themes.md's two tables. State first, then the methods.
        for name in [
            "users",
            "sessions",
            "default_session",
            "authentication_user",
            "is_authenticated",
            "in_authentication",
            "link_dead",
            // The bridge's own, but documented alongside the rest and read by
            // the default theme to tell "answer the prompt" from "try again",
            // so it is contract on all three ends like everything above it.
            "_prompt",
        ] {
            assert!(
                script.contains(&format!("{name}:")),
                "the injected API lost wdm.{name}"
            );
            assert!(
                doc.contains(&format!("wdm.{name}")),
                "themes.md no longer documents wdm.{name}"
            );
        }
        for method in ["authenticate", "respond", "cancel", "start_session"] {
            assert!(
                script.contains(&format!("{method}(")),
                "the injected API lost wdm.{method}()"
            );
            assert!(
                doc.contains(&format!("wdm.{method}(")),
                "themes.md no longer documents wdm.{method}()"
            );
        }

        // And the theme's end of it: what it reads and what it calls. Not the
        // whole table — a theme is free to ignore `cancel` or
        // `in_authentication`, and the default one does — but everything it
        // does touch has to still be there.
        for used in [
            "wdm.users",
            "wdm.sessions",
            "wdm.default_session",
            "wdm.is_authenticated",
            "wdm._prompt",
            // The one the theme reads defensively — `typeof wdm.link_dead !==
            // "undefined" && …` — so a rename does not throw, it just makes the
            // check false forever and the theme goes back to offering a retry
            // into a socket nobody reads. Nothing else would notice.
            "wdm.link_dead",
            "wdm.authenticate(",
            "wdm.respond(",
            "wdm.start_session(",
        ] {
            assert!(theme.contains(used), "the default theme lost {used}");
        }

        // The assignment the bridge keeps that field current with, which is a
        // separate string from the API's key and can drift from it on its own:
        // renaming one and not the other leaves the theme reading a field that
        // is never updated, and every check above still passes.
        let mut bridge = Bridge::default();
        let mut live = Model::default();
        live.link_dead = true;
        bridge.diff(&live);
        let assignment = bridge.flush().expect("nothing was queued").script;
        assert!(
            assignment.contains("window.wdm.link_dead = "),
            "the bridge stopped assigning wdm.link_dead: {assignment}"
        );

        // `_prompt` is the one contract field that is not a scalar, and the
        // checks above cannot see that: they match the *name*, and an object
        // renamed to a string keeps its name. The default theme only ever tests
        // it for truthiness, so a bridge that started assigning the bare prompt
        // text would pass every test in this crate while a third-party theme
        // written against the documented shape rendered "[object Object]" — or,
        // going the other way, stopped finding `.text` at all. Its members are
        // therefore pinned here alongside its name.
        //
        // Not `id`: the theme has no use for it — `wdm.respond()` answers
        // whatever is pending — so it is the one member the bridge could drop
        // without breaking anything documented.
        let mut prompt = wdm_greeter_client::Prompt::default();
        prompt.id = 1;
        prompt.text = "Password:".to_owned();
        prompt.secret = true;
        let mut asked = Model::default();
        asked.prompt = Some(prompt);
        let mut bridge = Bridge::default();
        bridge.diff(&asked);
        let assignment = bridge.flush().expect("nothing was queued").script;
        assert!(
            assignment.contains("window.wdm._prompt = {"),
            "wdm._prompt is no longer assigned an object: {assignment}"
        );
        for member in ["\"text\":", "\"secret\":"] {
            assert!(
                assignment.contains(member),
                "wdm._prompt lost {member} — themes.md documents the shape, not \
                 just the name: {assignment}"
            );
        }

        // The three callbacks are the other direction: the bridge names them,
        // and a theme that has stopped defining one goes quiet rather than
        // failing.
        for callback in ["show_prompt", "show_message", "authentication_complete"] {
            assert!(
                theme.contains(&format!("window.{callback} =")),
                "the default theme stopped defining {callback}"
            );
        }
    }

    #[test]
    fn the_default_theme_only_spends_a_buffered_answer_on_a_masked_prompt() {
        // A drift check on the one thing the theme holds that the greeter
        // cannot see: the answer typed before PAM asked for it. It is typed
        // into an input the theme renders as type="password", and the bridge
        // sends `kind` as "password" only for a secret prompt — so a
        // `show_prompt` handler that spends the buffer without looking at
        // `kind` answers an echo-on question (pam_oath, a username re-prompt)
        // with the user's password, which the PAM stack logs in the clear.
        //
        // Matched on the guard rather than the wording around it, and with
        // comments stripped, so the check cannot pass on the comment that
        // describes the rule instead of the code that applies it.
        let theme = theme_code();
        let guard = theme
            .split_once("window.show_prompt")
            .expect("the default theme stopped defining show_prompt")
            .1;
        let spend = guard
            .find("wdm.respond")
            .expect("show_prompt no longer answers anything");
        assert!(
            guard[..spend].contains("kind === \"password\""),
            "the default theme spends its buffered answer without checking \
             whether the prompt is masked"
        );
    }

    /// The API surface a theme must not lose, whichever theme it is.
    ///
    /// Shorter than the default theme's list because it is the intersection: a
    /// theme is free to ignore `cancel` or `in_authentication`. Everything here
    /// is something a theme cannot do its job without.
    /// Deliberately not including `_prompt`. It is what lets a theme tell
    /// "answer the question on screen" from "start again", and both
    /// hand-written themes read it — but a theme that tracks the conversation
    /// in its own state, as the React one does, already knows. Requiring it
    /// here would be requiring a particular design rather than a contract.
    /// `the_default_theme_uses_the_api_it_is_shipped_with` still pins its name,
    /// its shape and the assignment that keeps it current.
    const REQUIRED_BY_EVERY_THEME: [&str; 8] = [
        "wdm.users",
        "wdm.sessions",
        "wdm.default_session",
        "wdm.is_authenticated",
        "wdm.link_dead",
        "wdm.authenticate(",
        "wdm.respond(",
        "wdm.start_session(",
    ];

    #[test]
    fn every_shipped_theme_uses_the_api_it_is_shipped_with() {
        // The same drift check the default theme gets, applied to every theme
        // in the tree. Without it, a second theme is a second copy of the
        // contract that nothing verifies: renaming a field would fail the
        // default theme's test, get fixed there, and leave the other one
        // reading `undefined` on a screen no test ever loads.
        let script = bridge::api_script(&Model::default());

        for theme in shipped_themes() {
            let name = theme.name;
            for used in REQUIRED_BY_EVERY_THEME {
                // Spelled with the theme's own name for the API object. The
                // React theme takes it as a prop, so a check hardcoding `wdm.`
                // would match nothing there and pass for the wrong reason.
                let bare = used.trim_start_matches("wdm.").trim_end_matches('(');
                let spelled = used.replace("wdm.", &format!("{}.", theme.api));
                assert!(
                    theme.code.contains(&spelled),
                    "the {name} theme lost {spelled}"
                );
                // The theme's end and the bridge's end, checked together:
                // matching only the theme would go green on a theme that kept
                // using a name the API had dropped.
                assert!(
                    script.contains(&format!("{bare}:")) || script.contains(&format!("{bare}(")),
                    "the injected API lost {used}, which the {name} theme uses"
                );
            }

            for callback in ["show_prompt", "show_message", "authentication_complete"] {
                assert!(
                    theme.code.contains(&format!("window.{callback} =")),
                    "the {name} theme stopped defining {callback}"
                );
            }
        }
    }

    #[test]
    fn every_shipped_theme_is_covered_somehow() {
        // The check that keeps the rest honest. A theme whose rules are
        // neither matched here nor executed by a suite of its own is a login
        // screen with no coverage at all — and the pattern-matched checks
        // below are written against the hand-written themes' shapes, so a
        // theme built any other way would slip past them silently rather than
        // fail. This makes that state impossible to reach by accident.
        let themes = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");

        for theme in shipped_themes() {
            if let Coverage::Suite(suite) = theme.coverage {
                let path = themes.join(suite);
                assert!(
                    path.is_file(),
                    "the {} theme claims its rules are executed by {suite}, which does \
                     not exist",
                    theme.name
                );
                // Named rather than merely present: a suite that stopped
                // covering the lockout guards would still be a file.
                let source = std::fs::read_to_string(&path).expect("unreadable suite");
                for rule in [
                    "does not arm PAM before the user asks",
                    "refuses an empty first answer",
                    "never sends the buffered password to an echo-on prompt",
                    "does not retry on its own after a failure",
                ] {
                    assert!(
                        source.contains(rule),
                        "{suite} no longer covers \"{rule}\" — that rule is what stops \
                         an unattended login screen locking an account out"
                    );
                }
            }
        }
    }

    #[test]
    fn every_shipped_theme_is_installed() {
        // shipped_themes() is a hand-written list, and a theme missing from it
        // is a theme none of these checks see. Comparing it against the tree is
        // what stops the two drifting: a new directory under themes/ has to be
        // added here, and then it cannot escape the checks above.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");
        let listed: Vec<&str> = shipped_themes().iter().map(|t| t.name).collect();

        for entry in std::fs::read_dir(&root).expect("themes/ is unreadable") {
            let entry = entry.expect("unreadable theme directory");
            if !entry.file_type().expect("no file type").is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                listed.contains(&name.as_str()),
                "themes/{name} exists but is not in shipped_themes(), so nothing checks \
                 it — add it there"
            );
        }
    }

    #[test]
    fn the_hand_written_themes_keep_the_guards_that_stop_a_lockout() {
        // These are the three behaviours that cost this project a locked
        // account and a password in a log, and they are properties of the
        // *theme* — the greeter cannot enforce any of them, because a greeter
        // that decided them would be fighting every theme that disagreed.
        //
        // Pattern-matched, which is all that is available for a theme with no
        // test runner, and only for those themes: the shapes below are the
        // hand-written ones' and a theme organised differently would match
        // nothing and pass. The React theme is held to the same rules by
        // machine.test.js, which executes them; every_shipped_theme_is_covered_
        // somehow is what ensures one of the two applies to every theme.
        //
        // Checked with comments stripped, so a theme cannot satisfy them with
        // the paragraph explaining the rule instead of the code applying it.
        for theme in shipped_themes() {
            if !matches!(theme.coverage, Coverage::Grep) {
                continue;
            }
            let name = theme.name;
            let code = &theme.code;

            // 1. The buffered answer reaches a masked prompt only. It was typed
            //    into an input rendered type="password", so spending it on an
            //    echo-on question hands the PAM stack a password it logs in the
            //    clear.
            let after = code
                .split_once("window.show_prompt")
                .unwrap_or_else(|| panic!("the {name} theme stopped defining show_prompt"))
                .1;
            let spend = after
                .find("wdm.respond")
                .unwrap_or_else(|| panic!("{name}'s show_prompt no longer answers anything"));
            assert!(
                after[..spend].contains("kind === \"password\""),
                "the {name} theme spends its buffered answer without checking whether \
                 the prompt is masked"
            );

            // 2. Nothing arms PAM before the user asks. A conversation is a
            //    login attempt for its whole duration as far as pam_faillock is
            //    concerned, so a theme that authenticates at load spends one
            //    every time the screen is left alone. The call must be reached
            //    from the submit handler, never from top level: `ready()` is
            //    what runs at load, and it must not contain it.
            let ready = code
                .split_once("const ready = ")
                .unwrap_or_else(|| panic!("the {name} theme has no ready()"))
                .1;
            let ready_body = &ready[..ready.find("\n};").unwrap_or(ready.len())];
            assert!(
                !ready_body.contains("wdm.authenticate("),
                "the {name} theme arms PAM from ready(), which runs at load — this is \
                 what locked an unattended account out in 0.4.0"
            );

            // 3. Messages accumulate rather than overwrite. The verdict arrives
            //    as one more show_message after the reason, so a handler that
            //    assigned would leave "Authentication failure" alone on screen
            //    with the only actionable line erased.
            assert!(
                code.contains("node.append(line)"),
                "the {name} theme no longer appends messages — the verdict will erase \
                 the reason it failed"
            );
        }
    }

    #[test]
    fn every_asset_a_theme_references_is_present_in_it() {
        // A theme is installed by copying a list of files, and that list is
        // written by hand in three packaging files. Forgetting one does not
        // fail any build: it produces a login screen with no stylesheet and
        // every icon a missing-glyph box, on the one screen whose failures
        // nobody can read a log from.
        //
        // So the references are checked against the tree. This does not prove
        // the packaging ships them — nothing here can see a PKGBUILD — but it
        // does mean the list this test walks is the same list the packaging has
        // to match, and it fails the moment a theme grows an asset.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");

        for theme in shipped_themes() {
            let name = theme.name;
            let html = theme.html;
            let mut referenced = Vec::new();
            for tail in html
                .match_indices("href=\"")
                .chain(html.match_indices("src=\""))
                .map(|(i, m)| &html[i + m.len()..])
            {
                let value = &tail[..tail.find('"').expect("unterminated attribute")];
                // Only what resolves to a file beside the theme. A data: URI
                // carries its own payload and there is nothing to install.
                if !value.is_empty() && !value.contains(':') && !value.starts_with('#') {
                    referenced.push(value);
                }
            }

            assert!(
                !referenced.is_empty(),
                "the {name} theme references no local assets — did index.html change shape?"
            );

            for asset in referenced {
                let path = root.join(name).join(asset);
                assert!(
                    path.is_file(),
                    "the {name} theme references {asset}, which is not in the theme \
                     directory: {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn the_arch_theme_clock_handles_midnight_and_noon() {
        // The only arithmetic in the theme, and the only part of it that is
        // wrong in a way nobody notices for eleven hours at a time: `hours % 12`
        // is 0 at both midnight and noon, so the naive conversion renders
        // "0:05 AM" and "0:05 PM". There is no JavaScript test runner in this
        // repository, so this is a drift check on the guard rather than an
        // execution of it — the same shape as the checks above, and for the
        // same reason.
        let code = arch_theme_code();
        assert!(
            code.contains("hours % 12 === 0 ? 12"),
            "the arch theme's 12-hour clock lost its midnight/noon guard — it will \
             render midnight as 0:00 AM"
        );
        // And that the meridiem is chosen from the 24-hour value, not from the
        // converted one: `hour12 < 12` is true for every hour of the day.
        assert!(
            code.contains("hours < 12 ? \"AM\" : \"PM\""),
            "the arch theme picks AM/PM from something other than the 24-hour value"
        );
    }

    #[test]
    fn the_arch_theme_ships_the_font_its_stylesheet_names() {
        // The one asset no <link> or <src> names: the webfont is reached from
        // inside vendor/fontawesome.css by a url(), which the scan above cannot
        // see. Without the file every icon renders as a tofu box, and because
        // font-display is `block` the boxes are what stays on screen.
        let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/arch/vendor");
        let css = std::fs::read_to_string(vendor.join("fontawesome.css"))
            .expect("the arch theme lost vendor/fontawesome.css");

        let quoted = css
            .split_once("url(\"")
            .expect("fontawesome.css no longer references a font file")
            .1;
        let font = &quoted[..quoted.find('"').expect("unterminated url()")];

        assert!(
            vendor.join(font).is_file(),
            "vendor/fontawesome.css names {font}, which is not shipped beside it"
        );
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
