#!/usr/bin/env bash
# Run by release.yml's `linux-sign` job: sign every Linux checksum file in <dir>
# with the release key, writing <file>.sha256.sig beside it.
#
#   linux-sign.sh <dir> <private key PEM> <update_install.rs of the pinned commit>
#
# **Only with the key the updater trusts.** The public half of the key is
# compared with `RELEASE_KEY` in the source being released first: a release
# signed by any other key is one every installed updater refuses, so it is
# refused here instead, before anything is signed. Each signature is then
# checked against that pinned public half alone, and a checksum is signed only
# once its archive is proven to match it.
set -euo pipefail

dir="$1"
key="$2"
source="$3"

pinned="$(sed -n '/^pub const RELEASE_KEY/,/^];/p' "$source" \
  | grep -o '0x[0-9a-f][0-9a-f]' | sed 's/^0x//' | tr -d '\n')"
if [ "${#pinned}" != 64 ]; then
  echo "::error::$source pins no 32-byte RELEASE_KEY"
  exit 1
fi
held="$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')"
if [ "$held" != "$pinned" ]; then
  echo "::error::the signing key is not the key the updater trusts (RELEASE_KEY); nothing was signed"
  exit 1
fi

# The pinned public half as a key file: the DER prefix of an Ed25519 public key,
# then its 32 bytes.
public="$(mktemp)"
trap 'rm -f "$public"' EXIT
printf '%b' "$(printf '302a300506032b6570032100%s' "$pinned" | sed 's/../\\x&/g')" \
  | openssl pkey -pubin -inform DER -out "$public"

signed=0
for sum in "$dir"/*.sha256; do
  (cd "$dir" && sha256sum --check --quiet "$(basename "$sum")")
  openssl pkeyutl -sign -rawin -inkey "$key" -in "$sum" -out "$sum.sig"
  if [ "$(wc -c < "$sum.sig")" -ne 64 ]; then
    echo "::error::$sum.sig is not a 64-byte Ed25519 signature"
    exit 1
  fi
  openssl pkeyutl -verify -rawin -pubin -inkey "$public" -in "$sum" -sigfile "$sum.sig" > /dev/null
  signed=$((signed + 1))
done
# Two architectures, each a core and a tmux archive.
if [ "$signed" -ne 4 ]; then
  echo "::error::signed $signed checksum files, expected 4"
  exit 1
fi
echo "signed $signed checksum files with the key RELEASE_KEY pins"
