//! Construction shared by both backends: socket, event sources, and state.

use std::path::PathBuf;

use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use crate::comp::{LoopData, Wdm, client_state};
use crate::config::Config;
use crate::login::Login;
use crate::supervise::Greeter;
use crate::users::{LastSessions, UidRange};

/// Build the compositor state and register the event sources it needs.
///
/// Returns the state and the name of the socket the greeter must connect to.
/// Nothing is started yet: [`start`] does that once the backend has an output,
/// because a greeter that binds before any output exists has nothing to attach a
/// layer surface to.
pub fn build(
    display: &mut Display<Wdm>,
    loop_handle: &LoopHandle<'static, LoopData>,
    config: Config,
    privileged: bool,
) -> Result<(Wdm, String), Box<dyn std::error::Error>> {
    let greeter_user = config.greeter.user.clone();
    let greeter_command = config.greeter.command.clone();

    // The socket lives in XDG_RUNTIME_DIR. When privileged, that is wdm's own
    // directory, which the greeter user owns and nobody else can enter — the
    // outer half of the trust boundary that lets a plaintext password travel
    // over this connection.
    if privileged {
        // SAFETY of the value, not of memory: pointing at a directory the
        // greeter cannot reach would leave it unable to connect at all.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", crate::supervise::RUNTIME_DIR) };
    }

    let handle = display.handle();

    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    loop_handle.insert_source(socket, |stream, _, data| {
        if let Err(e) = data.display.handle().insert_client(stream, client_state()) {
            log::error!("accepting greeter connection: {e}");
        }
    })?;

    // Client requests arrive on the display's own fd.
    let poll_fd = {
        use std::os::fd::AsFd;
        display.backend().poll_fd().as_fd().try_clone_to_owned()?
    };
    loop_handle.insert_source(
        Generic::new(poll_fd, Interest::READ, Mode::Level),
        |_, _, data: &mut LoopData| {
            if let Err(e) = data.dispatch() {
                // A protocol error kills the offending client, not wdm. Losing
                // the greeter is recoverable; exiting is not.
                log::error!("dispatching clients: {e}");
            }
            Ok(PostAction::Continue)
        },
    )?;

    // PAM threads report here.
    let (events_tx, events_rx) = smithay::reexports::calloop::channel::channel();
    loop_handle.insert_source(events_rx, |event, _, data| {
        if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
            let action = data.state.login.handle_auth_event(event);
            data.state.queue_action(action);
        }
    })?;

    let last_sessions_path = PathBuf::from(crate::users::LAST_SESSION_PATH);
    let history = LastSessions::load(&last_sessions_path);
    let users = crate::users::discover(UidRange::from_system(), &history);

    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let sessions = crate::sessions::discover(&locales);

    if sessions.is_empty() {
        // Not fatal: a greeter can still show the error, and a session may be
        // installed while wdm is running.
        log::warn!("no sessions found; nothing will be launchable");
    }
    log::info!(
        "{} user(s), {} session(s) available",
        users.len(),
        sessions.len()
    );

    let login = Login::new(
        users,
        sessions,
        config.default_session.clone(),
        last_sessions_path,
        config.vt,
        events_tx,
        loop_handle.clone(),
    );

    let greeter = Greeter::new(&greeter_command, &greeter_user, &socket_name, privileged)?;
    greeter.prepare_runtime_dir()?;

    let state = Wdm::new(&handle, config, login, greeter)?;
    Login::create_global(&handle);

    Ok((state, socket_name))
}

/// Launch the greeter, once outputs exist.
///
/// A greeter that will not start is not fatal: it goes through the same backoff
/// and give-up policy as one that crashes, so a misconfigured `greeter.command`
/// ends with an explanation on screen rather than wdm exiting.
pub fn start(
    data: &mut LoopData,
    loop_handle: &LoopHandle<'static, LoopData>,
    socket_name: &str,
) {
    log::info!("greeter socket is {socket_name}");
    crate::backend::spawn_greeter(data, loop_handle);
}
