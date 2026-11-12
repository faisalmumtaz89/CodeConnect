#!/bin/bash
# The agent host's tests, run against a built executable.
#
#   mac/agent-host/test.sh <executable>
#
# The executable is both the Bun that runs `bun test` (BUN_BE_BUN=1), so the tests run on the pinned Bun the host is
# made from, and the host the tests start (CC_AGENT_HOST), each with an environment of its own that holds no
# BUN_BE_BUN. test/protocol.test.ts type-checks src/protocol.ts and src/link.ts against fixtures/host/messages.jsonl,
# the lines ccd's own tests emit; test/host.test.ts drives the executable over its socket. The tests import only
# Bun's and Node's built-in modules, so nothing is installed, and they write only into temporary folders.
set -euo pipefail
executable="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
cd "$(dirname "$0")"
CC_AGENT_HOST="$executable" BUN_BE_BUN=1 "$executable" test ./test/
