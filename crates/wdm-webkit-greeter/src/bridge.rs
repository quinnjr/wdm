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
}

impl Bridge {
    /// The JavaScript reporting everything that changed since the last call.
    ///
    /// Each entry is a complete statement, evaluated in the page. A theme that
    /// has not defined a given callback simply does not get called, which is
    /// what makes a minimal theme possible.
    pub fn diff(&mut self, model: &Model) -> Vec<String> {
        let mut out = Vec::new();

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
                out.push(call("show_message", &[json!(text), json!("error")]));
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
            out.push(format!(
                "window.wdm.is_authenticated = {};\nwindow.wdm.in_authentication = false;\nwindow.wdm._prompt = null;",
                model.authenticated
            ));
            out.push(call("authentication_complete", &[]));
        }

        if !model.conversation_over {
            self.over = false;
        }

        out
    }

    /// Forget the previous conversation, so its verdict is reported again if it
    /// repeats. Called when the theme starts a new attempt.
    pub fn restart(&mut self) {
        *self = Self::default();
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

/// A call to a theme-defined global, skipped when the theme did not define it.
fn call(name: &str, args: &[serde_json::Value]) -> String {
    let args: Vec<String> = args.iter().map(literal).collect();
    format!(
        "if (typeof window.{name} === 'function') {{ window.{name}({}); }}",
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

    fn model() -> Model {
        Model {
            users: vec![User {
                name: "joseph".to_owned(),
                display_name: "Joseph".to_owned(),
                last_session: "gnome".to_owned(),
            }],
            sessions: vec![Session {
                id: "gnome".to_owned(),
                name: "GNOME".to_owned(),
            }],
            ..Model::default()
        }
    }

    #[test]
    fn reports_a_prompt_once() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(Prompt {
            id: 7,
            text: "Password:".to_owned(),
            secret: true,
        });

        let js = bridge.diff(&model).join("\n");
        assert!(js.contains("show_prompt"), "{js}");
        assert!(js.contains("\"password\""), "{js}");

        // Polled state must not re-fire; a theme animating the prompt would be
        // restarted sixty times a second.
        assert!(bridge.diff(&model).is_empty());
    }

    #[test]
    fn a_new_prompt_with_the_same_text_still_fires() {
        // Prompt ids are never reused, which is what makes them the right key:
        // PAM asking "Password:" a second time is a different question.
        let mut bridge = Bridge::default();
        let mut model = model();
        for id in [1, 2] {
            model.prompt = Some(Prompt {
                id,
                text: "Password:".to_owned(),
                secret: true,
            });
            assert!(
                bridge.diff(&model).iter().any(|js| js.contains("show_prompt")),
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

        let js = bridge.diff(&model);
        let message = js.iter().position(|s| s.contains("show_message")).unwrap();
        let complete = js
            .iter()
            .position(|s| s.contains("authentication_complete"))
            .unwrap();
        // A theme that redraws on completion needs the reason already in hand.
        assert!(message < complete, "{js:?}");
        assert!(js.iter().any(|s| s.contains("is_authenticated = false")));

        assert!(bridge.diff(&model).is_empty());
    }

    #[test]
    fn reports_success() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;

        let js = bridge.diff(&model).join("\n");
        assert!(js.contains("is_authenticated = true"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
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
        assert!(!bridge.diff(&model).is_empty());

        model.begin_attempt();
        bridge.restart();
        assert!(bridge.diff(&model).is_empty());

        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());
        let js = bridge.diff(&model).join("\n");
        assert!(js.contains("show_message"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
    }

    #[test]
    fn text_from_pam_cannot_escape_its_literal() {
        // The trust boundary. PAM modules are configured by the administrator,
        // but their text reaches the page verbatim, and a greeter that builds
        // JavaScript by concatenation hands whoever writes that text the run of
        // the login screen.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(Prompt {
            id: 1,
            text: "</script><script>alert('x')//\"'\n\u{2028}".to_owned(),
            secret: false,
        });

        let js = bridge.diff(&model).join("\n");
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
