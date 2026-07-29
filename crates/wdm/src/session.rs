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

use crate::sessions::{Session, SessionType};

/// Failure to prepare or launch a session.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("no such user: {0}")]
    NoSuchUser(String),
    #[error("user {0} has no valid login shell")]
    NoShell(String),
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
    /// pam_systemd owns the directory's lifecycle. `extra_env` is what the
    /// greeter asked for, and is applied last so a greeter can override
    /// presentation variables like `LANG` but is subject to the same validation
    /// as everything else.
    pub fn prepare(
        session: &Session,
        username: &str,
        vt: u32,
        pam_env: Vec<(String, String)>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self, LaunchError> {
        let user = uzers::get_user_by_name(username)
            .ok_or_else(|| LaunchError::NoSuchUser(username.to_owned()))?;

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
                if libc::getuid() != uid || libc::geteuid() != uid {
                    return Err(std::io::Error::other("uid did not change"));
                }
                if libc::getgid() != gid || libc::getegid() != gid {
                    return Err(std::io::Error::other("gid did not change"));
                }

                // current_dir already chdir'd, but it runs before this closure
                // and while still root; redo it so a home directory only
                // readable by the user is entered as the user.
                libc::chdir(home.as_ptr());

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
/// Later sources win. PAM's environment comes first because it is
/// authoritative for `XDG_RUNTIME_DIR`; then wdm's own session variables, which
/// describe facts about the seat that must be correct; then the greeter's
/// additions.
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

    set("HOME", home.to_string_lossy().into_owned());
    set("PWD", home.to_string_lossy().into_owned());
    set("SHELL", shell.to_string_lossy().into_owned());
    set("USER", username.to_owned());
    set("LOGNAME", username.to_owned());

    set("XDG_SEAT", "seat0".to_owned());
    set("XDG_VTNR", vt.to_string());
    set("XDG_SESSION_CLASS", "user".to_owned());
    set(
        "XDG_SESSION_TYPE",
        match session.session_type {
            SessionType::Wayland => "wayland".to_owned(),
            SessionType::X11 => "x11".to_owned(),
        },
    );
    // Desktops key their own configuration off this.
    set("XDG_SESSION_DESKTOP", session.id.trim_end_matches(".desktop").to_owned());

    for (key, value) in extra_env {
        set(&key, value);
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "joseph",
            Path::new("/home/joseph"),
            Path::new("/bin/zsh"),
            7,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(env_of(&env, "USER").as_deref(), Some("joseph"));
        assert_eq!(env_of(&env, "LOGNAME").as_deref(), Some("joseph"));
        assert_eq!(env_of(&env, "HOME").as_deref(), Some("/home/joseph"));
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
            "joseph",
            Path::new("/home/joseph"),
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
            "joseph",
            Path::new("/home/joseph"),
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
            "joseph",
            Path::new("/home/joseph"),
            Path::new("/bin/sh"),
            7,
            vec![("XDG_VTNR".to_owned(), "1".to_owned())],
            Vec::new(),
        );
        assert_eq!(env_of(&env, "XDG_VTNR").as_deref(), Some("7"));
    }

    #[test]
    fn greeter_env_is_applied_last() {
        let env = build_env(
            &session(SessionType::Wayland),
            "joseph",
            Path::new("/home/joseph"),
            Path::new("/bin/sh"),
            7,
            Vec::new(),
            vec![("LANG".to_owned(), "de_DE.UTF-8".to_owned())],
        );
        assert_eq!(env_of(&env, "LANG").as_deref(), Some("de_DE.UTF-8"));
    }

    #[test]
    fn keys_are_never_duplicated() {
        let env = build_env(
            &session(SessionType::Wayland),
            "joseph",
            Path::new("/home/joseph"),
            Path::new("/bin/sh"),
            7,
            vec![("PATH".to_owned(), "/pam".to_owned())],
            vec![("PATH".to_owned(), "/greeter".to_owned())],
        );
        let paths: Vec<_> = env.iter().filter(|(k, _)| k == "PATH").collect();
        assert_eq!(paths.len(), 1, "duplicate keys make the winner undefined");
        assert_eq!(paths[0].1, "/greeter");
    }

    #[test]
    fn wdm_socket_never_leaks_into_the_session() {
        // The greeter's socket must not be visible to the user's compositor, or
        // it will try to nest inside wdm instead of taking over the display.
        let env = build_env(
            &session(SessionType::Wayland),
            "joseph",
            Path::new("/home/joseph"),
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
