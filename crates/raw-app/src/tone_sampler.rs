//! The tone eyedropper's gesture: whether a press and a release were a click or a box, and
//! which points of the screen a box samples.
//!
//! # Why this is not in `tone_sample_tool`
//!
//! The reason `curve_mask` and `crop` are not inside the viewport: this is the part of
//! the tool that is arithmetic, and kept here it has tests that name the cases. What the
//! points *read*, and what a sample does to a range, is further left again, in
//! `raw_core::zone` and `raw_core::dodgeburn`; the tool itself only owns the pointer and
//! the painter.
//!
//! # A box is points on the screen
//!
//! The box is dragged over the picture as it is shown, and the negative under it may be
//! turned, straightened or corrected for perspective, so the box is not a rectangle of
//! the negative. Rather than work out what it is, the tool lays a lattice of points
//! across the box on screen and carries each one onto the negative the way a click is
//! carried. A click is the same thing with one point.

use egui::{Pos2, Rect};

/// How far a release may be from its press, in points, and still be a click. A hand
/// moves a little between the two, and a box that small would sample one pixel anyway.
pub const CLICK_SLOP: f32 = 4.0;

/// The most points a box is sampled at along one side.
///
/// The picture is read from a proxy 480 pixels on its long edge, so a box across the
/// whole frame crosses at most 480 of them: 48 a side is every tenth, more than a
/// percentile needs, and bounds a release at a few thousand reads however big the box.
pub const LATTICE: usize = 48;

/// The spacing a lattice aims for, in points, before [`LATTICE`] caps it.
const PITCH: f32 = 3.0;

/// Whether a press at `anchor` released at `at` drew a box rather than clicked.
pub fn is_box(anchor: Pos2, at: Pos2) -> bool {
    let d = at - anchor;
    d.x.abs().max(d.y.abs()) > CLICK_SLOP
}

/// The screen points one gesture samples: the release point for a click, a lattice
/// across the box for a drag.
///
/// The lattice sits at the centers of an even grid of cells, so no point is on the
/// box's own edge, where a box drawn up to the border of a subject would read the
/// background beside it.
pub fn points(anchor: Pos2, at: Pos2) -> Vec<Pos2> {
    if !is_box(anchor, at) {
        return vec![at];
    }
    let rect = Rect::from_two_pos(anchor, at);
    let along = |extent: f32| ((extent / PITCH).ceil() as usize).clamp(1, LATTICE);
    let (nx, ny) = (along(rect.width()), along(rect.height()));
    (0..ny)
        .flat_map(|j| (0..nx).map(move |i| (i, j)))
        .map(|(i, j)| {
            rect.min
                + egui::vec2(
                    rect.width() * (i as f32 + 0.5) / nx as f32,
                    rect.height() * (j as f32 + 0.5) / ny as f32,
                )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    #[test]
    fn a_release_near_its_press_is_a_click_at_the_release() {
        let press = pos2(100.0, 80.0);
        for at in [press, pos2(103.0, 78.0), pos2(96.5, 84.0)] {
            assert!(!is_box(press, at));
            assert_eq!(points(press, at), vec![at]);
        }
        assert!(
            is_box(press, pos2(105.0, 80.0)),
            "past the slop on one axis is a box"
        );
    }

    #[test]
    fn a_box_is_sampled_across_itself_and_never_on_its_edge() {
        // Dragged up and to the left, to check the corners are taken either way round.
        let (press, at) = (pos2(200.0, 150.0), pos2(140.0, 120.0));
        let sampled = points(press, at);
        assert_eq!(sampled.len(), 20 * 10, "a 60 by 30 box at a 3pt pitch");
        let rect = Rect::from_two_pos(press, at);
        for p in &sampled {
            assert!(p.x > rect.left() && p.x < rect.right(), "{p:?}");
            assert!(p.y > rect.top() && p.y < rect.bottom(), "{p:?}");
        }
        // Evenly: the mean of the points is the middle of the box.
        let n = sampled.len() as f32;
        let mean = sampled
            .iter()
            .fold(egui::Vec2::ZERO, |sum, p| sum + p.to_vec2())
            / n;
        assert!((mean - rect.center().to_vec2()).length() < 1e-3);
    }

    #[test]
    fn a_box_across_the_whole_screen_is_still_a_bounded_number_of_reads() {
        let sampled = points(pos2(0.0, 0.0), pos2(3000.0, 2000.0));
        assert_eq!(sampled.len(), LATTICE * LATTICE);
        // A sliver is a row of points, not nothing.
        let sliver = points(pos2(0.0, 0.0), pos2(300.0, 1.0));
        assert_eq!(sliver.len(), LATTICE);
        assert!(sliver.iter().all(|p| (p.y - 0.5).abs() < 1e-6));
    }
}
