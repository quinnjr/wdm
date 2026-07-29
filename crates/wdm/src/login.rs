//! Server side of `wdm_greeter_v1`: the login state machine.
//!
//! The greeter is untrusted. Every request is validated against the phase the
//! conversation is actually in, prompts are id-tagged so a slow greeter cannot
//! answer a superseded question, and failed attempts are rate limited here
//! rather than in the greeter.

use std::path::PathBuf;
use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use wdm_protocol::server::wdm_greeter_v1::{self, WdmGreeterV1};

use crate::auth::{AuthEvent, AuthHandle};
use crate::comp::{LoopData, Wdm};
use crate::sessions::Session;
use crate::users::{LastSessions, User};

/// The scanner generates typed enums from the protocol XML, so these conversions
/// are where wdm's internal types meet the wire. Keeping them explicit means a
/// new variant on either side is a compile error rather than a wrong number.
fn wire_session_type(t: crate::sessions::SessionType) -> wdm_greeter_v1::SessionType {
    match t {
        crate::sessions::SessionType::Wayland => wdm_greeter_v1::SessionType::Wayland,
        crate::sessions::SessionType::X11 => wdm_greeter_v1::SessionType::X11,
    }
}

fn wire_prompt_style(style: crate::auth::PromptStyle) -> wdm_greeter_v1::PromptStyle {
    match style {
        crate::auth::PromptStyle::Secret => wdm_greeter_v1::PromptStyle::Secret,
        crate::auth::PromptStyle::Visible => wdm_greeter_v1::PromptStyle::Visible,
        crate::auth::PromptStyle::Info => wdm_greeter_v1::PromptStyle::Info,
        crate::auth::PromptStyle::Error => wdm_greeter_v1::PromptStyle::Error,
    }
}

/// Backoff applied after each consecutive failed attempt, in seconds.
///
/// The delay is applied to the *failure response*, which is what actually slows
/// a brute force attempt: the greeter cannot try again until it has been told
/// the previous try failed. Capped so a user who mistyped a few times is not
/// locked out of their own machine for minutes.
const BACKOFF_SECS: &[u64] = &[0, 1, 2, 4, 8, 10];

/// Where the conversation currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No conversation; `create_session` is allowed.
    Idle,
    /// PAM is running; `respond` and `cancel` are allowed.
    Authenticating,
    /// Authenticated; `start_session` is allowed.
    Authenticated,
    /// A failure is being reported after its backoff delay. `create_session` is
    /// refused until the delay elapses, which is the rate limit.
    Cooldown,
    /// A session is being launched; the greeter is about to be torn down.
    Launching,
}

/// What the event loop must do after handling a greeter request or auth event.
///
/// Returned rather than performed inline because launching needs to release DRM
/// master and tear down the greeter, which only the backend can do.
#[derive(Debug)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Authentication succeeded and a session should be launched.
    Launch(LaunchRequest),
    /// The greeter should be restarted, optionally told why.
    RestartGreeter { error: Option<String> },
}

/// A validated request to launch a session.
#[derive(Debug)]
pub struct LaunchRequest {
    pub username: String,
    pub session: Session,
    /// Environment from `pam_open_session`.
    pub pam_env: Vec<(String, String)>,
    /// Environment the greeter asked for.
    pub extra_env: Vec<(String, String)>,
}

/// The login state machine and the data the enumerate phase advertises.
pub struct Login {
    users: Vec<User>,
    sessions: Vec<Session>,
    default_session: Option<String>,
    /// Why the previous launch attempt failed, advertised once to the next
    /// greeter so the user is not bounced back with no explanation.
    last_error: Option<String>,
    last_sessions_path: PathBuf,
    vt: u32,

    /// Greeter objects currently bound to the global.
    bound: Vec<WdmGreeterV1>,
    /// Outputs in rank order, for `output_rank`.
    ranked_outputs: Vec<Output>,

    phase: Phase,
    auth: Option<AuthHandle>,
    /// The prompt the greeter is expected to answer.
    pending_prompt: Option<u32>,
    /// Consecutive failures, indexing into [`BACKOFF_SECS`].
    failures: usize,
    /// Session chosen by `start_session`, held until PAM reports the session
    /// environment.
    chosen: Option<(Session, Vec<(String, String)>)>,

    events: smithay::reexports::calloop::channel::Sender<AuthEvent>,
    loop_handle: LoopHandle<'static, LoopData>,
}

impl Login {
    pub fn new(
        users: Vec<User>,
        sessions: Vec<Session>,
        default_session: Option<String>,
        last_sessions_path: PathBuf,
        vt: u32,
        events: smithay::reexports::calloop::channel::Sender<AuthEvent>,
        loop_handle: LoopHandle<'static, LoopData>,
    ) -> Self {
        Self {
            users,
            sessions,
            default_session,
            last_error: None,
            last_sessions_path,
            vt,
            bound: Vec::new(),
            ranked_outputs: Vec::new(),
            phase: Phase::Idle,
            auth: None,
            pending_prompt: None,
            failures: 0,
            chosen: None,
            events,
            loop_handle,
        }
    }

    /// Advertise the global.
    pub fn create_global(display: &DisplayHandle) {
        display.create_global::<Wdm, WdmGreeterV1, _>(1, ());
    }

    /// Record why the previous launch failed, for the next greeter to display.
    pub fn set_last_error(&mut self, error: Option<String>) {
        self.last_error = error;
    }

    /// Reset conversation state, used when the greeter is restarted.
    pub fn reset(&mut self) {
        self.bound.clear();
        self.phase = Phase::Idle;
        // Dropping the handle cancels any conversation in flight.
        self.auth = None;
        self.pending_prompt = None;
        self.chosen = None;
    }

    /// Refresh the users list, picking up new accounts and session history.
    pub fn refresh_users(&mut self, range: crate::users::UidRange) {
        let history = LastSessions::load(&self.last_sessions_path);
        self.users = crate::users::discover(range, &history);
    }

    /// Update output ranks and push them to any bound greeter.
    pub fn set_output_ranks(&mut self, outputs: &[Output]) {
        self.ranked_outputs = outputs.to_vec();
        for greeter in self.bound.clone() {
            self.send_output_ranks(&greeter);
        }
    }

    fn send_output_ranks(&self, greeter: &WdmGreeterV1) {
        let Ok(client) = greeter.client().ok_or(()) else {
            return;
        };
        for (rank, output) in self.ranked_outputs.iter().enumerate() {
            // An output the greeter has not been told about yet has no resource
            // for this client; skip it rather than inventing one.
            for wl_output in output.client_outputs(&client) {
                greeter.output_rank(&wl_output, rank as u32);
            }
        }
    }

    /// Push the enumerate phase to a freshly bound greeter.
    fn send_initial_state(&mut self, greeter: &WdmGreeterV1) {
        for user in &self.users {
            greeter.user(
                user.name.clone(),
                user.display_name.clone(),
                user.avatar_path.clone(),
                // Fall back to the configured default so a first-time user is
                // offered something sensible. Preselecting is the greeter's
                // policy; wdm only reports.
                if user.last_session.is_empty() {
                    self.default_session.clone().unwrap_or_default()
                } else {
                    user.last_session.clone()
                },
            );
        }

        for session in &self.sessions {
            greeter.session(
                session.id.clone(),
                session.name.clone(),
                session.exec.clone(),
                wire_session_type(session.session_type),
            );
        }

        self.send_output_ranks(greeter);

        // Sent once: the next greeter should not be told about an error the user
        // has already seen.
        if let Some(error) = self.last_error.take() {
            greeter.last_error(error);
        }

        greeter.done();
    }

    fn broadcast(&self, f: impl Fn(&WdmGreeterV1)) {
        for greeter in &self.bound {
            f(greeter);
        }
    }

    /// Handle an event from the PAM thread.
    pub fn handle_auth_event(&mut self, event: AuthEvent) -> Action {
        match event {
            AuthEvent::Prompt { id, text, style } => {
                self.pending_prompt = if style.expects_response() {
                    Some(id)
                } else {
                    // Informational messages are not answered, so the previous
                    // question stays pending.
                    self.pending_prompt
                };
                self.broadcast(|g| g.prompt(id, text.clone(), wire_prompt_style(style)));
                Action::None
            }

            AuthEvent::Ok => {
                self.phase = Phase::Authenticated;
                self.pending_prompt = None;
                self.failures = 0;
                self.broadcast(|g| g.auth_ok());
                Action::None
            }

            AuthEvent::Failed(reason) => {
                self.auth = None;
                self.pending_prompt = None;
                self.phase = Phase::Cooldown;

                let delay = self.next_backoff();
                self.failures = self.failures.saturating_add(1);

                // Reporting the failure late is the rate limit: the greeter
                // cannot start another attempt until it hears about this one.
                let timer = Timer::from_duration(delay);
                let result = self.loop_handle.insert_source(timer, move |_, _, data| {
                    data.state.login.phase = Phase::Idle;
                    data.state.login.broadcast(|g| g.auth_failed(reason.clone()));
                    TimeoutAction::Drop
                });

                if let Err(e) = result {
                    // Without a timer the failure would never be reported and the
                    // greeter would hang, so report it immediately instead.
                    log::error!("arming backoff timer: {e}");
                    self.phase = Phase::Idle;
                    self.broadcast(|g| g.auth_failed("authentication failed".to_owned()));
                }

                Action::None
            }

            AuthEvent::SessionOpened { env } => {
                let Some(auth) = &self.auth else {
                    log::error!("PAM session opened with no attempt in flight");
                    return Action::RestartGreeter { error: None };
                };
                let Some((session, extra_env)) = self.chosen.take() else {
                    log::error!("PAM session opened with no session chosen");
                    return Action::RestartGreeter { error: None };
                };

                Action::Launch(LaunchRequest {
                    username: auth.username().to_owned(),
                    session,
                    pam_env: env,
                    extra_env,
                })
            }

            AuthEvent::SessionFailed(reason) => {
                self.auth = None;
                self.chosen = None;
                self.phase = Phase::Idle;
                Action::RestartGreeter {
                    error: Some(reason),
                }
            }
        }
    }

    fn next_backoff(&self) -> Duration {
        let index = self.failures.min(BACKOFF_SECS.len() - 1);
        Duration::from_secs(BACKOFF_SECS[index])
    }

    /// Tell the PAM thread the user's session process has exited.
    ///
    /// This is what runs `pam_close_session`. Skipping it leaks a logind session
    /// per login, because pam_systemd only releases one when the session closes.
    pub fn end_session(&mut self) {
        if let Some(auth) = &self.auth {
            auth.session_ended();
        }
        self.auth = None;
    }

    /// Record a successful launch so the session is preselected next time.
    pub fn remember_session(&mut self, username: &str, session_id: &str) {
        let mut history = LastSessions::load(&self.last_sessions_path);
        history.set(username, session_id);
        if let Err(e) = history.save(&self.last_sessions_path) {
            // Losing the preference is cosmetic; the login still succeeded.
            log::warn!(
                "recording last session in {}: {e}",
                self.last_sessions_path.display()
            );
        }
    }
}

impl GlobalDispatch<WdmGreeterV1, ()> for Wdm {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WdmGreeterV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let greeter = data_init.init(resource, ());
        state.login.bound.push(greeter.clone());
        state.login.send_initial_state(&greeter);
    }
}

impl Dispatch<WdmGreeterV1, ()> for Wdm {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &WdmGreeterV1,
        request: wdm_greeter_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let action = match request {
            wdm_greeter_v1::Request::CreateSession { username } => {
                state.create_session(resource, username)
            }
            wdm_greeter_v1::Request::Respond { id, response } => {
                state.respond(resource, id, response)
            }
            wdm_greeter_v1::Request::Cancel => {
                state.login.auth = None;
                state.login.pending_prompt = None;
                // Cooldown must survive a cancel, or a greeter could cancel its
                // way out of the rate limit.
                if state.login.phase == Phase::Authenticating
                    || state.login.phase == Phase::Authenticated
                {
                    state.login.phase = Phase::Idle;
                }
                Action::None
            }
            wdm_greeter_v1::Request::StartSession { session_id, env } => {
                state.start_session(resource, session_id, env)
            }
            wdm_greeter_v1::Request::Destroy => {
                state.login.reset();
                Action::None
            }
            _ => Action::None,
        };

        state.pending_action = action;
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &WdmGreeterV1, _data: &()) {
        state.login.bound.retain(|g| g != resource);
    }
}

use smithay::reexports::wayland_server::backend::ClientId;

impl Wdm {
    fn create_session(&mut self, resource: &WdmGreeterV1, username: String) -> Action {
        match self.login.phase {
            Phase::Idle => {}
            Phase::Cooldown => {
                // Not a protocol error: the greeter is allowed to try again, just
                // not yet. Telling it so keeps it from hanging.
                resource.auth_failed("too many attempts, try again shortly".to_owned());
                return Action::None;
            }
            _ => {
                resource.post_error(
                    wdm_greeter_v1::Error::AuthInProgress,
                    "a conversation is already in progress",
                );
                return Action::None;
            }
        }

        if !self.login.users.iter().any(|u| u.name == username) {
            // Not rejected outright: PAM deliberately conflates "no such user"
            // with "wrong password", and short-circuiting here would turn the
            // greeter into a user enumeration oracle.
            log::debug!("create_session for unadvertised user {username:?}");
        }

        let tty = format!("/dev/tty{}", self.login.vt);
        match AuthHandle::start(&username, &tty, self.login.events.clone()) {
            Ok(handle) => {
                self.login.auth = Some(handle);
                self.login.phase = Phase::Authenticating;
            }
            Err(e) => {
                log::error!("spawning auth thread for {username}: {e}");
                resource.auth_failed("could not start authentication".to_owned());
            }
        }

        Action::None
    }

    fn respond(&mut self, resource: &WdmGreeterV1, id: u32, response: String) -> Action {
        if self.login.phase != Phase::Authenticating {
            resource.post_error(
                wdm_greeter_v1::Error::NoAuth,
                "respond with no conversation in progress",
            );
            return Action::None;
        }

        if self.login.pending_prompt != Some(id) {
            resource.post_error(
                wdm_greeter_v1::Error::StalePrompt,
                "respond carried an id that is not the pending prompt",
            );
            return Action::None;
        }

        self.login.pending_prompt = None;
        if let Some(auth) = &self.login.auth {
            auth.respond(id, response);
        }

        Action::None
    }

    fn start_session(
        &mut self,
        resource: &WdmGreeterV1,
        session_id: String,
        env: Vec<u8>,
    ) -> Action {
        if self.login.phase != Phase::Authenticated {
            resource.post_error(
                wdm_greeter_v1::Error::NoAuth,
                "start_session before authentication succeeded",
            );
            return Action::None;
        }

        let Some(session) = self
            .login
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .cloned()
        else {
            resource.post_error(
                wdm_greeter_v1::Error::InvalidSession,
                format!("no such session: {session_id}"),
            );
            return Action::None;
        };

        let extra_env = match wdm_protocol::env::decode(&env) {
            Ok(env) => env,
            Err(e) => {
                resource.post_error(wdm_greeter_v1::Error::InvalidEnv, e.to_string());
                return Action::None;
            }
        };

        self.login.chosen = Some((session, extra_env));
        self.login.phase = Phase::Launching;

        // The PAM thread opens the session and reports its environment; the
        // launch happens when that arrives.
        if let Some(auth) = &self.login.auth {
            auth.start_session();
        }

        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_types_map_to_the_wire() {
        assert_eq!(
            wire_session_type(crate::sessions::SessionType::Wayland) as u32,
            0
        );
        assert_eq!(wire_session_type(crate::sessions::SessionType::X11) as u32, 1);
    }

    #[test]
    fn prompt_styles_map_to_the_wire() {
        use crate::auth::PromptStyle::*;
        assert_eq!(wire_prompt_style(Secret) as u32, 0);
        assert_eq!(wire_prompt_style(Visible) as u32, 1);
        assert_eq!(wire_prompt_style(Info) as u32, 2);
        assert_eq!(wire_prompt_style(Error) as u32, 3);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        // A user who mistyped must not be locked out for minutes, and a brute
        // force attempt must not be allowed to retry at full speed.
        assert_eq!(BACKOFF_SECS[0], 0, "the first attempt must not be delayed");
        for pair in BACKOFF_SECS.windows(2) {
            assert!(pair[1] >= pair[0], "backoff must not shrink: {BACKOFF_SECS:?}");
        }
        assert!(*BACKOFF_SECS.last().unwrap() <= 30);
    }
}
