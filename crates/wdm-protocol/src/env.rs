//! The `start_session` `env` argument: its codec, and what survives it.
//!
//! The argument is a `wl_array` of NUL-separated `KEY=VALUE` entries, matching
//! the layout of `environ(7)`. Both sides of the protocol need this, so it
//! lives here rather than being reimplemented in the compositor and in every
//! greeter — the allowlist in [`is_honoured`] included, because an entry that
//! fails it is dropped in silence and a greeter that cannot ask would have no
//! way to find out but to ship.

/// Why an `env` array could not be decoded.
///
/// The compositor raises `invalid_env` on the greeter's object for any of
/// these. A greeter is untrusted, so the array is validated rather than
/// trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvError {
    /// An entry was not valid UTF-8.
    NotUtf8,
    /// An entry had no `=`, so it names no variable.
    NoSeparator,
    /// An entry had an empty name, which `putenv(3)` rejects.
    EmptyName,
    /// A variable name contained `=` or a NUL, which cannot round-trip.
    InvalidName,
}

impl core::fmt::Display for EnvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::NotUtf8 => "environment entry is not valid UTF-8",
            Self::NoSeparator => "environment entry has no '=' separator",
            Self::EmptyName => "environment entry has an empty variable name",
            Self::InvalidName => "environment variable name is not valid",
        };
        f.write_str(s)
    }
}

impl core::error::Error for EnvError {}

/// Encode `KEY=VALUE` pairs into the wire format.
///
/// Entries are emitted in iteration order and NUL-terminated, including the
/// last one, so a decoder needs no special case for the tail.
pub fn encode<I, K, V>(vars: I) -> Vec<u8>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = Vec::new();
    for (key, value) in vars {
        out.extend_from_slice(key.as_ref().as_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_ref().as_bytes());
        out.push(0);
    }
    out
}

/// Decode the wire format into `(name, value)` pairs.
///
/// A trailing NUL does not produce an empty final entry, and an entirely empty
/// array decodes to no entries. Duplicate names are returned as-is; deciding
/// which wins is the caller's business.
pub fn decode(bytes: &[u8]) -> Result<Vec<(String, String)>, EnvError> {
    let mut out = Vec::new();

    for entry in bytes.split(|&b| b == 0) {
        // split() yields a trailing empty slice for the final NUL, and the
        // whole-empty-input case yields one empty slice. Neither is an entry.
        if entry.is_empty() {
            continue;
        }

        let entry = core::str::from_utf8(entry).map_err(|_| EnvError::NotUtf8)?;
        let (name, value) = entry.split_once('=').ok_or(EnvError::NoSeparator)?;

        if name.is_empty() {
            return Err(EnvError::EmptyName);
        }
        // A name may not contain '=' by construction of split_once, but reject
        // anything that is not a plausible shell variable name so a greeter
        // cannot smuggle oddities into the session's environment.
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(EnvError::InvalidName);
        }

        out.push((name.to_owned(), value.to_owned()));
    }

    Ok(out)
}

/// Whether the compositor will honour a variable a greeter contributes, or drop
/// it on the floor.
///
/// An allowlist, not a denylist, and the same one wdm applies when it builds the
/// session's environment. The greeter is unprivileged, untrusted code, and the
/// session it is contributing to runs as the authenticated user through a login
/// shell — so a variable it controls is a variable it can use to run code as
/// that user. `LD_PRELOAD` and `LD_LIBRARY_PATH` are the obvious ones, but
/// `PATH`, `SHELL`, `IFS`, `BASH_ENV` and `ENV` are equally good, and
/// enumerating them is a game one eventually loses. Only presentation variables
/// are allowed through, which is all a greeter has any business setting.
///
/// Values are checked as well as keys, and for the same reason the key list is
/// short. glibc treats a locale name containing a slash as a path and loads the
/// locale from there — it refuses that only for setuid binaries, which a user
/// session is not — and libxkbcommon honours `XKB_CONFIG_ROOT` as a data root.
/// Either would let the greeter point the authenticated user's session at files
/// it controls. A locale or layout name is also short, so anything over 64
/// *bytes* is not one and is a plausible attempt at something else.
///
/// Here rather than only in wdm because a rejection is silent: an entry that
/// fails this is dropped, not an `invalid_env` error, so a greeter with no way
/// to ask would find out by shipping. It is exactly the reimplementation this
/// module exists to prevent, and like [`GREETER_GAVE_UP_EXIT`] it is outside
/// both feature gates — the greeter pre-checks with it and the compositor
/// enforces with it, so gating it on either would put the two halves of one
/// agreement behind different switches.
///
/// Advisory, not authoritative: wdm applies the greeter's contribution *before*
/// its own variables, so a key that passes here can still be overwritten by a
/// fact about the seat. This answers "will it be dropped", not "will it win".
///
/// [`GREETER_GAVE_UP_EXIT`]: crate::GREETER_GAVE_UP_EXIT
pub fn is_honoured(key: &str, value: &str) -> bool {
    const ALLOWED: &[&str] = &["LANG", "LANGUAGE"];
    const ALLOWED_PREFIXES: &[&str] = &["LC_", "XKB_"];

    if !(ALLOWED.contains(&key) || ALLOWED_PREFIXES.iter().any(|p| key.starts_with(p))) {
        return false;
    }

    if value.contains('/') || value.contains('\0') {
        return false;
    }

    value.len() <= 64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let vars = [("LANG", "en_US.UTF-8"), ("XKB_LAYOUT", "us")];
        let decoded = decode(&encode(vars)).unwrap();
        assert_eq!(
            decoded,
            vec![
                ("LANG".to_owned(), "en_US.UTF-8".to_owned()),
                ("XKB_LAYOUT".to_owned(), "us".to_owned()),
            ]
        );
    }

    #[test]
    fn empty_array_is_no_entries() {
        assert_eq!(decode(b"").unwrap(), vec![]);
        assert_eq!(decode(b"\0").unwrap(), vec![]);
        assert!(encode::<_, &str, &str>([]).is_empty());
    }

    #[test]
    fn empty_value_is_allowed() {
        // Unsetting by assigning empty is legitimate and must not be an error.
        assert_eq!(
            decode(b"LANG=\0").unwrap(),
            vec![("LANG".to_owned(), String::new())]
        );
    }

    #[test]
    fn value_may_contain_equals() {
        assert_eq!(
            decode(b"A=b=c\0").unwrap(),
            vec![("A".to_owned(), "b=c".to_owned())]
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode(b"NOEQUALS\0"), Err(EnvError::NoSeparator));
        assert_eq!(decode(b"=value\0"), Err(EnvError::EmptyName));
        assert_eq!(decode(b"BAD-NAME=v\0"), Err(EnvError::InvalidName));
        assert_eq!(decode(b"HAS SPACE=v\0"), Err(EnvError::InvalidName));
        assert_eq!(decode(&[b'A', b'=', 0xff, 0]), Err(EnvError::NotUtf8));
    }

    #[test]
    fn only_presentation_variables_are_honoured() {
        // The greeter is unprivileged and untrusted, but the session runs as
        // the authenticated user through a login shell. Any of these would be
        // arbitrary code execution as that user.
        for key in [
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
        ] {
            assert!(!is_honoured(key, "x"), "{key} must not be greeter-settable");
        }
    }

    #[test]
    fn allowlisted_keys_still_reject_path_values() {
        // glibc loads a locale from a path if the name contains a slash, and
        // libxkbcommon honours XKB_CONFIG_ROOT as a data root. Allowing the key
        // is not the same as allowing any value for it.
        assert!(is_honoured("LANG", "de_DE.UTF-8"));
        assert!(is_honoured("LC_ALL", "en_GB.UTF-8"));
        assert!(is_honoured("XKB_DEFAULT_LAYOUT", "de"));

        assert!(!is_honoured("LC_ALL", "/tmp/evil/locale"));
        assert!(!is_honoured("LANG", "../../tmp/evil"));
        assert!(!is_honoured("XKB_CONFIG_ROOT", "/tmp/evil"));
        assert!(!is_honoured("LANG", &"a".repeat(4096)));
    }

    #[test]
    fn a_nul_in_a_value_is_refused() {
        // It cannot round-trip through the wire format above — `decode` would
        // split the entry in two — so a value carrying one is either a bug or
        // an attempt to smuggle a second variable past the filter.
        assert!(!is_honoured("LANG", "de_DE.UTF-8\0LD_PRELOAD=x"));
    }

    #[test]
    fn the_length_limit_is_64_bytes_not_64_characters() {
        // The XML promises "values longer than 64 bytes" are rejected, so the
        // boundary is part of the contract a greeter is written against, and
        // `str::len` returning bytes rather than chars is the sort of thing a
        // refactor to `chars().count()` silently reverses.
        assert!(is_honoured("LANG", &"a".repeat(64)));
        assert!(!is_honoured("LANG", &"a".repeat(65)));
        assert!(is_honoured("LANGUAGE", &"a".repeat(64)));
        assert!(!is_honoured("LANGUAGE", &"a".repeat(65)));

        // 33 two-byte characters: under the limit counted as characters, over it
        // counted as bytes. It is bytes that decide how much of the session's
        // environment a greeter controls, so this must be refused.
        let multibyte = "é".repeat(33);
        assert_eq!(multibyte.len(), 66);
        assert!(!is_honoured("LC_ALL", &multibyte));
        // And 32 of them, at 64 bytes, are still allowed.
        assert!(is_honoured("LC_ALL", &"é".repeat(32)));
    }
}
