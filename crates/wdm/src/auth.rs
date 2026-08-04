//! PAM authentication: wdm's end of the conversation with [`crate::pamhelper`].
//!
//! PAM does not run in this process. `wdm --pam-helper` is a freshly `exec`'d
//! copy of wdm's own binary with a `SOCK_SEQPACKET` socket on fd 3, and this
//! module spawns it, drives it, and turns what it says into [`AuthEvent`]s. The
//! four defects that made the move necessary — a module that `fork`s and `exit`s
//! inside `pam_sm_open_session` taking the compositor's EGL state down with it,
//! `pam_loginuid` writing from a thread that is not the thread-group leader,
//! per-thread `prctl` state that never reaches the session — are all consequences
//! of PAM having run in the address space that owns DRM, EGL, libinput and the
//! seat. They are stated in full in [`crate::pamhelper`].
//!
//! What that leaves here is a socket and a thread that reads it. The thread is
//! not the defect and removing it is not the fix: it blocks on `recv` and
//! forwards, and holds no PAM state at all.
//!
//! ponytail: the socket could be a `calloop` event source instead, which would
//! retire the thread entirely. It is not, because [`AuthHandle::start`] would
//! then need a `LoopHandle` and every caller would change with it — a wider
//! change than the one that fixes the defect, and one that should be made on its
//! own.
//!
//! Not every [`AuthEvent::Prompt`] originates from a PAM module: on
//! [`RESPONSE_TIMEOUT`] the helper composes an `error` prompt of its own before
//! failing the attempt. That matters to readers of the other end —
//! `handle_auth_event` routes it exactly like PAM's, which is the point. It is
//! the event that tells a greeter the failure was explained and stops it
//! auto-retrying into an account lockout.
//!
//! Cancellation is expressed by dropping [`AuthHandle`], as it always was. The
//! mechanism is now a socket shutdown rather than an `mpsc` disconnect: the
//! helper's next read hits EOF, its conversation returns `PAM_CONV_ERR`, and PAM
//! unwinds by itself. The contract `login.rs` relies on — `Login::reset()` drops
//! the handle to cancel — is unchanged.
//!
//! Several pieces here are `pub(crate)` rather than private — the prompt-id
//! counter, [`RESPONSE_TIMEOUT`], [`describe`], [`pam_session_env`] and
//! [`SessionDescription::pam_items`] — because [`crate::pamhelper`] is where they
//! are used. They live here rather than there so that the two ends of the socket
//! cannot disagree about an id counter, a timeout, or an error string that must
//! not leak whether an account exists.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use zeroize::Zeroize;

use crate::pamwire::{MAX_MESSAGE, Msg};

/// PAM service name; wdm ships `/etc/pam.d/wdm`.
pub const SERVICE: &str = "wdm";

/// Source of prompt ids, shared by every attempt in the process.
///
/// Per-attempt counters would restart at 0, so a late `respond` carrying id 0
/// from a cancelled conversation would match the first prompt of the *next*
/// one — feeding one user's secret to another user's PAM stack. An id must
/// identify the question it was issued for, which means it may never be reused.
static NEXT_PROMPT_ID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn next_prompt_id() -> u32 {
    NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed)
}

/// How long a prompt may go unanswered before the attempt is abandoned.
///
/// A greeter that stops answering would otherwise pin a thread and a PAM
/// transaction forever. Measured from when the prompt was emitted, not from the
/// last message received, so a stream of stale responses cannot extend it.
///
/// This is a leak guard, **not** a user-facing patience limit, and the
/// difference is load-bearing. There is no way to end a `pam_authenticate` that
/// is mid-conversation without failing it: whether the conversation callback
/// returns `CONV_ERR` because it timed out or because the greeter cancelled,
/// PAM unwinds through its failure path and `pam_faillock`'s `authfail` arm
/// records an attempt. So every second of this timeout is a second in which a
/// user who is merely *reading the screen* can be charged with a failed login.
///
/// At 60s that was reachable by a user who walked away — and, with a greeter
/// that opens a conversation before anyone has typed, by a machine sitting
/// untouched at its login screen. Three of those reach Arch's `deny=3` and lock
/// the account, unattended, in about three minutes.
///
/// Half an hour is chosen so that only a greeter that is genuinely stuck gets
/// here. The cost of being wrong in this direction is one pinned thread for
/// half an hour; the cost of being wrong in the other direction is a locked-out
/// account, so the asymmetry decides the value.
pub(crate) const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What wdm tells pam_systemd about the seat before opening a session.
///
/// These have to reach PAM's own environment before `pam_open_session`;
/// putting them only in the launched child's environment is too late, because
/// pam_systemd has already registered the logind session by then.
#[derive(Debug, Clone)]
pub struct SessionDescription {
    pub seat: String,
    pub vtnr: u32,
    /// `wayland` or `x11`. Provisional: the greeter has not chosen a session
    /// when authentication starts, and the chosen type replaces this in PAM's
    /// environment before `pam_open_session`.
    pub session_type: String,
    /// The desktop id, without the `.desktop` suffix. Empty until the greeter
    /// has chosen one, and likewise replaced before `pam_open_session`.
    pub desktop: String,
}

impl SessionDescription {
    pub(crate) fn pam_items(&self) -> Vec<(&'static str, String)> {
        let mut items = vec![
            ("XDG_SEAT", self.seat.clone()),
            ("XDG_VTNR", self.vtnr.to_string()),
            ("XDG_SESSION_CLASS", "user".to_owned()),
            ("XDG_SESSION_TYPE", self.session_type.clone()),
        ];
        if !self.desktop.is_empty() {
            items.push(("XDG_SESSION_DESKTOP", self.desktop.clone()));
        }
        items
    }
}

/// How a prompt should be presented, mirroring PAM's message styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStyle {
    /// Expects a response that must not be echoed.
    Secret,
    /// Expects a response that may be echoed.
    Visible,
    /// Informational; no response expected.
    Info,
    /// Error text; no response expected.
    Error,
}

impl PromptStyle {
    /// Whether the greeter is expected to answer.
    pub fn expects_response(self) -> bool {
        matches!(self, Self::Secret | Self::Visible)
    }
}

/// Something the auth thread reports to the event loop.
#[derive(Debug)]
pub enum AuthEvent {
    /// PAM is asking something. Answer with [`AuthHandle::respond`].
    Prompt {
        id: u32,
        text: String,
        style: PromptStyle,
    },
    /// Authentication and account management both succeeded.
    Ok,
    /// The attempt failed and the thread is exiting.
    Failed(String),
    /// The user's session is running, as a child of the helper.
    ///
    /// Informational: wdm is not the session's parent and cannot `wait` on this
    /// pid. What it is for is the log line naming the process that now owns the
    /// display, which is the first thing anyone reads when a login goes dark.
    SessionStarted { pid: u32 },
    /// The user's session exited.
    ///
    /// `status` is the helper's rendering of the wait status, because everything
    /// wdm does with it is put it in a log line or a `last_error`.
    SessionEnded { status: String, ran_for: Duration },
    /// `pam_open_session`, the environment assembly or the fork failed, and the
    /// helper is exiting.
    SessionFailed(String),
}

/// The descriptor the helper expects its socket on.
///
/// Fixed rather than passed as an argument: `--pam-helper` deliberately takes
/// none, so a mode that needs a pre-arranged fd cannot be entered by accident
/// from a shell. The other end of this constant is `pamhelper::SOCKET_FD`; they
/// are separate because neither module should have to depend on the other to
/// know a number that is part of the exec contract.
const HELPER_FD: RawFd = 3;

/// wdm's end of the socket to one helper.
///
/// Shared between [`AuthHandle`] and the thread reading the socket, which is why
/// it exists as a type at all: the handle sends and the reader receives, both on
/// `&self`, so neither needs a lock.
struct Wire {
    sock: UnixDatagram,
    /// Set immediately before wdm shuts the socket down itself.
    ///
    /// EOF on the reader means either "wdm cancelled" or "the helper died", and
    /// the two need opposite responses: the first must be silent, because
    /// cancelling is routine and `login.rs` has already forgotten the attempt,
    /// while the second must be reported or the greeter waits forever for a
    /// verdict that is never coming. Nothing about the read itself distinguishes
    /// them, so the closing side says so.
    closing: AtomicBool,
}

impl Wire {
    /// Send one datagram, scrubbing the encoded copy afterwards.
    ///
    /// The scrub is for [`Msg::Response`]: `encode` copies the secret into a
    /// fresh `Vec`, and `Msg`'s own `Drop` knows nothing about that copy.
    fn send(&self, msg: &Msg) {
        let mut bytes = msg.encode();
        if let Err(e) = self.sock.send(&bytes) {
            // Nothing can be done here. If the helper has gone, the reader
            // thread's EOF is what reports it, through the one path that also
            // knows whether wdm asked for it.
            log::debug!("sending to the PAM helper: {e}");
        }
        bytes.zeroize();
    }

    /// Close wdm's end, which is how the helper is told to stop.
    ///
    /// `shutdown` rather than dropping the socket, because the reader thread
    /// holds the same `Wire`: this delivers EOF to *both* ends at once, so the
    /// helper unwinds PAM and the reader stops and reaps, from one call.
    fn close(&self) {
        // Before the shutdown, so the reader cannot observe EOF and read a stale
        // `false`.
        self.closing.store(true, Ordering::SeqCst);
        // Errors are expected and uninteresting: ENOTCONN means the helper
        // already went away, which is the state this was trying to reach.
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

/// Handle to a running authentication attempt.
///
/// Dropping this cancels the attempt: wdm's end of the socket is shut down, the
/// helper's next read hits EOF, and PAM unwinds by itself.
pub struct AuthHandle {
    wire: Arc<Wire>,
    /// The user being authenticated, for logging and for the session launch.
    username: String,
}

impl AuthHandle {
    /// Spawn a helper process to authenticate `username`.
    ///
    /// `tty` is set as `PAM_TTY` so modules that make policy decisions based on
    /// the terminal (`pam_access`, `pam_time`) see the VT wdm runs on, and
    /// `session` describes the seat to pam_systemd.
    pub fn start(
        username: &str,
        tty: &str,
        session: SessionDescription,
        events: calloop::channel::Sender<AuthEvent>,
    ) -> io::Result<Self> {
        let (ours, theirs) = seqpacket_pair()?;
        let child = spawn_helper(helper_command(), theirs)?;
        log::debug!("pam helper for {username} started as pid {}", child.id());

        let handle = Self::adopt(ours, Some(child), username, events);

        // Immediately, and before anything else can be sent: the helper reads
        // exactly one Start and refuses whatever else arrives first.
        handle.wire.send(&Msg::Start {
            username: username.to_owned(),
            tty: tty.to_owned(),
            seat: session.seat,
            vtnr: session.vtnr,
            session_type: session.session_type,
            desktop: session.desktop,
        });

        Ok(handle)
    }

    /// Take ownership of an already-connected socket and start reading it.
    ///
    /// Split out from [`Self::start`] so a test can drive the whole handle
    /// against a fake helper over a `socketpair`, with no PAM and no process.
    fn adopt(
        sock: UnixDatagram,
        child: Option<Child>,
        username: &str,
        events: calloop::channel::Sender<AuthEvent>,
    ) -> Self {
        let wire = Arc::new(Wire {
            sock,
            closing: AtomicBool::new(false),
        });

        let reader = Arc::clone(&wire);
        let name = username.to_owned();
        // Detached: it ends when the socket does, which is either when the
        // helper exits or when this handle is dropped. A failure to spawn it is
        // not fatal — the helper is already running and cancellation still works
        // — but nothing would ever be heard from it, so it is reported and the
        // attempt is failed rather than left silent.
        if let Err(e) = std::thread::Builder::new()
            .name(format!("pam-reader-{username}"))
            .spawn(move || pump(&reader, child, &events, &name))
        {
            log::error!("spawning the PAM reader thread for {username}: {e}");
            wire.close();
        }

        Self {
            wire,
            username: username.to_owned(),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Answer the prompt with the given id.
    pub fn respond(&self, id: u32, secret: String) {
        self.wire.send(&Msg::Response { id, secret });
    }

    /// Ask the helper to open the PAM session and run the user's session.
    ///
    /// Valid only after [`AuthEvent::Ok`], and — this is the part no type can
    /// express — only once wdm has released the display. The helper replies with
    /// [`AuthEvent::SessionStarted`] then [`AuthEvent::SessionEnded`], or with
    /// [`AuthEvent::SessionFailed`]. Sending this while the DRM device, the
    /// renderer or the seat are still held reintroduces the whole defect the
    /// helper exists to fix, because `pam_open_session` runs the moment it
    /// arrives. The ordering is enforced by the one call site,
    /// [`crate::login::Login::launch`], and by review.
    ///
    /// `session_type` and `desktop` are sent as their own fields as well as
    /// being derivable from `session`, because they go into PAM's *own*
    /// environment before `open_session` — where pam_systemd reads them — which
    /// is a different place from the launched child's environment.
    pub fn start_session(
        &self,
        session: &crate::sessions::Session,
        extra_env: Vec<(String, String)>,
        vt: u32,
    ) {
        self.wire.send(&Msg::Launch {
            session_type: session.xdg_session_type().to_owned(),
            desktop: session.xdg_session_desktop().to_owned(),
            session_id: session.id.clone(),
            session_name: session.name.clone(),
            session_exec: session.exec.clone(),
            extra_env,
            vt,
        });
    }

    /// Dismiss the helper once the session is over.
    ///
    /// The helper pairs `pam_close_session` itself, on the handle that opened
    /// the session, immediately after it reaps the session process — it has to,
    /// because it is the only process holding that handle. So this is not what
    /// runs it. What it does is release wdm's end: the reader thread's `recv`
    /// ends, it reaps the helper, and nothing is left as a zombie or as a socket
    /// wdm still believes in. Idempotent, and also reached through `Drop`.
    pub fn session_ended(&self) {
        self.wire.close();
    }
}

impl Drop for AuthHandle {
    fn drop(&mut self) {
        // Cancellation, and the reason `Login::reset` works. Idempotent, so a
        // handle dropped after `session_ended` is not a special case.
        self.wire.close();
    }
}

/// The reader thread: forward what the helper says, then reap it.
///
/// Reaping happens here rather than in [`AuthHandle`]'s `Drop` for two reasons.
/// It is the only place that knows the helper has actually gone — `Drop` runs on
/// the event loop thread, where a blocking `wait` would stall every other client
/// while PAM unwinds. And it cannot be skipped: every way the socket can end
/// arrives here, so there is no path that drops the handle without reaping.
fn pump(
    wire: &Wire,
    child: Option<Child>,
    events: &calloop::channel::Sender<AuthEvent>,
    username: &str,
) {
    let outcome = forward(wire, events);

    if let Some(mut child) = child {
        match child.wait() {
            Ok(status) => log::debug!("pam helper for {username} {status}"),
            Err(e) => log::error!("waiting on the PAM helper for {username}: {e}"),
        }
    }

    match outcome {
        // The helper said how it ended; nothing to add.
        Outcome::Reported => {}
        // wdm closed the socket. Cancelled, or the session ended.
        _ if wire.closing.load(Ordering::SeqCst) => {
            log::debug!("the PAM helper for {username} was dismissed");
        }
        // The helper vanished with the greeter still waiting. Saying nothing
        // would leave the login screen sitting on a prompt or a spinner for
        // ever, so the failure is invented here — it is the one place that can
        // tell the difference between a helper that died and one that was told
        // to stop.
        Outcome::BeforeVerdict => {
            log::error!("the PAM helper for {username} exited before answering");
            send(
                events,
                AuthEvent::Failed("the authentication helper exited".to_owned()),
            );
        }
        Outcome::AfterVerdict => {
            log::error!("the PAM helper for {username} exited after authenticating");
            send(
                events,
                AuthEvent::SessionFailed("the authentication helper exited".to_owned()),
            );
        }
    }
}

/// How the socket ended, from the point of view of what the greeter has heard.
enum Outcome {
    /// A terminal message crossed the wire; the greeter has been told.
    Reported,
    /// The socket ended with no verdict at all.
    BeforeVerdict,
    /// Authentication succeeded, then the socket ended without a session.
    AfterVerdict,
}

/// Read the socket until it ends, turning messages into events.
fn forward(wire: &Wire, events: &calloop::channel::Sender<AuthEvent>) -> Outcome {
    // Heap and allocated once: MAX_MESSAGE is 256 KiB, which does not belong on
    // a thread stack and does not belong in the per-message path either.
    let mut buf = vec![0u8; MAX_MESSAGE];
    let mut authenticated = false;

    loop {
        let n = match wire.sock.recv(&mut buf) {
            // Zero on SOCK_SEQPACKET is the peer closing. The codec never
            // produces an empty datagram, so nothing is confusable with it.
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::error!("reading the PAM socket: {e}");
                break;
            }
        };

        let Some(msg) = Msg::decode(&buf[..n]) else {
            // Both ends are the same binary, so this is a bug or an attack,
            // never version skew.
            log::error!("undecodable message of {n} bytes from the PAM helper");
            break;
        };

        // Borrowed, not destructured: `Msg` has a `Drop`.
        match &msg {
            Msg::Prompt { id, text, style } => send(
                events,
                AuthEvent::Prompt {
                    id: *id,
                    text: text.clone(),
                    style: *style,
                },
            ),
            Msg::Ok => {
                authenticated = true;
                send(events, AuthEvent::Ok);
            }
            Msg::Failed(reason) => {
                send(events, AuthEvent::Failed(reason.clone()));
                return Outcome::Reported;
            }
            Msg::SessionStarted { pid } => send(events, AuthEvent::SessionStarted { pid: *pid }),
            Msg::SessionEnded { status, ran_for_ms } => {
                send(
                    events,
                    AuthEvent::SessionEnded {
                        status: status.clone(),
                        ran_for: Duration::from_millis(*ran_for_ms),
                    },
                );
                // Terminal, and it has to be: the helper's very next acts are
                // pam_close_session and exit, which close the socket. Without
                // this the loop would fall out on EOF with `authenticated` set
                // and invent a SessionFailed for a session that ended normally —
                // which wdm would then show the next greeter as the reason the
                // login went wrong.
                return Outcome::Reported;
            }
            Msg::SessionFailed(reason) => {
                send(events, AuthEvent::SessionFailed(reason.clone()));
                return Outcome::Reported;
            }
            // wdm's own direction. A helper sending one of these is confused or
            // hostile, and either way there is nothing here that acts on it.
            other => log::debug!("ignoring {} from the PAM helper", name_of(other)),
        }
    }

    if authenticated {
        Outcome::AfterVerdict
    } else {
        Outcome::BeforeVerdict
    }
}

/// A message's variant name, for logs.
///
/// By hand rather than by `Debug`, which on [`Msg::Response`] would print the
/// secret. wdm never receives one, but a log line is not the place to rely on
/// that.
fn name_of(msg: &Msg) -> &'static str {
    match msg {
        Msg::Start { .. } => "start",
        Msg::Response { .. } => "response",
        Msg::Launch { .. } => "launch",
        Msg::Prompt { .. } => "prompt",
        Msg::Ok => "ok",
        Msg::Failed(_) => "failed",
        Msg::SessionStarted { .. } => "session_started",
        Msg::SessionFailed(_) => "session_failed",
        Msg::SessionEnded { .. } => "session_ended",
    }
}

/// A connected `SOCK_SEQPACKET` pair: wdm's end, then the helper's.
///
/// `SOCK_SEQPACKET` rather than `SOCK_DGRAM` for two properties the protocol
/// leans on: message boundaries, so there is no framing layer, and a connection,
/// so closing one end is observable as EOF at the other — which is the entire
/// cancellation mechanism.
fn seqpacket_pair() -> io::Result<(UnixDatagram, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: writes two descriptors into an array of two.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are fresh and owned by nothing else.
    unsafe {
        Ok((
            UnixDatagram::from_raw_fd(fds[0]),
            OwnedFd::from_raw_fd(fds[1]),
        ))
    }
}

/// The command that runs the helper: wdm's own binary, and one argument.
///
/// `/proc/self/exe` rather than `argv[0]` or a configured path: it is the
/// running binary whatever the working directory is, whatever `PATH` says, and
/// whether or not the file has since been replaced by a package upgrade.
fn helper_command() -> Command {
    let mut command = Command::new("/proc/self/exe");
    command.arg("--pam-helper");
    command
}

/// Spawn `command` with `theirs` as the helper's fd 3.
///
/// `command` is a parameter so a test can put a shell there and observe what the
/// child actually inherits; production always passes [`helper_command`].
fn spawn_helper(mut command: Command, theirs: OwnedFd) -> io::Result<Child> {
    // Captured by value before the closure, which may not allocate: `OwnedFd`'s
    // `Drop` would close the descriptor, and borrowing it into a `'static`
    // closure is not expressible anyway.
    let raw = theirs.as_raw_fd();

    // SAFETY: runs in the forked child; only libc calls on a `RawFd` captured
    // above, no allocation and no locks. PAM is not running in this process any
    // more, but wdm still has a renderer and an event loop with their own
    // threads, so the async-signal-safety rule is unchanged.
    unsafe {
        command.pre_exec(move || {
            // First, before anything else: calloop's signal source blocks
            // SIGTERM and SIGINT on wdm's thread, and a blocked mask survives
            // both fork and exec. A helper that inherits it cannot be killed
            // normally, and neither can anything it goes on to start.
            crate::supervise::unblock_signals()?;

            // dup2 clears FD_CLOEXEC on the new descriptor — except when the
            // two are already equal, in which case it is a no-op and the flag
            // the socketpair was created with survives into the exec, leaving
            // the helper with no socket at all. Hence both steps: the fd is
            // only sometimes already 3, so this fails only sometimes, which is
            // the worst kind.
            if raw != HELPER_FD && libc::dup2(raw, HELPER_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(HELPER_FD, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn();
    // Explicit, and after the spawn: the descriptor has to still be open while
    // the child is forked. Holding it any longer would keep the helper's end of
    // the socket alive in wdm, so the helper's peer would never see EOF and
    // cancellation would silently stop working.
    drop(theirs);
    child
}

/// The `KEY=VALUE` pairs to push into PAM's environment before `open_session`.
///
/// An empty value yields no pair at all rather than `KEY=`: a session with no
/// desktop must leave `XDG_SESSION_DESKTOP` *unset*, because an empty string is
/// a value logind will happily record and everything reading it will believe.
pub(crate) fn pam_session_env(session_type: &str, desktop: &str) -> Vec<String> {
    [
        ("XDG_SESSION_TYPE", session_type),
        ("XDG_SESSION_DESKTOP", desktop),
    ]
    .into_iter()
    .filter(|(_, value)| !value.is_empty())
    .map(|(key, value)| format!("{key}={value}"))
    .collect()
}

fn send(events: &calloop::channel::Sender<AuthEvent>, event: AuthEvent) {
    if events.send(event).is_err() {
        log::debug!("event loop is gone, dropping auth event");
    }
}

/// PAM's own message if it gave one, otherwise the error code's description.
///
/// The text reaches the greeter, so it must not leak whether an account exists:
/// PAM already conflates "no such user" and "wrong password" into
/// `Authentication failure`, and this preserves that.
pub(crate) fn describe(e: &pam_client2::Error) -> String {
    e.message()
        .map(str::to_owned)
        .unwrap_or_else(|| e.to_string())
}

pub(crate) fn os_to_string(s: &OsStr) -> Option<String> {
    match s.to_str() {
        Some(s) => Some(s.to_owned()),
        None => {
            log::warn!("dropping non-UTF-8 PAM environment entry");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Instant;

    use super::*;

    #[test]
    fn pam_session_env_pairs_both_variables() {
        // Order matters only for the log line, but pinning it keeps the failure
        // message stable when someone reorders the array.
        assert_eq!(
            pam_session_env("x11", "i3"),
            vec!["XDG_SESSION_TYPE=x11", "XDG_SESSION_DESKTOP=i3"]
        );
    }

    #[test]
    fn pam_session_env_omits_an_empty_desktop() {
        // Not `XDG_SESSION_DESKTOP=`: unset and empty are different to logind.
        assert_eq!(
            pam_session_env("wayland", ""),
            vec!["XDG_SESSION_TYPE=wayland"]
        );
    }

    #[test]
    fn pam_session_env_omits_an_empty_type() {
        assert_eq!(
            pam_session_env("", "sway"),
            vec!["XDG_SESSION_DESKTOP=sway"]
        );
        assert!(pam_session_env("", "").is_empty());
    }

    #[test]
    fn only_prompts_expect_responses() {
        assert!(PromptStyle::Secret.expects_response());
        assert!(PromptStyle::Visible.expects_response());
        assert!(!PromptStyle::Info.expects_response());
        assert!(!PromptStyle::Error.expects_response());
    }

    #[test]
    fn prompt_ids_are_never_reused() {
        // The whole point of the global counter: a late response from a
        // cancelled conversation must not match a prompt in the next one.
        let a = next_prompt_id();
        let b = next_prompt_id();
        assert_ne!(a, b);
        assert!(b > a);
    }

    #[test]
    fn a_message_name_never_contains_a_secret() {
        // name_of exists because Debug on a Response would print the password.
        // wdm never receives one, but the log line must not become the place
        // that discovers otherwise.
        assert_eq!(
            name_of(&Msg::Response {
                id: 1,
                secret: "hunter2".to_owned(),
            }),
            "response"
        );
    }

    /// The helper's end of a fresh pair, as something a test can talk over.
    fn fake_helper() -> (UnixDatagram, UnixDatagram) {
        let (ours, theirs) = seqpacket_pair().expect("socketpair");
        (ours, UnixDatagram::from(theirs))
    }

    /// Read one message from the helper's end, failing rather than hanging.
    fn expect(sock: &UnixDatagram, what: &str) -> Msg {
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = vec![0u8; MAX_MESSAGE];
        let n = sock
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("waiting for {what}: {e}"));
        assert_ne!(n, 0, "wdm closed the socket instead of sending {what}");
        Msg::decode(&buf[..n]).unwrap_or_else(|| panic!("undecodable {what}"))
    }

    /// Collects [`AuthEvent`]s off the calloop channel, the way `Wdm` does.
    ///
    /// The events have to be read through a real event loop rather than an
    /// `mpsc`: `AuthHandle::start` is handed a `calloop::channel::Sender`, and
    /// the point of the reader thread is that its output arrives where the
    /// compositor already dispatches.
    struct EventStream {
        event_loop: calloop::EventLoop<'static, VecDeque<AuthEvent>>,
        seen: VecDeque<AuthEvent>,
    }

    impl EventStream {
        fn new(channel: calloop::channel::Channel<AuthEvent>) -> Self {
            let event_loop = calloop::EventLoop::try_new().unwrap();
            event_loop
                .handle()
                .insert_source(channel, |event, _, seen: &mut VecDeque<AuthEvent>| {
                    if let calloop::channel::Event::Msg(event) = event {
                        seen.push_back(event);
                    }
                })
                .unwrap();
            Self {
                event_loop,
                seen: VecDeque::new(),
            }
        }

        fn next(&mut self, what: &str) -> AuthEvent {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.seen.is_empty() {
                assert!(Instant::now() < deadline, "{what} never arrived");
                self.event_loop
                    .dispatch(Duration::from_millis(20), &mut self.seen)
                    .unwrap();
            }
            self.seen.pop_front().expect("checked non-empty")
        }

        /// Nothing arrived within a short grace period.
        fn is_quiet(&mut self) -> bool {
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                self.event_loop
                    .dispatch(Duration::from_millis(20), &mut self.seen)
                    .unwrap();
                if !self.seen.is_empty() {
                    return false;
                }
            }
            true
        }
    }

    /// A session, as `login.rs` describes one.
    fn a_session() -> crate::sessions::Session {
        crate::sessions::Session {
            id: "sway.desktop".to_owned(),
            name: "Sway".to_owned(),
            exec: "sway".to_owned(),
            session_type: crate::sessions::SessionType::Wayland,
            path: std::path::PathBuf::from("/usr/share/wayland-sessions/sway.desktop"),
        }
    }

    /// prompt -> respond -> ok -> start_session -> started -> ended -> dismissed.
    ///
    /// The whole seam in one test, with no PAM and no process: what
    /// `AuthHandle` puts on the wire, and that what comes back reaches the event
    /// loop as the same `AuthEvent`s `login.rs` consumes.
    #[test]
    fn a_fake_helper_drives_the_conversation_end_to_end() {
        let (ours, helper) = fake_helper();
        let (events_tx, events_rx) = calloop::channel::channel();
        let mut events = EventStream::new(events_rx);

        let handle = AuthHandle::adopt(ours, None, "testuser", events_tx);
        assert_eq!(handle.username(), "testuser");

        // A prompt the greeter has to answer.
        helper
            .send(
                &Msg::Prompt {
                    id: 7,
                    text: "Password: ".to_owned(),
                    style: PromptStyle::Secret,
                }
                .encode(),
            )
            .unwrap();

        let AuthEvent::Prompt { id, text, style } = events.next("a prompt") else {
            panic!("expected a prompt first");
        };
        assert_eq!(id, 7);
        assert_eq!(text, "Password: ");
        assert_eq!(style, PromptStyle::Secret);

        handle.respond(id, "hunter2".to_owned());
        let response = expect(&helper, "the response");
        let Msg::Response { id, secret } = &response else {
            panic!("expected a response, got {}", name_of(&response));
        };
        assert_eq!(*id, 7);
        assert_eq!(secret, "hunter2");

        helper.send(&Msg::Ok.encode()).unwrap();
        assert!(matches!(events.next("the verdict"), AuthEvent::Ok));

        handle.start_session(
            &a_session(),
            vec![("LANG".to_owned(), "de_DE.UTF-8".to_owned())],
            7,
        );
        let launch = expect(&helper, "the launch");
        let Msg::Launch {
            session_type,
            desktop,
            session_id,
            session_name,
            session_exec,
            extra_env,
            vt,
        } = &launch
        else {
            panic!("expected a launch, got {}", name_of(&launch));
        };
        // Everything the helper needs to run the session, because it has no
        // access to wdm's configuration and cannot ask for any of it later.
        assert_eq!(session_type, "wayland");
        assert_eq!(desktop, "sway");
        assert_eq!(session_id, "sway.desktop");
        assert_eq!(session_name, "Sway");
        assert_eq!(session_exec, "sway");
        assert_eq!(
            extra_env,
            &vec![("LANG".to_owned(), "de_DE.UTF-8".to_owned())]
        );
        assert_eq!(*vt, 7);

        // The helper forks the session and says so. wdm is not its parent, so
        // this pid is for the log line and nothing else.
        helper
            .send(&Msg::SessionStarted { pid: 4321 }.encode())
            .unwrap();
        let AuthEvent::SessionStarted { pid } = events.next("the session starting") else {
            panic!("expected the session to start");
        };
        assert_eq!(pid, 4321);

        helper
            .send(
                &Msg::SessionEnded {
                    status: "exit status: 0".to_owned(),
                    ran_for_ms: 90_000,
                }
                .encode(),
            )
            .unwrap();
        let AuthEvent::SessionEnded { status, ran_for } = events.next("the session ending") else {
            panic!("expected the session to end");
        };
        assert_eq!(status, "exit status: 0");
        assert_eq!(ran_for, Duration::from_secs(90));

        // And wdm dismisses the helper, which has already paired
        // pam_close_session itself.
        handle.session_ended();
        let mut buf = [0u8; 64];
        helper
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(
            helper.recv(&mut buf).unwrap(),
            0,
            "session_ended must close the socket"
        );

        // A dismissed helper is silent: the reader thread knows wdm asked for
        // the EOF, so it must not invent a failure for an attempt that
        // succeeded.
        assert!(
            events.is_quiet(),
            "session_ended produced an event of its own"
        );
    }

    #[test]
    fn a_session_that_ended_normally_is_not_also_reported_as_a_failure() {
        // The helper's next acts after SessionEnded are pam_close_session and
        // exit, which close the socket. The reader thread sees EOF with the
        // attempt authenticated, and its rule for that is "the helper vanished
        // after authenticating" — SessionFailed. Without SessionEnded being
        // terminal, every successful login would be followed by a spurious
        // failure, and wdm would greet the user's next login with "could not
        // start session" as the reason their last one ended.
        let (ours, helper) = fake_helper();
        let (events_tx, events_rx) = calloop::channel::channel();
        let mut events = EventStream::new(events_rx);

        let _handle = AuthHandle::adopt(ours, None, "testuser", events_tx);
        helper.send(&Msg::Ok.encode()).unwrap();
        assert!(matches!(events.next("the verdict"), AuthEvent::Ok));

        helper
            .send(
                &Msg::SessionEnded {
                    status: "exit status: 0".to_owned(),
                    ran_for_ms: 1_000,
                }
                .encode(),
            )
            .unwrap();
        assert!(matches!(
            events.next("the session ending"),
            AuthEvent::SessionEnded { .. }
        ));

        // Exactly what the helper does next.
        drop(helper);

        assert!(
            events.is_quiet(),
            "a session that ended normally was also reported as failed"
        );
    }

    #[test]
    fn dropping_the_handle_closes_the_socket() {
        // The cancellation contract, unchanged from the mpsc version and now
        // resting on this: Login::reset() drops the handle, the helper's next
        // read hits EOF, and PAM unwinds by itself. Nothing else tells it to
        // stop.
        let (ours, helper) = fake_helper();
        let (events_tx, _events_rx) = calloop::channel::channel();

        let handle = AuthHandle::adopt(ours, None, "testuser", events_tx);
        drop(handle);

        helper
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 64];
        assert_eq!(
            helper.recv(&mut buf).unwrap(),
            0,
            "the peer must see EOF when the handle is dropped"
        );
    }

    #[test]
    fn a_helper_that_dies_before_a_verdict_fails_the_attempt() {
        // Without this the greeter sits on a prompt for ever: a helper that
        // segfaults, or one that cannot open /etc/pam.d/wdm and exits, says
        // nothing at all. The reader thread is the only place that can tell a
        // dead helper from a dismissed one, so it is where the failure is
        // invented.
        let (ours, helper) = fake_helper();
        let (events_tx, events_rx) = calloop::channel::channel();
        let mut events = EventStream::new(events_rx);

        let _handle = AuthHandle::adopt(ours, None, "testuser", events_tx);
        drop(helper);

        let AuthEvent::Failed(reason) = events.next("a failure") else {
            panic!("a helper that vanished must fail the attempt");
        };
        assert!(reason.contains("helper"), "{reason}");
    }

    #[test]
    fn a_helper_that_dies_after_authenticating_fails_the_session() {
        // The same hole one step later, and it needs a different event:
        // auth_failed after auth_ok is not something the protocol allows, and
        // the greeter is waiting on a launch rather than on a prompt.
        let (ours, helper) = fake_helper();
        let (events_tx, events_rx) = calloop::channel::channel();
        let mut events = EventStream::new(events_rx);

        let _handle = AuthHandle::adopt(ours, None, "testuser", events_tx);
        helper.send(&Msg::Ok.encode()).unwrap();
        assert!(matches!(events.next("the verdict"), AuthEvent::Ok));
        drop(helper);

        let AuthEvent::SessionFailed(reason) = events.next("a session failure") else {
            panic!("a helper that vanished after auth_ok must fail the session");
        };
        assert!(reason.contains("helper"), "{reason}");
    }

    #[test]
    fn the_helper_command_is_this_binary_and_nothing_else() {
        // `--pam-helper` is reachable only with a socket on fd 3, so the
        // argument list is the whole interface. /proc/self/exe rather than
        // argv[0] because the helper must be *this* binary whatever the working
        // directory is and whatever a package upgrade has since done to the
        // file on disk.
        let command = helper_command();
        assert_eq!(command.get_program(), "/proc/self/exe");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--pam-helper"]);
    }

    /// The signal mask a process has blocked, read from /proc as the child's
    /// parent cannot otherwise observe it.
    fn blocked_mask(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let line = status
            .lines()
            .find(|l| l.starts_with("SigBlk:"))
            .expect("SigBlk in /proc status");
        u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).unwrap()
    }

    #[test]
    fn the_helper_gets_the_socket_on_fd_3_with_no_signals_blocked() {
        // Both halves of the exec contract, and both fail silently in ways
        // nobody notices for a while. A helper that does not get the socket on
        // fd 3 prints to stderr and exits, so login simply stops working; a
        // helper that inherits calloop's blocked SIGTERM cannot be killed
        // normally, and neither can anything it goes on to start.
        //
        // The child is a shell rather than the real helper because the property
        // is a property of the pre_exec closure, which needs neither PAM nor a
        // compositor — and because the real helper would need /etc/pam.d/wdm to
        // get far enough to be observed.

        // The mask is process-global state a failing assertion must not leak:
        // under --test-threads=1 a block left in place outlives this test for
        // the whole run, and among the signals it blocks is SIGINT, which makes
        // Ctrl-C on the test binary inert.
        struct Mask(libc::sigset_t);
        impl Drop for Mask {
            fn drop(&mut self) {
                // SAFETY: restoring the set captured when the guard was built.
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
                }
            }
        }

        let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: valid pointers to sigset_t storage; sigemptyset initialises
        // `blocked` and pthread_sigmask initialises `previous` on success, both
        // of which are checked before either is read.
        let guard = unsafe {
            assert_eq!(libc::sigemptyset(blocked.as_mut_ptr()), 0);
            let mut blocked = blocked.assume_init();
            libc::sigaddset(&mut blocked, libc::SIGTERM);
            libc::sigaddset(&mut blocked, libc::SIGINT);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr()),
                0
            );
            Mask(previous.assume_init())
        };

        let (ours, theirs) = seqpacket_pair().unwrap();
        let mut command = Command::new("/bin/sh");
        // Writes to fd 3 and then stays alive, so its /proc entry can be read
        // while the descriptor it wrote on is proven to be the socket.
        command.args(["-c", "printf hello >&3; exec sleep 60"]);

        let mut child = spawn_helper(command, theirs).unwrap();
        let pid = child.id();

        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 64];
        let n = ours.recv(&mut buf).unwrap();
        let mask = blocked_mask(pid);

        // Whatever the assertions below decide, the sleep must not outlive the
        // test.
        let _ = child.kill();
        let _ = child.wait();
        drop(guard);

        assert_eq!(
            &buf[..n],
            b"hello",
            "the child did not receive the socket on fd {HELPER_FD}"
        );
        assert_eq!(
            mask, 0,
            "the helper inherited blocked signals: SigBlk {mask:016x}"
        );
    }
}
