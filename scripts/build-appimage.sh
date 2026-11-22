#!/usr/bin/env bash
# Builds the Linux AppImage locally, the same way the release workflow does.
#
#     scripts/build-appimage.sh [out file]
#
# The default out file is dist/linux-x86_64/scriptschnell-term.AppImage. The
# version comes from [workspace.package] in Cargo.toml. APPIMAGETOOL names the
# appimagetool binary; without it the script uses appimagetool from PATH, or
# downloads it once into ~/.cache/scriptschnell-term-build. The AppImage links
# against this machine's glibc, so it may not run on older distributions than
# the one it was built on (CI builds on Ubuntu 22.04 for that reason).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

out="${1:-$root/dist/linux-x86_64/scriptschnell-term.AppImage}"
version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)"
if [ -z "$version" ]; then
  echo "no version in [workspace.package] of Cargo.toml" >&2
  exit 1
fi

if [ -z "${APPIMAGETOOL:-}" ]; then
  if command -v appimagetool >/dev/null; then
    APPIMAGETOOL="$(command -v appimagetool)"
  else
    cache="${XDG_CACHE_HOME:-$HOME/.cache}/scriptschnell-term-build"
    APPIMAGETOOL="$cache/appimagetool-x86_64.AppImage"
    if [ ! -x "$APPIMAGETOOL" ]; then
      mkdir -p "$cache"
      echo "downloading appimagetool to $APPIMAGETOOL" >&2
      curl -fsSL -o "$APPIMAGETOOL.part" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
      chmod +x "$APPIMAGETOOL.part"
      mv "$APPIMAGETOOL.part" "$APPIMAGETOOL"
    fi
  fi
fi
export APPIMAGETOOL
# appimagetool is itself an AppImage; extracting it avoids needing FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1

# No --locked: the sibling checkouts often move ahead of Cargo.lock locally.
cargo build --release \
  --bin scriptschnell-term --bin cmdterm-remote --bin cmdterm-proxy-bridge

mkdir -p "$(dirname "$out")"
packaging/linux/build-appimage.sh target/release "$version" "$out"
echo "built $out (version $version)"
