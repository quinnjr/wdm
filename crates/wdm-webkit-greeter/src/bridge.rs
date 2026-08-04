//! The two directions of traffic between the theme and the protocol.
//!
//! Everything here is pure: [`parse`] turns what the page posted into a
//! [`Request`], and [`Bridge::diff`] turns a change in the [`Model`] into the
//! JavaScript that reports it. Neither touches GTK, so both are testable —
//! which matters more here than elsewhere, because the alternative is checking
//! a login screen's edge cases by hand.

use serde_json::json;
use wdm_greeter_client::Model;

/// What a theme asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Authenticate(String),
    Respond(String),
    Cancel,
    StartSession(String),
}

/// Parse one `postMessage` payload: `["verb"]` or `["verb", "argument"]`.
///
/// JSON for one reason. The payload crosses as a C string, so a separator-based
/// format loses everything past the first NUL — and the one argument that can
/// plausibly contain one is the password. JSON escapes it as text, so an answer
/// stays exactly what the user typed.
pub fn parse(raw: &str) -> Option<Request> {
    let message: Vec<String> = serde_json::from_str(raw).ok()?;
    let (verb, arg) = match message.as_slice() {
        [verb] => (verb.as_str(), ""),
        [verb, arg] => (verb.as_str(), arg.as_str()),
        _ => return None,
    };

    match verb {
        "authenticate" if !arg.is_empty() => Some(Request::Authenticate(arg.to_owned())),
        "respond" => Some(Request::Respond(arg.to_owned())),
        "cancel" => Some(Request::Cancel),
        "start_session" if !arg.is_empty() => Some(Request::StartSession(arg.to_owned())),
        _ => None,
    }
}

/// The most statements the queue will hold before dropping some.
///
/// A wedged web process never acknowledges anything, so `pending` would
/// otherwise grow for as long as the machine sits at the login screen.
///
/// What goes first is decided by [`Stmt`], not by age alone. Assignments are
/// the page's *state* and a later one supersedes an earlier one, so an evicted
/// assignment costs nothing as long as a newer one is still queued — they are
/// dropped oldest-first until none are left. Only then are callbacks touched,
/// because `show_message` and `show_prompt` are events with no successor and
/// one dropped here is gone for good: the lockout explanation the user never
/// sees. Callbacks are therefore the last thing dropped, not the first.
const PENDING_LIMIT: usize = 256;

/// Consecutive failed evaluations after which the page is declared dead.
///
/// The pump ticks every 16ms, so an evaluation that comes back `Err` every time
/// makes this roughly a second. Below that a reload or a crash-and-respawn
/// recovers on its own and quitting would be worse than waiting.
///
/// A silent web process escalates the same way but more slowly, because each
/// failure costs it [`VERDICT_DEADLINE`] ticks first: about half a minute
/// before the greeter gives up. Waiting longer on silence than on a refusal is
/// the right way round — silence is the case a slow theme can also produce, and
/// a slow theme is given a way back: a verdict that arrives after its deadline
/// has passed still clears the count, because a page that answered late is
/// alive. See [`Bridge::delivered`].
const WEDGED_AFTER: u32 = 60;

/// Pump ticks an evaluation may go without a verdict before it counts as one
/// that failed.
///
/// About half a second, which no callback in a login screen has any business
/// taking. Being wrong about it is not free: unlike an evaluation that came
/// back `Err`, one that merely went quiet may well have *run*, so its callbacks
/// cannot be sent again — they are dropped and only the assignments are
/// retransmitted. That asymmetry is why this is generous rather than tight.
const VERDICT_DEADLINE: u32 = 30;

/// The statement an idle page is given to answer, so silence is still visible.
///
/// Three things are required of it, and no other literal would do: it parses as
/// a complete JavaScript *statement*, so it can be concatenated with the rest of
/// a flush; it does nothing, so a theme cannot tell a heartbeat from silence by
/// its effects; and it is queued as a [`Stmt::Assignment`], so
/// [`Bridge::enforce_limit`] treats it as expendable and it is the first thing
/// thrown away when a wedged page fills the queue rather than something that
/// costs the user a message.
const HEARTBEAT: &str = "void 0;";

/// What kind of thing a queued statement is, which decides what may be done to
/// it when the queue has to lose something or send something twice.
///
/// The two halves of what `diff` produces behave differently under exactly the
/// operations this bridge performs, and conflating them is how a lockout
/// explanation ends up either duplicated or silently dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stmt {
    /// Page state: `window.wdm._prompt`, `is_authenticated`,
    /// `in_authentication`, `authentication_user`. Idempotent, and a later one
    /// supersedes an earlier one — so re-running it is invisible, and dropping
    /// it is harmless as long as a newer one follows.
    Assignment,
    /// A call into the theme: `show_message`, `show_prompt`,
    /// `authentication_complete`. An event with no successor. Running it twice
    /// shows the user the same message twice; dropping it loses it for good.
    Callback,
}

/// One statement, with what may be done to it.
struct Queued {
    script: String,
    kind: Stmt,
    /// The pair this statement belongs to, if it is half of one.
    ///
    /// The two halves are evicted together or not at all — see
    /// [`Bridge::enforce_limit`]. Only `window.wdm._prompt` and the
    /// `show_prompt` beside it are paired: the assignment classes as expendable
    /// state, which is true of it in isolation and false of it here, because
    /// the default theme's `show_prompt` answers a buffered password with
    /// `wdm.respond`, and `respond` with `_prompt` null throws "with no prompt
    /// pending" into `call`'s own try/catch. The user is left looking at a
    /// disabled field reading "Checking…" that nothing will ever update.
    ///
    /// A token rather than a flag on one of them, so the relation survives
    /// eviction and the timed-out-verdict retain, neither of which keeps the
    /// two adjacent.
    pair: Option<u64>,
}

impl Queued {
    fn assignment(script: String) -> Self {
        Self {
            script,
            kind: Stmt::Assignment,
            pair: None,
        }
    }

    fn callback(script: String) -> Self {
        Self {
            script,
            kind: Stmt::Callback,
            pair: None,
        }
    }

    /// Tag this statement as one half of an indivisible pair.
    fn paired(mut self, token: u64) -> Self {
        self.pair = Some(token);
        self
    }
}

/// The evaluation whose verdict is still out.
///
/// One value rather than a flag, a count and a tick counter kept in step by
/// hand. The flag existed only because [`Bridge::enforce_limit`] can evict the
/// very statements an unresolved evaluation carried, taking the count to zero
/// while the evaluation is still coming — so "is one outstanding" and "how many
/// did it carry" are genuinely different questions, and answering the first
/// with the second disabled the hang detector exactly when the queue was full.
/// Making outstanding-ness the `Option` puts that beyond reach: eviction can
/// only touch `count`, and every place that ends an evaluation is one
/// assignment of `None`.
struct InFlight {
    /// How many statements from the front of `pending` the evaluation carried.
    count: usize,
    /// How many pump ticks it has gone without a verdict.
    ///
    /// Counted rather than timed, so `Bridge` stays free of clocks: the pump's
    /// interval is the unit. See [`Bridge::tick`].
    ticks: u32,
}

/// One flush's worth of script, tagged with the epoch it belongs to.
///
/// The tag is what makes a late verdict harmless: see [`Bridge::delivered`].
pub struct Outbound {
    pub epoch: u64,
    pub script: String,
}

/// What the page has already been told.
///
/// The model is polled, so every field it holds would otherwise be re-reported
/// on each pump. Themes are written expecting a callback per event — a theme
/// that types out `show_message` a character at a time, or plays a sound, would
/// be unusable if it fired sixty times a second.
#[derive(Default)]
pub struct Bridge {
    prompt: Option<u32>,
    /// How many of `Model::notices` the page has already been told about.
    ///
    /// A count rather than a copy of the last one: within a stretch where the
    /// list only grows, everything past this index is new, and each is reported
    /// with its own style instead of being folded into one line.
    ///
    /// Valid only for the generation it was counted in — see
    /// `notices_generation`.
    notices_reported: usize,
    /// Which generation of `Model::notices` `notices_reported` counts.
    ///
    /// The client clears that list without telling us — `AuthOk` does it while
    /// the phase is still `Launching`, which is exactly when `pam_open_session`
    /// forwards the password-expiry warning, the motd and the last-login line.
    ///
    /// The length cannot detect that clear, which is why this exists.
    /// `Link::pump` drains a whole batch of Wayland events before `diff` runs
    /// once, so the clear and the notices that follow it land together and the
    /// list is the same length or longer than it was: a counter compared
    /// against a length sees nothing happen and swallows every one of them. The
    /// client stamps the list instead, bumping this wherever it clears, so a
    /// difference here *is* the clear and the count starts again from zero.
    notices_generation: u64,
    error: Option<String>,
    authenticated: bool,
    over: bool,
    /// Whether the page has been told the connection to wdm is gone.
    ///
    /// Baked into the injected API as well, but that script is generated once
    /// at document-start, so the value there is only ever the one that was true
    /// before the theme existed. Kept current by an assignment from
    /// [`diff`](Self::diff), like every other piece of page state.
    link_dead: bool,
    /// Statements the page has not yet confirmed running.
    ///
    /// [`diff`](Self::diff) commits its state the moment it notices a change,
    /// but the JavaScript is evaluated asynchronously and can fail — a crashed
    /// web process, a page mid-reload. Without this queue a single failed
    /// evaluation dropped the event permanently: the diff would never notice it
    /// again, and a theme waiting on `authentication_complete` waited forever.
    pending: Vec<Queued>,
    /// The evaluation whose verdict is still out, if there is one.
    ///
    /// `Some` *is* outstanding-ness — see [`InFlight`] for why that is not the
    /// same as its count being non-zero.
    in_flight: Option<InFlight>,
    /// How many pump ticks have passed with nothing outstanding and nothing
    /// queued. See [`tick`](Self::tick)'s heartbeat.
    idle_ticks: u32,
    /// Which evaluation `in_flight` belongs to.
    ///
    /// [`restart`](Self::restart) and a timed-out [`tick`](Self::tick) both bump
    /// it, and a verdict tagged with an older one is discarded. Without that,
    /// the real ordering loses events: the theme calls `authenticate()`, which
    /// restarts the bridge; the next tick queues and evaluates the new
    /// conversation's statements; and only *then* does the pre-restart
    /// evaluation land, acknowledging statements it never carried. Zeroing
    /// `in_flight` is not enough, because the straggler arrives after it has
    /// been set again — which is exactly the shape of a late verdict for an
    /// evaluation that was already written off as timed out.
    epoch: u64,
    /// Failed evaluations since the last successful one, counting one that
    /// never answered at all.
    consecutive_failures: u32,
    /// The last WebKit error text the journal was told about in this run of
    /// trouble.
    ///
    /// Not the same question as "is this the first failure": a missed deadline
    /// produces a failure with no error string to log, so counting was enough
    /// to spend the once-only budget on silence and then discard the WebKit
    /// error that arrived next — the only line naming the statement that broke.
    ///
    /// The text rather than a bool, because a run of failures is not
    /// necessarily a run of the *same* failure: one statement's `ReferenceError`
    /// and another's `TypeError` are two different things to fix, and a bool
    /// printed whichever arrived first and silently swallowed the other for as
    /// long as the trouble lasted. See
    /// [`claim_error_report`](Self::claim_error_report).
    last_reported: Option<String>,
    /// Next token handed to a [`Queued::pair`], never reused.
    ///
    /// A counter rather than the prompt id it happens to accompany: the ids are
    /// wdm's, and a pairing added here for anything else would have to borrow
    /// an id space it does not own.
    next_pair: u64,
}

impl Bridge {
    /// Queue the JavaScript reporting everything that changed since last call.
    ///
    /// Each entry is a complete statement, evaluated in the page. A theme that
    /// has not defined a given callback simply does not get called, which is
    /// what makes a minimal theme possible.
    ///
    /// ## What `show_message`'s `kind` means
    ///
    /// One call per message PAM sent, carrying that message's own style —
    /// `"info"` for `PAM_TEXT_INFO`, `"error"` for `PAM_ERROR_MSG`. PAM sends
    /// them one at a time and the two mean different things, so they are
    /// reported one at a time rather than joined into a single line: a theme
    /// that wants to show a lockout in red and the minutes-remaining beside it
    /// in grey can, and one that treats both the same simply ignores the
    /// argument.
    ///
    /// `Model::error` is reported as `"error"` too, but it is a different kind
    /// of thing — the conversation's verdict rather than something the stack
    /// chose to say — and it arrives after every notice, so a theme that
    /// re-renders on it still has the explanation in hand.
    pub fn diff(&mut self, model: &Model) {
        let out = &mut self.pending;

        // Ordering is deliberate: messages explaining a failure are delivered
        // before the completion callback that a theme reacts to, so a theme
        // that re-renders on completion still has the explanation in hand.
        //
        // First, because everything below it is something a theme reacts to and
        // this is the fact that decides what reacting can usefully do. When the
        // connection to wdm is gone, `Link::pump` returns false forever: the
        // conversation still ends, `authentication_complete` still fires, and a
        // theme that offers "press Enter to try again" would post into a socket
        // nobody reads and clear the one message explaining why nothing works.
        if model.link_dead != self.link_dead {
            self.link_dead = model.link_dead;
            out.push(Queued::assignment(format!(
                "window.wdm.link_dead = {};",
                model.link_dead
            )));
        }

        // The count is repaired first rather than trusted. The list is cleared
        // by the client on its own account — `AuthOk` does it with no restart
        // here — and a counter left pointing past the end swallows every notice
        // that follows: the password-expiry warning, the motd and the last-login
        // line all arrive from `pam_open_session` inside exactly that window,
        // as does a `link_lost` during the handoff.
        //
        // Keyed on the client's generation rather than on the length, because
        // the length cannot see the clear that matters. `Link::pump` drains the
        // whole batch of events before this runs once, so the clear and the
        // notices that follow it arrive together and leave the list the same
        // size or larger — a length comparison reports nothing happened, and a
        // clamp to the length has nothing to clamp. Re-reporting a notice the
        // page has already seen is what this trades for, and it is the
        // recoverable failure of the two.
        if model.notices_generation != self.notices_generation {
            self.notices_generation = model.notices_generation;
            self.notices_reported = 0;
        }
        if model.notices.len() > self.notices_reported {
            for notice in &model.notices[self.notices_reported..] {
                out.push(Queued::callback(call(
                    "show_message",
                    &[json!(notice.text), json!(notice.kind.as_str())],
                )));
            }
            self.notices_reported = model.notices.len();
        }

        if model.error != self.error {
            self.error = model.error.clone();
            if let Some(text) = &self.error {
                out.push(Queued::callback(error_script(text)));
            }
        }

        let prompt = model.prompt.as_ref().map(|p| p.id);
        if prompt != self.prompt {
            self.prompt = prompt;
            if let Some(prompt) = &model.prompt {
                // One pair, not two independent statements: the theme is told
                // to ask, and `window.wdm._prompt` is what its answer is sent
                // against. See [`Queued::pair`].
                let token = self.next_pair;
                self.next_pair += 1;
                out.push(
                    Queued::assignment(format!(
                        "window.wdm._prompt = {};",
                        literal(&json!({
                            "id": prompt.id,
                            "text": prompt.text,
                            "secret": prompt.secret,
                        }))
                    ))
                    .paired(token),
                );
                let kind = if prompt.secret { "password" } else { "text" };
                out.push(
                    Queued::callback(call("show_prompt", &[json!(prompt.text), json!(kind)]))
                        .paired(token),
                );
            }
        }

        // A conversation ends exactly once, either way. `authenticated` and
        // `conversation_over` are separate flags in the model rather than one
        // verdict, so both are watched.
        let finished = model.authenticated != self.authenticated
            || (model.conversation_over && !self.over && !model.authenticated);
        if finished {
            self.authenticated = model.authenticated;
            self.over = model.conversation_over;
            // `authentication_user` is who the current conversation is for, or
            // null. A failure ends that conversation, so the name goes with it;
            // success keeps it, because the session about to start is theirs.
            let user = if model.authenticated {
                ""
            } else {
                "\nwindow.wdm.authentication_user = null;"
            };
            out.push(Queued::assignment(format!(
                "window.wdm.is_authenticated = {};\nwindow.wdm.in_authentication = false;\nwindow.wdm._prompt = null;{user}",
                model.authenticated
            )));
            out.push(Queued::callback(call("authentication_complete", &[])));
        }

        if !model.conversation_over {
            self.over = false;
        }

        self.enforce_limit();
    }

    /// Drop statements once the queue is past [`PENDING_LIMIT`], assignments
    /// first.
    ///
    /// Only reachable when the page has stopped acknowledging anything, since
    /// a healthy flush empties the queue. Assignments go oldest-first because a
    /// newer one supersedes them; callbacks are touched only once no assignment
    /// is left, because each is an event the user never gets back — and the one
    /// most worth keeping, the explanation of why an account is locked, is the
    /// *oldest* thing in the queue. Evicting by age alone dropped exactly that
    /// and kept a later `show_prompt`.
    ///
    /// Once no assignment is left, callbacks go *newest*-first, which is the
    /// opposite of the assignment pass and deliberately so: the oldest
    /// surviving event is the lockout explanation, and it is the last thing
    /// this will give up. Walking from index 0 on both passes undid the whole
    /// point — the moment assignments ran out it went back to dropping the
    /// oldest event, which is the exact failure the two-pass split exists to
    /// prevent.
    ///
    /// Neither pass may split a [`Queued::pair`]: dooming either half dooms the
    /// other. That is what stops the assignment pass — which by design spends
    /// every assignment before touching an event — from stripping
    /// `window.wdm._prompt` out from under a `show_prompt` it kept.
    ///
    /// ponytail: a page wedged past [`PENDING_LIMIT`] loses the oldest events
    /// it has not acknowledged, and `show_message`/`show_prompt` have no
    /// successor to restore them. The ceiling is the queue depth; the upgrade
    /// path is to acknowledge per statement rather than per evaluation, so what
    /// the page has actually run is known and only genuinely unseen events need
    /// holding — a queue that never fills for any page still answering at all.
    ///
    /// Statements still in flight can be dropped along with the rest, so
    /// `in_flight`'s count shrinks with them. That is why outstanding-ness is
    /// `in_flight` being `Some` and not the count being non-zero: nothing here
    /// touches the epoch, so the evaluation those statements belonged to is
    /// still coming.
    fn enforce_limit(&mut self) {
        let excess = self.pending.len().saturating_sub(PENDING_LIMIT);
        if excess == 0 {
            return;
        }
        log::warn!("theme queue full; dropping {excess} unacknowledged statement(s)");

        let mut doomed = vec![false; self.pending.len()];
        let mut left = excess;
        for pass in [Stmt::Assignment, Stmt::Callback] {
            let newest_first = pass == Stmt::Callback;
            for step in 0..self.pending.len() {
                if left == 0 {
                    break;
                }
                let index = if newest_first {
                    self.pending.len() - 1 - step
                } else {
                    step
                };
                if doomed[index] || self.pending[index].kind != pass {
                    continue;
                }
                doomed[index] = true;
                left -= 1;

                // A pair goes as a unit. Without this the assignment pass —
                // which runs first, precisely because assignments are the
                // expendable ones — dropped `window.wdm._prompt` and left the
                // `show_prompt` beside it standing, and a theme that answers
                // that prompt from its buffer calls `wdm.respond` with no
                // prompt pending. See [`Queued::pair`].
                //
                // Charged against `left` too, so the pass stops as soon as
                // enough has gone rather than over-running by one statement per
                // pair.
                if let Some(token) = self.pending[index].pair {
                    for (other, queued) in self.pending.iter().enumerate() {
                        if !doomed[other] && queued.pair == Some(token) {
                            doomed[other] = true;
                            left = left.saturating_sub(1);
                        }
                    }
                }
            }
        }

        let lost_in_flight = match &self.in_flight {
            Some(flight) => doomed[..flight.count].iter().filter(|d| **d).count(),
            None => 0,
        };
        let mut index = 0;
        self.pending.retain(|_| {
            let keep = !doomed[index];
            index += 1;
            keep
        });
        if let Some(flight) = &mut self.in_flight {
            flight.count -= lost_in_flight;
        }
    }

    /// Everything queued, as one script — or nothing while a verdict is out.
    ///
    /// One evaluation at a time: sending more while one is unresolved would
    /// mean not knowing which statements a later failure lost.
    ///
    /// The join is redone from scratch on every retransmission rather than
    /// cached, which against a page refusing everything is one rebuild per pump
    /// tick. Deliberately: `Outbound::script` is an owned `String` the caller
    /// hands to WebKit as a `&str`, so a cache would still have to be cloned
    /// into it — the allocation and the copy of the whole script stay, and all
    /// that is saved is the intermediate `Vec` and the walk over a queue capped
    /// at [`PENDING_LIMIT`]. Trading that for an invalidation obligation on
    /// every one of the five places that touch `pending` is a bad bargain: a
    /// missed one sends the page a stale script, which is a wrong login screen
    /// rather than a slow one. The whole run is bounded anyway — [`WEDGED_AFTER`]
    /// ticks and the greeter exits to be respawned.
    pub fn flush(&mut self) -> Option<Outbound> {
        if self.in_flight.is_some() || self.pending.is_empty() {
            return None;
        }
        self.in_flight = Some(InFlight {
            count: self.pending.len(),
            ticks: 0,
        });
        self.idle_ticks = 0;
        Some(Outbound {
            epoch: self.epoch,
            script: self
                .pending
                .iter()
                .map(|q| q.script.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        })
    }

    /// Record the verdict on the [`flush`](Self::flush) that produced `epoch`.
    ///
    /// A verdict from before a [`restart`](Self::restart) — or from an
    /// evaluation [`tick`](Self::tick) already wrote off — drains nothing: it
    /// describes statements that have since been abandoned or retransmitted,
    /// and acting on it would acknowledge what the page never ran. A late
    /// *success* still forgives a failure, though, because answering at all is
    /// proof the page is alive. Without that, a theme consistently slower than
    /// [`VERDICT_DEADLINE`] gained a failure per deadline and could never shed
    /// one: "slow" escalated to wedged, the greeter quit, respawned, and did it
    /// again.
    ///
    /// One failure, not all of them. The count a stale success finds is not
    /// the count its own evaluation earned — the retransmission that replaced
    /// it has been failing since, and zeroing erases that. A page answering one
    /// deadline late nets out at zero either way, so the slow theme is still
    /// forgiven; a page answering less often than it is asked keeps climbing,
    /// which is the difference between "slow" and "not really there".
    ///
    /// A failure keeps the statements queued, so the next flush retransmits all
    /// of them — an evaluation that came back `Err` demonstrably did not run,
    /// so nothing it carried has been seen. That is *not* true of one written
    /// off for silence; [`tick`](Self::tick) is where the difference is paid.
    pub fn delivered(&mut self, epoch: u64, ok: bool) {
        if epoch != self.epoch {
            if ok {
                self.consecutive_failures = self.consecutive_failures.saturating_sub(1);
                if self.consecutive_failures == 0 {
                    self.last_reported = None;
                }
            }
            return;
        }
        if ok {
            let carried = self.in_flight.as_ref().map_or(0, |flight| flight.count);
            self.pending.drain(..carried);
            self.consecutive_failures = 0;
            self.last_reported = None;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        }
        self.in_flight = None;
    }

    /// Claim the right to print `error` to the journal, if it has not already
    /// been printed in this run of trouble.
    ///
    /// A predicate with a side effect, named for the claim rather than for the
    /// printing: the caller does the logging, and `if … && bridge.report_error()`
    /// read as though this were the thing that logged and could fail.
    ///
    /// True at most once per *distinct* error text, and only for a caller that
    /// has one — gating on the failure count instead spent the budget on the
    /// first missed deadline, which has no error text, and then discarded the
    /// WebKit error that named the broken statement. Keying on the text keeps
    /// the flood suppressed, because a page failing the same way sixty times a
    /// second reports the same string sixty times a second, while a second,
    /// different cause still reaches the journal on the tick it first appears.
    pub fn claim_error_report(&mut self, error: &str) -> bool {
        if self.last_reported.as_deref() == Some(error) {
            return false;
        }
        self.last_reported = Some(error.to_owned());
        true
    }

    /// Advance the deadline on the evaluation in flight, if there is one.
    ///
    /// Called once per pump tick. This is the only thing that notices a web
    /// process which took the script and said nothing — [`delivered`] hears
    /// only from evaluations that finished, so a hung web process or a theme
    /// callback in an infinite loop reaches it never. Left to itself that state
    /// is permanent: `in_flight` stays set, [`flush`] returns `None` on every
    /// tick, and the greeter sits alive in front of a login screen that does
    /// nothing — which is precisely the case [`is_wedged`] is named for and,
    /// before this existed, the one case it could not see.
    ///
    /// A missed deadline is counted as a failed evaluation rather than as its
    /// own kind of trouble, so there is one way to give up instead of two, and
    /// a page that alternates between refusing and stalling still escalates.
    ///
    /// It also *heartbeats*. A theme whose top-level script never returns, or a
    /// web process that hangs before `LoadEvent::Finished`, queues nothing at
    /// all — so with nothing in flight there was nothing to time out, nothing
    /// to escalate, and the greeter sat alive in front of a frozen page: the
    /// exact state this whole mechanism exists to end. An idle stretch as long
    /// as the deadline therefore queues a statement that does nothing, purely
    /// so the page has something to answer. Answering it proves the page is
    /// alive; not answering it escalates down the path above.
    ///
    /// [`delivered`]: Self::delivered
    /// [`flush`]: Self::flush
    /// [`is_wedged`]: Self::is_wedged
    pub fn tick(&mut self) {
        match &mut self.in_flight {
            None => {
                self.heartbeat();
                return;
            }
            Some(flight) => {
                flight.ticks = flight.ticks.saturating_add(1);
                if flight.ticks < VERDICT_DEADLINE {
                    return;
                }
            }
        }

        if self.consecutive_failures == 0 {
            log::warn!("the theme has not answered in {VERDICT_DEADLINE} pump ticks");
        }

        // Everything the write-off carried may already have run — silence is
        // not a refusal — so the callbacks among it are dropped rather than
        // sent again. The default theme appends messages, so a page merely
        // slower than the deadline would otherwise render "The account is
        // locked." once per retransmission. The assignments stay: they are
        // idempotent, and losing them would leave the page's state behind the
        // model's.
        //
        // ponytail: a page that stalls past VERDICT_DEADLINE loses the PAM
        // messages of that evaluation — the lockout explanation among them —
        // because "may have run" is the most that can be said about any of
        // them. The ceiling is that a verdict describes a whole evaluation; the
        // upgrade path is to acknowledge per statement rather than per
        // evaluation, so a timed-out flush can be replayed exactly, resending
        // what the page never reached and nothing it did.
        // Taken here rather than reset at the end: the evaluation is over as of
        // this line, and its count is the split point below.
        let carried = self
            .in_flight
            .take()
            .expect("the match above returned unless one was outstanding")
            .count;
        let tail = self.pending.split_off(carried);
        let before = self.pending.len();
        self.pending.retain(|q| q.kind == Stmt::Assignment);
        let dropped = before - self.pending.len();
        self.pending.extend(tail);
        if dropped > 0 {
            log::warn!(
                "theme did not answer in time; dropping {dropped} callback(s) it may have run"
            );
        }

        // The epoch is bumped for the same reason `restart` bumps it: the
        // evaluation has been written off, but it has not been cancelled, and
        // WebKit may still produce a verdict for it. Tagged with the old epoch,
        // that verdict drains nothing — where without the bump a late success
        // would drain `in_flight` statements belonging to the retransmission
        // that has since been sent, acknowledging what the page never ran.
        self.epoch = self.epoch.wrapping_add(1);
        // Along with `in_flight` above: the page is back to having nothing outstanding,
        // and that is the state `idle_ticks` counts. Leaving it where the last
        // idle stretch happened to stop is currently harmless — the queue is
        // never empty here — but every other transition leaves this consistent
        // and one that does not is a trap for whoever changes the next one.
        self.idle_ticks = 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// Queue something for an idle page to answer, so silence is still visible.
    ///
    /// [`HEARTBEAT`] is an assignment as far as the queue is concerned —
    /// nothing to duplicate, nothing to lose — and it exists only to give the
    /// next flush a script to send.
    fn heartbeat(&mut self) {
        if !self.pending.is_empty() {
            self.idle_ticks = 0;
            return;
        }
        self.idle_ticks = self.idle_ticks.saturating_add(1);
        if self.idle_ticks < VERDICT_DEADLINE {
            return;
        }
        self.idle_ticks = 0;
        self.pending.push(Queued::assignment(HEARTBEAT.to_owned()));
    }

    /// Failed evaluations since the last that succeeded.
    ///
    /// Whether to log is [`claim_error_report`](Self::claim_error_report)'s question, not
    /// this one's: retrying every 16ms forever would otherwise be sixty
    /// warnings a second in the journal.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Whether the page has stopped answering for long enough to give up on.
    ///
    /// A web process that is wedged rather than crashed leaves wdm's supervisor
    /// looking at a healthy greeter process and a login screen that does
    /// nothing, indefinitely. Exiting is what turns that into a respawn.
    ///
    /// Both halves of "stopped answering" count towards this, and they have to:
    /// a web process that refuses every evaluation is reported by
    /// [`delivered`](Self::delivered), but one that is hung rather than crashed
    /// refuses nothing — it simply never comes back, and only
    /// [`tick`](Self::tick) sees that.
    pub fn is_wedged(&self) -> bool {
        self.consecutive_failures >= WEDGED_AFTER
    }

    /// Forget what the *page* has been told about the link, so the next
    /// [`diff`](Self::diff) says it again whatever it is.
    ///
    /// Called on load-finished, and only for `link_dead`, because that is the
    /// one piece of page state a reload silently rewinds. It is baked into the
    /// document-start [`api_script`], which re-runs on every load with the value
    /// frozen at startup — so a link that died after startup leaves a reloaded
    /// page believing it is alive, while this bridge, having already sent the
    /// assignment once, sees no change and never corrects it. The theme then
    /// offers a retry into a socket nobody reads and clears the one message
    /// explaining why nothing works.
    ///
    /// Inverting the mirror rather than emitting directly keeps one place
    /// deciding what the assignment looks like: the next diff finds a
    /// disagreement and does what it always does.
    pub fn resync(&mut self) {
        self.link_dead = !self.link_dead;
    }

    /// Forget the previous conversation, so its verdict is reported again if it
    /// repeats. Called when the theme starts a new attempt.
    ///
    /// Also drops anything still pending: those statements describe the
    /// conversation being abandoned. The epoch is carried across and bumped, so
    /// a verdict from before the restart is recognised as stale rather than
    /// applied to whatever has been queued since.
    pub fn restart(&mut self) {
        let epoch = self.epoch.wrapping_add(1);
        *self = Self::default();
        self.epoch = epoch;
    }
}

/// A JavaScript literal for a value that came from outside.
///
/// `serde_json` escapes what JSON requires, which is enough for the two places
/// this is used — both hand their result straight to a JavaScript engine, with
/// no HTML parser in between. The extra three escapes are for the day that
/// stops being true: `<` cannot start a `</script>` that ends an inline script,
/// and U+2028/U+2029 are line terminators to a parser older than ES2019.
fn literal(value: &serde_json::Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// The statement reporting an error to the page, shared between [`Bridge::diff`]
/// and the one delivery that happens outside the queue: an error that predates
/// the page — a failed session's parting message — is sent directly on
/// load-finished, and it must go through the same escaping as everything else,
/// because its text is PAM's and wdm's, not ours.
pub fn error_script(text: &str) -> String {
    call("show_message", &[json!(text), json!("error")])
}

/// A call to a theme-defined global, skipped when the theme did not define it.
///
/// The try/catch is load-bearing: the queued statements are evaluated as one
/// script, so a theme callback that throws would otherwise abort every
/// statement after it — including the `is_authenticated` assignment a broken
/// `show_message` has no business suppressing.
///
/// `name` is interpolated into the script unescaped, which is why it is
/// `&'static str`: it is the *shape* of the statement, not data, and every
/// caller passes a literal. The compiler enforcing that is the point — nothing
/// derived from PAM, from the account database or from the page can reach it,
/// and the escaping discipline that covers `args` has no hole to guard here.
fn call(name: &'static str, args: &[serde_json::Value]) -> String {
    let args: Vec<String> = args.iter().map(literal).collect();
    format!(
        "if (typeof window.{name} === 'function') {{ try {{ window.{name}({}); }} catch (e) {{ console.error('wdm: {name}:', e); }} }}",
        args.join(", ")
    )
}

/// The API object, built from the model once the lists are known.
///
/// Injected at document-start so a theme can read `wdm.users` from its own
/// top-level script rather than having to wait for a ready callback.
pub fn api_script(model: &Model) -> String {
    let users: Vec<_> = model
        .users
        .iter()
        .map(|u| {
            json!({
                "name": u.name,
                "display_name": u.display_name,
                "last_session": u.last_session,
            })
        })
        .collect();

    let sessions: Vec<_> = model
        .sessions
        .iter()
        .map(|s| json!({ "id": s.id, "name": s.name }))
        .collect();

    // `post` is the only way out of the page. Everything else is sugar over it,
    // including the argument checks — a theme that calls respond() with no
    // pending prompt gets an exception it can see, rather than a message the
    // compositor rejects as a protocol error and kills the greeter for.
    //
    // start_session needs two checks, not one. Its fallback to the first
    // session has nothing to fall back to when no sessions are installed, and
    // posting `undefined` then sends `["start_session"]` — which `parse`
    // refuses and the message handler logs to a journal nobody is reading,
    // while the theme, having thrown nothing, sits on "Starting session…"
    // forever. An exception is the whole promise of these guards: a mistake
    // shows up in the theme, not as silence.
    format!(
        r#"window.wdm = {{
  users: {users},
  sessions: {sessions},
  default_session: {default_session},
  authentication_user: null,
  is_authenticated: false,
  in_authentication: false,
  link_dead: {link_dead},
  _prompt: null,
  _post(verb, arg) {{
    window.webkit.messageHandlers.wdm.postMessage(
      JSON.stringify(arg === undefined ? [verb] : [verb, String(arg)]));
  }},
  authenticate(username) {{
    if (!username) {{ throw new Error('wdm.authenticate needs a username'); }}
    this.authentication_user = username;
    this.in_authentication = true;
    this.is_authenticated = false;
    this._prompt = null;
    this._post('authenticate', username);
  }},
  respond(text) {{
    if (!this._prompt) {{ throw new Error('wdm.respond with no prompt pending'); }}
    this._prompt = null;
    this._post('respond', text);
  }},
  cancel() {{
    this.authentication_user = null;
    this.in_authentication = false;
    this._prompt = null;
    this._post('cancel');
  }},
  start_session(id) {{
    if (!this.is_authenticated) {{ throw new Error('wdm.start_session before authenticating'); }}
    const session = id || (this.sessions[0] && this.sessions[0].id);
    if (!session) {{ throw new Error('wdm.start_session needs a session id'); }}
    this._post('start_session', session);
  }},
}};
"#,
        users = literal(&serde_json::Value::Array(users)),
        sessions = literal(&serde_json::Value::Array(sessions)),
        // The machine's configured default, an empty string when unset. A
        // theme's preselection chain — history, then this, then the first
        // session — mirrors Model::preferred_session; exposing the middle link
        // is what lets a theme make the same choice.
        default_session = literal(&json!(model.default_session)),
        // Whether the connection to wdm is already gone. Only ever true here if
        // it died before the page loaded; after that `Bridge::diff` keeps it
        // current, because this script runs once and never again.
        link_dead = model.link_dead,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wdm_greeter_client::{NoticeKind, Prompt, Session, User};

    #[test]
    fn parses_the_verbs() {
        assert_eq!(
            parse(r#"["authenticate","joseph"]"#),
            Some(Request::Authenticate("joseph".to_owned()))
        );
        assert_eq!(parse(r#"["cancel"]"#), Some(Request::Cancel));
        assert_eq!(
            parse(r#"["start_session","gnome"]"#),
            Some(Request::StartSession("gnome".to_owned()))
        );
    }

    #[test]
    fn an_answer_survives_verbatim() {
        // The NUL is the point: the payload crosses as a C string, so the
        // first version of this — verb, separator, argument — silently
        // delivered an empty password. JSON escapes it as text instead.
        let Some(Request::Respond(text)) = parse(r#"["respond","p\u0000a s\tsw\"rd"]"#) else {
            panic!("not a respond");
        };
        assert_eq!(text, "p\0a s\tsw\"rd");

        // And an empty answer is legitimate — PAM asks, the user answers
        // nothing, and PAM decides what that means.
        assert_eq!(
            parse(r#"["respond",""]"#),
            Some(Request::Respond(String::new()))
        );
    }

    #[test]
    fn rejects_nonsense_and_missing_arguments() {
        assert_eq!(parse(r#"["authenticate"]"#), None);
        assert_eq!(parse(r#"["authenticate",""]"#), None);
        assert_eq!(parse(r#"["start_session",""]"#), None);
        assert_eq!(parse(r#"["respond","a","b"]"#), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"["shutdown","now"]"#), None);
    }

    /// The client's types are `#[non_exhaustive]`, so each of these is built
    /// through `Default` and mutated rather than written as a literal.
    fn user() -> User {
        let mut user = User::default();
        user.name = "joseph".to_owned();
        user.display_name = "Joseph".to_owned();
        user.last_session = "gnome".to_owned();
        user
    }

    fn session() -> Session {
        let mut session = Session::default();
        session.id = "gnome".to_owned();
        session.name = "GNOME".to_owned();
        session
    }

    fn prompt(id: u32, text: &str, secret: bool) -> Prompt {
        let mut prompt = Prompt::default();
        prompt.id = id;
        prompt.text = text.to_owned();
        prompt.secret = secret;
        prompt
    }

    fn model() -> Model {
        let mut model = Model::default();
        model.users = vec![user()];
        model.sessions = vec![session()];
        model
    }

    /// Diff, flush and acknowledge in one step — one healthy pump tick.
    fn drain(bridge: &mut Bridge, model: &Model) -> String {
        bridge.diff(model);
        let Some(out) = bridge.flush() else {
            return String::new();
        };
        bridge.delivered(out.epoch, true);
        out.script
    }

    #[test]
    fn reports_a_prompt_once() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(prompt(7, "Password:", true));

        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_prompt"), "{js}");
        assert!(js.contains("\"password\""), "{js}");

        // Polled state must not re-fire; a theme animating the prompt would be
        // restarted sixty times a second.
        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn a_new_prompt_with_the_same_text_still_fires() {
        // Prompt ids are never reused, which is what makes them the right key:
        // PAM asking "Password:" a second time is a different question.
        let mut bridge = Bridge::default();
        let mut model = model();
        for id in [1, 2] {
            model.prompt = Some(prompt(id, "Password:", true));
            assert!(
                drain(&mut bridge, &model).contains("show_prompt"),
                "prompt {id} was swallowed"
            );
        }
    }

    #[test]
    fn reports_failure_then_completion() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());

        let js = drain(&mut bridge, &model);
        let message = js.find("show_message").unwrap();
        let complete = js.find("authentication_complete").unwrap();
        // A theme that redraws on completion needs the reason already in hand.
        assert!(message < complete, "{js}");
        assert!(js.contains("is_authenticated = false"), "{js}");
        // The conversation the name belonged to is over; a theme reading
        // authentication_user after a failure must see null, as documented.
        assert!(js.contains("authentication_user = null"), "{js}");

        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn each_notice_carries_the_style_pam_gave_it() {
        // The defect this pins: both PAM styles used to arrive as one joined
        // string reported as "info", so a theme could not tell a lockout from
        // the sentence explaining how long it lasts, and the WebKit greeter
        // disagreed with the reference greeter on identical input.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        model.push_notice(NoticeKind::Info, "(10 minutes left to unlock)".to_owned());

        let js = drain(&mut bridge, &model);
        let locked = js.find("The account is locked.").unwrap();
        let minutes = js.find("10 minutes left").unwrap();

        // Two calls, not one joined line, and in the order PAM sent them.
        assert_eq!(js.matches("window.show_message(").count(), 2, "{js}");
        assert!(locked < minutes, "{js}");

        // Each with its own style: the lockout is the error, the duration is not.
        let error_kind = js.find("\"error\"").unwrap();
        let info_kind = js.find("\"info\"").unwrap();
        assert!(locked < error_kind && error_kind < minutes, "{js}");
        assert!(minutes < info_kind, "{js}");

        // Polled state must not re-report what the page has already been told.
        assert!(drain(&mut bridge, &model).is_empty());

        // And a notice arriving later is the only one reported.
        model.push_notice(NoticeKind::Error, "Account expired.".to_owned());
        let js = drain(&mut bridge, &model);
        assert_eq!(js.matches("window.show_message(").count(), 1, "{js}");
        assert!(js.contains("Account expired."), "{js}");
    }

    /// What `AuthOk` does to the model: the notices go, and the generation is
    /// stamped so anyone counting them knows to start again.
    fn clear_notices(model: &mut Model) {
        model.notices.clear();
        model.notices_generation = model.notices_generation.wrapping_add(1);
    }

    #[test]
    fn a_notice_after_the_model_clears_is_still_reported() {
        // The counter cannot trust the list to only grow. `wdm-greeter-client`
        // clears it from its own `AuthOk` handler with no restart here, and
        // that is precisely the window `pam_open_session` speaks in: the
        // password-expiry warning, the motd, the last-login line — and a
        // link_lost during the handoff. A stale-high counter swallows them all.
        let mut bridge = Bridge::default();
        let mut model = model();
        for n in 0..3 {
            model.push_notice(NoticeKind::Info, format!("notice {n}"));
        }
        assert!(drain(&mut bridge, &model).contains("notice 2"));

        clear_notices(&mut model);
        model.push_notice(
            NoticeKind::Info,
            "your password expires in 3 days".to_owned(),
        );

        let js = drain(&mut bridge, &model);
        assert!(js.contains("your password expires in 3 days"), "{js}");
    }

    #[test]
    fn a_clear_the_length_cannot_see_still_restarts_the_count() {
        // The case the length comparison could not fire for, and the reason
        // this is keyed on the client's generation instead. `Link::pump` drains
        // a whole batch of Wayland events before `diff` runs once, so `AuthOk`'s
        // clear and everything `pam_open_session` says after it arrive
        // together: one notice reported, cleared, one pushed, and the list is
        // exactly as long as it was. A shrink check never fires, a clamp has
        // nothing to clamp, and the password-expiry warning is swallowed.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "welcome back".to_owned());
        assert!(drain(&mut bridge, &model).contains("welcome back"));

        clear_notices(&mut model);
        model.push_notice(
            NoticeKind::Info,
            "your password expires in 3 days".to_owned(),
        );
        assert_eq!(
            model.notices.len(),
            1,
            "the equal-length case was not set up"
        );

        let js = drain(&mut bridge, &model);
        assert!(js.contains("your password expires in 3 days"), "{js}");

        // And the same again with more than one, so a fix that happens to work
        // for a single notice is not enough: report two, clear, push two.
        for n in 0..2 {
            model.push_notice(NoticeKind::Info, format!("first {n}"));
        }
        drain(&mut bridge, &model);
        clear_notices(&mut model);
        for n in 0..3 {
            model.push_notice(NoticeKind::Info, format!("second {n}"));
        }
        let js = drain(&mut bridge, &model);
        for n in 0..3 {
            assert!(js.contains(&format!("second {n}")), "{js}");
        }
    }

    #[test]
    fn the_explanation_precedes_the_verdict() {
        // The default theme appends, so the order these are queued in is the
        // order the user reads them in: reversing the two blocks in `diff`
        // would put "Authentication failure" above the reason for it.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());

        let js = drain(&mut bridge, &model);
        let notice = js.find("The account is locked.").unwrap();
        let verdict = js.find("Authentication failure").unwrap();
        let complete = js.find("authentication_complete").unwrap();
        assert!(notice < verdict, "{js}");
        assert!(verdict < complete, "{js}");
    }

    #[test]
    fn a_new_attempt_reports_its_notices_again() {
        // begin_attempt clears the list and restart() resets the counter with
        // it; if the two ever disagreed, the count would index past the end or
        // silently swallow the next conversation's first messages.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        assert!(drain(&mut bridge, &model).contains("show_message"));

        model.begin_attempt();
        bridge.restart();
        assert!(drain(&mut bridge, &model).is_empty());

        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        let js = drain(&mut bridge, &model);
        assert_eq!(js.matches("window.show_message(").count(), 1, "{js}");
        assert!(js.contains("\"error\""), "{js}");
    }

    #[test]
    fn reports_success() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;

        let js = drain(&mut bridge, &model);
        assert!(js.contains("is_authenticated = true"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
        // On success the name stays: the session being started is for them.
        assert!(!js.contains("authentication_user = null"), "{js}");
    }

    #[test]
    fn a_preexisting_error_is_reported() {
        // The error a failed session leaves behind is already in the model when
        // the greeter connects — no later revision change ever announces it. A
        // fresh bridge diffing that model must report it anyway; the bug was
        // that diff() was only ever called on a change.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.error = Some("session exited unexpectedly".to_owned());

        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_message"), "{js}");
        assert!(js.contains("\"error\""), "{js}");
    }

    #[test]
    fn the_api_exposes_the_machine_default_session() {
        // Model::preferred_session falls back history → default → first; a
        // theme can only mirror that chain if the middle link is in the API.
        let mut model = model();
        model.default_session = "gnome".to_owned();
        let script = api_script(&model);
        assert!(script.contains("default_session: \"gnome\""), "{script}");

        // And unset is an empty string, not an absent field.
        assert!(api_script(&Model::default()).contains("default_session: \"\""));
    }

    #[test]
    fn a_second_attempt_reports_its_own_failure() {
        // Two identical failures in a row are two events. Keying on the
        // *value* of the error would report only the first, leaving a theme
        // that clears its own form waiting forever on the second.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());
        assert!(!drain(&mut bridge, &model).is_empty());

        model.begin_attempt();
        bridge.restart();
        assert!(drain(&mut bridge, &model).is_empty());

        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());
        let js = drain(&mut bridge, &model);
        assert!(js.contains("show_message"), "{js}");
        assert!(js.contains("authentication_complete"), "{js}");
    }

    #[test]
    fn a_failed_evaluation_is_retransmitted() {
        // The web process can be mid-crash or mid-reload when the script is
        // sent. The diff has already committed its state by then, so losing
        // the script here would lose the event permanently — a theme waiting
        // on authentication_complete would wait forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;

        bridge.diff(&model);
        let first = bridge.flush().unwrap();
        bridge.delivered(first.epoch, false);

        let retry = bridge.flush().unwrap();
        assert_eq!(first.script, retry.script);

        bridge.delivered(retry.epoch, true);
        assert!(bridge.flush().is_none());
    }

    #[test]
    fn one_evaluation_at_a_time() {
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "first".to_owned());

        bridge.diff(&model);
        let first = bridge.flush().unwrap();
        assert!(first.script.contains("first"), "{}", first.script);

        // A change arriving while the verdict is out queues behind it…
        model.error = Some("second".to_owned());
        bridge.diff(&model);
        assert!(bridge.flush().is_none());

        // …and success only acknowledges what was actually sent.
        bridge.delivered(first.epoch, true);
        let second = bridge.flush().unwrap().script;
        assert!(!second.contains("first"), "{second}");
        assert!(second.contains("second"), "{second}");
    }

    #[test]
    fn restart_drops_the_queue() {
        // Statements pending at restart describe a conversation the theme
        // abandoned; delivering them into the new one would report a stale
        // verdict.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;
        bridge.diff(&model);
        assert!(bridge.flush().is_some());

        bridge.restart();
        assert!(bridge.flush().is_none());
    }

    #[test]
    fn a_straggler_from_before_a_restart_acknowledges_nothing() {
        // The real ordering, which is what makes the epoch necessary. The
        // theme calls authenticate(), which restarts the bridge; the next
        // pump tick diffs, flushes and evaluates the new conversation; and
        // only *then* does the pre-restart evaluation come back. Zeroing
        // in_flight in restart() cannot help, because it has been set again by
        // the time the straggler lands — it would drain statements it never
        // carried, and the real verdict would then drain nothing, losing an
        // authentication_complete and leaving the theme waiting forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.authenticated = true;
        bridge.diff(&model);
        let stale = bridge.flush().unwrap();

        bridge.restart();

        model.begin_attempt();
        model.authenticated = true;
        bridge.diff(&model);
        let fresh = bridge.flush().unwrap();
        assert!(fresh.script.contains("authentication_complete"));
        assert_ne!(stale.epoch, fresh.epoch);

        bridge.delivered(stale.epoch, true); // the straggler, landing late
        bridge.delivered(fresh.epoch, false); // and then the real verdict

        let retry = bridge.flush().expect("the straggler ate the new event");
        assert_eq!(retry.script, fresh.script);
    }

    #[test]
    fn a_wedged_page_is_eventually_declared_dead() {
        // A permanently failing evaluation is retried every 16ms forever while
        // wdm's supervisor sees a healthy greeter process, so the login screen
        // sits dead indefinitely unless the greeter admits it.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);

        for _ in 0..WEDGED_AFTER - 1 {
            let out = bridge.flush().expect("still retransmitting");
            bridge.delivered(out.epoch, false);
            assert!(!bridge.is_wedged());
        }
        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, false);
        assert!(bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);

        // And a page that comes back is not wedged. Recovery is the common
        // case — a web process crash-and-respawn is a handful of ticks.
        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, true);
        assert!(!bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), 0);
    }

    #[test]
    fn a_page_that_never_answers_at_all_is_declared_dead() {
        // The case is_wedged is named for and could not see: a web process hung
        // rather than crashed, or a theme callback in an infinite loop. Nothing
        // ever comes back, so `delivered` is never called, and before `tick`
        // existed in_flight stayed set, flush returned None on every tick, and
        // the greeter sat alive in front of a dead login screen forever.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);

        // The pump, faithfully: tick, then flush, and a verdict never arrives.
        let first = {
            let mut first = None;
            let mut ticks = 0u32;
            while !bridge.is_wedged() {
                bridge.tick();
                if let Some(out) = bridge.flush() {
                    first.get_or_insert(out.epoch);
                }
                ticks += 1;
                assert!(ticks < 10_000, "the greeter never gave up");
            }
            first.expect("nothing was ever sent")
        };
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);

        // And a *failed* verdict for that evaluation cannot undo it: it
        // describes an attempt already written off. (A late success is
        // different, and deliberately so — see the slow-page test below.)
        bridge.delivered(first, false);
        assert!(bridge.is_wedged());
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);
    }

    #[test]
    fn a_page_slower_than_the_deadline_can_still_recover() {
        // Every verdict arrives after its own deadline has passed, so every
        // evaluation is written off — but the page is answering. Discarding a
        // late verdict wholesale gained it a failure per deadline with no way
        // to shed one, so "slow" escalated to wedged: the greeter quit,
        // respawned, and did the same thing again.
        let mut bridge = Bridge::default();
        let mut model = model();

        for n in 0..WEDGED_AFTER * 2 {
            model.push_notice(NoticeKind::Info, format!("notice {n}"));
            bridge.diff(&model);
            let out = bridge.flush().expect("nothing was queued");
            for _ in 0..VERDICT_DEADLINE {
                bridge.tick();
            }
            bridge.delivered(out.epoch, true); // late, but it answered
            assert!(!bridge.is_wedged(), "a slow page was declared dead");
            assert_eq!(bridge.consecutive_failures(), 0);
        }
    }

    #[test]
    fn a_page_answering_less_often_than_it_is_asked_still_escalates() {
        // The other side of the forgiveness above, and what a stale-epoch
        // success zeroing the count destroyed. The count a late verdict finds
        // is not the one its own evaluation earned: the retransmission that
        // replaced it has been failing since, and wiping those failures let a
        // page answering one evaluation in two sit at zero forever — never
        // wedged, never recovered, and a login screen doing nothing.
        //
        // Forgiving exactly one is what tells the two apart: a page one
        // deadline late nets out at zero, a page half as fast as it is asked
        // climbs.
        let mut bridge = Bridge::default();
        let mut model = model();

        let mut rounds = 0u32;
        while !bridge.is_wedged() {
            let mut oldest = None;
            for _ in 0..2 {
                model.push_notice(NoticeKind::Info, "still here".to_owned());
                bridge.diff(&model);
                if let Some(out) = bridge.flush() {
                    oldest.get_or_insert(out.epoch);
                }
                for _ in 0..VERDICT_DEADLINE {
                    bridge.tick();
                }
            }
            // One answer for the two it was asked, and long stale by now.
            bridge.delivered(oldest.expect("nothing was ever sent"), true);

            rounds += 1;
            assert!(
                rounds < 10_000,
                "a page half as fast as the pump never gave up"
            );
        }
    }

    #[test]
    fn a_timed_out_evaluation_does_not_repeat_its_callbacks() {
        // A timed-out evaluation is not a refused one: it may well have run.
        // The default theme appends messages, so retransmitting the whole
        // script rendered "The account is locked." once per deadline — up to
        // sixty times for a page slower than half a second.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        model.authenticated = true;
        bridge.diff(&model);
        let out = bridge.flush().unwrap();
        assert!(out.script.contains("The account is locked."));

        for _ in 0..VERDICT_DEADLINE {
            bridge.tick();
        }

        let retry = bridge.flush().expect("nothing was retransmitted");
        assert!(
            !retry.script.contains("The account is locked."),
            "a message the page may already have shown was sent again: {}",
            retry.script
        );
        assert!(
            !retry.script.contains("authentication_complete"),
            "{}",
            retry.script
        );
        // The assignments do go again: they are idempotent, and losing them
        // leaves the page's state behind the model's.
        assert!(
            retry.script.contains("is_authenticated = true"),
            "{}",
            retry.script
        );
    }

    #[test]
    fn a_full_queue_does_not_disarm_the_hang_detector() {
        // `enforce_limit` can evict the very statements an unresolved
        // evaluation carried, taking `in_flight` to zero without touching the
        // epoch. Reading that as "nothing outstanding" stopped the deadline
        // advancing and let the next flush issue a second concurrent
        // evaluation sharing an epoch — so the detector switched itself off in
        // exactly the state that fills the queue in the first place.
        let mut bridge = Bridge::default();
        let mut model = model();

        // The heartbeat is a lone assignment, which makes it the first thing
        // evicted once the queue fills.
        for _ in 0..VERDICT_DEADLINE {
            bridge.tick();
        }
        let out = bridge.flush().expect("no heartbeat was queued");
        assert_eq!(out.script, HEARTBEAT);

        for n in 0..=PENDING_LIMIT {
            model.push_notice(NoticeKind::Info, format!("notice {n}"));
            bridge.diff(&model);
        }
        assert_eq!(
            bridge.in_flight.as_ref().map(|f| f.count),
            Some(0),
            "the case was never reached: the evaluation must still be outstanding \
             with every statement it carried evicted"
        );

        assert!(
            bridge.flush().is_none(),
            "issued a second concurrent evaluation"
        );
        for _ in 0..VERDICT_DEADLINE {
            bridge.tick();
        }
        assert_eq!(bridge.consecutive_failures(), 1, "the deadline never fired");
    }

    #[test]
    fn an_idle_page_is_still_asked_whether_it_is_there() {
        // A theme whose top-level script never returns, or a web process that
        // hangs before load-finished, queues nothing at all — so there was
        // nothing to time out, and the greeter sat alive in front of a frozen
        // page forever. The heartbeat is what gives silence something to be
        // silent about.
        let mut bridge = Bridge::default();
        assert!(bridge.flush().is_none(), "something was queued unprompted");

        let mut ticks = 0u32;
        while !bridge.is_wedged() {
            bridge.tick();
            if let Some(out) = bridge.flush() {
                assert_eq!(out.script, HEARTBEAT);
            }
            ticks += 1;
            assert!(ticks < 10_000, "an idle greeter never noticed");
        }
        assert_eq!(bridge.consecutive_failures(), WEDGED_AFTER);
    }

    #[test]
    fn a_page_that_answers_the_heartbeat_is_left_alone() {
        // The other half: an idle greeter in front of a working theme must not
        // talk itself into quitting.
        let mut bridge = Bridge::default();
        for _ in 0..WEDGED_AFTER * 2 {
            for _ in 0..VERDICT_DEADLINE {
                bridge.tick();
            }
            let out = bridge.flush().expect("no heartbeat was queued");
            bridge.delivered(out.epoch, true);
            assert!(!bridge.is_wedged());
        }
    }

    #[test]
    fn the_documented_durations_match_the_pump() {
        // Three comments here quote wall-clock durations derived from a
        // constant that lives in main.rs, so changing the pump silently
        // rescales the give-up threshold and falsifies all three.
        use std::time::Duration;
        assert_eq!(
            crate::PUMP_INTERVAL * VERDICT_DEADLINE,
            Duration::from_millis(480),
            "VERDICT_DEADLINE is no longer about half a second"
        );
        assert_eq!(
            crate::PUMP_INTERVAL * WEDGED_AFTER,
            Duration::from_millis(960),
            "a refusing page no longer gives up after roughly a second"
        );
        assert_eq!(
            crate::PUMP_INTERVAL * VERDICT_DEADLINE * WEDGED_AFTER,
            Duration::from_millis(28_800),
            "a silent page no longer gives up after about half a minute"
        );
    }

    #[test]
    fn a_dead_link_reaches_the_page_after_load() {
        // The injected API is generated once at document-start, so the value
        // baked in there is the one from before the theme existed. A link that
        // dies later has to arrive as an assignment, or the theme offers "press
        // Enter to try again" into a socket nobody reads and wipes the only
        // message explaining why nothing happens.
        let mut bridge = Bridge::default();
        let mut model = model();
        assert!(api_script(&model).contains("link_dead: false"));
        assert!(drain(&mut bridge, &model).is_empty());

        model.link_dead = true;
        let js = drain(&mut bridge, &model);
        assert!(js.contains("window.wdm.link_dead = true;"), "{js}");

        // And once told, not told again on every pump.
        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn a_dead_link_is_told_to_the_page_before_anything_reacts_to_it() {
        // The ordering `diff` argues for at length and nothing pinned. Moving
        // the block below the notices loop leaves every other test green while
        // `authentication_complete` fires with the page's `link_dead` still
        // false — so the theme offers "press Enter to try again" into a socket
        // nobody reads and clears the notice explaining why.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.link_lost("the compositor went away");

        let js = drain(&mut bridge, &model);
        let dead = js.find("window.wdm.link_dead = true;").expect(&js);
        let notice = js.find("show_message").expect(&js);
        let complete = js.find("authentication_complete").expect(&js);
        assert!(
            dead < notice,
            "the explanation arrived before the fact: {js}"
        );
        assert!(notice < complete, "{js}");
    }

    #[test]
    fn a_reloaded_page_is_told_the_link_is_dead_again() {
        // The document-start API script re-runs on every load with the value
        // frozen at startup, so a reload after the link died silently resets
        // the page's copy to false. The diff emits on *change* and both sides
        // already read true, so nothing corrects it — and the theme goes back
        // to offering a retry. `resync` is what load-finished uses to make the
        // next diff say it whatever it is.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.link_dead = true;
        assert!(drain(&mut bridge, &model).contains("window.wdm.link_dead = true;"));
        assert!(drain(&mut bridge, &model).is_empty());

        bridge.resync();
        let js = drain(&mut bridge, &model);
        assert!(
            js.contains("window.wdm.link_dead = true;"),
            "a reloaded document was left believing the socket is alive: {js}"
        );

        // And it settles again: resync is a one-shot, not a mode.
        assert!(drain(&mut bridge, &model).is_empty());
    }

    #[test]
    fn the_journal_hears_each_distinct_error_once() {
        // The throttle whose two failure modes are silence at the moment of
        // interest and sixty lines a second. A bool for "already reported"
        // bought the first statement's ReferenceError and then swallowed a
        // second, different cause for as long as the trouble lasted.
        let mut bridge = Bridge::default();

        // Once for a given text…
        assert!(bridge.claim_error_report("ReferenceError: x is not defined"));
        assert!(!bridge.claim_error_report("ReferenceError: x is not defined"));
        assert!(!bridge.claim_error_report("ReferenceError: x is not defined"));

        // …but a genuinely different cause is not the same news.
        assert!(bridge.claim_error_report("TypeError: y is not a function"));
        assert!(!bridge.claim_error_report("TypeError: y is not a function"));

        // And the first one is claimable again once it is the current story,
        // which is the flood this suppresses: alternating causes are two lines
        // per pair of ticks, not sixty a second of one.
        assert!(bridge.claim_error_report("ReferenceError: x is not defined"));
    }

    #[test]
    fn recovery_lets_the_journal_speak_again() {
        // A run of trouble that ends and comes back is two runs. Keying on the
        // text alone would print the same error once ever, so a theme that
        // breaks, is reloaded and breaks again the same way would be silent the
        // second time — which is the time someone is looking.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);

        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, false);
        assert!(bridge.claim_error_report("ReferenceError: x is not defined"));
        assert!(!bridge.claim_error_report("ReferenceError: x is not defined"));

        let out = bridge.flush().unwrap();
        bridge.delivered(out.epoch, true);
        assert_eq!(bridge.consecutive_failures(), 0);
        assert!(
            bridge.claim_error_report("ReferenceError: x is not defined"),
            "a second run of the same trouble went unreported"
        );
    }

    #[test]
    fn a_slow_but_successful_evaluation_is_not_wedged() {
        // The deadline must be a deadline, not a speed limit. A theme callback
        // that takes a few hundred milliseconds is slow, not dead, and
        // declaring it dead would quit the greeter out from under the user.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);
        let out = bridge.flush().unwrap();

        for _ in 0..VERDICT_DEADLINE - 1 {
            bridge.tick();
            assert!(bridge.flush().is_none(), "sent a second evaluation");
            assert!(!bridge.is_wedged());
        }

        bridge.delivered(out.epoch, true);
        assert_eq!(bridge.consecutive_failures(), 0);
        assert!(!bridge.is_wedged());
        assert!(bridge.flush().is_none(), "the statement was not drained");
    }

    #[test]
    fn a_stale_verdict_is_not_counted_as_a_failure() {
        // Otherwise a straggler could push a perfectly healthy page towards
        // the wedged threshold and quit the greeter out from under the user.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Info, "hello".to_owned());
        bridge.diff(&model);
        let stale = bridge.flush().unwrap();

        bridge.restart();
        bridge.delivered(stale.epoch, false);
        assert_eq!(bridge.consecutive_failures(), 0);
    }

    #[test]
    fn the_queue_cannot_grow_without_bound() {
        // A page that acknowledges nothing would otherwise queue statements
        // for as long as the machine sits at the login screen.
        let mut bridge = Bridge::default();
        let mut model = model();

        for id in 1..(PENDING_LIMIT as u32 * 2) {
            model.prompt = Some(prompt(id, "Password:", true));
            bridge.diff(&model);
            let out = bridge.flush().expect("nothing was ever acknowledged");
            bridge.delivered(out.epoch, false);
        }

        let out = bridge.flush().unwrap();
        assert_eq!(out.script.lines().count(), PENDING_LIMIT);

        // Long past the point where the queue is nothing but prompts, so every
        // eviction here had to split a pair or take both halves.
        //
        // The defect this guards: it took the assignment and left the callback.
        // Assignments are evicted first because a later one supersedes them,
        // which is true of `window.wdm._prompt` in isolation and false of it
        // beside its `show_prompt` — the default theme's `show_prompt` spends a
        // buffered password with `wdm.respond`, and `respond` with `_prompt`
        // null throws "with no prompt pending" into `call`'s own try/catch. The
        // user is left looking at a disabled field reading "Checking…" that
        // nothing will ever update, which is the state this test used to
        // assert.
        let lines: Vec<&str> = out.script.lines().collect();
        let mut prompts = 0;
        for (index, line) in lines.iter().enumerate() {
            if !line.contains("show_prompt") {
                continue;
            }
            prompts += 1;
            assert!(
                index > 0 && lines[index - 1].starts_with("window.wdm._prompt = {"),
                "a show_prompt survived without the state its answer is matched against"
            );
        }
        assert!(
            prompts > 0,
            "the case was never reached: nothing but prompts was ever queued"
        );
    }

    #[test]
    fn eviction_spends_replaceable_state_before_it_touches_an_event() {
        // The invariant the project states repeatedly: the explanation of why
        // an account is locked must not scroll past. It is the *oldest* thing
        // in the queue and the least replaceable, so evicting by age alone
        // dropped exactly it while keeping a later show_prompt.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        bridge.diff(&model);

        // Well past the limit, but not so far that the assignments run out.
        for id in 1..200u32 {
            model.prompt = Some(prompt(id, "Password:", true));
            bridge.diff(&model);
            let out = bridge.flush().expect("nothing was ever acknowledged");
            bridge.delivered(out.epoch, false);
        }

        let script = bridge.flush().unwrap().script;
        // Not exactly the limit: a prompt is an indivisible pair, so a queue
        // that is one statement over gives up two. Bounded by the limit is the
        // property that matters; overshooting by less than a pair is the price
        // of never splitting one.
        assert!(
            (PENDING_LIMIT - 1..=PENDING_LIMIT).contains(&script.lines().count()),
            "kept {} statements",
            script.lines().count()
        );
        assert!(
            script.contains("The account is locked."),
            "the explanation was evicted while state was still expendable"
        );

        // Built through the same `literal` the diff emits, rather than guessed
        // as a substring: asserting the *absence* of hand-written JSON passes
        // just as well when serde changes its spacing, or when eviction has
        // over-run and thrown away everything. Two assertions, so a queue that
        // kept nothing fails as loudly as one that kept the wrong thing.
        let assignment = |id: u32| {
            format!(
                "window.wdm._prompt = {};",
                literal(&json!({ "id": id, "text": "Password:", "secret": true }))
            )
        };
        assert!(
            script.contains(&assignment(199)),
            "the newest state was evicted, so the page is behind the model"
        );
        assert!(
            !script.contains(&assignment(1)),
            "an obsolete assignment outlived an event"
        );
    }

    #[test]
    fn a_queue_of_nothing_but_events_gives_up_the_newest_first() {
        // Once the assignments are gone the eviction order inverts, and it has
        // to: the reason callbacks are spent last is that the oldest of them is
        // the lockout explanation, and dropping the oldest is the very thing
        // the two-pass split exists to prevent. Walking from index 0 on both
        // passes handed that back the moment there was nothing expendable left.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.push_notice(NoticeKind::Error, "The account is locked.".to_owned());
        for n in 0..PENDING_LIMIT + 8 {
            model.push_notice(NoticeKind::Info, format!("notice {n}"));
        }
        bridge.diff(&model);

        let script = bridge.flush().expect("nothing was queued").script;
        assert_eq!(script.lines().count(), PENDING_LIMIT);
        assert!(
            script.contains("The account is locked."),
            "the first-queued message was evicted: {script}"
        );
        // Quoted, because "notice 25" is a prefix of "notice 255".
        assert!(
            !script.contains(&format!("\"notice {}\"", PENDING_LIMIT + 7)),
            "the newest event survived while an older one was dropped"
        );
    }

    #[test]
    fn a_throwing_callback_cannot_abort_the_script() {
        // The queue is evaluated as one script, so an exception in a theme's
        // show_message would abort every statement after it — including the
        // is_authenticated assignment. Each call is therefore fenced.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.conversation_over = true;
        model.error = Some("Authentication failure".to_owned());

        let js = drain(&mut bridge, &model);
        for line in js.lines().filter(|l| l.contains("window.show_")) {
            assert!(line.contains("try {"), "unfenced callback: {line}");
        }
    }

    #[test]
    fn text_from_pam_cannot_escape_its_literal() {
        // The trust boundary. PAM modules are configured by the administrator,
        // but their text reaches the page verbatim, and a greeter that builds
        // JavaScript by concatenation hands whoever writes that text the run of
        // the login screen.
        let mut bridge = Bridge::default();
        let mut model = model();
        model.prompt = Some(prompt(
            1,
            "</script><script>alert('x')//\"'\n\u{2028}",
            false,
        ));
        // The per-notice path carries PAM's text too, and is the newer of the
        // two: asserting only on the prompt left a refactor of that loop to
        // `format!` green while handing whoever writes a PAM message the run of
        // a page with `window.wdm` in scope.
        model.push_notice(
            NoticeKind::Error,
            "</script><script>alert('n')//\"'\n\u{2028}".to_owned(),
        );

        let js = drain(&mut bridge, &model);
        // Every dangerous character is inside a string literal it cannot close:
        // no bare quote, no bare newline, no `<`, no line separator.
        assert!(!js.contains("</script>"), "{js}");
        assert!(!js.contains('\u{2028}'), "{js}");
        for line in js.lines() {
            let quotes = line.matches('"').count() - line.matches("\\\"").count();
            assert_eq!(quotes % 2, 0, "unbalanced quotes in: {line}");
        }
    }

    #[test]
    fn start_session_refuses_to_post_nothing() {
        // With no sessions installed the fallback to sessions[0] resolves to
        // undefined, and _post would send ["start_session"] — a message wdm's
        // own parse refuses, leaving the theme on "Starting session…" with no
        // exception and no callback. The guard is what makes that visible.
        let script = api_script(&Model::default());
        assert!(
            script.contains("wdm.start_session needs a session id"),
            "{script}"
        );
        // And it is the resolved id that is checked, not the argument: a theme
        // that passes nothing on a machine that *has* sessions is still fine.
        // Asserted as an order rather than as the lines themselves — copying
        // the source verbatim is a snapshot that fails for reformatting and
        // cannot fail for the bug.
        let find = |needle: &str| {
            script
                .find(needle)
                .unwrap_or_else(|| panic!("no {needle} in {script}"))
        };
        let resolved = find("this.sessions[0]");
        let guarded = find("wdm.start_session needs a session id");
        let posted = find("'start_session'");
        assert!(
            resolved < guarded,
            "the guard checks the argument: {script}"
        );
        assert!(guarded < posted, "nothing is posted unguarded: {script}");
    }

    #[test]
    fn a_username_cannot_close_the_api_object() {
        let mut model = model();
        model.users[0].display_name = "\";window.evil=1;//".to_owned();

        let script = api_script(&model);
        // The payload survives as text — it is a display name, and showing it
        // is the point. What must not survive is the quote that would end the
        // literal and let the rest of it run.
        assert!(script.contains("\\\";window.evil=1;//"), "{script}");
        assert!(!script.contains("\"\";window.evil"), "{script}");
    }
}
