//! Composition: orientation, straighten, perspective and crop. The stage that decides the frame
//! every other module measures against.
//!
//! # Nothing here rotates a pixel
//!
//! This module produces **numbers**, not images: a frame size, a crop rectangle,
//! and one projective matrix that maps a frame coordinate back to a source coordinate.
//! The rotation happens where the view transform already happens — in the input
//! node, which `raw_graph` documents as "the only node that touches source
//! geometry". Rotating the data would be the one irreversible mistake available
//! here: the CFA pattern is defined against the sensor array origin, `CfaGeometry`
//! exists entirely to stop off-by-2 phase bugs, and a quarter turn of the mosaic
//! swaps the Bayer phase in a way nothing downstream expects.
//!
//! # Three grids, and they are not the same grid
//!
//! ```text
//!   source    the LumaImage, as derived. `LumaImage::output_dims`, so SuperPixel
//!             is half the sensor and everything below is in those pixels.
//!      |  orientation — a quarter turn. Swaps w/h at 90 and 270.
//!      v
//!   oriented  the frame the right way up. Same pixels, possibly transposed.
//!      |  straighten — a small rotation. GROWS the extent to the bounding box.
//!      v
//!   frame     what the graph runs on, uncropped. `Frame::frame`.
//!      |  crop — a sub-rectangle. Does NOT change the grid.
//!      v
//!   crop      what is displayed, exported, and reported. `Frame::crop`.
//! ```
//!
//! The distinction that matters is the last one: **crop restricts what is shown
//! without shrinking the grid the chain runs on.** Crop-then-blur has no data past the
//! boundary; blur-then-crop is correct. `Roi::clamp_to_full` clamps aprons to `frame`,
//! so a spatial module reads real pixels outside the crop, and moving a crop handle is
//! a `render` change because nothing above the sink sees it.
//!
//! # The crop rectangle is stored as fractions of the ORIENTED frame
//!
//! Fractions rather than pixels for the reason `ContrastMaskParams::spacer` is a
//! percentage: everything downstream measures in pixels of the LUMINANCE image, and
//! SuperPixel is half resolution. A crop in pixels would cover a different part of
//! the picture after a change of sampling mode — the same defect, in the one module
//! where it would move the subject out of the frame.
//!
//! **Fractions of `oriented`, not of `frame` — the decision the module turns on.**
//! `frame` grows with the straighten angle, so fractions normalised against it mean
//! something different at every angle: the crop changed size and shape under its own
//! feet as the slider moved, and the auto-crop fought the growth every frame. Two of
//! the bugs the maintainer reported in testing were this one defect; `docs/decisions.md` has the
//! measurements. `oriented` does not move, so a crop's pixel size, aspect and exported
//! dimensions do not depend on the angle at all.
//!
//! A consequence worth stating: a straightened crop may legitimately sit **outside**
//! `[0, 1]`, reaching into the corners the rotation emptied. That is the "drag back
//! out and reclaim them" the maintainer asked for, so the bound this type is clamped to is
//! the *frame*, applied where the frame is known, and not the unit square.

use crate::geometry::Dims;

/// Quarter-turn orientation. **Clockwise**, and named for what has to be done to
/// the stored pixels to make them upright — which is also how EXIF names it, so
/// `Rotate270` here is exiftool's `Rotate 270 CW`.
///
/// Only the four rotations. EXIF also defines four mirrored orientations (2, 4, 5,
/// 7); no camera in the corpus writes one, they come from scanners and flipped
/// optics, and a mirror is a different operation from a rotation with different
/// consequences for text in the frame. [`Orientation::from_exif`] takes their
/// rotation component and drops the flip, which is closer than ignoring the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Rotate0,
    Rotate90,
    Rotate180,
    Rotate270,
}

impl Orientation {
    /// From the IFD `Orientation` tag (0x0112). Anything unrecognised — including
    /// the 0 that a few bodies write for "not set" — reads as upright, which is
    /// what the app did before it read the tag at all.
    pub fn from_exif(tag: u16) -> Self {
        match tag {
            3 | 4 => Self::Rotate180,
            5 | 6 => Self::Rotate90,
            7 | 8 => Self::Rotate270,
            _ => Self::Rotate0,
        }
    }

    /// Quarter turn anticlockwise.
    pub fn left(self) -> Self {
        match self {
            Self::Rotate0 => Self::Rotate270,
            Self::Rotate90 => Self::Rotate0,
            Self::Rotate180 => Self::Rotate90,
            Self::Rotate270 => Self::Rotate180,
        }
    }

    /// Quarter turn clockwise.
    pub fn right(self) -> Self {
        match self {
            Self::Rotate0 => Self::Rotate90,
            Self::Rotate90 => Self::Rotate180,
            Self::Rotate180 => Self::Rotate270,
            Self::Rotate270 => Self::Rotate0,
        }
    }

    /// Whether this turn transposes the frame.
    pub fn transposes(self) -> bool {
        matches!(self, Self::Rotate90 | Self::Rotate270)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rotate0 => "0°",
            Self::Rotate90 => "90° CW",
            Self::Rotate180 => "180°",
            Self::Rotate270 => "270° CW",
        }
    }

    /// Source dims after the turn.
    fn applied_to(self, d: Dims) -> Dims {
        if self.transposes() {
            Dims { w: d.h, h: d.w }
        } else {
            d
        }
    }

    /// Oriented coordinates -> source coordinates, represented in the shared
    /// projective form (this particular map is affine).
    ///
    /// Continuous, not indices: the extent is used rather than `extent - 1`,
    /// because these are sample positions on a grid and not array subscripts. A
    /// half-pixel of error here is a half-pixel of error in every frame.
    fn inverse(self, src: Dims) -> Projective {
        let (w, h) = (src.w as f32, src.h as f32);
        match self {
            // sx = ox, sy = oy
            Self::Rotate0 => Projective::IDENTITY,
            // sx = oy, sy = h - ox
            Self::Rotate90 => Projective([0.0, 1.0, 0.0, -1.0, 0.0, h, 0.0, 0.0, 1.0]),
            // sx = w - ox, sy = h - oy
            Self::Rotate180 => Projective([-1.0, 0.0, w, 0.0, -1.0, h, 0.0, 0.0, 1.0]),
            // sx = w - oy, sy = ox
            Self::Rotate270 => Projective([0.0, -1.0, w, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
        }
    }
}

/// A rectangle in normalised coordinates of the **oriented** frame.
///
/// `(0, 0, 1, 1)` is the whole picture the right way up, before any straighten.
/// Resolution-independent by construction, and angle-independent by construction;
/// see the module note, because neither is a convenience.
///
/// **May legitimately leave `[0, 1]`** once the frame is straightened, reaching into
/// the corners the rotation emptied. Bounding it is therefore the job of whoever
/// knows the frame — see [`Frame::place`] — and not of this type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// A normalised point in the oriented picture. Perspective guides use the same
/// resolution-independent coordinate system as the crop, so changing luminance
/// sampling cannot move a guide away from the line it was placed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn sane(self, fallback: Self) -> Self {
        if self.x.is_finite() && self.y.is_finite() {
            Self {
                x: self.x.clamp(0.0, 1.0),
                y: self.y.clamp(0.0, 1.0),
            }
        } else {
            fallback
        }
    }
}

/// Which guide geometry is being used to rectify the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeystoneMode {
    #[default]
    Off,
    Vertical,
    Horizontal,
    Rectangle,
}

impl KeystoneMode {
    pub const ORDER: [Self; 3] = [Self::Vertical, Self::Horizontal, Self::Rectangle];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Vertical => "Vertical",
            Self::Horizontal => "Horizontal",
            Self::Rectangle => "Rectangle",
        }
    }
}

/// The crop proposed after changing a perspective fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeystoneCrop {
    /// Largest axis-aligned rectangle containing real image data.
    Largest,
    /// Same, constrained to the oriented photograph's original aspect.
    #[default]
    Original,
}

impl KeystoneCrop {
    pub const ORDER: [Self; 2] = [Self::Original, Self::Largest];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Largest => "Largest",
            Self::Original => "Original",
        }
    }
}

/// Manual perspective correction. The four points are ordered TL, TR, BR, BL.
/// Their neutral positions form an inset rectangle; moving them onto lines in the
/// photograph defines the quadrilateral that should become that rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeystoneParams {
    pub mode: KeystoneMode,
    pub guides: [Point; 4],
    /// 0–1. Phocus deliberately defaults line correction to 80% because a wholly
    /// rectified building often looks wider at the top than it did to the eye.
    pub correction: f32,
    /// Horizontal shape compensation, as a percentage. Positive values widen.
    pub aspect: f32,
    pub crop: KeystoneCrop,
}

impl KeystoneParams {
    pub const TARGETS: [Point; 4] = [
        Point::new(0.2, 0.2),
        Point::new(0.8, 0.2),
        Point::new(0.8, 0.8),
        Point::new(0.2, 0.8),
    ];
    pub const CORRECTION_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;
    pub const ASPECT_RANGE: std::ops::RangeInclusive<f32> = -50.0..=50.0;

    pub fn is_active(self) -> bool {
        let guides_change_shape = self.selected_guides() != Self::TARGETS;
        self.mode != KeystoneMode::Off
            && (self.aspect.abs() > f32::EPSILON
                || (self.correction > f32::EPSILON && guides_change_shape))
    }

    pub fn reset_for(self, mode: KeystoneMode) -> Self {
        Self {
            mode,
            correction: if mode == KeystoneMode::Rectangle {
                1.0
            } else {
                0.8
            },
            ..Self::default()
        }
    }

    /// Move one authored handle if it still describes a usable quadrilateral.
    /// Line-guide endpoints move freely: their slope, rather than only their x or y
    /// coordinate, is the measurement the correction is built from.
    pub fn move_guide(&mut self, index: usize, point: Point) {
        let Some(old) = self.guides.get(index).copied() else {
            return;
        };
        let point = point.sane(old);
        let mut candidate = *self;
        match self.mode {
            KeystoneMode::Off => return,
            KeystoneMode::Vertical | KeystoneMode::Horizontal | KeystoneMode::Rectangle => {
                candidate.guides[index] = point;
            }
        }
        if Projective::quad_is_convex(candidate.selected_guides()) {
            self.guides = candidate.guides;
        }
    }

    /// Commit two freely drawn lines as a horizontal or vertical guide pair.
    /// Endpoints and line order are normalised here so drawing right-to-left, or the
    /// lower/right line first, produces the same authored correction.
    pub fn set_drawn_lines(
        &mut self,
        mode: KeystoneMode,
        mut first: [Point; 2],
        mut second: [Point; 2],
    ) -> bool {
        let direction_matches = |line: [Point; 2]| {
            let dx = (line[1].x - line[0].x).abs();
            let dy = (line[1].y - line[0].y).abs();
            match mode {
                KeystoneMode::Vertical => dy > dx && dy > 1.0e-3,
                KeystoneMode::Horizontal => dx > dy && dx > 1.0e-3,
                _ => false,
            }
        };
        if !direction_matches(first) || !direction_matches(second) {
            return false;
        }
        match mode {
            KeystoneMode::Vertical => {
                first.sort_by(|a, b| a.y.total_cmp(&b.y));
                second.sort_by(|a, b| a.y.total_cmp(&b.y));
                if (first[0].x + first[1].x) > (second[0].x + second[1].x) {
                    std::mem::swap(&mut first, &mut second);
                }
            }
            KeystoneMode::Horizontal => {
                first.sort_by(|a, b| a.x.total_cmp(&b.x));
                second.sort_by(|a, b| a.x.total_cmp(&b.x));
                if (first[0].y + first[1].y) > (second[0].y + second[1].y) {
                    std::mem::swap(&mut first, &mut second);
                }
            }
            _ => return false,
        }
        let guides = match mode {
            KeystoneMode::Vertical => [first[0], second[0], second[1], first[1]],
            KeystoneMode::Horizontal => [first[0], first[1], second[1], second[0]],
            _ => unreachable!(),
        };
        let candidate = Self {
            mode,
            guides,
            ..*self
        };
        if !Projective::quad_is_convex(candidate.selected_guides()) {
            return false;
        }
        self.mode = mode;
        self.guides = guides;
        true
    }

    fn selected_guides(self) -> [Point; 4] {
        let targets = Self::TARGETS;
        let x_at_y = |a: Point, b: Point, y: f32| {
            let span = b.y - a.y;
            if span.abs() < 1.0e-6 {
                a.x
            } else {
                a.x + (b.x - a.x) * (y - a.y) / span
            }
        };
        let y_at_x = |a: Point, b: Point, x: f32| {
            let span = b.x - a.x;
            if span.abs() < 1.0e-6 {
                a.y
            } else {
                a.y + (b.y - a.y) * (x - a.x) / span
            }
        };
        match self.mode {
            KeystoneMode::Off => targets,
            KeystoneMode::Vertical => std::array::from_fn(|i| Point {
                x: if matches!(i, 0 | 3) {
                    x_at_y(self.guides[0], self.guides[3], targets[i].y)
                } else {
                    x_at_y(self.guides[1], self.guides[2], targets[i].y)
                },
                y: targets[i].y,
            }),
            KeystoneMode::Horizontal => std::array::from_fn(|i| Point {
                x: targets[i].x,
                y: if matches!(i, 0 | 1) {
                    y_at_x(self.guides[0], self.guides[1], targets[i].x)
                } else {
                    y_at_x(self.guides[3], self.guides[2], targets[i].x)
                },
            }),
            KeystoneMode::Rectangle => self.guides,
        }
    }

    fn sane(self) -> Self {
        let mut guides = self.guides;
        for (i, guide) in guides.iter_mut().enumerate() {
            *guide = guide.sane(Self::TARGETS[i]);
        }
        Self {
            mode: self.mode,
            guides,
            correction: self.correction.clamp(
                *Self::CORRECTION_RANGE.start(),
                *Self::CORRECTION_RANGE.end(),
            ),
            aspect: self
                .aspect
                .clamp(*Self::ASPECT_RANGE.start(), *Self::ASPECT_RANGE.end()),
            crop: self.crop,
        }
    }
}

impl Default for KeystoneParams {
    fn default() -> Self {
        Self {
            mode: KeystoneMode::Off,
            guides: Self::TARGETS,
            correction: 0.8,
            aspect: 0.0,
            crop: KeystoneCrop::Original,
        }
    }
}

impl Rect {
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    /// Smallest extent, as a fraction. A fraction rather than a pixel count because
    /// this type does not know the frame size. One in ten thousand of a 100 MP frame
    /// is still eleven pixels, and of a 10 MP frame is still three — small enough to
    /// be unreachable by a drag, large enough that nothing downstream divides by
    /// zero.
    const MIN: f32 = 1.0e-4;

    pub fn is_full(self) -> bool {
        self == Self::FULL
    }

    /// Centred on the oriented frame, with the given normalised extent.
    pub fn centred(w: f32, h: f32) -> Self {
        Self {
            x: (1.0 - w) * 0.5,
            y: (1.0 - h) * 0.5,
            w,
            h,
        }
    }

    /// A rectangle that is real: finite, and not of zero size.
    ///
    /// **Deliberately does not trim to the unit square.** It used to, which is what
    /// made a straightened crop unable to reach the corners it was entitled to; the
    /// bound that matters is the frame, and only [`Frame::place`] knows it. A
    /// non-finite value falls back to the whole picture rather than to a corner,
    /// because the only way to get one is a hand-edited sidecar and the whole
    /// picture is the honest reading of a number that is not one.
    pub fn sane(self) -> Self {
        if ![self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite())
        {
            return Self::FULL;
        }
        Self {
            w: self.w.max(Self::MIN),
            h: self.h.max(Self::MIN),
            ..self
        }
    }
}

impl Default for Rect {
    fn default() -> Self {
        Self::FULL
    }
}

/// A rectangle in whole frame pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl IRect {
    pub fn dims(self) -> Dims {
        Dims {
            w: self.w as usize,
            h: self.h as usize,
        }
    }
}

/// The aspect the crop drag is locked to, **as a landscape value**.
///
/// On `Params` rather than in the UI because it survives closing the tool: a crop
/// set to 1:1 that silently became freeform the next time a handle was grabbed
/// would be a worse surprise than remembering it. It does not affect the render —
/// the rectangle is the truth and this only shapes the drag that produces it.
///
/// **Portrait is a separate flag, not a second set of entries**, which is how the
/// prototype does it: one combo of landscape ratios beside a `↕` toggle. The
/// alternative — a `Fixed(2, 3)` sitting next to a `Fixed(3, 2)` — doubles a
/// twenty-one item list, and makes "the same crop the other way up" a different
/// selection rather than the same one seen differently.
///
/// `Fixed` carries a float rather than a `w:h` pair because six of the presets are
/// not rational: A4 is √2, XPan is 65:24 as 2.7083, Ōban is 1.52.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Ratio {
    Free,
    /// The uncropped picture's own aspect, after orientation.
    #[default]
    Original,
    /// Width / height, landscape. See the type note.
    Fixed(f32),
}

impl Ratio {
    /// The presets the panel offers, in order, **transcribed from the Python
    /// prototype's `CROP_RATIOS`** — names, values and order.
    ///
    /// the maintainer asked for this list specifically. It is not a set of round numbers
    /// somebody thought sensible: `11×14` and `Ōban` are there because they are
    /// paper he prints on, and a list that quietly dropped them for being untidy
    /// would be missing the ones that get used. Keep it in step with the prototype
    /// rather than tidying it.
    ///
    /// `None` in the second slot marks a **group heading**, which the prototype
    /// carries as source comments; the combo draws them as unselectable labels.
    pub const PRESETS: &'static [(&'static str, Option<Self>)] = &[
        ("Freehand", Some(Self::Free)),
        ("Original", Some(Self::Original)),
        ("Square  1:1", Some(Self::Fixed(1.0))),
        ("Photographic", None),
        ("5:4  —  4×5, 8×10", Some(Self::Fixed(1.25))),
        ("11×14", Some(Self::Fixed(1.2727))),
        ("4:3  —  VGA", Some(Self::Fixed(1.3333))),
        ("5:7", Some(Self::Fixed(1.4))),
        ("3:2  —  4×6, 35mm", Some(Self::Fixed(1.5))),
        ("16:10", Some(Self::Fixed(1.6))),
        ("16:9  —  HDTV", Some(Self::Fixed(1.7778))),
        ("VistaVision  1.85", Some(Self::Fixed(1.85))),
        ("XPan  65:24", Some(Self::Fixed(2.7083))),
        ("Panorama  3:1", Some(Self::Fixed(3.0))),
        ("Paper", None),
        // √2 exactly, not the prototype's rounded 1.4142. The ISO 216 series is
        // *defined* by that ratio — it is what makes A4 fold into A5 — so the
        // constant is the faithful transcription and the literal was the
        // approximation. 1.4e-5 apart, well inside `Ratio::same`'s tolerance, so a
        // sidecar written against either value still reads back as A4.
        ("A4", Some(Self::Fixed(std::f32::consts::SQRT_2))),
        ("Letter  8.5×11", Some(Self::Fixed(1.2941))),
        ("Legal  8.5×14", Some(Self::Fixed(1.6471))),
        ("Tabloid  11×17", Some(Self::Fixed(1.5455))),
        ("Japanese print", None),
        ("Kiku", Some(Self::Fixed(1.4667))),
        ("Ōban", Some(Self::Fixed(1.52))),
    ];

    /// The target width/height for a crop, given the picture it sits in and which
    /// way up the frame is wanted.
    ///
    /// `oriented` is the picture after its quarter turn and **before** any
    /// straighten. Using the straightened bounding box here would make `Original`
    /// mean a different shape at every angle.
    ///
    /// `None` is freeform, which is the absence of a constraint rather than a ratio
    /// of one.
    pub fn of(self, oriented: Dims, portrait: bool) -> Option<f32> {
        let landscape = match self {
            Self::Free => return None,
            // **`Original` stands on its end too.** It first did not, on the
            // argument that the picture's own shape is already the right way up —
            // which is true and is not the question. A landscape frame cropped to a
            // portrait of the same proportions is an ordinary thing to want, and it
            // is the one shape the list otherwise could not express.
            Self::Original => oriented.w as f32 / oriented.h as f32,
            Self::Fixed(v) if v.is_finite() && v > 0.0 => v,
            Self::Fixed(_) => return None,
        };
        Some(if portrait { 1.0 / landscape } else { landscape })
    }

    /// The name the combo shows, or the value if the table has moved on since a
    /// sidecar was written.
    /// How a photographer says the shape of a `w x h` rectangle: `3:2`, not `1.50:1`,
    /// and never `0.67:1` for a portrait.
    ///
    /// Reduce by the GCD and print `a:b` when both sides come out small — which covers
    /// every preset a crop snaps to. Otherwise print a decimal oriented the way the
    /// picture is, so a portrait reads `1:1.50`.
    ///
    /// **No rational approximation.** Calling a freehand crop `4:3` because it is within
    /// a pixel would state a precision nobody asked for, and this app offers √2 and
    /// 2.7083, which have no honest `a:b` at all. A selected preset shows its name.
    pub fn aspect_label(w: u32, h: u32) -> String {
        if w == 0 || h == 0 {
            return "—".to_owned();
        }
        fn gcd(a: u32, b: u32) -> u32 {
            if b == 0 { a } else { gcd(b, a % b) }
        }
        let g = gcd(w, h);
        let (a, b) = (w / g, h / g);
        // 24 admits 16:9, 17:11 and everything a preset produces, and rejects the
        // five-digit pairs a freehand drag lands on.
        if a.max(b) <= 24 {
            return format!("{a}:{b}");
        }
        let (w, h) = (w as f32, h as f32);
        if w >= h {
            format!("{:.2}:1", w / h)
        } else {
            format!("1:{:.2}", h / w)
        }
    }

    pub fn label(self) -> String {
        if let Some((name, _)) = Self::PRESETS
            .iter()
            .find(|(_, r)| r.is_some_and(|r| self.same(r)))
        {
            return (*name).to_owned();
        }
        match self {
            Self::Fixed(v) => format!("{v:.4}"),
            Self::Free => "Freehand".into(),
            Self::Original => "Original".into(),
        }
    }

    /// Equality that tolerates the round-trip through a decimal sidecar value.
    pub fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Fixed(a), Self::Fixed(b)) => (a - b).abs() < 5.0e-5,
            (a, b) => std::mem::discriminant(&a) == std::mem::discriminant(&b),
        }
    }

    /// Whether the `↕` toggle applies. Everything with a shape; only `Freehand`
    /// has none.
    pub fn can_flip(self) -> bool {
        !matches!(self, Self::Free)
    }

    /// Whether a **quarter turn of the picture** should flip the portrait flag.
    ///
    /// Not the same question as [`Ratio::can_flip`], and the difference is easy to
    /// miss. A `Fixed` ratio is absolute, so turning the picture stands it on its
    /// end and the flag has to follow. `Original` is defined *against the frame*,
    /// which transposed at the same moment — so its base already flipped, and
    /// flipping the flag as well would turn it back.
    pub fn flips_with_the_frame(self) -> bool {
        matches!(self, Self::Fixed(_))
    }
}

/// Orientation, straighten, perspective and crop, as a value.
///
/// # Why `orientation` is an `Option`
///
/// `None` is **as shot** — take whatever the EXIF tag says. `Some` is a deliberate
/// override, and it stores the *absolute* result rather than a turn to apply on top
/// of the tag, so reopening a file cannot compose the EXIF rotation with the user's
/// a second time.
///
/// That is what keeps `is_default` a question about the parameters alone, the way
/// it is for every other module. Storing the resolved orientation directly would
/// make an untouched portrait frame read as modified — every such file would open
/// with a ruby dot on a module nobody had touched — because at composition defaults
/// the app really would render it differently. The `Option` says "the user has not
/// decided" without needing the file to hand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositionParams {
    /// Module bypass. **Suppresses crop and straighten, not orientation** — see
    /// [`CompositionParams::bypassed`].
    pub enabled: bool,
    /// `None` is as shot. See the type note.
    pub orientation: Option<Orientation>,
    /// Degrees clockwise, on top of the orientation.
    pub straighten: f32,
    /// Manual projective correction, after straighten has been undone and before
    /// the quarter-turn orientation is mapped back to the stored pixels.
    pub keystone: KeystoneParams,
    /// In normalised coordinates of the **oriented** frame. See the module note —
    /// normalising this against the straightened frame instead was the defect
    /// behind two of the bugs reported in testing.
    pub crop: Rect,
    pub ratio: Ratio,
    /// Whether the locked ratio is stood on its end. The prototype's `↕` button.
    pub portrait: bool,
}

impl CompositionParams {
    /// **±45, the prototype's range**, not the ±15 this first shipped with.
    ///
    /// ±15 was argued from "this is horizon correction, and the auto-crop at 45° on
    /// a 3:2 frame keeps under a third of the picture". Both halves are true and the
    /// conclusion was still wrong, because it was reasoned about the *slider* while
    /// the real control is the draw-a-horizon tool: a line traced down a leaning
    /// doorframe can imply any angle, and silently clamping the picture to 15° when
    /// the user has just told you 22° is the tool disagreeing with its own input.
    ///
    /// 45 rather than 90 because past 45 a drawn line is better read as a vertical,
    /// and `crop::angle_of` wraps it into this range on exactly that argument.
    pub const STRAIGHTEN_RANGE: std::ops::RangeInclusive<f32> = -45.0..=45.0;

    /// The effective orientation, given what the file says.
    pub fn orientation(&self, exif: Orientation) -> Orientation {
        self.orientation.unwrap_or(exif)
    }

    /// What this renders as when the module is switched off.
    ///
    /// **Orientation survives the bypass; crop, straighten and keystone do not**, which is
    /// the one place this module departs from `Params::effective`'s "a bypassed
    /// module renders as its default". The reason is that these fields are not the
    /// same kind of thing: crop, straighten and keystone are decisions about the
    /// picture, and orientation is a *fact about the file* — the same class as decode
    /// and luminance, which `effective` already declines to bypass because "those are
    /// not effects, they are the chain". A switch that laid a portrait frame on its
    /// side would read as a bug in the switch.
    pub fn bypassed(&self) -> Self {
        Self {
            orientation: self.orientation,
            ..Self::default()
        }
    }

    /// What actually applies: these parameters, or their bypass.
    ///
    /// One place, so `Params::effective` and every caller that needs only the
    /// composition — the app resolves a `Frame` several times a frame — cannot
    /// disagree about what "switched off" means.
    pub fn applied(&self) -> Self {
        if self.enabled { *self } else { self.bypassed() }
    }

    /// Whether this changes anything: switched on, and moved off neutral.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.is_default()
    }

    /// Whether the user has touched it, ignoring the bypass. See
    /// `ExposureParams::is_default`.
    pub fn is_default(&self) -> bool {
        Self {
            enabled: Self::default().enabled,
            ..*self
        } == Self::default()
    }

    /// Whether the module's dot should read as modified. See `params::is_modified`.
    pub fn is_modified(&self) -> bool {
        crate::params::is_modified(self.is_default(), self.is_active(), false)
    }

    /// Whether the crop is doing anything. Distinct from `is_active`, because
    /// straightening without cropping is still a composition edit.
    pub fn crops(&self) -> bool {
        self.enabled && !self.crop.is_full()
    }
}

impl Default for CompositionParams {
    fn default() -> Self {
        Self {
            enabled: true,
            orientation: None,
            straighten: 0.0,
            keystone: KeystoneParams::default(),
            crop: Rect::FULL,
            ratio: Ratio::default(),
            portrait: false,
        }
    }
}

/// A row-major 3×3 projective transform. Affines are the subset whose final row is
/// `[0, 0, 1]`; keeping the full form everywhere means perspective is composed into
/// the same one-pass sample rather than becoming a second resampling stage.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Projective([f32; 9]);

impl Projective {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);

    /// `self ∘ other` — apply `other` first.
    fn then(self, outer: Self) -> Self {
        let (a, b) = (self.0, outer.0);
        let mut out = [0.0; 9];
        for row in 0..3 {
            for col in 0..3 {
                out[row * 3 + col] = (0..3).map(|k| b[row * 3 + k] * a[k * 3 + col]).sum();
            }
        }
        Self(out)
    }

    fn translate(dx: f32, dy: f32) -> Self {
        Self([1.0, 0.0, dx, 0.0, 1.0, dy, 0.0, 0.0, 1.0])
    }

    fn scale(sx: f32, sy: f32) -> Self {
        Self([sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0])
    }

    /// Rotate by `deg` **anticlockwise on screen**, which in y-down coordinates is
    /// the negative of the clockwise matrix. The straighten control is clockwise, so
    /// this is its inverse and that is the only direction this is used in.
    fn unrotate(deg: f32) -> Self {
        let (s, c) = deg.to_radians().sin_cos();
        Self([c, s, 0.0, -s, c, 0.0, 0.0, 0.0, 1.0])
    }

    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        let m = self.0;
        let w = m[6] * x + m[7] * y + m[8];
        if w.abs() < 1.0e-8 {
            return (f32::NAN, f32::NAN);
        }
        (
            (m[0] * x + m[1] * y + m[2]) / w,
            (m[3] * x + m[4] * y + m[5]) / w,
        )
    }

    fn inverse(self) -> Option<Self> {
        let m = self.0;
        let det = m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
            + m[2] * (m[3] * m[7] - m[4] * m[6]);
        if !det.is_finite() || det.abs() < 1.0e-10 {
            return None;
        }
        Some(Self([
            (m[4] * m[8] - m[5] * m[7]) / det,
            (m[2] * m[7] - m[1] * m[8]) / det,
            (m[1] * m[5] - m[2] * m[4]) / det,
            (m[5] * m[6] - m[3] * m[8]) / det,
            (m[0] * m[8] - m[2] * m[6]) / det,
            (m[2] * m[3] - m[0] * m[5]) / det,
            (m[3] * m[7] - m[4] * m[6]) / det,
            (m[1] * m[6] - m[0] * m[7]) / det,
            (m[0] * m[4] - m[1] * m[3]) / det,
        ]))
    }

    /// Homography taking `from[i]` to `to[i]`. Eight unknowns with h₂₂ fixed to 1.
    fn from_quad(from: [Point; 4], to: [Point; 4]) -> Option<Self> {
        // A crossed or nearly collapsed guide quadrilateral has no useful
        // photographic interpretation and can put a projective horizon through the
        // image. Refuse it here, at the common CPU/GPU transform boundary, rather
        // than letting an extreme sidecar or a fast handle drag produce infinities.
        if !Self::quad_is_convex(from) || !Self::quad_is_convex(to) {
            return None;
        }
        if from == to {
            return Some(Self::IDENTITY);
        }
        let mut a = [[0.0_f64; 9]; 8];
        for i in 0..4 {
            let (x, y) = (from[i].x as f64, from[i].y as f64);
            let (u, v) = (to[i].x as f64, to[i].y as f64);
            a[i * 2] = [x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y, u];
            a[i * 2 + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y, v];
        }
        for col in 0..8 {
            let pivot =
                (col..8).max_by(|&r0, &r1| a[r0][col].abs().total_cmp(&a[r1][col].abs()))?;
            if a[pivot][col].abs() < 1.0e-10 {
                return None;
            }
            a.swap(col, pivot);
            let d = a[col][col];
            for v in &mut a[col][col..=8] {
                *v /= d;
            }
            // Copied rather than borrowed: the elimination reads the pivot row while
            // writing another, and eight rows of nine f64 is cheaper than convincing
            // the borrow checker that the two are disjoint.
            let pivot_row = a[col];
            for (row, target_row) in a.iter_mut().enumerate() {
                if row == col {
                    continue;
                }
                let f = target_row[col];
                for (target, source) in target_row[col..=8].iter_mut().zip(&pivot_row[col..=8]) {
                    *target -= f * source;
                }
            }
        }
        let mut h = [0.0_f32; 9];
        for i in 0..8 {
            h[i] = a[i][8] as f32;
        }
        h[8] = 1.0;
        h.iter().all(|v| v.is_finite()).then_some(Self(h))
    }

    fn quad_is_convex(q: [Point; 4]) -> bool {
        let mut sign = 0_i8;
        for i in 0..4 {
            let (a, b, c) = (q[i], q[(i + 1) % 4], q[(i + 2) % 4]);
            let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
            if !cross.is_finite() || cross.abs() < 1.0e-5 {
                return false;
            }
            let this_sign = if cross > 0.0 { 1 } else { -1 };
            if sign != 0 && this_sign != sign {
                return false;
            }
            sign = this_sign;
        }
        true
    }
}

/// One image's resolved geometry: the grids, the crop, and the map between them.
///
/// Derived rather than stored, and passed **by value** into the render the way
/// `Params` is, for the reason the handoff gives for `Params` itself — a viewport
/// that reached for ambient orientation state would need keeping in step with the
/// tab, and this is a pure function of three things it already has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// The stored luminance image. `LumaImage::output_dims`.
    pub source: Dims,
    /// After the quarter turn, before straighten.
    pub oriented: Dims,
    /// The grid the graph runs on, uncropped. The straightened bounding box.
    pub frame: Dims,
    /// What is shown, exported and reported, in `frame` pixels.
    pub crop: IRect,
    /// The effective orientation, EXIF and override already resolved.
    pub orientation: Orientation,
    pub straighten: f32,
    /// `frame` pixels -> `source` pixels.
    inverse: Projective,
    projective: bool,
}

impl Frame {
    /// Resolve the geometry for one image under one set of parameters.
    pub fn resolve(source: Dims, exif: Orientation, c: &CompositionParams) -> Self {
        let orientation = c.orientation(exif);
        let oriented = orientation.applied_to(source);
        let theta = c.straighten;

        // The straightened bounding box. `ceil`, for the reason `Roi::grid_for`
        // ceils: the last row is only partly covered and still has to exist.
        let (s, cos) = (
            theta.to_radians().sin().abs(),
            theta.to_radians().cos().abs(),
        );
        let (ow, oh) = (oriented.w as f32, oriented.h as f32);
        let frame = Dims {
            w: (ow * cos + oh * s).ceil().max(1.0) as usize,
            h: (ow * s + oh * cos).ceil().max(1.0) as usize,
        };

        // frame px -> source px, right to left: centre the frame, undo the
        // straighten, un-centre onto the oriented grid, undo the quarter turn.
        let keystone = c.keystone.sane();
        let targets = KeystoneParams::TARGETS;
        let selected = keystone.selected_guides();
        let sources = std::array::from_fn(|i| Point {
            x: targets[i].x + (selected[i].x - targets[i].x) * keystone.correction,
            y: targets[i].y + (selected[i].y - targets[i].y) * keystone.correction,
        });
        let perspective = if keystone.is_active() {
            let guide_map = Projective::from_quad(targets, sources).unwrap_or(Projective::IDENTITY);
            let aspect = (1.0 + keystone.aspect / 100.0).clamp(0.5, 1.5);
            let aspect_map = Projective::translate(-0.5, -0.5)
                .then(Projective::scale(1.0 / aspect, 1.0))
                .then(Projective::translate(0.5, 0.5));
            Projective::scale(1.0 / ow, 1.0 / oh)
                .then(aspect_map)
                .then(guide_map)
                .then(Projective::scale(ow, oh))
        } else {
            // Literal identity, not a homography solved from four equal point pairs.
            // This is what keeps every pre-keystone sidecar bit-identical.
            Projective::IDENTITY
        };

        let inverse = Projective::translate(-(frame.w as f32) * 0.5, -(frame.h as f32) * 0.5)
            .then(Projective::unrotate(theta))
            .then(Projective::translate(ow * 0.5, oh * 0.5))
            .then(perspective)
            .then(orientation.inverse(source));

        let projective = perspective != Projective::IDENTITY;
        let mut f = Self {
            source,
            oriented,
            frame,
            crop: IRect {
                x: 0,
                y: 0,
                w: frame.w as u32,
                h: frame.h as u32,
            },
            orientation,
            straighten: theta,
            inverse,
            projective,
        };
        f.crop = f.place(c.crop);
        f
    }

    /// A normalised crop, as whole pixels on the frame grid.
    ///
    /// **The two grids meet here, and they are concentric rather than aligned.** The
    /// rectangle is normalised to `oriented`, which does not move with the angle;
    /// the grid it has to land on is `frame`, which grows around the same centre. So
    /// the extent comes from `oriented` — making a crop's pixel size, its aspect and
    /// its exported dimensions independent of the straighten — and the offset is
    /// measured from the shared centre.
    ///
    /// Clamped to the frame, not to the oriented picture: a straightened crop is
    /// entitled to the corners the rotation emptied. Every consumer allocates
    /// against this — the export buffer, the histogram's sample count, the footer's
    /// print size — so it is never empty and never outside the grid.
    pub fn place(&self, r: Rect) -> IRect {
        let r = r.sane();
        let (ow, oh) = (self.oriented.w as f32, self.oriented.h as f32);
        let (fw, fh) = (self.frame.w as f32, self.frame.h as f32);
        // The oriented picture's top-left, in frame pixels. Zero when unstraightened.
        let (ox, oy) = ((fw - ow) * 0.5, (fh - oh) * 0.5);

        let w = (r.w * ow).round().clamp(1.0, fw);
        let h = (r.h * oh).round().clamp(1.0, fh);
        let x = (ox + r.x * ow).round().clamp(0.0, fw - w);
        let y = (oy + r.y * oh).round().clamp(0.0, fh - h);
        IRect {
            x: x as i32,
            y: y as i32,
            w: w as u32,
            h: h as u32,
        }
    }

    /// The inverse of [`Frame::place`]: whole frame pixels back to a normalised
    /// crop. What a drag produces, since a drag happens in pixels.
    pub fn unplace(&self, r: IRect) -> Rect {
        let (ow, oh) = (self.oriented.w as f32, self.oriented.h as f32);
        let (fw, fh) = (self.frame.w as f32, self.frame.h as f32);
        let (ox, oy) = ((fw - ow) * 0.5, (fh - oh) * 0.5);
        Rect {
            x: (r.x as f32 - ox) / ow,
            y: (r.y as f32 - oy) / oh,
            w: r.w as f32 / ow,
            h: r.h as f32 / oh,
        }
        .sane()
    }

    /// The dims everything that reports a size reads: the footer, Info, and export.
    pub fn output_dims(&self) -> Dims {
        self.crop.dims()
    }

    /// Whether the crop is the whole frame. The crop tool suppresses the crop
    /// rather than the module, so this is not the same question as `is_default`.
    pub fn is_uncropped(&self) -> bool {
        self.crop
            == IRect {
                x: 0,
                y: 0,
                w: self.frame.w as u32,
                h: self.frame.h as u32,
            }
    }

    /// Whether sampling has to interpolate.
    ///
    /// A quarter turn maps whole pixels onto whole pixels, so nearest-neighbour is
    /// still showing real data and the input node's "no interpolation invents
    /// detail" rule holds. A straighten does not: there is no source pixel under a
    /// rotated sample position, so nearest-neighbour would show the frame's own
    /// jaggies rather than the picture's. Interpolation is not a preference here,
    /// it is the only honest answer.
    pub fn resamples(&self) -> bool {
        self.straighten != 0.0 || self.projective
    }

    /// The projective map the shader applies, as nine row-major floats.
    pub fn inverse(&self) -> [f32; 9] {
        self.inverse.0
    }

    /// A frame coordinate, in source pixels. The CPU twin of the shader's
    /// `source_coord`, and what the histogram and the footer readout sample through.
    pub fn to_source(&self, fx: f32, fy: f32) -> (f32, f32) {
        self.inverse.apply(fx, fy)
    }

    /// A source coordinate, in frame pixels. The other direction.
    ///
    /// **Needed the moment something stored in source coordinates has to be
    /// drawn.** Dodge & Burn's dabs and gradients are welded to the negative — see
    /// `raw_core::dodgeburn` — so placing a gradient's handles on screen is exactly
    /// this map. It is inverted here rather than storing a second matrix; a malformed
    /// or degenerate projective fit safely falls back to identity.
    pub fn from_source(&self, sx: f32, sy: f32) -> (f32, f32) {
        self.inverse
            .inverse()
            .unwrap_or(Projective::IDENTITY)
            .apply(sx, sy)
    }

    /// A persisted perspective guide point to the corrected frame. Guides live in
    /// the oriented photograph, so they stay attached to the chosen architectural
    /// line while the homography itself changes underneath them.
    pub fn guide_to_frame(&self, point: Point) -> (f32, f32) {
        let oriented = (
            point.x * self.oriented.w as f32,
            point.y * self.oriented.h as f32,
        );
        let source = self
            .orientation
            .inverse(self.source)
            .apply(oriented.0, oriented.1);
        self.from_source(source.0, source.1)
    }

    /// The inverse of [`Frame::guide_to_frame`], used by a handle drag.
    pub fn frame_to_guide(&self, fx: f32, fy: f32) -> Point {
        let source = self.to_source(fx, fy);
        let oriented = self
            .orientation
            .inverse(self.source)
            .inverse()
            .unwrap_or(Projective::IDENTITY)
            .apply(source.0, source.1);
        Point {
            x: (oriented.0 / self.oriented.w.max(1) as f32).clamp(0.0, 1.0),
            y: (oriented.1 / self.oriented.h.max(1) as f32).clamp(0.0, 1.0),
        }
    }

    /// The largest crop of aspect `ratio` that contains only real pixels, centred.
    ///
    /// The whole of the straighten auto-crop, and it is four lines because the
    /// constraint is: a centred box of half-extents `(a, b)` fits inside the
    /// oriented frame's half-extents `(A, B)` rotated by θ exactly when
    ///
    /// ```text
    ///     a·cos + b·sin <= A          (the box's corner, projected onto the
    ///     a·sin + b·cos <= B           frame's own two axes)
    /// ```
    ///
    /// Fix `a/b = r` and each line gives an upper bound on `a`; the answer is the
    /// smaller. Returned in normalised coordinates of the straightened frame, so it
    /// is a crop rectangle like any other and a handle can drag straight back out of
    /// it — which is what "auto-crop, adjustable" means.
    pub fn inscribed(&self, ratio: f32) -> Rect {
        let (s, c) = (
            self.straighten.to_radians().sin().abs(),
            self.straighten.to_radians().cos().abs(),
        );
        let (a_max, b_max) = (self.oriented.w as f32 * 0.5, self.oriented.h as f32 * 0.5);
        // `r * s + c` and `r * c + s` are both >= 1 for any real ratio, so neither
        // divisor can vanish however the angle is set.
        let a = (a_max * ratio / (ratio * c + s)).min(b_max * ratio / (ratio * s + c));
        let b = a / ratio;
        // **A pixel of inset, and removing it does not converge.** The constraint is
        // solved in continuous coordinates; [`Frame::place`] then rounds offset and
        // extent *independently*, so an edge can land a whole frame pixel outside the
        // rectangle solved for, while [`Frame::covers`] allows half of one. Without the
        // inset the auto-crop's own output fails the coverage test that decides whether
        // to auto-crop, so every frame of a straighten drag re-fits a slightly smaller
        // crop. `the_inscribed_crop_passes_the_test_that_asks_for_it` measures it.
        //
        // A factor rather than a subtraction, so the ratio asked for survives. **Not at
        // zero degrees**, where `place` rounds nothing: straightening back to 0 has to
        // give the whole frame back — `an_unstraightened_frame_inscribes_itself`.
        let (a, b) = if self.straighten == 0.0 {
            (a, b)
        } else {
            let inset = ((a - 1.0) / a).min((b - 1.0) / b).clamp(0.0, 1.0);
            (a * inset, b * inset)
        };
        // Normalised against `oriented`, like every other `Rect` — and the same
        // halves the constraint was solved in, so this needs no second conversion.
        Rect::centred(a / a_max, b / b_max).sane()
    }

    /// The aspect the crop should snap to, or `None` for freeform.
    ///
    /// Against `oriented`, so `Original` means one shape at every angle.
    pub fn target_ratio(&self, ratio: Ratio, portrait: bool) -> Option<f32> {
        ratio.of(self.oriented, portrait)
    }

    /// Whether every corner of `r` has real pixels behind it.
    ///
    /// The question the straighten auto-crop asks. Only the four corners are tested,
    /// and that is sufficient rather than approximate: the region with data is the
    /// source rectangle under a projective map, so it remains convex, and an
    /// axis-aligned rectangle lies inside a convex region exactly when its corners do.
    ///
    /// Half a pixel of slack, because a crop is rounded to whole frame pixels and
    /// the corner it was cut from was computed in continuous ones. Without it a
    /// freshly inscribed crop reports itself uncovered and the auto-crop shrinks the
    /// picture a little more on every frame of a slider drag.
    pub fn covers(&self, r: Rect) -> bool {
        let c = self.place(r);
        let (x1, y1) = ((c.x + c.w as i32) as f32, (c.y + c.h as i32) as f32);
        self.covers_px(c.x as f32, c.y as f32, x1, y1, -0.5)
    }

    /// The continuous twin of [`Frame::covers`]: a rectangle given in **frame pixels**,
    /// with `margin` pixels demanded at every corner.
    ///
    /// A negative margin is slack, and that is what `covers` passes — half a pixel of
    /// it, because the rectangle it is handed has been rounded to whole frame pixels
    /// and the corner it was cut from was computed in continuous ones.
    ///
    /// **A positive margin is what a search wants.** The crop tool solves for the
    /// largest rectangle that fits and then rounds it once; without room to spare, the
    /// rounding is free to put the answer back outside the picture, and the caller
    /// would have to check its own result and retry. Asking for the margin up front is
    /// the same trick `inscribed`'s inset is, said in the one place both can use.
    pub fn covers_px(&self, x0: f32, y0: f32, x1: f32, y1: f32, margin: f32) -> bool {
        let (w, h) = (self.source.w as f32, self.source.h as f32);
        [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
            .into_iter()
            .map(|(x, y)| self.to_source(x, y))
            .all(|(u, v)| u >= margin && v >= margin && u <= w - margin && v <= h - margin)
    }

    /// Largest useful axis-aligned crop after a projective warp. The valid image
    /// footprint is a convex quadrilateral because projective maps keep straight
    /// lines straight. Searching pairs of horizontal cuts and intersecting that
    /// quadrilateral is therefore both stable and cheap; this runs only when a
    /// guide gesture ends or the user asks for a crop, never during rendering.
    pub fn keystone_crop(&self, kind: KeystoneCrop) -> Rect {
        if !self.projective && self.straighten == 0.0 {
            return Rect::FULL;
        }
        let sw = self.source.w as f32;
        let sh = self.source.h as f32;
        let poly = [
            self.from_source(0.0, 0.0),
            self.from_source(sw, 0.0),
            self.from_source(sw, sh),
            self.from_source(0.0, sh),
        ];
        if poly.iter().any(|(x, y)| !x.is_finite() || !y.is_finite()) {
            return Rect::FULL;
        }
        let min_y = poly
            .iter()
            .map(|p| p.1)
            .fold(f32::INFINITY, f32::min)
            .clamp(0.0, self.frame.h as f32);
        let max_y = poly
            .iter()
            .map(|p| p.1)
            .fold(f32::NEG_INFINITY, f32::max)
            .clamp(0.0, self.frame.h as f32);
        if max_y - min_y < 2.0 {
            return Rect::FULL;
        }

        let span_at = |y: f32| -> Option<(f32, f32)> {
            let mut xs = Vec::with_capacity(4);
            for i in 0..4 {
                let a = poly[i];
                let b = poly[(i + 1) % 4];
                let (lo, hi) = (a.1.min(b.1), a.1.max(b.1));
                if y + 1.0e-4 < lo || y - 1.0e-4 > hi || (b.1 - a.1).abs() < 1.0e-6 {
                    continue;
                }
                let t = ((y - a.1) / (b.1 - a.1)).clamp(0.0, 1.0);
                xs.push(a.0 + (b.0 - a.0) * t);
            }
            xs.sort_by(f32::total_cmp);
            (xs.len() >= 2).then(|| (*xs.first().unwrap(), *xs.last().unwrap()))
        };

        const STEPS: usize = 72;
        let ys: Vec<f32> = (0..=STEPS)
            .map(|i| min_y + (max_y - min_y) * i as f32 / STEPS as f32)
            .collect();
        let original = self.oriented.w as f32 / self.oriented.h.max(1) as f32;
        let mut best = None::<(f32, f32, f32, f32, f32)>; // area, x, y, w, h
        for (i, &y0) in ys.iter().enumerate() {
            for &y1 in ys.iter().skip(i + 1) {
                let h = y1 - y0;
                if h < 1.0 {
                    continue;
                }
                let mut critical = vec![y0, y1];
                critical.extend(poly.iter().map(|p| p.1).filter(|y| *y > y0 && *y < y1));
                let mut left = f32::NEG_INFINITY;
                let mut right = f32::INFINITY;
                let mut valid = true;
                for y in critical {
                    let Some((l, r)) = span_at(y) else {
                        valid = false;
                        break;
                    };
                    left = left.max(l);
                    right = right.min(r);
                }
                if !valid {
                    continue;
                }
                left = left.max(0.0) + 1.0;
                right = right.min(self.frame.w as f32) - 1.0;
                let available = right - left;
                let w = match kind {
                    KeystoneCrop::Largest => available,
                    KeystoneCrop::Original => original * h,
                };
                if w < 1.0 || w > available {
                    continue;
                }
                let x = (left + right - w) * 0.5;
                let area = w * h;
                if best.is_none_or(|b| area > b.0) {
                    best = Some((area, x, y0, w, h));
                }
            }
        }
        let Some((_, x, y, w, h)) = best else {
            return Rect::FULL;
        };
        self.unplace(IRect {
            x: x.round() as i32,
            y: y.round() as i32,
            w: w.round().max(1.0) as u32,
            h: h.round().max(1.0) as u32,
        })
    }

    /// The crop's own aspect, in frame pixels.
    pub fn crop_ratio(&self) -> f32 {
        self.crop.w as f32 / self.crop.h.max(1) as f32
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn from_source_undoes_to_source() {
        // The round trip, under every composition that changes the map: a quarter
        // turn transposes it, a straighten rotates it, and a crop must not affect
        // it at all — the crop is a region, not a transform.
        for (orientation, straighten) in [
            (Orientation::Rotate0, 0.0),
            (Orientation::Rotate90, 0.0),
            (Orientation::Rotate270, 0.0),
            (Orientation::Rotate0, 7.5),
            (Orientation::Rotate180, -3.25),
        ] {
            let c = CompositionParams {
                straighten,
                crop: Rect {
                    x: 0.2,
                    y: 0.1,
                    w: 0.5,
                    h: 0.6,
                },
                ..Default::default()
            };
            let f = Frame::resolve(Dims { w: 600, h: 400 }, orientation, &c);
            for (sx, sy) in [(0.0, 0.0), (123.5, 77.25), (599.0, 399.0), (300.0, 200.0)] {
                let (fx, fy) = f.from_source(sx, sy);
                let (bx, by) = f.to_source(fx, fy);
                assert!(
                    (bx - sx).abs() < 1e-2 && (by - sy).abs() < 1e-2,
                    "{orientation:?} {straighten}deg: ({sx}, {sy}) -> ({fx}, {fy}) -> ({bx}, {by})"
                );
            }
        }
    }
    use super::*;

    #[test]
    fn a_fresh_crop_keeps_the_original_shape() {
        assert_eq!(Ratio::default(), Ratio::Original);
        assert_eq!(CompositionParams::default().ratio, Ratio::Original);
    }

    fn dims(w: usize, h: usize) -> Dims {
        Dims { w, h }
    }

    fn frame_of(src: Dims, o: Orientation) -> Frame {
        Frame::resolve(src, o, &CompositionParams::default())
    }

    fn with(c: CompositionParams, src: Dims) -> Frame {
        Frame::resolve(src, Orientation::Rotate0, &c)
    }

    /// The four corners of a frame, mapped back to source, rounded for comparison.
    fn corners(f: &Frame) -> [(i32, i32); 4] {
        let (w, h) = (f.frame.w as f32, f.frame.h as f32);
        [(0.0, 0.0), (w, 0.0), (w, h), (0.0, h)]
            .map(|(x, y)| f.to_source(x, y))
            .map(|(x, y)| (x.round() as i32, y.round() as i32))
    }

    #[test]
    fn the_exif_tag_becomes_a_quarter_turn() {
        // The defect Composition opened with: two files in the corpus are tag 8
        // and displayed sideways because nothing read this.
        assert_eq!(Orientation::from_exif(1), Orientation::Rotate0);
        assert_eq!(Orientation::from_exif(3), Orientation::Rotate180);
        assert_eq!(Orientation::from_exif(6), Orientation::Rotate90);
        assert_eq!(Orientation::from_exif(8), Orientation::Rotate270);
        // Unset, and out of range. Both mean "upright", which is what the app did
        // before it read the tag at all — an unrecognised tag must not be a new
        // failure mode.
        assert_eq!(Orientation::from_exif(0), Orientation::Rotate0);
        assert_eq!(Orientation::from_exif(9), Orientation::Rotate0);
        // The mirrored orientations contribute their rotation and drop the flip.
        assert_eq!(Orientation::from_exif(5), Orientation::Rotate90);
        assert_eq!(Orientation::from_exif(7), Orientation::Rotate270);
    }

    #[test]
    fn turning_four_times_returns_to_where_it_started() {
        let mut o = Orientation::Rotate0;
        for _ in 0..4 {
            o = o.right();
        }
        assert_eq!(o, Orientation::Rotate0);
        assert_eq!(Orientation::Rotate90.left(), Orientation::Rotate0);
        assert_eq!(Orientation::Rotate0.left().right(), Orientation::Rotate0);
    }

    #[test]
    fn a_quarter_turn_transposes_the_frame_and_a_half_turn_does_not() {
        let src = dims(6000, 4000);
        assert_eq!(frame_of(src, Orientation::Rotate0).frame, dims(6000, 4000));
        assert_eq!(frame_of(src, Orientation::Rotate90).frame, dims(4000, 6000));
        assert_eq!(
            frame_of(src, Orientation::Rotate180).frame,
            dims(6000, 4000)
        );
        assert_eq!(
            frame_of(src, Orientation::Rotate270).frame,
            dims(4000, 6000)
        );
    }

    #[test]
    fn each_turn_sends_the_frame_corners_to_the_right_source_corners() {
        // The one place a sign error is invisible until a picture is upside down.
        // Read the frame's top-left corner and ask which corner of the sensor it is.
        let src = dims(600, 400);
        let tl = |o| corners(&frame_of(src, o))[0];

        assert_eq!(
            tl(Orientation::Rotate0),
            (0, 0),
            "upright: the frame IS the source"
        );
        // Rotate90 turns the picture clockwise, so what was the bottom-left of the
        // sensor arrives at the top-left of the frame.
        assert_eq!(tl(Orientation::Rotate90), (0, 400));
        assert_eq!(tl(Orientation::Rotate180), (600, 400));
        // Rotate270 is anticlockwise: the sensor's top-right arrives top-left.
        assert_eq!(tl(Orientation::Rotate270), (600, 0));
    }

    #[test]
    fn every_orientation_maps_the_frame_exactly_onto_the_source() {
        // No turn may lose or invent a pixel: the four mapped corners must be the
        // four source corners, in some order. This is what makes the quarter turns
        // resample-free.
        let src = dims(600, 400);
        let want = [(0, 0), (600, 0), (600, 400), (0, 400)];
        for o in [
            Orientation::Rotate0,
            Orientation::Rotate90,
            Orientation::Rotate180,
            Orientation::Rotate270,
        ] {
            let mut got = corners(&frame_of(src, o));
            got.sort();
            let mut want = want;
            want.sort();
            assert_eq!(got, want, "{o:?} did not cover the source exactly");
        }
    }

    #[test]
    fn an_aspect_reads_the_way_a_photographer_says_it() {
        // The complaint, first: a portrait frame read `0.67:1`.
        assert_eq!(Ratio::aspect_label(4000, 6000), "2:3");
        assert_eq!(Ratio::aspect_label(6000, 4000), "3:2");
        // Real sensors, whose pixel counts are not round.
        assert_eq!(Ratio::aspect_label(5472, 3648), "3:2");
        assert_eq!(Ratio::aspect_label(2160, 1440), "3:2");
        assert_eq!(Ratio::aspect_label(4000, 3000), "4:3");
        assert_eq!(Ratio::aspect_label(1920, 1080), "16:9");
        assert_eq!(Ratio::aspect_label(3000, 3000), "1:1");
        // A freehand drag reduces to nothing useful, so it gets a decimal — and the
        // portrait one puts it on the right, which is the whole point.
        assert_eq!(Ratio::aspect_label(4001, 2999), "1.33:1");
        assert_eq!(Ratio::aspect_label(2999, 4001), "1:1.33");
        // Degenerate rather than panicking.
        assert_eq!(Ratio::aspect_label(0, 100), "—");
    }

    #[test]
    fn a_quarter_turn_does_not_resample_and_a_straighten_does() {
        // The rule the input node's sampling branch reads. A quarter turn lands on
        // whole pixels, so nearest is still showing real data; a rotation has no
        // pixel under the sample position and nearest would show frame jaggies.
        let src = dims(600, 400);
        assert!(!frame_of(src, Orientation::Rotate90).resamples());
        let c = CompositionParams {
            straighten: 2.5,
            ..Default::default()
        };
        assert!(Frame::resolve(src, Orientation::Rotate0, &c).resamples());
    }

    #[test]
    fn straightening_grows_the_frame_to_the_bounding_box() {
        // 600x400 at 30°: 600·cos30 + 400·sin30 = 719.6, 600·sin30 + 400·cos30 = 646.4.
        // Well outside STRAIGHTEN_RANGE, and used here because the numbers are
        // checkable by hand.
        let c = CompositionParams {
            straighten: 30.0,
            ..Default::default()
        };
        let f = Frame::resolve(dims(600, 400), Orientation::Rotate0, &c);
        assert_eq!(
            f.frame,
            dims(720, 647),
            "the bounding box must ceil, not round"
        );
        assert_eq!(
            f.oriented,
            dims(600, 400),
            "straighten must not disturb the turn"
        );
    }

    #[test]
    fn a_crop_does_not_move_or_change_shape_when_the_picture_is_straightened() {
        // **The bug the maintainer reported as the biggest one in testing, as an assertion.**
        //
        // The crop used to be normalised against the straightened bounding box,
        // which grows with the angle — so the same stored fractions covered more
        // pixels at every step, the auto-crop pulled back against that growth, and
        // the result wandered. Measured then: a 1:1 crop became 0.9926 at half a
        // degree, and its pixel size went 4000x4000, 4023x4053, 3988x4016 — not even
        // monotonic. Two reported symptoms, "straighten enlarges uncontrollably" and
        // "ratios do not stick", were this one defect.
        //
        // Normalised against `oriented`, which does not move, a crop's pixel size is
        // simply not a function of the angle. Walked in fine steps because the real
        // gesture is a slider drag, and the old failure compounded per step.
        let src = dims(6000, 4000);
        let mut c = CompositionParams {
            crop: Rect {
                x: 0.2,
                y: 0.15,
                w: 0.5,
                h: 0.5,
            },
            ..Default::default()
        };
        let flat = with(c, src).crop;
        for step in 1..=30 {
            c.straighten = step as f32 * 0.5;
            let f = with(c, src);
            assert_eq!(
                (f.crop.w, f.crop.h),
                (flat.w, flat.h),
                "the crop changed size at {}°",
                c.straighten
            );
            // And it stays over the same part of the picture: both grids share a
            // centre, so the crop's centre must stay put relative to the frame's.
            let cx = f.crop.x as f32 + f.crop.w as f32 * 0.5 - f.frame.w as f32 * 0.5;
            let want = flat.x as f32 + flat.w as f32 * 0.5 - src.w as f32 * 0.5;
            assert!(
                (cx - want).abs() <= 1.0,
                "the crop drifted at {}°",
                c.straighten
            );
        }
    }

    #[test]
    fn a_locked_ratio_holds_through_a_straighten_drag() {
        // The other half of the same report. The auto-crop re-fits whenever the
        // corners go empty, and each re-fit has to land on the ratio that was asked
        // for — not on the ratio the previous re-fit happened to leave behind.
        let src = dims(6000, 4000);
        let mut c = CompositionParams {
            ratio: Ratio::Fixed(1.0),
            ..Default::default()
        };
        c.crop = with(c, src).inscribed(1.0);

        for step in 1..=30 {
            c.straighten = step as f32 * 0.5;
            let f = with(c, src);
            if !f.covers(c.crop) {
                let target = f.target_ratio(c.ratio, c.portrait).expect("locked");
                c.crop = f.inscribed(target);
            }
            let placed = with(c, src).crop;
            let got = placed.w as f32 / placed.h as f32;
            assert!(
                (got - 1.0).abs() < 3.0e-3,
                "the 1:1 lock drifted to {got} at {}°",
                c.straighten
            );
        }

        // And it shrank monotonically rather than wandering — the auto-crop pulling
        // in, not the auto-crop fighting the frame.
        let mut prev = u32::MAX;
        let mut c2 = CompositionParams {
            ratio: Ratio::Fixed(1.0),
            ..Default::default()
        };
        c2.crop = with(c2, src).inscribed(1.0);
        for step in 0..=30 {
            c2.straighten = step as f32 * 0.5;
            let f = with(c2, src);
            if !f.covers(c2.crop) {
                c2.crop = f.inscribed(1.0);
            }
            let w = with(c2, src).crop.w;
            assert!(
                w <= prev,
                "the crop grew at {}°: {prev} -> {w}",
                c2.straighten
            );
            prev = w;
        }
    }

    #[test]
    fn a_straightened_crop_may_reach_into_the_emptied_corners() {
        // the maintainer's decision was "auto-crop, adjustable": a handle drags back out and
        // the blank corners come back. That requires the rectangle to be expressible
        // outside the oriented picture, which is why `Rect` is no longer trimmed to
        // the unit square — only the *frame* bounds it.
        let c = CompositionParams {
            straighten: 8.0,
            crop: Rect {
                x: -0.05,
                y: -0.05,
                w: 1.1,
                h: 1.1,
            },
            ..Default::default()
        };
        let f = with(c, dims(600, 400));
        assert!(
            !f.covers(c.crop),
            "a crop over the empty corners must report as uncovered"
        );
        // ...and it is still a real, allocatable rectangle inside the frame.
        assert!(f.crop.w >= 1 && f.crop.h >= 1);
        assert!(f.crop.x >= 0 && f.crop.x + f.crop.w as i32 <= f.frame.w as i32);
        assert!(f.crop.y >= 0 && f.crop.y + f.crop.h as i32 <= f.frame.h as i32);
    }

    #[test]
    fn placing_and_unplacing_a_crop_round_trips() {
        // `crop.rs` works in frame pixels and converts at both ends. If these two
        // disagreed, every drag would nudge the rectangle by the difference — a
        // crop that creeps while you hold a handle still.
        for angle in [0.0, 3.0, -7.5, 15.0f32] {
            let c = CompositionParams {
                straighten: angle,
                ..Default::default()
            };
            let f = with(c, dims(6000, 4000));
            for r in [
                Rect::FULL,
                Rect {
                    x: 0.2,
                    y: 0.1,
                    w: 0.5,
                    h: 0.6,
                },
                Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.25,
                    h: 0.25,
                },
            ] {
                let back = f.unplace(f.place(r));
                for (a, b, name) in [
                    (r.x, back.x, "x"),
                    (r.y, back.y, "y"),
                    (r.w, back.w, "w"),
                    (r.h, back.h, "h"),
                ] {
                    assert!((a - b).abs() < 1.0e-3, "{angle}°: {name} {a} -> {b}");
                }
            }
        }
    }

    #[test]
    fn straighten_and_its_negative_give_the_same_frame() {
        // The bounding box is symmetric in the angle; only the content leans the
        // other way. A frame that changed size with the sign would move the crop.
        let f = |a: f32| {
            let c = CompositionParams {
                straighten: a,
                ..Default::default()
            };
            Frame::resolve(dims(6000, 4000), Orientation::Rotate0, &c).frame
        };
        assert_eq!(f(7.5), f(-7.5));
    }

    #[test]
    fn straighten_leans_the_picture_clockwise() {
        // Sign, pinned — and pinned as the property rather than as a corner, because
        // the corners of a rotated bounding box are the one place the answer is not
        // obvious by inspection. A positive angle turns the picture clockwise, which
        // is what corrects a horizon falling to the left.
        //
        // Walk right along one row of the frame and the source row being read must
        // go UP: a fixed row of the source therefore appears in the frame sloping
        // down to the right, which is a clockwise turn.
        let c = CompositionParams {
            straighten: 10.0,
            ..Default::default()
        };
        let f = Frame::resolve(dims(600, 400), Orientation::Rotate0, &c);
        let mid = f.frame.h as f32 * 0.5;
        let (_, left) = f.to_source(f.frame.w as f32 * 0.2, mid);
        let (_, right) = f.to_source(f.frame.w as f32 * 0.8, mid);
        assert!(
            right < left,
            "the picture leans anticlockwise: {left:.1} -> {right:.1}"
        );

        // And the negative angle is the mirror of it, so the control is symmetric.
        let c = CompositionParams {
            straighten: -10.0,
            ..Default::default()
        };
        let f = Frame::resolve(dims(600, 400), Orientation::Rotate0, &c);
        let (_, left) = f.to_source(f.frame.w as f32 * 0.2, mid);
        let (_, right) = f.to_source(f.frame.w as f32 * 0.8, mid);
        assert!(right > left, "a negative angle must lean the other way");
    }

    #[test]
    fn the_empty_corners_of_a_straightened_frame_read_outside_the_source() {
        // What makes the auto-crop necessary in the first place: after a rotation
        // the bounding box has four triangles with no data behind them, and each
        // corner must map somewhere the sampler will report as uncovered. If any of
        // them landed inside the source, the frame would be showing a picture that
        // is not there.
        let c = CompositionParams {
            straighten: 10.0,
            ..Default::default()
        };
        let f = Frame::resolve(dims(600, 400), Orientation::Rotate0, &c);
        for (x, y) in corners(&f).map(|(x, y)| (x as f32, y as f32)) {
            let outside = x < 0.0 || y < 0.0 || x > 600.0 || y > 400.0;
            assert!(outside, "corner ({x}, {y}) is inside the source");
        }
    }

    #[test]
    fn the_identity_composition_is_the_identity_transform() {
        // The whole existing pipeline is this case, and it must reduce to exactly
        // what `source_coord` computed before this module existed — not to something
        // within a rounding error of it.
        let f = frame_of(dims(6000, 4000), Orientation::Rotate0);
        assert_eq!(f.inverse(), Projective::IDENTITY.0);
        assert_eq!(f.to_source(123.0, 456.0), (123.0, 456.0));
        assert!(f.is_uncropped());
        assert_eq!(f.output_dims(), dims(6000, 4000));
    }

    #[test]
    fn the_crop_is_a_fraction_so_sampling_mode_cannot_move_it() {
        // The defect the spacer percentage already fixed, in the module where it
        // would move the subject out of the picture: SuperPixel is half resolution,
        // and a crop in pixels would cover a different quarter of the frame after a
        // change of sampling mode.
        let c = CompositionParams {
            crop: Rect {
                x: 0.25,
                y: 0.25,
                w: 0.5,
                h: 0.5,
            },
            ..Default::default()
        };
        let full = Frame::resolve(dims(6000, 4000), Orientation::Rotate0, &c);
        let half = Frame::resolve(dims(3000, 2000), Orientation::Rotate0, &c);
        assert_eq!(
            full.crop,
            IRect {
                x: 1500,
                y: 1000,
                w: 3000,
                h: 2000
            }
        );
        assert_eq!(
            half.crop,
            IRect {
                x: 750,
                y: 500,
                w: 1500,
                h: 1000
            }
        );
        // The same part of the picture, in both.
        assert_eq!(
            full.crop.w as f32 / full.frame.w as f32,
            half.crop.w as f32 / half.frame.w as f32
        );
    }

    #[test]
    fn a_crop_does_not_shrink_the_grid_the_chain_runs_on() {
        // The load-bearing property of this module. Contrast Mask's apron is
        // clamped to `frame`, so a blur at the crop boundary reads real pixels
        // outside it — and moving a handle changes nothing above the sink.
        let c = CompositionParams {
            crop: Rect {
                x: 0.4,
                y: 0.4,
                w: 0.2,
                h: 0.2,
            },
            ..Default::default()
        };
        let f = Frame::resolve(dims(6000, 4000), Orientation::Rotate0, &c);
        assert_eq!(
            f.frame,
            dims(6000, 4000),
            "the crop shrank the working grid"
        );
        assert_eq!(f.output_dims(), dims(1200, 800), "but not what is reported");
        assert!(!f.is_uncropped());
    }

    #[test]
    fn a_degenerate_crop_still_allocates_something() {
        // These numbers become buffer sizes and divisors. A zero-width crop must be
        // impossible to express rather than merely unlikely.
        for r in [
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            Rect {
                x: 2.0,
                y: -1.0,
                w: 5.0,
                h: 5.0,
            },
            Rect {
                x: 0.999,
                y: 0.999,
                w: 0.5,
                h: 0.5,
            },
            Rect {
                x: f32::NAN,
                y: 0.0,
                w: 0.5,
                h: 0.5,
            },
        ] {
            let c = CompositionParams {
                crop: r,
                ..Default::default()
            };
            let f = Frame::resolve(dims(600, 400), Orientation::Rotate0, &c);
            assert!(f.crop.w >= 1 && f.crop.h >= 1, "{r:?} gave {:?}", f.crop);
            assert!(f.crop.x >= 0 && f.crop.y >= 0, "{r:?} gave {:?}", f.crop);
            assert!(
                f.crop.x + f.crop.w as i32 <= 600,
                "{r:?} escaped right: {:?}",
                f.crop
            );
            assert!(
                f.crop.y + f.crop.h as i32 <= 400,
                "{r:?} escaped bottom: {:?}",
                f.crop
            );
        }
    }

    #[test]
    fn the_inscribed_crop_passes_the_test_that_asks_for_it() {
        // `covers` is what decides whether to auto-crop and `inscribed` is what the
        // auto-crop returns, so an `inscribed` rectangle that `covers` rejects is a
        // loop: every frame of a straighten drag re-fits a crop that is already fitted
        // and hands back a slightly smaller one. It shrank about half a percent per
        // frame, which at sixty frames a second is most of the picture in two seconds.
        //
        // The two disagreed because `place` rounds the offset and the extent
        // independently — see the inset in `inscribed`. Measured across the angles and
        // shapes where the rounding actually bites, and note the odd sizes: an even
        // frame at an even angle rounds cleanly and would have passed throughout.
        for angle in [
            -30.0, -15.0, -7.5, -4.63, -1.0, 0.5, 1.5, 7.5, 15.0, 31.7, 45.0f32,
        ] {
            for (sw, sh) in [
                (600, 400),
                (400, 600),
                (500, 500),
                (5201, 7863),
                (4001, 2999),
            ] {
                for ratio in [1.0, 1.5, 2.0 / 3.0, std::f32::consts::SQRT_2] {
                    let c = CompositionParams {
                        straighten: angle,
                        ..Default::default()
                    };
                    let f = Frame::resolve(dims(sw, sh), Orientation::Rotate0, &c);
                    let r = f.inscribed(ratio);
                    assert!(
                        f.covers(r),
                        "{angle}° on {sw}x{sh} at {ratio}: inscribed {:?} is not covered",
                        f.place(r)
                    );
                }
            }
        }
    }

    #[test]
    fn the_inscribed_crop_contains_only_real_pixels() {
        // Auto-crop, measured rather than asserted: every corner of the returned
        // rectangle must map back inside the source. This is the test that catches a
        // sign error in the constraint, which would otherwise show as blank corners
        // at one angle and an over-tight crop at its negative.
        for angle in [-15.0, -7.5, -1.0, 1.0, 7.5, 15.0f32] {
            for (sw, sh) in [(600, 400), (400, 600), (500, 500)] {
                let mut c = CompositionParams {
                    straighten: angle,
                    ..Default::default()
                };
                let f0 = Frame::resolve(dims(sw, sh), Orientation::Rotate0, &c);
                c.crop = f0.inscribed(sw as f32 / sh as f32);
                let f = Frame::resolve(dims(sw, sh), Orientation::Rotate0, &c);

                let r = f.crop;
                for (x, y) in [
                    (r.x as f32, r.y as f32),
                    ((r.x + r.w as i32) as f32, r.y as f32),
                    ((r.x + r.w as i32) as f32, (r.y + r.h as i32) as f32),
                    (r.x as f32, (r.y + r.h as i32) as f32),
                ] {
                    let (u, v) = f.to_source(x, y);
                    // A pixel of slack: the crop is rounded to whole frame pixels
                    // and the constraint is satisfied in continuous coordinates.
                    assert!(
                        u >= -1.0 && v >= -1.0 && u <= sw as f32 + 1.0 && v <= sh as f32 + 1.0,
                        "{angle}° on {sw}x{sh}: corner ({x}, {y}) -> ({u:.2}, {v:.2}) is outside"
                    );
                }
            }
        }
    }

    #[test]
    fn the_inscribed_crop_keeps_the_ratio_it_was_asked_for() {
        let c = CompositionParams {
            straighten: 6.0,
            ..Default::default()
        };
        let f = Frame::resolve(dims(6000, 4000), Orientation::Rotate0, &c);
        for want in [1.0, 1.5, 4.0 / 3.0, 0.8] {
            // Measured on the PLACED rectangle, in the pixels that get allocated.
            // Multiplying the fractions by the *frame* is the mistake this whole
            // milestone turned on: the rect is normalised against `oriented`, and
            // the frame is a different, angle-dependent size.
            let c = f.place(f.inscribed(want));
            let got = c.w as f32 / c.h as f32;
            assert!((got - want).abs() < 2.0e-3, "asked {want}, got {got}");
        }
    }

    #[test]
    fn an_unstraightened_frame_inscribes_itself() {
        // At zero degrees the auto-crop must be the whole frame, or straightening to
        // 0 would leave the picture cropped for no reason.
        let f = frame_of(dims(6000, 4000), Orientation::Rotate0);
        let r = f.inscribed(1.5);
        assert!(
            (r.w - 1.0).abs() < 1.0e-4 && (r.h - 1.0).abs() < 1.0e-4,
            "{r:?}"
        );
    }

    #[test]
    fn as_shot_is_not_a_modification_and_a_turn_is() {
        // Why `orientation` is an Option. Every portrait frame in the corpus would
        // otherwise open with a ruby dot on a module nobody had touched.
        let c = CompositionParams::default();
        assert!(c.is_default() && !c.is_modified());
        assert_eq!(
            c.orientation(Orientation::Rotate270),
            Orientation::Rotate270,
            "as shot"
        );

        let turned = CompositionParams {
            orientation: Some(Orientation::Rotate0),
            ..c
        };
        assert!(turned.is_modified(), "an override must light the dot");
        assert_eq!(
            turned.orientation(Orientation::Rotate270),
            Orientation::Rotate0
        );
    }

    #[test]
    fn the_bypass_keeps_the_orientation_and_drops_the_rest() {
        // The one departure from "a bypassed module renders as its default", and the
        // reason: orientation is a fact about the file, not a decision about the
        // picture. A switch that laid a portrait frame on its side reads as a bug.
        let c = CompositionParams {
            enabled: false,
            orientation: Some(Orientation::Rotate90),
            straighten: 3.0,
            keystone: KeystoneParams {
                mode: KeystoneMode::Vertical,
                guides: [
                    Point::new(0.25, 0.2),
                    Point::new(0.75, 0.2),
                    Point::new(0.8, 0.8),
                    Point::new(0.2, 0.8),
                ],
                ..Default::default()
            },
            crop: Rect {
                x: 0.1,
                y: 0.1,
                w: 0.5,
                h: 0.5,
            },
            ratio: Ratio::Fixed(1.0),
            portrait: true,
        };
        let b = c.bypassed();
        assert_eq!(b.orientation, Some(Orientation::Rotate90));
        assert_eq!(b.straighten, 0.0);
        assert!(b.crop.is_full());
        // And the stored edit survives being bypassed, like every other module.
        assert_eq!(c.straighten, 3.0);
    }

    #[test]
    fn portrait_is_a_flag_and_only_a_fixed_ratio_has_another_way_up() {
        // The prototype's shape: one combo of landscape ratios beside a `↕` toggle.
        // Listing every entry twice would double a twenty-one item list and make
        // "the same crop the other way up" a different selection.
        assert_eq!(Ratio::Fixed(1.5).of(dims(600, 400), false), Some(1.5));
        let portrait = Ratio::Fixed(1.5).of(dims(600, 400), true).expect("flips");
        assert!((portrait - 1.0 / 1.5).abs() < 1.0e-6, "{portrait}");

        // Everything with a shape stands on its end; only `Freehand` has none.
        assert!(Ratio::Fixed(1.5).can_flip() && Ratio::Original.can_flip());
        assert!(!Ratio::Free.can_flip());

        // **`Original` flips too.** A landscape frame cropped to a portrait of the
        // same proportions is an ordinary thing to want, and the one shape the list
        // could not otherwise express.
        assert_eq!(Ratio::Original.of(dims(600, 400), false), Some(1.5));
        let up = Ratio::Original.of(dims(600, 400), true).expect("flips");
        assert!((up - 1.0 / 1.5).abs() < 1.0e-6, "{up}");
        assert_eq!(Ratio::Free.of(dims(600, 400), false), None);
        // Zero, negative and NaN are not ratios; freeform rather than dividing.
        assert_eq!(Ratio::Fixed(0.0).of(dims(600, 400), false), None);
        assert_eq!(Ratio::Fixed(f32::NAN).of(dims(600, 400), false), None);
    }

    #[test]
    fn only_an_absolute_ratio_flips_when_the_picture_turns() {
        // The distinction `can_flip` does not draw, and it is easy to miss because
        // both questions sound like "can this be portrait".
        //
        // A `Fixed` ratio is absolute: turn the picture and a 3:2 crop is standing
        // on its end, so the flag has to follow. `Original` is defined *against the
        // frame*, which transposed at the same instant — its base already flipped,
        // and flipping the flag as well would turn it straight back.
        assert!(Ratio::Fixed(1.5).flips_with_the_frame());
        assert!(!Ratio::Original.flips_with_the_frame());
        assert!(!Ratio::Free.flips_with_the_frame());

        // Measured rather than asserted: `Original` over a landscape frame and over
        // the same frame turned must describe the same *physical* crop, with the
        // flag untouched.
        let landscape = Ratio::Original.of(dims(600, 400), false).expect("a shape");
        let turned = Ratio::Original.of(dims(400, 600), false).expect("a shape");
        assert!(
            (landscape * turned - 1.0).abs() < 1.0e-6,
            "{landscape} vs {turned}"
        );
    }

    #[test]
    fn the_preset_list_is_the_prototypes() {
        // the maintainer asked for the prototype's list specifically, and the entries that
        // look untidy are the ones that matter: `11×14` and `Ōban` are paper he
        // prints on. A list tidied into round numbers would be missing them.
        let names: Vec<&str> = Ratio::PRESETS.iter().map(|(n, _)| *n).collect();
        for want in [
            "Freehand",
            "Original",
            "Square  1:1",
            "5:4  —  4×5, 8×10",
            "11×14",
            "3:2  —  4×6, 35mm",
            "XPan  65:24",
            "A4",
            "Kiku",
            "Ōban",
        ] {
            assert!(names.contains(&want), "{want} is missing from the list");
        }
        // Three group headings, carried as `None` and drawn unselectable.
        let heads: Vec<&str> = Ratio::PRESETS
            .iter()
            .filter(|(_, r)| r.is_none())
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(heads, ["Photographic", "Paper", "Japanese print"]);
        // Every selectable entry has a distinct value, or two rows of the combo
        // would light up together.
        let vals: Vec<Ratio> = Ratio::PRESETS.iter().filter_map(|(_, r)| *r).collect();
        for (i, a) in vals.iter().enumerate() {
            for b in &vals[i + 1..] {
                assert!(!a.same(*b), "{a:?} and {b:?} are the same entry");
            }
        }
    }

    #[test]
    fn a_ratio_recovers_its_label_after_a_round_trip_through_a_decimal() {
        // The sidecar stores the value, not an index — the prototype stores the
        // index, which is what makes its list unreorderable. So the label has to be
        // recoverable from a number that has been through `%g` and back.
        for (name, r) in Ratio::PRESETS.iter().filter(|(_, r)| r.is_some()) {
            let r = r.expect("filtered");
            let round_tripped: Ratio = match r {
                Ratio::Fixed(v) => Ratio::Fixed(format!("{v}").parse().expect("parses")),
                other => other,
            };
            assert_eq!(&round_tripped.label(), name, "{r:?} lost its name");
        }
    }

    #[test]
    fn composing_projective_maps_applies_them_in_the_order_written() {
        // `then` reads left to right, which is the opposite of matrix notation and
        // the same as the pipeline it describes. Getting it backwards produces a
        // transform that is plausible and wrong — the expensive kind.
        let t = Projective::translate(10.0, 0.0).then(Projective::translate(0.0, 5.0));
        assert_eq!(t.apply(0.0, 0.0), (10.0, 5.0));
        // Translate then rotate is not rotate then translate.
        let a = Projective::translate(10.0, 0.0).then(Projective::unrotate(90.0));
        let b = Projective::unrotate(90.0).then(Projective::translate(10.0, 0.0));
        assert_ne!(a.apply(0.0, 0.0).0.round(), b.apply(0.0, 0.0).0.round());
    }

    #[test]
    fn rectangle_guides_land_on_the_neutral_rectangle_after_full_correction() {
        let guides = [
            Point::new(0.27, 0.18),
            Point::new(0.74, 0.24),
            Point::new(0.82, 0.79),
            Point::new(0.18, 0.84),
        ];
        let c = CompositionParams {
            keystone: KeystoneParams {
                mode: KeystoneMode::Rectangle,
                guides,
                correction: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let f = Frame::resolve(dims(1000, 800), Orientation::Rotate0, &c);
        assert!(f.resamples());
        for (guide, target) in guides.into_iter().zip(KeystoneParams::TARGETS) {
            let (x, y) = f.guide_to_frame(guide);
            assert!((x / 1000.0 - target.x).abs() < 1.0e-3, "x {x}");
            assert!((y / 800.0 - target.y).abs() < 1.0e-3, "y {y}");
        }
    }

    #[test]
    fn keystone_crop_contains_only_real_source_pixels() {
        let c = CompositionParams {
            keystone: KeystoneParams {
                mode: KeystoneMode::Vertical,
                guides: [
                    Point::new(0.29, 0.2),
                    Point::new(0.71, 0.2),
                    Point::new(0.82, 0.8),
                    Point::new(0.18, 0.8),
                ],
                correction: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let f = Frame::resolve(dims(1200, 800), Orientation::Rotate0, &c);
        let crop = f.keystone_crop(KeystoneCrop::Largest);
        assert!(
            f.covers(crop),
            "auto crop left the valid image footprint: {crop:?}"
        );
    }

    #[test]
    fn crossed_guides_are_rejected_instead_of_making_a_singular_frame() {
        let c = CompositionParams {
            keystone: KeystoneParams {
                mode: KeystoneMode::Rectangle,
                guides: [
                    Point::new(0.8, 0.2),
                    Point::new(0.2, 0.2),
                    Point::new(0.8, 0.8),
                    Point::new(0.2, 0.8),
                ],
                correction: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let f = Frame::resolve(dims(1200, 800), Orientation::Rotate0, &c);
        assert!(
            !f.resamples(),
            "a crossed quadrilateral reached the renderer"
        );
        assert_eq!(f.inverse(), Projective::IDENTITY.0);
    }

    #[test]
    fn zero_correction_is_a_literal_no_op() {
        let c = CompositionParams {
            keystone: KeystoneParams {
                mode: KeystoneMode::Vertical,
                guides: [
                    Point::new(0.3, 0.2),
                    Point::new(0.7, 0.2),
                    Point::new(0.8, 0.8),
                    Point::new(0.2, 0.8),
                ],
                correction: 0.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let f = Frame::resolve(dims(1200, 800), Orientation::Rotate0, &c);
        assert!(!f.resamples());
        assert_eq!(f.inverse(), Projective::IDENTITY.0);
    }

    #[test]
    fn a_guide_handle_cannot_cross_the_quadrilateral() {
        let mut k = KeystoneParams {
            mode: KeystoneMode::Rectangle,
            ..Default::default()
        };
        let before = k.guides;
        k.move_guide(0, Point::new(0.9, 0.9));
        assert_eq!(k.guides, before, "an invalid handle movement was accepted");
        k.move_guide(0, Point::new(0.25, 0.25));
        assert_eq!(k.guides[0], Point::new(0.25, 0.25));
    }

    #[test]
    fn two_drawn_vertical_lines_are_ordered_and_measured_at_the_target_heights() {
        let mut k = KeystoneParams::default();
        // Right line first and bottom-to-top; the gesture order must not matter.
        assert!(k.set_drawn_lines(
            KeystoneMode::Vertical,
            [Point::new(0.8, 0.9), Point::new(0.7, 0.1)],
            [Point::new(0.2, 0.9), Point::new(0.3, 0.1)],
        ));
        let selected = k.selected_guides();
        assert!((selected[0].x - 0.2875).abs() < 1.0e-5);
        assert!((selected[3].x - 0.2125).abs() < 1.0e-5);
        assert!((selected[1].x - 0.7125).abs() < 1.0e-5);
        assert!((selected[2].x - 0.7875).abs() < 1.0e-5);
    }

    #[test]
    fn two_drawn_horizontal_lines_are_ordered_and_measured_at_the_target_widths() {
        let mut k = KeystoneParams::default();
        // Bottom line first and right-to-left; neither gesture direction is state.
        assert!(k.set_drawn_lines(
            KeystoneMode::Horizontal,
            [Point::new(0.9, 0.8), Point::new(0.1, 0.7)],
            [Point::new(0.9, 0.2), Point::new(0.1, 0.3)],
        ));
        let selected = k.selected_guides();
        assert!((selected[0].y - 0.2875).abs() < 1.0e-5);
        assert!((selected[1].y - 0.2125).abs() < 1.0e-5);
        assert!((selected[3].y - 0.7125).abs() < 1.0e-5);
        assert!((selected[2].y - 0.7875).abs() < 1.0e-5);
    }

    #[test]
    fn a_mismatched_second_line_does_not_complete_the_pair() {
        let mut k = KeystoneParams::default();
        assert!(!k.set_drawn_lines(
            KeystoneMode::Vertical,
            [Point::new(0.2, 0.1), Point::new(0.3, 0.9)],
            [Point::new(0.1, 0.6), Point::new(0.9, 0.6)],
        ));
        assert_eq!(k, KeystoneParams::default());
    }

    #[test]
    fn perspective_auto_crop_defaults_to_the_original_shape() {
        assert_eq!(KeystoneParams::default().crop, KeystoneCrop::Original);
    }
}
