# Local patch: tab-bar arrows snap to tabs

This directory is upstream [`egui_tiles` 0.16.0][crate] (MIT OR Apache-2.0, see
`LICENSE-MIT` and `LICENSE-APACHE`), vendored so monopro can change how a tab
bar's scroll arrows move. It is wired in with `[patch.crates-io]` in the
workspace `Cargo.toml`.

[crate]: https://crates.io/crates/egui_tiles/0.16.0

## Why

When a panel is narrower than its tab names, egui_tiles shows ◀ ▶ arrows that
scroll a third of the bar's width per click. At a narrow panel that is a few
characters, so clicking the arrow barely moves and never lands on the tab you
could not see. The step is hard-coded in the private `ScrollState` and cannot be
changed from the `Behavior` trait.

## Delta

All in `src/container/tabs.rs`:

- `ScrollState` records each visible tab's left edge (`tab_lefts`) and the
  arrow width as laid out. It stops being `Copy` because of the `Vec`.
- The arrows call `ScrollState::snap`, which moves `offset` straight to the
  next tab's left edge in that direction, correcting for the offset shift
  `update` applies when the left arrow appears or disappears.
- `scroll_increment` is gone.

Nothing else is changed. To update, re-vendor the new release and re-apply
these three edits (search for `LOCAL PATCH`).
