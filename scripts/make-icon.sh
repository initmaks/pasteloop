#!/bin/zsh
# Renders assets/icon.svg into assets/icon.icns (and the README's assets/icon.png) with the
# system's Quick Look renderer.
set -euo pipefail
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
iconset="$tmp/icon.iconset"
mkdir -p "$iconset"
qlmanage -t -s 1024 -o "$tmp" assets/icon.svg >/dev/null 2>&1
src="$tmp/icon.svg.png"
for size in 16 32 128 256 512; do
  sips -z $size $size "$src" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$src" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o assets/icon.icns
sips -z 512 512 "$src" --out assets/icon.png >/dev/null
echo "wrote assets/icon.icns and assets/icon.png"
