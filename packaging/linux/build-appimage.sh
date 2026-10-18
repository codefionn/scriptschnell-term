#!/usr/bin/env bash
# Packs release binaries into an AppImage.
#
#     packaging/linux/build-appimage.sh <target dir> <version> <out file>
#
# <target dir> holds scriptschnell-term, cmdterm-remote and
# cmdterm-proxy-bridge from `cargo build --release`. APPIMAGETOOL names the
# appimagetool binary (default: appimagetool on PATH). fontconfig and
# freetype are the only libraries the app links, and every desktop has them,
# so the AppImage bundles none.
set -euo pipefail

target_dir=$1
version=$2
out=$3
here="$(cd "$(dirname "$0")" && pwd)"
appdir="$(mktemp -d)/AppDir"
trap 'rm -rf "$(dirname "$appdir")"' EXIT

mkdir -p "$appdir/usr/bin" \
  "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/scalable/apps"
for bin in scriptschnell-term cmdterm-remote cmdterm-proxy-bridge; do
  install -m 755 "$target_dir/$bin" "$appdir/usr/bin/$bin"
done
install -m 755 "$here/AppRun" "$appdir/AppRun"
install -m 644 "$here/scriptschnell-term.desktop" "$appdir/scriptschnell-term.desktop"
install -m 644 "$here/scriptschnell-term.desktop" "$appdir/usr/share/applications/"
install -m 644 "$here/scriptschnell-term.svg" "$appdir/scriptschnell-term.svg"
install -m 644 "$here/scriptschnell-term.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/"
ln -s scriptschnell-term.svg "$appdir/.DirIcon"

ARCH=x86_64 VERSION="$version" "${APPIMAGETOOL:-appimagetool}" --no-appstream "$appdir" "$out"
