# Release checklist

Cutting a release is what makes the update notice in `codeconnect claude` and `codeconnect codex`
ring on other machines, so the steps below are a contract, not a
convention: the checker compares GitHub's **latest published Release tag**
against each binary's built-in version, and every step exists to keep that
comparison truthful. A test (`the_workspace_version_is_parseable` in
`mac/codeconnect/src/update_check.rs`) fails CI if the workspace version
ever stops being a plain `X.Y.Z`.

The iPhone app is a separate track: it ships through the App Store, and
nothing here touches it. Within a protocol major, the app
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
     -only-testing:CodeConnectTests \
     CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM= PROVISIONING_PROFILE_SPECIFIER=)
   (cd ios && rm -rf .artifacts/uitest-deriveddata && xcodebuild test -project CodeConnect.xcodeproj -scheme CodeConnect \
     -destination "platform=iOS Simulator,name=iPhone 17 Pro" \
     -derivedDataPath .artifacts/uitest-deriveddata \
     -only-testing:CodeConnectUITests CODE_SIGNING_ALLOWED=NO)
   ```

   CI does not build the Mac app, so build it once here; the release builds it
   again for Release and checks the `.dmg` (step 4):

   ```sh
   xcodebuild -project macos/CodeConnect.xcodeproj -scheme CodeConnect -configuration Debug \
     -destination 'platform=macOS' build
   ```

   CI's hygiene job runs on push; do not tag until the release commit is
   green there too.

2. **Bump.** Set the new version in one place — `mac/Cargo.toml`
   (`[workspace.package] version`) — and write the release's notes for the
   Mac app's update window in `macos/release-notes.txt`, one plain paragraph
   (the GitHub release's own notes are separate). Then build the shipped
   binary and read the version back:

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

   If the release changes `mac/push-relay`, deploy the relay from this commit
   first ([`ops/push-relay/README.md`](ops/push-relay/README.md#deploy-and-rollback))
   and confirm its `/readyz` reports this commit's SHA before tagging. A Mac
   ahead of the relay still works — it rings a kind the relay refuses as one
   it accepts — but says less than it should until the relay catches up.

4. **Tag. The push builds and verifies a draft — it never publishes.**

   Pushing the tag starts `.github/workflows/release.yml`, which pins the
   tag to one commit, builds both architectures from that commit — the
   binaries, and CodeConnect's own tmux from the pinned sources in
   `mac/tmux/SOURCES` — signs and notarizes them, attaches the two archives
   (binaries, and tmux beside them) to a **draft**, downloads those exact
   assets back and re-verifies them with `.github/workflows/release-verify.sh`
   — the check step 5 repeats before publishing — and stops there. A draft is
   visible only to people who can see the repository's drafts, so nothing a
   user can reach has changed yet. Do not create the release yourself: a
   version that is not strictly newer than the current `releases/latest` is
   refused before anything is built, and a tag that already carries a
   published release is refused at the draft step — after signing, but
   before any live release is touched. `codeconnect update` independently
   refuses to install a release older than what is running — the producer
   checks stop a bad release being made, the updater's stops one that
   already exists from being installed.

   The same run builds the Linux archives (`linux-build.yml`: static x86_64
   and aarch64 binaries and tmux). Job `linux-sign` — the only job given the
   secret `LINUX_RELEASE_SIGNING_KEY` — refuses unless that key's public half
   is `RELEASE_KEY` in the tagged source
   (`mac/codeconnect/src/update_install.rs`), checks each archive against its
   `.sha256`, signs each `.sha256`, and verifies every signature against the
   pinned half before handing the files on. The draft then holds the seven
   macOS assets (the two archives and their checksums, and the app's `.dmg`,
   its checksum and the update feed `appcast.xml`) and twelve Linux ones, each downloaded
   back and checked, and publication requires exactly those nineteen: a Linux
   build that fails stops the release. The key's private half lives only in that secret and the
   owner's offline backup. Losing it means a new key and a new `RELEASE_KEY`,
   and every installed Linux updater refuses releases signed with the new key
   until the installer — one that pins the new key — is run again with
   `--reinstall` (`curl -fsSL https://codeconnect.sh | sh -s -- --reinstall`):
   a plain rerun on an existing install finishes its setup and downloads nothing.

   The same job builds the Mac app (`macos/`) from the pinned commit, with the
   binaries it has just signed and notarized inside it, on GitHub's `xcode-27`
   image with Xcode 27.1 pinned by path — the Xcode the app is built and
   tested with. The app is exported for Developer ID and packed as
   `CodeConnect-X.Y.Z.dmg`, which is notarized, stapled, and signed for Sparkle
   with the secret `SPARKLE_ED_PRIVATE_KEY`. That signature goes into the
   update feed, `appcast.xml`, which `.github/workflows/appcast.sh` writes
   whole from the release's own values: one item, this release's (older ones
   are not kept), with the notes from `macos/release-notes.txt` and the
   `.dmg`'s versioned URL, and no link but that and the feed's own address.
   `release-verify.sh` writes it again from the pinned source and requires
   the same bytes.
   `.github/workflows/mac-app-verify.sh` checks the `.dmg`
   the way Sparkle and Gatekeeper will — the public key baked into the app must
   verify its signature, Gatekeeper must accept the app as notarized
   Developer ID, and every library the app loads must be found inside it —
   once before attaching and again on the bytes downloaded back. The key's
   private half lives only in that secret and the owner's offline backup.
   Installed apps accept only updates signed with it, so losing
   it means their next version is a manual download. After publishing,
   codeconnect.sh copies the release's `appcast.xml`, unchanged, to
   `/mac/appcast.xml` — the address every copy of the app asks — and points
   `/mac` at the release's `.dmg`, in one deploy (codeconnect-web's
   `scripts/release-day.sh`).

   ```sh
   git tag vX.Y.Z && git push origin vX.Y.Z
   gh run watch                                   # ends at "verified draft"
   ```

5. **Publish, as its own deliberate act: the draft is promoted unchanged.**
   Run the same workflow with publishing enabled. Its `promote` job builds,
   signs and notarizes nothing and reads no repository secret. It refuses
   outright if step 4's draft does not exist — a publish run never quietly
   becomes a create-and-publish run. It downloads the draft's nineteen assets
   by id and checks them with the same `release-verify.sh` step 4 ran.
   Every asset is bound to the tag: each checksum is its own file's, the Linux
   ones signed with `RELEASE_KEY`; each Mac binary, in the archives and inside
   the app, is signed by the pinned team's Developer ID with a secure timestamp
   and carries both architectures; CodeConnect's own binaries carry the tag's
   commit in every slice, and each archive's cc-tmux the tmux bundle
   `mac/tmux/SOURCES` names; the Mac archives' binaries are notarized (asked of
   Apple online); every archive holds only regular files and is within the
   size its downloaders accept, and each core archive holds the pinned
   licence; the app carries the tag's version and the project's Sparkle key;
   and the `.dmg` is checked as Sparkle and Gatekeeper will check it. Then, only after rechecking that the draft is still a draft
   holding exactly those assets, that the version is still newer than the
   published latest, and that the tag still names the pinned commit, it flips
   the draft live: the bytes people download are the bytes step 4 built and
   anyone tested. This is the moment `codeconnect update` on every machine can
   see it. `--ref` runs the workflow as it is at the tag; without it, `main`'s
   copy runs, and an older workflow checks an older set of assets.

   ```sh
   gh workflow run release.yml --ref vX.Y.Z -f tag=vX.Y.Z -f publish=true
   gh run watch
   ```

   The first time the signing secrets are exercised, do step 4 alone and
   inspect the draft before running this — what you inspect is what step 5
   publishes, and that is the whole rehearsal procedure. A rehearsal draft
   that should be discarded:
   `gh release delete vX.Y.Z --yes` (the tag itself stays).

6. **Verify the claim on the server, not just locally** — this is what the
   checkers will actually read:

   ```sh
   gh release view vX.Y.Z --json tagName,isDraft,isPrerelease
   gh api 'repos/{owner}/{repo}/releases/latest' --jq .tag_name   # must print vX.Y.Z
   ```

7. **Update this machine**, so your own launches stay quiet. Where
   CodeConnect.app installed CodeConnect, update the app (CodeConnect › Check
   for Updates…): it installs the new binaries when it next opens, and
   `codeconnect update` refuses there. Elsewhere:

   ```sh
   codeconnect update             # installs the release, restarts the daemon
   codeconnect --version          # prints X.Y.Z
   codeconnect daemon status      # no STALE line, no upgrade note
   ```

   Silence from the update notice is the success state: installed equals
   latest. A machine still on the old build learns of the release across
   its next launches — a launch at least a day after its previous check
   fetches in the background, and the launch after that shows the notice.
