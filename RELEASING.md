# Release checklist

Cutting a release is what makes the update notice in `codeconnect claude`
ring on other machines, so the steps below are a contract, not a
convention: the checker compares GitHub's **latest published Release tag**
against each binary's built-in version, and every step exists to keep that
comparison truthful. A test (`the_workspace_version_is_parseable` in
`mac/codeconnect/src/update_check.rs`) fails CI if the workspace version
ever stops being a plain `X.Y.Z`.

The iOS app is a separate track: it is built from source in Xcode and is
not distributed, and nothing here touches it. Within a protocol major, the app
tolerates an older daemon per surface — newer features hide or say what to
update; a protocol-*major* mismatch is refused outright, on both sides.

## The contract

* The tag names the exact commit whose workspace version it matches:
  `vX.Y.Z` tags a commit where `mac/Cargo.toml` says `version = "X.Y.Z"`.
* Publish releases from ever-newer commits, with strictly increasing
  versions. The workflow claims "latest" explicitly at publication
  (`make_latest`), and GitHub defaults new releases to that claim anyway —
  so whatever publishes last is what every checker sees, whoever published
  it. That is exactly why the workflow refuses to publish a version that is
  not strictly newer, twice: once before building and again at the moment
  of publication — and why a release must never be published by hand.
* A published release is never deleted or retagged. Machines have already
  compared against it; rewriting it rewrites their history.
* **Version numbers move with capability, not only with releases.** Any
  commit that bumps `PROTOCOL_MINOR` bumps the workspace version in the same
  commit, and every release's version is strictly above the last published
  one. Between releases the binaries stay tellable-apart anyway: every build
  embeds its git commit — `codeconnect --version` prints
  `codeconnect X.Y.Z (<12-hex commit>)`, `-dirty` when built from an edited
  tree, `(build unknown)` outside a checkout — so two builds that share a
  version number are still tellable apart by the commit they name.
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

4. **Tag. The push builds and verifies a draft — it never publishes.**

   Pushing the tag starts `.github/workflows/release.yml`, which pins the
   tag to one commit, builds both architectures from that commit, signs and
   notarizes them, attaches the archive to a **draft**, downloads those
   exact assets back and re-verifies them — and stops there. A draft is
   visible only to people who can see the repository's drafts, so nothing a
   user can reach has changed yet. Do not create the release yourself: a
   version that is not strictly newer than the current `releases/latest` is
   refused before anything is built, and a tag that already carries a
   published release is refused at the draft step — after signing, but
   before any live release is touched. `codeconnect update` independently
   refuses to install a release older than what is running — the producer
   checks stop a bad release being made, the updater's stops one that
   already exists from being installed.

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   gh run watch                                   # ends at "verified draft"
   ```

5. **Publish, as its own deliberate act.** Run the same workflow with
   publishing enabled. It refuses outright if step 4's draft does not exist —
   a publish run never quietly becomes a create-and-publish run. It rebuilds
   and re-verifies everything from the same tag — publication publishes
   exactly the bytes the publishing run itself proved — and then, only after
   rechecking that the draft is still a draft with exactly the verified
   assets, that the version is still newer than the published latest, and
   that the tag still names the pinned commit, flips it live. This is the
   moment `codeconnect update` on every machine can see it.

   ```sh
   gh workflow run release.yml -f tag=vX.Y.Z -f publish=true
   gh run watch
   ```

   The first time the signing secrets are exercised, do step 4 alone and
   inspect the draft before running this — that is the whole rehearsal
   procedure. A rehearsal draft that should be discarded:
   `gh release delete vX.Y.Z --yes` (the tag itself stays).

6. **Verify the claim on the server, not just locally** — this is what the
   checkers will actually read:

   ```sh
   gh release view vX.Y.Z --json tagName,isDraft,isPrerelease
   gh api 'repos/{owner}/{repo}/releases/latest' --jq .tag_name   # must print vX.Y.Z
   ```

7. **Update this machine**, so your own launches stay quiet:

   ```sh
   codeconnect update             # installs the release, restarts the daemon
   codeconnect --version          # prints X.Y.Z
   codeconnect daemon status      # no STALE line, no upgrade note
   ```

   Silence from the update notice is the success state: installed equals
   latest. A machine still on the old build learns of the release across
   its next launches — a launch at least a day after its previous check
   fetches in the background, and the launch after that shows the notice.
