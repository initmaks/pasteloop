#!/bin/zsh
# Replaces the installed app with a freshly built bundle and restarts it, so a change can be tried
# at once. Usage: scripts/install.sh <built .app> [install dir]
set -euo pipefail

src="${1:-dist/PasteLoop.app}"
dest_dir="${2:-/Applications}"
app="$dest_dir/${src:t}"
gui="$app/Contents/MacOS/pasteloop"
log="$HOME/Library/Logs/PasteLoop/pasteloop.log"

if [[ ! -d "$src" ]]; then
  echo "bundle not found: $src (run just bundle)" >&2
  exit 1
fi
bundle_id="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$src/Contents/Info.plist")"

# The menu-bar app runs with no arguments. `pasteloop <command>` runs from the same binary (an
# import may be going) and is left alone.
running() { pgrep -f "^$gui\$" >/dev/null }

wait_for_exit() {
  local tries=$1
  for _ in $(seq 1 "$tries"); do
    running || return 0
    sleep 0.25
  done
  ! running
}

if running; then
  osascript -e "tell application id \"$bundle_id\" to quit" >/dev/null 2>&1 || true
  if ! wait_for_exit 40; then
    echo "PasteLoop did not quit; sending SIGTERM" >&2
    pkill -TERM -f "^$gui\$" || true
    wait_for_exit 20 || { echo "PasteLoop is still running; not replacing it" >&2; exit 1; }
  fi
fi

rm -rf "$app"
ditto "$src" "$app"
codesign --verify "$app"

offset=$(stat -f %z "$log" 2>/dev/null || echo 0)
open "$app"
ready=""
for _ in $(seq 1 60); do
  ready="$(tail -c +$((offset + 1)) "$log" 2>/dev/null | grep -m1 'ready after' || true)"
  [[ -n "$ready" ]] && break
  sleep 0.25
done
if [[ -z "$ready" ]]; then
  echo "PasteLoop did not report ready; see $log" >&2
  exit 1
fi

new_lines="$(tail -c +$((offset + 1)) "$log")"
version="$("$gui" --version)"
access="$(grep -m1 -o 'accessibility=[a-z]*' <<<"$new_lines" || echo 'accessibility=unknown')"
echo "installed $app: $version, ${ready##*INFO }, $access"
if [[ "$access" == "accessibility=false" ]]; then
  echo "Accessibility is off for this build: pasting will ask for it" >&2
fi
