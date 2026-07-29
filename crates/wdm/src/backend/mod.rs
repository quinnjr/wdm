//! Backends, and the parts of the main loop they share.
//!
//! Two backends exist because the DRM path cannot be exercised without owning a
//! seat. [`udev`] is the real one; [`winit`] runs wdm nested inside an existing
//! Wayland session as an ordinary window, which is how everything except the
//! DRM, seat and handoff paths gets tested with no root and no VT switching.

pub mod setup;
pub mod udev;
pub mod winit;

use std::time::Duration;

use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::LoopHandle;

use crate::comp::LoopData;
use crate::login::{Action, LaunchRequest};
use crate::session::Launch;
use crate::supervise::Disposition;

/// Something an event source noticed that only the backend can act on.
///
/// Event source closures receive the compositor state, not the backend that owns
/// the DRM device and the seat, so they queue requests here and the backend
/// drains them on its next pass. One queue rather than a field per case, because
/// a field per case is how they get forgotten.
#[derive(Debug)]
pub enum Request {
    /// Ctrl+Alt+F<n> was pressed.
    SwitchVt(i32),
    /// The seat became active (`true`) or is being paused (`false`).
    SessionActive(bool),
    /// A GPU appeared.
    DeviceAdded(std::path::PathBuf),
    /// A GPU went away. Identified by device id because that is all udev
    /// reports on removal.
    DeviceRemoved(libc::dev_t),
    /// The device changed, which is how monitor hotplug arrives.
    RescanConnectors,
    /// A page flip completed on this CRTC.
    VBlank(smithay::reexports::drm::control::crtc::Handle),
}

/// Outcome of handling an [`Action`].
pub enum Handled {
    /// Carry on running the greeter.
    Continue,
    /// A session was prepared and the display should be handed to it.
    ///
    /// The backend must release DRM master and input before calling
    /// [`Launch::spawn`], which is why this is returned rather than done here.
    HandOff(Launch),
}

/// Act on whatever the greeter's last request or the PAM thread asked for.
pub fn handle_action(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) -> Handled {
    let action = std::mem::replace(&mut data.state.pending_action, Action::None);

    match action {
        Action::None => Handled::Continue,

        Action::Launch(request) => match prepare(data, request) {
            Some(launch) => Handled::HandOff(launch),
            None => Handled::Continue,
        },

        Action::RestartGreeter { error } => {
            restart_greeter(data, loop_handle, error, Duration::ZERO);
            Handled::Continue
        }
    }
}

/// Resolve a launch request while still privileged.
///
/// Done before the greeter is torn down so a failure can be reported to the
/// greeter that is still on screen, rather than after the display has gone dark.
fn prepare(data: &mut LoopData, request: LaunchRequest) -> Option<Launch> {
    let vt = data.state.config.vt;
    let session_id = request.session.id.clone();

    match Launch::prepare(
        &request.session,
        &request.username,
        vt,
        request.pam_env,
        request.extra_env,
    ) {
        Ok(launch) => {
            // Recorded now rather than after exec: the session is what the user
            // chose, and a compositor that crashes on startup should still be
            // preselected so they can try it again or pick another.
            data.state
                .login
                .remember_session(&request.username, &session_id);
            Some(launch)
        }
        Err(e) => {
            log::error!("preparing session {session_id}: {e}");
            data.state.pending_action = Action::RestartGreeter {
                error: Some(e.to_string()),
            };
            None
        }
    }
}

/// Tear down the current greeter and start a fresh one after `delay`.
///
/// `error` is advertised to the new greeter through `last_error`, so a user
/// whose session failed to start is told why instead of being bounced back to a
/// login prompt with no explanation.
pub fn restart_greeter(
    data: &mut LoopData,
    loop_handle: &LoopHandle<'static, LoopData>,
    error: Option<String>,
    delay: Duration,
) {
    data.state.greeter.kill();
    data.state.login.reset();
    data.state.login.set_last_error(error);
    // Surfaces belonging to the dead greeter must go, or they keep being
    // rendered over the new one.
    data.state.layers.clear();

    if data.state.greeter.gave_up() {
        return;
    }

    // Users may have been added since wdm started, and the session history has
    // changed if the last login succeeded.
    data.state
        .login
        .refresh_users(crate::users::UidRange::from_system());

    if delay.is_zero() {
        spawn_greeter(data);
        return;
    }

    let timer = Timer::from_duration(delay);
    if let Err(e) = loop_handle.insert_source(timer, |_, _, data| {
        spawn_greeter(data);
        TimeoutAction::Drop
    }) {
        // Without the timer nothing would ever restart the greeter, so start it
        // immediately instead of leaving a blank screen.
        log::error!("arming greeter restart timer: {e}");
        spawn_greeter(data);
    }
}

fn spawn_greeter(data: &mut LoopData) {
    if let Err(e) = data.state.greeter.spawn() {
        log::error!("spawning greeter: {e}");
    }
}

/// Reap the greeter if it exited and apply its restart policy.
pub fn poll_greeter(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) {
    let Some(disposition) = data.state.greeter.poll() else {
        return;
    };

    match disposition {
        Disposition::Restart(delay) => {
            log::info!("restarting greeter in {delay:?}");
            restart_greeter(data, loop_handle, None, delay);
        }
        Disposition::GaveUp { reason } => {
            // Nothing more will be started. The backend draws this on screen;
            // tty1 is the way back in.
            log::error!("{reason}");
            data.state.layers.clear();
            data.state.give_up_reason = Some(reason);
        }
    }
}
