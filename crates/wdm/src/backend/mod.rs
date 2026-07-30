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

/// Act on everything the greeter's requests and the PAM thread queued.
///
/// Drains the whole queue: handling only one per pass would let a burst of
/// requests strand an action until the next event wakes the loop.
pub fn handle_action(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) -> Handled {
    while let Some(action) = data.state.pending_actions.pop_front() {
        match action {
            Action::None => {}

            Action::Launch(request) => {
                if let Some(launch) = prepare(data, request) {
                    // Anything still queued is moot: the display is about to
                    // change hands and the greeter is about to be killed. A
                    // pending respawn is moot too, and would otherwise fire in
                    // the next login generation.
                    data.state.pending_actions.clear();
                    disarm_respawn(data, loop_handle);
                    return Handled::HandOff(launch);
                }
            }

            Action::RestartGreeter { error } => {
                restart_greeter(data, loop_handle, error, Duration::ZERO);
            }
        }
    }

    Handled::Continue
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
            data.state.queue_action(Action::RestartGreeter {
                error: Some(e.to_string()),
            });
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
    // The old greeter's objects belong to a connection that is gone.
    data.state.login.clear_bindings();
    data.state.login.set_last_error(error);
    // Surfaces belonging to the dead greeter must go, or they keep being
    // rendered over the new one.
    data.state.layers.clear();
    // Focus was on a surface that no longer exists.
    data.state.update_focus();

    if data.state.greeter.gave_up() {
        return;
    }

    // Users may have been added since wdm started, and the session history has
    // changed if the last login succeeded.
    data.state
        .login
        .refresh_users(crate::users::UidRange::from_system());

    if delay.is_zero() {
        spawn_greeter(data, loop_handle);
    } else {
        arm_respawn(data, loop_handle, delay);
    }
}

/// Start the greeter, applying the restart policy if it will not start.
///
/// Deliberately does not route a failure back through [`restart_greeter`]: that
/// would close a call cycle, and a policy that ever returned a zero delay would
/// turn it into unbounded recursion. Respawning is scheduled directly instead.
pub fn spawn_greeter(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) {
    // This attempt is happening now, so any timer scheduling another one is
    // stale. Without disarming it a timer armed before a login can fire in the
    // next generation and start a second greeter alongside the first.
    disarm_respawn(data, loop_handle);

    match data.state.greeter.spawn() {
        Ok(()) => {}

        // Not failures of this attempt: the policy has already spoken, or a
        // greeter is already up. Re-running the failure accounting would count
        // a success against the budget.
        Err(e @ (crate::supervise::GreeterError::GaveUp
            | crate::supervise::GreeterError::AlreadyRunning)) => {
            log::debug!("not starting a greeter: {e}");
        }

        Err(e) => {
            log::error!("spawning greeter: {e}");
            match data.state.greeter.note_spawn_failure(&e.to_string()) {
                Disposition::Restart(delay) => arm_respawn(data, loop_handle, delay),
                Disposition::GaveUp { reason } => give_up(data, reason),
            }
        }
    }
}

/// Cancel a pending respawn, if one is scheduled.
pub fn disarm_respawn(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) {
    if let Some(token) = data.state.respawn_token.take() {
        loop_handle.remove(token);
    }
}

/// Schedule another attempt at starting the greeter.
fn arm_respawn(
    data: &mut LoopData,
    loop_handle: &LoopHandle<'static, LoopData>,
    delay: Duration,
) {
    disarm_respawn(data, loop_handle);

    let handle = loop_handle.clone();
    let timer = Timer::from_duration(delay);

    match loop_handle.insert_source(timer, move |_, _, data| {
        data.state.respawn_token = None;
        spawn_greeter(data, &handle);
        TimeoutAction::Drop
    }) {
        Ok(token) => data.state.respawn_token = Some(token),
        Err(e) => {
            // Without the timer nothing would ever restart the greeter. Giving
            // up is better than a blank screen, because it puts the reason on
            // the display.
            log::error!("arming greeter restart timer: {e}");
            give_up(data, format!("cannot schedule a greeter restart: {e}"));
        }
    }
}

/// Stop trying, and put the reason on screen.
///
/// tty1 is the way back in, which is why the message says so.
fn give_up(data: &mut LoopData, reason: String) {
    log::error!("{reason}");
    data.state.layers.clear();
    data.state.login.clear_bindings();
    data.state.update_focus();
    data.state.give_up_reason = Some(reason);
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
        Disposition::GaveUp { reason } => give_up(data, reason),
    }
}
