//! `wdm_greeter_v1` over GTK's own Wayland connection.
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

use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wdm_protocol::client::wdm_greeter_v1::{self, WdmGreeterV1};

/// A user wdm offered.
#[derive(Clone)]
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
#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub name: String,
}

/// A question PAM is waiting on.
#[derive(Clone)]
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
#[derive(Default)]
pub struct Model {
    pub greeter: Option<WdmGreeterV1>,
    pub users: Vec<User>,
    pub sessions: Vec<Session>,
    pub prompt: Option<Prompt>,
    pub error: Option<String>,
    pub info: Option<String>,
    /// Set by `auth_ok`; the UI launches a session in response.
    pub authenticated: bool,
    /// Set only by `auth_failed`, and cleared when a new attempt starts.
    ///
    /// The UI's auto-retry keys off this rather than off `error`, which also
    /// carries `last_error` and PAM's own error-style messages: retrying on
    /// those cancelled a live conversation and discarded an answer the user had
    /// already given.
    pub conversation_over: bool,
    pub revision: u64,
}

impl Model {
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn session_id(&self, index: usize) -> Option<&str> {
        self.sessions.get(index).map(|s| s.id.as_str())
    }

    /// Index of the session this user last used, or 0.
    ///
    /// wdm reports `last_session`; preselecting it is the greeter's choice.
    pub fn preferred_session(&self, user: usize) -> usize {
        let Some(user) = self.users.get(user) else {
            return 0;
        };
        self.sessions
            .iter()
            .position(|s| s.id == user.last_session)
            .unwrap_or(0)
    }
}

/// The connection and queue this greeter drives.
pub struct Link {
    connection: Connection,
    queue: EventQueue<Model>,
}

impl Link {
    /// Bind `wdm_greeter_v1` on GDK's connection.
    ///
    /// Returns once the enumerate phase has been received, so the UI can be
    /// built against a populated model rather than flickering into one.
    pub fn connect(
        display: &gdk4_wayland::WaylandDisplay,
    ) -> Result<(Self, Model), Box<dyn std::error::Error>> {
        // wl_display() is inherent on WaylandDisplay, not on an extension trait.
        let wl_display = display
            .wl_display()
            .ok_or("GDK is not running on Wayland; wdm-gtk-greeter needs a Wayland session")?;

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

        Ok((Self { connection, queue }, model))
    }

    /// Deliver any events that have arrived.
    ///
    /// Returns true when the model changed.
    pub fn pump(&mut self, model: &mut Model) -> bool {
        let before = model.revision;

        if let Err(e) = self.queue.dispatch_pending(model) {
            log::error!("dispatching wdm_greeter_v1: {e}");
        }
        if let Err(e) = self.connection.flush() {
            log::error!("flushing: {e}");
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
            name, interface, ..
        } = event
            && interface == "wdm_greeter_v1"
        {
            state.greeter = Some(registry.bind(name, 1, handle, ()));
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
                    Ok(PromptStyle::Info) => state.info = Some(text),
                    Ok(PromptStyle::Error) => state.error = Some(text),
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
                state.prompt = None;
                state.error = None;
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
