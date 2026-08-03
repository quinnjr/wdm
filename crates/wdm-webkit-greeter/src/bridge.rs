//! The two directions of traffic between the theme and the protocol.
//!
//! Everything here is pure: [`parse`] turns what the page posted into a
//! [`Request`], and [`Bridge::diff`] turns a change in the [`Model`] into the
//! JavaScript that reports it. Neither touches GTK, so both are testable —
//! which matters more here than elsewhere, because the alternative is checking
//! a login screen's edge cases by hand.

use serde_json::json;
use wdm_greeter_client::Model;

/// What a theme asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Authenticate(String),
    Respond(String),
    Cancel,
    StartSession(String),
}

/// Parse one `postMessage` payload: `["verb"]` or `["verb", "argument"]`.
///
/// JSON for one reason. The payload crosses as a C string, so a separator-based
/// format loses everything past the first NUL — and the one argument that can
/// plausibly contain one is the password. JSON escapes it as text, so an answer
/// stays exactly what the user typed.
pub fn parse(raw: &str) -> Option<Request> {
    let message: Vec<String> = serde_json::from_str(raw).ok()?;
    let (verb, arg) = match message.as_slice() {
        [verb] => (verb.as_str(), ""),
        [verb, arg] => (verb.as_str(), arg.as_str()),
        _ => return None,
    };

    match verb {
        "authenticate" if !arg.is_empty() => Some(Request::Authenticate(arg.to_owned())),
        "respond" => Some(Request::Respond(arg.to_owned())),
        "cancel" => Some(Request::Cancel),
        "start_session" if !arg.is_empty() => Some(Request::StartSession(arg.to_owned())),
        _ => None,
    }
}

/// The most statements the queue will hold before dropping the oldest.
///
/// A wedged web process never acknowledges anything, so `pending` would
/// otherwise grow for as long as the machine sits at the login screen. The
/// oldest statements are the ones worth losing: the page's state is
/// assignments, and a later one supersedes an earlier one.
const PENDING_LIMIT: usize = 256;

/// Consecutive failed evaluations after which the page is declared dead.
///
/// The pump ticks every 16ms, so this is roughly a second of a web process
/// that answers nothing. Below that a reload or a crash-and-respawn recovers
/// on its own and quitting would be worse than waiting.
const WEDGED_AFTER: u32 = 60;

/// One flush's worth of script, tagged with the epoch it belongs to.
///
/// The tag is what makes a late verdict harmless: see [`Bridge::delivered`].
pub struct Outbound {
    pub epoch: u64,
    pub script: String,
}

/// What the page has already been told.
///
/// The model is polled, so every field it holds would otherwise be re-reported
/// on each pump. Themes are written expecting a callback per event — a theme
/// that types out `show_message` a character at a time, or plays a sound, would
/// be unusable if it fired sixty times a second.
#[derive(Default)]
pub struct Bridge {
    prompt: Option<u32>,
    notice: Option<String>,
    error: Option<String>,
    authenticated: bool,
    over: bool,
    /// Statements the page has not yet confirmed running.
    ///
    /// [`diff`](Self::diff) commits its state the moment it notices a change,
    /// but the JavaScript is evaluated asynchronously and can fail — a crashed
    /// web process, a page mid-reload. Without this queue a single failed
    /// evaluation dropped the event permanently: the diff would never notice it
    /// again, and a theme waiting on `authentication_complete` waited forever.
    pending: Vec<String>,
    /// How many of `pending` are in an evaluation whose verdict is still out.
    in_flight: usize,
    /// Which generation of the conversation `in_flight` belongs to.
    ///
    /// [`restart`](Self::restart) bumps it, and a verdict tagged with an older
    /// one is discarded. Without that, the real ordering loses events: the
    /// theme calls `authenticate()`, which restarts the bridge; the next tick
    /// queues and evaluates the new conversation's statements; and only *then*
    /// does the pre-restart evaluation land, acknowledging statements it never
    /// carried. Zeroing `in_flight` in `restart` is not enough, because the
    /// straggler arrives after it has been set again.
    epoch: u64,
    /// Failed evaluations since the last successful one.
    consecutive_failures: u32,
}

impl Bridge {
    /// Queue the JavaScript reporting everything that changed since last call.
    ///
    /// Each entry is a complete statement, evaluated in the page. A theme that
    /// has not defined a given callback simply does not get called, which is
    /// what makes a minimal theme possible.
    pub fn diff(&mut self, model: &Model) {
        let out = &mut self.pending;

        // Ordering is deliberate: messages explaining a failure are delivered
        // before the completion callback that a theme reacts to, so a theme
        // that re-renders on completion still has the explanation in hand.
        if model.notice != self.notice {
            self.notice = model.notice.clone();
            if let Some(text) = &self.notice {
                out.push(call("show_message", &[json!(text), json!("info")]));
            }
        }

        if model.error != self.error {
            self.error = model.error.clone();
            if let Some(text) = &self.error {
                out.push(error_script(text));
            }
        }

        let prompt = model.prompt.as_ref().map(|p| p.id);
        if prompt != self.prompt {
            self.prompt = prompt;
            if let Some(prompt) = &model.prompt {
                out.push(format!(
                    "window.wdm._prompt = {};",
                    literal(&json!({
                        "id": prompt.id,
                        "text": prompt.text,
                        "secret": prompt.secret,
                    }))
                ));
                let kind = if prompt.secret { "password" } else { "text" };
                out.push(call("show_prompt", &[json!(prompt.text), json!(kind)]));
            }
        }

        // A conversation ends exactly once, either way. `authenticated` and
        // `conversation_over` are separate flags in the model rather than one
        // verdict, so both are watched.
        let finished = model.authenticated != self.authenticated
            || (model.conversation_over && !self.over && !model.authenticated);
        if finished {
            self.authenticated = model.authenticated;
            self.over = model.conversation_over;
            // `authentication_user` is who the current conversation is for, or
            // null. A failure ends that conversation, so the name goes with it;
            // success keeps it, because the session about to start is theirs.
            let user = if model.authenticated {
                ""
            } else {
                "\nwindow.wdm.authentication_user = null;"
            };
            out.push(format!(
                "window.wdm.is_authenticated = {};\nwindow.wdm.in_authentication = false;\nwindow.wdm._prompt = null;{user}",
                model.authenticated
            ));
            out.push(call("authentication_complete", &[]));
        }

        if !model.conversation_over {
            self.over = false;
        }

        self.enforce_limit();
    }

    /// Drop the oldest statements once the queue is past [`PENDING_LIMIT`].
    ///
    /// Only reachable when the page has stopped acknowledging anything, since
    /// a healthy flush empties the queue. Statements still in flight are
    /// dropped along with the rest, so `in_flight` shrinks with them; the
    /// verdict on them is then a verdict on fewer statements, which is exactly
    /// what the epoch check makes safe to be wrong about.
    fn enforce_limit(&mut self) {
        let Some(excess) = self.pending.len().checked_sub(PENDING_LIMIT) else {
            return;
        };
        if excess == 0 {
            return;
        }
        log::warn!("theme queue full; dropping {excess} unacknowledged statement(s)");
        self.pending.drain(..excess);
        self.in_flight = self.in_flight.saturating_sub(excess);
    }

    /// Everything queued, as one script — or nothing while a verdict is out.
    ///
    /// One evaluation at a time: sending more while one is unresolved would
    /// mean not knowing which statements a later failure lost.
    pub fn flush(&mut self) -> Option<Outbound> {
        if self.in_flight > 0 || self.pending.is_empty() {
            return None;
        }
        self.in_flight = self.pending.len();
        Some(Outbound {
            epoch: self.epoch,
            script: self.pending.join("\n"),
        })
    }

    /// Record the verdict on the [`flush`](Self::flush) that produced `epoch`.
    ///
    /// A verdict from before a [`restart`](Self::restart) is ignored entirely:
    /// it describes an abandoned conversation, and acting on it would either
    /// drain statements it never carried or count a failure against a page that
    /// is answering fine.
    ///
    /// A failure keeps the statements queued, so the next flush retransmits
    /// them; the page's state is assignments plus idempotent callbacks, and
    /// repeating those is recoverable where dropping them is not.
    pub fn delivered(&mut self, epoch: u64, ok: bool) {
        if epoch != self.epoch {
            return;
        }
        if ok {
            self.pending.drain(..self.in_flight);
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
        self.in_flight = 0;
    }

    /// Failed evaluations since the last that succeeded.
    ///
    /// The caller logs on the first one only: retrying every 16ms forever is
    /// otherwise sixty warnings a second in the journal.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Whether the page has stopped answering for long enough to give up on.
    ///
    /// A web process that is wedged rather than crashed leaves wdm's supervisor
    /// looking at a healthy greeter process and a login screen that does
    /// nothing, indefinitely. Exiting is what turns that into a respawn.
    pub fn is_wedged(&self) -> bool {
        self.consecutive_failures >= WEDGED_AFTER
    }

    /// Forget the previous conversation, so its verdict is reported again if it
    /// repeats. Called when the theme starts a new attempt.
    ///
    /// Also drops anything still pending: those statements describe the
    /// conversation being abandoned. The epoch is carried across and bumped, so
    /// a verdict from before the restart is recognised as stale rather than
    /// applied to whatever has been queued since.
    pub fn restart(&mut self) {
        let epoch = self.epoch.wrapping_add(1);
        *self = Self::default();
        self.epoch = epoch;
    }
}

/// A JavaScript literal for a value that came from outside.
///
/// `serde_json` escapes what JSON requires, which is enough for the two places
/// this is used — both hand their result straight to a JavaScript engine, with
/// no HTML parser in between. The extra three escapes are for the day that
/// stops being true: `<` cannot start a `</script>` that ends an inline script,
/// and U+2028/U+2029 are line terminators to a parser older than ES2019.
fn literal(value: &serde_json::Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// The statement reporting an error to the page, shared between [`Bridge::diff`]
/// and the one delivery that happens outside the queue: an error that predates
/// the page — a failed session's parting message — is sent directly on
/// load-finished, and it must go through the same escaping as everything else,
/// because its text is PAM's and wdm's, not ours.
pub fn error_script(text: &str) -> String {
    call("show_message", &[json!(text), json!("error")])
}

/// A call to a theme-defined global, skipped when the theme did not define it.
///
/// The try/catch is load-bearing: the queued statements are evaluated as one
/// script, so a theme callback that throws would otherwise abort every
/// statement after it — including the `is_authenticated` assignment a broken
/// `show_message` has no business suppressing.
fn call(name: &str, args: &[serde_json::Value]) -> String {
    let args: Vec<String> = args.iter().map(literal).collect();
    format!(
        "if (typeof window.{name} === 'function') {{ try {{ window.{name}({}); }} catch (e) {{ console.error('wdm: {name}:', e); }} }}",
        args.join(", ")
    )
}

/// The API object, built from the model once the lists are known.
///
/// Injected at document-start so a theme can read `wdm.users` from its own
/// top-level script rather than having to wait for a ready callback.
pub fn api_script(model: &Model) -> String {
    let users: Vec<_> = model
        .users
        .iter()
        .map(|u| {
            json!({
                "name": u.name,
                "display_name": u.display_name,
                "last_session": u.last_session,
            })
        })
        .collect();

    let sessions: Vec<_> = model
        .sessions
        .iter()
        .map(|s| json!({ "id": s.id, "name": s.name }))
        .collect();

    // `post` is the only way out of the page. Everything else is sugar over it,
    // including the argument checks — a theme that calls respond() with no
    // pending prompt gets an exception it can see, rather than a message the
    // compositor rejects as a protocol error and kills the greeter for.
    format!(
        r#"window.wdm = {{
  users: {users},
  sessions: {sessions},
  default_session: {default_session},
  authentication_user: null,
  is_authenticated: false,
  in_authentication: false,
  _prompt: null,
  _post(verb, arg) {{
    window.webkit.messageHandlers.wdm.postMessage(
      JSON.stringify(arg === undefined ? [verb] : [verb, String(arg)]));
  }},
  authenticate(username) {{
    if (!username) {{ throw new Error('wdm.authenticate needs a username'); }}
    this.authentication_user = username;
    this.in_authentication = true;
    this.is_authenticated = false;
    this._prompt = null;
    this._post('authenticate', username);
  }},
  respond(text) {{
    if (!this._prompt) {{ throw new Error('wdm.respond with no prompt pending'); }}
    this._prompt = null;
    this._post('respond', text);
  }},
  cancel() {{
    this.authentication_user = null;
    this.in_authentication = false;
    this._prompt = null;
    this._post('cancel');
  }},
  start_session(id) {{
    if (!this.is_authenticated) {{ throw new Error('wdm.start_session before authenticating'); }}
    this._post('start_session', id || (this.sessions[0] && this.sessions[0].id));
  }},
}};
"#,
        users = literal(&serde_json::Value::Array(users)),
        sessions = literal(&serde_json::Value::Array(sessions)),
        // The machine's configured default, an empty string when unset. A
        // theme's preselection chain — history, then this, then the first
        // session — mirrors Model::preferred_session; exposing the middle link
        // is what lets a theme make the same choice.
        default_session = literal(&json!(model.default_session)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wdm_greeter_client::{Prompt, Session, User};

    #[test]
    fn parses_the_verbs() {
        assert_eq!(
            parse(r#"["authenticate","joseph"]"#),
            Some(Request::Authenticate("joseph".to_owned()))
        );
        assert_eq!(parse(r#"["cancel"]"#), Some(Request::Cancel));
        assert_eq!(
            parse(r#"["start_session","gnome"]"#),
            Some(Request::StartSession("gnome".to_owned()))
        );
    }

    #[test]
    fn an_answer_survives_verbatim() {
        // The NUL is the point: the payload crosses as a C string, so the
        // first version of this — verb, separator, argument — silently
        // delivered an empty password. JSON escapes it as text instead.
        let Some(Request::Respond(text)) = parse(r#"["respond","p\u0000a s\tsw\"rd"]"#) else {
            panic!("not a respond");
        };
        assert_eq!(text, "p\0a s\tsw\"rd");

        // And an empty answer is legitimate — PAM asks, the user answers
        // nothing, and PAM decides what that means.
        assert_eq!(
            parse(r#"["respond",""]"#),
            Some(Request::Respond(String::new()))
        );
    }

    #[test]
    fn rejects_nonsense_and_missing_arguments() {
        assert_eq!(parse(r#"["authenticate"]"#), None);
        assert_eq!(parse(r#"["authenticate",""]"#), None);
        assert_eq!(parse(r#"["start_session",""]"#), None);
        assert_eq!(parse(r#"["respond","a","b"]"#), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"["shutdown","now"]"#), None);
    }

    /// The client's types are `#[non_exhaustive]`, so each of these is built
    /// through `Default` and mutated rather than written as a literal.
    fn user() -> User {
        let mut user = User::default();
        user.name = "joseph".to_owned();
        user.display_name = "Joseph".to_owned();
        user.last_session = "gnome".to_owned();
        user
    }

    fn session() -> Session {
        let mut session = Session::default();
        session.id = "gnome".to_owned();
        session.name = "GNOME".to_owned();
        session
    }

    fn prompt(id: u32, text: &str, secret: bool) -> Prompt {
        let mut prompt = Prompt::default();
        prompt.id = id;
        prompt.text = text.to_owned();
        prompt.secret = secret;
        prompt
    }

    fn model() -> Model {
        let mut model = Model::default();
        model.users = vec![user()];
        model.sessions = vec![session()];
        model
    }

    /// Diff, flush and acknowledge in one step — one healthy pump tick.
    fn drain(bridge: &mut Bridge, model: &Model) -> String {
        bridge.diff(model);
        let Some(out) = bridge.flush() else {
            return String::new();
        };
        bridge.delivered(out.epoch, true);
        out.script
    }

    #[test]
    fn reports_a_prompt_once() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(prompt(7, "Password:", true));

        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_prompt"), "{js}");
        assert!(js.contains("\"password\""), "{js}");

        // Polled state must not re-fire; a theme animating the prompt would be
        // restarted sixty times a second.
        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn a_new_prompt_with_the_same_text_still_fires() {
        // Prompt ids are never reused, which is what makes them the right key:
        // PAM asking "Password:" a second time is a different question.
        let mut bridge = Bridge::default();
        let mut model = model();
        for id in [1, 2] {
            model.prompt = Some(prompt(id, "Password:", true));
            assert!(
                drain(&mut bridge, &model).contains("show_prompt"),
                "prompt {id} was swallowed"
            );
        }
    }

    #[test]
    fn reports_failure_then_completion() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());

        let js = drain(&mut bridge, &model);
        let message = js.find("show_message").unwrap();
        let complete = js.find("authentication_complete").unwrap();
        // A theme that redraws on completion needs the reason already in hand.
        assert!(message < complete, "{js}");
        assert!(js.contains("is_authenticated = false"), "{js}");
        // The conversation the name belonged to is over; a theme reading
        // authentication_user after a failure must see null, as documented.
        assert!(js.contains("authentication_user = null"), "{js}");

        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn reports_success() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;

        let js = drain(&mut bridge, &model);
        assert!(js.contains("is_authenticated = true"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
        // On success the name stays: the session being started is for them.
        assert!(!js.contains("authentication_user = null"), "{js}");
    }

    #[test]
    fn a_preexisting_error_is_reported() {
        // The error a failed session leaves behind is already in the model when
        // the greeter connects — no later revision change ever announces it. A
        // fresh bridge diffing that model must report it anyway; the bug was
        // that diff() was only ever called on a change.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.error = Some("session exited unexpectedly".to_owned());

        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_message"), "{js}");
        assert!(js.contains("\"error\""), "{js}");
    }

    #[test]
    fn the_api_exposes_the_machine_default_session() {
        // Model::preferred_session falls back history → default → first; a
        // theme can only mirror that chain if the middle link is in the API.
        let mut model = model();
        model.default_session = "gnome".to_owned();
        let script = api_script(&model);
        assert!(script.contains("default_session: \"gnome\""), "{script}");

        // And unset is an empty string, not an absent field.
        assert!(api_script(&Model::default()).contains("default_session: \"\""));
    }

    #[test]
    fn a_second_attempt_reports_its_own_failure() {
        // Two identical failures in a row are two events. Keying on the
        // *value* of the error would report only the first, leaving a theme
        // that clears its own form waiting forever on the second.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());
        assert!(!drain(&mut bridge, &model).is_empty());

        model.begin_attempt();
        bridge.restart();
        assert!(drain(&mut bridge, &model).is_empty());

        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());
        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_message"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
    }

    #[test]
    fn a_failed_evaluation_is_retransmitted() {
        // The web process can be mid-crash or mid-reload when the script is
        // sent. The diff has already committed its state by then, so losing
        // the script here would lose the event permanently — a theme waiting
        // on authentication_complete would wait forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;

        bridge.diff(&model);
        let first = bridge.flush().unwrap();
        bridge.delivered(first.epoch, false);

        let retry = bridge.flush().unwrap();
        assert_eq!(first.script, retry.script);

        bridge.delivered(retry.epoch, true);
        assert!(bridge.flush().is_none());
    }

    #[test]
    fn one_evaluation_at_a_time() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.notice = Some("first".to_owned());

        bridge.diff(&model);
        let first = bridge.flush().unwrap();
        assert!(first.script.contains("first"), "{}", first.script);

        // A change arriving while the verdict is out queues behind it…
        model.error = Some("second".to_owned());
        bridge.diff(&model);
        assert!(bridge.flush().is_none());

        // …and success only acknowledges what was actually sent.
        bridge.delivered(first.epoch, true);
        let second = bridge.flush().unwrap().script;
        assert!(!second.contains("first"), "{second}");
        assert!(second.contains("second"), "{second}");
    }

    #[test]
    fn restart_drops_the_queue() {
        // Statements pending at restart describe a conversation the theme
        // abandoned; delivering them into the new one would report a stale
        // verdict.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;
        bridge.diff(&model);
        assert!(bridge.flush().is_some());

        bridge.restart();
        assert!(bridge.flush().is_none());
    }

    #[test]
    fn a_straggler_from_before_a_restart_acknowledges_nothing() {
        // The real ordering, which is what makes the epoch necessary. The
        // theme calls authenticate(), which restarts the bridge; the next
        // pump tick diffs, flushes and evaluates the new conversation; and
        // only *then* does the pre-restart evaluation come back. Zeroing
        // in_flight in restart() cannot help, because it has been set again by
        // the time the straggler lands — it would drain statements it never
        // carried, and the real verdict would then drain nothing, losing an
        // authentication_complete and leaving the theme waiting forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;
        bridge.diff(&model);
        let stale = bridge.flush().unwrap();

        bridge.restart();

        model.begin_attempt();
        model.authenticated = true;
        bridge.diff(&model);
        let fresh = bridge.flush().unwrap();
        assert!(fresh.script.contains("authentication_complete"));
        assert_ne!(stale.epoch, fresh.epoch);

        bridge.delivered(stale.epoch, true); // the straggler, landing late
        bridge.delivered(fresh.epoch, false); // and then the real verdict

        let retry = bridge.flush().expect("the straggler ate the new event");
        assert_eq!(retry.script, fresh.script);
    }

    #[test]
    fn a_wedged_page_is_eventually_declared_dead() {
        // A permanently failing evaluation is retried every 16ms forever while
        // wdm's supervisor sees a healthy greeter process, so the login screen
        // sits dead indefinitely unless the greeter admits it.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.notice = Some("hello".to_owned());
        bridge.diff(&model);

        for _ in 0..WEDGED_AFTER - 1 {
            let out = bridge.flush().expect("still retransmitting");
            bridge.delivered(out.epoch, false);
            assert!(!bridge.is_wedged());
        }
        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, false);
        assert!(bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);

        // And a page that comes back is not wedged. Recovery is the common
        // case — a web process crash-and-respawn is a handful of ticks.
        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, true);
        assert!(!bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), 0);
    }

    #[test]
    fn a_stale_verdict_is_not_counted_as_a_failure() {
        // Otherwise a straggler could push a perfectly healthy page towards
        // the wedged threshold and quit the greeter out from under the user.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.notice = Some("hello".to_owned());
        bridge.diff(&model);
        let stale = bridge.flush().unwrap();

        bridge.restart();
        bridge.delivered(stale.epoch, false);
        assert_eq!(bridge.consecutive_failures(), 0);
    }

    #[test]
    fn the_queue_cannot_grow_without_bound() {
        // A page that acknowledges nothing would otherwise queue statements
        // for as long as the machine sits at the login screen.
        let mut bridge = Bridge::default();
        let mut model = model();

        for id in 1..(PENDING_LIMIT as u32 * 2) {
            model.prompt = Some(prompt(id, "Password:", true));
            bridge.diff(&model);
            let out = bridge.flush().expect("nothing was ever acknowledged");
            bridge.delivered(out.epoch, false);
        }

        let out = bridge.flush().unwrap();
        assert_eq!(out.script.lines().count(), PENDING_LIMIT);
        // The oldest go first, so what survives is the most recent state —
        // which is the state the page actually needs.
        assert!(
            out.script
                .contains(&format!("\"id\":{}", PENDING_LIMIT as u32 * 2 - 1)),
            "the newest statement was dropped"
        );
    }

    #[test]
    fn a_throwing_callback_cannot_abort_the_script() {
        // The queue is evaluated as one script, so an exception in a theme's
        // show_message would abort every statement after it — including the
        // is_authenticated assignment. Each call is therefore fenced.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());

        let js = drain(&mut bridge, &model);
        for line in js.lines().filter(|l| l.contains("window.show_")) {
            assert!(line.contains("try {"), "unfenced callback: {line}");
        }
    }

    #[test]
    fn text_from_pam_cannot_escape_its_literal() {
        // The trust boundary. PAM modules are configured by the administrator,
        // but their text reaches the page verbatim, and a greeter that builds
        // JavaScript by concatenation hands whoever writes that text the run of
        // the login screen.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(prompt(
            1,
            "</script><script>alert('x')//\"'\n\u{2028}",
            false,
        ));

        let js = drain(&mut bridge, &model);
        // Every dangerous character is inside a string literal it cannot close:
        // no bare quote, no bare newline, no `<`, no line separator.
        assert!(!js.contains("</script>"), "{js}");
        assert!(!js.contains('\u{2028}'), "{js}");
        for line in js.lines() {
            let quotes = line.matches('"').count() - line.matches("\\\"").count();
            assert_eq!(quotes % 2, 0, "unbalanced quotes in: {line}");
        }
    }

    #[test]
    fn a_username_cannot_close_the_api_object() {
        let mut model = model();
        model.users[0].display_name = "\";window.evil=1;//".to_owned();

        let script = api_script(&model);
        // The payload survives as text — it is a display name, and showing it
        // is the point. What must not survive is the quote that would end the
        // literal and let the rest of it run.
        assert!(script.contains("\\\";window.evil=1;//"), "{script}");
        assert!(!script.contains("\"\";window.evil"), "{script}");
    }
}
