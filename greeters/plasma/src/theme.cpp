#include "theme.h"

#include <fstream>

namespace wdm {

namespace {

ThemeResult failure(std::string error) { return ThemeResult{std::nullopt, std::move(error)}; }

} // namespace

ThemeResult resolveTheme(std::string_view requested, const std::filesystem::path &root) {
    const std::string name(requested.empty() ? kDefaultThemeName : requested);

    // A separator is what distinguishes "the theme called plasma-ish" from
    // "the directory ./themes/mine". Checked on the string rather than by
    // asking the filesystem, so that the decision does not depend on what
    // happens to exist: a name that also names a directory in the current
    // working directory must still resolve under the theme root, or which
    // theme a greeter shows would depend on where it was started from.
    const bool isPath = name.find('/') != std::string::npos;
    const std::filesystem::path directory = isPath ? std::filesystem::path(name) : root / name;

    std::error_code ec;
    const auto status = std::filesystem::status(directory, ec);
    if (ec || !std::filesystem::exists(status)) {
        return failure("theme '" + name + "' not found: " + directory.string()
                       + " does not exist");
    }
    if (!std::filesystem::is_directory(status)) {
        return failure("theme '" + name + "' is not usable: " + directory.string()
                       + " is not a directory");
    }

    const std::filesystem::path mainQml = directory / std::filesystem::path(kThemeEntryPoint);

    // Opened rather than stat'd. exists() answers a question nobody asked: the
    // greeter is about to read this file, and a Main.qml that exists but cannot
    // be opened — mode 0600 owned by root, with the greeter running as the
    // unprivileged greeter account, which is how a theme installed by hand
    // usually arrives — would pass an exists() check and then fail in the QML
    // engine, where the reason is a warning rather than a startup failure.
    //
    // The is_regular_file check comes first because opening is not enough on
    // its own: Linux lets a directory be opened for reading and only fails the
    // read, so a directory called Main.qml would otherwise resolve.
    if (!std::filesystem::is_regular_file(mainQml, ec)) {
        return failure("theme '" + name + "' is not usable: " + mainQml.string()
                       + " is missing or is not a file");
    }
    std::ifstream entry(mainQml);
    if (!entry.is_open()) {
        return failure("theme '" + name + "' is not usable: cannot read " + mainQml.string());
    }

    return ThemeResult{Theme{directory, mainQml}, std::string()};
}

} // namespace wdm
