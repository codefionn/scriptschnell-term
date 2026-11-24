#!/usr/bin/env bash
# Packs release binaries into an AppImage.
#
#     packaging/linux/build-appimage.sh <target dir> <version> <out file>
#
# <target dir> holds scriptschnell-term, scriptschnell-term-remote and
# scriptschnell-term-proxy-bridge from `cargo build --release`. APPIMAGETOOL names the
# appimagetool binary (default: appimagetool on PATH). APPIMAGE_RUNTIME names
# the type2 runtime file; without it appimagetool downloads the runtime
# itself. fontconfig and
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
  "$appdir/usr/share/icons/hicolor/scalable/apps" \
  "$appdir/usr/share/icons/hicolor/256x256/apps"
for bin in scriptschnell-term scriptschnell-term-remote scriptschnell-term-proxy-bridge; do
  install -m 755 "$target_dir/$bin" "$appdir/usr/bin/$bin"
done
install -m 755 "$here/AppRun" "$appdir/AppRun"
install -m 644 "$here/scriptschnell-term.desktop" "$appdir/scriptschnell-term.desktop"
install -m 644 "$here/scriptschnell-term.desktop" "$appdir/usr/share/applications/"
# appimagetool wants a PNG for .DirIcon. scriptschnell-term.png is the SVG
# rendered at 256x256 (rsvg-convert -w 256 -h 256); render it again after
# changing the SVG.
install -m 644 "$here/scriptschnell-term.png" "$appdir/scriptschnell-term.png"
install -m 644 "$here/scriptschnell-term.png" "$appdir/usr/share/icons/hicolor/256x256/apps/"
install -m 644 "$here/scriptschnell-term.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/"
ln -s scriptschnell-term.png "$appdir/.DirIcon"

runtime_args=()
if [ -n "${APPIMAGE_RUNTIME:-}" ]; then
  runtime_args=(--runtime-file "$APPIMAGE_RUNTIME")
fi
ARCH=x86_64 VERSION="$version" "${APPIMAGETOOL:-appimagetool}" --no-appstream \
  "${runtime_args[@]}" "$appdir" "$out"
