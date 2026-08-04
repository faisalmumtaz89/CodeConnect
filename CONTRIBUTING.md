# Contributing

This is a personal project shared in public. Issues and pull requests are welcome, but expect a slow and opinionated maintainer.

## Before a large change

Open an issue first. Much of this codebase is the way it is because of something that was measured — the comments record those numbers, and a change that contradicts one should say why the measurement no longer holds.

## Building

```sh
cd mac && cargo test --workspace     # daemon, shim, protocol
cd mac && ./install.sh               # build and install to ~/.codeconnect/bin
```

The iOS app is an Xcode project in `ios/`; open `ios/CodeConnect.xcodeproj`. Unit tests need no daemon. The suites named `*LiveUITests` do — they drive the real app against a running `ccd` and skip cleanly when one is not reachable.

## House rules

These are not style preferences; each one was paid for by a bug.

**Never parse terminal bytes for meaning.** Approvals ride structured channels — hooks, and later ACP or Codex's app-server. Claude Code's transcript contains no approval events at all, and three independent projects abandoned screen-scraping before this one started.

**Never let the UI claim something it does not know.** Every rendered fact carries its age. An unmeasured value is an em dash at `textDisabled`, never a `0`. A disabled control draws the reason it is disabled. If the daemon said something, quote it and attribute it; if the app is speaking, say so.

**Fail toward the human.** If the daemon is unreachable, a permission prompt goes back to the local keyboard. A command that cannot be verified is refused, not guessed at.

**Colour is information.** It appears for risk, status and freshness. If a colour is not telling the reader something actionable, remove it.

**Make the wrong thing unconstructible.** Prefer deleting the API that permits a defect over fixing each instance. There is no `.tail` truncation for a command, `CCHairline` has no inset parameter, and a section header cannot be placed on the wrong column — because each of those was a real bug that would otherwise recur.

## Testing UI

Two traps, both of which have produced false findings here:

- **Measure at `UICTContentSizeCategoryL`.** It is iOS's default; `M` is one step below it and renders about 7% small, including `@ScaledMetric` sizes. Several "defects" measured at `M` were correct code.
- **An expanded `contentShape` is reported to the accessibility tree without being hit-tested.** A control can measure a perfect 44pt in `element.frame` and still do nothing under a thumb. Draw the real target, and verify by coordinate tap plus pixel diff.

Adding or removing a user-visible screen, or a state that concerns safety or honesty, means updating the render catalog in the same change.

## Releasing

Maintainer-only. The exact steps — and the contract behind them, which the
in-app update checker depends on — live in [`RELEASING.md`](RELEASING.md).

## Commits

Present tense, and say what changed and why. If a change is driven by a measurement, put the measurement in the message or the comment — the numbers in this codebase are load-bearing.
