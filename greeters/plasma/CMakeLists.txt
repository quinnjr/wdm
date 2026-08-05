# wdm-plasma-greeter — a Qt/QML greeter for wdm.
#
# Standalone on purpose. The Rust workspace does not know this directory exists
# and `cargo build --workspace` must never try to enter it: a build.rs that
# shelled out to cmake would make `cargo test --workspace` fail on every machine
# without Qt, which is most of the machines that build wdm.
#
#   cmake -B build -S greeters/plasma -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#   ctest --test-dir build
#
# Build dependencies: Qt 6.5+ (Core Gui Qml Quick), LayerShellQt 6.0+,
# wayland-client, wayland-server, wayland-scanner, and Catch2 3 for the tests.
# Catch2 is needed only when tests are built; a packaging build that wants the
# binary and nothing else configures with -DBUILD_TESTING=OFF and needs no
# Catch2 at all.
#
# The build is layered the way the greeter is. `wdm-plasma-core` is the protocol
# client and theme resolution and links no Qt at all; `wdm-plasma-qt` is the
# QObject a theme binds against and links QtCore and QtQml; the executable is
# the only thing that links QtGui, QtQuick and LayerShellQt, because it is the
# only thing with a window. -DWDM_PLASMA_QT=OFF builds and tests the first of
# those on a machine that has never had Qt installed, which is the property the
# layering exists for; see the option below.
#
# -DWDM_PLASMA_SANITIZE_ADDRESS=ON or -DWDM_PLASMA_SANITIZE_THREAD=ON build
# everything under a sanitizer, so the claims made about this code are things
# anyone can re-run rather than things someone once did.

cmake_minimum_required(VERSION 3.22)

# C as well as C++, for one file: wayland-scanner's private-code output. It is
# compiled as C rather than pulled into a C++ translation unit because a
# namespace-scope `const` object has internal linkage in C++ and external
# linkage in C, and every one of the marshalling tables is a namespace-scope
# const. Compiling them as C++ would hide wdm_greeter_v1_interface from the
# linker.
project(wdm-plasma-greeter VERSION 0.7.0 LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
# No GNU extensions: the only thing this code would take from them is
# `__typeof__` inside libwayland's wl_container_of, and it does not use it.
set(CMAKE_CXX_EXTENSIONS OFF)

include(GNUInstallDirs)
# Defines the BUILD_TESTING option, ON by default, and calls enable_testing().
# CMake's own spelling rather than a WDM_BUILD_TESTS of our own, because every
# distribution's packaging already knows this one: a builder that wants only the
# binary passes -DBUILD_TESTING=OFF and gets a configure that never looks for a
# test framework.
include(CTest)

# Applied per target rather than globally, because wayland-scanner's output is
# not ours to keep warning-free and a warning nobody can act on is a warning
# everybody learns to scroll past.
if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang")
    # -Werror=switch, and an error rather than a warning deliberately. The
    # protocol's enums cross a language boundary no compiler otherwise checks:
    # a variant added to wdm-greeter-v1.xml turns every switch over it into a
    # case that silently falls through to whatever follows. As a warning that is
    # a line in a build log nobody reads; as an error it is the build failing on
    # the file that has to change. See kindOf in src/wdm.cpp, whose comment
    # names this flag.
    set(WDM_PLASMA_WARNINGS -Wall -Wextra -Werror=switch)
else()
    set(WDM_PLASMA_WARNINGS "")
endif()

# --------------------------------------------------------------------------
# Sanitizers
# --------------------------------------------------------------------------
#
# Two options and not one, because ASan and TSan cannot be linked into the same
# binary — they both interpose the allocator — and a single WDM_PLASMA_SANITIZE
# taking a list would let a builder ask for a combination that fails at link
# time with a message about __asan_init.
#
# They exist so that the claims made about this code are reproducible. `Link`
# hands raw pointers from libwayland to a std::optional whose lifetime the
# protocol decides, and the fake server is a second thread the tests read state
# from; both are exactly the shape of defect that passes every assertion. A
# sanitizer run that anyone can reproduce with one flag is the difference
# between that having been checked and it having been claimed.
option(WDM_PLASMA_SANITIZE_ADDRESS "Build with AddressSanitizer and UBSan" OFF)
option(WDM_PLASMA_SANITIZE_THREAD "Build with ThreadSanitizer" OFF)

if(WDM_PLASMA_SANITIZE_ADDRESS AND WDM_PLASMA_SANITIZE_THREAD)
    message(FATAL_ERROR
        "WDM_PLASMA_SANITIZE_ADDRESS and WDM_PLASMA_SANITIZE_THREAD are mutually exclusive; "
        "ASan and TSan cannot be linked into one binary. Configure two build trees.")
endif()

# Added to CMAKE_*_FLAGS rather than per target: a sanitizer that instruments
# some translation units and not others reports the parts it can see and stays
# quiet about the rest, which is worse than not running it. -g and
# -fno-omit-frame-pointer because the whole output of a sanitizer is a stack
# trace.
if(WDM_PLASMA_SANITIZE_ADDRESS)
    set(WDM_PLASMA_SANITIZE_FLAGS
        -fsanitize=address,undefined -fno-sanitize-recover=all
        -fno-omit-frame-pointer -g)
elseif(WDM_PLASMA_SANITIZE_THREAD)
    set(WDM_PLASMA_SANITIZE_FLAGS -fsanitize=thread -fno-omit-frame-pointer -g)
endif()

if(WDM_PLASMA_SANITIZE_FLAGS)
    add_compile_options(${WDM_PLASMA_SANITIZE_FLAGS})
    add_link_options(${WDM_PLASMA_SANITIZE_FLAGS})
endif()

# --------------------------------------------------------------------------
# Qt
# --------------------------------------------------------------------------
#
# Behind an option, because this file currently builds only the Qt-free half and
# a hard `find_package(Qt6 REQUIRED)` made that a claim the build contradicted:
# `Link` links no Qt library, tst_link exists to keep it that way, and yet
# configuring on a machine without Qt6 and LayerShellQt failed outright. The
# option defaults ON so that the eventual full build is what a bare `cmake -B`
# gives you, and -DWDM_PLASMA_QT=OFF builds and tests the protocol client on a
# machine that has never had Qt installed — which is the property the layering
# is for.
#
# The floor is stated here, and stated as a version check at configure time
# rather than left to runtime: Qt 6.5 and LayerShellQt 6.0 are what make
# `LayerShellQt::Window::get(window)` before `show()` the whole of the
# layer-shell setup. On anything older that arrangement produces an
# xdg_toplevel, which wdm closes on sight because it advertises xdg_wm_base for
# popups only, and the result is a blank screen with no error on either side.
option(WDM_PLASMA_QT "Build the Qt layer (the greeter executable and its theme)" ON)

if(WDM_PLASMA_QT)
    # No `Test` component. Nothing here links Qt6::Test — the Qt layer's tests
    # are Catch2, for the reason given where they are defined — and a REQUIRED
    # component nothing links is a configure that fails on a machine with Qt
    # Quick but not qt6-base's test module, for a target that does not exist.
    find_package(Qt6 6.5 REQUIRED COMPONENTS Core Gui Qml Quick)
    find_package(LayerShellQt 6.0 REQUIRED)
endif()

find_package(PkgConfig REQUIRED)
pkg_check_modules(WaylandClient REQUIRED IMPORTED_TARGET wayland-client)
# Server-side libwayland is a test dependency only: the fake wdm the protocol
# client is tested against is a real compositor-side implementation of
# wdm_greeter_v1 rather than a mock of one, which is the only way a test can
# fail for the reason a real compositor would.
pkg_check_modules(WaylandServer REQUIRED IMPORTED_TARGET wayland-server)

# --------------------------------------------------------------------------
# Protocol
# --------------------------------------------------------------------------
#
# Generated at build time from the XML the Rust bindings are generated from.
# There is deliberately no copy of that file under greeters/: it is the one file
# in this repository that must have exactly one source of truth, because a
# greeter and the compositor disagreeing about an opcode is not a compile error
# anywhere — it is a login screen that hangs.

pkg_get_variable(WAYLAND_SCANNER wayland-scanner wayland_scanner)
if(NOT WAYLAND_SCANNER)
    find_program(WAYLAND_SCANNER NAMES wayland-scanner REQUIRED)
endif()

cmake_path(SET WDM_PROTOCOL_DIR NORMALIZE
    "${CMAKE_CURRENT_SOURCE_DIR}/../../crates/wdm-protocol")
set(WDM_PROTOCOL_XML "${WDM_PROTOCOL_DIR}/protocol/wdm-greeter-v1.xml")
# The Rust source is not compiled here; tst_exitcode reads it to check a
# constant that no compiler checks. See tests/tst_exitcode.cpp.
set(WDM_PROTOCOL_RS "${WDM_PROTOCOL_DIR}/src/lib.rs")

if(NOT EXISTS "${WDM_PROTOCOL_XML}")
    message(FATAL_ERROR
        "${WDM_PROTOCOL_XML} is missing.\n"
        "This greeter is built from inside the wdm source tree because it "
        "generates its protocol bindings from wdm's own XML rather than from a "
        "copy. Configure it as `cmake -B build -S greeters/plasma` from a wdm "
        "checkout.")
endif()

set(WDM_PROTOCOL_GEN "${CMAKE_CURRENT_BINARY_DIR}/protocol")
file(MAKE_DIRECTORY "${WDM_PROTOCOL_GEN}")

add_custom_command(
    OUTPUT "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-client-protocol.h"
    COMMAND "${WAYLAND_SCANNER}" client-header
            "${WDM_PROTOCOL_XML}" "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-client-protocol.h"
    DEPENDS "${WDM_PROTOCOL_XML}"
    COMMENT "wayland-scanner client-header wdm-greeter-v1.xml"
    VERBATIM)

add_custom_command(
    OUTPUT "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-server-protocol.h"
    COMMAND "${WAYLAND_SCANNER}" server-header
            "${WDM_PROTOCOL_XML}" "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-server-protocol.h"
    DEPENDS "${WDM_PROTOCOL_XML}"
    COMMENT "wayland-scanner server-header wdm-greeter-v1.xml"
    VERBATIM)

add_custom_command(
    OUTPUT "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-protocol.c"
    COMMAND "${WAYLAND_SCANNER}" private-code
            "${WDM_PROTOCOL_XML}" "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-protocol.c"
    DEPENDS "${WDM_PROTOCOL_XML}"
    COMMENT "wayland-scanner private-code wdm-greeter-v1.xml"
    VERBATIM)

# One static library for the marshalling tables, because there is exactly one of
# them: the same wdm_greeter_v1_interface serves the client and the server side,
# and compiling private-code into both would put two definitions of it in
# tst_link, which links both.
add_library(wdm-plasma-protocol STATIC
    "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-protocol.c"
    "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-client-protocol.h"
    "${WDM_PROTOCOL_GEN}/wdm-greeter-v1-server-protocol.h")
target_include_directories(wdm-plasma-protocol PUBLIC "${WDM_PROTOCOL_GEN}")
# wl_output_interface is referenced by the output_rank event's argument table and
# is defined in both libwayland-client and libwayland-server; the client is the
# one every consumer of this target already needs.
target_link_libraries(wdm-plasma-protocol PUBLIC PkgConfig::WaylandClient)
set_property(TARGET wdm-plasma-protocol PROPERTY POSITION_INDEPENDENT_CODE ON)

# --------------------------------------------------------------------------
# The Qt-free half
# --------------------------------------------------------------------------
#
# Link and theme resolution link no Qt library at all. That is not tidiness: it
# is what lets tst_link run the protocol client in a plain loop with no
# QGuiApplication, and therefore on a machine with no display of any kind.
add_library(wdm-plasma-core STATIC
    src/link.cpp
    src/theme.cpp)
target_include_directories(wdm-plasma-core PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/src")
target_link_libraries(wdm-plasma-core PUBLIC wdm-plasma-protocol PkgConfig::WaylandClient)
target_compile_options(wdm-plasma-core PRIVATE ${WDM_PLASMA_WARNINGS})
set_property(TARGET wdm-plasma-core PROPERTY POSITION_INDEPENDENT_CODE ON)

# --------------------------------------------------------------------------
# The Qt layer
# --------------------------------------------------------------------------
#
# Two targets and not one. `wdm-plasma-qt` is the QObject a theme binds against
# and the two models behind it; it links QtCore and QtQml and nothing else,
# which is what lets tst_wdm drive the entire state machine under a
# QCoreApplication with no QPA platform plugin, no window and no display. The
# executable is the only thing that links QtGui, QtQuick and LayerShellQt,
# because it is the only thing that has a window.
#
# AUTOMOC is set on the targets rather than globally: wayland-scanner's output
# is in this build too, and pointing moc at generated C would be asking a tool
# to scan files nobody wrote for a macro nobody used.

if(WDM_PLASMA_QT)
    add_library(wdm-plasma-qt STATIC
        src/models.cpp
        src/wdm.cpp)
    set_property(TARGET wdm-plasma-qt PROPERTY AUTOMOC ON)
    target_include_directories(wdm-plasma-qt PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/src")
    # Qml and not just Core: raising a QML error at the call site is what turns
    # a theme bug into a message with a file and a line, and QQmlEngine is where
    # throwError lives. Nothing here includes a QtGui header.
    target_link_libraries(wdm-plasma-qt PUBLIC wdm-plasma-core Qt6::Core Qt6::Qml)
    target_compile_options(wdm-plasma-qt PRIVATE ${WDM_PLASMA_WARNINGS})
    set_property(TARGET wdm-plasma-qt PROPERTY POSITION_INDEPENDENT_CODE ON)

    add_executable(wdm-plasma-greeter src/main.cpp)
    set_property(TARGET wdm-plasma-greeter PROPERTY AUTOMOC ON)
    target_link_libraries(wdm-plasma-greeter
        PRIVATE wdm-plasma-qt Qt6::Gui Qt6::Quick LayerShellQt::Interface)
    target_compile_options(wdm-plasma-greeter PRIVATE ${WDM_PLASMA_WARNINGS})

    # /usr/lib/wdm, beside the three Rust greeters. Not in $PATH, deliberately:
    # a greeter is spawned by wdm from an absolute path in its configuration and
    # is not a thing a user runs.
    #
    # A literal "lib/wdm", and GNUInstallDirs' CMAKE_INSTALL_LIBDIR is bypassed
    # here on purpose. That variable is multiarch — `lib/x86_64-linux-gnu` on
    # Debian and Ubuntu, `lib64` on Fedora — which is right for a library the
    # dynamic linker has to find among several architectures' copies, and wrong
    # for this. This is a private executable that nothing links against and
    # nothing dlopens: the only thing that ever names it is an administrator
    # writing `greeter.command` in wdm.toml, the three Rust greeters install to
    # a literal /usr/lib/wdm on every distribution, and docs/src/pages/
    # plasma-themes.md tells that administrator to write
    # /usr/lib/wdm/wdm-plasma-greeter. A greeter.command naming a path that does
    # not exist is a startup failure with no greeter left to report it, so the
    # four greeters have to agree on one path on all three distributions rather
    # than on one variable that expands to three.
    #
    # CMAKE_INSTALL_DATADIR below is left alone: it is `share` everywhere, so it
    # has none of this problem.
    install(TARGETS wdm-plasma-greeter RUNTIME DESTINATION "lib/wdm")
    # The whole directory and not just Main.qml: a theme is a directory of QML
    # with one entry point, and everything else in it is the theme's own
    # business — which includes the images and the components the default theme
    # does not have yet and a theme copied from it will.
    install(DIRECTORY themes/default
        DESTINATION "${CMAKE_INSTALL_DATADIR}/wdm/plasma-greeter/themes")
endif()

# --------------------------------------------------------------------------
# Tests
# --------------------------------------------------------------------------

# Catch2 3, found and not vendored, and deliberately not FetchContent.
#
# FetchContent downloads at configure time, and a configure that reaches the
# network cannot run in a clean chroot or on an offline builder — which is where
# every one of the Arch, Debian and Fedora packages is built. Catch2 becomes a
# declared build dependency of those packages instead, which is a line in a
# PKGBUILD and a guarantee, rather than a tarball fetched per build tree.
#
# Version 3 specifically: v2's single header is deprecated upstream and v3 is a
# compiled library, so the `find_package` has something real to find.
#
# Catch2 links no Qt, which is the property that made it the choice here.
# tst_link exists in part to prove that `Link` links no Qt library at all — that
# is what lets it drive the protocol client in a plain loop with no
# QGuiApplication, and therefore on a machine with no display of any kind — and
# QTest would have quietly destroyed that by dragging in QtCore. The Qt layer's
# own tests are Catch2 too — see tst_wdm below for why a second framework in one
# directory would be two ways to write one thing.
#
# QUIET and not REQUIRED: a missing Catch2 skips the test targets with a note
# below, rather than failing configure for a build that never wanted them.
if(BUILD_TESTING)
    find_package(Catch2 3 QUIET)
    if(NOT Catch2_FOUND)
        message(STATUS
            "Catch2 3 not found: skipping tests. Install catch2 (Arch), "
            "catch2 (Fedora) or catch2 >= 3 (Debian), or configure with "
            "-DBUILD_TESTING=OFF to stop looking.")
    endif()
endif()

if(BUILD_TESTING AND Catch2_FOUND)
    # catch_discover_tests, which runs each built test binary once at build time
    # to enumerate its TEST_CASEs and registers each one as its own ctest entry.
    # That is the point of it: `ctest -R` and a failure report then name the case
    # that failed rather than the binary it lived in.
    include(Catch)

    add_library(wdm-plasma-fakewdm STATIC tests/fakewdm.cpp)
    target_include_directories(wdm-plasma-fakewdm PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/tests")
    target_link_libraries(wdm-plasma-fakewdm PUBLIC wdm-plasma-protocol PkgConfig::WaylandServer)
    target_compile_options(wdm-plasma-fakewdm PRIVATE ${WDM_PLASMA_WARNINGS})

    # Catch2WithMain and not Catch2: none of these tests needs a main() of its
    # own, and one supplied by the framework is one that cannot forget to return
    # the failure count.
    add_executable(tst_link tests/tst_link.cpp)
    target_compile_options(tst_link PRIVATE ${WDM_PLASMA_WARNINGS})
    target_link_libraries(tst_link
        PRIVATE wdm-plasma-core wdm-plasma-fakewdm Catch2::Catch2WithMain)
    catch_discover_tests(tst_link)

    add_executable(tst_theme tests/tst_theme.cpp)
    target_compile_options(tst_theme PRIVATE ${WDM_PLASMA_WARNINGS})
    target_link_libraries(tst_theme PRIVATE wdm-plasma-core Catch2::Catch2WithMain)
    catch_discover_tests(tst_theme)

    add_executable(tst_exitcode tests/tst_exitcode.cpp)
    target_compile_options(tst_exitcode PRIVATE ${WDM_PLASMA_WARNINGS})
    target_link_libraries(tst_exitcode PRIVATE wdm-plasma-core Catch2::Catch2WithMain)
    # The path is a compile definition rather than something the test guesses
    # from its own location, so the test fails loudly when the tree it was
    # configured against moves instead of quietly finding nothing to compare.
    target_compile_definitions(tst_exitcode PRIVATE
        WDM_PROTOCOL_RS_PATH="${WDM_PROTOCOL_RS}")
    catch_discover_tests(tst_exitcode)

    # tst_link, tst_theme and tst_exitcode construct no Qt object at all, so
    # none of them needs a QPA platform plugin, an X server or a Wayland
    # compositor. tst_wdm below constructs a QCoreApplication and a QQmlEngine
    # and still needs none of those three: the split between Link and Wdm is
    # what makes the state machine testable without a window.

    if(WDM_PLASMA_QT)
        # Everything registered in this branch carries the `qt` label, and that
        # label is the whole of how CI can tell a Qt build from a Qt-free one.
        #
        # `--no-tests=error` fires only on *zero* tests, and tst_link, tst_theme
        # and tst_exitcode are registered outside this `if`. So a configure that
        # silently lost the Qt half — the WDM_PLASMA_QT default flipped, or a
        # container that stopped shipping Qt — still builds, still runs three
        # suites, and still exits 0, having compiled none of the state machine.
        # `ctest -L qt --no-tests=error` is the assertion that turns that into a
        # red job, because with the Qt half absent nothing carries the label and
        # zero is what --no-tests=error is for. See the plasma-greeter-qt job in
        # .github/workflows/rust.yml, which makes exactly that call.
        set(WDM_PLASMA_QT_TEST_LABEL qt)

        # Catch2 and not Qt6::Test, for the reason above and one more: these
        # cases drive a real Link against the real fake compositor over a
        # socketpair, so tst_wdm and tst_link are the same kind of test with
        # different subjects, and a second framework in the same directory
        # would be two ways to write one thing.
        add_executable(tst_wdm tests/tst_wdm.cpp)
        set_property(TARGET tst_wdm PROPERTY AUTOMOC ON)
        target_compile_options(tst_wdm PRIVATE ${WDM_PLASMA_WARNINGS})
        target_link_libraries(tst_wdm
            PRIVATE wdm-plasma-qt wdm-plasma-fakewdm Catch2::Catch2WithMain)
        # Paths rather than a guess from the test binary's own location, so that
        # a tree that has moved fails loudly instead of quietly finding nothing
        # to compare — the same rule tst_exitcode's WDM_PROTOCOL_RS_PATH follows,
        # and for the same reason: these two files are checked for agreement
        # across a language boundary that no compiler looks at.
        target_compile_definitions(tst_wdm PRIVATE
            WDM_DEFAULT_THEME_QML_PATH="${CMAKE_CURRENT_SOURCE_DIR}/themes/default/Main.qml"
            WDM_GTK_GREETER_UI_RS_PATH="${CMAKE_CURRENT_SOURCE_DIR}/../../crates/wdm-gtk-greeter/src/ui.rs")
        catch_discover_tests(tst_wdm PROPERTIES LABELS "${WDM_PLASMA_QT_TEST_LABEL}")

        # The argument snapshot's *ordering*, which no unit test can reach: the
        # refusal happens before any Qt object exists, so the only way to drive
        # it is to run the binary. Moving the snapshot below QGuiApplication
        # passes every other test in this tree while silently accepting
        # `-platform offscreen` — the single argument most able to leave an
        # administrator looking at a greeter that cannot work.
        #
        # PASS_REGULAR_EXPRESSION pins the refusal; FAIL_REGULAR_EXPRESSION is
        # what catches the mutation, because a snapshot taken after
        # QGuiApplication lets Qt consume `-platform offscreen` and the run
        # reaches the QPA check instead, which says "QPA platform is
        # 'offscreen'". No display is needed either way: the correct build exits
        # before QGuiApplication is constructed.
        #
        # CTest ignores the exit status once PASS_REGULAR_EXPRESSION is set, so
        # this entry pins the message rather than the 69. That is the right
        # division: tst_exitcode already pins kGaveUpExit against the Rust
        # constant, and what this test needs to know is which branch was taken,
        # which only the message says.
        add_test(NAME plasma-greeter-refuses-qt-arguments
            COMMAND wdm-plasma-greeter -platform offscreen)
        set_tests_properties(plasma-greeter-refuses-qt-arguments PROPERTIES
            LABELS "${WDM_PLASMA_QT_TEST_LABEL}"
            PASS_REGULAR_EXPRESSION "unrecognised argument: -platform"
            FAIL_REGULAR_EXPRESSION "QPA platform is")

        # qmllint over the default theme, as a test rather than as a build step.
        #
        # It catches the class of typo that otherwise ships as a blank login
        # screen: a misspelled property, a signal handler whose name does not
        # match any signal, a component that does not exist. None of that is a
        # compile error anywhere — QML is resolved at load — so without this the
        # first thing that notices is a user who cannot log in.
        #
        # Found rather than required: qmllint ships with qt6-declarative's tools
        # on some distributions and in a separate package on others, and a
        # missing linter should skip a test rather than fail a build that
        # otherwise works.
        #
        # Two flags, and both are load-bearing:
        #
        # --unqualified disable is the only check turned off. `wdm` is a context
        # property set from C++, so every reference to it is unqualified by
        # construction and qmllint has no way to know the object exists. The
        # alternative — generating a .qmltypes for a type no theme ever
        # instantiates — would be a build step producing a file to silence a
        # warning about the one object the whole contract is made of.
        #
        # -W 0 is what makes this a test at all. qmllint prints its warnings and
        # exits 0 by default, so without a maximum the entry would pass on a
        # theme full of misspelled properties and report nothing — a green test
        # that checks nothing, which is worse than no test.
        #
        # Qt6::qmllint and not find_program: /usr/bin/qmllint is Qt 5's on any
        # machine with both installed, it does not understand these options, and
        # a test that silently ran the wrong linter is the same green-and-empty
        # failure again. The imported target comes with Qt6::Qml, which this
        # branch already required.
        #
        # The `else()` branch below is the same green-and-empty failure one more
        # time, and it is kept anyway: on a developer machine a missing linter
        # should skip a test rather than fail a build that otherwise works.
        # What stops it from being silent in the places that matter is CI,
        # which asserts the entry by name — see the plasma-greeter-qt job.
        if(TARGET Qt6::qmllint)
            add_test(NAME qmllint-default-theme
                COMMAND Qt6::qmllint --unqualified disable -W 0
                        "${CMAKE_CURRENT_SOURCE_DIR}/themes/default/Main.qml")
            set_tests_properties(qmllint-default-theme
                PROPERTIES LABELS "${WDM_PLASMA_QT_TEST_LABEL}")

            # The negative control for the entry above, and the reason -W 0 is
            # a checked fact rather than an asserted one.
            #
            # Everything the comment above claims rests on that flag, and
            # nothing verified it. If -W 0 is renamed across a Qt minor, or
            # comes to mean something other than "any warning is a failure",
            # qmllint-default-theme passes forever on a theme full of
            # misspelled properties — and CI's `grep -q qmllint-default-theme`
            # still reports success, because the entry is still registered
            # under the same name. That is the same green-and-empty failure the
            # flag exists to prevent, one level up, and it is invisible in both
            # jobs' output.
            #
            # So: the same linter, the same two flags, pointed at a file that
            # is deliberately wrong, under WILL_FAIL. It passes only when
            # qmllint exits non-zero, which it does only while -W 0 still turns
            # a warning into a failure. Measured rather than assumed —
            # tests/lint/Broken.qml exits 255 with -W 0 and 0 without it, so
            # this entry is sensitive to exactly the flag it is here to pin.
            #
            # Not installed, and not under themes/: the install() above takes
            # the whole themes/default directory, and a broken QML file that
            # shipped inside a theme would be a theme that does not load.
            add_test(NAME qmllint-detects-a-broken-theme
                COMMAND Qt6::qmllint --unqualified disable -W 0
                        "${CMAKE_CURRENT_SOURCE_DIR}/tests/lint/Broken.qml")
            set_tests_properties(qmllint-detects-a-broken-theme PROPERTIES
                WILL_FAIL TRUE
                LABELS "${WDM_PLASMA_QT_TEST_LABEL}")
        else()
            message(STATUS
                "Qt6::qmllint not found: skipping the default theme's lint. It ships with "
                "Qt's declarative tools (qt6-declarative on Arch, qt6-declarative-dev-tools "
                "on Debian and Ubuntu, qt6-qtdeclarative-devel on Fedora).")
        endif()
    endif()
endif()
