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
# Build dependencies: Qt 6.5+ (Core Gui Qml Quick Test), LayerShellQt 6.0+,
# wayland-client, wayland-server, wayland-scanner, and Catch2 3 for the tests.
# Catch2 is needed only when tests are built; a packaging build that wants the
# binary and nothing else configures with -DBUILD_TESTING=OFF and needs no
# Catch2 at all.
#
# This file currently builds the Qt-free half of the greeter — the protocol
# client (`Link`) and theme resolution — plus its tests. The QObject the QML
# binds against, the default theme and the executable arrive with the Qt layer.
# Until they do, -DWDM_PLASMA_QT=OFF configures and tests all of it on a machine
# with no Qt at all; see the option below.
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
    set(WDM_PLASMA_WARNINGS -Wall -Wextra)
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
    find_package(Qt6 6.5 REQUIRED COMPONENTS Core Gui Qml Quick Test)
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
# own tests (tst_wdm, qmllint over the default theme) will use Qt6::Test, where
# a QCoreApplication is needed anyway.
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

    # Every test here is a plain executable: none of them constructs a Qt
    # object, so none of them needs a QPA platform plugin, an X server or a
    # Wayland compositor to run. The Qt-side tests (tst_wdm, and qmllint over
    # the default theme) arrive with the Qt layer and will use Qt6::Test.
endif()
