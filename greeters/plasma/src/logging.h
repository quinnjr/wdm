// WDM_GREETER_LOG, and the rule that turns a critical into the give-up screen.
//
// This lived in an anonymous namespace in main.cpp, which is the executable's
// only translation unit — so nothing could reach it and nothing tested it,
// while two of the three decisions in it are contract rather than convenience:
//
//   - severityRank exists because QtMsgType's numeric order is not one.
//     QtInfoMsg was added to the enum last and has the highest value of the four
//     ordinary levels, so comparing the enum directly would make
//     WDM_GREETER_LOG=info the *quietest* setting there is;
//   - gGaveUp implements the exit-69 rule docs/src/pages/plasma-themes.md
//     publishes to theme authors: a critical anywhere — including one the QML
//     engine raised out of a theme — means restarting will not help, so wdm
//     counts the run against the restart budget whatever the uptime was and
//     reaches its give-up screen rather than reloading a broken theme forever.
//
// Header-only, with inline variables and inline functions, and that is a
// deliberate shape rather than a shortcut. The natural home is a logging.h /
// logging.cpp pair in wdm-plasma-qt, but a new .cpp is a new entry in
// CMakeLists.txt, and moving a file between build targets to make it testable
// is a change to the build for a change in the tests. Everything here is either
// a pure function of its argument or a scalar with one writer in a
// single-threaded process, so a header carries all of it with no link-time
// surface at all — and tst_wdm, which links wdm-plasma-qt, includes this and
// tests the two decisions above directly.
//
// What is *not* covered by that: messageHandler's own printing, and the
// std::_Exit on a fatal. Exercising either means installing a process-wide
// message handler and capturing stderr, and the fatal path ends the process by
// construction. See the "not tested here" list at the top of tst_wdm.cpp.

#pragma once

#include <cstdio>
#include <cstdlib>

#include <QByteArray>
#include <QString>
#include <QtGlobal>

#include "exitcode.h"

namespace wdm::logging {

/// Everything this greeter prints to a human wears this, because its stderr is
/// wdm's journal and a line with no subject in it is a line nobody can act on.
inline constexpr const char *kProgram = "wdm-plasma-greeter";

/// The lowest severity that is printed. Set once, from install().
inline QtMsgType gThreshold = QtWarningMsg;

/// Set by a critical or a fatal, and turned into kGaveUpExit at the end of
/// main().
///
/// Plain and not atomic: this greeter has one thread. Qt's own logging is
/// thread-safe because Qt is, but nothing in this process runs a second thread
/// — Link is polled from a QTimer, and there is no worker anywhere — so an
/// atomic here would be documenting a hazard that does not exist.
inline bool gGaveUp = false;

/// The threshold WDM_GREETER_LOG asks for.
///
/// env_logger's words, because that is what the three Rust greeters take
/// through env_logger and an administrator copies the setting between them. A
/// Qt greeter whose logging was spelled differently would be one an
/// administrator has to look up separately at the moment they are debugging a
/// login screen.
inline QtMsgType thresholdFromEnv() {
    const QByteArray value = qgetenv("WDM_GREETER_LOG").toLower();
    if (value == "trace" || value == "debug") {
        return QtDebugMsg;
    }
    if (value == "info") {
        return QtInfoMsg;
    }
    if (value == "error") {
        return QtCriticalMsg;
    }
    if (value == "off") {
        // Above every real severity, so nothing is printed. Fatals still
        // terminate — that is Qt's doing and not this handler's — which is
        // correct: "off" is a request for quiet, not a request to keep running
        // after an unrecoverable error.
        return QtFatalMsg;
    }
    // Including unset and including a value nobody here understands. Warnings
    // are what a login screen's failures arrive as.
    return QtWarningMsg;
}

/// Order the severities, because QtMsgType's numeric order is not one. See the
/// top of this file.
inline int severityRank(QtMsgType type) {
    switch (type) {
    case QtDebugMsg:
        return 0;
    case QtInfoMsg:
        return 1;
    case QtWarningMsg:
        return 2;
    case QtCriticalMsg:
        return 3;
    case QtFatalMsg:
        return 4;
    }
    // Not one of the enumerators, which C++ permits the type to hold. Ranked
    // with warnings so that an unknown severity is printed at the default
    // threshold rather than silently dropped.
    return 2;
}

inline const char *severityName(QtMsgType type) {
    switch (type) {
    case QtDebugMsg:
        return "debug";
    case QtInfoMsg:
        return "info";
    case QtWarningMsg:
        return "warn";
    case QtCriticalMsg:
        return "error";
    case QtFatalMsg:
        return "fatal";
    }
    return "warn";
}

/// Where every diagnostic in the process goes, including QML's.
///
/// The handler is what routes a *theme's* runtime diagnostics here. A binding
/// loop, or a reference to a property that does not exist, arrives as a
/// qWarning from the QML engine; without a handler those go to stderr
/// unfiltered and unattributed regardless of what the administrator asked for.
inline void messageHandler(QtMsgType type, const QMessageLogContext &context,
                           const QString &message) {
    if (type == QtCriticalMsg || type == QtFatalMsg) {
        // Recorded before the threshold test, because WDM_GREETER_LOG=off must
        // silence the *printing* and not the verdict.
        gGaveUp = true;
    }
    if (severityRank(type) < severityRank(gThreshold)) {
        return;
    }

    // The file and line are what make a theme's QML warning actionable, and
    // they are exactly what is missing when a theme is loaded from a path
    // nobody has in front of them. Absent for messages Qt raises from C++ built
    // without QT_MESSAGELOGCONTEXT, which is every release build, so they are
    // printed only when they are there.
    if (context.file != nullptr) {
        std::fprintf(stderr, "%s: %s: %s (%s:%d)\n", kProgram, severityName(type),
                     qUtf8Printable(message), context.file, context.line);
    } else {
        std::fprintf(stderr, "%s: %s: %s\n", kProgram, severityName(type),
                     qUtf8Printable(message));
    }

    if (type == QtFatalMsg) {
        // Qt calls abort() the moment this handler returns, so main()'s
        // `gGaveUp ? kGaveUpExit : status` is never reached on this path and
        // wdm sees SIGABRT — which it judges on uptime, so a theme that
        // qFatal()s instantly gets restarted forever instead of reaching the
        // give-up screen carrying the line just printed. Exiting here is what
        // makes gGaveUp mean the same thing for a fatal as for a critical.
        //
        // std::_Exit and not std::exit: a fatal is a process in a state Qt has
        // already declared unrecoverable, and running static destructors and
        // atexit handlers through it — including a graphics driver's, on the
        // one path where the platform plugin may be mid-dispatch — is how a
        // stated failure turns into a signal with no message. The same reason
        // CloseWatch in main.cpp goes through the event loop rather than
        // calling exit() from inside an event filter.
        std::_Exit(kGaveUpExit);
    }
}

/// Read the environment and install the handler. The first thing main() does.
inline void install() {
    gThreshold = thresholdFromEnv();
    qInstallMessageHandler(messageHandler);
}

} // namespace wdm::logging
