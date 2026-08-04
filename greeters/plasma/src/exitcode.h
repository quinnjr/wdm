// The one number this greeter and wdm agree on with no compiler in between.

#pragma once

namespace wdm {

/// Exit status that says "restarting me will not help".
///
/// The value lives in crates/wdm-protocol/src/lib.rs as GREETER_GAVE_UP_EXIT
/// and is duplicated here because this greeter is not a Rust crate and cannot
/// include it. Nothing in either build checks that the two agree, so
/// tests/tst_exitcode.cpp reads that file and compares — a cross-language
/// constant with no compiler to check it needs something that does.
///
/// It is EX_UNAVAILABLE from sysexits.h. wdm counts an exit with this status
/// against the greeter's restart budget whatever the greeter's uptime was,
/// where every other exit is judged on uptime; so it must not be used to mean
/// anything else, and an ordinary failure spelled this way would exhaust the
/// budget early and reach the give-up screen from a machine that was fine.
inline constexpr int kGaveUpExit = 69;

// A compile-time contract and not a test, because it is one: both operands are
// known here, so nothing a test could run adds information. Anything above 255
// is truncated to its low byte by the kernel on the way out and reaches wdm as
// a different number, which wdm would then judge on uptime; and 0 would tell
// wdm the greeter succeeded.
static_assert(kGaveUpExit > 0 && kGaveUpExit < 256,
              "an exit status outside 1..255 is truncated by the kernel and reaches wdm as a "
              "different number");

} // namespace wdm
