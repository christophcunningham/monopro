//! Snapshots: a look you can come back to, and the pins that feed the compare grid.
//!
//! **There is no capture machinery here, and that is the point.** A snapshot is a
//! `Params` and a picture of it. `Params` has been `Clone + PartialEq` from the
//! start, expressly so that "undo, duplication, and serialization" are cheap — and a
//! snapshot is the fourth thing on that list. `tab.params.clone()` *is* the capture;
//! what it is a capture *of* is [`raw_core::Params::restore_look_from`], which owns the
//! one decision in this file that took a decision.
//!
//! # Session-only, by the maintainer's decision
//!
//! The prototype does not persist snapshots and this does not either. `Params`
//! serialises whole and the sidecar is versioned, so persisting them would have been
//! easy — which is exactly why it was worth deciding rather than drifting into. The
//! argument that won: **a snapshot is a scratch comparison**, taken to answer a question
//! you are asking now, and twenty of them in a sidecar makes that file a different kind
//! of document. It also drags the thumbnails onto disk or forces them to be re-rendered
//! at open, neither of which anybody asked for.
//!
//! # What actually costs memory
//!
//! Brush payload is shared with history and the live look until edited. The remaining
//! visible cost is the **thumbnails**, which is why they are reduced at capture rather
//! than held at viewport size. A 1600×1100 read-back is 7 MB; the same picture at
//! [`THUMB_MAX`] is under 90 KB. There is deliberately **no cap on the count** — a
//! limit you hit while working is worse than memory you can see — but the reduction is
//! what makes that affordable, so it is not an optimisation to remove.

use raw_core::Params;

/// The longest side of a stored thumbnail, in pixels.
///
/// Sized for the snapshot list, which is the only place it is drawn. Compare's cells
/// want something else entirely — `Viewport::patch` at 1:1 — and the two should not
/// share an answer merely because both are "a picture of a snapshot".
///
/// Generous rather than exact: the list is in a resizable pane and a thumbnail that has
/// to be upscaled to fill its row looks like a mistake, where one drawn at half its
/// stored size merely looks sharp.
pub const THUMB_MAX: u32 = 320;

/// One captured look.
pub struct Snapshot {
    /// 1-based, per tab, and **never reused** — deleting `v002` does not free the name.
    /// The label is what you read; the index is what makes two snapshots taken either
    /// side of a delete distinguishable.
    pub index: u32,
    /// `v001` until renamed. Editable in place, like a Dodge & Burn layer.
    pub label: String,
    /// The whole `Params` as it stood, including the parts a restore will not apply.
    ///
    /// **Stored whole rather than pre-filtered**, so the record is of what was actually
    /// on screen. `restore_look_from` decides what comes back, and keeping the rest
    /// means a snapshot can later say "this was taken under a different demosaic"
    /// without needing to have predicted the question.
    pub params: Params,
    /// The complete composed photograph, fitted and reduced. It includes the stored
    /// crop but no viewport zoom, pan, canvas, Surround or diagnostic overlay.
    /// `None` if the render/read-back failed—which is a missing thumbnail and not a
    /// missing snapshot, so it is an `Option` rather than a reason to refuse capture.
    pub thumb: Option<egui::TextureHandle>,
    /// Shown in the compare grid.
    pub pinned: bool,
}

/// A tab's snapshots, newest first.
///
/// Per tab rather than per app, because a snapshot is a version *of an image* — which
/// is also what makes the compare grid affordable: every cell is the same decode under
/// different parameters, so there is one luminance texture and not four.
#[derive(Default)]
pub struct Snapshots {
    /// **Newest first.** The prototype's order, and the right one: the list is read
    /// from the top and the one you just took is the one you are working against.
    items: Vec<Snapshot>,
    /// How many have ever been captured, so `index` never repeats.
    taken: u32,
}

impl Snapshots {
    /// How many of the first captures are pinned automatically.
    ///
    /// The prototype's rule, and it earns its place: compare shows *pinned* snapshots,
    /// so without it the first press of `k` would open an empty grid and the feature
    /// would look broken at exactly the moment it is first tried. Four because four is
    /// what the grid holds.
    pub const AUTO_PIN: u32 = 4;

    /// The most cells the grid can show.
    pub const MAX_CELLS: usize = 4;

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Snapshot> {
        self.items.iter()
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut Snapshot> {
        self.items.get_mut(i)
    }

    /// The snapshots the compare grid shows, in list order, capped at [`Self::MAX_CELLS`].
    ///
    /// Capped here rather than at the call site so the grid, the layout buttons and the
    /// status line cannot disagree about how many there are.
    pub fn pinned(&self) -> Vec<&Snapshot> {
        self.items
            .iter()
            .filter(|s| s.pinned)
            .take(Self::MAX_CELLS)
            .collect()
    }

    /// What each pinned snapshot renders as: its params with module bypass resolved.
    /// The stored params keep the bypassed values, as the live edit does.
    pub fn pinned_looks(&self) -> Vec<Params> {
        self.pinned().iter().map(|s| s.params.effective()).collect()
    }

    pub fn pinned_count(&self) -> usize {
        self.pinned().len()
    }

    /// Capture `params`, with `thumb` as its complete composed photograph.
    ///
    /// Returns the new snapshot's label, for the status line.
    pub fn capture(&mut self, params: Params, thumb: Option<egui::TextureHandle>) -> String {
        self.taken += 1;
        let label = format!("v{:03}", self.taken);
        self.items.insert(
            0,
            Snapshot {
                index: self.taken,
                label: label.clone(),
                params,
                thumb,
                pinned: self.taken <= Self::AUTO_PIN,
            },
        );
        label
    }

    /// Swap the *n*th and *m*th **pinned** snapshots within the list.
    ///
    /// Compare's cells are the pinned subset in list order, so a drag in the grid is a
    /// move in this list — which is what keeps the grid and the panel agreeing about
    /// which `v003` is which. Taking pinned positions rather than list positions means
    /// the caller can speak in cells and never has to know where the unpinned ones sit.
    pub fn swap_pinned(&mut self, a: usize, b: usize) {
        let at: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, s)| s.pinned)
            .map(|(i, _)| i)
            .take(Self::MAX_CELLS)
            .collect();
        if let (Some(&x), Some(&y)) = (at.get(a), at.get(b))
            && x != y
        {
            self.items.swap(x, y);
        }
    }

    /// Drop one, by list position.
    pub fn remove(&mut self, i: usize) {
        if i < self.items.len() {
            self.items.remove(i);
        }
    }

    /// Pin or unpin, refusing a pin that would exceed the grid.
    ///
    /// **Refuses rather than evicting.** Silently unpinning somebody else's choice to
    /// make room is the kind of helpfulness that loses work, and the alternative — a
    /// fifth pin that the grid then cannot show — is a control that appears to do
    /// something and does not. Returns whether the state changed, so a refusal can say
    /// so instead of looking like a dead click.
    pub fn toggle_pin(&mut self, i: usize) -> bool {
        let full = self.pinned_count() >= Self::MAX_CELLS;
        match self.items.get_mut(i) {
            Some(s) if s.pinned => {
                s.pinned = false;
                true
            }
            Some(s) if !full => {
                s.pinned = true;
                true
            }
            _ => false,
        }
    }
}

/// Reduce a read-back to a thumbnail: box-average down to at most [`THUMB_MAX`] on the
/// long side, and **never up**.
///
/// A box average rather than nearest, because the thing being reduced is a photograph
/// and point-sampling a 1600px picture to 320 is aliasing, not scaling — on this corpus
/// it turns fine detail into a shimmer that reads as noise in the list.
///
/// Returns `None` for an empty image rather than panicking on a zero divisor, because
/// the caller is a keypress and a snapshot of a tab mid-resize should decline rather
/// than take the app down.
pub fn reduce(w: u32, h: u32, rgba: &[u8]) -> Option<egui::ColorImage> {
    if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
        return None;
    }
    // Integer factor, so every output pixel averages the same number of inputs and no
    // row or column is weighted differently from its neighbours. A fractional resample
    // would be better and is not worth a filter here: this is a 320px picture of a
    // picture, and the alternative to a slightly-too-large thumbnail is a resampler
    // that has to be right about phase.
    let factor = (w.max(h).div_ceil(THUMB_MAX)).max(1);
    if factor == 1 {
        return Some(egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            &rgba[..(w * h * 4) as usize],
        ));
    }
    let (ow, oh) = ((w / factor).max(1), (h / factor).max(1));
    let mut out = Vec::with_capacity((ow * oh * 4) as usize);
    for oy in 0..oh {
        for ox in 0..ow {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for dy in 0..factor {
                for dx in 0..factor {
                    // Clamped rather than skipped: the last output pixel of a row whose
                    // width is not a whole multiple would otherwise average fewer
                    // samples and come out with a different weight from its neighbours.
                    let sx = (ox * factor + dx).min(w - 1);
                    let sy = (oy * factor + dy).min(h - 1);
                    let i = ((sy * w + sx) * 4) as usize;
                    r += rgba[i] as u32;
                    g += rgba[i + 1] as u32;
                    b += rgba[i + 2] as u32;
                }
            }
            let n = factor * factor;
            out.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8, 255]);
        }
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [ow as usize, oh as usize],
        &out,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w x h` image where every pixel carries `v`.
    fn flat(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; (w * h * 4) as usize]
    }

    #[test]
    fn a_compared_snapshot_leaves_out_the_modules_it_had_bypassed() {
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        p.exposure.enabled = false;
        let mut snapshots = Snapshots::default();
        snapshots.capture(p, None);
        snapshots.capture(Params::default(), None);
        for s in &mut snapshots.items {
            s.pinned = true;
        }

        let looks = snapshots.pinned_looks();
        assert!(
            looks.iter().all(|look| look.exposure.ev == 0.0),
            "a bypassed exposure was drawn in the compare grid"
        );
        assert!(
            snapshots
                .pinned()
                .iter()
                .any(|s| s.params.exposure.ev == 1.5),
            "resolving the bypass lost the stored edit"
        );
    }

    #[test]
    fn a_thumbnail_is_never_larger_than_the_cap_and_never_upscaled() {
        // Both directions, because they are two different mistakes: a cap that does not
        // bind puts a 7 MB texture in a list row, and one that "scales to fit" turns a
        // 40px preview into a blurred 320px one.
        let big = reduce(1600, 1100, &flat(1600, 1100, 128)).expect("reduces");
        assert!(
            big.size[0] <= THUMB_MAX as usize && big.size[1] <= THUMB_MAX as usize,
            "a {:?} thumbnail is over the {THUMB_MAX}px cap",
            big.size
        );
        let small = reduce(40, 30, &flat(40, 30, 128)).expect("reduces");
        assert_eq!(small.size, [40, 30], "a small read-back was upscaled");
    }

    #[test]
    fn a_thumbnail_keeps_the_picture_s_shape() {
        // The list draws these at a fixed height, so a thumbnail whose aspect drifted
        // would letterbox against its neighbours — visible immediately and easy to
        // introduce with an independent per-axis divisor.
        let (w, h) = (1600u32, 1000u32);
        let t = reduce(w, h, &flat(w, h, 90)).expect("reduces");
        let want = w as f32 / h as f32;
        let got = t.size[0] as f32 / t.size[1] as f32;
        assert!(
            (got - want).abs() < 0.02,
            "aspect drifted: {want} became {got}"
        );
    }

    #[test]
    fn reducing_averages_rather_than_samples() {
        // The claim that makes this a box filter and not a decimation, and it needs a
        // pattern a point-sampler would get *wrong* rather than merely different: a
        // checkerboard of 0 and 200 averages to 100 everywhere, while nearest-neighbour
        // returns one or the other and never the mean.
        let (w, h) = (640u32, 640u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 200 } else { 0 };
                let i = ((y * w + x) * 4) as usize;
                src[i] = v;
                src[i + 1] = v;
                src[i + 2] = v;
                src[i + 3] = 255;
            }
        }
        let t = reduce(w, h, &src).expect("reduces");
        let mid = t.pixels[t.pixels.len() / 2];
        assert!(
            (mid.r() as i32 - 100).abs() <= 1,
            "a checkerboard reduced to {} — this is sampling one phase, not averaging",
            mid.r()
        );
    }

    #[test]
    fn reducing_declines_rather_than_panicking_on_nothing() {
        // The caller is a keypress. A zero-sized read-back is what a snapshot taken
        // during a pane resize looks like, and it must decline.
        assert!(reduce(0, 0, &[]).is_none());
        assert!(
            reduce(4, 4, &[0; 8]).is_none(),
            "a short buffer must not be indexed"
        );
    }

    #[test]
    fn the_first_four_captures_are_pinned_and_the_fifth_is_not() {
        // Without this the first press of `k` opens an empty grid and the feature looks
        // broken at the one moment it is being tried for the first time.
        let mut s = Snapshots::default();
        for _ in 0..5 {
            s.capture(Params::default(), None);
        }
        assert_eq!(s.pinned_count(), 4, "the auto-pin did not stop at four");
        // Newest first, so the *last* item is the first capture.
        assert!(
            !s.iter().next().expect("five taken").pinned,
            "the fifth was auto-pinned"
        );
    }

    #[test]
    fn labels_are_never_reused_after_a_delete() {
        // `v002` must mean one thing for the whole session. A counter reset to the list
        // length would hand the name of a deleted snapshot to a new one, and the two
        // would be indistinguishable in a screenshot or a note.
        let mut s = Snapshots::default();
        for _ in 0..3 {
            s.capture(Params::default(), None);
        }
        s.remove(0); // v003, the newest
        let next = s.capture(Params::default(), None);
        assert_eq!(next, "v004", "a deleted label came back round");
    }

    #[test]
    fn swapping_two_cells_moves_the_snapshots_they_came_from() {
        // The grid's cells are the pinned subset in list order, so a drag in the grid
        // has to move the *list* — anything else and the panel and the grid disagree
        // about which `v003` is which.
        //
        // The unpinned rows in between are what makes this worth a test: the caller
        // speaks in cell positions, and cell 0 and cell 1 here are list rows 0 and 2.
        let mut s = Snapshots::default();
        for _ in 0..3 {
            s.capture(Params::default(), None);
        }
        s.toggle_pin(1); // v002, the middle one, off — leaving v003 and v001 pinned
        let pinned: Vec<String> = s.pinned().iter().map(|p| p.label.clone()).collect();
        assert_eq!(
            pinned,
            vec!["v003", "v001"],
            "the fixture is not what the test assumes"
        );

        s.swap_pinned(0, 1);
        let after: Vec<String> = s.pinned().iter().map(|p| p.label.clone()).collect();
        assert_eq!(after, vec!["v001", "v003"], "the cells did not swap");
        // And the row nobody dragged stayed where it was.
        let all: Vec<String> = s.iter().map(|p| p.label.clone()).collect();
        assert_eq!(
            all,
            vec!["v001", "v002", "v003"],
            "an unpinned row was moved too"
        );
    }

    #[test]
    fn a_fifth_pin_is_refused_rather_than_evicting_one() {
        // The grid holds four. Silently unpinning somebody else's choice to make room
        // is how a comparison you had set up disappears while you are looking at it.
        let mut s = Snapshots::default();
        for _ in 0..6 {
            s.capture(Params::default(), None);
        }
        assert_eq!(s.pinned_count(), 4);
        // Items 0 and 1 are the two newest, which the auto-pin did not reach.
        assert!(!s.toggle_pin(0), "a fifth pin was accepted");
        assert_eq!(s.pinned_count(), 4, "a refusal still changed the pins");
        assert!(s.toggle_pin(2), "unpinning must always be allowed");
        assert!(
            s.toggle_pin(0),
            "and it must make room for the pin that was refused"
        );
        assert_eq!(s.pinned_count(), 4);
    }
}
