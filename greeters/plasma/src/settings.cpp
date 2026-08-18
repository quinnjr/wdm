// See settings.h for why this is hand-parsed and what the rules are.

#include "settings.h"

#include <cctype>
#include <fstream>
#include <sstream>

namespace wdm {

namespace {

SettingsResult failure(std::string error) {
    SettingsResult result;
    result.error = std::move(error);
    return result;
}

} // namespace

SettingsResult loadSettings(const std::filesystem::path &path) {
    std::error_code ec;
    if (!std::filesystem::exists(path, ec)) {
        // Absent is the defaults; unknowable (EACCES on a parent directory,
        // say) is not, and falls through to the open below so it reports.
        if (!ec) {
            SettingsResult result;
            result.settings = Settings{};
            return result;
        }
    }

    // is_regular_file first: libstdc++ opens a directory successfully and
    // reads it as empty, which would parse as "the defaults" — an existing
    // config silently ignored, the exact bug class this file refuses.
    if (!std::filesystem::is_regular_file(path, ec)) {
        return failure(path.string() + ": not a regular file");
    }
    std::ifstream file(path);
    if (!file) {
        return failure(path.string() + ": cannot be read");
    }
    std::ostringstream text;
    text << file.rdbuf();
    if (file.fail() && !file.eof()) {
        // A mid-read I/O error must not degrade to whatever half arrived.
        return failure(path.string() + ": read failed");
    }
    return parseSettings(text.str(), path.string());
}

namespace {

std::string_view trim(std::string_view s) {
    while (!s.empty() && (std::isspace(static_cast<unsigned char>(s.front())) != 0)) {
        s.remove_prefix(1);
    }
    while (!s.empty() && (std::isspace(static_cast<unsigned char>(s.back())) != 0)) {
        s.remove_suffix(1);
    }
    return s;
}

/// "#rrggbb" exactly, any case.
bool isHexColor(std::string_view value) {
    if (value.size() != 7 || value.front() != '#') {
        return false;
    }
    for (const char c : value.substr(1)) {
        if (std::isxdigit(static_cast<unsigned char>(c)) == 0) {
            return false;
        }
    }
    return true;
}

} // namespace

SettingsResult parseSettings(std::string_view text, const std::string &fileName) {
    Settings settings;
    bool sawTheme = false;
    bool sawColorScheme = false;
    bool sawBackground = false;

    std::size_t lineNumber = 0;
    while (!text.empty()) {
        const std::size_t newline = text.find('\n');
        std::string_view line = text.substr(0, newline);
        text.remove_prefix(newline == std::string_view::npos ? text.size() : newline + 1);
        ++lineNumber;

        line = trim(line);
        if (line.empty() || line.front() == ';' || line.front() == '#') {
            continue;
        }

        const auto refuse = [&](const std::string &why) {
            return failure(fileName + ":" + std::to_string(lineNumber) + ": " + why);
        };

        if (line.front() == '[') {
            // [General] is accepted because it is what Qt's own tools write
            // into an INI by default; any other section is a key namespace
            // this file does not have, and therefore a typo.
            if (line != "[General]") {
                return refuse("unknown section " + std::string(line)
                              + "; only [General] is recognised");
            }
            continue;
        }

        const std::size_t equals = line.find('=');
        if (equals == std::string_view::npos) {
            return refuse("expected key=value, got \"" + std::string(line) + "\"");
        }
        const std::string_view key = trim(line.substr(0, equals));
        const std::string_view value = trim(line.substr(equals + 1));

        // A repeated key is refused rather than last-one-wins, the same rule
        // parseThemeArgument applies to a repeated --theme: quietly taking
        // the later value shows a login screen other than the one half the
        // file asked for.
        if (key == "theme") {
            if (sawTheme) {
                return refuse("theme given more than once");
            }
            sawTheme = true;
            if (value.empty()) {
                return refuse("theme must not be empty");
            }
            settings.theme = std::string(value);
        } else if (key == "colorScheme") {
            if (sawColorScheme) {
                return refuse("colorScheme given more than once");
            }
            sawColorScheme = true;
            if (value != "dark" && value != "light") {
                // Worded like the Rust greeters' error, because these are the
                // sentences that reach the give-up screen.
                return refuse("colorScheme must be \"dark\" or \"light\", not \""
                              + std::string(value) + "\"");
            }
            settings.colorScheme = std::string(value);
        } else if (key == "background") {
            if (sawBackground) {
                return refuse("background given more than once");
            }
            sawBackground = true;
            if (isHexColor(value)) {
                settings.background = std::string(value);
                // Normalised to lower case: everything downstream compares
                // colours as strings.
                for (char &c : settings.background) {
                    c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
                }
            } else if (!value.empty() && value.front() == '/') {
                settings.background = std::string(value);
            } else {
                // A relative path could only be relative to wdm's working
                // directory, which no administrator controls.
                return refuse("background must be #rrggbb or an absolute path, not \""
                              + std::string(value) + "\"");
            }
        } else {
            // Most likely a typo of a key this parser does know, and
            // accepting it means the administrator's intent is ignored
            // without a word said.
            return refuse("unknown key \"" + std::string(key) + "\"");
        }
    }

    SettingsResult result;
    result.settings = std::move(settings);
    return result;
}

} // namespace wdm
