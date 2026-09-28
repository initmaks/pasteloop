#!/bin/zsh
# Builds dist/<App>.app around a release binary: writes Info.plist, copies the binary and icon,
# and signs the bundle. Usage: scripts/bundle.sh <dist dir> <app name> <bundle id> <binary>
set -euo pipefail

dist="${1:-dist}"
app_name="${2:-PasteLoop}"
bundle_id="${3:-io.github.initmaks.pasteloop}"
binary="${4:-target/release/pasteloop}"
version="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
identity="${CODESIGN_IDENTITY:-}"

if [[ ! -x "$binary" ]]; then
  echo "binary not found: $binary (run cargo build --release)" >&2
  exit 1
fi

app="$dist/$app_name.app"
contents="$app/Contents"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$binary" "$contents/MacOS/pasteloop"
if [[ -f assets/icon.icns ]]; then
  cp assets/icon.icns "$contents/Resources/icon.icns"
fi

cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>$app_name</string>
  <key>CFBundleExecutable</key><string>pasteloop</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>$app_name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>MIT License</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
printf 'APPL????' > "$contents/PkgInfo"

# Prefer an explicit identity, then the self-signed dev certificate, then ad hoc.
if [[ -z "$identity" ]]; then
  if security find-identity -v -p codesigning 2>/dev/null | grep -q '"PasteLoop Dev"'; then
    identity="PasteLoop Dev"
  else
    identity="-"
  fi
fi
if [[ "$identity" == "-" ]]; then
  echo "signing ad hoc (Accessibility permission will not survive rebuilds; run scripts/make-dev-cert.sh)" >&2
fi
codesign --force --sign "$identity" --options runtime --timestamp=none \
  --entitlements scripts/entitlements.plist "$app"
codesign --verify --verbose=2 "$app" 2>&1 | sed 's/^/codesign: /' >&2
echo "built $app (signed: $identity)"
