//! Codec for the `start_session` `env` argument.
//!
//! The argument is a `wl_array` of NUL-separated `KEY=VALUE` entries, matching
//! the layout of `environ(7)`. Both sides of the protocol need this, so it
//! lives here rather than being reimplemented in the compositor and in every
//! greeter.

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
}
