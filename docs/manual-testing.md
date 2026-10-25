# Manual testing

Window behavior cannot be unit tested, so it is checked by hand before a release. Each case
names the line it writes to `~/Library/Logs/PasteLoop/pasteloop.log` (debug level, the default
for the app's own messages), which proves the behavior happened. Add a row when you add a
behavior.

Setup: `just bundle && open dist/PasteLoop.app`, or `just run` for a debug build that inherits
the terminal's Accessibility permission. `PASTELOOP_DATA_DIR=/tmp/pasteloop-qa just run` tests
against a scratch database. Quit any other clipboard manager that uses Shift-Cmd-V first.

## Capture and history

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Launch the app | Menu-bar clipboard icon within 200 ms; no Dock icon | `status item visible after N ms` |
| 2 | With TextEdit frontmost press Shift-Cmd-V | Panel slides in at the bottom of the display under the mouse; TextEdit stays frontmost; its window keeps its focus ring | `hotkey -> first render: N ms` |
| 3 | Copy text in any app | A text card appears first, headed by the source app's name | `captured Text` |
| 4 | Copy a URL, a `#1A2B3C` color, a screenshot (Shift-Ctrl-Cmd-4), a file in Finder (Cmd-C) | One card each: link with domain, color swatch with hex, image thumbnail with W×H, file with name | `captured Link/Color/Image/File` |
| 5 | Copy `235442` | Text card, not a color | `captured Text` |
| 5b | Copy text on an iPhone signed in to the same Apple Account (Handoff on), wait a few seconds | A text card appears; the bar never freezes while macOS fetches it | `pasteboard changed with no types yet; waiting for them`, then `pasteboard types arrived after the change` and `captured Text` |
| 5c | In Safari, Share → Copy (a link carrying only a URL) | A link card | `captured Link` |
| 5d | Take a screenshot with a tool that copies the file (CleanShot), or copy an image file in Finder; then delete the file | The card shows the picture with the file name below, and still does after the file is gone; Space previews it; searching a word in the picture finds it | `captured File`, then `ocr: N chars for item` |
| 6 | Copy a password from 1Password/Bitwarden/Apple Passwords | No card | `skipped: Concealed` |
| 7 | Copy the same text twice | One card, moved to the front | `new=false` |
| 8 | Panel open, press Right/Left, Cmd-Down, then Up (also Home, Cmd-Up), Shift-Right | Selection ring moves; Cmd-Down jumps to the last card; Up scrolls back to the first card and selects it; multi-selection rings | — |
| 9 | Select a card, press Return | Panel hides, then content lands at the cursor in the target app. Verify in both TextEdit and a terminal (iTerm2/Terminal): the ⌘V is posted to the session tap after the panel resigns key focus, so it must reach apps that dispatch ⌘V through the key window | `paste: writes=1 inject=true outcome=Pasted` |
| 10 | In TextEdit, Return on a rich text card, then Shift-Return on it | Return pastes the text without formatting; Shift-Return keeps bold, fonts, and links. With Settings → General → Paste text set to "With formatting", the two swap | `paste:` |
| 10b | Shift-Return on an image card, then on a file card, in TextEdit | The image pastes as the path of a PNG in `~/Library/Caches/PasteLoop/paste`; the file as its path | `paste:` |
| 11 | Look at the first nine cards, then hold Cmd | Each shows a ⌘1–⌘9 keycap; holding Cmd lights them all in amber; Cmd-3 pastes the third | `paste:` |
| 12 | Cmd-C on a card | Pasteboard holds the item, panel hides, nothing pasted | `copied 1 item(s)` |
| 13 | Escape | Panel hides | `panel hidden (resumable=true)` |
| 14 | Press the hotkey with the panel open and no collections | Panel hides | `panel hidden` |
| 15 | Click another app's window while the panel is open | Panel hides | `panel lost key status; hiding` |
| 16 | Quit TextEdit while the panel is open over it | Panel hides | `target app quit; hiding panel` |
| 17 | Drag the panel's top edge | Height changes live; below 220 pt the card headers disappear (compact mode); height persists across relaunch | `panel height set to N` |
| 18 | Menu-bar icon: left click | Toggles the panel | `status command: TogglePanel` |
| 19 | Menu-bar icon: right click → Pause Capture → For 5 Minutes | Icon dims; copies are not captured; Resume restores | `capture paused`, `skipped: Paused` |
| 20 | Cmd-T in the panel | Toggles pause with a toast | `capture paused` / `capture resumed` |
| 21 | Menu → Erase History… → Erase | Every card not in a collection goes; collections stay | `erased N items` |
| 22 | Edit `settings.json` retention to `"day"` and relaunch with items older than a day | Items older than a day are gone | `sweep: expired=N` |
| 23 | Delete key, then Cmd-Z | Card disappears, then comes back selected | `deleted 1 item(s)` |
| 24 | Revoke Accessibility, press Return on a card, then on another | Each time: the sheet "Allow PasteLoop to paste for you" with Don't Ask Again, Copy Only, Open System Settings. Copy Only copies (Shift-Return still pastes the formatted form); Don't Ask Again copies and stops the sheet until Settings → General turns it back on. Return on the sheet presses Open System Settings (in every sheet, Return presses the highlighted button; Esc closes it) | `paste needs Accessibility (trusted=false); asking`, then `opening Accessibility settings` |
| 25 | Leave the app idle for a minute with Activity Monitor open | CPU under 0.5 %; RSS under 80 MB | — |
| 26 | Launch without Accessibility, grant it while the app runs, press the hotkey | Sheet "Relaunch PasteLoop to finish enabling paste"; Return meanwhile copies with a toast; "Relaunch PasteLoop" restarts the app and Return then pastes | `accessibility granted after launch; asking to relaunch`, `relaunching for the Accessibility grant`, then `accessibility=true` at startup |
| 27 | Search or switch to a collection, arrow to the fourth card, press Escape, press the hotkey again within a minute | Same tab, search, and card as before | `panel opened where it was left` |
| 28 | As 27, but wait over a minute, or copy something new, before pressing the hotkey | History tab, empty search, the newest card selected | `panel opened at the newest card` |
| 29 | Paste a card from a search or a collection, then press the hotkey | History tab, empty search, the newest card selected | `panel opened at the newest card` |
| 30 | Scroll far right, press Escape, wait a minute, copy something new, press the hotkey; repeat a few times | The bar rises and fades in with the newest card at the left; the previous cards never flash first | no `panel revealed by the fallback` |
| 31 | Scroll the strip slowly, then press Cmd-Down and Cmd-Up | The toolbar counter (for example `012 / 348`) rolls its digit wheels to the first card in view; a long jump spins briefly and settles; on a search it reads `/ 200+` while more results are unloaded | — |

## Search

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Panel open, type `rus` | Search field focuses on the first key; results replace the strip on every keystroke | `list reloaded: N rows in N µs (search=true)` |
| 2 | Return, then Right/Left, then Return | Focus moves to the results with the filter kept; the selected result pastes | `paste:` |
| 3 | Tab | Toggles between field and results | — |
| 4 | Type `safa`, press Tab | A "Safari" chip appears in the field; results narrow to Safari | — |
| 5 | Type `today`, Tab; `link`, Tab | Date chip and kind chip | — |
| 6 | Cmd-F twice | First focuses the field, second opens the filter popover; clicking a chip toggles it | — |
| 7 | Escape while searching: once with the cursor in the search field, once on the results (after Return) | In the field: clears the search and shows history. On the results: hides, and the hotkey within a minute brings back the same search and card | `panel hidden (resumable=true)` |
| 8 | Search for an item that is in a collection | Its card shows a colored dot per collection | — |
| 9 | Cmd-G on a search result | Search closes, the home list scrolls to the item, 600 ms highlight | — |
| 10 | Match highlight | The matched text in a text card is highlighted | — |
| 11 | Type `rust book` in the search field; Cmd-R, Cmd-E, and Cmd-N and type words with spaces; Shift-Cmd-N and type a two-word name | Every Space types a space; the preview window never opens and the editor stays open | — |
| 12 | Type `rus`, press Right twice, Shift-Right, Left, Up; type `t` | The arrows move the selection through the results as on the cards, the caret stays at the end of the field, and `t` extends the search. In Cmd-R and Cmd-E the arrows still move the caret | — |

## Collections

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Shift-Cmd-N, type a name, pick a color, Return | New tab at the end with its color, empty state "Drag a card here, or right-click a card → Add to Collection" | — |
| 2 | Right-click a card, hover or click Add to Collection › (near the right edge and near the bottom too), choose a collection | The submenu opens beside the menu, which stays open (to the left near the right edge, upward in the lower half); the card appears in the collection; clicking outside both menus closes them | — |
| 3 | Right-click a tab → Rename / Color / Delete | Each works; delete asks for confirmation and keeps items in history | — |
| 4 | Cmd-Right / Cmd-Left; hotkey while open | Cycles history → collections → history | — |
| 5 | Set retention to 1 day, with collection items older than a day | They survive the sweep | `sweep:` |
| 6 | Drag a card onto a tab | Adds it to that collection | — |
| 7 | Scroll the strip right, then click the tab that is already selected (Clipboard or a collection) | The strip scrolls back to the first card and selects it; no slide animation | — |

## Preview and edit

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Select an image card, press Space | The preview window floats above the panel showing the image; arrowing to other cards updates it to their content; Space or Esc closes it; the panel keeps focus | — |
| 1b | Space on a large screenshot (larger than the space above the bar), then on a small image | The window takes the large one at up to 90% of the space above the bar, proportions kept; the small one at its actual size | `preview: order_front at … size …` |
| 2 | Space on a link card, then on file cards: an image file, a text file, a PDF | The link shows a clean URL card (domain, title if it was captured, URL) with no network use. The image file shows as an image, the text file as text, the PDF as its icon, name, path, and size. macOS Quick Look never opens | — |
| 3 | Space on a text card | The preview window shows the full text (monospaced for code); the panel keeps focus; Space again or Esc closes; ⌘E opens the editor | — |
| 4 | Cmd-R, type a name, Return | Card header shows the name; search finds it | — |
| 5 | Cmd-E on a text card, change text, Cmd-Return | Card updates; formatting dropped; search finds the new text | — |
| 6 | Cmd-N, type text, Cmd-Return | New text card at the front | — |
| 7 | Cmd-O on a link and on a file | Opens in the browser / default app; panel hides | — |
| 8 | Copy a screenshot with text, then search a word from it | The image card is a result | `ocr: N chars for item` |
| 9 | Right-click an image → Extract Text | New text card with the recognized text; toast | — |
| 10 | Settings → Integrations → Link previews on; copy a URL | Link card shows the fetched title (and image when the page has one) | `link metadata stored for item` |

## Paste Queue, dragging, system integration

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Shift-Cmd-C | Queue window at the top-right; "The queue is empty…" | `paste queue activated (tap=true)` |
| 2 | Copy three things | Three rows, the first highlighted, "Oldest first" | — |
| 3 | Cmd-V three times in TextEdit | Items paste in order; rows disappear; empty state with Done | `queue advanced past item` ×3 |
| 4 | Toggle "Newest first" | Head becomes the last copy | — |
| 5 | Revoke Accessibility, repeat | "Paste next" button advances manually | `paste queue activated (tap=false)` |
| 6 | Drag a text card into TextEdit; a file card into Finder; an image card (images as files on) into a Slack message | Text inserted; file copied; image attached | `started native text drag` |
| 7 | System Settings → Screen Sharing on (or a screen recording) with "show during screen sharing" off | Panel invisible in the recording | — |
| 8 | Plug in a second display, press the hotkey with the mouse there | Panel appears on that display; unplugging re-anchors an open panel | `status command: ScreenParametersChanged` |
| 9 | Accessibility → Reduce Motion on | Show/hide only fade; cards do not lift | — |
| 10 | Cmd-, with the bar tall (Settings → Appearance), then with it short | Settings opens frontmost, centered in the space above the bar and never under it; with little room it is shorter, down to its minimum; closing it returns focus to the previous app | `settings window opened`, `settings window closed` |
| 11 | Settings → Shortcuts → click Activate PasteLoop, press Ctrl-Alt-Cmd-P | Shortcut re-registers immediately | `hotkeys re-registered` |
| 12 | Settings → General → Keep history → 1 day (with older items) | Confirmation naming the count; Delete and Apply removes them | `sweep: expired=N` |

## Appearance settings

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Settings → Appearance; drag the top edge of the live bar | The bar follows smoothly, showing empty space below a partial row, and snaps to whole rows on release | `panel height set to N (R row(s))` |
| 2 | Step Card width and Card height with −/+ | Cards resize on the live bar; changing height grows/shrinks the bar to keep the same row count | — |
| 3b | Settings → General: add lines to the ignore list until it is longer than nine | The box grows with every line; no line ever sits outside it | — |
| 3 | Drag the Text size knob, or click the small/large "A" ends | Card and preview text scale together; the percent readout updates; the value persists across relaunch | — |

## Bundle and command line

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | `just bundle && open dist/PasteLoop.app` | Menu-bar icon, no Dock icon, "Open at login?" prompt once | `status item visible after` |
| 2 | Grant Accessibility to PasteLoop.app, rebuild with `just bundle` (dev certificate trusted) | Permission survives the rebuild | — |
| 3 | `pasteloop recent 5`, `pasteloop search rust`, `pasteloop export --json /tmp/x.json` | JSON output; the GUI shows items added by `pasteloop add` within 2 s | — |

## Backup to iCloud Drive

| # | Steps | Expected | Log |
|---|---|---|---|
| 1 | Fresh install from the bundle: answer the login question, open the bar again | "Back up your history to iCloud Drive?"; Back Up turns it on | `iCloud Drive backup turned on` |
| 2 | With backup on, wait for the first check (20 s after launch) | macOS asks once for access to iCloud Drive; after Allow, `iCloud Drive/PasteLoop/<Mac name> (…)` appears with `history.sqlite.lzfse`, `blobs/`, `manifest.json` | `backup: N items, … to …` |
| 3 | Settings → General | The switch is on; the line reads "Last backup: … · N items · uploading", then "· in iCloud" | — |
| 4 | Copy something, then wait 30 minutes (or press Back Up Now) | A new backup with 0 or 1 blobs copied, in seconds | `backup: …` |
| 5 | Delete an image card, wait 5 minutes | The next backup prunes its blob | `… 1 pruned …` |
| 6 | Copy something, then quit from the menu-bar menu, and reopen PasteLoop right away | The bar and the menu-bar icon disappear at once; the reopened app starts once the old one has finished its backup | `quitting; status item and hotkeys released`, `backed up before quitting`, then `waiting for the previous instance to finish quitting` |
| 7 | New Mac, or a test user account signed in to the same iCloud: install, turn backup on | History, collections, and settings come back; toast "Restored N items from iCloud Drive"; image cards fill in as files download | `restoring N items backed up on …`, `restore: merged N items …` |
| 8 | System Settings → Privacy & Security → Files and Folders: turn iCloud Drive off for PasteLoop | Settings shows "Last backup failed: allow PasteLoop under …" | `backup: access to iCloud Drive denied` |

## Measuring the budgets

| Metric | How |
|---|---|
| Cold launch, hotkey to first frame, capture to card | The log: `status item visible after N ms`, `hotkey -> first render: N ms`, `captured … in N ms` |
| Search latency | `just bench` (100k synthetic items, release profile) |
| Idle CPU and memory | `pasteloop seed 10000`, leave the app idle for a minute, then Activity Monitor or `footprint pasteloop` |
| Release binary | `ls -l target/release/pasteloop` |
