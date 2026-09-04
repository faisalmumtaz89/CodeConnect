# Vendored codex 0.147 reference bundle

Everything in this directory describes **codex-cli 0.147.0** and nothing else. It is the
reference the launch gate (`codeconnect::codex::ensure_guarded_surface`) compares an
installed codex against, and the reference the broker's own exhaustiveness proof reads.

It is deliberately **never regenerated from whatever codex happens to be installed**. A
reference that re-derives itself from the binary it is checking compares equal to
everything, which is a gate that has been switched off without anyone editing it.

Two different things live here, at two different resolutions.

## 1. The method census — `methods-{stable,experimental}.json`

The enumeration of every client→server method (JSON-RPC `ClientRequest` +
`ClientNotification`) in each 0.147 schema bundle. Source of truth for the allowlist's
**exhaustiveness check**: every method here must carry exactly one disposition, and the
four wire-native code-exec bypass methods (`command/exec`, `thread/shellCommand`,
`process/spawn`, `fs/writeFile`) plus any unknown/future method must resolve to `refuse`.

Counts (asserted by `tests/exhaustiveness.rs`):

- stable: 95 client requests + 1 client notification (`initialized`)
- experimental: 133 client requests + 1 client notification (`initialized`)

Derivation — the `method` constant of each top-level `oneOf` variant of
`ClientRequest.json` / `ClientNotification.json`. **Verified reproducible**: the recipe
below regenerates both files byte-identically, in the same order, from the 0.147 binary.

```sh
CODEX=~/.codex/packages/standalone/releases/0.147.0-aarch64-apple-darwin/bin/codex
OUT=$(mktemp -d)
"$CODEX" app-server generate-json-schema --out "$OUT/stable"
"$CODEX" app-server generate-json-schema --out "$OUT/experimental" --experimental
```

then, for each bundle, the ordered `properties.method.enum[0]` of every `oneOf` variant
of `$OUT/<bundle>/ClientRequest.json` and `ClientNotification.json`.

> An earlier version of this file said these were derived from
> `schema-0.147/{stable,experimental}/ClientRequest.json` "in the Phase-0 spike bundle".
> **Those directories were never committed** — only the extracted census was — so the
> stated derivation could not be re-run. The recipe above is the measured one and was
> checked against the committed files before this note was written.

## 2. The guarded surface — `guarded-wire-*.json`, `guarded-argv.json`

The census is method NAMES. It cannot answer "did the shape of something we forward
change?", which is the question the launch gate has to ask. These files carry that
resolution — and there are **two sets of them**: this directory holds the 0.147
**baseline**, and `../schema-0.153/` holds the **grounded ceiling** that
`guarded_surface::ADJUDICATED_WIRE` bridges to. See "Two references, one bridge" below.

- `guarded-wire-stable.json` (19 entries) / `guarded-wire-experimental.json` (21) —
  each **guarded** method's resolved `params` schema, its JSON-RPC envelope, and the
  transitive closure of definitions those reach. Guarded means `allowlist::disposition`
  does something other than refuse it on at least one `(role, kind)` cell; the set is
  derived from that function, never listed by hand, so it cannot drift from the
  allowlist.

  Entries are keyed `"<kind> <method>"` — both `ClientRequest.json` and
  `ClientNotification.json` are projected, because `initialized` is an admitted
  notification and a request of the same name must not overwrite it. A bundle that
  describes one `(kind, method)` twice is a refusal, not a last-writer-wins.

  `$ref`s are **kept as references** with every definition they reach carried alongside;
  the earlier inlining (recursion collapsed to a `{"$cycle": …}` marker) cost 37 s per
  launch and caught nothing extra, so it was removed. `description` and `title` are
  dropped **only where they are JSON Schema annotations** — at a schema node. Inside
  `properties` / `definitions` / `$defs` / `patternProperties` / `dependentSchemas` those
  spellings are wire field names, and inside `const` / `default` / `enum` / `examples`
  they are literal data; both are carried through. 0.147's `DynamicToolNamespaceTool` is
  the measurement that forced this: it genuinely requires a wire field named
  `description`, and a context-blind strip deleted it from the projection while leaving
  it in `required`.

  The two bundles are not the same size, and that is measured rather than sloppy: on
  0.147 `thread/turns/list` and `thread/items/list` exist **only** in the experimental
  bundle, while the allowlist binds them on both legs. (Both became stable in 0.153.)

- `guarded-argv.json` (30 subcommands, 32 flags) — the root CLI surface, which
  `generate-json-schema` does not describe at all and which `validate_codex_argv`
  guards. Parsed from the root `opts="…"` line of `codex completion bash`, the same
  output the 0.147 `is_subcommand` table was originally built from; this makes that
  derivation executable rather than a comment.

  **This is the fullest census either binary emits, and it is still not complete.**
  MEASURED: hidden *subcommands* DO appear here (`execpolicy`, `responses-api-proxy`,
  `stdio-to-uds` are in the completion script and absent from `codex --help`), but a
  hidden *alias* appears in nothing — `cloud-tasks` dispatches on both binaries and is in
  neither `--help` nor any of the five completion shells. That class is carried by hand in
  `guarded_surface::HIDDEN_ROOT_ALIASES`, and both directions are proved:
  `every_dispatchable_root_token_is_accounted_for` requires every referenced subcommand to
  be refused *and* every refused token to be in a reference or on that list, while
  `the_hidden_root_aliases_still_dispatch_and_are_still_hidden` re-measures against live
  binaries that each alias still dispatches and is still absent from every completion.

### Two references, one bridge

An exact-match gate against one reference admits exactly one build; CodeConnect hosts
two. So both are vendored, and `ADJUDICATED_WIRE` / `ADJUDICATED_ARGV` are the audited
bridge: each entry is an RFC 6901 pointer into a projected entry plus the fact that the
value there may move from the baseline's to the ceiling's, with the measurement that made
it safe.

At a launch, `admissible_wire` starts from the baseline and splices in exactly the deltas
the installed binary exhibits **at their measured value**, then the whole projection must
equal the installed one. Nothing is filtered after the comparison — the comparison *is*
the admission — so an adjudicated addition cannot mask a `required` move or a nested type
change beside it, and an adjudicated field carrying a different shape is simply not
spliced and refuses by name.

`the_adjudicated_table_bridges_the_two_references` proves the table applied to the
baseline reproduces the ceiling exactly, in an ordinary suite run with no binary
installed; `the_baseline_is_still_admissible` proves 0.147 keeps working.

### Regenerating (deliberate act, reviewable diff)

Produced by the gate's own projection code (`codex_broker::guarded_surface`) — never by
a second implementation, which could disagree with the gate about what its own reference
means:

```sh
for V in 0.147 0.153; do
  CODEX=<the $V binary>
  OUT=$(mktemp -d)/$V; mkdir -p "$OUT"
  "$CODEX" app-server generate-json-schema --out "$OUT/stable"
  "$CODEX" app-server generate-json-schema --out "$OUT/experimental" --experimental
  "$CODEX" completion bash > "$OUT/completion.bash"
done
CC_REGENERATE_GUARDED_SURFACE_0147=<…/0.147> \
CC_REGENERATE_GUARDED_SURFACE_0153=<…/0.153> \
  cargo test -p codex-broker --test guarded_surface_reference \
  regenerate_the_vendored_references -- --ignored --nocapture
```

`--ignored` because regeneration must never happen inside an ordinary suite run.

Reproducibility is enforced, not asserted:
`the_vendored_references_reproduce_from_live_binaries` re-derives every file from the real
binaries and demands **value equality of the projection** — not of the serialized bytes,
which would pin `serde_json::to_string_pretty` rather than the surface
(`CC_CODEX_LIVE=1 CC_CODEX_0147=<path> CC_CODEX_0153=<path>`).

## 3. The disposition matrix — `disposition-matrix.tsv`

A checked-in expected `(role × kind × method)` matrix — an independent restatement of the
allowlist rules, one row per pinned method per role. `tests/exhaustiveness.rs` asserts the
live `disposition()` equals it everywhere, so a drift in either direction fails.

## Re-grounding has two halves, and only one of them is in this directory

The gate here compares **shapes**. That is necessary and it is not sufficient, and the
0.153 re-grounding is what proved it: two of the changes that broke a real session were
invisible to every file in this directory, because the schema did not move at all.

| | Shape | Content |
|---|---|---|
| **What moves** | methods, parameter names, types, enum members, root CLI tokens | the *values* a codex build's own TUI sends |
| **Read from** | `codex app-server generate-json-schema` + `codex completion bash` | the in-broker frame tee, off a live session |
| **Compared against** | the vendored files here and in `../schema-0.153/` | fixtures + `fingerprint`'s captured pins |
| **When** | every launch, in `ensure_guarded_surface` | once per codex release, by a human taking a capture |
| **0.153 examples** | `turn/start` +4 params, `agents`/`queue`/`migrate-rollouts`, `--psp` gone | `dynamicTools` populated with the `codex_tui` bundle; `developer_instructions` 925 → 1288 bytes |

Both 0.153 content changes sat behind a **byte-identical schema**. A shape-only gate would
have admitted the build and then refused every session — the first at `thread/start`, the
second at `turn/start`. Content drift is caught by capture and pinned in the fixtures the
fingerprint reads; the delta list records the adjudication.

### Taking a capture

The instrument is `codex_broker::frame_tee`, and it is a **capture build**, not a runtime
switch. It sits behind the `frame-tee` cargo feature, off by default: without it the
`CC_CODEX_FRAME_TEE` read is not compiled, so setting the variable in front of a shipping
`codeconnect` does nothing (`a_shipping_build_cannot_enable_the_frame_tee`). That matters
because the variable's parent is not always ours — a shell, an IDE or a LaunchAgent can
set anything — and what it turns on is a verbatim, unredacted transcript written to a path
the setter chose. The launcher also neither sets it nor offers a way to
(`the_shipping_launcher_cannot_enable_the_frame_tee`); the coordinator forwards it into
the tmux pane and never originates it.

```sh
cargo build -p codeconnect --features frame-tee
CC_CODEX_FRAME_TEE=/tmp/capture.jsonl \
CODEX_HOME=<an isolated home, with auth.json copied in at 0600> \
CODECONNECT_CODEX_BIN=<the codex to ground against> \
  target/debug/codeconnect codex
```

It writes one line per frame, both directions:
`{"run":…,"seq":…,"conn":…,"dir":…,"raw":…,"frame":…}`. `raw` is the bytes exactly as they
crossed; `frame` is the parsed view beside it, so a capture stays comparable to
`fixtures/codex/thread-switch.jsonl`. Recording the raw bytes is the point — a parse
normalises away whitespace, key order, number spelling and duplicate keys, which are
precisely the details a capture is taken to settle. `run`/`seq` keep two appended runs
separable, and a run brackets itself with `open`/`close` markers: a run id with no `close`
was truncated, so a missing frame can be told from a stopped recorder. A write that fails
marks the run and says so on stderr rather than vanishing.

Verbatim is also why it is not production-reachable: `redact.rs` guarantees the audit log
carries fixed vocabulary and counts only, so `broker.log` structurally cannot say *which*
parameter a new codex sent. A capture therefore contains whatever the session contained;
sanitising one for `fixtures/` is a deliberate step.

One limit worth knowing before trusting a capture: a broker **refusal** is synthesised
locally and never crosses the upstream leg the tee taps, so refused requests appear but
their verdicts live in `broker.log`.

A second one used to matter and no longer does. A refused tool-invoked method presented to
the model as a HANG rather than an error, which could stop a turn before it reached the
tool you were trying to observe — that is why `set_thread_archived` went untraced through
the whole 0.153 re-grounding. Measured to the frame: the broker tombstoned the
`item/tool/call` capability, so the TUI's well-formed failure result was dropped
("method-less response has no live capability"), the app-server never learned the tool
call had finished, and `turn/interrupt` was itself refused at the time, so the turn could
not be escaped either. Both halves are closed: `turn/interrupt` is now bound to the running
turn rather than refused, and the dispatch is answerable
(`response_capability::DYNAMIC_TOOL_CALL`), so a refused tool fails as a tool and the turn
completes — a capture can drive several tools in one turn again.

## What a drift means

A new codex whose guarded surface matches this reference is admitted whatever its version
string says. One whose guarded surface differs is refused **naming the exact method,
field, or subcommand that moved** — that name is the work item for the re-grounding, and
re-vendoring to a new version is only correct once every change it names has been
measured and pinned.
