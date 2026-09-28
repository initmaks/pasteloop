#!/bin/zsh
# Signs the bundle with a Developer ID certificate and submits it to Apple's notary service.
# Usage: DEVELOPER_ID="Developer ID Application: Name (TEAMID)" NOTARY_PROFILE=<keychain profile> \
#        scripts/notarize.sh dist/PasteLoop.app
# The keychain profile comes from: xcrun notarytool store-credentials <profile>
set -euo pipefail

app="${1:?path to .app}"
: "${DEVELOPER_ID:?set DEVELOPER_ID to the Developer ID Application identity}"
: "${NOTARY_PROFILE:?set NOTARY_PROFILE to a notarytool keychain profile}"

codesign --force --deep --sign "$DEVELOPER_ID" --options runtime --timestamp \
  --entitlements scripts/entitlements.plist "$app"
zip="${app%.app}.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait
xcrun stapler staple "$app"
spctl --assess --type execute --verbose=2 "$app"
echo "notarized $app"
