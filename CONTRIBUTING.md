# Contributing

This is a personal project shared in public. Issues and pull requests are welcome, but expect a slow and opinionated maintainer.

## Before a large change

Open an issue first. Much of this codebase is the way it is because of something that was measured — the comments record those numbers, and a change that contradicts one should say why the measurement no longer holds.

## Building

```sh
cd mac && cargo test --workspace     # daemon, CLI, protocol
cd mac && ./install.sh               # build and install to ~/.codeconnect/bin
```

The Rust workspace is in `mac/`, the iPhone app in `ios/`, the Mac app in `macos/`, the
code both apps share in the `CodeConnectKit/` package, design notes in `docs/`, and the
recordings the tests replay in `fixtures/`.

Building needs Rust 1.85 or later and Xcode 26 or newer. The app deploys to iOS 17, but
one navigation-chrome call is guarded with `if #available(iOS 26, *)`, and `#available`
is a runtime check, so the symbol still has to exist in the SDK to compile.

The iOS app is an Xcode project in `ios/`; open `ios/CodeConnect.xcodeproj`. Unit tests need no daemon. The suites named `*LiveUITests` do — they drive the real app against a running `ccd` and skip cleanly when one is not reachable.

The Mac app is an Xcode project in `macos/`; open `macos/CodeConnect.xcodeproj`. It deploys to macOS 26 and builds against the same `CodeConnectKit` package. A Debug build photographs its own window to a PNG and quits when launched with `-CC_RENDER_WINDOW <path>`, and `-CC_RENDER_WINDOW_SIZE <width>x<height>` sets the window's size first: `open -g -n -W CodeConnect.app --args -CC_RENDER_WINDOW out.png`. Launch it in the background (`-g`) but not hidden: the app cannot become active during a render, puts its window behind every other on a plain backdrop, and the window server photographs it, which a hidden app's window cannot be. `-CC_ADD_COMPUTER YES` opens the Add-a-computer sheet, which is photographed with the window; `-CC_RENDER_PAIR "<address> <code>"` submits that pairing in the sheet as if typed, and the window is photographed once the computer has answered: refused, or paired and connected. A launch with any `-CC_` flag uses no keychain, has an installation id of its own that is never saved to the app's defaults, and keeps the app's data in a throwaway folder in the user's temporary folder, removed when its render ends.

## House rules

These are not style preferences; each one was paid for by a bug.

**Never parse terminal bytes for meaning.** Approvals ride structured channels: Claude Code's agent host and Codex's app server. Claude Code's transcript contains no approval events at all, and three independent projects abandoned screen-scraping before this one started.

**Never let the UI claim something it does not know.** Every rendered fact carries its age. An unmeasured value is an em dash at `textDisabled`, never a `0`. A disabled control draws the reason it is disabled. If the daemon said something, quote it and attribute it; if the app is speaking, say so.

**Fail toward the human.** A command that cannot be verified is refused, not guessed at.

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
