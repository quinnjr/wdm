// /etc/wdm/plasma-greeter.ini parsing.
//
// Every case here is a way of an administrator's choice being silently
// ignored, which is the bug class settings.h exists to refuse: a file that
// exists and cannot be understood is a startup error, never a fallback.

#include <filesystem>
#include <fstream>
#include <string>

#include <unistd.h>

#include <catch2/catch_test_macros.hpp>

#include "settings.h"

namespace fs = std::filesystem;
using wdm::loadSettings;
using wdm::parseSettings;

namespace {

/// A temporary tree, removed however the test ends.
class TempTree {
public:
    TempTree() {
        root_ = fs::temp_directory_path()
                / ("wdm-plasma-settings-test-" + std::to_string(::getpid()) + "-"
                   + std::to_string(counter()));
        fs::create_directories(root_);
    }
    ~TempTree() {
        std::error_code ec;
        fs::remove_all(root_, ec);
    }
    TempTree(const TempTree &) = delete;
    TempTree &operator=(const TempTree &) = delete;

    const fs::path &root() const { return root_; }

    fs::path write(const std::string &name, const std::string &contents) {
        const fs::path path = root_ / name;
        std::ofstream file(path);
        file << contents;
        return path;
    }

private:
    static int counter() {
        static int n = 0;
        return ++n;
    }
    fs::path root_;
};

} // namespace

TEST_CASE("an absent file is the defaults") {
    TempTree tree;
    const auto result = loadSettings(tree.root() / "nope.ini");
    REQUIRE(result.ok());
    CHECK(result.settings->theme.empty());
    CHECK(result.settings->colorScheme.empty());
    CHECK(result.settings->background.empty());
}

TEST_CASE("an empty file is the defaults") {
    const auto result = parseSettings("", "test.ini");
    REQUIRE(result.ok());
    CHECK(result.settings->theme.empty());
}

TEST_CASE("every key parses, with or without the General header") {
    const std::string body = "theme=breeze\ncolorScheme=light\nbackground=#AABBCC\n";
    for (const std::string &text : {body, "[General]\n" + body}) {
        const auto result = parseSettings(text, "test.ini");
        REQUIRE(result.ok());
        CHECK(result.settings->theme == "breeze");
        CHECK(result.settings->colorScheme == "light");
        // Normalised to lower case: everything downstream compares colours.
        CHECK(result.settings->background == "#aabbcc");
    }
}

TEST_CASE("comments, blank lines and whitespace are tolerated") {
    const auto result = parseSettings("; a comment\n# another\n\n  theme = breeze  \n", "t.ini");
    REQUIRE(result.ok());
    CHECK(result.settings->theme == "breeze");
}

TEST_CASE("unknown keys are refused as the typos they usually are") {
    const auto result = parseSettings("colourScheme=dark\n", "test.ini");
    REQUIRE_FALSE(result.ok());
    CHECK(result.error.find("colourScheme") != std::string::npos);
    CHECK(result.error.find("test.ini") != std::string::npos);
}

TEST_CASE("sections other than General are refused") {
    const auto result = parseSettings("[Appearance]\ntheme=breeze\n", "test.ini");
    REQUIRE_FALSE(result.ok());
    CHECK(result.error.find("Appearance") != std::string::npos);
}

TEST_CASE("a line that is not key=value is refused") {
    REQUIRE_FALSE(parseSettings("theme\n", "t.ini").ok());
}

TEST_CASE("a repeated key is refused rather than last-one-wins") {
    // The same rule parseThemeArgument applies to a repeated --theme:
    // quietly taking the later value shows a login screen other than the one
    // half the file asked for.
    REQUIRE_FALSE(parseSettings("theme=a\ntheme=b\n", "t.ini").ok());
}

TEST_CASE("colorScheme accepts exactly dark and light") {
    CHECK(parseSettings("colorScheme=dark\n", "t.ini").ok());
    CHECK(parseSettings("colorScheme=light\n", "t.ini").ok());
    CHECK_FALSE(parseSettings("colorScheme=Light\n", "t.ini").ok());
    CHECK_FALSE(parseSettings("colorScheme=\n", "t.ini").ok());
}

TEST_CASE("background is a colour or an absolute path") {
    CHECK(parseSettings("background=#123abc\n", "t.ini").ok());
    CHECK(parseSettings("background=/usr/share/wall.png\n", "t.ini").ok());
    CHECK_FALSE(parseSettings("background=#12g\n", "t.ini").ok());
    CHECK_FALSE(parseSettings("background=wall.png\n", "t.ini").ok());
    CHECK_FALSE(parseSettings("background=\n", "t.ini").ok());
}

TEST_CASE("theme must not be empty") {
    REQUIRE_FALSE(parseSettings("theme=\n", "t.ini").ok());
}

TEST_CASE("errors from a real file name the file") {
    TempTree tree;
    const auto path = tree.write("plasma-greeter.ini", "background=nope\n");
    const auto result = loadSettings(path);
    REQUIRE_FALSE(result.ok());
    CHECK(result.error.find("plasma-greeter.ini") != std::string::npos);
}

TEST_CASE("the command line beats the file beats the default") {
    wdm::Settings fromFile;
    fromFile.theme = "breeze";
    CHECK(wdm::chooseTheme("arch", fromFile) == "arch");
    CHECK(wdm::chooseTheme("", fromFile) == "breeze");
    // Empty resolves to kDefaultThemeName inside resolveTheme.
    CHECK(wdm::chooseTheme("", wdm::Settings{}).empty());
}

TEST_CASE("a directory at the config path is an error, not the defaults") {
    TempTree tree;
    const auto result = loadSettings(tree.root());
    REQUIRE_FALSE(result.ok());
    CHECK(result.error.find("not a regular file") != std::string::npos);
}

TEST_CASE("a real file round-trips") {
    TempTree tree;
    const auto path = tree.write("plasma-greeter.ini",
                                 "[General]\ntheme=/opt/theme\nbackground=#010203\n");
    const auto result = loadSettings(path);
    REQUIRE(result.ok());
    CHECK(result.settings->theme == "/opt/theme");
    CHECK(result.settings->background == "#010203");
    CHECK(result.settings->colorScheme.empty());
}
