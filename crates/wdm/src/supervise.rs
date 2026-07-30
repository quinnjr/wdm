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
            started: None,
            rapid_failures: 0,
            gave_up: false,
        })
    }

    /// Create the runtime directory the greeter and socket live in.
    ///
    /// Owned by the greeter user and `0700`, so no other unprivileged process
    /// can reach the Wayland socket. The socket's own permissions are the
    /// trust boundary for authentication, and this is the outer half of it.
    pub fn prepare_runtime_dir(&self) -> Result<(), GreeterError> {
        let Some(credentials) = &self.credentials else {
            // Unprivileged: the existing XDG_RUNTIME_DIR is already the user's.
            return Ok(());
        };

        let dir = Path::new(RUNTIME_DIR);
        let wrap = |e: std::io::Error| GreeterError::RuntimeDir(dir.to_owned(), e);

        std::fs::create_dir_all(dir).map_err(wrap)?;

        let permissions = std::os::unix::fs::PermissionsExt::from_mode(0o700);
        std::fs::set_permissions(dir, permissions).map_err(wrap)?;

        std::os::unix::fs::chown(dir, Some(credentials.uid), Some(credentials.gid))
            .map_err(wrap)?;

        Ok(())
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
            .env("WAYLAND_DISPLAY", &self.socket)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            // A greeter drawing a login form has no business talking to the
            // network or a session bus, but it does need a runtime dir and a
            // locale to render text correctly.
            .env(
                "LANG",
                std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".to_owned()),
            );

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
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
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
                Some(self.note_exit(status))
            }
            Ok(None) => None,
            Err(e) => {
                // The child is unwaitable, which means the handle is useless.
                // Treat it as an exit so wdm does not wait on it forever.
                log::error!("waiting on greeter: {e}");
                self.child = None;
                Some(self.note_exit(ExitStatus::from_raw(0)))
            }
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
    pub fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        // SAFETY: kill(2) on a pid this process owns and has not yet reaped.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => break,
                Err(e) => {
                    log::error!("waiting on greeter during shutdown: {e}");
                    return;
                }
            }
        }

        log::warn!("greeter ignored SIGTERM, killing it");
        let _ = child.kill();
        let _ = child.wait();
    }
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

    #[test]
    fn unprivileged_greeter_has_no_credentials_to_drop() {
        // The development path must not try to setuid, which would fail as a
        // normal user and make `--backend winit` unusable.
        assert!(greeter("/bin/true").credentials.is_none());
    }
}
