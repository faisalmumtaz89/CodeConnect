#!/usr/bin/env bash
# Real new -> old -> new rollback gate for the agent seam (Phase 1).
#
# Builds the ACTUAL previous-release ccd from a git worktree and drives a real
# rollback with both binaries: the new binary migrates a DB (adding the additive
# agent-seam columns), the real old binary opens it and runs its real migrate +
# recovery + a write (its liveness sweep marks a stale live session exited), and
# the new binary reopens and confirms no data loss and that the old write left
# the agent-seam columns intact.
#
# WHAT THIS PROVES / DOES NOT: both the new and the v0.6.0 binary are schema
# version 3 (store.rs `SCHEMA_VERSION`), and the agent seam adds COLUMNS without
# bumping it. So NO `user_version` downgrade actually occurs here — this gate
# proves ADDITIVE-COLUMN round-trip compatibility through the real old binary,
# not a schema downgrade. A genuine downgrade, and the agent-scoped row-level
# isolation that must precede any real Codex row, are the binding Phase-2
# pre-exposure gate (plan amendment A5).
#
# A shell harness on purpose: building a historical binary is a manual/harness
# step, not something `cargo test` should do on every run. It lives under
# ccd/tests/ so it is tracked and conventionally located; cargo ignores non-.rs
# files here, so it never runs as part of the unit suite.
#
# Usage: mac/ccd/tests/new-old-new-real.sh [old_commit]   (default: v0.6.0)
set -euo pipefail

OLD_COMMIT="${1:-8e5b172}"
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
NEW="$REPO/mac/target/debug/ccd"
WT="$(mktemp -d)/cc-old"
H="$(mktemp -d)"
# A free ephemeral port, chosen at run time so the harness never collides with a
# live ccd (or a parallel run) on a fixed port.
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

cleanup() { git -C "$REPO" worktree remove --force "$WT" 2>/dev/null || true; }
trap cleanup EXIT

echo "== building new ccd (working tree) =="
( cd "$REPO/mac" && cargo build -p ccd --bin ccd )

echo "== building old ccd ($OLD_COMMIT) in a worktree =="
git -C "$REPO" worktree add --detach "$WT" "$OLD_COMMIT"
( cd "$WT/mac" && CARGO_TARGET_DIR="$WT/target" cargo build --bin ccd )
OLD="$WT/target/debug/ccd"

# Loopback + a free port + push off, so both binaries do their real migrate and
# recovery without fighting a live ccd, and a fast liveness sweep so the old
# binary actually writes during its short run.
cat > "$H/config.json" <<EOF
{ "ws_port": $PORT, "ws_bind": "127.0.0.1", "ws_loopback": true, "push_enabled": false, "liveness_sweep_secs": 1 }
EOF

# Run a daemon for a few seconds and REJECT an early/abnormal exit. With this
# config the daemon runs until the timeout kills it, so `timeout` returns 124
# (killed while running). Any other code means it exited early — a bind failure
# or a crash — which would make the SQLite assertions below pass over a daemon
# that never really opened the DB. So we insist on 124.
run() {
  local bin="$1" tag="$2" rc=0
  CODECONNECT_HOME="$H" timeout 5 "$bin" > "$H/$tag.log" 2>&1 || rc=$?
  if [ "$rc" -ne 124 ]; then
    echo "FAIL: $tag ccd exited early (rc=$rc) — it did not stay up to do its work:"
    cat "$H/$tag.log"
    exit 1
  fi
}
q() { sqlite3 "$H/events.db" "$1"; }

# **Assert** (never merely print) that user_version is exactly 3 — before the old
# run and after it. If a future new binary bumps the schema to v4, this catches
# the real v3-old-opening-v4 downgrade here instead of passing while the comment
# claims "no downgrade".
assert_uv3() {
  local when="$1" uv
  uv="$(q 'PRAGMA user_version;')"
  [ "$uv" = "3" ] || { echo "FAIL: user_version is $uv, not 3 ($when) — a real schema change is not covered by this additive-only harness; this is the Phase-2 downgrade gate (A5)"; exit 1; }
  echo "  user_version $when: 3"
}

# The authoritative list of columns the agent-seam migration adds
# (store.rs COLUMN_ADDITIONS / create_schema): every one must round-trip.
SEAM_COLS_SESSIONS="agent codex_thread_id codex_socket"
SEAM_COLS_DEVICES="features features_epoch"
assert_seam_columns_exist() {
  local when="$1" table col
  for col in $SEAM_COLS_SESSIONS; do
    q "PRAGMA table_info(sessions);" | awk -F'|' '{print $2}' | grep -qx "$col" \
      || { echo "FAIL: sessions.$col missing $when"; exit 1; }
  done
  for col in $SEAM_COLS_DEVICES; do
    q "PRAGMA table_info(devices);" | awk -F'|' '{print $2}' | grep -qx "$col" \
      || { echo "FAIL: devices.$col missing $when"; exit 1; }
  done
  echo "  all seam columns present $when: sessions($SEAM_COLS_SESSIONS) devices($SEAM_COLS_DEVICES)"
}
# The SESSION seam columns, pipe-joined: nothing on either binary's code path
# touches these, so they must round-trip byte-for-byte through the whole
# new -> old -> new sequence.
session_seam() {
  q "SELECT agent, COALESCE(codex_thread_id,''), COALESCE(codex_socket,'') \
     FROM sessions WHERE session_uid='01K1B3XQ8ZC0DE5FGH7JKMNPQR';"
}
# The DEVICE seam columns, pipe-joined. These round-trip through the OLD binary
# untouched (it does not know the columns), but the NEW binary's startup
# *fail-closed invalidation* deliberately clears a feature set not confirmed
# under the current run's epoch — so after a new-daemon run they are empty. That
# clearing is the correct behaviour, and asserting it here proves the fail-closed
# device path end-to-end against the real binaries.
device_seam() {
  q "SELECT COALESCE(features,''), COALESCE(features_epoch,'') \
     FROM devices WHERE device_id='dev-probe';"
}
SESSION_SEAM_EXPECT="claude|th_probe|/sock_probe"
DEVICE_SEAM_SEEDED='{"agents":["codex"]}|epoch_probe'
DEVICE_SEAM_CLEARED="|"

echo "== 1) new ccd migrates the DB =="
run "$NEW" new1
assert_uv3 "after new migrate"
assert_seam_columns_exist "after new migrate"

echo "== 2) seed a live Claude session + a device, with PROBE values in every seam column =="
# A raw insert (not registration) so we can put a distinct value in each seam
# column and prove the old daemon leaves them byte-for-byte intact. The old
# binary knows none of these columns, so its positional writes must not touch
# them. (A real build never puts Codex identity on a Claude row — this is a
# storage-layer round-trip probe, not a registration.)
q "INSERT INTO sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,claude_session_id,transcript_path,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket)
   VALUES('01K1B3XQ8ZC0DE5FGH7JKMNPQR','cc-9','cc-9','codeconnect','/tmp',NULL,NULL,'live','t','t','claude','th_probe','/sock_probe');
   INSERT INTO devices(device_id,name,token_hash,created_at,features,features_epoch)
   VALUES('dev-probe','iPhone','th-1','t','{\"agents\":[\"codex\"]}','epoch_probe');"
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: session seed did not land: $(session_seam)"; exit 1; }
[ "$(device_seam)" = "$DEVICE_SEAM_SEEDED" ] || { echo "FAIL: device seed did not land: $(device_seam)"; exit 1; }

echo "== 3) REAL old ccd opens it: real migrate + recovery + write =="
run "$OLD" old
grep -iE "liveness|run\(s\)|marked exited" "$H/old.log" | head -4
assert_uv3 "after old daemon"
[ "$(q "SELECT lifecycle FROM sessions;")" = "exited" ] || { echo "FAIL: old daemon did not write"; exit 1; }
assert_seam_columns_exist "after old daemon"
# The old daemon knows none of the seam columns, so its write (marking the
# session exited) must leave EVERY seam value byte-for-byte intact — session and
# device alike.
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: old daemon disturbed a session seam column: $(session_seam)"; exit 1; }
[ "$(device_seam)" = "$DEVICE_SEAM_SEEDED" ] || { echo "FAIL: old daemon disturbed a device seam column: $(device_seam)"; exit 1; }
echo "  seam columns intact through the old daemon: session=$(session_seam) device=$(device_seam)"

echo "== 4) new ccd reopens =="
run "$NEW" new2
assert_uv3 "after new reopen"
[ "$(q "SELECT lifecycle FROM sessions;")" = "exited" ] || { echo "FAIL: lifecycle lost"; exit 1; }
# Session seam columns still round-trip byte-for-byte.
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: a session seam column did not survive the round-trip: $(session_seam)"; exit 1; }
# The device feature set, confirmed under a foreign epoch, is deliberately
# cleared by the new binary's fail-closed startup invalidation — the columns
# survive, the stale eligibility does not.
[ "$(device_seam)" = "$DEVICE_SEAM_CLEARED" ] || { echo "FAIL: the new daemon did not fail-closed-clear the stale device set: $(device_seam)"; exit 1; }
echo "  after new reopen: session seam intact=$(session_seam); stale device set fail-closed-cleared"

echo "PASS: real new->old->new lost no session data; user_version stayed 3 throughout;"
echo "      session seam columns round-tripped byte-for-byte through the old daemon's write;"
echo "      the stale device feature set was fail-closed-cleared by the new daemon."
echo "      (No downgrade: both binaries are schema v3.)"
