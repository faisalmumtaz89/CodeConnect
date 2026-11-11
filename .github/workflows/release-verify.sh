#!/usr/bin/env bash
# A release's nineteen assets as a user receives them, checked the way the product and the platform will check them.
# release.yml's draft job runs it on the draft it has just filled, and its promote job on the draft it is about to
# publish: one check, so a draft is promoted only after passing the same proof that made it.
#
#   release-verify.sh fetch <release id> <dir>   the release must hold exactly the nineteen expected assets, each
#                                                uploaded; each is downloaded by id into <dir>, and the ids and names
#                                                go to $GITHUB_OUTPUT for the Publish step
#   release-verify.sh check <dir>                the nineteen files in <dir>, checked
#
# Both take VERSION, SHA (the pinned commit) and GITHUB_REPOSITORY from the environment, `check` also OPENSSL
# (OpenSSL 3 or later, for Ed25519 with -rawin). Every asset is bound to that commit, by what it carries or by what the pinned source says:
# the Apple team and the Linux release key every installed updater enforces (mac/codeconnect/src/update_install.rs),
# the Sparkle key the app is built with (macos/CodeConnect.xcodeproj), the tmux bundle (mac/tmux/SOURCES) and the
# licence.
set -euo pipefail

: "${VERSION:?}" "${SHA:?}" "${GITHUB_REPOSITORY:?}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
source_file="$root/mac/codeconnect/src/update_install.rs"
dir_name="codeconnect-$VERSION"
archive="codeconnect-$VERSION-macos-universal.tar.gz"
tmux_archive="codeconnect-$VERSION-tmux-macos-universal.tar.gz"
dmg="CodeConnect-$VERSION.dmg"
linux=()
for arch in aarch64 x86_64; do
  for kind in "" "tmux-"; do
    a="codeconnect-$VERSION-${kind}linux-$arch.tar.gz"
    linux+=("$a" "$a.sha256" "$a.sha256.sig")
  done
done
nineteen="$(printf '%s\n' "$archive" "$archive.sha256" "$tmux_archive" "$tmux_archive.sha256" \
  "$dmg" "$dmg.sha256" appcast.xml "${linux[@]}" | sort)"

# Exactly the nineteen names, each once: an extra or a missing asset is a release the updater or the Publish step
# would treat differently from the one verified.
same_names() {
  if [ "$1" != "$nineteen" ]; then
    echo "::error::$2 does not hold exactly the nineteen release assets"
    diff <(printf '%s\n' "$nineteen") <(printf '%s\n' "$1") || true
    exit 1
  fi
}

fetch() {
  local id="$1" dir="$2" listing
  listing="$(gh api "repos/$GITHUB_REPOSITORY/releases/$id" --jq '[.assets[] | {id, name, state}]')"
  same_names "$(jq -r '.[].name' <<<"$listing" | sort)" "release $id"
  if [ "$(jq '[.[] | select(.state != "uploaded")] | length' <<<"$listing")" != 0 ]; then
    echo "::error::release $id holds an asset that is not fully uploaded"
    exit 1
  fi
  mkdir -p "$dir"
  # By asset id, with the octet-stream accept header — the only form in which a draft's assets can be fetched at all.
  while read -r asset name; do
    gh api -H "Accept: application/octet-stream" "repos/$GITHUB_REPOSITORY/releases/assets/$asset" > "$dir/$name"
  done < <(jq -r '.[] | "\(.id) \(.name)"' <<<"$listing")
  id_of() { jq -r --arg name "$1" '.[] | select(.name == $name) | .id' <<<"$listing"; }
  {
    echo "archive=$archive"
    echo "tmux_archive=$tmux_archive"
    echo "dmg=$dmg"
    echo "archive_id=$(id_of "$archive")"
    echo "checksum_id=$(id_of "$archive.sha256")"
    echo "tmux_archive_id=$(id_of "$tmux_archive")"
    echo "tmux_checksum_id=$(id_of "$tmux_archive.sha256")"
    echo "dmg_id=$(id_of "$dmg")"
    echo "dmg_checksum_id=$(id_of "$dmg.sha256")"
    echo "appcast_id=$(id_of appcast.xml)"
    echo "linux_assets=$(jq -c '[.[] | select(.name | test("-linux-")) | {id, name}]' <<<"$listing")"
  } >> "$GITHUB_OUTPUT"
}

# The rule the updater applies (update_release.rs, validate_members), applied here: exactly these members, each once and
# each a regular file — no directory entry, no link. Extracting and hoping would not have caught a member the product
# refuses.
holds_exactly() {
  local archive="$1" expected actual other
  shift
  expected="$(printf '%s\n' "$@" | sort)"
  actual="$(tar -tzf "$archive" | sort)"
  if [ "$expected" != "$actual" ]; then
    echo "::error::$archive does not hold exactly the expected members"
    diff <(printf '%s\n' "$expected") <(printf '%s\n' "$actual") || true
    exit 1
  fi
  other="$(tar -tvzf "$archive" | grep -v '^-' || true)"
  if [ -n "$other" ]; then
    echo "::error::$archive holds a member that is not a regular file: $other"
    exit 1
  fi
}

# A checksum vouches only for the file it is named for: it is exactly `shasum -a 256 <file>`, one line naming this
# file with this file's digest — what the updater requires of it (update_release.rs). `shasum -c` alone checks whichever
# file the line names, so another archive's checksum and signature would pass in this one's place.
own_checksum() {
  if [ "$(cat "$1.sha256")" != "$(shasum -a 256 "$1")" ]; then
    echo "::error::$1.sha256 is not the checksum of $1 alone"
    exit 1
  fi
}

# Each slice of a CodeConnect binary carries the commit it was built from: `--version`'s 12 hex digits are a string
# constant baked in by protocol/build.rs. A slice this runner cannot run is still proven to be the pinned commit's.
names_the_commit() {
  local binary="$1" path="$2"
  if ! LC_ALL=C grep -a -q -F "${SHA:0:12}" "$path" || ! LC_ALL=C grep -a -q -F "$VERSION" "$path"; then
    echo "::error::$binary ($path) was not built as $VERSION from ${SHA:0:12}"
    exit 1
  fi
}

# A file that ships as it is in the repository (the licence) is the pinned commit's, byte for byte.
from_source() {
  if ! cmp -s "$1" "$root/$2"; then
    echo "::error::$1 is not the pinned commit's $2"
    exit 1
  fi
}

# A tmux archive holds the bundle the pinned mac/tmux/SOURCES names — what `cc-tmux -V` reports and `codeconnect`
# requires, bumped whenever a source changes: the notices say they are that bundle's (mac/tmux/build.sh writes the
# line), and the binary carries it, in every slice.
of_the_bundle() {
  local notices="$1" binary="$2"
  if [ "$(head -1 "$notices")" != "CodeConnect's tmux ($bundle) is built from the following sources." ]; then
    echo "::error::$notices does not name the bundle $bundle"
    exit 1
  fi
  if ! LC_ALL=C grep -a -q -F "$bundle" "$binary"; then
    echo "::error::$binary is not the tmux bundle $bundle"
    exit 1
  fi
}

# One Mac binary that ships, in an archive or inside the app, held to what `codeconnect update` enforces: signed as
# itself by the pinned team's Developer ID, and carrying both architectures a release ships (update_install.rs,
# is_universal) — a slice extracted alone keeps a valid signature, and every updater refuses it. Then a secure
# timestamp and, for CodeConnect's own three, the pinned commit: the slice this runner runs says exactly what it is, and
# every slice carries the commit.
shipped_binary() {
  local binary="$1" path="$2" info expected said slice archs
  codesign --verify --strict --all-architectures -R="identifier \"$binary\" and $req" "$path"
  archs=" $(lipo -archs "$path") "
  if [[ "$archs" != *" arm64 "* || "$archs" != *" x86_64 "* ]]; then
    echo "::error::$path carries${archs% } rather than both arm64 and x86_64"
    exit 1
  fi
  # Captured, then searched — not piped into `grep -q`. `grep -q` exits at the first match, and the SIGPIPE that can
  # leave on `codesign` would turn a present timestamp into a failed check.
  info="$(codesign --display --verbose=4 "$path" 2>&1)"
  if [[ $'\n'"$info" != *$'\n'Timestamp=* ]]; then
    echo "::error::$path has no secure timestamp"
    exit 1
  fi
  [ "$binary" = cc-tmux ] && return 0
  expected="$binary $VERSION (${SHA:0:12})"
  said="$("$path" --version)"
  if [ "$said" != "$expected" ]; then
    echo "::error::$path reports '$said', expected '$expected'"
    exit 1
  fi
  for slice in $(lipo -archs "$path"); do
    lipo -thin "$slice" -output "$work/thin.$slice" "$path"
    names_the_commit "$binary ($slice)" "$work/thin.$slice"
  done
}

check() {
  local dir="$1" team key sparkle_key bundle ceiling public verdict expected said app value slice signature pub_date file
  cd "$dir"
  same_names "$(find . -mindepth 1 -maxdepth 1 | sed 's|^\./||' | sort)" "$dir"
  # Unpacked beside, never into, the downloaded files; global, so the trap still names it after `check` returns.
  work="$(mktemp -d)"
  trap 'hdiutil detach -quiet "$work/dmg" 2>/dev/null || true; rm -rf "$work"' EXIT
  team="$(awk -F'"' '/^pub const TEAM_ID/{print $2; exit}' "$source_file")"
  key="$(sed -n '/^pub const RELEASE_KEY/,/^];/p' "$source_file" | grep -o '0x[0-9a-f][0-9a-f]' | sed 's/^0x//' | tr -d '\n')"
  bundle="$(awk '$1 == "BUNDLE" {print $2; exit}' "$root/mac/tmux/SOURCES")"
  ceiling="$(sed -n 's/^pub const MAX_ARCHIVE_BYTES: u64 = \([0-9 *]*\);$/\1/p' \
    "$root/mac/codeconnect/src/update_release.rs")"
  if [ -z "$team" ] || [ "${#key}" != 64 ] || [ -z "$bundle" ] || [ -z "$ceiling" ]; then
    echo "::error::the pinned source names no TEAM_ID, no 32-byte RELEASE_KEY, no tmux BUNDLE, or no MAX_ARCHIVE_BYTES"
    exit 1
  fi

  # Every archive is within the ceilings the ones who download it enforce: the updater's MAX_ARCHIVE_BYTES, from the
  # pinned source, and the website installer's 100 MiB (`cc_fetch … 104857600` in codeconnect-web's
  # public/installers/v1.7.8.sh). Its members don't bound an archive's size: bytes after the end of the tar stream
  # add to it unseen.
  ceiling=$(( ceiling ))
  if [ "$ceiling" -gt 104857600 ]; then ceiling=104857600; fi
  for file in "$archive" "$tmux_archive" "${linux[@]}"; do
    case "$file" in *.tar.gz) ;; *) continue ;; esac
    if [ "$(stat -f %z "$file")" -gt "$ceiling" ]; then
      echo "::error::$file is $(stat -f %z "$file") bytes, past the $ceiling-byte ceiling its downloaders enforce"
      exit 1
    fi
  done

  # macOS: the archives match their checksums and hold exactly their members.
  own_checksum "$archive"
  own_checksum "$tmux_archive"
  holds_exactly "$archive" "$dir_name/codeconnect" "$dir_name/ccd" "$dir_name/cc-hook" "$dir_name/LICENSE"
  holds_exactly "$tmux_archive" "$dir_name/cc-tmux" "$dir_name/THIRD_PARTY_NOTICES"
  mkdir "$work/mac"
  tar -xzf "$archive" -C "$work/mac"
  tar -xzf "$tmux_archive" -C "$work/mac"

  req="anchor apple generic \
    and certificate 1[field.1.2.840.113635.100.6.2.6] exists \
    and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
    and certificate leaf[subject.OU] = \"$team\""
  for binary in codeconnect ccd cc-hook cc-tmux; do
    path="$work/mac/$dir_name/$binary"
    shipped_binary "$binary" "$path"
    # Notarized, as Apple's service confirms online: nothing can be stapled to a bare executable.
    verdict="$(spctl -a -vv -t install "$path" 2>&1)" || true
    if [[ "$verdict" != *"source=Notarized Developer ID"* ]]; then
      echo "::error::$binary is not notarized: $verdict"
      exit 1
    fi
  done
  # The pair a user receives agrees: the downloaded tmux is the one the downloaded codeconnect requires. Asked as
  # assignments, so a binary that does not run fails rather than answering with the same empty string as its partner.
  expected="$("$work/mac/$dir_name/codeconnect" --tmux-version)"
  said="$("$work/mac/$dir_name/cc-tmux" -V)"
  if [ -z "$expected" ] || [ "$said" != "$expected" ]; then
    echo "::error::the released cc-tmux reports '$said', the released codeconnect requires '$expected'"
    exit 1
  fi
  from_source "$work/mac/$dir_name/LICENSE" LICENSE
  for slice in $(lipo -archs "$work/mac/$dir_name/cc-tmux"); do
    lipo -thin "$slice" -output "$work/thin.cc-tmux.$slice" "$work/mac/$dir_name/cc-tmux"
    of_the_bundle "$work/mac/$dir_name/THIRD_PARTY_NOTICES" "$work/thin.cc-tmux.$slice"
  done

  # Linux: each checksum is signed by the key RELEASE_KEY pins — the proof the updater makes — each archive matches
  # its checksum and holds exactly what the updater allows, and each binary carries the pinned commit.
  public="$work/release-key.pem"
  printf '%b' "$(printf '302a300506032b6570032100%s' "$key" | sed 's/../\\x&/g')" \
    | "$OPENSSL" pkey -pubin -inform DER -out "$public"
  for arch in aarch64 x86_64; do
    core="codeconnect-$VERSION-linux-$arch.tar.gz"
    tmux="codeconnect-$VERSION-tmux-linux-$arch.tar.gz"
    for archive_name in "$core" "$tmux"; do
      if ! "$OPENSSL" pkeyutl -verify -rawin -pubin -inkey "$public" \
          -in "$archive_name.sha256" -sigfile "$archive_name.sha256.sig" > /dev/null; then
        echo "::error::$archive_name.sha256.sig does not verify against RELEASE_KEY"
        exit 1
      fi
      own_checksum "$archive_name"
    done
    holds_exactly "$core" "$dir_name/codeconnect" "$dir_name/ccd" "$dir_name/cc-hook" "$dir_name/LICENSE"
    holds_exactly "$tmux" "$dir_name/cc-tmux" "$dir_name/THIRD_PARTY_NOTICES"
    mkdir "$work/linux-$arch"
    tar -xzf "$core" -C "$work/linux-$arch"
    tar -xzf "$tmux" -C "$work/linux-$arch"
    for binary in codeconnect ccd cc-hook; do
      names_the_commit "$binary (linux $arch)" "$work/linux-$arch/$dir_name/$binary"
    done
    from_source "$work/linux-$arch/$dir_name/LICENSE" LICENSE
    of_the_bundle "$work/linux-$arch/$dir_name/THIRD_PARTY_NOTICES" "$work/linux-$arch/$dir_name/cc-tmux"
  done

  # The Mac app a user receives, from /mac or from Sparkle: the .dmg, its checksum and the update feed, checked as
  # Sparkle and Gatekeeper will check them.
  own_checksum "$dmg"
  OPENSSL="$OPENSSL" bash "$root/.github/workflows/mac-app-verify.sh" "$dmg" appcast.xml notarized
  # The feed is exactly the one appcast.sh writes for this release from the pinned notes, the tag's version, this
  # repository, the .dmg's size and the signature S1 just proved: written again here and compared byte for byte (as
  # files, so not even a trailing byte differs), so its url, versions, length and every other byte are this release's.
  # Its date is the one value taken from the feed itself, and appcast.sh takes only text in an RFC 822 date's shape.
  xmllint --noout appcast.xml
  signature="$(sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p' appcast.xml)"
  pub_date="$(sed -n 's|^ *<pubDate>\(.*\)</pubDate>$|\1|p' appcast.xml)"
  "$root/.github/workflows/appcast.sh" "$VERSION" "$GITHUB_REPOSITORY" "$root/macos/release-notes.txt" \
    "$pub_date" "$signature" "$(stat -f %z "$dmg")" > "$work/appcast.xml"
  if ! cmp -s appcast.xml "$work/appcast.xml"; then
    echo "::error::appcast.xml is not the feed this release writes"
    exit 1
  fi
  # The app is this release's: the tag's version, the pinned team, the Sparkle key the pinned project builds in — so
  # the signature S1 verified is by CodeConnect's key — and helpers that are this commit's, checked as the archives'
  # are. mac-app-verify.sh reads the key and the team from the app itself; these pin both to the source.
  sparkle_key="$(sed -n 's/.*SPARKLE_PUBLIC_KEY = "\([^"]*\)";.*/\1/p' \
    "$root/macos/CodeConnect.xcodeproj/project.pbxproj" | sort -u)"
  if [ -z "$sparkle_key" ] || [ "$(grep -c . <<<"$sparkle_key")" != 1 ]; then
    echo "::error::the project pins no single SPARKLE_PUBLIC_KEY"
    exit 1
  fi
  mkdir "$work/dmg"
  hdiutil attach -quiet -nobrowse -readonly -mountpoint "$work/dmg" "$dmg"
  app="$work/dmg/CodeConnect.app"
  for value in CFBundleShortVersionString CFBundleVersion; do
    if [ "$(/usr/libexec/PlistBuddy -c "Print :$value" "$app/Contents/Info.plist")" != "$VERSION" ]; then
      echo "::error::the app's $value is not $VERSION"
      exit 1
    fi
  done
  if [ "$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$app/Contents/Info.plist")" != "$sparkle_key" ]; then
    echo "::error::the app does not carry the project's Sparkle key"
    exit 1
  fi
  codesign --verify --strict -R="$req" "$app"
  for binary in codeconnect ccd cc-hook cc-tmux; do
    shipped_binary "$binary" "$app/Contents/Helpers/$binary"
  done
  # The Mac tmux archive's notices are the ones sealed in the signed app, the same build's, byte for byte; its first
  # line alone was checked above.
  if ! cmp -s "$work/mac/$dir_name/THIRD_PARTY_NOTICES" "$app/Contents/Resources/THIRD_PARTY_NOTICES"; then
    echo "::error::the Mac tmux archive's THIRD_PARTY_NOTICES are not the app's"
    exit 1
  fi
  hdiutil detach -quiet "$work/dmg"
  echo "the nineteen assets of $VERSION ($SHA) verified"
}

case "${1:-}" in
  fetch) fetch "$2" "$3" ;;
  check) check "$2" ;;
  *) echo "usage: release-verify.sh fetch <release id> <dir> | check <dir>" >&2; exit 2 ;;
esac
