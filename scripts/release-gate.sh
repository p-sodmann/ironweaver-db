#!/bin/sh
# The release gate (step 17e, ADR 0060): what a version tag needs before
# anything is built.
#
#   scripts/release-gate.sh [--rehearsal] <tag> [<root>]
#
# For the tag v<version>, in the repository at <root> (default .):
#
# - <version> is semver, with at most a pre-release suffix -alpha.N,
#   -beta.N or -rc.N (the ones with a Python spelling: aN, bN, rcN);
# - it is the workspace version (`[workspace.package]` in Cargo.toml) and
#   the version of the workspace's crates in Cargo.lock;
# - its release fixture crates/iwdb/tests/fixtures/releases/<version>/
#   exists (crates/iwdb/tests/release_fixture.rs writes it);
# - CHANGELOG.md has a `## [<version>]` section, not empty.
#
# Every failed check is printed; the exit code is 1 if any failed. With
# --rehearsal (the workflow started by hand) failures are warnings and the
# exit code is 0. It prints version=, python_version= and prerelease=
# (true or false), and appends them to $GITHUB_OUTPUT when it is set.

set -u

rehearsal=false
if [ "${1:-}" = "--rehearsal" ]; then
    rehearsal=true
    shift
fi
if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 [--rehearsal] <tag> [<root>]" >&2
    exit 2
fi
tag=$1
root=${2:-.}
failed=0

fail() {
    if [ "$rehearsal" = true ]; then
        echo "::warning::release gate (rehearsal): $1"
    else
        echo "::error::release gate: $1"
    fi
    failed=1
}

version=${tag#v}
if [ "$version" = "$tag" ]; then
    fail "the tag $tag doesn't start with v"
fi

# X.Y.Z without leading zeros, then an optional -alpha.N, -beta.N or -rc.N
python_version=
prerelease=false
if echo "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-(alpha|beta|rc)\.(0|[1-9][0-9]*))?$'; then
    case "$version" in
        *-*)
            prerelease=true
            core=${version%%-*}
            pre=${version#*-}
            kind=${pre%.*}
            number=${pre#*.}
            case "$kind" in
                alpha) python_version="${core}a${number}" ;;
                beta) python_version="${core}b${number}" ;;
                rc) python_version="${core}rc${number}" ;;
            esac
            ;;
        *) python_version=$version ;;
    esac
else
    fail "the version $version isn't X.Y.Z or X.Y.Z-alpha.N, -beta.N or -rc.N (ADR 0060)"
fi

# The version in [workspace.package]
workspace=$(awk '
    /^\[/ { section = $0; next }
    section == "[workspace.package]" && $1 == "version" {
        sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }' "$root/Cargo.toml" 2>/dev/null)
if [ -z "$workspace" ]; then
    fail "no version in [workspace.package] of $root/Cargo.toml"
elif [ "$workspace" != "$version" ]; then
    fail "the tag is $tag, but the workspace version (Cargo.toml) is $workspace"
fi

# The workspace's crates in Cargo.lock (a `cargo build --locked` after a
# version bump without the lock file would fail later, in every build job)
locked=$(awk '
    $0 == "[[package]]" { name = "" }
    $1 == "name" { name = $3 }
    $1 == "version" && name == "\"iwdb-server\"" { v = $3; gsub(/"/, "", v); print v; exit }
    ' "$root/Cargo.lock" 2>/dev/null)
if [ "$locked" != "$version" ]; then
    fail "Cargo.lock has iwdb-server ${locked:-(missing)}, not $version: run cargo build and commit Cargo.lock"
fi

fixture="crates/iwdb/tests/fixtures/releases/$version"
if [ ! -f "$root/$fixture/expected.txt" ]; then
    fail "no release fixture $fixture/: write it with \`cargo test -p iwdb --test release_fixture -- --ignored generate_fixture\` and commit it (documentation/releasing.md)"
fi

# The release's notes are this section (scripts/release-notes.sh)
if ! "$(dirname "$0")/release-notes.sh" "$version" "$root/CHANGELOG.md" > /dev/null 2>&1; then
    fail "CHANGELOG.md has no \"## [$version]\" section, or it is empty: move [Unreleased] under it"
fi

echo "version=$version"
echo "python_version=$python_version"
echo "prerelease=$prerelease"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
    {
        echo "version=$version"
        echo "python_version=$python_version"
        echo "prerelease=$prerelease"
    } >> "$GITHUB_OUTPUT"
fi

if [ "$failed" = 1 ] && [ "$rehearsal" = false ]; then
    exit 1
fi
exit 0
