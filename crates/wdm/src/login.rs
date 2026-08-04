//! Server side of `wdm_greeter_v1`: the login state machine.
//!
//! The greeter is untrusted. Every request is validated against the phase the
//! conversation is actually in, prompts are id-tagged so a slow greeter cannot
//! answer a superseded question, and failed attempts are rate limited here
//! rather than in the greeter.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
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

/// What to put in a `user` event's `last_session` for a peer at `version`.
///
/// From version 2 the field is the user's own history and nothing else: the
/// machine-wide default travels on `default_session`, so a greeter can tell the
/// administrator's choice from the user's and decide which to preselect.
///
/// A version 1 peer has no `default_session` event and therefore no other way
/// to learn the default at all. Dropping the substitution for those peers would
/// silently stop honouring `default_session` in the config the moment wdm was
/// upgraded under an older greeter, so version 1 keeps the old conflated
/// meaning: history, or the configured default when there is no history.
fn last_session_for(version: u32, history: &str, default: Option<&str>) -> String {
    if version >= DEFAULT_SESSION_SINCE || !history.is_empty() {
        history.to_owned()
    } else {
        default.unwrap_or_default().to_owned()
    }
}

/// Highest `wdm_greeter_v1` version this compositor implements.
///
/// Version 2 added `default_session`. Bumped rather than extended in place
/// because greeters are separate binaries built against `wdm-protocol`, and a
/// greeter compiled against version 1 would decode shifted opcodes as garbage.
const INTERFACE_VERSION: u32 = 2;

/// The version at which `default_session` became available.
const DEFAULT_SESSION_SINCE: u32 = 2;

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
    /// The pending deferred failure report, so it can be replaced rather than
    /// stacked.
    failure_timer: Option<smithay::reexports::calloop::RegistrationToken>,
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
            failure_timer: None,
            chosen: None,
            events,
            loop_handle,
        }
    }

    /// Advertise the global.
    ///
    /// Version 2 adds `default_session`. A greeter built against version 1
    /// binds at 1 and is served the version 1 contract; nothing here assumes
    /// the greeter ships in lockstep with wdm.
    pub fn create_global(display: &DisplayHandle) {
        display.create_global::<Wdm, WdmGreeterV1, _>(INTERFACE_VERSION, ());
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
        let version = greeter.version();

        for user in &self.users {
            greeter.user(
                user.name.clone(),
                user.display_name.clone(),
                user.avatar_path.clone(),
                last_session_for(version, &user.last_session, self.default_session.as_deref()),
            );
        }

        if version >= DEFAULT_SESSION_SINCE {
            greeter.default_session(self.default_session.clone().unwrap_or_default());
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
                // Same guard as the Ok arm below. The PAM thread holds a clone
                // of the sender, so a prompt emitted just before cancel is still
                // in the channel; forwarding it makes the greeter ask a question
                // whose answer wdm then kills it for (no_auth).
                if self.auth.is_none() {
                    log::debug!("discarding a prompt for an abandoned attempt");
                    return Action::None;
                }

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

        // One pending report at a time. Stacking timers would deliver several
        // auth_failed events for one conversation, and the greeter would treat
        // each as a fresh failure.
        if let Some(token) = self.failure_timer.take() {
            self.loop_handle.remove(token);
        }

        // Tagged with the current attempt so a timer armed here cannot fire into
        // a later, possibly successful, one and force it back to Idle with a
        // spurious auth_failed.
        let generation = self.generation;
        let timer = Timer::from_duration(delay);

        let result = self.loop_handle.insert_source(timer, move |_, _, data| {
            let login = &mut data.state.login;
            login.failure_timer = None;
            if login.generation == generation && login.phase == Phase::Cooldown {
                login.phase = Phase::Idle;
                login.broadcast(|g| g.auth_failed(reason.clone()));
            } else {
                log::debug!("dropping a backoff timer from an abandoned attempt");
            }
            TimeoutAction::Drop
        });

        match result {
            Ok(token) => self.failure_timer = Some(token),
            Err(e) => {
                // Without a timer the failure would never be reported and the
                // greeter would hang, so report it immediately instead. The
                // cooldown deadline still applies, so this degrades to a busy
                // greeter, not an open door.
                log::error!("arming backoff timer: {e}");
                self.phase = Phase::Idle;
                self.broadcast(|g| g.auth_failed("authentication failed".to_owned()));
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

        // One conversation, one driver. A second object could otherwise cancel
        // the first's conversation simply by destroying itself, and nothing
        // limits how many times a client may bind.
        if !state.login.bound.is_empty() {
            greeter.post_error(
                wdm_greeter_v1::Error::AlreadyBound,
                "wdm_greeter_v1 is already bound",
            );
            return;
        }

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
            // Armed unconditionally, and disarming any existing timer first, so
            // every accepted create_session gets exactly one terminal event. The
            // previous guard skipped arming while already in Cooldown, which
            // silently dropped the request and hung a greeter that waits for a
            // reply per request.
            self.login
                .report_failure_after("too many attempts".to_owned(), remaining);
            return Action::None;
        }

        match self.login.phase {
            Phase::Idle | Phase::Cooldown => {}

            // The conversation is over — auth_ok was already delivered — so this
            // is a greeter backing out to pick a different account. The XML never
            // says cancel is required after auth_ok, so killing it here would
            // punish a conforming client. Treated as an implicit cancel.
            Phase::Authenticated => {
                log::debug!("create_session after auth_ok, restarting the conversation");
                self.login.reset();
            }

            Phase::Authenticating | Phase::Launching => {
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

        // The session type is not known until start_session, so the seat facts
        // that are known now are supplied and the type defaults to wayland —
        // better than pam_systemd's own fallback of "tty". The real type and
        // desktop are put into PAM's environment once the greeter has chosen,
        // before pam_open_session, which is when pam_systemd reads them.
        let description = crate::auth::SessionDescription {
            seat: "seat0".to_owned(),
            vtnr: self.login.vt,
            session_type: "wayland".to_owned(),
            desktop: String::new(),
        };

        match AuthHandle::start(&username, &tty, description, self.login.events.clone()) {
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
        // launch happens when that arrives. The chosen session's type and
        // desktop travel with the command so pam_systemd registers the logind
        // session correctly — the values match what build_env gives the child.
        auth.start_session(
            session.xdg_session_type().to_owned(),
            session.xdg_session_desktop().to_owned(),
        );

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
        assert_eq!(
            wire_session_type(crate::sessions::SessionType::X11) as u32,
            1
        );
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
    fn version_2_reports_history_only() {
        // The default travels on its own event, so the two are distinguishable.
        assert_eq!(
            last_session_for(2, "sway.desktop", Some("river.desktop")),
            "sway.desktop"
        );
        assert_eq!(last_session_for(2, "", Some("river.desktop")), "");
    }

    #[test]
    fn version_1_still_carries_the_configured_default() {
        // A version 1 greeter never receives default_session, so without this
        // substitution upgrading wdm would silently stop honouring the
        // administrator's default for first-time users.
        assert_eq!(
            last_session_for(1, "", Some("river.desktop")),
            "river.desktop"
        );
        // History still wins over the default, as it always did.
        assert_eq!(
            last_session_for(1, "sway.desktop", Some("river.desktop")),
            "sway.desktop"
        );
        // And no configured default leaves the field empty rather than inventing one.
        assert_eq!(last_session_for(1, "", None), "");
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
            assert!(
                pair[1] >= pair[0],
                "backoff must not shrink: {BACKOFF_SECS:?}"
            );
        }
        assert!(*BACKOFF_SECS.last().unwrap() <= 30);
    }

    /// Enough of the Wayland wire format to bind a global by hand.
    ///
    /// wdm depends on `wdm-protocol` with the `server` feature only, so there
    /// is no client library anywhere in this dependency graph to drive a test
    /// connection with. The messages a bind needs are few and small, so they
    /// are encoded here instead: host byte order, an eight byte header of
    /// object id followed by size and opcode packed into one word, and every
    /// argument padded to four bytes.
    ///
    /// This exists because the version gating and the `already_bound` refusal
    /// live in `bind` and `send_initial_state`, which no pure function test can
    /// reach — a regression there would leave every other test in this file
    /// passing.
    mod wire {
        use std::io::{ErrorKind, Read, Write};
        use std::os::unix::net::UnixStream;

        /// wl_display is always object 1; the other ids are this client's to
        /// allocate and are fixed because each connection binds exactly once.
        pub const DISPLAY: u32 = 1;
        pub const REGISTRY: u32 = 2;
        pub const GREETER: u32 = 3;

        /// wl_display.error, the event a protocol violation arrives as.
        pub const DISPLAY_ERROR: u16 = 0;
        /// wl_registry.global, one per advertised global.
        pub const REGISTRY_GLOBAL: u16 = 0;

        // Event opcodes are the order the events appear in the XML, which is
        // why default_session was appended rather than inserted.
        pub const USER: u16 = 0;
        pub const DONE: u16 = 4;
        pub const AUTH_OK: u16 = 6;
        pub const DEFAULT_SESSION: u16 = 8;

        pub struct Message {
            pub object: u32,
            pub opcode: u16,
            body: Vec<u8>,
        }

        impl Message {
            pub fn args(&self) -> Args<'_> {
                Args {
                    data: &self.body,
                    pos: 0,
                }
            }
        }

        pub struct Args<'a> {
            data: &'a [u8],
            pos: usize,
        }

        impl Args<'_> {
            pub fn uint(&mut self) -> u32 {
                let word = self.data[self.pos..self.pos + 4].try_into().unwrap();
                self.pos += 4;
                u32::from_ne_bytes(word)
            }

            pub fn string(&mut self) -> String {
                // The length counts the terminating NUL, which is not part of
                // the value; the whole argument is then padded to four bytes.
                let len = self.uint() as usize;
                let text = if len == 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&self.data[self.pos..self.pos + len - 1]).into_owned()
                };
                self.pos += len.next_multiple_of(4);
                text
            }
        }

        pub struct Client {
            sock: UnixStream,
            received: Vec<Message>,
        }

        impl Client {
            pub fn new(sock: UnixStream) -> Self {
                // Non-blocking so a read after the server has flushed drains
                // whatever is there and stops, rather than waiting for an event
                // that is never coming.
                sock.set_nonblocking(true).unwrap();
                Self {
                    sock,
                    received: Vec::new(),
                }
            }

            fn send(&mut self, object: u32, opcode: u16, body: &[u8]) {
                let size = 8 + body.len();
                let mut message = Vec::with_capacity(size);
                message.extend_from_slice(&object.to_ne_bytes());
                message.extend_from_slice(&(((size as u32) << 16) | opcode as u32).to_ne_bytes());
                message.extend_from_slice(body);
                self.sock.write_all(&message).unwrap();
            }

            /// wl_display.get_registry.
            pub fn get_registry(&mut self) {
                self.send(DISPLAY, 1, &REGISTRY.to_ne_bytes());
            }

            /// wl_registry.bind, whose new_id is untyped and so carries the
            /// interface name and the version the client is built against.
            pub fn bind(&mut self, name: u32, interface: &str, version: u32) {
                let mut body = Vec::new();
                body.extend_from_slice(&name.to_ne_bytes());
                let bytes = interface.as_bytes();
                body.extend_from_slice(&((bytes.len() + 1) as u32).to_ne_bytes());
                body.extend_from_slice(bytes);
                body.push(0);
                while body.len() % 4 != 0 {
                    body.push(0);
                }
                body.extend_from_slice(&version.to_ne_bytes());
                body.extend_from_slice(&GREETER.to_ne_bytes());
                self.send(REGISTRY, 0, &body);
            }

            /// Drain everything the server has written so far.
            pub fn pump(&mut self) {
                let mut chunk = [0u8; 16 * 1024];
                let mut data = Vec::new();
                loop {
                    match self.sock.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => data.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => panic!("reading from the compositor: {e}"),
                    }
                }

                let mut pos = 0;
                while pos + 8 <= data.len() {
                    let object = u32::from_ne_bytes(data[pos..pos + 4].try_into().unwrap());
                    let word = u32::from_ne_bytes(data[pos + 4..pos + 8].try_into().unwrap());
                    let size = (word >> 16) as usize;
                    let opcode = (word & 0xffff) as u16;
                    self.received.push(Message {
                        object,
                        opcode,
                        body: data[pos + 8..pos + size].to_vec(),
                    });
                    pos += size;
                }
            }

            pub fn events(&self) -> &[Message] {
                &self.received
            }

            /// The registry name of a global, so it can be bound.
            pub fn global(&self, interface: &str) -> Option<u32> {
                self.received
                    .iter()
                    .filter(|m| m.object == REGISTRY && m.opcode == REGISTRY_GLOBAL)
                    .find_map(|m| {
                        let mut args = m.args();
                        let name = args.uint();
                        (args.string() == interface).then_some(name)
                    })
            }

            /// Every event the compositor sent to the greeter object.
            pub fn greeter_events(&self) -> Vec<&Message> {
                self.received
                    .iter()
                    .filter(|m| m.object == GREETER)
                    .collect()
            }
        }
    }

    /// A compositor with a real `wayland_server::Display` behind it.
    ///
    /// Everything here runs in-process and needs neither root nor a GPU: the
    /// display never opens a listening socket, and clients are inserted from
    /// socket pairs.
    struct Harness {
        display: smithay::reexports::wayland_server::Display<Wdm>,
        state: Wdm,
        /// The event loop the `Login` took its handle from. Never dispatched;
        /// held only so the handle stays usable.
        _event_loop: smithay::reexports::calloop::EventLoop<'static, LoopData>,
        /// The auth channel's receiving half, which a running wdm keeps in its
        /// event loop. Dropping it would close the channel, so nothing here
        /// could ever start a conversation.
        _auth_rx: smithay::reexports::calloop::channel::Channel<AuthEvent>,
    }

    struct NoClientData;
    impl smithay::reexports::wayland_server::backend::ClientData for NoClientData {}

    impl Harness {
        fn new(users: Vec<User>, default_session: Option<&str>) -> Self {
            let display =
                smithay::reexports::wayland_server::Display::<Wdm>::new().unwrap();
            let handle = display.handle();

            let (events, auth_rx) = smithay::reexports::calloop::channel::channel();
            let event_loop: smithay::reexports::calloop::EventLoop<'static, LoopData> =
                smithay::reexports::calloop::EventLoop::try_new().unwrap();

            let login = Login::new(
                users,
                Vec::new(),
                default_session.map(str::to_owned),
                PathBuf::from("/nonexistent/wdm-test"),
                7,
                events,
                event_loop.handle(),
            );

            let config: crate::config::Config = toml::from_str("").unwrap();
            let greeter =
                crate::supervise::Greeter::new("/bin/true", "nobody", "wayland-test", false)
                    .unwrap();
            let state = Wdm::new(&handle, config, login, greeter).unwrap();
            // Advertised separately from the compositor's own globals, exactly
            // as the backends do it.
            Login::create_global(&handle);

            Self {
                display,
                state,
                _event_loop: event_loop,
                _auth_rx: auth_rx,
            }
        }

        /// Attach a client to the display and hand back its end of the socket.
        fn connect(&mut self) -> wire::Client {
            let (theirs, ours) = std::os::unix::net::UnixStream::pair().unwrap();
            self.display
                .handle()
                .insert_client(ours, std::sync::Arc::new(NoClientData))
                .unwrap();
            wire::Client::new(theirs)
        }

        fn dispatch(&mut self) {
            self.display.dispatch_clients(&mut self.state).unwrap();
            self.display.flush_clients().unwrap();
        }

        /// Connect, bind `wdm_greeter_v1` at `version`, and collect the reply.
        fn bind_greeter(&mut self, version: u32) -> wire::Client {
            let mut client = self.connect();
            client.get_registry();
            self.dispatch();
            client.pump();

            let name = client
                .global("wdm_greeter_v1")
                .expect("wdm_greeter_v1 was not advertised");
            client.bind(name, "wdm_greeter_v1", version);
            self.dispatch();
            client.pump();
            client
        }
    }

    fn test_user(last_session: &str) -> User {
        User {
            name: "testuser".to_owned(),
            display_name: "Test User".to_owned(),
            avatar_path: String::new(),
            last_session: last_session.to_owned(),
        }
    }

    /// The `last_session` string a bound greeter was told for the only user.
    fn user_last_session(client: &wire::Client) -> String {
        let user = client
            .greeter_events()
            .into_iter()
            .find(|m| m.opcode == wire::USER)
            .expect("no user event");
        let mut args = user.args();
        args.string();
        args.string();
        args.string();
        args.string()
    }

    #[test]
    fn a_version_2_greeter_is_told_the_default_separately() {
        // The pure test above says what last_session_for computes; this says
        // that the enumerate phase actually applies it per resource version,
        // which is the part a greeter observes.
        let mut h = Harness::new(vec![test_user("")], Some("river.desktop"));
        let client = h.bind_greeter(2);

        assert_eq!(
            user_last_session(&client),
            "",
            "history was conflated with the configured default"
        );

        let events = client.greeter_events();
        let default = events
            .iter()
            .position(|m| m.opcode == wire::DEFAULT_SESSION)
            .expect("no default_session event");
        let done = events
            .iter()
            .position(|m| m.opcode == wire::DONE)
            .expect("no done event");
        assert!(default < done, "default_session arrived after done");
        assert_eq!(
            events[default].args().string(),
            "river.desktop"
        );
    }

    #[test]
    fn a_version_1_greeter_gets_the_default_in_last_session_instead() {
        // A version 1 peer has no default_session event to learn the
        // administrator's choice from, so it keeps the old conflated meaning.
        // Sending the event anyway would be a wire error on that peer.
        let mut h = Harness::new(vec![test_user("")], Some("river.desktop"));
        let client = h.bind_greeter(1);

        assert_eq!(user_last_session(&client), "river.desktop");
        assert!(
            !client
                .greeter_events()
                .iter()
                .any(|m| m.opcode == wire::DEFAULT_SESSION),
            "default_session was sent to a version 1 resource"
        );
        assert!(
            client
                .greeter_events()
                .iter()
                .any(|m| m.opcode == wire::DONE),
            "the enumerate phase never ended"
        );
    }

    #[test]
    fn a_second_bind_is_refused_and_gets_no_initial_state() {
        // One conversation, one driver: a second object could otherwise cancel
        // the first's conversation just by destroying itself. The refusal must
        // also come *before* any state is pushed, or a client that binds twice
        // walks away with a second copy of the user list.
        let mut h = Harness::new(vec![test_user("sway.desktop")], None);
        let first = h.bind_greeter(2);
        assert!(!first.greeter_events().is_empty());

        let second = h.bind_greeter(2);

        let error = second
            .events()
            .iter()
            .find(|m| m.object == wire::DISPLAY && m.opcode == wire::DISPLAY_ERROR)
            .expect("the second bind was accepted");
        let mut args = error.args();
        assert_eq!(args.uint(), wire::GREETER, "the error named another object");
        assert_eq!(
            args.uint(),
            wdm_greeter_v1::Error::AlreadyBound as u32,
            "refused with the wrong error code"
        );
        assert!(
            second.greeter_events().is_empty(),
            "a refused bind was still sent the enumerate phase"
        );

        // Only the first object is left driving the conversation.
        assert_eq!(h.state.login.bound.len(), 1);

        // And it still works: a refused rival must not take the live greeter
        // down with it.
        let mut first = first;
        h.state.login.broadcast(|g| g.auth_ok());
        h.dispatch();
        first.pump();
        assert!(
            first
                .greeter_events()
                .iter()
                .any(|m| m.opcode == wire::AUTH_OK),
            "the surviving greeter stopped receiving events"
        );
    }
}
