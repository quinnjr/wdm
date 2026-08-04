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

use smithay::backend::allocator::Format as DrmFormat;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_seat, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, TraversalAction,
    with_surface_tree_downward,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
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
    PopupSurface, PositionerState, ToplevelSurface, XdgPopupSurfaceData, XdgShellHandler,
    XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::{
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_fractional_scale,
    delegate_input_method_manager, delegate_layer_shell, delegate_output, delegate_presentation,
    delegate_seat, delegate_shm, delegate_text_input_manager, delegate_viewporter,
    delegate_xdg_shell,
};

use smithay::desktop::{
    PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy,
    get_popup_toplevel_coords,
};

use crate::config::Config;
use crate::login::{Action, Login};
use crate::render::WdmElement;
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

/// How many layer surfaces the greeter may have at once.
///
/// The greeter is untrusted, and nothing else in the protocol bounds this: each
/// surface costs a full `render_elements_from_surface_tree` per output per
/// frame, plus a `with_states` in `send_preferred_scales` and `send_frames`, so
/// a greeter that creates them in a loop degrades the login screen for as long
/// as it is up. The legitimate shape is one surface per output — a background on
/// each, a login form on the primary — so the cap is generous against eight
/// monitors and still small enough to bound the per-frame work.
///
/// Counted against *created* surfaces, not mapped ones: smithay calls
/// [`WlrLayerShellHandler::new_layer_surface`] from the `get_layer_surface`
/// request, before any commit, so `layers` holds every surface the greeter has
/// asked for whether it has ever attached a buffer or not. A greeter that
/// creates sixteen and commits none is refused the seventeenth, which is a
/// coarser rule than the per-frame cost alone would need — but the cost is not
/// the only one, `configure_layers` and `send_preferred_scales` walk the list
/// too, and the consequence of being coarse here falls entirely on the greeter,
/// which is the untrusted party.
///
/// The slot comes straight back on destroy, and on disconnect: `layer_destroyed`
/// retains, and smithay fires it from the resource's destructor, so a greeter
/// that opens and closes a surface per redraw is doing something odd but never
/// runs out.
const MAX_LAYER_SURFACES: usize = 16;

/// Whether a greeter already holding `held` layer surfaces may create another.
///
/// A free function rather than the comparison written inline, because that is
/// the only way the boundary is reachable from a test: a [`MappedLayer`] needs a
/// [`LayerSurface`], and smithay builds one only from a bound resource on a
/// connected client, so nothing short of a second process on the other end of
/// the socket can drive `new_layer_surface` far enough to hit the cap.
const fn layer_slot_free(held: usize) -> bool {
    held < MAX_LAYER_SURFACES
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

    /// Created once the backend has a renderer, since its advertised formats
    /// come from that renderer. `None` until then.
    pub dmabuf_state: Option<DmabufState>,
    dmabuf_global: Option<DmabufGlobal>,
    /// What the live global advertises, so a renderer change can be detected.
    dmabuf_formats: Vec<DrmFormat>,

    /// What the pointer should look like right now.
    pub cursor: CursorImageStatus,

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
    /// Popups parented to a layer surface. `xdg_wm_base` exists for these.
    pub popups: PopupManager,

    /// Users, sessions, the PAM conversation, and what the greeter may do next.
    pub login: Login,

    /// The greeter process and its restart policy.
    pub greeter: Greeter,

    /// A scheduled greeter respawn, kept so it can be cancelled. A timer left
    /// armed across a login fires in the next generation and starts a second
    /// greeter beside the first.
    pub respawn_token: Option<smithay::reexports::calloop::RegistrationToken>,

    /// Actions queued by greeter requests and auth events for the event loop to
    /// act on once dispatch returns. Requests are handled inside Wayland
    /// dispatch, which is not a place to tear down the display and fork.
    ///
    /// A queue rather than one slot: several requests and auth events can be
    /// handled in a single dispatch batch, and overwriting would silently drop a
    /// Launch or a RestartGreeter, leaving the login screen hung with no event.
    pub pending_actions: std::collections::VecDeque<Action>,

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
            dmabuf_state: None,
            dmabuf_global: None,
            dmabuf_formats: Vec::new(),
            cursor: CursorImageStatus::default_named(),
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
            popups: PopupManager::default(),
            login,
            greeter,
            respawn_token: None,
            pending_actions: std::collections::VecDeque::new(),
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

    /// Queue an action for the event loop.
    pub fn queue_action(&mut self, action: Action) {
        if !matches!(action, Action::None) {
            self.pending_actions.push_back(action);
        }
    }

    /// Advertise `zwp_linux_dmabuf_v1` with the renderer's formats.
    ///
    /// Called by the backend whenever a renderer exists. Without this a greeter
    /// built on GTK4, Qt or wgpu cannot attach a GPU buffer and must fall back
    /// to `wl_shm` or fail to start — which would make "any toolkit" untrue for
    /// exactly the third-party greeters wdm exists to host.
    ///
    /// Re-advertised when the formats change, which happens when the GPU is
    /// hot-removed and a different one is opened. Keeping the first GPU's
    /// formats would promise a client buffers the current renderer cannot
    /// import, and it would find out by never appearing on screen.
    pub fn init_dmabuf(&mut self, display: &DisplayHandle, formats: Vec<DrmFormat>) {
        if self.dmabuf_formats == formats {
            return;
        }

        // The old global names formats that are no longer true; withdraw it so
        // clients renegotiate rather than binding a stale one.
        if let (Some(state), Some(global)) = (&mut self.dmabuf_state, self.dmabuf_global.take()) {
            state.destroy_global::<Self>(display, global);
        }

        let state = self.dmabuf_state.get_or_insert_with(DmabufState::new);
        self.dmabuf_global = Some(state.create_global::<Self>(display, formats.clone()));
        self.dmabuf_formats = formats;
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
        // `Output::name()` allocates, so it is called once per output here and
        // the ranking works on the borrowed names.
        let names: Vec<String> = connected.iter().map(|o| o.name()).collect();
        let ranked = self.config.rank_outputs(names.iter().map(String::as_str));
        self.outputs = order_by_rank(&ranked, &connected);

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
        // After the reassignment above, so a surface moved to a differently
        // scaled monitor is told before it draws its next frame.
        self.send_preferred_scales();
        // The primary output may have changed, which changes which surface
        // should hold the keyboard. Without this the caret and the input target
        // can end up on different screens after a hotplug.
        self.update_focus();
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

    /// Forget every surface the greeter left behind.
    ///
    /// One place rather than four, because there are four ways a greeter stops
    /// existing — the handoff, a restart, giving up, and the nested backend's
    /// launch — and each of them used to clear a different subset. `layers` was
    /// cleared everywhere, `update_focus` on two paths of the four, and `cursor`
    /// on none at all.
    ///
    /// The cursor is the one with teeth. A GTK or Qt greeter sets
    /// `CursorImageStatus::Surface(...)`; after the handoff that client is gone,
    /// but the status outlives it, and the next generation's first
    /// `cursor_elements` hands the dead `WlSurface` to `with_states` and to
    /// `render_elements_from_surface_tree`, which resolves it through
    /// `surface.data::<SurfaceUserData>().unwrap()`. At best a dead tree
    /// renders; at worst that unwrap panics in the root process one frame into
    /// the new login screen.
    ///
    /// Focus has to go with them: a keyboard focused on a destroyed layer
    /// surface carries into the next generation, so the new greeter's first
    /// keystrokes are delivered to a surface that no longer exists.
    pub fn forget_greeter(&mut self) {
        // Surfaces belonging to the dead greeter must go, or they keep being
        // rendered over the new one.
        self.layers.clear();
        // Back to the built-in arrow. `Hidden` would be wrong: a pointer that
        // never reappears is indistinguishable from a wedged compositor.
        self.cursor = CursorImageStatus::default_named();
        self.update_focus();
    }

    /// Render elements for one output, front to back.
    ///
    /// smithay treats index 0 as topmost, so popups are emitted before their
    /// parent layer surface in order to land on top of it. An `xdg_popup` is its
    /// own surface, not a subsurface, so walking the parent's tree does not
    /// reach one.
    pub fn elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<WdmElement<GlesRenderer>> {
        let scale = output.current_scale().fractional_scale();

        // The cursor first, because smithay treats index 0 as topmost and the
        // pointer belongs on top of everything. Built here rather than spliced
        // in at the front afterwards: a splice at 0 shifts the whole vector, and
        // this runs once per output per frame.
        let mut elements: Vec<WdmElement<GlesRenderer>> = self.cursor_elements(renderer, output);

        for layer in self
            .layers
            .iter()
            .filter(|l| l.output.as_ref() == Some(output))
        {
            let parent = layer.surface.wl_surface();

            for (popup, offset) in PopupManager::popups_for_surface(parent) {
                elements.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        popup.wl_surface(),
                        // The popup manager resolves offsets in logical
                        // coordinates; this API takes physical. They only agree
                        // at scale 1, which is why the nested backend never
                        // showed this.
                        to_physical(offset, scale),
                        scale,
                        1.0,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .map(WdmElement::Surface),
                );
            }

            elements.extend(
                render_elements_from_surface_tree(
                    renderer,
                    parent,
                    (0, 0),
                    scale,
                    1.0,
                    Kind::Unspecified,
                )
                .into_iter()
                .map(WdmElement::Surface),
            );
        }

        elements
    }

    /// Render elements for the pointer, if it should be visible.
    ///
    /// A client-set cursor surface is drawn as-is. `Named` and `Default` fall
    /// back to a small built-in arrow rather than nothing, because a compositor
    /// that offers a pointer and never draws one is worse than one that offers
    /// no pointer at all.
    fn cursor_elements(
        &self,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Vec<WdmElement<GlesRenderer>> {
        let Some(pointer) = self.seat.get_pointer() else {
            return Vec::new();
        };
        // Only on the output the pointer is actually over. With one fullscreen
        // surface per output, that is the primary.
        if self.outputs.first() != Some(output) {
            return Vec::new();
        }

        let scale = output.current_scale().fractional_scale();
        // current_location and clamp_to_output both work in logical pixels; the
        // element APIs below take physical ones.
        let location = pointer.current_location().to_physical(scale);

        match &self.cursor {
            CursorImageStatus::Hidden => Vec::new(),

            CursorImageStatus::Surface(surface) => {
                // The hotspot is where the client says the click point is.
                let hotspot = smithay::wayland::compositor::with_states(surface, |states| {
                    states
                        .data_map
                        .get::<std::sync::Mutex<smithay::input::pointer::CursorImageAttributes>>()
                        .map(|a| a.lock().expect("not poisoned").hotspot)
                        .unwrap_or_default()
                });

                // The hotspot is logical too, so it is scaled with the
                // position rather than subtracted from a physical one.
                let hotspot = hotspot.to_f64().to_physical(scale);
                render_elements_from_surface_tree(
                    renderer,
                    surface,
                    (
                        (location.x - hotspot.x).round() as i32,
                        (location.y - hotspot.y).round() as i32,
                    ),
                    scale,
                    1.0,
                    Kind::Cursor,
                )
                .into_iter()
                .map(WdmElement::Surface)
                .collect()
            }

            CursorImageStatus::Named(_) => {
                let buffer = crate::render::pointer_buffer();
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    (location.x, location.y),
                    buffer,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => vec![WdmElement::Image(element)],
                    Err(e) => {
                        log::debug!("drawing the pointer: {e}");
                        Vec::new()
                    }
                }
            }
        }
    }

    /// Release frame callbacks so the greeter draws its next frame.
    ///
    /// Without this a greeter that waits on frame callbacks — which is every
    /// toolkit — renders once and then freezes.
    pub fn send_frames(&self, time_ms: u32) {
        for layer in &self.layers {
            let parent = layer.surface.wl_surface();

            // Popups are walked separately, mirroring `elements()`: an
            // `xdg_popup` is its own surface, not a subsurface, so the parent's
            // tree does not reach it. Without this an open GTK drop-down gets
            // exactly one frame callback and then never another — hover
            // highlights stop following the mouse and the list will not scroll,
            // while the rest of the greeter keeps animating.
            for (popup, _) in PopupManager::popups_for_surface(parent) {
                send_frames_surface_tree(popup.wl_surface(), time_ms);
            }

            send_frames_surface_tree(parent, time_ms);
        }
    }

    /// The output a layer surface was assigned, looked up by its `wl_surface`.
    ///
    /// Popups and input-method surfaces are parented to a layer surface, so this
    /// is how anything hanging off the greeter finds the screen it is on. With
    /// per-connector `scale` in the config, assuming the primary output here is
    /// wrong for every surface on a secondary one.
    fn output_of(&self, surface: &WlSurface) -> Option<&Output> {
        self.layers
            .iter()
            .find(|l| l.surface.wl_surface() == surface)
            .and_then(|l| l.output.as_ref())
    }

    /// Tell every layer surface the scale of the output it is actually on.
    ///
    /// [`FractionalScaleHandler::new_fractional_scale`] can only guess, because
    /// the surface has no output when the fractional-scale object is created.
    /// This is the correction, and it has to run again on hotplug: a surface
    /// reassigned from a 1x monitor to a 2x one that is never told renders at
    /// half resolution and is upscaled for the rest of the session.
    fn send_preferred_scales(&self) {
        for layer in &self.layers {
            let Some(output) = layer.output.as_ref() else {
                continue;
            };
            let scale = output.current_scale().fractional_scale();
            smithay::wayland::compositor::with_states(layer.surface.wl_surface(), |states| {
                smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional| {
                    fractional.set_preferred_scale(scale);
                });
            });
        }
    }
}

/// Reorder `connected` to follow `ranked`, dropping anything unranked.
///
/// A ranked name with no connected output is skipped rather than being an
/// error: ranks are computed from the names of the very outputs passed in, but
/// pulling this apart from `set_outputs` makes it testable, and a mismatch must
/// not panic on the hotplug path.
///
/// Names are matched with a linear scan, first match wins. Two outputs
/// reporting the same name should not happen, but a driver quirk or a device
/// re-added before the old `Output` drops can produce it — and then the *older*
/// one keeps the name, so layer surfaces already assigned to it stay put.
fn order_by_rank(ranked: &[&str], connected: &[Output]) -> Vec<Output> {
    ranked
        .iter()
        .filter_map(|name| connected.iter().find(|o| o.name() == *name).cloned())
        .collect()
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

        self.popups.commit(surface);

        // A popup may not attach a buffer until it has been configured, and
        // nothing else sends that first configure — PopupManager only tracks
        // geometry. Without this a GTK drop-down opens a popup that never maps,
        // so clicking it appears to do nothing at all.
        if let Some(popup) = self.popups.find_popup(surface) {
            match popup {
                PopupKind::Xdg(ref xdg) => {
                    let sent = smithay::wayland::compositor::with_states(surface, |states| {
                        states
                            .data_map
                            .get::<XdgPopupSurfaceData>()
                            .is_some_and(|data| {
                                data.lock()
                                    .expect("popup state is not poisoned")
                                    .initial_configure_sent
                            })
                    });

                    if !sent {
                        self.unconstrain_popup(xdg);
                        if let Err(e) = xdg.send_configure() {
                            log::warn!("configuring popup: {e}");
                        }
                    }
                }
                // Input method popups are configured by their own protocol.
                PopupKind::InputMethod(_) => {}
            }
        }

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

/// Convert a logical offset to the physical one the render APIs take.
///
/// smithay's `render_elements_from_surface_tree` and
/// `MemoryRenderBufferRenderElement::from_buffer` both declare their location
/// parameter as `Physical`, while `PopupManager` and pointer positions are
/// `Logical`. The two coincide at scale 1, so getting this wrong is invisible
/// on an unscaled display and misplaces every popup and the cursor on a scaled
/// one.
fn to_physical(offset: Point<i32, Logical>, scale: f64) -> Point<i32, Physical> {
    offset.to_f64().to_physical(scale).to_i32_round()
}

/// The rectangle a popup is unconstrained against, in its *parent surface's*
/// coordinate space.
///
/// smithay documents the target as relative to the parent's geometry. For a
/// popup hanging off the fullscreen layer surface that is the output at the
/// origin, but for a nested popup — a submenu, or a combo inside a menu — the
/// parent sits at some offset inside the layer surface, so the output rectangle
/// has to be shifted back by it. Passing the unshifted output means a submenu
/// near the bottom of the screen is told it fits when the part below the fold
/// does not, and it opens off-screen where nothing can click it.
fn unconstrain_rect(output: (i32, i32), offset: Point<i32, Logical>) -> Rectangle<i32, Logical> {
    Rectangle::new((-offset.x, -offset.y).into(), output.into())
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

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // Recorded so the render path can draw it. Ignoring this leaves the
        // pointer invisible, which makes clamp_to_output's job pointless and a
        // mouse unusable on the login screen.
        self.cursor = image;
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

        log::debug!(
            "new layer surface {namespace:?} on {:?}",
            output.as_ref().map(Output::name)
        );

        // Closed rather than ignored, and closed rather than a protocol error:
        // `closed` is what the protocol gives a compositor to withdraw a
        // surface, so a greeter that is merely buggy can notice and recover,
        // while one that is not gets no more of wdm's per-frame budget either
        // way. Killing the client would restart it into the same loop.
        if !layer_slot_free(self.layers.len()) {
            log::warn!(
                "greeter asked for more than {MAX_LAYER_SURFACES} layer surfaces; closing {namespace:?}"
            );
            surface.send_close();
            return;
        }

        self.layers.push(MappedLayer { surface, output });
        self.configure_layers();
        // The output is only known now, so this is the first point at which the
        // scale sent from new_fractional_scale — the primary's, as a guess — can
        // be corrected for a surface that asked for a secondary output.
        self.send_preferred_scales();
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

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // The initial configure is sent from commit(), once the client has
        // supplied the state that decides its geometry.
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            log::warn!("tracking popup: {e}");
        }
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Self>::from_resource(&seat) else {
            return;
        };
        let popup = PopupKind::Xdg(surface);

        // Without a grab a popup can never take the keyboard and never be
        // dismissed by clicking away from it, which is the whole point of one.
        if let Ok(mut grab) = self.popups.grab_popup(
            self.keyboard
                .current_focus()
                .unwrap_or_else(|| popup.wl_surface().clone()),
            popup,
            &seat,
            serial,
        ) {
            if let Some(keyboard) = seat.get_keyboard()
                && keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || grab.previous_serial().is_some_and(|s| keyboard.has_grab(s)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }

            if let Some(keyboard) = seat.get_keyboard() {
                keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            }
            if let Some(pointer) = seat.get_pointer() {
                pointer.set_grab(
                    self,
                    PopupPointerGrab::new(&grab),
                    serial,
                    smithay::input::pointer::Focus::Keep,
                );
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        // Silently ignoring this hangs a client that is waiting for the
        // `repositioned` event before it draws again.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
}

impl Wdm {
    /// Keep a popup inside its output.
    ///
    /// A popup positioned past the edge of the screen is invisible and
    /// unreachable; the positioner's own rules say what to do about it.
    fn unconstrain_popup(&self, surface: &PopupSurface) {
        let popup = PopupKind::Xdg(surface.clone());

        // The popup's own output, not the primary: with per-connector `scale`
        // and per-connector modes, a drop-down on a secondary monitor would
        // otherwise be unconstrained against a screen of the wrong size — and on
        // a smaller secondary it would be told it fits when it does not.
        let output = smithay::desktop::find_popup_root_surface(&popup)
            .ok()
            .and_then(|root| self.output_of(&root).cloned())
            .or_else(|| self.primary_output().cloned());

        let Some(size) = output.as_ref().and_then(logical_size) else {
            return;
        };

        let offset = get_popup_toplevel_coords(&popup);
        let target = unconstrain_rect(size, offset);

        surface.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Drop dead popups and finished grabs.
    ///
    /// smithay only prunes these here; `iter_popups` filters dead nodes without
    /// removing them, so without a periodic call every drop-down a greeter ever
    /// opened stays in the tree and is walked once per output per frame. It is
    /// also what releases a grab whose popup died, which otherwise keeps
    /// swallowing focus changes.
    pub fn cleanup_popups(&mut self) {
        self.popups.cleanup();
    }
}

impl DmabufHandler for Wdm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        // Cannot be reached before init_dmabuf, since no global exists to bind
        // until then — but a panic here would take the whole login screen down,
        // so the state is created rather than asserted.
        self.dmabuf_state.get_or_insert_with(DmabufState::new)
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // ponytail: accepted without a trial import, because the renderer lives
        // in the backend and is not reachable from here. The advertised formats
        // come from that same renderer, so a client using one of them will
        // import at render time; one that does not gets a failed frame rather
        // than a protocol error. Wire the renderer through if that proves too
        // loose.
        let _ = notifier.successful::<Self>();
    }
}

impl OutputHandler for Wdm {
    fn output_bound(&mut self, output: Output, wl_output: WlOutput) {
        // A client cannot be told an output's rank before it has a resource for
        // that output, so the rank is delivered here as well as from
        // set_output_ranks. This is what makes ranks reach a greeter that binds
        // wdm_greeter_v1 before wl_output, which is the normal order.
        self.login.send_rank_for(&output, &wl_output);
    }
}

impl FractionalScaleHandler for Wdm {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Tell the client the scale of the output its surface will appear on, so
        // a greeter on a HiDPI panel renders sharp instead of upscaled.
        //
        // The primary's scale is a *guess*: this fires when the client creates
        // the fractional-scale object, which is before the surface has a role
        // and therefore before it has an output. `send_preferred_scales`
        // corrects it from `new_layer_surface`, once the output is known, and
        // again from `set_outputs` on hotplug.
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
    // An on-screen keyboard's candidate window. Tracked through the same popup
    // manager as xdg popups so it is actually rendered — it is a separate
    // surface, not part of the greeter's tree.
    fn new_popup(&mut self, surface: ImePopupSurface) {
        if let Err(e) = self.popups.track_popup(PopupKind::InputMethod(surface)) {
            log::warn!("tracking input method popup: {e}");
        }
    }

    fn dismiss_popup(&mut self, surface: ImePopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::InputMethod(surface));
        }
    }

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> Rectangle<i32, Logical> {
        // The greeter is always fullscreen on its output, so the parent's
        // geometry is the output's — the one that surface is actually on, since
        // outputs may differ in mode and configured scale. Returning a zero
        // rectangle would pin every popup to the top-left corner.
        self.output_of(parent)
            .or_else(|| self.primary_output())
            .and_then(logical_size)
            .map(|size| Rectangle::from_size(size.into()))
            .unwrap_or_default()
    }
}

delegate_dmabuf!(Wdm);
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

#[cfg(test)]
mod tests {
    use super::*;

    use smithay::output::{Mode as OutputMode, PhysicalProperties, Subpixel};
    use smithay::reexports::calloop::EventLoop;
    use smithay::utils::Size;

    /// A compositor with no backend behind it.
    ///
    /// Everything `Wdm::new` needs exists without a GPU or a seat: the display
    /// is a real `wayland-server` one with no clients, the greeter is
    /// unprivileged so no account is resolved, and the `Login` is empty. The
    /// event loop is returned only because the `LoopHandle` inside `Login`
    /// borrows from it.
    #[allow(clippy::type_complexity)]
    fn test_wdm() -> (EventLoop<'static, LoopData>, Display<Wdm>, Wdm) {
        let event_loop: EventLoop<'static, LoopData> = EventLoop::try_new().unwrap();
        let display: Display<Wdm> = Display::new().unwrap();
        let handle = display.handle();

        let (events, _rx) = smithay::reexports::calloop::channel::channel();
        let login = Login::new(
            Vec::new(),
            Vec::new(),
            None,
            std::path::PathBuf::from("/nonexistent/wdm-test"),
            7,
            events,
            event_loop.handle(),
        );
        // Unprivileged, so no user is looked up and nothing is spawned.
        let greeter = crate::supervise::Greeter::new("/bin/true", "nobody", "wayland-test", false)
            .expect("greeter");

        let state = Wdm::new(&handle, Config::default(), login, greeter).expect("compositor");
        (event_loop, display, state)
    }

    #[test]
    fn forgetting_the_greeter_drops_its_cursor_and_its_focus() {
        // The defect: `cursor` was the one greeter surface reference no path
        // reset — not the handoff, not restart_greeter, not give_up. A GTK or Qt
        // greeter sets CursorImageStatus::Surface, and after the handoff that
        // client is gone; the next generation's first cursor_elements then hands
        // the dead surface to with_states and to
        // render_elements_from_surface_tree, which resolves it through an
        // `unwrap` on the surface's user data — a panic in the root process one
        // frame into the new login screen.
        //
        // Hidden stands in for the Surface case here, because building a real
        // WlSurface needs a connected client and the assertion is the same one:
        // whatever the greeter left in this field, forget_greeter must replace
        // it with something wdm owns.
        let (_loop, _display, mut state) = test_wdm();

        state.cursor = CursorImageStatus::Hidden;
        state.forget_greeter();

        assert!(
            matches!(state.cursor, CursorImageStatus::Named(_)),
            "the greeter's cursor survived it"
        );
        assert!(state.layers.is_empty());
        assert_eq!(
            state.keyboard.current_focus(),
            None,
            "keyboard focus survived the surface it was on"
        );
    }

    #[test]
    fn the_seventeenth_layer_surface_is_the_one_refused() {
        // The cap is off-by-one-shaped: `>=` against a length is the comparison
        // people get wrong, and both directions are silent. One too strict and a
        // legitimate greeter loses its sixteenth surface — a background on each
        // of eight monitors plus a form is nowhere near it, but a greeter that
        // does something more elaborate could be. One too lax and the bound is
        // simply a different number, which nothing else in the protocol
        // constrains.
        //
        // Written against `layer_slot_free` and not through
        // `new_layer_surface`, which cannot be reached without a client: see the
        // function's own note.
        assert!(layer_slot_free(0), "a greeter with nothing was refused");
        assert!(layer_slot_free(MAX_LAYER_SURFACES - 1), "the last slot");
        assert!(
            !layer_slot_free(MAX_LAYER_SURFACES),
            "a seventeenth surface was allowed"
        );
        // And a greeter that somehow got past the cap stays refused rather than
        // wrapping back under it.
        assert!(!layer_slot_free(MAX_LAYER_SURFACES + 1));
        assert!(!layer_slot_free(usize::MAX));
    }

    #[test]
    fn forgetting_the_greeter_is_idempotent() {
        // It runs on four paths and two of them can follow one another — a
        // restart that gives up, a give-up during a handoff — so a second call
        // against already-cleared state must be a no-op rather than a panic.
        let (_loop, _display, mut state) = test_wdm();

        state.forget_greeter();
        state.forget_greeter();

        assert!(matches!(state.cursor, CursorImageStatus::Named(_)));
        assert!(state.layers.is_empty());
    }

    /// An `Output` needs no GPU, so the ranking can be exercised directly.
    /// `model` carries the identity the duplicate-name test tells copies apart by.
    fn test_output(name: &str, model: &str) -> Output {
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: "wdm".to_owned(),
                model: model.to_owned(),
            },
        );
        output.change_current_state(
            Some(OutputMode {
                size: Size::from((1920, 1080)),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        output
    }

    #[test]
    fn rank_order_is_applied_to_the_outputs() {
        // Probe order reversed, as udev is free to report it.
        let connected = vec![test_output("DP-2", "b"), test_output("DP-1", "a")];
        let ordered = order_by_rank(&["DP-1", "DP-2"], &connected);
        let names: Vec<String> = ordered.iter().map(|o| o.name()).collect();
        assert_eq!(names, ["DP-1", "DP-2"]);
    }

    #[test]
    fn a_ranked_name_with_no_output_is_dropped() {
        // Must not panic and must not leave a hole: this runs on hotplug.
        let connected = vec![test_output("DP-1", "a")];
        let ordered = order_by_rank(&["HDMI-A-1", "DP-1"], &connected);
        let names: Vec<String> = ordered.iter().map(|o| o.name()).collect();
        assert_eq!(names, ["DP-1"]);
    }

    #[test]
    fn duplicate_names_resolve_to_the_first_output() {
        // The older Output keeps the name, so layer surfaces already assigned to
        // it are not reassigned by a device re-added before the old one drops.
        let connected = vec![test_output("DP-1", "old"), test_output("DP-1", "new")];
        let ordered = order_by_rank(&["DP-1"], &connected);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].physical_properties().model, "old");
    }

    #[test]
    fn logical_offsets_scale_to_physical() {
        // The defect this guards: passing a logical offset to an API that takes
        // physical. At scale 1 the two agree, which is why an unscaled test
        // display hides it entirely.
        let offset = Point::<i32, Logical>::from((100, 50));

        assert_eq!(to_physical(offset, 1.0), Point::from((100, 50)));
        assert_eq!(to_physical(offset, 2.0), Point::from((200, 100)));
        assert_eq!(to_physical(offset, 1.5), Point::from((150, 75)));
    }

    #[test]
    fn physical_offsets_grow_with_scale() {
        let offset = Point::<i32, Logical>::from((10, 10));
        let single = to_physical(offset, 1.0);
        let double = to_physical(offset, 2.0);
        assert!(double.x > single.x && double.y > single.y);
    }

    #[test]
    fn a_popup_on_the_layer_surface_is_unconstrained_against_the_whole_output() {
        // Offset zero is the drop-down hanging straight off the fullscreen
        // greeter: the target is the output at the origin.
        let target = unconstrain_rect((1920, 1080), Point::from((0, 0)));
        assert_eq!(target.loc, Point::from((0, 0)));
        assert_eq!(target.size, Size::from((1920, 1080)));
    }

    #[test]
    fn a_nested_popup_shifts_the_output_back_by_its_parent_offset() {
        // A submenu whose parent menu already sits 300px down the screen has
        // only 780 logical pixels of headroom below it. The positioner works in
        // the parent's coordinates, so the rectangle it is given must start at
        // -300 — passing (0,0) tells it the screen extends 300px further than it
        // does and the submenu opens off the bottom edge, where it cannot be
        // clicked and cannot be dismissed by clicking away from it.
        let target = unconstrain_rect((1920, 1080), Point::from((200, 300)));
        assert_eq!(target.loc, Point::from((-200, -300)));
        assert_eq!(target.size, Size::from((1920, 1080)));
        // The far edge in parent coordinates is what the positioner clamps to.
        assert_eq!(target.loc.y + target.size.h, 780);
    }

    #[test]
    fn fractional_scales_round_rather_than_truncate() {
        // 1.25 * 7 = 8.75; truncating would put a popup a pixel high and drift
        // further the further down the screen it is.
        assert_eq!(to_physical(Point::from((7, 7)), 1.25), Point::from((9, 9)));
    }
}
