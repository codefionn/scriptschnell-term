#!/usr/bin/env bash
# Checks the GPG signature embedded in an AppImage against the release key.
#
#     packaging/linux/verify-appimage.sh <AppImage> [key file]
#
# The key file defaults to appimage-signing-key.asc next to this script. The
# signature must verify against it, and the public key embedded in the
# AppImage must have the same primary key. sign-appimage.sh and the release
# workflow run this; users can too. It needs gpg and builds
# scriptschnell-term-release with cargo unless SCRIPTSCHNELL_RELEASE_TOOL
# names it. The keyring it uses is a temporary one, never ~/.gnupg.
set -euo pipefail

fail() {
  echo "verify-appimage.sh: $*" >&2
  exit 1
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
appimage="${1:-}"
keyfile="${2:-$here/appimage-signing-key.asc}"
[ -f "$appimage" ] || fail "usage: verify-appimage.sh <AppImage> [key file]"
[ -r "$keyfile" ] || fail "cannot read $keyfile"
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

work="$(mktemp -d)"
trap 'gpgconf --homedir "$work/gnupg" --kill all 2>/dev/null || true; rm -rf "$work"' EXIT
mkdir -m 700 "$work/gnupg"
export GNUPGHOME="$work/gnupg"

"$release_tool" appimage-signature --signature "$work/signature.asc" --key "$work/key.asc" "$appimage"
# The signed data is the hex digest without a newline.
printf '%s' "$("$release_tool" appimage-digest "$appimage")" >"$work/digest"

primary() {
  gpg --batch --quiet --with-colons --show-keys "$1" | awk -F: '$1 == "fpr" { print $10; exit }'
}
expected="$(primary "$keyfile")"
embedded="$(primary "$work/key.asc")"
[ -n "$expected" ] || fail "$keyfile holds no public key"
[ "$embedded" = "$expected" ] ||
  fail "$appimage embeds the key ${embedded:-(none)}, not $expected"

gpg --batch --quiet --import "$keyfile"
gpg --batch --status-fd 3 --verify "$work/signature.asc" "$work/digest" 3>"$work/status" 2>/dev/null ||
  fail "the signature of $appimage does not verify against $keyfile"
# GOODSIG only for a key that has not expired or been revoked; VALIDSIG ends
# with the primary key's fingerprint.
grep -q '^\[GNUPG:\] GOODSIG ' "$work/status" ||
  fail "the signature of $appimage is not good: $(grep '^\[GNUPG:\]' "$work/status" | tr '\n' ' ')"
signer="$(awk '$2 == "VALIDSIG" { print $NF }' "$work/status")"
[ "$signer" = "$expected" ] || fail "$appimage is signed by ${signer:-(nobody)}, not $expected"
echo "$(basename "$appimage"): good signature from $expected"
