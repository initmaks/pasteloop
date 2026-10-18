# How PasteLoop works

One Rust crate. `src/lib.rs` holds the app and every module so the benchmark can link against
the store; `src/main.rs` only calls `pasteloop::run()`. The same binary is the menu-bar app and
the command-line tool: `pasteloop search …` runs a subcommand and exits without starting the UI.

| File | Role |
|---|---|
| `lib.rs` | Startup, hotkeys, status item, pasteboard poller, maintenance loop |
| `state.rs` | The app state entity: the loaded list, selection, search, pause, resume window, queue |
| `panel.rs` | The bar: toolbar, tabs, search field, card strip, context menus, keymap |
| `cards.rs` | Card rendering, theme, the virtualized strip element, image cache |
| `preview.rs`, `queue.rs`, `settings_ui.rs` | The other windows |
| `text_input.rs` | Text field, adapted from GPUI's `examples/input.rs` |
| `store.rs` | SQLite schema, migrations, queries, search, retention, blob store |
| `clipboard.rs` | Capture rules: what to skip, how to classify, size caps |
| `paste.rs` | Building pasteboard contents, posting Cmd-V, multi-item pastes |
| `jobs.rs` | Background work: thumbnails, preview images, OCR, link metadata |
| `import.rs` | Reading the Paste app's database |
| `cli.rs`, `settings.rs` | Subcommands; settings file and paths |
| `macos.rs` | All `objc2` and C calls, and every `unsafe` block |

## Windows

The bar is a GPUI `PopUp` window, which GPUI creates as a non-activating `NSPanel` at the pop-up
level. It becomes the key window, so it gets the keyboard, while the app you were in stays
frontmost and keeps its text cursor. The app runs with the `Accessory` activation policy, so it
has no Dock icon.

The window is created once and kept. Hiding orders it out at zero alpha and shrinks it to a
1×1 frame, which makes the window server drop its drawing surfaces (about 50 MB for a
full-width bar on a Retina display). Showing prepares the first page of cards, sets the frame,
and reveals the window after GPUI has drawn the first frame at the new size, so the previous
contents never flash. A 100 ms fallback reveals it anyway and logs a warning.

AppKit delivers `windowDidBecomeKey:` synchronously, and GPUI's window delegate borrows the app
state inside it. Ordering a window front or out from inside a GPUI update therefore fails
with a re-entrant borrow. Every order call goes through `macos::NativeWindow` from a spawned
task, which is also how GPUI's own `activate_window` does it.

The bar hides when you click anywhere outside it. Losing key status covers clicks in other
apps' windows; a global mouse-down monitor covers clicks that do not move key status, such as
the desktop, the menu bar, and the Dock.

## Capture

A foreground task polls `NSPasteboard.changeCount` every 100 ms. When it moves, the skip rules
run on the type list alone: our own writes, pause, concealed and transient markers (the
nspasteboard.org convention password managers follow), and ignored apps. Only then is the data
read, on a background thread, because a source app with promised data renders it on demand
and can take a while. If the change count moved again during the read, the result is dropped
and the next poll takes the newer copy.

Classification order: file URLs, then images, then a single color like `#1A2B3C`, then a
single URL, then text. A TIFF that comes with text types (spreadsheets and rich editors put
both on the pasteboard) is text. Turning every copied spreadsheet range into a picture helps
no one.

## Storage

SQLite in WAL mode: one write connection and a small pool of readers. Text up to 64 KB is
stored inline; longer text, images, and rich-text representations go to
`blobs/<prefix>/<blake3>` with a reference count, so copying the same image twice stores it
once. A sweep removes unreferenced blobs.

Item ids follow creation time. Copying something that is already in history moves it to the
front by setting `last_used_at` instead of inserting a duplicate, and so does pasting it.
Imports go below the lowest existing id, keeping ids in time order.

### Search

An FTS5 table with the trigram tokenizer indexes titles, text, link titles, app names, and OCR
text. Kind, app, date, and deletion are UNINDEXED columns of the same table, so filters apply
inside the full-text scan.

A common trigram matches tens of thousands of rows, and asking SQLite for all matches and then
sorting by time is too slow for the 10 ms budget. The query walks matches in descending rowid
order, which FTS5 produces natively, and stops once it has a page plus some slack. Rowid
order equals time order for everything except items moved to the front; those carry
`last_used_at` and have a partial index, so a second small query finds them, but only when the
first window was cut short, and only among rows newer than its oldest match. The union is
sorted by time.

Terms shorter than three characters cannot use trigrams. They are matched with `LIKE` over
the newest 5,000 items.

On a real library of 110k items, most searches take 1 to 8 ms; `just bench` measures the same
shapes on 100k synthetic items.

### Schema changes

`meta.schema_version` records the version. A fresh database gets the current schema in one
step; each later version adds a migration from the previous one inside a transaction.

## Pasting

1. Write the item's representations to the general pasteboard: plain text, RTF or HTML, URL,
   file URLs, or image data.
2. Hide the bar and wait one run-loop turn so key status returns to the target app.
3. Post Command down, V down, V up, Command up to the session event tap, each V event carrying
   the Command flag.

Posting to the session tap rather than to the target's pid matters: some apps (iTerm2, for
one) dispatch Cmd-V through the key window's responder chain, which a pid-targeted event
skips. Posting an explicit Command key press matters for apps that track modifiers as held
keys.

Posting keys needs Accessibility permission, and macOS only honors a grant for a process that
had it when it launched: after a grant made while the app runs, `AXIsProcessTrusted` returns
true but posted events are silently dropped. PasteLoop records whether it was trusted at
launch, copies instead of pasting until it is relaunched, and offers to relaunch itself.

A selection of several text-like items pastes as one joined text. Several files paste as one
file list. A mix is pasted as a sequence, one item at a time.

Only one GUI instance runs per data directory, enforced by an advisory lock on
`pasteloop.lock`. Two instances would both register the hotkey and both steal key status, and
the posted Cmd-V could land in the other bar.

### Paste Queue

While the queue is active, a `CGEventTap` on its own thread watches Cmd-V (matched by keycode
or character, so non-QWERTY layouts work). 60 ms after each one, when the target app has read
the pasteboard, the queue advances and writes the next item.

## Memory

The strip is virtualized: only visible cards are laid out and painted. Card thumbnails are
decoded off the main thread and held in a least-recently-used cache of 64 images that also
releases their GPU atlas space. The preview window shows large images at up to 1600 px. On
hide, the SQLite page caches are shrunk and free heap pages are returned to the system.
Idle CPU with capture on is effectively zero.

## Backup to iCloud Drive

`backup.rs` keeps one folder per library under `iCloud Drive/PasteLoop/`:

- `history.sqlite.lzfse`: a `VACUUM INTO` copy of the database with deleted items removed and
  the search index emptied, compressed with LZFSE (Apple's Compression framework, streamed).
  On a 110k-item library that is 36 MB, down from a 320 MB database. It is built next to the
  live database and renamed into iCloud Drive, so iCloud never sees a partial file.
- `blobs/`: the blob files. They are content-addressed and never change, so each one uploads
  once; files the new snapshot no longer uses are pruned after it is in place.
- `settings.json` and `manifest.json` (library id, device, item count, time).

The live database never sits in iCloud Drive, because syncing a file SQLite is writing to
corrupts it.

A worker thread at utility priority checks every five minutes. A change fingerprint (counts and
latest timestamps of items, recognized text, link titles, deletions, collections) says whether
anything is not yet backed up. A backup runs after 30 minutes without changes, after 5 when
items were removed, at least every 6 hours while changes keep coming, and before quitting (up
to 20 seconds). A first backup copies every blob; later ones take a few seconds.

Restore: a library younger than 14 days that finds a backup of another library more than
twice its size merges it in (`Store::merge_snapshot`). Items already present (same id or same
content) are skipped. The rest go below the lowest local id, since the backup is older, so ids
keep following time, and the search index fills in through the insert triggers. Collections,
memberships, apps, and blob rows come along, blob files are copied (iCloud downloads them on
read), and the library adopts the backup's folder so the next backup continues there. The age
limit keeps two Macs in daily use from folding their histories into each other.

## Import from the Paste app

`import.rs` opens the Paste app's Core Data store read-only. Each clip's pasteboard payload
comes in one of three encodings: a binary property list, an LZFSE stream, or a raw-DEFLATE JSON
array with base64 values. The compressed ones are decoded with Apple's Compression framework. Text,
links, and colors are imported with their date, source app, title, and pinboard membership.
Images are stored in a separate format and are skipped.

## Building without the Metal compiler

Recent Xcode versions ship the Metal shader compiler as a separate download. GPUI's
`runtime_shaders` feature compiles its shaders with `MTLDevice.newLibraryWithSource` when the
renderer starts, so the app builds with only the Command Line Tools. The cost is a one-time
compile at launch, not on every show.
