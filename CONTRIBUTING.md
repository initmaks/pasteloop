# Contributing

Bug reports and pull requests are welcome. For anything larger than a fix, open an issue first
so we can agree on the shape before you write it.

## Setup

- Apple Silicon Mac, macOS 15 or later, Rust stable, the Xcode Command Line Tools, and
  [`just`](https://github.com/casey/just).
- `just run` starts a debug build from the terminal. It inherits the terminal's Accessibility
  permission, which is the easiest way to test pasting.
- `PASTELOOP_DATA_DIR=/tmp/pasteloop-dev just run` keeps your real history out of it. Quit the
  installed app first, or give the development instance other shortcuts: both would answer
  Shift-Cmd-V.
- `PASTELOOP_LOG=debug` (or any `tracing` filter) changes log verbosity. Logs go to
  `~/Library/Logs/PasteLoop/pasteloop.log`; `just log` tails them.

macOS ties the Accessibility grant to the code signature. An ad-hoc signed bundle changes
signature on every build, so `just bundle` signs with a local certificate named
"PasteLoop Dev" when one exists. Create it once with `just dev-cert`.

## Before you send a change

```sh
just check      # cargo fmt --check, clippy with warnings as errors, tests
```

CI runs the same on every push and pull request, plus `just audit` (`cargo deny`: known
advisories, licenses, and dependency sources, configured in `deny.toml`) and CodeQL; both also
run weekly. Dependabot opens update pull requests for crates and the pinned GitHub Actions.
If you touch search or storage, also run `just bench` (search latency over 100k synthetic items)
and compare before and after.

## Ground rules

- Rust only. Apple frameworks are reached through the `objc2` crates, and every `unsafe` block
  lives in `src/macos.rs` with a `// SAFETY:` comment.
- UI is GPUI at the pinned zed revision in `Cargo.toml`. Read its source under
  `~/.cargo/git/checkouts/zed-*/<rev>/crates/` rather than guessing at the API; it changes
  often.
- Few files, little abstraction: no trait with one implementation, no builder for a small
  struct. Comments say why, not what.
- `anyhow` at the edges, `Result` below. Never `unwrap()` on pasteboard, database, or network
  data.
- Never log clipboard contents.
- Keep the main thread free: no image decoding, no database writes, no blocking pasteboard
  reads on it.
- A new dependency needs a reason in the pull request: what it does, and its effect on the
  release binary's size.

## Tests

Unit tests cover the logic that can run without a window server: storage, migrations, search,
retention, capture rules and classification, paste planning, selection and list state, and
settings. They use a real SQLite database in a temporary directory; there are no AppKit mocks.

Window behavior is checked by hand. [docs/manual-testing.md](docs/manual-testing.md) lists the
cases, each with the log line that proves it happened. Add a row when you add a behavior.

## Performance budgets

These are requirements, and a change that breaks one needs a very good reason.

| Metric | Budget |
|---|---|
| Cold launch to menu-bar icon | ≤ 200 ms |
| Hotkey to first frame of the bar | ≤ 30 ms |
| Scroll frame time at 120 Hz, 10k items | ≤ 8 ms p99 |
| Keystroke to search results, 100k items | ≤ 10 ms |
| 1 MB text copy to visible card | ≤ 50 ms |
| Idle CPU with capture on | ≤ 0.5 % |
| Release binary | ≤ 30 MB |

## Releasing

1. Move the `Unreleased` notes in `CHANGELOG.md` under a new version heading and set the same
   version in `Cargo.toml`.
2. Commit, then tag and push: `git tag v1.2.3 && git push origin main v1.2.3`.
3. The release workflow builds `PasteLoop.app`, signs it ad hoc, zips it with a SHA-256
   checksum, and publishes a GitHub release with that version's changelog section.

To ship a notarized build instead, run `just notarize` locally with a Developer ID
(`DEVELOPER_ID` and a `NOTARY_PROFILE` from `xcrun notarytool store-credentials`), zip the
stapled app with `ditto -c -k --keepParent`, and replace the asset on the release.
