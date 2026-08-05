#!/usr/bin/env bash
#
# Regenerates vendor/ — the two files the theme actually ships, plus the
# licences of what is bundled into them.
#
# The theme ships vendor/ checked in, so installing it copies files and runs
# nothing: no npm on the target machine, none in the build chroot, and no
# network during packaging. This script exists so that "where did vendor/app.js
# come from" has an answer that is a command rather than a memory.
#
# Needs npm and a network. Run it from this directory.
set -euo pipefail

cd "$(dirname "$0")"

echo "==> installing pinned dependencies"
# `npm ci` and not `npm install`: it installs exactly package-lock.json and
# fails if the lockfile and package.json disagree, which is the difference
# between a rebuild that reproduces this bundle and one that quietly picks up
# a new React.
npm ci

echo "==> running the test suite"
# Before the build, deliberately. These are the rules that stop an unattended
# login screen locking somebody out; shipping a bundle built from source that
# fails them would be worse than shipping nothing.
npm test

# The licences of what is bundled are emitted by the build itself, not copied
# here: vendor/ is wiped on every build, so a copy step in this script would be
# undone by anyone running `npm run build` directly — including CI.
echo "==> building vendor/"
npm run build

echo "==> re-running the suite against the built bundle"
# bundle.test.js mounts vendor/app.js in a DOM. Running the suite again now
# that the artefacts exist is what makes those assertions mean anything: on the
# first pass they tested whatever was in vendor/ beforehand.
npm test

echo
echo "done. vendor/ is:"
ls -la vendor
