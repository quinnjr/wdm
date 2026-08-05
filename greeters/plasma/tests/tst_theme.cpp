// --theme resolution.
//
// Every case here is a way of getting the wrong login screen, or none. The one
// that matters most is the last: a theme that cannot be loaded is a startup
// failure and never a fallback, because a misspelled theme name that silently
// shows something else is a configuration bug nobody notices until they are
// looking at the wrong login screen.

#include <filesystem>
#include <fstream>
#include <string>

#include <unistd.h>

#include <catch2/catch_test_macros.hpp>

#include "theme.h"

namespace fs = std::filesystem;
using wdm::resolveTheme;

namespace {

/// A temporary tree, removed however the test ends — including when a REQUIRE
/// throws out of the middle of one, which is the ordinary way a Catch2 test
/// stops.
class TempTree {
public:
    TempTree() {
        root_ = fs::temp_directory_path()
                / ("wdm-plasma-theme-test-" + std::to_string(::getpid()) + "-"
                   + std::to_string(counter()));
        fs::create_directories(root_);
    }
    ~TempTree() {
        std::error_code ec;
        fs::permissions(root_, fs::perms::owner_all, fs::perm_options::add, ec);
        fs::remove_all(root_, ec);
    }

    TempTree(const TempTree &) = delete;
    TempTree &operator=(const TempTree &) = delete;

    const fs::path &root() const { return root_; }

    /// Create `<root>/<relative>/Main.qml`, the whole of what makes a directory
    /// a theme.
    fs::path makeTheme(const std::string &relative) {
        const fs::path directory = root_ / relative;
        fs::create_directories(directory);
        std::ofstream(directory / "Main.qml") << "import QtQuick\nItem {}\n";
        return directory;
    }

private:
    static int counter() {
        static int next = 0;
        return next++;
    }
    fs::path root_;
};

} // namespace

TEST_CASE("an absent theme means default", "[theme]") {
    TempTree tree;
    const fs::path expected = tree.makeTheme("default");

    const auto result = resolveTheme("", tree.root());
    // REQUIRE and not CHECK: `result.theme` is an empty optional when this
    // fails, and the two dereferences below would be undefined rather than
    // merely wrong. This is why the original had an `if (result.ok())` around
    // them; REQUIRE is that guard, and it reports as well.
    INFO("resolveTheme said: " << result.error);
    REQUIRE(result.ok());
    CHECK(result.theme->directory.string() == expected.string());
    CHECK(result.theme->mainQml.string() == (expected / "Main.qml").string());
    // A success carries no error text: ThemeResult promises one or the other,
    // never both, and a caller that logged `error` unconditionally would print
    // a stale reason next to a working theme.
    CHECK(result.error.empty());
}

TEST_CASE("a bare name resolves under the theme root", "[theme]") {
    TempTree tree;
    const fs::path expected = tree.makeTheme("breeze");

    const auto result = resolveTheme("breeze", tree.root());
    INFO("resolveTheme said: " << result.error);
    REQUIRE(result.ok());
    CHECK(result.theme->directory.string() == expected.string());
}

TEST_CASE("a name that is a path is used as given", "[theme]") {
    TempTree tree;
    // Deliberately not under the theme root: a path is a path, and a greeter
    // started with --theme ./mytheme during development must get that one.
    const fs::path elsewhere = tree.makeTheme("elsewhere/mytheme");

    const auto result = resolveTheme(elsewhere.string(), tree.root() / "themes");
    INFO("resolveTheme said: " << result.error);
    REQUIRE(result.ok());
    CHECK(result.theme->directory.string() == elsewhere.string());
}

TEST_CASE("a bare name never picks up the working directory", "[theme]") {
    // The reason the separator test is on the string rather than on what
    // exists: if a bare name could resolve against the current directory, which
    // theme the greeter showed would depend on where wdm happened to start it,
    // and a directory called `default` in the greeter account's home would
    // quietly replace the installed login screen.
    TempTree tree;
    tree.makeTheme("root/breeze");
    const fs::path decoy = tree.makeTheme("cwd/breeze");

    const fs::path previous = fs::current_path();
    fs::current_path(tree.root() / "cwd");
    const auto result = resolveTheme("breeze", tree.root() / "root");
    // Restored before the first assertion, because a failing REQUIRE throws and
    // a test process left in a deleted temporary directory poisons every case
    // that runs after it in the same binary.
    fs::current_path(previous);

    INFO("resolveTheme said: " << result.error);
    REQUIRE(result.ok());
    CHECK(result.theme->directory != decoy);
    CHECK(result.theme->directory.string() == (tree.root() / "root" / "breeze").string());
}

TEST_CASE("a missing theme is a failure and not a fallback", "[theme]") {
    TempTree tree;
    // The default exists and is loadable, which is exactly the situation in
    // which falling back would be tempting and wrong.
    tree.makeTheme("default");

    const auto result = resolveTheme("brezee", tree.root());
    REQUIRE(!result.ok());
    // The message is what wdm's give-up screen shows after three failures, so
    // it has to name the thing that was misspelled.
    CAPTURE(result.error);
    CHECK(result.error.find("brezee") != std::string::npos);
}

TEST_CASE("a directory without a main qml is not a theme", "[theme]") {
    TempTree tree;
    fs::create_directories(tree.root() / "empty");

    const auto result = resolveTheme("empty", tree.root());
    REQUIRE(!result.ok());
    CAPTURE(result.error);
    CHECK(result.error.find("Main.qml") != std::string::npos);
}

TEST_CASE("a theme that is a file is not a theme", "[theme]") {
    TempTree tree;
    std::ofstream(tree.root() / "afile") << "not a directory\n";

    const auto result = resolveTheme("afile", tree.root());
    REQUIRE(!result.ok());
    CAPTURE(result.error);
    CHECK(result.error.find("not a directory") != std::string::npos);
}

TEST_CASE("a main qml that cannot be read is not a theme", "[theme]") {
    // The way a theme installed by hand usually arrives: right path, wrong
    // mode. It exists, so an exists() check passes it, and the failure then
    // lands in the QML engine as a warning rather than at startup as a reason.
    if (::geteuid() == 0) {
        // root ignores the mode bits, so there is nothing to test here. SKIP
        // rather than the original's bare `return`, which reported a pass:
        // under Catch2 the run says out loud that this case did not execute,
        // so a CI job that happens to run as root cannot look like a clean one.
        SKIP("running as root: mode bits do not deny root, so there is nothing to test");
    }
    TempTree tree;
    const fs::path directory = tree.makeTheme("locked");
    fs::permissions(directory / "Main.qml", fs::perms::none);

    const auto result = resolveTheme("locked", tree.root());
    REQUIRE(!result.ok());
    CAPTURE(result.error);
    CHECK(result.error.find("cannot read") != std::string::npos);
}

// --------------------------------------------------------------------------
// The argument list
// --------------------------------------------------------------------------
//
// Parsed rather than scanned, and every malformed form is an error rather than
// a default, for the same reason a missing theme is: this greeter is launched
// from a `greeter.command` line in wdm's configuration file, and an argument
// that is quietly ignored there is a setting an administrator believes is in
// effect. The webkit greeter's `theme_argument` applies exactly these rules;
// an administrator should not have to remember which greeter is stricter.

TEST_CASE("a theme name is taken in either spelling", "[theme][args]") {
    CHECK(wdm::parseThemeArgument({"--theme", "breeze"}).name == "breeze");
    CHECK(wdm::parseThemeArgument({"--theme=breeze"}).name == "breeze");
    // A path, which resolveTheme distinguishes by the separator rather than by
    // asking the filesystem.
    CHECK(wdm::parseThemeArgument({"--theme=/srv/themes/mine"}).name == "/srv/themes/mine");
}

TEST_CASE("no argument at all means the default theme", "[theme][args]") {
    const wdm::ThemeArgument parsed = wdm::parseThemeArgument({});
    REQUIRE(parsed.ok());
    // Empty, and resolveTheme reads empty as kDefaultThemeName — so the two
    // agree about what "not given" means without either of them naming the
    // default twice.
    CHECK(parsed.name.empty());
    CHECK(resolveTheme(parsed.name, fs::path("/nonexistent")).error.find("'default'")
          != std::string::npos);
}

TEST_CASE("a malformed argument list is refused rather than defaulted", "[theme][args]") {
    SECTION("a trailing --theme with no value") {
        const wdm::ThemeArgument parsed = wdm::parseThemeArgument({"--theme"});
        CHECK(!parsed.ok());
        CHECK(parsed.name.empty());
    }

    SECTION("--theme= with nothing after it") {
        // Not the same as asking for the default: an empty name in a
        // configuration file is a mistake, and resolving it to `default` would
        // hide it behind a login screen that looks fine.
        CHECK(!wdm::parseThemeArgument({"--theme="}).ok());
    }

    SECTION("--theme given twice") {
        // Quietly taking the later value shows a login screen other than the
        // one that was asked for, and the one that was asked for is right there
        // in the same command line.
        CHECK(!wdm::parseThemeArgument({"--theme", "a", "--theme", "b"}).ok());
        CHECK(!wdm::parseThemeArgument({"--theme=a", "--theme=b"}).ok());
    }

    SECTION("anything else") {
        const wdm::ThemeArgument parsed = wdm::parseThemeArgument({"--verbose"});
        CHECK(!parsed.ok());
        // Named, because the person reading this is looking at a login screen
        // that did not appear and needs to know which word was the problem.
        CHECK(parsed.error.find("--verbose") != std::string::npos);
    }
}
