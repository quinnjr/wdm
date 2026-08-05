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

/// Which of PAM's two no-answer message styles a [`Notice`] carries.
///
/// The protocol's `prompt_style` has four entries; `secret` and `visible`
/// expect an answer and become a [`Prompt`], so only these two land here.
///
/// Non-exhaustive: a fifth `prompt_style` that expects no answer would force a
/// third variant here, and an out-of-tree greeter must not need a source change
/// to keep compiling. The cost today is a `_ =>` arm on out-of-tree matches,
/// which is the arm they want anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoticeKind {
    /// `PAM_TEXT_INFO` — an explanation, not a failure.
    Info,
    /// `PAM_ERROR_MSG` — PAM saying something went wrong. Distinct from
    /// [`Model::error`], which is the conversation's verdict rather than a
    /// message the stack chose to send.
    Error,
}

impl NoticeKind {
    /// The string `show_message` carries as its second argument, which is how a
    /// WebKit theme learns whether to style a message as an aside or a failure.
    ///
    /// Not every greeter uses it: the GTK greeter has one notice label and
    /// drops the distinction by design. Changing either string breaks every
    /// theme that branches on it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Error => "error",
        }
    }
}

/// Where one `prompt` event goes.
///
/// The whole routing decision, in one value the dispatch arm only switches on.
/// It was previously split between a pattern in that arm (which styles are
/// notices) and a `style == Secret` expression beside it (which prompts are
/// masked), and neither half could be tested without a live compositor: a test
/// could only restate the pattern, which is true of any pattern, including a
/// wrong one. With the decision extracted, a test names a style and gets the
/// answer the compositor would get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Routed {
    /// PAM said something and expects no answer; it sticks in
    /// [`Model::notices`].
    Notice(NoticeKind),
    /// PAM asked a question; it becomes [`Model::prompt`].
    Prompt {
        /// Whether the answer must be masked.
        secret: bool,
    },
}

/// Written as `From` rather than a bare `route(style)` because it is total and
/// infallible for every style the crate knows, and the impl block says so
/// without a reader having to find the function and check its arms.
impl From<wdm_greeter_v1::PromptStyle> for Routed {
    fn from(style: wdm_greeter_v1::PromptStyle) -> Self {
        use wdm_greeter_v1::PromptStyle;
        match style {
            // The single place the info/error distinction is made: everything
            // the two toolkit greeters do with a notice — grey aside or red
            // failure, and the `"info"`/`"error"` a WebKit theme branches on —
            // follows from these two arms.
            PromptStyle::Info => Self::Notice(NoticeKind::Info),
            PromptStyle::Error => Self::Notice(NoticeKind::Error),
            PromptStyle::Secret => Self::Prompt { secret: true },
            PromptStyle::Visible => Self::Prompt { secret: false },
            // The generated enum is `#[non_exhaustive]`, so this arm is
            // required; it is spelled out rather than folded into the `Info`
            // arm because a style added to the protocol later would otherwise
            // be rendered as an informational aside in both toolkit greeters
            // and every WebKit theme, with nothing anywhere saying why.
            //
            // Treated as a notice rather than a prompt because an unknown style
            // may well expect no answer, and a greeter waiting for a response
            // PAM is not asking for hangs the login screen; showing the text
            // and moving on cannot.
            other => {
                log::warn!(
                    "unrecognised prompt style {other:?}; showing it as an informational notice"
                );
                Self::Notice(NoticeKind::Info)
            }
        }
    }
}

/// One thing PAM said that expects no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    /// Which of PAM's two no-answer styles sent this; a WebKit theme receives
    /// it as [`NoticeKind::as_str`]'s `"info"` or `"error"`.
    pub kind: NoticeKind,
    /// The message as PAM wrote it, unmodified — it is the only account of
    /// itself the stack gives the user.
    pub text: String,
}

impl Notice {
    /// One message, ready to push onto [`Model::notices`].
    pub fn new(kind: NoticeKind, text: String) -> Self {
        Self { kind, text }
    }
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
    /// The `info` and `error` style messages PAM has sent this conversation.
    ///
    /// Sticky: they survive the end of a conversation, and exactly two places
    /// clear them — [`Model::begin_attempt`], where the user has deliberately
    /// started another attempt and so has read what was there, and `auth_ok`,
    /// where the conversation succeeded and there is nothing left to explain.
    /// Nothing else, and no passage of time, empties this. This is where a lockout
    /// notice arrives ("the account is locked, 10 minutes left"), and it is
    /// precisely the text that must not scroll past.
    ///
    /// A list rather than one string because PAM sends them one at a time and
    /// the two styles mean different things — a greeter that can show them
    /// differently should be able to. One that cannot has
    /// [`notice_text`](Model::notice_text), which joins them the way a single
    /// label needs.
    pub notices: Vec<Notice>,
    /// Bumped every time `notices` is cleared, so a consumer tracking how many
    /// it has reported can tell "the list grew" from "the list was replaced".
    ///
    /// Comparing lengths cannot answer that question. A consumer that keeps an
    /// index into `notices` and diffs once per event batch — which is what the
    /// WebKit bridge does, because `Link::pump` drains the whole batch before
    /// the diff runs — sees a clear and the pushes that follow it as one step.
    /// `auth_ok` clears the list, and `pam_open_session` then pushes the
    /// password-expiry warning, the motd and the last-login line, all in that
    /// same pump: one notice reported, one notice present, length unchanged,
    /// and the new ones are silently swallowed. Those are exactly the messages
    /// a user is meant to read on the way in.
    ///
    /// So it is a counter and not a flag: a consumer stores the value it last
    /// diffed against, and any difference means "start again from index 0",
    /// however many clears happened in between.
    pub notices_generation: u64,
    /// Set by `auth_ok`; the UI launches a session in response.
    pub authenticated: bool,
    /// Set only by `auth_failed`, and cleared when a new attempt starts.
    ///
    /// Distinct from `error`, which also carries `last_error` and PAM's own
    /// error-style messages. A UI that keyed "the attempt is over" off `error`
    /// would cancel a live conversation and discard an answer the user had
    /// already given.
    pub conversation_over: bool,
    /// Set when PAM sent a notice during this conversation — a locked account,
    /// an expiry warning, a timeout wdm composed itself.
    ///
    /// It marks a failure PAM *explained*, which a UI should leave on screen.
    ///
    /// This crate used to offer `should_auto_retry`, which combined this with
    /// `conversation_over` to tell a greeter when to open another conversation
    /// unasked. It is gone, and no greeter in this repository retries by
    /// itself, because every automatic attempt is charged to the user by
    /// `pam_faillock` exactly like a real one: a greeter that retried on an
    /// unexplained failure would arm a conversation nobody was present for,
    /// which then sat on PAM's prompt until wdm's response timeout — a second
    /// charge against an account whose owner had walked away. Enough of those
    /// lock it. A retry must be a keypress.
    pub blocked: bool,
    /// Set by [`Model::link_lost`], and never cleared: a Wayland connection
    /// does not come back.
    ///
    /// It lives here rather than being read off [`Link`] so the UI keeps
    /// drawing from one place, and because it outlives the conversation state
    /// around it — [`Model::begin_attempt`] clears everything an attempt owns,
    /// and a dead socket is not one of those things.
    ///
    /// Deliberately narrower than it sounds, and named for what it records: it
    /// is about the socket only. Whether another attempt is *sensible* is a
    /// separate question, answered by [`Model::blocked`] and
    /// [`Model::conversation_over`] — and one this crate no longer answers on a
    /// greeter's behalf. See `blocked` for why nothing retries automatically.
    ///
    /// The UI ends a conversation by inviting the user to press Enter and try
    /// again. Once this is set that invitation is a lie: `Link::pump` returns
    /// early forever after a failure, so nothing the form sends leaves the
    /// process and the screen never changes again. The greeter says what is
    /// actually true instead — the session is unreachable from here.
    ///
    /// Read as the field, in this polarity, everywhere. It is the name a WebKit
    /// theme sees as `wdm.link_dead`, and one bit spelled three ways — a field,
    /// an inverted accessor, and a JS property — was three chances to get the
    /// polarity wrong.
    pub link_dead: bool,
    pub revision: u64,
}

impl Model {
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Record one of PAM's explanations.
    ///
    /// PAM often splits a single explanation over two messages ("locked due to
    /// 3 failed logins" then "10 minutes left"), so they accumulate rather than
    /// the second replacing the first.
    pub fn push_notice(&mut self, kind: NoticeKind, text: String) {
        self.notices.push(Notice::new(kind, text));
        self.blocked = true;
    }

    /// Empty the notice list, telling anyone tracking it that it was replaced
    /// rather than appended to.
    ///
    /// One method rather than two `notices.clear()` calls so the counter cannot
    /// be forgotten at a third clear site: a clear that does not bump it is
    /// invisible to every consumer, and invisible in exactly the case the
    /// counter exists for. See [`Model::notices_generation`].
    fn clear_notices(&mut self) {
        self.notices.clear();
        self.notices_generation = self.notices_generation.wrapping_add(1);
    }

    /// Every notice as one string, for a greeter with a single label to put it
    /// in.
    ///
    /// Joined with a space because the pieces are usually one sentence PAM
    /// split in two, and `None` when there is nothing to say, so a caller can
    /// hide the label rather than showing an empty one.
    pub fn notice_text(&self) -> Option<String> {
        if self.notices.is_empty() {
            return None;
        }
        Some(
            self.notices
                .iter()
                .map(|n| n.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    /// Record that PAM accepted the user.
    ///
    /// A method rather than the body of the `auth_ok` dispatch arm because a
    /// `Dispatch::event` arm cannot be called from a test — it wants a live
    /// proxy and a queue handle — and this arm clears the notice list, which is
    /// the half of [`notices_generation`](Model::notices_generation) a consumer
    /// most needs to see and the half hardest to reach by hand.
    pub fn auth_ok(&mut self) {
        self.authenticated = true;
        self.conversation_over = false;
        self.blocked = false;
        self.prompt = None;
        self.error = None;
        self.clear_notices();
        self.touch();
    }

    /// Clear everything belonging to the previous attempt.
    pub fn begin_attempt(&mut self) {
        self.conversation_over = false;
        self.blocked = false;
        self.error = None;
        self.clear_notices();
        self.prompt = None;
        self.touch();
    }

    /// Record that the connection to wdm is gone.
    ///
    /// Reported through the model because the UIs already draw everything from
    /// it: the failure lands as a sticky notice (so it is shown, not replaced by
    /// a fresh prompt — `blocked` marks a failure that was explained, which a UI
    /// leaves on screen) and ends whatever the conversation was doing. The
    /// alternative was a log line nobody at a login screen can read, under a
    /// greeter stuck on "Starting session…" forever.
    pub fn link_lost(&mut self, why: &str) {
        self.push_notice(
            NoticeKind::Error,
            format!("Lost the connection to wdm: {why}"),
        );
        self.authenticated = false;
        self.conversation_over = true;
        self.link_dead = true;
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

    /// Deliver the events GDK has already read off the connection, and flush
    /// our own requests.
    ///
    /// This dispatches what libwayland has *queued*; it never reads the socket.
    /// Nothing here owns the fd — GDK does, and its main loop is what turns
    /// bytes on the wire into queued events for our objects. That is the whole
    /// reason this crate shares GDK's connection rather than opening its own,
    /// and it is why a stalled GDK loop shows up here as a `pump` that quietly
    /// returns false forever rather than as an error. Reading the fd ourselves
    /// would race the owner of it.
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
        state.apply(event);
    }
}

impl Model {
    /// Fold one `wdm_greeter_v1` event into the model.
    ///
    /// The whole of the [`Dispatch`] impl above, which does nothing else. It
    /// lives here because `Dispatch::event` needs a bound `WdmGreeterV1` and a
    /// live `Connection` to call, and neither exists without a compositor on
    /// the other end — so a test that wants to know what an event does to the
    /// model could only restate the arm rather than run it.
    pub(crate) fn apply(&mut self, event: wdm_greeter_v1::Event) {
        match event {
            wdm_greeter_v1::Event::User {
                name,
                display_name,
                last_session,
                ..
            } => self.users.push(User {
                name,
                display_name,
                last_session,
            }),

            wdm_greeter_v1::Event::Session { id, name, .. } => {
                self.sessions.push(Session { id, name });
            }

            wdm_greeter_v1::Event::DefaultSession { id } => {
                self.default_session = id;
            }

            // Placement is left to the compositor. wdm puts a layer surface with
            // no output on the rank 0 output and moves it when ranks change, so
            // honouring the rank by hand would only reimplement that — and doing
            // so would mean mapping a wl_output back to a GdkMonitor, which is
            // more machinery than the default already gives for free.
            wdm_greeter_v1::Event::OutputRank { .. } => {}

            wdm_greeter_v1::Event::LastError { text } => {
                self.error = Some(text);
                self.touch();
            }

            // Link::connect roundtrips until this arrives, so there is nothing
            // to record — the UI is only built once the lists are populated.
            wdm_greeter_v1::Event::Done => self.touch(),

            wdm_greeter_v1::Event::Prompt { id, text, style } => {
                match style.into_result() {
                    // Which of the two this is belongs to `Routed`, not here: a
                    // decision made in a match arm is a decision no test can
                    // reach. `Err` is a value outside the enum entirely — a
                    // compositor sending a style number this crate's protocol
                    // XML does not define — and there is nothing to show.
                    Ok(style) => match Routed::from(style) {
                        // Expects no answer. PAM explaining itself, so it
                        // sticks: it sets `blocked`, which marks a failure a UI
                        // leaves on screen rather than replacing with a fresh
                        // prompt.
                        Routed::Notice(kind) => self.push_notice(kind, text),
                        Routed::Prompt { secret } => {
                            self.prompt = Some(Prompt { id, text, secret });
                        }
                    },
                    Err(e) => log::warn!("unknown prompt style: {e}"),
                }
                self.touch();
            }

            wdm_greeter_v1::Event::AuthOk => self.auth_ok(),

            wdm_greeter_v1::Event::AuthFailed { reason } => {
                self.authenticated = false;
                self.conversation_over = true;
                self.prompt = None;
                self.error = Some(reason);
                self.touch();
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model that has just been failed by wdm, through the event that fails
    /// it.
    ///
    /// Built by `apply` rather than by a struct literal: a literal would assert
    /// its own initialiser back to itself, and no edit to the `AuthFailed` arm
    /// could make a test written that way fail.
    fn failed() -> Model {
        let mut model = Model::default();
        model.apply(wdm_greeter_v1::Event::AuthFailed {
            reason: "Authentication failure".to_owned(),
        });
        model
    }

    #[test]
    fn a_plain_failure_leaves_nothing_for_the_ui_to_explain() {
        // The state a mistyped password produces: over, unexplained, and no
        // longer holding a question. This used to be the input to
        // `should_auto_retry`, which answered "yes" — and that yes is what
        // armed a conversation nobody was present for. The state is still worth
        // pinning; what a greeter does with it is now the greeter's, and every
        // greeter in this repository waits for a keypress.
        //
        // The transition is what is pinned, not the end state: the model starts
        // mid-conversation, holding a question and believing it authenticated,
        // and `AuthFailed` is what has to undo all three.
        let mut model = Model::default();
        model.apply(wdm_greeter_v1::Event::Prompt {
            id: 1,
            text: "Password: ".to_owned(),
            style: wayland_client::WEnum::Value(wdm_greeter_v1::PromptStyle::Secret),
        });
        model.authenticated = true;
        let before = model.revision;
        assert!(model.prompt.is_some(), "the setup did not arm a prompt");

        model.apply(wdm_greeter_v1::Event::AuthFailed {
            reason: "Authentication failure".to_owned(),
        });

        assert!(model.conversation_over, "the attempt must be over");
        assert!(!model.authenticated, "a failure must revoke authentication");
        assert!(
            model.prompt.is_none(),
            "a question nobody will read the answer to must not stay on screen"
        );
        assert_eq!(model.error.as_deref(), Some("Authentication failure"));
        assert!(!model.blocked, "a bare failure carries no explanation");
        assert!(model.notice_text().is_none());
        assert_ne!(model.revision, before, "the UI was never told to redraw");
    }

    #[test]
    fn pam_explaining_itself_is_recorded_as_blocked() {
        // With a faillock-locked account PAM sends "the account is locked, 10
        // minutes left". `blocked` is how a UI knows the failure came with a
        // reason worth leaving on screen, rather than one worth replacing with
        // a fresh prompt.
        let mut model = failed();
        model.push_notice(
            NoticeKind::Info,
            "The account is locked due to 3 failed logins.".to_owned(),
        );

        assert!(model.blocked);
        assert!(
            model
                .notice_text()
                .is_some_and(|t| t.contains("3 failed logins")),
            "the explanation must survive for the UI to show"
        );
    }

    #[test]
    fn a_deliberate_attempt_clears_the_previous_one() {
        let mut model = Model {
            blocked: true,
            notices: vec![Notice::new(NoticeKind::Info, "locked".to_owned())],
            error: Some("Authentication failure".to_owned()),
            ..failed()
        };

        model.begin_attempt();

        // The user chose to try again, so the explanation has been read.
        assert!(model.notices.is_empty());
        assert!(model.error.is_none());
        assert!(!model.blocked);
        assert!(!model.conversation_over);
    }

    #[test]
    fn a_timed_out_attempt_does_not_retry_itself() {
        // The greeter half of the unattended-lockout loop. wdm answers a prompt
        // that goes unanswered with an `error` prompt and then auth_failed; this
        // asserts that pair leaves the model unwilling to start another attempt.
        //
        // Without the error prompt only the auth_failed arrives, which is
        // indistinguishable from a mistyped password — so the greeter retried,
        // wdm armed the timeout again, and the two spun. Every turn cost a
        // pam_faillock entry, and three of those lock the account, on a machine
        // nobody has touched.
        let mut model = Model::default();

        // Ordering is the contract, not an implementation detail: `blocked` has
        // to be set before the conversation ends, because the retry decision is
        // taken the moment auth_failed lands.
        model.push_notice(
            NoticeKind::Error,
            "The login attempt timed out waiting for a response.".to_owned(),
        );
        model.authenticated = false;
        model.conversation_over = true;
        model.prompt = None;
        model.error = Some("authentication failed".to_owned());

        assert!(
            model.blocked,
            "a timed-out attempt must arrive explained; unexplained is what a \
             greeter used to re-arm on, and that loop is the lockout"
        );
        assert!(model.notice_text().unwrap().contains("timed out"));
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
        assert!(model.notice_text().unwrap().contains("broken pipe"));
        // A lost link is PAM-independent, but it is a failure, so it is styled
        // as one rather than as an aside.
        assert_eq!(model.notices[0].kind, NoticeKind::Error);
        assert!(!model.authenticated);
        assert!(model.conversation_over);
        assert!(
            model.blocked,
            "the UI must show this rather than move past it"
        );
        assert_ne!(model.revision, before, "the UI would never repaint");
    }

    #[test]
    fn a_lost_link_leaves_no_retry_to_offer() {
        // The defect this guards: the form ended a lost link with "Press Enter
        // to try again", but Link::pump returns early forever once it has
        // failed, so Enter provably could not reach wdm.
        let mut model = Model::default();
        assert!(!model.link_dead, "a live link can be retried");

        model.link_lost("broken pipe");
        assert!(model.link_dead);

        // And starting an attempt does not resurrect it: begin_attempt clears
        // what the attempt owned, and the socket is not that.
        model.begin_attempt();
        assert!(model.link_dead);
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
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        model.push_notice(NoticeKind::Info, "(10 minutes left to unlock)".to_owned());

        // The exact string, because the separator is the whole point: the two
        // halves are one sentence and must read as one.
        let notice = model.notice_text().unwrap();
        assert_eq!(notice, "The account is locked. (10 minutes left to unlock)");
        // And either half is enough to mark the failure explained, so a UI
        // leaves it up rather than replacing it with a fresh prompt.
        assert!(model.blocked);
    }

    #[test]
    fn no_notices_is_none_rather_than_empty() {
        // The GTK greeter does set_visible(notice.is_some()), so a Some("")
        // here would show a blank notice row on every screen from boot.
        assert_eq!(Model::default().notice_text(), None);
    }

    #[test]
    fn every_prompt_style_routes_where_pam_meant_it_to() {
        use wdm_greeter_v1::PromptStyle;

        // All four, against the routing the dispatch arm actually performs —
        // not against a pattern restated in the test, which the previous
        // version of this did and which stayed green however the arm was
        // widened.
        //
        // Swapping the first two renders a locked account as a red failure and
        // "10 minutes left" as a grey aside in both toolkit greeters. Routing
        // either of the last two to a notice swallows the password box, and
        // the user faces a login screen with nothing to type into.
        assert_eq!(
            Routed::from(PromptStyle::Info),
            Routed::Notice(NoticeKind::Info),
        );
        assert_eq!(
            Routed::from(PromptStyle::Error),
            Routed::Notice(NoticeKind::Error),
        );
        assert_eq!(
            Routed::from(PromptStyle::Secret),
            Routed::Prompt { secret: true },
        );
        // Visible is a prompt too, but the answer is echoed: it is PAM asking
        // for a one-time token or a username, not a password.
        assert_eq!(
            Routed::from(PromptStyle::Visible),
            Routed::Prompt { secret: false },
        );

        // And these are the strings a WebKit theme branches on; changing either
        // breaks every theme, so they are pinned here rather than only by a
        // substring search in another crate.
        assert_eq!(NoticeKind::Info.as_str(), "info");
        assert_eq!(NoticeKind::Error.as_str(), "error");
    }

    #[test]
    fn clearing_the_notices_is_visible_to_someone_counting_them() {
        // The defect this guards: the WebKit bridge kept an index into
        // `notices` and detected a clear by comparing lengths. auth_ok clears
        // while the phase is still Launching, Link::pump drains the whole
        // batch before the diff runs, and pam_open_session's messages land in
        // that same batch — one reported, one present, no change seen, and the
        // password-expiry warning, the motd and the last-login line all lost.
        let mut model = Model::default();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        let start = model.notices_generation;

        // Appending is not a replacement: a consumer's index stays valid.
        model.push_notice(NoticeKind::Info, "(10 minutes left)".to_owned());
        assert_eq!(model.notices_generation, start, "a push is not a clear");

        // Both clear sites bump it.
        model.begin_attempt();
        let after_attempt = model.notices_generation;
        assert_ne!(after_attempt, start, "begin_attempt cleared the list");

        // What a consumer has reported at this point: one notice, at this
        // generation.
        model.push_notice(NoticeKind::Error, "Authentication failure".to_owned());
        let reported = (model.notices.len(), model.notices_generation);

        // Then the clear and the refill that lengths cannot tell apart, both
        // inside one pump.
        model.auth_ok();
        model.push_notice(NoticeKind::Info, "Your password expires today.".to_owned());

        assert_eq!(model.notices.len(), reported.0, "the length is no help");
        assert_ne!(
            model.notices_generation, reported.1,
            "the generation is what says the list was replaced, not appended to",
        );
        assert_ne!(model.notices_generation, after_attempt);
    }
}
