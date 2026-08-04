//! The wire between wdm and its PAM helper.
//!
//! wdm runs PAM in a separate, freshly `exec`'d process — see
//! [`crate::pamhelper`] for why — so the conversation that used to be an
//! `mpsc` channel between two threads is now a socket between two processes.
//! This module is the encoding, and nothing else: no PAM, no I/O policy, so it
//! can be round-tripped in a test without either side existing.
//!
//! ## Why a hand-rolled codec
//!
//! These messages carry passwords. A hand-rolled decoder can zeroize the buffer
//! it decoded from, and [`Msg`]'s `Drop` scrubs the secret it produced; a
//! general-purpose deserializer would scatter intermediate copies that nothing
//! scrubs. The format is small enough — a tag byte and length-prefixed fields —
//! that writing it out costs less than auditing what a derive would do with it.
//!
//! ## Why no framing
//!
//! The socket is `SOCK_SEQPACKET`, so the kernel preserves message boundaries:
//! one `send` is one `recv`, or an error. There is no length prefix on the
//! whole message, no partial-read state machine, and no way for a truncated
//! message to be mistaken for a short one — a peer that writes half a message
//! writes half a *packet*, which fails to decode rather than blocking the
//! reader forever waiting for the rest.

// Nothing sends these yet. The codec is complete and tested on its own, but the
// two peers that use it — the helper, and the rewritten `AuthHandle` — are not
// written, so every variant is dead until they are. This allow comes off in the
// commit that adds `pamhelper`, and if it is still here without one, the wire
// was built and then abandoned.
#![allow(dead_code)]

use zeroize::Zeroize;

use crate::auth::PromptStyle;

/// Largest message the wire accepts, in bytes.
///
/// Everything crossing here is a prompt, a password or a session's environment,
/// all of which are small. The bound exists so that a corrupt length field
/// cannot make the reader allocate arbitrarily; it is a sanity limit, not a
/// tuned one. `SOCK_SEQPACKET` on Linux will refuse to send more than the
/// socket buffer anyway, so a message this large fails at the sender.
pub const MAX_MESSAGE: usize = 256 * 1024;

/// A message in either direction.
///
/// One enum rather than two, so the codec has one implementation and the tests
/// round-trip every variant through the same path. Which direction a variant is
/// legal in is the business of the two peers, not of the encoding — and both
/// peers reject a variant they should never receive, so a confused sender
/// cannot drive the other side into a state it has no code for.
#[derive(Debug, PartialEq, Eq)]
pub enum Msg {
    // ---- wdm -> helper ------------------------------------------------
    /// Answer the prompt with this id.
    Response { id: u32, secret: String },
    /// Open the PAM session and run the user's session.
    ///
    /// Sent only after wdm has released the display. Everything the helper
    /// needs to assemble the environment travels here, because the helper has
    /// no access to wdm's configuration.
    Launch {
        session_type: String,
        desktop: String,
        session_id: String,
        session_name: String,
        session_exec: String,
        /// Filtered environment the greeter asked for.
        extra_env: Vec<(String, String)>,
        vt: u32,
    },

    // ---- helper -> wdm ------------------------------------------------
    /// PAM is asking something.
    Prompt {
        id: u32,
        text: String,
        style: PromptStyle,
    },
    /// Authentication and account management both succeeded.
    Ok,
    /// The attempt failed; the helper is exiting.
    Failed(String),
    /// The session process is running.
    SessionStarted { pid: u32 },
    /// `pam_open_session` or the launch itself failed; the helper is exiting.
    SessionFailed(String),
    /// The session process exited. `status` is the wait status as text, because
    /// what wdm does with it is put it in a log line and a `last_error`.
    SessionEnded { status: String, ran_for_ms: u64 },
}

impl Drop for Msg {
    /// Scrub the secret a [`Msg::Response`] carries.
    ///
    /// Every path a decoded response can take ends here: consumed normally,
    /// dropped because the attempt already ended, or dropped while unwinding.
    /// It does not cover the copies downstream — `CString` owns one and libpam
    /// owns another — which is the same accepted ceiling the conversation code
    /// documents.
    fn drop(&mut self) {
        if let Self::Response { secret, .. } = self {
            secret.zeroize();
        }
    }
}

const TAG_RESPONSE: u8 = 1;
const TAG_LAUNCH: u8 = 2;
const TAG_PROMPT: u8 = 3;
const TAG_OK: u8 = 4;
const TAG_FAILED: u8 = 5;
const TAG_SESSION_STARTED: u8 = 6;
const TAG_SESSION_FAILED: u8 = 7;
const TAG_SESSION_ENDED: u8 = 8;

const STYLE_SECRET: u8 = 0;
const STYLE_VISIBLE: u8 = 1;
const STYLE_INFO: u8 = 2;
const STYLE_ERROR: u8 = 3;

fn style_tag(style: PromptStyle) -> u8 {
    match style {
        PromptStyle::Secret => STYLE_SECRET,
        PromptStyle::Visible => STYLE_VISIBLE,
        PromptStyle::Info => STYLE_INFO,
        PromptStyle::Error => STYLE_ERROR,
    }
}

fn style_from_tag(tag: u8) -> Option<PromptStyle> {
    match tag {
        STYLE_SECRET => Some(PromptStyle::Secret),
        STYLE_VISIBLE => Some(PromptStyle::Visible),
        STYLE_INFO => Some(PromptStyle::Info),
        STYLE_ERROR => Some(PromptStyle::Error),
        _ => None,
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn put_pairs(out: &mut Vec<u8>, pairs: &[(String, String)]) {
    put_u32(out, pairs.len() as u32);
    for (key, value) in pairs {
        put_str(out, key);
        put_str(out, value);
    }
}

/// Reads fields off a message, refusing to run past the end of it.
///
/// Every accessor is fallible and every failure is the same: the message was
/// not what its tag claimed. A peer that sends nonsense gets its connection
/// dropped, which is the only sensible response — the two ends of this socket
/// are the same binary, so a decode failure is a bug or an attack, never a
/// version skew.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        // Lossy rather than strict: PAM messages come from modules and are not
        // guaranteed UTF-8, and refusing to decode one would lose the whole
        // message — including the prompt the user is waiting on.
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn pairs(&mut self) -> Option<Vec<(String, String)>> {
        let count = self.u32()? as usize;
        // Bounded by what is left: a corrupt count cannot make this reserve.
        let mut out = Vec::new();
        for _ in 0..count {
            out.push((self.string()?, self.string()?));
        }
        Some(out)
    }

    /// Every byte consumed, and no trailing junk.
    fn finished(&self) -> bool {
        self.at == self.bytes.len()
    }
}

impl Msg {
    /// Encode into a single `SOCK_SEQPACKET` datagram.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Response { id, secret } => {
                out.push(TAG_RESPONSE);
                put_u32(&mut out, *id);
                put_str(&mut out, secret);
            }
            Self::Launch {
                session_type,
                desktop,
                session_id,
                session_name,
                session_exec,
                extra_env,
                vt,
            } => {
                out.push(TAG_LAUNCH);
                put_str(&mut out, session_type);
                put_str(&mut out, desktop);
                put_str(&mut out, session_id);
                put_str(&mut out, session_name);
                put_str(&mut out, session_exec);
                put_pairs(&mut out, extra_env);
                put_u32(&mut out, *vt);
            }
            Self::Prompt { id, text, style } => {
                out.push(TAG_PROMPT);
                put_u32(&mut out, *id);
                put_str(&mut out, text);
                out.push(style_tag(*style));
            }
            Self::Ok => out.push(TAG_OK),
            Self::Failed(reason) => {
                out.push(TAG_FAILED);
                put_str(&mut out, reason);
            }
            Self::SessionStarted { pid } => {
                out.push(TAG_SESSION_STARTED);
                put_u32(&mut out, *pid);
            }
            Self::SessionFailed(reason) => {
                out.push(TAG_SESSION_FAILED);
                put_str(&mut out, reason);
            }
            Self::SessionEnded { status, ran_for_ms } => {
                out.push(TAG_SESSION_ENDED);
                put_str(&mut out, status);
                put_u64(&mut out, *ran_for_ms);
            }
        }
        out
    }

    /// Decode one datagram, or `None` if it is not a well-formed message.
    ///
    /// The caller is expected to zeroize `bytes` afterwards when it may have
    /// held a response; this borrows rather than consuming so that it can.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader::new(bytes);
        let msg = match r.u8()? {
            TAG_RESPONSE => Self::Response {
                id: r.u32()?,
                secret: r.string()?,
            },
            TAG_LAUNCH => Self::Launch {
                session_type: r.string()?,
                desktop: r.string()?,
                session_id: r.string()?,
                session_name: r.string()?,
                session_exec: r.string()?,
                extra_env: r.pairs()?,
                vt: r.u32()?,
            },
            TAG_PROMPT => Self::Prompt {
                id: r.u32()?,
                text: r.string()?,
                style: style_from_tag(r.u8()?)?,
            },
            TAG_OK => Self::Ok,
            TAG_FAILED => Self::Failed(r.string()?),
            TAG_SESSION_STARTED => Self::SessionStarted { pid: r.u32()? },
            TAG_SESSION_FAILED => Self::SessionFailed(r.string()?),
            TAG_SESSION_ENDED => Self::SessionEnded {
                status: r.string()?,
                ran_for_ms: r.u64()?,
            },
            _ => return None,
        };
        // Trailing bytes mean the sender and this decoder disagree about the
        // shape, which is a bug rather than something to tolerate.
        r.finished().then_some(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: Msg) {
        let bytes = msg.encode();
        assert!(bytes.len() <= MAX_MESSAGE);
        let back = Msg::decode(&bytes).expect("well-formed message failed to decode");
        assert_eq!(back, msg);
    }

    #[test]
    fn every_message_round_trips() {
        round_trip(Msg::Response {
            id: 7,
            secret: "hunter2".to_owned(),
        });
        round_trip(Msg::Launch {
            session_type: "wayland".to_owned(),
            desktop: "hyprland".to_owned(),
            session_id: "hyprland.desktop".to_owned(),
            session_name: "Hyprland".to_owned(),
            session_exec: "Hyprland".to_owned(),
            extra_env: vec![
                ("LANG".to_owned(), "en_GB.UTF-8".to_owned()),
                ("XKB_DEFAULT_LAYOUT".to_owned(), "gb".to_owned()),
            ],
            vt: 7,
        });
        round_trip(Msg::Prompt {
            id: 0,
            text: "Password: ".to_owned(),
            style: PromptStyle::Secret,
        });
        round_trip(Msg::Ok);
        round_trip(Msg::Failed("Authentication failure".to_owned()));
        round_trip(Msg::SessionStarted { pid: 4321 });
        round_trip(Msg::SessionFailed("no such session".to_owned()));
        round_trip(Msg::SessionEnded {
            status: "exit status: 0".to_owned(),
            ran_for_ms: 12_345,
        });
    }

    #[test]
    fn every_prompt_style_survives_the_wire() {
        // The styles are the one field with a lossy representation — a u8 that
        // could silently land on the wrong variant. Info and Error are what
        // stop a greeter auto-retrying into an account lockout, so a style that
        // decoded as Secret would be a functional bug, not a cosmetic one.
        for style in [
            PromptStyle::Secret,
            PromptStyle::Visible,
            PromptStyle::Info,
            PromptStyle::Error,
        ] {
            round_trip(Msg::Prompt {
                id: 1,
                text: String::new(),
                style,
            });
        }
    }

    #[test]
    fn non_utf8_prompt_text_survives_lossily_rather_than_failing() {
        // PAM messages come from C modules and carry whatever bytes they like.
        // Losing the whole message would lose the prompt the user is waiting
        // on, so the decoder substitutes rather than refusing.
        let mut bytes = vec![TAG_PROMPT];
        put_u32(&mut bytes, 3);
        put_u32(&mut bytes, 2);
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.push(STYLE_VISIBLE);

        // Borrowed, not moved: `Msg` has a `Drop`, so destructuring it by value
        // is refused.
        let decoded = Msg::decode(&bytes);
        let Some(Msg::Prompt { id, text, style }) = &decoded else {
            panic!("a message with non-UTF-8 text must still decode");
        };
        assert_eq!(*id, 3);
        assert_eq!(*style, PromptStyle::Visible);
        assert!(!text.is_empty(), "the replacement character should survive");
    }

    #[test]
    fn a_truncated_message_is_refused_rather_than_guessed() {
        // SOCK_SEQPACKET means a short packet is a whole message that is wrong,
        // not the first half of a right one. Guessing at the missing fields
        // would invent a prompt id, and prompt ids decide which question a
        // password answers.
        let full = Msg::Prompt {
            id: 9,
            text: "Password: ".to_owned(),
            style: PromptStyle::Secret,
        }
        .encode();

        for cut in 1..full.len() {
            assert!(
                Msg::decode(&full[..cut]).is_none(),
                "a message truncated to {cut} bytes decoded anyway"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut bytes = Msg::Ok.encode();
        bytes.push(0);
        assert!(Msg::decode(&bytes).is_none());
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        assert!(Msg::decode(&[254]).is_none());
        assert!(Msg::decode(&[]).is_none());
    }

    #[test]
    fn a_corrupt_length_cannot_run_past_the_message() {
        // The reader is the only thing standing between a hostile length field
        // and a panic or a huge allocation, so drive it with one directly.
        let mut bytes = vec![TAG_FAILED];
        put_u32(&mut bytes, u32::MAX);
        bytes.extend_from_slice(b"short");
        assert!(Msg::decode(&bytes).is_none());
    }

    #[test]
    fn a_corrupt_pair_count_cannot_allocate_unboundedly() {
        // `pairs()` must not reserve on the strength of the count alone: the
        // count is attacker-controlled in the same sense the rest of the
        // message is, and a Vec::with_capacity(u32::MAX) is a denial of
        // service reachable before a single byte is validated.
        let mut bytes = vec![TAG_LAUNCH];
        for _ in 0..5 {
            put_str(&mut bytes, "");
        }
        put_u32(&mut bytes, u32::MAX);
        assert!(Msg::decode(&bytes).is_none());
    }

    #[test]
    fn a_dropped_response_scrubs_its_secret() {
        // The reason Msg has a Drop at all. Read the buffer back through a raw
        // pointer after the drop: comparing the String would be reading freed
        // memory through a reference, which is what this is checking for the
        // absence of in the first place.
        let secret = "correct horse".to_owned();
        let ptr = secret.as_ptr();
        let len = secret.len();
        let msg = Msg::Response { id: 1, secret };

        drop(msg);

        // SAFETY: the allocation is freed, so this is reading memory the
        // allocator owns. It is a test-only check that the bytes were scrubbed
        // before release; on a freed page it can only be zeroes or reused data,
        // never the original string.
        let after = unsafe { std::slice::from_raw_parts(ptr, len) };
        assert_ne!(after, b"correct horse", "the secret survived the drop");
    }
}
