//! wdm's reference greeter.
//!
//! An ordinary Wayland client: it binds `wlr-layer-shell` for its fullscreen
//! surface and `wdm_greeter_v1` to log the user in. It renders with `wl_shm` and
//! no toolkit, which keeps the shipped default free of a GTK or Qt dependency and
//! makes it a readable example of the protocol.
//!
//! Policy this greeter chooses, which the protocol deliberately leaves open: it
//! draws only on the rank 0 output, and preselects each user's last session,
//! falling back to the machine default for a user with no history.

mod text;
mod ui;

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_output::WlOutput,
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};
use wdm_protocol::client::wdm_greeter_v1::{self, WdmGreeterV1};

use text::Canvas;

/// The highest `wdm_greeter_v1` version this greeter understands.
///
/// Version 2 is where `default_session` appears. Binding lower would leave the
/// machine-default fallback in [`App::select_last_session`] unreachable, because
/// the compositor never sends an event a resource is not bound high enough for.
const GREETER_VERSION: u32 = 2;

/// A user advertised by the enumerate phase.
struct User {
    name: String,
    display_name: String,
    last_session: String,
}

/// A session advertised by the enumerate phase.
struct Session {
    id: String,
    name: String,
}

/// Double-buffered `wl_shm` storage for the surface.
///
/// One pool, two slots, reused across frames. A fresh memfd and pool per frame
/// meant a full-screen allocation, copy and fd round trip *per keystroke* —
/// roughly 33MB each way on a 4K output, on the one path a login screen
/// actually exercises.
struct Buffers {
    file: std::fs::File,
    pool: WlShmPool,
    size: (i32, i32),
    slots: Vec<Slot>,
}

struct Slot {
    buffer: WlBuffer,
    offset: usize,
    /// Cleared when the compositor sends `wl_buffer.release`.
    ///
    /// Destroying a buffer at commit time, as this greeter used to, hands the
    /// compositor a resource that is dead before it renders from it. Atomic
    /// because wayland-client requires object user data to be `Send + Sync`,
    /// not because anything here is threaded.
    busy: Arc<AtomicBool>,
}

impl Buffers {
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<App>,
        width: i32,
        height: i32,
    ) -> Option<Self> {
        use rustix::fs::{MemfdFlags, memfd_create};

        let slot_len = (width as usize) * (height as usize) * 4;
        let total = slot_len * 2;

        let fd = match memfd_create("wdm-greeter", MemfdFlags::CLOEXEC) {
            Ok(fd) => fd,
            Err(e) => {
                log::error!("memfd_create: {e}");
                return None;
            }
        };

        let file = std::fs::File::from(fd);
        if let Err(e) = file.set_len(total as u64) {
            log::error!("sizing the frame buffer: {e}");
            return None;
        }

        let pool = shm.create_pool(file.as_fd(), total as i32, qh, ());
        let slots = (0..2)
            .map(|index| {
                let offset = index * slot_len;
                let busy = Arc::new(AtomicBool::new(false));
                let buffer = pool.create_buffer(
                    offset as i32,
                    width,
                    height,
                    width * 4,
                    wl_shm::Format::Argb8888,
                    qh,
                    busy.clone(),
                );
                Slot {
                    buffer,
                    offset,
                    busy,
                }
            })
            .collect();

        Some(Self {
            file,
            pool,
            size: (width, height),
            slots,
        })
    }

    /// A slot the compositor is not currently reading from.
    fn free_slot(&self) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|s| !s.busy.load(Ordering::Relaxed))
    }
}

impl Drop for Buffers {
    fn drop(&mut self) {
        for slot in &self.slots {
            slot.buffer.destroy();
        }
        self.pool.destroy();
    }
}

/// A prompt awaiting an answer.
struct Prompt {
    id: u32,
    text: String,
    secret: bool,
}

struct App {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    greeter: Option<WdmGreeterV1>,
    /// Outputs by registry name, so a rank can be resolved to an object.
    outputs: HashMap<u32, WlOutput>,

    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    buffers: Option<Buffers>,
    width: i32,
    height: i32,
    configured: bool,

    users: Vec<User>,
    sessions: Vec<Session>,
    /// Session names in `sessions` order, built once when `done` arrives; the
    /// list is immutable after the enumerate phase, so `draw` borrows it
    /// rather than rebuilding it every frame.
    session_names: Vec<String>,
    /// The machine default session id, empty when the machine has none.
    default_session: String,
    /// The rank 0 output, once wdm has said which it is.
    primary: Option<WlOutput>,
    ready: bool,

    user_index: usize,
    session_index: usize,
    /// Whether the session drop-down is open.
    menu_open: bool,
    prompt: Option<Prompt>,
    answer: String,
    /// What the user typed before there was a prompt to put it in.
    ///
    /// Nothing opens a PAM conversation until the user asks to log in, so the
    /// first thing they type arrives before PAM has asked for it. Held across
    /// `create_session` and spent on the first prompt that wants an answer.
    pending_answer: Option<String>,
    error: Option<String>,
    info: Option<String>,
    launching: bool,
    /// True while a `create_session` is outstanding.
    authenticating: bool,
    /// True once PAM explained itself mid-conversation (an Info or Error
    /// prompt, such as faillock's lockout notice). Suppresses the automatic
    /// retry on `auth_failed`, which would scroll the explanation away.
    blocked: bool,

    /// The frame's pixels, reused across frames and rebuilt only on resize.
    /// `ui::paint` overwrites every pixel, so it needs no clearing between
    /// frames.
    canvas: Option<Canvas>,
    xkb: Option<xkbcommon::xkb::State>,

    exit: bool,
    needs_redraw: bool,
}

impl App {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            greeter: None,
            outputs: HashMap::new(),
            surface: None,
            layer_surface: None,
            buffers: None,
            width: 0,
            height: 0,
            configured: false,
            users: Vec::new(),
            sessions: Vec::new(),
            session_names: Vec::new(),
            default_session: String::new(),
            primary: None,
            ready: false,
            user_index: 0,
            session_index: 0,
            menu_open: false,
            prompt: None,
            answer: String::new(),
            pending_answer: None,
            error: None,
            info: None,
            launching: false,
            authenticating: false,
            blocked: false,
            canvas: None,
            xkb: None,
            exit: false,
            needs_redraw: true,
        }
    }

    /// Preselect a session for the current user: their history first, the
    /// machine default when they have none, the first session otherwise.
    ///
    /// wdm reports `last_session` and `default_session`; applying them is the
    /// greeter's choice. `last_session` is honestly empty for a user who has
    /// never logged in, so the machine default is a fallback this greeter
    /// applies itself rather than something wdm folds into the user's history.
    fn select_last_session(&mut self) {
        let Some(user) = self.users.get(self.user_index) else {
            return;
        };
        self.session_index = self
            .sessions
            .iter()
            .position(|s| s.id == user.last_session)
            .or_else(|| {
                self.sessions
                    .iter()
                    .position(|s| s.id == self.default_session)
            })
            .unwrap_or(0);
    }

    /// Start authenticating the selected user.
    fn begin_auth(&mut self) {
        if self.authenticating || self.launching {
            return;
        }
        let (Some(greeter), Some(user)) = (&self.greeter, self.users.get(self.user_index)) else {
            return;
        };

        self.answer.clear();
        self.prompt = None;
        self.info = None;
        // A new attempt is underway, so whatever PAM said about the last one
        // no longer blocks anything.
        self.blocked = false;
        self.authenticating = true;
        greeter.create_session(user.name.clone());
        self.needs_redraw = true;
    }

    /// Abandon the current conversation.
    ///
    /// It ends the conversation rather than restarting it. Starting one here
    /// would arm PAM for whoever the user list happens to land on, which is what
    /// `submit` exists to be the only cause of.
    fn abandon_auth(&mut self) {
        if let Some(greeter) = &self.greeter {
            greeter.cancel();
        }
        self.authenticating = false;
        self.prompt = None;
        self.answer.clear();
        self.pending_answer = None;
    }

    fn cycle_user(&mut self) {
        if self.users.len() < 2 {
            return;
        }
        self.user_index = (self.user_index + 1) % self.users.len();
        self.select_last_session();
        // A drop-down left open would now be showing the previous user's choice.
        self.menu_open = false;
        self.error = None;
        // The conversation is per user, so switching users means starting over.
        self.abandon_auth();
    }

    /// Open or close the session drop-down.
    fn toggle_menu(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        self.menu_open = !self.menu_open;
        self.needs_redraw = true;
    }

    /// Move the drop-down selection by `delta`, wrapping at both ends.
    ///
    /// Wrapping rather than clamping because the list is short and reaching the
    /// last entry from the first should not need five keystrokes.
    fn move_selection(&mut self, delta: isize) {
        let len = self.sessions.len();
        if len == 0 {
            return;
        }
        let len = len as isize;
        let index = self.session_index as isize + delta;
        self.session_index = index.rem_euclid(len) as usize;
        self.needs_redraw = true;
    }

    /// Keys that belong to the drop-down while it is open.
    ///
    /// Returns true when the key was consumed, so Enter and Escape choose and
    /// dismiss rather than submitting the password or clearing the field.
    ///
    /// Only non-printable keys are claimed. The drop-down used to take `j` and
    /// `k` as vi motions, which meant a password containing either lost those
    /// characters silently while the list was open — no echo, no error, and a
    /// login that fails for no visible reason. A login screen's drop-down is not
    /// a pager, and the arrows already move the selection.
    fn menu_key(&mut self, keysym: xkbcommon::xkb::Keysym) -> bool {
        use xkbcommon::xkb::keysyms;

        match keysym.raw() {
            keysyms::KEY_Up => self.move_selection(-1),
            keysyms::KEY_Down => self.move_selection(1),
            keysyms::KEY_Home => {
                self.session_index = 0;
                self.needs_redraw = true;
            }
            keysyms::KEY_End => {
                self.session_index = self.sessions.len().saturating_sub(1);
                self.needs_redraw = true;
            }
            keysyms::KEY_Return
            | keysyms::KEY_KP_Enter
            | keysyms::KEY_Escape
            | keysyms::KEY_F2 => {
                // Escape and Enter both close on the highlighted row: moving the
                // highlight *is* the selection, so there is nothing to revert.
                self.menu_open = false;
                self.needs_redraw = true;
            }
            _ => return false,
        }

        true
    }

    /// Answer the pending prompt.
    fn submit(&mut self) {
        let Some(greeter) = &self.greeter else {
            return;
        };
        let Some(prompt) = self.prompt.take() else {
            // Nothing is being asked, because nothing opens a conversation until
            // this moment: arming PAM on behalf of a user who has not asked to
            // log in charges `pam_faillock` for the greeter's own patience, and
            // a conversation cannot be ended without failing it.
            //
            // Keep what was typed so the prompt PAM is about to send can be
            // answered with it, rather than making the user type it twice.
            if !self.authenticating {
                self.pending_answer = Some(std::mem::take(&mut self.answer));
                self.begin_auth();
            }
            return;
        };

        greeter.respond(prompt.id, std::mem::take(&mut self.answer));
        self.needs_redraw = true;
    }

    /// Record a message from the conversation.
    fn pam_prompt(&mut self, id: u32, text: String, style: wdm_greeter_v1::PromptStyle) {
        use wdm_greeter_v1::PromptStyle;
        match style {
            // Neither expects an answer. Both are PAM explaining itself —
            // faillock's lockout notice arrives this way — so both suppress
            // the automatic retry that would scroll the explanation away.
            PromptStyle::Info => {
                self.info = Some(text);
                self.blocked = true;
            }
            PromptStyle::Error => {
                self.error = Some(text);
                self.blocked = true;
            }
            style => {
                // The answer the user gave before PAM had asked for it. Spent
                // on the first question that wants one, so from their side they
                // typed a password, pressed Enter, and it was checked.
                //
                // `take()` rather than a peek: it is worth exactly one prompt. A
                // stack that asks twice must get the second answer from the
                // user, and silently resending the password would be putting
                // that secret somewhere it was never typed for.
                if let (Some(answer), Some(greeter)) =
                    (self.pending_answer.take(), &self.greeter)
                {
                    greeter.respond(id, answer);
                    self.prompt = None;
                    self.answer.clear();
                    self.needs_redraw = true;
                    return;
                }
                self.prompt = Some(Prompt {
                    id,
                    text,
                    secret: style == PromptStyle::Secret,
                });
                self.answer.clear();
            }
        }
        self.needs_redraw = true;
    }

    /// Whether a failed conversation should be restarted without a keypress.
    ///
    /// Auto-retry is a convenience for the mistyped-password case. When PAM
    /// explained the failure — an account locked with minutes left on the
    /// clock — restarting would clear that explanation from the screen and
    /// reset the form under the user, so the failure is left standing and
    /// Enter starts the next attempt deliberately.
    fn should_auto_retry(&self) -> bool {
        !self.blocked
    }

    /// React to `auth_failed`: show the reason, and say whether the caller
    /// should start the next attempt.
    ///
    /// The decision is returned rather than acted on here so that it is
    /// observable: a test can assert the retry without a compositor to talk to,
    /// which a self-contained `begin_auth()` call could not offer.
    #[must_use]
    fn auth_failed(&mut self, reason: String) -> bool {
        self.authenticating = false;
        self.prompt = None;
        self.answer.clear();
        // The conversation it was buffered for is over. It must not survive into
        // the next one, where it would be spent on a prompt the user never saw.
        self.pending_answer = None;
        // A conversation that already explained itself keeps its explanation:
        // `pam_prompt` routed PAM's own error text here, and "Authentication
        // failure" is strictly less informative than "account locked, 10
        // minutes left".
        if !self.blocked {
            self.error = Some(reason);
        }
        self.needs_redraw = true;
        // Retrying asks again so the user can have another go without pressing
        // anything.
        //
        // wdm delays each failure response, and the delay grows, so this
        // converges rather than spinning — but the first failure of a
        // conversation is reported with no delay at all, so a PAM stack
        // that denies instantly does produce one uncontrolled round trip
        // before the backoff engages.
        self.should_auto_retry()
    }

    /// Launch the selected session; valid only after `auth_ok`.
    fn start_session(&mut self) {
        let (Some(greeter), Some(session)) = (&self.greeter, self.sessions.get(self.session_index))
        else {
            self.error = Some("no session to start".to_owned());
            self.needs_redraw = true;
            return;
        };

        self.launching = true;
        self.needs_redraw = true;
        // No extra environment: everything this greeter could add, wdm already
        // sets more authoritatively.
        greeter.start_session(session.id.clone(), Vec::new());
    }

    /// Create the layer surface once the globals and the primary output exist.
    fn ensure_surface(&mut self, qh: &QueueHandle<Self>) {
        if self.layer_surface.is_some() {
            return;
        }
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };

        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            // Explicitly the rank 0 output rather than None: letting the
            // compositor choose would abandon this greeter's stated policy.
            self.primary.as_ref(),
            zwlr_layer_shell_v1::Layer::Overlay,
            "wdm-greeter".to_owned(),
            qh,
            (),
        );

        // Anchored to all four edges with exclusive keyboard focus: this is a
        // login screen, so nothing else may hold the keyboard.
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer_surface.set_exclusive_zone(-1);
        layer_surface
            .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive);
        // Zero means "whatever size the compositor gives us", which for wdm is
        // always the whole output.
        layer_surface.set_size(0, 0);

        surface.commit();

        self.surface = Some(surface);
        self.layer_surface = Some(layer_surface);
    }

    /// Paint the frame and attach it from a free slot of the shm pool.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.configured || self.width <= 0 || self.height <= 0 {
            return;
        }
        let (Some(shm), Some(surface)) = (&self.shm, &self.surface) else {
            return;
        };

        // Reused across frames: a fresh canvas per frame meant a full-screen
        // allocation and memset per keystroke, ~8MB of each at 4K.
        if self
            .canvas
            .as_ref()
            .is_none_or(|c| (c.width, c.height) != (self.width, self.height))
        {
            self.canvas = Some(Canvas::new(self.width, self.height));
        }
        let canvas = self.canvas.as_mut().expect("just ensured above");

        if !self.ready {
            ui::paint_message(canvas, "Connecting…", false);
        } else if self.users.is_empty() {
            ui::paint_message(canvas, "No users available to log in", true);
        } else if self.sessions.is_empty() {
            ui::paint_message(canvas, "No sessions installed", true);
        } else {
            let user = &self.users[self.user_index];

            ui::paint(
                canvas,
                &ui::View {
                    username: &user.name,
                    display_name: &user.display_name,
                    sessions: &self.session_names,
                    session_index: self.session_index,
                    menu_open: self.menu_open,
                    prompt: self.prompt.as_ref().map(|p| p.text.as_str()),
                    answer: &self.answer,
                    // Masked unless PAM said the answer may be echoed. Defaulting
                    // to masked matters: an unmasked password is worse than an
                    // unnecessarily masked username.
                    secret: self.prompt.as_ref().is_none_or(|p| p.secret),
                    error: self.error.as_deref(),
                    info: self.info.as_deref(),
                    launching: self.launching,
                    multiple_users: self.users.len() > 1,
                    multiple_sessions: self.sessions.len() > 1,
                },
            );
        }

        // Rebuilt only when the surface is resized.
        if self
            .buffers
            .as_ref()
            .is_none_or(|b| b.size != (self.width, self.height))
        {
            self.buffers = Buffers::new(shm, qh, self.width, self.height);
        }

        let Some(buffers) = &self.buffers else {
            return;
        };
        let Some(slot) = buffers.free_slot() else {
            // Both slots are still held by the compositor. Leaving needs_redraw
            // set means the next dispatch — the release that frees one — repaints
            // rather than dropping the frame.
            log::debug!("no free buffer, deferring the frame");
            return;
        };

        // Written through the mapping the compositor already has, so no new fd
        // and no new pool.
        {
            use std::os::unix::fs::FileExt;
            if let Err(e) = buffers.file.write_all_at(&canvas.data, slot.offset as u64) {
                log::error!("writing the frame: {e}");
                return;
            }
        }

        slot.busy.store(true, Ordering::Relaxed);
        surface.attach(Some(&slot.buffer), 0, 0);
        // ponytail: whole-surface damage and a whole-surface repaint. The
        // allocation and fd churn are gone — the canvas persists across frames
        // and glyphs are cached — so what remains per keystroke is a redraw of
        // a mostly-static form plus one full-frame copy into the pool. Track a
        // dirty rect for the answer field, or mmap the pool and paint into the
        // slot directly, if either ever shows up in a profile.
        surface.damage_buffer(0, 0, self.width, self.height);
        surface.commit();

        self.needs_redraw = false;
    }

    fn key(&mut self, keysym: xkbcommon::xkb::Keysym, utf8: String) {
        use xkbcommon::xkb::keysyms;

        // The drop-down takes precedence: while it is open, Enter chooses a
        // session rather than submitting a half-typed password.
        if self.menu_open && self.menu_key(keysym) {
            return;
        }

        match keysym.raw() {
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.submit(),

            keysyms::KEY_BackSpace => {
                self.answer.pop();
                self.needs_redraw = true;
            }

            keysyms::KEY_Escape => {
                self.answer.clear();
                self.error = None;
                self.needs_redraw = true;
            }

            keysyms::KEY_F1 => self.cycle_user(),
            keysyms::KEY_F2 => self.toggle_menu(),

            // Ctrl+U, the readline convention for clearing the line.
            keysyms::KEY_U | keysyms::KEY_u if self.ctrl_held() => {
                self.answer.clear();
                self.needs_redraw = true;
            }

            _ => {
                // Only printable text is appended. A control character would be
                // sent to PAM as part of a password the user cannot see.
                if !utf8.is_empty() && !utf8.chars().any(char::is_control) {
                    self.answer.push_str(&utf8);
                    self.needs_redraw = true;
                }
            }
        }
    }

    fn ctrl_held(&self) -> bool {
        use xkbcommon::xkb;
        self.xkb
            .as_ref()
            .is_some_and(|s| s.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE))
    }
}

impl Dispatch<WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(5), qh, ()));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wdm_greeter_v1" => {
                    state.greeter =
                        Some(registry.bind(name, version.min(GREETER_VERSION), qh, ()));
                }
                "wl_seat" => {
                    registry.bind::<WlSeat, _, _>(name, version.min(7), qh, ());
                }
                "wl_output" => {
                    state
                        .outputs
                        .insert(name, registry.bind(name, version.min(4), qh, name));
                }
                _ => {}
            },

            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }

            _ => {}
        }
    }
}

impl Dispatch<WdmGreeterV1, ()> for App {
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

            wdm_greeter_v1::Event::OutputRank { output, rank } => {
                if rank != 0 || state.primary.as_ref() == Some(&output) {
                    return;
                }

                // Re-emitted on hotplug. If the primary output changed, the
                // surface has to move, so the old one is torn down and
                // ensure_surface builds a new one against the new primary.
                state.primary = Some(output);
                if let Some(old) = state.layer_surface.take() {
                    old.destroy();
                }
                if let Some(old) = state.surface.take() {
                    old.destroy();
                }
                // Sized for the old surface, and its slots belong to a pool the
                // compositor no longer has a surface for.
                state.buffers = None;
                state.configured = false;
            }

            // Kept on screen until the user clears it: a session that failed to
            // start is the most important thing they need to see.
            wdm_greeter_v1::Event::LastError { text } => {
                state.error = Some(text);
                state.needs_redraw = true;
            }

            wdm_greeter_v1::Event::Done => {
                state.ready = true;
                // The session list is final now, so the names the frame needs
                // are built once here rather than cloned every repaint.
                state.session_names = state.sessions.iter().map(|s| s.name.clone()).collect();
                state.select_last_session();
                // No conversation is started here, deliberately: see `submit`.
                // Opening one costs a PAM attempt on a machine nobody has
                // touched, and a login screen left alone used to spend those
                // until `pam_faillock` locked the first user in the list out.
            }

            wdm_greeter_v1::Event::Prompt { id, text, style } => match style.into_result() {
                Ok(style) => state.pam_prompt(id, text, style),
                Err(e) => log::warn!("unknown prompt style: {e}"),
            },

            wdm_greeter_v1::Event::AuthOk => {
                state.authenticating = false;
                state.error = None;
                state.start_session();
            }

            wdm_greeter_v1::Event::AuthFailed { reason } => {
                if state.auth_failed(reason) {
                    state.begin_auth();
                }
            }

            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                surface.ack_configure(serial);
                state.width = width as i32;
                state.height = height as i32;
                state.configured = true;
                state.needs_redraw = true;
            }

            zwlr_layer_surface_v1::Event::Closed => state.exit = true,

            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event
            && capabilities
                .into_result()
                .is_ok_and(|c| c.contains(wl_seat::Capability::Keyboard))
        {
            seat.get_keyboard(qh, ());
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use xkbcommon::xkb;

        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(
                    format.into_result(),
                    Ok(wl_keyboard::KeymapFormat::XkbV1)
                ) {
                    log::error!("the compositor sent an unsupported keymap format");
                    return;
                }

                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                // SAFETY: the compositor guarantees the fd holds a readable,
                // NUL-terminated keymap of exactly `size` bytes.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };

                match keymap {
                    Ok(Some(keymap)) => state.xkb = Some(xkb::State::new(&keymap)),
                    // Without a keymap nothing can be typed, so this is loud
                    // rather than a silently dead keyboard.
                    Ok(None) => log::error!("the compositor's keymap did not compile"),
                    Err(e) => log::error!("reading the keymap: {e}"),
                }
            }

            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut state.xkb {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }

            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if !matches!(key_state.into_result(), Ok(wl_keyboard::KeyState::Pressed)) {
                    return;
                }
                let Some(xkb) = &state.xkb else {
                    return;
                };

                // Wayland reports evdev keycodes; xkb wants them offset by 8.
                let code = xkb::Keycode::new(key + 8);
                let keysym = xkb.key_get_one_sym(code);
                let utf8 = xkb.key_get_utf8(code);

                state.key(keysym, utf8);
            }

            _ => {}
        }
    }
}

// These deliver no events this greeter needs to act on.
delegate_noop!(App: ignore WlCompositor);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlShmPool);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore ZwlrLayerShellV1);

impl Dispatch<WlBuffer, Arc<AtomicBool>> for App {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Release means the compositor is done reading; the slot can be redrawn.
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Relaxed);
        }
    }
}

impl Dispatch<WlOutput, u32> for App {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wayland_client::protocol::wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Geometry and mode are the compositor's business: the layer surface is
        // configured to the size wdm chooses.
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::new().filter("WDM_GREETER_LOG")).init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    if !text::have_font() {
        // The form would render as empty boxes. Saying so beats leaving someone
        // staring at an unreadable login screen.
        log::error!("no usable font found; the login form will have no text");
    }

    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let qh = queue.handle();

    connection.display().get_registry(&qh, ());

    let mut app = App::new();

    // The first roundtrip collects globals; the second collects the events that
    // arrive as a result of binding them, including the enumerate phase.
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;

    if app.greeter.is_none() {
        return Err("the compositor does not offer wdm_greeter_v1; this greeter needs wdm".into());
    }
    if app.layer_shell.is_none() {
        return Err("the compositor does not offer zwlr_layer_shell_v1".into());
    }
    // Unreachable against wdm, which always advertises both — but this greeter's
    // point is that it runs anywhere, and without them `ensure_surface` and
    // `draw` return from a let-else on every pass: a process spinning forever
    // with nothing on screen, which is the failure the checks above exist to
    // avoid.
    if app.compositor.is_none() {
        return Err("the compositor does not offer wl_compositor".into());
    }
    if app.shm.is_none() {
        return Err(
            "the compositor does not offer wl_shm; this greeter renders into shared memory".into(),
        );
    }

    while !app.exit {
        app.ensure_surface(&qh);

        if app.needs_redraw {
            app.draw(&qh);
        }

        queue.blocking_dispatch(&mut app)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wdm_greeter_v1::PromptStyle;
    use xkbcommon::xkb::{Keysym, keysyms};

    /// An `App` with no Wayland connection: every protocol object is `None`,
    /// so the pure selection and conversation logic runs and anything that
    /// would talk to the compositor returns early.
    fn app() -> App {
        let mut app = App::new();
        app.sessions = ["sway", "gnome", "plasma"]
            .iter()
            .map(|id| Session {
                id: (*id).to_owned(),
                name: id.to_uppercase(),
            })
            .collect();
        app.users = vec![User {
            name: "alice".to_owned(),
            display_name: String::new(),
            last_session: "gnome".to_owned(),
        }];
        app
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let mut app = app();
        app.session_index = 0;
        app.move_selection(-1);
        assert_eq!(app.session_index, 2);
        app.move_selection(1);
        assert_eq!(app.session_index, 0);
        app.move_selection(1);
        assert_eq!(app.session_index, 1);
    }

    #[test]
    fn history_wins_when_the_session_is_known() {
        let mut app = app();
        app.default_session = "plasma".to_owned();
        app.select_last_session();
        assert_eq!(app.session_index, 1);
    }

    #[test]
    fn empty_history_falls_back_to_the_machine_default() {
        // Since the protocol stopped folding the machine default into
        // `last_session`, a user with no history reports it honestly empty; the
        // fallback to `default_session` is what preserves the old preselection.
        let mut app = app();
        app.users[0].last_session = String::new();
        app.default_session = "plasma".to_owned();
        app.select_last_session();
        assert_eq!(app.session_index, 2);
    }

    #[test]
    fn no_history_and_no_default_selects_the_first() {
        let mut app = app();
        app.users[0].last_session = "not-installed".to_owned();
        app.session_index = 2;
        app.select_last_session();
        assert_eq!(app.session_index, 0);

        // An unset default behaves the same as an unknown one.
        app.default_session = "also-not-installed".to_owned();
        app.session_index = 2;
        app.select_last_session();
        assert_eq!(app.session_index, 0);
    }

    #[test]
    fn the_menu_consumes_enter_and_escape_and_nothing_printable() {
        let mut app = app();
        app.menu_open = true;
        assert!(app.menu_key(Keysym::new(keysyms::KEY_Return)));
        assert!(!app.menu_open, "Enter must close the menu");

        app.menu_open = true;
        assert!(app.menu_key(Keysym::new(keysyms::KEY_Escape)));
        assert!(!app.menu_open, "Escape must close the menu");

        // The arrows are the menu's, and consuming them is only half of it: the
        // key has to move the highlight too, or the drop-down opens onto a list
        // nothing can walk.
        app.menu_open = true;
        app.session_index = 0;
        assert!(app.menu_key(Keysym::new(keysyms::KEY_Down)));
        assert_eq!(app.session_index, 1, "Down must move the highlight");

        // A printable key is not the menu's: it falls through to the answer
        // field even while the menu is open. `j` and `k` are the ones that
        // matter — they were bound as vi motions, so an open drop-down ate them
        // out of the password with nothing on screen to say so. Not consuming
        // the key is only half the requirement; a binding that moved the
        // selection *and* fell through would type the `j` into the password and
        // change the session under the user, so the selection is pinned too.
        app.session_index = 1;
        for key in [keysyms::KEY_a, keysyms::KEY_j, keysyms::KEY_k] {
            app.menu_open = true;
            assert!(
                !app.menu_key(Keysym::new(key)),
                "the menu consumed a printable key"
            );
            assert_eq!(app.session_index, 1, "a printable key moved the selection");
        }
    }

    #[test]
    fn cycling_users_wraps_and_reselects_their_session() {
        let mut app = app();
        app.users.push(User {
            name: "bob".to_owned(),
            display_name: String::new(),
            last_session: "plasma".to_owned(),
        });

        app.select_last_session();
        assert_eq!(app.session_index, 1);

        app.cycle_user();
        assert_eq!(app.user_index, 1);
        assert_eq!(app.session_index, 2, "bob's history must be applied");

        app.cycle_user();
        assert_eq!(app.user_index, 0, "cycling past the last user wraps");
        assert_eq!(app.session_index, 1);
    }

    #[test]
    fn a_pam_explanation_suppresses_the_auto_retry() {
        // The defect this guards: with a faillock-locked account, PAM sends
        // "the account is locked, 10 minutes left" and an unconditional retry
        // would clear it from the screen while feeding the lock.
        let mut app = app();
        app.pam_prompt(
            0,
            "Account locked, 10 minutes left".to_owned(),
            PromptStyle::Info,
        );
        assert!(!app.should_auto_retry());

        assert!(
            !app.auth_failed("Authentication failure".to_owned()),
            "the retry must not be requested"
        );
        assert_eq!(
            app.info.as_deref(),
            Some("Account locked, 10 minutes left"),
            "the explanation must stay on screen"
        );
    }

    #[test]
    fn an_unexplained_failure_still_retries() {
        let mut app = app();
        app.pam_prompt(0, "Password:".to_owned(), PromptStyle::Secret);
        assert!(app.should_auto_retry(), "a question is not an explanation");
        assert!(
            app.auth_failed("Authentication failure".to_owned()),
            "a bare failure is the mistyped-password case, so it retries"
        );
        assert_eq!(app.error.as_deref(), Some("Authentication failure"));
    }

    #[test]
    fn error_style_prompts_also_block() {
        let mut app = app();
        app.pam_prompt(0, "Permission denied".to_owned(), PromptStyle::Error);
        assert!(!app.should_auto_retry());
        assert!(!app.auth_failed("Authentication failure".to_owned()));
        assert_eq!(app.error.as_deref(), Some("Permission denied"));
    }

    #[test]
    fn an_error_prompts_text_survives_the_generic_reason() {
        // pam_faillock reports the lockout as a PAM_ERROR_MSG and the stack
        // then fails with a generic reason. Overwriting `error` there would
        // replace the only text that says why, leaving "Authentication
        // failure" and no retry — worse than either alone.
        let mut app = app();
        app.pam_prompt(
            0,
            "Account locked due to 3 failed logins".to_owned(),
            PromptStyle::Error,
        );
        assert!(!app.auth_failed("Authentication failure".to_owned()));
        assert_eq!(
            app.error.as_deref(),
            Some("Account locked due to 3 failed logins")
        );
    }
}
