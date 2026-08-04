# Release checklist

Cutting a release is what makes the update notice in `codeconnect claude`
ring on other machines, so the steps below are a contract, not a
convention: the checker compares GitHub's **latest published Release tag**
against each binary's built-in version, and every step exists to keep that
comparison truthful. A test (`the_workspace_version_is_parseable` in
`mac/codeconnect/src/update_check.rs`) fails CI if the workspace version
ever stops being a plain `X.Y.Z`.

The iOS app is a separate track: it ships through the App Store on its own
version, and nothing here touches it. Within a protocol major, the app
tolerates an older daemon per surface — newer features hide or say what to
update; a protocol-*major* mismatch is refused outright, on both sides.

## The contract

* The tag names the exact commit whose workspace version it matches:
  `vX.Y.Z` tags a commit where `mac/Cargo.toml` says `version = "X.Y.Z"`.
* Publish releases from ever-newer commits, with strictly increasing
  versions. GitHub's `releases/latest` is the non-draft, non-prerelease
  release whose **tagged commit is most recent** — commit date, not
  publication date, and not the greatest semver — so a release cut from an
  older commit after a newer one would misreport "latest" to every checker.
* A published release is never deleted or retagged. Machines have already
  compared against it; rewriting it rewrites their history.
* Plain `X.Y.Z` only — no suffixes. The checker's validator rejects
  anything else outright, so a `v0.3.0-rc1` tag would simply never reach a
  user, and publishing it as "latest" would mask the release before it.

## Steps

Each block starts from the repository root.

1. **Gates.** Everything CI runs, plus the UI suites CI skips:

   ```sh
   (cd mac && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace)
   (cd ios && xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
     -destination "platform=iOS Simulator,name=iPhone 17 Pro" \
     -only-testing:CodeConnectTests CODE_SIGNING_ALLOWED=NO)
   (cd ios && rm -rf .artifacts/uitest-deriveddata && xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
     -destination "platform=iOS Simulator,name=iPhone 17 Pro" \
     -derivedDataPath .artifacts/uitest-deriveddata \
     -only-testing:CodeConnectUITests CODE_SIGNING_ALLOWED=NO)
   ```

   CI's hygiene job runs on push; do not tag until the release commit is
   green there too.

2. **Bump.** Set the new version in one place — `mac/Cargo.toml`
   (`[workspace.package] version`) — then build the shipped binary and read
   the version back:

   ```sh
   (cd mac && cargo build --release --bin codeconnect && ./target/release/codeconnect --version)
   ```

   All three shipped binaries (`codeconnect`, `ccd`, `cc-hook`) inherit the
   workspace version; the printed version must be the one you are about to
   tag.

3. **Commit and push.** The bump rides in the release commit:

   ```sh
   git add -A && git commit && git push
   ```

   Wait for CI to go green on that commit before tagging it — a tag is a
   claim the commit builds, and CI is the proof.

4. **Confirm the version is a step forward, then tag and publish.**
   `--verify-tag` is load-bearing: without it, `gh release create` invents
   a missing tag from the default branch, which may not be the commit you
   verified.

   ```sh
   gh api 'repos/{owner}/{repo}/releases/latest' --jq .tag_name   # must be older than X.Y.Z (HTTP 404 means first release)
   git tag vX.Y.Z && git push origin vX.Y.Z
   gh release create vX.Y.Z --verify-tag --title "CodeConnect X.Y.Z" --notes "…"
   ```

   Not a draft, not a prerelease — the checker only sees published
   releases.

5. **Verify the claim on the server, not just locally** — this is what the
   checkers will actually read:

   ```sh
   gh release view vX.Y.Z --json tagName,isDraft,isPrerelease
   gh api 'repos/{owner}/{repo}/releases/latest' --jq .tag_name   # must print vX.Y.Z
   ```

6. **Update this machine**, so your own launches stay quiet:

   ```sh
   (cd mac && ./install.sh)      # builds, installs, restarts the daemon
   codeconnect --version          # prints X.Y.Z
   codeconnect daemon status      # no STALE line, no upgrade note
   ```

   Silence from the update notice is the success state: installed equals
   latest. A machine still on the old build learns of the release across
   its next launches — a launch at least a day after its previous check
   fetches in the background, and the launch after that shows the notice.
