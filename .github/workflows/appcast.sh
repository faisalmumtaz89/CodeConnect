#!/usr/bin/env bash
# The Mac app's update feed (Sparkle's appcast) for one release: one item, this release's, and no URL but the
# versioned .dmg it names and the feed's own address — Sparkle fetches any other link it is given as soon as it shows
# an update, before the person accepts anything. Written from the release's own values, so nothing in it is typed by
# hand but the notes, which are macos/release-notes.txt, the text Sparkle's update window shows.
#
#   appcast.sh VERSION REPOSITORY NOTES_FILE PUB_DATE SIGNATURE LENGTH
#
# release.yml's draft job writes appcast.xml with it from `sign_update`'s output; release-verify.sh writes it again from
# the pinned source and the .dmg, and requires the two to be the same, byte for byte. codeconnect.sh serves the file
# unchanged at /mac/appcast.xml, the address every copy of the app asks.
set -euo pipefail

version="$1" repository="$2" notes_file="$3" pub_date="$4" signature="$5" length="$6"
if ! [[ $pub_date =~ ^[A-Z][a-z]{2},\ [0-9]{2}\ [A-Z][a-z]{2}\ [0-9]{4}\ [0-9]{2}:[0-9]{2}:[0-9]{2}\ \+0000$ ]] \
    || ! [[ $signature =~ ^[A-Za-z0-9+/]+=*$ ]] || ! [[ $length =~ ^[1-9][0-9]*$ ]]; then
  echo "::error::appcast.sh: '$pub_date', '$signature' or '$length' is not an RFC 822 UTC date, a base64 signature and a size" >&2
  exit 1
fi
# The notes as one HTML paragraph: escaped, so they can carry no markup — no image or link Sparkle would fetch — and
# no `]]>` that would end the CDATA.
notes="$(sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' "$notes_file" | tr '\n' ' ' | sed -e 's/  */ /g' -e 's/^ //' -e 's/ $//')"
if [ -z "$notes" ]; then
  echo "::error::$notes_file holds no release notes" >&2
  exit 1
fi
cat <<FEED
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>CodeConnect</title>
    <link>https://codeconnect.sh/mac/appcast.xml</link>
    <description>Updates to the CodeConnect app for Mac.</description>
    <language>en</language>
    <item>
      <title>CodeConnect $version</title>
      <pubDate>$pub_date</pubDate>
      <sparkle:version>$version</sparkle:version>
      <sparkle:shortVersionString>$version</sparkle:shortVersionString>
      <description><![CDATA[<p>$notes</p>]]></description>
      <enclosure url="https://github.com/$repository/releases/download/v$version/CodeConnect-$version.dmg" type="application/octet-stream" sparkle:edSignature="$signature" length="$length" />
    </item>
  </channel>
</rss>
FEED
