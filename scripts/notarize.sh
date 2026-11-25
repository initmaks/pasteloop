#!/bin/zsh
# Submits a Developer ID-signed bundle (scripts/bundle.sh signs it) to Apple's notary service and
# staples the ticket to it. Usage: scripts/notarize.sh dist/PasteLoop.app
# Credentials, either:
#   NOTARY_PROFILE=<profile> from `xcrun notarytool store-credentials <profile>`, or
#   NOTARY_KEY=<path to AuthKey_*.p8> NOTARY_KEY_ID=<key ID> NOTARY_ISSUER=<issuer ID>
#   (an App Store Connect team API key with the Developer role; CI uses this).
set -euo pipefail

app="${1:?path to .app}"
if [[ -n "${NOTARY_PROFILE:-}" ]]; then
  auth=(--keychain-profile "$NOTARY_PROFILE")
elif [[ -n "${NOTARY_KEY:-}" ]]; then
  auth=(--key "$NOTARY_KEY" --key-id "${NOTARY_KEY_ID:?set NOTARY_KEY_ID}"
    --issuer "${NOTARY_ISSUER:?set NOTARY_ISSUER}")
else
  echo "set NOTARY_PROFILE, or NOTARY_KEY, NOTARY_KEY_ID and NOTARY_ISSUER" >&2
  exit 1
fi

authority="$(codesign -dvv "$app" 2>&1 | grep -m1 '^Authority=' || true)"
if [[ "$authority" != "Authority=Developer ID Application: "* ]]; then
  echo "$app is not signed with a Developer ID (${authority:-unsigned})" >&2
  exit 1
fi

zip="$(mktemp -d)/${app:t:r}.zip"
ditto -c -k --keepParent "$app" "$zip"
out="$(xcrun notarytool submit "$zip" "${auth[@]}" --wait --output-format json)"
rm -rf "${zip:h}"
id="$(plutil -extract id raw -o - - <<<"$out")"
notary_status="$(plutil -extract status raw -o - - <<<"$out")"
if [[ "$notary_status" != "Accepted" ]]; then
  echo "notarization $id: $notary_status" >&2
  xcrun notarytool log "$id" "${auth[@]}" >&2 || true
  exit 1
fi

xcrun stapler staple "$app"
spctl --assess --type execute --verbose=2 "$app"
echo "notarized $app ($id)"
