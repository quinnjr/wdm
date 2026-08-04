//! `wdm --pam-helper` — the process that runs PAM, and nothing else.
//!
//! wdm's own process owns DRM, EGL, libinput and the libseat session, and runs
//! PAM on a spawned thread. Four defects follow from that, and none of them is
//! fixable by configuration:
//!
//! - A module that `fork`s inside `pam_sm_open_session` and then `exit`s rather
//!   than `exec`ing — `pam_kwallet5` does, on its error path — runs the graphics
//!   driver's `atexit` handlers in a child that inherited wdm's live EGL
//!   context. On NVIDIA that faults wdm's own channel and kills the compositor
//!   mid-login.
//! - `pam_loginuid` can never succeed: `proc_loginuid_write` returns `EPERM`
//!   unless the writing task is the one `/proc/self` names, which is the
//!   thread-group leader, and PAM runs on a spawned thread.
//! - `pam_systemd` cannot raise ambient capabilities, `prctl` being per-thread.
//! - `pam_keyinit`, `pam_selinux` and `pam_namespace` set per-thread or
//!   per-process state on the PAM thread, and `fork()` from `main` copies only
//!   the calling thread.
//!
//! This module is the other end of the fix. wdm re-`exec`s its own binary with
//! a `SOCK_SEQPACKET` socket on fd 3; the fresh address space has never loaded
//! a graphics driver, so there are no `atexit` handlers and no EGL state to
//! corrupt, whatever a module does. The helper is single-threaded, which is what
//! makes it the thread-group leader `pam_loginuid` needs and what makes the
//! eventual `fork` for the session happen from a process with no other threads.
//!
//! # Reaching it
//!
//! `wdm --pam-helper`, with the socket on fd 3 and no other arguments. It is
//! undocumented in `wdm(1)` deliberately: it is an implementation detail with a
//! file descriptor for an interface, not a supported command, and a mode that
//! needs a pre-arranged fd cannot be entered by accident from a shell.
//!
//! # Cancellation
//!
//! The same contract as the thread it replaces, by a different mechanism. wdm
//! drops its end, the helper's next read hits EOF, the conversation returns
//! `CONV_ERR`, and PAM unwinds by itself. No locks, no signals, no shared state.
//!
//! # The helper is the session's parent
//!
//! On [`Msg::Launch`] the helper runs `pam_open_session`, assembles the
//! environment with [`Launch::build`], and **forks** the user's session. It does
//! not `exec` it: `pam_open_session` and `pam_close_session` must be paired on
//! one handle, so the helper has to outlive the session to close it. So it
//! `waitpid`s, reports [`Msg::SessionEnded`], closes the PAM session and exits.
//!
//! Forking here rather than in wdm is what makes `pam_open_session` mean
//! anything. `loginuid`, the session keyring, the mount namespace and ambient
//! capabilities are all set on the process that ran it, and `fork` copies them
//! to the child; a session forked from wdm instead descends from a process that
//! never ran `open_session` and inherits none of it.
//!
//! The fork is also from a process with exactly one thread, which is why
//! [`Launch::spawn`]'s `pre_exec` closure is safe rather than merely careful.
//! The rule it is written around — no allocation, no locks, `libc` only — is
//! kept regardless; see the comments there.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::process::{Child, ExitCode};
use std::time::{Duration, Instant};

use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use zeroize::Zeroize;

use crate::auth::{
    Phase, PromptStyle, RESPONSE_TIMEOUT, SERVICE, SessionDescription, next_prompt_id,
    notice_to_greeter, os_to_string, pam_session_env, to_greeter,
};
use crate::pamwire::{MAX_MESSAGE, Msg};
use crate::session::Launch;
use crate::sessions::{Session, SessionType};

/// The descriptor wdm arranges for the socket before `exec`.
///
/// Not configurable and not an argument: a fixed number is the whole point —
/// the helper takes no arguments at all, so there is nothing to get wrong and
/// nothing a shell could supply.
const SOCKET_FD: RawFd = 3;

/// Entry point for `wdm --pam-helper`.
///
/// The exit code says whether the helper could speak the protocol, not whether
/// the user authenticated: a refused password is a successful conversation and
/// is reported as [`Msg::Failed`] on the socket, where wdm can show it to the
/// greeter. Only a socket wdm cannot talk to is a failure of the helper itself.
pub fn run() -> ExitCode {
    let wire = match Wire::from_fd(SOCKET_FD) {
        Ok(wire) => wire,
        Err(e) => {
            // Not a log line: if this fails there may be no wdm listening and no
            // journal configured, and the operator who ran it by hand deserves
            // to be told what it wanted.
            eprintln!("wdm --pam-helper: fd {SOCKET_FD} is not a usable socket: {e}");
            return ExitCode::FAILURE;
        }
    };

    let start = match wire.recv(None) {
        Recv::Msg(msg) => match Start::from_msg(&msg) {
            Some(start) => start,
            None => {
                log::error!("first message on the PAM socket was not a Start");
                return ExitCode::FAILURE;
            }
        },
        Recv::Closed => {
            log::error!("the PAM socket closed before the first message");
            return ExitCode::FAILURE;
        }
        // Unreachable with no timeout, but a `_` arm here would hide a future
        // one.
        Recv::Timeout => return ExitCode::FAILURE,
    };

    log::debug!(
        "pam helper started for {:?} on {}",
        start.username,
        start.tty
    );

    let conv = WireConversation {
        wire: &wire,
        timeout: RESPONSE_TIMEOUT,
        username: &start.username,
        authenticated: false,
    };

    let context = match Context::new(SERVICE, Some(&start.username), conv) {
        Ok(context) => context,
        Err(e) => {
            // Almost always a missing /etc/pam.d/wdm. Say so, because the
            // generic PAM message is unhelpful.
            log::error!("opening PAM context for service {SERVICE}: {e}");
            // Phase::Start, not Phase::Authenticate, even though this is a
            // pre-authentication path: pam_start records the username without
            // resolving it, so the text is the same for every name typed and is
            // a fact about /etc/pam.d/wdm rather than about any account.
            wire.send(&Msg::Failed(to_greeter(Phase::Start, &start.username, &e)));
            return ExitCode::SUCCESS;
        }
    };

    let mut pam = LibPam {
        context,
        start: &start,
    };
    let mut spawner = SessionSpawn {
        username: &start.username,
    };

    serve(&wire, &mut pam, &mut spawner);
    ExitCode::SUCCESS
}

/// What [`Msg::Start`] carried, in the shape the rest of this module wants.
///
/// Copied out of the message rather than borrowed from it because `Msg` has a
/// `Drop` and cannot be destructured by value.
struct Start {
    username: String,
    tty: String,
    session: SessionDescription,
}

impl Start {
    fn from_msg(msg: &Msg) -> Option<Self> {
        let Msg::Start {
            username,
            tty,
            seat,
            vtnr,
            session_type,
            desktop,
        } = msg
        else {
            return None;
        };
        Some(Self {
            username: username.clone(),
            tty: tty.clone(),
            session: SessionDescription {
                seat: seat.clone(),
                vtnr: *vtnr,
                session_type: session_type.clone(),
                desktop: desktop.clone(),
            },
        })
    }
}

/// What [`Msg::Launch`] carried, in the shape the rest of this module wants.
///
/// Copied out of the message for the same reason [`Start`] is: `Msg` has a
/// `Drop` and cannot be destructured by value.
struct LaunchArgs {
    /// What goes into PAM's own environment before `open_session`. Derivable
    /// from `session`, and sent separately because that is a different
    /// environment from the launched child's — see [`Msg::Launch`].
    session_type: String,
    desktop: String,
    session: Session,
    extra_env: Vec<(String, String)>,
    vt: u32,
}

impl LaunchArgs {
    fn from_msg(msg: &Msg) -> Option<Self> {
        let Msg::Launch {
            session_type,
            desktop,
            session_id,
            session_name,
            session_exec,
            extra_env,
            vt,
        } = msg
        else {
            return None;
        };
        Some(Self {
            session_type: session_type.clone(),
            desktop: desktop.clone(),
            session: Session {
                id: session_id.clone(),
                name: session_name.clone(),
                exec: session_exec.clone(),
                session_type: SessionType::from_xdg(session_type),
                // Diagnostics only, and the helper has no desktop file to name:
                // the entry was read and validated in wdm, and only the fields
                // that decide what runs crossed the socket.
                path: PathBuf::new(),
            },
            extra_env: extra_env.clone(),
            vt: *vt,
        })
    }
}

/// What `pam_open_session` produced, or the text explaining why it did not.
type Opened = Result<Vec<(String, String)>, String>;

/// A task's threading and identity, as `/proc/<pid>/status` reports them.
///
/// These are the two facts the whole helper is built to arrange. The kernel's
/// `proc_loginuid_write` refuses any write where `current` is not the task the
/// opened path names, and `/proc/self` — the path `pam_loginuid` opens — always
/// names the thread-group leader, so a write can only ever succeed from a
/// leader. Single-threadedness is the other half: `fork` copies only the calling
/// thread, and `prctl` is per-thread, so `pam_keyinit`, `pam_namespace`,
/// `pam_selinux` and `pam_systemd`'s ambient capabilities reach the session only
/// if there is no other thread for them to land on.
///
/// Both are one `thread::spawn` away from being lost, and losing them is silent:
/// PAM logs a line nobody reads and the login still succeeds. Hence a check at
/// the moment it matters, and a test that spawns the real helper and reads this
/// same file out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskShape {
    threads: u32,
    pid: i32,
    tgid: i32,
}

impl TaskShape {
    /// Read the shape of the running process.
    fn of_self() -> io::Result<Self> {
        let status = std::fs::read_to_string("/proc/self/status")?;
        Self::parse(&status).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "/proc/self/status is missing Threads, Pid or Tgid",
            )
        })
    }

    /// Pull `Threads`, `Pid` and `Tgid` out of a `/proc/<pid>/status`.
    ///
    /// Separate from the read, and taking the text rather than a pid, because
    /// the test that inspects a *spawned* helper reads the same file for a
    /// different process and must not re-implement the parse to do it.
    fn parse(status: &str) -> Option<Self> {
        fn field(status: &str, name: &str) -> Option<i64> {
            status.lines().find_map(|line| {
                // The colon is part of the key: `Pid:` must not match `PPid:`,
                // and `NSpid:` must not match either.
                line.strip_prefix(name)?.trim().parse().ok()
            })
        }

        Some(Self {
            threads: u32::try_from(field(status, "Threads:")?).ok()?,
            pid: i32::try_from(field(status, "Pid:")?).ok()?,
            tgid: i32::try_from(field(status, "Tgid:")?).ok()?,
        })
    }

    /// Is this the shape PAM has to run in, and if not, why not?
    ///
    /// The message is the whole value: "PAM ran in the wrong shape" is not
    /// actionable, "this is thread N of process M" names the `thread::spawn`
    /// that has to go.
    fn single_threaded_leader(self) -> Result<(), String> {
        if self.pid != self.tgid {
            return Err(format!(
                "this is thread {} of process {}, and /proc/self names the leader",
                self.pid, self.tgid
            ));
        }
        if self.threads != 1 {
            return Err(format!(
                "this process has {} threads, and fork copies only the calling one",
                self.threads
            ));
        }
        Ok(())
    }
}

/// Complain if the process is not shaped the way `pam_open_session` needs.
///
/// A log line and not a refusal: every one of the four defects this shape fixes
/// is a degradation, not a security hole, and failing a login over a lost
/// `loginuid` would be a worse outcome than the lost `loginuid`. What matters is
/// that it stops being *silent* — the kernel returns `EPERM` to `pam_loginuid`,
/// which logs `set_loginuid failed` and carries on, and that line is
/// indistinguishable from a dozen other benign ones.
fn warn_unless_pam_can_take_effect() {
    match TaskShape::of_self() {
        Ok(shape) => {
            if let Err(why) = shape.single_threaded_leader() {
                log::error!(
                    "the PAM helper is not a single-threaded thread-group leader ({why}); \
                     pam_loginuid, pam_keyinit, pam_namespace and ambient capabilities \
                     will not reach the session"
                );
            }
        }
        // Not an error: a kernel or a container without a readable /proc is a
        // legitimate configuration, and this check is a diagnostic.
        Err(e) => log::debug!("could not read /proc/self/status: {e}"),
    }
}

/// Forking the user's session, behind a seam.
///
/// The real implementation drops privileges, which needs `CAP_SETGID` for
/// `setgroups`, so the whole of [`run_session`] would be unreachable in a test
/// suite that must not run as root — and [`run_session`] is where the message
/// sequence lives: that `SessionStarted` carries the pid of a process that
/// exists, that `SessionEnded` follows it and only it, and that a failed launch
/// produces neither. With the seam, a test spawns an ordinary child and asserts
/// all of that.
///
/// What the seam cannot check is the privilege drop itself, which is
/// [`Launch::spawn`]'s and is held by its own tests and by review.
trait Spawn {
    fn spawn(
        &mut self,
        launch: &LaunchArgs,
        pam_env: Vec<(String, String)>,
    ) -> Result<Child, String>;
}

/// The real thing: assemble the environment and fork the session.
struct SessionSpawn<'a> {
    username: &'a str,
}

impl Spawn for SessionSpawn<'_> {
    fn spawn(
        &mut self,
        launch: &LaunchArgs,
        pam_env: Vec<(String, String)>,
    ) -> Result<Child, String> {
        // build rather than validate-then-build at the call site: build
        // re-validates, which is deliberate. wdm checked the account before it
        // released the display, but this is the process that is about to run
        // setuid, and a check the exec path does not make is one that can be
        // routed around.
        let launch = Launch::build(
            &launch.session,
            self.username,
            launch.vt,
            pam_env,
            launch.extra_env.clone(),
        )
        .map_err(|e| e.to_string())?;

        log::debug!("launching {:?}'s session", launch.username());
        launch.spawn().map_err(|e| e.to_string())
    }
}

/// The PAM half of the helper, so the message sequencing can be tested.
///
/// The alternative — driving the real [`LibPam`] against a service that always
/// succeeds — needs a `pam_permit` stack installed as `/etc/pam.d/<something>`,
/// and writing into `/etc/pam.d` needs root, which the test suite does not have
/// and must not need. With no such file libpam falls through to
/// `/etc/pam.d/other`, which is `pam_deny` on every distribution wdm targets, so
/// the success path would be untestable and the sequencing after it — that
/// `open_session` happens only after [`Msg::Launch`], and that a closed socket
/// still reaches `pam_close_session` — would have no coverage at all. Hence the
/// seam: [`serve`] owns the order, and it is the order that is being asserted.
///
/// What this trait cannot check is that [`LibPam`] calls PAM correctly. That is
/// held by review against [`crate::auth::run`], which does the same sequence and
/// is the code currently in production.
trait Pam {
    /// `pam_start` through `pam_acct_mgmt`, inclusive.
    fn authenticate(&mut self) -> Result<(), String>;

    /// Open the session, run `hold` while it is open, then close it.
    ///
    /// A callback rather than a returned guard because pam_client2's `Session`
    /// borrows the `Context` it came from, which no struct field can hold
    /// alongside it — and because the pairing of `pam_open_session` with
    /// `pam_close_session` on one handle is the reason this process exists, so
    /// it is worth expressing as a scope rather than as a rule to remember.
    fn with_session(&mut self, session_type: &str, desktop: &str, hold: &mut dyn FnMut(Opened));
}

/// The protocol, with PAM behind [`Pam`] and the fork behind [`Spawn`].
///
/// Every exit from here is deliberate: an authentication failure is reported and
/// the helper stops, a closed socket before [`Msg::Launch`] is a cancellation
/// and the helper stops, and a session that opened is held until the session
/// process exits so that `pam_close_session` runs on the handle that opened it.
fn serve(wire: &Wire, pam: &mut dyn Pam, spawner: &mut dyn Spawn) {
    if let Err(reason) = pam.authenticate() {
        wire.send(&Msg::Failed(reason));
        return;
    }

    wire.send(&Msg::Ok);

    // Wait for the greeter's choice, which wdm forwards only once it has
    // released the display. A closed socket here is cancellation, and returning
    // runs pam_end through the Context's Drop.
    let launch = loop {
        // Bound rather than matched in place: `Msg` has a `Drop`, so its fields
        // can only be borrowed out of it.
        let msg = match wire.recv(None) {
            Recv::Msg(msg) => msg,
            Recv::Closed | Recv::Timeout => {
                log::debug!("the attempt was cancelled before launch");
                return;
            }
        };
        if let Some(launch) = LaunchArgs::from_msg(&msg) {
            break launch;
        }
        log::debug!("ignoring {} before launch", name_of(&msg));
    };

    pam.with_session(
        &launch.session_type,
        &launch.desktop,
        &mut |opened| match opened {
            Ok(env) => {
                log::debug!("PAM session opened with {} environment entries", env.len());
                run_session(wire, spawner, &launch, env);
            }
            Err(reason) => {
                log::error!("opening the PAM session failed: {reason}");
                wire.send(&Msg::SessionFailed(reason));
            }
        },
    );
}

/// Fork the user's session and stay for its whole life.
///
/// Runs inside [`Pam::with_session`]'s callback, so returning from it is what
/// closes the PAM session — which is why the `waitpid` is here and not anywhere
/// that could be skipped. wdm is not the session's parent and cannot do this.
///
/// Nothing here reads the socket. A wdm that vanishes mid-session is not a
/// reason to kill the user's session, and the sends failing is enough: the
/// helper still reaps its child and still closes the PAM session on the way out.
fn run_session(
    wire: &Wire,
    spawner: &mut dyn Spawn,
    launch: &LaunchArgs,
    pam_env: Vec<(String, String)>,
) {
    let mut child = match spawner.spawn(launch, pam_env) {
        Ok(child) => child,
        Err(reason) => {
            // The display is already gone by now, so there is no greeter to show
            // this to. wdm records it and the next greeter displays it as
            // `last_error`, which is the price of not opening a PAM session
            // while wdm still holds the GPU.
            log::error!("launching the session: {reason}");
            wire.send(&Msg::SessionFailed(reason));
            return;
        }
    };

    let pid = child.id();
    log::info!("session running as pid {pid}");
    wire.send(&Msg::SessionStarted { pid });

    let started = Instant::now();
    let status = match child.wait() {
        Ok(status) => status.to_string(),
        // Not a SessionFailed: the session did start, and wdm has already been
        // told so. What failed is this process's ability to describe how it
        // ended, which is a different and much smaller thing.
        Err(e) => {
            log::error!("waiting for the session: {e}");
            format!("could not be waited for: {e}")
        }
    };
    let ran_for = started.elapsed();

    log::info!("session pid {pid} ended after {ran_for:?}: {status}");
    wire.send(&Msg::SessionEnded {
        status,
        // Saturating rather than `as`: a session that ran for 584 million years
        // is impossible, and a truncating cast that turned it into a small
        // number would make wdm report it as a session that died instantly.
        ran_for_ms: u64::try_from(ran_for.as_millis()).unwrap_or(u64::MAX),
    });
}

/// A message's variant name, for logs.
///
/// By hand rather than by `Debug`, because `Debug` on [`Msg::Response`] would
/// print the secret.
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

/// The real thing: [`crate::auth::run`]'s PAM sequence, out of process.
///
/// Any divergence from that function is a bug in one of the two. It is
/// deliberately a transcription and not a refactor of it — the thread stays in
/// production until the whole change lands, and a shared implementation would
/// have to be shaped for both callers before either had proved itself.
///
/// Concrete in its conversation rather than generic over `ConversationHandler`,
/// because it has to reach back into that conversation and tell it
/// authentication has succeeded — see [`WireConversation::authenticated`]. There
/// was never a second implementation: the seam that tests use is [`Pam`].
struct LibPam<'a> {
    context: Context<WireConversation<'a>>,
    start: &'a Start,
}

impl Pam for LibPam<'_> {
    fn authenticate(&mut self) -> Result<(), String> {
        if let Err(e) = self.context.set_tty(Some(&self.start.tty)) {
            // Not fatal: modules that care about the tty are the exception.
            log::warn!("setting PAM_TTY to {}: {e}", self.start.tty);
        }

        // pam_systemd reads these from the PAM environment, not from the
        // child's. Without them it registers the logind session as Type=tty
        // with no desktop, so `loginctl` misreports it and anything keying off
        // sd_session_get_type — screen lockers, logind's own idle handling —
        // sees a tty session driving a Wayland compositor.
        for (key, value) in self.start.session.pam_items() {
            if let Err(e) = self.context.putenv(format!("{key}={value}").as_str()) {
                log::warn!("setting {key} for PAM: {e}");
            }
        }

        // DISALLOW_NULL_AUTHTOK: an account with an empty password must not be
        // loginable from the greeter.
        if let Err(e) = self.context.authenticate(Flag::DISALLOW_NULL_AUTHTOK) {
            // Phase::Authenticate is the one phase whose text the greeter never
            // sees, or the login screen is an account oracle; the real cause is
            // logged inside. See `auth::AUTH_FAILED`, and
            // `tests::an_authentication_failure_reaches_wdm_as_one_fixed_string`
            // below, which is what stops this reverting to PAM's own words with
            // a green suite.
            return Err(to_greeter(Phase::Authenticate, &self.start.username, &e));
        }

        // Past here a credential has been proven, so PAM modules may speak to
        // the user in their own words again — notably the expired-password
        // change below, whose whole job is to say what is wrong with the new
        // password.
        self.context.conversation_mut().authenticated = true;

        match self.context.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ErrorCode::NEW_AUTHTOK_REQD => {
                // The password is expired. PAM will not let the account in
                // until it is changed, and the change has to run through the
                // same conversation so the greeter can display its prompts.
                // Without this a user with an expired password can never log in.
                log::info!(
                    "{:?} must change their password before logging in",
                    self.start.username
                );
                self.context
                    .chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK)
                    .map_err(|e| to_greeter(Phase::Chauthtok, &self.start.username, &e))
            }
            Err(e) => Err(to_greeter(Phase::AcctMgmt, &self.start.username, &e)),
        }
    }

    fn with_session(&mut self, session_type: &str, desktop: &str, hold: &mut dyn FnMut(Opened)) {
        // The greeter's choice supersedes the defaults set before
        // authentication. This must land before open_session: pam_systemd
        // registers the logind session there, and a session registered with the
        // wrong type misleads everything keying off sd_session_get_type.
        for pair in pam_session_env(session_type, desktop) {
            if let Err(e) = self.context.putenv(pair.as_str()) {
                // Not fatal: a cosmetic environment variable must not block the
                // handoff. But it is not cosmetic to logind — open_session
                // below registers the session with whatever survived, so say
                // which variable was lost and what the machine will believe
                // instead.
                log::error!(
                    "putenv {pair} for {} failed: {e}; \
                     the logind session will be registered with the wrong type",
                    self.start.username
                );
            }
        }

        // Checked here rather than at startup because here is the moment it has
        // to hold: everything the shape buys — loginuid, the session keyring,
        // the mount namespace, ambient capabilities — is set by the modules
        // `open_session` is about to run, on whichever task runs them.
        warn_unless_pam_can_take_effect();

        match self.context.open_session(Flag::NONE) {
            Ok(session) => {
                let env = session
                    .envlist()
                    .iter_tuples()
                    .filter_map(|(key, value)| Some((os_to_string(key)?, os_to_string(value)?)))
                    .collect();
                hold(Ok(env));
                // Explicit, because the whole reason this process outlives the
                // exec is that pam_open_session and pam_close_session must be
                // paired on one handle.
                drop(session);
            }
            Err(e) => hold(Err(to_greeter(
                Phase::OpenSession,
                &self.start.username,
                &e,
            ))),
        }
    }
}

/// The socket, and the only I/O policy in this module.
struct Wire {
    sock: UnixDatagram,
}

/// What came off the socket.
enum Recv {
    Msg(Msg),
    /// The deadline passed with nothing readable.
    Timeout,
    /// EOF, an I/O error, or a datagram that did not decode. All three mean the
    /// same thing to every caller here — there is no useful conversation left —
    /// and collapsing them keeps that from being re-decided at each call site.
    Closed,
}

impl Wire {
    /// Adopt an already-open descriptor.
    ///
    /// Checked with `fcntl` first: `from_raw_fd` on a closed descriptor would
    /// produce a `UnixDatagram` that fails on every operation and, worse, would
    /// close whatever later landed on that number.
    fn from_fd(fd: RawFd) -> io::Result<Self> {
        // SAFETY: F_GETFD reads a flag word and touches no memory of ours.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is open, and nothing else in this process has
        // taken ownership of it — the helper does nothing before this.
        Ok(Self {
            sock: unsafe { UnixDatagram::from_raw_fd(fd) },
        })
    }

    #[cfg(test)]
    fn from_socket(sock: UnixDatagram) -> Self {
        Self { sock }
    }

    /// Send one datagram, or log why not.
    ///
    /// Nothing here can act on a send failure: the peer is wdm, and if it has
    /// gone the next `recv` reports it as [`Recv::Closed`] and the helper
    /// unwinds through the path it already has for cancellation.
    fn send(&self, msg: &Msg) {
        if let Err(e) = self.sock.send(&msg.encode()) {
            log::debug!("sending {} to wdm: {e}", name_of(msg));
        }
    }

    /// Receive one datagram, waiting at most `timeout`.
    fn recv(&self, timeout: Option<Duration>) -> Recv {
        // Zero would mean "no timeout" to set_read_timeout, which is the
        // opposite of what a caller passing an exhausted deadline means.
        if timeout.is_some_and(|t| t.is_zero()) {
            return Recv::Timeout;
        }
        if let Err(e) = self.sock.set_read_timeout(timeout) {
            log::error!("setting the PAM socket timeout: {e}");
            return Recv::Closed;
        }

        // Heap, not stack: MAX_MESSAGE is 256 KiB and this runs on whatever
        // stack the exec gave us.
        let mut buf = vec![0u8; MAX_MESSAGE];
        let received = self.sock.recv(&mut buf);

        let decoded = match received {
            // A zero-length read on SOCK_SEQPACKET is the peer closing. The
            // codec never produces an empty datagram, so there is nothing this
            // could be confused with.
            Ok(0) => {
                log::debug!("wdm closed the PAM socket");
                Recv::Closed
            }
            Ok(n) => match Msg::decode(&buf[..n]) {
                Some(msg) => Recv::Msg(msg),
                None => {
                    // Both ends are the same binary, so this is a bug or an
                    // attack, never version skew.
                    log::error!("undecodable message of {n} bytes on the PAM socket");
                    Recv::Closed
                }
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Recv::Timeout
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // A signal, not an end. The caller owns the deadline, so
                // reporting a timeout lets it decide whether any time is left
                // rather than looping here without one.
                Recv::Timeout
            }
            Err(e) => {
                log::error!("reading the PAM socket: {e}");
                Recv::Closed
            }
        };

        // The buffer held a password if that was a Response. `Msg`'s Drop
        // scrubs the String it decoded into; this is the copy the kernel wrote,
        // which nothing else would ever touch again.
        buf.zeroize();

        decoded
    }
}

/// Bridges libpam's blocking conversation to the socket.
///
/// The same shape as [`crate::auth`]'s `ChannelConversation`, with a socket
/// where that has two channels. Notably the same in one place that matters: the
/// deadline is computed once, before the loop, so a peer sending stale responses
/// cannot extend it.
struct WireConversation<'a> {
    wire: &'a Wire,
    /// Always [`RESPONSE_TIMEOUT`] in production; a field so tests can reach the
    /// timeout path, which at half an hour is otherwise unreachable.
    timeout: Duration,
    /// Whose login this is, for the journal line that replaces a suppressed
    /// notice. Never sent on the wire — the greeter supplied it.
    username: &'a str,
    /// Whether `pam_authenticate` has returned success, set by [`LibPam`].
    ///
    /// Gates [`Self::tell`]: until it is set, a module's `text_info` and
    /// `error_msg` reach the greeter as a fixed string instead of their own
    /// words. See [`notice_to_greeter`] for why that channel is the sharper
    /// oracle of the two. Prompts are never gated — a question that is not shown
    /// cannot be answered, and every one of them is a question wdm asked for.
    authenticated: bool,
}

impl WireConversation<'_> {
    /// Emit a prompt and block until the matching response arrives.
    fn ask(&mut self, prompt: &CStr, style: PromptStyle) -> Result<CString, ErrorCode> {
        let id = next_prompt_id();
        // PAM messages come from modules and are not guaranteed UTF-8.
        self.emit(id, prompt.to_string_lossy().into_owned(), style);

        let deadline = Instant::now() + self.timeout;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = match self.wire.recv(Some(remaining)) {
                Recv::Msg(msg) => msg,
                Recv::Timeout => {
                    log::info!("no response to prompt {id} within {:?}", self.timeout);

                    // Say so before failing, and say it as an `error` prompt.
                    //
                    // PAM logs "conversation failed" to the journal and tells
                    // the greeter nothing, so without this the attempt ends as
                    // an unexplained `auth_failed` — which is precisely the
                    // shape a greeter treats as a mistyped password and retries
                    // on its own. That retry re-arms the same timeout, and the
                    // pair spins: one `pam_faillock` entry per timeout until the
                    // account locks, with nobody at the keyboard.
                    //
                    // An `error` prompt is how a greeter learns that PAM
                    // explained itself: it lands in `Model::push_notice`, which
                    // sets `blocked`, which is what `Model::should_auto_retry`
                    // consults. So this single event both tells the user why
                    // their form reset and stops the loop.
                    self.emit(
                        next_prompt_id(),
                        "The login attempt timed out waiting for a response.".to_owned(),
                        PromptStyle::Error,
                    );

                    return Err(ErrorCode::CONV_ERR);
                }
                // wdm dropped its end: cancelled, or the greeter died.
                Recv::Closed => return Err(ErrorCode::CONV_ERR),
            };

            let Msg::Response {
                id: answered,
                secret,
            } = &msg
            else {
                log::debug!("ignoring {} while a prompt is outstanding", name_of(&msg));
                continue;
            };

            if *answered != id {
                // A response the greeter sent for a prompt that has already been
                // superseded. The protocol raises stale_prompt for this, but a
                // race can still deliver one legitimately, so drop it rather
                // than failing the whole attempt. Drop zeroizes it.
                log::debug!("discarding response for stale prompt {answered}");
                continue;
            }

            // CString rejects interior NUL, which cannot be part of a password
            // PAM could ever verify.
            //
            // The String is zeroized when `msg` drops. The CString this makes is
            // not, and neither is the copy libpam takes of it, so the plaintext
            // survives in freed heap until something reuses it. That is the same
            // accepted ceiling `auth.rs` documents: pam_authenticate frees the
            // answer itself and there is no hook to scrub it first.
            return CString::new(secret.as_bytes()).map_err(|_| {
                log::info!("response to prompt {id} contained a NUL byte");
                ErrorCode::CONV_ERR
            });
        }
    }

    /// Emit a message the greeter is not expected to answer.
    ///
    /// The style is preserved even when the text is not: `Error` is what sets
    /// `blocked` in the greeter's model and stops it auto-retrying, so replacing
    /// an error with an info would restore the retry spin.
    fn tell(&mut self, msg: &CStr, style: PromptStyle) {
        // PAM messages come from modules and are not guaranteed UTF-8.
        let text = msg.to_string_lossy().into_owned();
        let text = if self.authenticated {
            text
        } else {
            notice_to_greeter(self.username, &text)
        };
        self.emit(next_prompt_id(), text, style);
    }

    fn emit(&self, id: u32, text: String, style: PromptStyle) {
        // The prompt text, never a response: this is what PAM asked, and the
        // answers are the one thing here that must not reach a log.
        log::debug!("prompt {id} ({style:?}): {text:?}");
        self.wire.send(&Msg::Prompt { id, text, style });
    }
}

impl ConversationHandler for WireConversation<'_> {
    fn prompt_echo_on(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(prompt, PromptStyle::Visible)
    }

    fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(prompt, PromptStyle::Secret)
    }

    fn text_info(&mut self, msg: &CStr) {
        self.tell(msg, PromptStyle::Info);
    }

    fn error_msg(&mut self, msg: &CStr) {
        self.tell(msg, PromptStyle::Error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connected `SOCK_SEQPACKET` pair.
    ///
    /// `UnixDatagram::pair` would give `SOCK_DGRAM`, which is close enough to
    /// pass every test here and wrong in production: the helper's socket is
    /// connection-oriented, and a datagram pair has no EOF, so cancellation —
    /// the one behaviour that has no other expression — would never be
    /// exercised.
    fn seqpacket_pair() -> (UnixDatagram, UnixDatagram) {
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
        assert_eq!(rc, 0, "socketpair: {}", io::Error::last_os_error());
        // SAFETY: both descriptors are fresh and owned by nothing else.
        unsafe {
            (
                UnixDatagram::from_raw_fd(fds[0]),
                UnixDatagram::from_raw_fd(fds[1]),
            )
        }
    }

    /// Blocking reader for the wdm side of the socket.
    fn expect(sock: &UnixDatagram, what: &str) -> Msg {
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = vec![0u8; MAX_MESSAGE];
        let n = sock
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("waiting for {what}: {e}"));
        assert_ne!(
            n, 0,
            "the helper closed the socket instead of sending {what}"
        );
        Msg::decode(&buf[..n]).unwrap_or_else(|| panic!("undecodable {what}"))
    }

    fn send(sock: &UnixDatagram, msg: &Msg) {
        sock.send(&msg.encode()).unwrap();
    }

    /// A conversation on `wire`, at whichever side of `pam_authenticate`
    /// `authenticated` names.
    fn conversation(wire: &Wire, timeout: Duration, authenticated: bool) -> WireConversation<'_> {
        WireConversation {
            wire,
            timeout,
            username: "someone",
            authenticated,
        }
    }

    #[test]
    fn from_fd_refuses_a_closed_descriptor() {
        // The check exists so that a helper started without its socket says so
        // rather than adopting whatever descriptor 3 later becomes.
        //
        // 4096 is above any descriptor this test process has open and below the
        // usual soft limit is irrelevant — fcntl reports EBADF either way.
        assert!(Wire::from_fd(4096).is_err());
    }

    /// A [`Pam`] that succeeds, asks one question through the conversation, and
    /// records what it was told.
    struct FakePam<'a> {
        conv: WireConversation<'a>,
        /// Set when `with_session` runs, so a test can prove `open_session` did
        /// not happen before [`Msg::Launch`].
        opened: Option<(String, String)>,
        /// Set when the session is closed, which must happen even when wdm
        /// simply drops the socket.
        closed: bool,
        answer: Option<String>,
    }

    impl Pam for FakePam<'_> {
        fn authenticate(&mut self) -> Result<(), String> {
            match self.conv.prompt_echo_off(c"Password:") {
                Ok(answer) => {
                    self.answer = Some(answer.to_string_lossy().into_owned());
                    Ok(())
                }
                Err(_) => Err("Authentication failure".to_owned()),
            }
        }

        fn with_session(
            &mut self,
            session_type: &str,
            desktop: &str,
            hold: &mut dyn FnMut(Opened),
        ) {
            self.opened = Some((session_type.to_owned(), desktop.to_owned()));
            hold(Ok(vec![(
                "XDG_RUNTIME_DIR".to_owned(),
                "/run/user/1000".to_owned(),
            )]));
            self.closed = true;
        }
    }

    impl FakePam<'_> {
        fn new(wire: &Wire) -> FakePam<'_> {
            FakePam {
                conv: conversation(wire, Duration::from_secs(5), false),
                opened: None,
                closed: false,
                answer: None,
            }
        }
    }

    /// A [`Pam`] whose `authenticate` fails the way [`LibPam`]'s does.
    ///
    /// Deliberately not a hardcoded string: it calls the same `to_greeter` the
    /// real one calls, with a cause that is the account oracle itself, so that
    /// the assertion below is on the value that actually crosses the socket.
    /// [`FakePam`] hardcodes its own failure text and so cannot fail for this.
    struct RefusingPam;

    impl Pam for RefusingPam {
        fn authenticate(&mut self) -> Result<(), String> {
            Err(to_greeter(
                Phase::Authenticate,
                "bob",
                "User not known to the underlying authentication module",
            ))
        }

        fn with_session(&mut self, _: &str, _: &str, _: &mut dyn FnMut(Opened)) {
            panic!("with_session must not run after authentication failed");
        }
    }

    #[test]
    fn an_authentication_failure_reaches_wdm_as_one_fixed_string() {
        // What `LibPam::authenticate` refers to. Without it, reverting that line
        // to PAM's own words reopens the account oracle with a green suite:
        // `auth`'s own test pins `to_greeter`, and nothing pinned that the
        // helper's wire message is what `to_greeter` returned.
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        serve(&wire, &mut RefusingPam, &mut FakeSpawn::refusing("unused"));

        let verdict = expect(&wdm, "the verdict");
        let Msg::Failed(reason) = &verdict else {
            panic!("expected a Failed");
        };
        // The literal, not `auth::AUTH_FAILED`: the constant is private to
        // `auth` precisely so that this is the only call site able to name the
        // greeter-facing string, and a change to it has to be made twice
        // deliberately rather than tracked silently here.
        assert_eq!(
            reason, "Authentication failure",
            "the cause reached wdm, and wdm forwards Failed to the greeter \
             verbatim — the login screen is an account oracle again"
        );
    }

    /// A [`Spawn`] that forks an ordinary child instead of a user session.
    ///
    /// [`SessionSpawn`] drops privileges, and `setgroups` needs `CAP_SETGID`, so
    /// the real one cannot run in a suite that must not be root. What is under
    /// test here is [`run_session`]'s sequencing, which is the same either way.
    struct FakeSpawn {
        /// The environment `pam_open_session` handed over, recorded so a test
        /// can prove it reached the launch rather than being dropped on the way.
        pam_env: Option<Vec<(String, String)>>,
        /// Set when the launch is asked for, so a test can prove it was not.
        asked: bool,
        /// When set, the launch fails with this instead of forking.
        refuse: Option<String>,
        /// What to run. Something short-lived, so `wait` returns.
        argv: Vec<String>,
    }

    impl FakeSpawn {
        fn running(argv: &[&str]) -> Self {
            Self {
                pam_env: None,
                asked: false,
                refuse: None,
                argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            }
        }

        fn refusing(reason: &str) -> Self {
            Self {
                pam_env: None,
                asked: false,
                refuse: Some(reason.to_owned()),
                argv: Vec::new(),
            }
        }
    }

    impl Spawn for FakeSpawn {
        fn spawn(
            &mut self,
            _launch: &LaunchArgs,
            pam_env: Vec<(String, String)>,
        ) -> Result<Child, String> {
            self.asked = true;
            self.pam_env = Some(pam_env);
            if let Some(reason) = &self.refuse {
                return Err(reason.clone());
            }
            std::process::Command::new(&self.argv[0])
                .args(&self.argv[1..])
                .spawn()
                .map_err(|e| e.to_string())
        }
    }

    /// Send the launch wdm sends, once the display is gone.
    fn send_launch(wdm: &UnixDatagram) {
        send(
            wdm,
            &Msg::Launch {
                session_type: "wayland".to_owned(),
                desktop: "sway".to_owned(),
                session_id: "sway.desktop".to_owned(),
                session_name: "Sway".to_owned(),
                session_exec: "sway".to_owned(),
                extra_env: Vec::new(),
                vt: 7,
            },
        );
    }

    /// prompt -> respond -> ok -> launch -> started -> ended -> close.
    ///
    /// The order is the thing under test, and two parts of it are load-bearing.
    /// `open_session` running before [`Msg::Launch`] would mean opening a PAM
    /// session while wdm still holds the GPU, which is the entire defect this
    /// helper exists to fix. And `pam_close_session` must run after the session
    /// process has been reaped, on the handle that opened it — which is why the
    /// helper cannot simply `exec` the session and why `closed` is asserted last.
    #[test]
    fn the_helper_runs_the_session_between_opening_and_closing_it() {
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let prompt = expect(&wdm, "a prompt");
            let Msg::Prompt { id, text, style } = &prompt else {
                panic!("expected a prompt first");
            };
            assert_eq!(text, "Password:");
            assert_eq!(*style, PromptStyle::Secret);
            send(
                &wdm,
                &Msg::Response {
                    id: *id,
                    secret: "hunter2".to_owned(),
                },
            );

            assert_eq!(expect(&wdm, "the verdict"), Msg::Ok);

            send_launch(&wdm);

            let started = expect(&wdm, "the session starting");
            let Msg::SessionStarted { pid } = started else {
                panic!("expected SessionStarted, got {}", name_of(&started));
            };
            assert_ne!(pid, 0, "SessionStarted must name a real process");

            let ended = expect(&wdm, "the session ending");
            let Msg::SessionEnded { status, .. } = &ended else {
                panic!("expected SessionEnded, got {}", name_of(&ended));
            };
            assert!(
                status.contains("exit status: 0"),
                "the wait status did not survive: {status:?}"
            );

            drop(wdm);
        });

        let mut pam = FakePam::new(&wire);
        let mut spawner = FakeSpawn::running(&["/bin/true"]);
        serve(&wire, &mut pam, &mut spawner);
        driver.join().unwrap();

        assert_eq!(pam.answer.as_deref(), Some("hunter2"));
        assert_eq!(
            pam.opened,
            Some(("wayland".to_owned(), "sway".to_owned())),
            "the session must be opened with the choice Launch carried"
        );
        assert_eq!(
            spawner.pam_env,
            Some(vec![(
                "XDG_RUNTIME_DIR".to_owned(),
                "/run/user/1000".to_owned()
            )]),
            "the environment pam_open_session produced did not reach the launch"
        );
        assert!(
            pam.closed,
            "pam_close_session did not run after the session was reaped"
        );
    }

    #[test]
    fn a_launch_that_fails_is_reported_and_still_closes_the_pam_session() {
        // The display is already released by the time this can happen, so the
        // only way the user hears about it is SessionFailed becoming the next
        // greeter's last_error. And the PAM session was opened a moment earlier,
        // so it has to be closed even though nothing ever ran inside it — a
        // pam_systemd session leaked here is leaked for the life of the machine.
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let Msg::Prompt { id, .. } = expect(&wdm, "a prompt") else {
                panic!("expected a prompt first");
            };
            send(
                &wdm,
                &Msg::Response {
                    id,
                    secret: "hunter2".to_owned(),
                },
            );
            assert_eq!(expect(&wdm, "the verdict"), Msg::Ok);
            send_launch(&wdm);

            let failed = expect(&wdm, "the failure");
            let Msg::SessionFailed(reason) = &failed else {
                panic!("expected SessionFailed, got {}", name_of(&failed));
            };
            assert!(reason.contains("no such user"), "{reason}");
            drop(wdm);
        });

        let mut pam = FakePam::new(&wire);
        let mut spawner = FakeSpawn::refusing("no such user: nobody-at-all");
        serve(&wire, &mut pam, &mut spawner);
        driver.join().unwrap();

        assert!(pam.closed, "a failed launch left the PAM session open");
    }

    /// A failed authentication is reported and no session is opened.
    #[test]
    fn a_closed_socket_cancels_before_the_verdict() {
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            // Read the prompt so the helper is definitely blocked, then vanish.
            expect(&wdm, "a prompt");
            drop(wdm);
        });

        let mut pam = FakePam::new(&wire);
        let mut spawner = FakeSpawn::running(&["/bin/true"]);
        serve(&wire, &mut pam, &mut spawner);
        driver.join().unwrap();

        assert!(pam.answer.is_none(), "no answer should have arrived");
        assert!(
            pam.opened.is_none(),
            "a cancelled attempt must never open a PAM session"
        );
        assert!(
            !spawner.asked,
            "a cancelled attempt launched a session anyway"
        );
    }

    #[test]
    fn a_launch_rebuilds_the_session_it_was_told_about() {
        // The helper has no desktop files and no configuration: everything that
        // decides what runs crossed the socket. If this decoding drifts, the
        // session that starts is not the one the user picked — and the type is
        // what logind records, so getting it wrong misinforms every screen
        // locker on the machine.
        let msg = Msg::Launch {
            session_type: "x11".to_owned(),
            desktop: "i3".to_owned(),
            session_id: "i3.desktop".to_owned(),
            session_name: "i3".to_owned(),
            session_exec: "i3 --shmlog-size 0".to_owned(),
            extra_env: vec![("LANG".to_owned(), "de_DE.UTF-8".to_owned())],
            vt: 7,
        };
        let args = LaunchArgs::from_msg(&msg).expect("a Launch must parse");

        assert_eq!(args.vt, 7);
        assert_eq!(args.session.exec, "i3 --shmlog-size 0");
        assert_eq!(args.session.id, "i3.desktop");
        assert_eq!(args.session.name, "i3");
        assert_eq!(args.extra_env.len(), 1);

        // The two derived values the launched child sees must match the two the
        // helper puts into PAM's own environment before open_session. They come
        // from different fields of the same message, so this is where they could
        // disagree.
        assert_eq!(args.session.xdg_session_type(), args.session_type);
        assert_eq!(args.session.xdg_session_desktop(), args.desktop);

        assert!(LaunchArgs::from_msg(&Msg::Ok).is_none());
    }

    /// The defect that had no test because it was unreachable.
    ///
    /// `pam_loginuid` writes `/proc/self/loginuid`, and `proc_loginuid_write`
    /// returns `EPERM` unless the writing task is the one `/proc/self` names —
    /// the thread-group leader. PAM used to run on a spawned thread, so the
    /// write could never succeed; the helper is single-threaded, so it can.
    ///
    /// **What this covers:** that the session process inherits the helper's
    /// `loginuid`, which is the half the new architecture is responsible for.
    /// The value is set on the helper by `pam_loginuid` and copied by `fork`,
    /// and this asserts the copy by reading `/proc/<child>/loginuid` for the pid
    /// the helper reported.
    ///
    /// **What it does not cover:** that `pam_loginuid`'s write itself succeeds.
    /// That half is split across the three tests below —
    /// [`the_helper_runs_as_a_single_threaded_thread_group_leader`] asserts the
    /// architectural precondition on the real binary,
    /// [`only_the_task_a_loginuid_path_names_may_write_it`] asserts the kernel
    /// rule that makes the precondition necessary, and
    /// [`pam_loginuid_sets_the_loginuid_of_a_single_threaded_leader`] does the
    /// real privileged write behind `#[ignore]`.
    #[test]
    fn the_session_inherits_the_helpers_loginuid() {
        // Absent when the kernel is built without audit support, which is a
        // legitimate configuration rather than a failure of this code.
        let Ok(ours) = std::fs::read_to_string("/proc/self/loginuid") else {
            return;
        };

        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let Msg::Prompt { id, .. } = expect(&wdm, "a prompt") else {
                panic!("expected a prompt first");
            };
            send(
                &wdm,
                &Msg::Response {
                    id,
                    secret: "hunter2".to_owned(),
                },
            );
            assert_eq!(expect(&wdm, "the verdict"), Msg::Ok);
            send_launch(&wdm);

            let started = expect(&wdm, "the session starting");
            let Msg::SessionStarted { pid } = started else {
                panic!("expected SessionStarted, got {}", name_of(&started));
            };

            // Read while it is still alive — `sleep` outlives this — because a
            // reaped process has no /proc entry to read.
            let theirs = std::fs::read_to_string(format!("/proc/{pid}/loginuid"))
                .expect("the session process has no loginuid");

            // Not merely "is set": equal to the parent's, which is what `fork`
            // guarantees and what a session started from anywhere else would
            // not have.
            assert_eq!(
                theirs.trim(),
                ours.trim(),
                "the session did not inherit the helper's loginuid"
            );

            // SAFETY: the pid names a live child of this process — it has not
            // been waited for yet, so it cannot have been recycled.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

            expect(&wdm, "the session ending");
            drop(wdm);
        });

        let mut pam = FakePam::new(&wire);
        // Long enough that the reader above always wins the race, and killed as
        // soon as it has.
        let mut spawner = FakeSpawn::running(&["/bin/sleep", "30"]);
        serve(&wire, &mut pam, &mut spawner);
        driver.join().unwrap();
    }

    // ---------------------------------------------------------------------
    // The shape `pam_loginuid` needs, and the kernel rule that needs it.
    //
    // `the_session_inherits_the_helpers_loginuid` above covers the inheritance
    // half — that `fork` copies the helper's `loginuid` to the session. These
    // three cover the other half, that the helper's own write can succeed:
    //
    // 1. the real `--pam-helper` process is a single-threaded thread-group
    //    leader (unprivileged, unconditional);
    // 2. the kernel refuses a `/proc/self/loginuid` write from anything else
    //    (unprivileged, unconditional, and it changes nothing);
    // 3. `pam_loginuid` itself really does set it in that shape (root only,
    //    `#[ignore]`d).
    //
    // (1) is two tests, because neither alone is both unconditional and
    // complete: [`the_helpers_startup_spawns_no_thread`] runs the real [`run`]
    // and always executes, and
    // [`the_helper_binary_runs_as_a_single_threaded_thread_group_leader`] runs
    // the real binary — covering `main` as well — but needs a `cargo build`
    // that `cargo test` does not perform, so it can skip.
    //
    // **What none of them covers, and what is therefore still held by review:**
    //
    // - That no thread is spawned *between* the helper's startup, which (1)
    //   measures, and `pam_open_session`, which is where the shape has to hold.
    //   Nothing between those two points creates a thread today — the helper
    //   blocks on `recv`, and libpam is synchronous — and
    //   `warn_unless_pam_can_take_effect` logs it at the real moment if that
    //   ever changes, but a log line is not an assertion and no test reads it.
    //   Observing the shape at the instant of `open_session` would need either a
    //   real PAM stack in the helper (see (3), which needs root) or a probe
    //   fired from inside libpam, and neither is reachable unprivileged.
    // - That a PAM *module* does not spawn a thread of its own before writing
    //   `loginuid`. That is the module's bug and not wdm's, and wdm has no
    //   position from which to check it.

    /// The `wdm` binary this test binary was built beside, if it is there.
    ///
    /// `CARGO_BIN_EXE_wdm` is set only for integration tests and benches, and
    /// this crate's tests live in the bin target itself, so there is no cleaner
    /// handle than the layout: `current_exe` is `target/<profile>/deps/wdm-<hash>`
    /// and the binary is `target/<profile>/wdm`.
    fn wdm_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let candidate = exe.parent()?.parent()?.join("wdm");
        candidate.is_file().then_some(candidate)
    }

    /// The scheduler state character from `/proc/<pid>/stat`, if it is readable.
    ///
    /// Field 3, and parsed from the *last* `)` rather than by splitting: field 2
    /// is the executable name in parentheses and may itself contain spaces and
    /// parentheses.
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 1..]
            .split_whitespace()
            .next()?
            .chars()
            .next()
    }

    /// Spawn the real `wdm --pam-helper`, with `theirs` on its fd 3.
    ///
    /// A transcription of `auth::spawn_helper`'s descriptor handling rather than
    /// a call to it, because what is under test here is the process the
    /// production path produces, and a test that used the production spawner
    /// would prove nothing about the binary if the spawner were the thing that
    /// broke.
    fn spawn_real_helper(binary: &std::path::Path, theirs: &UnixDatagram) -> io::Result<Child> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let raw = theirs.as_raw_fd();
        let mut command = std::process::Command::new(binary);
        command.arg("--pam-helper");
        // SAFETY: runs in the forked child and calls nothing but `dup2` and
        // `fcntl` on a `RawFd` copied in by value. No allocation, no locks.
        unsafe {
            command.pre_exec(move || {
                // dup2 clears FD_CLOEXEC on the new descriptor, except when the
                // two are equal and it is a no-op — hence both steps.
                if raw != SOCKET_FD && libc::dup2(raw, SOCKET_FD) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(SOCKET_FD, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn()
    }

    #[test]
    fn a_task_shape_is_read_out_of_proc_status() {
        // Trimmed from a real /proc/self/status, keeping the neighbours that
        // could be mismatched: `PPid` and `NSpid` both end in the key `Pid:`,
        // and a prefix match without the colon would take `Ngid` for `Tgid`.
        let sample = "Name:\twdm\n\
                      Umask:\t0022\n\
                      State:\tS (sleeping)\n\
                      Tgid:\t1740183\n\
                      Ngid:\t0\n\
                      Pid:\t1740190\n\
                      PPid:\t1\n\
                      NSpid:\t1740190\n\
                      Threads:\t4\n";

        let shape = TaskShape::parse(sample).expect("a status must parse");
        assert_eq!(
            shape,
            TaskShape {
                threads: 4,
                pid: 1_740_190,
                tgid: 1_740_183,
            }
        );

        // A thread of a process: refused, and the message names both ids so a
        // reader knows which spawn to go and find.
        let why = shape.single_threaded_leader().unwrap_err();
        assert!(why.contains("1740190"), "{why}");
        assert!(why.contains("1740183"), "{why}");

        assert!(
            TaskShape {
                threads: 1,
                pid: 42,
                tgid: 42
            }
            .single_threaded_leader()
            .is_ok()
        );

        // A leader that grew a thread. The kernel would let it write loginuid,
        // but fork copies only the calling thread, so pam_keyinit and
        // pam_namespace would still be lost — this is deliberately stricter
        // than proc_loginuid_write.
        assert!(
            TaskShape {
                threads: 2,
                pid: 42,
                tgid: 42
            }
            .single_threaded_leader()
            .is_err()
        );

        assert!(TaskShape::parse("Name:\twdm\nThreads:\t1\n").is_none());
    }

    /// Wait until `pid` is asleep in a blocking read, or give up.
    ///
    /// The synchronisation both helper tests need, and it is exact rather than
    /// approximate: the helper's startup contains exactly one blocking call, the
    /// `recv` waiting for [`Msg::Start`]. Nothing else in it can sleep. So `S`
    /// means startup is *finished* — which is the point, because a process that
    /// has merely been forked is single-threaded for free and measuring it there
    /// would pass no matter what the helper does.
    fn wait_until_blocked(pid: u32, extra: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if extra() && proc_state(pid) == Some('S') {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Read a task's shape and assert it is the one PAM needs.
    fn assert_single_threaded_leader(pid: u32, what: &str) {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap_or_else(|e| panic!("{what} has no /proc status: {e}"));
        let shape = TaskShape::parse(&status).expect("a status must parse");

        assert_eq!(
            i64::from(shape.pid),
            i64::from(pid),
            "read the status of the wrong process"
        );
        assert_eq!(
            shape.threads, 1,
            "{what} has grown a thread; pam_loginuid, pam_keyinit, pam_namespace and \
             ambient capabilities will silently stop reaching the session"
        );
        assert_eq!(
            shape.pid, shape.tgid,
            "{what} is not a thread-group leader, and /proc/self names the leader"
        );
        assert_eq!(shape.single_threaded_leader(), Ok(()));
    }

    /// The architectural precondition, on the real [`run`], unconditionally.
    ///
    /// `fork` hands the child a single thread whatever the parent had, so the
    /// shape this asserts is the shape the child *starts* in — and therefore
    /// what it really asserts is that [`run`] does not spawn one on the way to
    /// its first read. That is the regression this is here to catch: a worker
    /// thread, an async runtime, a logger with a background writer. Any of them
    /// would take `pam_loginuid` back to the `EPERM` it returned for wdm's whole
    /// life before this helper existed, with no symptom but a journal line.
    ///
    /// The exec'd binary is covered separately, and skippably, below. This one
    /// forks so that it always runs: `cargo test` builds no `wdm` binary.
    #[test]
    fn the_helpers_startup_spawns_no_thread() {
        use std::os::fd::AsRawFd;

        let (ours, theirs) = seqpacket_pair();
        let ours_fd = ours.as_raw_fd();
        let theirs_fd = theirs.as_raw_fd();

        // SAFETY: the child calls `dup2`, `close`, `run` and `_exit`. `run` is
        // not async-signal-safe — it allocates — which is sound here for two
        // specific reasons and not in general: glibc locks and reinitialises the
        // malloc arenas across `fork`, and the log macros reach a `NopLogger`
        // with no lock behind it, because this test binary has no `main` to
        // initialise `env_logger`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: plain descriptor manipulation in a single-threaded child.
            unsafe {
                // wdm's end is inherited too, and while the child holds it open
                // the child's own socket can never see EOF — so it would never
                // return from `run` and this test would hang.
                libc::close(ours_fd);
                if theirs_fd != SOCKET_FD && libc::dup2(theirs_fd, SOCKET_FD) < 0 {
                    libc::_exit(70);
                }
                if libc::fcntl(SOCKET_FD, libc::F_SETFD, 0) < 0 {
                    libc::_exit(71);
                }
            }
            // Blocks on the socket until the parent drops its end.
            let _ = run();
            // SAFETY: terminates the child without running atexit handlers or
            // flushing stdio buffers shared with the parent.
            unsafe { libc::_exit(0) };
        }
        drop(theirs);

        let child = u32::try_from(pid).expect("a pid is positive");
        assert!(
            wait_until_blocked(child, || true),
            "the helper never reached its blocking read"
        );
        assert_single_threaded_leader(child, "the PAM helper");

        // And it unwinds on EOF, which is also how this test cleans up.
        drop(ours);
        let mut wstatus: libc::c_int = 0;
        // SAFETY: `pid` is this process's child and `wstatus` is a live local.
        unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        assert!(
            libc::WIFEXITED(wstatus) && libc::WEXITSTATUS(wstatus) == 0,
            "the helper did not return from run() when wdm closed the socket"
        );
    }

    /// The architectural precondition, asserted on the real binary.
    ///
    /// [`the_helpers_startup_spawns_no_thread`] covers [`run`], but the helper
    /// is a whole *process*, and `main` runs before `run` does: argument
    /// parsing, `env_logger::init`, and whatever the next person puts there. A
    /// thread spawned in any of it costs exactly as much and no fork-based test
    /// can see it. So: run the actual `wdm --pam-helper`, wait until it is
    /// blocked reading its socket — past every line of its startup — and read
    /// `/proc/<pid>/status` out of it.
    ///
    /// Skippable, and it is the weaker of the two for that reason: `cargo test`
    /// builds the bin target only as a test harness, so the binary exists only
    /// if someone ran `cargo build`. Hence the unconditional fork-based test
    /// above rather than this one alone.
    #[test]
    fn the_helper_binary_runs_as_a_single_threaded_thread_group_leader() {
        let Some(binary) = wdm_binary() else {
            println!(
                "SKIP the_helper_binary_runs_as_a_single_threaded_thread_group_leader: \
                 no `wdm` binary beside the test binary — `cargo test` does not build one. \
                 Run `cargo build -p wdm` first to include this check; \
                 the_helpers_startup_spawns_no_thread covers run() either way."
            );
            return;
        };
        let resolved = std::fs::canonicalize(&binary).expect("the wdm binary must resolve");

        let (ours, theirs) = seqpacket_pair();
        let mut child = spawn_real_helper(&binary, &theirs).expect("spawning the helper");
        // Before anything reads: while the helper's end of the socketpair is
        // open in this process too, closing wdm's end would not produce the EOF
        // the helper exits on.
        drop(theirs);
        let pid = child.id();

        // `exe` as well as the sleeping state, because `spawn` returns after the
        // fork and possibly before the exec: a pre-exec child is asleep and
        // single-threaded without any of the helper's code having run.
        let ready = wait_until_blocked(pid, || {
            std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|target| target == resolved)
        });
        assert!(
            ready,
            "the helper never reached its own code and blocked on the socket"
        );
        assert_single_threaded_leader(pid, "the PAM helper binary");

        // Now let it go: with wdm's end closed too the helper's blocked read
        // returns EOF and it unwinds through the path cancellation uses.
        drop(ours);
        let exited = {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break true,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => break false,
                }
            }
        };
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the helper did not exit when wdm closed the socket");
        }
    }

    /// A payload `/proc/<pid>/loginuid` can never accept.
    ///
    /// The point is *where* it is rejected. `proc_loginuid_write` checks that
    /// the writing task is the one the opened path names **before** it parses,
    /// so a task that is allowed to write gets `EINVAL` for this and a task that
    /// is not gets `EPERM` — which separates the two without any privilege,
    /// without needing an unset `loginuid`, and without ever changing a
    /// `loginuid`, since a payload that does not parse is never applied. That
    /// last part is what makes the test safe to run in-process: `loginuid` is
    /// write-once, and a test that could really set it would poison the run.
    const UNPARSABLE_LOGINUID: &[u8] = b"not-a-number";

    /// `open` + `write` with no allocation, returning `errno` (0 on success).
    ///
    /// Raw `libc` rather than `std::fs` because this is also called in a forked
    /// child, where only async-signal-safe work is allowed. `open`, `write`,
    /// `close` and reading `errno` all are.
    fn poke_loginuid(path: &CStr) -> i32 {
        // SAFETY: `path` is a NUL-terminated C string that outlives the call,
        // and the buffer handed to `write` is a `'static` slice.
        unsafe {
            let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
            if fd < 0 {
                return io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO);
            }
            let written = libc::write(
                fd,
                UNPARSABLE_LOGINUID.as_ptr().cast(),
                UNPARSABLE_LOGINUID.len(),
            );
            let errno = if written < 0 {
                io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO)
            } else {
                0
            };
            libc::close(fd);
            errno
        }
    }

    /// The kernel rule the helper's whole shape exists to satisfy.
    ///
    /// `pam_loginuid` writes `/proc/self/loginuid`, and:
    ///
    /// ```c
    /// if (current != pid_task(proc_pid(inode), PIDTYPE_PID))
    ///         return -EPERM;
    /// ```
    ///
    /// `/proc/self` resolves to the thread-group leader, so **any write from a
    /// non-leader thread is `EPERM`** — which is why PAM on a spawned thread, as
    /// wdm ran it before this helper existed, could never set `loginuid` at all.
    ///
    /// Three writes, all of an unparsable payload so that nothing is changed:
    ///
    /// - from a spawned thread to `/proc/self/loginuid` — `EPERM`, refused for
    ///   identity;
    /// - from that same thread to `/proc/thread-self/loginuid`, which names the
    ///   thread itself — `EINVAL`, so the `EPERM` above was about *who wrote*
    ///   and not about privilege, the payload, or the file being unwritable;
    /// - from a freshly forked child, which is the helper's exact shape — one
    ///   thread, therefore its own leader — `EINVAL`, so a process shaped like
    ///   the helper gets past the check that stopped the thread.
    ///
    /// Without this test the single-threaded requirement reads as taste, and the
    /// next person to want a worker thread in the helper has nothing telling
    /// them what it costs.
    #[test]
    fn only_the_task_a_loginuid_path_names_may_write_it() {
        if !std::path::Path::new("/proc/self/loginuid").exists() {
            println!(
                "SKIP only_the_task_a_loginuid_path_names_may_write_it: \
                 this kernel has no /proc/self/loginuid (CONFIG_AUDIT is off), \
                 so pam_loginuid is a no-op here and there is no rule to check"
            );
            return;
        }

        // A thread of our own, deliberately: libtest runs a test on a spawned
        // thread with `--test-threads` above 1 and on the main thread with 1, so
        // the harness alone decides whether the calling thread is the leader.
        // Spawning makes it certain.
        let (via_self, via_thread_self) = std::thread::spawn(|| {
            (
                poke_loginuid(c"/proc/self/loginuid"),
                poke_loginuid(c"/proc/thread-self/loginuid"),
            )
        })
        .join()
        .expect("the probe thread panicked");

        assert_eq!(
            via_self,
            libc::EPERM,
            "a non-leader thread was allowed past proc_loginuid_write's identity check; \
             if this ever stops being EPERM the helper's single-threadedness is no longer \
             what makes pam_loginuid work, and the reason for it needs rewriting"
        );
        assert_eq!(
            via_thread_self,
            libc::EINVAL,
            "the same thread writing the path that names *it* should have got past the \
             identity check and failed on the payload; EPERM here would mean the EPERM \
             above proved nothing"
        );

        // And now the helper's shape. SAFETY: the child does nothing but
        // `open`, `write`, `close` and `_exit`, all async-signal-safe, which is
        // the rule for a fork out of this multi-threaded test runner.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            let errno = poke_loginuid(c"/proc/self/loginuid");
            // _exit, not exit: no atexit handlers, no flushing of a stdio buffer
            // this process shares with the parent.
            // SAFETY: terminates the child and nothing else.
            unsafe { libc::_exit(errno) };
        }

        let mut wstatus: libc::c_int = 0;
        // SAFETY: `pid` is this process's child and `wstatus` is a live local.
        let waited = unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        assert_eq!(waited, pid, "waitpid: {}", io::Error::last_os_error());
        assert!(
            libc::WIFEXITED(wstatus),
            "the probe child died on signal {}",
            libc::WTERMSIG(wstatus)
        );
        assert_eq!(
            libc::WEXITSTATUS(wstatus),
            libc::EINVAL,
            "a single-threaded, freshly forked process — the helper's exact shape — was \
             refused its own /proc/self/loginuid; the architecture no longer buys what \
             the helper exists for"
        );
    }

    /// The real `pam_loginuid` write, end to end.
    ///
    /// `#[ignore]` rather than a runtime skip, because the preconditions are not
    /// merely absent in CI but absent by design: this needs `CAP_AUDIT_CONTROL`
    /// (so, root) and a PAM service file, which means writing into `/etc/pam.d`.
    /// A test that touches `/etc` must be asked for by name, and `#[ignore]`
    /// makes asking explicit and visible in the summary line, where a
    /// conditional skip would be invisible unless someone passed `--nocapture`.
    ///
    /// ```text
    /// sudo -E cargo test -p wdm --bin wdm -- --ignored --nocapture pam_loginuid_sets
    /// ```
    ///
    /// It forks, because `loginuid` is write-once per process and setting it
    /// here would poison every later test in the run. The child is exactly the
    /// helper's shape — single-threaded and its own leader — which is the point:
    /// this is the assertion that the shape the two tests above pin is the shape
    /// in which `pam_loginuid` actually succeeds.
    #[test]
    #[ignore = "needs root and writes a temporary /etc/pam.d service; see the doc comment"]
    fn pam_loginuid_sets_the_loginuid_of_a_single_threaded_leader() {
        const SERVICE_NAME: &str = "wdm-loginuid-selftest";
        const TARGET_USER: &str = "nobody";

        let missing = if !std::path::Path::new("/proc/self/loginuid").exists() {
            Some("this kernel has no /proc/self/loginuid (CONFIG_AUDIT is off)")
        // SAFETY: geteuid reads a field of the current process and cannot fail.
        } else if unsafe { libc::geteuid() } != 0 {
            Some("not root, so there is no CAP_AUDIT_CONTROL and no way to write /etc/pam.d")
        } else if !std::path::Path::new("/etc/pam.d").is_dir() {
            Some("there is no /etc/pam.d to install a service into")
        } else {
            None
        };
        if let Some(reason) = missing {
            println!("SKIP pam_loginuid_sets_the_loginuid_of_a_single_threaded_leader: {reason}");
            return;
        }

        let Some(target_uid) = uid_of(TARGET_USER) else {
            println!(
                "SKIP pam_loginuid_sets_the_loginuid_of_a_single_threaded_leader: \
                 there is no `{TARGET_USER}` account for pam_loginuid to resolve"
            );
            return;
        };

        // Its own service, never /etc/pam.d/wdm: this writes a file, and
        // clobbering the production stack on a developer's machine would be an
        // unforgivable thing for a test to do. pam_permit for auth and account
        // so no password is needed; pam_loginuid alone for session, so the only
        // thing under test is the write.
        let service_path = PathBuf::from(format!("/etc/pam.d/{SERVICE_NAME}"));
        if service_path.exists() {
            println!(
                "SKIP pam_loginuid_sets_the_loginuid_of_a_single_threaded_leader: \
                 {} already exists and this test will not overwrite it",
                service_path.display()
            );
            return;
        }
        std::fs::write(
            &service_path,
            "auth     required pam_permit.so\n\
             account  required pam_permit.so\n\
             session  required pam_loginuid.so\n\
             session  required pam_permit.so\n",
        )
        .expect("writing the test PAM service");

        // Not RAII: the child must not run this on its own exit path, and a
        // guard would. The parent removes it on every return below.
        let outcome =
            std::panic::catch_unwind(|| run_loginuid_child(SERVICE_NAME, TARGET_USER, target_uid));
        let _ = std::fs::remove_file(&service_path);

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => panic!("{reason}"),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// The uid `pam_loginuid` will resolve for `name`, if the account exists.
    fn uid_of(name: &str) -> Option<u32> {
        let entry = CString::new(name).ok()?;
        // SAFETY: getpwnam takes a NUL-terminated string and returns a pointer
        // into a static buffer, read immediately and not retained.
        let pw = unsafe { libc::getpwnam(entry.as_ptr()) };
        if pw.is_null() {
            return None;
        }
        // SAFETY: non-null, and pointing at a live passwd for the duration of
        // this read.
        Some(unsafe { (*pw).pw_uid })
    }

    /// Fork, run the PAM sequence in the child, and report what its `loginuid`
    /// became.
    ///
    /// The child does allocate and does `dlopen` PAM modules, which is more than
    /// `fork` from a multi-threaded parent formally allows. That is tolerable
    /// only because this test is `#[ignore]`d and run by hand: a wedged child is
    /// visible to whoever asked for it, and the alternative — re-`exec`ing to
    /// get a clean single-threaded process — would need a second entry point in
    /// the binary that exists purely for a test.
    fn run_loginuid_child(service: &str, user: &str, target_uid: u32) -> Result<(), String> {
        // Exit codes, because a pipe would be one more thing to get wrong in a
        // child that must stay simple. Kept below 64 so nothing collides with a
        // shell's signal encoding.
        const OK: i32 = 0;
        const CONTEXT_FAILED: i32 = 10;
        const AUTH_FAILED: i32 = 11;
        const ACCT_FAILED: i32 = 12;
        const OPEN_FAILED: i32 = 13;
        const UNREADABLE: i32 = 14;
        const NOT_SET: i32 = 15;

        // SAFETY: the child's work is documented above as knowingly beyond
        // async-signal-safety, and bounded to this `#[ignore]`d test.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!("fork: {}", io::Error::last_os_error()));
        }
        if pid == 0 {
            let code = loginuid_child_body(service, user, target_uid, |what| match what {
                ChildStep::Context => CONTEXT_FAILED,
                ChildStep::Auth => AUTH_FAILED,
                ChildStep::Acct => ACCT_FAILED,
                ChildStep::Open => OPEN_FAILED,
                ChildStep::Unreadable => UNREADABLE,
                ChildStep::NotSet => NOT_SET,
            });
            // SAFETY: terminates the child without running the parent's atexit
            // handlers or flushing its stdio buffers.
            unsafe { libc::_exit(code) };
        }

        let mut wstatus: libc::c_int = 0;
        // SAFETY: `pid` is this process's child and `wstatus` is a live local.
        unsafe { libc::waitpid(pid, &mut wstatus, 0) };
        if !libc::WIFEXITED(wstatus) {
            return Err(format!(
                "the PAM child died on signal {}",
                libc::WTERMSIG(wstatus)
            ));
        }
        match libc::WEXITSTATUS(wstatus) {
            OK => Ok(()),
            CONTEXT_FAILED => Err(format!("pam_start failed for service {service}")),
            AUTH_FAILED => Err("pam_authenticate failed against pam_permit".to_owned()),
            ACCT_FAILED => Err("pam_acct_mgmt failed against pam_permit".to_owned()),
            OPEN_FAILED => Err("pam_open_session failed".to_owned()),
            UNREADABLE => Err("/proc/self/loginuid was unreadable in the child".to_owned()),
            NOT_SET => Err(format!(
                "pam_loginuid did not set loginuid to {user}'s uid ({target_uid}) in a \
                 single-threaded thread-group leader — the write the whole helper exists \
                 to make possible did not happen"
            )),
            other => Err(format!("the PAM child exited {other}")),
        }
    }

    /// Which step of the child's PAM sequence failed.
    enum ChildStep {
        Context,
        Auth,
        Acct,
        Open,
        Unreadable,
        NotSet,
    }

    /// The child's half of [`run_loginuid_child`], factored out so the `_exit`
    /// stays on one line at the call site.
    fn loginuid_child_body(
        service: &str,
        user: &str,
        target_uid: u32,
        code: impl Fn(ChildStep) -> i32,
    ) -> i32 {
        let Ok(mut context) = Context::new(
            service,
            Some(user),
            pam_client2::conv_null::Conversation::new(),
        ) else {
            return code(ChildStep::Context);
        };
        if context.authenticate(Flag::NONE).is_err() {
            return code(ChildStep::Auth);
        }
        if context.acct_mgmt(Flag::NONE).is_err() {
            return code(ChildStep::Acct);
        }
        let Ok(session) = context.open_session(Flag::NONE) else {
            return code(ChildStep::Open);
        };

        let result = match std::fs::read_to_string("/proc/self/loginuid") {
            Ok(value) if value.trim() == target_uid.to_string() => 0,
            Ok(_) => code(ChildStep::NotSet),
            Err(_) => code(ChildStep::Unreadable),
        };
        drop(session);
        result
    }

    #[test]
    fn a_timeout_explains_itself_before_failing() {
        // The regression this exists for, ported from auth.rs. A prompt nobody
        // answers must fail the attempt *and say so as an `error` prompt*: an
        // unexplained failure is the same shape as a mistyped password, so
        // greeters retry, re-arm the timeout and spin — one pam_faillock entry
        // per turn until the account locks, with nobody at the keyboard.
        //
        // Assert the style, not merely that something was said: an `info`
        // prompt would display identically and would not set `blocked`, which
        // is what Model::should_auto_retry consults.
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let first = expect(&wdm, "a prompt");
            let Msg::Prompt { style, .. } = first else {
                panic!("expected a prompt first");
            };
            assert_eq!(style, PromptStyle::Secret);

            // Answer nothing at all, and read what the helper says next.
            let notice = expect(&wdm, "the timeout notice");
            let Msg::Prompt { text, style, .. } = &notice else {
                panic!("expected a second prompt");
            };
            assert_eq!(*style, PromptStyle::Error);
            assert!(
                text.contains("timed out"),
                "the timeout notice should say what happened, got {text:?}"
            );
        });

        let mut conv = conversation(&wire, Duration::from_millis(50), false);
        assert_eq!(
            conv.prompt_echo_off(c"Password:").unwrap_err(),
            ErrorCode::CONV_ERR
        );
        driver.join().unwrap();
    }

    #[test]
    fn stale_responses_cannot_extend_the_deadline() {
        // RESPONSE_TIMEOUT is measured from when the prompt was emitted, not
        // from the last message received, and that is the whole leak guard: a
        // peer that can restart the clock by sending anything can pin the helper
        // and the PAM transaction forever. The guarantee comes from computing
        // `deadline` once, before the loop.
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let Msg::Prompt { id, .. } = expect(&wdm, "a prompt") else {
                panic!("expected a prompt first");
            };
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                // Mismatched ids, so each is discarded and the loop goes round
                // again without the deadline moving.
                if wdm
                    .send(
                        &Msg::Response {
                            id: id.wrapping_add(1000),
                            secret: "stale".to_owned(),
                        }
                        .encode(),
                    )
                    .is_err()
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let mut conv = conversation(&wire, Duration::from_millis(50), false);
        let started = Instant::now();
        assert_eq!(
            conv.prompt_echo_off(c"Password:").unwrap_err(),
            ErrorCode::CONV_ERR
        );
        let waited = started.elapsed();
        driver.join().unwrap();

        // Generous upwards because a loaded CI box schedules threads late; what
        // matters is that it did not run for the two seconds of chatter.
        assert!(
            waited < Duration::from_millis(1500),
            "stale responses extended the deadline: waited {waited:?}"
        );
    }

    #[test]
    fn answers_reach_the_conversation_and_ids_advance() {
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let Msg::Prompt { id: first, .. } = expect(&wdm, "the username prompt") else {
                panic!("expected a prompt");
            };
            send(
                &wdm,
                &Msg::Response {
                    id: first,
                    secret: "testuser".to_owned(),
                },
            );
            let Msg::Prompt { id: second, .. } = expect(&wdm, "the password prompt") else {
                panic!("expected a prompt");
            };
            assert_ne!(first, second, "ids must not repeat within a conversation");
            send(
                &wdm,
                &Msg::Response {
                    id: second,
                    secret: "hunter2".to_owned(),
                },
            );
        });

        let mut conv = conversation(&wire, Duration::from_secs(5), false);
        let username = conv.prompt_echo_on(c"Username:").unwrap();
        let password = conv.prompt_echo_off(c"Password:").unwrap();
        driver.join().unwrap();

        assert_eq!(username.to_str().unwrap(), "testuser");
        assert_eq!(password.to_str().unwrap(), "hunter2");
    }

    #[test]
    fn a_response_containing_nul_is_refused() {
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let driver = std::thread::spawn(move || {
            let Msg::Prompt { id, .. } = expect(&wdm, "a prompt") else {
                panic!("expected a prompt");
            };
            send(
                &wdm,
                &Msg::Response {
                    id,
                    secret: "bad\0password".to_owned(),
                },
            );
        });

        let mut conv = conversation(&wire, Duration::from_secs(5), false);
        assert_eq!(
            conv.prompt_echo_off(c"Password:").unwrap_err(),
            ErrorCode::CONV_ERR
        );
        driver.join().unwrap();
    }

    #[test]
    fn informational_messages_need_no_response() {
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let mut conv = conversation(&wire, Duration::from_secs(5), true);
        // text_info and error_msg must not block waiting for an answer, so this
        // runs on the test thread with nobody reading the other end.
        conv.text_info(c"Welcome");
        conv.error_msg(c"Nope");

        let Msg::Prompt {
            id: info, style, ..
        } = expect(&wdm, "the info message")
        else {
            panic!("expected a prompt");
        };
        assert_eq!(style, PromptStyle::Info);
        let Msg::Prompt {
            id: error, style, ..
        } = expect(&wdm, "the error message")
        else {
            panic!("expected a prompt");
        };
        assert_eq!(style, PromptStyle::Error);
        assert!(error > info, "ids must advance");
    }

    #[test]
    fn module_text_before_authentication_succeeds_does_not_reach_the_greeter() {
        // The oracle `notice_to_greeter` closes, and the sharper of the two:
        // pam_faillock announces a lockout only for an account that exists, and
        // it announces it through this channel before any password has been
        // accepted. Two notices that say different things must arrive
        // indistinguishable.
        let (helper_sock, wdm) = seqpacket_pair();
        let wire = Wire::from_socket(helper_sock);

        let mut before = conversation(&wire, Duration::from_secs(5), false);
        before.error_msg(c"Account locked due to 3 failed logins");
        before.text_info(c"Server message: user not found");

        let first = expect(&wdm, "the lockout notice");
        let Msg::Prompt { text: locked, .. } = &first else {
            panic!("expected a prompt");
        };
        let second = expect(&wdm, "the unknown-user notice");
        let Msg::Prompt { text: unknown, .. } = &second else {
            panic!("expected a prompt");
        };
        assert_eq!(
            locked, unknown,
            "the module's own words crossed the socket, so the greeter can tell \
             a locked account from an account that does not exist"
        );
        assert!(!locked.contains("locked"));

        // And the other half: once authenticate has returned Ok the module gets
        // its words back, because that is where the expired-password change runs
        // and "password too short" is the only useful thing it says.
        let mut after = conversation(&wire, Duration::from_secs(5), true);
        after.error_msg(c"BAD PASSWORD: it is too short");
        let complaint = expect(&wdm, "the password-quality complaint");
        let Msg::Prompt { text, .. } = &complaint else {
            panic!("expected a prompt");
        };
        assert_eq!(text, "BAD PASSWORD: it is too short");
    }

    #[test]
    fn start_is_the_only_message_that_starts_an_attempt() {
        // The helper reads exactly one Start and refuses anything else, so a
        // peer cannot skip authentication by opening with a Launch.
        let start = Msg::Start {
            username: "testuser".to_owned(),
            tty: "/dev/tty7".to_owned(),
            seat: "seat0".to_owned(),
            vtnr: 7,
            session_type: "wayland".to_owned(),
            desktop: String::new(),
        };
        let parsed = Start::from_msg(&start).expect("a Start must parse");
        assert_eq!(parsed.username, "testuser");
        assert_eq!(parsed.tty, "/dev/tty7");
        assert_eq!(parsed.session.seat, "seat0");
        assert_eq!(parsed.session.vtnr, 7);

        assert!(Start::from_msg(&Msg::Ok).is_none());
        assert!(
            Start::from_msg(&Msg::Launch {
                session_type: "wayland".to_owned(),
                desktop: String::new(),
                session_id: String::new(),
                session_name: String::new(),
                session_exec: String::new(),
                extra_env: Vec::new(),
                vt: 7,
            })
            .is_none()
        );
    }

    #[test]
    fn a_message_name_never_contains_a_secret() {
        // name_of exists because Debug on a Response would print the password.
        // If someone replaces it with format!("{msg:?}"), this fails.
        let msg = Msg::Response {
            id: 1,
            secret: "hunter2".to_owned(),
        };
        assert_eq!(name_of(&msg), "response");
    }
}
