#!/usr/bin/env bash
# Signs the AppImage of a draft release with GPG on this machine and uploads
# it to the draft. Run it before packaging/windows/sign-release.sh, which
# starts release.yml; that workflow checks the AppImage too, signs the update
# manifests and publishes the release.
#
#     packaging/linux/sign-release.sh v0.2.0
#
# It downloads the unsigned AppImage that ci.yml built for the tag, signs it
# with sign-appimage.sh (APPIMAGE_SIGNING_KEY, the release key by default,
# which may sit on a security key) and uploads it as
# linux-x86_64-scriptschnell-term.AppImage.
#
# Needs gh (logged in, with write access to the repository), gpg with the
# secret key, and cargo. SCRIPTSCHNELL_RELEASE_REPO names owner/name, by
# default this checkout's repository.
#
# crates/cmdterm-update/README.md ("Signing the AppImage") explains the key.

set -euo pipefail

fail() {
  echo "sign-release.sh: $*" >&2
  exit 1
}

platform=linux-x86_64
name=scriptschnell-term.AppImage
tag="${1:-}"
case "$tag" in
  v[0-9]*) ;;
  *) fail "usage: sign-release.sh v<version>" ;;
esac
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for program in gh gpg cargo; do
  command -v "$program" >/dev/null || fail "$program is not installed"
done

repo="${SCRIPTSCHNELL_RELEASE_REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
draft="$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft)" ||
  fail "$repo has no release $tag yet: wait for ci.yml"
[ "$draft" = true ] || fail "$tag of $repo is published already"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

sha="$(gh api "repos/$repo/commits/$tag" --jq .sha)"
run="$(gh run list --repo "$repo" --workflow ci.yml --event push --branch "$tag" --commit "$sha" \
  --status success --limit 1 --json databaseId --jq '.[0].databaseId')"
[ -n "$run" ] || fail "no successful ci.yml run for $tag"
gh run download "$run" --repo "$repo" --name "$platform-unsigned" --dir "$work/files"
[ -f "$work/files/$name" ] || fail "the $platform-unsigned artifact has no $name"

"$here/sign-appimage.sh" "$work/files/$name"

mv "$work/files/$name" "$work/$platform-$name"
gh release upload "$tag" --repo "$repo" --clobber "$work/$platform-$name"
echo "uploaded $platform-$name to $tag; now run packaging/windows/sign-release.sh $tag"
