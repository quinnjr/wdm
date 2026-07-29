//! wdm's reference greeter.
//!
//! An ordinary Wayland client: it binds `wlr-layer-shell` for its fullscreen
//! surface and `wdm_greeter_v1` to log the user in. It renders with `wl_shm` and
//! no toolkit, which keeps the shipped default free of a GTK or Qt dependency and
//! makes it a readable example of the protocol.
//!
//! Policy this greeter chooses, which the protocol deliberately leaves open: it
//! draws only on the rank 0 output, and preselects each user's last session.

mod text;
mod ui;

use std::collections::HashMap;
use std::os::fd::AsFd;
use std::process::ExitCode;

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
    width: i32,
    height: i32,
    configured: bool,

    users: Vec<User>,
    sessions: Vec<Session>,
    /// The rank 0 output, once wdm has said which it is.
    primary: Option<WlOutput>,
    ready: bool,

    user_index: usize,
    session_index: usize,
    prompt: Option<Prompt>,
    answer: String,
    error: Option<String>,
    info: Option<String>,
    launching: bool,
    /// True while a `create_session` is outstanding.
    authenticating: bool,

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
            width: 0,
            height: 0,
            configured: false,
            users: Vec::new(),
            sessions: Vec::new(),
            primary: None,
            ready: false,
            user_index: 0,
            session_index: 0,
            prompt: None,
            answer: String::new(),
            error: None,
            info: None,
            launching: false,
            authenticating: false,
            xkb: None,
            exit: false,
            needs_redraw: true,
        }
    }

    /// Preselect the session this user last used, falling back to the first.
    ///
    /// wdm reports `last_session`; applying it is the greeter's choice.
    fn select_last_session(&mut self) {
        let Some(user) = self.users.get(self.user_index) else {
            return;
        };
        self.session_index = self
            .sessions
            .iter()
            .position(|s| s.id == user.last_session)
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
        self.authenticating = true;
        greeter.create_session(user.name.clone());
        self.needs_redraw = true;
    }

    /// Abandon the current conversation and start a new one.
    fn restart_auth(&mut self) {
        if let Some(greeter) = &self.greeter {
            greeter.cancel();
        }
        self.authenticating = false;
        self.prompt = None;
        self.answer.clear();
        self.begin_auth();
    }

    fn cycle_user(&mut self) {
        if self.users.len() < 2 {
            return;
        }
        self.user_index = (self.user_index + 1) % self.users.len();
        self.select_last_session();
        self.error = None;
        // The conversation is per user, so switching users means starting over.
        self.restart_auth();
    }

    fn cycle_session(&mut self) {
        if self.sessions.len() < 2 {
            return;
        }
        self.session_index = (self.session_index + 1) % self.sessions.len();
        self.needs_redraw = true;
    }

    /// Answer the pending prompt.
    fn submit(&mut self) {
        let Some(greeter) = &self.greeter else {
            return;
        };
        let Some(prompt) = self.prompt.take() else {
            // Nothing is being asked. Enter restarts a finished attempt, so a
            // user who just saw an error can simply try again.
            if !self.authenticating {
                self.restart_auth();
            }
            return;
        };

        greeter.respond(prompt.id, std::mem::take(&mut self.answer));
        self.needs_redraw = true;
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

    /// Draw a frame into a fresh shm buffer and attach it.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.configured || self.width <= 0 || self.height <= 0 {
            return;
        }
        let (Some(shm), Some(surface)) = (&self.shm, &self.surface) else {
            return;
        };

        let mut canvas = Canvas::new(self.width, self.height);

        if !self.ready {
            ui::paint_message(&mut canvas, "Connecting…", false);
        } else if self.users.is_empty() {
            ui::paint_message(&mut canvas, "No users available to log in", true);
        } else if self.sessions.is_empty() {
            ui::paint_message(&mut canvas, "No sessions installed", true);
        } else {
            let user = &self.users[self.user_index];
            let session_name = self
                .sessions
                .get(self.session_index)
                .map(|s| s.name.as_str())
                .unwrap_or("none");

            ui::paint(
                &mut canvas,
                &ui::View {
                    username: &user.name,
                    display_name: &user.display_name,
                    session_name,
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

        let Some(buffer) = shm_buffer(shm, qh, &canvas) else {
            return;
        };

        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, self.width, self.height);
        surface.commit();

        // Destroyed immediately: the compositor keeps the mapping it needs, and
        // this greeter draws on keystrokes rather than at 60Hz, so there is no
        // pool worth tracking.
        buffer.destroy();

        self.needs_redraw = false;
    }

    fn key(&mut self, keysym: xkbcommon::xkb::Keysym, utf8: String) {
        use xkbcommon::xkb::keysyms;

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
            keysyms::KEY_F2 => self.cycle_session(),

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

/// Build a `wl_shm` buffer holding the canvas.
///
/// A fresh memfd per frame rather than a reused pool: the greeter draws only when
/// something changed, so the allocation is off any hot path, and it removes the
/// need to track buffer release.
fn shm_buffer(shm: &WlShm, qh: &QueueHandle<App>, canvas: &Canvas) -> Option<WlBuffer> {
    use rustix::fs::{MemfdFlags, memfd_create};
    use std::io::Write;

    let fd = match memfd_create("wdm-greeter", MemfdFlags::CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("memfd_create: {e}");
            return None;
        }
    };

    let mut file = std::fs::File::from(fd);
    if let Err(e) = file.write_all(&canvas.data) {
        log::error!("writing the frame: {e}");
        return None;
    }

    let pool = shm.create_pool(file.as_fd(), canvas.data.len() as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        canvas.width,
        canvas.height,
        canvas.width * 4,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    // The pool is not needed once the buffer exists; the mapping lives until the
    // buffer is destroyed.
    pool.destroy();

    Some(buffer)
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
                "wdm_greeter_v1" => state.greeter = Some(registry.bind(name, 1, qh, ())),
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
                state.select_last_session();
                state.begin_auth();
            }

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
                        state.answer.clear();
                    }
                    Err(e) => log::warn!("unknown prompt style: {e}"),
                }
                state.needs_redraw = true;
            }

            wdm_greeter_v1::Event::AuthOk => {
                state.authenticating = false;
                state.error = None;
                state.start_session();
            }

            wdm_greeter_v1::Event::AuthFailed { reason } => {
                state.authenticating = false;
                state.prompt = None;
                state.answer.clear();
                state.error = Some(reason);
                state.needs_redraw = true;
                // Ask again straight away so the user can retry without pressing
                // anything. wdm rate limits, so this cannot become a hot loop.
                state.begin_auth();
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
delegate_noop!(App: ignore WlBuffer);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore ZwlrLayerShellV1);

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

    while !app.exit {
        app.ensure_surface(&qh);

        if app.needs_redraw {
            app.draw(&qh);
        }

        queue.blocking_dispatch(&mut app)?;
    }

    Ok(())
}
