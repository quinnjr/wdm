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

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use smithay::reexports::calloop::EventLoop;

use crate::comp::LoopData;
use crate::login::{Action, SessionOutcome};
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
    /// A session was validated and the display must now be handed to it.
    ///
    /// The backend releases DRM master, the renderer, libinput and the seat, and
    /// **only then** calls [`crate::login::Login::launch`], which is what tells
    /// the helper to run `pam_open_session`. Returned rather than done here
    /// because only the backend can release any of that.
    ///
    /// The username is for the log line and nothing else: what gets launched is
    /// held in the `Login` and is unreachable from here, so there is no second
    /// copy of it that some other call site could start while the GPU is still
    /// held.
    HandOff { username: String },
}

/// Act on everything the greeter's requests and the PAM helper queued.
///
/// Drains the whole queue: handling only one per pass would let a burst of
/// requests strand an action until the next event wakes the loop.
pub fn handle_action(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) -> Handled {
    while let Some(action) = data.state.pending_actions.pop_front() {
        match action {
            Action::None => {}

            Action::Launch { username } => {
                // Anything still queued is moot: the display is about to change
                // hands and the greeter is about to be killed. A pending
                // respawn is moot too, and would otherwise fire in the next
                // login generation.
                data.state.pending_actions.clear();
                // Backend requests go the same way, and for a sharper reason: a
                // VBlank(crtc) queued between the last drain_requests and the
                // handoff is acted on in the *next* generation, against a
                // freshly built Device where that CRTC handle may belong to a
                // different Head — so frame_submitted is called for a frame
                // nobody queued and logs an error. A DeviceAdded or
                // RescanConnectors left here is equally a statement about a
                // device that no longer exists.
                data.state.requests.clear();
                disarm_respawn(data, loop_handle);
                return Handled::HandOff { username };
            }

            Action::RestartGreeter { error } => {
                restart_greeter(data, loop_handle, error, Duration::ZERO);
            }
        }
    }

    Handled::Continue
}

/// Block until the user's session ends, dispatching the loop while it runs.
///
/// The backend has released the display and killed the greeter, so there is
/// nothing left to draw and nothing to serve; what the loop is still doing is
/// carrying the helper's messages, which arrive on the auth channel, and reading
/// the signalfd so a `systemctl stop wdm` during a session is noticed rather
/// than left pending for hours.
///
/// It deliberately does **not** return early when `running` goes false. wdm
/// exiting while the user's session still owns the display would orphan it, and
/// the shutdown path the backend already has runs after this returns.
///
/// The consequence is worth naming, because the two halves are each obviously
/// right and only the combination bites: the SIGTERM source in
/// [`setup::build`] exists so `systemctl stop wdm` is not left pending, and this
/// loop ignores what it recorded, so for the whole of a user's session the
/// handler is inert. A `systemctl stop wdm` issued then does not complete when
/// the user logs out — it completes at `TimeoutStopSec`, with a SIGKILL that
/// takes the PAM helper and so skips `pam_close_session`. `packaging/wdm.service`
/// sets that timeout explicitly and says why it is neither shorter nor
/// `infinity`; a deployment that changes this policy has to change that too.
pub fn wait_for_session(
    event_loop: &mut EventLoop<'static, LoopData>,
    data: &mut LoopData,
) -> SessionOutcome {
    loop {
        if let Some(outcome) = data.state.login.take_session_outcome() {
            // Actions queued while the display was gone belong to nothing: the
            // greeter that could have caused them is dead, and the next login
            // generation starts from a fresh greeter. Left here they would be
            // drained by the next generation's first handle_action.
            data.state.pending_actions.clear();
            return outcome;
        }

        // A timeout rather than a block, so a wedged auth channel cannot make
        // this unresponsive to anything else the loop still owns.
        if let Err(e) = event_loop.dispatch(Some(Duration::from_millis(250)), data) {
            // Nothing here can recover: the loop is the only way the session's
            // end could ever be heard about, so spinning on a broken one would
            // hang wdm for the life of the machine. Reporting it as a failed
            // session at least gets a greeter back on screen with a reason.
            log::error!("dispatching while the session runs: {e}");
            data.state.pending_actions.clear();
            return SessionOutcome::Failed(format!("wdm's event loop failed: {e}"));
        }
    }
}

/// Tear down the current greeter and start a fresh one after `delay`.
///
/// `error`, when there is one, is advertised to the new greeter through
/// `last_error`, so a user whose session failed to start is told why instead of
/// being bounced back to a login prompt with no explanation. `None` means "this
/// restart has nothing to add", not "forget what was recorded": a greeter that
/// dies before it ever binds restarts with no error of its own, and overwriting
/// here would discard the reason the session failed before anyone had seen it.
/// That is why the `None` arm skips the setter rather than passing it along:
/// [`crate::login::Login::set_last_error`] takes a `String` and cannot express
/// "clear". The error is cleared where it has served its purpose —
/// [`crate::login::Login::begin_attempt`], once the user has moved on to a new
/// attempt.
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
    if let Some(error) = error {
        data.state.login.set_last_error(error);
    }
    // Surfaces, cursor and focus all belonged to a greeter that no longer
    // exists; see Wdm::forget_greeter for why the cursor is not optional.
    data.state.forget_greeter();

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
        Err(
            e @ (crate::supervise::GreeterError::GaveUp
            | crate::supervise::GreeterError::AlreadyRunning),
        ) => {
            log::debug!("not starting a greeter: {e}");
        }

        Err(e) => {
            log::error!("spawning greeter: {e}");
            match data.state.greeter.note_spawn_failure(&e.to_string()) {
                // The reason is dropped here rather than shown: this path has
                // not started a greeter, so there is nothing on screen and
                // nothing that could bind to be told. arm_respawn is the whole
                // response.
                Disposition::Restart { delay, .. } => arm_respawn(data, loop_handle, delay),
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
fn arm_respawn(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>, delay: Duration) {
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
    data.state.login.clear_bindings();
    // Nothing of the greeter's may survive onto the give-up screen, the cursor
    // surface included: it is the one thing the error screen would still render.
    data.state.forget_greeter();
    data.state.give_up_reason = Some(reason);
}

/// Reap the greeter if it exited and apply its restart policy.
pub fn poll_greeter(data: &mut LoopData, loop_handle: &LoopHandle<'static, LoopData>) {
    let Some(disposition) = data.state.greeter.poll() else {
        return;
    };

    match disposition {
        Disposition::Restart { delay, reason } => {
            log::info!("restarting greeter in {delay:?}: {reason}");
            // Some, not None: a greeter that crashed knows why, and passing None
            // here meant the user watched the login screen vanish and reappear
            // twice with no indication anything was wrong, learning the reason
            // only on the third failure via the give-up screen. The
            // None-means-preserve contract is for the case with nothing to add —
            // a greeter that died before it ever bound — not this one.
            restart_greeter(data, loop_handle, Some(reason), delay);
        }
        Disposition::GaveUp { reason } => give_up(data, reason),
    }
}
