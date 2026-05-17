#!/usr/bin/env bash
# Re-render the atelier app icon from assets/icon.svg into the Tauri icons dir.
# Produces: macOS .icns (Cmd+Tab / Dock / Finder), Windows .ico, and the
# standard Tauri PNG set. Requires rsvg-convert, iconutil, magick.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$ROOT/assets/icon.svg"
ICONS="$ROOT/crates/atelier-gui/icons"
ICONSET="$(mktemp -d)/atelier-icon.iconset"
ICO_TMP="$(mktemp -d)"

mkdir -p "$ICONSET" "$ICONS"

render() { rsvg-convert -w "$1" -h "$1" -f png "$SVG" -o "$2"; }

# macOS .icns
render 16   "$ICONSET/icon_16x16.png"
render 32   "$ICONSET/icon_16x16@2x.png"
render 32   "$ICONSET/icon_32x32.png"
render 64   "$ICONSET/icon_32x32@2x.png"
render 128  "$ICONSET/icon_128x128.png"
render 256  "$ICONSET/icon_128x128@2x.png"
render 256  "$ICONSET/icon_256x256.png"
render 512  "$ICONSET/icon_256x256@2x.png"
render 512  "$ICONSET/icon_512x512.png"
render 1024 "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$ICONS/icon.icns"

# Tauri PNG set
render 1024 "$ICONS/icon.png"
render 32   "$ICONS/32x32.png"
render 128  "$ICONS/128x128.png"
render 256  "$ICONS/128x128@2x.png"

# Windows .ico (multi-res). Rasterise first so we don't need Ghostscript.
for s in 16 32 48 64 128 256; do render "$s" "$ICO_TMP/$s.png"; done
magick "$ICO_TMP/16.png" "$ICO_TMP/32.png" "$ICO_TMP/48.png" \
       "$ICO_TMP/64.png" "$ICO_TMP/128.png" "$ICO_TMP/256.png" "$ICONS/icon.ico"

echo "Rebuilt icon set in $ICONS"
