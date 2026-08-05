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
///
/// The first entry is zero because the first failure is not rate limited — that
/// is [`FAILURE_FLOOR`]'s job, and it is a floor rather than a backoff because
/// it exists for a different reason. See [`failure_delay`].
const BACKOFF_SECS: &[u64] = &[0, 1, 2, 4, 8, 10];

/// The least wall time a failed authenticate may appear to take.
///
/// `auth::AUTH_FAILED` flattens every `pam_authenticate` failure to one string
/// so the greeter cannot read which module refused. That closes the *message*
/// channel and leaves the clock wide open: a stack that reaches `pam_unix` and
/// runs a yescrypt or SHA-512 verify answers tens to hundreds of milliseconds
/// later than one that gives up before hashing, which is far above the noise
/// floor for a local client with a monotonic clock. Reporting `now + backoff`
/// does not mask it — the greeter subtracts a delay it already knows and
/// recovers PAM's own time exactly.
///
/// So the failure is reported on a *deadline* measured from wdm's side of the
/// conversation instead, and this is the floor under that deadline. Two seconds
/// is the same order as `login(1)`'s `FAIL_DELAY`: long enough to swallow any
/// plausible difference in how far down the stack a refusal came from, short
/// enough that a user who mistyped does not think the machine has hung.
const FAILURE_FLOOR: Duration = Duration::from_secs(2);

/// The least time between two accepted `create_session` calls.
///
/// The backoff and the cooldown are both charged from `AuthEvent::Failed`, and a
/// **cancel** is deliberately charged nothing: switching account in a greeter
/// cancels, and a user picking a different name has not attempted a login. That
/// left `create_session → respond → cancel → repeat` costing nothing at all —
/// and each turn of it forks and `exec`s `/proc/self/exe --pam-helper` as root
/// *and* unwinds PAM through `CONV_ERR`, which `pam_faillock`'s `authfail` arm
/// records. A greeter that is compromised, or merely buggy, could walk any named
/// account into a lockout at wire speed.
///
/// So there are two gates, and they charge different things. `cooldown_until` is
/// the rate limit proper: it is armed only by a real failure, it grows, and it
/// survives reset. This is the cheap one that *every* accepted attempt pays,
/// cancelled or not — no failure counter, no growth, just a floor under how
/// often the helper can be started.
///
/// One second, because it has to be invisible to the thing it must not punish: a
/// user choosing an account from a list. It bounds the loop rather than closing
/// it — a determined greeter can still reach `deny=3` in a few seconds — and
/// closing it is `pam_faillock`'s business, not wdm's, because wdm cannot tell a
/// cancelled attempt from a refused one without asking PAM.
const MIN_ATTEMPT_INTERVAL: Duration = Duration::from_secs(1);

/// When a failure must be reported so the attempt costs a constant time.
///
/// `anchor` is the moment wdm last handed PAM something to work on, so
/// everything PAM's timing could reveal happened after it. Reporting at
/// `anchor + max(backoff, floor)` means the wall time a greeter observes is a
/// function of its own request times and the failure count it can already
/// count — never of which module refused.
///
/// Saturates to zero rather than going negative: an answer that arrived after
/// the deadline has already paid the floor, and delaying it further would make
/// a *slow* PAM cost more than a fast one, which is the same leak upside down.
///
/// That saturation is also the ceiling on what the floor buys, and the word for
/// it is "usually", not "never". Everything PAM does under [`FAILURE_FLOOR`] is
/// hidden; everything over it is not. A local `pam_unix` refusal is tens to
/// hundreds of milliseconds and disappears. A network stack — `pam_sss` against
/// a directory, Kerberos against a KDC — routinely takes longer than two
/// seconds, and there the observed time is exactly PAM's own, so a name the
/// directory does not know can still be told from one it does. Raising the floor
/// past the slowest plausible network round trip is not a fix: it is a
/// multi-second delay charged to every user who mistypes, which is the trade the
/// two-second value already decided.
///
/// Free-standing so the arithmetic can be tested without a PAM conversation to
/// drive it; [`AuthHandle`] cannot be built in a test.
fn failure_delay(anchor: Option<Instant>, backoff: Duration, now: Instant) -> Duration {
    // No anchor means no attempt was recorded, which should not happen — pay
    // the full floor rather than answering instantly, because the safe error
    // here is the slow one.
    let deadline = anchor.unwrap_or(now) + backoff.max(FAILURE_FLOOR);
    deadline.saturating_duration_since(now)
}

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
    /// Authentication succeeded, the account and the session have been
    /// validated, and the display must now be released.
    ///
    /// Deliberately carries nothing but a name for the log line. What is
    /// launched is held in [`Login::chosen`] and is only acted on by
    /// [`Login::launch`], which the backend calls *after* the display is gone —
    /// so there is no second copy of it that could be launched from somewhere
    /// that still holds the GPU.
    Launch { username: String },
    /// The greeter should be restarted, optionally told why.
    RestartGreeter { error: Option<String> },
}

/// How the user's session ended, once the helper has said so.
///
/// Reaches the backend through [`Login::take_session_outcome`] rather than
/// through [`Action`], because the backend is not running `handle_action` at
/// that point: it is sitting in its handoff, with the display released and the
/// greeter dead, waiting for exactly this.
#[derive(Debug)]
pub enum SessionOutcome {
    /// The session ran and exited. `status` is the helper's rendering of the
    /// wait status.
    Ended { status: String, ran_for: Duration },
    /// `pam_open_session`, the environment assembly or the fork failed, so no
    /// session ever ran.
    Failed(String),
}

impl std::fmt::Display for SessionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ended { status, ran_for } => write!(f, "{status} after {ran_for:?}"),
            Self::Failed(reason) => write!(f, "never started: {reason}"),
        }
    }
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
    /// When an attempt was last accepted, for [`MIN_ATTEMPT_INTERVAL`].
    ///
    /// Survives [`Login::reset`] for the same reason `cooldown_until` does, and
    /// more sharply: the loop this gates *is* a reset loop, so a field it
    /// cleared would gate nothing at all.
    last_attempt: Option<Instant>,
    /// When PAM was last given something to work on, for [`failure_delay`].
    ///
    /// Set by [`Login::begin_attempt`] and re-anchored by every `respond`,
    /// because that is the honest point: a `respond` is what hands PAM the
    /// secret it then does or does not hash, so it is the instant every
    /// distinguishable amount of work starts from. Anchoring only at
    /// `create_session` would leave the floor already elapsed by the time a
    /// user who took three seconds to type submitted, and the oracle open for
    /// exactly the users who exist to be enumerated.
    attempt_started: Option<Instant>,
    /// Incremented for every attempt, so a timer armed for one attempt cannot
    /// act on a later one.
    generation: u64,
    /// The pending deferred failure report, so it can be replaced rather than
    /// stacked.
    failure_timer: Option<smithay::reexports::calloop::RegistrationToken>,
    /// Session chosen by `start_session`, held until the backend has released
    /// the display and calls [`Login::launch`].
    chosen: Option<(Session, Vec<(String, String)>)>,
    /// Set once [`Login::launch`] has told the helper to go.
    ///
    /// What it decides is where a `SessionFailed` is reported. Before the
    /// handoff there is still a greeter, and the answer is to restart it with
    /// the reason; after it, the display is gone and the backend is blocked
    /// waiting for an outcome, so the same event has to become one instead —
    /// otherwise the backend waits for a session that will never start.
    handed_off: bool,
    /// How the session ended, once the helper has said. Drained by the backend.
    session_outcome: Option<SessionOutcome>,

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
            last_attempt: None,
            attempt_started: None,
            generation: 0,
            failure_timer: None,
            chosen: None,
            handed_off: false,
            session_outcome: None,
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
    ///
    /// Set only: there is deliberately no way to spell "clear" here. Clearing is
    /// [`Login::begin_attempt`]'s alone, because that is the one moment where the
    /// user has visibly moved on from the failure. A caller with nothing to
    /// report must not call this at all — an overwrite with an empty reason would
    /// discard why the last session died before anyone had read it, and every
    /// restart path that has nothing to add reaches here.
    pub fn set_last_error(&mut self, error: String) {
        self.last_error = Some(error);
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
        // The next login generation starts before the handoff, not after the
        // previous one: leaving these set would route its first SessionFailed
        // into an outcome nobody is waiting for, and hand the generation after
        // that a stale one immediately.
        self.handed_off = false;
        self.session_outcome = None;

        // Timers live on the long-lived LoopHandle, which outlives this login
        // generation and every one after it. Bumping the generation only makes
        // the closure a no-op when it fires; the source itself stays registered,
        // so a cooldown armed before a successful login is still sitting in the
        // event loop for the whole of the user's session. Drop it here, the same
        // way report_failure_after drops the one it is replacing.
        if let Some(token) = self.failure_timer.take() {
            self.loop_handle.remove(token);
        }
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

    /// Handle an event from the PAM helper.
    pub fn handle_auth_event(&mut self, event: AuthEvent) -> Action {
        match event {
            AuthEvent::Prompt { id, text, style } => {
                // Same guard as the Ok arm below. The reader thread holds a
                // clone of the sender, so a prompt emitted just before cancel is still
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
                // Same guard as the Prompt and Ok arms, and load-bearing for two
                // separate reasons. Cancelling is how the helper is told to
                // stop: `reset` drops the handle, the helper's `recv` hits EOF,
                // and PAM unwinds through CONV_ERR into exactly this event. Without the guard a cancel produces the `auth_failed`
                // the XML promises will not follow — `report_failure_after`
                // captures the *post*-reset generation, so the timer's
                // generation-and-phase check passes and the still-bound greeter
                // is told its abandoned attempt failed.
                //
                // It would also charge the cancellation: switching user in a
                // greeter cancels, so every account switch would accrue a
                // failure and a cooldown for a login nobody attempted — the same
                // class of bug as an idle greeter walking an account into
                // pam_faillock's lockout.
                if self.auth.is_none() {
                    log::debug!("discarding auth_failed for an abandoned attempt");
                    return Action::None;
                }

                self.auth = None;
                self.pending_prompt = None;

                // Measured from when PAM was handed the secret, not from now:
                // `now + backoff` is a constant the greeter can subtract, and
                // what is left underneath is PAM's own time — which says
                // whether the account exists. See failure_delay.
                let now = Instant::now();
                let delay = failure_delay(self.attempt_started, self.next_backoff(), now);
                self.failures = self.failures.saturating_add(1);
                // The deadline, not the phase, is what actually rate limits:
                // it survives destroy, rebind and greeter respawn.
                self.cooldown_until = Some(now + delay);

                self.report_failure_after(reason, delay);
                Action::None
            }

            AuthEvent::SessionStarted { pid } => {
                // Informational. wdm is not this process's parent — the helper
                // is, and has to be — so there is nothing to record and nothing
                // to wait on. It is logged because it names the process that now
                // owns the display, which is the first thing anyone wants when a
                // login goes dark.
                log::info!("the session is running as pid {pid}");
                Action::None
            }

            AuthEvent::SessionEnded { status, ran_for } => {
                self.session_outcome = Some(SessionOutcome::Ended { status, ran_for });
                Action::None
            }

            AuthEvent::SessionFailed(reason) => {
                if self.handed_off {
                    // The display is already released and the backend is waiting
                    // on an outcome, not dispatching actions. Restarting the
                    // greeter from here would queue an action nobody drains and
                    // leave the backend waiting for a session that will never
                    // start.
                    log::error!("the session never started: {reason}");
                    self.session_outcome = Some(SessionOutcome::Failed(reason));
                    return Action::None;
                }

                // Before the handoff this is the helper dying between auth_ok
                // and the launch, and there is still a greeter to tell.
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

    /// How long until another attempt may be accepted, if the last was too
    /// recent.
    ///
    /// The gate a *cancel* pays. [`Self::rate_limited`] is charged only by a
    /// failure and is escaped entirely by cancelling; this is charged by every
    /// accepted attempt and is escaped by nothing but waiting. See
    /// [`MIN_ATTEMPT_INTERVAL`].
    fn too_soon(&self) -> Option<Duration> {
        (self.last_attempt? + MIN_ATTEMPT_INTERVAL).checked_duration_since(Instant::now())
    }

    /// Note that a new attempt is starting.
    ///
    /// Bumps the generation so anything armed for the previous attempt becomes a
    /// no-op, clears the stale launch error the user has moved on from, charges
    /// the minimum interval, and starts the clock a failure will be reported
    /// against.
    pub fn begin_attempt(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.last_error = None;
        // Every accepted attempt, whatever becomes of it. An attempt that is
        // cancelled a moment from now is charged nothing else — no failure, no
        // cooldown — and this is the only thing it does pay.
        self.last_attempt = Some(Instant::now());
        // An attempt that fails before it ever prompts — the stack refuses on
        // `pam_start`, or a module ahead of `pam_unix` says no with nothing to
        // ask — still has to cost the floor, so the anchor exists from here.
        self.attempt_started = Some(Instant::now());
    }

    /// Report a failure once `delay` has elapsed.
    ///
    /// Reporting late *is* the rate limit: a greeter cannot start another
    /// attempt until it hears how the last one went, so the delay is what slows
    /// a brute force down. The greeter is untrusted, and a cooldown it can
    /// discover the length of by being told "no" is a cooldown it can sit out at
    /// full speed; withholding the verdict is what bounds its request rate to
    /// one attempt per delay, whatever it is written to do on failure. No
    /// greeter in this repository retries by itself — since v0.7.0 a retry is a
    /// keypress — so this is not politeness towards a client, it is the limit.
    ///
    /// The cost is borne by the user, not by the attacker: up to
    /// `BACKOFF_SECS.last()` seconds — ten — of a greeter sitting on "Waiting…"
    /// after a mistyped password, with nothing to say why, because saying why is
    /// the thing being withheld.
    ///
    /// It is also what makes a failed authenticate constant-time, and that cost
    /// nothing to add: this was already a deadline mechanism, so hiding PAM's
    /// timing was a matter of choosing a different `delay` — see
    /// [`failure_delay`] — not of building anything. The only thing that changed
    /// for a user is that a failed login now takes at least [`FAILURE_FLOOR`].
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

    /// Tell the helper to open the PAM session and run the chosen session.
    ///
    /// **Only call this once the display has been released** — the DRM device,
    /// the renderer, libinput and the libseat session all dropped. It is the
    /// single call site of [`crate::auth::AuthHandle::start_session`] for
    /// exactly that reason: `pam_open_session` runs the moment the message
    /// arrives, and a module that `fork`s and `exit`s inside it takes the
    /// compositor's EGL state with it if there is still one to take.
    ///
    /// Returns whether the helper was told anything. A `false` has already set
    /// an outcome, so the caller's wait ends immediately rather than blocking on
    /// a session that was never asked for.
    pub fn launch(&mut self) -> bool {
        let Some((session, extra_env)) = self.chosen.take() else {
            // Only reachable if the backend handed off without a validated
            // choice, which start_session is what prevents.
            log::error!("launch with no session chosen");
            self.session_outcome = Some(SessionOutcome::Failed("no session was chosen".to_owned()));
            return false;
        };
        let Some(auth) = &self.auth else {
            // The attempt was cancelled between the handoff decision and here.
            // The greeter is already dead and the display already gone, so this
            // can only be reported through the next greeter's last_error.
            log::error!("launch with no authentication in flight");
            self.session_outcome = Some(SessionOutcome::Failed(
                "the login attempt expired before the session could start".to_owned(),
            ));
            return false;
        };

        // Before the send, not after: a SessionFailed can arrive on the very
        // next dispatch, and it has to be routed as an outcome rather than as a
        // greeter restart.
        self.handed_off = true;
        auth.start_session(&session, extra_env, self.vt);
        true
    }

    /// Take how the session ended, if the helper has said yet.
    pub fn take_session_outcome(&mut self) -> Option<SessionOutcome> {
        self.session_outcome.take()
    }

    /// Dismiss the helper now the session is over.
    ///
    /// `pam_close_session` is the helper's own, paired on the handle that opened
    /// the session — it cannot be anyone else's, since no other process holds
    /// that handle. This releases wdm's end so the reader thread stops and reaps
    /// the helper, rather than leaving a zombie per login.
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
        if state.login.handed_off {
            // The greeter was killed and the display released before the helper
            // was told to launch, but bytes it had already written are still on
            // its socket and are dispatched from `wait_for_session`. Every
            // request here reaches `Login::reset` one way or another — `cancel`
            // and `destroy` directly — and a reset now would discard the
            // session outcome the backend is blocked waiting for, leaving it
            // waiting forever with no greeter and no session. Nothing a greeter
            // can say is actionable at this point anyway: the login it is
            // talking about has already happened.
            log::debug!("ignoring a greeter request made after the handoff");
            return;
        }

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
        // The phase first, because two of its arms are protocol errors the XML
        // mandates and a deferred `auth_failed` is not a substitute for either.
        // The limits below can only be reached from a phase that accepts a new
        // conversation at all — today a live cooldown during `Authenticating` is
        // unreachable, since a failure clears the handle, but the ordering must
        // not depend on that staying true.
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

        // Both limits, and neither is the phase: the phase is what a greeter can
        // reset at will — by destroying its object, or by dying and being
        // respawned — and these are what it cannot. `rate_limited` is charged by
        // failures; `too_soon` is charged by every accepted attempt, including
        // the ones that are cancelled a moment later and so charged nothing
        // else. See MIN_ATTEMPT_INTERVAL.
        if let Some(remaining) = self
            .login
            .rate_limited()
            .into_iter()
            .chain(self.login.too_soon())
            .max()
        {
            // Not a protocol error: the greeter may try again, just not yet. The
            // refusal is delayed until the limit expires rather than sent now,
            // because the greeter is untrusted and an immediate "no" tells it
            // the cooldown is running while costing it nothing — it can ask
            // again at once, for the whole cooldown. Withholding the answer is
            // what caps its rate at one request per limit. Nothing in this
            // repository retries by itself; the bound is on what an adversary
            // can do, not on what a conforming greeter does.
            log::debug!("deferring create_session for {remaining:?}");
            // Armed unconditionally, and disarming any existing timer first, so
            // there is at most one pending report at a time — a second deferred
            // call replaces the first's timer rather than stacking onto it, so
            // the two produce one `auth_failed` between them. The previous guard
            // skipped arming while already in Cooldown, which silently dropped
            // the request and hung a greeter that waits for a reply per request.
            self.login
                .report_failure_after("too many attempts".to_owned(), remaining);
            return Action::None;
        }

        if !self.login.users.iter().any(|u| u.name == username) {
            // Not rejected outright, and the reason is no longer PAM's. PAM does
            // *not* conflate "no such user" with "wrong password" — under this
            // repository's own /etc/pam.d/wdm the first comes back as "User not
            // known to the underlying authentication module" and the second as
            // "Authentication failure". The two are indistinguishable in the one
            // place wdm has made them so: the *verdict*. auth::AUTH_FAILED
            // flattens its text, and failure_delay reports it on a deadline so
            // its timing says nothing either.
            //
            // Short-circuiting here would reintroduce the difference on this
            // side of PAM — an unadvertised name would fail by a path with its
            // own timing and its own phase transitions, and the enumeration
            // oracle would be back without either of those defences noticing.
            //
            // ponytail: the verdict is not the whole conversation, and the rest
            // of it is not flattened. `handle_auth_event`'s Prompt arm forwards
            // every prompt immediately and unconditionally, and `pam_sss` and
            // `pam_ldap` skip the password prompt entirely for a name they do
            // not know (see auth::notice_to_greeter) — so counting prompts still
            // answers "does this account exist", with no password offered and no
            // clock consulted. The upgrade path is to give every attempt the same
            // visible shape: when the stack asks nothing before refusing, wdm
            // emits a synthetic `Secret` prompt of its own and discards the
            // answer. It is not done here because it is a change to the meaning
            // of the protocol rather than to a string — the greeter would be
            // collecting a password that nothing verifies, the id would have to
            // come from the helper's reserved block without the helper knowing,
            // and it equalises only the zero-prompt case, not two stacks that
            // ask different numbers of real questions. That wants deciding as a
            // protocol change, with the greeters in the room.
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
                log::error!("spawning the PAM helper for {username:?}: {e}");
                // Through report_failure_after like every other failure, rather
                // than answering this one resource inline. Nothing here depends
                // on the username, so there was no oracle to close — but this
                // was one of only two exits that skipped the function where
                // every stated invariant lives: the floor, the single pending
                // report, and the broadcast to *every* bound object rather than
                // to whichever one happened to send the request.
                let delay = failure_delay(
                    self.login.attempt_started,
                    self.login.next_backoff(),
                    Instant::now(),
                );
                self.login
                    .report_failure_after("could not start authentication".to_owned(), delay);
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

        // The clock a failure is reported against restarts here, not at
        // create_session: this is the request that hands PAM the secret, so
        // every difference in how long the stack takes to refuse it is measured
        // from now. Anchoring at create_session instead would let a user who
        // spent longer than FAILURE_FLOOR typing pay no floor at all, and the
        // timing oracle would be open for precisely the accounts an attacker
        // could take their time over.
        self.login.attempt_started = Some(Instant::now());

        let Some(auth) = &self.login.auth else {
            // Phase said Authenticating but the handle is gone. Swallowing the
            // answer would leave the greeter waiting for a reply that can never
            // come, so the attempt is failed explicitly — and through
            // report_failure_after, for the reason create_session's spawn
            // failure goes through it: it is where the floor, the one-pending-
            // report rule and the broadcast to every bound object live, and an
            // exit that replies to one resource inline has none of them.
            log::error!("respond with no auth handle; failing the attempt");
            let delay = failure_delay(
                self.login.attempt_started,
                self.login.next_backoff(),
                Instant::now(),
            );
            self.login
                .report_failure_after("the login attempt was interrupted".to_owned(), delay);
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
        let username = auth.username().to_owned();

        // Here, and not after the handoff, because this is the last moment at
        // which there is a greeter on screen to be told. Everything that follows
        // — releasing the display, opening the PAM session, forking — happens
        // with nothing to display an error on, so a failure there can only reach
        // the user through the *next* greeter's last_error.
        //
        // This is also the check that stops `create_session("root")` plus the
        // root password from launching a root graphical session; see
        // Launch::validate for why that is reachable at all.
        if let Err(e) = crate::session::Launch::validate(&session, &username) {
            log::error!("preparing session {session_id}: {e}");
            self.login.reset();
            return Action::RestartGreeter {
                error: Some(e.to_string()),
            };
        }

        // Recorded now rather than after the session starts: the session is what
        // the user chose, and a compositor that crashes on startup should still
        // be preselected so they can try it again or pick another.
        self.login.remember_session(&username, &session.id);

        // Held, not sent. The message that starts PAM's session goes out from
        // Login::launch, which the backend calls only once the display has been
        // released — that ordering is the whole reason the helper exists.
        self.login.chosen = Some((session, extra_env));
        self.login.phase = Phase::Launching;

        Action::Launch { username }
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
        login.set_last_error("session died".to_owned());
        login.begin_attempt();
        assert!(login.last_error.is_none());
    }

    #[test]
    fn a_recorded_error_survives_a_restart_that_has_nothing_to_add() {
        // The restart paths that carry no reason of their own — a greeter that
        // dies before it ever binds, the "session opened with no session chosen"
        // path — must leave the recorded reason alone, or the user is bounced
        // back to a login prompt with no explanation for why their session went
        // away. The invariant used to live only in backend::restart_greeter's
        // `if error.is_some()` guard, a file away from the field it protects;
        // set_last_error now cannot express "clear" at all, and this is the test
        // that says so from the side that owns the state.
        let mut login = login();
        login.set_last_error("session exited immediately".to_owned());

        // Everything a greeter respawn does to this state, short of a new attempt.
        login.reset();
        login.clear_bindings();

        assert_eq!(
            login.last_error.as_deref(),
            Some("session exited immediately"),
            "the reason the session died was discarded before anyone saw it"
        );

        login.begin_attempt();
        assert!(
            login.last_error.is_none(),
            "begin_attempt is the one place that clears it"
        );
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        // Driven through `next_backoff` and `failure_delay`, not read off
        // `BACKOFF_SECS`. Every assertion here used to name the table and
        // nothing else, so it could not fail for anything the table is *used*
        // for — and nothing pinned that `next_backoff` indexes with the
        // pre-increment count, so the sequence a greeter actually experiences is
        // not the table as written.
        let mut login = login();
        let anchor = Instant::now();

        let delivered: Vec<u64> = (0..BACKOFF_SECS.len())
            .map(|_| {
                // The order handle_auth_event uses: the delay is computed from
                // the count *before* this failure is charged.
                let delay = failure_delay(Some(anchor), login.next_backoff(), anchor);
                login.failures += 1;
                delay.as_secs()
            })
            .collect();

        // Not [0, 1, 2, 4, 8, 10]: the first three are swallowed by
        // FAILURE_FLOOR, which is the floor doing its job rather than the
        // backoff failing at it.
        assert_eq!(
            delivered,
            vec![2, 2, 2, 4, 8, 10],
            "the delay a greeter waits is not what the table plus the floor say"
        );

        // Capped: further failures do not keep growing, so a user who mistyped
        // is not locked out of their own machine for minutes.
        login.failures += 20;
        assert_eq!(
            failure_delay(Some(anchor), login.next_backoff(), anchor).as_secs(),
            *delivered.last().unwrap(),
            "the backoff ran off the end of the table"
        );

        // And it never shrinks, which is what makes it a backoff at all.
        for pair in delivered.windows(2) {
            assert!(pair[1] >= pair[0], "backoff shrank: {delivered:?}");
        }
    }

    #[test]
    fn a_cancelled_attempt_is_free_of_cooldown_but_not_free() {
        // `create_session → respond → cancel → repeat` was charged nothing at
        // all: the failure counter and `cooldown_until` are armed only from the
        // Failed arm, and a cancel deliberately reaches neither. Each turn of
        // that loop forks and execs a root helper and unwinds PAM through
        // CONV_ERR, which pam_faillock records as an authfail — so a compromised
        // greeter could walk any named account into a lockout at wire speed.
        let mut login = login();

        // What an accepted create_session does, and all a cancelled attempt
        // ever does.
        login.begin_attempt();
        login.reset();

        // The gate that must not have been charged: switching account in a
        // greeter cancels, and a user picking a different name has not failed a
        // login.
        assert_eq!(login.failures, 0, "a cancellation was charged as a failure");
        assert!(
            login.rate_limited().is_none(),
            "a cancellation armed the failure cooldown"
        );

        // And the gate that must have been.
        let remaining = login
            .too_soon()
            .expect("a cancel-and-retry loop costs nothing");
        assert!(remaining <= MIN_ATTEMPT_INTERVAL);

        // Reset is how the loop goes round, so a gate reset could clear would
        // gate nothing.
        login.reset();
        assert!(
            login.too_soon().is_some(),
            "destroying the greeter object escaped the minimum interval"
        );

        // It is a floor, not a lockout: it lapses on its own.
        login.last_attempt = Some(Instant::now() - MIN_ATTEMPT_INTERVAL * 2);
        assert!(login.too_soon().is_none());
    }

    #[test]
    fn an_interrupted_respond_is_reported_like_every_other_failure() {
        // `respond` with the phase saying Authenticating and the handle gone
        // used to reply `auth_failed` inline to the one resource that asked:
        // no FAILURE_FLOOR, and no broadcast, so a second bound object would
        // never hear that the attempt was over. Neither depends on the
        // username, so there was no oracle — but they were the only exits that
        // skipped the function every stated invariant lives in.
        let mut h = Harness::new(vec![test_user("")], None);
        let mut client = h.bind_greeter(2);
        let resource = h.state.login.bound[0].clone();

        h.state.login.begin_attempt();
        h.state.login.phase = Phase::Authenticating;
        h.state.login.pending_prompt = Some(7);
        // There is no AuthHandle to build in a test, so this is the interrupted
        // path by construction.
        h.state.respond(&resource, 7, "hunter2".to_owned());

        assert_eq!(
            h.state.login.phase,
            Phase::Cooldown,
            "the failure was reported without entering the deferred path"
        );
        assert!(
            h.state.login.failure_timer.is_some(),
            "nothing was armed to tell the greeter the attempt ended"
        );

        // Deferred, not immediate: the harness never dispatches its event loop,
        // so an inline reply is the only thing that could reach the wire.
        h.dispatch();
        client.pump();
        assert!(
            !client
                .greeter_events()
                .iter()
                .any(|m| m.opcode == wire::AUTH_FAILED),
            "auth_failed was answered inline, skipping the failure floor"
        );
    }

    #[test]
    fn a_failure_is_never_reported_before_the_floor() {
        // The message channel is closed — every authenticate failure is
        // auth::AUTH_FAILED — so what is left to read is the clock. A stack that
        // reaches pam_unix and hashes answers hundreds of milliseconds later
        // than one that refuses before hashing, and a delay measured from the
        // answer is a constant the greeter subtracts back off. Measured from the
        // attempt instead, both land on the same deadline.
        let anchor = Instant::now();
        let floor = FAILURE_FLOOR;

        // Two PAM answers whose only difference is how far down the stack they
        // came from.
        let fast = failure_delay(
            Some(anchor),
            Duration::ZERO,
            anchor + Duration::from_millis(5),
        );
        let slow = failure_delay(
            Some(anchor),
            Duration::ZERO,
            anchor + Duration::from_millis(400),
        );

        assert_eq!(
            anchor + Duration::from_millis(5) + fast,
            anchor + floor,
            "a fast refusal was reported before the floor"
        );
        assert_eq!(
            anchor + Duration::from_millis(400) + slow,
            anchor + floor,
            "a slow refusal was reported at a different instant from a fast one"
        );
        assert!(
            fast > slow,
            "the delay did not absorb the difference in PAM's own time"
        );
    }

    #[test]
    fn an_answer_after_the_floor_is_not_delayed_further() {
        // Saturating, not negative — and deliberately not "floor again". A PAM
        // stack that is slower than the floor has already paid it; charging it
        // another one would make slow answers cost more than fast ones, which is
        // the same oracle upside down.
        let anchor = Instant::now();
        let late = anchor + FAILURE_FLOOR + Duration::from_secs(1);
        assert_eq!(
            failure_delay(Some(anchor), Duration::ZERO, late),
            Duration::ZERO,
            "an answer that arrived after its deadline was delayed again"
        );
    }

    #[test]
    fn the_backoff_wins_once_it_exceeds_the_floor() {
        // The floor hides which module refused; the backoff slows repetition.
        // They are the same deadline, so the larger of the two is what applies
        // — a floor that shortened the backoff would undo the rate limit.
        let anchor = Instant::now();
        let backoff = Duration::from_secs(8);
        assert!(
            backoff > FAILURE_FLOOR,
            "this test proves nothing otherwise"
        );
        assert_eq!(
            failure_delay(Some(anchor), backoff, anchor),
            backoff,
            "the floor swallowed a backoff longer than itself"
        );
    }

    #[test]
    fn a_missing_anchor_pays_the_full_floor() {
        // Unreachable — begin_attempt sets it — but the safe direction when it
        // is wrong is the slow one, not an instant answer.
        let now = Instant::now();
        assert_eq!(failure_delay(None, Duration::ZERO, now), FAILURE_FLOOR);
    }

    #[test]
    fn an_attempt_starts_the_clock_it_will_be_judged_against() {
        let mut login = login();
        assert!(login.attempt_started.is_none());
        login.begin_attempt();
        assert!(
            login.attempt_started.is_some(),
            "a failure would be reported against no deadline at all"
        );
    }

    #[test]
    fn responding_re_anchors_the_failure_clock() {
        // A respond is what hands PAM the secret, so it is where the timeable
        // work starts. If the anchor stayed at create_session, a user who took
        // longer than FAILURE_FLOOR to type would have already burned the floor
        // before PAM was even asked, and the failure would come back at PAM's
        // own speed — the oracle, open again, for exactly the accounts an
        // attacker can afford to be slow about.
        let mut h = Harness::new(vec![test_user("")], None);
        let _client = h.bind_greeter(2);
        let resource = h.state.login.bound[0].clone();

        h.state.login.begin_attempt();
        let at_create_session = h.state.login.attempt_started.expect("no anchor");

        // The user spends longer than the floor typing.
        h.state.login.attempt_started = Some(at_create_session - FAILURE_FLOOR * 2);
        let stale = h.state.login.attempt_started.unwrap();

        h.state.login.phase = Phase::Authenticating;
        h.state.login.pending_prompt = Some(7);
        // There is no AuthHandle to build in a test, so this takes respond's
        // interrupted path — but the re-anchor happens before that fork, which
        // is the whole point: it is not conditional on the conversation being
        // healthy.
        h.state.respond(&resource, 7, "hunter2".to_owned());

        let anchor = h.state.login.attempt_started.expect("the anchor was lost");
        assert!(
            anchor > stale,
            "respond did not restart the clock; a slow typist gets no floor"
        );
        assert_eq!(
            failure_delay(Some(anchor), Duration::ZERO, anchor),
            FAILURE_FLOOR,
            "a failure answered immediately after respond would skip the floor"
        );
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
        /// wdm_greeter_v1.destroy, the first request in the XML.
        pub const DESTROY: u16 = 0;
        pub const AUTH_OK: u16 = 6;
        pub const AUTH_FAILED: u16 = 7;
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
                assert!(
                    self.pos + 4 <= self.data.len(),
                    "message body ended mid-argument: wanted 4 bytes at {}, body is {} bytes",
                    self.pos,
                    self.data.len()
                );
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
                    assert!(
                        self.pos + len <= self.data.len(),
                        "message body ended mid-string: declared {len} bytes at {}, body is {} bytes",
                        self.pos,
                        self.data.len()
                    );
                    String::from_utf8_lossy(&self.data[self.pos..self.pos + len - 1]).into_owned()
                };
                self.pos += len.next_multiple_of(4);
                text
            }
        }

        pub struct Client {
            sock: UnixStream,
            received: Vec<Message>,
            /// Set once the server has hung up. Read at EOF is indistinguishable
            /// from "nothing more to read" at the syscall, but the two mean
            /// opposite things here: wdm closes the connection when a client
            /// commits a protocol error, so an unnoticed EOF turns a rejection
            /// into a missing-event panic several frames later.
            closed: bool,
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
                    closed: false,
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

            /// wdm_greeter_v1.destroy, which also cancels the conversation.
            pub fn destroy_greeter(&mut self) {
                self.send(GREETER, DESTROY, &[]);
            }

            /// Drain everything the server has written so far.
            pub fn pump(&mut self) {
                let mut chunk = [0u8; 16 * 1024];
                let mut data = Vec::new();
                loop {
                    match self.sock.read(&mut chunk) {
                        Ok(0) => {
                            self.closed = true;
                            break;
                        }
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
                    // Payloads here are tens of bytes and fit any socket buffer,
                    // so this cannot fire today. It is here because when a future
                    // change does push a flush past the buffer, the bare slice
                    // panicked with a range error three frames away that named
                    // neither the test nor the truncation.
                    assert!(
                        size >= 8 && pos + size <= data.len(),
                        "truncated wayland message: declared {size} bytes, {} available",
                        data.len() - pos
                    );
                    self.received.push(Message {
                        object,
                        opcode,
                        body: data[pos + 8..pos + size].to_vec(),
                    });
                    pos += size;
                }

                // Each pump parses only what it read, so a message split across
                // two reads would be dropped rather than reassembled. That is
                // acceptable while every flush fits the socket buffer, but it
                // must not be acceptable *silently*.
                assert_eq!(
                    pos,
                    data.len(),
                    "trailing {} bytes of a partial message",
                    data.len() - pos
                );
            }

            pub fn events(&self) -> &[Message] {
                &self.received
            }

            /// Panic if the compositor has hung up.
            ///
            /// wdm answers a protocol violation it cannot report on the wire by
            /// dropping the connection, so every accessor that reads results has
            /// to distinguish "the server has sent everything it is going to"
            /// from "the server hung up on us". Both look like an empty read at
            /// the syscall. Without this, a rejected request surfaces as a
            /// missing event several frames from the request that caused it.
            ///
            /// A hangup preceded by a wl_display.error is not that case: the
            /// compositor said what it objected to before disconnecting, the
            /// event is in `received`, and `already_bound` is tested for exactly
            /// that. Only a silent hangup is unexplained, and only that panics.
            fn assert_live(&self) {
                if !self.closed {
                    return;
                }
                assert!(
                    self.received
                        .iter()
                        .any(|m| m.object == DISPLAY && m.opcode == DISPLAY_ERROR),
                    "the compositor closed the connection without saying why; \
                     it rejected an earlier request"
                );
            }

            /// The registry name of a global, so it can be bound.
            pub fn global(&self, interface: &str) -> Option<u32> {
                self.assert_live();
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
                self.assert_live();
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
            let display = smithay::reexports::wayland_server::Display::<Wdm>::new().unwrap();
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

    #[test]
    fn the_wire_opcodes_still_match_the_generated_bindings() {
        // `mod wire` hardcodes event opcodes positionally, and the sharpest
        // assertion in this file is a *negative* one: that no default_session
        // event reached a version 1 greeter. Insert an event ahead of it in the
        // XML and that test goes green while checking nothing. The generated
        // bindings already carry the real table, so pin against it here rather
        // than trusting the numbers to stay true by themselves.
        //
        // The signature is pinned alongside the name because the tests read
        // arguments *positionally* — `user_last_session` takes the fourth
        // string. An argument inserted into `user` would move last_session
        // without moving any opcode, so a name-only check would stay green
        // while both version-gating tests silently asserted on display_name.
        use smithay::reexports::wayland_server::backend::protocol::{AllowNull, ArgumentType};

        let str_arg = ArgumentType::Str(AllowNull::No);
        let events = <WdmGreeterV1 as Resource>::interface().events;
        for (op, name, signature) in [
            (
                wire::USER,
                "user",
                &[str_arg, str_arg, str_arg, str_arg][..],
            ),
            (wire::DONE, "done", &[][..]),
            (wire::AUTH_OK, "auth_ok", &[][..]),
            (wire::AUTH_FAILED, "auth_failed", &[str_arg][..]),
            (wire::DEFAULT_SESSION, "default_session", &[str_arg][..]),
        ] {
            let event = events
                .get(op as usize)
                .unwrap_or_else(|| panic!("no event at opcode {op}"));
            assert_eq!(event.name, name, "opcode {op} is no longer {name}");
            assert_eq!(
                event.signature, signature,
                "{name}'s arguments changed; the positional reads in this \
                 module now name different fields"
            );
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
        assert_eq!(events[default].args().string(), "river.desktop");
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
    fn a_cancelled_attempt_is_neither_reported_nor_charged() {
        // The XML says of `cancel`: "Aborts the PAM conversation. No auth_ok or
        // auth_failed follows." But cancelling is *implemented* by dropping the
        // AuthHandle, which closes wdm's end of the helper's socket, which makes
        // the helper's `recv` hit EOF, which unwinds PAM through CONV_ERR and
        // arrives here as AuthEvent::Failed. So the one
        // event the protocol promises will not follow a cancel is precisely the
        // event a cancel generates.
        let mut h = Harness::new(vec![test_user("")], None);
        let mut client = h.bind_greeter(2);

        h.state.login.phase = Phase::Authenticating;
        let before = h.state.login.failures;

        // What Wdm::cancel does, and what a greeter reaches by destroying its
        // object or by dying and being respawned.
        h.state.login.reset();
        h.state
            .login
            .handle_auth_event(AuthEvent::Failed("Authentication failure".to_owned()));

        // The accounting half. Switching user in a greeter cancels, so charging
        // a cancellation accrues rate-limit penalties for logins nobody
        // attempted.
        assert_eq!(
            h.state.login.failures, before,
            "a cancelled attempt was charged as a failure"
        );
        assert!(
            h.state.login.cooldown_until.is_none(),
            "a cancelled attempt armed the rate limit"
        );

        // The reporting half. The broadcast itself is deferred onto a timer, and
        // this harness never dispatches its event loop, so the wire alone cannot
        // see it — assert instead that nothing was ever armed to send it. Phase
        // and timer together are what report_failure_after sets, and the timer's
        // own guard (generation and Cooldown) would pass, because reset bumped
        // the generation *before* the event arrived.
        assert_eq!(
            h.state.login.phase,
            Phase::Idle,
            "a cancelled attempt entered Cooldown, which arms auth_failed"
        );
        assert!(
            h.state.login.failure_timer.is_none(),
            "a cancelled attempt armed a deferred auth_failed"
        );

        // And the immediate path — report_failure_after broadcasts inline when
        // the timer cannot be armed — really did not fire.
        h.dispatch();
        client.pump();
        assert!(
            !client
                .greeter_events()
                .iter()
                .any(|m| m.opcode == wire::AUTH_FAILED),
            "auth_failed reached a greeter that had cancelled"
        );
    }

    #[test]
    fn a_session_failure_after_the_handoff_becomes_an_outcome_not_a_restart() {
        // The two halves of the same event, and getting it wrong hangs wdm.
        // Before the handoff there is still a greeter, so the answer is to
        // restart it with the reason. After it, the display is released and the
        // backend is sitting in `wait_for_session` — which does not drain
        // actions — so a RestartGreeter there would be queued for nobody while
        // the backend waited forever for a session that will never start.
        let mut login = login();

        // Before.
        let action = login.handle_auth_event(AuthEvent::SessionFailed("pam said no".to_owned()));
        assert!(
            matches!(action, Action::RestartGreeter { error: Some(ref e) } if e == "pam said no"),
            "a pre-handoff failure must reach the greeter that is still on screen: {action:?}"
        );
        assert!(login.take_session_outcome().is_none());

        // After.
        login.handed_off = true;
        let action = login.handle_auth_event(AuthEvent::SessionFailed("pam said no".to_owned()));
        assert!(
            matches!(action, Action::None),
            "a post-handoff failure queued an action nobody drains: {action:?}"
        );
        let outcome = login
            .take_session_outcome()
            .expect("the backend was left waiting for a session that never started");
        assert!(
            matches!(outcome, SessionOutcome::Failed(ref e) if e == "pam said no"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_session_that_ends_produces_an_outcome_once() {
        let mut login = login();
        login.handed_off = true;

        assert!(login.take_session_outcome().is_none());
        let action = login.handle_auth_event(AuthEvent::SessionEnded {
            status: "exit status: 0".to_owned(),
            ran_for: Duration::from_secs(90),
        });
        assert!(matches!(action, Action::None));

        let outcome = login.take_session_outcome().expect("no outcome recorded");
        let SessionOutcome::Ended { status, ran_for } = outcome else {
            panic!("expected a session that ended, got {outcome:?}");
        };
        assert_eq!(status, "exit status: 0");
        assert_eq!(ran_for, Duration::from_secs(90));

        // Taken, not peeked: leaving it behind would end the *next* login
        // generation's wait immediately, before its session had even started.
        assert!(login.take_session_outcome().is_none());
    }

    #[test]
    fn a_greeter_request_after_the_handoff_cannot_strand_the_backend() {
        // The greeter is killed before the helper is told to launch, but bytes
        // it had already written are still on its socket and get dispatched from
        // `wait_for_session`. A `cancel` or `destroy` among them reaches
        // Login::reset, which clears the session outcome — so the backend would
        // sit in its wait forever, with no greeter, no session and a released
        // display. The only way out would be the VT chord.
        let mut h = Harness::new(vec![test_user("")], None);
        let mut client = h.bind_greeter(2);

        h.state.login.phase = Phase::Launching;
        h.state.login.handed_off = true;
        h.state.login.session_outcome = Some(SessionOutcome::Ended {
            status: "exit status: 0".to_owned(),
            ran_for: Duration::from_secs(90),
        });

        // wdm_greeter_v1.destroy, which is the request a dying greeter is most
        // likely to have written last.
        client.destroy_greeter();
        h.dispatch();

        assert!(
            h.state.login.take_session_outcome().is_some(),
            "a greeter request after the handoff discarded the session outcome"
        );
        assert!(
            h.state.pending_actions.is_empty(),
            "a greeter request after the handoff queued work for nobody"
        );
    }

    #[test]
    fn reset_forgets_the_previous_generations_handoff() {
        // `reset` runs between login generations. Leaving `handed_off` set would
        // route the next generation's pre-handoff SessionFailed — the helper
        // dying between auth_ok and the launch — into an outcome nobody is
        // waiting for, so the greeter would never be restarted and the login
        // screen would sit there.
        let mut login = login();
        login.handed_off = true;
        login.session_outcome = Some(SessionOutcome::Failed("stale".to_owned()));

        login.reset();

        assert!(
            !login.handed_off,
            "the handoff flag outlived its generation"
        );
        assert!(
            login.take_session_outcome().is_none(),
            "a stale outcome would end the next generation's wait immediately"
        );
    }

    #[test]
    fn launching_with_nothing_chosen_ends_the_wait_rather_than_hanging() {
        // Only reachable if the backend handed off without a validated choice,
        // which start_session is what prevents. It matters anyway: by the time
        // `launch` is called the display is already gone, so a `false` that did
        // not also record an outcome would leave the backend blocked forever
        // with no greeter and no session — a black screen with no way out but
        // the VT chord.
        let mut login = login();
        assert!(!login.launch(), "launch claimed to have started something");
        assert!(
            login.take_session_outcome().is_some(),
            "a refused launch left the backend waiting forever"
        );
    }

    #[test]
    fn reset_tears_down_the_backoff_timer() {
        // Timers are registered on the long-lived LoopHandle, which survives the
        // handoff into the user's session. The generation check only makes the
        // closure a no-op; the source stays registered and would sit in the next
        // login generation's event loop.
        let mut login = login();
        login.report_failure_after("nope".to_owned(), Duration::from_secs(10));
        assert!(login.failure_timer.is_some(), "nothing was armed to test");

        login.reset();
        assert!(
            login.failure_timer.is_none(),
            "a backoff timer outlived the attempt that armed it"
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
