//! Server side of `wdm_greeter_v1`: the login state machine.
//!
//! The greeter is untrusted. Every request is validated against the phase the
//! conversation is actually in, prompts are id-tagged so a slow greeter cannot
//! answer a superseded question, and failed attempts are rate limited here
//! rather than in the greeter.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
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
    /// A failure is being reported after its backoff delay.
    ///
    /// Note this is not itself the rate limit — a greeter can leave this phase
    /// by destroying and rebinding the global. The limit is `cooldown_until`,
    /// which survives reset.
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
    ///
    /// Deliberately *not* cleared by [`Login::reset`]: a greeter that destroys
    /// its object, or dies and is respawned, must not get a fresh budget.
    failures: usize,
    /// No attempt may start before this instant.
    ///
    /// The real rate limit. Phase alone is not enough, because `destroy`
    /// followed by a rebind resets the phase, and a greeter that kills itself
    /// gets a brand new object either way. Survives reset for the same reason.
    cooldown_until: Option<Instant>,
    /// Incremented for every attempt, so a timer armed for one attempt cannot
    /// act on a later one.
    generation: u64,
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
            cooldown_until: None,
            generation: 0,
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
    ///
    /// Deliberately preserves `failures` and `cooldown_until`. Everything a
    /// greeter can do to reach this path — destroying its object, exiting so it
    /// gets respawned — would otherwise clear the rate limit, which is the one
    /// thing an untrusted greeter must not be able to do. Bumping the generation
    /// makes any timer armed for the abandoned attempt a no-op.
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
        self.generation = self.generation.wrapping_add(1);
        // Dropping the handle cancels any conversation in flight.
        self.auth = None;
        self.pending_prompt = None;
        self.chosen = None;
    }

    /// Forget every bound object, used when the greeter process is gone.
    ///
    /// Separate from [`Login::reset`] because destroying one object must not
    /// silence another that is still alive.
    pub fn clear_bindings(&mut self) {
        self.bound.clear();
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

    /// Send the rank of one output to whichever greeter owns `wl_output`.
    ///
    /// Called from `OutputHandler::output_bound`, because a client has no
    /// `wl_output` resource until it binds the global — and the reference
    /// greeter binds `wdm_greeter_v1` first, so at enumerate time there is
    /// nothing to address the rank to. Without this the greeter never learns any
    /// rank and the contract's "contiguous from 0" guarantee is vacuous.
    pub fn send_rank_for(&self, output: &Output, wl_output: &WlOutput) {
        let Some(rank) = self.ranked_outputs.iter().position(|o| o == output) else {
            // Disabled or not yet ranked; it will be re-sent by set_output_ranks.
            return;
        };

        let Some(target) = wl_output.client() else {
            return;
        };
        for greeter in &self.bound {
            if greeter.client().is_some_and(|c| c.id() == target.id()) {
                greeter.output_rank(wl_output, rank as u32);
            }
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

        // Cloned rather than taken. Taking it on first bind means a greeter that
        // binds twice, or a second connection that arrives first, consumes the
        // error and the real greeter shows the silent bounce this event exists
        // to prevent. It is cleared when the user starts a new attempt instead.
        if let Some(error) = &self.last_error {
            greeter.last_error(error.clone());
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
                // A result for an attempt that was cancelled must not revive it:
                // the handle is gone, so start_session would have nothing to
                // drive and the greeter would hang waiting for a launch.
                if self.auth.is_none() {
                    log::debug!("discarding auth_ok for an abandoned attempt");
                    return Action::None;
                }
                self.phase = Phase::Authenticated;
                self.pending_prompt = None;
                self.failures = 0;
                self.cooldown_until = None;
                self.broadcast(|g| g.auth_ok());
                Action::None
            }

            AuthEvent::Failed(reason) => {
                self.auth = None;
                self.pending_prompt = None;

                let delay = self.next_backoff();
                self.failures = self.failures.saturating_add(1);
                // The deadline, not the phase, is what actually rate limits:
                // it survives destroy, rebind and greeter respawn.
                self.cooldown_until = Some(Instant::now() + delay);

                self.report_failure_after(reason, delay);
                Action::None
            }

            AuthEvent::SessionOpened { env } => {
                let Some(auth) = &self.auth else {
                    log::error!("PAM session opened with no attempt in flight");
                    return Action::RestartGreeter {
                        error: Some("the login attempt was cancelled".to_owned()),
                    };
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

    /// How long an attempt must wait, if the rate limit is still in force.
    ///
    /// Extracted so the limit can be tested without standing up a compositor —
    /// it is the one thing an untrusted greeter must not be able to escape, and
    /// it previously had no test at all.
    pub fn rate_limited(&self) -> Option<Duration> {
        self.cooldown_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    /// Note that a new attempt is starting.
    ///
    /// Bumps the generation so anything armed for the previous attempt becomes a
    /// no-op, and clears the stale launch error the user has moved on from.
    pub fn begin_attempt(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.last_error = None;
    }

    /// Report a failure once `delay` has elapsed.
    ///
    /// Reporting late *is* the rate limit: a greeter cannot start another
    /// attempt until it hears how the last one went, so the delay is what slows
    /// a brute force down. Answering immediately would leave a greeter that
    /// retries on failure — which is the natural thing to write — spinning at
    /// full speed for the whole cooldown.
    fn report_failure_after(&mut self, reason: String, delay: Duration) {
        self.phase = Phase::Cooldown;

        // Tagged with the current attempt so a timer armed here cannot fire into
        // a later, possibly successful, one and force it back to Idle with a
        // spurious auth_failed.
        let generation = self.generation;
        let timer = Timer::from_duration(delay);

        let result = self.loop_handle.insert_source(timer, move |_, _, data| {
            let login = &mut data.state.login;
            if login.generation == generation && login.phase == Phase::Cooldown {
                login.phase = Phase::Idle;
                login.broadcast(|g| g.auth_failed(reason.clone()));
            } else {
                log::debug!("dropping a backoff timer from an abandoned attempt");
            }
            TimeoutAction::Drop
        });

        if let Err(e) = result {
            // Without a timer the failure would never be reported and the greeter
            // would hang, so report it immediately instead. The cooldown deadline
            // still applies, so this degrades to a busy greeter, not an open door.
            log::error!("arming backoff timer: {e}");
            self.phase = Phase::Idle;
            self.broadcast(|g| g.auth_failed("authentication failed".to_owned()));
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
            wdm_greeter_v1::Request::Cancel => state.cancel(),
            wdm_greeter_v1::Request::StartSession { session_id, env } => {
                state.start_session(resource, session_id, env)
            }
            wdm_greeter_v1::Request::Destroy => {
                // Cancels the conversation, per the protocol. Bindings are left
                // to destroyed(): silencing every other bound object because one
                // was destroyed would be a bug, not a cancellation.
                state.cancel()
            }
            _ => Action::None,
        };

        state.queue_action(action);
    }

    fn destroyed(state: &mut Self, _client: ClientId, resource: &WdmGreeterV1, _data: &()) {
        state.login.bound.retain(|g| g != resource);
    }
}

use smithay::reexports::wayland_server::backend::ClientId;

impl Wdm {
    /// Abandon the conversation without reporting a result.
    ///
    /// Shared by `cancel` and `destroy`. Bumping the generation through
    /// [`Login::reset`] disarms any backoff timer, so a cancel during Cooldown
    /// does not later emit the `auth_failed` the protocol says will not follow.
    fn cancel(&mut self) -> Action {
        // Launching means PAM has been told to open a session and the reply is
        // in flight. Dropping the handle there would strand the greeter in a
        // phase that accepts nothing, so the greeter is restarted instead.
        let was_launching = self.login.phase == Phase::Launching;
        self.login.reset();

        if was_launching {
            Action::RestartGreeter {
                error: Some("the login was cancelled while starting the session".to_owned()),
            }
        } else {
            Action::None
        }
    }

    fn create_session(&mut self, resource: &WdmGreeterV1, username: String) -> Action {
        // Checked before the phase, because the phase is the part a greeter can
        // reset at will and this is the part it cannot.
        if let Some(remaining) = self.login.rate_limited() {
            // Not a protocol error: the greeter may try again, just not yet. The
            // refusal is delayed until the limit expires rather than sent now,
            // because a greeter that retries on failure would otherwise spin for
            // the whole cooldown. Answering late makes its retry land exactly
            // when it is allowed to.
            log::debug!("deferring create_session for {remaining:?}");
            if self.login.phase != Phase::Cooldown {
                self.login
                    .report_failure_after("too many attempts".to_owned(), remaining);
            }
            return Action::None;
        }

        match self.login.phase {
            Phase::Idle | Phase::Cooldown => {}
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
        self.login.begin_attempt();

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

        let Some(auth) = &self.login.auth else {
            // Phase said Authenticating but the handle is gone. Swallowing the
            // answer would leave the greeter waiting for a reply that can never
            // come, so the attempt is failed explicitly.
            log::error!("respond with no auth handle; failing the attempt");
            self.login.phase = Phase::Idle;
            resource.auth_failed("the login attempt was interrupted".to_owned());
            return Action::None;
        };

        auth.respond(id, response);
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

        let Some(auth) = &self.login.auth else {
            // Authenticated with no handle: the attempt was cancelled between
            // auth_ok and this request. Restarting the greeter is the only way
            // to get back to a state it can act in.
            log::error!("start_session with no auth handle");
            self.login.reset();
            return Action::RestartGreeter {
                error: Some("the login attempt expired, please try again".to_owned()),
            };
        };

        // The PAM thread opens the session and reports its environment; the
        // launch happens when that arrives.
        auth.start_session();

        self.login.chosen = Some((session, extra_env));
        self.login.phase = Phase::Launching;

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

    /// A `Login` with no compositor behind it.
    ///
    /// The event loop is never dispatched, so no `LoopData` is ever needed —
    /// which is what makes the state machine testable in isolation.
    fn login() -> Login {
        let (events, _rx) = smithay::reexports::calloop::channel::channel();
        let event_loop: smithay::reexports::calloop::EventLoop<'static, LoopData> =
            smithay::reexports::calloop::EventLoop::try_new().unwrap();

        Login::new(
            Vec::new(),
            Vec::new(),
            None,
            PathBuf::from("/nonexistent/wdm-test"),
            7,
            events,
            event_loop.handle(),
        )
    }

    #[test]
    fn a_fresh_login_is_not_rate_limited() {
        assert!(login().rate_limited().is_none());
    }

    #[test]
    fn the_rate_limit_survives_reset() {
        // reset() is reachable by the greeter: destroying its object, or simply
        // exiting so it gets respawned. If either cleared the limit, an
        // untrusted greeter could retry without bound.
        let mut login = login();
        login.cooldown_until = Some(Instant::now() + Duration::from_secs(30));
        login.failures = 3;

        login.reset();

        assert!(
            login.rate_limited().is_some(),
            "destroying the object escaped the rate limit"
        );
        assert_eq!(login.failures, 3, "reset refilled the failure budget");
    }

    #[test]
    fn an_elapsed_cooldown_stops_limiting() {
        let mut login = login();
        login.cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(login.rate_limited().is_none());
    }

    #[test]
    fn reset_invalidates_a_timer_armed_for_the_previous_attempt() {
        // The generation is what stops a backoff timer from an abandoned attempt
        // firing into a later, successful one and forcing a spurious
        // auth_failed.
        let mut login = login();
        let armed = login.generation;

        login.reset();
        assert_ne!(login.generation, armed);

        login.begin_attempt();
        assert_ne!(login.generation, armed);
    }

    #[test]
    fn starting_an_attempt_clears_the_previous_launch_error() {
        let mut login = login();
        login.set_last_error(Some("session died".to_owned()));
        login.begin_attempt();
        assert!(login.last_error.is_none());
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
