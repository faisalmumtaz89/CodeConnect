# Pinned Codex app-server method census — schema 0.147

`methods-stable.json` / `methods-experimental.json` are the **version-pinned** enumeration
of every client→server method (JSON-RPC `ClientRequest` + `ClientNotification`) in the two
Codex app-server schema bundles CodeConnect vendors. They are the source of truth for the
broker allowlist's **exhaustiveness check**: every method here must carry exactly one
disposition, and the four wire-native code-exec bypass methods (`command/exec`,
`thread/shellCommand`, `process/spawn`, `fs/writeFile`) plus any unknown/future method must
resolve to `refuse`.

Counts (must match the plan's "95 stable / 133 experimental methods"):
- stable:       95 client requests + 1 client notification (`initialized`)
- experimental: 133 client requests + 1 client notification (`initialized`)

Derivation: the `method` const/enum of each top-level `oneOf` variant of
`schema-0.147/{stable,experimental}/ClientRequest.json` and `ClientNotification.json` in the
Phase-0 spike bundle. Regenerate by re-running the extraction against a re-vendored bundle;
a drift (a method added/removed by a new Codex release) fails the exhaustiveness test **before**
the broker is exposed, which is the point of pinning.
