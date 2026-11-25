# PasteLoop build tasks. `just` runs the default recipe list.

set shell := ["zsh", "-cu"]

app_name := "PasteLoop"
bundle_id := "io.github.initmaks.pasteloop"
dist := "dist"

default:
    @just --list

# Debug build
build:
    cargo build

# Release build (opt-level 3, thin LTO)
release:
    cargo build --release

# Unit tests
test:
    cargo test

# Dependency advisories, licenses, and sources (deny.toml); CI runs it too
audit:
    cargo deny check

# Format, lint with warnings as errors, and test — the pre-commit gate
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test

# Run the app from the debug build with logs in ~/Library/Logs/PasteLoop
run:
    cargo run

# Build the release binary and wrap it in dist/PasteLoop.app, signed with a Developer ID when one
# is in the keychain, else the "PasteLoop Dev" certificate (scripts/make-dev-cert.sh), else ad hoc.
bundle: release
    scripts/bundle.sh "{{dist}}" "{{app_name}}" "{{bundle_id}}" target/release/pasteloop

# Zip the bundle for a release: dist/PasteLoop-<version>-arm64.zip and its SHA-256. Notarizes
# first when notary credentials are set (see scripts/notarize.sh).
package: bundle
    #!/bin/zsh
    set -euo pipefail
    if [[ -n "${NOTARY_PROFILE:-}${NOTARY_KEY:-}" ]]; then
      scripts/notarize.sh "{{dist}}/{{app_name}}.app"
    fi
    version="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
    zip="{{dist}}/{{app_name}}-$version-arm64.zip"
    rm -f "$zip"
    ditto -c -k --keepParent "{{dist}}/{{app_name}}.app" "$zip"
    (cd "{{dist}}" && shasum -a 256 "${zip:t}" > "${zip:t}.sha256")
    echo "$zip"

# Build the bundle, replace /Applications/PasteLoop.app with it, and restart it
install: bundle
    scripts/install.sh "{{dist}}/{{app_name}}.app"

# Launch the bundled app (build it first with `just bundle`)
open:
    open "{{dist}}/{{app_name}}.app"

# Create the self-signed development certificate once
dev-cert:
    scripts/make-dev-cert.sh

# Notarize and staple the Developer ID-signed bundle (needs notary credentials; see the script)
notarize: bundle
    scripts/notarize.sh "{{dist}}/{{app_name}}.app"

# Tail the app log
log:
    tail -f ~/Library/Logs/PasteLoop/pasteloop.log

# Regenerate assets/icon.icns from assets/icon.svg
icon:
    scripts/make-icon.sh

# Search benchmark (100k items)
bench:
    cargo bench --bench search
