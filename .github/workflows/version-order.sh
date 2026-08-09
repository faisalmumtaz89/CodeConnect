# shellcheck shell=bash
# Sourced by release.yml wherever versions must be ordered.
#
# **No shell arithmetic.** Bash `(( ))` is signed 64-bit, so a component past
# 2^63 wraps negative and a mathematically older version can read as newer.
# The updater parses components as u64 — this comparator must never disagree
# with it, so digits are compared as decimals: with leading zeroes refused,
# the longer run of digits is the larger number, and equal lengths compare
# lexicographically. Components are capped at 19 digits, inside u64 — the
# updater refuses anything it cannot parse as u64, so a producer stricter
# than the updater can never publish a release the updater cannot order.

# valid_version X.Y.Z — plain semver, no v, no leading zeroes, u64-sized.
valid_version() {
  [[ "$1" =~ ^(0|[1-9][0-9]{0,18})\.(0|[1-9][0-9]{0,18})\.(0|[1-9][0-9]{0,18})$ ]]
}

# digits_greater A B — decimal comparison of two non-negative integers
# written without leading zeroes.
digits_greater() {
  if [ "${#1}" -ne "${#2}" ]; then
    [ "${#1}" -gt "${#2}" ]
  else
    [[ "$1" > "$2" ]]
  fi
}

# version_strictly_newer CANDIDATE PREVIOUS — both plain X.Y.Z. True only
# when CANDIDATE is a strict step forward; anything unorderable is false,
# and the caller refuses.
version_strictly_newer() {
  local candidate="$1" previous="$2"
  valid_version "$candidate" || {
    echo "::error::'$candidate' is not a version this can order" >&2
    return 1
  }
  valid_version "$previous" || {
    echo "::error::'$previous' is not a version this can order" >&2
    return 1
  }
  # `|| return 1` on each read, explicitly: callers invoke this inside `if`,
  # which suppresses errexit, and a failed second read would otherwise leave
  # its components empty — and an empty component loses every length
  # comparison, which is the fail-open direction.
  local cM cm cp pM pm pp
  IFS=. read -r cM cm cp <<<"$candidate" || return 1
  IFS=. read -r pM pm pp <<<"$previous" || return 1
  if digits_greater "$cM" "$pM"; then return 0; fi
  if digits_greater "$pM" "$cM"; then return 1; fi
  if digits_greater "$cm" "$pm"; then return 0; fi
  if digits_greater "$pm" "$cm"; then return 1; fi
  digits_greater "$cp" "$pp"
}
