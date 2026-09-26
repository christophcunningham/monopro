# Local patches

This directory is upstream [`egui_tiles` 0.16.0][crate] (MIT OR Apache-2.0, see
`LICENSE-MIT` and `LICENSE-APACHE`), vendored so monopro can change what the
`Behavior` trait does not reach. It is wired in with `[patch.crates-io]` in the
workspace `Cargo.toml`. There are two changes, both marked `LOCAL PATCH` in the
source.

[crate]: https://crates.io/crates/egui_tiles/0.16.0

## 1. Tab-bar arrows snap to tabs

### Why

When a panel is narrower than its tab names, egui_tiles shows ◀ ▶ arrows that
scroll a third of the bar's width per click. At a narrow panel that is a few
characters, so clicking the arrow barely moves and never lands on the tab you
could not see. The step is hard-coded in the private `ScrollState` and cannot be
changed from the `Behavior` trait.

### Delta

All in `src/container/tabs.rs`:

- `ScrollState` records each visible tab's left edge (`tab_lefts`) and the
  arrow width as laid out. It stops being `Copy` because of the `Vec`.
- The arrows call `ScrollState::snap`, which moves `offset` straight to the
  next tab's left edge in that direction, correcting for the offset shift
  `update` applies when the left arrow appears or disappears.
- `scroll_increment` is gone.

## 2. The behavior answers a seam double-click

### Why

Double-clicking a seam in monopro puts the side panel back at its shipped width.
Upstream evens out the two sides of the seam instead, and there is no hook to
change that. monopro used to repair it afterwards by re-deriving from the
pointer position which seam had been hit — and got that wrong wherever egui's
hit test reached further than the geometry did: a double-click a few points
into a panel (on its scroll bar, say) still landed on the seam through egui's
`interact_radius`, upstream evened it out, the repair missed, and the panel went
to half the window.

### Delta

- `Behavior::on_seam_double_click` in `src/behavior.rs`, defaulting to `false`
  (upstream behavior).
- `resize_interaction` in `src/container/linear.rs` takes the tiles and the
  container's direction and asks the behavior before evening out the split. Its
  two callers pass them.

## Updating

Nothing else is changed. To update, re-vendor the new release and re-apply
these edits (search for `LOCAL PATCH`).
