# AGENTS.md

PasteLoop is a native macOS clipboard manager in Rust: a bar of cards along the bottom of the
screen, collections, instant search, Quick Paste, and a Paste Queue. Read `CONTRIBUTING.md`
(rules, tests, budgets) and `docs/architecture.md` (how it works) before changing code.

## Constraints

- Rust only. Apple frameworks through the `objc2` crates; every `unsafe` block lives in
  `src/macos.rs` with a `// SAFETY:` comment.
- Target `aarch64-apple-darwin`, macOS 15 or later. No App Sandbox.
- UI is GPUI from `zed-industries/zed`, pinned by git `rev` in `Cargo.toml`. Read the source
  at that rev under `~/.cargo/git/checkouts/zed-*/<rev>/crates/`; do not rely on memory of the
  API.
- Never order a window front or out inside a GPUI update: go through `macos::NativeWindow` from
  a spawned task.
- No work on the main thread that can block: image decoding, database writes, pasteboard reads.
- Never log clipboard contents.
- "Paste" is another company's product. Mention it only where PasteLoop imports from it.

## Workflow

- `just check` before every commit: `cargo fmt --check`, clippy with `-D warnings`, tests.
- Finish every change to the app with `just install`, without being asked: it rebuilds the
  bundle, replaces `/Applications/PasteLoop.app`, and restarts it, so the user can try the change
  at once. Report the line it prints (version, startup time, Accessibility).
- Unit tests for logic that runs without a window server, against a real SQLite database in a
  temporary directory. No AppKit mocks.
- Every new window behavior gets a `tracing` line that proves it and a row in
  `docs/manual-testing.md`.
- Conventional commit messages.
- A GUI change is not verified until someone has watched it happen; say so rather than claim
  it works.
