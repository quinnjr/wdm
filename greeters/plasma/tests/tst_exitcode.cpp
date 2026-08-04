// The give-up exit code, checked against the Rust source that defines it.
//
// wdm and this greeter are separate programs in separate languages built by
// separate build systems, and the only thing that makes 69 mean "restarting me
// will not help" is that both ends spell it the same way. Nothing else in
// either build would notice them diverging: the greeter would exit with a
// number wdm judges on uptime, so a wedged greeter would respawn into the same
// wedge forever and the give-up screen — the only thing that tells the user to
// switch to a text console — would never be reached.
//
// So the test reads crates/wdm-protocol/src/lib.rs rather than restating the
// number in a comment. A duplicated constant with a comment pointing at the
// original is a constant that drifts; one that is read at test time is one that
// cannot.

#include <fstream>
#include <regex>
#include <sstream>
#include <string>

#include <catch2/catch_test_macros.hpp>

#include "exitcode.h"

TEST_CASE("the give up exit matches the rust constant", "[exitcode]") {
    std::ifstream source(WDM_PROTOCOL_RS_PATH);
    if (!source.is_open()) {
        // FAIL and not CHECK: everything below reads `text`, and the reason the
        // file is unreadable is the only useful thing this run can report.
        FAIL("cannot read " << WDM_PROTOCOL_RS_PATH
                            << "; this test is only meaningful inside a wdm checkout");
    }
    std::ostringstream buffer;
    buffer << source.rdbuf();
    const std::string text = buffer.str();

    // Anchored on the whole declaration, not on the name: matching the name
    // alone would also match the doc comment above it, and a test that passes
    // by reading prose is a test that passes after the constant is deleted.
    const std::regex declaration(R"(pub const GREETER_GAVE_UP_EXIT\s*:\s*u8\s*=\s*(\d+)\s*;)");
    std::smatch match;
    if (!std::regex_search(text, match, declaration)) {
        FAIL("no GREETER_GAVE_UP_EXIT declaration in "
             << WDM_PROTOCOL_RS_PATH << "; if it was renamed, this greeter's copy is now wrong");
    }

    CHECK(std::stoi(match[1].str()) == wdm::kGaveUpExit);
    // The range — that the value is one a process can actually exit with — is
    // not checked here. Both operands would be known at compile time in this
    // translation unit, so no edit outside exitcode.h could fail it and an edit
    // inside exitcode.h is already caught by the comparison above. It lives in
    // exitcode.h as a static_assert, where it is a contract the compiler
    // enforces on every build rather than a claim a test run happens to
    // restate.
}
