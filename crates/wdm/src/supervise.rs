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

/// Cache directory for the greeter, pointed at by `XDG_CACHE_HOME`.
///
/// The greeter's home is `/var/empty`, which is root-owned and must stay empty,
/// so `$HOME/.cache` — where every toolkit looks by default — cannot be created.
/// GTK's Vulkan renderer reacts to that by logging "Failed to create pipeline
/// cache directory" and recompiling every shader on every start, which cost
/// this greeter more than ten seconds before it drew anything. A login screen
/// that is blank for ten seconds reads as a machine that has failed to boot,
/// and the user reaches for a VT switch or the power button.
///
/// Persistent rather than under [`RUNTIME_DIR`] precisely because a tmpfs one
/// would be empty on every boot, which is the case that matters: the cost is
/// paid at the login screen, once per boot, in front of someone waiting.
///
/// Owned by the greeter account, unlike `/var/lib/wdm`. The distinction is not
/// an inconsistency: root creates and renames files in `/var/lib/wdm`, so
/// handing it to an unprivileged account would be a symlink-attack surface,
/// whereas nothing but the greeter ever touches this one.
pub const CACHE_DIR: &str = "/var/cache/wdm";

#[derive(Debug, thiserror::Error)]
pub enum GreeterError {
    #[error("greeter user {0} does not exist")]
    NoSuchUser(String),
    #[error("greeter.command is empty")]
    EmptyCommand,
    #[error("preparing {0}: {1}")]
    RuntimeDir(PathBuf, #[source] std::io::Error),
    #[error("preparing {0}: {1}")]
    CacheDir(PathBuf, #[source] std::io::Error),
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
    /// Restart after `delay`.
    ///
    /// `reason` describes what went wrong, in the same words the give-up screen
    /// would use. It is carried here and not only on [`Disposition::GaveUp`]
    /// because a user watching the login screen vanish and reappear twice before
    /// the third failure explains itself has been told nothing about the first
    /// two; the new greeter advertises it as `last_error`.
    Restart { delay: Duration, reason: String },
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

/// Create the greeter's cache directory and hand it to the greeter account.
///
/// See [`CACHE_DIR`] for why it exists at all. Unlike the runtime directory it
/// is created and handed over in one step, because nothing is ever placed
/// inside it by root — there is no window to keep the greeter out of.
pub fn create_cache_dir(
    owner: Option<(libc::uid_t, libc::gid_t)>,
) -> Result<(), GreeterError> {
    // Unprivileged: the greeter runs as the developer, whose own XDG_CACHE_HOME
    // is already writable, and /var/cache is not.
    let Some((uid, gid)) = owner else {
        return Ok(());
    };
    let dir = Path::new(CACHE_DIR);
    create_cache_dir_at(dir, uid, gid).map_err(|e| GreeterError::CacheDir(dir.to_owned(), e))
}

fn create_cache_dir_at(dir: &Path, uid: libc::uid_t, gid: libc::gid_t) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;

    // Only the leaf gets 0700. `DirBuilder::mode` applies to *every* component
    // it creates, so a recursive create of the whole path on a system without
    // /var/cache would leave /var/cache itself root-owned and root-only, and
    // every other package that expects to write there would break. The parent is
    // created at the default mode, and normally exists already.
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(dir)?;

    // Everything after this point goes through a file descriptor rather than the
    // path, and the descriptor is opened `O_NOFOLLOW`.
    //
    // This directory outlives a boot and is owned by an unprivileged account for
    // all of it, which the runtime directory on tmpfs is not. If the greeter
    // account ever gains write access to the parent — a packaging mistake, or a
    // deployment that puts the cache somewhere group-writable; /var/cache itself
    // is root-owned 0755, so this is a precondition and not a given — then a
    // compromised greeter could replace the directory with a symlink and wait
    // for the next boot, when root would chmod and chown whatever it pointed at.
    // `O_NOFOLLOW` turns that into ELOOP here instead, and `recursive(true)` is
    // what makes it reachable — it returns Ok for an existing path rather than
    // EEXIST, so a symlink survives the create and has to be caught on the open.
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    let fd = handle.as_raw_fd();

    // Reassert both: `recursive(true)` leaves an existing directory's mode and
    // owner alone, and the greeter account may have been reconfigured since the
    // directory was made.
    //
    // SAFETY: fd is open and owned by `handle` for the whole call.
    if unsafe { libc::fchmod(fd, 0o700) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fchown(fd, uid, gid) } < 0 {
        return Err(std::io::Error::last_os_error());
    }

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
        // SAFETY: runs in the forked child; only libc calls on stack data, no
        // allocation.
        unsafe {
            command.pre_exec(|| {
                // First, before anything else, and the reason is long enough to
                // live with the function: see unblock_signals.
                unblock_signals()?;

                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        if let Some(credentials) = &self.credentials {
            command
                .env("XDG_RUNTIME_DIR", RUNTIME_DIR)
                .env("XDG_CACHE_HOME", CACHE_DIR)
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
                    // privilege split, so refuse rather than exec. Both halves
                    // are checked: setuid and setgid can fail in ways that do
                    // not set errno on every libc, and a greeter left in root's
                    // group reaches every file root's group can, which for the
                    // process this codebase designates untrusted is not a
                    // meaningfully smaller failure than keeping the uid.
                    if libc::getuid() != uid || libc::geteuid() != uid {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    }
                    if libc::getgid() != gid || libc::getegid() != gid {
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
        let waited = self.child.as_mut()?.try_wait();
        self.dispose(waited)
    }

    /// Turn the outcome of a wait into a disposition.
    ///
    /// Split out from [`Self::poll`] so every arm can be driven from a test.
    /// A real child can only ever produce the two `Ok` arms, so with the match
    /// inlined in `poll` the `Err` wiring was unpinned: reverting it to a
    /// fabricated successful status left the whole suite green, because the
    /// tests reached [`Self::note_unwaitable`] directly and never through the
    /// branch that decides to call it. The bug lived in the wiring, so the
    /// wiring is what the seam exposes.
    fn dispose(&mut self, waited: std::io::Result<Option<ExitStatus>>) -> Option<Disposition> {
        // waitpid can be interrupted by a signal, and wdm takes signals — the
        // VT-switch chord and calloop's signalfd both arrive asynchronously.
        // EINTR says nothing about the greeter, so retrying once keeps a
        // transient interruption from being charged to the restart budget as a
        // rapid failure. One retry, not a loop: a handle that is genuinely
        // unwaitable must still reach the Err arm below rather than spin.
        let waited = match waited {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => match self.child.as_mut() {
                Some(child) => child.try_wait(),
                None => Err(e),
            },
            other => other,
        };

        match waited {
            Ok(Some(status)) => {
                self.child = None;
                // A crashed greeter leaves its helpers behind just as surely as
                // one that is killed for the handoff, so the group is swept on
                // every exit path, not only in kill().
                self.sweep_group();
                Some(self.note_exit(status))
            }
            Ok(None) => None,
            Err(e) => Some(self.note_unwaitable(&e)),
        }
    }

    /// Record a greeter whose handle can no longer be waited on.
    ///
    /// Treated as an exit so wdm does not wait on it forever, but deliberately
    /// not routed through [`Self::note_exit`] with a fabricated status: there is
    /// no status, and inventing a successful one would put "greeter exited
    /// cleanly" on the give-up screen, contradicting the error logged beside it.
    /// Like [`Self::note_spawn_failure`], this counts as a rapid failure
    /// whatever the greeter's uptime was — an unwaitable handle says nothing
    /// about how long the process lived, so the elapsed time is discarded
    /// rather than trusted.
    ///
    /// A named step rather than an inlined arm, because the Err case cannot be
    /// provoked from a test with a real child; [`Self::dispose`] is what lets a
    /// test reach it through the branch that decides to call it.
    fn note_unwaitable(&mut self, e: &std::io::Error) -> Disposition {
        log::error!("waiting on greeter: {e}");
        let child = self.child.take();
        self.sweep_group();
        // sweep_group has already SIGKILLed the group, so this cannot block for
        // long, and it is what keeps the leader from sitting as a zombie for the
        // rest of wdm's life — dropping the handle never reaps. Same reasoning
        // as the wait() after the escalation in kill().
        if let Some(mut child) = child {
            let _ = child.wait();
        }
        self.started = None;
        self.record_failure(true, format!("could not be waited for: {e}"))
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
        // A greeter that reports it gave up is counted against the budget
        // whatever its uptime, because the thing it reports takes longer to
        // detect than [`RAPID_FAILURE`] allows: the webkit greeter waits about
        // half a minute before deciding a silent web process is never going to
        // answer, and a greeter respawning into the same wedge every thirty
        // seconds would look like a healthy one restarting rather than a machine
        // nobody can log into. Without this bypass the give-up screen is
        // unreachable on that path. The status comes from the protocol crate, so
        // wdm and the greeters cannot drift apart on its value.
        let rapid = status.code() == Some(i32::from(wdm_protocol::GREETER_GAVE_UP_EXIT))
            || ran_for.is_none_or(|d| d < RAPID_FAILURE);

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
        Disposition::Restart {
            delay: Duration::from_secs(BACKOFF_SECS[index.min(BACKOFF_SECS.len() - 1)]),
            reason: format!("greeter {reason}"),
        }
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

/// Clear the calling thread's signal mask, for use as the first statement of a
/// `pre_exec` closure.
///
/// calloop's signal source blocks SIGTERM and SIGINT on wdm's thread (see
/// `backend/setup.rs`) so its signalfd is the only reader, and a blocked mask is
/// inherited across fork *and survives exec*. Every process wdm starts must
/// therefore have it cleared, and the consequences differ per path but are bad
/// on both: a greeter started with them blocked cannot receive the SIGTERM
/// [`Greeter::kill`] sends before the handoff, so every login burns the full
/// grace period and ends in SIGKILL while wdm is dropping DRM master; a user
/// session started with them blocked passes the mask to every process it ever
/// starts, so `loginctl terminate-session`, `systemctl stop` and systemd's
/// shutdown TERM phase all degrade to SIGKILL and Ctrl-C is dead in every
/// terminal.
///
/// Shared by [`Greeter::spawn`] and [`crate::session::Launch::spawn`] rather
/// than written out twice: it is one rule about how wdm hands processes to the
/// rest of the system, and a rule stated in two places is a rule that gets fixed
/// in one of them.
///
/// # Safety
///
/// Callable anywhere, but written for the forked child of a process with live
/// PAM threads: it allocates nothing and takes no locks, which is why it is a
/// plain non-generic function and reports `pthread_sigmask`'s error number with
/// `from_raw_os_error` rather than boxing a message. Do not add anything here
/// that allocates.
pub(crate) unsafe fn unblock_signals() -> std::io::Result<()> {
    // MaybeUninit rather than mem::zeroed: sigset_t is an opaque foreign type,
    // and while every libc wdm targets defines it as a plain integer array, a
    // zeroed bit pattern is not something this code is entitled to assume.
    // sigemptyset is the initialiser the platform provides.
    let mut empty = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: a valid, suitably aligned pointer to uninitialised sigset_t
    // storage, which is exactly what sigemptyset takes.
    if unsafe { libc::sigemptyset(empty.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: sigemptyset returned success, so the set is initialised.
    let empty = unsafe { empty.assume_init() };

    // pthread_sigmask returns the error number rather than setting errno, so it
    // is reported directly.
    // SAFETY: a valid pointer to an initialised set, and a null oldset, which
    // the interface documents as "do not report the previous mask".
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    Ok(())
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
        // Ahead of the generic arm: this status is the one that reaches the
        // give-up screen most often, and that screen's whole job is to tell a
        // user staring at a dead display what happened. "exited with status 69"
        // is a true sentence that conveys nothing.
        Some(code) if code == i32::from(wdm_protocol::GREETER_GAVE_UP_EXIT) => {
            "stopped responding and gave up".to_owned()
        }
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

    /// The wait status a greeter that gave up exits with, as waitpid reports it.
    fn gave_up_status() -> ExitStatus {
        ExitStatus::from_raw(i32::from(wdm_protocol::GREETER_GAVE_UP_EXIT) << 8)
    }

    /// The delay a disposition carries, or `None` if it is not a restart.
    ///
    /// A restart now carries a reason too, which most of these tests do not care
    /// about; comparing whole dispositions would tie every backoff assertion to
    /// the exact wording of an error message.
    fn restart_delay(disposition: &Disposition) -> Option<Duration> {
        match disposition {
            Disposition::Restart { delay, .. } => Some(*delay),
            Disposition::GaveUp { .. } => None,
        }
    }

    /// The explanation a disposition carries, whichever kind it is.
    fn reason_of(disposition: &Disposition) -> &str {
        match disposition {
            Disposition::Restart { reason, .. } | Disposition::GaveUp { reason } => reason,
        }
    }

    /// Kills the greeter however the test ends.
    ///
    /// A failing assertion in a test that spawned a real child would otherwise
    /// leave it running for its full lifetime, poisoning every rerun in that
    /// window — and for a SIGTERM-proof child, leave it running for a minute.
    struct Reaper(Greeter);
    impl Drop for Reaper {
        fn drop(&mut self) {
            self.0.kill();
        }
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
        while let Disposition::Restart { delay, .. } = g.note_exit(ExitStatus::from_raw(0)) {
            seen.push(delay);
        }
        assert_eq!(seen, Greeter::possible_delays());
    }

    #[test]
    fn backoff_grows_then_gives_up() {
        let mut g = greeter("/bin/true");
        // Never started, so every exit counts as rapid.
        assert_eq!(
            restart_delay(&g.note_exit(ExitStatus::from_raw(0))),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            restart_delay(&g.note_exit(ExitStatus::from_raw(0))),
            Some(Duration::from_secs(2))
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
            restart_delay(&g.note_exit(ExitStatus::from_raw(0))),
            Some(Duration::from_secs(1))
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
            restart_delay(&g.note_spawn_failure("no such file")),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            restart_delay(&g.note_spawn_failure("no such file")),
            Some(Duration::from_secs(2))
        );

        let Disposition::GaveUp { reason } = g.note_spawn_failure("no such file") else {
            panic!("expected give-up");
        };
        assert!(reason.contains("could not be started"), "{reason}");
        assert!(reason.contains("tty1"), "{reason}");
    }

    #[test]
    fn an_unwaitable_greeter_is_not_reported_as_a_clean_exit() {
        // Driven through dispose(), the seam poll() delegates to, rather than by
        // calling note_unwaitable directly: the bug being guarded against was in
        // the wiring, not the helper — an Err from waitpid turned into a
        // fabricated successful status, making the give-up screen read "exited
        // cleanly" right next to a logged waitpid error. A test that called the
        // helper itself stayed green with that revert in place.
        let mut g = greeter("/bin/true");
        let unwaitable = || Err(std::io::Error::from_raw_os_error(libc::ECHILD));

        // A greeter that has been running for a while: the elapsed time must be
        // discarded, not trusted, because an unwaitable handle says nothing
        // about how long the process lived.
        g.started = Some(Instant::now() - RAPID_FAILURE - Duration::from_secs(1));
        let first = g.dispose(unwaitable()).expect("an error is not still running");
        assert_eq!(restart_delay(&first), Some(Duration::from_secs(1)));
        assert!(reason_of(&first).contains("could not be waited for"), "{first:?}");
        assert!(!reason_of(&first).contains("exited cleanly"), "{first:?}");
        assert!(g.started.is_none(), "started must be cleared");
        assert_eq!(g.rapid_failures, 1, "must count as a rapid failure");

        let _ = g.dispose(unwaitable());
        let Some(Disposition::GaveUp { reason }) = g.dispose(unwaitable()) else {
            panic!("expected give-up");
        };
        assert!(reason.contains("could not be waited for"), "{reason}");
        assert!(!reason.contains("exited cleanly"), "{reason}");
        assert!(reason.contains("tty1"), "{reason}");
    }

    /// The signal mask a process has blocked, read from /proc as the child's
    /// parent cannot otherwise observe it.
    fn blocked_mask(pid: u32) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let line = status
            .lines()
            .find(|l| l.starts_with("SigBlk:"))
            .expect("SigBlk in /proc status");
        u64::from_str_radix(line["SigBlk:".len()..].trim(), 16).unwrap()
    }

    #[test]
    fn spawned_children_do_not_inherit_a_blocked_signal_mask() {
        // calloop's Signals source blocks SIGTERM and SIGINT on wdm's thread so
        // its signalfd is the only reader, and a blocked mask is inherited by
        // fork *and survives exec*. Without the pthread_sigmask in spawn()'s
        // pre_exec, the greeter cannot receive the SIGTERM kill() sends before
        // the handoff — every login would burn the full grace period and end in
        // SIGKILL — and a user session would be unkillable by loginctl or
        // systemd shutdown for the same reason.
        //
        // Blocking happens on this thread rather than through calloop, and the
        // child is /bin/sh reporting its own mask rather than a real greeter,
        // because the property under test is a property of the pre_exec closure
        // Greeter::spawn installs, which needs neither root nor a compositor.

        // The mask is process-global state a failing assertion must not leak:
        // under --test-threads=1 a block left in place outlives this test for
        // the whole run, and among the signals it blocks is SIGINT, which makes
        // Ctrl-C on the test binary inert. Hence a guard rather than a
        // straight-line restore — every unwrap between here and the end is a
        // path that would otherwise skip it.
        struct Mask(libc::sigset_t);
        impl Drop for Mask {
            fn drop(&mut self) {
                // SAFETY: restoring the set captured when the guard was built.
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut());
                }
            }
        }

        // MaybeUninit rather than mem::zeroed: sigset_t is an opaque foreign
        // type, so the initialised value comes from the platform's own
        // initialiser, and `previous` is only read after pthread_sigmask has
        // filled it.
        let mut blocked = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        let mut previous = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: valid pointers to sigset_t storage; sigemptyset initialises
        // `blocked` and pthread_sigmask initialises `previous` on success, both
        // of which are checked before either is read.
        let guard = unsafe {
            assert_eq!(libc::sigemptyset(blocked.as_mut_ptr()), 0);
            let mut blocked = blocked.assume_init();
            libc::sigaddset(&mut blocked, libc::SIGTERM);
            libc::sigaddset(&mut blocked, libc::SIGINT);
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, previous.as_mut_ptr()),
                0
            );
            Mask(previous.assume_init())
        };

        // Built directly rather than through greeter(): new() splits on
        // whitespace, which cannot express a quoted `sh -c` script. The child
        // sleeps so its /proc entry can be read while it is still alive — and is
        // wrapped in a Reaper so a failure below does not leave it sleeping for
        // its full minute.
        let mut g = Reaper(Greeter {
            argv: vec!["/bin/sleep".to_owned(), "60".to_owned()],
            credentials: None,
            socket: "wayland-test".to_owned(),
            child: None,
            pgid: None,
            started: None,
            rapid_failures: 0,
            gave_up: false,
        });

        g.0.spawn().unwrap();
        let pid = g.0.child.as_ref().unwrap().id();
        let mask = blocked_mask(pid);
        g.0.kill();
        drop(guard);

        assert_eq!(
            mask, 0,
            "child inherited blocked signals: SigBlk {mask:016x}"
        );
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
        // The give-up status is a sentence, not a number: it is the one that
        // most often ends up on the give-up screen.
        assert_eq!(
            describe_exit(gave_up_status()),
            "stopped responding and gave up"
        );
    }

    #[test]
    fn a_greeter_that_gave_up_counts_against_the_budget_however_long_it_ran() {
        // The webkit greeter takes about half a minute to conclude a silent web
        // process is never going to answer — well past RAPID_FAILURE. Judged on
        // uptime alone each of those exits looks like a healthy greeter that
        // happened to stop, `rapid_failures` resets to 1 every time, and the
        // give-up screen is unreachable: the login screen just reloads for ever.
        let mut g = greeter("/bin/true");
        let long_enough_to_look_healthy = Instant::now() - (RAPID_FAILURE * 3);

        for _ in 0..MAX_RAPID_FAILURES {
            g.started = Some(long_enough_to_look_healthy);
            if let Disposition::GaveUp { reason } = g.note_exit(gave_up_status()) {
                assert!(reason.contains("tty1"), "{reason}");
                // And the screen has to say something a user can act on. The
                // status is an implementation detail of the greeter contract;
                // "exited with status 69" on the one screen whose entire job is
                // a truthful actionable message is a wasted screen.
                assert!(reason.contains("gave up"), "{reason}");
                assert!(!reason.contains("status 69"), "{reason}");
                return;
            }
        }
        panic!("a greeter that gave up never reached the give-up screen");
    }

    #[test]
    fn a_long_running_greeter_that_merely_crashed_still_gets_a_fresh_budget() {
        // The counterpart: only the give-up status bypasses the uptime rule, so
        // an ordinary crash after hours of use is still a first failure.
        let mut g = greeter("/bin/true");
        for _ in 0..MAX_RAPID_FAILURES * 2 {
            g.started = Some(Instant::now() - (RAPID_FAILURE * 3));
            assert!(
                !matches!(g.note_exit(ExitStatus::from_raw(0x100)), Disposition::GaveUp { .. }),
                "an occasional crash was treated as a failure to start"
            );
        }
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

        assert!(matches!(disposition, Disposition::Restart { .. }));
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
    fn the_cache_dir_is_created_tight_and_reasserted() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("wdm");
        // Chowning to ourselves is the one ownership change an unprivileged test
        // is allowed to make; in production these are the greeter's.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

        create_cache_dir_at(&dir, uid, gid).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&meta) & 0o777, 0o700);
        assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), uid);

        // Unlike the runtime directory this one persists across boots and is
        // owned by the greeter throughout, so a second pass must still pull the
        // mode back rather than trusting what it finds.
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();
        create_cache_dir_at(&dir, uid, gid).unwrap();
        let meta = std::fs::metadata(&dir).unwrap();
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&meta) & 0o777, 0o700);
    }

    #[test]
    fn creating_the_cache_dir_does_not_lock_down_its_parent() {
        // DirBuilder::mode applies to every component it creates, so a recursive
        // create with 0700 on a system without /var/cache would leave that
        // root-owned and root-only and break every other package that writes
        // there. Only the leaf may be tightened.
        let base = tempfile::tempdir().unwrap();
        let parent = base.path().join("var-cache");
        let dir = parent.join("wdm");
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

        create_cache_dir_at(&dir, uid, gid).unwrap();

        let parent_mode =
            std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&parent).unwrap()) & 0o777;
        assert_ne!(parent_mode, 0o700, "the parent must not inherit the leaf's mode");
        assert!(parent_mode & 0o055 != 0, "the parent stays traversable: {parent_mode:o}");

        let leaf = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&dir).unwrap()) & 0o777;
        assert_eq!(leaf, 0o700);
    }

    #[test]
    fn a_symlinked_cache_dir_is_refused_rather_than_followed() {
        // The attack O_NOFOLLOW exists for: the greeter owns this directory
        // between boots, so a compromised one can replace it with a symlink and
        // wait for root to chmod and chown the target on the next boot.
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("someone-elses");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let dir = base.path().join("wdm");
        std::os::unix::fs::symlink(&target, &dir).unwrap();

        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let err = create_cache_dir_at(&dir, uid, gid).unwrap_err();
        // Either errno is a refusal and that is all this asserts. POSIX says
        // O_NOFOLLOW reports ELOOP, but Linux reports ENOTDIR when O_DIRECTORY
        // is also set, and pinning one of them would fail on the other kernel
        // while the protection was working.
        assert!(
            matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
            "a symlink must fail the open, not be followed: {err}"
        );

        // And the target is untouched — the whole point.
        let meta = std::fs::metadata(&target).unwrap();
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&meta) & 0o777, 0o755);
    }

    #[test]
    fn unprivileged_greeter_has_no_credentials_to_drop() {
        // The development path must not try to setuid, which would fail as a
        // normal user and make `--backend winit` unusable.
        assert!(greeter("/bin/true").credentials.is_none());
    }
}
