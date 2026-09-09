# Vendored codex 0.153 reference bundle

Everything in this directory describes **codex-cli 0.153** and nothing else. It is the
**grounded ceiling** half of the launch gate's two-reference pair: `../schema-0.147/` holds
the baseline every live gate in this repo was proven on, this directory holds the surface of
the build that is actually installed, and `guarded_surface::ADJUDICATED_WIRE` /
`ADJUDICATED_ARGV` are the audited bridge between them. Read
`../schema-0.147/PROVENANCE.md` first — it states the shared rules (what a projection is,
why annotations are stripped only at schema nodes, how the bridge is applied at a launch),
and this file states only what is specific to 0.153.

Like the baseline, it is deliberately **never regenerated from whatever codex happens to be
installed**. A reference that re-derives itself from the binary it is checking compares equal
to everything, which is a gate that has been switched off without anyone editing it. The
ceiling is regenerated only as a deliberate, reviewable act — and only after every difference
it introduces has been measured and written into the adjudication table.

## What landed, and when

Every file here arrived in a single commit and nothing has touched them since:

```
ee3affecf3213e5796e80f75dec8d4b23f98dd37
Fri Sep 4 06:14:22 2026 +0300
Guarded-surface gate: pin to what we guard, not to a version; admit codex 0.153
```

That commit is the re-grounding: it replaced an exact codex-version pin (which refused every
weekly release) with a check of the two surfaces the broker actually guards, and it vendored
this directory as the second of the two references that check compares against.

## Which 0.153, exactly — and the limit of that answer

Three 0.153 builds are present on the grounding machine:
`~/.codex/packages/standalone/releases/0.153.{0,2,4}-aarch64-apple-darwin`. **MEASURED: all
three emit byte-identical inputs to this reference** — the same
`app-server generate-json-schema` output for both bundles and the same
`completion bash` script (sha256 of `ClientRequest.json`: `25bc001b…` stable,
`05c82ead…` experimental; of the completion script: `213c5bd5…`; identical across
0.153.0, 0.153.2 and 0.153.4).

So the honest statement is: **these files pin "a 0.153", not a particular patch of it**, and
no measurement taken from them can tell those three builds apart. The build the re-grounding
was actually driven against — the one the frame capture and every live proof in `ee3affe`
came off, and the one `docs/codex.md` records as measured at that commit — is
**`codex-cli 0.153.4`**. That is a fact about the session that produced the adjudications,
not a constraint this directory can enforce, which is the right shape: the gate pins to what
it guards, never to a version string (see `guarded_surface`'s module header).

The patch level *does* matter to the **content** pins, which do not live here — the fixtures
name `0.153.4` explicitly (`fixtures/codex/compose-refusals-0.153.4.txt`) because a content
capture is taken off one binary on one day. See "Two halves" below.

## What is here — and what is deliberately not

- `guarded-wire-stable.json` — 31 entries: 20 client requests, 1 client notification
  (`initialized`), 10 server→client requests.
- `guarded-wire-experimental.json` — 32 entries: 20 client requests, 1 client notification,
  11 server→client requests.
- `guarded-argv.json` — 33 root subcommands, 31 root flags.

Each wire entry is keyed `"<kind> <method>"` and carries that method's resolved `params`
schema, its JSON-RPC `envelope`, the transitive closure of `definitions` those reach, and —
for a client request — its `result` type. Only **guarded** methods appear: the set is derived
from `allowlist::disposition` by `guarded_surface::is_guarded_as`, never listed by hand, so it
cannot drift from the allowlist.

> The counts above are stated by category on purpose. `../schema-0.147/PROVENANCE.md`
> says "19 entries / 21", which was true of the *client* entries when it was written and is
> now stale for the file as a whole: that bundle carries 29 and 32 entries respectively once
> the server-request projections it later grew are counted. Nothing depends on either
> number — the reproduction and bridge tests compare whole projections, not counts — but a
> provenance note that quietly disagrees with the file it describes is worth naming rather
> than inheriting.

Two entries exist here and not in the baseline: `request thread/turns/list` and
`request thread/items/list`, which were experimental-only on 0.147 and became **stable** in
0.153 while the allowlist bound them on both legs all along. Both are adjudicated
(`ADJUDICATED_WIRE`, `DeltaKind::Added` on the stable bundle).

**Not here, and not an oversight:**

- **No `methods-{stable,experimental}.json`.** The method census — and the exhaustiveness
  proof `tests/exhaustiveness.rs` runs against it — is anchored to 0.147 and stays there. It
  asks "does every method this binary can name resolve to exactly one disposition", and the
  allowlist is refuse-by-default, so a *larger* future census cannot open a hole: everything
  0.153 added that 0.147 did not have falls through to `Refuse(NotAllowlisted)` by
  construction. Pinning the census to the baseline keeps that proof stable across releases.
  For the record, measured the same way the 0.147 census was: 0.153 emits **99** stable
  client requests and **155** experimental (against 0.147's 95 and 133), one
  client notification (`initialized`) in both bundles.
- **No `disposition-matrix.tsv`.** There is exactly one, in `../schema-0.147/`, for the same
  reason: it is a restatement of the allowlist's rules, and the allowlist is version-free.

## Derivation

Identical to the baseline's, from a 0.153 binary instead of a 0.147 one. Both halves of the
pair are produced by **one** run of the regeneration test — see the `for V in 0.147 0.153`
recipe in `../schema-0.147/PROVENANCE.md`, whose 0.153 leg is
`CC_REGENERATE_GUARDED_SURFACE_0153=<the 0.153 binary>`.

The projection is done by the gate's own code (`codex_broker::guarded_surface`), never by a
second implementation, which could disagree with the gate about what its own reference means.
Regeneration is `--ignored` so it can never happen inside an ordinary suite run.

Reproducibility is enforced rather than asserted:
`the_vendored_references_reproduce_from_live_binaries` re-derives every file in *both*
directories from the real binaries and demands **value equality of the projection** — not of
the serialized bytes, which would pin `serde_json::to_string_pretty` rather than the surface
(`CC_CODEX_LIVE=1 CC_CODEX_0147=<path> CC_CODEX_0153=<path>`). Re-run against the real
0.147.0 and 0.153.4 binaries while this note was written: green, so every file here still
falls out of the 0.153 binary that produced it.

## Why a ceiling at all, and what it does NOT license

Re-vendoring to 0.153 alone would have refused 0.147: its *absent* `projectId` and
`excludeTurns` read as removals, and tolerating removals as a class is the one thing this gate
must never do. So both are vendored and `ADJUDICATED_WIRE` bridges them.

The bridge is not a version switch. Nothing is keyed by version, nothing branches on version,
and there is no registry: each entry is an RFC 6901 pointer into a projected entry plus the
fact that the value there may move from the baseline's to **this file's**, and a build either
exhibits that difference exactly or does not exhibit it at all. At a launch,
`admissible_wire` splices the measured values in and then the whole projection must equal the
installed one — so an adjudicated addition cannot mask a `required` move or a nested type
change beside it.

This directory is therefore the place each delta's *measured post-change fragment* lives.
Storing the fragments as a projection rather than as string literals beside the table means
they are regenerated by the gate's own projector from a real binary, reviewed as a diff, and
checkable offline: `the_adjudicated_table_bridges_the_two_references` proves the table applied
to the baseline reproduces this file exactly, in an ordinary suite run with no binary
installed, and `the_baseline_is_still_admissible` proves 0.147 keeps working.

## Two halves, and only one of them is in this directory

The files here compare **shapes**. The 0.153 re-grounding is what proved that is necessary and
not sufficient: two of the changes that broke a real session were invisible to every file in
this directory, because the schema did not move at all — the TUI's `dynamicTools` bundle went
from empty to a six-tool `codex_tui` payload, and the pinned developer instructions grew from
925 to 1288 bytes, both behind a **byte-identical schema**. A shape-only gate would have
admitted the build and then refused every session.

Content drift is caught by capture (the `frame-tee` feature build) and pinned in the fixtures
the fingerprint reads. That is also why the fixtures name a patch version and this directory
cannot: a content pin is a fact about one binary on one day, and a shape pin is a fact about
every build that describes itself this way.

## What a drift means

A codex whose guarded surface equals the baseline, or the baseline plus any subset of the
adjudicated deltas at their exact measured values, is admitted whatever its version string
says. One whose surface differs anywhere else is refused **naming the exact method, field, or
subcommand that moved** — that name is the work item for the next re-grounding, and moving
this ceiling to a newer codex is only correct once every change it names has been measured,
adjudicated, and pinned.

## Licence

The files here are machine-generated self-descriptions emitted by an upstream binary, not
upstream source. See `../NOTICE` for the upstream project, its licence, and an explicit
statement of what could and could not be established from the artifacts this bundle was
extracted from.
