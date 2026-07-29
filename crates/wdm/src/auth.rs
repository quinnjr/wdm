//! PAM authentication, and the only place wdm uses threads.
//!
//! `pam_authenticate` blocks, and its conversation callback is a C callback
//! invoked from inside libpam. It cannot be driven from an event loop, so each
//! authentication attempt gets a thread. The conversation forwards each question
//! to the event loop over a [`calloop::channel`] and blocks on an [`mpsc`]
//! receiver for the answer.
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
use std::sync::mpsc;
use std::time::Duration;

use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use zeroize::Zeroize;

/// PAM service name; wdm ships `/etc/pam.d/wdm`.
pub const SERVICE: &str = "wdm";

/// How long a prompt may go unanswered before the attempt is abandoned.
///
/// A greeter that stops answering would otherwise pin a thread and a PAM
/// transaction forever.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

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
    StartSession,
    /// The user's session process has exited; close the PAM session.
    SessionEnded,
}

/// An answer to a specific prompt.
struct PromptResponse {
    id: u32,
    /// Zeroized after being handed to PAM: this is frequently a password.
    secret: String,
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
    /// the terminal (`pam_access`, `pam_time`) see the VT wdm runs on.
    pub fn start(
        username: &str,
        tty: &str,
        events: calloop::channel::Sender<AuthEvent>,
    ) -> std::io::Result<Self> {
        let (responses_tx, responses_rx) = mpsc::channel();
        let (commands_tx, commands_rx) = mpsc::channel();

        let thread_user = username.to_owned();
        let thread_tty = tty.to_owned();

        std::thread::Builder::new()
            .name(format!("pam-{username}"))
            .spawn(move || {
                run(&thread_user, &thread_tty, &events, responses_rx, commands_rx);
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

    /// Ask the thread to open the PAM session.
    ///
    /// Valid only after [`AuthEvent::Ok`]. The thread replies with
    /// [`AuthEvent::SessionOpened`] or [`AuthEvent::SessionFailed`].
    pub fn start_session(&self) {
        let _ = self.commands.send(AuthCommand::StartSession);
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
/// Runs to completion in one of four ways: authentication fails, session
/// opening fails, the session ends normally, or the handle is dropped and every
/// `recv` starts failing.
fn run(
    username: &str,
    tty: &str,
    events: &calloop::channel::Sender<AuthEvent>,
    responses: mpsc::Receiver<PromptResponse>,
    commands: mpsc::Receiver<AuthCommand>,
) {
    let conv = ChannelConversation {
        events: events.clone(),
        responses,
        next_id: 0,
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
    loop {
        match commands.recv() {
            Ok(AuthCommand::StartSession) => break,
            Ok(AuthCommand::SessionEnded) => {
                // No session was ever started; nothing to close.
                log::debug!("session_ended before start_session for {username}");
            }
            Err(_) => {
                log::debug!("authentication for {username} cancelled before launch");
                return;
            }
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
        Ok(AuthCommand::StartSession) => {
            log::warn!("ignoring second start_session for {username}");
        }
        Err(_) => log::info!("supervisor dropped the PAM handle for {username}"),
    }

    drop(session);
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
    next_id: u32,
}

impl ChannelConversation {
    /// Emit a prompt and block until the matching response arrives.
    fn ask(&mut self, prompt: &CStr, style: PromptStyle) -> Result<CString, ErrorCode> {
        let id = self.next_id;
        // Wrapping is unreachable in practice but must not panic on a login
        // screen; the id only has to be unique among live prompts.
        self.next_id = self.next_id.wrapping_add(1);

        self.emit(id, prompt, style)?;

        loop {
            let mut response = match self.responses.recv_timeout(RESPONSE_TIMEOUT) {
                Ok(response) => response,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log::info!("no response to prompt {id} within {RESPONSE_TIMEOUT:?}");
                    return Err(ErrorCode::CONV_ERR);
                }
                // The handle was dropped: cancelled, or the greeter died.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(ErrorCode::CONV_ERR),
            };

            if response.id != id {
                // A response the greeter sent for a prompt that has already been
                // superseded. The protocol raises stale_prompt for this, but a
                // race can still deliver one legitimately, so drop it rather
                // than failing the whole attempt.
                log::debug!("discarding response for stale prompt {}", response.id);
                response.secret.zeroize();
                continue;
            }

            // CString rejects interior NUL, which cannot be part of a password
            // PAM could ever verify.
            let result = CString::new(response.secret.as_bytes()).map_err(|_| {
                log::info!("response to prompt {id} contained a NUL byte");
                ErrorCode::CONV_ERR
            });
            response.secret.zeroize();
            return result;
        }
    }

    /// Emit a message the greeter is not expected to answer.
    fn tell(&mut self, msg: &CStr, style: PromptStyle) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        // Nothing to do on failure: PAM's text_info and error_msg cannot fail.
        let _ = self.emit(id, msg, style);
    }

    fn emit(&self, id: u32, text: &CStr, style: PromptStyle) -> Result<(), ErrorCode> {
        // PAM messages come from modules and are not guaranteed UTF-8.
        let text = text.to_string_lossy().into_owned();
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
    fn only_prompts_expect_responses() {
        assert!(PromptStyle::Secret.expects_response());
        assert!(PromptStyle::Visible.expects_response());
        assert!(!PromptStyle::Info.expects_response());
        assert!(!PromptStyle::Error.expects_response());
    }

    /// Drive the conversation directly, without libpam, to check the id
    /// matching and cancellation behaviour that the protocol depends on.
    fn conversation() -> (
        ChannelConversation,
        mpsc::Sender<PromptResponse>,
        calloop::channel::Channel<AuthEvent>,
    ) {
        let (events_tx, events_rx) = calloop::channel::channel();
        let (responses_tx, responses_rx) = mpsc::channel();
        let conv = ChannelConversation {
            events: events_tx,
            responses: responses_rx,
            next_id: 0,
        };
        (conv, responses_tx, events_rx)
    }

    #[test]
    fn answers_a_prompt() {
        let (mut conv, responses, _events) = conversation();
        responses
            .send(PromptResponse {
                id: 0,
                secret: "hunter2".to_owned(),
            })
            .unwrap();

        let answer = conv.prompt_echo_off(c"Password:").unwrap();
        assert_eq!(answer.to_str().unwrap(), "hunter2");
    }

    #[test]
    fn ignores_stale_responses_and_keeps_waiting() {
        let (mut conv, responses, _events) = conversation();
        // The greeter answers a prompt that no longer exists, then the real one.
        responses
            .send(PromptResponse {
                id: 99,
                secret: "stale".to_owned(),
            })
            .unwrap();
        responses
            .send(PromptResponse {
                id: 0,
                secret: "fresh".to_owned(),
            })
            .unwrap();

        let answer = conv.prompt_echo_off(c"Password:").unwrap();
        assert_eq!(answer.to_str().unwrap(), "fresh");
    }

    #[test]
    fn prompt_ids_advance() {
        let (mut conv, responses, _events) = conversation();
        for id in 0..3 {
            responses
                .send(PromptResponse {
                    id,
                    secret: format!("s{id}"),
                })
                .unwrap();
            assert_eq!(
                conv.prompt_echo_on(c"Token:").unwrap().to_str().unwrap(),
                format!("s{id}")
            );
        }
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
        let (mut conv, responses, _events) = conversation();
        responses
            .send(PromptResponse {
                id: 0,
                secret: "bad\0password".to_owned(),
            })
            .unwrap();
        assert_eq!(
            conv.prompt_echo_off(c"Password:").unwrap_err(),
            ErrorCode::CONV_ERR
        );
    }

    #[test]
    fn emits_prompts_to_the_event_loop() {
        let (mut conv, responses, events) = conversation();
        responses
            .send(PromptResponse {
                id: 0,
                secret: "x".to_owned(),
            })
            .unwrap();
        conv.prompt_echo_off(c"Password:").unwrap();
        conv.text_info(c"Welcome");
        conv.error_msg(c"Nope");

        // The channel is an event source, so draining it means running a loop.
        let mut event_loop: calloop::EventLoop<Vec<(u32, String, PromptStyle)>> =
            calloop::EventLoop::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(events, |event, _, seen| {
                if let calloop::channel::Event::Msg(AuthEvent::Prompt { id, text, style }) = event {
                    seen.push((id, text, style));
                }
            })
            .unwrap();

        let mut seen = Vec::new();
        while seen.len() < 3 {
            event_loop
                .dispatch(Duration::from_millis(50), &mut seen)
                .unwrap();
        }

        assert_eq!(
            seen,
            vec![
                (0, "Password:".to_owned(), PromptStyle::Secret),
                (1, "Welcome".to_owned(), PromptStyle::Info),
                (2, "Nope".to_owned(), PromptStyle::Error),
            ]
        );
    }

    #[test]
    fn handles_non_utf8_pam_messages() {
        let (mut conv, responses, _events) = conversation();
        responses
            .send(PromptResponse {
                id: 0,
                secret: "x".to_owned(),
            })
            .unwrap();
        // Modules are not required to emit UTF-8; this must not panic.
        let msg = CString::new([0xff, 0xfe, b'?']).unwrap();
        assert!(conv.prompt_echo_off(&msg).is_ok());
    }
}
