#!/bin/sh
# A release's notes (step 17e): the CHANGELOG section of a version.
#
#   scripts/release-notes.sh <version> [<changelog>]
#
# Prints the lines under `## [<version>]` (or `## [Unreleased]`), up to the
# next `## ` heading or the link references at the end, without the heading
# and without leading or trailing blank lines. Exits 1 if there is no such
# section or it is empty.

set -u

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 <version> [<changelog>]" >&2
    exit 2
fi
version=$1
changelog=${2:-CHANGELOG.md}

awk -v h="## [$version]" '
    index($0, h) == 1 && (length($0) == length(h) || substr($0, length(h) + 1, 1) == " ") {
        inside = 1; found = 1; next
    }
    inside && (/^## / || /^\[[^]]+\]: /) { inside = 0 }
    inside {
        if ($0 ~ /^[ \t]*$/) { blank++; next }
        if (printed) { for (i = 0; i < blank; i++) print "" }
        blank = 0; printed = 1; print
    }
    END {
        if (!found) { print "no \"" h "\" section in the changelog" > "/dev/stderr"; exit 1 }
        if (!printed) { print "the \"" h "\" section is empty" > "/dev/stderr"; exit 1 }
    }' "$changelog"
