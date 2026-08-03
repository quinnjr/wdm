//! Greeter process lifecycle: spawn, privilege drop, and restart backoff.
//!
//! The greeter is untrusted third-party code, so it runs as an unprivileged
//! user with no access to the shadow database and no DRM device. It is also
//! expected to crash occasionally, which must not leave the machine with no way
//! to log in — hence the backoff and the give-up state.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

use uzers::os::unix::UserExt;

/// A greeter that exits sooner than this is treated as having failed to start,
/// rather than as a long-running greeter that happened to crash.
const RAPID_FAILURE: Duration = Duration::from_secs(10);

/// Consecutive rapid failures before wdm stops trying.
///
/// Restarting forever would leave a flickering black screen with no indication
/// of what is wrong. Giving up and showing the reason is more useful, because
/// tty1 is still there to fix it from.
const MAX_RAPID_FAILURES: u32 = 3;

/// Delay before each restart, in seconds, indexed by consecutive failures.
///
/// Only two entries because wdm gives up on the third rapid failure, so a third
/// delay is never reached. A longer ramp would need a larger
/// [`MAX_RAPID_FAILURES`], which is a different trade: more patience with a
/// broken greeter, longer before the user is told what is wrong.
const BACKOFF_SECS: &[u64] = &[1, 2];

/// Runtime directory for the greeter, and the home of the Wayland socket.
///
/// wdm points its own `XDG_RUNTIME_DIR` here before binding the socket, so the
/// greeter inherits a directory it can actually reach. Toolkits refuse to start
/// without one.
pub const RUNTIME_DIR: &str = "/run/wdm";

#[derive(Debug, thiserror::Error)]
pub enum GreeterError {
    #[error("greeter user {0} does not exist")]
    NoSuchUser(String),
    #[error("greeter.command is empty")]
    EmptyCommand,
    #[error("preparing {0}: {1}")]
    RuntimeDir(PathBuf, #[source] std::io::Error),
    // No "spawning greeter:" prefix here: callers add their own context, and
    // the give-up screen shows this text verbatim.
    #[error("{0}")]
    Spawn(#[source] std::io::Error),
    #[error("a greeter is already running")]
    AlreadyRunning,
    #[error("wdm has stopped trying to start a greeter")]
    GaveUp,
}

/// What to do after the greeter exited.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Restart after this delay.
    Restart(Duration),
    /// Too many rapid failures; show the reason and stop.
    GaveUp { reason: String },
}

/// The greeter process and its restart policy.
pub struct Greeter {
    argv: Vec<String>,
    /// Credentials to drop to, or `None` when wdm is not privileged. Running
    /// unprivileged is the development case (`--backend winit`), where there is
    /// nothing to drop and no separate greeter account.
    credentials: Option<Credentials>,
    socket: String,
    child: Option<Child>,
    /// The greeter's process group, which equals its pid because spawn() makes
    /// it a session leader. Held separately from the `Child` because the group
    /// outlives the leader: reaping the leader drops the handle, and without
    /// this the id of the group still holding forked helpers would be lost.
    pgid: Option<libc::pid_t>,
    started: Option<Instant>,
    rapid_failures: u32,
    gave_up: bool,
}

struct Credentials {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
    home: PathBuf,
}

/// Create the runtime directory the greeter and the socket live in, still
/// owned by root and `0700`.
///
/// This is the first half of securing the socket: while the directory is
/// root-only, no unprivileged process can enter it, so the socket can be bound
/// and have its permissions and ownership fixed by path without racing a
/// hostile process swapping the socket for a symlink. Only after the socket is
/// secured does [`hand_over_runtime_dir`] give the directory to the greeter.
///
/// The mode is set at creation time via `DirBuilder`, never widened afterwards:
/// a `create_dir_all` followed by `set_permissions` would leave the directory
/// at umask width between the two calls. If the directory already exists —
/// left over from a previous run, possibly still owned by the greeter — it is
/// pulled back to root `0700` before anything is created inside it.
///
/// Free functions rather than methods because they have to run *before* the
/// socket is created, which is before the socket has a name to build a
/// [`Greeter`] with.
pub fn create_runtime_dir(privileged: bool) -> Result<(), GreeterError> {
    // Unprivileged: the existing XDG_RUNTIME_DIR is already the user's.
    if !privileged {
        return Ok(());
    }
    let dir = Path::new(RUNTIME_DIR);
    create_runtime_dir_at(dir).map_err(|e| GreeterError::RuntimeDir(dir.to_owned(), e))
}

fn create_runtime_dir_at(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;

    // recursive(true) succeeds silently on an existing directory without
    // touching its mode or owner, so reassert both: a directory a previous run
    // handed to the greeter must be root-only again before the socket is bound
    // in it.
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    // SAFETY: geteuid cannot fail and touches no memory.
    let root = unsafe { libc::geteuid() };
    std::os::unix::fs::chown(dir, Some(root), None)?;

    Ok(())
}

/// Hand the runtime directory to the greeter account.
///
/// The second half of [`create_runtime_dir`]: called only after the socket has
/// been bound and secured, so there is no window in which the greeter uid can
/// write to the directory while root is still manipulating paths inside it.
/// After this, `0700` plus greeter ownership means no *other* unprivileged
/// process can reach the Wayland socket — the outer half of the trust boundary,
/// with the socket's own permissions as the inner half.
pub fn hand_over_runtime_dir(
    owner: Option<(libc::uid_t, libc::gid_t)>,
) -> Result<(), GreeterError> {
    let Some((uid, gid)) = owner else {
        return Ok(());
    };
    let dir = Path::new(RUNTIME_DIR);
    std::os::unix::fs::chown(dir, Some(uid), Some(gid))
        .map_err(|e| GreeterError::RuntimeDir(dir.to_owned(), e))
}

impl Greeter {
    /// Resolve the greeter's account and command.
    ///
    /// `socket` is the Wayland socket name the greeter should connect to.
    pub fn new(
        command: &str,
        user: &str,
        socket: &str,
        privileged: bool,
    ) -> Result<Self, GreeterError> {
        // Split on whitespace rather than running a shell: a greeter command is
        // a path and flags, and going through sh would let a config file inject
        // arbitrary shell into a root process.
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if argv.is_empty() {
            return Err(GreeterError::EmptyCommand);
        }

        let credentials = if privileged {
            let account = uzers::get_user_by_name(user)
                .ok_or_else(|| GreeterError::NoSuchUser(user.to_owned()))?;
            let gid = account.primary_group_id();
            Some(Credentials {
                uid: account.uid(),
                gid,
                // Deliberately just the primary group, not getgrouplist: the
                // greeter is untrusted code and the spec promises it has no
                // supplementary groups. Deriving them from the account would
                // make that true only by packaging convention — add the greeter
                // user to `video` and it silently gains DRM access.
                groups: vec![gid],
                home: account.home_dir().to_owned(),
            })
        } else {
            None
        };

        Ok(Self {
            argv,
            credentials,
            socket: socket.to_owned(),
            child: None,
            pgid: None,
            started: None,
            rapid_failures: 0,
            gave_up: false,
        })
    }

    /// Whether wdm has stopped trying to run a greeter.
    pub fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// Launch the greeter.
    pub fn spawn(&mut self) -> Result<(), GreeterError> {
        if self.gave_up {
            return Err(GreeterError::GaveUp);
        }
        if self.child.is_some() {
            // Overwriting the handle would orphan the running greeter: nothing
            // would ever kill or reap it, and it would survive into the user's
            // session drawing over their screen.
            return Err(GreeterError::AlreadyRunning);
        }

        let mut command = Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .env_clear()
            // The exported constant, not a literal: the protocol crate promises
            // greeter authors this variable is set, and using the constant here
            // is what keeps that promise from drifting.
            .env(wdm_protocol::GREETER_SOCKET_ENV, &self.socket)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            // A greeter drawing a login form has no business talking to the
            // network or a session bus, but it does need a runtime dir and a
            // locale to render text correctly.
            .env(
                "LANG",
                std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_owned()),
            );

        // The greeter always becomes a session leader, privileged or not, so
        // its pid doubles as its process group id and kill() can signal the
        // whole group — greeters fork helpers (WebKit web processes, wrapper
        // scripts) that must not outlive the greeter into the user's session.
        // SAFETY: runs in the forked child; a single libc call, no allocation.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        if let Some(credentials) = &self.credentials {
            command
                .env("XDG_RUNTIME_DIR", RUNTIME_DIR)
                .env("HOME", &credentials.home)
                .current_dir(&credentials.home);

            let uid = credentials.uid;
            let gid = credentials.gid;
            let groups = credentials.groups.clone();

            // SAFETY: runs in the forked child; only libc calls on data captured
            // above, no allocation. std applies its own uid/gid after pre_exec,
            // so wdm does the whole drop here and never uses Command::uid/gid.
            // Order is mandatory: groups, gid, uid.
            unsafe {
                command.pre_exec(move || {
                    if libc::setgroups(groups.len(), groups.as_ptr()) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::setgid(gid) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::setuid(uid) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // The greeter running as root would defeat the entire
                    // privilege split, so refuse rather than exec.
                    if libc::getuid() != uid || libc::geteuid() != uid {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    }
                    Ok(())
                });
            }
        } else {
            // Development: inherit the developer's own runtime dir and home.
            if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
                command.env("XDG_RUNTIME_DIR", dir);
            }
            if let Ok(home) = std::env::var("HOME") {
                command.env("HOME", home);
            }
        }

        let child = command.spawn().map_err(GreeterError::Spawn)?;
        log::info!("greeter {} started as pid {}", self.argv[0], child.id());

        // setsid() in pre_exec made the child a session leader, so its pid is
        // its process group id.
        self.pgid = Some(child.id() as libc::pid_t);
        self.child = Some(child);
        self.started = Some(Instant::now());

        Ok(())
    }

    /// Reap the greeter if it has exited, returning what to do next.
    ///
    /// Returns `None` while the greeter is still running.
    pub fn poll(&mut self) -> Option<Disposition> {
        let child = self.child.as_mut()?;

        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                // A crashed greeter leaves its helpers behind just as surely as
                // one that is killed for the handoff, so the group is swept on
                // every exit path, not only in kill().
                self.sweep_group();
                Some(self.note_exit(status))
            }
            Ok(None) => None,
            Err(e) => {
                // The child is unwaitable, which means the handle is useless.
                // Treat it as an exit so wdm does not wait on it forever.
                log::error!("waiting on greeter: {e}");
                self.child = None;
                self.sweep_group();
                Some(self.note_exit(ExitStatus::from_raw(0)))
            }
        }
    }

    /// SIGKILL whatever is left of the greeter's process group after the leader
    /// has already been reaped.
    ///
    /// Sent immediately after the reap and never later: once the leader is
    /// reaped, only the surviving members keep the pgid from being recycled, so
    /// delaying this would eventually risk signalling an unrelated group.
    /// There is no SIGTERM grace period here — the greeter itself is already
    /// gone, so nothing is left that has state worth unwinding.
    fn sweep_group(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        // A setsid() regression would leave the greeter in wdm's own group, and
        // this line would then kill wdm.
        debug_assert_ne!(pgid, unsafe { libc::getpgrp() });
        // SAFETY: kill(2) with a negative pid signals a process group; the only
        // requirement is a valid pgid, and errors (ESRCH for an empty group)
        // are the expected case.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }

    /// Record an exit and decide whether to restart.
    fn note_exit(&mut self, status: ExitStatus) -> Disposition {
        let ran_for = self.started.take().map(|t| t.elapsed());
        let rapid = ran_for.is_none_or(|d| d < RAPID_FAILURE);

        log::warn!(
            "greeter exited ({}) after {:?}",
            describe_exit(status),
            ran_for.unwrap_or_default()
        );

        self.record_failure(rapid, describe_exit(status))
    }

    /// Whether a greeter process is currently running.
    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Record a greeter that could not be started at all.
    ///
    /// A misconfigured `greeter.command` must land in the same backoff and
    /// give-up policy as one that starts and crashes. Treating it as fatal
    /// instead would exit wdm on the first try, so the user gets no login
    /// prompt *and* no explanation — the give-up screen exists for exactly this.
    pub fn note_spawn_failure(&mut self, error: &str) -> Disposition {
        self.started = None;
        self.record_failure(true, format!("could not be started: {error}"))
    }

    fn record_failure(&mut self, rapid: bool, reason: String) -> Disposition {
        if rapid {
            self.rapid_failures += 1;
        } else {
            // A greeter that ran for a while and then died is a crash, not a
            // broken configuration. Resetting means a long-lived greeter that
            // crashes once a day never exhausts the budget.
            self.rapid_failures = 1;
        }

        log::warn!("greeter failure {}/{MAX_RAPID_FAILURES}", self.rapid_failures);

        if self.rapid_failures >= MAX_RAPID_FAILURES {
            self.gave_up = true;
            return Disposition::GaveUp {
                reason: format!("greeter {reason} — switch to tty1"),
            };
        }

        let index = (self.rapid_failures as usize).saturating_sub(1);
        Disposition::Restart(Duration::from_secs(
            BACKOFF_SECS[index.min(BACKOFF_SECS.len() - 1)],
        ))
    }

    /// The delays this policy can actually produce, longest first.
    #[cfg(test)]
    fn possible_delays() -> Vec<Duration> {
        BACKOFF_SECS.iter().map(|s| Duration::from_secs(*s)).collect()
    }

    /// Stop the greeter, used before handing the display to a session.
    ///
    /// SIGTERM first so a toolkit can release its buffers, then SIGKILL if it
    /// does not go. Blocking here is acceptable: the display is about to change
    /// hands and nothing should be drawing during the transition.
    ///
    /// Signals go to the negative pid, which kill(2) interprets as the whole
    /// process group. spawn() made the greeter a session leader, so its pid is
    /// its pgid, and the group includes everything it forked — WebKit web
    /// processes, wrapper-script children — none of which may survive into the
    /// user's session still running as the greeter uid.
    ///
    /// The condition being waited for is therefore "the group is empty", not
    /// "the leader is reaped": a helper that ignores SIGTERM outlives its
    /// parent, and returning as soon as the leader goes would hand the display
    /// over with that helper still drawing as the greeter uid.
    pub fn kill(&mut self) {
        let Some(pgid) = self.pgid.take() else {
            return;
        };
        let mut child = self.child.take();

        // A setsid() regression would leave the greeter in wdm's own group, and
        // this line would then kill wdm.
        debug_assert_ne!(pgid, unsafe { libc::getpgrp() });
        // SAFETY: kill(2) on the process group of a child this process owns
        // and has not yet reaped, so the pgid cannot have been recycled.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }

        let deadline = Instant::now() + Duration::from_secs(2);

        // Reap the leader first, so it does not sit in the group as a zombie
        // that the emptiness probe below would count forever.
        if let Some(child) = child.as_mut() {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::error!("waiting on greeter during shutdown: {e}");
                        break;
                    }
                }
            }
        }

        // Reaping the leader does not free the pgid while any member is still
        // alive, so probing it here cannot reach an unrelated group.
        while Instant::now() < deadline {
            if group_is_empty(pgid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        log::warn!("greeter process group ignored SIGTERM, killing it");
        // SAFETY: as above — a pgid whose members are still alive.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        // The leader may still be unreaped, if it was the process ignoring
        // SIGTERM. wait() after SIGKILL cannot block for long, and leaves no
        // zombie behind. child.kill() would only signal the leader again.
        if let Some(mut child) = child {
            let _ = child.wait();
        }

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if group_is_empty(pgid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        log::error!("process group {pgid} survived SIGKILL");
    }
}

/// Whether any process is left in the group.
///
/// Signal 0 delivers nothing; kill(2) only performs the permission and
/// existence checks, and reports ESRCH for a group with no members. A zombie
/// still counts as a member, which is why the leader is reaped before this is
/// consulted.
fn group_is_empty(pgid: libc::pid_t) -> bool {
    // SAFETY: signal 0 sends nothing and only probes for existence.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

impl Drop for Greeter {
    fn drop(&mut self) {
        // Leaving a greeter running after wdm exits would leave a process
        // holding a dead socket and, worse, drawing over whatever comes next.
        self.kill();
    }
}

/// Human-readable exit description, naming the signal where there was one.
fn describe_exit(status: ExitStatus) -> String {
    if let Some(signal) = status.signal() {
        return format!("killed by signal {signal}");
    }
    match status.code() {
        Some(0) => "exited cleanly".to_owned(),
        Some(code) => format!("exited with status {code}"),
        None => "exited for an unknown reason".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn greeter(command: &str) -> Greeter {
        Greeter::new(command, "nobody", "wayland-test", false).unwrap()
    }

    #[test]
    fn rejects_empty_command() {
        assert!(matches!(
            Greeter::new("   ", "nobody", "wayland-test", false),
            Err(GreeterError::EmptyCommand)
        ));
    }

    #[test]
    fn splits_command_without_a_shell() {
        let g = greeter("/usr/bin/env FOO=bar");
        assert_eq!(g.argv, ["/usr/bin/env", "FOO=bar"]);
    }

    #[test]
    fn every_backoff_entry_is_reachable() {
        // A table with entries the policy can never reach is a table that lies
        // about the behaviour.
        let mut g = greeter("/bin/true");
        let mut seen = Vec::new();
        while let Disposition::Restart(delay) = g.note_exit(ExitStatus::from_raw(0)) {
            seen.push(delay);
        }
        assert_eq!(seen, Greeter::possible_delays());
    }

    #[test]
    fn backoff_grows_then_gives_up() {
        let mut g = greeter("/bin/true");
        // Never started, so every exit counts as rapid.
        assert_eq!(
            g.note_exit(ExitStatus::from_raw(0)),
            Disposition::Restart(Duration::from_secs(1))
        );
        assert_eq!(
            g.note_exit(ExitStatus::from_raw(0)),
            Disposition::Restart(Duration::from_secs(2))
        );
        assert!(matches!(
            g.note_exit(ExitStatus::from_raw(0)),
            Disposition::GaveUp { .. }
        ));
        assert!(g.gave_up());
    }

    #[test]
    fn long_running_greeter_does_not_exhaust_the_budget() {
        let mut g = greeter("/bin/true");
        g.note_exit(ExitStatus::from_raw(0));
        g.note_exit(ExitStatus::from_raw(0));
        assert_eq!(g.rapid_failures, 2);

        // A greeter that ran past the rapid threshold resets the count, so one
        // crash a day never adds up to a give-up.
        g.started = Some(Instant::now() - RAPID_FAILURE - Duration::from_secs(1));
        assert_eq!(
            g.note_exit(ExitStatus::from_raw(0)),
            Disposition::Restart(Duration::from_secs(1))
        );
        assert_eq!(g.rapid_failures, 1);
        assert!(!g.gave_up());
    }

    #[test]
    fn give_up_reason_points_at_the_recovery_path() {
        let mut g = greeter("/bin/true");
        g.note_exit(ExitStatus::from_raw(0));
        g.note_exit(ExitStatus::from_raw(0));
        let Disposition::GaveUp { reason } = g.note_exit(ExitStatus::from_raw(11)) else {
            panic!("expected give-up");
        };
        // The user is looking at a broken screen; the message must say what to do.
        assert!(reason.contains("tty1"), "{reason}");
        assert!(reason.contains("signal 11"), "{reason}");
    }

    #[test]
    fn a_greeter_that_cannot_start_still_gives_up() {
        // A bad greeter.command must not be fatal on the first try: it goes
        // through the same policy, so the user ends up looking at the error
        // screen rather than at nothing.
        let mut g = Greeter::new("/nonexistent/greeter", "nobody", "wayland-test", false).unwrap();

        assert!(g.spawn().is_err());
        assert_eq!(
            g.note_spawn_failure("no such file"),
            Disposition::Restart(Duration::from_secs(1))
        );
        assert_eq!(
            g.note_spawn_failure("no such file"),
            Disposition::Restart(Duration::from_secs(2))
        );

        let Disposition::GaveUp { reason } = g.note_spawn_failure("no such file") else {
            panic!("expected give-up");
        };
        assert!(reason.contains("could not be started"), "{reason}");
        assert!(reason.contains("tty1"), "{reason}");
    }

    #[test]
    fn describes_exits() {
        assert_eq!(describe_exit(ExitStatus::from_raw(0)), "exited cleanly");
        // from_raw takes a wait status: 0x100 is exit code 1.
        assert_eq!(
            describe_exit(ExitStatus::from_raw(0x100)),
            "exited with status 1"
        );
        assert_eq!(
            describe_exit(ExitStatus::from_raw(9)),
            "killed by signal 9"
        );
    }

    #[test]
    fn spawns_and_reaps_a_real_process() {
        let mut g = greeter("/bin/true");
        g.spawn().unwrap();
        assert!(g.is_running());

        // try_wait is racy by nature; poll until the child is reaped.
        let deadline = Instant::now() + Duration::from_secs(5);
        let disposition = loop {
            if let Some(d) = g.poll() {
                break d;
            }
            assert!(Instant::now() < deadline, "child never exited");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert!(matches!(disposition, Disposition::Restart(_)));
        assert!(!g.is_running());
    }

    #[test]
    fn kill_terminates_a_running_process() {
        let mut g = greeter("/bin/sleep 60");
        g.spawn().unwrap();
        assert!(g.is_running());
        g.kill();
        assert!(!g.is_running());
    }

    /// How many *live* processes belong to the given process group.
    ///
    /// Counted from /proc rather than kill(-pgid, 0), for two reasons: the test
    /// needs to wait for the group to *grow* before killing it, which a
    /// zero-signal probe cannot express, and a zombie is still a group member
    /// as far as kill(2) is concerned. Under a PID 1 that does not reap — any
    /// plain container — trusting the probe would fail a passing
    /// implementation, so state `Z` is excluded here.
    fn live_group_members(pgid: libc::pid_t) -> usize {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())))
            .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
            .filter(|stat| {
                // The comm field is parenthesised and can itself contain both
                // spaces and parentheses, so split after its *last* closing
                // one. What follows is state, ppid, then pgrp.
                let Some((_, rest)) = stat.rsplit_once(')') else {
                    return false;
                };
                let mut fields = rest.split_whitespace();
                let state = fields.next();
                let _ppid = fields.next();
                let pgrp = fields.next().and_then(|f| f.parse::<libc::pid_t>().ok());
                pgrp == Some(pgid) && state != Some("Z")
            })
            .count()
    }

    #[test]
    fn kill_takes_the_whole_process_group_with_it() {
        // A greeter that forks helpers (WebKit web processes, wrapper-script
        // children) must not leave them running as the greeter uid after the
        // handoff. spawn() puts the greeter in its own session, so its pid is
        // the group, and kill() signals the group — this pins both halves.
        //
        // The script ignores SIGTERM, which is the case that matters: an
        // ignored disposition survives exec, so neither sleep goes on the first
        // signal and kill() has to escalate the whole group to SIGKILL.
        //
        // Built directly rather than through greeter(): new() splits the
        // command on whitespace, which cannot express a quoted `sh -c` script.
        let g = Greeter {
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                r#"trap "" TERM; sleep 60 & exec sleep 60"#.to_owned(),
            ],
            credentials: None,
            socket: "wayland-test".to_owned(),
            child: None,
            pgid: None,
            started: None,
            rapid_failures: 0,
            gave_up: false,
        };

        // A failing assertion below would otherwise leave two SIGTERM-proof
        // sleeps running for a minute, poisoning every rerun in that window.
        struct Reaper(Greeter);
        impl Drop for Reaper {
            fn drop(&mut self) {
                self.0.kill();
            }
        }
        let mut reaper = Reaper(g);

        reaper.0.spawn().unwrap();
        let pgid = reaper.0.pgid.unwrap();

        // Wait for the shell to have forked the background sleep, so the test
        // proves the *group* dies and not just the leader.
        let deadline = Instant::now() + Duration::from_secs(5);
        while live_group_members(pgid) < 2 {
            assert!(Instant::now() < deadline, "background child never appeared");
            std::thread::sleep(Duration::from_millis(10));
        }

        reaper.0.kill();

        // Signal delivery is asynchronous; poll until the group is empty.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = live_group_members(pgid);
            if left == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process group {pgid} survived kill(): {left} member(s) left",
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn an_existing_loose_runtime_dir_is_tightened_before_reuse() {
        // The other half of the property — that the directory is never
        // observable wider than 0700, not even for an instant between creation
        // and a chmod — is enforced by DirBuilder::mode and by review. It is
        // deliberately not tested: the transient window is not observable after
        // the fact, and the only way to provoke it would be a process-global
        // libc::umask, which every other test in this binary would then race.
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("wdm");

        create_runtime_dir_at(&dir).unwrap();
        let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&dir).unwrap());
        assert_eq!(mode & 0o777, 0o700);

        // A pre-existing directory with loose permissions — a leftover from a
        // previous run, possibly still owned by the greeter — is tightened and
        // taken back, not accepted as-is.
        std::fs::set_permissions(
            &dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        create_runtime_dir_at(&dir).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&meta) & 0o777, 0o700);
        // Ownership is reasserted to the effective uid, which is root in
        // production and the test user here.
        assert_eq!(
            std::os::unix::fs::MetadataExt::uid(&meta),
            unsafe { libc::geteuid() }
        );
    }

    #[test]
    fn unprivileged_greeter_has_no_credentials_to_drop() {
        // The development path must not try to setuid, which would fail as a
        // normal user and make `--backend winit` unusable.
        assert!(greeter("/bin/true").credentials.is_none());
    }
}
