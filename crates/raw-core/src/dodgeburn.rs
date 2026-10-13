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

/// How a dab fades from its middle to its rim.
///
/// **Two, because a stroke keeps the edge it was painted with.** Feather once meant how
/// far a bell curve had fallen by the rim, and that curve paints every stroke saved
/// before Feather meant the width of the fade. Redrawing those with the new edge would
/// enlarge and strengthen every dodge and burn already in somebody's prints, so a dab
/// says which it is, as it says which nib.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Falloff {
    /// The edge of every stroke saved before [`Core`](Self::Core): a bell curve that
    /// Feather narrowed, held to zero at the rim by a window across the whole radius.
    /// It reached the rim only at the smallest feathers, and at Feather 1.0 it had
    /// faded out by half the radius, so a softer brush painted a smaller mark.
    Bell,
    /// Full strength out to a core of `1 − feather` of the radius, then a smoothstep to
    /// nothing at the rim. **Feather is how much of the radius the fade takes**, as in
    /// Lightroom's brush: the cursor's outer circle is where paint ends and its inner
    /// circle where it is at full strength, and Feather 1.0 fades from the middle all
    /// the way out. A dab at Feather 1.0 has the cross-section of a [`Radial`] with no
    /// Core and Transition 1.0.
    #[default]
    Core,
}

/// One dab: a brush contact, and the whole storage primitive.
///
/// Ten numbers, laid out in the order the GPU buffer packs them, the nib and the
/// falloff sharing a word there.
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
    /// 0 is a hard disc, 1 is fully soft; what it measures is the [`falloff`]'s. Below
    /// [`Dab::HARD`] the dab is a disc whichever falloff it has.
    ///
    /// [`falloff`]: Self::falloff
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
    pub falloff: Falloff,
}

impl Dab {
    /// A round nib at rest: aspect 1, no rotation, and nothing else set.
    ///
    /// The tail for a struct-update literal — `Dab { x, y, radius, .., ..Dab::ROUND }`
    /// — so the common case reads as six numbers even though a dab now carries ten.
    /// Not `Default`, because a dab of radius zero is not a sensible default, it is
    /// the absence of one; this is explicitly *the round part* of a dab, with the edge
    /// the brush paints now.
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
        falloff: Falloff::Core,
    };

    /// Below this feather the profile is a hard disc.
    ///
    /// For a [`Falloff::Bell`] dab it is not a rounding convenience: the Gaussian's
    /// sigma is `radius / sqrt(-2·ln(1-feather))`, which goes to zero as feather does,
    /// so the smallest feathers would otherwise cost a division that tends to infinity
    /// to describe a shape the disc already describes exactly. A [`Falloff::Core`] dab
    /// there fades over a hundredth of its radius, which is a disc in all but name.
    pub const HARD: f32 = 0.01;

    /// Normalised brush profile at `distance`, before opacity and EV.
    ///
    /// Kept in one function because the cursor's feather guide must describe this
    /// exact falloff rather than inventing a second interpretation of Feather.
    fn profile(distance: f32, feather: f32, falloff: Falloff) -> f32 {
        if distance >= 1.0 {
            return 0.0;
        }
        if feather < Self::HARD {
            return 1.0;
        }
        match falloff {
            Falloff::Bell => {
                let f = feather.clamp(Self::HARD, 0.999);
                let sigma = 1.0 / (-2.0 * (1.0 - f).ln()).sqrt();
                let t = (1.0 - distance).clamp(0.0, 1.0);
                (-0.5 * (distance / sigma) * (distance / sigma)).exp() * t * t * (3.0 - 2.0 * t)
            }
            Falloff::Core => {
                let f = feather.min(1.0);
                let s = ((distance - (1.0 - f)) / f).clamp(0.0, 1.0);
                1.0 - s * s * (3.0 - 2.0 * s)
            }
        }
    }

    /// Where a [`Falloff::Core`] dab stops being at full strength, as a fraction of its
    /// radius: the brush cursor's inner circle. `None` for a hard brush, which is at
    /// full strength out to its rim, where the outer circle already is.
    ///
    /// Only the falloff the brush paints with, because the cursor is the brush's: a
    /// stroke already saved with the other one is never under it.
    pub fn core_radius(feather: f32) -> Option<f32> {
        (feather >= Self::HARD).then(|| 1.0 - feather.min(1.0))
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
        Self::profile(d, self.feather, self.falloff) * self.opacity * self.ev.abs()
    }

    /// This dab's coverage independent of its EV intensity.
    ///
    /// Local contrast uses the same painted shape as exposure, but Intensity must
    /// not secretly become a second Contrast amount. Opacity and feather still
    /// belong to the mark and therefore remain part of coverage.
    pub fn coverage_at(&self, x: f32, y: f32, frame: f32) -> f32 {
        Self::profile(self.distance(x, y, frame), self.feather, self.falloff) * self.opacity
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
///
/// # The island
///
/// A range selects every part of the frame that has its tones: set on a bright cloud,
/// it takes the white shirt too. With [`island`](Self::island) on and a seed placed,
/// the range selects only what is **connected to a seed**, growing from where the
/// picture was clicked through neighbors of the same tones and stopping where the tone
/// leaves the range. It is still this range, read the same way; the island is where it
/// is allowed to reach.
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
    /// Where the range may reach: everywhere, or only what is connected to its seeds.
    pub island: Island,
}

/// The seeds a tonal range grows from, when it is limited to what they are connected to.
///
/// **Points on the negative, and not a picture of the result.** A seed is where the
/// photograph was clicked, source-normalized, so a crop or a turn cannot move it off its
/// subject, and what grows from it is worked out again whenever the tones under it
/// change: an island follows an exposure change the way a range does.
///
/// A fixed few and not a list, which keeps a [`ZoneMask`] a plain value that is copied
/// and compared everywhere a range is. [`MAX`](Self::MAX) is more separate subjects
/// than one mask is for; past that, another instance says it better.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Island {
    /// The island's own switch. Off keeps the seeds, so turning it back on returns to
    /// the selection and not to an empty picture. On with no seed placed limits
    /// nothing: there is nowhere yet to be connected to.
    pub enabled: bool,
    seeds: [(f32, f32); Island::MAX],
    count: u8,
}

impl Island {
    /// The most seeds one range holds.
    pub const MAX: usize = 8;

    /// An island with no seeds yet, switched on or off.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }

    /// The seeds placed, in the order they were placed.
    pub fn seeds(&self) -> &[(f32, f32)] {
        &self.seeds[..usize::from(self.count)]
    }

    /// Whether the range is limited to what its seeds reach: switched on, with at
    /// least one seed to reach from.
    pub fn is_live(&self) -> bool {
        self.enabled && self.count > 0
    }

    /// Start again from one seed at `at`: what a plain click with the eyedropper does.
    /// `false`, and the seeds there were are kept, when `at` is not a point of the
    /// negative.
    pub fn set(&mut self, at: (f32, f32)) -> bool {
        let mut fresh = Self::new(self.enabled);
        if !fresh.add(at) {
            return false;
        }
        *self = fresh;
        true
    }

    /// One more seed at `at`: what `⇧`-click does. `false`, and nothing changed, when
    /// the island is full or `at` is not a point of the negative.
    pub fn add(&mut self, at: (f32, f32)) -> bool {
        let on_negative = |v: f32| (0.0..=1.0).contains(&v);
        if usize::from(self.count) >= Self::MAX || !on_negative(at.0) || !on_negative(at.1) {
            return false;
        }
        self.seeds[usize::from(self.count)] = at;
        self.count += 1;
        true
    }

    /// Take seed `index` away and close the gap, keeping the others in order.
    pub fn remove(&mut self, index: usize) {
        let n = usize::from(self.count);
        if index >= n {
            return;
        }
        self.seeds.copy_within(index + 1..n, index);
        self.count -= 1;
        // The freed slot goes back to zero, so two islands with the same seeds are
        // equal whatever was placed and removed on the way.
        self.seeds[n - 1] = (0.0, 0.0);
    }

    /// Take every seed away. The switch stays as it is.
    pub fn clear(&mut self) {
        *self = Self {
            enabled: self.enabled,
            ..Self::default()
        };
    }
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
    ///
    /// **An island is not in this answer**: whether a point is connected to a seed is a
    /// fact about the picture around it, which one EV cannot say.
    /// [`Basis::evaluate`](crate::Basis::evaluate) is where an island is grown.
    pub fn evaluate(&self, ev: f32) -> f32 {
        let m = self.select(ev);
        if self.invert { 1.0 - m } else { m }
    }

    /// How strongly the range selects a tone, in `[0, 1]`, **before `Inv`**: the
    /// trapezoid alone. What an island grows through, since `Inv` turns over the
    /// finished selection and not the tones it was grown from.
    pub fn select(&self, ev: f32) -> f32 {
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
        m
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

    /// TK9's ladder, as trapezoids: Lights 1 to 4, then Darks 1 to 4, each as
    /// `(label, lo, hi, f_lo, f_hi)` like [`PRESETS`](Self::PRESETS).
    ///
    /// In Tony Kuyper's panels each Lights step multiplies the mask by itself, so Lights-2
    /// is about the luminosity squared, Lights-3 its cube, and each selects a narrower and
    /// brighter part of the picture than the last. Darks are the same on the shadow side,
    /// from the inverted luminosity. These are **soft masks with no flat top**: there is no
    /// plateau, only a long shoulder that stiffens with every step.
    ///
    /// **The trapezoid approximates them rather than reproducing them**, which was a
    /// decision (a second falloff kind would be truer and would add a field to the file).
    /// Each is the shoulder that best fits `L^n`, where `L` is the sRGB-encoded
    /// luminance at that many stops from middle gray, found by a grid search over this
    /// ruler's window and rounded to a quarter stop. **Every Lights step is pinned to the
    /// ruler's top and every Darks step to its bottom**, and only the shoulder varies. That
    /// is what makes a later step never select more than an earlier one at any tone, as
    /// `L^2 <= L` never does in TK9: free-fitting the top point as well gave a slightly
    /// closer curve and broke the nesting, which a test caught. A test also holds each step
    /// to a bound against the function it stands for.
    ///
    /// The fit cannot do better than a smoothstep allows: TK9's tails are longer than
    /// one, so Lights-1 reads a little low in the deep shadows and a little high in the
    /// midtones. It is a starting point to refine on the ruler, not a measurement.
    pub const LADDER: [(&'static str, f32, f32, f32, f32); 8] = [
        ("Lights 1", 5.0, 5.0, 10.25, 1.0),
        ("Lights 2", 5.0, 5.0, 7.75, 1.0),
        ("Lights 3", 5.0, 5.0, 6.75, 1.0),
        ("Lights 4", 5.0, 5.0, 6.25, 1.0),
        ("Darks 1", -5.0, -5.0, 1.0, 9.5),
        ("Darks 2", -5.0, -5.0, 1.0, 7.5),
        ("Darks 3", -5.0, -5.0, 1.0, 5.75),
        ("Darks 4", -5.0, -5.0, 1.0, 4.75),
    ];

    /// How wide a range sampled from the picture starts, in EV: one zone.
    ///
    /// A click names one tone and a range has to have some width to select anything, so
    /// this is the least that reads as "that tone" on the ruler. It is a starting point
    /// to widen there, not a measurement of what was clicked.
    pub const SAMPLE_WIDTH: f32 = 1.0;
    /// The shoulders a sampled range is given, in EV: one zone either side.
    pub const SAMPLE_SHOULDER: f32 = 1.0;

    /// Whether neither end of the range is set, so it selects every tone.
    pub fn is_open(&self) -> bool {
        self.lo <= Self::MIN_EV + 1e-6 && self.hi >= Self::MAX_EV - 1e-6
    }

    /// Set the range from tones sampled in the picture, `lo` to `hi` in EV: what the
    /// eyedropper does with a click (`lo == hi`) and with a dragged box.
    ///
    /// **Never narrower than [`SAMPLE_WIDTH`](Self::SAMPLE_WIDTH)**, widened about its own
    /// middle, so a click and a box over a flat wall give the same range. The shoulders
    /// are reset with it, because the sample replaces the range rather than adjusting it,
    /// and the range is switched on, as choosing a preset does. `Inv` is left alone: it
    /// says what the range means, and the sample only says where the range is.
    ///
    /// A sample at or past an end of the ruler clamps onto it, which reads as open on
    /// that side: sampling the deepest shadow selects it and everything below.
    pub fn sample(&mut self, lo: f32, hi: f32) {
        if !lo.is_finite() || !hi.is_finite() {
            return;
        }
        let (mut lo, mut hi) = (lo.min(hi), lo.max(hi));
        if hi - lo < Self::SAMPLE_WIDTH {
            let middle = 0.5 * (lo + hi);
            lo = middle - 0.5 * Self::SAMPLE_WIDTH;
            hi = middle + 0.5 * Self::SAMPLE_WIDTH;
        }
        self.lo = lo.clamp(Self::MIN_EV, Self::MAX_EV);
        self.hi = hi.clamp(Self::MIN_EV, Self::MAX_EV);
        self.f_lo = Self::SAMPLE_SHOULDER;
        self.f_hi = Self::SAMPLE_SHOULDER;
        self.enabled = true;
    }

    /// Widen the range to take in tones `lo` to `hi` as well: the eyedropper with `⇧`.
    ///
    /// **It only ever widens.** An end moves outward to reach a tone it did not include
    /// and otherwise stays, and the shoulders are kept, since they were set for this
    /// range. A range open at both ends already includes every tone, so there is nothing
    /// for it to do: starting a range there instead was tried, and it meant a range
    /// widened to the whole ruler collapsed to one zone on the next `⇧`-click, which a
    /// test caught. A plain click is what starts a range.
    pub fn extend(&mut self, lo: f32, hi: f32) {
        if !lo.is_finite() || !hi.is_finite() {
            return;
        }
        self.lo = self.lo.min(lo.min(hi)).clamp(Self::MIN_EV, Self::MAX_EV);
        self.hi = self.hi.max(lo.max(hi)).clamp(Self::MIN_EV, Self::MAX_EV);
        self.enabled = true;
    }
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
            island: Island::default(),
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

    /// The passes of a painted shape, or nothing at all for a gradient.
    pub fn passes(&self) -> &[Gesture] {
        match self {
            Self::Brush { passes, .. } => passes,
            _ => &[],
        }
    }

    /// `None` for a gradient, which has no passes to add one to.
    pub fn passes_mut(&mut self) -> Option<&mut Vec<Gesture>> {
        match self {
            Self::Brush { passes, .. } => Some(passes),
            _ => None,
        }
    }

    /// How much of a point a painted shape covers, in `[0, 1]`, **as a mask**: each
    /// pass counts by its coverage, an eraser pass takes its coverage away, and the sum
    /// is held between nothing and everything.
    ///
    /// The same sum a Dodge layer's [`Instance::coverage_at`] is, since painting a mask
    /// is painting in a positive direction: a pass whose dabs carry a positive `ev`
    /// adds and one whose dabs are negative erases, and how large the `ev` is does not
    /// come into it. Zero for a gradient, which is not painted.
    pub fn painted_at(&self, x: f32, y: f32, aspect: f32) -> f32 {
        self.passes()
            .iter()
            .map(|pass| pass.sign() * pass.coverage_at(x, y, aspect))
            .sum::<f32>()
            .clamp(0.0, 1.0)
    }
}

/// What has been painted, **as a small picture**: a painted shape's coverage, kept up
/// to date as strokes are added, for a caller that asks about many points at once.
///
/// [`Shape::painted_at`] is the truth and tests every dab for every point, which is
/// right for a click and ruinous for the curve editor's histogram: fifty thousand
/// samples against a thousand dabs, on every frame of a stroke. This holds the same
/// coverage on a grid [`LONG_EDGE`](Self::LONG_EDGE) on its long side, and a stroke in
/// progress costs only its new dabs, each over its own bounding box.
///
/// It is the picture a mask is: soft, with no detail finer than a brush, which is why
/// a few hundred pixels hold it. It is an estimate all the same, and nothing that
/// renders reads it; the shader walks the dabs.
#[derive(Debug, Clone)]
pub struct Coverage {
    w: usize,
    h: usize,
    aspect: f32,
    /// The passes folded into `total`, in order.
    done: Vec<Gesture>,
    /// Their sum, signed and unclamped, as `painted_at` sums before it clamps.
    total: Vec<f32>,
    /// The newest pass, which may still be growing: what of it has been drawn, and its
    /// coverage, a maximum over its dabs. Folded into `total` when a pass follows it.
    last: Option<(Gesture, Vec<f32>)>,
}

impl Coverage {
    /// The grid's long side, in pixels: the size the zone masks are prepared at, for
    /// the same reason. A mask has no detail finer than this.
    pub const LONG_EDGE: usize = 480;

    /// An empty picture for a negative whose height over width is `aspect`.
    pub fn new(aspect: f32) -> Self {
        let aspect = if aspect.is_finite() && aspect > 0.0 {
            aspect
        } else {
            1.0
        };
        let (w, h) = if aspect <= 1.0 {
            (Self::LONG_EDGE, (Self::LONG_EDGE as f32 * aspect).round() as usize)
        } else {
            ((Self::LONG_EDGE as f32 / aspect).round() as usize, Self::LONG_EDGE)
        };
        let (w, h) = (w.max(1), h.max(1));
        Self {
            w,
            h,
            aspect,
            done: Vec::new(),
            total: vec![0.0; w * h],
            last: None,
        }
    }

    /// The negative's height over width this was made for.
    pub fn aspect(&self) -> f32 {
        self.aspect
    }

    /// Bring the picture up to `passes`.
    ///
    /// **Only what is new is drawn.** When the passes held are still the first of
    /// `passes` and the newest has only grown, the new dabs are drawn and nothing else:
    /// that is every frame of a stroke, and every stroke after the first. Anything else,
    /// an undo, an erased pass, another mask's strokes, starts again from nothing.
    pub fn update(&mut self, passes: &[Gesture]) {
        let kept = self.done.len();
        let same_start =
            passes.len() >= kept && self.done.iter().zip(passes).all(|(a, b)| a == b);
        let newest_grew = match &self.last {
            None => true,
            Some((drawn, _)) => passes.get(kept).is_some_and(|now| {
                now.dabs.len() >= drawn.dabs.len()
                    && now.dabs[..drawn.dabs.len()] == drawn.dabs[..]
            }),
        };
        if !(same_start && newest_grew) {
            self.done.clear();
            self.total.fill(0.0);
            self.last = None;
        }
        for i in self.done.len()..passes.len() {
            let pass = &passes[i];
            let (w, h, aspect) = (self.w, self.h, self.aspect);
            let (drawn, cover) = self
                .last
                .get_or_insert_with(|| (Gesture::default(), vec![0.0; w * h]));
            for dab in &pass.dabs[drawn.dabs.len()..] {
                Self::draw(cover, w, h, aspect, dab);
            }
            *drawn = pass.clone();
            // Every pass but the newest is finished, and joins the sum.
            if i + 1 < passes.len() {
                let sign = pass.sign();
                if let Some((done, cover)) = self.last.take() {
                    for (total, c) in self.total.iter_mut().zip(&cover) {
                        *total += sign * c;
                    }
                    self.done.push(done);
                }
            }
        }
    }

    /// One dab into a pass's coverage, as a maximum, over the dab's own bounding box.
    fn draw(cover: &mut [f32], w: usize, h: usize, aspect: f32, dab: &Dab) {
        let reach = dab.bound();
        let column = |x: f32| ((x * w as f32).floor().max(0.0) as usize).min(w - 1);
        let row = |y: f32| ((y * h as f32).floor().max(0.0) as usize).min(h - 1);
        // The bound is in width units; a row is a fraction of the height.
        let (x0, x1) = (column(dab.x - reach), column(dab.x + reach));
        let (y0, y1) = (row(dab.y - reach / aspect), row(dab.y + reach / aspect));
        for py in y0..=y1 {
            let y = (py as f32 + 0.5) / h as f32;
            for px in x0..=x1 {
                let x = (px as f32 + 0.5) / w as f32;
                let at = &mut cover[py * w + px];
                *at = at.max(dab.coverage_at(x, y, aspect));
            }
        }
    }

    /// What has been painted at a source-normalized point, in `[0, 1]`: the grid's
    /// answer to [`Shape::painted_at`], blended from the four pixels around the point.
    pub fn at(&self, x: f32, y: f32) -> f32 {
        let (w, h) = (self.w, self.h);
        let fx = (x * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
        let fy = (y * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let newest = self.last.as_ref().map(|(pass, cover)| (pass.sign(), cover));
        let cell = |cx: usize, cy: usize| {
            let i = cy * w + cx;
            self.total[i] + newest.map_or(0.0, |(sign, cover)| sign * cover[i])
        };
        let top = cell(x0, y0) + (cell(x1, y0) - cell(x0, y0)) * tx;
        let bottom = cell(x0, y1) + (cell(x1, y1) - cell(x0, y1)) * tx;
        (top + (bottom - top) * ty).clamp(0.0, 1.0)
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
    /// sRGB-encoded luminance at `ev` stops from middle gray: the `L` TK9's masks are
    /// powers of.
    fn tk9_luminance(ev: f32) -> f32 {
        let linear = (MID_GREY * ev.exp2()).clamp(0.0, 1.0);
        if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        }
    }

    fn ladder(label: &str) -> ZoneMask {
        let &(_, lo, hi, f_lo, f_hi) = ZoneMask::LADDER
            .iter()
            .find(|row| row.0 == label)
            .expect("a step of the ladder");
        ZoneMask {
            enabled: true,
            lo,
            hi,
            f_lo,
            f_hi,
            ..ZoneMask::default()
        }
    }

    /// RMS difference between a step and the function TK9's step is, over the ruler.
    fn rms_against(mask: &ZoneMask, target: impl Fn(f32) -> f32) -> f32 {
        let evs: Vec<f32> = (0..=200).map(|i| -5.0 + i as f32 * 0.05).collect();
        let sum: f32 = evs
            .iter()
            .map(|&ev| (mask.evaluate(ev) - target(ev)).powi(2))
            .sum();
        (sum / evs.len() as f32).sqrt()
    }

    #[test]
    fn each_ladder_step_stands_for_a_power_of_luminosity() {
        for n in 1..=4 {
            let lights = ladder(&format!("Lights {n}"));
            let darks = ladder(&format!("Darks {n}"));
            let lights_err = rms_against(&lights, |ev| tk9_luminance(ev).powi(n));
            let darks_err = rms_against(&darks, |ev| (1.0 - tk9_luminance(ev)).powi(n));
            // A smoothstep cannot be TK9's long tail, so this is a bound on how far
            // from it the preset is allowed to drift, not a claim of equality.
            assert!(lights_err < 0.13, "Lights {n} is {lights_err} from L^{n}");
            assert!(darks_err < 0.13, "Darks {n} is {darks_err} from (1-L)^{n}");
        }
    }

    #[test]
    fn each_step_is_narrower_and_stiffer_than_the_last() {
        // Narrower: a later step selects less of every tone that an earlier one only
        // partly selected, so at any stop below white its weight is no greater.
        for family in ["Lights", "Darks"] {
            for n in 1..4 {
                let (wide, tight) = (
                    ladder(&format!("{family} {n}")),
                    ladder(&format!("{family} {}", n + 1)),
                );
                for i in 0..=100 {
                    let ev = -5.0 + i as f32 * 0.1;
                    assert!(
                        tight.evaluate(ev) <= wide.evaluate(ev) + 1.0e-6,
                        "{family} {} selects more than {family} {n} at {ev} EV",
                        n + 1
                    );
                }
                // And its shoulder is shorter, which is what "narrower" means on the ruler.
                let shoulder = |m: &ZoneMask| if family == "Lights" { m.f_lo } else { m.f_hi };
                assert!(shoulder(&tight) < shoulder(&wide), "{family} {n}");
            }
        }
    }

    #[test]
    fn lights_reach_the_highlights_and_darks_the_shadows() {
        for n in 1..=4 {
            let lights = ladder(&format!("Lights {n}"));
            let darks = ladder(&format!("Darks {n}"));
            assert!(lights.evaluate(5.0) > 0.99 && lights.evaluate(-5.0) < 0.01);
            assert!(darks.evaluate(-5.0) > 0.99 && darks.evaluate(5.0) < 0.01);
        }
        // No flat top: the band has no width, only shoulders.
        for (label, lo, hi, ..) in ZoneMask::LADDER {
            assert!((hi - lo).abs() < 1.0e-6, "{label} has a flat top");
        }
    }

    #[test]
    fn every_ladder_step_sits_inside_the_ruler() {
        for (label, lo, hi, f_lo, f_hi) in ZoneMask::LADDER {
            assert!((ZoneMask::MIN_EV..=ZoneMask::MAX_EV).contains(&lo), "{label}");
            assert!((ZoneMask::MIN_EV..=ZoneMask::MAX_EV).contains(&hi), "{label}");
            assert!(lo <= hi && f_lo > 0.0 && f_hi > 0.0, "{label}");
        }
    }

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
            near_edge > 0.0 && near_edge < 0.15,
            "feathered edge, not a cliff: {near_edge}"
        );
        assert!(
            soft.magnitude_at(0.5, 0.5, SQUARE) > 0.99,
            "full strength at the centre"
        );
    }

    #[test]
    fn feather_is_how_much_of_the_radius_the_fade_takes() {
        let at = |d: f32, feather: f32| Dab::profile(d, feather, Falloff::Core);
        for feather in [0.1, 0.4, 0.6, 1.0] {
            let core = 1.0 - feather;
            // Full strength to the core, half way through the fade at its middle, and
            // nothing at the rim: the cursor's two circles and the halfway between.
            assert_eq!(at(core * 0.99, feather), 1.0, "inside the core at {feather}");
            let half = at(1.0 - feather * 0.5, feather);
            assert!((half - 0.5).abs() < 1e-5, "half way at {feather}: {half}");
            assert!(at(0.999, feather) > 0.0, "the fade reaches the rim at {feather}");
            assert_eq!(at(1.0, feather), 0.0);
            // Smooth all the way: no step anywhere along the radius.
            let steepest = (0..1000)
                .map(|i| (at(i as f32 / 1000.0, feather) - at((i + 1) as f32 / 1000.0, feather)).abs())
                .fold(0.0, f32::max);
            assert!(steepest < 0.02 / feather.max(0.1), "a step of {steepest} at {feather}");
        }
        // **A softer brush is not a smaller one**, which is what the bell did: at
        // Feather 1.0 it had faded below a twentieth by 0.55 of the radius.
        assert!(at(0.85, 1.0) > 0.05);
        assert!(Dab::profile(0.6, 1.0, Falloff::Bell) < 0.05);
        // The cursor's inner circle is the core.
        assert_eq!(Dab::core_radius(0.0), None, "a hard edge has no inner circle");
        assert!((Dab::core_radius(0.6).unwrap() - 0.4).abs() < 1e-6);
        assert_eq!(Dab::core_radius(1.0), Some(0.0));
    }

    #[test]
    fn a_saved_stroke_keeps_the_bell_it_was_painted_with() {
        // The old falloff, pinned: the default Feather 0.40 was at half strength a
        // little short of mid-radius. A stroke read from a file saved before the core
        // falloff is drawn this way still.
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let mid = (lo + hi) * 0.5;
            if Dab::profile(mid, 0.4, Falloff::Bell) > 0.5 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        assert!((lo - 0.465).abs() < 0.01, "half strength at {lo}");
        let bell = Dab {
            feather: 0.4,
            falloff: Falloff::Bell,
            ..dab(0.5, 0.5, -1.0)
        };
        let core = Dab {
            falloff: Falloff::Core,
            ..bell
        };
        assert_ne!(
            bell.coverage_at(0.57, 0.5, SQUARE),
            core.coverage_at(0.57, 0.5, SQUARE),
            "the two edges are not the same edge"
        );
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

    // ------------------------------------------------- painted coverage, as a picture

    /// A soft dab for a mask: `paint` adds, and not erases.
    fn mask_dab(x: f32, y: f32, paint: bool) -> Dab {
        Dab {
            x,
            y,
            radius: 0.12,
            feather: 0.5,
            opacity: 0.8,
            ev: if paint { 1.0 } else { -1.0 },
            ..Dab::ROUND
        }
    }

    fn stroke(y: f32, n: usize, paint: bool) -> Gesture {
        Gesture::new(
            (0..n)
                .map(|i| mask_dab(0.1 + 0.8 * i as f32 / n.max(1) as f32, y, paint))
                .collect(),
        )
    }

    /// The worst difference between the picture and `painted_at`, at the grid's own
    /// pixel centers, where the two are the same arithmetic.
    fn worst(cover: &Coverage, passes: &[Gesture]) -> f32 {
        let shape = Shape::brush(passes.to_vec());
        let (w, h) = (cover.w, cover.h);
        let mut worst = 0.0f32;
        for py in (0..h).step_by(7) {
            for px in (0..w).step_by(7) {
                let (x, y) = ((px as f32 + 0.5) / w as f32, (py as f32 + 0.5) / h as f32);
                worst = worst.max((cover.at(x, y) - shape.painted_at(x, y, cover.aspect)).abs());
            }
        }
        worst
    }

    #[test]
    fn the_picture_of_what_is_painted_is_what_is_painted() {
        // Two passes that overlap, and an eraser through them, on a negative that is
        // not square. At its own pixels the picture is `painted_at`, to rounding.
        for aspect in [0.667, 1.5] {
            let passes = vec![stroke(0.3, 12, true), stroke(0.45, 12, true), stroke(0.38, 5, false)];
            let mut cover = Coverage::new(aspect);
            assert_eq!(cover.w.max(cover.h), Coverage::LONG_EDGE);
            cover.update(&passes);
            assert!(worst(&cover, &passes) < 1.0e-5, "{aspect}: {}", worst(&cover, &passes));
            // And there is something in it to agree about.
            assert!(cover.at(0.5, 0.3) > 0.5);
            assert_eq!(cover.at(0.5, 0.95), 0.0);
        }
    }

    #[test]
    fn a_stroke_drawn_dab_by_dab_is_the_stroke_drawn_at_once() {
        // The incremental path is the one that runs: a pass grows a few dabs a frame,
        // then another starts. After every step the picture is what a fresh one of the
        // same passes would be.
        let mut passes: Vec<Gesture> = Vec::new();
        let mut cover = Coverage::new(0.667);
        for (y, n, paint) in [(0.3, 9, true), (0.6, 7, true), (0.45, 6, false), (0.5, 4, true)] {
            passes.push(Gesture::new(Vec::new()));
            for dab in stroke(y, n, paint).dabs.iter() {
                passes.last_mut().unwrap().dabs_mut().push(*dab);
                cover.update(&passes);
                assert!(worst(&cover, &passes) < 1.0e-5);
            }
        }
        let mut fresh = Coverage::new(0.667);
        fresh.update(&passes);
        assert_eq!(cover.total, fresh.total);
        assert_eq!(cover.done.len(), 3, "three finished passes and one still growing");
    }

    #[test]
    fn a_picture_follows_an_undo_and_a_change_of_strokes() {
        let passes = vec![stroke(0.3, 8, true), stroke(0.6, 8, true)];
        let mut cover = Coverage::new(1.0);
        cover.update(&passes);
        // The last stroke undone, then all of them.
        cover.update(&passes[..1]);
        assert!(worst(&cover, &passes[..1]) < 1.0e-5);
        assert_eq!(cover.at(0.5, 0.6), 0.0, "the undone stroke is still in the picture");
        cover.update(&[]);
        assert_eq!(cover.at(0.5, 0.3), 0.0);
        // A dab of the stroke in flight taken back, which a growing pass never does.
        let mut shorter = passes.clone();
        shorter[1].dabs_mut().truncate(3);
        cover.update(&passes);
        cover.update(&shorter);
        assert!(worst(&cover, &shorter) < 1.0e-5);
        // Other strokes altogether.
        let other = vec![stroke(0.8, 5, true)];
        cover.update(&other);
        assert!(worst(&cover, &other) < 1.0e-5);
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
    fn a_sampled_tone_becomes_one_zone_with_one_zone_shoulders() {
        // Starting from a range somebody had shaped, inverted and switched off: the sample
        // replaces where the range is, switches it on, and leaves what it means alone.
        let mut m = ZoneMask {
            lo: -4.0,
            hi: -3.0,
            f_lo: 0.25,
            f_hi: 2.0,
            invert: true,
            region: 0.08,
            ..Default::default()
        };
        m.sample(1.5, 1.5);
        assert!(m.enabled, "sampling a range is asking to see it");
        assert_eq!((m.lo, m.hi), (1.0, 2.0), "one zone, centered on the tone");
        assert_eq!((m.f_lo, m.f_hi), (1.0, 1.0));
        assert!(m.invert, "Inv says what the range means, not where it is");
        assert_eq!(m.region, 0.08, "and how it reads the picture is not the sample's");
        // The sampled tone is at full weight, once Inv is set aside.
        m.invert = false;
        assert_eq!(m.evaluate(1.5), 1.0);
        assert_eq!(m.evaluate(-0.5), 0.0, "past the lower shoulder");
    }

    #[test]
    fn a_sampled_box_spans_its_tones_and_is_never_narrower_than_a_click() {
        let mut m = ZoneMask::default();
        // Given either way round.
        m.sample(2.0, -1.0);
        assert_eq!((m.lo, m.hi), (-1.0, 2.0));
        // A box over a flat wall is a click on it.
        let mut flat = ZoneMask::default();
        flat.sample(0.4, 0.6);
        let mut click = ZoneMask::default();
        click.sample(0.5, 0.5);
        assert_eq!(flat, click);
        assert!((flat.hi - flat.lo - ZoneMask::SAMPLE_WIDTH).abs() < 1e-6);
    }

    #[test]
    fn a_sample_at_the_end_of_the_ruler_is_open_on_that_side() {
        // The deepest shadow: there is nothing below it to leave out, so the range runs
        // off the bottom of the ruler, which is what open means.
        let mut m = ZoneMask::default();
        m.sample(-4.8, -4.8);
        assert_eq!(m.lo, ZoneMask::MIN_EV);
        assert!((m.hi + 4.3).abs() < 1e-6);
        assert_eq!(m.evaluate(-9.0), 1.0, "everything below is in");
        // A tone off the ruler altogether still selects itself.
        let mut off = ZoneMask::default();
        off.sample(7.0, 7.0);
        assert_eq!((off.lo, off.hi), (ZoneMask::MAX_EV, ZoneMask::MAX_EV));
        assert_eq!(off.evaluate(7.0), 1.0);
        assert_eq!(off.evaluate(3.5), 0.0);
        // And a sample that is not a number changes nothing.
        let before = off;
        off.sample(f32::NAN, 1.0);
        off.extend(1.0, f32::INFINITY);
        assert_eq!(off, before);
    }

    #[test]
    fn extending_a_range_widens_it_and_never_narrows_it() {
        let start = ZoneMask {
            enabled: true,
            lo: -1.0,
            hi: 1.0,
            f_lo: 0.5,
            f_hi: 1.5,
            ..Default::default()
        };
        // A tone already inside moves nothing, shoulders included.
        let mut m = start;
        m.extend(0.25, 0.25);
        assert_eq!(m, start);
        // One in a shoulder, only partly selected, is taken in whole.
        m.extend(-1.25, -1.25);
        assert_eq!((m.lo, m.hi), (-1.25, 1.0));
        assert_eq!(m.evaluate(-1.25), 1.0);
        // One above moves the top and leaves the bottom where it was.
        m.extend(3.0, 3.0);
        assert_eq!((m.lo, m.hi), (-1.25, 3.0));
        assert_eq!((m.f_lo, m.f_hi), (0.5, 1.5), "the shoulders were set for this range");
        // A box reaches both ways at once, and every step so far only grew the range.
        m.extend(-2.0, 3.5);
        assert_eq!((m.lo, m.hi), (-2.0, 3.5));
        for (lo, hi) in [(-9.0, -9.0), (0.0, 0.0), (9.0, 9.0), (-3.0, 4.0)] {
            let before = m;
            m.extend(lo, hi);
            assert!(m.lo <= before.lo && m.hi >= before.hi, "narrowed by {lo}..{hi}");
        }
        assert!(m.is_open(), "widened right off both ends of the ruler");
        // Extending a range that was off switches it on: it was asked to select something.
        let mut off = ZoneMask { enabled: false, ..start };
        off.extend(2.0, 2.0);
        assert!(off.enabled && off.hi == 2.0);
    }

    #[test]
    fn extending_an_open_range_leaves_it_open() {
        // Open at both ends already includes every tone, so there is no end to move
        // outward, and narrowing it to the tone clicked would be the one thing extend
        // must never do. Whether the range was never set or was widened to everything
        // cannot be told apart, and need not be.
        let mut m = ZoneMask::default();
        m.extend(2.0, 2.0);
        assert!(m.is_open() && m.enabled);
        // Open at one end only is a range like any other.
        let mut shadows = ZoneMask {
            enabled: true,
            hi: -2.0,
            ..Default::default()
        };
        shadows.extend(-4.0, -4.0);
        assert_eq!((shadows.lo, shadows.hi), (ZoneMask::MIN_EV, -2.0));
        shadows.extend(0.0, 0.0);
        assert_eq!((shadows.lo, shadows.hi), (ZoneMask::MIN_EV, 0.0));
    }

    #[test]
    fn seeds_are_placed_added_and_taken_away_in_order() {
        let mut island = Island::default();
        assert!(!island.is_live() && island.seeds().is_empty());
        // A plain click starts again from one seed; a shift-click adds.
        island.set((0.25, 0.5));
        assert!(island.add((0.5, 0.5)));
        assert!(island.add((0.75, 0.25)));
        assert_eq!(island.seeds(), &[(0.25, 0.5), (0.5, 0.5), (0.75, 0.25)]);
        assert!(!island.is_live(), "seeds, but not switched on");
        island.enabled = true;
        assert!(island.is_live());
        // The middle one taken away closes the gap and keeps the order.
        island.remove(1);
        assert_eq!(island.seeds(), &[(0.25, 0.5), (0.75, 0.25)]);
        island.remove(7);
        assert_eq!(island.seeds().len(), 2, "no such seed, nothing removed");
        // Two islands with the same seeds are equal whatever was removed on the way.
        let mut direct = Island::new(true);
        direct.add((0.25, 0.5));
        direct.add((0.75, 0.25));
        assert_eq!(island, direct);
        // A click starts again; clearing keeps the switch.
        island.set((0.5, 0.125));
        assert_eq!(island.seeds(), &[(0.5, 0.125)]);
        island.clear();
        assert!(island.seeds().is_empty() && island.enabled && !island.is_live());
    }

    #[test]
    fn an_island_holds_a_fixed_few_seeds_and_only_points_of_the_negative() {
        let mut island = Island::new(true);
        for i in 0..Island::MAX {
            assert!(island.add((i as f32 / Island::MAX as f32, 0.5)), "seed {i}");
        }
        assert!(!island.add((0.5, 0.5)), "one more than it holds");
        assert_eq!(island.seeds().len(), Island::MAX);
        let mut island = Island::new(true);
        for off in [(-0.1, 0.5), (0.5, 1.2), (f32::NAN, 0.5), (0.5, f32::INFINITY)] {
            assert!(!island.add(off), "{off:?} is not on the negative");
        }
        assert!(island.seeds().is_empty());
        assert!(island.add((0.0, 1.0)), "the corners are");
        // A click that missed the picture does not throw the selection away.
        assert!(!island.set((1.5, 0.5)));
        assert_eq!(island.seeds(), &[(0.0, 1.0)]);
        assert!(island.set((0.5, 0.5)) && island.enabled);
        assert_eq!(island.seeds(), &[(0.5, 0.5)]);
    }

    #[test]
    fn an_island_is_not_in_a_tones_own_answer_and_inv_still_is() {
        // One EV cannot say whether a point is connected to a seed, so the per-tone
        // answer is the range's, island or not; `select` is that answer before `Inv`.
        let mut m = ZoneMask {
            enabled: true,
            lo: 1.0,
            ..Default::default()
        };
        let plain = m;
        m.island.enabled = true;
        m.island.add((0.5, 0.5));
        for ev in [-2.0, 0.5, 1.0, 3.0] {
            assert_eq!(m.evaluate(ev), plain.evaluate(ev));
            assert_eq!(m.select(ev), plain.evaluate(ev));
        }
        m.invert = true;
        assert_eq!(m.select(3.0), 1.0, "before Inv");
        assert_eq!(m.evaluate(3.0), 0.0, "after it");
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
