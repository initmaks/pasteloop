#!/bin/zsh
# Creates a self-signed code-signing certificate "PasteLoop Dev" in the login keychain, once.
# TCC ties Accessibility permission to the code signature; ad-hoc signatures change on every
# build, a stable certificate keeps the grant across rebuilds.
set -euo pipefail

name="PasteLoop Dev"
if security find-identity -v -p codesigning | grep -q "\"$name\""; then
  echo "certificate \"$name\" already exists"
  exit 0
fi
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/ext.cnf" <<CNF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CNF
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
  -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/ext.cnf" >/dev/null 2>&1
openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/dev.p12" \
  -passout pass:pasteloop -name "$name" >/dev/null 2>&1 \
  || openssl pkcs12 -export -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/dev.p12" \
  -passout pass:pasteloop -name "$name" >/dev/null 2>&1
keychain="$HOME/Library/Keychains/login.keychain-db"
security import "$tmp/dev.p12" -k "$keychain" -P pasteloop -T /usr/bin/codesign -T /usr/bin/security >/dev/null
echo "created certificate \"$name\" in the login keychain"
# Trusting the certificate changes the user's trust settings, which macOS gates behind a
# password dialog; do it here only when asked, otherwise print the one-time instruction.
if [[ "${PASTELOOP_TRUST_CERT:-}" == "1" ]]; then
  security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$tmp/cert.pem"
else
  cat >&2 <<MSG
If codesign rejects the certificate ("unable to build chain"), trust it once:
  PASTELOOP_TRUST_CERT=1 scripts/make-dev-cert.sh   (asks for your login password)
or open Keychain Access, find "$name", and set Code Signing to Always Trust.
MSG
fi
