//! Nested backend: wdm as an ordinary window inside an existing session.
//!
//! This is the development and test path. It exercises the protocol, the PAM
//! conversation, the greeter lifecycle, enumerate and output ranking with no
//! root and no VT switching. What it deliberately does not exercise is DRM, the
//! seat, and the handoff — there is no display to hand over, so a launched
//! session runs without one and is only useful for checking that the environment
//! and privilege drop are right.

use std::time::{Duration, Instant};

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, Frame, Renderer};
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::{Physical, Rectangle, Size, Transform};

use crate::comp::{LoopData, Wdm};
use crate::config::Config;
use crate::render::WdmElement;

use super::{Handled, handle_action, poll_greeter};

/// How long the loop blocks waiting for events before drawing again.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

pub fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, LoopData> = EventLoop::try_new()?;
    let mut display: Display<Wdm> = Display::new()?;
    let handle = display.handle();
    let loop_handle = event_loop.handle();

    // Never privileged: this backend exists to run as a normal user, so there is
    // no separate greeter account and nothing to drop.
    let (mut state, socket_name) = super::setup::build(&mut display, &loop_handle, config, false)?;

    let (mut backend, mut winit_events) = winit::init::<GlesRenderer>()?;

    // A stand-in for a connector. The name is not a real connector, so config
    // output blocks will not match it — correct, since there is no monitor here.
    let output = Output::new(
        "WINIT-1".to_owned(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "wdm".to_owned(),
            model: "winit".to_owned(),
        },
    );
    output.create_global::<Wdm>(&handle);

    let mut mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    state.set_outputs(vec![output.clone()]);

    let formats = backend
        .renderer()
        .egl_context()
        .dmabuf_render_formats()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    state.init_dmabuf(&handle, formats);

    let mut data = LoopData { state, display };
    super::setup::start(&mut data, &loop_handle, &socket_name);

    let start = Instant::now();
    let mut error_screen = None;
    // Latches the "could not build the give-up screen" report. The failure is
    // a property of the buffer and the renderer, neither of which changes
    // between frames, so logging it per pass is 60 identical lines a second.
    let mut error_screen_logged = false;
    // Not for partial rendering — every drawn frame is drawn in full — but as
    // the answer to "did anything change since the last pass", which is what
    // lets an idle login screen skip the frame entirely. `from_output` so a
    // window resize (a mode change on the output) reads as full damage.
    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    while data.state.running {
        let status = winit_events.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                // The greeter must be reconfigured before the next frame or it
                // draws at the old size.
                data.state.configure_layers();
            }
            WinitEvent::Input(event) => {
                if let Some(vt) = crate::input::handle(&mut data.state, event) {
                    // There is no seat here, so this is the one input behaviour
                    // the nested backend cannot honour.
                    log::info!("ignoring VT switch to {} in the nested backend", vt.0);
                }
            }
            WinitEvent::CloseRequested => data.state.running = false,
            _ => {}
        });

        if matches!(status, PumpStatus::Exit(_)) {
            data.state.running = false;
        }

        poll_greeter(&mut data, &loop_handle);

        if let Handled::HandOff { username } = handle_action(&mut data, &loop_handle) {
            // The same order as the udev backend, for the same reason and with
            // one part missing: there is no display to release, because the
            // nested backend never held one. The greeter still goes first, so
            // the sequence a reader sees here is the sequence that matters.
            data.state.greeter.kill();
            data.state.layers.clear();

            // The session is still launched, so the environment, the privilege
            // drop and the helper's own handling can be inspected — but it will
            // not find a compositor to talk to.
            log::warn!("launching a session for {username} without a display to hand over");
            if data.state.login.launch() {
                let outcome = super::wait_for_session(&mut event_loop, &mut data);
                log::info!("session for {username}: {outcome}");
            }
            data.state.login.end_session();
            data.state.running = false;
            break;
        }

        data.state.cleanup_popups();

        if let Err(e) = render(
            &mut data,
            &mut backend,
            &output,
            &mut damage_tracker,
            start,
            &mut error_screen,
            &mut error_screen_logged,
        ) {
            log::error!("rendering: {e}");
            // damage_output() commits the elements it was given as "what is on
            // screen" before any of the GL calls that actually put them there.
            // A transient failure — a resize losing the context, say — would
            // otherwise leave the tracker believing a frame it never drew was
            // presented: the next pass sees no damage, returns early, and
            // nothing is ever submitted again. Throwing the tracker away
            // restores the only safe belief, which is that the screen contents
            // are unknown and the next frame must be full.
            damage_tracker = OutputDamageTracker::from_output(&output);
        }

        // Flush what this pass generated before blocking; see LoopData::dispatch.
        if let Err(e) = data.dispatch() {
            log::error!("dispatching clients: {e}");
        }

        if event_loop
            .dispatch(Some(FRAME_INTERVAL), &mut data)
            .is_err()
        {
            break;
        }
    }

    Ok(())
}

fn render(
    data: &mut LoopData,
    backend: &mut smithay::backend::winit::WinitGraphicsBackend<GlesRenderer>,
    output: &Output,
    damage_tracker: &mut OutputDamageTracker,
    start: Instant,
    error_screen: &mut Option<(String, Size<i32, Physical>, MemoryRenderBuffer)>,
    error_screen_logged: &mut bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let size = backend.window_size();
    let damage = [Rectangle::from_size(size)];
    let give_up = data.state.give_up_reason.clone();

    let elements: Vec<WdmElement<GlesRenderer>> = match &give_up {
        // Same element path as the DRM backend, so the give-up screen is
        // exercised by whichever backend is in use rather than only here.
        Some(reason) => {
            // Cached on message *and* size, as the DRM backend does.
            // Rasterising a full-screen image every frame is exactly what
            // render::error_buffer's doc says must not happen.
            if error_screen
                .as_ref()
                .is_none_or(|(cached, cached_size, _)| cached != reason || *cached_size != size)
            {
                *error_screen = Some((
                    reason.clone(),
                    size,
                    crate::render::error_buffer(reason, size),
                ));
                // A new message or size is a new buffer, so a failure to import
                // it is news again.
                *error_screen_logged = false;
            }

            let (_, _, buffer) = error_screen.as_ref().expect("just populated");
            // Explicit logical size, as in the DRM backend: `None` would make
            // smithay read the buffer's physical size as logical and scale it up
            // again. This backend hardcodes scale 1 so the two agree here — which
            // is precisely why the DRM backend's version of this went unnoticed.
            let logical =
                crate::render::error_element_size(size, output.current_scale().fractional_scale());
            match MemoryRenderBufferRenderElement::from_buffer(
                backend.renderer(),
                (0.0, 0.0),
                buffer,
                None,
                None,
                Some(logical),
                Kind::Unspecified,
            ) {
                Ok(element) => vec![WdmElement::Image(element)],
                Err(e) => {
                    if !*error_screen_logged {
                        log::error!("building the error screen: {e}");
                        *error_screen_logged = true;
                    }
                    // Leaving the window showing whatever was last drawn beats
                    // clearing it to nothing: the previous frame is at least
                    // the greeter the user was looking at, whereas an empty
                    // window says the display manager died. Returning here
                    // also keeps the damage tracker's idea of the screen
                    // truthful, since nothing is submitted.
                    return Ok(());
                }
            }
        }
        None => {
            let renderer = backend.renderer();
            data.state.elements(renderer, output)
        }
    };

    // The udev backend's `drew` guard, mirrored: when nothing changed since the
    // last pass there is nothing to submit, and releasing frame callbacks for a
    // frame that was not drawn asks the greeter to render again immediately —
    // a busy loop between two idle processes. An age of 1 is always right here
    // because every frame that *is* drawn is drawn in full, so the window
    // always shows exactly the state the tracker recorded last.
    if damage_tracker.damage_output(1, &elements)?.0.is_none() {
        return Ok(());
    }

    // Scoped: the framebuffer borrows the backend, and submit() needs it back.
    {
        let (renderer, mut framebuffer) = backend.bind()?;

        // Flipped180 because winit's framebuffer origin is the opposite of ours.
        let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
        frame.clear(Color32F::new(0.05, 0.05, 0.07, 1.0), &damage)?;
        smithay::backend::renderer::utils::draw_render_elements(
            &mut frame, 1.0, &elements, &damage,
        )?;

        // The nested compositor synchronises for us, so the fence needs no wait.
        let _sync = frame.finish()?;
    }

    backend.submit(Some(&damage))?;

    data.state.send_frames(start.elapsed().as_millis() as u32);

    Ok(())
}
