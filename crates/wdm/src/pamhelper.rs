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
//! # What is not here yet
//!
//! `Msg::Launch` gets as far as `pam_open_session` and stops. Forking the user's
//! session out of the helper — and the `Launch::validate`/`Launch::build` split
//! that goes with it — is the next layer of this change; until it lands, wdm
//! still authenticates through [`crate::auth`]'s thread and nothing sends this
//! helper a message. See the `ponytail:` in [`serve`].

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use zeroize::Zeroize;

use crate::auth::{
    PromptStyle, RESPONSE_TIMEOUT, SERVICE, SessionDescription, describe, next_prompt_id,
    os_to_string, pam_session_env,
};
use crate::pamwire::{MAX_MESSAGE, Msg};

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

    log::debug!("pam helper started for {} on {}", start.username, start.tty);

    let conv = WireConversation {
        wire: &wire,
        timeout: RESPONSE_TIMEOUT,
    };

    let context = match Context::new(SERVICE, Some(&start.username), conv) {
        Ok(context) => context,
        Err(e) => {
            // Almost always a missing /etc/pam.d/wdm. Say so, because the
            // generic PAM message is unhelpful.
            log::error!("opening PAM context for service {SERVICE}: {e}");
            wire.send(&Msg::Failed(describe(&e)));
            return ExitCode::SUCCESS;
        }
    };

    let mut pam = LibPam {
        context,
        start: &start,
    };

    serve(&wire, &mut pam);
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

/// What `pam_open_session` produced, or the text explaining why it did not.
type Opened = Result<Vec<(String, String)>, String>;

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

/// The protocol, with PAM behind [`Pam`].
///
/// Every exit from here is deliberate: an authentication failure is reported and
/// the helper stops, a closed socket before [`Msg::Launch`] is a cancellation
/// and the helper stops, and a session that opened is held until the socket
/// closes so that `pam_close_session` runs on the handle that opened it.
fn serve(wire: &Wire, pam: &mut dyn Pam) {
    if let Err(reason) = pam.authenticate() {
        wire.send(&Msg::Failed(reason));
        return;
    }

    wire.send(&Msg::Ok);

    // Wait for the greeter's choice. A closed socket here is cancellation, and
    // returning runs pam_end through the Context's Drop.
    let (session_type, desktop) = loop {
        // Bound rather than matched in place: `Msg` has a `Drop`, so its fields
        // can only be borrowed out of it.
        let msg = match wire.recv(None) {
            Recv::Msg(msg) => msg,
            Recv::Closed | Recv::Timeout => {
                log::debug!("the attempt was cancelled before launch");
                return;
            }
        };
        if let Msg::Launch {
            session_type,
            desktop,
            ..
        } = &msg
        {
            break (session_type.clone(), desktop.clone());
        }
        log::debug!("ignoring {} before launch", name_of(&msg));
    };

    pam.with_session(&session_type, &desktop, &mut |opened| match opened {
        Ok(env) => {
            // ponytail: the session is not launched here yet. This layer proves
            // the helper can authenticate and open a session out of process;
            // forking, dropping privileges and `exec`ing the session — with the
            // environment `env` holds, which is where XDG_RUNTIME_DIR comes
            // from — belongs to the layer that also splits `Launch::prepare`
            // into validate and build. Until then wdm still launches sessions
            // itself and nothing sends this helper a Launch, so the pid is a
            // placeholder rather than a lie about a process that exists.
            log::debug!("PAM session opened with {} environment entries", env.len());
            wire.send(&Msg::SessionStarted { pid: 0 });

            // Hold the PAM session open until wdm says otherwise. The `hold`
            // callback returning is what drops the session and closes it, so
            // every way out of this loop is also a `pam_close_session`.
            while let Recv::Msg(msg) = wire.recv(None) {
                log::debug!("ignoring {} while the session runs", name_of(&msg));
            }
        }
        Err(reason) => {
            log::error!("opening the PAM session failed: {reason}");
            wire.send(&Msg::SessionFailed(reason));
        }
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
struct LibPam<'a, C: ConversationHandler> {
    context: Context<C>,
    start: &'a Start,
}

impl<C: ConversationHandler> Pam for LibPam<'_, C> {
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
            log::info!("authentication failed for {}: {e}", self.start.username);
            return Err(describe(&e));
        }

        match self.context.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ErrorCode::NEW_AUTHTOK_REQD => {
                // The password is expired. PAM will not let the account in
                // until it is changed, and the change has to run through the
                // same conversation so the greeter can display its prompts.
                // Without this a user with an expired password can never log in.
                log::info!(
                    "{} must change their password before logging in",
                    self.start.username
                );
                self.context
                    .chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK)
                    .map_err(|e| {
                        log::info!("password change failed for {}: {e}", self.start.username);
                        describe(&e)
                    })
            }
            Err(e) => {
                log::info!("account management failed for {}: {e}", self.start.username);
                Err(describe(&e))
            }
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
            Err(e) => hold(Err(describe(&e))),
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
    fn tell(&mut self, msg: &CStr, style: PromptStyle) {
        self.emit(next_prompt_id(), msg.to_string_lossy().into_owned(), style);
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

    /// prompt -> respond -> ok -> launch -> session -> close.
    ///
    /// The order is the thing under test. `open_session` running before
    /// [`Msg::Launch`] would mean opening a PAM session while wdm still holds
    /// the GPU, which is the entire defect this helper exists to fix.
    #[test]
    fn the_helper_opens_a_session_only_after_launch() {
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

            send(
                &wdm,
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

            assert_eq!(expect(&wdm, "the session"), Msg::SessionStarted { pid: 0 });

            // Dropping wdm's end is how a finished session and a cancellation
            // both present.
            drop(wdm);
        });

        let mut pam = FakePam {
            conv: WireConversation {
                wire: &wire,
                timeout: Duration::from_secs(5),
            },
            opened: None,
            closed: false,
            answer: None,
        };
        serve(&wire, &mut pam);
        driver.join().unwrap();

        assert_eq!(pam.answer.as_deref(), Some("hunter2"));
        assert_eq!(
            pam.opened,
            Some(("wayland".to_owned(), "sway".to_owned())),
            "the session must be opened with the choice Launch carried"
        );
        assert!(
            pam.closed,
            "a closed socket must still reach pam_close_session"
        );
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

        let mut pam = FakePam {
            conv: WireConversation {
                wire: &wire,
                timeout: Duration::from_secs(5),
            },
            opened: None,
            closed: false,
            answer: None,
        };
        serve(&wire, &mut pam);
        driver.join().unwrap();

        assert!(pam.answer.is_none(), "no answer should have arrived");
        assert!(
            pam.opened.is_none(),
            "a cancelled attempt must never open a PAM session"
        );
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

        let mut conv = WireConversation {
            wire: &wire,
            timeout: Duration::from_millis(50),
        };
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

        let mut conv = WireConversation {
            wire: &wire,
            timeout: Duration::from_millis(50),
        };
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

        let mut conv = WireConversation {
            wire: &wire,
            timeout: Duration::from_secs(5),
        };
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

        let mut conv = WireConversation {
            wire: &wire,
            timeout: Duration::from_secs(5),
        };
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

        let mut conv = WireConversation {
            wire: &wire,
            timeout: Duration::from_secs(5),
        };
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
