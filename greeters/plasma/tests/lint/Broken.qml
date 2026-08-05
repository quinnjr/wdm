// Deliberately broken, and never installed. This is the negative control for
// the qmllint entry in ../../CMakeLists.txt.
//
// That entry's whole claim rests on `-W 0`: qmllint prints its warnings and
// exits 0 by default, so without a maximum warning level it would pass on a
// theme full of misspelled properties and report nothing. Nothing checked that
// claim. If `-W 0` is renamed across a Qt minor, or comes to mean something
// other than "any warning is a failure", the entry passes forever and CI's
// `grep -q qmllint-default-theme` still reports success — the same
// green-and-empty failure the flag exists to prevent, one level up.
//
// So this file is linted with exactly the flags the real entry uses, under
// WILL_FAIL. It must produce at least one warning, and that warning must be
// enough to make qmllint exit non-zero. If it ever stops doing so, the
// negative control goes red and says the positive one has stopped checking.
//
// `wdth` for `width` is the misspelling on purpose: it is the exact class of
// typo the real entry exists to catch — not a syntax error, which every parser
// would reject anyway, but a name that is simply not a property of the type,
// which QML resolves at load and reports as a runtime warning on a login screen
// nobody is reading. Nothing here references the `wdm` context property, so the
// failure cannot come from `--unqualified` being dropped instead.

import QtQuick

Item {
    wdth: 100
}
