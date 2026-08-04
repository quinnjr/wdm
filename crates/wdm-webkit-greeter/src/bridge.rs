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
/// oldest go first because the page's *state* is assignments, and a later
/// assignment supersedes an earlier one.
///
/// That is not true of the callbacks queued alongside them: `show_message` and
/// `show_prompt` are events with no successor, so one dropped here is gone for
/// good — a lockout explanation the user never sees. It is accepted rather than
/// worked around, because reaching this limit already means the page has not
/// run any of them: 256 unacknowledged statements is a web process that stopped
/// answering long before the queue filled, and the escalation that ends in
/// [`Bridge::is_wedged`] is the answer to that, not a cleverer eviction order.
const PENDING_LIMIT: usize = 256;

/// Consecutive failed evaluations after which the page is declared dead.
///
/// The pump ticks every 16ms, so an evaluation that comes back `Err` every time
/// makes this roughly a second. Below that a reload or a crash-and-respawn
/// recovers on its own and quitting would be worse than waiting.
///
/// A silent web process escalates the same way but more slowly, because each
/// failure costs it [`VERDICT_DEADLINE`] ticks first: about half a minute
/// before the greeter gives up. Waiting longer on silence than on a refusal is
/// the right way round — silence is the case a slow theme can also produce.
const WEDGED_AFTER: u32 = 60;

/// Pump ticks an evaluation may go without a verdict before it counts as one
/// that failed.
///
/// About half a second, which no callback in a login screen has any business
/// taking. Being wrong about it costs a retransmission, and the statements are
/// assignments plus callbacks a theme should tolerate seeing twice — the same
/// cost a genuinely failed evaluation already carries.
const VERDICT_DEADLINE: u32 = 30;

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
    /// How many of `Model::notices` the page has already been told about.
    ///
    /// A count rather than a copy of the last one: the list only grows within a
    /// conversation, so everything past this index is new, and each is reported
    /// with its own style instead of being folded into one line.
    notices_reported: usize,
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
    /// How many pump ticks the evaluation in flight has gone without a verdict.
    ///
    /// Counted rather than timed, so `Bridge` stays free of clocks: the pump's
    /// interval is the unit. See [`tick`](Self::tick).
    ticks_in_flight: u32,
    /// Which evaluation `in_flight` belongs to.
    ///
    /// [`restart`](Self::restart) and a timed-out [`tick`](Self::tick) both bump
    /// it, and a verdict tagged with an older one is discarded. Without that,
    /// the real ordering loses events: the theme calls `authenticate()`, which
    /// restarts the bridge; the next tick queues and evaluates the new
    /// conversation's statements; and only *then* does the pre-restart
    /// evaluation land, acknowledging statements it never carried. Zeroing
    /// `in_flight` is not enough, because the straggler arrives after it has
    /// been set again — which is exactly the shape of a late verdict for an
    /// evaluation that was already written off as timed out.
    epoch: u64,
    /// Failed evaluations since the last successful one, counting one that
    /// never answered at all.
    consecutive_failures: u32,
}

impl Bridge {
    /// Queue the JavaScript reporting everything that changed since last call.
    ///
    /// Each entry is a complete statement, evaluated in the page. A theme that
    /// has not defined a given callback simply does not get called, which is
    /// what makes a minimal theme possible.
    ///
    /// ## What `show_message`'s `kind` means
    ///
    /// One call per message PAM sent, carrying that message's own style —
    /// `"info"` for `PAM_TEXT_INFO`, `"error"` for `PAM_ERROR_MSG`. PAM sends
    /// them one at a time and the two mean different things, so they are
    /// reported one at a time rather than joined into a single line: a theme
    /// that wants to show a lockout in red and the minutes-remaining beside it
    /// in grey can, and one that treats both the same simply ignores the
    /// argument.
    ///
    /// `Model::error` is reported as `"error"` too, but it is a different kind
    /// of thing — the conversation's verdict rather than something the stack
    /// chose to say — and it arrives after every notice, so a theme that
    /// re-renders on it still has the explanation in hand.
    pub fn diff(&mut self, model: &Model) {
        let out = &mut self.pending;

        // Ordering is deliberate: messages explaining a failure are delivered
        // before the completion callback that a theme reacts to, so a theme
        // that re-renders on completion still has the explanation in hand.
        //
        // Notices only ever accumulate within a conversation — `begin_attempt`
        // clears the list, and `restart` resets this counter with it — so the
        // count is enough to tell which are new, and a theme is never told the
        // same message twice.
        if model.notices.len() > self.notices_reported {
            for notice in &model.notices[self.notices_reported..] {
                out.push(call(
                    "show_message",
                    &[json!(notice.text), json!(notice.kind.as_str())],
                ));
            }
            self.notices_reported = model.notices.len();
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
        self.ticks_in_flight = 0;
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
        self.ticks_in_flight = 0;
    }

    /// Advance the deadline on the evaluation in flight, if there is one.
    ///
    /// Called once per pump tick. This is the only thing that notices a web
    /// process which took the script and said nothing — [`delivered`] hears
    /// only from evaluations that finished, so a hung web process or a theme
    /// callback in an infinite loop reaches it never. Left to itself that state
    /// is permanent: `in_flight` stays set, [`flush`] returns `None` on every
    /// tick, and the greeter sits alive in front of a login screen that does
    /// nothing — which is precisely the case [`is_wedged`] is named for and,
    /// before this existed, the one case it could not see.
    ///
    /// A missed deadline is counted as a failed evaluation rather than as its
    /// own kind of trouble, so there is one way to give up instead of two, and
    /// a page that alternates between refusing and stalling still escalates.
    ///
    /// [`delivered`]: Self::delivered
    /// [`flush`]: Self::flush
    /// [`is_wedged`]: Self::is_wedged
    pub fn tick(&mut self) {
        if self.in_flight == 0 {
            return;
        }
        self.ticks_in_flight = self.ticks_in_flight.saturating_add(1);
        if self.ticks_in_flight < VERDICT_DEADLINE {
            return;
        }

        // The epoch is bumped for the same reason `restart` bumps it: the
        // evaluation has been written off, but it has not been cancelled, and
        // WebKit may still produce a verdict for it. Tagged with the old epoch,
        // that verdict is discarded — where without the bump a late success
        // would drain `in_flight` statements belonging to the retransmission
        // that has since been sent, acknowledging what the page never ran.
        self.epoch = self.epoch.wrapping_add(1);
        self.in_flight = 0;
        self.ticks_in_flight = 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
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
    ///
    /// Both halves of "stopped answering" count towards this, and they have to:
    /// a web process that refuses every evaluation is reported by
    /// [`delivered`](Self::delivered), but one that is hung rather than crashed
    /// refuses nothing — it simply never comes back, and only
    /// [`tick`](Self::tick) sees that.
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
    //
    // start_session needs two checks, not one. Its fallback to the first
    // session has nothing to fall back to when no sessions are installed, and
    // posting `undefined` then sends `["start_session"]` — which `parse`
    // refuses and the message handler logs to a journal nobody is reading,
    // while the theme, having thrown nothing, sits on "Starting session…"
    // forever. An exception is the whole promise of these guards: a mistake
    // shows up in the theme, not as silence.
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
    const session = id || (this.sessions[0] && this.sessions[0].id);
    if (!session) {{ throw new Error('wdm.start_session needs a session id'); }}
    this._post('start_session', session);
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
    use wdm_greeter_client::{NoticeKind, Prompt, Session, User};

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
    fn each_notice_carries_the_style_pam_gave_it() {
        // The defect this pins: both PAM styles used to arrive as one joined
        // string reported as "info", so a theme could not tell a lockout from
        // the sentence explaining how long it lasts, and the WebKit greeter
        // disagreed with the reference greeter on identical input.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        model.push_notice(
            NoticeKind::Info,
            "(10 minutes left to unlock)".to_owned(),
        );

        let js = drain(&mut bridge, &model);
        let locked = js.find("The account is locked.").unwrap();
        let minutes = js.find("10 minutes left").unwrap();

        // Two calls, not one joined line, and in the order PAM sent them.
        assert_eq!(js.matches("window.show_message(").count(), 2, "{js}");
        assert!(locked < minutes, "{js}");

        // Each with its own style: the lockout is the error, the duration is not.
        let error_kind = js.find("\"error\"").unwrap();
        let info_kind = js.find("\"info\"").unwrap();
        assert!(locked < error_kind && error_kind < minutes, "{js}");
        assert!(minutes < info_kind, "{js}");

        // Polled state must not re-report what the page has already been told.
        assert!(drain(&mut bridge, &model).is_empty());

        // And a notice arriving later is the only one reported.
        model.push_notice(NoticeKind::Error, "Account expired.".to_owned());
        let js = drain(&mut bridge, &model);
        assert_eq!(js.matches("window.show_message(").count(), 1, "{js}");
        assert!(js.contains("Account expired."), "{js}");
    }

    #[test]
    fn a_new_attempt_reports_its_notices_again() {
        // begin_attempt clears the list and restart() resets the counter with
        // it; if the two ever disagreed, the count would index past the end or
        // silently swallow the next conversation's first messages.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        assert!(drain(&mut bridge, &model).contains("show_message"));

        model.begin_attempt();
        bridge.restart();
        assert!(drain(&mut bridge, &model).is_empty());

        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        let js = drain(&mut bridge, &model);
        assert_eq!(js.matches("window.show_message(").count(), 1, "{js}");
        assert!(js.contains("\"error\""), "{js}");
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
        model.push_notice(NoticeKind::Info, "first".to_owned());

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
        model.push_notice(NoticeKind::Info, "hello".to_owned());
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
    fn a_page_that_never_answers_at_all_is_declared_dead() {
        // The case is_wedged is named for and could not see: a web process hung
        // rather than crashed, or a theme callback in an infinite loop. Nothing
        // ever comes back, so `delivered` is never called, and before `tick`
        // existed in_flight stayed set, flush returned None on every tick, and
        // the greeter sat alive in front of a dead login screen forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);

        // The pump, faithfully: tick, then flush, and a verdict never arrives.
        let first = {
            let mut first = None;
            let mut ticks = 0u32;
            while !bridge.is_wedged() {
                bridge.tick();
                if let Some(out) = bridge.flush() {
                    first.get_or_insert(out.epoch);
                }
                ticks += 1;
                assert!(ticks < 10_000, "the greeter never gave up");
            }
            first.expect("nothing was ever sent")
        };
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);

        // And the verdict that evaluation may still eventually produce cannot
        // undo it: it describes an attempt already written off, and the
        // statements it carried have been retransmitted since.
        bridge.delivered(first, true);
        assert!(bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);
    }

    #[test]
    fn a_slow_but_successful_evaluation_is_not_wedged() {
        // The deadline must be a deadline, not a speed limit. A theme callback
        // that takes a few hundred milliseconds is slow, not dead, and
        // declaring it dead would quit the greeter out from under the user.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);
        let out = bridge.flush().unwrap();

        for _ in 0..VERDICT_DEADLINE - 1 {
            bridge.tick();
            assert!(bridge.flush().is_none(), "sent a second evaluation");
            assert!(!bridge.is_wedged());
        }

        bridge.delivered(out.epoch, true);
        assert_eq!(bridge.consecutive_failures(), 0);
        assert!(!bridge.is_wedged());
        assert!(bridge.flush().is_none(), "the statement was not drained");
    }

    #[test]
    fn a_stale_verdict_is_not_counted_as_a_failure() {
        // Otherwise a straggler could push a perfectly healthy page towards
        // the wedged threshold and quit the greeter out from under the user.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
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
    fn start_session_refuses_to_post_nothing() {
        // With no sessions installed the fallback to sessions[0] resolves to
        // undefined, and _post would send ["start_session"] — a message wdm's
        // own parse refuses, leaving the theme on "Starting session…" with no
        // exception and no callback. The guard is what makes that visible.
        let script = api_script(&Model::default());
        assert!(
            script.contains("wdm.start_session needs a session id"),
            "{script}"
        );
        // And it is the resolved id that is checked, not the argument: a theme
        // that passes nothing on a machine that *has* sessions is still fine.
        assert!(
            script.contains("const session = id || (this.sessions[0] && this.sessions[0].id);"),
            "{script}"
        );
        assert!(
            script.contains("this._post('start_session', session);"),
            "{script}"
        );
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
