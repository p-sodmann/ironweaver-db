#!/bin/sh
# Tests of scripts/release-gate.sh and scripts/release-notes.sh (step 17e),
# on small repository trees in a temporary directory. CI runs it
# (ci.yml#release-gate), and the release workflow before its gate.
#
#   sh scripts/release-gate-test.sh

set -u

here=$(cd "$(dirname "$0")" && pwd)
gate="$here/release-gate.sh"
notes="$here/release-notes.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# A tree at version $1 (Cargo.toml and Cargo.lock), with a fixture and a
# CHANGELOG section for $2 (empty: none)
tree() {
    dir="$work/$1-$2"
    rm -rf "$dir"
    mkdir -p "$dir"
    cat > "$dir/Cargo.toml" <<EOF
[workspace]
members = ["crates/iwdb"]

[workspace.package]
version = "$1"
edition = "2024"

[workspace.dependencies]
version-of-something = { version = "9.9.9" }
EOF
    cat > "$dir/Cargo.lock" <<EOF
version = 4

[[package]]
name = "iwdb"
version = "$1"

[[package]]
name = "iwdb-server"
version = "$1"
dependencies = [
 "iwdb",
]
EOF
    {
        echo "# Changelog"
        echo
        echo "## [Unreleased]"
        echo
        if [ -n "$2" ]; then
            echo "## [$2] - 2026-10-20"
            echo
            echo "### Added"
            echo "- Everything."
            echo
        fi
        echo "## [0.0.1] - 2026-01-01"
        echo
        echo "- The first one."
    } > "$dir/CHANGELOG.md"
    if [ -n "$2" ]; then
        mkdir -p "$dir/crates/iwdb/tests/fixtures/releases/$2"
        echo "namespace default" > "$dir/crates/iwdb/tests/fixtures/releases/$2/expected.txt"
    fi
    echo "$dir"
}

# expect <0|1> <what> <pattern in the output or ""> -- <command...>
expect() {
    code=$1 what=$2 pattern=$3
    shift 4
    output=$("$@" 2>&1)
    got=$?
    if [ "$got" != "$code" ]; then
        echo "FAIL $what: exit $got, expected $code"
        echo "$output" | sed 's/^/    /'
        failures=$((failures + 1))
    elif [ -n "$pattern" ] && ! echo "$output" | grep -Fq -- "$pattern"; then
        echo "FAIL $what: no \"$pattern\" in"
        echo "$output" | sed 's/^/    /'
        failures=$((failures + 1))
    else
        echo "ok   $what"
    fi
}

ok=$(tree 1.0.0 1.0.0)
expect 0 "a release passes" "prerelease=false" -- sh "$gate" v1.0.0 "$ok"
expect 0 "a release's Python version is its version" "python_version=1.0.0" -- sh "$gate" v1.0.0 "$ok"

rc=$(tree 1.0.0-rc.1 1.0.0-rc.1)
expect 0 "a pre-release passes" "prerelease=true" -- sh "$gate" v1.0.0-rc.1 "$rc"
expect 0 "rc.N is rcN in Python" "python_version=1.0.0rc1" -- sh "$gate" v1.0.0-rc.1 "$rc"
alpha=$(tree 2.1.0-alpha.3 2.1.0-alpha.3)
expect 0 "alpha.N is aN in Python" "python_version=2.1.0a3" -- sh "$gate" v2.1.0-alpha.3 "$alpha"
beta=$(tree 2.1.0-beta.0 2.1.0-beta.0)
expect 0 "beta.N is bN in Python" "python_version=2.1.0b0" -- sh "$gate" v2.1.0-beta.0 "$beta"

expect 1 "a tag that isn't the workspace version fails" "the tag is v9.9.9, but the workspace version (Cargo.toml) is 1.0.0" -- sh "$gate" v9.9.9 "$ok"
expect 1 "a tag without v fails" "doesn't start with v" -- sh "$gate" 1.0.0 "$ok"
expect 1 "a pre-release suffix of another kind fails" "isn't X.Y.Z" -- sh "$gate" v1.0.0-preview1 "$ok"
expect 1 "a leading zero fails" "isn't X.Y.Z" -- sh "$gate" v1.01.0 "$ok"

nofixture=$(tree 1.0.0 1.0.0)
rm -rf "$nofixture/crates/iwdb/tests/fixtures/releases/1.0.0"
expect 1 "a missing release fixture fails" "no release fixture crates/iwdb/tests/fixtures/releases/1.0.0/" -- sh "$gate" v1.0.0 "$nofixture"

nochangelog=$(tree 1.0.0 1.0.0)
sed 's/^## \[1.0.0\] - 2026-10-20$/## [1.0.0-rc.1] - 2026-10-20/' "$nochangelog/CHANGELOG.md" > "$nochangelog/c" && mv "$nochangelog/c" "$nochangelog/CHANGELOG.md"
expect 1 "a missing CHANGELOG section fails (1.0.0-rc.1 isn't 1.0.0)" 'CHANGELOG.md has no "## [1.0.0]" section' -- sh "$gate" v1.0.0 "$nochangelog"

stale=$(tree 1.0.0 1.0.0)
sed 's/^version = "1.0.0"$/version = "0.9.0"/' "$stale/Cargo.lock" > "$stale/l" && mv "$stale/l" "$stale/Cargo.lock"
expect 1 "a stale Cargo.lock fails" "Cargo.lock has iwdb-server 0.9.0, not 1.0.0" -- sh "$gate" v1.0.0 "$stale"

bare=$(tree 0.1.0 "")
expect 1 "every failed check is reported (fixture)" "no release fixture" -- sh "$gate" v0.1.0 "$bare"
expect 1 "every failed check is reported (changelog)" 'no "## [0.1.0]" section' -- sh "$gate" v0.1.0 "$bare"
expect 0 "a rehearsal only warns" "::warning::release gate (rehearsal): no release fixture" -- sh "$gate" --rehearsal v0.1.0 "$bare"

expect 0 "the notes are the version's section" "- Everything." -- sh "$notes" 1.0.0 "$ok/CHANGELOG.md"
output=$(sh "$notes" 1.0.0 "$ok/CHANGELOG.md")
if [ "$output" = "$(printf '### Added\n- Everything.')" ]; then
    echo "ok   the notes stop at the next section, without the heading and blank lines"
else
    echo "FAIL the notes are"
    echo "$output" | sed 's/^/    /'
    failures=$((failures + 1))
fi
expect 1 "an empty section has no notes" "is empty" -- sh "$notes" Unreleased "$ok/CHANGELOG.md"
expect 1 "a missing section has no notes" "no \"## [3.0.0]\" section" -- sh "$notes" 3.0.0 "$ok/CHANGELOG.md"
expect 1 "1.0.0 isn't 1.0.0-rc.1" "" -- sh "$notes" 1.0.0 "$rc/CHANGELOG.md"

if [ "$failures" != 0 ]; then
    echo "$failures failed"
    exit 1
fi
echo "all passed"
