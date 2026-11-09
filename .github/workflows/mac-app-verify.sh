#!/usr/bin/env bash
# The Mac app's .dmg, checked as Sparkle and Gatekeeper will check it, each check beside the must-fail that shows it
# can fail: a check that passes everything certifies nothing.
#
#   mac-app-verify.sh <CodeConnect-X.Y.Z.dmg> <appcast-item.xml> [notarized]
#
# S1: the SUPublicEDKey baked into the app inside the .dmg verifies the .dmg's Sparkle signature (appcast-item.xml,
#     from sign_update). A wrong key here would strand every install, so it is checked against the built app, never
#     against the secret. Must-fail: a copy of the .dmg with one byte changed, and a throwaway key, are refused.
# S2: the app passes `codesign --verify --deep --strict`; with `notarized` (the release job), Gatekeeper says
#     "Notarized Developer ID" and the .dmg's staple validates. Must-fail: a copy of the app with one byte of its
#     executable changed fails codesign.
#     With `notarized`, each helper the app installs is signed as itself (`identifier "ccd"` …) by the app's own
#     team's Developer ID: macOS keeps a permission a user granted ccd against that identity, identifier included.
#     Must-fail: the app's codeconnect, checked as ccd, is refused.
#     The app can start: every @rpath library its executable loads resolves, through the executable's own LC_RPATHs,
#     to a file inside the app, as dyld resolves it at launch. Must-fail: a copy without that rpath is refused.
#
# OPENSSL names an OpenSSL 3 or later (Ed25519 with -rawin); its version is printed.
set -euo pipefail
dmg=$1
item=$2
notarized=${3:-}
openssl=${OPENSSL:-openssl}
work=$(mktemp -d)
mnt="$work/mnt"
cleanup() {
  hdiutil detach -quiet "$mnt" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

"$openssl" version
# One byte of a file, inverted, in a copy.
changed() { cp -R "$1" "$2" && python3 -c 'import sys; p=sys.argv[1]; b=bytearray(open(p,"rb").read()); b[len(b)//2]^=0xFF; open(p,"wb").write(b)' "$3"; }

hdiutil attach -quiet -nobrowse -readonly -mountpoint "$mnt" "$dmg"
app="$mnt/CodeConnect.app"

echo "== S1: the app's own key verifies the .dmg"
key=$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$app/Contents/Info.plist")
signature=$(sed -n 's/.*sparkle:edSignature="\([^"]*\)".*/\1/p' "$item")
[ -n "$key" ] && [ -n "$signature" ] || { echo "::error::no SUPublicEDKey in the app, or no edSignature in $item"; exit 1; }
# An Ed25519 public key in DER: the algorithm's fixed prefix, then the key's 32 bytes.
der() { { printf '\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00'; base64 --decode <<<"$1"; } > "$2"; }
der "$key" "$work/app.pub"
base64 --decode <<<"$signature" > "$work/dmg.sig"
verifies() { "$openssl" pkeyutl -verify -pubin -keyform DER -inkey "$1" -rawin -in "$2" -sigfile "$work/dmg.sig" >/dev/null 2>&1; }
verifies "$work/app.pub" "$dmg" || { echo "::error::S1: the app's SUPublicEDKey does not verify the .dmg's signature"; exit 1; }
echo "S1: verified with the app's key $key"
changed "$dmg" "$work/changed.dmg" "$work/changed.dmg"
if verifies "$work/app.pub" "$work/changed.dmg"; then echo "::error::S1 must-fail: a changed .dmg verified"; exit 1; fi
"$openssl" genpkey -algorithm ed25519 -outform DER -out "$work/other.key"
"$openssl" pkey -inform DER -in "$work/other.key" -pubout -outform DER -out "$work/other.pub"
if verifies "$work/other.pub" "$dmg"; then echo "::error::S1 must-fail: another key verified the .dmg"; exit 1; fi
echo "S1 must-fail: a changed .dmg and another key are both refused"

echo "== S2: what Gatekeeper accepts"
codesign --verify --deep --strict --verbose=2 "$app"
changed "$app" "$work/changed.app" "$work/changed.app/Contents/MacOS/CodeConnect"
if codesign --verify --deep --strict "$work/changed.app" 2>/dev/null; then echo "::error::S2 must-fail: a changed app passed codesign"; exit 1; fi
echo "S2 must-fail: a changed app fails codesign"
# resolves <app>: every @rpath library of its executable is a file inside it, through the executable's LC_RPATHs.
resolves() {
  local exe="$1/Contents/MacOS/CodeConnect" rpaths lib r found
  rpaths=$(otool -l "$exe" | awk '/cmd LC_RPATH/ {f=1} f && $1 == "path" {print $2; f=0}')
  for lib in $(otool -L "$exe" | awk 'NR > 1 && $1 ~ /^@rpath\// {print $1}'); do
    found=
    for r in $rpaths; do
      r=${r//@executable_path/$1/Contents/MacOS}
      r=${r//@loader_path/$1/Contents/MacOS}
      case $r in "$1"/*) [ -e "$r/${lib#@rpath/}" ] && found=1 ;; esac
    done
    [ -n "$found" ] || { echo "$lib does not resolve inside the app"; return 1; }
  done
}
resolves "$app" || { echo "::error::S2: the app cannot start: a library it loads is not found inside it"; exit 1; }
cp -R "$app" "$work/norpath.app"
install_name_tool -delete_rpath @executable_path/../Frameworks "$work/norpath.app/Contents/MacOS/CodeConnect"
if resolves "$work/norpath.app" >/dev/null; then echo "::error::S2 must-fail: an app without its rpath passed"; exit 1; fi
echo "S2: every library the app loads is inside it; must-fail: a copy without its rpath is refused"
if [ "$notarized" = notarized ]; then
  verdict=$(spctl -a -t exec -vv "$app" 2>&1)
  echo "$verdict"
  grep -q "source=Notarized Developer ID" <<<"$verdict" || { echo "::error::S2: Gatekeeper did not accept the app as Notarized Developer ID"; exit 1; }
  xcrun stapler validate "$dmg"
  team=$(codesign -dv "$app" 2>&1 | sed -n 's/^TeamIdentifier=//p')
  signed_as() {
    codesign --verify --strict --all-architectures -R="identifier \"$1\" and anchor apple generic \
      and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists \
      and certificate leaf[subject.OU] = \"$team\"" "$2"
  }
  for name in ccd codeconnect cc-hook cc-tmux; do
    signed_as "$name" "$app/Contents/Helpers/$name" || { echo "::error::S2: the app's $name is not signed as $name by team $team"; exit 1; }
  done
  if signed_as ccd "$app/Contents/Helpers/codeconnect" 2>/dev/null; then echo "::error::S2 must-fail: codeconnect passed as ccd"; exit 1; fi
  echo "S2: each helper the app installs is signed as itself by team $team; must-fail: codeconnect checked as ccd is refused"
fi
echo "S1 and S2 passed"
