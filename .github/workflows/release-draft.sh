#!/usr/bin/env bash
# The one draft that holds a release, found by release.yml's draft and promote jobs, its numeric id written to
# $GITHUB_OUTPUT as `id`. A draft is invisible to the get-release-by-tag endpoint, so it is found in the
# authenticated listing, which does include drafts.
#
#   TAG SHA VERSION PROMOTING=true|false GH_TOKEN GITHUB_REPOSITORY  release-draft.sh
#
# The draft job (PROMOTING=false) adopts the draft or creates it. The promote job (PROMOTING=true) only finds it:
# publication promotes a draft a draft run built and verified, and a promote run that finds none must not become a
# create-and-publish run.
set -euo pipefail

# Before anything is created, adopted, or promoted: the tag must still name the pinned commit. Adoption deletes and
# replaces a draft's assets, and destroying a valid draft over a moved tag is damage even when a later check would
# have refused it.
git fetch --no-tags --depth=1 origin "refs/tags/$TAG"
at="$(git rev-parse "FETCH_HEAD^{commit}")"
if [ "$at" != "$SHA" ]; then
  echo "::error::$TAG now resolves to $at, not the pinned $SHA; refusing"
  exit 1
fi

# `gh api` exits non-zero on any API failure, which fails this step: an unreadable listing is not permission to
# create anything. A fresh draft takes a few seconds to appear in the listing, so an empty answer is re-asked before
# it is believed — otherwise an immediate rerun would try to recreate a draft that exists and die on the collision.
find_matches() {
  gh api --paginate "repos/$GITHUB_REPOSITORY/releases?per_page=100" \
    --jq "[.[] | select(.tag_name == \"$TAG\")]" | jq -s 'add'
}
matches="$(find_matches)"
for _ in 1 2 3; do
  [ "$(jq 'length' <<<"$matches")" != "0" ] && break
  sleep 5
  matches="$(find_matches)"
done
count="$(jq 'length' <<<"$matches")"
case "$count" in
  0)
    if [ "$PROMOTING" = "true" ]; then
      echo "::error::no draft holds $TAG; run the workflow without publish first"
      exit 1
    fi
    id="$(gh api -X POST "repos/$GITHUB_REPOSITORY/releases" \
      -f tag_name="$TAG" -F draft=true -F generate_release_notes=true \
      -f name="CodeConnect $VERSION" --jq .id)"
    ;;
  1)
    if [ "$(jq -r '.[0].draft' <<<"$matches")" != "true" ]; then
      echo "::error::$TAG is already published; this workflow will not modify a live release"
      exit 1
    fi
    id="$(jq -r '.[0].id' <<<"$matches")"
    ;;
  *)
    echo "::error::$count releases carry the tag $TAG; refusing to guess which"
    exit 1
    ;;
esac
echo "id=$id" >> "$GITHUB_OUTPUT"
echo "draft id $id holds $TAG"
