#!/bin/bash
# Whether an agent host executable runs on this machine as it is signed.
#
#   mac/agent-host/check.sh <executable>
#
# The executable runs as the Bun it is built on (BUN_BE_BUN=1), so the check holds whatever src/ contains: it creates a
# SharedArrayBuffer, which JavaScriptCore withholds when it cannot JIT (a macOS executable under the hardened runtime
# without allow-jit), and starts a child that answers on its standard output, as the SDK starts `claude`. The answer
# must come back unchanged.
set -euo pipefail
executable="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat > "$work/child" <<'EOF'
#!/bin/sh
echo "answered $1"
EOF
chmod 0755 "$work/child"
said="$(cd "$work" && CHILD="$work/child" BUN_BE_BUN=1 "$executable" -e '
  new Int32Array(new SharedArrayBuffer(8));
  const run = require("node:child_process").spawnSync(process.env.CHILD, ["started"], { encoding: "utf8" });
  process.stdout.write(run.stdout);
')"
if [ "$said" != "answered started" ]; then
  echo "$executable did not run here as it is signed: it said '$said'" >&2
  exit 1
fi
echo "$executable runs here: it started a child and read its answer"
