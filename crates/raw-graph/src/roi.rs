//! Regions of interest and the aprons spatial operations need around them.
//!
//! A `Roi` is a rectangle *plus the grid it lives on*. Carrying the grid is what
//! makes the arithmetic checkable: a union of two regions from different grids is
//! meaningless, and saying so out loud is cheaper than debugging the image it
//! would produce.
//!
//! All coordinates are in the node's **own** pixels, at its own `scale`. The one
//! place source-pixel coordinates appear is inside the input node, which is the
//! only node that touches source geometry.

/// `ceil`, with a few ulps of slack.
///
/// `scale` is a float approximation of a zoom level, so a 200px support at 15%
/// computes to 30.000001 rather than 30, and a bare `ceil` charges a whole extra
/// pixel of apron — or a whole extra column of image — for the last bits of the
/// mantissa. The slack is relative and tiny: a genuine 30.5 still rounds up.
///
/// Both callers matter. In `grid_for` this decides the image's extent, and an
/// off-by-one there shifts every region in the graph.
pub(crate) fn ceil_px(v: f32) -> u32 {
    if v <= 0.0 {
        return 0;
    }
    (v - v * 4.0 * f32::EPSILON).ceil() as u32
}

/// How far a spatial operation has to reach outside the region it is asked to
/// produce.
///
/// Asymmetric by construction, and deliberately so. A separable blur reaches only
/// along its own axis; the contrast mask's registration offset shifts its mask
/// input in one direction and therefore reaches further one way than the other.
/// A single `radius` field would force both of those into a square and quietly
/// compute (and pay for) apron that is never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Apron {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl Apron {
    /// A pointwise operation. The overwhelmingly common case, and the default a
    /// node gets for free.
    pub const NONE: Self = Self {
        left: 0,
        right: 0,
        top: 0,
        bottom: 0,
    };

    pub fn uniform(n: u32) -> Self {
        Self {
            left: n,
            right: n,
            top: n,
            bottom: n,
        }
    }

    /// Reach along x only — one axis of a separable blur.
    pub fn horizontal(n: u32) -> Self {
        Self {
            left: n,
            right: n,
            top: 0,
            bottom: 0,
        }
    }

    /// Reach along y only.
    pub fn vertical(n: u32) -> Self {
        Self {
            left: 0,
            right: 0,
            top: n,
            bottom: n,
        }
    }

    /// Reach needed to read the input shifted by `(dx, dy)` output pixels. A
    /// positive `dx` means the operation reads from further right, so it needs
    /// apron on the right.
    pub fn shift(dx: f32, dy: f32) -> Self {
        Self {
            left: ceil_px(-dx),
            right: ceil_px(dx),
            top: ceil_px(-dy),
            bottom: ceil_px(dy),
        }
    }

    pub fn is_none(self) -> bool {
        self == Self::NONE
    }

    /// Per-side sum. Two spatial stages in series reach the sum of their reaches.
    pub fn sum(self, other: Self) -> Self {
        Self {
            left: self.left + other.left,
            right: self.right + other.right,
            top: self.top + other.top,
            bottom: self.bottom + other.bottom,
        }
    }
}

/// A rectangle on a specific image grid.
///
/// `full` is the whole image *at this grid's scale*; `x/y/w/h` is the part this node
/// produces. **The origin is signed**: an apron may push it negative, and clamping is a
/// separate step so a node can tell it asked for more than exists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Roi {
    /// Full extent of the image on this grid.
    pub full: (u32, u32),
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// This grid's pixels per source pixel. 1.0 is 100% — one grid pixel per
    /// output pixel, NOT per photosite, per `docs/decisions.md`.
    pub scale: f32,
}

impl Roi {
    /// The whole image at `scale`.
    pub fn full_frame(full: (u32, u32), scale: f32) -> Self {
        Self {
            full,
            x: 0,
            y: 0,
            w: full.0,
            h: full.1,
            scale,
        }
    }

    /// A window on a given grid.
    pub fn window(full: (u32, u32), scale: f32, x: i32, y: i32, w: u32, h: u32) -> Self {
        Self {
            full,
            x,
            y,
            w,
            h,
            scale,
        }
    }

    /// Full extent of a source image viewed at `scale`.
    ///
    /// `ceil`, not `round`: at scale 0.3 a 100px image occupies 30 output pixels
    /// and the 30th is only partly covered, but it still has to be written or the
    /// last column of the image is missing.
    pub fn grid_for(source: (u32, u32), scale: f32) -> (u32, u32) {
        (
            ceil_px(source.0 as f32 * scale).max(1),
            ceil_px(source.1 as f32 * scale).max(1),
        )
    }

    pub fn right(self) -> i32 {
        self.x + self.w as i32
    }

    /// Cover this region on another grid, rounding edges outward. Keep the
    /// identical-grid case exact so existing crop and empty-region rules survive.
    pub fn on_grid(self, full: (u32, u32), scale: f32) -> Self {
        if self.scale == scale {
            return Self { full, ..self };
        }
        let ratio = scale / self.scale;
        let x = (self.x as f32 * ratio).floor() as i32;
        let y = (self.y as f32 * ratio).floor() as i32;
        let right = (self.right() as f32 * ratio).ceil() as i32;
        let bottom = (self.bottom() as f32 * ratio).ceil() as i32;
        Self::window(
            full,
            scale,
            x,
            y,
            (right - x).max(0) as u32,
            (bottom - y).max(0) as u32,
        )
    }

    pub fn bottom(self) -> i32 {
        self.y + self.h as i32
    }

    pub fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// Grow by an apron. Does **not** clamp — the result may sit partly outside
    /// the image, which the caller resolves with `clamp_to_full`.
    pub fn expand(self, a: Apron) -> Self {
        Self {
            x: self.x - a.left as i32,
            y: self.y - a.top as i32,
            w: self.w + a.left + a.right,
            h: self.h + a.top + a.bottom,
            ..self
        }
    }

    /// Trim to the image. Apron that falls off the edge is unavailable; the reader
    /// clamps its tap coordinate, as `sample.wgsl` does.
    pub fn clamp_to_full(self) -> Self {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.right().min(self.full.0 as i32);
        let y1 = self.bottom().min(self.full.1 as i32);
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0) as u32,
            h: (y1 - y0).max(0) as u32,
            ..self
        }
    }

    /// Smallest rectangle containing both.
    ///
    /// What a fork costs: two branches reading one producer at different reaches
    /// make it write the union, not a region each. Get it wrong and the image is
    /// right in the middle and wrong along one edge — it reads as a blur artefact
    /// rather than as a scheduling bug.
    pub fn union(self, other: Self) -> Self {
        debug_assert_eq!(self.full, other.full, "union across different grids");
        debug_assert!(
            (self.scale - other.scale).abs() <= f32::EPSILON * self.scale.abs().max(1.0),
            "union across different scales: {} vs {}",
            self.scale,
            other.scale
        );
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0) as u32,
            h: (y1 - y0) as u32,
            ..self
        }
    }

    /// The overlap of two regions on the same grid, empty if they do not meet.
    ///
    /// **Intersect first, expand second.** The crop is applied to what the sink asks
    /// its input for; aprons are then expanded from that and clamped to `full`, the
    /// uncropped frame, so a blur at the crop boundary still reads real pixels from
    /// outside it. The other order clamps the reach at the crop edge and puts a halo
    /// along it.
    pub fn intersect(self, other: Self) -> Self {
        debug_assert_eq!(self.full, other.full, "intersection across different grids");
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0) as u32,
            h: (y1 - y0).max(0) as u32,
            ..self
        }
    }

    /// Whether `other` lies entirely inside `self`. The apron assertion: a node's
    /// input buffer must contain everything the node will read.
    pub fn contains(self, other: Self) -> bool {
        other.is_empty()
            || (self.x <= other.x
                && self.y <= other.y
                && self.right() >= other.right()
                && self.bottom() >= other.bottom())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> (u32, u32) {
        (100, 80)
    }

    #[test]
    fn an_apron_grows_the_region_on_the_sides_it_names() {
        let r = Roi::window(grid(), 1.0, 10, 10, 20, 20);
        let e = r.expand(Apron::horizontal(3));
        assert_eq!(
            (e.x, e.y, e.w, e.h),
            (7, 10, 26, 20),
            "horizontal must not grow y"
        );

        let e = r.expand(Apron::vertical(3));
        assert_eq!(
            (e.x, e.y, e.w, e.h),
            (10, 7, 20, 26),
            "vertical must not grow x"
        );
    }

    #[test]
    fn a_separable_blur_costs_less_apron_than_a_square_one() {
        // The whole reason Apron has four fields. Two separable passes at radius 8
        // touch far fewer pixels than one 8-in-every-direction pass would suggest,
        // and modelling them as squares would over-allocate every intermediate.
        let r = Roi::window(grid(), 1.0, 20, 20, 40, 40);
        let separable = r.expand(Apron::horizontal(8)).expand(Apron::vertical(8));
        let square = r.expand(Apron::uniform(8));
        assert_eq!(separable.area(), square.area(), "same final extent");
        // ...but each individual pass is cheaper, which is what the scheduler sees.
        assert!(r.expand(Apron::horizontal(8)).area() < square.area());
    }

    #[test]
    fn a_shift_reaches_only_the_way_it_points() {
        // Registration offset. Shifting the mask right means reading from further
        // right, so apron is needed on the right and not the left.
        let a = Apron::shift(4.0, 0.0);
        assert_eq!((a.left, a.right), (0, 4));
        assert_eq!((a.top, a.bottom), (0, 0));

        let a = Apron::shift(0.0, -4.0);
        assert_eq!((a.top, a.bottom), (4, 0));
    }

    #[test]
    fn clamping_drops_the_apron_that_falls_off_the_edge() {
        let r = Roi::window(grid(), 1.0, 0, 0, 20, 20).expand(Apron::uniform(5));
        assert_eq!((r.x, r.y), (-5, -5), "expansion is unclamped by design");
        let c = r.clamp_to_full();
        assert_eq!(
            (c.x, c.y, c.w, c.h),
            (0, 0, 25, 25),
            "the outside half is unavailable"
        );
    }

    #[test]
    fn clamping_a_region_entirely_outside_gives_nothing_not_a_negative_size() {
        let r = Roi::window(grid(), 1.0, 200, 200, 10, 10).clamp_to_full();
        assert!(r.is_empty());
        assert_eq!(
            (r.w, r.h),
            (0, 0),
            "a signed underflow here would allocate nonsense"
        );
    }

    #[test]
    fn a_fork_makes_the_producer_satisfy_the_union() {
        // Contrast Mask in miniature: one branch reads the region directly, the
        // other reads it blurred and therefore wider. The shared producer must
        // write the wider one.
        let direct = Roi::window(grid(), 1.0, 30, 30, 20, 20);
        let blurred = direct.expand(Apron::uniform(6));
        let u = direct.union(blurred);
        assert_eq!((u.x, u.y, u.w, u.h), (24, 24, 32, 32));
        assert!(
            u.contains(direct) && u.contains(blurred),
            "union must satisfy both branches"
        );
    }

    #[test]
    fn union_with_an_empty_region_is_the_other_region() {
        // A skipped node contributes no request; it must not drag the union to
        // the origin.
        let r = Roi::window(grid(), 1.0, 30, 30, 20, 20);
        let empty = Roi::window(grid(), 1.0, 0, 0, 0, 0);
        assert_eq!(r.union(empty), r);
        assert_eq!(empty.union(r), r);
    }

    #[test]
    fn spatial_stages_in_series_sum_their_reaches() {
        let a = Apron::horizontal(4).sum(Apron::horizontal(6));
        assert_eq!(a, Apron::horizontal(10));
    }

    #[test]
    fn the_grid_covers_a_partly_filled_last_pixel() {
        // ceil, not round: at 0.3 the 30th column is only 30% covered but must
        // still exist, or the right edge of the image is missing.
        assert_eq!(Roi::grid_for((100, 100), 0.3), (30, 30));
        assert_eq!(Roi::grid_for((101, 101), 0.3), (31, 31));
        assert_eq!(Roi::grid_for((100, 100), 1.0), (100, 100));
    }

    #[test]
    fn a_scale_that_is_not_representable_does_not_buy_an_extra_pixel() {
        // 0.15 and 0.3 are not exact in binary, so 100 * 0.3 is 30.000001 and a
        // bare ceil returns 31 — an image one column wider than it is, and every
        // region in the graph shifted with it.
        assert_eq!(ceil_px(100.0 * 0.3), 30);
        assert_eq!(ceil_px(200.0 * 0.15), 30);
        // The slack must not swallow a genuine fraction.
        assert_eq!(ceil_px(30.5), 31);
        assert_eq!(ceil_px(30.001), 31);
        assert_eq!(ceil_px(0.0), 0);
        assert_eq!(
            ceil_px(-5.0),
            0,
            "a negative shift reaches the other way, not backwards"
        );
    }

    #[test]
    fn a_grid_never_collapses_to_nothing() {
        // Zoomed far enough out, a naive floor gives a zero-sized texture and
        // wgpu rejects the allocation.
        assert_eq!(Roi::grid_for((100, 100), 0.0001), (1, 1));
    }
}
