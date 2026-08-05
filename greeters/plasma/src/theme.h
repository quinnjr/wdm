// --theme resolution.
//
// A free function taking its search root as an argument, rather than a method
// on the application reading a compiled-in path, so that a test can resolve
// against a directory it created instead of against whatever happens to be
// installed on the machine running the test.

#pragma once

#include <filesystem>
#include <optional>
#include <string>
#include <string_view>
#include <vector>

namespace wdm {

/// Where a bare `--theme` name is looked up.
inline constexpr std::string_view kThemeRoot = "/usr/share/wdm/plasma-greeter/themes";

/// The theme used when `--theme` is absent.
inline constexpr std::string_view kDefaultThemeName = "default";

/// The file a theme directory must contain. A theme is a directory of QML with
/// one entry point; everything else in it is the theme's own business.
inline constexpr std::string_view kThemeEntryPoint = "Main.qml";

/// A theme that exists and can be read.
struct Theme {
    std::filesystem::path directory;
    /// directory/Main.qml, already checked to exist and open.
    std::filesystem::path mainQml;
};

/// Either a theme or the reason there is not one. Never both.
struct ThemeResult {
    std::optional<Theme> theme;
    /// Non-empty exactly when `theme` is empty. Written for the user of a login
    /// screen that is about to not appear, so it names the path it looked at.
    std::string error;

    bool ok() const { return theme.has_value(); }
};

/// Resolve `--theme` to a directory.
///
/// - a value containing a separator is used as a path, exactly as given;
/// - a bare name resolves under `root`;
/// - an empty value means kDefaultThemeName.
///
/// A theme that is missing, is not a directory, has no Main.qml, or has one
/// that cannot be opened is an error and never a fallback to the default. A
/// misspelled theme name that silently shows something else is a configuration
/// bug nobody notices until they are looking at the wrong login screen — and
/// the caller's response to an error here is to exit with the reason on stderr,
/// where wdm's supervisor turns it into the give-up screen's text. A blank
/// screen is never the result.
ThemeResult resolveTheme(std::string_view requested,
                         const std::filesystem::path &root = std::filesystem::path(kThemeRoot));

/// The `--theme` value found in an argument list, or why the list is unusable.
struct ThemeArgument {
    /// Empty when `--theme` was not given, which resolveTheme reads as the
    /// default theme.
    std::string name;
    /// Non-empty exactly when the arguments could not be understood.
    std::string error;

    bool ok() const { return error.empty(); }
};

/// Pull `--theme` out of an argument list, minus argv[0].
///
/// Accepts `--theme NAME` and `--theme=NAME`. A trailing `--theme` with no
/// value, a repeated `--theme`, and any argument that is neither are all
/// errors, for the same reason a misspelled theme name is a startup failure
/// rather than a fallback: quietly taking the later of two values, or ignoring
/// something the administrator wrote in `greeter.command`, shows a login screen
/// other than the one that was asked for, and nobody notices until they are
/// looking at the wrong one. The same rule the webkit greeter's
/// `theme_argument` applies, deliberately, because the two are configured the
/// same way and an administrator should not have to remember which is which.
///
/// A free function taking the list rather than reading QCoreApplication's
/// arguments, so that it can be tested without a Qt application object — and so
/// that it lives with resolveTheme, which is the only thing that consumes it.
ThemeArgument parseThemeArgument(const std::vector<std::string> &args);

} // namespace wdm
