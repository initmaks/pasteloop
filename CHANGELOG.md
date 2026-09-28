# Changelog

All notable changes to PasteLoop. Versions follow [Semantic Versioning](https://semver.org).

## Unreleased

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
