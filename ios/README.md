# CodeConnect — iPhone app

SwiftUI, iOS 17+. Talks to the Mac daemon (`ccd`) over the tailnet WebSocket
protocol defined in `mac/protocol/src/ws.rs`.

## Session identity

A run has two names and they do different jobs (see "Session identity" in
`mac/README.md`). `session_id` is the tmux name — `cc-1` — and it is **reused**:
`codeconnect claude` takes the lowest free number, so the next run is called `cc-1` again.
`session_uid` is a ULID minted once at spawn and never reused.

This app keys **everything** by `SessionSummary.sessionKey` / `Event.sessionKey`,
which is the uid where the daemon mints one and the tmux name where it does not:
the event stores, the subscriptions, the on-disk cache, the diff cache, the
review marks, the fleet rows, the navigation routes, and the run an answer is
scoped to. The name is display only — plus the one place it is the *right*
answer, `tmux attach -t =cc-1`, which is taken from the fleet rather than from
the route because tmux has never heard of a uid.

Two consequences worth stating:

* **Two runs of one name are two sessions.** They get two rows, two timelines
  and two rails. The uid keys them; it never names them. A row is called by the
  **project** the daemon resolved (`RunLabel`), and where two runs share one
  project each row adds the time it started — but only when that is both true
  and useful: never for an adopted run, whose timestamp is a first sighting
  rather than a start, and never when two runs started in the same minute,
  because a discriminator that ties is worse than none.
* **An answer names its run** (`answer.session_id`). The daemon refuses an
  unscoped answer when one `request_id` is open in two runs rather than guessing;
  sending the uid means that path is never reached. In-flight answer state is
  keyed by `(run, request_id)` for the same reason.

`Capabilities.session_uid` — or `protocol_minor >= 2` — is the gate. A daemon
that advertises neither is treated exactly as before, name-keyed, and
`DeckUITests` still exercises that path against a minor-1 fixture.

The local cache records which of the two regimes wrote it. On the first connect
to a uid-minting daemon the name-keyed files are **dropped**, not remapped: a
file called `cc-1` holds whatever runs have held that name, spliced, and nothing
in it says which parts belong to the run now offering that uid. The sessions that
lost history say so (`GapNotice.cacheDiscarded`); a cold re-sync costs one
bounded backfill.

One third-party dependency, pinned, only for the live terminal:

| Package | Pin | Why |
|---|---|---|
| [SwiftTerm](https://github.com/migueldeicaza/SwiftTerm) | exact `1.15.0` | terminal emulator (`TerminalView`) |

The terminal needs no network library of its own: it rides the paired
`URLSessionWebSocketTask` the rest of the app already holds. Everything else
resolved is transitive and Apple-owned; `Package.resolved` is committed and is
the real pin.

## Open and run

Sources live in Xcode **synchronized file groups**, so adding a Swift file
requires no project edit.

```sh
open ios/CodeConnect.xcodeproj          # or:
cd ios
xcodebuild -project CodeConnect.xcodeproj -scheme CodeConnect \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Pair by scanning the QR that `codeconnect pair` prints on the Mac. Manual entry
takes either the eight-character code from the same output or the static
`codeconnect token`.

## Layout

| Path | What it is |
|---|---|
| `CodeConnect/Protocol/` | `Codable` mirror of the Rust wire types, a `serde_json`-compatible canonical serialiser, and typed reads over raw hook/transcript payloads. |
| `CodeConnect/Net/` | `DaemonConnection` (one supervised `URLSessionWebSocketTask`) and `TerminalCarrier`, which streams tmux over that same socket. |
| `CodeConnect/Store/` | Keychain pairing, this device's identity, preferences, the on-disk event cache. |
| `CodeConnect/Model/` | Event ingest with gap detection, the timeline builder, fleet ordering, link health, payload-hash verification, risk reconciliation, the Deck queue, the unified-diff parser. |
| `CodeConnect/Views/` | Fleet, Deck, session detail (Timeline / Terminal), decision card, diff, pairing and settings. |
| `CodeConnectTests/` | Unit tests. Wire types are tested against JSON written by hand from `mac/protocol/src`. |
| `CodeConnectUITests/` | End-to-end. `ApprovalFlowUITests` and `LiveDaemonUITests` need a **real** daemon; `DeckUITests` replays contract-shaped frames. |
| `CodeConnectRenderHarness/` | The render catalog. Its own target, in its own scheme, **excluded from the product scheme's TestAction** — nothing here changes the product's test counts. See *The render harness*. |
| `scripts/render-screens.sh` | Drives it. |

## Tests

```sh
cd ios
# Unit tests — no daemon needed. 110 of them.
xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' -only-testing:CodeConnectTests

# The Deck, driven by replayed daemon frames — no daemon needed. 22 of them.
xcodebuild test … -only-testing:CodeConnectUITests/DeckUITests
```

Both suites are green from any starting state: **`DeckUITests` pins its own
Dynamic Type size on every launch** (`-UIPreferredContentSizeCategoryName`), so
the old footgun — leave the simulator at AX5 after a render pass and four tests
fail because `Come back to this` is below the fold — no longer applies. Do the
same in any new UI test rather than reading the simulator's ambient size.

Against a live `ccd`, the environment has to reach the **test runner**, not
`xcodebuild`:

```sh
TEST_RUNNER_CC_HOST=100.x.y.z TEST_RUNNER_CC_TOKEN="$(codeconnect token)" \
xcodebuild test … -only-testing:CodeConnectUITests/LiveDaemonUITests

# Pairing needs a fresh code — they are single-use and expire in five minutes.
CODE=$(codeconnect pair | grep -o '"code":"[^"]*"' | cut -d'"' -f4)
TEST_RUNNER_CC_PAIR_CODE=$CODE TEST_RUNNER_CC_PAIR_HOST=100.x.y.z \
xcodebuild test … -only-testing:CodeConnectUITests/PairingLiveUITests
```

`TEST_RUNNER_`-prefixed variables in the *shell* environment are the documented
path and they work for `xcodebuild test`. They do **not** survive a
`build-for-testing` / `test-without-building` split, which is what a screenshot
or CI pass usually wants. There, the values live in the generated `.xctestrun`
and the working method is to edit it:

```sh
xcodebuild build-for-testing … -derivedDataPath dd
python3 - "$(ls dd/Build/Products/*.xctestrun)" <<'PY'
import plistlib, sys
path = sys.argv[1]
plan = plistlib.load(open(path, "rb"))
for cfg in plan.get("TestConfigurations", []):
    for target in cfg.get("TestTargets", []):
        target.setdefault("EnvironmentVariables", {})["CC_HOST"] = "100.x.y.z"
plistlib.dump(plan, open(path, "wb"))
PY
xcodebuild test-without-building -xctestrun dd/Build/Products/*.xctestrun …
```

Passing `CC_HOST=… xcodebuild test …` sets the variable for `xcodebuild` and
nothing else; the runner never sees it.

Tests that need a pending approval skip rather than fail when the fleet is clear.

### Test seams

All `#if DEBUG`, all driven from the launch command line, none present in a
release build:

| Argument | What it does |
|---|---|
| `-CC_HOST -CC_TOKEN` | pairs without typing |
| `-CC_PAIR_HOST -CC_PAIR_CODE` | runs the real pairing-code exchange without a camera |
| `-CC_FIXTURE deck` | replays daemon frames for three blocked agents at three risk classes |
| `-CC_FIXTURE stacked` | the same fleet plus a **second decision on one agent** and a fifth agent **running a tool**. With one card per agent, "count the agents" and "count the cards" return the same number, so the fleet's headline and its accessory bar agreed by coincidence while counting different things; and with no Running band, a shell command set in proportional type could not be reached from a fixture at all |
| `-CC_FIXTURE_LINK stale` | withholds the fixture's keep-alive `pong`, so `LinkHealth` crosses its 45-second `staleAfter` on its own and every action disables itself **with its reason**. Pairs with either fixture; without it, `stale` is unreachable under a fixture and therefore never rendered or tested |
| `-CC_FIXTURE_CACHED <seconds>` | restages the fixture's fleet as one read off **disk** that many seconds ago, so the cached banner draws its age. Without it the cached fleet — the state where every wait clock ticks off data that arrived before launch, and a reader cannot tell an amber `5m40s` from a live one — could not be rendered or tested at all. Compose with `-CC_FIXTURE_LINK stale` for the **compound** banner the ladder was rebuilt for: the link's classification carrying the cache's age, one banner, both facts |
| `-CC_RENDER_PROBE YES` | adds a 1pt invisible element carrying the content-size category this process actually resolved to (`cc-render-probe`). The render harness reads it before it photographs anything — see *The render harness* below. Off by default so it cannot appear in a tree a product test is counting |
| `-CC_DEEPLINK codeconnect://…` | delivers a deep link at launch, the same way a tapped notification does. `…/deck/<request-id>` opens a **named** card — a URL can name one; a notification cannot, and does not try — which is the only way to assert the read gate on one card without three drags and a postpone standing between the test and the assertion |
| `-CC_BIOMETRICS allow\|deny\|cancel\|unavailable` | injects the Face ID *outcome*; the system sheet cannot be driven by XCUITest |
| `-cc.debug.diff sample` | renders a diff shaped to exercise every part of the diff grid at once — all three syntax roles, word-level tints, a wrapped line, a hunk header — without a daemon or a dirty worktree |
| `-cc.debug.diff truncated` | a capture the daemon cut at its 512KB cap, longer than the grid draws in one pass — the truncation banner, the per-hunk `N more lines` marker, `Draw more`, and the terminal `Truncated at 512KB`. Reaching this state for real took a generated 13,000-line file and a live Mac, which is why it shipped as a blank black rectangle |
| `-cc.debug.diffRows <n>` | lowers the grid's per-hunk row budget from 400, so the held-back marker is on screen without dragging through 400 rows |
| `-cc.debug.terminalState attaching\|ended\|endedLive` | the terminal states a healthy Mac never shows, which otherwise need a session killed mid-flight or the link dropped |
| `-cc.debug.sendText sent\|recovered\|recovered-empty\|lost` | resolves typed sends locally, the way `-CC_FIXTURE` resolves decisions, so the snapshot sheet's four outcomes can be photographed. `recovered` carries a real 80-column `/status` capture measured off a live Mac. Against a daemon these states need a Mac view opened and its Escape failing on cue, which is not something a render pass can arrange |
| `-cc.debug.sendText kept-model\|kept-effort\|set-effort-session\|duplicate` | answers `sent` **and** injects Claude Code's own transcript receipt, the way the real daemon delivers it — the send returns, the line follows. These reach the Model and Effort sheets' `no change` states and a scope-bearing confirmation, none of which any render could reach before: a `kept` receipt needs Claude Code's `Switch model?` / `Change effort level?` confirmation opened and then declined. Every line is verbatim from the 2.1.223 rig, escape codes included, so the render exercises the same parsing the live path does |

`-CC_FIXTURE` also resolves answers locally, because the Deck's advance
behaviour cannot be exercised otherwise. The real answer path — daemon, ledger,
duplicate detection — is covered by `ApprovalFlowUITests` against a live `ccd`.

### The render harness

```sh
ios/scripts/render-screens.sh --device "iPhone 17 Pro"
```

Every catalogued screen, at **`L` and AX5**, into
`ios/.artifacts/ui-renders/<timestamp>/{L,ax5}/<scenario>.png` with a
`report.txt` beside them. Gitignored; CI uploads them. `--size L` or `--size ax5`
renders one; `--keep-simulator` leaves the throwaway device behind for poking at.

It is an **inspection instrument, not an assertion test**. It makes no pixel
assertions. It reaches a state, proves the process is at the size the pass
claims, and photographs it — and it **exits nonzero if any scenario cannot be
reached or captured**, because a state nobody can reach is a state nobody has
looked at. Seven Deck states went a long time being exactly that, and the two
worst diff-sheet bugs — a blank black rectangle, and the app's own words
attributed to the daemon — were states no render could reach.

Three things about how it is built, each of which is a bug that has already been
paid for:

* **It runs its own simulator and deletes it.** A pass that leaves your working
  device parked at AX5 costs four `DeckUITests` failures the next morning.
* **The type size comes from `xcrun simctl ui <device> content_size`, once per
  invocation, and the harness never calls `XCUIApplication.launch()`** — it
  `terminate()`s and `activate()`s. `xcodebuild test` plus `launch()` resets the
  simulator's content-size category, and `TEST_RUNNER_*` never reaches the
  runner, so an "AX5 pass" that silently ran at `L` is indistinguishable from one
  that worked. Some earlier AX5 measurements were exactly that.
* **It carries a calibration mark.** `-CC_RENDER_PROBE` puts the category the app
  *actually resolved to* in the accessibility tree, and the harness checks it
  before every capture. A render that cannot state its own conditions is not
  evidence.

**The catalog is part of the screen inventory.** Adding or removing a
user-visible screen, or a safety- or honesty-relevant state of one, requires
updating `CodeConnectRenderHarness/RenderCatalog.swift` in the same change. A
scenario is a name, a purpose, the launch seams that reach it, and the assertions
that prove it arrived — about fifteen lines, and the reason the next look at a
screen can start from pictures instead of guesses.

Nothing in that target runs under `-scheme CodeConnect`: it lives in
`CodeConnect Renders`, so the product suites' counts are untouched.

### Driving the UI from a test

Two idioms, both found the hard way:

* **`app.swipeUp()` scrolls nothing in this app.** Measured on the Deck at AX5:
  five consecutive calls moved the card's scroll view exactly zero points. The
  flick's end point lands in the navigation bar and the gesture is never
  delivered to the scroll view. Use
  `press(forDuration: 0.05, thenDragTo:)` between two coordinates that are
  **both** inside the document — `(0.5, 0.45)` to `(0.5, 0.15)` works every
  time.
* **`firstMatch` on the action bar.** SwiftUI's `safeAreaInset` renders it into
  the hierarchy more than once, so an exact query is ambiguous even when only
  one is on screen.

## The honesty rules, and where they live in the code

"Never lie about state" is not a coat of paint; these are the specific places it
is enforced.

* **Freshness is measured, not assumed.** `LinkHealth.evaluate` derives from
  `lastContactAt` — when a frame last *arrived* — not from whether a socket is
  open. A 10-second app-level ping keeps that number meaningful on an idle link.
* **Stale links disable actions with a reason.** `LinkHealth.disabledReason`
  feeds `blockedReason` on the decision card and `sendBlockedReason` on the
  compose bar. There are no dead buttons without an explanation.
* **Gaps are surfaced, never smoothed over.** `SessionState.ingest` raises a
  `GapNotice` on a daemon `resync`, on a `seq` that does not follow the previous
  one, and when the daemon's log is *shorter* than the local cache.
* **…and withdrawn when they stop being true.** A sequence-gap banner records
  which `seq` numbers are missing and is restated — or removed — as a replay
  fills them. A warning left up after its reason is gone teaches people to
  ignore warnings.
* **Cold opens are stamped.** Anything read from disk carries its age until live
  data replaces it, on the fleet and per session.
* **No optimistic approvals.** A tap shows a spinner; the card only changes when
  `answer_result` comes back.
* **The card proves its own text.** `ApprovalCard.verification` re-hashes
  `display_text` against `payload_hash`.
* **An inferred outcome is never stated as fact.** `ResolvedBy::Local` carries
  `inferred: true` — the daemon saw the prompt leave, it did not see the answer —
  so the UI says "answered at the keyboard", never "Allowed"
  (`AnswerOutcome.decisionLabel`).
* **A disconnected terminal says so.** The last bytes stay on screen under an
  explicit not-live banner, because a frozen terminal is indistinguishable from
  a live one and clearing it would lie about what you just read.
* **A terminal rebuilt from a capped buffer says so.** Coming back to a session
  replays the last 224 KiB the carrier held, which for a chatty agent begins
  mid-session; the pane that was handed those remains draws a `CCGapMarker`
  above itself until the Mac repaints it whole. Out of band on purpose — a
  notice written *into* the emulator is erased by the next `ESC[2J` it replays.
* **A diff carries its capture time and its truncation.** An empty diff renders
  the daemon's `note`, which is the only thing that tells a clean tree from a
  directory that was never a git repository.
* **Capability badges are reported, not guessed.** The trust screen lists every
  advertised capability, including ones this build has no name for.

## Correctness invariants worth not breaking

* **Ingest is coalesced, and the buffer is settled before anything reads it.**
  A replay arrives one frame per turn of the main actor, so events at or below
  the tail are held and merged in one pass per 16ms window instead of one
  whole-array insert and one full timeline build each. `settlePendingEvents()`
  drains that buffer before the cache is written, so a snapshot taken mid-window
  can never persist a log with a hole in it.
* **One live request per correlation key** (`DaemonConnection.request`).
  `send_text_result`, `capture_result` and `diff` carry only a `session_id`, so
  replies can only be matched by position; two concurrent requests for one key
  could have their *results swapped*.
* **Abandoned requests leave a tombstone**, so a late reply is consumed and
  discarded rather than handed to the next request for the same key.
* **Answer-in-flight state lives on `AppModel`, keyed by `request_id`** — not in
  the decision card, which gets fresh `@State` when re-presented.
* **Subscriptions are generation-stamped** (`AppModel.subscribeIfNeeded`).
* **Sockets are closed by identity** (`DaemonConnection.close(socket:session:)`).
* **`PaneOptions.parse` takes the last list, never the longest**, within 25 lines
  of the bottom of the snapshot.
* **A pairing code is never persisted.** It is single-use with a five-minute
  life; storing one would produce a pairing that looks valid and can never
  connect. The device token from `hello_ack` is what reaches the Keychain — and
  a pairing hello omits `token` entirely, because the daemon *prefers* a token
  when both are present, so even an empty string would be checked as the
  credential and the code would never be read.
* **Risk is reconciled by taking the stricter reading** (`RiskAssessment`). The
  daemon sees the repo and can tighten a gate; it can never loosen one below
  what this build's own reading of the command justifies. `git push --force`
  cannot become a single tap because a rule file said `low`.
* **Nothing in the Deck is a swipe.** Advancing is a button and answering is a
  button. Even the "Done, unreviewed" fleet row opens diff-first on a tap rather
  than growing a swipe action, so the gesture is never taught anywhere.
* **HIGH-risk approvals need `biometryCurrentSet`,** not
  `deviceOwnerAuthentication` — the flag that invalidates when the enrolled set
  changes. A phone with no passcode cannot approve a HIGH card at all; it says
  so and sends you to the Mac.
* **The app names a session, never a pane.** `TerminalCarrier` sends the
  `session_uid` and nothing else; the daemon resolves it to one live pane and
  addresses every write there, so the phone has no way to reach a pane it was
  not shown. Keystrokes are delivered as data, never as tmux commands.

## Transport

The app starts on the scheme the pairing remembers and **alternates after two
consecutive failures**. A daemon that turns TLS on stops speaking `ws://` on that
port altogether, so nothing arrives to announce the change — the handshake simply
never completes. Alternating self-corrects in both directions for the price of at
most one extra attempt, and the scheme that works is remembered.

A pairing made against an IP literal never tries `wss://`: a `tailscale cert`
certificate carries a DNS SAN and can never validate against `100.x.y.z`.
Settings says so and points at re-pairing from a fresh QR, which carries the
MagicDNS name.

`capabilities.tls` ("the daemon holds a certificate") and `capabilities.tls_active`
("this connection is encrypted") are different facts and are shown separately.

## Not built yet

Live Activities are not implemented.
Push notifications ring; an approval opens the Deck and every other kind opens
the fleet. The deep-link routes
(`codeconnect://deck`, `codeconnect://session/<id>`, `…/diff`) can name a
particular target; a notification cannot — its payload carries no identifier at
all, only which *kind* of doorbell rang — so a tapped approval opens the Deck and every other kind opens the fleet — the
kind is the only thing a notification says about itself.

---

# Design system

Dark-only — no light variant and no `@Environment(\.colorScheme)` branching
anywhere. Tokens live in `CodeConnect/Views/DesignKit.swift`; components live in
`CodeConnect/Views/DesignSystem/`.

Four principles the rest of this section is downstream of: colour appears only to
carry state, never decoration; separation comes from hairlines and spacing rather
than fills; identifiers, commands, diffs and durations are monospace and prose
never is; and motion is confirmation, not decoration.

**Raw hex appears exactly once**, in the private `Hex` enum in `DesignKit.swift`.
Every colour elsewhere is a semantic token. If you type a hex value or `.blue`
outside that file, it is a bug.

## Force-dark

Three layers, all required:

| Layer | Mechanism |
|---|---|
| UIKit (keyboard, camera preview, alerts, selection menus) | `UIUserInterfaceStyle = Dark` in `Support/Info.plist` |
| SwiftUI | `.ccAppearance()` on `RootView` in `CodeConnectApp.swift` |
| Tokens | absolute `Color(.sRGB, …)` values — never a `UIColor` dynamic provider, never `Color(.systemBackground)` |

## Tokens

```
CC.color   bg surface surfaceRaised surfaceOverlay
           border borderStrong borderFocus
           accent accentPressed onAccent
           success info warning danger
           muted(_ tone:)                       // 12% semantic wash
           successMuted dangerMuted             // diff row tints, 6%
           successWord dangerWord               // diff word tints, 22%
           syntaxKeyword syntaxString syntaxComment   // three roles, only three

CC.text    primary secondary tertiary disabled onAccent

CC.ansi    table[16] background foreground     // the 16-colour terminal palette

CC.space   xxs 4 · xs 8 · sm 12 · md 16 · lg 20 · xl 24 · xxl 32 · xxxl 48
CC.radius  sm 6 · md 8 · lg 12 · xl 16 · pill
CC.stroke  hairline 1 · emphasis 1.5 · focus 2 · ring 3
CC.size    hitTarget 44 · controlSm 36 · controlMd 44 · controlLg 52
           rowMin 64 · rowRoomy 76 · factRow 48 · badge 20 · icon 16
           chip 32 · chipMaxScale 1.75
           dot 8 · dotSm 6 · dotMaxScale 1.75
           glyph 28 · glyphSm 24 · emptyGlyph 32 · emptyGlyphCircle 64 · …
CC.opacity muted .12 · press .08 · disabled .45 · pulse .45

CC.duration micro .12 · small .18 · medium .22 · exit .24 · draw .30
            hold 1.2 · toast 1.4 · reduced .12
CC.motion   micro small medium exit physical draw reduced standard linear(_:)

CC.type    display title headline body callout footnote
           micro badgeLabel fieldLabel mono monoSmall
```

**Type is applied with `.ccType(_:)`**, never `.font(...)`. The modifier scales
size, tracking *and* leading together against Dynamic Type, so the scale keeps
its proportions from `xSmall` to `AX5`. `display` and `title` stop growing at
`.accessibility1`; everything carrying information is uncapped. Mono tokens use
`monospacedDigit()` so ticking ages do not reflow the row.

`.ccType` deliberately does **not** set colour — components apply the
documented colour explicitly, so a call site's own `.foregroundStyle` is never
in a precedence fight with the type token.

**A face is a property of the token, not a modifier on the view.**
`CCTextStyle.monospaced()` returns the style at the monospaced design with
tabular figures and no tracking — for the one shape the scale has no token for, a
number that is the *headline* (`CCStatStrip`'s value at `title`, where `mono` 14
and `monoSmall` 12 both render smaller than the 11pt label above them).
`.ccType(CC.type.title).monospaced()` is what shipped and it does nothing:
`.ccType` sets `.font(…)` inside its own body, closer to the `Text` than the
outer transform, so the environment change is discarded before it is read.
`THIS PASS`'s `26s` measured two advances **inside one number** — 17.67 then
19.33 — while `waiting 1m03s` two screens earlier was set in SF Mono. It also
left the leading wrong, because `.ccType` derives that from `UIFont.systemFont`
unless the style says monospaced.

**Three 11pt roles, not one token doing seven jobs.** `micro` shipped carrying
section headers, row statuses, badge labels, card statuses, chips, field labels
and capability pills, in four colours — so `BLOCKED`, `MEDIUM` and
`HELD FOR YOU` measured as the same 11pt uppercase `#828282` on one screen and
the reader's parser had nothing to grip. Each role now has one token and one
colour rule:

| Token | Job | Colour |
|---|---|---|
| `micro` 11/14 +0.06em | **section labels only** — the word that names a group of rows | `textTertiary`, always |
| `badgeLabel` 11/14 **semibold** +0.04em | badge and chip labels, count chips, a `CCBanner`'s classification, a gap marker's pill | its variant's tint, set by the component |
| `fieldLabel` 11/14 +0.02em | the label that *names a value* — a form field, a stat column, a numbered step | `textSecondary`, dimming to `textTertiary` once the thing it labels is done |

**`CCTone`** (`neutral · success · info · warning · danger`) is the semantic
axis. Domain enums map onto it in `CCDomain.swift` and nowhere else:
`RiskClass.ccTone`, `FleetStatus.ccTone` / `.ccDotColor` / `.ccDotPulses`,
`ToolStatus.ccTone`, `LinkHealth.Level.ccTone`, `CapabilityBadge.ccTone`.

**Motion honours Reduce Motion.** Use `.ccAnimation(_:value:)` rather than
`.animation(_:value:)` — it collapses every motion class to a 120ms cross-fade,
and `.ccScaleEffect` / `.ccPressScale` become no-ops. Haptics are unaffected: a
haptic is not motion, and removing it removes information.

**Haptics** are the closed set in `CCHaptic`: `rowPress` (silent),
`openHeavy`, `light`, `decision`, `holdTick`, `commit`, `success`, `warning`,
`failure`. There are no others.

## Components

| Component | API sketch |
|---|---|
| `CCButton` | `CCButton("Allow", icon:, variant: .primary/.secondary/.ghost/.destructive, size: .sm/.md/.lg, fullWidth:, isLoading:, disabledReason:) { }` |
| `CCHoldButton` | `CCHoldButton("Hold to allow", tone: .danger, duration: 1.2, isLoading:, disabledReason:) { }` — ring around the button's own rect, soft ticks at 25/50/75%, rigid on commit, plus a VoiceOver activation |
| `CCCard` | `CCCard { }` · `CCCard(header:content:)` · `CCCard(header:content:footer:)` — `padding:`, `contentColumn:`, `radius:`, `border:`. Its default puts content on the 52pt column; `contentColumn: false` gives a **centred** card four even edges |
| `CCRow` | `CCRow(title, subtitle:, meta:, titleTruncation:, subtitleLineLimit:, showsChevron:, separator:, density:, isDimmed:, disabledReason:, accessibilityLabelText:, action:) { leading } trailing: { } meta: { }` — 64pt min, full-row target. **`meta` spans the whole row**, under the trailing accessory rather than beside it |
| `CCFactRow` | `CCFactRow(label, value:, labelStyle: .prose/.identifier/.key, tone:, isUnmeasured:, age:, separator:)` · `CCFactRow(label) { value }` · `CCFactRow(label, detail: { })` — 48pt. Lands its own text on the 52pt column; no call site adds anything |
| `CCBadge` | `CCBadge(text, icon:, tone:, count:, isSelected:, action:)` · `CCBadge(status:count:)` · `CCBadge(risk:)` · `CCBadge(capability:)` — **one construction**, and no `style:` to pick another: tint@12 fill, tint@40 border, full-strength label, radius 6. A badge is **20pt**; the same construction **with an `action`** is a control and is `CC.size.chip` 32 with a wider inset, because a chip is a thing you press. `danger` takes a 1.5pt edge and `risk: .high` a leading glyph; nothing takes a saturated fill |
| `CCCountChip` | `CCCountChip(3)` — the same construction with a number in it. Used by `CCSectionHeader`; never hand-rolled |
| `CCStatusDot` | `CCStatusDot(color:size:isHollow:pulses:)` · `CCStatusDot(tone:…)` · `CCStatusDot(status:size:isCached:)` — scales with Dynamic Type to `CC.size.dotMaxScale` (8 → 14 at AX5) |
| `CCFreshnessPill` | `CCFreshnessPill(health:action:)` — the only written copy of the freshness table: each `LinkHealth.level` maps to one dot, one mono label, whether actions are enabled, and which banner (if any) shows |
| `CCField` | `CCField(label:text:placeholder:hint:error:axis:lineLimit:isSecure:keyboardType:…:isMono:onSubmit:)` — 52pt. `label` is **optional**: an empty string still reserves ~22pt of its line, `nil` draws no row |
| `CCSectionHeader` | `CCSectionHeader("Blocked", count:, dotColor:, dotPulses:, note:, noteAction:, actionTitle:, action:)` — **place it flush**: it puts its own label on the 52pt column and takes no column parameter. The dot hangs itself into the 32pt gutter, takes no layout width, and stays centred on that axis as it scales |
| `CCEmptyState` | `CCEmptyState(glyph:title:message:tone:actionTitle:action:actionDisabledReason:secondaryActionTitle:secondaryAction:)` — the primary action is `.primary`, because an empty state has exactly one action worth taking, and `actionDisabledReason:` *draws* why the only way out is shut |
| `CCScreenMark` | `CCScreenMark(glyph:tone:)` — a 32pt glyph in a 64pt bordered circle. **A component, not a modifier, because the Dynamic Type ramp is welded to the token**: `ccGlyphContainer`'s free `relativeTo:` let two marks meaning "here is the thing this screen is about" ride `.title2` and `.largeTitle`, which measured 150.33 against 108.67 at AX5 and 0.00 apart at the default `L`. Use it for any such mark |
| `CCSheetChrome` | `CCSheetChrome("Decision", subtitle:, onClose:) { content }` (+ `trailing:` slot); pairs with `CCActionBar`. The **subtitle** resolves backtick markup, so a sheet's anchor sets its path in mono; the **title** is verbatim and bounded to two lines, because it carries a project name — arbitrary text from a filesystem, which a markup renderer would eat |
| `CCActionPair` | `CCActionPair { deny } allow: { allow }` — **40 : 60, 12pt gap**, and no ratio parameter. A `Layout`, not a measurement: exact on the first frame, no `@State`, no `PreferenceKey`. Stacks at accessibility sizes |
| `CCMonoBlock` | `CCMonoBlock(text, lineLimit:, tone:, showsCopy:, wraps:, truncation:, isSmall:)` — **wraps at the measured column with the diff grid's `↳`; never fades, never ellipsises, never runs under the copy button.** `lineLimit` collapses with a `SHOW ALL n LINES` disclosure rather than truncating. `wraps:` soft-wraps prose-shaped output and marks only the breaks that cut a token. `truncation:` is `CCMonoTruncation.head`/`.middle` and is for **unbounded identifiers only** — there is no `.tail`, because tail truncation cuts a command's arguments. `CCMonoBlock(inline:truncation:lines:)` is the same run with **no container**: no raised surface, no copy button, no extra height, for the three places a command appears inside something else |
| `CCSegmented` | `CCSegmented(selection:options:accessibilityLabel:)` with `CCSegmentedOption(value, title:, icon:)` |
| `CCGapMarker` | `CCGapMarker(label:actionLabel:action:)` — inline, at its position in time |
| `CCBanner` / `CCBannerSlot` | `CCBannerSlot([CCBannerItem(.rejected, title:…), …])` renders **exactly one**, by the ladder `rejected > offline > stale > cached > gap > truncated` |
| `CCHunkHeader` | `CCHunkHeader(header:fontSize:actions: CCHunkActions(comment:copyHunk:copyPath:))` — the kit draws the glyph, owns the menu and **is** the 44pt target. Pass actions as values, not a built control; the `menu:` closure form is legacy and cannot carry the gesture |
| Primitives | `CCHairline`, `CCIcon`, `CCProse`, `CCProgressRing` (the only spinner), `CCAdaptiveStack`, `CCPasteboard`, `CCMeasured`, `CCInlineCode`, `.ccSurface`, `.ccHitTarget`, `.ccDisabled`, `.ccFocusRing`, `.ccGlyphContainer`, `.ccColumnInset` |

### Global chrome

Three modifiers live in `DesignKit.swift` beside `.ccAppearance()`, because they
are the app's chrome and not any screen's business:

| Modifier | What it does |
|---|---|
| `.ccAppearance()` | dark, tint, window background. Once, at the root |
| `.ccNavigationChrome()` | **both** halves of the flat bar: an opaque `bg` toolbar background *and* `scrollEdgeEffectStyle(.hard, for: .top)`. iOS 26 draws the scroll-edge blur independently of the toolbar background, so the colour alone leaves a lighter slab at the top of a black screen and the hard edge alone leaves the bar translucent. They are welded into one modifier because every screen that took one and not the other shipped a different wrong bar |
| `.ccPlainToolbarItem()` | on each `ToolbarItem`: hides iOS 26's shared Liquid Glass capsule, leaving the item's self-drawn 36pt circle. A bare `ToolbarItem` is a bug |

### A glyph and its container scale together

`.ccGlyphContainer(diameter:radius:level:border:relativeTo:)` is the one
implementation of "a circle around a symbol". `CCIcon` scales with Dynamic Type
and a `.frame(width: 28)` does not, so at `accessibility-extra-large` the sheet's
close cross measured 34pt inside its 28pt ring. Pass the **same** `relativeTo`
the glyph was built with. Used by `CCSheetChrome`, `CCMonoBlock`, `CCEmptyState`,
`CCStepRow`, and the three screens with a bordered-circle toolbar button.

### The drawn control and the finger are two different objects

**44pt of target, whatever the drawn chrome measures — and the 44 is always
real layout, never a `contentShape` reaching outside a frame.** An expanded
content shape is *reported* to the accessibility tree without being hit-tested:
it measures as a pass and fails under a thumb.

So `CCSegmented` draws its 36pt track inside a button that is genuinely 44pt
tall (4pt of transparent overhang, 3pt of track padding, a 30pt thumb — every
point inside the `Button`'s own frame); it shipped with the border drawn around
the hit target and measured **49.67pt**. `CCFreshnessPill` draws its 36 inside
44 — it shipped as the height of its own content, **22.33pt**, beside a 36pt
bordered circle on the same baseline. And a `CCBadge` that carries an `action` is
not a badge: file chips, compose templates and toggles are `CC.size.chip`
32, not the label's 20, so a row of things to press stops reading as a strip of
captions.

### A separator has one inset, and it is none

`CCHairline` runs the full width of whatever contains it. Two insets shipped —
full card width on Fleet and Link Health, left-inset 16 with a flush right edge
in Settings — which measured as one asymmetric, unique rule. If you want a
narrower rule, inset the container. `CCHairline(leadingInset:)` and
`CCRow(separatorInset:)` are **gone**, along with `CCBadge(style:)` and
`CCCard(level:)`: all four survived a release as ignored arguments so the screens
could drop them in their own commit, and every call site now has.

### Two left edges, and the kit owns both

`CCColumn` is where the two vertical edges are written down, measured from a
**card's own** leading edge: `gutter` 16 (32 on screen — dots, glyphs and index
badges, and **nothing written**) and `content` 36 (52 on screen — every text run
there is). `gap` is the 20 between them, which is what a component adds to step
from one to the other.

**A section-header label is text**, so it starts on 52 like everything else, and
`CCSectionHeader` now places its own label there. It
takes no column parameter, and **a call site must place it flush** — no leading
inset, no `.padding(.horizontal, CC.space.md)`. It owns 36 on the leading edge
and `CC.space.md` on the trailing one, which is what lands a header's note on 370
rather than 386. Eleven headers measured off the column before this — ten at
x=32.00 and one at x=16.00, against the 52.00 every row they named was already
holding — across three different ways of being wrong.

Two containers therefore both know a number, which is exactly how a label ends up
on 72. `CCCard` publishes its own inset with `.ccColumnInset(_:)` and
`CCSectionHeader` / `CCFactRow` ask `CCColumn.step(from:)` for the **remainder**:
free-standing they add the whole 36, inside a card's header slot they add nothing,
and neither call site is told which case it is in. The gallery walked into the
72pt collision the day the column moved — that is the argument for the component
owning the number, not against it.

### The column governs text, not chrome

A **nested surface** — a `CCMonoBlock`, a tinted band, an alarm panel — keeps its
*text* on the content column and hangs its own border 12pt left into the gutter
to do so: **block text at 52, block border at 40**. Monospace is never set hard
against a border, and a container never pushes what is inside it onto a third
text edge. A border is not a mark and not a text run; it is free to sit where the
type requires.

`CCColumn.hang(from:)` is the one implementation, and `CCMonoBlock` is the one
caller. Three cases, and the component is told which it is in rather than the
call site being asked: inside a container that declares its column (`CCCard`,
`CCStepRow`, `CCFactRow`'s detail slot) it hangs the 12 its
own padding will add; free-standing on a page it finds the content column itself,
exactly as `CCSectionHeader` does, and hangs back from there; and inside a
**centred** container — `CCEmptyState`, `CCCard(contentColumn: false)`, both of
which declare themselves with `.ccCentredContent()` — nothing moves, because
there is no gutter and a 12pt step on one side of a centred 280pt block is a 6pt
error in the middle of it.

Measured at AX5 before the rule existed: every string on the card at 32.00 and
the command at **44.00** — one leftover text edge. In a card it was 64.00
against 52.00.

`CCBanner` lands on the same two edges by the same construction as a row: the
glyph takes the 8pt gutter slot and bleeds symmetrically, the words start on 52.
It measured glyph ink at 30.33 and text at 62.33, agreeing with neither of the
cards it usually sits between.

`CCRow`, `CCStepRow` and `CCFactRow` land on both by themselves. A row's leading
slot reserves `CC.size.dot` and lets a wider mark **bleed symmetrically** about
the gutter axis, so the title is on 52 whether the row is led by a dot, a glyph
or an index badge — before this, a dot row put its title on 52.00 and a `CCIcon`
row put its title on 65.33 inside one Settings card. `CCCard`'s default padding
lands its content on 52 as well, so a paragraph in a card starts where the title
of a row in the card beside it does — pass `contentColumn: false` only for a card
whose content is *centred*, where a 20pt step on one side is a 10pt error in the
middle. Nothing outside the kit should be adding a number to reach a column; two
screens carried a private 20pt constant at every call site, and that is what this
replaced.

### Identifiers are monospace, in prose too

Backticks are **markup**, and markup never renders. `CCProse(_:style:color:)`
draws a `CCTextStyle` exactly as `.ccType` does — same scaled size, tracking and
leading — except that a backtick-delimited span is set at the monospaced design
and the backticks are consumed. Colour is inherited, never changed: the rule
being enforced is "identifiers are monospace", and repainting the span would
invent a second rule that fights every toned surface a message can land on. An
**odd** number of backticks is not markup and renders literally, so a lone
backtick in a value is never silently eaten.

`CCSheetChrome`'s title and subtitle, `CCButton`'s label, `CCBanner`'s message,
`CCEmptyState`'s message, `CCField`'s hint and error, `ccDisabled`'s reason and
the gallery's own captions all resolve it. Strings that already carry backticks
— the comment sheet's ``In `…/Sender.swift` lines 12–28``, `codeconnect pair`, `cc
token` — became correct without their call sites being touched.

### A value nobody measured

`CCMeasured` is the only implementation of that rule, and both components that
print a value ask it. An unmeasured value renders `CCMeasured.mark` (`—`, never
`0` — `0` is a measurement) in `textDisabled`, and **unmeasured outranks tone**:
a number nobody took cannot also be news. A value that *is* already the em dash
is recognised without the flag, which is what stops the two components drifting
apart again — `CCStat` had no notion of the state at all and drew `MISSED
DECISIONS`'s `—` at #EDEDED / 16.91:1, indistinguishable from the two real
measurements beside it, while `CCFactRow` eleven rows below drew the identical
state at #525252 / 2.53:1. Prefer `CCStat.unmeasured(_:)`, which sets the mark
and the state together.

### Tall content is capped, not spilled

`.ccScrollCap(_ fraction: CGFloat = 0.45, minimum: CGFloat = 120)` bounds a
block at a fraction of the viewport and scrolls the overflow. Use it on anything
that grows without limit at accessibility sizes — a bar list, a stat column, an
action bar. Rendering a HIGH card at AX5 shows the alternative: the bar
grew until it owned the bottom 45% of the screen and then kept going, and **zero
characters of the command were visible while `Hold to allow` was fully armed**.

A `ScrollView` takes every point it is offered, so the cap is a measured height,
not a `maxHeight`: below the cap the block is exactly as tall as its content and
does not scroll at all. Tell it the truth about its container with
`.ccViewport(height:)` where a sheet or card knows its own bounds; otherwise it
asks the key window — and a deck card is not the window, so the decision card's
pinned footer capped itself at 874 × 0.45 = 393.3 and took **56.1% of a 699.33pt
card**.

**Measure it from outside the thing it bounds.** `DeckView` publishes the height,
not `DecisionCardView`: the first attempt measured the card's own scroll view and
fed the number into the environment its own pinned footer lays out in, which
closed the feedback loop `DecisionCardView.probe` describes. Three AX5
`DeckUITests` went from 6s to 177s and failed with `Failed to get matching
snapshots` — the app never once reporting itself idle. The deck's `ZStack` is
sized by the navigation stack and by nothing inside it, so the value cannot chase
what it is bounding.

**Do not cap a command block.** `CCMonoBlock` deliberately does not use this: an
internally-scrolling command would put its bottom marker on screen while most of
the command was not, which is exactly what the decision card's read gate
measures.

### Rules for screen implementers

- **Never** `ProgressView`, `.pickerStyle(.segmented)`, `ContentUnavailableView`,
  `DisclosureGroup`, `.textFieldStyle(.roundedBorder)`, `.background(.bar)`, or
  `.listStyle(.insetGrouped)`. Each has a CC replacement above.
- **Disabling requires a reason.** `.ccDisabled(CCDisabledReason("…"))` *draws*
  the reason in `footnote` `warning` next to the control — it is not only an
  accessibility hint, and the control is never dimmed with `.opacity`.
- **One banner per screen.** Hand `CCBannerSlot` every candidate and let it pick.
- **Status is one dot plus one band word.** Never a pill *and* a rail *and* a
  glyph. `running` renders white, not blue. Hollow dot = from cache.
- Two left edges only: **32pt** (gutter — dots, glyphs and index badges, and
  nothing written) and **52pt** (every text run, section-header labels
  included), and they are `CCColumn.gutter` / `CCColumn.content`. `CCRow`,
  `CCStepRow`, `CCFactRow`, `CCSectionHeader` and `CCCard`'s own padding land on
  them automatically. **If you are writing a number to reach a column, you are
  about to double it** — place the component flush and let it do this.
- **Identifiers and commands are monospace in prose too.** Write them in
  backticks and the kit's text slots set them in mono and eat the markup; never
  hand-build a mixed run, and never ship a literal grave accent.
- **A control is 44pt.** If a component becomes tappable, its band grows to
  `CC.size.hitTarget` — an expanded `contentShape` is *reported* to the
  accessibility tree without being hit-tested, which measures as a pass and
  fails under a thumb (verified by coordinate tap on `CCHunkHeader`).

## Accessibility verification

Measured with the WCAG 2.1 relative-luminance formula; every pairing the kit can
actually produce was checked.

**Text on surface** — all ✅ at AA (4.5:1) except where noted:

| | `bg` #000000 | `surface` #0A0A0A | `surfaceRaised` #131313 | `surfaceOverlay` #1A1A1A |
|---|---|---|---|---|
| `text` #EDEDED | 17.94 | 16.91 | 15.87 | 14.87 |
| `textSecondary` #A1A1A1 | 8.13 | 7.66 | 7.19 | 6.74 |
| `textTertiary` #828282 | 5.42 | 5.11 | 4.79 | 4.50 |
| `textDisabled` #525252 | 2.69 | 2.53 | 2.38 | 2.23 — exempt |

- **`textTertiary` was #737373 and failed AA on all four surfaces** (4.43 / 4.18
  / 3.92 / 3.67). A first correction to #7D7D7D cleared the first three but sat
  at 4.23 on `surfaceOverlay` — which would have needed a "don't use it there" rule.
  It is now **#828282**, which clears AA on all four, so the rule is deleted
  rather than documented: a token that is safe everywhere cannot be misused
  anywhere.
- `textDisabled` is intentionally below AA and is exempt under WCAG 1.4.3
  ("inactive user interface component"). Permitted uses only: diff gutter line
  numbers, the cwd breadcrumb, the `·` between a project and its start time, disabled-button
  labels — each of which sits beside a full-contrast explanation.

**Semantic on surface** — all ✅: success 10.52 / info 5.71 / warning 10.36 /
danger 6.43 on `bg`; the tightest is info on `surfaceOverlay` at 4.73.
Semantic-on-its-own-12%-wash: success 8.31, warning 8.20, danger 5.44, info 4.84.
Diff tints carry `text` at 12.70:1 or better. Primary button: `#000` on `#EDEDED`
= 17.94, pressed `#D4D4D4` = 14.17.

**Syntax** — measured against every background a diff line can have. The
deciding one is not `bg`, it is the 22% word-diff tint `successWord` #0D2D1F:
keyword #C792EA 8.73 / 6.18, string #C3E88D 15.25 / 10.80, comment #909090
6.58 / 4.66. Comments are deliberately **not** set in `textDisabled`, for the
same reason `textTertiary` was corrected: `textDisabled` is reserved for
genuinely inactive text, and a code comment — at 2.69:1 — is something you have
to read.

**ANSI** — ten of the sixteen are palette tokens re-used by name, so the
terminal's green is a `CCDiffRow`'s green. On `bg`, in index order: `—` / 6.43 /
10.52 / 10.36 / 5.71 / 5.28 / 9.74 / 8.13 · `—` / 9.92 / 14.75 / 13.84 / 10.00 /
8.73 / 14.91 / 17.94. The two exemptions are **black** (#131313, 1.13) and
**bright black** (#525252, 2.69): ANSI black is a background colour and is
invisible on a black terminal by construction — lifting it would break
`\e[30;47m` — and bright black is the conventional dim slot, where printing a
program's "dim" brightly would misreport what the program said. Neither is a
colour this app chooses for its own text.

**Borders are not AA-rated and do not need to be.** `border` #262626 measures
1.39:1 on `bg` and `borderStrong` #383838 measures 1.79:1. They are *separation*,
never the sole identifier of a control: every CC control is identified by its
label (≥ 4.5:1) and, when focused, by a 2pt `borderFocus` ring at **17.94:1**.
WCAG 1.4.11 and 2.4.11 are met through the label and the focus ring.

**Dynamic Type** — verified by screenshot at `medium`, `xxxLarge`, and
`AX5` (`accessibility-extra-extra-extra-large`) on iPhone 17 Pro. Nothing clips
and nothing truncates. Six layout switches make that true: `CCRow` moves its
trailing accessory below the text block at accessibility sizes; `CCSheetChrome`
does the same with its toolbar circles, which are ~60pt each at AX5 and left
`Diff · fx-1` about 150pt to wrap in; `CCBadge` stops being `fixedSize`
horizontally so it wraps instead of starving the row; `CCMonoBlock` moves its
copy button out of the text's line and into a labelled row underneath, giving
the command the block's full width; `CCStatStrip` becomes a vertical stack; and
`CCAdaptiveStack` does the same for button groups, banners and section headers.

**Chrome is not content, and at AX5 the diff sheet is the proof.** Measured on an
874pt screen, the code started ~700pt down: the reader opened a diff and could
see six lines of it. Four accommodations, each giving the same information in
less height rather than giving up a fact:

* the stamp's counts become **one wrapping `monoSmall` run** instead of three
  stacked `mono` lines (side by side they clipped to `+2,… −2,… 1 fi…`, which was
  the reason they were stacked);
* the provenance line drops the constant — `Captured 0s ago`, not `Captured on
  the Mac 0s ago · git diff HEAD` — because `git diff HEAD` is the same command
  on every diff this product draws and the *age* is the whole point. VoiceOver
  still gets the full sentence;
* the sticky file header drops its `+n −n` (restated on the chip directly above)
  and draws the **basename**, so the band that answers *which file am I in* stops
  head-truncating to `….swift`;
* chip floors stop at `CC.size.chipMaxScale`; a 32pt chip was reserving ~115pt
  for a ~40pt line;
* and the stamp band uses **`ccScrollCap`** instead of a hand-rolled
  `ScrollView` + `.frame(maxHeight:)`, which is exactly the mistake that modifier
  exists to prevent — a `ScrollView` takes every point it is offered, so the band
  claimed 45% of the sheet whether or not it had anything to put there:
  **253.33pt of band around 115pt of content**. Its ceiling is a third rather
  than the usual 45%, because 45% is the figure for a block where the decision is
  made and this one is provenance standing above the thing the reader opened.

Measured on the render harness, `diff-sample` at AX5: the first line of code
moved from **707pt to 574pt** down an 874pt screen, so the grid went from 167pt
to 300pt — 80% more code, nothing dropped.

The largest remaining item is measured and **not** fixed: a sheet's two toolbar
circles are ~94pt each at AX5 (`ccGlyphContainer(28, relativeTo: .footnote)`
× 3.4), which is 188pt of `CCSheetChrome` before any content. The kit's own answer
exists — `CCMonoBlock` already turns its copy button into a labelled 44pt row at
those sizes, on the reasoning that "a 60pt icon-only square is a worse target
than a labelled 44pt row" — but applying it to `CCSheetChrome` changes every
sheet in the product and wants its own render pass.

**A command never breaks where the layout engine wants to.** Handed
`git push --force origin main` at AX5, the engine breaks after the `--`, which
reads as a bare `--` separator followed by a file called `force`.
`CCMonoBlock` — block *and* `inline:` form — wraps at the measured column
instead and marks every break with the diff grid's `↳`, so a break inside a
token is always visible as one.

**Nothing round is a fixed size.** `CCStatusDot`, every badge and chip, and every
glyph container ride `@ScaledMetric`. Dots stop at `CC.size.dotMaxScale` (8 → 14)
so a growing disc cannot burst the 32–40pt gutter column the spine is built on;
chips scale their padding as well as their height, or a two-letter badge renders
as a tall portrait box with a word wedged into it.

**VoiceOver** — every interactive component carries a label, the right traits
(`.isButton`, `.isSelected`, `.isHeader`), and a value where state exists
(`"Busy"` while loading). `CCHoldButton` exposes a plain activation because
VoiceOver cannot express a hold. `CCFreshnessPill` speaks ages as words
("14 seconds ago"), never "fourteen ess".

## Reviewing the system

`CCGallery` (DEBUG only) renders every component in every state across eight
pages. Open `CodeConnect/Views/DesignSystem/CCGallery.swift` and run any
`#Preview` — `Gallery · AX5 (worst case)` is the one that matters. Pressed
states are live, not mocked.

**Every component under `Views/DesignSystem/` has a page**, and the gallery's own
header says so, which makes it a claim that can fail. The `terminal` and `diff`
pages exist for the components whose only other render is a screen needing a
live terminal or a real diff — `CCKeyCap`, `CCKeyCapDivider`, `CCSkeleton`,
`CCSkeletonRow`, `CCWaitingNotice`, `CCProgressRing`, `CCScannerFrame`,
`CCDisclosure` and the diff strip — because "a control is 44pt" written about a
component nobody can photograph is a claim nobody has checked.
`render-screens.sh` photographs all eight at `L` and AX5.

To drive it on a simulator, launch with `SIMCTL_CHILD_CC_GALLERY_PAGE=buttons`
(or `rows`, `indicators`, `controls`, `feedback`, `terminal`, `diff`) — the app
boots into the gallery whenever `CC_GALLERY_PAGE` is set (DEBUG only,
`CodeConnectApp.swift`), rather than needing the app root edited by hand before
every render. A page is long enough that "scroll to the component"
is not an instruction anyone follows to the end, so
`SIMCTL_CHILD_CC_GALLERY_SECTION=CCFactRow` opens directly on a section — the
string is the section's own heading. Set the type size with
`xcrun simctl ui <dev> content_size accessibility-extra-extra-extra-large`, and
**set it back to `large`, not `medium`.**

`large` is iOS's default — `UICTContentSizeCategoryL`. `medium` is one step
below it and renders about 7% small, and the error does not stop at fonts:
`@ScaledMetric` sizes badges, dots, chips, skeleton bars and glyph containers, so
five independent tokens read 1–4pt short at `medium` and are exact at `large`.
**Eight earlier measurements had to be thrown out because of this**, and one real
defect hid behind it — two glyph circles that measured 2.99pt apart at `medium`, 0.00 at `large`
and **41.66pt at AX5**, because `medium` is not where the two ramps cross.
Measure at `large` and AX5; never "fix" a token to match a `medium` reading.

Note also that a `contentShape` larger than a view's frame is **reported** to the
accessibility tree without being hit-tested: `element.frame` will read a perfect
`44.0` while a coordinate tap inside it does nothing. Verify any touch-target
change by coordinate tap, never by frame measurement.

Reduce Motion has no writable environment key and must be checked in
Settings → Accessibility → Motion.
