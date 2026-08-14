#!/bin/bash
# Run the chaos gauntlet against a real `codeconnect claude` session.
#
# What this script exists to do is start a *genuine* session — not a fixture —
# and then get out of the way. `codeconnect claude` execs into a tmux client, so it needs
# a terminal; the trick is to give it one by running it inside a pane on a
# throwaway tmux server (`-L ccsoak-driver`). The session it creates lives on the
# `codeconnect` server, so tearing the driver down at the end detaches a client
# and leaves nothing behind.
#
#   soak/run.sh                 # build, start a session, run every scenario
#   soak/run.sh --keep          # leave the session running afterwards
#   soak/run.sh --session cc-1  # use a session that is already running
#   soak/run.sh -- kill         # pass a scenario (and any flags) to ccsoak
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
prefix="${CODECONNECT_HOME:-$HOME/.codeconnect}"
bin="$prefix/bin"
driver_server="ccsoak-driver"

keep=0
existing=""
scenario_args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) keep=1; shift ;;
        --session) existing="${2:-}"; shift 2 ;;
        --) shift; scenario_args=("$@"); break ;;
        *) scenario_args=("$@"); break ;;
    esac
done

tmux_bin="$(command -v tmux || echo /opt/homebrew/bin/tmux)"

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "building"
# Release for the installed binaries (that is what is under test), debug for the
# harness itself — the gauntlet is IO-bound and a release build of it would only
# add a minute of LTO to every run.
"$root/install.sh" >/dev/null
cargo build --quiet --manifest-path "$root/Cargo.toml" -p ccsoak
soak_bin="$root/target/debug/ccsoak"

say "daemon"
# The kill storm needs something to bring ccd back. Installing is idempotent and
# takes over from a daemon started by hand.
"$bin/codeconnect" daemon status || true
if ! "$bin/codeconnect" daemon status | grep -q 'managed  yes'; then
    "$bin/codeconnect" daemon install
else
    # Already managed — and therefore still running the binary it was started
    # with. `install.sh` above replaced the file on disk; launchd keeps
    # executing the process it already has. Without this restart the gauntlet
    # attacks the *previous* build and reports passes for code that was never
    # loaded, which ccsoak now also refuses on a protocol-minor mismatch.
    "$bin/codeconnect" daemon restart
fi

session_ref="$existing"
workdir=""

# Is Claude's composer on this pane?
#
# The box, which is what the daemon's own presence check reads: a row of
# box-drawing horizontals with the `❯` prompt row directly beneath it, both
# starting at column 0. Barrier and daemon therefore agree on what a composer
# is, and the gauntlet cannot start on a screen production would refuse.
#
# The footer hints cannot carry this. `? for shortcuts` is dropped the moment
# the shift+tab mode hint needs the room, and `← for agents` becomes
# `← 1 agent` once a subagent exists — so a barrier keyed on them spends its
# whole budget on a perfectly live composer, and `case` matching nothing is a
# success, so `set -e` never sees it: the gauntlet starts on a wall-clock guess
# and the scenarios that check the composer report `skipped`.
#
# awk over literal byte sequences rather than a `grep` pipeline: this runs on
# whatever grep and locale the operator has, and `[[:space:]]` against a
# no-break space is not the same answer everywhere.
composer_is_drawn() {
    printf '%s\n' "$1" | awk '
        /^(─|━|═)+[ \t]*$/ { rule = 1; next }
        /^❯/ && rule       { found = 1; exit }
                           { rule = 0 }
        END                { exit found ? 0 : 1 }
    '
}

# The trap exists BEFORE anything is spawned: a failure after a spawn but
# before a later trap would leak the driver server, the session, and the
# workdir. Everything it cleans is guarded on being set, so installing it
# early costs nothing.
cleanup() {
    if [ "$keep" -eq 1 ]; then
        echo
        echo "left running: session $session_ref${workdir:+ in $workdir}"
        echo "  attach with: $bin/codeconnect attach $session_ref"
        return
    fi
    if [ -n "$workdir" ]; then
        "$tmux_bin" -L "$driver_server" kill-server 2>/dev/null || true
        if [ -z "$session_ref" ]; then
            # The session may exist without ever having registered — created
            # by `codeconnect claude` moments before a failure — and the
            # daemon prints no cwd for a session it never met. tmux itself
            # always knows: its panes carry their working directory, and this
            # script's own temp dir is unambiguous.
            # Tab-separated, because a working directory may contain spaces
            # and whitespace-split fields would never match it.
            session_ref="$("$tmux_bin" -L codeconnect list-panes -a \
                -F "#{session_name}$(printf '\t')#{pane_current_path}" 2>/dev/null \
                | awk -F '\t' -v d="$workdir" '$2 == d {print $1; exit}')" || true
        fi
        if [ -n "$session_ref" ]; then
            "$tmux_bin" -L codeconnect kill-session -t "=$session_ref" 2>/dev/null || true
        fi
        rm -rf "$workdir"
    fi
}
trap cleanup EXIT
if [ -z "$session_ref" ]; then
    say "starting a session"
    # `pwd -P`: mktemp hands back /var/folders/…, which is a symlink to
    # /private/var/folders/…. `codeconnect claude` records the physical path, so matching
    # on the logical one would never find the session it just created.
    workdir="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/ccsoak-work.XXXXXX")" && pwd -P)"
    # A tiny repo so `get_diff` and the transcript have something real to chew
    # on, and so the agent's cheap prompts have a directory to sit in.
    git -C "$workdir" init --quiet
    printf 'soak\n' > "$workdir/README.md"
    git -C "$workdir" add README.md
    git -C "$workdir" -c user.email=soak@local -c user.name=soak commit --quiet -m init

    "$tmux_bin" -L "$driver_server" kill-server 2>/dev/null || true
    "$tmux_bin" -L "$driver_server" new-session -d -s driver -x 200 -y 50 \
        -c "$workdir" "$bin/codeconnect claude; sleep 600"

    printf 'waiting for the session to register'
    for _ in $(seq 1 60); do
        # `index` rather than a field comparison: a working directory may
        # contain spaces, and awk would split it across columns.
        found="$("$bin/codeconnect" ls 2>/dev/null | awk -v d="$workdir" 'index($0, d) {print $1; exit}')"
        if [ -n "$found" ]; then
            session_ref="$found"
            break
        fi
        printf '.'
        sleep 1
    done
    printf '\n'
    if [ -z "$session_ref" ]; then
        echo "no session appeared in $workdir; is claude installed and logged in?" >&2
        exit 1
    fi
    echo "session $session_ref in $workdir"

    # Claude asks whether it can trust a directory it has not seen before, and
    # the composer never appears until that is answered. Answering it here is
    # the operator's decision to make — the directory is one this script created
    # a moment ago — and without it every scenario that types would be refused
    # for a reason that has nothing to do with what is being tested.
    printf 'waiting for the composer'
    for _ in $(seq 1 40); do
        pane="$("$tmux_bin" -L codeconnect capture-pane -p -J -t "=$session_ref:" 2>/dev/null || true)"
        case "$pane" in
            *"trust this folder"*)
                "$tmux_bin" -L codeconnect send-keys -t "=$session_ref:" '1'
                sleep 0.3
                "$tmux_bin" -L codeconnect send-keys -t "=$session_ref:" Enter
                ;;
        esac
        if composer_is_drawn "$pane"; then
            printf ' ready\n'
            break
        fi
        printf '.'
        sleep 1
    done
fi

say "gauntlet"
set +e
"$soak_bin" --session "$session_ref" "${scenario_args[@]:-all}"
status=$?
set -e
exit "$status"
