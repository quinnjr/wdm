//! Force `gtk4-layer-shell` ahead of `libwayland-client` on the link line.
//!
//! gtk4-layer-shell works by interposing a handful of libwayland-client
//! symbols. That only takes effect if it is loaded first, so when the linker
//! puts libwayland-client ahead of it the library silently does nothing and
//! every window comes up as an ordinary toplevel — which, against wdm, means a
//! greeter that is closed the moment it appears.
//!
//! `--no-as-needed` is required alongside it: the binary has no direct
//! references to gtk4-layer-shell's symbols (the crate dlopen-free wrapper calls
//! them through the sys crate), so the linker would otherwise drop the DT_NEEDED
//! entry it was just given.
fn main() {
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
    println!("cargo:rustc-link-arg=-lgtk4-layer-shell");
    println!("cargo:rustc-link-arg=-Wl,--as-needed");
    println!("cargo:rerun-if-changed=build.rs");
}
