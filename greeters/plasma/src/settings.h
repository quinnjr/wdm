// /etc/wdm/plasma-greeter.ini parsing.
//
// wdm spawns greeters with a cleared environment, so a file in /etc/wdm is the
// only channel an administrator has to this process besides argv. The file is
// optional — absent means the built-in defaults — but a file that exists and
// cannot be understood is a startup error, never a fallback, by the same rule
// resolveTheme applies to a misspelled theme name: silently showing the
// default look hides the mistake until someone is looking at the wrong login
// screen.
//
// Free functions taking their input as arguments rather than methods reading a
// compiled-in path, so that a test can parse text it wrote instead of whatever
// is installed on the machine running the test — the same shape theme.h has,
// for the same reason.
//
// Hand-parsed rather than QSettings, and that is a correctness decision, not a
// dependency one: QSettings silently accepts every unknown key and malformed
// line it meets, and a key this file does not know is most likely a typo of
// one it does. This keeps wdm-plasma-core Qt-free as a side effect, which is
// what lets tst_settings run without a QGuiApplication.

#pragma once

#include <filesystem>
#include <optional>
#include <string>
#include <string_view>

namespace wdm {

/// Where the file lives. INI rather than TOML because this greeter's world is
/// Qt, where INI is the native dialect — but the keys say the same things the
/// Rust greeters' TOML keys say.
inline constexpr std::string_view kSettingsPath = "/etc/wdm/plasma-greeter.ini";

/// The administrator's choices. Empty strings mean "not set": every field has
/// a meaning only the theme (or the command line) can finish deciding, so
/// there is nothing sensible to default them to here.
struct Settings {
    /// Theme name or path, same semantics as --theme — which outranks it,
    /// because argv is written per-deployment in `greeter.command` and this
    /// file is the distribution-wide layer under it.
    std::string theme;
    /// "dark" or "light".
    std::string colorScheme;
    /// "#rrggbb" or an absolute path to an image.
    std::string background;
};

/// Either settings or the reason there are none. Never both.
struct SettingsResult {
    std::optional<Settings> settings;
    /// Non-empty exactly when `settings` is empty. Written for the user of a
    /// login screen that is about to not appear, so it names what it refused.
    std::string error;

    bool ok() const { return settings.has_value(); }
};

/// Read `path`, an absent file meaning defaults. Every other failure —
/// unreadable, malformed, unknown key, bad value — is an error carrying the
/// path, because the person reading it is looking at wdm's give-up screen and
/// needs to know which file to fix.
SettingsResult
loadSettings(const std::filesystem::path &path = std::filesystem::path(kSettingsPath));

/// Parse the file's text. Split from loadSettings so tests need no file at
/// all; `fileName` is only for error messages.
SettingsResult parseSettings(std::string_view text, const std::string &fileName);

/// The theme the greeter will show: --theme over the file over the default.
///
/// resolveTheme reads an empty name as kDefaultThemeName, so this only has to
/// pick the first non-empty of the two. A free function for the same reason
/// parseThemeArgument is one: main() cannot be tested, and the webkit greeter
/// keeps the identical rule in a tested pick_theme.
inline std::string chooseTheme(const std::string &cli, const Settings &settings) {
    return cli.empty() ? settings.theme : cli;
}

} // namespace wdm
