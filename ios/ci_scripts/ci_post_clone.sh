#!/bin/sh
#
# Xcode Cloud runs this after cloning, before it builds.
#
# **Why it exists.** App Store Connect rejects an upload whose build number it
# has already seen, and the project pins `CURRENT_PROJECT_VERSION = 1`. Left
# alone, the first upload succeeds and every one after it fails with a message
# about the build number, long after the build itself has passed.
#
# `CI_BUILD_NUMBER` is Xcode Cloud's own monotonically increasing counter, so
# using it means the number can never repeat and never has to be remembered by a
# human. The marketing version (`MARKETING_VERSION`) is deliberately *not*
# touched: what version this is, is a decision, not a side effect of CI.
#
# Outside Xcode Cloud this script does nothing, so a local `xcodebuild archive`
# still produces whatever the project says.

set -eu

if [ -z "${CI_BUILD_NUMBER:-}" ]; then
    echo "not an Xcode Cloud build — leaving the build number alone"
    exit 0
fi

project="${CI_PRIMARY_REPOSITORY_PATH:-$(cd "$(dirname "$0")/../.." && pwd)}/ios/CodeConnect.xcodeproj/project.pbxproj"

if [ ! -f "$project" ]; then
    echo "::error::cannot find project.pbxproj at $project"
    exit 1
fi

# Every configuration, so the archive and its test targets agree. `-i ''` is the
# BSD sed form; the runner is macOS.
sed -i '' "s/CURRENT_PROJECT_VERSION = [0-9][0-9]*;/CURRENT_PROJECT_VERSION = ${CI_BUILD_NUMBER};/g" "$project"

echo "build number set to ${CI_BUILD_NUMBER}"
grep -m1 -o "CURRENT_PROJECT_VERSION = [0-9]*;" "$project"
