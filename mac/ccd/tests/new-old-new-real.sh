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
# WHAT THIS PROVES. Two things, and the second one is the Phase-2 pre-exposure
# gate A5 always pointed at:
#
#   * **Additive-column round-trip.** The seam columns the new binary writes
#     (`sessions.agent`, `codex_thread_id`, `codex_socket`;
#     `devices.features`, `features_epoch`) come back byte-for-byte through the
#     real old daemon's own write, because it names none of them.
#
#   * **A REAL schema downgrade, and agent-scoped isolation under it.** The new
#     binary is now `SCHEMA_VERSION` 4 (agent-scoped session storage); v0.6.0 is
#     3. So a genuine v3-binary-opens-a-v4-database downgrade happens here, and
#     the `user_version` really is driven 4 -> 3 -> 4. That is measured, not
#     assumed: v0.6.0 reads `user_version`, ignores what it finds, and writes 3
#     back unconditionally — which is also exactly why a version fence could
#     never have protected anything, and why the isolation does not rest on one.
#     It rests on the table name: v0.6.0 contains no statement that names
#     `codex_sessions` — and, for the one path that could still have put a Codex
#     run into the table it DOES name, on the `sessions_refuse_codex_shadow`
#     trigger, which lives in the schema and so is the one part of this version a
#     rolled-back binary keeps and runs against itself (step 7).
#     Steps 3 and 5 below drive the real old daemon AND its
#     real `codeconnect sessions prune` — the two paths measured to enumerate and
#     then DELETE a Codex row when one lived in the shared table — and require
#     that Codex state come through byte-for-byte, while the old daemon's Claude
#     handling stays completely intact.
#
# HOW THE MUTATION CLAIM IS MEASURED. Each old-daemon run is bracketed on its
# own: the Codex rows' exact bytes are captured immediately before that run
# starts and compared immediately after it ends, so no window ever contains a
# write by anything but the old binary. The first bracket is the load-bearing
# one — it opens at the seed, while the Codex row is still `live`, so it can see
# the mutation that matters most, the old daemon's liveness sweep flipping a
# live Codex run to `exited`. Two other writers touch that row legitimately and
# are deliberately kept outside every window: the NEW daemon's own sweep (there
# is no tmux pane `cx-1`, so it correctly ends the run and files a
# `session_end`), and this harness's pre-prune `UPDATE ... lifecycle='exited'`,
# which is undone before its window is closed.
#
# The non-enumeration checks assert two things per command, not one: that the
# old CLI actually SUCCEEDED, and that its output names nothing Codex. A crashed
# or silent CLI matches nothing either, and would otherwise be scored as proof.
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
# Honour CARGO_TARGET_DIR: with it set, `cargo build` below puts the binary
# there and not in the in-repo path, and a hardcoded `mac/target` would run
# whatever stale binary happened to be sitting in it — or none at all.
#
# It has to be made ABSOLUTE first, and absolute against the right directory.
# `cargo build` runs with cwd `$REPO/mac`, so cargo resolves a relative
# CARGO_TARGET_DIR against `$REPO/mac`; this script resolves it against
# whatever directory the caller happened to be in. Those are two different
# paths, and the gap is silent and dangerous: cargo writes the fresh binary to
# one of them while `$NEW` runs a stale binary sitting at the other. So resolve
# it once, here, against cargo's cwd, and re-export it so the child cargo is
# given the same absolute path we will run from. Doing it before ANY use is the
# point — every later reference must see the resolved value.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  case "$CARGO_TARGET_DIR" in
    /*) ;;
    *) CARGO_TARGET_DIR="$REPO/mac/$CARGO_TARGET_DIR" ;;
  esac
  export CARGO_TARGET_DIR
  TARGET_DIR="$CARGO_TARGET_DIR"
else
  # Unset: cargo's own default, which is also relative to cargo's cwd.
  TARGET_DIR="$REPO/mac/target"
fi
NEW="$TARGET_DIR/debug/ccd"
# The working-tree CLI, and with it the working-tree supervisor. Step 7(f) drives
# the REAL supervisor against the REAL old daemon, and `serve_once` /
# `report_exit` are only reachable by running the binary that owns them — a
# hand-written frame down the socket exercises neither.
NEWCC="$TARGET_DIR/debug/codeconnect"
WT="$(mktemp -d)/cc-old"
H="$(mktemp -d)"
# The Codex run used by phase 3b. A ULID ending CX so it is obvious in a log.
CX=01K1B3XQ8ZC0DE5FGH7JKMNPCX
# Its thread id, in a variable for the same reason the uid is: the seed below
# and the non-enumeration regexes must be the SAME literal. Hardcoding it twice
# lets the seed change while the regex goes on searching for a string nothing
# ever writes — a check that can no longer fail.
CX_THREAD=th_ABC123
# A free ephemeral port, chosen at run time so the harness never collides with a
# live ccd (or a parallel run) on a fixed port.
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"

# Every background process this harness owns dies with it. The daemons and the
# supervisors are killed explicitly at the end of the step that started them; this
# is the net under an early `exit`, and it matters most for the supervisors, which
# are the only children here that would otherwise outlive the script's own home
# directory and go on reconnecting to a socket nobody is listening on.
cleanup() {
  # Each pid guarded on being SET, never defaulted to `0`: `kill 0` is not a
  # no-op, it signals the whole process group — this script, the shell that
  # started it, and whatever else shares that group. An unset variable here
  # means the process was never started, so there is nothing to kill.
  local pid
  for pid in "${SUPPID:-}" "${SUPXPID:-}" "${OLDPID:-}"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  done
  git -C "$REPO" worktree remove --force "$WT" 2>/dev/null || true
}
trap cleanup EXIT

echo "== building new ccd + codeconnect (working tree) =="
# `codeconnect` too, and for the same reason the OLD one is built below: half of
# what this gate measures is only reachable by running a real binary. Step 7(f)
# runs this supervisor's live path and its exit path against the real v0.6.0
# daemon.
( cd "$REPO/mac" && cargo build -p ccd --bin ccd -p codeconnect --bin codeconnect )

echo "== building old ccd + codeconnect ($OLD_COMMIT) in a worktree =="
# `codeconnect` too, because the destructive half of the gate is the OLD
# `codeconnect sessions prune` — the path measured to delete a Codex row and all
# its events when one lived in the shared table. It is only reachable over IPC,
# so the old CLI has to be real as well.
git -C "$REPO" worktree add --detach "$WT" "$OLD_COMMIT"
( cd "$WT/mac" && CARGO_TARGET_DIR="$WT/target" cargo build --bin ccd --bin codeconnect )
OLD="$WT/target/debug/ccd"
OLDCC="$WT/target/debug/codeconnect"

# Loopback + a free port + push off, so both binaries do their real migrate and
# recovery without fighting a live ccd, and a fast liveness sweep so the old
# binary actually writes during its short run.
cat > "$H/config.json" <<EOF
{ "ws_port": $PORT, "ws_bind": "127.0.0.1", "ws_loopback": true, "push_enabled": false, "liveness_sweep_secs": 1 }
EOF

# How long each daemon gets. It is a named constant because it is load-bearing,
# not cosmetic: step 3 asserts the old daemon's liveness sweep actually ran, and
# that sweep needs `EXIT_CONFIRMATIONS` consecutive observations at
# `liveness_sweep_secs` apart before it will end a run — a second look is what
# stops a tmux server restart producing a false exit. At 5s that left roughly two
# seconds of headroom, which is enough on an idle machine and measured NOT to be
# under load: run concurrently with the `ccd` suite, step 3 failed with "old
# daemon did not write" because the sweep had not reached its second observation
# before the kill. The assertion was right and the window was too tight. 8s
# restores the headroom; the cost is three seconds per daemon run and the
# assertions are unchanged.
DAEMON_WINDOW=8

# Run a daemon for a few seconds and REJECT an early/abnormal exit. With this
# config the daemon runs until the timeout kills it, so `timeout` returns 124
# (killed while running). Any other code means it exited early — a bind failure
# or a crash — which would make the SQLite assertions below pass over a daemon
# that never really opened the DB. So we insist on 124.
run() {
  local bin="$1" tag="$2" rc=0
  CODECONNECT_HOME="$H" timeout "$DAEMON_WINDOW" "$bin" > "$H/$tag.log" 2>&1 || rc=$?
  if [ "$rc" -ne 124 ]; then
    echo "FAIL: $tag ccd exited early (rc=$rc) — it did not stay up to do its work:"
    cat "$H/$tag.log"
    exit 1
  fi
}
q() { sqlite3 "$H/events.db" "$1"; }

# Every string that identifies the seeded Codex run: its tmux/session name, its
# uid, and its thread id. One regex, built once, so a non-enumeration check
# cannot quietly cover less than it claims to.
codex_identifiers() { printf 'cx-1|%s|%s' "$CX" "$CX_THREAD"; }

# Run an OLD CLI command and assert BOTH halves of non-enumeration:
#   (a) the command SUCCEEDED, and
#   (b) its output names nothing Codex.
# Both, because a crashed CLI, a refused IPC connection or an empty response
# also produce no match — and scoring silence as non-enumeration passes the gate
# on a command that never ran, which proves precisely nothing. A broken old CLI
# has to fail this gate loudly; it is exactly the case the gate cannot judge.
#
# The status is captured with `|| rc=$?` rather than tested with `&&`/a bare
# assignment: under `set -e` a failing command substitution would abort the
# script before we could report which command failed and why.
assert_old_cli_clean() {
  local label="$1"; shift
  local out rc=0
  out="$(CODECONNECT_HOME="$H" "$@" 2>&1)" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "FAIL: old \`$label\` did not run (rc=$rc) — a command that failed cannot show non-enumeration:"
    printf '%s\n' "$out"
    kill "${OLDPID:-0}" 2>/dev/null || true
    exit 1
  fi
  if printf '%s\n' "$out" | grep -qiE "$(codex_identifiers)"; then
    echo "FAIL: old \`$label\` enumerated Codex state:"
    printf '%s\n' "$out" | grep -inE "$(codex_identifiers)"
    kill "${OLDPID:-0}" 2>/dev/null || true
    exit 1
  fi
  echo "  (1) old \`$label\` ran (rc=0) and named no Codex session, thread or uid"
}

# **Assert** (never merely print) the version at each step. The expected value
# is a parameter because the whole point is that it CHANGES: 4 after a new
# migrate, 3 after the old daemon has written its own number back over ours, 4
# again after the new binary reopens. Each of those three is a claim about a
# different binary's behaviour, and asserting one constant would hide two of
# them.
assert_uv() {
  local want="$1" when="$2" uv
  uv="$(q 'PRAGMA user_version;')"
  [ "$uv" = "$want" ] || { echo "FAIL: user_version is $uv, not $want ($when)"; exit 1; }
  echo "  user_version $when: $uv"
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
# The DEVICE seam columns, pipe-joined. They round-trip byte-for-byte through
# BOTH binaries: the old one because it does not know the columns, the new one
# because **nothing in this phase writes them at all**. This harness was written
# in Phase 1, when a startup *fail-closed invalidation* cleared a set not stamped
# by the current run, and it asserted that clearing; Phase 2e-5 deleted the whole
# device-feature write side — machinery for an input no shipping client can send
# — so `Store::set_device_features` is `allow(dead_code)` outside tests and no
# production path can clear anything. Asserting the clearing outlived the code
# that did it, and this harness has been failing on that line since.
#
# The fail-closed property did not go away, it moved to the READ: a set stamped
# with another run's epoch decodes as `DeviceFeatures::Unconfirmable` and
# authorizes nothing, which `a_stored_feature_set_is_only_this_runs_word_if_this_run_stamped_it`
# pins. So the honest assertion here is survival of the bytes, exactly as for the
# session seam — `epoch_probe` is not any real run's epoch, so the surviving set
# grants nothing.
device_seam() {
  q "SELECT COALESCE(features,''), COALESCE(features_epoch,'') \
     FROM devices WHERE device_id='dev-probe';"
}
SESSION_SEAM_EXPECT="claude|th_probe|/sock_probe"
DEVICE_SEAM_SEEDED='{"agents":["codex"]}|epoch_probe'

echo "== 1) new ccd migrates the DB =="
run "$NEW" new1
assert_uv 4 "after new migrate"
assert_seam_columns_exist "after new migrate"
for object in codex_sessions all_sessions; do
  [ "$(q "SELECT COUNT(*) FROM sqlite_master WHERE name='$object';")" = "1" ] \
    || { echo "FAIL: $object was not built by the new binary"; exit 1; }
done
echo "  agent-scoped storage built: codex_sessions, all_sessions"

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

echo "== 2b) seed a LIVE Codex run in codex_sessions, with events =="
# Where a real Codex registration will put it. The old binary has no statement
# that names this table, so nothing it does can reach the row.
q "INSERT INTO codex_sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,claude_session_id,transcript_path,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket)
   VALUES('$CX','cx-1','cx-1','codeconnect','/work/codex',NULL,NULL,'live','t','t','codex','$CX_THREAD','/tmp/cch.x/ccd.sock');
   INSERT INTO events(session_uid,session_id,seq,ts,kind,payload,source,source_event_id)
   VALUES('$CX','cx-1',1,'t','tool_call','{}','hook','x1'),('$CX','cx-1',2,'t','tool_call','{}','hook','x2');"
# The complete durable Codex state, hashed. Anything the old daemon or the old
# CLI touches changes this.
codex_state() { q "SELECT * FROM codex_sessions ORDER BY session_uid; SELECT * FROM events WHERE session_uid='$CX' ORDER BY seq;"; }
[ -n "$(codex_state)" ] || { echo "FAIL: the Codex seed did not land"; exit 1; }
echo "  seeded: codex_sessions=$(q 'SELECT COUNT(*) FROM codex_sessions;') codex events=$(q "SELECT COUNT(*) FROM events WHERE session_uid='$CX';") lifecycle=$(q "SELECT lifecycle FROM codex_sessions WHERE session_uid='$CX';")"

# --- baseline for the FIRST old-daemon window --------------------------------
# Taken HERE, immediately after the seed and immediately before the old daemon
# starts, while the Codex row is still `live`. That timing is the whole measure-
# ment. The mutation this gate exists to rule out is the old daemon's liveness
# sweep flipping a LIVE Codex run to `exited` — the same sweep it is about to
# run, correctly, over the Claude row. A baseline taken any later is compared
# against an already-exited row and cannot see that mutation at all; it would
# assert only that an exited row stayed exited, which is not the claim.
#
# Every comparison in this script spans exactly one old-daemon window: bytes
# captured immediately before that daemon starts, compared immediately after it
# ends. Nothing else may run inside a window, because two other writers legit-
# imately touch this row and neither is the old binary — see step 5.
CODEX_BEFORE_OLD1="$(codex_state)"

echo "== 3) REAL old ccd opens it: real migrate + recovery + write =="
run "$OLD" old
# A diagnostic print, not an assertion — so it must not be able to fail the run.
# Under `pipefail` a grep that matches nothing returns 1, `set -e` takes that as
# the script's verdict, and the harness dies here with no message at all. The
# lines it looks for are the liveness sweep's, which land around 2s into a 5s
# window, so "nothing matched" is a timing accident and not a finding — and the
# assertions that follow are what actually decide whether the old daemon wrote.
grep -iE "liveness|run\(s\)|marked exited" "$H/old.log" | head -4 || true
assert_uv 3 "after old daemon"
[ "$(q "SELECT lifecycle FROM sessions;")" = "exited" ] || { echo "FAIL: old daemon did not write"; exit 1; }
assert_seam_columns_exist "after old daemon"
# The old daemon knows none of the seam columns, so its write (marking the
# session exited) must leave EVERY seam value byte-for-byte intact — session and
# device alike.
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: old daemon disturbed a session seam column: $(session_seam)"; exit 1; }
[ "$(device_seam)" = "$DEVICE_SEAM_SEEDED" ] || { echo "FAIL: old daemon disturbed a device seam column: $(device_seam)"; exit 1; }
echo "  seam columns intact through the old daemon: session=$(session_seam) device=$(device_seam)"

# (2) MUTATION, first old-daemon window — closed here. Between the baseline
# captured after the seed and this line, the ONLY thing that ran is this old
# daemon, so any difference at all is its write. And this is the window where
# the row was `live`: the old daemon just swept a stale live session to
# `exited` (asserted above, on the Claude row) and must not have reached the
# Codex row while doing it. Asserting the lifecycle separately keeps the failure
# legible — a whole-row diff would report the same mutation as an opaque blob.
[ "$(codex_state)" = "$CODEX_BEFORE_OLD1" ] || {
  echo "FAIL: the old daemon mutated Codex state"
  echo "  BEFORE: $CODEX_BEFORE_OLD1"; echo "  AFTER : $(codex_state)"; exit 1; }
[ "$(q "SELECT lifecycle FROM codex_sessions WHERE session_uid='$CX';")" = "live" ] \
  || { echo "FAIL: the old daemon's liveness sweep marked the LIVE Codex run exited"; exit 1; }
echo "  (2) the LIVE Codex run is byte-for-byte untouched by the old daemon, and still live"

echo "== 4) new ccd reopens =="
run "$NEW" new2
assert_uv 4 "after new reopen"
[ "$(q "SELECT lifecycle FROM sessions;")" = "exited" ] || { echo "FAIL: lifecycle lost"; exit 1; }
# The Codex run came through the downgrade untouched and did not leak into the
# table the old binary sweeps.
[ "$(q "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the Codex run did not survive the downgrade"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM sessions WHERE agent <> 'claude';")" = "0" ] \
  || { echo "FAIL: a Codex row is in the shared sessions table"; exit 1; }
# Session seam columns still round-trip byte-for-byte.
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: a session seam column did not survive the round-trip: $(session_seam)"; exit 1; }
# And so do the device seam columns: nothing in this phase writes them, and the
# foreign-epoch stamp is what makes the surviving set authorize nothing.
[ "$(device_seam)" = "$DEVICE_SEAM_SEEDED" ] || { echo "FAIL: a device seam column did not survive the round-trip: $(device_seam)"; exit 1; }
echo "  after new reopen: session seam intact=$(session_seam); device seam intact=$(device_seam) (foreign epoch ⇒ Unconfirmable ⇒ authorizes nothing)"

echo "== 5) AGENT-SCOPED ISOLATION: the old daemon and its real prune, live =="
# A SECOND baseline, re-taken HERE rather than reused from the seed, because a
# window must contain the old binary and only the old binary. Since the first
# window closed, the NEW daemon ran twice, and it legitimately swept the seeded
# run: there is no tmux pane called `cx-1`, so it marked the row `exited` and
# filed a `session_end`. That is the current daemon doing its job over a fleet
# it owns — precisely the behaviour the old one must not have — and carrying the
# earlier bytes across it would report our own correct write as the old binary's
# mutation. The live->exited transition is not lost by re-baselining: it was
# already measured, in the first window, where the row was still live.
CODEX_BEFORE_OLD2="$(codex_state)"
CODEX_LIFECYCLE_BEFORE="$(q "SELECT lifecycle FROM codex_sessions WHERE session_uid='$CX';")"
CODEX_EVENTS_BEFORE="$(q "SELECT COUNT(*) FROM events WHERE session_uid='$CX';")"
echo "  codex baseline: lifecycle=$CODEX_LIFECYCLE_BEFORE events=$CODEX_EVENTS_BEFORE"

# A long-running old daemon this time, not a `timeout 5` one, because the
# destructive path is the old CLI talking to it over IPC.
CODECONNECT_HOME="$H" "$OLD" > "$H/old-live.log" 2>&1 &
OLDPID=$!
sleep 4
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon died on a database it must tolerate"; tail -20 "$H/old-live.log"; exit 1; }
echo "  the old daemon is up on a v4 database it does not understand"

# (1) ENUMERATION. Nothing either old-daemon log says may name the Codex run —
# not its session name, not its uid, not its thread id, and not the word codex.
if grep -qiE "$(codex_identifiers)|codex" "$H/old-live.log" "$H/old.log"; then
  echo "FAIL: the old daemon named Codex state:"; grep -inE "$(codex_identifiers)|codex" "$H/old-live.log" "$H/old.log"; kill $OLDPID; exit 1
fi
echo "  (1) neither old-daemon log names a Codex session, thread or uid"

# Top-level `ls` FIRST, and it proves **command health, not non-enumeration**.
# At 8e5b172 `list()` asks tmux and returns early — printing "no CodeConnect
# sessions" — whenever tmux is empty, which it is here; the daemon is consulted
# only for names tmux already returned. So a clean `ls` says the old CLI still
# runs against a v4 database, which is worth asserting and is all it says.
assert_old_cli_clean "ls (command health)" "$OLDCC" ls

# **The binding enumeration probe.** `codeconnect sessions list` at 8e5b172 goes
# straight to the daemon — `sessions::list()` calls `fetch()`, which is one
# `ListSessions` IPC round trip — and prints every row it is given, with no tmux
# short-circuit anywhere. This is the command that WOULD name a Codex run if the
# old daemon could see one, so it is the one whose silence means something.
assert_old_cli_clean "sessions list (daemon enumeration)" "$OLDCC" sessions list
# And prove it is not silent for the trivial reason: it must be listing the
# Claude run, or its silence about Codex is the silence of an empty report.
#
# Captured and then matched with `case`, never piped into `grep -q`. Under
# `pipefail` that pipeline is a coin flip on a *passing* run: `grep -q` exits the
# moment it matches, the CLI still has its summary lines to write, Rust ignores
# SIGPIPE so the write returns EPIPE and `println!` panics with status 101 — and
# `pipefail` then reports the pipeline as failed even though the match succeeded.
# Measured: this line failed once with the uid plainly present in the output the
# failure message went on to print.
OLD_SESSIONS_LIST="$(CODECONNECT_HOME="$H" "$OLDCC" sessions list 2>&1)"
case "$OLD_SESSIONS_LIST" in
  *01K1B3XQ8ZC0DE5FGH7JKMNPQR*) ;;
  *) echo "FAIL: old \`sessions list\` enumerated nothing at all, so its silence proves nothing:";
     printf '%s\n' "$OLD_SESSIONS_LIST"; kill $OLDPID; exit 1;;
esac
echo "  (1) old \`sessions list\` enumerated the CLAUDE run over IPC and still named no Codex state"

# The old prune is defined over ended runs, so end everything first — including
# the Codex row, which is the strongest form of the question: even a Codex run
# the old binary would consider prunable must be unreachable to it.
q "UPDATE sessions SET lifecycle='exited'; UPDATE codex_sessions SET lifecycle='exited';"
assert_old_cli_clean "sessions prune --dry-run" "$OLDCC" sessions prune --dry-run

# (4) And the old binary's CLAUDE handling must still be whole: isolation bought
# by breaking the old daemon is not isolation, it is a second outage.
CODECONNECT_HOME="$H" "$OLDCC" sessions prune > "$H/old-prune.log" 2>&1
sleep 1
[ "$(q 'SELECT COUNT(*) FROM sessions;')" = "0" ] \
  || { echo "FAIL: the old daemon could not prune its own Claude session"; cat "$H/old-prune.log"; kill $OLDPID; exit 1; }
[ "$(q "SELECT COUNT(*) FROM events WHERE session_uid='01K1B3XQ8ZC0DE5FGH7JKMNPQR';")" = "0" ] \
  || { echo "FAIL: the Claude session's events did not go with it"; kill $OLDPID; exit 1; }
echo "  (4) the old daemon pruned its Claude session and its events, exactly as it always did"

kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null

# (2) MUTATION, second old-daemon window — closed here. One write inside this
# window was ours, not the old binary's: the `UPDATE ... lifecycle='exited'`
# above, which had to happen mid-window because the old prune only considers
# ended runs. So undo exactly that nudge — back to whatever the baseline held,
# not to a guess — leaving the comparison to measure the old daemon, its real
# IPC and its real prune, and nothing else.
q "UPDATE codex_sessions SET lifecycle='$CODEX_LIFECYCLE_BEFORE';"
[ "$(codex_state)" = "$CODEX_BEFORE_OLD2" ] || {
  echo "FAIL: the old daemon mutated Codex state"
  echo "  BEFORE: $CODEX_BEFORE_OLD2"; echo "  AFTER : $(codex_state)"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM events WHERE session_uid='$CX';")" = "$CODEX_EVENTS_BEFORE" ] \
  || { echo "FAIL: Codex events were destroyed"; exit 1; }
echo "  (2) every Codex row and event is byte-for-byte what it was"


echo "== 6) new ccd reopens after the old prune: the forward path is intact =="
run "$NEW" new3
assert_uv 4 "after the final new reopen"
[ "$(q "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the Codex run did not survive the whole round trip"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM sessions WHERE agent <> 'claude';")" = "0" ] \
  || { echo "FAIL: a Codex row leaked back into the shared table"; exit 1; }
echo "  the Codex run survived new -> old -> new -> old + real prune -> new"

echo "== 7) THE PRODUCER, and the two guards that close it — real binaries throughout =="
# Everything above proves the old binary cannot reach a Codex row *that stays in
# codex_sessions*. It says nothing about how one could stop being there, and
# there is exactly one way: a supervisor registering a Codex run with a daemon
# that has no agent column. Such a daemon drops the three fields it does not
# know and would file the run in the SHARED table under `DEFAULT 'claude'`, and
# from that moment its prune can reach the run — through the shadow.
#
# TWO INDEPENDENT GUARDS now stand in the way of that, on opposite sides of the
# socket, and this step drives both against the real old daemon:
#
#   * (b) THE STORAGE GUARD. `sessions_refuse_codex_shadow` is a BEFORE INSERT
#     trigger, and a trigger lives in the SCHEMA — so a rolled-back v0.6.0
#     inherits it and cannot drop it by writing `user_version` back to 3. Its
#     shadow-writing upsert fails `SQLITE_CONSTRAINT`, and the old daemon, which
#     has no vocabulary for that, drops the connection without answering. This is
#     the guard that works even for a supervisor that never asks.
#
#   * (e) THE SUPERVISOR'S WITHHOLD. The introduction is not made at all, so the
#     daemon is never asked to refuse it.
#
# **What this step no longer does, and it is worth being plain about it.** It
# used to measure the LOSS — ack, shadow row, prune destroys the events,
# tombstone — so that (e)'s silence meant something. With the trigger in the
# schema the loss is no longer producible at all, from either side, so there is
# nothing left to reproduce. What is asserted instead is that the refusal is
# real and is not itself an outage: no shadow row, no answer, and a daemon that
# is still up and still registering Claude sessions afterwards. A guard bought by
# breaking the old daemon would be a second outage, not isolation.

# Fresh events for the run, so the counts below are measured against a live count
# rather than against whatever survived step 5.
q "DELETE FROM events WHERE session_uid='$CX';
   INSERT INTO events(session_uid,session_id,seq,ts,kind,payload,source,source_event_id)
   VALUES('$CX','cx-1',1,'t','tool_call','{}','hook','y1'),('$CX','cx-1',2,'t','tool_call','{}','hook','y2');
   UPDATE codex_sessions SET lifecycle='live' WHERE session_uid='$CX';"

# One NDJSON frame down an old daemon's own socket, and its reply. The home is a
# parameter — defaulting to this run's — because the falsifiability arm below
# drives a SECOND daemon on a scratch copy of the database, and the frame it
# sends has to be the identical one.
old_ipc() {
  CC_SOCK="${2:-$H}/ccd.sock" CC_FRAME="$1" python3 - <<'PY'
import os, socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(5)
s.connect(os.environ["CC_SOCK"])
f = s.makefile("rwb")
f.write((os.environ["CC_FRAME"] + "\n").encode()); f.flush()
try:
    print(f.readline().decode().strip() or "<EOF>")
except Exception:
    print("<no reply>")
PY
}
CODEX_REGISTER="{\"type\":\"register\",\"session_id\":\"cx-1\",\"session_uid\":\"$CX\",\"tmux_session\":\"cx-1\",\"tmux_socket\":\"codeconnect\",\"cwd\":\"/work/codex\",\"supervisor_pid\":424242,\"agent\":\"codex\",\"agent_bin\":\"/usr/bin/codex\",\"codex_thread_id\":\"$CX_THREAD\",\"codex_socket\":\"/tmp/cch.x/ccd.sock\",\"started_at\":\"2026-08-28T00:00:00.000Z\",\"protocol_minor\":15}"
NEGOTIATE='{"type":"negotiate_support","agent":"codex"}'

# (b0) THE FALSIFIABILITY ARM: the SAME producer against the schema as it was
# BEFORE the trigger shipped, where it must still do the damage.
#
# Without this the rest of step 7 is vacuously green. "No shadow row, no
# tombstone, every event intact" is exactly what a gate would also report if the
# trigger did nothing, if the Register never arrived, or if the harm had never
# been possible in the first place. So the harm is reproduced once, here, on a
# schema that differs from the real one in exactly one object — and then the
# refusal a few lines below means the trigger, and only the trigger.
#
# On a SCRATCH COPY of the whole home, never on the real one. The real database
# is the thing every later arm reasons about, and a `DROP TRIGGER` on it would
# leave the guard off for the rest of the run if anything between here and the
# restore exited early. A copy costs one `cp` and cannot.
H2="$H.pretrigger"
mkdir -p "$H2"
cp "$H/config.json" "$H2/"
# `.backup`, not `cp`: the database is in WAL mode, so a plain file copy can
# leave committed rows behind in the write-ahead log this copy would not carry.
sqlite3 "$H/events.db" ".backup '$H2/events.db'"
q2() { sqlite3 "$H2/events.db" "$1"; }
q2 "DROP TRIGGER sessions_refuse_codex_shadow;"
[ "$(q2 "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='sessions_refuse_codex_shadow';")" = "0" ] \
  || { echo "FAIL: the pre-trigger staging did not remove the trigger, so (b0) would prove nothing"; exit 1; }
CODECONNECT_HOME="$H2" "$OLD" > "$H2/old-pretrigger.log" 2>&1 &
OLDPID=$!
for _ in $(seq 1 40); do [ -S "$H2/ccd.sock" ] && break; sleep 0.25; done
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon did not come up for the pre-trigger arm"; tail -20 "$H2/old-pretrigger.log"; exit 1; }
PRE_REPLY="$(old_ipc "$CODEX_REGISTER" "$H2")"
[ "$PRE_REPLY" = '{"type":"ack"}' ] \
  || { echo "FAIL: without the trigger the old daemon must ACK this Register, so the harm this gate rules out is not being reproduced at all: $PRE_REPLY"; kill $OLDPID; exit 1; }
sleep 1
[ "$(q2 "SELECT agent FROM sessions WHERE session_uid='$CX';")" = "claude" ] \
  || { echo "FAIL: without the trigger a default-claude SHADOW must appear in the swept table"; kill $OLDPID; exit 1; }
q2 "UPDATE sessions SET lifecycle='exited';"
CODECONNECT_HOME="$H2" "$OLDCC" sessions prune > "$H2/old-prune-pretrigger.log" 2>&1
sleep 1
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null
PRE_EVENTS="$(q2 "SELECT COUNT(*) FROM events WHERE session_uid='$CX';")"
PRE_TOMB="$(q2 "SELECT COUNT(*) FROM deleted_sessions WHERE session_uid='$CX';")"
PRE_ORPHAN="$(q2 "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")"
[ "$PRE_EVENTS" = "0" ] && [ "$PRE_TOMB" = "1" ] && [ "$PRE_ORPHAN" = "1" ] \
  || { echo "FAIL: the harm did not reproduce even with the trigger dropped (events=$PRE_EVENTS tombstone=$PRE_TOMB codex_row=$PRE_ORPHAN); everything below would then be asserting nothing"; exit 1; }
rm -rf "$H2"
echo "  (b0) with ONE object removed from the schema, the identical Register reproduced the"
echo "       whole harm: acked, filed a default-claude SHADOW, and the real old prune then"
echo "       destroyed the Codex run's events (2 -> 0), tombstoned its uid, and left the"
echo "       codex_sessions row standing to contradict that tombstone. So the refusal below"
echo "       is the trigger's doing and nothing else's."

CODECONNECT_HOME="$H" "$OLD" > "$H/old-live2.log" 2>&1 &
OLDPID=$!
for _ in $(seq 1 40); do [ -S "$H/ccd.sock" ] && break; sleep 0.25; done
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon did not come up for step 7"; tail -20 "$H/old-live2.log"; exit 1; }

# (a) THE SIGNAL. `ClientFrame` is internally tagged with no catch-all variant,
# so `negotiate_support` is a VARIANT the old build cannot decode — and unknown
# variants, unlike unknown fields, fail loudly. This is the whole basis of the
# withhold, so it is measured against the real binary rather than assumed.
NEG_REPLY="$(old_ipc "$NEGOTIATE")"
echo "  old daemon answers negotiate_support(codex): $(printf '%.110s' "$NEG_REPLY")"
case "$NEG_REPLY" in
  *'"type":"error"'*'negotiate_support'*) ;;
  *) echo "FAIL: the old daemon's answer is not the refusal the withhold reads: $NEG_REPLY"; kill $OLDPID; exit 1;;
esac
echo "  (a) the signal exists and is unambiguous: an undecodable-variant error, never an ack"

# (b) WITHOUT THE WITHHOLD, AND AGAINST THE STORAGE GUARD: send the Register the
# supervisor would have sent, and read what the real old daemon does with it now.
#
# The trigger is what it runs into, so the trigger is proved to be there FIRST —
# and proved to be there *after the old daemon has done its own migrate*, which
# is the whole claim. v0.6.0 writes `user_version` 3 back over ours (asserted in
# step 3) and has no statement that drops a trigger, so the one thing it cannot
# roll back is the schema object it never learned to name.
[ "$(q "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='sessions_refuse_codex_shadow';")" = "1" ] \
  || { echo "FAIL: the shadow-refusing trigger did not survive the rollback, so (b) below would prove nothing"; kill $OLDPID; exit 1; }
REG_REPLY="$(old_ipc "$CODEX_REGISTER")"
echo "  old daemon answers register(agent=codex): $REG_REPLY"
# `<EOF>` is `old_ipc`'s word for "the daemon closed the connection without
# answering", which is what a v0.6.0 whose upsert raised SQLITE_CONSTRAINT does:
# it has no vocabulary for a refused write, so the read loop ends. An `ack` here
# would mean the shadow had been filed and the guard was gone.
[ "$REG_REPLY" = "<EOF>" ] \
  || { echo "FAIL: the old daemon answered the Codex Register with $REG_REPLY; the storage guard did not refuse it"; kill $OLDPID; exit 1; }
sleep 1
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: a default-claude SHADOW of the Codex run was filed in the SWEPT table anyway"; kill $OLDPID; exit 1; }
# A refusal that takes the daemon down is an outage wearing a guard's clothes.
kill -0 $OLDPID 2>/dev/null \
  || { echo "FAIL: the refused Register killed the old daemon"; tail -20 "$H/old-live2.log"; exit 1; }
echo "  (b) the old daemon's shadow-writing upsert was REFUSED by the trigger it inherited:"
echo "      no answer, no shadow row in the swept table, and the daemon still up"

# (c) And its own Claude work is untouched by that refusal — the register that
# SHOULD land still lands, over the same IPC, on the same connection-per-frame
# basis, immediately after the refused one.
#
# A uid of its own, deliberately NOT the `...NPQR` this harness has been carrying.
# Step 5's real old prune deleted that run and wrote its TOMBSTONE, and v0.6.0
# refuses to resurrect a tombstoned uid — `upsert_session`'s
# `WHERE NOT EXISTS (SELECT 1 FROM deleted_sessions ...)` writes no row, returns
# `Tombstoned`, `register_supervisor` bails, and the read loop ends, which closes
# the connection. Measured: the first draft of this arm reused `...NPQR` and got
# exactly that closed connection, which this assertion would have reported as the
# daemon having stopped acking altogether. It is correct v0.6.0 behaviour and has
# nothing to do with the trigger, so the probe gets a uid nothing has buried.
CLAUDE_UID=01K1B3XQ8ZC0DE5FGH7JKMNPHC
CLAUDE_REGISTER="{\"type\":\"register\",\"session_id\":\"cc-8\",\"session_uid\":\"$CLAUDE_UID\",\"tmux_session\":\"cc-8\",\"tmux_socket\":\"codeconnect\",\"cwd\":\"/tmp\",\"supervisor_pid\":424243,\"started_at\":\"2026-08-28T00:00:00.000Z\",\"protocol_minor\":15}"
CLAUDE_REPLY="$(old_ipc "$CLAUDE_REGISTER")"
[ "$CLAUDE_REPLY" = '{"type":"ack"}' ] \
  || { echo "FAIL: the old daemon stopped acking ordinary Claude registrations: $CLAUDE_REPLY"; kill $OLDPID; exit 1; }
sleep 1
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_uid='$CLAUDE_UID';")" = "1" ] \
  || { echo "FAIL: the acked Claude registration did not land"; kill $OLDPID; exit 1; }
# Still serving, proved rather than inferred from the ack: an ack written on the
# way down would read the same.
kill -0 $OLDPID 2>/dev/null \
  || { echo "FAIL: the old daemon died after acking the Claude registration"; tail -20 "$H/old-live2.log"; exit 1; }

# ...and the real old prune, run over everything it can see, reaches no part of
# the Codex run — because there is no shadow for it to reach.
q "UPDATE sessions SET lifecycle='exited';"
CODECONNECT_HOME="$H" "$OLDCC" sessions prune > "$H/old-prune2.log" 2>&1
sleep 1
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null
KEPT_EVENTS="$(q "SELECT COUNT(*) FROM events WHERE session_uid='$CX';")"
TOMBSTONED="$(q "SELECT COUNT(*) FROM deleted_sessions WHERE session_uid='$CX';")"
KEPT_CODEX="$(q "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")"
[ "$KEPT_EVENTS" = "2" ] && [ "$TOMBSTONED" = "0" ] && [ "$KEPT_CODEX" = "1" ] \
  || { echo "FAIL: the Codex run was still reachable through the old prune (events=$KEPT_EVENTS tombstone=$TOMBSTONED codex_row=$KEPT_CODEX)"; exit 1; }
echo "  (c) the old daemon went on acking and filing its own Claude registration, and its real"
echo "      prune then found no shadow to destroy: the Codex run kept both events, kept its row,"
echo "      and was never tombstoned."

# (d) The new binary reopens on top of the refusal: nothing to repair, and no
# resurrection — the carry may not walk the isolated row into the swept table.
run "$NEW" new4
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: the Codex uid turned up in the swept table after the new binary reopened"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the isolated Codex row did not survive the refused registration"; exit 1; }
echo "  (d) the new binary reopened with nothing to repair and put no Codex uid in the swept table"

# (e) WITH THE WITHHOLD: the same daemon, the same run, and the negotiation's
# answer read as the supervisor now reads it — so no Register is sent at all.
#
# **Why this half is a hand-written frame and (f) below is not.** Driving the
# real supervisor here would need it to send a Codex `Register`, and nothing that
# ships can: `supervisor::registration_frame` writes `agent: AgentKind::Claude`
# as a literal, `codeconnect supervise` parses only `--session`,
# `--session-uid`, `--tmux-session`, `--cwd` and `--claude-bin`, and there is no
# environment variable on that path — `codeconnect codex` itself still `bail!`s
# before it launches anything. `withhold_unless_hosted` returns `Ok` immediately
# for Claude, so a real supervisor pointed at this daemon would register, not
# withhold: it would measure the opposite of this arm's claim. The seam does not
# exist yet, and inventing an `--agent` flag to make a harness reachable would be
# shipping production surface for a test. So the wire is written by hand HERE,
# where the input is one the live system cannot yet produce, and the real binary
# is run in (f), where it can.
q "DELETE FROM deleted_sessions WHERE session_uid='$CX';
   DELETE FROM events WHERE session_uid='$CX';
   INSERT INTO events(session_uid,session_id,seq,ts,kind,payload,source,source_event_id)
   VALUES('$CX','cx-1',1,'t','tool_call','{}','hook','z1'),('$CX','cx-1',2,'t','tool_call','{}','hook','z2');
   UPDATE codex_sessions SET lifecycle='live' WHERE session_uid='$CX';"
WITHHELD_BEFORE="$(codex_state)"
CODECONNECT_HOME="$H" "$OLD" > "$H/old-live3.log" 2>&1 &
OLDPID=$!
for _ in $(seq 1 40); do [ -S "$H/ccd.sock" ] && break; sleep 0.25; done
# The supervisor asks, reads an answer that is not `supported: true`, and stops.
# Nothing else goes down this connection — which is the entire behaviour change.
old_ipc "$NEGOTIATE" > /dev/null
q "UPDATE sessions SET lifecycle='exited';"
CODECONNECT_HOME="$H" "$OLDCC" sessions prune > "$H/old-prune3.log" 2>&1
sleep 1
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null
[ "$(codex_state)" = "$WITHHELD_BEFORE" ] \
  || { echo "FAIL: Codex state changed even though Register was withheld"; echo "  BEFORE: $WITHHELD_BEFORE"; echo "  AFTER : $(codex_state)"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM events WHERE session_uid='$CX';")" = "2" ] \
  || { echo "FAIL: the Codex events did not survive the withheld run"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM deleted_sessions WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: a tombstone was written for a run the old daemon was never told about"; exit 1; }
echo "  (e) with the Register withheld, the same old daemon and the same real prune left"
echo "      every Codex row and event byte-for-byte intact and wrote no tombstone"

# (f) THE REAL WORKING-TREE SUPERVISOR, against the REAL old daemon.
#
# Everything above drove the wire by hand, because the withhold needs a Codex
# `Register` and no shipping binary can send one (see the note above (e)). What a
# real binary CAN be made to do is the rest of the supervisor, and it is exactly
# the half a rollback leaves running: a working-tree supervisor still alive
# beside a daemon that has been rolled back under it. Both of its paths to that
# daemon are driven here with the real binary — `serve_once`, which registers and
# reconnects, and `report_exit`, which replays the registration and files the end
# — so a supervisor/old-daemon integration regression fails this gate instead of
# being invisible to it.
#
# And the thing no hand-written frame can show: the DAEMON-SIDE CONNECTION
# LIFETIME. `ccd` accepts on a bounded permit, so a supervisor that goes on
# holding a connection it has already finished with — through a reconnect backoff
# that reaches ten seconds — starves the hooks, supervisors and CLI calls queued
# behind it. Whether it does is a property of the daemon's open descriptors, not
# of anything either binary prints, so it is counted here.

# tmux is answered from a private, EMPTY root, so this neither asks nor disturbs
# the tmux server the person running this harness is using. One empty root gives
# the two different verdicts both arms need, because the supervisor picks its
# liveness probe off the uid:
#   * with a well-formed `--session-uid` it resolves BY UID, and an unreachable
#     server is `Unknown` — never an exit — so it stays up and reconnects, which
#     is what the connection counting needs;
#   * with no uid it falls back to the name-addressed `has-session`, whose "no
#     server running" IS a definite `Gone`, which is what drives `report_exit`.
SUPTMUX="$H/tmux-none"
mkdir -p "$SUPTMUX"
SUP_UID=01K1B3XQ8ZC0DE5FGH7JKMNPSV
SUPLOG="$H/logs/supervisor-cc-sup-$SUP_UID.log"

# Live IPC connections the old daemon is holding, counted off that process's own
# open unix-domain descriptors. The listening socket is one of them, so this is
# never zero while the daemon is up and the number only means anything against a
# baseline taken before any supervisor existed — hence `CONNS_BASE` below rather
# than an absolute expectation.
daemon_ipc_conns() { lsof -nP -a -p "$1" -U 2>/dev/null | tail -n +2 | wc -l | tr -d ' '; }

CODEX_BEFORE_SUP="$(codex_state)"
CODECONNECT_HOME="$H" "$OLD" > "$H/old-live4.log" 2>&1 &
OLDPID=$!
for _ in $(seq 1 40); do [ -S "$H/ccd.sock" ] && break; sleep 0.25; done
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon did not come up for step 7(f)"; tail -20 "$H/old-live4.log"; exit 1; }
CONNS_BASE="$(daemon_ipc_conns $OLDPID)"
# An lsof that reports nothing for a daemon that is demonstrably listening would
# make every comparison below trivially true, so the probe is proved first.
[ "$CONNS_BASE" -ge 1 ] \
  || { echo "FAIL: lsof sees no unix socket for a listening daemon, so the connection counts prove nothing"; kill $OLDPID; exit 1; }
echo "  old daemon idle IPC descriptors: $CONNS_BASE (its listener)"

# --- the LIVE path, and the connection lifetime across a real reconnect -------
# The supervisor is started against the daemon that is already up, registers, and
# is then left alone while the daemon is bounced under it: `serve_once` returns,
# the reconnect loop backs off, and it comes back to a daemon that is a different
# process. Sampled right through that, the number of connections the daemon holds
# for it must reach the baseline plus exactly one and never more — one is the
# reconnect having happened at all, and more is a supervisor still occupying a
# permit for a link it has already lost.
#
# `timeout` is a net, not the mechanism: this supervisor is killed below, and a
# uid-addressed liveness probe against an empty tmux root never ends it by
# itself.
CODECONNECT_HOME="$H" TMUX_TMPDIR="$SUPTMUX" timeout 90 \
  "$NEWCC" supervise --session cc-sup --session-uid "$SUP_UID" \
  --tmux-session cc-sup --cwd "$H" > "$H/sup-live.log" 2>&1 &
SUPPID=$!
for _ in $(seq 1 40); do
  [ "$(q "SELECT COUNT(*) FROM sessions WHERE session_uid='$SUP_UID';")" = "1" ] && break
  sleep 0.25
done
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_uid='$SUP_UID';")" = "1" ] \
  || { echo "FAIL: the real working-tree supervisor never registered with the real old daemon:";
       cat "$H/sup-live.log" "$SUPLOG" 2>/dev/null; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
# The registration is the supervisor's own, not a row this harness seeded: the
# old daemon recorded the cwd and tmux name it was handed on the wire.
[ "$(q "SELECT tmux_session || '|' || cwd FROM sessions WHERE session_uid='$SUP_UID';")" = "cc-sup|$H" ] \
  || { echo "FAIL: the registered row is not the one the supervisor sent: $(q "SELECT tmux_session || '|' || cwd FROM sessions WHERE session_uid='$SUP_UID';")"; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
# And a REAL registration lands wearing the schema default — the old daemon drops
# `agent` because it has no such column. For Claude that is harmless and correct,
# and it is exactly why a Codex registration through the same path had to be
# stopped twice over: by the trigger in (b), and by the withhold in (e).
[ "$(q "SELECT agent FROM sessions WHERE session_uid='$SUP_UID';")" = "claude" ] \
  || { echo "FAIL: a real registration did not land under the old schema's DEFAULT"; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
CONNS_LIVE="$(daemon_ipc_conns $OLDPID)"
[ "$CONNS_LIVE" = "$((CONNS_BASE + 1))" ] \
  || { echo "FAIL: a registered supervisor should cost the old daemon exactly one connection (base=$CONNS_BASE now=$CONNS_LIVE)"; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
echo "  the real supervisor's serve_once registered over real IPC and holds one connection ($CONNS_LIVE)"

# Take the daemon away, so the supervisor is genuinely in its reconnect backoff
# with nothing to connect to, then give it back.
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null
sleep 3
CODECONNECT_HOME="$H" "$OLD" > "$H/old-live5.log" 2>&1 &
OLDPID=$!
for _ in $(seq 1 40); do [ -S "$H/ccd.sock" ] && break; sleep 0.25; done
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon did not come back for the reconnect"; tail -20 "$H/old-live5.log"; kill $SUPPID 2>/dev/null; exit 1; }
# Sampled across a window longer than RECONNECT_MAX, so the reconnect is inside
# it however far the backoff had grown while the socket was gone.
CONNS_MAX=0
for _ in $(seq 1 56); do
  CONNS_NOW="$(daemon_ipc_conns $OLDPID)"
  [ "$CONNS_NOW" -gt "$CONNS_MAX" ] && CONNS_MAX="$CONNS_NOW"
  sleep 0.25
done
[ "$CONNS_MAX" -le "$((CONNS_BASE + 1))" ] \
  || { echo "FAIL: the supervisor grew the old daemon's connection count across its backoff (base=$CONNS_BASE peak=$CONNS_MAX)"; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
[ "$CONNS_MAX" = "$((CONNS_BASE + 1))" ] \
  || { echo "FAIL: the supervisor never reconnected (base=$CONNS_BASE peak=$CONNS_MAX), so the count above proves nothing"; cat "$SUPLOG" 2>/dev/null; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
# Two `serve_once` runs, in the supervisor's own words — the count is the daemon's
# side of the same fact and neither is left to stand alone.
SUP_REGISTRATIONS="$(grep -c "registered with ccd" "$SUPLOG" 2>/dev/null || true)"
[ "${SUP_REGISTRATIONS:-0}" -ge 2 ] \
  || { echo "FAIL: the supervisor logged $SUP_REGISTRATIONS registration(s); the daemon bounce did not drive a second real serve_once"; cat "$SUPLOG" 2>/dev/null; kill $SUPPID $OLDPID 2>/dev/null; exit 1; }
echo "  the supervisor rode the daemon bounce: $SUP_REGISTRATIONS real serve_once runs, peak IPC descriptors $CONNS_MAX (baseline + 1)"
# `|| true` on the wait, unlike the daemon kills above, because this child is a
# `timeout` wrapper: `ccd` handles SIGTERM and exits 0, so waiting on one is
# harmless under `set -e`, but `timeout` forwards the signal and then leaves with
# 143 — which would end this script here, silently, with every assertion below
# unrun and nothing printed to say so. Measured.
kill $SUPPID 2>/dev/null || true; wait $SUPPID 2>/dev/null || true
SUPPID=

# --- the EXIT path ------------------------------------------------------------
# The same binary with no `--session-uid`, so its liveness falls back to the
# name-addressed check, whose "no server running" is a definite `Gone`: after
# EXIT_CONFIRMATIONS looks it stops and runs `report_exit`.
#
# What separates `report_exit` from the daemon's own liveness sweep, which
# reaches the same `exited` lifecycle by itself: the row is DELETED out from
# under the supervisor here, and `report_exit` REPLAYS the registration on a
# fresh connection before it files the exit. A row that is back — and exited —
# can only have been put there by that replay. The sweep creates nothing.
CODECONNECT_HOME="$H" TMUX_TMPDIR="$SUPTMUX" timeout 30 \
  "$NEWCC" supervise --session cc-sup-exit --tmux-session cc-sup-exit --cwd "$H" \
  > "$H/sup-exit.log" 2>&1 &
SUPXPID=$!
for _ in $(seq 1 40); do
  [ "$(q "SELECT COUNT(*) FROM sessions WHERE session_id='cc-sup-exit';")" = "1" ] && break
  sleep 0.25
done
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_id='cc-sup-exit';")" = "1" ] \
  || { echo "FAIL: the real supervisor never registered its exit-path run:"; cat "$H/sup-exit.log"; kill $SUPXPID $OLDPID 2>/dev/null; exit 1; }
q "DELETE FROM sessions WHERE session_id='cc-sup-exit';"
SUPX_RC=0
wait $SUPXPID || SUPX_RC=$?
[ "$SUPX_RC" -eq 0 ] \
  || { echo "FAIL: the real supervisor did not reach its exit path cleanly (rc=$SUPX_RC — 124 means it never decided the session was gone):"; cat "$H/sup-exit.log"; kill $OLDPID 2>/dev/null; exit 1; }
sleep 1
[ "$(q "SELECT COUNT(*) FROM sessions WHERE session_id='cc-sup-exit';")" = "1" ] \
  || { echo "FAIL: report_exit did not replay the registration onto the row deleted under it"; kill $OLDPID 2>/dev/null; exit 1; }
[ "$(q "SELECT lifecycle FROM sessions WHERE session_id='cc-sup-exit';")" = "exited" ] \
  || { echo "FAIL: report_exit's SessionExited did not reach the old daemon"; kill $OLDPID 2>/dev/null; exit 1; }
# And the connection it opened to say so is not one it kept: back to the idle
# listener, with no supervisor left holding a permit.
CONNS_AFTER="$(daemon_ipc_conns $OLDPID)"
[ "$CONNS_AFTER" = "$CONNS_BASE" ] \
  || { echo "FAIL: the old daemon is still holding $CONNS_AFTER IPC descriptors after both supervisors ended (baseline $CONNS_BASE)"; kill $OLDPID 2>/dev/null; exit 1; }
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null

# The same bracket every other old-daemon window gets: the real supervisor is a
# writer too, and a Codex row it reached would be as fatal as one the old prune
# reached.
[ "$(codex_state)" = "$CODEX_BEFORE_SUP" ] || {
  echo "FAIL: the real supervisor's run against the old daemon mutated Codex state"
  echo "  BEFORE: $CODEX_BEFORE_SUP"; echo "  AFTER : $(codex_state)"; exit 1; }
echo "  (f) the REAL working-tree supervisor ran both its paths against the real old daemon:"
echo "      serve_once registered over real IPC and re-registered across a daemon bounce, and"
echo "      report_exit replayed that registration onto a row deleted under it and ended the"
echo "      run. The daemon's IPC descriptors went $CONNS_BASE -> $CONNS_MAX -> $CONNS_AFTER: one"
echo "      connection at a time through the whole reconnect backoff, and none left behind."

echo "PASS: a REAL v3-binary-opens-a-v4-database downgrade, driven 4 -> 3 -> 4 by the"
echo "      real binaries: the old daemon rewrote user_version to its own 3, as measured,"
echo "      and that is harmless because the isolation never rested on the number."
echo "      Seam columns round-tripped byte-for-byte through the old daemon's write;"
echo "      the foreign-epoch device set survived as bytes and authorizes nothing;"
echo "      both old-daemon runs were bracketed individually — bytes captured immediately"
echo "      before each run and compared immediately after it — and in the first bracket"
echo "      the Codex run was LIVE and stayed live, so the old sweep's live -> exited"
echo "      mutation is measured absent, not assumed; old \`ls\` and old \`sessions prune"
echo "      --dry-run\` each ran SUCCESSFULLY and still named no Codex session, thread or"
echo "      uid — and \`sessions list\`, which asks the DAEMON with no tmux short-circuit,"
echo "      enumerated the Claude run and still named no Codex state — while the real old"
echo "      prune destroyed its own Claude session completely."
echo ""
echo "      And step 7 drove the one PRODUCER that can undo all of that, with the real"
echo "      binaries and the real IPC, against BOTH guards that now stand in its way. A"
echo "      Register naming agent=codex ran into the sessions_refuse_codex_shadow trigger"
echo "      the rolled-back daemon inherited and cannot drop: the upsert was refused, no"
echo "      shadow row reached the swept table, the daemon stayed up, went on acking and"
echo "      filing its own Claude registration, and its real prune then found nothing of"
echo "      the Codex run to destroy. The same daemon answers negotiate_support with an"
echo "      undecodable-variant error — the signal the supervisor withholds on — and with"
echo "      the Register withheld the identical sequence left every byte intact. The loss"
echo "      itself is no longer reproducible from either side, which is the point — and"
echo "      that it is the TRIGGER doing that, rather than a producer this gate merely"
echo "      failed to stage, was shown by removing exactly that one schema object from a"
echo "      scratch copy and watching the identical Register destroy the run there."
echo ""
echo "      And the REAL working-tree supervisor was run against that same real old"
echo "      daemon: its serve_once registered over real IPC, re-registered across a"
echo "      daemon bounce, and its report_exit replayed the registration onto a row"
echo "      deleted under it before ending the run — with the daemon holding exactly one"
echo "      IPC descriptor for it at a time through the whole reconnect backoff, and"
echo "      none of them left behind. The Codex half of step 7 stays hand-written for"
echo "      one reason and it is written down beside it: registration_frame hardcodes"
echo "      agent=Claude and no flag or environment variable overrides it, so no"
echo "      shipping binary can send the Codex Register the withhold refuses."
