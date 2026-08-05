// wdm-plasma-greeter: the application, the layer-shell window, and the theme.
//
// The order of the first four statements in main() is the whole of this file's
// design, and every one of them is load-bearing:
//
//   1. QGuiApplication, so that Qt opens the Wayland connection. The greeter
//      cannot open its own: wdm checks SO_PEERCRED once per connection and the
//      greeter is a single client, so objects created on a second connection
//      belong to a client with no surfaces and nothing they say can be drawn.
//   2. Link on Qt's wl_display, with its own event queue. Qt never sees a
//      wdm_greeter_v1 event and we never see one of Qt's.
//   3. Link::connect(), which returns only once the enumerate phase has ended,
//      so the models are populated before a single QML binding is evaluated.
//      A theme therefore never flickers into its user list.
//   4. LayerShellQt::Window::get() *before* show(). After show() the surface
//      has already been created as an xdg_toplevel, which wdm closes on sight
//      because it advertises xdg_wm_base for popups only — a blank screen with
//      no error on either side, and the same trap gtk4-layer-shell's link order
//      is documented for in CLAUDE.md.
//
// Nothing here has policy. Which session to preselect, what a failure looks
// like, whether to offer a retry: those are the theme's, and this file's job is
// to fail loudly when the theme cannot be shown at all rather than to decide
// anything on its behalf.

#include <cstddef>
#include <cstdio>
#include <string>
#include <vector>

#include <QCoreApplication>
#include <QEvent>
#include <QGuiApplication>
#include <QQmlContext>
#include <QQmlEngine>
#include <QQmlError>
#include <QQuickItem>
#include <QQuickView>
#include <QString>
#include <QUrl>
#include <QtGlobal>

#include <qguiapplication_platform.h>

#include <LayerShellQt/Window>

#include "exitcode.h"
#include "link.h"
#include "logging.h"
#include "theme.h"
#include "wdm.h"

namespace {

using wdm::logging::gGaveUp;
using wdm::logging::kProgram;

// --------------------------------------------------------------------------
// How the layer surface is actually asked for
// --------------------------------------------------------------------------
//
// LayerShellQt has two mechanisms, and this greeter uses the second:
//
//   - Shell::useLayerShell(), the process-wide one. It qputenvs
//     QT_WAYLAND_SHELL_INTEGRATION=layer-shell so that the Qt Wayland platform
//     plugin loads its liblayer-shell.so shell-integration plugin for *every*
//     window the process creates. Deprecated since LayerShellQt 6.6,
//     documented as unnecessary since Qt 6.5, and compiled behind a deprecation
//     macro — a build with deprecations disabled would not even find it.
//
//   - Window::get(QWindow *), the per-window one, which is what this file
//     calls. It creates a LayerShellQt::Window bound to that QWindow, and that
//     object's initializeShell() constructs a QWaylandLayerShellIntegration and
//     hands it to the window through QWaylandWindow::setShellIntegration.
//
// The second sets no environment variable. Verified against LayerShellQt
// 6.7.3 by disassembly: both references to the QT_WAYLAND_SHELL_INTEGRATION
// string and the "Unable to set …" warning resolve inside useLayerShell() and
// nowhere else, while Window::get() reaches setShellIntegration and touches no
// environment at all. An earlier version of this file read the variable back
// after Window::get() and treated a mismatch as fatal, which meant the greeter
// exited before it had drawn anything, every single time.
//
// There is therefore *no* observable, before show(), that says the next window
// will be a layer surface — LayerShellQt exposes no such query, and setting the
// variable here by hand would be re-implementing the deprecated global path
// rather than checking the per-window one. CloseWatch below is the whole check,
// and it is a real one: it catches both halves of the failure, because whether
// LayerShellQt never installed its integration or installed it and the
// compositor refused the layer surface, Qt ends up with an xdg_toplevel and wdm
// closes it on sight.

// --------------------------------------------------------------------------
// Logging
// --------------------------------------------------------------------------
//
// WDM_GREETER_LOG, the message handler and the exit-69 rule now live in
// logging.h, which is part of the wdm-plasma-qt library rather than of this
// executable's only translation unit. They moved because they are contract —
// the words WDM_GREETER_LOG takes are the three Rust greeters' words, and a
// critical marking the greeter as having given up is a rule published to theme
// authors — and nothing in an anonymous namespace in main.cpp can be tested by
// anything. See tst_wdm's "logging" cases.

/// One line to stderr, for the failures that happen before there is a logging
/// threshold worth consulting.
void fail(const std::string &reason) {
    std::fprintf(stderr, "%s: %s\n", kProgram, reason.c_str());
}

// --------------------------------------------------------------------------
// The window closing
// --------------------------------------------------------------------------

/// Turns the compositor closing our window into an explanation.
///
/// This is the second half of the layer-shell check, and it catches the case
/// the environment variable cannot: LayerShellQt asked for a layer surface and
/// the compositor did not give it one, so Qt fell back to an xdg_toplevel,
/// which wdm closes the moment it is created. Without this the greeter would
/// exit 0 with nothing on screen and nothing in the journal, which is precisely
/// the blank-login-screen failure the design names twice.
///
/// kGaveUpExit and not EXIT_FAILURE: restarting will produce the same window
/// and the same close, so wdm should reach its give-up screen carrying this
/// text rather than loop.
class CloseWatch : public QObject {
public:
    bool eventFilter(QObject *watched, QEvent *event) override {
        if (event->type() == QEvent::Close) {
            fail("wdm closed the greeter's window. This is what wdm does to an "
                 "xdg_toplevel: it advertises xdg_wm_base for popups only. The layer surface "
                 "was not created — check that LayerShellQt is installed and that the "
                 "compositor supports zwlr_layer_shell_v1.");
            // Through the event loop rather than std::exit, so the unwinding
            // happens where Qt expects it: exit() from inside an event filter
            // runs static destructors while the platform plugin is still
            // dispatching, and a greeter that crashes on the way out reports a
            // signal rather than the sentence above. gGaveUp carries the
            // disposition; the argument is what main() would return anyway.
            gGaveUp = true;
            QCoreApplication::exit(wdm::kGaveUpExit);
        }
        return QObject::eventFilter(watched, event);
    }
};

} // namespace

// Every early return in main() below is wdm::kGaveUpExit and not EXIT_FAILURE,
// and the rule that decides that is one question: would running this again, on
// this machine, with this `greeter.command`, do anything different? For every
// failure reachable before app.exec() the answer is no. An unparseable argument
// list, a theme that is not installed, a theme whose QML does not compile, a QPA
// platform that is not Wayland and a wdm that does not advertise
// wdm_greeter_v1 are all facts about the configuration and the machine, fixed
// for as long as both are. wdm counts kGaveUpExit against the restart budget
// whatever the uptime was, so it reaches the give-up screen carrying the
// sentence printed here; EXIT_FAILURE is judged on uptime, and while an
// immediate exit is also counted as rapid, spelling a deterministic failure that
// way asks wdm to keep trying something that cannot start.
//
// The give-up screen is the point. These messages go to wdm's journal *and*
// become its text, so the user reads why there is no login screen instead of
// watching one flicker.

int main(int argc, char **argv) {
    wdm::logging::install();

    // Snapshotted before QGuiApplication, and that is the whole point of doing
    // it here rather than after: QGuiApplication's constructor *consumes* the
    // arguments it recognises — -platform, -style, -qwindowgeometry,
    // -qmljsdebugger and the rest — removing them from argv and shortening
    // argc. A loop that read argv afterwards would never see them, so the
    // refusal below would quietly accept every one of them while claiming
    // nothing is quietly accepted. `-platform offscreen` is the one that
    // matters: it is accepted by Qt, it is not a Wayland platform, and it is the
    // single argument most able to leave an administrator looking at a greeter
    // that cannot work.
    std::vector<std::string> args;
    args.reserve(static_cast<std::size_t>(argc > 1 ? argc - 1 : 0));
    for (int i = 1; i < argc; ++i) {
        args.emplace_back(argv[i]);
    }

    // Parsed before QGuiApplication too, so that a rejected argument is a
    // sentence rather than whatever Qt does with it first. Qt aborts the process
    // outright on a -platform naming a plugin that will not load, which would
    // replace this message with a Qt one that does not mention wdm.
    const wdm::ThemeArgument requested = wdm::parseThemeArgument(args);
    if (!requested.ok()) {
        fail(requested.error);
        return wdm::kGaveUpExit;
    }

    // Before QGuiApplication, because QQuickStyle reads this when the first
    // QtQuick.Controls import is resolved and the environment is the way to set
    // it without linking Qt6::QuickControls2 for one call. Only when unset, so
    // that an administrator can override it; a style that is not installed
    // falls back to Basic with a warning rather than failing, which is the one
    // place in this greeter where a fallback is right — a login screen that
    // looks wrong is still a login screen, and org.kde.desktop is a
    // recommendation rather than part of the contract.
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        qputenv("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop");
    }

    // Qt's own arguments are not accepted, deliberately, and neither is
    // anything else: this process is spawned by wdm from a `greeter.command`
    // line in a configuration file, and an argument that is quietly ignored
    // there is a setting an administrator believes is in effect. parseThemeArgument
    // above ran on the pre-Qt snapshot precisely so that "anything else"
    // includes Qt's own.
    QGuiApplication app(argc, argv);

    auto *wayland = app.nativeInterface<QNativeInterface::QWaylandApplication>();
    if (wayland == nullptr) {
        // An X11 or offscreen platform plugin. There is nothing to fall back
        // to — wdm is a Wayland compositor and this greeter is one of its
        // clients — so the platform is named rather than left to be guessed
        // from a null-pointer crash.
        fail("this greeter runs only on the Wayland platform plugin; QPA platform is '"
             + QGuiApplication::platformName().toStdString() + "'");
        return wdm::kGaveUpExit;
    }

    // Constructed in this order because the two need each other: Link takes its
    // observer at construction, and Wdm needs the Link to send requests
    // through. Wdm is the one that can be attached afterwards, because Link
    // delivers nothing until connect() below.
    wdm::Wdm bridge;
    wdm::Link link(wayland->display(), &bridge);
    bridge.attach(&link);

    std::string error;
    if (!link.connect(&error)) {
        fail(error);
        return wdm::kGaveUpExit;
    }

    const wdm::ThemeResult theme = wdm::resolveTheme(requested.name);
    if (!theme.ok()) {
        // Never a fallback to the default theme. A misspelled name that
        // silently shows something else is a configuration bug nobody notices
        // until they are looking at the wrong login screen.
        fail(theme.error);
        return wdm::kGaveUpExit;
    }

    // QQuickView and not QQmlApplicationEngine, and the difference is the
    // contract rather than convenience. QQmlApplicationEngine expects the
    // theme's root object to be a Window, which would put the layer-shell
    // configuration inside the theme — where a theme author can get it wrong
    // and be handed a blank screen with no error. QQuickView owns the one
    // QWindow, so this file configures the layer surface and the theme's root
    // is an Item; a root that is not an Item is refused by QQuickView itself,
    // which is exactly the startup failure the design asks for.
    QQuickView view;
    view.setResizeMode(QQuickView::SizeRootObjectToView);
    view.setTitle(QStringLiteral("wdm"));
    view.rootContext()->setContextProperty(QStringLiteral("wdm"), &bridge);
    bridge.setQmlEngine(view.engine());

    view.setSource(QUrl::fromLocalFile(QString::fromStdString(theme.theme->mainQml.string())));
    if (view.status() != QQuickView::Ready || view.rootObject() == nullptr) {
        // Every error, not just the first: a QML failure is usually one real
        // mistake and several consequences of it, and the real one is not
        // reliably first. wdm's give-up screen carries this text, so it is the
        // only account of the failure anyone gets.
        for (const QQmlError &qmlError : view.errors()) {
            fail(qmlError.toString().toStdString());
        }
        fail("the theme at " + theme.theme->mainQml.string()
             + " did not load. A theme's Main.qml must have an Item as its root object.");
        return wdm::kGaveUpExit;
    }

    // Installed here, before anything below can return early, because this is
    // the *only* check that the window became a layer surface — see the comment
    // at the top of this file. A version of this that installed the filter after
    // a readback of QT_WAYLAND_SHELL_INTEGRATION had neither half of the
    // intended two-check assertion: the readback could not succeed, so it
    // returned first and this was never reached. An event filter on a window
    // that is never shown costs nothing, so there is no reason to install it any
    // later than the moment the window exists.
    CloseWatch closeWatch;
    view.installEventFilter(&closeWatch);

    // Before show(). After it the platform window exists and the shell
    // integration has already been chosen.
    LayerShellQt::Window *layer = LayerShellQt::Window::get(&view);
    if (layer == nullptr) {
        fail("LayerShellQt would not attach to the greeter's window");
        return wdm::kGaveUpExit;
    }
    layer->setLayer(LayerShellQt::Window::LayerOverlay);
    // A login screen: nothing else may hold the keyboard while it is up.
    layer->setKeyboardInteractivity(LayerShellQt::Window::KeyboardInteractivityExclusive);
    // Braces and not `|`: QFlags has no operator| taking an int, and the
    // enumerators are plain enum constants, so an or-expression is an int the
    // setter will not take.
    layer->setAnchors({LayerShellQt::Window::AnchorTop, LayerShellQt::Window::AnchorBottom,
                       LayerShellQt::Window::AnchorLeft, LayerShellQt::Window::AnchorRight});
    layer->setScope(QStringLiteral("wdm-greeter"));
    // No screen is set on purpose: wdm places a layer surface with no output on
    // the rank 0 output and moves it when the ranks change on hotplug, so
    // choosing one here would reimplement that, less well. The output_rank
    // event Link receives and ignores is the same decision seen from the other
    // side.
    //
    // LayerShellQt::Shell::useLayerShell() is deliberately not called, for the
    // reasons at the top of this file. get() before show() is the whole of the
    // API, and CloseWatch above is the whole of the check.

    view.show();

    // Last, so that nothing is dispatched into a theme that has not loaded: a
    // prompt delivered before the engine had the context property would set
    // properties nothing was bound to.
    bridge.startPumping();

    const int status = app.exec();
    // A critical anywhere — including one the QML engine raised out of a
    // theme, and including the raise() a contract violation produces when there
    // is no engine to throw into — means restarting will not help.
    return gGaveUp ? wdm::kGaveUpExit : status;
}
