# Changelog

All notable changes to PasteLoop. Versions follow [Semantic Versioning](https://semver.org).

## Unreleased

<!-- One line per entry: GitHub release notes keep line breaks. -->

- Importing from Paste brings over images and files too: the importer now reads the clips Paste stores outside its database, which are most images and files and the longest texts. It also finds the Mac App Store edition of Paste, and running it again only adds what is missing, without duplicating collections.
- A copy that carries only HTML (from web views and Chromium browsers) becomes a text card with readable text instead of an empty one.

## 1.1.0 — 2026-10-23

- Optional backup to iCloud Drive, asked about once on first launch. It runs on its own after changes settle and before quitting, and a new Mac or a fresh macOS install restores history, collections, and settings automatically. The live database never syncs; a compressed snapshot (36 MB for 110k items) and the stored files do. `pasteloop backup` and `pasteloop restore --from <folder>` do the same from a terminal.
- Return now pastes text without its formatting, and Shift-Return pastes it with formatting. Images and files still paste as themselves on Return and as paths on Shift-Return. Settings → General → Paste text switches the default back. The context menu names the second paste for each kind of card ("Paste with Formatting", "Paste as Path").
- Copies from an iPhone (Universal Clipboard) are captured: PasteLoop waits for their types to arrive, and fetching them no longer blocks the bar. Links that carry only a URL are captured too.
- Every paste explains how to allow pasting into apps while Accessibility permission is missing, instead of silently copying; Don't Ask Again (or a Settings switch) turns that off.
- In any dialog, Return presses the highlighted button, so a second Return on the Accessibility explanation opens System Settings.
- The Settings window opens above the bar, shrunk to fit, instead of underneath it.
- The ignore list's box grows with its text instead of letting lines run below it.

## 1.0.0 — 2026-09-27

First public release.

- Clipboard history in a bar of cards along the bottom of the screen, shown with Shift-Cmd-V
  over any app without taking focus from it. The bar can be resized and wraps into several
  rows when tall enough.
- Captures text with its rich formatting, links, images, files, and colors. Skips concealed
  and transient entries from password managers and any application you ignore.
- Paste into the previous app with Return, as plain text with Shift-Return, or by number with
  Cmd-1 to Cmd-9. Plain-text paste of an image pastes the path of a PNG copy; of files, their
  paths.
- Instant search over 100k+ items: titles, text, link titles, app names, and text recognized
  inside images. Chips filter by type, app, date, and collection.
- Collections: named, colored lists whose items never expire.
- Paste Queue: copy several things, then paste them one Cmd-V at a time.
- Preview window for every kind, sized to the image up to 90% of the space above the bar.
- Reopening within a minute returns to the same list, search, and card.
- A rolling position counter in the toolbar; Up jumps to the first card.
- Import of text, link, and color history from the Paste app.
- `pasteloop` command line: search, get, recent, add, export, stats.
- Retention from one day to forever, pause capture, launch at login, hidden from screen
  sharing by default.
