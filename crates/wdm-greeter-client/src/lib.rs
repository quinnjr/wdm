//! `wdm_greeter_v1` over GTK's own Wayland connection.
//!
//! Everything a toolkit greeter needs that is not its user interface: the
//! connection, the event queue, and the [`Model`] the protocol's events write
//! into. Shared by `wdm-gtk-greeter` and `wdm-webkit-greeter`, which differ
//! only in how they draw it.
//!
//! A GTK application does not own its Wayland connection — GDK does, and GDK
//! knows nothing about `wdm_greeter_v1`. Opening a second connection to the
//! compositor would not work either: wdm's socket accepts the greeter once, and
//! the protocol's objects would belong to a client with no surfaces.
//!
//! So this shares GDK's connection. `gdk4-wayland`'s `wayland_crate` feature
//! hands back GDK's `wl_display` as a `wayland-client` proxy; from it we take a
//! backend, make our *own* event queue on the same connection, and bind the
//! global there. Events for our objects land in our queue; GDK's land in GDK's.

use std::cell::RefCell;
use std::rc::Rc;

use gdk4_wayland::gdk;
use gdk4_wayland::prelude::*;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wdm_protocol::client::wdm_greeter_v1::{self, WdmGreeterV1};

/// Highest `wdm_greeter_v1` version this crate understands.
///
/// Bound as `min(advertised, this)`, so a greeter built against version 2 still
/// runs on a wdm that only offers version 1 — it simply never sees
/// `default_session`, and `last_session` carries the configured default for it
/// as it did before the split.
const INTERFACE_VERSION: u32 = 2;

/// A user wdm offered.
///
/// Non-exhaustive: the protocol advertises more about a user than the UI
/// currently draws, and adding a field must not break an out-of-tree greeter.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct User {
    pub name: String,
    pub display_name: String,
    pub last_session: String,
}

impl User {
    /// What to show in the user list.
    pub fn label(&self) -> String {
        if self.display_name.is_empty() {
            self.name.clone()
        } else {
            format!("{} ({})", self.display_name, self.name)
        }
    }
}

/// A session wdm offered.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct Session {
    pub id: String,
    pub name: String,
}

/// A question PAM is waiting on.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct Prompt {
    pub id: u32,
    pub text: String,
    /// Whether the answer must be masked.
    pub secret: bool,
}

/// Everything the UI draws from.
///
/// Wayland events mutate this; the UI reads it. `revision` increments on every
/// change so the UI can tell whether a repaint is warranted without diffing.
///
/// Non-exhaustive: every protocol addition lands here as a new field, and an
/// out-of-tree greeter must not need a source change to keep compiling. It is
/// constructed through [`Default`] and [`Link::connect`], never by literal.
#[derive(Default)]
#[non_exhaustive]
pub struct Model {
    pub greeter: Option<WdmGreeterV1>,
    pub users: Vec<User>,
    pub sessions: Vec<Session>,
    /// The machine-wide default session from wdm's configuration, empty when
    /// none is set. A user's `last_session` is their own history only, so this
    /// is the fallback for a user who has never logged in.
    pub default_session: String,
    pub prompt: Option<Prompt>,
    pub error: Option<String>,
    /// The last `info` or `error` style message PAM sent.
    ///
    /// Sticky: it survives the end of a conversation and is cleared only when
    /// the user deliberately starts another attempt. This is where a lockout
    /// notice arrives ("the account is locked, 10 minutes left"), and it is
    /// precisely the text that must not scroll past.
    pub notice: Option<String>,
    /// Set by `auth_ok`; the UI launches a session in response.
    pub authenticated: bool,
    /// Set only by `auth_failed`, and cleared when a new attempt starts.
    ///
    /// The UI's auto-retry keys off this rather than off `error`, which also
    /// carries `last_error` and PAM's own error-style messages: retrying on
    /// those cancelled a live conversation and discarded an answer the user had
    /// already given.
    pub conversation_over: bool,
    /// Set when PAM sent a notice during this conversation, which suppresses
    /// auto-retry.
    pub blocked: bool,
    pub revision: u64,
}

impl Model {
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Whether the greeter should start another attempt without being asked.
    ///
    /// Retrying immediately is right for a mistyped password: the user wants to
    /// type it again, not click first. It is wrong when PAM has explained
    /// itself — a locked account, an expired password — because the retry
    /// scrolls the explanation away, resets the form under the user, and on a
    /// faillock stack each attempt can extend the lock.
    pub fn should_auto_retry(&self) -> bool {
        self.conversation_over && self.prompt.is_none() && !self.authenticated && !self.blocked
    }

    /// Record one of PAM's explanations.
    ///
    /// PAM often splits a single explanation over two messages ("locked due to
    /// 3 failed logins" then "10 minutes left"), so they are joined rather than
    /// the second replacing the first.
    pub fn push_notice(&mut self, text: String) {
        self.notice = Some(match self.notice.take() {
            Some(prior) => format!("{prior} {text}"),
            None => text,
        });
        self.blocked = true;
    }

    /// Clear everything belonging to the previous attempt.
    pub fn begin_attempt(&mut self) {
        self.conversation_over = false;
        self.blocked = false;
        self.error = None;
        self.notice = None;
        self.prompt = None;
        self.touch();
    }

    /// Record that the connection to wdm is gone.
    ///
    /// Reported through the model because the UIs already draw everything from
    /// it: the failure lands as a sticky notice (so it is shown, not scrolled
    /// away by a retry — `blocked` suppresses auto-retry, which would only spin
    /// against a dead socket) and ends whatever the conversation was doing. The
    /// alternative was a log line nobody at a login screen can read, under a
    /// greeter stuck on "Starting session…" forever.
    pub fn link_lost(&mut self, why: &str) {
        self.push_notice(format!("Lost the connection to wdm: {why}"));
        self.authenticated = false;
        self.conversation_over = true;
        self.prompt = None;
        self.touch();
    }

    pub fn session_id(&self, index: usize) -> Option<&str> {
        self.sessions.get(index).map(|s| s.id.as_str())
    }

    /// Index of the session this user last used, the configured default, or 0.
    ///
    /// wdm reports the user's history and the machine default separately;
    /// preselecting either is the greeter's choice. History wins because the
    /// user's own last choice is a better guess than the administrator's.
    pub fn preferred_session(&self, user: usize) -> usize {
        let by_id = |id: &str| self.sessions.iter().position(|s| s.id == id);
        self.users
            .get(user)
            .and_then(|u| by_id(&u.last_session))
            .or_else(|| by_id(&self.default_session))
            .unwrap_or(0)
    }
}

/// The connection and queue this greeter drives.
pub struct Link {
    connection: Connection,
    queue: EventQueue<Model>,
    /// Set once dispatch or flush has failed. A Wayland connection does not
    /// recover; pumping a dead one would report the same failure sixty times a
    /// second.
    dead: bool,
}

impl Link {
    /// Bind `wdm_greeter_v1` on GDK's connection.
    ///
    /// Returns once the enumerate phase has been received, so a UI can be built
    /// against a populated model rather than flickering into one.
    pub fn connect(display: &gdk::Display) -> Result<(Self, Model), Box<dyn std::error::Error>> {
        let display = display
            .downcast_ref::<gdk4_wayland::WaylandDisplay>()
            .ok_or("a wdm greeter needs a Wayland session")?;

        // wl_display() is inherent on WaylandDisplay, not on an extension trait.
        let wl_display = display
            .wl_display()
            .ok_or("GDK is not running on Wayland; a wdm greeter needs a Wayland session")?;

        let backend = wl_display
            .backend()
            .upgrade()
            .ok_or("GDK's Wayland connection has already been closed")?;

        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();

        // Our own registry on our own queue. GDK has its own; the two do not
        // interfere, because libwayland routes each object's events to the queue
        // that object was created on.
        wl_display.get_registry(&handle, ());

        let mut model = Model::default();

        // Two roundtrips: the first collects globals, the second the events that
        // arrive as a result of binding wdm_greeter_v1 — which is the whole
        // enumerate phase.
        queue.roundtrip(&mut model)?;
        queue.roundtrip(&mut model)?;

        if model.greeter.is_none() {
            return Err(
                "the compositor does not offer wdm_greeter_v1; this greeter needs wdm".into(),
            );
        }

        Ok((
            Self {
                connection,
                queue,
                dead: false,
            },
            model,
        ))
    }

    /// Deliver any events that have arrived.
    ///
    /// Returns true when the model changed. A dispatch or flush failure is not
    /// swallowed: a flush that fails has dropped a request on the floor — a
    /// `respond` never answered, a `start_session` never started — so it is
    /// written into the model as [`Model::link_lost`] for the UI to show,
    /// instead of a log line under a greeter waiting forever.
    pub fn pump(&mut self, model: &mut Model) -> bool {
        if self.dead {
            return false;
        }
        let before = model.revision;

        if let Err(e) = self.queue.dispatch_pending(model) {
            log::error!("dispatching wdm_greeter_v1: {e}");
            self.dead = true;
            model.link_lost(&e.to_string());
        } else if let Err(e) = self.connection.flush() {
            log::error!("flushing: {e}");
            self.dead = true;
            model.link_lost(&e.to_string());
        }

        model.revision != before
    }
}

/// Shared handle to the connection and the model, for GTK callbacks.
pub type Shared = Rc<RefCell<Model>>;
pub type SharedLink = Rc<RefCell<Link>>;

impl Dispatch<WlRegistry, ()> for Model {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "wdm_greeter_v1"
        {
            // Version 2 adds default_session. Binding above what the compositor
            // advertises is a protocol error, and binding above what this crate
            // was generated against would promise events it cannot decode.
            state.greeter = Some(registry.bind(name, version.min(INTERFACE_VERSION), handle, ()));
        }
    }
}

impl Dispatch<WdmGreeterV1, ()> for Model {
    fn event(
        state: &mut Self,
        _: &WdmGreeterV1,
        event: wdm_greeter_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wdm_greeter_v1::Event::User {
                name,
                display_name,
                last_session,
                ..
            } => state.users.push(User {
                name,
                display_name,
                last_session,
            }),

            wdm_greeter_v1::Event::Session { id, name, .. } => {
                state.sessions.push(Session { id, name });
            }

            wdm_greeter_v1::Event::DefaultSession { id } => {
                state.default_session = id;
            }

            // Placement is left to the compositor. wdm puts a layer surface with
            // no output on the rank 0 output and moves it when ranks change, so
            // honouring the rank by hand would only reimplement that — and doing
            // so would mean mapping a wl_output back to a GdkMonitor, which is
            // more machinery than the default already gives for free.
            wdm_greeter_v1::Event::OutputRank { .. } => {}

            wdm_greeter_v1::Event::LastError { text } => {
                state.error = Some(text);
                state.touch();
            }

            // Link::connect roundtrips until this arrives, so there is nothing
            // to record — the UI is only built once the lists are populated.
            wdm_greeter_v1::Event::Done => state.touch(),

            wdm_greeter_v1::Event::Prompt { id, text, style } => {
                use wdm_greeter_v1::PromptStyle;
                match style.into_result() {
                    // Neither expects an answer. Both are PAM explaining
                    // itself, so both stick and both stop the auto-retry.
                    Ok(PromptStyle::Info | PromptStyle::Error) => state.push_notice(text),
                    Ok(style) => {
                        state.prompt = Some(Prompt {
                            id,
                            text,
                            secret: style == PromptStyle::Secret,
                        });
                    }
                    Err(e) => log::warn!("unknown prompt style: {e}"),
                }
                state.touch();
            }

            wdm_greeter_v1::Event::AuthOk => {
                state.authenticated = true;
                state.conversation_over = false;
                state.blocked = false;
                state.prompt = None;
                state.error = None;
                state.notice = None;
                state.touch();
            }

            wdm_greeter_v1::Event::AuthFailed { reason } => {
                state.authenticated = false;
                state.conversation_over = true;
                state.prompt = None;
                state.error = Some(reason);
                state.touch();
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> Model {
        Model {
            conversation_over: true,
            ..Model::default()
        }
    }

    #[test]
    fn retries_a_plain_failure() {
        // A mistyped password: the user wants to type again, not click first.
        assert!(failed().should_auto_retry());
    }

    #[test]
    fn does_not_retry_when_pam_explained_itself() {
        // The defect this guards: with a faillock-locked account, PAM sends
        // "the account is locked, 10 minutes left" and the greeter used to
        // retry straight past it — scrolling the reason away, resetting the
        // form under the user, and feeding the lock.
        let model = Model {
            blocked: true,
            notice: Some("The account is locked due to 3 failed logins.".to_owned()),
            ..failed()
        };
        assert!(!model.should_auto_retry());
    }

    #[test]
    fn does_not_retry_mid_conversation() {
        let model = Model {
            conversation_over: false,
            ..Model::default()
        };
        assert!(!model.should_auto_retry());

        // Nor while a question is still pending.
        let model = Model {
            prompt: Some(Prompt {
                id: 0,
                text: "Password:".to_owned(),
                secret: true,
            }),
            ..failed()
        };
        assert!(!model.should_auto_retry());
    }

    #[test]
    fn does_not_retry_once_authenticated() {
        let model = Model {
            authenticated: true,
            ..failed()
        };
        assert!(!model.should_auto_retry());
    }

    #[test]
    fn a_deliberate_attempt_clears_the_previous_one() {
        let mut model = Model {
            blocked: true,
            notice: Some("locked".to_owned()),
            error: Some("Authentication failure".to_owned()),
            ..failed()
        };

        model.begin_attempt();

        // The user chose to try again, so the explanation has been read.
        assert!(model.notice.is_none());
        assert!(model.error.is_none());
        assert!(!model.blocked);
        assert!(!model.conversation_over);
    }

    #[test]
    fn a_lost_link_is_shown_and_ends_the_conversation() {
        // The failure a swallowed flush error used to produce: the user clicks
        // log in, the request never leaves, and the greeter shows
        // "Starting session…" until someone power-cycles the machine.
        let mut model = Model {
            authenticated: true,
            ..Model::default()
        };
        let before = model.revision;

        model.link_lost("broken pipe");

        // Visible, attributed, and final: the notice carries the reason, the
        // conversation is over, and nothing auto-retries against a dead socket.
        assert!(model.notice.as_deref().unwrap().contains("broken pipe"));
        assert!(!model.authenticated);
        assert!(model.conversation_over);
        assert!(!model.should_auto_retry());
        assert_ne!(model.revision, before, "the UI would never repaint");
    }

    fn model_with_sessions() -> Model {
        let session = |id: &str| Session {
            id: id.to_owned(),
            name: id.to_owned(),
        };
        Model {
            sessions: vec![
                session("sway.desktop"),
                session("hyprland.desktop"),
                session("river.desktop"),
            ],
            users: vec![User {
                name: "alice".to_owned(),
                display_name: String::new(),
                last_session: String::new(),
            }],
            ..Model::default()
        }
    }

    #[test]
    fn preferred_session_prefers_the_users_history() {
        let mut model = model_with_sessions();
        model.users[0].last_session = "river.desktop".to_owned();
        // Even with a default configured: the user's own last choice is a
        // better guess than the administrator's.
        model.default_session = "hyprland.desktop".to_owned();
        assert_eq!(model.preferred_session(0), 2);
    }

    #[test]
    fn preferred_session_falls_back_to_the_configured_default() {
        // A first-time user has no history; wdm reports last_session empty and
        // the machine default on its own event.
        let mut model = model_with_sessions();
        model.default_session = "hyprland.desktop".to_owned();
        assert_eq!(model.preferred_session(0), 1);
    }

    #[test]
    fn preferred_session_defaults_to_the_first_session() {
        // No history and no configured default: preselect something rather
        // than nothing.
        let model = model_with_sessions();
        assert_eq!(model.preferred_session(0), 0);
        // An out-of-range user index must not panic either.
        assert_eq!(model.preferred_session(7), 0);
    }

    #[test]
    fn consecutive_notices_are_joined() {
        // PAM splits a lockout across two messages; keeping only the last one
        // loses the half that says why.
        let mut model = Model::default();
        model.push_notice("The account is locked.".to_owned());
        model.push_notice("(10 minutes left to unlock)".to_owned());

        let notice = model.notice.as_deref().unwrap();
        assert!(notice.contains("locked"), "{notice}");
        assert!(notice.contains("10 minutes"), "{notice}");
        // And a notice always suppresses the retry that would scroll it away.
        model.conversation_over = true;
        assert!(!model.should_auto_retry());
    }
}
