//! Compositor state and Wayland protocol handlers.
//!
//! wdm exposes a deliberately small compositor. Greeter toplevels are
//! **layer surfaces**: `wlr-layer-shell` already means "fullscreen overlay, per
//! output, exclusive", which is exactly a greeter, and toolkits support it.
//! `xdg_wm_base` is advertised only because popups need it for grabs, and a
//! layer surface can parent an `xdg_popup`.
//!
//! No `xdg_toplevel` is ever created: [`XdgShellHandler::new_toplevel`] closes
//! anything that tries. A greeter therefore cannot spawn a floating window, so
//! wdm needs no window management, focus policy or stacking — the largest chunk
//! of compositor work it gets to skip.

use std::sync::Arc;

use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Rectangle, Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, TraversalAction,
    with_surface_tree_downward,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::input_method::{
    InputMethodHandler, InputMethodManagerState, PopupSurface as ImePopupSurface,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface, LayerSurfaceAttributes, LayerSurfaceConfigure, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_fractional_scale,
    delegate_input_method_manager, delegate_layer_shell, delegate_output, delegate_presentation,
    delegate_seat, delegate_shm, delegate_text_input_manager, delegate_viewporter,
    delegate_xdg_shell,
};

use crate::config::Config;
use crate::login::{Action, Login};
use crate::supervise::Greeter;

/// Per-client state; the greeter is the only client wdm ever accepts.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {
        log::debug!("greeter connected");
    }

    fn disconnected(&self, _id: ClientId, reason: DisconnectReason) {
        log::debug!("greeter disconnected: {reason:?}");
    }
}

/// A layer surface wdm has accepted, and the output it belongs to.
pub struct MappedLayer {
    pub surface: LayerSurface,
    /// The output this surface was assigned. `None` until an output exists, so a
    /// greeter that binds before hotplug settles is not dropped.
    pub output: Option<Output>,
}

/// The compositor.
pub struct Wdm {
    pub display: DisplayHandle,
    pub config: Config,

    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub data_device_state: DataDeviceState,

    // Held only so the globals they created stay advertised for the greeter's
    // lifetime; nothing reads them back. Dropping any of them would withdraw the
    // corresponding protocol.
    #[allow(dead_code)]
    output_manager_state: OutputManagerState,
    #[allow(dead_code)]
    viewporter_state: ViewporterState,
    #[allow(dead_code)]
    fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    presentation_state: PresentationState,
    #[allow(dead_code)]
    text_input_state: TextInputManagerState,
    #[allow(dead_code)]
    input_method_state: InputMethodManagerState,

    pub seat: Seat<Self>,
    pub keyboard: KeyboardHandle<Self>,

    /// Connected outputs in rank order; index 0 is the primary.
    pub outputs: Vec<Output>,
    pub layers: Vec<MappedLayer>,

    /// Users, sessions, the PAM conversation, and what the greeter may do next.
    pub login: Login,

    /// The greeter process and its restart policy.
    pub greeter: Greeter,

    /// The running user session, once one has been launched.
    pub session: Option<std::process::Child>,

    /// Set by a greeter request that the event loop must act on once dispatch
    /// returns. Requests are handled inside Wayland dispatch, which is not a
    /// place to tear down the display and fork.
    pub pending_action: Action,

    /// Requests queued by event sources for the backend to drain.
    pub requests: Vec<crate::backend::Request>,

    /// When wdm started, for frame callback timestamps.
    started: std::time::Instant,

    /// Set once wdm has stopped trying to run a greeter. The backend draws this
    /// on screen instead of a login prompt, because a black display with no
    /// explanation gives the user nothing to act on.
    pub give_up_reason: Option<String>,

    /// Set when the loop should stop: the session is launching, or wdm is
    /// shutting down.
    pub running: bool,
}

impl Wdm {
    /// Build the compositor and advertise its globals.
    pub fn new(
        display: &DisplayHandle,
        config: Config,
        login: Login,
        greeter: Greeter,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let compositor_state = CompositorState::new::<Self>(display);
        let shm_state = ShmState::new::<Self>(display, Vec::new());
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(display, "seat0");

        let keyboard_config = &config.keyboard;
        let xkb = XkbConfig {
            rules: &keyboard_config.rules,
            model: &keyboard_config.model,
            layout: &keyboard_config.layout,
            variant: &keyboard_config.variant,
            options: keyboard_config.options.clone(),
        };

        // 200/25 are the kernel's own defaults for VT repeat, so a user's muscle
        // memory from a text console carries over.
        let keyboard = match seat.add_keyboard(xkb, 200, 25) {
            Ok(keyboard) => keyboard,
            Err(e) => {
                // A bad layout in the config must not leave the machine with no
                // keyboard at all, because then nobody can type a password.
                log::error!(
                    "keyboard layout {:?} rejected ({e}), falling back to the default",
                    keyboard_config.layout
                );
                seat.add_keyboard(XkbConfig::default(), 200, 25)?
            }
        };

        // Pointer and touch are added unconditionally: a greeter with an
        // on-screen keyboard needs touch, and a user with a mouse expects a
        // cursor. Neither costs anything when no such device exists.
        seat.add_pointer();
        seat.add_touch();

        Ok(Self {
            display: display.clone(),
            compositor_state,
            shm_state,
            xdg_shell_state: XdgShellState::new::<Self>(display),
            layer_shell_state: WlrLayerShellState::new::<Self>(display),
            output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(display),
            data_device_state: DataDeviceState::new::<Self>(display),
            viewporter_state: ViewporterState::new::<Self>(display),
            fractional_scale_state: FractionalScaleManagerState::new::<Self>(display),
            presentation_state: PresentationState::new::<Self>(
                display,
                libc::CLOCK_MONOTONIC as u32,
            ),
            text_input_state: TextInputManagerState::new::<Self>(display),
            input_method_state: InputMethodManagerState::new::<Self, _>(display, |_| true),
            seat_state,
            seat,
            keyboard,
            config,
            outputs: Vec::new(),
            layers: Vec::new(),
            login,
            greeter,
            session: None,
            pending_action: Action::None,
            requests: Vec::new(),
            started: std::time::Instant::now(),
            give_up_reason: None,
            running: true,
        })
    }

    /// How long wdm has been running, used for frame callback timestamps.
    ///
    /// Frame callbacks want a monotonic millisecond clock, and clients only care
    /// that it advances.
    pub fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Queue a request for the backend.
    pub fn request(&mut self, request: crate::backend::Request) {
        self.requests.push(request);
    }

    /// The primary output, if anything is connected.
    pub fn primary_output(&self) -> Option<&Output> {
        self.outputs.first()
    }

    /// Recompute output ranks and tell the greeter.
    ///
    /// Called on startup and on every hotplug. `connected` is in whatever order
    /// the backend discovered devices; [`Config::rank_outputs`] imposes the
    /// configured order so the primary output does not move between boots.
    pub fn set_outputs(&mut self, connected: Vec<Output>) {
        let names: Vec<String> = connected.iter().map(|o| o.name()).collect();
        let ranked = self
            .config
            .rank_outputs(names.iter().map(String::as_str));

        self.outputs = ranked
            .iter()
            .filter_map(|name| connected.iter().find(|o| o.name() == *name).cloned())
            .collect();

        // Layer surfaces on an output that has gone away are reassigned rather
        // than closed: unplugging a monitor mid-login must move the login form,
        // not destroy the greeter.
        let primary = self.outputs.first().cloned();
        for layer in &mut self.layers {
            let still_present = layer
                .output
                .as_ref()
                .is_some_and(|o| self.outputs.contains(o));
            if !still_present {
                layer.output = primary.clone();
            }
        }

        self.login.set_output_ranks(&self.outputs);
        self.configure_layers();
    }

    /// Size every layer surface to its output.
    ///
    /// A greeter is always fullscreen, so wdm ignores the surface's requested
    /// size and hands it the output's dimensions. Anything smaller would leave
    /// the rest of the screen showing whatever the framebuffer happened to
    /// contain.
    pub fn configure_layers(&mut self) {
        for layer in &self.layers {
            let Some(size) = layer.output.as_ref().and_then(logical_size) else {
                continue;
            };
            layer.surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            layer.surface.send_pending_configure();
        }
    }

    /// Give keyboard focus to the surface that should have it.
    ///
    /// The greeter's own surface on the primary output wins. There is no window
    /// management to speak of, so this is the entirety of wdm's focus policy.
    pub fn update_focus(&mut self) {
        let primary = self.outputs.first().cloned();
        let target = self
            .layers
            .iter()
            .find(|l| primary.is_some() && l.output == primary)
            // With no primary output, focus whatever surface exists so a greeter
            // on a hotplugged-away display can still be typed into.
            .or_else(|| self.layers.last())
            .map(|l| l.surface.wl_surface().clone());

        // set_focus takes &mut self, so the handle is cloned out first. It is an
        // Arc internally, so this is not a real copy.
        let keyboard = self.keyboard.clone();
        if keyboard.current_focus() != target {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }

    /// Render elements for one output, back to front.
    pub fn elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
        let scale = output.current_scale().fractional_scale();

        self.layers
            .iter()
            .filter(|l| l.output.as_ref() == Some(output))
            .flat_map(|l| {
                render_elements_from_surface_tree(
                    renderer,
                    l.surface.wl_surface(),
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
            })
            .collect()
    }

    /// Release frame callbacks so the greeter draws its next frame.
    ///
    /// Without this a greeter that waits on frame callbacks — which is every
    /// toolkit — renders once and then freezes.
    pub fn send_frames(&self, time_ms: u32) {
        for layer in &self.layers {
            send_frames_surface_tree(layer.surface.wl_surface(), time_ms);
        }
    }

}

fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

impl CompositorHandler for Wdm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("every client wdm accepts is created with ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // A layer surface may not commit content until it has been configured,
        // and the client's initial state is not known until its first commit —
        // so the initial configure is sent from here, not from
        // new_layer_surface.
        //
        // Deliberately not `ensure_configured()`: that posts a protocol error on
        // *any* pre-configure commit, including the empty one every client makes
        // to trigger the first configure, which would kill the greeter before it
        // ever drew.
        let Some(layer) = self
            .layers
            .iter()
            .find(|l| l.surface.wl_surface() == surface)
        else {
            return;
        };

        let already_sent = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<std::sync::Mutex<LayerSurfaceAttributes>>()
                .is_some_and(|attributes| {
                    attributes
                        .lock()
                        .expect("layer surface attributes are not poisoned")
                        .initial_configure_sent
                })
        });

        if already_sent {
            return;
        }

        let surface = layer.surface.clone();
        if let Some(size) = layer.output.as_ref().and_then(logical_size) {
            surface.with_pending_state(|state| state.size = Some(size.into()));
        }
        surface.send_configure();
    }
}

/// An output's size in logical pixels, which is what a client is configured with.
fn logical_size(output: &Output) -> Option<(i32, i32)> {
    let mode = output.current_mode()?;
    let scale = output.current_scale().fractional_scale();
    Some((
        (f64::from(mode.size.w) / scale).round() as i32,
        (f64::from(mode.size.h) / scale).round() as i32,
    ))
}

impl BufferHandler for Wdm {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Wdm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Wdm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for Wdm {
    type SelectionUserData = ();
}

impl DataDeviceHandler for Wdm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for Wdm {}

impl ServerDndGrabHandler for Wdm {
    fn send(&mut self, _mime: String, _fd: std::os::unix::io::OwnedFd, _seat: Seat<Self>) {}
}

impl WlrLayerShellHandler for Wdm {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // The greeter may name an output; otherwise it lands on the primary.
        // Honouring the request matters because a greeter that draws a
        // background on every output creates one surface per output.
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .filter(|o| self.outputs.contains(o))
            .or_else(|| self.primary_output().cloned());

        log::debug!("new layer surface {namespace:?} on {:?}", output.as_ref().map(Output::name));

        self.layers.push(MappedLayer { surface, output });
        self.configure_layers();
        self.update_focus();
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        self.layers.retain(|l| l.surface != surface);
        self.update_focus();
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: LayerSurfaceConfigure) {}
}

impl XdgShellHandler for Wdm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // xdg_wm_base exists for popup grabs only. A greeter has no business
        // opening a window, and honouring one would drag in window management,
        // stacking and focus policy that wdm deliberately does not have.
        log::warn!("greeter tried to create an xdg_toplevel; closing it");
        surface.send_close();
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // Popups are children of a layer surface and are rendered as part of its
        // surface tree, so there is nothing to track here.
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl OutputHandler for Wdm {}

impl FractionalScaleHandler for Wdm {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Tell the client the scale of the output its surface will appear on, so
        // a greeter on a HiDPI panel renders sharp instead of upscaled.
        let scale = self
            .primary_output()
            .map(|o| o.current_scale().fractional_scale())
            .unwrap_or(1.0);

        smithay::wayland::compositor::with_states(&surface, |states| {
            smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl InputMethodHandler for Wdm {
    // An input method popup (a candidate window for an on-screen keyboard) is a
    // child of the greeter's surface and is rendered as part of its tree, so
    // there is no separate mapping to track.
    fn new_popup(&mut self, _surface: ImePopupSurface) {}

    fn dismiss_popup(&mut self, _surface: ImePopupSurface) {}

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {}

    fn parent_geometry(&self, _parent: &WlSurface) -> Rectangle<i32, Logical> {
        // The greeter is always fullscreen on its output, so the parent's
        // geometry is the output's. Returning a zero rectangle would pin every
        // popup to the top-left corner.
        self.primary_output()
            .and_then(logical_size)
            .map(|size| Rectangle::from_size(size.into()))
            .unwrap_or_default()
    }
}

delegate_compositor!(Wdm);
delegate_shm!(Wdm);
delegate_seat!(Wdm);
delegate_data_device!(Wdm);
delegate_output!(Wdm);
delegate_layer_shell!(Wdm);
delegate_xdg_shell!(Wdm);
delegate_viewporter!(Wdm);
delegate_fractional_scale!(Wdm);
delegate_presentation!(Wdm);
delegate_text_input_manager!(Wdm);
delegate_input_method_manager!(Wdm);

/// Client data for the greeter connection.
pub fn client_state() -> Arc<ClientState> {
    Arc::new(ClientState::default())
}

/// The event loop's data.
///
/// `dispatch_clients` needs `&mut Display` and `&mut Wdm` at the same time, so
/// the two cannot both live inside the compositor state. Every event source
/// therefore receives this pair.
pub struct LoopData {
    pub state: Wdm,
    pub display: Display<Wdm>,
}

impl LoopData {
    /// Dispatch pending client requests and flush replies.
    pub fn dispatch(&mut self) -> std::io::Result<()> {
        self.display.dispatch_clients(&mut self.state)?;
        self.display.flush_clients()
    }
}
