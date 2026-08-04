//! The wire between wdm and its PAM helper.
//!
//! wdm runs PAM in a separate, freshly `exec`'d process ([`crate::pamhelper`]),
//! so the conversation that is today an `mpsc` channel between two threads
//! becomes a socket between two processes. This module is the encoding, and
//! nothing else: no PAM, no I/O policy, so it can be round-tripped in a test
//! without either side existing.
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

use zeroize::Zeroize;

use crate::auth::PromptStyle;

/// Largest message the wire accepts, in bytes.
///
/// Everything crossing here is a prompt, a password or a session's environment,
/// all of which are small. It is a sanity limit, not a tuned one:
/// `SOCK_SEQPACKET` on Linux will refuse to send more than the socket buffer
/// anyway, so a message this large fails at the sender.
///
/// This bounds the *datagram*. What stops a corrupt length field inside one from
/// making the decoder allocate arbitrarily is [`Reader`], which resolves every
/// field against the bytes actually present and fails rather than reserving.
/// The two are separate guards and neither implies the other — this constant is
/// `pub`, so it is checked in [`Msg::decode`] rather than left as documentation
/// a caller could reasonably take for enforcement.
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
    /// Begin an attempt. Always the first message on the socket.
    ///
    /// Everything here would be argv on a helper that took arguments, and is
    /// not, because `--pam-helper` deliberately accepts none: a mode reachable
    /// only through a file descriptor cannot be reached by accident from a
    /// shell, and a username on a command line is world-readable in `/proc`.
    /// The fields are exactly what [`crate::auth`] passes to its thread —
    /// the user, the tty for `PAM_TTY`, and the seat description pam_systemd
    /// needs before `pam_open_session`.
    Start {
        username: String,
        tty: String,
        seat: String,
        vtnr: u32,
        /// Provisional `wayland` or `x11`; the [`Msg::Launch`] that follows
        /// carries the greeter's actual choice and supersedes it.
        session_type: String,
        /// Desktop id without the `.desktop` suffix; empty until chosen.
        desktop: String,
    },
    /// Answer the prompt with this id.
    Response { id: u32, secret: String },
    /// Open the PAM session and run the user's session.
    ///
    /// Sent only after wdm has released the display — the DRM device, the
    /// renderer, libinput and the libseat session are all gone by the time this
    /// crosses. That ordering is the entire point of the helper: a module that
    /// `fork`s and `exit`s inside `pam_sm_open_session` has no live EGL context
    /// to inherit, because there no longer is one anywhere.
    ///
    /// Everything the helper needs to assemble the environment travels here,
    /// because the helper has no access to wdm's configuration. `session_type`
    /// and `desktop` are redundant with `session_id` — they are what
    /// `Session::xdg_session_type` and `xdg_session_desktop` derive — and are
    /// sent anyway because they are what goes into PAM's *own* environment
    /// before `open_session`, which is a different thing from what goes into the
    /// child's.
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
    /// The session process is running, as a child of the helper.
    ///
    /// Implies `pam_open_session` succeeded: nothing is forked until it has.
    /// The pid is the helper's child, not wdm's — wdm cannot `wait` on it, and
    /// the message exists so a log line can name it and so a hung launch is
    /// distinguishable from one that never started.
    SessionStarted { pid: u32 },
    /// `pam_open_session`, the environment assembly or the fork failed; the
    /// helper is exiting.
    ///
    /// Arrives after wdm has already released the display, so there is no
    /// greeter left to tell directly. wdm records it as `last_error` and the
    /// next greeter shows it, which is the price of not opening a PAM session
    /// while holding the GPU.
    SessionFailed(String),
    /// The session process exited. `status` is the wait status as text, because
    /// what wdm does with it is put it in a log line and a `last_error`.
    ///
    /// Terminal: the helper sends this, then runs `pam_close_session` on the
    /// handle that opened the session, then exits.
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

const TAG_START: u8 = 0;
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
            Self::Start {
                username,
                tty,
                seat,
                vtnr,
                session_type,
                desktop,
            } => {
                out.push(TAG_START);
                put_str(&mut out, username);
                put_str(&mut out, tty);
                put_str(&mut out, seat);
                put_u32(&mut out, *vtnr);
                put_str(&mut out, session_type);
                put_str(&mut out, desktop);
            }
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
        // Refused before a tag is even read. Nothing legitimate is this large,
        // and the alternative is that MAX_MESSAGE is a `pub` constant carrying a
        // safety rationale that nothing on this side honours.
        if bytes.len() > MAX_MESSAGE {
            return None;
        }

        let mut r = Reader::new(bytes);
        let msg = match r.u8()? {
            TAG_START => Self::Start {
                username: r.string()?,
                tty: r.string()?,
                seat: r.string()?,
                vtnr: r.u32()?,
                session_type: r.string()?,
                desktop: r.string()?,
            },
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
        round_trip(Msg::Start {
            username: "testuser".to_owned(),
            tty: "/dev/tty7".to_owned(),
            seat: "seat0".to_owned(),
            vtnr: 7,
            session_type: "wayland".to_owned(),
            desktop: String::new(),
        });
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
        // A greeter that asked for nothing is the common case, and an empty pair
        // list is the shape a decoder is most likely to get wrong: it is the one
        // where "no bytes left" and "no entries" look the same.
        round_trip(Msg::Launch {
            session_type: "x11".to_owned(),
            desktop: "i3".to_owned(),
            session_id: "i3.desktop".to_owned(),
            session_name: "i3".to_owned(),
            session_exec: "i3".to_owned(),
            extra_env: Vec::new(),
            vt: 1,
        });
        round_trip(Msg::SessionStarted { pid: 4321 });
        // pid 0 is not a session wdm could ever be told about, but it is the
        // value a decoder that read the wrong field would most plausibly
        // produce, so the codec has to carry it faithfully rather than have the
        // test only exercise a number that happens to be non-zero.
        round_trip(Msg::SessionStarted { pid: 0 });
        round_trip(Msg::SessionFailed("no such session".to_owned()));
        round_trip(Msg::SessionEnded {
            status: "exit status: 0".to_owned(),
            ran_for_ms: 12_345,
        });
        // A session that died instantly is what `last_error` exists for, and
        // zero is where a u64 read as a u32 would still look right.
        round_trip(Msg::SessionEnded {
            status: "signal: 11 (SIGSEGV)".to_owned(),
            ran_for_ms: 0,
        });
        round_trip(Msg::SessionEnded {
            status: "exit status: 0".to_owned(),
            ran_for_ms: u64::MAX,
        });
    }

    #[test]
    fn a_truncated_launch_is_refused() {
        // Launch is the message the session is assembled from — the account it
        // runs as is not here, but the command and the environment are — and it
        // is the only one with a nested length (`extra_env`) between two
        // variable-width fields. Every prefix must fail rather than yield a
        // Launch with a plausible-looking Exec and a silently empty environment.
        let full = Msg::Launch {
            session_type: "wayland".to_owned(),
            desktop: "sway".to_owned(),
            session_id: "sway.desktop".to_owned(),
            session_name: "Sway".to_owned(),
            session_exec: "sway".to_owned(),
            extra_env: vec![("LANG".to_owned(), "en_GB.UTF-8".to_owned())],
            vt: 7,
        }
        .encode();

        for cut in 1..full.len() {
            assert!(
                Msg::decode(&full[..cut]).is_none(),
                "a Launch truncated to {cut} bytes decoded anyway"
            );
        }
    }

    #[test]
    fn the_session_lifecycle_messages_are_distinguishable() {
        // Three messages that all mean "the login is over" and mean opposite
        // things to wdm: started is not terminal, ended is the ordinary exit,
        // failed is the one that becomes a `last_error` the next greeter shows.
        // A tag collision would make one silently decode as another, and
        // SessionStarted and SessionEnded are adjacent in the table.
        let tags: Vec<u8> = [
            Msg::SessionStarted { pid: 1 },
            Msg::SessionFailed(String::new()),
            Msg::SessionEnded {
                status: String::new(),
                ran_for_ms: 0,
            },
            Msg::Ok,
            Msg::Failed(String::new()),
        ]
        .iter()
        .map(|m| m.encode()[0])
        .collect();

        let mut unique = tags.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            tags.len(),
            "two messages share a tag: {tags:?}"
        );
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
    fn a_truncated_start_is_refused() {
        // Start is the message with the most fields and the only one with a
        // fixed-width field between two variable-width ones, so it is where a
        // decoder that read the fields out of order would show up. Every prefix
        // of it must fail rather than yield a Start with a plausible username.
        let full = Msg::Start {
            username: "testuser".to_owned(),
            tty: "/dev/tty7".to_owned(),
            seat: "seat0".to_owned(),
            vtnr: 7,
            session_type: "wayland".to_owned(),
            desktop: "sway".to_owned(),
        }
        .encode();

        for cut in 1..full.len() {
            assert!(
                Msg::decode(&full[..cut]).is_none(),
                "a Start truncated to {cut} bytes decoded anyway"
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
    fn an_oversized_datagram_is_refused_before_it_is_parsed() {
        // MAX_MESSAGE is `pub` and documents itself as a bound, so it has to be
        // one. A well-formed message that is merely too big is still refused —
        // the limit is on the datagram, not on whether the contents make sense.
        // A reason long enough to push the whole encoded message one byte over.
        let overhead = Msg::Failed(String::new()).encode().len();
        let padding = MAX_MESSAGE + 1 - overhead;
        let bytes = Msg::Failed("x".repeat(padding)).encode();
        assert_eq!(bytes.len(), MAX_MESSAGE + 1);
        assert!(
            Msg::decode(&bytes).is_none(),
            "a datagram over MAX_MESSAGE decoded anyway"
        );

        // And exactly at the limit still decodes, so the bound is not off by one
        // in the direction that silently drops real messages.
        let at_limit = Msg::Failed("x".repeat(padding - 1)).encode();
        assert_eq!(at_limit.len(), MAX_MESSAGE);
        assert!(Msg::decode(&at_limit).is_some());
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
