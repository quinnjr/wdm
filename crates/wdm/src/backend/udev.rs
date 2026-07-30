//! The real backend: DRM/KMS, libinput and libseat, with no kiosk compositor.
//!
//! wdm is the compositor. It takes DRM master itself, drives the connectors
//! directly, and hosts the greeter as an ordinary Wayland client.
//!
//! Login is a **handoff, not a nesting**. When a session launches, everything
//! holding the display — the DRM device, the renderer, libinput, and the libseat
//! session itself — is dropped, so the user's compositor can acquire real DRM
//! master on the same VT. wdm then waits for the session to exit and takes the
//! seat back. Between the release and the user's compositor coming up nothing
//! owns the display; that black moment is what every display manager does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{Device as ControlDevice, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::{Display, DisplayHandle, backend::GlobalId};
use smithay::utils::{DeviceFd, Physical, Size};

use crate::comp::{LoopData, Wdm};
use crate::render::WdmElement;
use crate::config::{self, Config};

use super::{Handled, Request, handle_action, poll_greeter};

/// Formats tried for the primary plane, in order. Both are ubiquitous; `Argb`
/// first so a greeter that draws translucency gets it.
const COLOR_FORMATS: &[Fourcc] = &[Fourcc::Argb8888, Fourcc::Xrgb8888];

/// How long to wait for the seat after a session exits before giving up.
///
/// logind needs a moment to tear the previous session down. Retrying rather than
/// failing outright matters because failing here leaves the machine with no
/// login prompt at all.
const SEAT_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

type Allocator = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;
type OutputManager = DrmOutputManager<Allocator, Exporter, (), DrmDeviceFd>;
type Compositor = DrmOutput<Allocator, Exporter, (), DrmDeviceFd>;

/// Everything that holds the display, grouped so it can be dropped as a unit.
///
/// Releasing DRM master means dropping *every* handle to the device, including
/// the clone libinput holds through the session interface. Keeping them in one
/// struct is what makes the handoff reliable rather than a game of finding the
/// last stray clone.
struct Device {
    output_manager: OutputManager,
    renderer: GlesRenderer,
    /// One entry per active CRTC.
    outputs: HashMap<crtc::Handle, Head>,
    /// The give-up screen, rasterised once and reused. It never changes, and
    /// rebuilding a full-screen image every frame on the one path that runs when
    /// everything else has failed would be gratuitous.
    error_screen: Option<(String, Size<i32, Physical>, MemoryRenderBuffer)>,
    /// Wayland output globals, dropped with the device so clients see the
    /// outputs go away.
    drm_token: RegistrationToken,
    /// udev reports device removal by device id, not by path, so the id is what
    /// wdm has to match against.
    device_id: libc::dev_t,
}

/// A CRTC driving one connector, and the Wayland output that represents it.
struct Head {
    compositor: Compositor,
    output: Output,
    /// The advertised `wl_output`. Kept because it is the only handle that can
    /// withdraw the global again: `Output` has no `Drop` that removes it, and
    /// the global's own data holds a strong `Output` clone, so dropping the head
    /// alone leaks both. Without this a greeter never sees `global_remove` for
    /// an unplugged monitor, and every login cycle adds one global per
    /// connector for the lifetime of the process.
    global: GlobalId,
    /// Tracked here rather than asked of the compositor, because deciding whether
    /// an output survived a hotplug means comparing against the connectors the
    /// kernel now reports.
    connector: connector::Handle,
}

/// The backend's own state, separate from the compositor's.
struct Udev {
    session: LibSeatSession,
    libinput: Libinput,
    input_token: RegistrationToken,
    udev_token: RegistrationToken,
    session_token: RegistrationToken,
    device: Option<Device>,
    /// False while the session is inactive (another VT is in front). Rendering
    /// and input are both suspended, because touching DRM without master fails
    /// and delivering input to a greeter nobody can see is pointless.
    active: bool,
}

pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, LoopData> = EventLoop::try_new()?;
    let mut display: Display<Wdm> = Display::new()?;
    let loop_handle = event_loop.handle();

    let (state, socket_name) = super::setup::build(&mut display, &loop_handle, config, true)?;
    let mut data = LoopData { state, display };

    // Without this the udev backend has exactly one exit — a login handoff — so
    // `systemctl stop wdm` never runs Greeter's Drop and the greeter is orphaned
    // until SIGKILL.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    loop_handle.insert_source(signals, |event, _, data| {
        log::info!("caught {:?}, shutting down", event.signal());
        data.state.running = false;
    })?;

    let vt = data.state.config.vt;

    loop {
        let mut udev = Udev::new(&loop_handle, &mut data, vt)?;

        super::setup::start(&mut data, &loop_handle, &socket_name);

        let launch = pump(&mut event_loop, &mut data, &mut udev, &loop_handle)?;

        let Some(launch) = launch else {
            // The loop ended without a launch: a signal asked wdm to stop. The
            // greeter is killed by Greeter's Drop as `data` goes out of scope.
            log::info!("shutting down");
            return Ok(());
        };

        // Everything that holds the display goes now, in this order: greeter
        // first so it stops drawing, then the device, then the seat.
        data.state.greeter.kill();
        data.state.layers.clear();
        udev.release(&loop_handle, &data.state.display.clone());
        drop(udev);

        let username = launch.username().to_owned();
        match launch.spawn() {
            Ok(mut child) => {
                log::info!("session for {username} running as pid {}", child.id());
                let started = Instant::now();
                let status = child.wait();
                let ran_for = started.elapsed();

                // Closing the PAM session is what releases the logind session;
                // without it pam_systemd leaks one per login.
                data.state.login.end_session();

                match status {
                    Ok(status) if ran_for < Duration::from_secs(2) => {
                        // A session that dies immediately is a broken Exec or a
                        // compositor that cannot start. Say so, or the user just
                        // sees the login screen reappear.
                        let reason =
                            format!("session exited immediately ({status}) after {ran_for:?}");
                        log::error!("{reason}");
                        data.state.login.set_last_error(Some(reason));
                    }
                    Ok(status) => log::info!("session for {username} exited: {status}"),
                    Err(e) => log::error!("waiting for session: {e}"),
                }
            }
            Err(e) => {
                let reason = format!("could not start session: {e}");
                log::error!("{reason}");
                data.state.login.set_last_error(Some(reason));
                data.state.login.end_session();
            }
        }

        data.state.login.reset();
        data.state.running = true;
        log::info!("taking the seat back");
    }
}

/// Run the event loop until a session should be launched or wdm should exit.
fn pump(
    event_loop: &mut EventLoop<'static, LoopData>,
    data: &mut LoopData,
    udev: &mut Udev,
    loop_handle: &LoopHandle<'static, LoopData>,
) -> Result<Option<crate::session::Launch>, Box<dyn std::error::Error>> {
    while data.state.running {
        udev.drain_requests(data, loop_handle);
        poll_greeter(data, loop_handle);
        data.state.cleanup_popups();

        if let Handled::HandOff(launch) = handle_action(data, loop_handle) {
            return Ok(Some(launch));
        }

        if udev.active {
            udev.render(data);
        }

        if let Err(e) = data.dispatch() {
            log::error!("dispatching clients: {e}");
        }

        // A VT switch or a pause arrives through the loop, so a short timeout
        // keeps wdm responsive even with no client activity.
        if event_loop
            .dispatch(Some(Duration::from_millis(16)), data)
            .is_err()
        {
            break;
        }
    }

    Ok(None)
}

impl Udev {
    /// Take the seat, open the GPU, and register every event source.
    ///
    /// Retries acquiring the seat: after a session exits, logind needs a moment
    /// to release it, and failing here would leave the machine with no login
    /// prompt.
    fn new(
        loop_handle: &LoopHandle<'static, LoopData>,
        data: &mut LoopData,
        vt: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + SEAT_RETRY_TIMEOUT;
        let (session, notifier) = loop {
            match LibSeatSession::new() {
                Ok(pair) => break pair,
                Err(e) if Instant::now() < deadline => {
                    log::debug!("seat not available yet ({e}), retrying");
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e.into()),
            }
        };

        let seat_name = session.seat();
        log::info!("acquired {seat_name}");

        let session_token = loop_handle.insert_source(notifier, |event, _, data| {
            // A VT switch away pauses the session: DRM commits would fail and
            // input belongs to whoever is in front now.
            data.state.request(Request::SessionActive(matches!(
                event,
                SessionEvent::ActivateSession
            )));
        })?;

        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat_name)
            .map_err(|()| "assigning libinput to the seat failed")?;

        let input_token = loop_handle.insert_source(
            LibinputInputBackend::new(libinput.clone()),
            |event, _, data| {
                // Device add and remove need no compositor state; everything else
                // goes to the greeter.
                if matches!(
                    event,
                    InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
                ) {
                    return;
                }
                if let Some(vt) = crate::input::handle(&mut data.state, event) {
                    data.state.request(Request::SwitchVt(vt.0));
                }
            },
        )?;

        let udev_backend = UdevBackend::new(&seat_name)?;
        let initial_devices: Vec<PathBuf> = udev_backend
            .device_list()
            .map(|(_, path)| path.to_owned())
            .collect();

        let udev_token = loop_handle.insert_source(udev_backend, |event, _, data| {
            // Queued rather than handled: all three need the backend, which this
            // closure cannot reach.
            data.state.request(match event {
                UdevEvent::Added { path, .. } => Request::DeviceAdded(path),
                UdevEvent::Changed { .. } => Request::RescanConnectors,
                UdevEvent::Removed { device_id } => Request::DeviceRemoved(device_id),
            });
        })?;

        let mut udev = Self {
            session,
            libinput,
            input_token,
            udev_token,
            session_token,
            device: None,
            active: true,
        };

        // Prefer the GPU logind marks primary; otherwise the first one udev
        // reported. A machine with a discrete and an integrated GPU otherwise
        // gets whichever udev happened to enumerate first.
        let chosen = primary_gpu(&seat_name)
            .ok()
            .flatten()
            .or_else(|| all_gpus(&seat_name).ok().and_then(|g| g.into_iter().next()))
            .or_else(|| initial_devices.into_iter().next())
            .ok_or("no GPU found on this seat")?;

        udev.open_device(&chosen, loop_handle, data)?;

        // wdm's own VT. Requested after the device is up so the first thing the
        // VT shows is the greeter, not a half-initialised screen.
        if let Err(e) = udev.session.change_vt(vt as i32) {
            // Not fatal: logind normally has us on the right VT already.
            log::warn!("switching to vt {vt}: {e}");
        }

        Ok(udev)
    }

    fn open_device(
        &mut self,
        path: &Path,
        loop_handle: &LoopHandle<'static, LoopData>,
        data: &mut LoopData,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let fd = self.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        )?;
        let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let node = DrmNode::from_file(&device_fd)?;

        // Atomic modesetting: the greeter is one fullscreen surface, so there is
        // no reason to fall back to legacy commits.
        let (drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
        let gbm = GbmDevice::new(device_fd.clone())?;

        // SAFETY: the gbm device outlives the EGL display, which is kept in the
        // renderer stored alongside it below.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
        let egl_context = EGLContext::new(&egl_display)?;
        // SAFETY: the context is current on this thread and not shared.
        let renderer = unsafe { GlesRenderer::new(egl_context)? };

        let render_formats = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .collect::<Vec<_>>();

        // Advertised now that a renderer exists to name the formats.
        data.state
            .init_dmabuf(&data.state.display.clone(), render_formats.clone());

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = Exporter::new(gbm.clone(), Some(node));

        let output_manager = OutputManager::new(
            drm,
            allocator,
            exporter,
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            render_formats,
        );

        let drm_token = loop_handle.insert_source(drm_notifier, |event, meta, data| match event {
            DrmEvent::VBlank(crtc) => {
                let _ = meta;
                data.state.request(Request::VBlank(crtc));
            }
            DrmEvent::Error(e) => log::error!("drm: {e}"),
        })?;

        self.device = Some(Device {
            output_manager,
            renderer,
            outputs: HashMap::new(),
            error_screen: None,
            drm_token,
            device_id: node.dev_id(),
        });

        log::info!("opened {} as {node}", path.display());
        self.scan_connectors(data);

        Ok(())
    }

    /// Bring up an output for every connected connector.
    ///
    /// Called on startup and whenever udev reports the device changed, which is
    /// how monitor hotplug arrives.
    fn scan_connectors(&mut self, data: &mut LoopData) {
        let Some(device) = &mut self.device else {
            return;
        };

        let Ok(resources) = device.output_manager.device().resource_handles() else {
            log::error!("reading drm resources failed");
            return;
        };

        let connected: Vec<connector::Info> = resources
            .connectors()
            .iter()
            // force_probe only where the kernel does not already know: a forced
            // probe is a blocking DDC/EDID round trip of roughly 100ms per
            // connector, and doing it for all of them stalls the event loop
            // every time a cable moves.
            .filter_map(|handle| {
                let device = device.output_manager.device();
                let known = device
                    .get_connector(*handle, false)
                    .ok()
                    .filter(|info| info.state() != connector::State::Unknown);
                known.or_else(|| device.get_connector(*handle, true).ok())
            })
            .filter(|info| info.state() == connector::State::Connected)
            .collect();

        // Outputs whose connector went away must go, or wdm keeps trying to
        // commit to a CRTC that is no longer driving anything.
        let live: Vec<connector::Handle> = connected.iter().map(connector::Info::handle).collect();
        let display = data.state.display.clone();
        device.outputs.retain(|_, head| {
            let keep = live.contains(&head.connector);
            if !keep {
                log::info!("output {} disconnected", head.output.name());
                display.remove_global::<Wdm>(head.global.clone());
            }
            keep
        });

        // Decided while the device is borrowed, acted on after it is released:
        // add_output needs the device mutably too.
        let to_add: Vec<(connector::Info, String)> = connected
            .iter()
            .filter_map(|info| {
                let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
                if device.outputs.values().any(|h| h.output.name() == name) {
                    return None;
                }
                Some((info.clone(), name))
            })
            .collect();

        for (info, name) in to_add {
            let output_config = data.state.config.output_for(&name).cloned();
            if output_config.as_ref().is_some_and(|c| !c.enable) {
                log::info!("{name} is disabled by configuration");
                continue;
            }

            match self.add_output(&info, &name, output_config.as_ref(), data) {
                Ok(()) => log::info!("{name} is up"),
                Err(e) => log::error!("bringing up {name}: {e}"),
            }
        }

        // Ranks are recomputed here, which is what makes unplugging the primary
        // output promote the next one and move the greeter's login form.
        let outputs: Vec<Output> = self
            .device
            .as_ref()
            .map(|d| d.outputs.values().map(|h| h.output.clone()).collect())
            .unwrap_or_default();

        data.state.set_outputs(outputs);
    }

    fn add_output(
        &mut self,
        info: &connector::Info,
        name: &str,
        output_config: Option<&config::Output>,
        data: &mut LoopData,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let device = self.device.as_mut().ok_or("no device")?;

        let mode = pick_mode(info, output_config.and_then(|c| c.mode))
            .ok_or("connector reports no usable mode")?;

        let crtc = free_crtc(device, info).ok_or("no free crtc for this connector")?;

        let (physical_width, physical_height) = info.size().unwrap_or((0, 0));
        let output = Output::new(
            name.to_owned(),
            PhysicalProperties {
                size: (physical_width as i32, physical_height as i32).into(),
                subpixel: Subpixel::Unknown,
                make: "Unknown".to_owned(),
                model: info.interface().as_str().to_owned(),
            },
        );
        let global = output.create_global::<Wdm>(&data.state.display);

        let wl_mode = OutputMode::from(mode);
        let scale = output_config.and_then(|c| c.scale).unwrap_or(1.0);
        let transform = output_config
            .map(|c| transform_of(c.transform))
            .unwrap_or(smithay::utils::Transform::Normal);

        output.change_current_state(
            Some(wl_mode),
            Some(transform),
            Some(Scale::Fractional(scale)),
            Some((0, 0).into()),
        );
        output.set_preferred(wl_mode);

        let elements: DrmOutputRenderElements<GlesRenderer, WdmElement<GlesRenderer>> =
            DrmOutputRenderElements::default();

        let compositor = device.output_manager.initialize_output(
            crtc,
            mode,
            &[info.handle()],
            &output,
            None,
            &mut device.renderer,
            &elements,
        )?;

        device.outputs.insert(
            crtc,
            Head {
                compositor,
                output,
                global,
                connector: info.handle(),
            },
        );

        Ok(())
    }

    /// Act on everything the event sources queued.
    fn drain_requests(&mut self, data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) {
        // udev emits Changed in bursts while a cable settles, and each rescan
        // force-probes every connector — a blocking DDC round trip per
        // connector, on the thread that also serves input and the greeter.
        // Coalescing keeps one burst to one probe.
        let mut rescan = false;

        for request in std::mem::take(&mut data.state.requests) {
            match request {
                Request::SwitchVt(vt) => {
                    log::info!("switching to vt {vt}");
                    if let Err(e) = self.session.change_vt(vt) {
                        log::error!("switching to vt {vt}: {e}");
                    }
                }

                Request::SessionActive(active) => {
                    self.active = active;
                    if active {
                        log::info!("seat activated");
                        if let Err(()) = self.libinput.resume() {
                            // Without input there is no way to type a password
                            // and no way to use the VT chord that is the
                            // documented escape hatch, so this must not be silent.
                            log::error!("resuming input after vt switch failed");
                        }

                        if let Some(device) = &mut self.device {
                            // `true` so smithay resets the device state: whatever
                            // held the VT may have left conflicting CRTC and
                            // connector routing, and with `false` every
                            // subsequent atomic commit fails and the screen
                            // stays black with only the journal to say why.
                            if let Err(e) = device.output_manager.activate(true) {
                                log::error!("reactivating the drm device: {e}");
                            }
                            // Whatever had the VT drew over the framebuffers, so
                            // damage tracking from before the switch describes a
                            // screen that no longer exists. Resetting the buffers
                            // forces the next frame to be a full redraw instead of
                            // an incremental one against someone else's pixels.
                            for head in device.outputs.values() {
                                head.compositor.reset_buffers();
                            }
                        }

                        data.state.configure_layers();
                    } else {
                        log::info!("seat paused");
                        self.libinput.suspend();
                        if let Some(device) = &mut self.device {
                            device.output_manager.pause();
                        }
                    }
                }

                Request::RescanConnectors => rescan = true,

                Request::DeviceAdded(path) => {
                    if self.device.is_none() {
                        log::info!("gpu {} appeared", path.display());
                        if let Err(e) = self.open_device(&path, loop_handle, data) {
                            log::error!("opening {}: {e}", path.display());
                        }
                    }
                }

                Request::DeviceRemoved(device_id) => {
                    if self
                        .device
                        .as_ref()
                        .is_some_and(|d| d.device_id == device_id)
                    {
                        // Nothing can be drawn until another GPU appears, but the
                        // greeter and the session list are still valid, so wdm
                        // keeps running rather than exiting.
                        log::error!("the gpu wdm was using was removed");
                        if let Some(device) = self.device.take() {
                            loop_handle.remove(device.drm_token);
                            for head in device.outputs.into_values() {
                                data.state.display.remove_global::<Wdm>(head.global);
                            }
                        }
                        data.state.set_outputs(Vec::new());
                    }
                }

                Request::VBlank(crtc) => {
                    if let Some(device) = &mut self.device
                        && let Some(head) = device.outputs.get_mut(&crtc)
                        && let Err(e) = head.compositor.frame_submitted()
                    {
                        log::error!("frame submitted: {e}");
                    }
                }
            }
        }

        if rescan {
            self.scan_connectors(data);
        }
    }

    /// Draw every output that needs it.
    fn render(&mut self, data: &mut LoopData) {
        let Some(device) = &mut self.device else {
            return;
        };

        let give_up = data.state.give_up_reason.as_deref();
        let crtcs: Vec<crtc::Handle> = device.outputs.keys().copied().collect();
        let mut drew = false;

        for crtc in crtcs {
            let Some(head) = device.outputs.get_mut(&crtc) else {
                continue;
            };
            let output = head.output.clone();

            let elements: Vec<WdmElement<GlesRenderer>> = match give_up {
                // Once wdm has given up there is no greeter to draw, so the
                // error screen is composited instead. Drawing nothing here would
                // leave a flat colour with no explanation, which is exactly the
                // failure the give-up path exists to avoid.
                Some(reason) => {
                    let size = output
                        .current_mode()
                        .map(|m| m.size)
                        .unwrap_or_else(|| (640, 480).into());

                    // Keyed on the size as well as the message: two outputs of
                    // different resolutions would otherwise share one raster and
                    // the second would show the text in a box of the wrong size.
                    if device
                        .error_screen
                        .as_ref()
                        .is_none_or(|(cached, cached_size, _)| {
                            cached != reason || *cached_size != size
                        })
                    {
                        device.error_screen = Some((
                            reason.to_owned(),
                            size,
                            crate::render::error_buffer(reason, size),
                        ));
                    }

                    let (_, _, buffer) = device.error_screen.as_ref().expect("just populated");
                    match MemoryRenderBufferRenderElement::from_buffer(
                        &mut device.renderer,
                        (0.0, 0.0),
                        buffer,
                        None,
                        None,
                        None,
                        Kind::Unspecified,
                    ) {
                        Ok(element) => vec![WdmElement::Image(element)],
                        Err(e) => {
                            log::error!("building the error screen: {e}");
                            Vec::new()
                        }
                    }
                }
                None => data.state.elements(&mut device.renderer, &output),
            };

            match head
                .compositor
                .render_frame(&mut device.renderer, &elements, CLEAR, FrameFlags::DEFAULT)
            {
                Ok(frame) => {
                    if frame.is_empty {
                        continue;
                    }
                    drew = true;
                    if let Err(e) = head.compositor.queue_frame(()) {
                        log::error!("queueing frame on {}: {e}", output.name());
                    }
                }
                Err(e) => log::error!("rendering {}: {e}", output.name()),
            }
        }

        // Only when something was actually committed: releasing frame callbacks
        // for a frame that was not drawn asks the greeter to render again
        // immediately, which on an idle login screen is a busy loop between two
        // processes.
        if drew {
            data.state
                .send_frames(data.state.uptime().as_millis() as u32);
        }
    }


    /// Give up the display so a session's compositor can take it.
    ///
    /// Removes every event source first: leaving a libinput or DRM source
    /// registered against a closed device makes calloop spin on a dead fd.
    fn release(&mut self, loop_handle: &LoopHandle<'static, LoopData>, display: &DisplayHandle) {
        if let Some(mut device) = self.device.take() {
            loop_handle.remove(device.drm_token);
            // Each login generation builds fresh Outputs; without withdrawing
            // these the globals accumulate one per connector per login.
            for (_, head) in device.outputs.drain() {
                display.remove_global::<Wdm>(head.global);
            }
            // Dropping the manager and renderer closes the DRM fd, which is what
            // actually drops master.
            drop(device);
        }

        loop_handle.remove(self.input_token);
        loop_handle.remove(self.udev_token);
        loop_handle.remove(self.session_token);

        // libinput holds a clone of the session interface, so it has to go
        // before the session can be released.
        self.libinput.suspend();

        log::info!("released the display");
    }
}

/// Background behind the greeter, used on any pixel it does not cover.
const CLEAR: smithay::backend::renderer::Color32F =
    smithay::backend::renderer::Color32F::new(0.05, 0.05, 0.07, 1.0);

/// Choose the mode to set on a connector.
///
/// A configured mode wins if the connector actually reports it; otherwise the
/// connector's preferred mode, and failing that the first it lists. Silently
/// falling back matters because a stale mode in the config must not leave a
/// monitor dark.
fn pick_mode(
    info: &connector::Info,
    wanted: Option<config::Mode>,
) -> Option<smithay::reexports::drm::control::Mode> {
    let modes = info.modes();

    if let Some(wanted) = wanted {
        let matching = modes.iter().find(|mode| {
            let (w, h) = mode.size();
            if w != wanted.width || h != wanted.height {
                return false;
            }
            match wanted.refresh_mhz {
                None => true,
                // vrefresh is whole Hz, so compare against the millihertz value
                // rounded, which is what makes 59.94 match a mode reporting 60.
                Some(mhz) => {
                    let reported = mode.vrefresh() * 1000;
                    reported.abs_diff(mhz) < 1000
                }
            }
        });

        match matching {
            Some(mode) => return Some(*mode),
            None => log::warn!(
                "{}-{} does not support mode {wanted}, using its preferred mode",
                info.interface().as_str(),
                info.interface_id()
            ),
        }
    }

    modes
        .iter()
        .find(|mode| {
            mode.mode_type()
                .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
        })
        .or_else(|| modes.first())
        .copied()
}

/// Find a CRTC this connector can drive that is not already in use.
fn free_crtc(device: &Device, info: &connector::Info) -> Option<crtc::Handle> {
    let drm = device.output_manager.device();
    let resources = drm.resource_handles().ok()?;

    for encoder_handle in info.encoders() {
        let Ok(encoder) = drm.get_encoder(*encoder_handle) else {
            continue;
        };
        for crtc in resources.filter_crtcs(encoder.possible_crtcs()) {
            if !device.outputs.contains_key(&crtc) {
                return Some(crtc);
            }
        }
    }

    None
}

fn transform_of(transform: config::Transform) -> smithay::utils::Transform {
    use smithay::utils::Transform as T;
    match transform {
        config::Transform::Normal => T::Normal,
        config::Transform::Rotate90 => T::_90,
        config::Transform::Rotate180 => T::_180,
        config::Transform::Rotate270 => T::_270,
        config::Transform::Flipped => T::Flipped,
        config::Transform::Flipped90 => T::Flipped90,
        config::Transform::Flipped180 => T::Flipped180,
        config::Transform::Flipped270 => T::Flipped270,
    }
}
