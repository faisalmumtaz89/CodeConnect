#!/bin/bash
# A test run observes the machine; it does not change it — proven over the WHOLE
# SUITE, not one writer at a time.
#
# `crate::home_guard` brackets individual tests: the ones that drive a known writer
# snapshot the real `~/.codeconnect` before and assert it unchanged after. That is a
# fence around the writers somebody thought of. It cannot answer the sentence the
# claim is actually made in — "`cargo test` does not write into the operator's home"
# — because the writer that breaks it will be one nobody bracketed, reached from a
# test nobody suspected.
#
# This is that sentence, measured: a recursive inventory of the real home, the whole
# workspace suite, the same inventory again, and a diff. It is the shape that found
# the finding it exists for — 292 of 380 files in the real logs directory were
# supervisor logs from 71 dead test pids, while a one-filename in-process guard
# passed.
#
# Run from anywhere. Usage:
#   codeconnect/tests/real-home-unchanged.sh [extra cargo test args...]
#
# Exit 0 = the home is byte-for-byte the same set of files, sizes and mtimes.
# Exit 1 = something changed, and the difference is printed.
# Exit 2 = the suite itself failed (the home comparison is still reported).
#
# What is deliberately not compared: a live `ccd` writes its own database, WAL,
# socket and two logs into this same directory on its own schedule. Those are the
# daemon's, not the suite's, and they are excluded BY EXACT NAME — the same list
# `home_guard::is_the_daemons_own` uses, kept in one shape in each place because a
# shell and a Rust matcher cannot share a literal.

set -u

HOME_DIR="${CODECONNECT_HOME:-$HOME/.codeconnect}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/cc-real-home-XXXXXX")"
BEFORE="$WORK/before.txt"
AFTER="$WORK/after.txt"
SUITE="$WORK/suite.log"

# The crate root, from this script's own location, so the script works from any cwd.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAC_DIR="$(cd "$HERE/../.." && pwd)"

inventory() {
    # name, size, mtime — the same three facts the in-process guard compares. `find`
    # rather than a shell glob so every depth is reached, and `-print0` so a name
    # with a space or a newline in it cannot split a line.
    if [ ! -d "$HOME_DIR" ]; then
        echo "(no home directory at $HOME_DIR)"
        return 0
    fi
    find "$HOME_DIR" -type f -print0 2>/dev/null \
        | while IFS= read -r -d '' f; do
            rel="${f#"$HOME_DIR"/}"
            base="$(basename "$rel")"
            case "$rel" in
                ccd.sock|logs/ccd.out.log|logs/ccd.err.log) continue ;;
                events.db|events.db-wal|events.db-shm|events.db-journal) continue ;;
            esac
            [ "$base" = ".DS_Store" ] && continue
            stat -f '%N|%z|%m' "$f"
        done | LC_ALL=C sort
}

# A directory this user cannot read is a hole in the fence, not a hole in the home:
# the inventory would silently be of the part `find` could reach. Same rule as the
# Rust walk, which errors rather than skipping.
unreadable_dirs() {
    [ -d "$HOME_DIR" ] || return 0
    # `test -r` rather than `find ! -readable`: the latter is a GNU/bfs extension and
    # macOS ships BSD find, where it is an "unknown primary" — a script that relied on
    # it would fail loudly on this machine and, worse, could be "fixed" by dropping the
    # check. `find` still NAMES a directory it cannot descend into, so listing the
    # directories and testing each one is both portable and complete.
    find "$HOME_DIR" -type d -print0 2>/dev/null \
        | while IFS= read -r -d '' d; do
            [ -r "$d" ] && [ -x "$d" ] || printf '%s\n' "$d"
        done
}

echo "== real home: $HOME_DIR"
BLIND="$(unreadable_dirs)"
if [ -n "$BLIND" ]; then
    echo "== CANNOT PROVE ANYTHING: these directories are unreadable, so an inventory"
    echo "   of this home is an inventory of the part that happens to be reachable:"
    echo "$BLIND"
    exit 1
fi
inventory > "$BEFORE"
echo "== files inventoried before: $(wc -l < "$BEFORE" | tr -d ' ')"

echo "== cargo test --workspace --no-fail-fast -- --test-threads=1 $*"
( cd "$MAC_DIR" && cargo test --workspace --no-fail-fast -- --test-threads=1 "$@" ) \
    > "$SUITE" 2>&1
SUITE_RC=$?
grep -E '^test result' "$SUITE" | LC_ALL=C sort | uniq -c
echo "== suite exit: $SUITE_RC (full log: $SUITE)"

inventory > "$AFTER"
echo "== files inventoried after:  $(wc -l < "$AFTER" | tr -d ' ')"

if diff -u "$BEFORE" "$AFTER" > "$WORK/diff.txt"; then
    echo "== REAL HOME UNCHANGED: the suite created, removed and rewrote nothing in $HOME_DIR"
    # The evidence is KEPT on the passing path too. An earlier version deleted it,
    # which meant the one run somebody would want to cite — the green one — was the
    # only run with no log to cite. A pass is a measurement, not a formality.
    echo "== evidence kept in $WORK (suite log: $SUITE)"
    [ "$SUITE_RC" -eq 0 ] || { echo "== but the suite itself failed"; exit 2; }
    exit 0
fi

echo "== REAL HOME CHANGED. The suite wrote into the operator's own directory:"
cat "$WORK/diff.txt"
echo "== evidence kept in $WORK"
exit 1
