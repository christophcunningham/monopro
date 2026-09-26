# AGENTS.md

monopro is a monochrome RAW processor: one Cargo workspace of four crates that builds
one binary, `monopro`, which opens a window or runs headlessly from the command line.
`vendor/` holds two patched dependencies; they are not workspace members, and each
explains its patch in `LOCAL-PATCH.md`.

This file is orientation, not a rulebook. It lists the facts about this repository
that are easy to get wrong. Beyond them, use judgement and imagination, and propose a better
structure when you see one.

## Architecture

Read [ARCHITECTURE.md](ARCHITECTURE.md) before moving code or adding a subsystem. It
maps every file and says where each kind of change belongs.

Anything that can be tested without a window belongs as far left as it can go. Every
source file opens with a comment saying what it is and why. Read it before changing
the file, and keep it true afterwards.

## Language

Always use American English spelling (color, center, initialize, gray) in code, comments, variable names, and user visable copy.

## Build and test

Rust 1.92, edition 2024. Before a commit:

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

While iterating, `cargo test -p <crate> <name>` is faster.

- `./run` builds and launches the app: `--release` to judge speed, `--profile <name>`
  to keep its remembered state apart from the real app's.
- `cargo test -p raw-app visual -- --ignored` draws frames of the real interface into
  `target/visual/`, for checking a UI change by eye.
- Without a GPU adapter, `raw-gpu`'s render tests return early and count as passed.
  Their `no GPU adapter; skipping` notice shows only with `-- --nocapture`. Camera
  tests skip or are ignored without the private corpus (`docs/private-fixtures.md`).
- Code behind `cfg(not(target_os = "macos"))` is not compiled on a Mac. CI's Windows
  and Linux jobs are where it gets checked.
- When reporting, say what ran and what didn't: the GPU tests, Windows and Linux, the
  native menus.

## Speed and memory

Working images reach about 100 MP, and the frame loop runs every frame.

- Keep per-frame work cheap; move heavy work off the UI thread, as `decode.rs` and
  `zones.rs` do.
- Key a cache on exactly the inputs it reads, so an unrelated edit doesn't
  invalidate it.
- Check GPU allocations against device limits before wgpu sees them
  (`raw-gpu/src/limits.rs`).
- Judge speed with `./run --release` on a large raw; debug builds aren't
  representative.

CPU code that mirrors a shader (`display.rs` and `display.wgsl`, the zone basis and
the contrast mask) changes with it.

## Conventions

- A lint exemption is `#[expect(lint, reason = "…")]`, not a bare `#[allow]`.
- The sidecar (`<stem>.mono.xmp`) is a compatibility surface, written by hand. A
  change to its fields bumps `SCHEMA_VERSION`, and older files must keep opening.
- Comments name a milestone only once it is done; a forward reference names the
  thing instead. `raw-app/tests/milestone_references.rs` checks this.
- A user-visible change gets a line under **Unreleased** in `CHANGELOG.md`, in the
  same commit, as plain text, because it becomes the Sparkle release notes.
- Something monopro can't do goes in `docs/known-limitations.md`. Don't turn a
  failing check green by removing it.
- Commit `Cargo.lock`. An ignored advisory in `deny.toml` carries its reason.

## Git

Preserve unrelated working-tree changes, and stage explicit paths rather than
`git add -A`. A commit message is an imperative subject, then prose saying why.
