//! Wayland bindings for the `wdm_greeter_v1` protocol.
//!
//! A wdm greeter is an ordinary Wayland client. It binds [`wdm_greeter_v1`]
//! to enumerate users and sessions, drive a PAM conversation, and launch a
//! session. Enable the `client` feature to write a greeter, or the `server`
//! feature to implement the compositor side.
//!
//! [`wdm_greeter_v1`]: client::wdm_greeter_v1

#![forbid(improper_ctypes, unsafe_op_in_unsafe_fn)]

pub mod env;

// The scanner emits code that does not follow Rust naming or lint conventions,
// so the generated modules are quarantined here rather than allowing the lints
// crate-wide.
#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all)]
mod generated {
    #[cfg(feature = "client")]
    pub mod client {
        //! Client-side API of the protocol.
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocol/wdm-greeter-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("protocol/wdm-greeter-v1.xml");
    }

    #[cfg(feature = "server")]
    pub mod server {
        //! Server-side API of the protocol.
        use wayland_server;
        use wayland_server::protocol::*;

        pub mod __interfaces {
            use wayland_server::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocol/wdm-greeter-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_server_code!("protocol/wdm-greeter-v1.xml");
    }
}

#[cfg(feature = "client")]
pub use generated::client;

#[cfg(feature = "server")]
pub use generated::server;

/// Environment variable naming the socket a greeter should connect to.
///
/// wdm sets this in the greeter's environment. It is a plain `WAYLAND_DISPLAY`
/// style socket name, not a separate auth channel: authentication travels over
/// the same connection as rendering.
pub const GREETER_SOCKET_ENV: &str = "WAYLAND_DISPLAY";

/// Exit status a greeter uses to say it will not recover by being restarted.
///
/// Part of the greeter contract, which is why it lives here rather than in
/// either implementation: a greeter that has concluded it is wedged — a silent
/// web process, a renderer that never came up — exits with this status, and wdm
/// counts that against the restart budget **whatever the greeter's uptime was**.
/// Every other exit is judged on uptime, on the assumption that a greeter which
/// ran for a while and then died merely crashed. That assumption is wrong for
/// this case: deciding nothing is ever going to answer takes longer than the
/// rapid-failure window allows, so a greeter respawning into the same wedge
/// would look like a healthy one restarting, and the give-up screen — the only
/// thing that tells the user to switch to tty1 — would never be reached.
///
/// The value is `EX_UNAVAILABLE` from `sysexits.h`. A greeter must therefore not
/// use 69 to mean anything else: wdm cannot tell the two apart, and an ordinary
/// failure spelled this way would exhaust the restart budget early.
///
/// Deliberately outside both feature gates. Greeters (`client`) exit with it and
/// wdm (`server`) compares against it, so gating it on either would put the two
/// halves of one agreement behind different switches.
pub const GREETER_GAVE_UP_EXIT: u8 = 69;
