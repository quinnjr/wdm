//! Launching a user's session after a successful login.
//!
//! Everything that needs root — resolving the account, computing the
//! supplementary group list, opening the VT — happens in the parent before the
//! fork, so the child does only async-signal-safe work. That matters because
//! wdm has PAM threads alive at this point, and a forked child of a
//! multithreaded process may not allocate or take locks.
//!
//! Releasing DRM master is *not* done here. The caller closes its libseat
//! session before calling [`Launch::spawn`], because dropping master from the
//! child would not release the fds the parent still holds.

use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};

use uzers::os::unix::UserExt;

// `SessionType` is no longer named here: the XDG_SESSION_* mapping lives on
// `Session` so this module and auth.rs cannot drift apart on it.
use crate::sessions::Session;

/// Failure to prepare or launch a session.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("no such user: {0}")]
    NoSuchUser(String),
    #[error("user {0} has no valid login shell")]
    NoShell(String),
    #[error("user {0} is outside the loginable uid range")]
    NotLoginable(String),
    #[error("session command for {0} is empty")]
    EmptyCommand(String),
    #[error("resolving groups for {0}: {1}")]
    Groups(String, #[source] std::io::Error),
    #[error("a path for {0} is not representable as a C string")]
    NulInPath(String),
    #[error("spawning session: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Everything needed to launch a session, resolved while still privileged.
///
/// Built with [`Launch::prepare`] before the greeter is torn down, so a
/// resolution failure is reported while there is still a greeter to report it
/// to.
#[derive(Debug)]
pub struct Launch {
    username: String,
    uid: u32,
    gid: u32,
    /// Supplementary groups, resolved here because `initgroups` reads the group
    /// database and is not safe to call in a forked child.
    groups: Vec<u32>,
    home: PathBuf,
    shell: PathBuf,
    /// The session command, run through the login shell.
    command: String,
    /// Fully assembled environment; the child clears its own and uses this.
    env: Vec<(String, String)>,
    /// VT to make the session's controlling terminal.
    vt: u32,
}

impl Launch {
    /// Resolve an account and assemble the environment for a session.
    ///
    /// `pam_env` is the environment `pam_open_session` produced, which is where
    /// `XDG_RUNTIME_DIR` comes from — wdm must not invent that itself, because
    /// pam_systemd owns the directory's lifecycle.
    ///
    /// `extra_env` is what the greeter asked for. It is filtered through
    /// [`greeter_may_set`] and applied *before* wdm's own session variables, so
    /// a greeter can localise the session but can neither influence how it loads
    /// code nor contradict a fact about the seat.
    pub fn prepare(
        session: &Session,
        username: &str,
        vt: u32,
        pam_env: Vec<(String, String)>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self, LaunchError> {
        let user = uzers::get_user_by_name(username)
            .ok_or_else(|| LaunchError::NoSuchUser(username.to_owned()))?;

        // The uid range is re-checked here, not just when enumerating users.
        // create_session deliberately accepts usernames that were never
        // advertised, so that the greeter cannot be used to enumerate accounts —
        // which means an unadvertised name reaches this point with a valid
        // password behind it. Without this check, `create_session("root")` plus
        // the root password launches a root graphical session from a display
        // manager documented as never offering root.
        let range = crate::users::UidRange::from_system();
        if !range.contains(user.uid()) {
            return Err(LaunchError::NotLoginable(username.to_owned()));
        }
        if !crate::users::is_login_shell(user.shell()) {
            return Err(LaunchError::NoShell(username.to_owned()));
        }
        if session.exec.trim().is_empty() {
            return Err(LaunchError::EmptyCommand(session.id.clone()));
        }

        let uid = user.uid();
        let gid = user.primary_group_id();
        let home = user.home_dir().to_owned();
        let shell = user.shell().to_owned();

        let groups = supplementary_groups(username, gid)
            .map_err(|e| LaunchError::Groups(username.to_owned(), e))?;

        let env = build_env(session, username, &home, &shell, vt, pam_env, extra_env);

        Ok(Self {
            username: username.to_owned(),
            uid,
            gid,
            groups,
            home,
            shell,
            command: session.exec.clone(),
            env,
            vt,
        })
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Fork and exec the session.
    ///
    /// The caller must already have released DRM master and input devices, or
    /// the session's compositor will fail to acquire them.
    pub fn spawn(&self) -> Result<Child, LaunchError> {
        // Everything the child touches is built here, while allocation is still
        // legal. The pre_exec closure below only calls libc.
        let uid = self.uid;
        let gid = self.gid;
        let groups: Vec<libc::gid_t> = self.groups.clone();
        let home = cstring(self.home.clone().into_os_string(), &self.username)?;
        let tty = cstring(
            OsString::from(format!("/dev/tty{}", self.vt)),
            &self.username,
        )?;

        // Run through the login shell so /etc/profile and the user's dotfiles
        // are sourced. A desktop session launched without them is missing PATH
        // entries, locale, and toolkit settings, which users experience as the
        // session working from a TTY but not from the display manager.
        let mut command = Command::new(&self.shell);
        command
            .arg("-l")
            .arg("-c")
            .arg(format!("exec {}", self.command))
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&self.home);

        // SAFETY: this closure runs in the forked child of a process that has
        // PAM threads, so it must be async-signal-safe: no allocation, no
        // locks, only libc calls on data captured above.
        //
        // std applies its own uid/gid *before* pre_exec closures, which would
        // make setgroups fail, so wdm does the whole privilege drop here and
        // does not use Command::uid/gid at all. The order is mandatory:
        // supplementary groups, then gid, then uid. Dropping uid first would
        // forfeit the privilege needed for the other two.
        unsafe {
            command.pre_exec(move || {
                // First, before anything else: calloop's signal source blocks
                // SIGTERM and SIGINT on wdm's thread (see backend/setup.rs), and
                // a blocked mask is inherited by fork and survives exec. A user
                // session started with them blocked inherits that mask into
                // every process it ever starts: `loginctl terminate-session`,
                // `systemctl stop` and systemd's shutdown TERM phase all degrade
                // to SIGKILL, and Ctrl-C is dead in every terminal.
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                // pthread_sigmask returns the error number rather than setting
                // errno, so it is reported directly — and from_raw_os_error
                // does not allocate, which matters here for the reason given
                // below.
                let rc = libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                if rc != 0 {
                    return Err(std::io::Error::from_raw_os_error(rc));
                }

                // A new session and process group, so the session does not
                // share wdm's and is not killed with it.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                // Give the session a controlling terminal of its own. Without
                // this the session inherits wdm's stdio and job control does
                // not work for anything it starts.
                let fd = libc::open(tty.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                if fd >= 0 {
                    // Best effort: a session whose compositor talks to DRM does
                    // not strictly need a controlling tty, so failing here is
                    // not worth refusing the login over.
                    libc::ioctl(fd, libc::TIOCSCTTY, 1);
                    libc::dup2(fd, 0);
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    if fd > 2 {
                        libc::close(fd);
                    }
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

                // Refuse to continue if the privilege drop did not stick.
                // Exec'ing a user session with root credentials would be a
                // total compromise, and setuid can fail in ways that do not
                // set errno on every libc.
                //
                // from_raw_os_error rather than Error::other: the latter boxes
                // a message, which allocates, and this closure runs in a child
                // forked from a process with live PAM threads. If one of them
                // held the allocator lock at fork time, allocating here would
                // deadlock the child at exactly the moment it is trying to
                // abort a failed privilege drop.
                if libc::getuid() != uid || libc::geteuid() != uid {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                if libc::getgid() != gid || libc::getegid() != gid {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }

                // current_dir already chdir'd, but it runs before this closure
                // and while still root; redo it so a home directory only
                // readable by the user is entered as the user. A failure here
                // is fatal for the same reason the checks above are: the
                // session would start somewhere the user cannot read, and the
                // surrounding code has already committed to refusing rather
                // than continuing in a half-dropped state.
                if libc::chdir(home.as_ptr()) < 0 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }

        command.spawn().map_err(LaunchError::Spawn)
    }
}

fn cstring(value: OsString, username: &str) -> Result<CString, LaunchError> {
    CString::new(value.into_vec()).map_err(|_| LaunchError::NulInPath(username.to_owned()))
}

/// Resolve a user's full supplementary group list.
///
/// Uses `getgrouplist`, which consults NSS, so groups from LDAP or SSSD are
/// included. The primary gid is passed in and appears in the result, matching
/// what `initgroups` would have produced.
pub(crate) fn supplementary_groups(username: &str, gid: u32) -> std::io::Result<Vec<u32>> {
    let name = CString::new(username)
        .map_err(|_| std::io::Error::other("username contains a NUL byte"))?;

    // getgrouplist reports the required size through ngroups when it does not
    // fit, so the first call sizes the buffer and the second fills it.
    let mut count: libc::c_int = 16;
    loop {
        let mut buf = vec![0 as libc::gid_t; count.max(1) as usize];

        // SAFETY: name is a valid NUL-terminated string, and buf has room for
        // `count` entries, which is what is advertised.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid, buf.as_mut_ptr(), &mut count) };

        if rc >= 0 {
            buf.truncate(count.max(0) as usize);
            return Ok(buf);
        }

        // Negative means the buffer was too small and count now holds the
        // needed size. Guard against a libc that does not update it, which
        // would otherwise spin forever.
        if count <= buf.len() as libc::c_int {
            return Err(std::io::Error::other(
                "getgrouplist did not report a larger buffer size",
            ));
        }
    }
}

/// Assemble the session's environment.
///
/// Later sources win. PAM's environment comes first because it is authoritative
/// for `XDG_RUNTIME_DIR`; then the greeter's filtered additions; then wdm's own
/// session variables, which describe facts about the seat and must therefore be
/// last so nothing can contradict them.
fn build_env(
    session: &Session,
    username: &str,
    home: &std::path::Path,
    shell: &std::path::Path,
    vt: u32,
    pam_env: Vec<(String, String)>,
    extra_env: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |key: &str, value: String| {
        if let Some(slot) = env.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            env.push((key.to_owned(), value));
        }
    };

    // A minimal PATH; the login shell will replace it from /etc/profile. Without
    // any PATH at all, a session whose Exec is not an absolute path fails to
    // start with a confusing "not found".
    set("PATH", "/usr/local/bin:/usr/bin:/bin".to_owned());
    set("TERM", "linux".to_owned());

    for (key, value) in pam_env {
        set(&key, value);
    }

    // Applied before wdm's own facts, and filtered, so a greeter can localise a
    // session but cannot influence how it loads code or who it runs as.
    for (key, value) in extra_env {
        if greeter_may_set(&key, &value) {
            set(&key, value);
        } else {
            log::warn!("ignoring environment variable {key} requested by the greeter");
        }
    }

    set("HOME", home.to_string_lossy().into_owned());
    set("PWD", home.to_string_lossy().into_owned());
    set("SHELL", shell.to_string_lossy().into_owned());
    set("USER", username.to_owned());
    set("LOGNAME", username.to_owned());

    set("XDG_SEAT", "seat0".to_owned());
    set("XDG_VTNR", vt.to_string());
    set("XDG_SESSION_CLASS", "user".to_owned());
    set("XDG_SESSION_TYPE", session.xdg_session_type().to_owned());
    // Desktops key their own configuration off this.
    set("XDG_SESSION_DESKTOP", session.xdg_session_desktop().to_owned());

    env
}

/// Environment variables the greeter is allowed to contribute.
///
/// An allowlist, not a denylist. The greeter is unprivileged, untrusted code,
/// and the session it is populating runs as the authenticated user through a
/// login shell — so a variable it controls is a variable it can use to run code
/// as that user. `LD_PRELOAD` and `LD_LIBRARY_PATH` are the obvious ones, but
/// `PATH`, `SHELL`, `IFS`, `BASH_ENV` and `ENV` are equally good, and enumerating
/// them is a game one eventually loses. Only presentation variables are allowed
/// through, which is all a greeter has any business setting.
fn greeter_may_set(key: &str, value: &str) -> bool {
    const ALLOWED: &[&str] = &["LANG", "LANGUAGE"];
    const ALLOWED_PREFIXES: &[&str] = &["LC_", "XKB_"];

    if !(ALLOWED.contains(&key) || ALLOWED_PREFIXES.iter().any(|p| key.starts_with(p))) {
        return false;
    }

    // Values are checked too, not just keys. glibc treats a locale name
    // containing a slash as a path and loads the locale from there — it only
    // refuses that for setuid binaries, which a user session is not — and
    // libxkbcommon honours XKB_CONFIG_ROOT as a data root. Either would let the
    // greeter point the authenticated user's session at files it controls.
    if value.contains('/') || value.contains('\0') {
        return false;
    }

    // A locale or layout name is short. A long value is not one, and is a
    // plausible attempt at something else.
    value.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionType;
    use std::path::Path;

    fn session(session_type: SessionType) -> Session {
        Session {
            id: "sway.desktop".to_owned(),
            name: "Sway".to_owned(),
            exec: "sway".to_owned(),
            session_type,
            path: PathBuf::from("/usr/share/wayland-sessions/sway.desktop"),
        }
    }

    fn env_of(env: &[(String, String)], key: &str) -> Option<String> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn sets_the_session_variables() {
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/zsh"),
            7,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(env_of(&env, "USER").as_deref(), Some("testuser"));
        assert_eq!(env_of(&env, "LOGNAME").as_deref(), Some("testuser"));
        assert_eq!(env_of(&env, "HOME").as_deref(), Some("/home/testuser"));
        assert_eq!(env_of(&env, "SHELL").as_deref(), Some("/bin/zsh"));
        assert_eq!(env_of(&env, "XDG_VTNR").as_deref(), Some("7"));
        assert_eq!(env_of(&env, "XDG_SEAT").as_deref(), Some("seat0"));
        assert_eq!(env_of(&env, "XDG_SESSION_TYPE").as_deref(), Some("wayland"));
        assert_eq!(env_of(&env, "XDG_SESSION_CLASS").as_deref(), Some("user"));
        assert_eq!(env_of(&env, "XDG_SESSION_DESKTOP").as_deref(), Some("sway"));
        // A PATH must always be present or a non-absolute Exec cannot be found.
        assert!(env_of(&env, "PATH").is_some_and(|p| !p.is_empty()));
    }

    #[test]
    fn x11_sessions_are_labelled_x11() {
        let env = build_env(
            &session(SessionType::X11),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(env_of(&env, "XDG_SESSION_TYPE").as_deref(), Some("x11"));
    }

    #[test]
    fn pam_environment_supplies_runtime_dir() {
        // XDG_RUNTIME_DIR must come from pam_systemd, never be invented by wdm.
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            vec![("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned())],
            Vec::new(),
        );
        assert_eq!(
            env_of(&env, "XDG_RUNTIME_DIR").as_deref(),
            Some("/run/user/1000")
        );
    }

    #[test]
    fn wdm_facts_override_pam() {
        // A stale XDG_VTNR from PAM would point the session at the wrong VT.
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            vec![("XDG_VTNR".to_owned(), "1".to_owned())],
            Vec::new(),
        );
        assert_eq!(env_of(&env, "XDG_VTNR").as_deref(), Some("7"));
    }

    #[test]
    fn greeter_may_localise_the_session() {
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            vec![
                ("LANG".to_owned(), "de_DE.UTF-8".to_owned()),
                ("LC_TIME".to_owned(), "en_GB.UTF-8".to_owned()),
                ("XKB_DEFAULT_LAYOUT".to_owned(), "de".to_owned()),
            ],
        );
        assert_eq!(env_of(&env, "LANG").as_deref(), Some("de_DE.UTF-8"));
        assert_eq!(env_of(&env, "LC_TIME").as_deref(), Some("en_GB.UTF-8"));
        assert_eq!(env_of(&env, "XKB_DEFAULT_LAYOUT").as_deref(), Some("de"));
    }

    #[test]
    fn greeter_cannot_inject_code_into_the_session() {
        // The greeter is unprivileged and untrusted, but the session runs as the
        // authenticated user through a login shell. Any of these would be
        // arbitrary code execution as that user.
        let dangerous = [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "LD_AUDIT",
            "PATH",
            "SHELL",
            "HOME",
            "IFS",
            "BASH_ENV",
            "ENV",
            "XDG_RUNTIME_DIR",
            "XDG_VTNR",
            "XDG_SEAT",
            "PYTHONPATH",
            "PERL5LIB",
            "GIO_MODULE_DIR",
        ];

        for key in dangerous {
            assert!(
                !greeter_may_set(key, "x"),
                "{key} must not be greeter-settable"
            );
        }

        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            dangerous
                .iter()
                .map(|k| ((*k).to_owned(), "/tmp/evil".to_owned()))
                .collect(),
        );

        assert!(env_of(&env, "LD_PRELOAD").is_none());
        assert!(env_of(&env, "LD_LIBRARY_PATH").is_none());
        // The ones wdm sets itself must hold wdm's values, not the greeter's.
        assert_eq!(env_of(&env, "HOME").as_deref(), Some("/home/testuser"));
        assert_eq!(env_of(&env, "SHELL").as_deref(), Some("/bin/sh"));
        assert_eq!(env_of(&env, "XDG_VTNR").as_deref(), Some("7"));
        assert_ne!(env_of(&env, "PATH").as_deref(), Some("/tmp/evil"));
    }

    #[test]
    fn allowlisted_keys_still_reject_path_values() {
        // glibc loads a locale from a path if the name contains a slash, and
        // libxkbcommon honours XKB_CONFIG_ROOT as a data root. Allowing the key
        // is not the same as allowing any value for it.
        assert!(greeter_may_set("LANG", "de_DE.UTF-8"));
        assert!(greeter_may_set("LC_ALL", "en_GB.UTF-8"));
        assert!(greeter_may_set("XKB_DEFAULT_LAYOUT", "de"));

        assert!(!greeter_may_set("LC_ALL", "/tmp/evil/locale"));
        assert!(!greeter_may_set("LANG", "../../tmp/evil"));
        assert!(!greeter_may_set("XKB_CONFIG_ROOT", "/tmp/evil"));
        assert!(!greeter_may_set("LANG", &"a".repeat(4096)));
    }

    #[test]
    fn wdm_facts_win_over_the_greeter() {
        // Applied after the greeter's contribution, so even an allowlisted key
        // cannot displace a fact about the seat.
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            vec![("XDG_SESSION_TYPE".to_owned(), "x11".to_owned())],
        );
        assert_eq!(env_of(&env, "XDG_SESSION_TYPE").as_deref(), Some("wayland"));
    }

    #[test]
    fn keys_are_never_duplicated() {
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            vec![("PATH".to_owned(), "/pam".to_owned())],
            vec![("LANG".to_owned(), "de_DE.UTF-8".to_owned())],
        );

        for key in ["PATH", "LANG", "HOME"] {
            let hits = env.iter().filter(|(k, _)| k == key).count();
            assert!(hits <= 1, "duplicate {key} makes the winner undefined");
        }
        // PAM's PATH survives, since wdm only supplies a fallback.
        assert_eq!(env_of(&env, "PATH").as_deref(), Some("/pam"));
    }

    #[test]
    fn rejects_a_user_outside_the_login_range() {
        // root authenticates fine if someone knows the password; the uid range
        // is what stops it becoming a graphical root session.
        let err = Launch::prepare(
            &session(SessionType::Wayland),
            "root",
            7,
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, LaunchError::NotLoginable(_) | LaunchError::NoShell(_)),
            "root was not rejected: {err:?}"
        );
    }

    #[test]
    fn wdm_socket_never_leaks_into_the_session() {
        // The greeter's socket must not be visible to the user's compositor, or
        // it will try to nest inside wdm instead of taking over the display.
        let env = build_env(
            &session(SessionType::Wayland),
            "testuser",
            Path::new("/home/testuser"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            Vec::new(),
        );
        assert!(env_of(&env, "WAYLAND_DISPLAY").is_none());
    }

    #[test]
    fn resolves_groups_for_the_current_user() {
        // Against the real group database: the primary gid must always appear,
        // which is the invariant initgroups guarantees.
        let uid = unsafe { libc::getuid() };
        let user = uzers::get_user_by_uid(uid).expect("current user is in passwd");
        let name = user.name().to_str().unwrap();
        let gid = user.primary_group_id();

        let groups = supplementary_groups(name, gid).unwrap();
        assert!(groups.contains(&gid), "groups: {groups:?}");
    }

    #[test]
    fn rejects_unknown_user() {
        let err = Launch::prepare(
            &session(SessionType::Wayland),
            "definitely-not-a-user-on-this-box",
            7,
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(err, LaunchError::NoSuchUser(_)), "{err:?}");
    }

    #[test]
    fn rejects_empty_command() {
        let mut s = session(SessionType::Wayland);
        s.exec = "   ".to_owned();

        let uid = unsafe { libc::getuid() };
        let user = uzers::get_user_by_uid(uid).unwrap();
        let name = user.name().to_str().unwrap().to_owned();

        // Only reachable for a user with a login shell; skip if the test runs as
        // an account without one rather than asserting the wrong thing.
        if !crate::users::is_login_shell(user.shell()) {
            return;
        }

        let err = Launch::prepare(&s, &name, 7, Vec::new(), Vec::new()).unwrap_err();
        assert!(matches!(err, LaunchError::EmptyCommand(_)), "{err:?}");
    }
}
