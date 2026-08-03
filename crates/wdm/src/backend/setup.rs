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

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| "XDG_RUNTIME_DIR is not set, so there is nowhere to put the socket")?;

    let handle = display.handle();

    // The greeter account, resolved before the socket exists because the socket
    // has to belong to it.
    let greeter_credentials = if privileged {
        let user = uzers::get_user_by_name(&greeter_user)
            .ok_or_else(|| format!("greeter user {greeter_user} does not exist"))?;
        Some((user.uid(), user.primary_group_id()))
    } else {
        None
    };

    // The directory is created root-owned 0700, and stays root-only until the
    // socket inside it has been bound and secured. Chowning it to the greeter
    // first would open a window in which a process running as the greeter uid
    // could swap the socket for a symlink while root is still fixing the
    // socket's ownership by path.
    crate::supervise::create_runtime_dir(privileged)?;

    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    // The uid allowed to connect. Everything on this socket is trusted to the
    // extent that it can drive a PAM conversation, so the greeter account is the
    // only thing that may speak on it.
    let expected_uid = match greeter_credentials {
        Some((uid, _)) => uid,
        // SAFETY: getuid cannot fail and touches no memory.
        None => unsafe { libc::getuid() },
    };

    let socket_path = std::path::Path::new(&runtime_dir).join(&socket_name);
    if let Err(e) = secure_socket(&socket_path, greeter_credentials) {
        // Fatal both ways. Loose permissions would leave the socket reachable
        // while the spec promises otherwise; the wrong owner leaves the greeter
        // unable to connect at all.
        return Err(format!("securing {}: {e}", socket_path.display()).into());
    }

    // Only now, with the socket bound and secured, does the greeter get the
    // directory. Until this point it was root-only, so nothing could tamper
    // with the paths the two calls above operated on.
    crate::supervise::hand_over_runtime_dir(greeter_credentials)?;

    loop_handle.insert_source(socket, move |stream, _, data| {
        match peer_uid(&stream) {
            Ok(uid) if uid == expected_uid => {}
            Ok(uid) => {
                // Dropping the stream closes it. Root is refused too: wdm is
                // root, and a root process has no business posing as the greeter.
                log::warn!("refusing connection from uid {uid}, expected {expected_uid}");
                return;
            }
            Err(e) => {
                log::error!("reading peer credentials, refusing connection: {e}");
                return;
            }
        }

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

/// The uid on the other end of a connected unix socket.
///
/// `SO_PEERCRED` is resolved by the kernel at connect time from the peer's own
/// credentials, so it cannot be forged by the client the way a value sent over
/// the socket could be.
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;

    // SAFETY: cred and len are valid for the size advertised, and the fd is a
    // connected socket owned by the caller for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast::<libc::c_void>(),
            &raw mut len,
        )
    };

    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(cred.uid)
}



/// Lock the socket to the greeter account.
///
/// Two independent controls rather than one: `0700` on the directory is a
/// property of the path, and anything that can already reach the path — a bind
/// mount, a stray chmod, a process that inherited a directory fd — is then
/// unrestricted.
///
/// Both calls here operate by path and follow symlinks, so ordering carries
/// the guarantee: this runs while the runtime directory is still root-owned
/// `0700` — before [`crate::supervise::hand_over_runtime_dir`] gives it to the
/// greeter — so no unprivileged process can have replaced the socket with a
/// symlink and turned the chown into a write on an arbitrary file.
///
/// The owner is the half that is easy to forget, and forgetting it does not
/// weaken the socket, it breaks it: `0600` describes the *owner*, wdm is root,
/// and `connect(2)` needs write permission, so an unprivileged greeter is
/// refused by the kernel before it can say anything. It only happens once
/// privileges are actually split — in development wdm and the greeter are the
/// same user — so it first appears on a real machine at boot, as a greeter that
/// exits immediately with "Failed to open display".
fn secure_socket(
    path: &std::path::Path,
    owner: Option<(libc::uid_t, libc::gid_t)>,
) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;

    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::MetadataExt;

    #[test]
    fn a_secured_socket_belongs_to_the_greeter() {
        let dir = std::env::temp_dir().join(format!("wdm-socket-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wayland-0");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        // Chowning to who we already are: the call still has to happen and
        // still has to name the socket rather than the directory, which is what
        // this pins down. Running the test as root would not make it stricter.
        // SAFETY: neither call can fail nor touch memory.
        let owner = unsafe { (libc::getuid(), libc::getgid()) };
        secure_socket(&path, Some(owner)).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600, "socket is not 0600");
        assert_eq!(
            (meta.uid(), meta.gid()),
            owner,
            "socket does not belong to the greeter account"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unprivileged_leaves_ownership_alone() {
        // Development: wdm already runs as the greeter user, and a chown to
        // anyone would need privileges it does not have.
        let dir = std::env::temp_dir().join(format!("wdm-socket-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wayland-0");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        secure_socket(&path, None).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);

        std::fs::remove_dir_all(&dir).ok();
    }


    #[test]
    fn peer_uid_reports_the_connecting_process() {
        // The whole greeter trust boundary rests on this syscall being wired up
        // correctly; a getsockopt that silently returned 0 would admit everyone
        // while looking like it checked.
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();

        // SAFETY: getuid cannot fail and touches no memory.
        let me = unsafe { libc::getuid() };

        assert_eq!(peer_uid(&a).unwrap(), me);
        assert_eq!(peer_uid(&b).unwrap(), me);
    }

    #[test]
    fn peer_uid_is_not_hardcoded_to_root() {
        // Guards against the check passing trivially in a container that happens
        // to run as root: whatever it reports must be this process's own uid.
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let me = unsafe { libc::getuid() };
        let reported = peer_uid(&a).unwrap();

        assert_eq!(reported, me);
        if me != 0 {
            assert_ne!(reported, 0, "an unprivileged test reported uid 0");
        }
    }
}
