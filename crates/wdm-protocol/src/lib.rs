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
