#!/usr/bin/env bash
# Signs the Windows files of a draft release with a Certum code signing
# certificate on Linux, then starts release.yml, which checks them and the
# AppImage, signs the update manifests and publishes the release. Run
# packaging/linux/sign-release.sh first: the AppImage must be on the draft.
#
#     packaging/windows/sign-release.sh v0.2.0
#
# It downloads the unsigned programs that ci.yml built for the tag, signs
# them, builds the installer from the signed programs with Inno Setup under
# Wine, signs the installer and uploads all of them to the draft release.
#
# Needs gh (logged in, with write access to the repository), osslsigncode
# with libp11, Wine with Inno Setup 6, SimplySign Desktop logged in, and:
#
#   CERTUM_PKCS11_MODULE  the PKCS#11 module of SimplySign Desktop
#   CERTUM_KEY            the pkcs11: URI of the signing key
#   CERTUM_CERTS          a PEM file with the code signing certificate
#                         followed by Certum's intermediate certificate
#
# Optional:
#
#   CERTUM_PIN            the token PIN, if the module asks for one
#   CERTUM_PKCS11_ENGINE  libp11's pkcs11.so, if osslsigncode does not find it
#   CERTUM_TIMESTAMP_URL  the RFC 3161 server, http://time.certum.pl by default
#   ISCC                  ISCC.exe, by default Inno Setup 6 in $WINEPREFIX or ~/.wine
#   SCRIPTSCHNELL_RELEASE_REPO  owner/name, by default this checkout's repository
#
# crates/cmdterm-update/README.md ("Signing the Windows files") sets these up.

set -euo pipefail

fail() {
  echo "sign-release.sh: $*" >&2
  exit 1
}

platform=windows-x86_64
tag="${1:-}"
case "$tag" in
  v[0-9]*) ;;
  *) fail "usage: sign-release.sh v<version>" ;;
esac
version="${tag#v}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
timestamp="${CERTUM_TIMESTAMP_URL:-http://time.certum.pl}"
iscc="${ISCC:-${WINEPREFIX:-$HOME/.wine}/drive_c/Program Files (x86)/Inno Setup 6/ISCC.exe}"

for program in gh osslsigncode wine winepath; do
  command -v "$program" >/dev/null || fail "$program is not installed"
done
for variable in CERTUM_PKCS11_MODULE CERTUM_KEY CERTUM_CERTS; do
  [ -n "${!variable:-}" ] || fail "set $variable"
done
[ -r "$CERTUM_PKCS11_MODULE" ] || fail "cannot read $CERTUM_PKCS11_MODULE"
[ -r "$CERTUM_CERTS" ] || fail "cannot read $CERTUM_CERTS"
[ -f "$iscc" ] || fail "no Inno Setup at $iscc: install it under Wine or set ISCC"

repo="${SCRIPTSCHNELL_RELEASE_REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
draft="$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft)" ||
  fail "$repo has no release $tag yet: wait for ci.yml"
[ "$draft" = true ] || fail "$tag of $repo is published already"
gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name' |
  grep -qx linux-x86_64-scriptschnell-term.AppImage ||
  fail "the draft has no signed AppImage: run packaging/linux/sign-release.sh $tag first"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

sha="$(gh api "repos/$repo/commits/$tag" --jq .sha)"
run="$(gh run list --repo "$repo" --workflow ci.yml --event push --branch "$tag" --commit "$sha" \
  --status success --limit 1 --json databaseId --jq '.[0].databaseId')"
[ -n "$run" ] || fail "no successful ci.yml run for $tag"
gh run download "$run" --repo "$repo" --name "$platform-unsigned" --dir "$work/files"

signer=(osslsigncode sign -pkcs11module "$CERTUM_PKCS11_MODULE" -key "$CERTUM_KEY"
  -certs "$CERTUM_CERTS" -h sha256 -ts "$timestamp"
  -n scriptschnell-term -i https://codefionn.eu)
if [ -n "${CERTUM_PKCS11_ENGINE:-}" ]; then
  signer+=(-pkcs11engine "$CERTUM_PKCS11_ENGINE")
fi
if [ -n "${CERTUM_PIN:-}" ]; then
  # A file, so the PIN does not show up in the process list.
  (umask 077 && printf '%s' "$CERTUM_PIN" >"$work/pin")
  signer+=(-readpass "$work/pin")
fi

sign() {
  "${signer[@]}" -in "$1" -out "$1.signed"
  mv "$1.signed" "$1"
  echo "signed $(basename "$1")"
}

for file in "$work/files"/*; do
  sign "$file"
done

# Inno Setup signs its uninstaller only through a SignTool command that it
# runs itself under Wine, which osslsigncode cannot be, so the uninstaller
# inside stays unsigned. The installer is signed after the build.
mkdir -p "$work/out"
wine "$iscc" /Q "/DVersion=$version" \
  "/DSourceDir=$(winepath -w "$work/files")" \
  "/DOutputDir=$(winepath -w "$work/out")" \
  "$(winepath -w "$here/scriptschnell-term.iss")"
setup="$work/out/scriptschnell-term-$version-$platform-setup.exe"
[ -f "$setup" ] || fail "Inno Setup wrote no $(basename "$setup")"
sign "$setup"

mkdir -p "$work/upload"
for file in "$work/files"/*; do
  mv "$file" "$work/upload/$platform-$(basename "$file")"
done
mv "$setup" "$work/upload/"
gh release upload "$tag" --repo "$repo" --clobber "$work/upload"/*

gh workflow run release.yml --repo "$repo" --ref "$tag" -f tag="$tag"
echo "release.yml now checks and publishes $tag:"
echo "  gh run list --repo $repo --workflow release.yml --limit 1"
