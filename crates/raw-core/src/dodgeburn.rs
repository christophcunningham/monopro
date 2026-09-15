//! Dodge & Burn: local exposure as a list of parametric records.
//!
//! # Nothing here is rasterised
//!
//! A dab is a compact nine-value record. The sidecar packs those records, retained
//! edit states share each pass's list, and the GPU rasterises the same list through
//! a storage buffer. The unit of the edit is still not the dab. It is a pass.
//!
//! # The gesture model, which is the darkroom pass
//!
//! ```text
//!   dabs within one gesture   composite by MAX of magnitude
//!   gestures within one instance   SUM
//!   instances   SUM
//! ```
//!
//! One press-to-release drag is one pass of the wand under the enlarger. Sweeping
//! back and forth over the same spot during that pass deposits the pass intensity
//! **once**, however slowly the hand moves — which is why the dabs take a maximum
//! rather than accumulating. Passes then add. So `Intensity` reads literally as
//! *EV per pass*, a single click deposits exactly that, and drag speed does not
//! change the result. Every one of those properties falls out of max-then-sum and
//! none of them survives a flat sum, which is the model the handoff's six floats
//! would have suggested on their own.
//!
//! An eraser stroke (`⌥`-drag) is a gesture carrying the **opposite** sign to its
//! instance, so it subtracts passes locally. The instance total is then clamped to
//! its own half of the number line: erasing a burn can take it back to zero and no
//! further, never through zero into a dodge.
//!
//! # Why the total is clamped and not saturated
//!
//! The handoff says `tanh` saturation. The prototype used to and stopped, and the
//! reason is in its source: a full-strength +1 EV must produce *exactly* +1 EV —
//! twice the scene luminance, the same unit the global Exposure slider speaks —
//! and `tanh(1/4)·4 ≈ 0.98` is not that. So [`EV_MAX`] is a safety limit reached by
//! stacking, not a curve applied to everything. The literal reading of the number
//! survives the whole normal operating range.
//!
//! # Feed-forward masks
//!
//! A [`ZoneMask`] is evaluated on the luminance **entering** this stage, never on
//! its output. A burn therefore cannot shift the mask that is deciding where the
//! burn lands, which is the difference between a tool and a feedback loop. The
//! graph gives this for free: the node's own input is the pre-D&B signal.

use std::sync::Arc;

/// Where the total is clipped, in stops.
///
/// Matched to the global Exposure slider's range, so heavy stacking on deep
/// shadows cannot hit a ceiling the rest of the app does not have.
pub const EV_MAX: f32 = 4.0;

/// Middle grey. The anchor every zone in [`ZoneMask`] is measured from, and the
/// same 0.18 the rest of the chain uses.
pub const MID_GREY: f32 = 0.18;

/// What shape a dab presses.
///
/// **A circle is the least darkroom-honest brush there is.** Real dodging tools are
/// hands, wire wands, and cards with holes cut in them — mostly elongated, and mostly
/// held at an angle to the thing they are shading. The round brush that shipped in
/// Dodge & Burn shipped with the Photoshop default, not the physical one.
///
/// The card is the one to reach for: burning a corner or an edge through a straight
/// edge is *the* darkroom gesture, and a round brush cannot do it without a visibly
/// scalloped edge.
///
/// **Per dab, not per layer**, at the level `radius` and `feather` already sit at —
/// so a stroke keeps the shape the brush had when it was made even after the brush
/// changes. One nib mid-session does not rewrite what you painted before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Nib {
    /// A disc, or an ellipse once `aspect` is off 1.0.
    #[default]
    Round,
    /// A rectangle: a card with a straight edge, which is what you burn a corner
    /// through.
    Card,
}

impl Nib {
    pub const ALL: [Self; 2] = [Self::Round, Self::Card];

    pub fn label(self) -> &'static str {
        match self {
            Self::Round => "Round",
            Self::Card => "Card",
        }
    }

    /// The stored form. **Lower case and separate from [`label`](Self::label)**, which
    /// is the rule the module header states: a persisted value that follows the UI
    /// wording stops matching what the last release wrote.
    pub fn key(self) -> &'static str {
        match self {
            Self::Round => "round",
            Self::Card => "card",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "round" => Some(Self::Round),
            "card" => Some(Self::Card),
            _ => None,
        }
    }
}

/// One dab: a brush contact, and the whole storage primitive.
///
/// Nine numbers, laid out in the order the GPU buffer packs them.
///
/// # Coordinates
///
/// `x` is normalised to the frame **width**, `y` to the frame **height**, and
/// `radius` to the width alone. That mixture is deliberate and is the thing to be
/// careful about: it makes the record independent of resolution while keeping the
/// brush a **circle in pixels** rather than an ellipse that changes shape with the
/// frame's aspect. The correction is applied to `dy` at evaluation time — see
/// [`Dab::magnitude_at`] — and getting it backwards produces a brush that looks
/// right on a square test image and wrong on every real one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dab {
    pub x: f32,
    pub y: f32,
    /// Fraction of the frame width.
    pub radius: f32,
    /// 0 is a hard disc, 1 is fully soft. Below [`Dab::HARD`] the Gaussian is
    /// skipped entirely rather than approached asymptotically.
    pub feather: f32,
    pub opacity: f32,
    /// Signed: positive dodges, negative burns. The sign is what makes an eraser
    /// gesture expressible in the same record as an ordinary one.
    pub ev: f32,
    /// The nib's own height/width ratio. **1.0 is round**, whatever the negative's
    /// aspect — the frame correction and the brush's own shape are two different
    /// things and are kept apart; see [`Dab::magnitude_at`].
    pub aspect: f32,
    /// Rotation, in degrees, **in source space** — so a straighten or a quarter turn
    /// carries the nib round with the picture, which falls out of dabs being welded
    /// to the negative and needs no correction of its own.
    ///
    /// Does nothing at `aspect == 1.0` on a round nib: a circle has no orientation.
    /// It does something on a card at any aspect.
    pub angle: f32,
    pub nib: Nib,
}

impl Dab {
    /// A round nib at rest: aspect 1, no rotation, and nothing else set.
    ///
    /// The tail for a struct-update literal — `Dab { x, y, radius, .., ..Dab::ROUND }`
    /// — so the common case reads as six numbers even though a dab now carries nine.
    /// Not `Default`, because a dab of radius zero is not a sensible default, it is
    /// the absence of one; this is explicitly *the round part* of a dab.
    pub const ROUND: Self = Self {
        x: 0.0,
        y: 0.0,
        radius: 0.0,
        feather: 0.0,
        opacity: 1.0,
        ev: 0.0,
        aspect: 1.0,
        angle: 0.0,
        nib: Nib::Round,
    };

    /// Below this feather the profile is a hard disc.
    ///
    /// Not a rounding convenience: the Gaussian's sigma is `radius /
    /// sqrt(-2·ln(1-feather))`, which goes to zero as feather does, so the
    /// smallest feathers would otherwise cost a division that tends to infinity
    /// to describe a shape the disc already describes exactly.
    pub const HARD: f32 = 0.01;

    /// Normalised brush profile at `distance`, before opacity and EV.
    ///
    /// Kept in one function because the cursor's feather guide must describe this
    /// exact falloff rather than inventing a second interpretation of Feather.
    fn profile(distance: f32, feather: f32) -> f32 {
        if distance >= 1.0 {
            return 0.0;
        }
        if feather < Self::HARD {
            return 1.0;
        }
        let f = feather.clamp(Self::HARD, 0.999);
        let sigma = 1.0 / (-2.0 * (1.0 - f).ln()).sqrt();
        let t = (1.0 - distance).clamp(0.0, 1.0);
        (-0.5 * (distance / sigma) * (distance / sigma)).exp() * t * t * (3.0 - 2.0 * t)
    }

    /// Radius of the actual 50% strength contour, for the brush cursor.
    ///
    /// Sigma is not that contour: at the default Feather 0.40 sigma is almost the
    /// outer radius, so the old guide looked indistinguishable from a hard brush
    /// even though the painted dab was already below half strength near mid-radius.
    /// A tiny binary solve is app chrome only and keeps the preview pinned to the
    /// same profile the CPU and GPU render.
    pub fn half_strength_radius(feather: f32) -> Option<f32> {
        if feather < Self::HARD {
            return None;
        }
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..16 {
            let mid = (lo + hi) * 0.5;
            if Self::profile(mid, feather) > 0.5 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Some((lo + hi) * 0.5)
    }

    /// **Two aspects are in play here and they are not the same thing.**
    ///
    /// `frame` is the negative's height over its width, and it exists to undo the
    /// mixed normalisation — `x` is a fraction of the width, `y` of the height — so
    /// that a nib at `self.aspect == 1.0` is round *in pixels*. `self.aspect` is the
    /// nib's own ratio, and it exists to make it deliberately not round.
    ///
    /// The order matters: into width units first, then rotate, then scale by the
    /// nib's ratio. Rotating in normalised space shears the shape, which is the
    /// mistake the prototype's radial gradient makes and which `Radial::weight_at`
    /// records.
    ///
    /// Returns the **normalised distance** in the nib's own space: 1.0 is the
    /// boundary, whatever the shape.
    fn distance(&self, x: f32, y: f32, frame: f32) -> f32 {
        let dx = x - self.x;
        // Into the same units as `dx`: a fraction of the frame WIDTH. `y` is
        // normalised to the height, so a step of one across `y` is `frame` widths,
        // not one.
        let dy = (y - self.y) * frame;
        let (sin, cos) = self.angle.to_radians().sin_cos();
        let lx = dx * cos + dy * sin;
        let ly = -dx * sin + dy * cos;
        let r = self.radius.max(1e-6);
        let nx = lx / r;
        let ny = ly / (r * self.aspect.max(0.01));
        match self.nib {
            Nib::Round => (nx * nx + ny * ny).sqrt(),
            // Chebyshev, which is what makes a rectangle: the boundary is where
            // *either* axis reaches one, rather than where they reach one together.
            _ => nx.abs().max(ny.abs()),
        }
    }

    /// This dab's unsigned contribution at a point, in stops.
    ///
    /// Compactly supported: exactly zero at and beyond the boundary, because the
    /// smoothstep window closes there. That is what lets a rasteriser — CPU or GPU —
    /// reject a dab on a bounding test and lose nothing, and the cull in
    /// `dodge_burn.wgsl` depends on it.
    ///
    /// The feather is expressed in that same normalised space, so it goes on meaning
    /// *this fraction of the brush* whatever shape or aspect the nib has. That is
    /// what makes 10c an addition rather than a migration of everyone's strokes.
    pub fn magnitude_at(&self, x: f32, y: f32, frame: f32) -> f32 {
        let d = self.distance(x, y, frame);
        Self::profile(d, self.feather) * self.opacity * self.ev.abs()
    }

    /// This dab's coverage independent of its EV intensity.
    ///
    /// Local contrast uses the same painted shape as exposure, but Intensity must
    /// not secretly become a second Contrast amount. Opacity and feather still
    /// belong to the mark and therefore remain part of coverage.
    pub fn coverage_at(&self, x: f32, y: f32, frame: f32) -> f32 {
        Self::profile(self.distance(x, y, frame), self.feather) * self.opacity
    }

    /// The radius of the circle that encloses this dab, in width units.
    ///
    /// What the GPU cull tests against. Conservative — nothing that could contribute
    /// is dropped — and less effective the more elongated the nib, which is the
    /// honest trade and is not worth anything cleverer until somebody paints with a
    /// 10:1 card and complains.
    pub fn bound(&self) -> f32 {
        let a = self.aspect.max(0.01);
        match self.nib {
            // The long axis of the ellipse.
            Nib::Round => self.radius * a.max(1.0),
            // Half the rectangle's diagonal.
            Nib::Card => self.radius * (1.0 + a * a).sqrt(),
        }
    }
}

/// One press-to-release pass.
///
/// Its dabs share a sign, because the mode cannot change during a drag. The sign
/// is **derived** rather than stored, so there is no second copy of the fact to
/// disagree with the dabs.
#[derive(Debug, Clone, Default)]
pub struct Gesture {
    /// Shared between retained edit states; painting copies only the pass being
    /// extended.
    pub dabs: Arc<Vec<Dab>>,
}

impl Gesture {
    pub fn new(dabs: Vec<Dab>) -> Self {
        Self {
            dabs: Arc::new(dabs),
        }
    }

    pub fn dabs_mut(&mut self) -> &mut Vec<Dab> {
        Arc::make_mut(&mut self.dabs)
    }

    /// `+1` or `-1`. An empty gesture reads positive and contributes nothing
    /// either way, since its maximum is zero.
    pub fn sign(&self) -> f32 {
        match self.dabs.first() {
            Some(d) if d.ev < 0.0 => -1.0,
            _ => 1.0,
        }
    }

    /// This pass's deposit at a point: the maximum over its dabs, unsigned.
    pub fn magnitude_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        self.dabs
            .iter()
            .fold(0.0f32, |m, d| m.max(d.magnitude_at(x, y, aspect)))
    }

    /// This pass's painted coverage, independent of EV intensity.
    pub fn coverage_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        self.dabs
            .iter()
            .fold(0.0f32, |m, d| m.max(d.coverage_at(x, y, aspect)))
    }
}

impl PartialEq for Gesture {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.dabs, &other.dabs) || self.dabs.as_slice() == other.dabs.as_slice()
    }
}

/// What an instance is: a dodge or a burn. Not a per-dab property.
///
/// An instance's sign is fixed at creation and is what the eraser clamp is
/// measured against. It is an enum rather than a signed float because there are
/// exactly two of them and "an instance whose sign is 0.0" is not a state worth
/// being able to represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sign {
    Dodge,
    #[default]
    Burn,
}

impl Sign {
    pub fn ev(self) -> f32 {
        match self {
            Self::Dodge => 1.0,
            Self::Burn => -1.0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dodge => "Dodge",
            Self::Burn => "Burn",
        }
    }

    /// Clamp a total to this sign's half of the number line.
    ///
    /// The eraser rule: subtracting passes can reach zero and stop. A burn that
    /// erased past zero would start dodging, which is not what taking something
    /// back means.
    pub fn clamp(self, ev: f32) -> f32 {
        match self {
            Self::Dodge => ev.max(0.0),
            Self::Burn => ev.min(0.0),
        }
    }
}

/// Tonal-range mask — the digital descendant of darkroom film masking.
///
/// A trapezoid in EV space against middle grey, so Zone V is 0 EV and Zone N is
/// EV N−5. Full strength between the bounds; the feathers extend **outward** from
/// each one, asymmetrically on purpose — a shadow mask wants a hard low edge and a
/// soft high edge. A bound sitting at the ruler limit is treated as *open*, so
/// "highlights only" has exactly one active shoulder rather than one real edge and
/// one that happens to be off-screen.
///
/// # `edge_aware` is the whole difference between a mask and a tool
///
/// Off, the trapezoid reads raw per-pixel EV and selects a scattered **pixel set** —
/// dark pixels inside a highlight travel with the shadows.
///
/// On, an edge-aware self-guided filter smooths the EV image first, so the trapezoid
/// reads **regional** exposure: "Zone III" becomes the shadowed wall rather than a
/// scatter of dark pixels, and lifting it carries the wall's texture while its edge
/// against the sky stays clean. A hand under the enlarger is a spatially coherent
/// region, not a density-selective filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneMask {
    pub enabled: bool,
    /// Lower bound, EV against middle grey.
    pub lo: f32,
    pub hi: f32,
    /// Shoulder widths, in EV, extending outward from `lo` and `hi`.
    pub f_lo: f32,
    pub f_hi: f32,
    pub invert: bool,
    /// Diffusion spacer, as a fraction of the frame width.
    ///
    /// Masks were deliberately printed unsharp — it is the origin of the term
    /// "unsharp mask" — and without it a pixel-level mask inherits the granularity
    /// of whatever it was computed from.
    pub blur: f32,
    pub edge_aware: bool,
    /// Guidance window, as a fraction of the frame width: *how big is the hand*.
    pub region: f32,
    /// How strong an edge survives the smoothing, **in EV**.
    ///
    /// The filter's regularisation term is in the squared units of its input, so
    /// this is squared at the point of use. Stored as the EV threshold because
    /// that is the number a person can reason about and the number the slider
    /// shows — the same argument `ContrastMaskParams::spacer` makes for keeping a
    /// percentage rather than the pixels it becomes.
    pub edge: f32,
}

impl ZoneMask {
    /// Zone 0 and Zone X: the ruler's ends, and the values that mean *open*.
    pub const MIN_EV: f32 = -5.0;
    pub const MAX_EV: f32 = 5.0;
    pub const BLUR_RANGE: std::ops::RangeInclusive<f32> = 0.0..=0.100;
    pub const REGION_RANGE: std::ops::RangeInclusive<f32> = 0.005..=0.150;
    pub const EDGE_RANGE: std::ops::RangeInclusive<f32> = 0.1..=2.0;

    /// Whether this mask has no effect at all, and the whole evaluation can be
    /// skipped rather than computed and multiplied by one.
    pub fn is_identity(&self) -> bool {
        !self.enabled
            || (self.lo <= Self::MIN_EV + 1e-6 && self.hi >= Self::MAX_EV - 1e-6 && !self.invert)
    }

    /// The mask at one point, given the EV there.
    ///
    /// `ev` is `log2(luminance / 0.18)` — already guided-filtered when
    /// `edge_aware` is on, because that smoothing is a whole-image operation and
    /// cannot be done per point. Which EV image arrives is the caller's business;
    /// what the trapezoid does with it is this function's.
    pub fn evaluate(&self, ev: f32) -> f32 {
        let ss = |t: f32| {
            let t = t.clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };

        let mut m = if self.lo <= Self::MIN_EV + 1e-6 {
            1.0
        } else if self.f_lo > 1e-6 {
            ss((ev - (self.lo - self.f_lo)) / self.f_lo)
        } else {
            f32::from(ev >= self.lo)
        };

        if self.hi < Self::MAX_EV - 1e-6 {
            m *= if self.f_hi > 1e-6 {
                1.0 - ss((ev - self.hi) / self.f_hi)
            } else {
                f32::from(ev <= self.hi)
            };
        }

        if self.invert { 1.0 - m } else { m }
    }

    /// The guided filter's regularisation term, in the squared units of an EV
    /// image. See [`ZoneMask::edge`].
    pub fn eps(&self) -> f32 {
        (self.edge * self.edge).max(1e-6)
    }

    /// Shadows, midtones, highlights — the three chips over the ruler.
    ///
    /// Transcribed from the prototype, feathers included: the shadow preset's
    /// upper shoulder is wider than its lower one, which is the asymmetry the
    /// trapezoid exists to allow.
    pub const PRESETS: [(&'static str, f32, f32, f32, f32); 3] = [
        ("Shadows", -5.0, -2.0, 1.0, 1.5),
        ("Midtones", -2.0, 2.0, 1.0, 1.0),
        ("Highlights", 2.0, 5.0, 1.5, 1.0),
    ];
}

impl Default for ZoneMask {
    fn default() -> Self {
        // Open at both ends, so a newly created mask is the identity and switching
        // it on changes nothing until a bound is moved. `edge_aware` defaults on:
        // it is the mode that behaves like the tool this is a model of, and the
        // legacy per-pixel read is the fallback rather than the baseline.
        Self {
            enabled: false,
            lo: Self::MIN_EV,
            hi: Self::MAX_EV,
            f_lo: 1.0,
            f_hi: 1.0,
            invert: false,
            blur: 0.0,
            edge_aware: true,
            region: 0.040,
            edge: 0.50,
        }
    }
}

/// A linear gradient: full strength on one side of a band, nothing on the other.
///
/// Drag convention, and it is Photoshop's: **the press is full strength and the
/// release is zero.** The band is unbounded perpendicular to the drag — the whole
/// frame on the start side is affected and the whole frame on the end side is not —
/// so what the drag sets is the *transition*, not the extent.
///
/// All coordinates are source-normalised like [`Dab`]'s, and the projection is
/// **aspect-corrected**, which is a deliberate divergence from the prototype. Its
/// `_linear_gradient_weight` projects in raw normalised space, so on a 3:2 frame the
/// iso-lines of a diagonal gradient are not perpendicular to the drag that made it.
/// That is the same defect an un-corrected brush would have, and this codebase
/// already decided the other way for the brush.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Linear {
    /// Full strength.
    pub x0: f32,
    pub y0: f32,
    /// Zero.
    pub x1: f32,
    pub y1: f32,
    /// How much of the drag is transition. 0 is a hard step at the midpoint; 1
    /// blends across the whole drag, which is what the Photoshop convention means
    /// and therefore the default.
    pub feather: f32,
    /// Signed, like a dab's.
    pub ev: f32,
}

impl Linear {
    pub const FEATHER_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;

    /// Strength at a point, in `[0, 1]`.
    pub fn weight_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        let dx = self.x1 - self.x0;
        let dy = (self.y1 - self.y0) * aspect;
        let len2 = dx * dx + dy * dy;
        // A drag with no length has no direction to be a gradient along. Full
        // strength everywhere is the honest reading of "the transition is nowhere",
        // and it is what the user sees while the mouse is still on the press.
        if len2 < 1e-10 {
            return 1.0;
        }
        let t = ((x - self.x0) * dx + ((y - self.y0) * aspect) * dy) / len2;
        // The feather window is centred on the midpoint, so narrowing it tightens
        // the transition where it is rather than sliding it towards an end.
        let f = self.feather.clamp(0.001, 1.0);
        let (lo, hi) = (0.5 - f * 0.5, 0.5 + f * 0.5);
        let s = ((t - lo) / (hi - lo)).clamp(0.0, 1.0);
        1.0 - s * s * (3.0 - 2.0 * s)
    }
}

/// A radial gradient: a spotlight, or — inverted — a vignette.
///
/// Drag convention: the press is the centre, the release is a point on the outer
/// radius. `inner` is the full-strength boundary and `outer` the zero one, both as
/// fractions of the frame **width**, like [`Dab::radius`].
///
/// `aspect` is the ellipse's own height/width ratio and **1.0 is a circle**, which
/// is the second deliberate divergence from the prototype: its
/// `_radial_gradient_weight` normalises both axes by `outer_r` in a space where x is
/// a fraction of width and y a fraction of height, so its "circle" is as elliptical
/// as the frame is. Its own comment records the fudge. Here the correction is the
/// brush's, so `aspect = 1.0` is round on any negative and the control means what it
/// says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radial {
    pub cx: f32,
    pub cy: f32,
    /// Full-strength boundary, as a fraction of the frame width.
    pub inner: f32,
    /// Zero boundary.
    pub outer: f32,
    /// Height / width of the ellipse. 1.0 is a circle.
    pub aspect: f32,
    /// Rotation, in degrees. Does nothing at `aspect == 1.0`.
    pub angle: f32,
    /// 0 is a linear ramp between the boundaries, 1 a full smoothstep.
    pub feather: f32,
    /// Turn the spotlight into a vignette: zero at the centre, full outside.
    pub invert: bool,
    pub ev: f32,
}

impl Radial {
    pub const INNER_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;
    pub const OUTER_RANGE: std::ops::RangeInclusive<f32> = 0.01..=2.0;
    pub const ASPECT_RANGE: std::ops::RangeInclusive<f32> = 0.2..=5.0;
    pub const ANGLE_RANGE: std::ops::RangeInclusive<f32> = -90.0..=90.0;
    pub const FEATHER_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;

    /// Strength at a point, in `[0, 1]`.
    pub fn weight_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        let a = self.angle.to_radians();
        let (sin, cos) = a.sin_cos();
        // Into width units first, so the rotation happens in a space where a
        // circle is round — rotating in normalised space would shear the ellipse.
        let qx = x - self.cx;
        let qy = (y - self.cy) * aspect;
        let lx = qx * cos + qy * sin;
        let ly = -qx * sin + qy * cos;

        let r = self.outer.max(1e-4);
        let nx = lx / r;
        let ny = ly / (r * self.aspect.max(0.01));
        let dist = (nx * nx + ny * ny).sqrt();

        let inner = (self.inner / r).clamp(0.0, 0.999);
        let s = ((dist - inner) / (1.0 - inner).max(1e-6)).clamp(0.0, 1.0);
        let f = self.feather.clamp(0.0, 1.0);
        // Blend linear into smoothstep rather than switching, so the feather slider
        // is continuous — a hard changeover at some threshold would make one step of
        // the control do what the other twenty do together.
        let ramp = (1.0 - f) * s + f * (s * s * (3.0 - 2.0 * s));
        let w = 1.0 - ramp;
        if self.invert { 1.0 - w } else { w }
    }
}

/// What an instance is made of. Exactly one of three, never a mixture.
///
/// The prototype carries an `instance_type` string beside a `gestures` list *and* a
/// `gradient` slot, so "a radial instance with brush strokes on it" is a state its
/// data can represent and its renderer ignores. An enum cannot.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// Brush passes, in the order they were painted, and **the one nib they were all
    /// painted with**.
    ///
    /// **The layer is the authority on the nib.** Every [`Dab`] carries one too — the
    /// GPU's packing and the sidecar's wire format, and what makes a stroke on disk keep
    /// its shape — but the two cannot drift, because dabs are only ever created from
    /// this. See `paint::Brush::dab_with`.
    ///
    /// Holding it here is what makes two states unrepresentable rather than merely
    /// avoided: **a layer with both nibs**, and **a layer with no nib at all** — a fresh
    /// Card layer whose row said `BRUSH` because the only nib lived in dabs that did not
    /// exist yet. A layer is a Card layer from the moment you make it.
    Brush {
        nib: Nib,
        passes: Vec<Gesture>,
    },
    Linear(Linear),
    Radial(Radial),
}

impl Shape {
    /// A **round** brush layer holding `passes`.
    ///
    /// The nib a caller means when it does not say: `Nib::Round` is the default nib and
    /// the one every layer had before there were two. Exists so that adding the nib to
    /// this variant did not put `nib: Nib::Round` into thirty test fixtures that are
    /// about something else — and so the few places that *do* choose a nib are visible
    /// as the ones spelling the variant out.
    pub fn brush(passes: Vec<Gesture>) -> Self {
        Self::Brush {
            nib: Nib::Round,
            passes,
        }
    }

    /// The **discriminant**, and it is a wire format.
    ///
    /// `sidecar::to_xml` writes this as `monopro:Shape`, on the rule that the kind is
    /// always present so a reader never infers it from which optional group happens to
    /// be there. **Changing any of these three words invalidates every sidecar already
    /// on disk** — which is why the panel's word comes from [`ui_label`](Self::ui_label)
    /// instead, and why the two are separate functions rather than one that grew a
    /// second caller.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Brush { .. } => "brush",
            Self::Linear(_) => "linear",
            Self::Radial(_) => "radial",
        }
    }

    /// What the layer row calls this: **the nib**, for a brush, rather than the word
    /// "brush".
    ///
    /// the maintainer's ask. With two nibs shipped, a list of layers all saying `BRUSH` stopped
    /// telling him anything — the one thing that distinguishes two brush layers is
    /// whether they were painted with the disc or the card, and that was the fact the
    /// row was hiding. The gradients keep their kind, because their kind *is* what
    /// tells them apart.
    ///
    /// **Total, with no fallback**, which is the point of putting the nib on the layer:
    /// there is no such thing as a brush layer without one, and none with two. The
    /// first version of this derived the answer from the dabs and had to say "brush"
    /// when they disagreed or when there were none — and the second of those was
    /// exactly the bug, since a layer has no dabs for as long as it takes to make it
    /// and look at it.
    pub fn ui_label(&self) -> &'static str {
        match self {
            Self::Brush { nib, .. } => nib.label(),
            other => other.label(),
        }
    }

    /// The nib this layer paints with. `None` for a gradient, which has no nib.
    pub fn nib(&self) -> Option<Nib> {
        match self {
            Self::Brush { nib, .. } => Some(*nib),
            _ => None,
        }
    }
}

/// One dodge or one burn: a named stack of passes, with its own mask and opacity.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub name: String,
    pub sign: Sign,
    pub shape: Shape,
    /// Scales the whole instance. Applied at composite time and **not** baked into
    /// anything cached, so moving this slider cannot invalidate a rasterisation.
    pub opacity: f32,
    /// Signed local-detail gain. `0` is unchanged, `1` doubles the detail residual,
    /// and `-1` removes it within this layer's painted coverage.
    pub contrast: f32,
    /// The eye toggle: excluded from the render without being deleted.
    pub enabled: bool,
    pub mask: ZoneMask,
}

impl Instance {
    /// An empty brush instance — what `⌘D` and `⌘X` make.
    pub fn new(sign: Sign, name: String) -> Self {
        Self::of(
            sign,
            name,
            Shape::Brush {
                nib: Nib::Round,
                passes: Vec::new(),
            },
        )
    }

    pub fn of(sign: Sign, name: String, shape: Shape) -> Self {
        Self {
            name,
            sign,
            shape,
            opacity: 1.0,
            contrast: 0.0,
            enabled: true,
            mask: ZoneMask::default(),
        }
    }

    /// The passes, or nothing at all for a gradient.
    pub fn gestures(&self) -> &[Gesture] {
        match &self.shape {
            Shape::Brush { passes, .. } => passes,
            _ => &[],
        }
    }

    /// `None` for a gradient, which has no passes to add one to. Callers that paint
    /// have to handle that rather than silently appending to a list nothing reads.
    pub fn gestures_mut(&mut self) -> Option<&mut Vec<Gesture>> {
        match &mut self.shape {
            Shape::Brush { passes, .. } => Some(passes),
            _ => None,
        }
    }

    pub fn dab_count(&self) -> usize {
        self.gestures().iter().map(|g| g.dabs.len()).sum()
    }

    /// What the panel row says this instance is made of.
    pub fn summary(&self) -> String {
        match &self.shape {
            Shape::Brush { passes, .. } => {
                let n = passes.len();
                format!("{n} pass{}", if n == 1 { "" } else { "es" })
            }
            Shape::Linear(_) => "linear".into(),
            Shape::Radial(r) if r.invert => "vignette".into(),
            Shape::Radial(_) => "radial".into(),
        }
    }

    /// Whether this instance can change a pixel. An instance with no dabs is a
    /// row in the list and nothing else; a gradient always renders once placed.
    pub fn is_active(&self) -> bool {
        self.enabled
            && self.opacity > 0.0
            && match &self.shape {
                Shape::Brush { passes, .. } => passes.iter().any(|g| !g.dabs.is_empty()),
                Shape::Linear(l) => l.ev != 0.0 || self.contrast != 0.0,
                Shape::Radial(r) => r.ev != 0.0 || self.contrast != 0.0,
            }
    }

    /// This instance's signed contribution at a point, before its mask and its
    /// master opacity.
    ///
    /// The sign clamp is applied to all three shapes and is a **no-op for the
    /// gradients**, which have no eraser passes and therefore nothing that could
    /// take them across zero. Uniform on purpose: the shader has one code path
    /// after the weight is computed, and an exception there is a place for the
    /// third shape to be forgotten.
    pub fn ev_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        let ev = match &self.shape {
            Shape::Brush { passes, .. } => passes
                .iter()
                .map(|g| g.sign() * g.magnitude_at(x, y, aspect))
                .sum(),
            Shape::Linear(l) => l.weight_at(x, y, aspect) * l.ev,
            Shape::Radial(r) => r.weight_at(x, y, aspect) * r.ev,
        };
        self.sign.clamp(ev)
    }

    /// The layer's painted coverage, including erasure but before Tone Mask and
    /// master opacity. Unlike EV, this is independent of Dodge/Burn direction and
    /// brush Intensity; it is the spatial weight for this layer's local contrast.
    pub fn coverage_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        match &self.shape {
            Shape::Brush { passes, .. } => passes
                .iter()
                .map(|g| g.sign() * self.sign.ev() * g.coverage_at(x, y, aspect))
                .sum::<f32>()
                .clamp(0.0, 1.0),
            Shape::Linear(l) => l.weight_at(x, y, aspect),
            Shape::Radial(r) => r.weight_at(x, y, aspect),
        }
    }

    /// The next unused default name of this kind — `Burn 1`, `Dodge 2`.
    pub fn auto_name(sign: Sign, existing: &[Instance]) -> String {
        let kind = sign.label();
        (1..)
            .map(|n| format!("{kind} {n}"))
            .find(|n| !existing.iter().any(|i| &i.name == n))
            .expect("the naturals are not exhausted by a finite instance list")
    }
}

/// The module.
#[derive(Debug, Clone, PartialEq)]
pub struct DodgeBurnParams {
    /// Module bypass, like every other module's.
    ///
    /// **On by default**, unlike Contrast Mask, and the difference is worth
    /// stating: an untouched D&B has no instances and so renders nothing anyway,
    /// so "on" costs nothing and "off" would mean the first stroke a user painted
    /// did not appear. The switch is here so a finished set of passes can be
    /// compared against no passes at all without deleting them.
    pub enabled: bool,
    pub instances: Vec<Instance>,
}

impl Default for DodgeBurnParams {
    fn default() -> Self {
        Self {
            enabled: true,
            instances: Vec::new(),
        }
    }
}

impl DodgeBurnParams {
    /// How many instances one image may carry.
    ///
    /// A cap because the GPU binds one mask layer per instance and the shader
    /// loops them; eight is the tab cap's number and for the same reason — past it
    /// you are managing a list rather than making a picture.
    pub const MAX_INSTANCES: usize = 8;

    pub const CONTRAST_RANGE: std::ops::RangeInclusive<f32> = -1.0..=1.0;
    /// Shared local-detail scale as a fraction of the source diagonal. One shared
    /// decomposition keeps eight layers no more expensive than one.
    pub const CONTRAST_SCALE: f32 = 0.004;

    /// Instances that will actually be rasterised, in stacking order.
    pub fn active(&self) -> impl Iterator<Item = &Instance> {
        self.instances.iter().filter(|i| i.is_active())
    }

    /// Where instance `i` sits in the **rasterised** list, which is not where it
    /// sits in this one — inactive instances are packed out.
    ///
    /// The panel indexes the full list and the GPU indexes the compacted one, so
    /// anything that names an instance across that boundary has to go through here.
    /// `None` when the instance is not rendered at all, which is a real answer:
    /// there is nothing on the GPU to point at.
    pub fn active_index(&self, i: usize) -> Option<usize> {
        self.instances.get(i).filter(|inst| inst.is_active())?;
        Some(
            self.instances[..i]
                .iter()
                .filter(|inst| inst.is_active())
                .count(),
        )
    }

    pub fn is_active(&self) -> bool {
        self.enabled && self.active().next().is_some()
    }

    /// Whether the user has touched it, ignoring the bypass. An empty instance
    /// list is untouched even if the switch has been flicked.
    pub fn is_default(&self) -> bool {
        self.instances.is_empty()
    }

    pub fn is_modified(&self) -> bool {
        crate::params::is_modified(self.is_default(), self.is_active(), false)
    }

    pub fn total_dabs(&self) -> usize {
        self.instances.iter().map(Instance::dab_count).sum()
    }

    pub fn has_contrast(&self) -> bool {
        self.active().any(|i| i.contrast.abs() > 1.0e-6)
    }

    pub fn contrast_sigma_px(source_dims: (u32, u32)) -> f32 {
        let (w, h) = (source_dims.0 as f32, source_dims.1 as f32);
        (w.hypot(h) * Self::CONTRAST_SCALE).max(1.0)
    }

    /// The composite EV at one point — **the specification the compute shader
    /// implements**, in the order it implements it.
    ///
    /// `masks` holds one value per instance in `self.instances`, already evaluated
    /// at this point; a shorter slice reads as 1.0, which is what an identity mask
    /// contributes. Kept as an argument rather than computed here because the mask
    /// is a whole-image operation — the guided filter has no per-point form — and
    /// pretending otherwise would put a lie in the one function everything else is
    /// checked against.
    pub fn ev_at(&self, x: f32, y: f32, aspect: f32, masks: &[f32]) -> f32 {
        let mut total = 0.0;
        for (i, inst) in self.instances.iter().enumerate() {
            if !inst.is_active() {
                continue;
            }
            let ev = inst.ev_at(x, y, aspect);
            let m = masks.get(i).copied().unwrap_or(1.0);
            total += ev * m * inst.opacity;
        }
        total.clamp(-EV_MAX, EV_MAX)
    }

    /// Signed local-detail gain at one point. The shared log-detail residual is
    /// supplied by the caller; this function specifies only how layer coverage,
    /// Tone Mask, opacity and stacking combine.
    pub fn contrast_at(&self, x: f32, y: f32, aspect: f32, masks: &[f32]) -> f32 {
        let mut total = 0.0;
        for (i, inst) in self.instances.iter().enumerate() {
            if !inst.is_active() {
                continue;
            }
            let m = masks.get(i).copied().unwrap_or(1.0);
            total += inst.contrast * inst.coverage_at(x, y, aspect) * m * inst.opacity;
        }
        total.clamp(-1.0, 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dab(x: f32, y: f32, ev: f32) -> Dab {
        Dab {
            x,
            y,
            radius: 0.1,
            feather: 0.5,
            opacity: 1.0,
            ev,
            ..Dab::ROUND
        }
    }

    /// A square frame, so any test that does not mean to exercise the aspect
    /// correction cannot accidentally depend on it.
    const SQUARE: f32 = 1.0;

    fn burn(gestures: Vec<Gesture>) -> Instance {
        Instance::of(Sign::Burn, "Burn 1".into(), Shape::brush(gestures))
    }

    #[test]
    fn a_hard_dab_is_a_disc_and_a_feathered_one_is_not() {
        let hard = Dab {
            feather: 0.0,
            ..dab(0.5, 0.5, -1.0)
        };
        // Just inside the edge: a disc is still at full strength there.
        assert_eq!(hard.magnitude_at(0.59, 0.5, SQUARE), 1.0);
        assert_eq!(
            hard.magnitude_at(0.61, 0.5, SQUARE),
            0.0,
            "outside the radius"
        );

        let soft = dab(0.5, 0.5, -1.0);
        let near_edge = soft.magnitude_at(0.59, 0.5, SQUARE);
        assert!(
            near_edge > 0.0 && near_edge < 0.1,
            "feathered edge, not a cliff: {near_edge}"
        );
        assert!(
            soft.magnitude_at(0.5, 0.5, SQUARE) > 0.99,
            "full strength at the centre"
        );
    }

    #[test]
    fn the_cursor_guide_is_the_rendered_half_strength_contour() {
        assert_eq!(
            Dab::half_strength_radius(0.0),
            None,
            "a hard edge has no feather guide"
        );
        let radius = Dab::half_strength_radius(0.4).expect("the default is feathered");
        assert!(
            (radius - 0.47).abs() < 0.02,
            "the default guide should sit near mid-radius, not on the outer ring: {radius}"
        );
        assert!((Dab::profile(radius, 0.4) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn the_profile_is_compactly_supported() {
        // The property a rasteriser's bounding-box cull depends on. If a dab could
        // contribute anything at all beyond its radius, culling would clip it and
        // the CPU reference and the GPU would disagree in the last few percent of
        // the falloff — the hardest kind of difference to see.
        for feather in [0.0, 0.01, 0.4, 0.999] {
            let d = Dab {
                feather,
                ..dab(0.5, 0.5, -1.0)
            };
            assert_eq!(
                d.magnitude_at(0.5 + d.radius, 0.5, SQUARE),
                0.0,
                "feather {feather}"
            );
        }
    }

    #[test]
    fn a_dab_is_round_in_pixels_on_a_non_square_frame() {
        // The aspect correction, isolated. A 2:1 landscape frame: a radius of 0.1
        // is a tenth of the width horizontally, and the SAME number of pixels
        // vertically — which is a fifth of the height. Without the correction the
        // dab would reach 0.1 of the height too and come out as an ellipse.
        let aspect = 0.5; // h/w for a 1000x500 frame
        let d = Dab {
            feather: 0.0,
            ..dab(0.5, 0.5, -1.0)
        };

        // 0.1 of the height up is 0.05 widths: well inside a round dab.
        assert_eq!(d.magnitude_at(0.5, 0.6, aspect), 1.0, "inside vertically");
        // 0.25 of the height is 0.125 widths: outside it.
        assert_eq!(d.magnitude_at(0.5, 0.75, aspect), 0.0, "outside vertically");
        // And the horizontal extent is unchanged by the aspect.
        assert_eq!(d.magnitude_at(0.59, 0.5, aspect), 1.0);
        assert_eq!(d.magnitude_at(0.61, 0.5, aspect), 0.0);
    }

    #[test]
    fn sweeping_twice_in_one_pass_deposits_the_pass_once() {
        // The gesture model's whole point. Two overlapping dabs in ONE gesture at
        // the same spot must deposit one dab's worth, not two.
        let one = burn(vec![Gesture::new(vec![dab(0.5, 0.5, -1.0)])]);
        let twice = burn(vec![Gesture::new(vec![
            dab(0.5, 0.5, -1.0),
            dab(0.5, 0.5, -1.0),
        ])]);
        assert_eq!(one.ev_at(0.5, 0.5, SQUARE), twice.ev_at(0.5, 0.5, SQUARE));
    }

    #[test]
    fn two_passes_over_the_same_spot_add() {
        let one = burn(vec![Gesture::new(vec![dab(0.5, 0.5, -1.0)])]);
        let two = burn(vec![
            Gesture::new(vec![dab(0.5, 0.5, -1.0)]),
            Gesture::new(vec![dab(0.5, 0.5, -1.0)]),
        ]);
        let (a, b) = (one.ev_at(0.5, 0.5, SQUARE), two.ev_at(0.5, 0.5, SQUARE));
        assert!((b - 2.0 * a).abs() < 1e-6, "one pass {a}, two passes {b}");
    }

    #[test]
    fn intensity_is_ev_per_pass_exactly() {
        // A single click at full opacity and the centre of the dab must be the
        // intensity, to the bit. This is the property tanh saturation broke and is
        // why EV_MAX clips rather than curves.
        let inst = burn(vec![Gesture::new(vec![Dab {
            feather: 0.0,
            opacity: 1.0,
            ev: -1.0,
            ..dab(0.5, 0.5, -1.0)
        }])]);
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![inst],
        };
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[]), -1.0);
    }

    #[test]
    fn an_eraser_pass_subtracts_but_cannot_cross_zero() {
        // Two burn passes then three eraser passes: the erasure stops at zero
        // rather than turning into a one-pass dodge.
        let pass = || {
            Gesture::new(vec![Dab {
                feather: 0.0,
                ..dab(0.5, 0.5, -1.0)
            }])
        };
        let erase = || {
            Gesture::new(vec![Dab {
                feather: 0.0,
                ..dab(0.5, 0.5, 1.0)
            }])
        };

        let two_one = burn(vec![pass(), pass(), erase()]);
        assert!(
            (two_one.ev_at(0.5, 0.5, SQUARE) + 1.0).abs() < 1e-6,
            "two burns less one erase"
        );

        let over = burn(vec![pass(), pass(), erase(), erase(), erase()]);
        assert_eq!(
            over.ev_at(0.5, 0.5, SQUARE),
            0.0,
            "erasing past zero stops at zero"
        );
    }

    #[test]
    fn the_total_is_clipped_not_saturated() {
        let pass = || {
            Gesture::new(vec![Dab {
                feather: 0.0,
                ev: -1.0,
                ..dab(0.5, 0.5, -1.0)
            }])
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![burn(vec![pass(), pass(), pass()])],
        };
        // Three passes of one stop is exactly three stops — no roll-off on the way.
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[]), -3.0);

        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![burn(vec![pass(), pass(), pass(), pass(), pass(), pass()])],
        };
        assert_eq!(
            p.ev_at(0.5, 0.5, SQUARE, &[]),
            -EV_MAX,
            "and six stops clips at the limit"
        );
    }

    #[test]
    fn instances_stack_and_opposite_signs_cancel() {
        let d = |ev: f32| {
            Instance::of(
                if ev > 0.0 { Sign::Dodge } else { Sign::Burn },
                "x".into(),
                Shape::brush(vec![Gesture::new(vec![Dab {
                    feather: 0.0,
                    ev,
                    ..dab(0.5, 0.5, ev)
                }])]),
            )
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![d(-1.5), d(0.5)],
        };
        assert!((p.ev_at(0.5, 0.5, SQUARE, &[]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_disabled_instance_and_a_zero_opacity_one_contribute_nothing() {
        let g = vec![Gesture::new(vec![Dab {
            feather: 0.0,
            ..dab(0.5, 0.5, -1.0)
        }])];
        let off = Instance {
            enabled: false,
            ..burn(g.clone())
        };
        let clear = Instance {
            opacity: 0.0,
            ..burn(g.clone())
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![off, clear],
        };
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[]), 0.0);
    }

    #[test]
    fn master_opacity_scales_the_instance() {
        let g = vec![Gesture::new(vec![Dab {
            feather: 0.0,
            ev: -2.0,
            ..dab(0.5, 0.5, -2.0)
        }])];
        let half = Instance {
            opacity: 0.5,
            ..burn(g)
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![half],
        };
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[]), -1.0);
    }

    #[test]
    fn contrast_uses_coverage_not_brush_intensity() {
        let make = |ev| {
            let mut inst = Instance::of(
                Sign::Dodge,
                "Dodge 1".into(),
                Shape::brush(vec![Gesture::new(vec![Dab {
                    feather: 0.0,
                    ev,
                    ..dab(0.5, 0.5, ev)
                }])]),
            );
            inst.contrast = 0.6;
            DodgeBurnParams {
                enabled: true,
                instances: vec![inst],
            }
        };
        assert_eq!(make(0.1).contrast_at(0.5, 0.5, SQUARE, &[]), 0.6);
        assert_eq!(make(2.0).contrast_at(0.5, 0.5, SQUARE, &[]), 0.6);
    }

    #[test]
    fn erasing_a_layer_erases_its_contrast_coverage_too() {
        let ordinary = Gesture::new(vec![Dab {
            feather: 0.0,
            ev: -1.0,
            ..dab(0.5, 0.5, -1.0)
        }]);
        let erase = Gesture::new(vec![Dab {
            feather: 0.0,
            ev: 1.0,
            ..dab(0.5, 0.5, 1.0)
        }]);
        let mut inst = Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![ordinary, erase]),
        );
        inst.contrast = 1.0;
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![inst],
        };
        assert_eq!(p.contrast_at(0.5, 0.5, SQUARE, &[]), 0.0);
    }

    #[test]
    fn the_mask_multiplies_its_own_instance_only() {
        let g = || {
            vec![Gesture::new(vec![Dab {
                feather: 0.0,
                ev: -1.0,
                ..dab(0.5, 0.5, -1.0)
            }])]
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![burn(g()), burn(g())],
        };
        // Second instance fully masked out, first untouched.
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[1.0, 0.0]), -1.0);
        assert_eq!(p.ev_at(0.5, 0.5, SQUARE, &[0.5, 0.5]), -1.0);
    }

    // ----------------------------------------------------------- brush shapes
    //
    // **Every one of these is on a 2:1 frame at a non-trivial angle.** A brush-shape
    // test written on a square image with a 1:1 nib is symmetric in exactly the axis
    // an error would be in, which is the mistake that has now caught something in
    // three consecutive milestones. `LANDSCAPE` is the frame's h/w; the nib's own
    // ratio is a separate number and keeping them apart is the point.
    const LANDSCAPE: f32 = 0.5;

    fn nib(nib: Nib, aspect: f32, angle: f32) -> Dab {
        Dab {
            x: 0.5,
            y: 0.5,
            radius: 0.1,
            feather: 0.0,
            ev: -1.0,
            aspect,
            angle,
            nib,
            ..Dab::ROUND
        }
    }

    #[test]
    fn a_round_nib_at_aspect_one_is_the_disc_it_always_was() {
        // 10c must be an addition, not a migration: every stroke on disk is six
        // numbers that mean a round dab, and they have to keep meaning it.
        let d = nib(Nib::Round, 1.0, 0.0);
        assert_eq!(d.magnitude_at(0.5, 0.5, LANDSCAPE), 1.0);
        // 0.1 of the width across, and the same number of pixels down — which on a
        // 2:1 frame is 0.2 of the height.
        assert_eq!(d.magnitude_at(0.5 + 0.09, 0.5, LANDSCAPE), 1.0);
        assert_eq!(d.magnitude_at(0.5 + 0.11, 0.5, LANDSCAPE), 0.0);
        assert_eq!(d.magnitude_at(0.5, 0.5 + 0.18, LANDSCAPE), 1.0);
        assert_eq!(d.magnitude_at(0.5, 0.5 + 0.22, LANDSCAPE), 0.0);
        // And the angle does nothing, because a circle has no orientation.
        let turned = Dab { angle: 37.0, ..d };
        for at in [(0.55, 0.55), (0.45, 0.6), (0.58, 0.42)] {
            assert_eq!(
                d.magnitude_at(at.0, at.1, LANDSCAPE),
                turned.magnitude_at(at.0, at.1, LANDSCAPE)
            );
        }
    }

    #[test]
    fn an_elliptical_nib_is_long_on_its_own_axis_and_turns_with_the_angle() {
        // Aspect 3 is three times as tall as it is wide, in the nib's own frame.
        let flat = nib(Nib::Round, 3.0, 0.0);
        assert_eq!(
            flat.magnitude_at(0.5 + 0.09, 0.5, LANDSCAPE),
            1.0,
            "0.1 wide across"
        );
        assert_eq!(flat.magnitude_at(0.5 + 0.11, 0.5, LANDSCAPE), 0.0);
        // 0.3 of the WIDTH along the long axis is 0.6 of the height.
        assert_eq!(
            flat.magnitude_at(0.5, 0.5 + 0.56, LANDSCAPE),
            1.0,
            "0.3 wide along"
        );
        assert_eq!(flat.magnitude_at(0.5, 0.5 + 0.64, LANDSCAPE), 0.0);

        // Turned a quarter, the long axis is the horizontal one.
        let turned = Dab {
            angle: 90.0,
            ..flat
        };
        assert_eq!(turned.magnitude_at(0.5 + 0.28, 0.5, LANDSCAPE), 1.0);
        assert_eq!(turned.magnitude_at(0.5 + 0.32, 0.5, LANDSCAPE), 0.0);
        assert_eq!(turned.magnitude_at(0.5, 0.5 + 0.18, LANDSCAPE), 1.0);
        assert_eq!(turned.magnitude_at(0.5, 0.5 + 0.22, LANDSCAPE), 0.0);
    }

    #[test]
    fn a_card_has_corners_where_an_ellipse_has_none() {
        // The whole reason the card exists: a straight edge, and a corner you can
        // burn into. At 45° out from the centre an ellipse has already fallen off
        // and a rectangle of the same half-extents has not.
        let square = 0.1 / 2.0f32.sqrt();
        let round = nib(Nib::Round, 1.0, 0.0);
        let card = nib(Nib::Card, 1.0, 0.0);
        // A point at (0.09, 0.09) in WIDTH units is 0.127 from the centre — outside
        // a disc of radius 0.1, inside a square of half-width 0.1.
        let at = (0.5 + 0.09, 0.5 + 0.09 / LANDSCAPE);
        assert_eq!(
            round.magnitude_at(at.0, at.1, LANDSCAPE),
            0.0,
            "the disc has fallen off"
        );
        assert_eq!(
            card.magnitude_at(at.0, at.1, LANDSCAPE),
            1.0,
            "the card has a corner there"
        );
        // And just outside the corner, both are gone.
        let _ = square;
        assert_eq!(
            card.magnitude_at(0.5 + 0.11, 0.5 + 0.11 / LANDSCAPE, LANDSCAPE),
            0.0
        );
    }

    #[test]
    fn a_card_edge_is_straight() {
        // The property the darkroom gesture depends on. Walk along the card's own
        // edge: every point on it is at the boundary, so a hard card is at full
        // strength right up to it and nothing beyond — no scallop.
        let card = nib(Nib::Card, 0.4, 0.0);
        for t in [-0.9f32, -0.5, 0.0, 0.5, 0.9] {
            // Just inside the right-hand edge, at various heights up it.
            let y = 0.5 + t * 0.04 / LANDSCAPE;
            assert_eq!(
                card.magnitude_at(0.5 + 0.095, y, LANDSCAPE),
                1.0,
                "inside at t={t}"
            );
            assert_eq!(
                card.magnitude_at(0.5 + 0.105, y, LANDSCAPE),
                0.0,
                "outside at t={t}"
            );
        }
    }

    #[test]
    fn feather_stays_a_fraction_of_the_brush_whatever_shape_it_is() {
        // What makes 10c an addition rather than a migration: the feather control
        // does not change meaning. Half strength lands at the same *proportion* of
        // the way out for a disc, a long ellipse and a card.
        let half_at = |d: Dab, along_x: bool| {
            let mut lo = 0.0f32;
            let mut hi = 1.0f32;
            for _ in 0..40 {
                let m = (lo + hi) * 0.5;
                // A fraction of the nib's own extent on the axis being walked.
                let ext = if along_x {
                    d.radius
                } else {
                    d.radius * d.aspect
                };
                let p = if along_x {
                    (0.5 + m * ext, 0.5)
                } else {
                    (0.5, 0.5 + m * ext / LANDSCAPE)
                };
                if d.magnitude_at(p.0, p.1, LANDSCAPE) > 0.5 {
                    lo = m
                } else {
                    hi = m
                }
            }
            (lo + hi) * 0.5
        };
        let soft = |shape, aspect| Dab {
            feather: 0.6,
            ..nib(shape, aspect, 0.0)
        };
        let disc = half_at(soft(Nib::Round, 1.0), true);
        let long = half_at(soft(Nib::Round, 3.0), false);
        let card = half_at(soft(Nib::Card, 1.0), true);
        assert!(
            (disc - long).abs() < 0.02,
            "disc {disc} vs long axis {long}"
        );
        assert!((disc - card).abs() < 0.02, "disc {disc} vs card {card}");
    }

    /// The layer row names the nib, and a layer has one from the moment it is made.
    ///
    /// **The empty case is the bug the maintainer reported**: he made a Card layer and the row
    /// said `BRUSH`, because the first version of this derived the answer from the dabs
    /// and a fresh layer has none. A layer is a Card layer before you have painted on
    /// it — that is what picking Card *meant* — so the nib is on the layer and the
    /// answer is total.
    ///
    /// The rest is here because `ui_label` and `label` are one word apart at every call
    /// site and only one of them may ever reach a sidecar.
    #[test]
    fn a_brush_layer_is_named_by_its_nib_from_the_moment_it_is_made() {
        let empty = |nib| Shape::Brush {
            nib,
            passes: Vec::new(),
        };
        assert_eq!(
            empty(Nib::Card).ui_label(),
            "Card",
            "a Card layer says so before its first stroke"
        );
        assert_eq!(empty(Nib::Round).ui_label(), "Round");

        // Painted, and the strokes cannot disagree with the layer: `Brush::dab` takes
        // the nib from the instance, so there is no path that puts a card dab in a
        // round layer. `Shape::brush` is the round shorthand.
        let painted = Shape::Brush {
            nib: Nib::Card,
            passes: vec![Gesture::new(vec![nib(Nib::Card, 1.0, 0.0)])],
        };
        assert_eq!(painted.ui_label(), "Card");
        assert_eq!(
            Shape::brush(vec![Gesture::new(vec![nib(Nib::Round, 1.0, 0.0)])]).ui_label(),
            "Round"
        );

        // **The sidecar discriminant does not move**, whatever the row says. Every
        // `.mono.xmp` on disk carries these three words.
        assert_eq!(painted.label(), "brush");
        assert_eq!(empty(Nib::Card).label(), "brush");
        let grad = Shape::Linear(Linear {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
            feather: 1.0,
            ev: -1.0,
        });
        assert_eq!(
            grad.ui_label(),
            grad.label(),
            "a gradient's kind is what tells it apart, so the two agree"
        );
        assert_eq!(grad.nib(), None, "a gradient has no nib");
    }

    #[test]
    fn the_bound_encloses_the_nib_and_the_cull_can_trust_it() {
        // The GPU cull rejects a dab whose enclosing circle misses the tile. If the
        // bound were ever too small, a stroke would be hard-clipped along an
        // eight-pixel tile boundary — a step in the middle of a soft falloff.
        for (n, aspect, angle) in [
            (Nib::Round, 1.0, 0.0),
            (Nib::Round, 3.0, 41.0),
            (Nib::Round, 0.3, -70.0),
            (Nib::Card, 1.0, 0.0),
            (Nib::Card, 2.5, 33.0),
            (Nib::Card, 0.4, 115.0),
        ] {
            let d = nib(n, aspect, angle);
            let bound = d.bound();
            // Sweep the whole enclosing circle and a little past it: nothing outside
            // the bound may contribute, and something at the bound's own radius must
            // (or the bound is loose enough to be hiding an error).
            let mut reached = 0.0f32;
            for i in 0..720 {
                let t = i as f32 / 720.0 * std::f32::consts::TAU;
                for r in [0.999, 1.001, 1.2] {
                    let (dx, dy) = (bound * r * t.cos(), bound * r * t.sin() / LANDSCAPE);
                    let m = d.magnitude_at(0.5 + dx, 0.5 + dy, LANDSCAPE);
                    if r > 1.0 {
                        assert_eq!(
                            m, 0.0,
                            "{n:?} aspect {aspect} angle {angle} leaks past its bound"
                        );
                    } else {
                        reached = reached.max(bound * r);
                    }
                }
            }
            assert!(reached > 0.0, "{n:?} never reached its own bound");
        }
    }

    // ------------------------------------------------------------- gradients

    fn linear(x0: f32, y0: f32, x1: f32, y1: f32, feather: f32) -> Linear {
        Linear {
            x0,
            y0,
            x1,
            y1,
            feather,
            ev: -1.0,
        }
    }

    fn radial(outer: f32, feather: f32) -> Radial {
        Radial {
            cx: 0.5,
            cy: 0.5,
            inner: 0.0,
            outer,
            aspect: 1.0,
            angle: 0.0,
            feather,
            invert: false,
            ev: -1.0,
        }
    }

    #[test]
    fn a_linear_gradient_is_full_at_the_press_and_nothing_at_the_release() {
        // The drag convention, which is the only thing about this control anyone
        // has to remember: you drag FROM the effect TO nothing.
        let g = linear(0.2, 0.5, 0.8, 0.5, 1.0);
        assert_eq!(g.weight_at(0.2, 0.5, SQUARE), 1.0);
        assert_eq!(g.weight_at(0.8, 0.5, SQUARE), 0.0);
        assert!(
            (g.weight_at(0.5, 0.5, SQUARE) - 0.5).abs() < 1e-5,
            "half way is half"
        );
    }

    #[test]
    fn a_linear_gradient_is_unbounded_perpendicular_to_the_drag() {
        // What makes it a gradient rather than a shape: the band is infinite across
        // the drag, so the whole frame on the start side is affected and the whole
        // frame on the end side is not. A drag across the middle must therefore read
        // the same at the top of the picture as at the bottom.
        let g = linear(0.2, 0.5, 0.8, 0.5, 1.0);
        for y in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(g.weight_at(0.1, y, SQUARE), 1.0, "at y={y}");
            assert_eq!(g.weight_at(0.9, y, SQUARE), 0.0, "at y={y}");
        }
    }

    #[test]
    fn a_diagonal_gradient_is_square_to_the_drag_on_a_wide_frame() {
        // **The prototype's defect, as an assertion.** Its `_linear_gradient_weight`
        // projects in raw normalised space, where a step of 1 in y is a different
        // distance to a step of 1 in x — so the iso-lines of a diagonal gradient
        // come out skewed against the drag that made them.
        //
        // Two points placed symmetrically either side of the drag axis, in real
        // pixels, must have the same weight. On a 2:1 frame they do not without the
        // correction.
        let aspect = 0.5; // a 1000x500 frame
        let g = linear(0.3, 0.3, 0.7, 0.7, 1.0);
        // The axis in width units runs (0.4, 0.2); perpendicular is (-0.2, 0.4) —
        // which in y-normalised units is (-0.2, 0.8).
        let (mx, my) = (0.5, 0.5);
        let a = g.weight_at(mx - 0.05, my + 0.2, aspect);
        let b = g.weight_at(mx + 0.05, my - 0.2, aspect);
        assert!(
            (a - b).abs() < 1e-4,
            "the band is not square to the drag: {a} vs {b}"
        );
        assert!(
            (a - 0.5).abs() < 1e-4,
            "and the midpoint is still the midpoint: {a}"
        );
    }

    #[test]
    fn a_linear_feather_narrows_the_band_around_its_middle() {
        // Not towards an end: a hard-edged gradient breaks at the midpoint of the
        // drag, which is what makes feather a *width* control rather than a
        // position one.
        let hard = linear(0.0, 0.5, 1.0, 0.5, 0.0);
        assert_eq!(hard.weight_at(0.49, 0.5, SQUARE), 1.0);
        assert_eq!(hard.weight_at(0.51, 0.5, SQUARE), 0.0);
        // Half feather: the transition occupies the middle half of the drag.
        let half = linear(0.0, 0.5, 1.0, 0.5, 0.5);
        assert_eq!(half.weight_at(0.24, 0.5, SQUARE), 1.0);
        assert_eq!(half.weight_at(0.76, 0.5, SQUARE), 0.0);
        assert!((half.weight_at(0.5, 0.5, SQUARE) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn a_zero_length_drag_is_not_a_division_by_zero() {
        // Every frame between the press and the first movement is this.
        let g = linear(0.5, 0.5, 0.5, 0.5, 1.0);
        assert_eq!(g.weight_at(0.1, 0.9, SQUARE), 1.0);
        assert!(g.weight_at(0.1, 0.9, SQUARE).is_finite());
    }

    #[test]
    fn a_radial_gradient_is_round_in_pixels_at_aspect_one() {
        // The prototype's second fudge, and its own comment admits to it: it
        // normalises both axes by `outer_r` in a space where x is a fraction of
        // width and y a fraction of height, so its "circle" is as elliptical as the
        // frame. Here 1.0 is round, like the brush.
        let aspect = 0.5; // a 1000x500 frame
        let g = radial(0.2, 0.0); // hard-ish ramp, so the boundary is findable
        // 0.2 of the width horizontally, and the same number of pixels vertically —
        // which is 0.4 of the height.
        assert!(g.weight_at(0.5 + 0.19, 0.5, aspect) > 0.0);
        assert_eq!(g.weight_at(0.5 + 0.21, 0.5, aspect), 0.0);
        assert!(
            g.weight_at(0.5, 0.5 + 0.38, aspect) > 0.0,
            "and the same distance up"
        );
        assert_eq!(g.weight_at(0.5, 0.5 + 0.42, aspect), 0.0);
    }

    #[test]
    fn a_radial_aspect_makes_an_ellipse_and_the_angle_turns_it() {
        // On a square frame so the only anisotropy is the one the user asked for.
        let wide = Radial {
            aspect: 2.0,
            ..radial(0.2, 0.0)
        };
        assert_eq!(
            wide.weight_at(0.5 + 0.25, 0.5, SQUARE),
            0.0,
            "still 0.2 wide across"
        );
        assert!(
            wide.weight_at(0.5, 0.5 + 0.35, SQUARE) > 0.0,
            "and 0.4 tall"
        );

        // Turned a quarter, the tall axis is the horizontal one.
        let turned = Radial {
            angle: 90.0,
            ..wide
        };
        assert!(turned.weight_at(0.5 + 0.35, 0.5, SQUARE) > 0.0);
        assert_eq!(turned.weight_at(0.5, 0.5 + 0.25, SQUARE), 0.0);
    }

    #[test]
    fn inner_radius_is_the_full_strength_boundary() {
        let g = Radial {
            inner: 0.1,
            ..radial(0.3, 0.0)
        };
        assert_eq!(g.weight_at(0.5, 0.5, SQUARE), 1.0, "the centre");
        assert_eq!(
            g.weight_at(0.5 + 0.09, 0.5, SQUARE),
            1.0,
            "inside the inner radius"
        );
        assert!(
            g.weight_at(0.5 + 0.2, 0.5, SQUARE) < 1.0,
            "and falling off outside it"
        );
        assert_eq!(
            g.weight_at(0.5 + 0.31, 0.5, SQUARE),
            0.0,
            "gone at the outer"
        );
    }

    #[test]
    fn inverting_a_radial_turns_a_spotlight_into_a_vignette() {
        let spot = radial(0.3, 1.0);
        let vignette = Radial {
            invert: true,
            ..spot
        };
        for at in [0.0, 0.1, 0.2, 0.29, 0.5] {
            let (a, b) = (
                spot.weight_at(0.5 + at, 0.5, SQUARE),
                vignette.weight_at(0.5 + at, 0.5, SQUARE),
            );
            assert!((a + b - 1.0).abs() < 1e-6, "at {at}: {a} + {b}");
        }
        assert_eq!(
            vignette.weight_at(0.5, 0.5, SQUARE),
            0.0,
            "nothing in the middle"
        );
        assert_eq!(
            vignette.weight_at(0.95, 0.5, SQUARE),
            1.0,
            "everything at the edge"
        );
    }

    #[test]
    fn a_gradient_instance_scales_by_its_own_ev_and_the_instance_opacity() {
        let inst = Instance {
            opacity: 0.5,
            ..Instance::of(
                Sign::Burn,
                "Linear Burn 1".into(),
                Shape::Linear(Linear {
                    ev: -2.0,
                    ..linear(0.0, 0.5, 1.0, 0.5, 1.0)
                }),
            )
        };
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![inst],
        };
        assert_eq!(
            p.ev_at(0.0, 0.5, SQUARE, &[]),
            -1.0,
            "full strength, half opacity"
        );
        assert_eq!(
            p.ev_at(1.0, 0.5, SQUARE, &[]),
            0.0,
            "and nothing at the far end"
        );
    }

    #[test]
    fn a_gradient_takes_a_zone_mask_like_any_other_instance() {
        // The luminosity-masked gradient — "burn the sky values inside this ramp,
        // spare the whites" — which is the combination the two controls exist for.
        let inst = Instance::of(
            Sign::Burn,
            "Linear Burn 1".into(),
            Shape::Linear(Linear {
                ev: -2.0,
                ..linear(0.0, 0.5, 1.0, 0.5, 1.0)
            }),
        );
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![inst],
        };
        assert_eq!(p.ev_at(0.0, 0.5, SQUARE, &[0.25]), -0.5);
    }

    #[test]
    fn a_gradient_has_no_passes_to_paint_onto() {
        // The state the prototype's data can represent and its renderer ignores:
        // brush strokes on a radial instance. The enum refuses it, and this is the
        // accessor that makes every painting call site notice.
        let mut g = Instance::of(Sign::Burn, "x".into(), Shape::Radial(radial(0.3, 1.0)));
        assert!(g.gestures().is_empty());
        assert!(g.gestures_mut().is_none());
        assert_eq!(g.dab_count(), 0);
        assert!(g.is_active(), "but it still renders");
    }

    #[test]
    fn a_gradient_with_no_ev_renders_nothing() {
        // The equivalent of a brush instance with no dabs: a row in the list that
        // changes no pixel, so the graph must not emit a node for it.
        let g = Instance::of(
            Sign::Burn,
            "x".into(),
            Shape::Linear(Linear {
                ev: 0.0,
                ..linear(0.0, 0.5, 1.0, 0.5, 1.0)
            }),
        );
        assert!(!g.is_active());
    }

    #[test]
    fn an_open_mask_is_the_identity_and_says_so() {
        assert!(ZoneMask::default().is_identity(), "off");
        let open = ZoneMask {
            enabled: true,
            ..Default::default()
        };
        assert!(open.is_identity(), "on but open at both ends");
        assert!(
            !ZoneMask {
                invert: true,
                ..open
            }
            .is_identity(),
            "inverted, it is a hole"
        );
        assert!(!ZoneMask { hi: 0.0, ..open }.is_identity());
    }

    #[test]
    fn the_trapezoid_feathers_outward_from_its_bounds() {
        // Outward, not inward: the region the user selected is at FULL strength
        // and the shoulders live outside it. Feathering inward would mean a narrow
        // selection never reached 1.0 anywhere, which is the bug this pins.
        let m = ZoneMask {
            enabled: true,
            lo: -1.0,
            hi: 1.0,
            f_lo: 1.0,
            f_hi: 1.0,
            ..Default::default()
        };
        assert_eq!(m.evaluate(0.0), 1.0, "the middle of the band");
        assert_eq!(m.evaluate(-1.0), 1.0, "the lower bound itself is inside");
        assert_eq!(m.evaluate(1.0), 1.0, "and so is the upper");
        assert_eq!(m.evaluate(-2.0), 0.0, "one feather below is out");
        assert_eq!(m.evaluate(2.0), 0.0, "one feather above is out");
        assert!(
            (m.evaluate(-1.5) - 0.5).abs() < 1e-6,
            "halfway down the shoulder"
        );
    }

    #[test]
    fn a_bound_at_the_ruler_limit_is_open_and_has_no_shoulder() {
        // "Highlights only" must have exactly one edge. With the low bound at the
        // limit there is nothing below to fade into, and a shoulder drawn there
        // would darken the deepest shadows for no reason anyone asked for.
        let m = ZoneMask {
            enabled: true,
            lo: ZoneMask::MIN_EV,
            hi: 2.0,
            f_hi: 1.0,
            f_lo: 1.0,
            ..Default::default()
        };
        assert_eq!(m.evaluate(-5.0), 1.0);
        assert_eq!(m.evaluate(-4.5), 1.0, "no lower shoulder at all");
        assert_eq!(m.evaluate(2.0), 1.0);
        assert_eq!(m.evaluate(3.0), 0.0);
    }

    #[test]
    fn a_zero_feather_bound_is_a_step() {
        let m = ZoneMask {
            enabled: true,
            lo: 0.0,
            hi: 5.0,
            f_lo: 0.0,
            ..Default::default()
        };
        assert_eq!(m.evaluate(-0.01), 0.0);
        assert_eq!(m.evaluate(0.0), 1.0);
    }

    #[test]
    fn inverting_takes_the_complement() {
        let m = ZoneMask {
            enabled: true,
            lo: -1.0,
            hi: 1.0,
            ..Default::default()
        };
        let inv = ZoneMask { invert: true, ..m };
        for ev in [-3.0, -1.0, 0.0, 0.5, 2.0] {
            assert!(
                (m.evaluate(ev) + inv.evaluate(ev) - 1.0).abs() < 1e-6,
                "at {ev}"
            );
        }
    }

    #[test]
    fn edge_is_stored_in_ev_and_squared_at_use() {
        // The unit trap: the filter wants EV², the slider says EV, and one
        // conversion in one place is what keeps the label honest.
        let m = ZoneMask {
            edge: 0.5,
            ..Default::default()
        };
        assert!((m.eps() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn auto_name_skips_the_names_already_taken() {
        let existing = vec![
            Instance::new(Sign::Burn, "Burn 1".into()),
            Instance::new(Sign::Burn, "Burn 2".into()),
        ];
        assert_eq!(Instance::auto_name(Sign::Burn, &existing), "Burn 3");
        assert_eq!(Instance::auto_name(Sign::Dodge, &existing), "Dodge 1");
    }

    #[test]
    fn an_instance_with_no_dabs_renders_nothing() {
        let empty = burn(vec![Gesture::default()]);
        assert!(
            !empty.is_active(),
            "a gesture opened and released without a dab"
        );
        let p = DodgeBurnParams {
            enabled: true,
            instances: vec![empty],
        };
        assert!(!p.is_active());
    }
}
