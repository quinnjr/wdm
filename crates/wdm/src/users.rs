//! Enumeration of loginable users, and the record of what each last ran.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use uzers::os::unix::UserExt;

/// Where `UID_MIN` and `UID_MAX` are configured on a Linux system.
const LOGIN_DEFS: &str = "/etc/login.defs";

/// Used when `/etc/login.defs` is absent or does not set `UID_MIN`.
///
/// 1000 is the value every mainstream distribution ships. Guessing lower would
/// expose system accounts on the login screen.
const FALLBACK_UID_MIN: u32 = 1000;

/// Used when `UID_MAX` is unset. Excludes the `nobody` account, which sits at
/// 65534 and is not loginable.
const FALLBACK_UID_MAX: u32 = 60_000;

/// System-wide avatar directory maintained by AccountsService.
const ACCOUNTS_ICON_DIR: &str = "/var/lib/AccountsService/icons";

/// Where wdm records the session each user last logged into successfully.
pub const LAST_SESSION_PATH: &str = "/var/lib/wdm/last-session";

/// A user wdm is willing to offer on the login screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// Login name.
    pub name: String,
    /// Human readable name from the GECOS field, empty if unset.
    pub display_name: String,
    /// Absolute path to an avatar image, empty if none was found.
    pub avatar_path: String,
    /// Session id from this user's last successful login, empty if none.
    pub last_session: String,
}

/// The inclusive uid range considered to hold human accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UidRange {
    pub min: u32,
    pub max: u32,
}

impl Default for UidRange {
    fn default() -> Self {
        Self {
            min: FALLBACK_UID_MIN,
            max: FALLBACK_UID_MAX,
        }
    }
}

impl UidRange {
    /// Read the range from `/etc/login.defs`, falling back to defaults.
    pub fn from_system() -> Self {
        match std::fs::read_to_string(LOGIN_DEFS) {
            Ok(text) => Self::parse_login_defs(&text),
            Err(e) => {
                log::debug!("{LOGIN_DEFS}: {e}, using default uid range");
                Self::default()
            }
        }
    }

    /// Parse `UID_MIN` and `UID_MAX` out of `login.defs` syntax.
    ///
    /// The format is whitespace-separated `KEY VALUE` with `#` comments. Keys
    /// wdm does not care about are ignored, and an unparsable value falls back
    /// rather than failing: a malformed `login.defs` must not make the machine
    /// unloginable.
    pub fn parse_login_defs(text: &str) -> Self {
        let mut range = Self::default();

        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let value = value.trim();

            let target = match key {
                "UID_MIN" => &mut range.min,
                "UID_MAX" => &mut range.max,
                _ => continue,
            };
            match value.parse() {
                Ok(parsed) => *target = parsed,
                Err(_) => log::warn!("{LOGIN_DEFS}: {key} value {value:?} is not a number"),
            }
        }

        // Root is never a login target, whatever local policy says. A
        // `UID_MIN 0` in login.defs would otherwise enumerate root to the
        // untrusted greeter.
        range.min = range.min.max(1);

        if range.min > range.max {
            log::warn!(
                "{LOGIN_DEFS}: UID_MIN {} exceeds UID_MAX {}, using defaults",
                range.min,
                range.max
            );
            return Self::default();
        }

        range
    }

    pub fn contains(self, uid: u32) -> bool {
        uid >= self.min && uid <= self.max
    }
}

/// Whether a shell permits interactive login.
///
/// Accounts that exist only to own files are pointed at `nologin` or `false`.
/// An empty shell means the system default, which is loginable.
pub fn is_login_shell(shell: &Path) -> bool {
    let Some(name) = shell.file_name().and_then(|n| n.to_str()) else {
        // No file name at all: an empty shell field, meaning the system
        // default. Treat as loginable.
        return shell.as_os_str().is_empty();
    };
    !matches!(name, "nologin" | "false" | "true" | "sync" | "shutdown" | "halt")
}

/// GECOS is comma-separated; the first field is the human readable name.
fn display_name_from_gecos(gecos: &str) -> String {
    gecos.split(',').next().unwrap_or("").trim().to_owned()
}

/// Find an avatar for a user, preferring the system-managed one.
///
/// Returns an empty string when nothing is found, matching the protocol's
/// "empty if none" contract.
fn avatar_for(name: &str, home: &Path) -> String {
    let candidates = [
        PathBuf::from(ACCOUNTS_ICON_DIR).join(name),
        home.join(".face"),
        home.join(".face.icon"),
    ];
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Enumerate loginable users.
///
/// Goes through the passwd database rather than reading `/etc/passwd`, so users
/// provided by LDAP, SSSD or systemd-homed appear too.
pub fn discover(range: UidRange, last_sessions: &LastSessions) -> Vec<User> {
    // SAFETY: getpwent is not reentrant and shares a static iteration cursor, so
    // the invariant is that no other thread calls getpwent/setpwent/endpwent.
    // wdm calls this only from the event loop thread. The PAM threads do resolve
    // accounts through NSS, but via getpwnam_r, which does not touch that
    // cursor — the earlier justification named the wrong reason.
    let all = unsafe { uzers::all_users() };

    let mut users: Vec<User> = all
        .filter(|u| range.contains(u.uid()))
        .filter(|u| is_login_shell(u.shell()))
        .filter_map(|u| {
            let name = u.name().to_str()?.to_owned();
            let avatar_path = avatar_for(&name, u.home_dir());
            let last_session = last_sessions.get(&name).unwrap_or_default().to_owned();
            Some(User {
                display_name: display_name_from_gecos(
                    u.gecos().to_str().unwrap_or_default(),
                ),
                avatar_path,
                last_session,
                name,
            })
        })
        .collect();

    // Database order is not meaningful; sort so the greeter's list is stable.
    users.sort_by(|a, b| a.name.cmp(&b.name));
    users.dedup_by(|a, b| a.name == b.name);
    users
}

/// The mapping of user to last successfully launched session.
///
/// Persisted so a returning user finds their session preselected. wdm reports
/// the value; whether to preselect it is the greeter's policy.
#[derive(Debug, Clone, Default)]
pub struct LastSessions {
    entries: HashMap<String, String>,
}

impl LastSessions {
    /// Load the store, treating any problem as "no history".
    ///
    /// This is a convenience record, not state wdm needs to function, so a
    /// missing or corrupt file must never block logins.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!("{}: {e}, ignoring session history", path.display());
                Self::default()
            }
        }
    }

    /// Parse the tab-separated `user<TAB>session` format.
    ///
    /// Malformed lines are skipped individually rather than discarding the
    /// whole file, so one bad line does not lose every user's preference.
    pub fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .filter_map(|line| {
                let (user, session) = line.split_once('\t')?;
                let (user, session) = (user.trim(), session.trim());
                if user.is_empty() || session.is_empty() {
                    return None;
                }
                Some((user.to_owned(), session.to_owned()))
            })
            .collect();
        Self { entries }
    }

    pub fn get(&self, user: &str) -> Option<&str> {
        self.entries.get(user).map(String::as_str)
    }

    pub fn set(&mut self, user: &str, session_id: &str) {
        self.entries
            .insert(user.to_owned(), session_id.to_owned());
    }

    /// Serialise to the on-disk format, sorted so the file does not churn.
    pub fn to_text(&self) -> String {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|(user, session)| format!("{user}\t{session}"))
            .collect();
        lines.sort();
        lines.push(String::new()); // trailing newline
        lines.join("\n")
    }

    /// Write the store, creating the parent directory if needed.
    ///
    /// Writes to a temporary file and renames, so a crash mid-write cannot
    /// leave a truncated file that loses every user's preference.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("tmp");

        // Written through a File and synced before the rename: `fs::write` plus
        // `rename` is atomic against a process crash, but after a power cut the
        // filesystem can expose a zero-length file at the target, which loses
        // every user's preference rather than one.
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(self.to_text().as_bytes())?;
            file.sync_all()?;
        }

        std::fs::rename(&tmp, path)?;

        // The rename is the operation that has to survive a power cut, and only
        // a directory sync makes it durable — syncing the file alone leaves the
        // directory entry in the page cache, so the rename itself can be lost.
        // Best effort: losing a session preference is cosmetic, and failing the
        // save here would be worse than a stale preference.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
            && let Err(e) = dir.sync_all()
        {
            log::debug!("syncing {}: {e}", parent.display());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_login_defs() {
        let range = UidRange::parse_login_defs(
            "# comment\nUID_MIN\t\t 500\nUID_MAX 60000\nGID_MIN 1000\n",
        );
        assert_eq!(range, UidRange { min: 500, max: 60_000 });
    }

    #[test]
    fn login_defs_defaults_when_absent() {
        assert_eq!(UidRange::parse_login_defs(""), UidRange::default());
        assert_eq!(
            UidRange::parse_login_defs("GID_MIN 1000\n"),
            UidRange::default()
        );
    }

    #[test]
    fn login_defs_ignores_trailing_comments() {
        let range = UidRange::parse_login_defs("UID_MIN 2000 # start here\n");
        assert_eq!(range.min, 2000);
    }

    #[test]
    fn root_is_never_inside_the_range() {
        // Local policy does not get to make root loginable from a display
        // manager documented as never offering it.
        assert!(!UidRange::parse_login_defs("UID_MIN 0\n").contains(0));
        assert!(!UidRange::default().contains(0));
    }

    #[test]
    fn login_defs_falls_back_on_nonsense() {
        // A malformed value must not make the machine unloginable.
        assert_eq!(
            UidRange::parse_login_defs("UID_MIN abc\n").min,
            FALLBACK_UID_MIN
        );
        // An inverted range would exclude every user.
        assert_eq!(
            UidRange::parse_login_defs("UID_MIN 5000\nUID_MAX 100\n"),
            UidRange::default()
        );
    }

    #[test]
    fn uid_range_is_inclusive() {
        let range = UidRange { min: 1000, max: 2000 };
        assert!(!range.contains(999));
        assert!(range.contains(1000));
        assert!(range.contains(2000));
        assert!(!range.contains(2001));
        // nobody must never be offered.
        assert!(!UidRange::default().contains(65534));
        // root must never be offered.
        assert!(!UidRange::default().contains(0));
    }

    #[test]
    fn recognises_nologin_shells() {
        assert!(is_login_shell(Path::new("/bin/bash")));
        assert!(is_login_shell(Path::new("/usr/bin/zsh")));
        assert!(is_login_shell(Path::new("/bin/fish")));
        assert!(!is_login_shell(Path::new("/usr/sbin/nologin")));
        assert!(!is_login_shell(Path::new("/sbin/nologin")));
        assert!(!is_login_shell(Path::new("/bin/false")));
        // An empty shell means the system default, which is loginable.
        assert!(is_login_shell(Path::new("")));
    }

    #[test]
    fn takes_first_gecos_field() {
        assert_eq!(display_name_from_gecos("Joseph Quinn,,,"), "Joseph Quinn");
        assert_eq!(display_name_from_gecos(""), "");
        assert_eq!(display_name_from_gecos("  Spaced  ,x"), "Spaced");
    }

    #[test]
    fn no_avatar_yields_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(avatar_for("nobody-here", dir.path()), "");
    }

    #[test]
    fn finds_dotface_avatar() {
        let dir = tempfile::tempdir().unwrap();
        let face = dir.path().join(".face");
        std::fs::write(&face, b"png").unwrap();
        assert_eq!(avatar_for("someone", dir.path()), face.to_str().unwrap());
    }

    #[test]
    fn last_sessions_round_trip() {
        let mut store = LastSessions::default();
        store.set("joseph", "hyprland.desktop");
        store.set("ada", "sway.desktop");

        let reloaded = LastSessions::parse(&store.to_text());
        assert_eq!(reloaded.get("joseph"), Some("hyprland.desktop"));
        assert_eq!(reloaded.get("ada"), Some("sway.desktop"));
        assert_eq!(reloaded.get("nobody"), None);
    }

    #[test]
    fn last_sessions_skips_malformed_lines() {
        // One bad line must not lose the other users' preferences.
        let store = LastSessions::parse("joseph\thyprland.desktop\ngarbage\n\tno-user\nada\t\n");
        assert_eq!(store.get("joseph"), Some("hyprland.desktop"));
        assert_eq!(store.get("ada"), None);
    }

    #[test]
    fn last_sessions_output_is_sorted() {
        let mut store = LastSessions::default();
        store.set("zoe", "a.desktop");
        store.set("ada", "b.desktop");
        assert_eq!(store.to_text(), "ada\tb.desktop\nzoe\ta.desktop\n");
    }

    #[test]
    fn missing_store_is_empty_not_an_error() {
        let store = LastSessions::load(Path::new("/nonexistent/wdm-last-session"));
        assert_eq!(store.get("anyone"), None);
    }

    #[test]
    fn saves_and_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("last-session");

        let mut store = LastSessions::default();
        store.set("joseph", "hyprland.desktop");
        store.save(&path).unwrap();

        assert_eq!(
            LastSessions::load(&path).get("joseph"),
            Some("hyprland.desktop")
        );
        // The temporary file must not be left behind.
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn discover_excludes_system_accounts() {
        // Runs against the real passwd database: assert the invariants that
        // must hold on any machine rather than expecting specific users.
        let range = UidRange::from_system();
        for user in discover(range, &LastSessions::default()) {
            assert_ne!(user.name, "root");
            assert_ne!(user.name, "nobody");
            assert!(!user.name.is_empty());
        }
    }
}
