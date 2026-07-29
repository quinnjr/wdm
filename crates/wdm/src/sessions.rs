//! Discovery of launchable sessions from XDG desktop entries.
//!
//! Wayland sessions come from `/usr/share/wayland-sessions` and X11 sessions
//! from `/usr/share/xsessions`. wdm launches an X11 session like any other
//! command; bringing up an X server is the session's own business.

use std::path::{Path, PathBuf};

use freedesktop_desktop_entry::DesktopEntry;

/// Directories scanned for Wayland session entries, in precedence order.
pub const WAYLAND_DIRS: &[&str] = &[
    "/usr/local/share/wayland-sessions",
    "/usr/share/wayland-sessions",
];

/// Directories scanned for X11 session entries, in precedence order.
pub const X11_DIRS: &[&str] = &["/usr/local/share/xsessions", "/usr/share/xsessions"];

/// How a session expects to be displayed.
///
/// Mirrors the protocol's `session_type` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Wayland,
    X11,
}

/// A session wdm is willing to launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Stable identifier: the desktop file name, e.g. `hyprland.desktop`.
    pub id: String,
    /// Human readable name, localised where the entry provides translations.
    pub name: String,
    /// Command line with XDG field codes removed, ready to be split and exec'd.
    pub exec: String,
    pub session_type: SessionType,
    /// Where the entry was found, for diagnostics.
    pub path: PathBuf,
}

/// Scan the standard directories for sessions.
///
/// Entries earlier in the search path win, so a session dropped into
/// `/usr/local/share` overrides the distribution's copy of the same id.
/// Unreadable directories and malformed entries are logged and skipped: one bad
/// desktop file must not deprive the machine of every other session.
pub fn discover(locales: &[String]) -> Vec<Session> {
    let mut found: Vec<Session> = Vec::new();

    let dirs = WAYLAND_DIRS
        .iter()
        .map(|d| (Path::new(*d), SessionType::Wayland))
        .chain(X11_DIRS.iter().map(|d| (Path::new(*d), SessionType::X11)));

    for (dir, session_type) in dirs {
        for session in scan_dir(dir, session_type, locales) {
            if found.iter().any(|s| s.id == session.id) {
                log::debug!(
                    "ignoring {}, id {} already provided by an earlier directory",
                    session.path.display(),
                    session.id
                );
                continue;
            }
            found.push(session);
        }
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn scan_dir(dir: &Path, session_type: SessionType, locales: &[String]) -> Vec<Session> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::warn!("scanning {}: {e}", dir.display());
            return Vec::new();
        }
    };

    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "desktop") {
            continue;
        }
        match load(&path, session_type, locales) {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            Err(e) => log::warn!("skipping {}: {e}", path.display()),
        }
    }

    // read_dir order is filesystem order, which is not stable across machines.
    sessions.sort_by(|a, b| a.id.cmp(&b.id));
    sessions
}

fn load(
    path: &Path,
    session_type: SessionType,
    locales: &[String],
) -> Result<Option<Session>, Box<dyn std::error::Error>> {
    let entry = DesktopEntry::from_path(path.to_owned(), Some(locales))?;

    if entry.hidden() || entry.no_display() {
        return Ok(None);
    }

    // TryExec names a binary whose absence means the session is not installed,
    // which is how a package leaves its entry in place while being uninstalled.
    if let Some(try_exec) = entry.try_exec()
        && !binary_exists(try_exec)
    {
        log::debug!(
            "skipping {}: TryExec {try_exec} not found",
            path.display()
        );
        return Ok(None);
    }

    let Some(exec) = entry.exec() else {
        return Err("entry has no Exec key".into());
    };
    let exec = strip_field_codes(exec);
    if exec.trim().is_empty() {
        return Err("Exec key is empty".into());
    }

    let id = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("desktop file name is not valid UTF-8")?
        .to_owned();

    let name = entry
        .name(locales)
        .map(|n| n.into_owned())
        // A session with no Name is still launchable; showing the file name
        // beats hiding a working session from the user.
        .unwrap_or_else(|| id.clone());

    Ok(Some(Session {
        id,
        name,
        exec,
        session_type,
        path: path.to_owned(),
    }))
}

fn binary_exists(try_exec: &str) -> bool {
    let path = Path::new(try_exec);
    if path.is_absolute() {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(try_exec).is_file())
        })
        .unwrap_or(false)
}

/// Remove XDG field codes from an `Exec` line.
///
/// Session entries have no file or URL to substitute, so every code expands to
/// nothing. `%%` is an escaped literal percent and must survive as one, which
/// is why this cannot be a naive search-and-replace.
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();

    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // Escaped percent: emit one.
            Some('%') => out.push('%'),
            // Known codes expand to nothing for a session.
            Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'i' | 'c' | 'k' | 'v' | 'm') => {}
            // An unrecognised code is deprecated or a typo. Dropping it matches
            // what other launchers do and keeps the command runnable.
            Some(other) => log::debug!("ignoring unknown field code %{other} in Exec"),
            // Trailing bare percent; nothing to do.
            None => {}
        }
    }

    // Codes sit between arguments, so removing them leaves double spaces that
    // would become empty argv entries once split.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_field_codes() {
        assert_eq!(strip_field_codes("Hyprland"), "Hyprland");
        assert_eq!(strip_field_codes("sway %U"), "sway");
        assert_eq!(strip_field_codes("foo %f bar %U baz"), "foo bar baz");
        assert_eq!(strip_field_codes("gnome-session --mode %i"), "gnome-session --mode");
    }

    #[test]
    fn keeps_escaped_percent() {
        assert_eq!(strip_field_codes("cmd 100%%"), "cmd 100%");
        // %%f is a literal percent followed by a literal f, not a field code.
        assert_eq!(strip_field_codes("cmd %%f"), "cmd %f");
    }

    #[test]
    fn tolerates_trailing_percent() {
        assert_eq!(strip_field_codes("cmd %"), "cmd");
    }

    #[test]
    fn drops_unknown_codes() {
        assert_eq!(strip_field_codes("cmd %z arg"), "cmd arg");
    }

    #[test]
    fn collapses_whitespace_left_behind() {
        // Naive removal would leave "cmd  arg", which splits into an empty argv
        // entry and makes exec fail with a confusing error.
        assert_eq!(strip_field_codes("cmd %f arg"), "cmd arg");
        assert!(!strip_field_codes("cmd %f arg").contains("  "));
    }

    #[test]
    fn missing_directories_are_not_an_error() {
        assert!(scan_dir(
            Path::new("/nonexistent/wdm-test-sessions"),
            SessionType::Wayland,
            &[]
        )
        .is_empty());
    }

    #[test]
    fn loads_a_desktop_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("sway.desktop"),
            "[Desktop Entry]\nName=Sway\nComment=tiling\nExec=sway %U\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=nope\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("nodisplay.desktop"),
            "[Desktop Entry]\nName=NoDisplay\nExec=nope\nNoDisplay=true\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("uninstalled.desktop"),
            "[Desktop Entry]\nName=Gone\nExec=gone\nTryExec=/nonexistent/gone\n",
        )
        .unwrap();
        // Not a desktop file; must be ignored rather than parsed.
        std::fs::write(dir.path().join("README"), "not an entry").unwrap();

        let found = scan_dir(dir.path(), SessionType::Wayland, &[]);
        assert_eq!(found.len(), 1, "found: {found:?}");
        assert_eq!(found[0].id, "sway.desktop");
        assert_eq!(found[0].name, "Sway");
        assert_eq!(found[0].exec, "sway");
        assert_eq!(found[0].session_type, SessionType::Wayland);
    }

    #[test]
    fn entry_without_name_falls_back_to_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nameless.desktop"),
            "[Desktop Entry]\nExec=weston\n",
        )
        .unwrap();
        let found = scan_dir(dir.path(), SessionType::Wayland, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "nameless.desktop");
    }

    #[test]
    fn entry_without_exec_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("broken.desktop"),
            "[Desktop Entry]\nName=Broken\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("good.desktop"),
            "[Desktop Entry]\nName=Good\nExec=river\n",
        )
        .unwrap();
        // One unusable entry must not hide the other.
        let found = scan_dir(dir.path(), SessionType::Wayland, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Good");
    }

    #[test]
    fn localised_name_is_preferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("de.desktop"),
            "[Desktop Entry]\nName=Desktop\nName[de]=Arbeitsfläche\nExec=de\n",
        )
        .unwrap();
        let found = scan_dir(dir.path(), SessionType::Wayland, &["de".to_owned()]);
        assert_eq!(found[0].name, "Arbeitsfläche");
    }
}
