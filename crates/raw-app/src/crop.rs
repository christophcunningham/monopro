//! The crop tool: hit-testing, the drag arithmetic, and the overlay.
//!
//! # Why this is not in `viewport_panel`
//!
//! The drag is the only genuinely difficult arithmetic in composition — eight
//! handles, an optional aspect lock, a minimum size and a frame to stay inside, all
//! interacting. Kept here it is a pure function of a rectangle and a delta, with
//! unit tests that name the cases; folded into the viewport it would be
//! forty untested lines inside a function that also owns zoom, pan, the readout and
//! the GPU dispatch.
//!
//! # Everything works in frame pixels
//!
//! `Rect` is normalised because that is how it is *stored* — resolution-independent
//! and angle-independent, so neither a change of sampling mode nor a nudge of the
//! straighten slider can move it. But a drag is a distance, and the ratio lock is a
//! relationship between two distances, so both need real pixels or the aspect would
//! come out as the frame's aspect times the ratio.
//!
//! `Frame::place` and `Frame::unplace` are the conversion, and they are the *only*
//! conversion — this module never divides by a frame dimension itself. That matters
//! because the two grids are concentric rather than aligned: the rectangle is
//! normalised against `oriented` and lands on `frame`, which is bigger whenever the
//! picture is straightened.

use raw_core::Frame;
use raw_core::composition::{IRect, Rect};

use crate::tabs::Handle;

/// Smallest crop, in frame pixels, on either axis.
///
/// Not a fraction: the point is that the rectangle stays big enough to *grab*, and
/// grabbability is a number of pixels on a screen rather than a proportion of a
/// negative. `Rect::clamped`'s own floor is a last-resort guard against a
/// hand-edited sidecar; this is the one a drag meets.
const MIN_PX: f32 = 32.0;

/// The rectangle in whole frame pixels, as four edges.
fn edges(r: Rect, frame: &Frame) -> (f32, f32, f32, f32) {
    let p = frame.place(r);
    (
        p.x as f32,
        p.y as f32,
        (p.x + p.w as i32) as f32,
        (p.y + p.h as i32) as f32,
    )
}

fn to_rect(x0: f32, y0: f32, x1: f32, y1: f32, frame: &Frame) -> Rect {
    frame.unplace(IRect {
        x: x0.round() as i32,
        y: y0.round() as i32,
        w: (x1 - x0).round().max(1.0) as u32,
        h: (y1 - y0).round().max(1.0) as u32,
    })
}

/// The crop's own aspect, in frame pixels.
fn aspect(r: Rect, frame: &Frame) -> f32 {
    let (x0, y0, x1, y1) = edges(r, frame);
    let h = (y1 - y0).max(1.0);
    ((x1 - x0) / h).max(1.0e-3)
}

/// Four edges in frame pixels, **unrounded** — the form the search below works in.
///
/// The rounding to whole pixels happens once, at [`to_rect`], and that is not a detail.
/// A search that rounded every trial would be searching a lattice whose spacing depends
/// on which way the picture is turned, and it would do so with a *bias*: at any given
/// scale the height is as likely to round down as up, and rounding down is what makes a
/// slightly larger rectangle fit. The search follows that bias, so a straighten drag —
/// a hundred small re-fits in a second — walks the crop's aspect off its own shape. It
/// went 3:2 to 1.64:1 over two seconds of slider, and read as "straighten squashes the
/// crop", which is not a thing any single frame of it does.
type Px = (f32, f32, f32, f32);

/// How many halvings the search gets.
///
/// The interval is one drag's motion, at most a few hundred frame pixels, so sixteen
/// halvings resolve it to a hundredth of a pixel — comfortably under the whole pixel
/// the answer is rounded to. A fixed count rather than a tolerance, because the useful
/// tolerance is in pixels and the parameter is a fraction of an interval whose length
/// the loop does not otherwise need to know.
const BISECT: u32 = 16;

/// Most whole frame pixels [`settle`] will give back. Three is slack over the one the
/// rounding can cost; past that something is wrong that a fourth pixel will not fix.
const BACKOFF: i32 = 3;

/// Is this rectangle, in continuous frame pixels, wholly on the picture?
///
/// **The same slack `Frame::covers` grants**, which is what makes the search and the
/// invariant one question rather than two that nearly agree.
fn fits(p: Px, frame: &Frame) -> bool {
    frame.covers_px(p.0, p.1, p.2, p.3, -0.5)
}

/// Round a searched rectangle to whole frame pixels without losing the invariant.
///
/// [`to_rect`] rounds the offset and the extent independently, so rounding is free to
/// put a corner the search had just placed on the picture back off it. **The obvious
/// answer — search with a pixel of clearance and let the rounding spend it — costs far
/// more than it sounds.** At a shallow angle the binding corner travels almost *along*
/// the picture's edge: at 6° a pixel of clearance measured at the corner costs fourteen
/// pixels of reach, and at 1° it costs fifty. That is a visible band of picture the
/// crop will not open onto, at exactly the angles a straighten tool is used at.
///
/// So round first, at full reach, and give whole pixels back only in the rare case
/// where the rounding actually cost something.
fn settle(p: Px, frame: &Frame) -> Rect {
    for back in 0..=BACKOFF {
        let b = back as f32;
        let r = to_rect(p.0 + b, p.1 + b, p.2 - b, p.3 - b, frame);
        if frame.covers(r) {
            return r;
        }
    }
    to_rect(p.0, p.1, p.2, p.3, frame)
}

/// The furthest along the way from `a` to `b` that still lands only on real pixels.
///
/// **This is the hard clamp.** A straightened frame is a rotated rectangle inside a
/// larger axis-aligned box, so "inside the picture" is not a bound on `x0, y0, x1, y1`
/// that can be written down — the set of axis-aligned rectangles that fit is convex,
/// but its boundary is a corner cutting diagonally across the frame. Searching along
/// the requested move is how one predicate serves all nine handles, the aspect lock,
/// the body slide and the straighten re-fit without any of them deriving the geometry a
/// second time.
///
/// **Interpolating the rectangle is what preserves a locked ratio.** `a` and `b` share
/// an anchor and an aspect, so every rectangle between them has that aspect too.
/// Clamping one edge on its own would keep the crop inside the picture and silently
/// un-lock it, which is the failure `a_locked_crop_stops_rather_than_quietly_unlocking`
/// already names at the frame's edge.
fn largest_fit(a: Px, b: Px, frame: &Frame) -> Px {
    let at = |t: f32| {
        (
            a.0 + (b.0 - a.0) * t,
            a.1 + (b.1 - a.1) * t,
            a.2 + (b.2 - a.2) * t,
            a.3 + (b.3 - a.3) * t,
        )
    };
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..BISECT {
        let m = 0.5 * (lo + hi);
        if fits(at(m), frame) {
            lo = m;
        } else {
            hi = m;
        }
    }
    at(lo)
}

/// Pull a proposed drag back onto the picture, from where it was to where it asked.
///
/// `prev` uncovered means the invariant was already broken — a hand-edited sidecar, or
/// a file saved before this rule — and then there is nothing to retreat towards. The
/// move is allowed through rather than frozen: a crop tool that will not move is
/// indistinguishable from one that has crashed, and the straighten re-fit pulls it back
/// the moment the angle is touched.
fn pull_back(prev: Rect, cand: Px, frame: &Frame) -> Rect {
    let from = edges(prev, frame);
    if fits(cand, frame) || !frame.covers(prev) {
        return to_rect(cand.0, cand.1, cand.2, cand.3, frame);
    }
    settle(largest_fit(from, cand, frame), frame)
}

/// Shrink `r` about its own centre until it lands only on real pixels.
///
/// The other half of the hard clamp: [`pull_back`] is for a gesture, which has a
/// direction to retreat along, and this is for everything that changes the picture
/// *under* a crop that was already settled — the straighten slider, the rotate ring,
/// the drawn horizon, a ratio preset, and the reset.
///
/// **About the crop's own centre, not the frame's**, so straightening does not also
/// move the subject. Shrinking is what keeps the aspect: scaling a rectangle about its
/// centre cannot change its shape, so a locked 1:1 stays 1:1 without the lock being
/// consulted at all.
///
/// # It re-fits; it does not iterate
///
/// The obvious implementation — search inward from the crop you have until it fits —
/// **ratchets**, and it took a test to see it. A straighten drag is a hundred small
/// changes of angle a second, so `confine` is asked a hundred times, each time about
/// the rectangle the last one returned. Every answer has to leave a pixel of clearance
/// for the rounding, and none of them can give a pixel back, so the crop walks inward
/// at a couple of pixels per re-fit whether the angle is still moving or not. Measured:
/// 13% of the picture over two seconds of slider, and it reads as "straighten crops far
/// more than it should" rather than as a loop, because no single frame of it is wrong.
///
/// So the size comes from [`Frame::inscribed`], which is a pure function of the angle
/// and the aspect. Asking it a hundred times gives the same rectangle a hundred times,
/// and the drag is stable by construction rather than by a tolerance. The crop is then
/// **slid** back toward where it actually was, as far as the picture allows — that is
/// what keeps `inscribed`'s recentring from moving the subject, and it is a search
/// because how far it can go is the same convex question everything else here asks.
///
/// Never larger than the crop that was there: this is a clamp, and a clamp that grew a
/// small off-centre crop to fill the frame because the angle moved would be doing
/// something nobody asked for.
pub fn confine(r: Rect, ratio: Option<f32>, frame: &Frame) -> Rect {
    if frame.covers(r) {
        return r;
    }
    let (x0, y0, x1, y1) = edges(r, frame);
    let ar = ratio
        .filter(|a| a.is_finite() && *a > 0.0)
        .unwrap_or_else(|| aspect(r, frame));

    // The centred answer for this angle and this shape, capped by what was there.
    let ins = edges(frame.inscribed(ar), frame);
    let w = (x1 - x0).min(ins.2 - ins.0);
    let (w, h) = (w, w / ar);
    let (fx, fy) = ((ins.0 + ins.2) * 0.5, (ins.1 + ins.3) * 0.5);
    let centred = (fx - w * 0.5, fy - h * 0.5, fx + w * 0.5, fy + h * 0.5);

    // ...then back toward the crop's own centre, because straightening should move the
    // subject as little as it has to.
    let (dx, dy) = ((x0 + x1) * 0.5 - fx, (y0 + y1) * 0.5 - fy);
    let wanted = (
        centred.0 + dx,
        centred.1 + dy,
        centred.2 + dx,
        centred.3 + dy,
    );
    settle(
        if fits(wanted, frame) {
            wanted
        } else {
            largest_fit(centred, wanted, frame)
        },
        frame,
    )
}

/// Move the crop by one frame's worth of pointer motion.
///
/// `d` is the pointer delta in **frame pixels**; `ratio` is the locked width/height,
/// also in frame pixels, or `None` for freeform.
///
/// The shape of it, in order, because each step depends on the last:
///
/// 1. move the edges this handle owns,
/// 2. impose the aspect lock on the axis the handle does *not* own,
/// 3. keep it at least [`MIN_PX`] on both axes,
/// 4. bring it back inside the frame — **scaling about the anchor** when a ratio is
///    locked, because clamping one axis on its own would break the lock, and a crop
///    that silently stops being 1:1 when it reaches the edge is worse than one that
///    stops growing,
/// 5. and then back inside the **picture**, through [`pull_back`], which is the step
///    that makes step 4's bound the bounding box rather than the answer. Both are
///    needed: 4 is exact and cheap and does all the work at 0°, and 5 is the one that
///    knows about a rotated frame's empty corners.
pub fn drag(r: Rect, handle: Handle, d: (f32, f32), ratio: Option<f32>, frame: &Frame) -> Rect {
    let (fw, fh) = (frame.frame.w as f32, frame.frame.h as f32);
    let (mut x0, mut y0, mut x1, mut y1) = edges(r, frame);

    if handle == Handle::Body {
        // Slide, never resize — and clamp by *shifting* rather than by trimming, or
        // dragging the picture into a corner would eat the crop instead of stopping
        // it. Two different gestures should not share a failure mode.
        //
        // **One axis at a time**, so that a diagonal push into a straightened corner
        // slides along the edge it reaches instead of stopping dead. Bisecting the
        // combined delta would stop both axes the moment either one ran out, and a
        // crop that will not travel along an edge it is already touching reads as the
        // tool having seized.
        let mut cur = r;
        for (dx, dy) in [(d.0, 0.0), (0.0, d.1)] {
            let (x0, y0, x1, y1) = edges(cur, frame);
            let (w, h) = (x1 - x0, y1 - y0);
            let nx = (x0 + dx).clamp(0.0, (fw - w).max(0.0));
            let ny = (y0 + dy).clamp(0.0, (fh - h).max(0.0));
            cur = pull_back(cur, (nx, ny, nx + w, ny + h), frame);
        }
        return cur;
    }

    let e = handle.edges();
    if e.left {
        x0 += d.0;
    }
    if e.right {
        x1 += d.0;
    }
    if e.top {
        y0 += d.1;
    }
    if e.bottom {
        y1 += d.1;
    }

    // Never let an edge cross its opposite. Without this a fast drag inverts the
    // rectangle and the crop reappears mirrored on the other side of the frame.
    let min_w = MIN_PX.min(fw);
    let min_h = MIN_PX.min(fh);
    if x1 - x0 < min_w {
        if e.left {
            x0 = x1 - min_w;
        } else {
            x1 = x0 + min_w;
        }
    }
    if y1 - y0 < min_h {
        if e.top {
            y0 = y1 - min_h;
        } else {
            y1 = y0 + min_h;
        }
    }

    if let Some(ar) = ratio.filter(|a| a.is_finite() && *a > 0.0) {
        let (horiz, vert) = (e.horizontal(), e.vertical());
        let (w, h) = (x1 - x0, y1 - y0);
        if horiz && vert {
            // A corner drives both axes at once, so the pointer is almost never on
            // the locked aspect. Take the **nearest** rectangle that is: the
            // orthogonal projection of `(w, h)` onto the line `h = w / ar`.
            //
            // The obvious alternative — let whichever axis moved further lead — is
            // wrong for a reason that only shows up in the hand. A drag arrives as
            // sixty small deltas a second, so "which moved further" is decided
            // afresh on each of them, and near a diagonal the leading axis flickers
            // between the two and the crop jitters. This is a function of the
            // rectangle rather than of one frame's delta, so it is smooth by
            // construction, and it is provably the closest the lock allows the
            // corner to get to the cursor.
            let proj = (w * ar + h) / (ar * ar + 1.0);
            let (w, h) = (proj * ar, proj);
            if e.left {
                x0 = x1 - w;
            } else {
                x1 = x0 + w;
            }
            if e.top {
                y0 = y1 - h;
            } else {
                y1 = y0 + h;
            }
        } else if horiz {
            // An edge drives one axis; the other grows symmetrically about its
            // centre, so a locked ratio does not walk the crop up the frame as you
            // widen it.
            let (cy, nh) = ((y0 + y1) * 0.5, w / ar);
            y0 = cy - nh * 0.5;
            y1 = cy + nh * 0.5;
        } else if vert {
            let (cx, nw) = ((x0 + x1) * 0.5, h * ar);
            x0 = cx - nw * 0.5;
            x1 = cx + nw * 0.5;
        }

        // Back inside the frame, keeping the lock. The anchor is whatever this
        // handle holds still: the opposite corner or edge, and the centre on any
        // axis the handle does not own.
        let ax = if e.left {
            x1
        } else if e.right {
            x0
        } else {
            (x0 + x1) * 0.5
        };
        let ay = if e.top {
            y1
        } else if e.bottom {
            y0
        } else {
            (y0 + y1) * 0.5
        };
        let mut s = 1.0f32;
        let fit = |v: f32, a: f32, lo: f32, hi: f32| -> f32 {
            if v < a && v < lo {
                (a - lo) / (a - v)
            } else if v > a && v > hi {
                (hi - a) / (v - a)
            } else {
                1.0
            }
        };
        s = s.min(fit(x0, ax, 0.0, fw)).min(fit(x1, ax, 0.0, fw));
        s = s.min(fit(y0, ay, 0.0, fh)).min(fit(y1, ay, 0.0, fh));
        let s = s.clamp(0.0, 1.0);
        x0 = ax + (x0 - ax) * s;
        x1 = ax + (x1 - ax) * s;
        y0 = ay + (y0 - ay) * s;
        y1 = ay + (y1 - ay) * s;
    } else {
        x0 = x0.clamp(0.0, fw);
        x1 = x1.clamp(0.0, fw);
        y0 = y0.clamp(0.0, fh);
        y1 = y1.clamp(0.0, fh);
    }

    pull_back(r, (x0, y0, x1, y1), frame)
}

/// Reshape the crop to `ratio`, keeping its centre and roughly its area.
///
/// What picking a preset does. Two rules that sound alike and are not:
///
/// - **Keep the area, not the width.** Reshaping a 3:2 crop to 1:1 by keeping the
///   width would grow it past the frame on a landscape file and past nothing useful
///   on a portrait one; keeping the area means "the same amount of picture, a
///   different shape", which is what the choice means.
/// - **Keep the centre**, so the subject does not move. Then bring it back inside
///   the frame by scaling about that centre — never by trimming an axis, which
///   would leave a crop that is not the ratio it just claimed to be.
pub fn fit_ratio(r: Rect, ratio: f32, frame: &Frame) -> Rect {
    if !ratio.is_finite() || ratio <= 0.0 {
        return r;
    }
    let (fw, fh) = (frame.frame.w as f32, frame.frame.h as f32);
    let (x0, y0, x1, y1) = edges(r, frame);
    let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    let area = ((x1 - x0) * (y1 - y0)).max(MIN_PX * MIN_PX);
    let (mut w, mut h) = ((area * ratio).sqrt(), (area / ratio).sqrt());
    // Shrink to fit, ratio intact. Centred, so both halves have to fit.
    let s = (fw / w).min(fh / h).min(1.0);
    w *= s;
    h *= s;
    // ...then slide, rather than trim, if the centre is too near an edge.
    let cx = cx.clamp(w * 0.5, fw - w * 0.5);
    let cy = cy.clamp(h * 0.5, fh - h * 0.5);
    // And then inside the picture, which on a straightened frame is smaller than the
    // box the two clamps above just fitted it to. `confine` shrinks about the centre,
    // so the ratio this function exists to impose survives the step that enforces it.
    confine(
        to_rect(
            cx - w * 0.5,
            cy - h * 0.5,
            cx + w * 0.5,
            cy + h * 0.5,
            frame,
        ),
        Some(ratio),
        frame,
    )
}

// ── Zones ────────────────────────────────────────────────────────────────────

/// What the pointer is over.
///
/// Zone sizes are the prototype's, in points: 18 for a corner, 28 for the rotate
/// ring just outside it, 14 along an edge. They overlap deliberately and the order
/// below resolves them — a corner beats the two edges meeting at it, because a grab
/// that resolved to an edge would resize one axis where the user aimed at two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// Not on the crop at all.
    Outside,
    /// Resize or move. `Handle::Body` is the interior.
    Grip(Handle),
    /// **Outside** a corner: rotate the picture under the box. Capture One's
    /// gesture, and the reason a crop tool does not need a separate rotate mode —
    /// the corner you would reach for to resize is the corner you reach past to
    /// straighten.
    Rotate(Handle),
}

const CORNER_HIT: f32 = 18.0;
const ROTATE_RING: f32 = 28.0;
const EDGE_HIT: f32 = 14.0;

/// Where the pointer is, relative to the crop box on screen.
pub fn hit(screen: egui::Rect, p: egui::Pos2) -> Zone {
    let at = |h: Handle| {
        let (u, v) = h.at();
        egui::pos2(
            screen.min.x + u * screen.width(),
            screen.min.y + v * screen.height(),
        )
    };

    for h in Handle::CORNERS {
        if (p - at(h)).length() <= CORNER_HIT {
            return Zone::Grip(h);
        }
    }
    // The rotate ring is only *outside* the box. Inside, the same distance from a
    // corner is a place you might reasonably want to move the crop from.
    if !screen.contains(p) {
        for h in Handle::CORNERS {
            if (p - at(h)).length() <= ROTATE_RING {
                return Zone::Rotate(h);
            }
        }
    }
    for h in Handle::EDGES {
        if (p - at(h)).length() <= EDGE_HIT.max(CORNER_HIT) {
            return Zone::Grip(h);
        }
    }
    // Anywhere along an edge, not only at its midpoint tick: the tick marks the
    // edge, it is not the only place you may grab it.
    let near = |a: f32, b: f32| (a - b).abs() <= EDGE_HIT;
    let within_y = p.y >= screen.min.y - EDGE_HIT && p.y <= screen.max.y + EDGE_HIT;
    let within_x = p.x >= screen.min.x - EDGE_HIT && p.x <= screen.max.x + EDGE_HIT;
    if within_y && near(p.x, screen.min.x) {
        return Zone::Grip(Handle::W);
    }
    if within_y && near(p.x, screen.max.x) {
        return Zone::Grip(Handle::E);
    }
    if within_x && near(p.y, screen.min.y) {
        return Zone::Grip(Handle::N);
    }
    if within_x && near(p.y, screen.max.y) {
        return Zone::Grip(Handle::S);
    }
    if screen.contains(p) {
        return Zone::Grip(Handle::Body);
    }
    Zone::Outside
}

// ── The overlay ──────────────────────────────────────────────────────────────

/// Warm amber, from the prototype's palette (`accent` / `accent_bright`).
///
/// **Deliberately not the app's ruby.** Ruby means "this module would render
/// differently than at defaults" everywhere else in the chrome, and a crop border is
/// not that — it is a tool, present only while the tool is open.
///
/// The stronger argument is that this app is monochrome: everything in the picture
/// is grey, so *any* hue reads unambiguously as chrome rather than as content. That
/// is a property a greyscale app has and a colour one does not, and the prototype
/// spent it on amber.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0xC8, 0xA9, 0x6E);
const ACCENT_BRIGHT: egui::Color32 = egui::Color32::from_rgb(0xE0, 0xC0, 0x7A);

/// Which guide is drawn inside the crop. The prototype's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Guide {
    #[default]
    Thirds,
    Golden,
    Diagonals,
    Grid,
    None,
}

impl Guide {
    /// **No Golden Spiral.** The prototype has one and the maintainer asked for it out: it is
    /// a sampled logarithmic curve that only lands on the composition it promises
    /// when the crop happens to be φ:1, and at any other shape it is a decoration
    /// that looks like a rule.
    pub const ORDER: [Self; 5] = [
        Self::Thirds,
        Self::Golden,
        Self::Diagonals,
        Self::Grid,
        Self::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Thirds => "Rule of Thirds",
            Self::Golden => "Golden Ratio",
            Self::Diagonals => "Diagonals",
            Self::Grid => "Grid",
            Self::None => "None",
        }
    }
}

/// Everything the overlay needs to draw itself.
///
/// A struct rather than nine arguments, because every one of them is a different
/// kind of thing and a call site with nine positional parameters is a call site
/// where two of them get swapped.
pub struct Overlay {
    /// The whole image area, in points.
    pub viewport: egui::Rect,
    /// The crop box, in points.
    pub screen: egui::Rect,
    /// What the pointer is over, for hover highlighting.
    pub hover: Zone,
    /// A drag is in progress: lighten the veil so what is being excluded can still
    /// be judged.
    pub dragging: bool,
    /// A rotate or straighten is in progress: the guides give way to the alignment
    /// grid.
    pub levelling: bool,
    pub guide: Guide,
    /// Degrees, for the corner readout.
    pub angle: f32,
    /// The crop's output size, for the readout inside the box.
    pub dims: (u32, u32),
    /// The straighten tool's line, while one is being drawn.
    pub line: Option<(egui::Pos2, egui::Pos2)>,
}

/// Draw a segment twice — a wide dark pass under a thin light one.
///
/// darktable's trick, and it is not decoration: a single grey line in a **monochrome**
/// app has nothing to contrast against over matching midtones, so a guide vanishes
/// exactly where the picture is most interesting. Two passes read over any tonality.
fn dual(painter: &egui::Painter, a: egui::Pos2, b: egui::Pos2, dark: u8, light: u8) {
    painter.line_segment(
        [a, b],
        egui::Stroke::new(1.8, egui::Color32::from_black_alpha(dark)),
    );
    painter.line_segment(
        [a, b],
        egui::Stroke::new(0.8, egui::Color32::from_white_alpha(light)),
    );
}

/// Draw the crop box over the image.
///
/// The picture outside the crop is **dimmed, not hidden**, which is the whole
/// argument for rendering the uncropped frame while the tool is open: you can see
/// what you are excluding and reach back out for it. Hiding it would make the tool a
/// preview of a decision already taken.
pub fn overlay(painter: &egui::Painter, o: &Overlay) {
    let screen = o.screen;
    // Four bands rather than one rectangle with a hole: egui has no even-odd fill,
    // and four `rect_filled` calls are cheaper than a mesh.
    //
    // **Lightened during a drag**, the prototype's behaviour and Lightroom's: while
    // you are moving an edge, the thing you need to see is what the box is about to
    // exclude, and 115 alpha over it is too dark to judge.
    let veil = egui::Color32::from_black_alpha(if o.dragging { 45 } else { 115 });
    let (vp, c) = (o.viewport, screen.intersect(o.viewport));
    for band in [
        egui::Rect::from_min_max(vp.min, egui::pos2(vp.max.x, c.min.y)),
        egui::Rect::from_min_max(egui::pos2(vp.min.x, c.max.y), vp.max),
        egui::Rect::from_min_max(egui::pos2(vp.min.x, c.min.y), egui::pos2(c.min.x, c.max.y)),
        egui::Rect::from_min_max(egui::pos2(c.max.x, c.min.y), egui::pos2(vp.max.x, c.max.y)),
    ] {
        if band.is_positive() {
            painter.rect_filled(band, 0.0, veil);
        }
    }

    // Guides inside the box — or, while levelling, a screen-fixed lattice across the
    // whole viewport. That swap is the point of it: a spirit level has to be fixed
    // to the room, not to the thing being levelled, and features *outside* the crop
    // are usually the straightest edges available.
    if o.levelling {
        const STEP: f32 = 32.0;
        let mut x = vp.min.x + STEP;
        while x < vp.max.x {
            dual(
                painter,
                egui::pos2(x, vp.min.y),
                egui::pos2(x, vp.max.y),
                70,
                100,
            );
            x += STEP;
        }
        let mut y = vp.min.y + STEP;
        while y < vp.max.y {
            dual(
                painter,
                egui::pos2(vp.min.x, y),
                egui::pos2(vp.max.x, y),
                70,
                100,
            );
            y += STEP;
        }
    } else {
        for [a, b] in guide_segments(o.guide, screen) {
            dual(painter, a, b, 110, 150);
        }
    }

    painter.rect_stroke(
        screen,
        0.0,
        egui::Stroke::new(1.0, ACCENT),
        egui::StrokeKind::Inside,
    );

    // Corner brackets and edge ticks, not dots. A bracket names the corner it hugs
    // without covering the picture at it, and it scales with the box: a fixed-size
    // dot is enormous on a small crop and lost on a large one.
    let arm = (screen.width() + screen.height()).max(1.0) * 0.5 * 0.12;
    let arm = arm.clamp(8.0, 22.0);
    let tick = arm * 0.6;
    let stroke = egui::Stroke::new(2.0, ACCENT_BRIGHT);
    let hot = egui::Stroke::new(2.6, egui::Color32::from_white_alpha(235));

    for h in Handle::CORNERS {
        let (u, v) = h.at();
        let at = egui::pos2(
            screen.min.x + u * screen.width(),
            screen.min.y + v * screen.height(),
        );
        // Each arm runs back along the two edges meeting here.
        let sx = if u == 0.0 { 1.0 } else { -1.0 };
        let sy = if v == 0.0 { 1.0 } else { -1.0 };
        let lit = matches!(o.hover, Zone::Grip(g) | Zone::Rotate(g) if g == h);
        let pen = if lit { hot } else { stroke };
        painter.line_segment([at, egui::pos2(at.x + sx * arm, at.y)], pen);
        painter.line_segment([at, egui::pos2(at.x, at.y + sy * arm)], pen);
    }

    for h in Handle::EDGES {
        let (u, v) = h.at();
        let at = egui::pos2(
            screen.min.x + u * screen.width(),
            screen.min.y + v * screen.height(),
        );
        let along = if matches!(h, Handle::N | Handle::S) {
            egui::vec2(tick * 0.5, 0.0)
        } else {
            egui::vec2(0.0, tick * 0.5)
        };
        let lit = o.hover == Zone::Grip(h);
        if lit {
            // The whole edge, so it is unambiguous which one is about to move.
            let (a, b) = match h {
                Handle::N => (screen.left_top(), screen.right_top()),
                Handle::S => (screen.left_bottom(), screen.right_bottom()),
                Handle::W => (screen.left_top(), screen.left_bottom()),
                _ => (screen.right_top(), screen.right_bottom()),
            };
            painter.line_segment(
                [a, b],
                egui::Stroke::new(1.4, egui::Color32::from_white_alpha(200)),
            );
        }
        painter.line_segment([at - along, at + along], if lit { hot } else { stroke });
    }

    // The output size, tucked inside the top-left corner. For a printmaking tool the
    // pixel count is a first-class fact, not a detail for a panel — it is the number
    // that decides whether the crop can be printed at the size you want.
    let text = format!("{} × {} px", o.dims.0, o.dims.1);
    let font = egui::FontId::monospace(9.0);
    let at = screen.min + egui::vec2(8.0, 14.0);
    painter.text(
        at + egui::vec2(1.0, 1.0),
        egui::Align2::LEFT_TOP,
        &text,
        font.clone(),
        egui::Color32::from_black_alpha(200),
    );
    painter.text(
        at,
        egui::Align2::LEFT_TOP,
        &text,
        font.clone(),
        egui::Color32::from_white_alpha(210),
    );

    // The angle, at the bottom-left corner, once there is one worth reporting.
    if o.angle.abs() > 0.05 {
        painter.text(
            screen.left_bottom() + egui::vec2(4.0, -4.0),
            egui::Align2::LEFT_BOTTOM,
            format!("{:+.2}°", o.angle),
            font.clone(),
            ACCENT,
        );
    }

    // The straighten tool's line, while it is being drawn.
    if let Some((a, b)) = o.line {
        dashed(painter, a, b, ACCENT_BRIGHT);
        let d = b - a;
        if d.x.abs() > 4.0 || d.y.abs() > 4.0 {
            let deg = (-d.y).atan2(d.x).to_degrees();
            painter.text(
                a + d * 0.5 - egui::vec2(0.0, 14.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{deg:+.1}°"),
                font,
                ACCENT_BRIGHT,
            );
        }
    }
}

/// A dashed segment. egui has no dash pattern, so it is stepped by hand.
fn dashed(painter: &egui::Painter, a: egui::Pos2, b: egui::Pos2, colour: egui::Color32) {
    const DASH: f32 = 6.0;
    let d = b - a;
    let len = d.length();
    if len < 1.0 {
        return;
    }
    let step = d / len * DASH;
    let n = (len / DASH) as i32;
    let stroke = egui::Stroke::new(1.5, colour);
    for i in (0..n).step_by(2) {
        let p0 = a + step * i as f32;
        painter.line_segment([p0, p0 + step], stroke);
    }
}

/// The guide's segments, in screen points.
///
/// Normalised `(u, v)` mapped onto the box, so every guide follows it automatically
/// and adding one is a list of fractions rather than a drawing routine.
fn guide_segments(guide: Guide, r: egui::Rect) -> Vec<[egui::Pos2; 2]> {
    let at = |u: f32, v: f32| egui::pos2(r.min.x + u * r.width(), r.min.y + v * r.height());
    let mut segs = Vec::new();
    let mut cross = |ts: &[f32]| {
        for &t in ts {
            segs.push([at(t, 0.0), at(t, 1.0)]);
            segs.push([at(0.0, t), at(1.0, t)]);
        }
    };
    const PHI: f32 = 0.618_034;
    match guide {
        Guide::None => {}
        Guide::Thirds => cross(&[1.0 / 3.0, 2.0 / 3.0]),
        Guide::Golden => cross(&[PHI, 1.0 - PHI]),
        Guide::Grid => cross(&[0.25, 0.5, 0.75]),
        Guide::Diagonals => {
            segs.push([at(0.0, 0.0), at(1.0, 1.0)]);
            segs.push([at(1.0, 0.0), at(0.0, 1.0)]);
        }
    }
    segs
}

/// The angle a drawn line implies, in degrees clockwise — what to set `straighten`
/// to so that line becomes level.
///
/// Wrapped into `±45`, so a line drawn down a **vertical** — a doorframe, a post —
/// straightens against the vertical instead of asking for a quarter turn. That is
/// the prototype's rule and it is what makes the tool usable indoors.
///
/// `None` for a line too short to have a direction; a stray click must not level the
/// picture to whatever two adjacent pixels imply.
pub fn angle_of(a: egui::Pos2, b: egui::Pos2) -> Option<f32> {
    let d = b - a;
    if d.x.abs() <= 4.0 && d.y.abs() <= 4.0 {
        return None;
    }
    let mut deg = (-d.y).atan2(d.x).to_degrees();
    while deg > 45.0 {
        deg -= 90.0;
    }
    while deg < -45.0 {
        deg += 90.0;
    }
    Some(deg)
}

/// Snap a rotation to level.
///
/// Within a third of a degree of straight, take straight — the gesture cannot
/// reliably express less than that, and a picture left at 0.08° is a picture nobody
/// meant to leave tilted. Hold shift to mean it.
pub fn snap(deg: f32, shift: bool) -> f32 {
    if !shift && deg.abs() < 0.35 { 0.0 } else { deg }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 600x400 picture, unstraightened — so the frame and the oriented picture
    /// are the same grid and the numbers below read directly. The straightened
    /// case is covered in `raw_core::composition`, where the two grids differ.
    fn frame() -> raw_core::Frame {
        raw_core::Frame::resolve(
            raw_core::Dims { w: 600, h: 400 },
            raw_core::Orientation::Rotate0,
            &raw_core::CompositionParams::default(),
        )
    }

    /// The same picture, levelled by `deg`. The frame is now bigger than the picture
    /// and the two grids are concentric rather than aligned, so every assertion below
    /// about the empty corners has to go through `Frame::covers` rather than through
    /// a bound on the numbers.
    fn tilted(deg: f32) -> raw_core::Frame {
        raw_core::Frame::resolve(
            raw_core::Dims { w: 600, h: 400 },
            raw_core::Orientation::Rotate0,
            &raw_core::CompositionParams {
                straighten: deg,
                ..Default::default()
            },
        )
    }

    /// Every handle, dragged the same way, with only the edges its name promises
    /// allowed to move.
    ///
    /// `an_edge_moves_only_its_own_axis` covers the four sides; the corners are where
    /// the risk was. `edges()` used to be `(bool, bool, bool, bool)` destructured
    /// positionally, so transposing two of them compiled cleanly and resized the wrong
    /// side — and a crop that resizes the wrong side under the hand gets blamed on the
    /// pointer arithmetic rather than on the tuple.
    #[test]
    fn a_handle_moves_the_edges_its_compass_point_names() {
        // Inset from the frame so a drag in any direction stays legal and nothing is
        // clamped: 150, 100 -> 450, 300 on the 600x400 frame.
        let start = r(0.25, 0.25, 0.5, 0.5);
        let d = (30.0, 20.0);
        let before = px(start);

        for (handle, left, right, top, bottom) in [
            (Handle::N, false, false, true, false),
            (Handle::S, false, false, false, true),
            (Handle::W, true, false, false, false),
            (Handle::E, false, true, false, false),
            (Handle::NW, true, false, true, false),
            (Handle::NE, false, true, true, false),
            (Handle::SW, true, false, false, true),
            (Handle::SE, false, true, false, true),
        ] {
            let got = px(drag(start, handle, d, None, &frame()));
            assert_eq!(
                (
                    got.0 != before.0,
                    got.2 != before.2,
                    got.1 != before.1,
                    got.3 != before.3,
                ),
                (left, right, top, bottom),
                "{handle:?} moved the wrong edges: {got:?} from {before:?}"
            );
        }

        // The interior is the one handle whose bounds all move and whose extent must
        // not.
        let body = px(drag(start, Handle::Body, d, None, &frame()));
        assert_eq!(
            (body.2 - body.0, body.3 - body.1),
            (before.2 - before.0, before.3 - before.1),
            "the body drag resized the crop"
        );
        assert_eq!(
            body.0 - before.0,
            30,
            "the body drag did not follow the pointer"
        );
    }

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// The rectangle in frame pixels, rounded, for legible assertions.
    fn px(r: Rect) -> (i32, i32, i32, i32) {
        let (x0, y0, x1, y1) = edges(r, &frame());
        (
            x0.round() as i32,
            y0.round() as i32,
            x1.round() as i32,
            y1.round() as i32,
        )
    }

    #[test]
    fn a_corner_moves_two_edges_and_leaves_the_others_alone() {
        let out = drag(Rect::FULL, Handle::NW, (60.0, 40.0), None, &frame());
        assert_eq!(px(out), (60, 40, 600, 400));

        let out = drag(Rect::FULL, Handle::SE, (-60.0, -40.0), None, &frame());
        assert_eq!(px(out), (0, 0, 540, 360));
    }

    #[test]
    fn an_edge_moves_only_its_own_axis() {
        for (h, want) in [
            (Handle::N, (0, 40, 600, 400)),
            (Handle::S, (0, 0, 600, 360)),
            (Handle::W, (60, 0, 600, 400)),
            (Handle::E, (0, 0, 540, 400)),
        ] {
            let d = match h {
                Handle::N | Handle::W => (60.0, 40.0),
                _ => (-60.0, -40.0),
            };
            assert_eq!(px(drag(Rect::FULL, h, d, None, &frame())), want, "{h:?}");
        }
    }

    #[test]
    fn the_body_slides_without_resizing_and_stops_at_the_edge() {
        // Two gestures that must not share a failure mode: moving the crop into a
        // corner has to stop it, not eat it. Trimming instead of shifting would
        // shrink the rectangle the harder you pushed.
        let start = r(0.25, 0.25, 0.5, 0.5); // 150,100 .. 450,300
        let moved = drag(start, Handle::Body, (60.0, 40.0), None, &frame());
        assert_eq!(px(moved), (210, 140, 510, 340));

        let jammed = drag(start, Handle::Body, (9999.0, 9999.0), None, &frame());
        assert_eq!(
            px(jammed),
            (300, 200, 600, 400),
            "the crop was trimmed, not stopped"
        );
    }

    #[test]
    fn an_edge_cannot_cross_its_opposite() {
        // A fast drag past the far edge would otherwise invert the rectangle, and a
        // negative width reappears as a crop mirrored to the other side of the frame.
        let out = drag(Rect::FULL, Handle::W, (9999.0, 0.0), None, &frame());
        let (x0, _, x1, _) = px(out);
        assert!(x1 > x0, "the rectangle inverted: {x0}..{x1}");
        assert_eq!(
            (x0, x1),
            (568, 600),
            "the left edge should stop MIN_PX short"
        );

        let out = drag(Rect::FULL, Handle::SE, (-9999.0, -9999.0), None, &frame());
        assert_eq!(px(out), (0, 0, 32, 32));
    }

    #[test]
    fn a_drag_never_leaves_the_frame() {
        // Exhaustive over the handles and a spread of deltas, because the failure is
        // an out-of-frame crop that renders as surround inside the picture and is
        // easy to produce and hard to notice.
        for h in Handle::CORNERS
            .iter()
            .chain(Handle::EDGES.iter())
            .chain([&Handle::Body])
        {
            for d in [
                (-800.0, -800.0),
                (800.0, 800.0),
                (-800.0, 800.0),
                (0.0, 700.0),
            ] {
                for ratio in [None, Some(1.0), Some(1.5), Some(0.5)] {
                    let out = drag(r(0.2, 0.2, 0.4, 0.4), *h, d, ratio, &frame());
                    let (x0, y0, x1, y1) = px(out);
                    assert!(
                        x0 >= 0 && y0 >= 0 && x1 <= 600 && y1 <= 400 && x1 > x0 && y1 > y0,
                        "{h:?} {d:?} ratio {ratio:?} gave {:?}",
                        (x0, y0, x1, y1)
                    );
                }
            }
        }
    }

    #[test]
    fn a_locked_corner_keeps_its_ratio() {
        // The lock is in FRAME PIXELS, not in normalised units — the frame is 3:2
        // here, so a 1:1 crop is not a square in normalised coordinates and a
        // constraint applied to the stored fractions would give a 3:2 crop that
        // claimed to be square.
        let out = drag(Rect::FULL, Handle::SE, (-200.0, -10.0), Some(1.0), &frame());
        let (x0, y0, x1, y1) = px(out);
        assert_eq!(
            x1 - x0,
            y1 - y0,
            "1:1 is not square in frame pixels: {:?}",
            (x0, y0, x1, y1)
        );

        let out = drag(
            Rect::FULL,
            Handle::SE,
            (-100.0, -100.0),
            Some(1.5),
            &frame(),
        );
        let (x0, y0, x1, y1) = px(out);
        assert!(
            (((x1 - x0) as f32 / (y1 - y0) as f32) - 1.5).abs() < 0.02,
            "3:2 lock drifted: {:?}",
            (x0, y0, x1, y1)
        );
    }

    #[test]
    fn a_locked_corner_lands_as_near_the_pointer_as_the_lock_allows() {
        // A corner drag almost never lands on the locked aspect, so the rule is
        // "nearest rectangle that is". Measured as the property rather than as a
        // number: every other square reachable from this anchor must have its corner
        // further from where the pointer actually went.
        let (want_x, want_y) = (580.0f32, 200.0); // SE dragged (-20, -200) from FULL
        let out = drag(Rect::FULL, Handle::SE, (-20.0, -200.0), Some(1.0), &frame());
        let (x0, y0, x1, y1) = px(out);
        assert_eq!((x0, y0), (0, 0), "the anchor moved");
        assert_eq!(x1 - x0, y1 - y0, "not square");

        let dist = |s: f32| ((s - want_x).powi(2) + (s - want_y).powi(2)).sqrt();
        let got = dist(x1 as f32);
        for other in [100.0, 200.0, 300.0, 350.0, 400.0, 500.0, 580.0f32] {
            assert!(
                got <= dist(other) + 1.0,
                "a {other}px square is nearer than {x1}px"
            );
        }
    }

    #[test]
    fn a_locked_corner_does_not_jitter_along_a_diagonal_drag() {
        // Why the rule is a projection rather than "whichever axis moved further".
        // A drag arrives as many small deltas, so a per-frame choice of leading axis
        // flips back and forth near the diagonal and the crop shivers. Sixty one-pixel
        // steps must give the same answer as the smooth path they approximate.
        let mut stepped = Rect::FULL;
        for _ in 0..60 {
            stepped = drag(stepped, Handle::SE, (-1.0, -1.0), Some(1.0), &frame());
        }
        let (_, _, x1, y1) = px(stepped);
        assert_eq!(x1 - y1, 0, "the locked crop drifted off square over a drag");
        // And it tracked the pointer rather than stalling or running away.
        assert!(
            (300..=360).contains(&x1),
            "60px of diagonal drag ended at {x1}"
        );
    }

    #[test]
    fn a_locked_edge_grows_the_free_axis_about_its_centre() {
        // Otherwise widening a 1:1 crop walks it up the frame, because the height
        // would grow downward from a fixed top edge.
        let start = r(0.25, 0.25, 0.5, 0.5); // centre at 300, 200
        let out = drag(start, Handle::E, (-100.0, 0.0), Some(1.0), &frame());
        let (_, y0, _, y1) = px(out);
        assert_eq!(
            (y0 + y1) / 2,
            200,
            "the crop drifted off its own centre line"
        );
    }

    #[test]
    fn a_locked_crop_stops_rather_than_quietly_unlocking() {
        // Pushed into a corner, the constraint has to win over the pointer. Clamping
        // each axis independently would keep the crop inside the frame and silently
        // change its aspect, which is the failure nobody notices until the print.
        let out = drag(
            r(0.4, 0.4, 0.2, 0.2),
            Handle::NW,
            (-9999.0, -9999.0),
            Some(1.0),
            &frame(),
        );
        let (x0, y0, x1, y1) = px(out);
        assert!(
            x0 >= 0 && y0 >= 0,
            "escaped the frame: {:?}",
            (x0, y0, x1, y1)
        );
        assert!(
            (x1 - x0 - (y1 - y0)).abs() <= 1,
            "the lock broke at the edge: {:?}",
            (x0, y0, x1, y1)
        );
    }

    #[test]
    fn hit_testing_prefers_a_corner_to_the_edge_it_sits_on() {
        // The NW corner is on both the N and the W edge, and resolving it to either
        // resizes one axis where the user aimed at two.
        let s = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(200.0, 200.0));
        assert_eq!(hit(s, egui::pos2(100.0, 100.0)), Zone::Grip(Handle::NW));
        assert_eq!(hit(s, egui::pos2(300.0, 100.0)), Zone::Grip(Handle::NE));
        assert_eq!(hit(s, egui::pos2(200.0, 100.0)), Zone::Grip(Handle::N));
        assert_eq!(hit(s, egui::pos2(200.0, 200.0)), Zone::Grip(Handle::Body));
        assert_eq!(hit(s, egui::pos2(20.0, 20.0)), Zone::Outside);
    }

    #[test]
    fn the_rotate_ring_is_outside_the_corner_and_only_outside() {
        // Capture One's gesture: the corner resizes, the band just past it levels.
        //
        // Three concentric bands, and the middle one is easy to get wrong. The
        // corner's own 18pt reaches in *every* direction, including outside the
        // box — a crop often sits hard against the frame edge, and a corner you
        // could only grab from the inside would be half a handle. The rotate ring
        // is what lies beyond that, out to 28pt.
        let s = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(200.0, 200.0));
        // 14pt out on the diagonal: still the corner handle.
        assert_eq!(hit(s, egui::pos2(90.0, 90.0)), Zone::Grip(Handle::NW));
        // 21pt out: the ring.
        assert_eq!(hit(s, egui::pos2(85.0, 85.0)), Zone::Rotate(Handle::NW));
        assert_eq!(hit(s, egui::pos2(315.0, 315.0)), Zone::Rotate(Handle::SE));
        // Inside the box at the same remove: never the ring. The same distance
        // *inside* a corner is somewhere you might reasonably grab the crop to move
        // it, and rotating the picture when you meant to nudge it is not a small
        // surprise.
        assert!(matches!(hit(s, egui::pos2(115.0, 115.0)), Zone::Grip(_)));
        // And past the ring is nothing at all.
        assert_eq!(hit(s, egui::pos2(70.0, 70.0)), Zone::Outside);
    }

    #[test]
    fn an_edge_is_grabbable_along_its_whole_length() {
        // The tick marks the edge; it is not the only place you may take hold of it.
        // Testing only the midpoint would make a long edge a 28px target.
        let s = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), egui::vec2(400.0, 200.0));
        for x in [160.0, 250.0, 340.0, 430.0] {
            assert_eq!(
                hit(s, egui::pos2(x, 100.0)),
                Zone::Grip(Handle::N),
                "at x={x}"
            );
            assert_eq!(
                hit(s, egui::pos2(x, 300.0)),
                Zone::Grip(Handle::S),
                "at x={x}"
            );
        }
        for y in [160.0, 240.0] {
            assert_eq!(hit(s, egui::pos2(100.0, y)), Zone::Grip(Handle::W));
            assert_eq!(hit(s, egui::pos2(500.0, y)), Zone::Grip(Handle::E));
        }
    }

    #[test]
    fn a_drawn_line_gives_the_angle_that_levels_it() {
        // The straighten tool's whole contract. A horizon rising to the right has to
        // produce a POSITIVE angle, because `straighten` is degrees clockwise and
        // levelling that horizon means turning the picture clockwise. Getting the
        // sign wrong doubles the tilt instead of removing it, which looks like the
        // tool being broken rather than backwards.
        let up_right = angle_of(egui::pos2(0.0, 100.0), egui::pos2(100.0, 90.0)).expect("long");
        assert!(
            up_right > 0.0,
            "a horizon rising to the right gave {up_right}"
        );
        assert!((up_right - 5.71).abs() < 0.05, "{up_right}");

        let down_right = angle_of(egui::pos2(0.0, 90.0), egui::pos2(100.0, 100.0)).expect("long");
        assert!(
            (down_right + up_right).abs() < 1.0e-4,
            "the two directions disagree"
        );

        // Drawn backwards — right to left along the same horizon — must give the
        // same correction. Nobody draws a guide line in a prescribed direction.
        let backwards = angle_of(egui::pos2(100.0, 90.0), egui::pos2(0.0, 100.0)).expect("long");
        assert!(
            (backwards - up_right).abs() < 1.0e-4,
            "{backwards} vs {up_right}"
        );
    }

    #[test]
    fn a_line_down_a_vertical_levels_against_the_vertical() {
        // Traced down a doorframe leaning 5° from upright, the tool must ask for 5°,
        // not for 85°. Indoors there is often no horizon at all, and this is what
        // makes the tool usable there.
        let deg = angle_of(egui::pos2(100.0, 0.0), egui::pos2(109.0, 100.0)).expect("long");
        assert!((deg - 5.14).abs() < 0.1, "a near-vertical gave {deg}");
        assert!(deg.abs() <= 45.0);

        // Every direction stays inside the wrapped range.
        for (dx, dy) in [
            (1.0, 0.0),
            (0.0, 1.0),
            (-1.0, 0.0),
            (0.0, -1.0),
            (1.0, 1.0),
            (-1.0, 1.0),
        ] {
            let d = angle_of(egui::pos2(0.0, 0.0), egui::pos2(dx * 200.0, dy * 200.0));
            let d = d.expect("long enough");
            assert!(d.abs() <= 45.0 + 1.0e-4, "({dx}, {dy}) gave {d}");
        }
    }

    #[test]
    fn a_click_is_not_a_horizon() {
        // A press with no drag must not level the picture to whatever two adjacent
        // pixels imply — which at that length is pure noise.
        assert_eq!(
            angle_of(egui::pos2(50.0, 50.0), egui::pos2(50.0, 50.0)),
            None
        );
        assert_eq!(
            angle_of(egui::pos2(50.0, 50.0), egui::pos2(53.0, 52.0)),
            None
        );
    }

    #[test]
    fn rotation_snaps_to_level_unless_you_mean_it() {
        // The gesture cannot reliably express a tenth of a degree, and a picture
        // left at 0.08° is a picture nobody meant to leave tilted.
        assert_eq!(snap(0.2, false), 0.0);
        assert_eq!(snap(-0.3, false), 0.0);
        assert_eq!(snap(0.5, false), 0.5, "past the snap it must be believed");
        assert_eq!(snap(0.2, true), 0.2, "shift means it");
    }

    #[test]
    fn every_guide_stays_inside_the_crop() {
        // A guide is scaffolding drawn on the box; one that escaped it would be
        // drawn over the picture the box excludes, which is exactly the region the
        // veil is dimming to keep out of the way.
        let r = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(300.0, 200.0));
        for g in Guide::ORDER {
            for [a, b] in guide_segments(g, r) {
                for p in [a, b] {
                    assert!(
                        p.x >= r.min.x - 0.5
                            && p.x <= r.max.x + 0.5
                            && p.y >= r.min.y - 0.5
                            && p.y <= r.max.y + 0.5,
                        "{:?} drew outside the box at {p:?}",
                        g
                    );
                }
            }
        }
        assert!(
            guide_segments(Guide::None, r).is_empty(),
            "None must draw nothing"
        );
    }

    #[test]
    fn a_ratio_survives_being_picked_and_then_dragged() {
        // the maintainer: "the selected ratios do not stick when cropping". The full
        // sequence, because the two halves are separate code: picking a preset
        // snaps the crop through `fit_ratio`, and every later handle drag has to
        // hold that shape through `drag`.
        let f = frame();
        for want in [1.0f32, 1.5, 1.25, 2.7083, 1.0 / 1.5] {
            let mut c = fit_ratio(Rect::FULL, want, &f);
            let placed = f.place(c);
            assert!(
                ((placed.w as f32 / placed.h as f32) - want).abs() < 0.02,
                "picking {want} gave {}x{}",
                placed.w,
                placed.h
            );
            // Then twenty small drags on assorted handles, as a real gesture is.
            for (i, h) in [Handle::SE, Handle::N, Handle::W, Handle::NE, Handle::Body]
                .iter()
                .cycle()
                .take(20)
                .enumerate()
            {
                let d = if i % 2 == 0 { (-3.0, 2.0) } else { (2.0, -3.0) };
                c = drag(c, *h, d, Some(want), &f);
            }
            let placed = f.place(c);
            let got = placed.w as f32 / placed.h as f32;
            assert!(
                (got - want).abs() < 0.02,
                "{want} drifted to {got} over a drag ({}x{})",
                placed.w,
                placed.h
            );
        }
    }

    // ── The hard clamp ───────────────────────────────────────────────────────
    //
    // the maintainer's decision, 2026-08-06, and it reverses the one this tool shipped with:
    // no gesture may leave an empty corner inside the crop. What was there before —
    // auto-crop on straighten, with a handle free to drag back out and reclaim the
    // corners — is recorded in `docs/decisions.md` along with why it changed.

    #[test]
    fn no_handle_can_drag_the_crop_into_a_corner_the_rotation_emptied() {
        // The failure this exists to stop is a crop that contains frame the picture
        // does not cover: it renders as surround inside the print, and on a 4° tilt
        // the wedge is a few hundred pixels of black down one side of a master.
        //
        // Exhaustive, because the old bound — the straightened *bounding box* — let
        // every one of these through, and which handle you happened to try decided
        // whether you saw it.
        for deg in [-12.0f32, -4.63, 1.5, 8.0, 30.0] {
            let f = tilted(deg);
            for h in Handle::CORNERS
                .iter()
                .chain(Handle::EDGES.iter())
                .chain([&Handle::Body])
            {
                for d in [
                    (-800.0, -800.0),
                    (800.0, 800.0),
                    (-800.0, 800.0),
                    (0.0, 700.0),
                ] {
                    for ratio in [None, Some(1.0), Some(1.5)] {
                        let start = f.inscribed(ratio.unwrap_or(1.5));
                        let out = drag(start, *h, d, ratio, &f);
                        assert!(
                            f.covers(out),
                            "{deg}° {h:?} {d:?} ratio {ratio:?} reached off the picture"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_clamp_costs_nothing_on_a_level_frame() {
        // The bisection is only for the rotated case, and a level frame must still
        // land on the exact pixel the arithmetic asks for. A clamp that rounded the
        // common case would show up as a crop that will not sit on a round number.
        let out = drag(Rect::FULL, Handle::NW, (60.0, 40.0), None, &frame());
        assert_eq!(px(out), (60, 40, 600, 400));
        let out = drag(
            r(0.25, 0.25, 0.5, 0.5),
            Handle::Body,
            (60.0, 40.0),
            None,
            &frame(),
        );
        assert_eq!(px(out), (210, 140, 510, 340));
    }

    #[test]
    fn a_clamped_drag_still_travels_as_far_as_the_picture_allows() {
        // The other half of the clamp, and the one a bisection can quietly get wrong:
        // stopping is only correct if it stops *late*. A pull-back that gave up early
        // would read as a handle that will not reach the edge of the picture.
        let f = tilted(6.0);
        let start = f.inscribed(1.5);
        let out = drag(start, Handle::E, (800.0, 0.0), None, &f);
        let at = f.place(out);
        let was = f.place(start);
        assert!(at.w > was.w, "the east edge did not move at all");
        assert_eq!(at.x, was.x, "the anchor moved");
        // And a few pixels further would have been off the picture — which is what
        // makes this a clamp rather than a smaller crop that happens to fit. The
        // allowance is `MARGIN` plus the half-pixel of slack `covers` itself grants:
        // the search deliberately stops a pixel inside the boundary so that rounding
        // the answer cannot put it back outside, and that pixel is the price of storing
        // a crop as whole pixels of a grid that is not the picture's own.
        let slop = BACKOFF as u32 + 2;
        let wider = f.unplace(IRect {
            w: at.w + slop,
            ..at
        });
        assert!(
            !f.covers(wider),
            "there was still room: the clamp stopped short"
        );
    }

    #[test]
    fn sliding_into_a_straightened_corner_travels_along_the_edge_it_reaches() {
        // Why the body slide bisects each axis separately. Pushed diagonally into a
        // corner, a crop that stopped dead the moment *either* axis ran out would
        // refuse to travel along an edge it is already touching, which reads as the
        // tool having seized rather than as a limit.
        let f = tilted(8.0);
        // Half-size, so there is somewhere to slide to. A crop already filling the
        // picture is the one case that cannot tell a working slide from a seized one.
        let start = r(0.25, 0.25, 0.4, 0.4);
        assert!(f.covers(start));
        let jammed = drag(start, Handle::Body, (9999.0, 9999.0), None, &f);
        let along = drag(jammed, Handle::Body, (-9999.0, 0.0), None, &f);
        let place = |c: Rect| f.place(c);
        assert!(
            f.covers(jammed) && f.covers(along),
            "the slide left the picture"
        );
        assert!(
            place(jammed).x > place(start).x,
            "it would not travel at all"
        );
        assert!(
            place(along).x < place(jammed).x,
            "wedged into the corner, the crop would not slide back along the edge"
        );
        // The slide never resizes — that is the whole difference between this handle
        // and the eight others, and a clamp that trimmed instead of stopping would eat
        // the crop the harder you pushed.
        assert_eq!(
            place(jammed).dims(),
            place(start).dims(),
            "the slide resized the crop"
        );
    }

    #[test]
    fn straightening_pulls_the_crop_back_onto_the_picture() {
        // The auto-crop, as the invariant it now is. A crop that was legal at 0° is
        // not legal at 9°, and `confine` is what says so — for the slider, the rotate
        // ring and the drawn horizon alike.
        let f = tilted(9.0);
        assert!(
            !f.covers(Rect::FULL),
            "the test angle is too small to empty a corner"
        );
        let out = confine(Rect::FULL, None, &f);
        assert!(
            f.covers(out),
            "the whole frame was left holding empty corners"
        );
        // Shrunk about its own centre, so it kept its shape — straightening must not
        // also reshape the print.
        let (x0, y0, x1, y1) = edges(out, &f);
        assert!(
            ((x1 - x0) / (y1 - y0) - 1.5).abs() < 0.02,
            "3:2 became {:.3}:1",
            (x1 - x0) / (y1 - y0)
        );
    }

    #[test]
    fn a_straighten_drag_does_not_ratchet_the_crop_smaller() {
        // A slider drag arrives as a hundred small changes of angle, and each one asks
        // `confine` again. If the answer at a given angle is not stable the crop loses
        // a little on every frame and a two-second drag eats the picture — which is
        // what `inscribed` returning a rectangle its own `covers` test rejected used to
        // do, silently, at a rate small enough to read as "straighten crops a lot".
        let mut c = Rect::FULL;
        let mut deg = 0.0f32;
        for _ in 0..120 {
            deg += 0.05;
            c = confine(c, None, &tilted(deg));
        }
        let f = tilted(deg);
        assert!(f.covers(c));
        // Against the ideal for the angle it ended at: a ratcheting re-fit lands far
        // inside this, and only the *area* is compared because `confine` shrinks about
        // the crop's own centre and the ideal is centred on the frame.
        let ideal = f.place(f.inscribed(1.5));
        let got = f.place(c);
        let loss = 1.0 - (got.w as f32 * got.h as f32) / (ideal.w as f32 * ideal.h as f32);
        assert!(
            loss < 0.02,
            "120 frames of drag lost {:.1}% of the crop",
            loss * 100.0
        );
    }

    #[test]
    fn confine_keeps_the_subject_where_it_was() {
        // About the crop's own centre, not the frame's. Recentring would move the
        // subject every time the straighten slider crossed the angle that costs a
        // corner, which is the one moment you are looking hardest at the composition.
        let f = tilted(10.0);
        let off = r(0.05, 0.05, 0.6, 0.6);
        let out = confine(off, None, &f);
        assert!(f.covers(out));
        let mid = |r: Rect| {
            let (x0, y0, x1, y1) = edges(r, &f);
            ((x0 + x1) * 0.5, (y0 + y1) * 0.5)
        };
        let ((ax, ay), (bx, by)) = (mid(off), mid(out));
        assert!(
            (ax - bx).abs() < 1.5 && (ay - by).abs() < 1.5,
            "the crop was recentred"
        );
    }

    #[test]
    fn picking_a_ratio_on_a_tilted_frame_lands_on_the_picture() {
        // `fit_ratio` fitted the crop to the bounding box, which on a straightened
        // frame is bigger than the picture — so choosing 1:1 could hand back a square
        // with two black corners in it.
        let f = tilted(7.5);
        for want in [1.0f32, 1.5, 2.7083, 1.0 / 1.5] {
            let out = fit_ratio(Rect::FULL, want, &f);
            assert!(f.covers(out), "{want} left the picture");
            let placed = f.place(out);
            assert!(
                ((placed.w as f32 / placed.h as f32) - want).abs() < 0.02,
                "{want} came back as {}x{}",
                placed.w,
                placed.h
            );
        }
    }

    #[test]
    fn a_freeform_drag_of_zero_changes_nothing() {
        // The idle case: a press with no movement must not nudge the rectangle, or
        // every click on a handle would be an edit and an undo entry.
        let start = r(0.2, 0.3, 0.5, 0.4);
        for h in Handle::CORNERS
            .iter()
            .chain(Handle::EDGES.iter())
            .chain([&Handle::Body])
        {
            let out = drag(start, *h, (0.0, 0.0), None, &frame());
            assert_eq!(px(out), px(start), "{h:?} moved on a zero drag");
        }
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    /// Run the overlay through a real egui frame and hand back what it painted.
    ///
    /// The unit tests above check arithmetic; this checks that the thing actually
    /// draws. A panic in a painter — a NaN rect, an inverted range — is invisible to
    /// every other test here and immediate in use.
    fn paint(o: &Overlay) -> usize {
        let ctx = egui::Context::default();
        let mut shapes = 0;
        let out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |ui| {
                overlay(ui.painter(), o);
                shapes = 1;
            },
        );
        let _ = out;
        shapes
    }

    fn base() -> Overlay {
        Overlay {
            viewport: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0)),
            screen: egui::Rect::from_min_size(egui::pos2(100.0, 80.0), egui::vec2(400.0, 300.0)),
            hover: Zone::Outside,
            dragging: false,
            levelling: false,
            guide: Guide::Thirds,
            angle: 0.0,
            dims: (4000, 3000),
            line: None,
        }
    }

    #[test]
    fn the_overlay_draws_in_every_state_it_can_be_in() {
        // Every combination the tool can reach, because the states differ in which
        // branches run: the veil changes alpha, the guides give way to the lattice,
        // the readouts appear and disappear, and the line only exists mid-gesture.
        for guide in Guide::ORDER {
            assert_eq!(paint(&Overlay { guide, ..base() }), 1, "{guide:?}");
        }
        for hover in [
            Zone::Outside,
            Zone::Grip(Handle::NW),
            Zone::Grip(Handle::N),
            Zone::Grip(Handle::Body),
            Zone::Rotate(Handle::SE),
        ] {
            assert_eq!(paint(&Overlay { hover, ..base() }), 1, "{hover:?}");
        }
        assert_eq!(
            paint(&Overlay {
                dragging: true,
                ..base()
            }),
            1
        );
        assert_eq!(
            paint(&Overlay {
                levelling: true,
                angle: -3.25,
                ..base()
            }),
            1
        );
        assert_eq!(
            paint(&Overlay {
                levelling: true,
                line: Some((egui::pos2(120.0, 300.0), egui::pos2(460.0, 260.0))),
                ..base()
            }),
            1
        );
    }

    #[test]
    fn a_degenerate_or_offscreen_crop_does_not_take_the_painter_with_it() {
        // Reachable: zoom far enough out and the box is a couple of points across;
        // pan far enough and it leaves the viewport entirely. The arithmetic below
        // divides by lengths and steps along them, and neither case is one a user
        // would think to report as anything but a crash.
        for screen in [
            egui::Rect::from_min_size(egui::pos2(300.0, 300.0), egui::vec2(1.0, 1.0)),
            egui::Rect::from_min_size(egui::pos2(300.0, 300.0), egui::vec2(0.0, 0.0)),
            egui::Rect::from_min_size(egui::pos2(-900.0, -900.0), egui::vec2(200.0, 200.0)),
            egui::Rect::from_min_size(egui::pos2(2000.0, 2000.0), egui::vec2(200.0, 200.0)),
        ] {
            assert_eq!(paint(&Overlay { screen, ..base() }), 1, "{screen:?}");
        }
        // And a zero-length straighten line, which is one frame of every gesture.
        let p = egui::pos2(200.0, 200.0);
        assert_eq!(
            paint(&Overlay {
                line: Some((p, p)),
                levelling: true,
                ..base()
            }),
            1
        );
    }
}
