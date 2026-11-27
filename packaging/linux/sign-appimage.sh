#!/usr/bin/env bash
# Signs an AppImage in place with GPG, the way appimagetool --sign does.
#
#     packaging/linux/sign-appimage.sh <AppImage>
#
# The signature covers the hex SHA-256 of the AppImage with its signature
# sections zeroed. It goes into the .sha256_sig section, and the public key
# into .sig_key, where AppImageUpdate's validate and AppImageLauncher look
# for them. No other byte changes, so the release workflow can check that the
# signed AppImage is the one CI built.
#
# APPIMAGE_SIGNING_KEY names the key gpg signs with, by default the release
# key BC070532FE313455FAFDDFA4D826169BC45EC7A9. gpg picks its signing subkey;
# on a security key it waits for the PIN and a touch. The script checks the
# result with verify-appimage.sh against appimage-signing-key.asc, so a
# different key needs APPIMAGE_SIGNING_KEYFILE with its public key.
# SCRIPTSCHNELL_RELEASE_TOOL names scriptschnell-term-release; without it the
# script builds it with cargo.
set -euo pipefail

fail() {
  echo "sign-appimage.sh: $*" >&2
  exit 1
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
appimage="${1:-}"
key="${APPIMAGE_SIGNING_KEY:-BC070532FE313455FAFDDFA4D826169BC45EC7A9}"
keyfile="${APPIMAGE_SIGNING_KEYFILE:-$here/appimage-signing-key.asc}"
[ -f "$appimage" ] || fail "usage: sign-appimage.sh <AppImage>"
command -v gpg >/dev/null || fail "gpg is not installed"

# The scriptschnell-term-release of this checkout, built when
# SCRIPTSCHNELL_RELEASE_TOOL does not name one.
if [ -z "${SCRIPTSCHNELL_RELEASE_TOOL:-}" ]; then
  root="$(cd "$here/../.." && pwd)"
  cargo build --quiet --release --locked --manifest-path "$root/Cargo.toml" \
    -p cmdterm-update --bin scriptschnell-term-release
  SCRIPTSCHNELL_RELEASE_TOOL="${CARGO_TARGET_DIR:-$root/target}/release/scriptschnell-term-release"
fi
release_tool="$SCRIPTSCHNELL_RELEASE_TOOL"

export SCRIPTSCHNELL_RELEASE_TOOL
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

printf '%s' "$("$release_tool" appimage-digest --unsigned "$appimage")" >"$work/digest"
gpg --armor --export --export-options export-minimal "$key" >"$work/key.asc"
[ -s "$work/key.asc" ] || fail "gpg has no public key $key"
echo "signing $(basename "$appimage") with $key" >&2
gpg --local-user "$key" --armor --textmode --detach-sign --output "$work/signature.asc" "$work/digest"
"$release_tool" appimage-embed --signature "$work/signature.asc" --key "$work/key.asc" "$appimage"
"$here/verify-appimage.sh" "$appimage" "$keyfile"
