//! PAM authentication, and the only place wdm uses threads.
//!
//! `pam_authenticate` blocks, and its conversation callback is a C callback
//! invoked from inside libpam. It cannot be driven from an event loop, so each
//! authentication attempt gets a thread. The conversation forwards each question
//! to the event loop over a [`calloop::channel`] and blocks on an [`mpsc`]
//! receiver for the answer — with a deadline, [`RESPONSE_TIMEOUT`], so a greeter
//! that stops answering cannot pin a thread and a PAM transaction forever.
//!
//! On that deadline wdm composes an `error` prompt of its own before returning
//! `PAM_CONV_ERR`, so not every [`AuthEvent::Prompt`] originates from a PAM
//! module. That matters to readers of the other end: `handle_auth_event` routes
//! wdm's synthetic prompt exactly like PAM's, which is the point — it is the
//! event that tells a greeter the failure was explained and stops it
//! auto-retrying into an account lockout.
//!
//! Cancellation is expressed by dropping [`AuthHandle`]: both command senders
//! close, the conversation's `recv` fails, it returns `PAM_CONV_ERR`, and PAM
//! unwinds on its own. There are no locks and no shared mutable state.
//!
//! The thread also owns the PAM handle for the lifetime of the user's session,
//! because `pam_open_session` and `pam_close_session` must be paired on the same
//! handle. It opens the session, hands the resulting environment to the event
//! loop to `exec` with, and then blocks until told the session ended so it can
//! close the session and end the PAM transaction.

use std::ffi::{CStr, CString, OsStr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use zeroize::Zeroize;

/// PAM service name; wdm ships `/etc/pam.d/wdm`.
pub const SERVICE: &str = "wdm";

/// Source of prompt ids, shared by every attempt in the process.
///
/// Per-attempt counters would restart at 0, so a late `respond` carrying id 0
/// from a cancelled conversation would match the first prompt of the *next*
/// one — feeding one user's secret to another user's PAM stack. An id must
/// identify the question it was issued for, which means it may never be reused.
static NEXT_PROMPT_ID: AtomicU32 = AtomicU32::new(0);

fn next_prompt_id() -> u32 {
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
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

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
    fn pam_items(&self) -> Vec<(&'static str, String)> {
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
    /// The PAM session is open; these variables belong in the session's
    /// environment.
    SessionOpened { env: Vec<(String, String)> },
    /// `pam_open_session` failed and the thread is exiting.
    SessionFailed(String),
}

/// A command for the auth thread.
///
/// Prompt responses travel on their own channel because they must reach the
/// conversation callback while it is blocked inside libpam, whereas these are
/// consumed by the thread's own loop between PAM calls.
#[derive(Debug)]
enum AuthCommand {
    /// Open the PAM session and report its environment.
    ///
    /// Carries the facts that were unknown when the thread started: the
    /// greeter has chosen a session by now, and pam_systemd must hear its type
    /// and desktop before `pam_open_session` or the logind session is
    /// registered with the defaults from [`SessionDescription`].
    StartSession {
        /// `wayland` or `x11`, for `XDG_SESSION_TYPE`.
        session_type: String,
        /// For `XDG_SESSION_DESKTOP`; empty means leave it unset.
        desktop: String,
    },
    /// The user's session process has exited; close the PAM session.
    SessionEnded,
}

/// An answer to a specific prompt.
struct PromptResponse {
    id: u32,
    /// Zeroized on drop: this is frequently a password.
    secret: String,
}

impl Drop for PromptResponse {
    /// Zeroize every path this `String` can take.
    ///
    /// A response can be dropped when the channel is closed (the value comes
    /// back inside `SendError`), when the attempt has already ended, or still
    /// queued in the receiver when the thread unwinds. This covers all of them.
    ///
    /// It does *not* cover the copies downstream: `CString` owns one and libpam
    /// owns another, and neither is scrubbed. Closing that would mean patching
    /// PAM.
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

/// Handle to a running authentication attempt.
///
/// Dropping this cancels the attempt.
pub struct AuthHandle {
    responses: mpsc::Sender<PromptResponse>,
    commands: mpsc::Sender<AuthCommand>,
    /// The user being authenticated, for logging and for the session launch.
    username: String,
}

impl AuthHandle {
    /// Spawn a thread to authenticate `username`.
    ///
    /// `tty` is set as `PAM_TTY` so modules that make policy decisions based on
    /// the terminal (`pam_access`, `pam_time`) see the VT wdm runs on, and
    /// `session` describes the seat to pam_systemd.
    pub fn start(
        username: &str,
        tty: &str,
        session: SessionDescription,
        events: calloop::channel::Sender<AuthEvent>,
    ) -> std::io::Result<Self> {
        let (responses_tx, responses_rx) = mpsc::channel();
        let (commands_tx, commands_rx) = mpsc::channel();

        let thread_user = username.to_owned();
        let thread_tty = tty.to_owned();

        std::thread::Builder::new()
            .name(format!("pam-{username}"))
            .spawn(move || {
                run(
                    &thread_user,
                    &thread_tty,
                    &session,
                    &events,
                    responses_rx,
                    commands_rx,
                );
            })?;

        Ok(Self {
            responses: responses_tx,
            commands: commands_tx,
            username: username.to_owned(),
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Answer the prompt with the given id.
    ///
    /// A closed channel means the thread already gave up, which the event loop
    /// learns about through [`AuthEvent::Failed`]; nothing useful can be done
    /// here.
    pub fn respond(&self, id: u32, secret: String) {
        if self.responses.send(PromptResponse { id, secret }).is_err() {
            log::debug!("response for prompt {id} arrived after the attempt ended");
        }
    }

    /// Ask the thread to open the PAM session for the chosen session.
    ///
    /// Valid only after [`AuthEvent::Ok`]. The thread replies with
    /// [`AuthEvent::SessionOpened`] or [`AuthEvent::SessionFailed`].
    /// `session_type` and `desktop` are the values `crate::session::build_env`
    /// gives the child, delivered here too because pam_systemd reads them from
    /// the PAM environment, not from the child's.
    pub fn start_session(&self, session_type: String, desktop: String) {
        let _ = self.commands.send(AuthCommand::StartSession {
            session_type,
            desktop,
        });
    }

    /// Tell the thread the user's session process has exited.
    ///
    /// This is what closes the PAM session, so it must be sent even when the
    /// session failed immediately, or `pam_close_session` never runs and
    /// modules like `pam_systemd` leak a logind session.
    pub fn session_ended(&self) {
        let _ = self.commands.send(AuthCommand::SessionEnded);
    }
}

/// The auth thread body.
///
/// Runs to completion in one of five ways: authentication fails, session
/// opening fails, the session ends normally, the handle is dropped and every
/// `recv` starts failing, or a prompt goes unanswered for [`RESPONSE_TIMEOUT`]
/// and the conversation abandons it — which reaches the event loop as an
/// ordinary [`AuthEvent::Failed`], preceded by the `error` prompt that says why.
fn run(
    username: &str,
    tty: &str,
    session: &SessionDescription,
    events: &calloop::channel::Sender<AuthEvent>,
    responses: mpsc::Receiver<PromptResponse>,
    commands: mpsc::Receiver<AuthCommand>,
) {
    let conv = ChannelConversation {
        events: events.clone(),
        responses,
        timeout: RESPONSE_TIMEOUT,
    };

    let mut context = match Context::new(SERVICE, Some(username), conv) {
        Ok(context) => context,
        Err(e) => {
            // Almost always a missing /etc/pam.d/wdm. Say so, because the
            // generic PAM message is unhelpful.
            log::error!("opening PAM context for service {SERVICE}: {e}");
            send(events, AuthEvent::Failed(describe(&e)));
            return;
        }
    };

    if let Err(e) = context.set_tty(Some(tty)) {
        // Not fatal: modules that care about the tty are the exception.
        log::warn!("setting PAM_TTY to {tty}: {e}");
    }

    // pam_systemd reads these from the PAM environment, not from the child's.
    // Without them it registers the logind session as Type=tty with no desktop,
    // so `loginctl` misreports it and anything keying off sd_session_get_type —
    // screen lockers, logind's own idle handling — sees a tty session driving a
    // Wayland compositor.
    for (key, value) in session.pam_items() {
        if let Err(e) = context.putenv(format!("{key}={value}").as_str()) {
            log::warn!("setting {key} for PAM: {e}");
        }
    }

    // DISALLOW_NULL_AUTHTOK: an account with an empty password must not be
    // loginable from the greeter.
    if let Err(e) = context.authenticate(Flag::DISALLOW_NULL_AUTHTOK) {
        log::info!("authentication failed for {username}: {e}");
        send(events, AuthEvent::Failed(describe(&e)));
        return;
    }

    match context.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK) {
        Ok(()) => {}
        Err(e) if e.code() == ErrorCode::NEW_AUTHTOK_REQD => {
            // The password is expired. PAM will not let the account in until it
            // is changed, and the change has to run through the same
            // conversation so the greeter can display its prompts. Without this
            // a user with an expired password can never log in.
            log::info!("{username} must change their password before logging in");
            if let Err(e) = context.chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK) {
                log::info!("password change failed for {username}: {e}");
                send(events, AuthEvent::Failed(describe(&e)));
                return;
            }
        }
        Err(e) => {
            log::info!("account management failed for {username}: {e}");
            send(events, AuthEvent::Failed(describe(&e)));
            return;
        }
    }

    send(events, AuthEvent::Ok);

    // Wait for the greeter to choose a session. A closed channel means the
    // attempt was cancelled or the greeter died; unwinding here runs pam_end.
    let (session_type, desktop) = loop {
        match commands.recv() {
            Ok(AuthCommand::StartSession {
                session_type,
                desktop,
            }) => break (session_type, desktop),
            Ok(AuthCommand::SessionEnded) => {
                // No session was ever started; nothing to close.
                log::debug!("session_ended before start_session for {username}");
            }
            Err(_) => {
                log::debug!("authentication for {username} cancelled before launch");
                return;
            }
        }
    };

    // The greeter's choice supersedes the wayland-with-no-desktop defaults set
    // before authentication. This must land before open_session: pam_systemd
    // registers the logind session there, and a session registered as the wrong
    // type misleads everything keying off sd_session_get_type.
    for pair in pam_session_env(&session_type, &desktop) {
        if let Err(e) = context.putenv(pair.as_str()) {
            // Not fatal: a cosmetic environment variable must not block the
            // handoff. But it is not cosmetic to logind — open_session below
            // registers the session with whatever survived, so say which
            // variable was lost and what the machine will believe instead.
            log::error!(
                "putenv {pair} for {username} failed: {e}; \
                 the logind session will be registered with the wrong type"
            );
        }
    }

    let session = match context.open_session(Flag::NONE) {
        Ok(session) => session,
        Err(e) => {
            log::error!("opening PAM session for {username}: {e}");
            send(events, AuthEvent::SessionFailed(describe(&e)));
            return;
        }
    };

    let env = session
        .envlist()
        .iter_tuples()
        .filter_map(|(key, value)| Some((os_to_string(key)?, os_to_string(value)?)))
        .collect();

    send(events, AuthEvent::SessionOpened { env });

    // Hold the PAM session open for as long as the user's session runs. Ending
    // here — on an explicit SessionEnded or on the handle being dropped —
    // closes the session and then ends the transaction, in that order, which is
    // what pam_systemd needs to release the logind session.
    match commands.recv() {
        Ok(AuthCommand::SessionEnded) => log::info!("session for {username} ended"),
        Ok(AuthCommand::StartSession { .. }) => {
            log::warn!("ignoring second start_session for {username}");
        }
        Err(_) => log::info!("supervisor dropped the PAM handle for {username}"),
    }

    drop(session);
}

/// The `KEY=VALUE` pairs to push into PAM's environment before `open_session`.
///
/// An empty value yields no pair at all rather than `KEY=`: a session with no
/// desktop must leave `XDG_SESSION_DESKTOP` *unset*, because an empty string is
/// a value logind will happily record and everything reading it will believe.
fn pam_session_env(session_type: &str, desktop: &str) -> Vec<String> {
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
fn describe(e: &pam_client2::Error) -> String {
    e.message()
        .map(str::to_owned)
        .unwrap_or_else(|| e.to_string())
}

fn os_to_string(s: &OsStr) -> Option<String> {
    match s.to_str() {
        Some(s) => Some(s.to_owned()),
        None => {
            log::warn!("dropping non-UTF-8 PAM environment entry");
            None
        }
    }
}

/// Bridges libpam's blocking conversation to the event loop.
struct ChannelConversation {
    events: calloop::channel::Sender<AuthEvent>,
    responses: mpsc::Receiver<PromptResponse>,
    /// How long to wait for each response. Always [`RESPONSE_TIMEOUT`] in
    /// production; a field so tests can reach the timeout path, which at half an
    /// hour is otherwise unreachable in a test suite.
    timeout: Duration,
}

impl ChannelConversation {
    /// Emit a prompt and block until the matching response arrives.
    fn ask(&mut self, prompt: &CStr, style: PromptStyle) -> Result<CString, ErrorCode> {
        let id = next_prompt_id();
        self.emit(id, prompt, style)?;

        let deadline = std::time::Instant::now() + self.timeout;

        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let response = match self.responses.recv_timeout(remaining) {
                Ok(response) => response,
                Err(mpsc::RecvTimeoutError::Timeout) => {
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
                    // their form reset and stops the loop, in every greeter that
                    // shares the client — rather than in whichever ones remember
                    // to special-case a reason string.
                    self.emit_text(
                        next_prompt_id(),
                        "The login attempt timed out waiting for a response.".to_owned(),
                        PromptStyle::Error,
                    )?;

                    return Err(ErrorCode::CONV_ERR);
                }
                // The handle was dropped: cancelled, or the greeter died.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(ErrorCode::CONV_ERR),
            };

            if response.id != id {
                // A response the greeter sent for a prompt that has already been
                // superseded. The protocol raises stale_prompt for this, but a
                // race can still deliver one legitimately, so drop it rather
                // than failing the whole attempt. Drop zeroizes it.
                log::debug!("discarding response for stale prompt {}", response.id);
                continue;
            }

            // CString rejects interior NUL, which cannot be part of a password
            // PAM could ever verify.
            //
            // The String is zeroized when `response` drops. The CString this
            // makes is not, and neither is the copy libpam takes of it, so the
            // plaintext survives in freed heap until something reuses it. That
            // is the accepted ceiling: pam_authenticate frees the answer itself
            // and there is no hook to scrub it first. Copying the bytes only to
            // zeroize the copy would not narrow anything — it would add a
            // scrubbed allocation beside the unscrubbed one that matters.
            return CString::new(response.secret.as_bytes()).map_err(|_| {
                log::info!("response to prompt {id} contained a NUL byte");
                ErrorCode::CONV_ERR
            });
        }
    }

    /// Emit a message the greeter is not expected to answer.
    fn tell(&mut self, msg: &CStr, style: PromptStyle) {
        // Nothing to do on failure: PAM's text_info and error_msg cannot fail.
        let _ = self.emit(next_prompt_id(), msg, style);
    }

    fn emit(&self, id: u32, text: &CStr, style: PromptStyle) -> Result<(), ErrorCode> {
        // PAM messages come from modules and are not guaranteed UTF-8.
        self.emit_text(id, text.to_string_lossy().into_owned(), style)
    }

    /// Emit text wdm composed itself, which is already UTF-8.
    fn emit_text(&self, id: u32, text: String, style: PromptStyle) -> Result<(), ErrorCode> {
        // The prompt text, never a response: this is what PAM asked, and the
        // answers are the one thing in this file that must not reach a log.
        // Without this there is no way to see what a PAM stack actually sent,
        // which is the first question every "the greeter shows something odd"
        // report needs answered.
        log::debug!("prompt {id} ({style:?}): {text:?}");
        self.events
            .send(AuthEvent::Prompt { id, text, style })
            .map_err(|_| {
                log::debug!("event loop is gone, abandoning conversation");
                ErrorCode::CONV_ERR
            })
    }
}

impl ConversationHandler for ChannelConversation {
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

    /// Drive the conversation directly, without libpam, to check the id matching
    /// and cancellation behaviour the protocol depends on.
    fn conversation() -> (
        ChannelConversation,
        mpsc::Sender<PromptResponse>,
        calloop::channel::Channel<AuthEvent>,
    ) {
        conversation_with_timeout(RESPONSE_TIMEOUT)
    }

    /// The same, with a timeout a test can actually reach.
    fn conversation_with_timeout(
        timeout: Duration,
    ) -> (
        ChannelConversation,
        mpsc::Sender<PromptResponse>,
        calloop::channel::Channel<AuthEvent>,
    ) {
        let (events_tx, events_rx) = calloop::channel::channel();
        let (responses_tx, responses_rx) = mpsc::channel();
        let conv = ChannelConversation {
            events: events_tx,
            responses: responses_rx,
            timeout,
        };
        (conv, responses_tx, events_rx)
    }

    /// Reads prompts off the event channel as they are emitted.
    ///
    /// Prompt ids come from a process-global counter, so a test cannot predict
    /// them — it has to read them back off the wire, which is what a real
    /// greeter does too.
    struct PromptStream {
        event_loop: calloop::EventLoop<'static, Vec<(u32, String, PromptStyle)>>,
        seen: Vec<(u32, String, PromptStyle)>,
        taken: usize,
    }

    impl PromptStream {
        fn new(channel: calloop::channel::Channel<AuthEvent>) -> Self {
            let event_loop = calloop::EventLoop::try_new().unwrap();
            event_loop
                .handle()
                .insert_source(channel, |event, _, seen: &mut Vec<_>| {
                    if let calloop::channel::Event::Msg(AuthEvent::Prompt { id, text, style }) =
                        event
                    {
                        seen.push((id, text, style));
                    }
                })
                .unwrap();
            Self {
                event_loop,
                seen: Vec::new(),
                taken: 0,
            }
        }

        /// Block until the next prompt arrives.
        fn next(&mut self) -> (u32, String, PromptStyle) {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.seen.len() <= self.taken {
                assert!(std::time::Instant::now() < deadline, "prompt never arrived");
                self.event_loop
                    .dispatch(Duration::from_millis(20), &mut self.seen)
                    .unwrap();
            }
            self.taken += 1;
            self.seen[self.taken - 1].clone()
        }
    }

    #[test]
    fn answers_a_prompt() {
        let (mut conv, responses, events) = conversation();
        let worker = std::thread::spawn(move || conv.prompt_echo_off(c"Password:"));

        let (id, _, style) = PromptStream::new(events).next();
        assert_eq!(style, PromptStyle::Secret);
        responses
            .send(PromptResponse {
                id,
                secret: "hunter2".to_owned(),
            })
            .unwrap();

        let answer = worker.join().unwrap().unwrap();
        assert_eq!(answer.to_str().unwrap(), "hunter2");
    }

    #[test]
    fn a_timeout_explains_itself_before_failing() {
        // The regression this exists for: a prompt nobody answers used to fail
        // the attempt with nothing said to the greeter. PAM logs "conversation
        // failed" to the journal and the greeter sees only a bare auth_failed —
        // which is the same shape as a mistyped password, so every greeter
        // sharing wdm-greeter-client retried on its own, re-armed the timeout,
        // and spun. Each turn of that loop is one pam_faillock entry, so an
        // untouched machine locked its first user out.
        //
        // The `error` prompt below is what breaks it: it reaches
        // Model::push_notice, which sets `blocked`, which makes
        // Model::should_auto_retry false. Assert the style, not just that
        // something was said — an `info` prompt would display identically and
        // would not stop the loop.
        let (mut conv, _responses, events) = conversation_with_timeout(Duration::from_millis(50));
        let worker = std::thread::spawn(move || conv.prompt_echo_off(c"Password:"));

        let mut prompts = PromptStream::new(events);

        let (_, text, style) = prompts.next();
        assert_eq!(text, "Password:");
        assert_eq!(style, PromptStyle::Secret);

        let (_, text, style) = prompts.next();
        assert_eq!(style, PromptStyle::Error);
        assert!(
            text.contains("timed out"),
            "the timeout notice should say what happened, got {text:?}"
        );

        assert_eq!(worker.join().unwrap().unwrap_err(), ErrorCode::CONV_ERR);
    }

    #[test]
    fn ignores_stale_responses_and_keeps_waiting() {
        let (mut conv, responses, events) = conversation();
        let worker = std::thread::spawn(move || conv.prompt_echo_off(c"Password:"));

        let (id, _, _) = PromptStream::new(events).next();

        // An answer to a prompt that no longer exists, then the real one.
        responses
            .send(PromptResponse {
                id: id.wrapping_add(999),
                secret: "stale".to_owned(),
            })
            .unwrap();
        responses
            .send(PromptResponse {
                id,
                secret: "fresh".to_owned(),
            })
            .unwrap();

        let answer = worker.join().unwrap().unwrap();
        assert_eq!(answer.to_str().unwrap(), "fresh");
    }

    #[test]
    fn stale_responses_cannot_extend_the_deadline() {
        // RESPONSE_TIMEOUT is documented as measured from when the prompt was
        // emitted, not from the last message received, and that is the whole
        // leak guard: a peer that can restart the clock by sending anything can
        // pin the thread and the PAM transaction forever. The guarantee comes
        // from computing `deadline` once, before the loop — move it inside and
        // both of the other timeout tests stay green, because one never
        // approaches the deadline and the other sends nothing at all.
        let (mut conv, responses, events) = conversation_with_timeout(Duration::from_millis(50));
        let worker = std::thread::spawn(move || conv.prompt_echo_off(c"Password:"));

        let mut prompts = PromptStream::new(events);
        let (id, _, _) = prompts.next();

        // Mismatched ids, so each one is discarded and the loop goes round
        // again. Faster than the timeout, so a per-message deadline would never
        // be reached and this test would hang rather than fail — which the join
        // below turns into a visible timeout either way.
        let chatter = std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < until {
                if responses
                    .send(PromptResponse {
                        id: id.wrapping_add(1000),
                        secret: "stale".to_owned(),
                    })
                    .is_err()
                {
                    // The conversation gave up and dropped the receiver, which
                    // is the outcome under test.
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let started = std::time::Instant::now();
        assert_eq!(worker.join().unwrap().unwrap_err(), ErrorCode::CONV_ERR);
        let waited = started.elapsed();
        chatter.join().unwrap();

        // Near the deadline, not a multiple of it. Generous upwards because a
        // loaded CI box schedules threads late; the assertion that matters is
        // that it did not run for the two seconds of chatter.
        assert!(
            waited < Duration::from_millis(1500),
            "stale responses extended the deadline: waited {waited:?}"
        );
    }

    #[test]
    fn a_multi_prompt_conversation_uses_distinct_ids() {
        // PAM asks in sequence — password, then a token, then an expiry notice.
        // Each question must carry its own id or a slow greeter could answer the
        // wrong one.
        let (mut conv, responses, events) = conversation();
        let worker = std::thread::spawn(move || {
            let first = conv.prompt_echo_on(c"Username:")?;
            let second = conv.prompt_echo_off(c"Password:")?;
            Ok::<_, ErrorCode>((first, second))
        });

        let mut prompts = PromptStream::new(events);

        let (first_id, text, style) = prompts.next();
        assert_eq!(text, "Username:");
        assert_eq!(style, PromptStyle::Visible);
        responses
            .send(PromptResponse {
                id: first_id,
                secret: "testuser".to_owned(),
            })
            .unwrap();

        let (second_id, _, style) = prompts.next();
        assert_eq!(style, PromptStyle::Secret);
        assert_ne!(
            first_id, second_id,
            "ids must not repeat within a conversation"
        );
        responses
            .send(PromptResponse {
                id: second_id,
                secret: "hunter2".to_owned(),
            })
            .unwrap();

        let (username, password) = worker.join().unwrap().unwrap();
        assert_eq!(username.to_str().unwrap(), "testuser");
        assert_eq!(password.to_str().unwrap(), "hunter2");
    }

    #[test]
    fn cancellation_aborts_the_conversation() {
        let (mut conv, responses, _events) = conversation();
        // Dropping the sender is how cancel() and greeter death both present.
        drop(responses);
        assert_eq!(
            conv.prompt_echo_off(c"Password:").unwrap_err(),
            ErrorCode::CONV_ERR
        );
    }

    #[test]
    fn rejects_response_containing_nul() {
        let (mut conv, responses, events) = conversation();
        let worker = std::thread::spawn(move || conv.prompt_echo_off(c"Password:"));

        let (id, _, _) = PromptStream::new(events).next();
        responses
            .send(PromptResponse {
                id,
                secret: "bad\0password".to_owned(),
            })
            .unwrap();

        assert_eq!(worker.join().unwrap().unwrap_err(), ErrorCode::CONV_ERR);
    }

    #[test]
    fn informational_messages_need_no_response() {
        let (mut conv, _responses, events) = conversation();
        // text_info and error_msg must not block waiting for an answer.
        conv.text_info(c"Welcome");
        conv.error_msg(c"Nope");

        let mut prompts = PromptStream::new(events);
        let (info_id, text, style) = prompts.next();
        assert_eq!(text, "Welcome");
        assert_eq!(style, PromptStyle::Info);

        let (error_id, text, style) = prompts.next();
        assert_eq!(text, "Nope");
        assert_eq!(style, PromptStyle::Error);
        assert!(error_id > info_id, "ids must advance");
    }

    #[test]
    fn handles_non_utf8_pam_messages() {
        let (mut conv, responses, events) = conversation();
        // Modules are not required to emit UTF-8; this must not panic.
        let worker = std::thread::spawn(move || {
            let msg = CString::new([0xff, 0xfe, b'?']).unwrap();
            conv.prompt_echo_off(&msg)
        });

        let (id, _, _) = PromptStream::new(events).next();
        responses
            .send(PromptResponse {
                id,
                secret: "x".to_owned(),
            })
            .unwrap();

        assert!(worker.join().unwrap().is_ok());
    }
}
