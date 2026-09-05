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
#     (`sessions.agent`, `codex_thread_id`, `codex_socket`, `codex_generation`;
#     `devices.features`, `features_epoch`) come back byte-for-byte through the
#     real old daemon's own write, because it names none of them.
#     `codex_generation` is the A5.1 durable high-water — the number a Codex
#     registration is refused against — so its round-trip is not just data
#     preservation: a blanked value would let a stale supervisor re-adopt a
#     session at any generation on the way back up.
#
#   * **A REAL schema downgrade, and agent-scoped isolation under it.** The new
#     binary is now `SCHEMA_VERSION` 5 (agent-scoped session storage at 4,
#     agent-scoped approval CARDS at 5); v0.6.0 is 3. So a genuine
#     v3-binary-opens-a-v5-database downgrade happens here, and
#     the `user_version` really is driven 5 -> 3 -> 5. That is measured, not
#     assumed: v0.6.0 reads `user_version`, ignores what it finds, and writes 3
#     back unconditionally — which is also exactly why a version fence could
#     never have protected anything, and why the isolation does not rest on one.
#     It rests on the table name: v0.6.0 contains no statement that names
#     `codex_sessions`, none that names `codex_pending_approvals` and none that
#     names `mutation_ledger` — the generalized Codex mutation ledger a phone
#     answer claims in — and, for
#     the two paths that could still have put Codex state into the tables it DOES
#     name, on the `sessions_refuse_codex_shadow` and
#     `pending_approvals_refuse_codex_card` triggers, which live in the schema and
#     so are the parts of this version a rolled-back binary keeps and runs against
#     itself (step 7).
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
# The Codex approval identity a real card carries (chunk 3a). `CX_REQ` is a real
# `protocol::composite_id` value — the opaque id the phone would answer by — and
# is what the non-enumeration greps look for, because a short fake would match
# nothing and prove nothing.
CX_ITEM=exec-cf7b67c7-3a19-4dd8-a9a6-6f243db33bd4
CX_TURN=01a01282-c951-76c1-84d1-6e33d6fdb219
CX_REQ=AQAaMDFLMUIzWFE4WkMwREU1RkdIN0pLTU5QQ1gACXRoX0FCQzEyMwEAKWV4ZWMtY2Y3YjY3YzctM2ExOS00ZGQ4LWE5YTYtNmYyNDNkYjMzYmQ0AAAAAAAAAAc
# The tools this harness cannot run without, checked BEFORE anything is built or
# started so a missing one is named here rather than discovered a few hundred lines
# in, after a worktree build, as an unexplained failure of whatever step happened to
# reach it first.
#
# `timeout` is the one worth naming: **stock macOS does not ship it** — it arrives
# with GNU coreutils, usually via Homebrew — and this harness does not merely use it
# as a net. `run()` below insists on exit code 124 as its evidence that a daemon
# stayed up for its whole window, so `timeout` is the measuring instrument, not a
# convenience that could be swapped for a background kill. A machine without it
# cannot run this gate, and that is a sentence, not a silent skip.
# `tmux` is deliberately NOT in this list: step 7(g)'s leftover-session check is
# already guarded on its presence and skips cleanly without it.
for tool in timeout python3 sqlite3 cc lsof; do
  command -v "$tool" >/dev/null 2>&1 \
    || { echo "FAIL: this harness needs \`$tool\` and it is not on PATH."; \
         echo "      (\`timeout\` is not part of stock macOS: \`brew install coreutils\`.)"; exit 1; }
done

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
  # `NEWPID` included (round-2 F8): a failure after step 7(g) spawns the sandbox
  # daemon and before its explicit kill would otherwise leak it, and it is the one
  # child here holding the home directory this script removes.
  for pid in "${SUPPID:-}" "${SUPXPID:-}" "${OLDPID:-}" "${NEWPID:-}"; do
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
codex_identifiers() { printf 'cx-1|%s|%s|%s|%s|%s' "$CX" "$CX_THREAD" "$CX_REQ" "$CX_ITEM" "$CX_TURN"; }

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
#
# `codex_generation` is the A5.1 durable high-water and joins this list for a
# reason the other three do not have. The other three are identity a rollback
# merely has to leave alone; this one is the number a Codex registration is
# REFUSED against (`Daemon::register_supervisor`). If a trip through the old
# binary blanked it, a stale supervisor could re-adopt the session at any
# generation it liked on the way back up — so "the old daemon cannot reach it"
# is a claim this harness has to make about it by name.
SEAM_COLS_SESSIONS="agent codex_thread_id codex_socket codex_generation"
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
  q "SELECT agent, COALESCE(codex_thread_id,''), COALESCE(codex_socket,''), \
            COALESCE(codex_generation,'') \
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
SESSION_SEAM_EXPECT="claude|th_probe|/sock_probe|99"
DEVICE_SEAM_SEEDED='{"agents":["codex"]}|epoch_probe'

echo "== 1) new ccd migrates the DB =="
run "$NEW" new1
assert_uv 5 "after new migrate"
assert_seam_columns_exist "after new migrate"
for object in codex_sessions all_sessions codex_pending_approvals all_pending_approvals \
              pending_approvals_refuse_codex_card mutation_ledger; do
  [ "$(q "SELECT COUNT(*) FROM sqlite_master WHERE name='$object';")" = "1" ] \
    || { echo "FAIL: $object was not built by the new binary"; exit 1; }
done
echo "  agent-scoped storage built: codex_sessions, all_sessions, codex_pending_approvals,"
echo "                              all_pending_approvals, pending_approvals_refuse_codex_card,"
echo "                              mutation_ledger"

echo "== 2) seed a live Claude session + a device, with PROBE values in every seam column =="
# A raw insert (not registration) so we can put a distinct value in each seam
# column and prove the old daemon leaves them byte-for-byte intact. The old
# binary knows none of these columns, so its positional writes must not touch
# them. (A real build never puts Codex identity on a Claude row — this is a
# storage-layer round-trip probe, not a registration.)
q "INSERT INTO sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,claude_session_id,transcript_path,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket,codex_generation)
   VALUES('01K1B3XQ8ZC0DE5FGH7JKMNPQR','cc-9','cc-9','codeconnect','/tmp',NULL,NULL,'live','t','t','claude','th_probe','/sock_probe',99);
   INSERT INTO devices(device_id,name,token_hash,created_at,features,features_epoch)
   VALUES('dev-probe','iPhone','th-1','t','{\"agents\":[\"codex\"]}','epoch_probe');"
[ "$(session_seam)" = "$SESSION_SEAM_EXPECT" ] || { echo "FAIL: session seed did not land: $(session_seam)"; exit 1; }
[ "$(device_seam)" = "$DEVICE_SEAM_SEEDED" ] || { echo "FAIL: device seed did not land: $(device_seam)"; exit 1; }

echo "== 2b) seed a LIVE Codex run in codex_sessions, with events =="
# Where a real Codex registration will put it. The old binary has no statement
# that names this table, so nothing it does can reach the row.
# `codex_generation` is seeded with a real value, not left NULL: `codex_state`
# below is `SELECT *`, so a NULL would round-trip through the old binary
# whether or not the column survived, and the hash would prove nothing about
# the one seam column that decides which registrations get refused.
q "INSERT INTO codex_sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,claude_session_id,transcript_path,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket,codex_generation)
   VALUES('$CX','cx-1','cx-1','codeconnect','/work/codex',NULL,NULL,'live','t','t','codex','$CX_THREAD','/tmp/cch.x/ccd.sock',7);
   INSERT INTO events(session_uid,session_id,seq,ts,kind,payload,source,source_event_id)
   VALUES('$CX','cx-1',1,'t','tool_call','{}','hook','x1'),('$CX','cx-1',2,'t','tool_call','{}','hook','x2');"
# **And an OPEN APPROVAL CARD for that run** (chunk 3a). This is the row the
# approval observer now produces, and it is the second half of the same
# rollback question the `codex_sessions` seed asks. `pending_approvals` is one
# of the four tables v0.6.0 reads GLOBALLY — its recovery does not walk a
# `sessions` row to reach them — so a Codex card sitting there is one that
# daemon enumerates and can delete. `codex_pending_approvals` is a table it has
# never heard of, and that is the whole isolation.
#
# `request_id` is a real derived composite id, not a placeholder: it is what
# `codex_approval::Approval::request_id` mints over (session_uid, thread_id,
# itemId, generation), and the enumeration probes below grep for it. A short
# fake would match nothing and prove nothing.
q "INSERT INTO codex_pending_approvals(session_uid,session_id,request_id,card,generation,created_ms,thread_id,turn_id,item_id,family)
   VALUES('$CX','cx-1','$CX_REQ','{\"request_id\":\"$CX_REQ\",\"payload_hash\":\"h\",\"tool_name\":\"command\",\"tool_input\":{},\"display_text\":\"d\"}',7,1787016966352,'$CX_THREAD','$CX_TURN','$CX_ITEM','commandExecution');"
[ "$(q "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the Codex card seed did not land"; exit 1; }
# And prove the read side answers for both agents while the write side is split.
[ "$(q "SELECT COUNT(*) FROM all_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: all_pending_approvals does not see the scoped card"; exit 1; }

# The ANSWER to that card, in the generalized Codex mutation ledger Phase 1
# built for exactly this ("answer, compose, interrupt"). `answers` and
# `answer_claims` are two more of the four tables v0.6.0 reads globally — its
# recovery sweeps `answer_claims` into `answers` without walking a session row —
# so a Codex claim in either is a row the old daemon would settle as its own, for
# a run it cannot see and a pane that does not exist. `mutation_ledger` is a
# table it has never heard of, which is the same isolation the card gets and for
# the same reason.
#
# Seeded TERMINAL (`indeterminate`), and deliberately so: this row has to be
# byte-identical at the end of the round trip, and a live `applying` claim is one
# the NEW daemon is supposed to settle at its next start. Terminal-beside-an-open-
# card is also a real state rather than a contrivance — it is what a request the
# app-server re-delivered after a bounce looks like: the server really is still
# waiting, the keyboard can still answer, and the phone is refused by the ledger.
# The live-claim recovery is proven on its own, in its own home, at (6b).
q "INSERT INTO mutation_ledger(operation_kind,session_uid,client_request_id,claimed_hash,thread_id,generation,route,target_turn_id,status,outcome,started_at,settled_at)
   VALUES('answer','$CX','$CX_REQ','h','$CX_THREAD',7,'accept','$CX_TURN','indeterminate',NULL,'2026-09-05T00:00:00.000Z','2026-09-05T00:00:01.000Z');"
[ "$(q "SELECT COUNT(*) FROM mutation_ledger WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the Codex answer seed did not land"; exit 1; }

# The complete durable Codex state, hashed. Anything the old daemon or the old
# CLI touches changes this — the run, its events, AND its open cards.
codex_state() { q "SELECT * FROM codex_sessions ORDER BY session_uid; SELECT * FROM events WHERE session_uid='$CX' ORDER BY seq; SELECT * FROM codex_pending_approvals ORDER BY request_id; SELECT * FROM mutation_ledger ORDER BY client_request_id;"; }
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
assert_uv 5 "after new reopen"
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
echo "  the old daemon is up on a v5 database it does not understand"

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
# runs against a v5 database, which is worth asserting and is all it says.
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
echo "  (2) every Codex row, event and open card is byte-for-byte what it was"
[ "$(q "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the old daemon's approval recovery reached the Codex card"; exit 1; }
echo "  (2) and the open Codex approval card survived the old daemon and its real prune"

echo "== 5b) THE FALSIFIABILITY ARM FOR THE CARD SPLIT =="
# Without this, "the card survived" is exactly what a green run would also
# report if v0.6.0 had never been able to touch `pending_approvals` at all — and
# then the split above would be ceremony rather than a fix.
#
# So the SAME card is staged where a build without this schema would have put
# it: the shared table, with the one object that refuses it removed.
#
# **On a home built from scratch, not a copy of `$H`.** The (b0) principle
# applies twice over here: a window must contain the old binary and only the old
# binary, and this window must also contain a database no other daemon has
# already recovered. `$H` has been through two old-daemon windows and a real
# prune by this point, and staging on top of that would make the measurement
# depend on all of it.
H3="$H.sharedcard"
mkdir -p "$H3"
sed "s/\"ws_port\": $PORT/\"ws_port\": $((PORT + 1))/" "$H/config.json" > "$H3/config.json"
run_at() { CODECONNECT_HOME="$3" timeout "$DAEMON_WINDOW" "$1" > "$3/$2.log" 2>&1 || [ $? = 124 ]; }
run_at "$NEW" new-sharedcard "$H3"
q3() { sqlite3 "$H3/events.db" "$1"; }
[ "$(q3 'PRAGMA user_version;')" = "5" ] || { echo "FAIL: (5b) staging home is not at v5"; exit 1; }
q3 "DROP TRIGGER pending_approvals_refuse_codex_card;"
[ "$(q3 "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='pending_approvals_refuse_codex_card';")" = "0" ] \
  || { echo "FAIL: the pre-trigger staging did not remove the card trigger, so (5b) proves nothing"; exit 1; }
# The trigger is gone, so these are the rows a build without the split really
# could have written. Staged by hand because no shipping binary can write them
# any more, which is the point. The CLAIM is staged with the CARD because the
# claim is what v0.6.0's recovery walks: `unresolved_answer_claims` reads
# `answer_claims` globally, with no session join and no state filter, and
# `settle_indeterminate` then deletes the matching card.
q3 "INSERT INTO codex_sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket,codex_generation)
    VALUES('$CX','cx-1','cx-1','codeconnect','/work/codex','live','t','t','codex','$CX_THREAD','/tmp/cch.x/ccd.sock',7);
    INSERT INTO pending_approvals(session_uid,session_id,request_id,card,generation,created_ms)
    VALUES('$CX','cx-1','$CX_REQ','{}',7,1787016966352);
    INSERT INTO codex_pending_approvals(session_uid,session_id,request_id,card,generation,created_ms,thread_id,turn_id,item_id,family)
    VALUES('$CX','cx-1','$CX_REQ','{}',7,1787016966352,'$CX_THREAD','$CX_TURN','$CX_ITEM','commandExecution');
    INSERT INTO answer_claims(session_uid,session_id,request_id,payload_hash,decision,started_at)
    VALUES('$CX','cx-1','$CX_REQ','h','\"allow\"','2026-08-28T00:00:00.000Z');
    INSERT INTO mutation_ledger(operation_kind,session_uid,client_request_id,claimed_hash,thread_id,generation,route,target_turn_id,status,outcome,started_at,settled_at)
    VALUES('answer','$CX','$CX_REQ','h','$CX_THREAD',7,'accept','$CX_TURN','applying',NULL,'2026-08-28T00:00:00.000Z',NULL);"
[ "$(q3 "SELECT COUNT(*) FROM pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  && [ "$(q3 "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  && [ "$(q3 "SELECT COUNT(*) FROM answer_claims WHERE session_uid='$CX';")" = "1" ] \
  && [ "$(q3 "SELECT COUNT(*) FROM mutation_ledger WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the (5b) staging did not land, so it would prove nothing"; exit 1; }
echo "  (5b) staged: the IDENTICAL card in BOTH tables, the claim v0.6.0 walks, and the"
echo "       Codex answer claim beside it in mutation_ledger"

run_at "$OLD" old-sharedcard "$H3"
SHARED_CARD_LEFT="$(q3 "SELECT COUNT(*) FROM pending_approvals WHERE session_uid='$CX';")"
SHARED_CLAIM_LEFT="$(q3 "SELECT COUNT(*) FROM answer_claims WHERE session_uid='$CX';")"
SCOPED_CARD_LEFT="$(q3 "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")"
# `AND status='applying'` is the whole assertion, not decoration: a v0.6.0 that DID
# reach this row and rewrote its status in place would leave COUNT(*) at 1 and pass a
# bare count. The claim is that the row is untouched, so the count has to be of rows
# that are still exactly as they were staged.
SCOPED_ANSWER_LEFT="$(q3 "SELECT COUNT(*) FROM mutation_ledger WHERE session_uid='$CX' AND status='applying';")"
echo "  (5b) MEASURED against the real v0.6.0: shared_card_left=$SHARED_CARD_LEFT"
echo "       claim_left=$SHARED_CLAIM_LEFT scoped_card_left=$SCOPED_CARD_LEFT"
echo "       scoped_answer_left=$SCOPED_ANSWER_LEFT"
# The harm, measured rather than asserted from a comment: v0.6.0's recovery
# walks `answer_claims` globally, records the claim as indeterminate, and
# DELETES the pending card — for a run it has no other way of seeing. It cannot
# even file the outcome, because `record_answer` needs a `sessions` row this uid
# does not have, so the card and the claim are both destroyed and nothing is
# written in their place.
[ "$SHARED_CARD_LEFT" = "0" ] && [ "$SHARED_CLAIM_LEFT" = "0" ] \
  || { echo "FAIL: with the trigger dropped and the card in the SHARED table, v0.6.0 left it";
       echo "      alone — so the split above is defending against nothing and this arm must be";
       echo "      re-derived against what v0.6.0 actually does:";
       grep -iE "recovery|approval|claim" "$H3/old-sharedcard.log" | head -10; exit 1; }
# And the same daemon, in the same run, on the same database, could not reach
# the scoped copy of the very same card.
[ "$SCOPED_CARD_LEFT" = "1" ] \
  || { echo "FAIL: v0.6.0 reached codex_pending_approvals, which it cannot name"; exit 1; }
# And neither could it reach the ANSWER to that card. The claim one table over,
# in `answer_claims`, is the one it just destroyed — same run, same request id,
# same daemon, same second.
[ "$SCOPED_ANSWER_LEFT" = "1" ] \
  || { echo "FAIL: v0.6.0 reached mutation_ledger, which it cannot name"; exit 1; }
grep -qiE "$(codex_identifiers)" "$H3/old-sharedcard.log" \
  && { echo "FAIL: (5b) the old daemon named Codex state in its log"; exit 1; } || true
echo "  (5b) THE HARM REPRODUCED: v0.6.0 recorded the Codex claim as indeterminate and"
echo "       DESTROYED the shared-table card (1 -> 0) and its claim (1 -> 0), for a run it"
echo "       cannot list, cannot name and cannot file an answer for. The IDENTICAL card in"
echo "       codex_pending_approvals survived the same daemon, in the same run, untouched."
echo "       Same binary, same database, same card, one table apart — so the split is the"
echo "       thing doing the work, and nothing else is."
rm -rf "$H3"


echo "== 6) new ccd reopens after the old prune: the forward path is intact =="
run "$NEW" new3
assert_uv 5 "after the final new reopen"
[ "$(q "SELECT COUNT(*) FROM codex_sessions WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the Codex run did not survive the whole round trip"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM sessions WHERE agent <> 'claude';")" = "0" ] \
  || { echo "FAIL: a Codex row leaked back into the shared table"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  || { echo "FAIL: the open Codex card did not survive the whole round trip"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM pending_approvals WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: a Codex card leaked into the shared table on the way back up"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM mutation_ledger WHERE session_uid='$CX' AND status='indeterminate';")" = "1" ] \
  || { echo "FAIL: the terminal Codex answer did not survive the whole round trip"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM answer_claims WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: a Codex answer leaked into the shared claim table on the way back up"; exit 1; }
[ "$(q "SELECT COUNT(*) FROM answers WHERE session_uid='$CX';")" = "0" ] \
  || { echo "FAIL: a Codex outcome leaked into the shared answers table"; exit 1; }
echo "  the Codex run, its open card and its terminal answer survived"
echo "  new -> old -> new -> old + real prune -> new"

echo "== 6b) a LIVE answer claim, recovered by the new binary the way a restart does =="
# Step 6 proves a TERMINAL answer claim survives the round trip untouched. This
# proves the other half: an `applying` claim — a phone answer this daemon was in
# the middle of writing when it stopped — is made terminal at the next start, its
# card is retired with it, and neither half lands in a table v0.6.0 can reach.
#
# **On a home of its own, and rebuilt from scratch**, for (5b)'s reason: `$H` has
# been through two old-daemon windows and a real prune, and a recovery measured on
# top of all that would be measuring all of it. Here the only thing that has ever
# touched the database is the binary under test.
#
# The card is seeded as a DECODABLE `ApprovalCard`, unlike (5b)'s `{}` placeholder.
# That is load-bearing rather than tidy: recovery restores the cards into memory
# first and retirement claims one by removing it from that map, so a card the
# restore had to drop would make the retire find nothing and hide the very
# ordering this step exists to check.
H4="$H.livedanswer"
mkdir -p "$H4"
sed "s/\"ws_port\": $PORT/\"ws_port\": $((PORT + 2))/" "$H/config.json" > "$H4/config.json"
run_at "$NEW" new-livedanswer-build "$H4"
q4() { sqlite3 "$H4/events.db" "$1"; }
[ "$(q4 'PRAGMA user_version;')" = "5" ] || { echo "FAIL: (6b) staging home is not at v5"; exit 1; }
CARD_JSON='{"request_id":"'"$CX_REQ"'","payload_hash":"h","tool_name":"Bash","tool_input":{"command":"touch /tmp/a"},"display_text":"touch /tmp/a","generation":7,"identity_bound":false}'
q4 "INSERT INTO codex_sessions(session_uid,session_id,tmux_session,tmux_socket,cwd,lifecycle,created_at,updated_at,agent,codex_thread_id,codex_socket,codex_generation)
    VALUES('$CX','cx-1','cx-1','codeconnect','/work/codex','live','t','t','codex','$CX_THREAD','/tmp/cch.x/ccd.sock',7);
    INSERT INTO codex_pending_approvals(session_uid,session_id,request_id,card,generation,created_ms,thread_id,turn_id,item_id,family)
    VALUES('$CX','cx-1','$CX_REQ','$CARD_JSON',7,1787016966352,'$CX_THREAD','$CX_TURN','$CX_ITEM','commandExecution');
    INSERT INTO mutation_ledger(operation_kind,session_uid,client_request_id,claimed_hash,thread_id,generation,route,target_turn_id,status,outcome,started_at,settled_at)
    VALUES('answer','$CX','$CX_REQ','h','$CX_THREAD',7,'accept','$CX_TURN','applying',NULL,'2026-08-28T00:00:00.000Z',NULL);"
[ "$(q4 "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")" = "1" ] \
  && [ "$(q4 "SELECT COUNT(*) FROM mutation_ledger WHERE session_uid='$CX' AND status='applying';")" = "1" ] \
  || { echo "FAIL: the (6b) staging did not land, so it would prove nothing"; exit 1; }
echo "  (6b) staged: an open Codex card and the live answer claim a kill would leave"

run_at "$NEW" new-livedanswer "$H4"
LIVE_CARD_LEFT="$(q4 "SELECT COUNT(*) FROM codex_pending_approvals WHERE session_uid='$CX';")"
LIVE_CLAIM_STATUS="$(q4 "SELECT status FROM mutation_ledger WHERE session_uid='$CX' AND client_request_id='$CX_REQ';")"
LIVE_RESOLVED="$(q4 "SELECT COUNT(*) FROM events WHERE session_uid='$CX' AND kind='approval_resolved';")"
LIVE_SHARED_ANSWERS="$(q4 "SELECT COUNT(*) FROM answers WHERE session_uid='$CX';")"
echo "  (6b) MEASURED: card_left=$LIVE_CARD_LEFT claim_status=$LIVE_CLAIM_STATUS"
echo "       resolutions=$LIVE_RESOLVED shared_answers=$LIVE_SHARED_ANSWERS"
[ "$LIVE_CARD_LEFT" = "0" ] \
  || { echo "FAIL: (6b) the card outlived its own answer, so every future tap refuses it"; exit 1; }
[ "$LIVE_CLAIM_STATUS" = "indeterminate" ] \
  || { echo "FAIL: (6b) a claim nothing settled must be terminal, or the answer is sent twice"; exit 1; }
[ "$LIVE_RESOLVED" = "1" ] \
  || { echo "FAIL: (6b) the card was retired with no terminal anybody can read"; exit 1; }
[ "$LIVE_SHARED_ANSWERS" = "0" ] \
  || { echo "FAIL: (6b) a Codex recovery wrote the shared answers table"; exit 1; }
# A real assertion, not a no-op. `grep -q` prints nothing and `|| true` swallowed its
# status, so this line could neither pass nor fail — it measured nothing while looking
# like the arm-5b check it was copied from. Here the NEW daemon is the one running, and
# it is entitled to name Codex state in its own log; what it must not do is stay silent
# about a recovery it performed, because a silent recovery is one an operator cannot
# audit. So the direction is inverted from 5b's: a hit is required.
grep -qiE "$(codex_identifiers)" "$H4/new-livedanswer.log" \
  || { echo "FAIL: (6b) the new daemon recovered a Codex answer and said nothing about it in its log"; exit 1; }
echo "  (6b) the card is retired, the claim is terminal, the resolution is filed, and"
echo "       nothing v0.6.0 can read was written"
rm -rf "$H4"

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
# **Why this half is still a hand-written frame, and what changed under it.**
# The reason used to be that nothing that ships could send a Codex `Register` at
# all: `registration_frame` wrote `agent: AgentKind::Claude` as a literal. That
# is no longer true — 2e-7b built the producer, and `registration_frame` now
# reads the agent off a Codex seat. What is deliberately still absent is an
# *argv* path to it: the producer is the Codex coordinator, which supervises its
# own launch in-process and hands the seat across as values, so there is no
# `--agent` flag on `codeconnect supervise` for a harness to reach for. Reaching
# the real producer therefore means launching a real Codex session — a real
# codex binary, a real tmux server, a real app-server and TUI — which is exactly
# what this harness is built not to need, and what the live registration gate
# does instead (`codeconnect`'s `live_codex_registration`).
#
# So the frame stays hand-written, and it is not fiction: it is the frame
# `supervisor::registration_frame` produces from a seat, asserted field by field
# on the wire — agent, control-link socket, generation, no `claude_bin` and no
# thread id — by
# `supervisor::tests::a_codex_registration_is_sent_once_the_daemon_says_it_hosts_codex`.
# What (g) below adds is the half of the producer that this harness CAN drive
# with the real binary and no codex session at all: the launcher's preflight.
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

# Wait until a daemon under `$1` (a CODECONNECT_HOME) is **accepting**, not until
# its socket file exists.
#
# Every daemon in this script binds the same `$H/ccd.sock`, and `ccd` creates the
# file before it accepts on it — so `[ -S ... ]` can be satisfied instantly by the
# inode the PREVIOUS daemon left behind, and the arm that follows then races a
# daemon that is not up yet. A connect is the only readiness signal that is about
# this daemon. (The Rust live harness makes the same correction for the same
# reason.)
daemon_accepting() {
  for _ in $(seq 1 60); do
    if CC_SOCK="$1/ccd.sock" python3 - <<'PY'
import os, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.settimeout(1)
try:
    s.connect(os.environ["CC_SOCK"])
except Exception:
    sys.exit(1)
PY
    then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

CODEX_BEFORE_SUP="$(codex_state)"
CODECONNECT_HOME="$H" "$OLD" > "$H/old-live4.log" 2>&1 &
OLDPID=$!
daemon_accepting "$H" || { echo "FAIL: the old daemon did not accept for step 7(f)"; tail -20 "$H/old-live4.log"; exit 1; }
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

# (g) THE LAUNCHER'S PREFLIGHT, with the real new binary against the real old daemon.
#
# The withhold in (e) is what protects this daemon's history once a session is
# running. This is the half in front of it: `codeconnect codex` asks the same
# question, on the same wire, BEFORE it creates anything, and refuses to start a
# session that the daemon it can see could never be told about. Without it the
# operator gets a working TUI that no fleet, phone or `sessions list` will ever
# show, and no error anywhere.
#
# **Why a stub codex, and what it does not stand in for.** The launcher resolves
# and version-pins its binary before it asks the daemon anything, so reaching the
# preflight at all needs *a* codex on the path — and a native one: a `#!` script
# is refused as a wrapper by design. The stub is a four-line C program that
# answers `--version` with a pinned version and nothing else, which is all the
# steps before the preflight ask of it. It stands in for the binary, never for
# the daemon: both daemons below are real, and the answers they give are their
# own. `cc` is present by construction — cargo linked the binaries this harness
# is running a few hundred lines above.
STUB="$H/stub"
mkdir -p "$STUB"
cat > "$STUB/codex.c" <<'STUBC'
#include <stdio.h>
int main(void) { printf("codex-cli 0.147.0\n"); return 0; }
STUBC
cc -o "$STUB/codex" "$STUB/codex.c" 2>"$H/stub-cc.log" \
  || { echo "FAIL: could not build the stub codex, so the preflight arm cannot run"; cat "$H/stub-cc.log"; exit 1; }

CODECONNECT_HOME="$H" "$OLD" > "$H/old-live6.log" 2>&1 &
OLDPID=$!
daemon_accepting "$H" || { echo "FAIL: the old daemon did not accept for step 7(g)"; tail -20 "$H/old-live6.log"; exit 1; }
kill -0 $OLDPID 2>/dev/null || { echo "FAIL: the old daemon did not come up for step 7(g)"; tail -20 "$H/old-live6.log"; exit 1; }
CODEX_BEFORE_PREFLIGHT="$(codex_state)"
# **This daemon's own descriptor baseline**, taken here rather than reused.
# `CONNS_BASE` belongs to the arm-(f) daemon, which was killed hundreds of lines
# ago; comparing a DIFFERENT process's count against it is comparing two
# unrelated numbers that happen to be equal for a daemon whose listener count is
# the same. Same timing doctrine as the `codex_state` bracket above: capture it
# against the process the claim is about, immediately before the thing being
# measured.
CONNS_G_OLD="$(daemon_ipc_conns $OLDPID)"
[ "$CONNS_G_OLD" -ge 1 ] \
  || { echo "FAIL: lsof sees no unix socket for the step 7(g) daemon, so its connection count proves nothing"; kill $OLDPID 2>/dev/null; exit 1; }
PRE_RC=0
CODECONNECT_HOME="$H" CODECONNECT_CODEX_BIN="$STUB/codex" \
  "$NEWCC" codex > "$H/preflight-old.log" 2>&1 || PRE_RC=$?
[ "$PRE_RC" -ne 0 ] \
  || { echo "FAIL: the launcher started a Codex session against a daemon that cannot host one"; cat "$H/preflight-old.log"; kill $OLDPID 2>/dev/null; exit 1; }
grep -q "refusing to launch" "$H/preflight-old.log" \
  || { echo "FAIL: the launcher's refusal is not the preflight's:"; cat "$H/preflight-old.log"; kill $OLDPID 2>/dev/null; exit 1; }
grep -q "predates the agent seam" "$H/preflight-old.log" \
  || { echo "FAIL: the refusal does not name what the daemon actually answered:"; cat "$H/preflight-old.log"; kill $OLDPID 2>/dev/null; exit 1; }
# Non-mutating, and measured rather than argued: the question is one frame on a
# fresh connection and the daemon's own state is unchanged by having been asked.
[ "$(codex_state)" = "$CODEX_BEFORE_PREFLIGHT" ] \
  || { echo "FAIL: the preflight mutated Codex state"; kill $OLDPID 2>/dev/null; exit 1; }
[ "$(daemon_ipc_conns $OLDPID)" = "$CONNS_G_OLD" ] \
  || { echo "FAIL: the preflight left a connection behind on the old daemon"; kill $OLDPID 2>/dev/null; exit 1; }
# **NO DURABLE LAUNCH RECORD — and that is exactly, and only, what this measures.**
# While the command was gated this was true for free: the launcher could not launch
# at all. The launcher is live now, so "the refusal lands before a launch exists" is
# a claim about ordering that has to be measured. The launch record is the first
# durable artifact of any launch (the coordinator writes it as its first act), so
# its absence is the evidence: a preflight that ran too late would leave one behind
# for a session this daemon can never be told about.
#
# **Round-4 finding 3: what this does NOT prove.** An absent record says no launch
# was recorded. It does not say no uid was minted and no `cc-N` was taken — those
# happen inside `codex::launch`, leave nothing on disk of their own, and a mutation
# moving either in front of `refuse_unless_hostable` would still pass this arm. That
# ordering is real and it is pinned where it is visible, in
# `codex.rs::the_preflight_refuses_before_a_uid_or_a_tmux_name_is_taken`, which reads
# the source of `start` and `launch`. This line stays narrow on purpose rather than
# borrowing that test's claim.
[ -z "$(ls "$H"/sessions/*/launch.json 2>/dev/null)" ] \
  || { echo "FAIL: the preflight refused AFTER a launch was recorded: $(ls "$H"/sessions/*/launch.json)"; kill $OLDPID 2>/dev/null; exit 1; }
kill $OLDPID 2>/dev/null; wait $OLDPID 2>/dev/null

# **The falsifiability arm.** A launcher that refused for some unrelated reason —
# a bad stub, a missing home, an argv it disliked — would satisfy every check
# above. So the identical command is run again with the only difference being
# WHICH DAEMON IS LISTENING, and it must get further: past the preflight, into the
# launch itself. Same binary, same stub, same home; a different answer on the wire.
#
# **Ungate (2e-7d): what "further" means here changed, and so did the isolation.**
# This arm used to assert the gate's own "not yet enabled" line, which was a
# dependency on the refusal existing; the refusal is gone, so the marker is now the
# first durable artifact of a real launch — a launch record the launcher spawned a
# coordinator to write and then waited on. `TMUX_TMPDIR` is set for the same reason:
# a live launcher takes a `cc-N` name and creates a tmux session, and it must take
# them on this arm's own throwaway server rather than on the operator's fleet.
GTMUX="$H/tmux-g"
mkdir -p "$GTMUX"
CODECONNECT_HOME="$H" "$NEW" > "$H/new-live-preflight.log" 2>&1 &
NEWPID=$!
daemon_accepting "$H" || { echo "FAIL: the new daemon did not accept for step 7(g)"; tail -20 "$H/new-live-preflight.log"; exit 1; }
kill -0 $NEWPID 2>/dev/null || { echo "FAIL: the new daemon did not come up for step 7(g)"; tail -20 "$H/new-live-preflight.log"; exit 1; }
# **The affirmative answer, DRIVEN — on the LAUNCHER'S OWN connection.**
#
# Reaching the launch gate is not proof that this daemon said yes:
# `refuse_unless_hostable` refuses only a decoded "no", so an `Absent` or
# `Indeterminate` preflight — a daemon that never came up, a socket that was not
# there, a reply that could not be parsed — reaches the same gate and prints the
# same line. That would make this arm's whole claim ("a different answer on the
# wire") true of no answer at all.
#
# Round-2 F5: asking on a SECOND connection of the harness's own did not fix that.
# The probe's `supported:true` came back on a different round trip, so killing or
# wedging the daemon between the probe and the launcher left this arm passing on
# an answer the launcher never received. The observation has to be of the
# launcher's own negotiation, and the only party that sees that is the daemon.
#
# So the harness asks NOTHING here, and afterwards reads what the daemon logged.
# The count is what ties the line to the launcher: this daemon is fresh, this arm
# runs one command against it, and `daemon_accepting` only connects — so exactly
# one negotiation reached it, and it was the launcher's.
CODECONNECT_HOME="$H" CODECONNECT_CODEX_BIN="$STUB/codex" TMUX_TMPDIR="$GTMUX" \
  timeout 150 "$NEWCC" codex > "$H/preflight-new.log" 2>&1 || true
# The daemon writes its line once the answer is ON the connection's write queue
# (round-3 F6: written before the enqueue and with the result discarded, this line
# was an affirmative that a DROPPED answer could still produce). The enqueue and the
# log file write race for a moment; give the line a bounded chance to land rather
# than reading an empty file and calling it a failure.
for _ in $(seq 1 40); do
  grep -q 'negotiate_support for' "$H/new-live-preflight.log" && break
  sleep 0.1
done
NEGOTIATIONS="$(grep -c 'negotiate_support for' "$H/new-live-preflight.log" || true)"
[ "$NEGOTIATIONS" = "1" ] \
  || { echo "FAIL: expected exactly one negotiation on this daemon — the launcher's — and saw $NEGOTIATIONS:"; grep 'negotiate_support' "$H/new-live-preflight.log" || true; exit 1; }
grep -q 'negotiate_support for codex answered supported=true' "$H/new-live-preflight.log" \
  || { echo "FAIL: the daemon did not answer the LAUNCHER's support question affirmatively:"; grep 'negotiate_support' "$H/new-live-preflight.log" || true; exit 1; }
kill $NEWPID 2>/dev/null; wait $NEWPID 2>/dev/null; NEWPID=""
# `if`, not `&&`: under this script's `set -e`, a `grep -q … && { … }` whose grep
# finds nothing is a compound statement that returned non-zero, so the script
# would exit HERE — on the PASSING path, with every assertion below silently
# unrun. The same shape as the `timeout`/143 note above.
if grep -q "refusing to launch" "$H/preflight-new.log"; then
  echo "FAIL: the preflight refused a daemon that hosts Codex, so the refusal above proves nothing about the daemon"
  cat "$H/preflight-new.log"
  exit 1
fi
# **It launched.** The record is what the coordinator writes as its first act and
# what the launcher then waits on, so one existing here is proof the command went
# all the way through the wiring the preflight guards: minted an identity, spawned a
# coordinator, and waited on its outcome. (Against the OLD daemon, the identical
# command left none — asserted above.)
GREC="$(ls "$H"/sessions/*/launch.json 2>/dev/null | head -1)"
[ -n "$GREC" ] \
  || { echo "FAIL: against a hosting daemon the launcher never started a launch; it stopped somewhere else:"; cat "$H/preflight-new.log"; exit 1; }
# And it reached a TERMINAL outcome rather than being abandoned mid-flight. The
# stub answers `--version` and nothing else, so it cannot host a session: the
# app-server it is exec'd as exits immediately, bring-up fails, and the coordinator
# terminalizes the record. `Failed` here is the honest end of a real launch, not a
# refusal — which is exactly the distinction this arm exists to draw.
grep -q '"Failed"' "$GREC" \
  || { echo "FAIL: the launch record never reached a terminal state:"; cat "$GREC"; cat "$H/preflight-new.log"; exit 1; }
# **The launcher reported the RECORD's reason, not one of its own.** This is the
# wire between `wait_on_record` and the terminal, and without it a launch could
# fail for one reason and be reported for another.
#
# Taken FROM the record rather than written down here, because the stub produces
# several failure shapes and which one lands is a property of the machine. The stub
# answers `--version` and exits, so the pane's command dies within milliseconds of
# its `execve`; whether the coordinator notices during the bookkeeping that follows
# `new-session` ("the created tmux session could not be made safe: remain-on-exit
# could not be cleared…"), later in the bring-up wait ("wrapper bring-up failed: the
# wrapper did not prove ready…"), or not before its own 60s budget runs out ("launch
# deadline expired", seen on a loaded machine) is a race between two processes. All
# three have been observed. Pinning any one of them would make this arm flaky for a
# reason that has nothing to do with what it asserts — which is only that the
# terminal shows what the record holds.
# **Compared COMPLETE and EXACT** (round-4 finding 2). This used to `sed` the value
# out and `grep -F` its first 40 characters, which was two holes at once. The prefix
# was an unanchored substring, so a launcher that printed `different preface: <reason>`,
# or appended a story of its own after the reason, or emitted the reason buried in
# unrelated output, all passed. And the `sed`'d value is the JSON-ESCAPED spelling
# while the terminal carries the decoded one, so any reason containing a character
# JSON escapes was being compared against a string the launcher could never print.
#
# So: parse the record as JSON, and require the launcher's WHOLE output to be
# anyhow's own `Error: <reason>` presentation and nothing else. That is the exact
# rendering `codex::launch`'s `bail!("{reason}")` produces through
# `main() -> Result<()>`, and on this path the launcher prints nothing else at all.
CC_REC="$GREC" CC_OUT="$H/preflight-new.log" python3 - <<'PY' || exit 1
import json, os, sys, unicodedata

rec = json.load(open(os.environ["CC_REC"], encoding="utf-8"))
state = rec.get("state")
if not isinstance(state, dict) or "Failed" not in state:
    sys.exit(f"FAIL: the terminal record carries no failure reason to compare against: {state!r}")
reason = state["Failed"]["reason"]

# The launcher prints `wait_on_record`'s SANITIZED reason (single line, printable,
# 300 chars). Rather than reimplement that sanitizer here — a second copy of
# production logic, which the harness would then only be proving against itself —
# assert the recorded reason is already a FIXED POINT of it and compare verbatim.
# Every reason this arm can produce is; one that is not fails here, loudly, rather
# than quietly relaxing the comparison below.
if (reason != reason.strip()
        or any(unicodedata.category(c) == "Cc" for c in reason)
        or len(reason) > 300):
    sys.exit(f"FAIL: the recorded reason is not already terminal-safe, so this arm "
             f"cannot compare it verbatim: {reason!r}")

printed = open(os.environ["CC_OUT"], encoding="utf-8", errors="replace").read()
expected = f"Error: {reason}\n"
if printed != expected:
    sys.exit("FAIL: the launcher printed something other than the recorded reason, exactly.\n"
             f"  EXPECTED: {expected!r}\n"
             f"  PRINTED:  {printed!r}")
PY
# Nothing survives it. A failed launch owes no tmux session and no run dir; the
# coordinator and its custodian own that cleanup, and this is where it is measured.
if [ -x "$(command -v tmux || echo /nonexistent)" ]; then
  # `|| true` INSIDE the substitution, and it is load-bearing under this script's
  # `set -o pipefail`: "no server running" is tmux's answer for the PASSING case
  # here, and it is a non-zero exit. Without the guard the pipeline fails, the
  # assignment inherits that status, and `set -e` kills the script on the success
  # path with every assertion below silently unrun — the same shape as the
  # `grep -q … && { … }` note further up.
  LEFT="$( { TMUX_TMPDIR="$GTMUX" tmux -L codeconnect list-sessions 2>/dev/null || true; } | wc -l | tr -d ' ')"
  [ "$LEFT" = "0" ] \
    || { echo "FAIL: the failed launch left $LEFT tmux session(s) behind:"; TMUX_TMPDIR="$GTMUX" tmux -L codeconnect list-sessions 2>&1; exit 1; }
fi
# The record is pretty-printed, so the separator carries a space: `"run_dir": "…"`.
GRUN="$(sed -n 's/.*"run_dir"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$GREC" | head -1)"
if [ -n "$GRUN" ]; then
  [ ! -e "$GRUN" ] \
    || { echo "FAIL: the failed launch left its run dir behind at $GRUN"; ls -la "$GRUN"; exit 1; }
fi
# **And no PROCESS survives it either** (round-4 finding 4). The two checks above are
# about resources; "nothing survives" is also a claim about the two processes that
# own them. A coordinator or custodian that wrote `Failed`, swept the session and the
# run dir, and then simply kept running would satisfy every assertion above while
# leaving exactly the kind of thing this whole gate exists to find — the live
# `live_codex_coordinator` teardown gates check the recorded identities for that
# reason, and this arm now does the same.
#
# By the identities the RECORD names, not by a name or a `ps` grep: the coordinator
# is `codeconnect internal-codex-coordinator` and the custodian carries neither the
# uid nor the run dir in its argv, so nothing else here can address them. Liveness is
# `kill -0`, i.e. pid alone; the record's birth stamp is not compared, and the honest
# consequence is stated rather than hidden: a pid recycled inside this window would
# make this arm FAIL, never pass. It is bounded strictly the wrong way for masking.
GPIDS="$(CC_REC="$GREC" python3 -c '
import json, os
rec = json.load(open(os.environ["CC_REC"], encoding="utf-8"))
for field in ("coordinator", "custodian"):
    who = rec.get(field)
    if isinstance(who, dict) and isinstance(who.get("pid"), int):
        print(field, who["pid"])
')"
[ -n "$GPIDS" ] \
  || { echo "FAIL: the terminal record names no coordinator, so this arm cannot say what became of it:"; cat "$GREC"; exit 1; }
while read -r GWHO GPID; do
  [ -n "${GPID:-}" ] || continue
  GONE=0
  for _ in $(seq 1 300); do
    kill -0 "$GPID" 2>/dev/null || { GONE=1; break; }
    sleep 0.1
  done
  [ "$GONE" = "1" ] \
    || { echo "FAIL: the failed launch's $GWHO (pid $GPID) is still alive 30s after the record went terminal:"; ps -p "$GPID" -o pid=,ppid=,lstart=,command= 2>&1; kill -KILL "$GPID" 2>/dev/null || true; exit 1; }
done <<< "$GPIDS"
echo "  (g) the REAL launcher refused to start a Codex session against the rolled-back daemon,"
echo "      naming the daemon's own answer, mutating nothing, leaving no connection AND no"
echo "      launch record — the refusal lands before a launch is recorded — and the same"
echo "      command against the new daemon got past the preflight and actually LAUNCHED:"
echo "      a coordinator wrote a record, the launcher waited on it and printed EXACTLY the"
echo "      recorded reason and nothing else, and the failed launch left no tmux session, no"
echo "      run dir and neither of the processes its own record names still running,"
echo "      with the new daemon's OWN log showing exactly one negotiation — the launcher's —"
echo "      answered supported=true, so the affirmative control is the launcher's round trip"
echo "      rather than a second one the harness made on its own connection"

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
echo "      none of them left behind. The Codex Register of step 7 stays hand-written for"
echo "      one reason and it is written down beside it: the producer is now real, but it"
echo "      is the Codex coordinator supervising its own launch, so reaching it means"
echo "      running a real codex session — which this harness is built not to need."
echo ""
echo "      What the real new binary DID drive here is the half in front of the withhold:"
echo "      \`codeconnect codex\` asked the rolled-back daemon the same question, read the"
echo "      same undecodable-variant error, and REFUSED TO START — naming the daemon's own"
echo "      answer, mutating no Codex state and leaving no connection behind: the refusal"
echo "      lands before a launch record exists. The identical command against the new"
echo "      daemon got past that preflight and LAUNCHED for real, so the refusal is the"
echo "      daemon's doing and not the launcher's mood."
