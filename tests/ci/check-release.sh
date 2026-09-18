#!/usr/bin/env bash
#
# Checks a published release against what its own notes claim.
#
# This exists because four releases in a row (1.3.6 through 1.3.9) told people the SHA-256 of each
# installer was "published below", and it was not - the build appends that block to the draft, and
# writing the release notes over the draft body threw it away. Nobody noticed for weeks. The notes
# are the only thing standing behind an unsigned installer, so a release that promises a checksum
# and omits it gives up the one assurance there is.
#
# It checks promises rather than imposing a house style: a release that says nothing about
# checksums is not required to publish any, but one that mentions SHA-256 must name the hash of
# every asset it ships. The hashes are computed from the downloaded assets, never read out of
# SHA256SUMS.txt - a value copied from the file it is meant to corroborate corroborates nothing.
#
#   tests/ci/check-release.sh <owner/repo> <tag>
#
# Needs gh authenticated (GH_TOKEN on a runner). Both editions run this on their own releases; the
# PowerShell one checks out this directory to get it, the way it already does for the test servers.

set -euo pipefail

repo=${1:?usage: check-release.sh <owner/repo> <tag>}
tag=${2:?usage: check-release.sh <owner/repo> <tag>}
fail=0
say() { printf '  %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; fail=1; }

body=$(gh release view "$tag" --repo "$repo" --json body --jq .body)
mapfile -t assets < <(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name')

# 1. A release that ships nothing is a mistake in every case these repositories have.
if [ ${#assets[@]} -eq 0 ]; then
  bad "$tag has no assets"
else
  say "$tag ships ${#assets[@]} asset(s): ${assets[*]}"
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
[ ${#assets[@]} -gt 0 ] && gh release download "$tag" --repo "$repo" --dir "$dir" --clobber

# 2. Whatever the notes promise about checksums has to be there. The checksum file itself is
#    exempt: nothing can carry its own hash.
if printf '%s' "$body" | grep -qiE 'sha-?256'; then
  say "the notes mention SHA-256, so every asset's hash has to be in them"
  for a in "${assets[@]}"; do
    [ "$a" = SHA256SUMS.txt ] && continue
    hash=$(sha256sum "$dir/$a" | cut -d' ' -f1)
    if printf '%s' "$body" | grep -qiF "$hash"; then
      say "ok    $a  $hash"
    else
      bad "$a hashes $hash, which the notes do not name"
    fi
  done
else
  say "the notes claim nothing about checksums, so none is required of them"
fi

# 3. And if a checksum file is shipped, it has to describe the files as published. GitHub rewrites
#    spaces in an asset name to dots, which is a way this has been wrong before.
if [ -f "$dir/SHA256SUMS.txt" ]; then
  while read -r want name; do
    [ -z "${name:-}" ] && continue
    if [ ! -f "$dir/$name" ]; then
      bad "SHA256SUMS.txt lists $name, which is not an asset of this release"
    elif [ "$(sha256sum "$dir/$name" | cut -d' ' -f1)" != "$(printf '%s' "$want" | tr 'A-Z' 'a-z')" ]; then
      bad "SHA256SUMS.txt disagrees with the file it names: $name"
    else
      say "ok    SHA256SUMS.txt matches $name"
    fi
  done < <(tr -d '\r' < "$dir/SHA256SUMS.txt")
fi

# 4. A relative link in a release body resolves against the release's own URL, so [x](CODE_SIGNING.md)
#    lands on /releases/tag/CODE_SIGNING.md and 404s. Checked with GitHub's own renderer before this
#    was written; it leaves the href relative.
links=$(printf '%s' "$body" | grep -oE '\]\([^)]+\)' | sed -E 's/^\]\(//; s/\)$//' || true)
if [ -n "$links" ]; then
  while read -r l; do
    [ -z "$l" ] && continue
    case "$l" in
      http://*|https://*|mailto:*|'#'*) ;;
      *) bad "relative link in the notes, which a release page cannot resolve: $l" ;;
    esac
  done <<< "$links"
fi

if [ "$fail" -ne 0 ]; then
  printf '\n  the notes of %s do not describe what it ships - edit the release body\n' "$tag"
  exit 1
fi
printf '\n  %s says nothing about itself that is not true\n' "$tag"
