//! The tone curve: monotone cubic interpolation in a log2-EV domain, baked to a
//! LUT for the GPU.
//!
//! Three decisions from the handoff are implemented here and should not be
//! relitigated casually:
//!
//! - **The curve operates in a log domain**, not scene-linear. A curve editor on
//!   scene-linear data crushes the interesting range into the bottom few percent of
//!   the horizontal axis, and log handles above-1.0 headroom gracefully where linear
//!   has nowhere to put it. It is also darkroom-adjacent: the horizontal axis is
//!   stops, the vertical is density.
//! - **Monotone cubic (Fritsch-Carlson)**, not Catmull-Rom. Catmull-Rom can dip
//!   below its control points and produce tonal inversions that look like bugs.
//!   Fritsch-Carlson provably cannot overshoot; `no_overshoot_on_a_step` pins that.
//! - **Baked to a LUT on the CPU**, so editing the curve costs a small buffer
//!   upload per drag rather than a shader recompile.
//!
//! The curve is scene-referred in and scene-referred out. It reshapes the negative;
//! it does not fit the scene to the display. That is the display transform's job
//! (AgX), which is why AgX is not a curve preset -- see `raw-gpu`'s display pass.
//!
//! # Masks
//!
//! A pass can be limited to part of the picture. Where `m(p)` in `[0, 1]` is the
//! mask at a point of the negative, the pass becomes
//!
//! ```text
//! y = x + (f(x) - x) * opacity * m(p)
//! ```
//!
//! so the mask scales how far the pass moves a tone and never what it moves it to.
//! A mask is up to two parts -- a **shape** (a gradient laid on the negative) and a
//! **tone** (a range of tones) -- joined by a [`Rule`]. Either part alone is a whole
//! mask. The shape is Dodge & Burn's own [`Shape`] and the tone its [`ZoneMask`], so
//! the handles, the weight arithmetic and the tonal range are the same code in both
//! modules and cannot drift apart.
//!
//! [`CurveStack::apply_at`] is the reference the shader must agree with, pixel for
//! pixel. Everything that has to know where a pixel is goes through it; the
//! position-free [`CurveStack::bake`] ignores masks, because a table has no position
//! to read one at.

use crate::dodgeburn::{Radial, Shape, ZoneMask};

/// Bottom of the curve's log2 window, in stops relative to scene 1.0.
///
/// Ten stops below saturation covers the usable shadow range of every sensor in
/// `docs/corpus.md` with room to spare; below this the signal is read noise.
pub const LO_EV: f32 = -10.0;

/// Top of the curve's log2 window. Scene values reach `gains.headroom()`, measured
/// at 3.52 on the Leica M10-R (1.81 EV), and scene-linear processing can carry
/// slightly past it -- so +2 EV (4.0 linear) covers the real range with margin.
pub const HI_EV: f32 = 2.0;

/// Entries in the baked LUT. Matches the prototype's 65536.
///
/// Unlike the prototype this is uploaded as a **storage buffer**, not a 1D texture:
/// WebGPU's `maxTextureDimension1D` is 8192 by default, so a 65536-entry texture is
/// not portable. A 256 KB buffer with a manual lerp in the shader has neither that
/// limit nor the `float32-filterable` feature requirement.
pub const LUT_SIZE: usize = 65536;

/// Control points in normalised curve space: x is position in the log2 window,
/// y is output position in the same window. Both in `[0, 1]`.
///
/// Default is two points at the corners, which is the identity: a single Hermite
/// segment with both tangents at 1.0 reduces exactly to `y = x`.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    points: Vec<[f32; 2]>,
    /// This curve instance's bypass — whether this pass runs at all.
    ///
    /// Distinct from the curve *being* the identity, and the distinction is the
    /// whole point of the control: a bypassed curve keeps its shape so that
    /// switching it back on returns to the edit rather than to a straight line.
    /// That is what makes the dot an A/B against a real alternative instead of a
    /// destructive reset.
    ///
    /// It lives with the points because each named pass has its own A/B. The whole
    /// Curve module has a second switch on [`CurveStack`].
    pub enabled: bool,
}

/// One named pass in the Develop Curve module.
///
/// The name is part of the edit because it survives a reload; selection is not and
/// lives on the app tab. Each pass keeps its own bypass so a complicated stack can
/// be compared one contribution at a time without destroying any points.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveInstance {
    pub name: String,
    pub curve: Curve,
    /// Blend between the tone entering this pass and the pass's curved result.
    /// Stored per instance so a stack can be balanced without flattening its
    /// individual curve shapes.
    pub opacity: f32,
    /// Where on the negative this pass applies. `None` is everywhere.
    pub mask: Option<CurveMask>,
}

/// How a mask joins its shape and its tone, when it has both.
///
/// With `S` the shape's weight and `T` the tone's at a point, the three rules are the
/// three ways two soft selections combine, and the names are the darkroom's and
/// TK9's: a sky grad *intersected* with the bright tones brightens only the bright
/// part of the sky; *subtracting* the tone darkens the sky and spares the brightest
/// clouds; *adding* applies the pass to the sky or to anything in the shadows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rule {
    /// `S * T`: only where both select.
    #[default]
    Intersect,
    /// `S * (1 - T)`: the shape, except where the tone selects.
    Subtract,
    /// `S + T - S * T`: where either selects.
    Add,
}

impl Rule {
    pub const ALL: [Self; 3] = [Self::Intersect, Self::Subtract, Self::Add];

    /// What the panel calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Intersect => "Intersect",
            Self::Subtract => "Subtract",
            Self::Add => "Add",
        }
    }

    /// The word the sidecar stores. **A wire format**: changing one invalidates every
    /// file on disk that uses it, which is why [`label`](Self::label) is separate.
    pub fn key(self) -> &'static str {
        match self {
            Self::Intersect => "intersect",
            Self::Subtract => "subtract",
            Self::Add => "add",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|rule| rule.key() == key)
    }

    /// The two weights joined, in `[0, 1]`.
    pub fn join(self, shape: f32, tone: f32) -> f32 {
        match self {
            Self::Intersect => shape * tone,
            Self::Subtract => shape * (1.0 - tone),
            Self::Add => shape + tone - shape * tone,
        }
        .clamp(0.0, 1.0)
    }
}

/// The part of the picture one curve pass applies to.
///
/// **A mask with neither part masks nothing**, and so does a switched-off one: both
/// answer [`is_live`](Self::is_live) with no, and a pass whose mask is not live is
/// exactly an unmasked pass, byte for byte. The switch keeps the shape and the range,
/// so turning it back on returns to the edit rather than to a blank.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveMask {
    /// The mask's own bypass, as an instance's `enabled` is its curve's.
    pub enabled: bool,
    /// A gradient laid on the negative, or strokes painted on it. Dodge & Burn's
    /// [`Shape`], with what a mask does not use left unused: a gradient's `ev` is
    /// written as zero because the curve says how far to move a tone, a radial's
    /// `invert` is ignored in favor of [`invert`](Self::invert), so a mask has one way
    /// to be turned inside out, and a dab's `ev` says only whether its pass paints or
    /// erases.
    pub shape: Option<Shape>,
    /// The shape's invert: what it selected, it spares, and the reverse.
    pub invert: bool,
    /// A range of tones, read from the picture. Its own `invert` is the tone's `Inv`.
    ///
    /// **Its `enabled` is the part's own switch**, Dodge & Burn's meaning of it: off keeps
    /// the range and applies nothing, so switching the tone off and on returns to the edit
    /// and not to an open ruler. The mask's `enabled` above turns the whole mask off.
    pub tone: Option<ZoneMask>,
    pub rule: Rule,
}

impl CurveMask {
    /// A live mask limited to `shape`.
    pub fn of_shape(shape: Shape) -> Self {
        Self {
            enabled: true,
            shape: Some(shape),
            invert: false,
            tone: None,
            rule: Rule::default(),
        }
    }

    /// The part of the picture a Dodge & Burn layer is limited to, as a mask: its
    /// gradient, when it is one, and its tone mask, which a layer always intersects.
    ///
    /// **For looking at, not for applying.** It is what lets the mask view draw a layer
    /// with the code it draws a Curve mask with, so the two modules show a mask the same
    /// way. A brushed layer's strokes are not a shape a mask can hold, so for one this
    /// is its tone mask alone: where painting can reach, not where it has been.
    ///
    /// A radial's own invert, its Vignette, becomes the mask's, since a mask has one
    /// way to be turned inside out.
    pub fn of_layer(layer: &crate::dodgeburn::Instance) -> Self {
        let (shape, invert) = match &layer.shape {
            Shape::Linear(linear) => (Some(Shape::Linear(*linear)), false),
            Shape::Radial(radial) => (Some(Shape::Radial(*radial)), radial.invert),
            Shape::Brush { .. } => (None, false),
        };
        Self {
            enabled: true,
            shape,
            invert,
            tone: Some(layer.mask),
            rule: Rule::Intersect,
        }
    }

    /// The tone part, when it is there **and switched on**.
    pub fn live_tone(&self) -> Option<&ZoneMask> {
        self.tone.as_ref().filter(|tone| tone.enabled)
    }

    /// Whether this mask limits anything: switched on, and with something to limit by.
    pub fn is_live(&self) -> bool {
        self.enabled && (self.shape.is_some() || self.live_tone().is_some())
    }

    /// The shape's own coverage of a point of the negative, in `[0, 1]`, **before the
    /// invert**: a gradient's weight, or what has been painted there. `x` and `y` are
    /// source-normalized and `aspect` is the stored image's height over width, as
    /// everywhere in Dodge & Burn. `None` with no shape.
    ///
    /// A painted shape covers what has been painted ([`Shape::painted_at`]), so one
    /// with no strokes yet covers nothing: the pass is painted in, as a Dodge & Burn
    /// layer is.
    pub fn shape_coverage(&self, x: f32, y: f32, aspect: f32) -> Option<f32> {
        Some(match self.shape.as_ref()? {
            Shape::Linear(l) => l.weight_at(x, y, aspect),
            // The radial's own invert is not this mask's: see `shape`.
            Shape::Radial(r) => Radial {
                invert: false,
                ..*r
            }
            .weight_at(x, y, aspect),
            brush @ Shape::Brush { .. } => brush.painted_at(x, y, aspect),
        })
    }

    /// The shape's weight at a point, with the invert applied: what the shape
    /// contributes to the mask there. `None` with no shape.
    pub fn shape_weight(&self, x: f32, y: f32, aspect: f32) -> Option<f32> {
        self.shape_coverage(x, y, aspect)
            .map(|covered| if self.invert { 1.0 - covered } else { covered })
    }

    /// The tone's weight for a pixel whose basis reads `ev` (stops against middle
    /// gray), or `None` with no tone that is switched on.
    ///
    /// **`ev` is the raw basis, read per pixel**, which is what the trapezoid does to it.
    /// The shader reads the same trapezoid *prepared* at proxy resolution, regionally when
    /// the tone is edge-aware and diffused when it has a spacer, so the two agree where
    /// the tone is neither: the reference for a tone that is both is the prepared mask.
    pub fn tone_weight(&self, ev: f32) -> Option<f32> {
        self.live_tone().map(|tone| tone.evaluate(ev))
    }

    /// `m(p)`: how much of the pass lands at this point, in `[0, 1]`.
    pub fn weight_at(&self, x: f32, y: f32, aspect: f32, tone_ev: f32) -> f32 {
        self.weight_with(x, y, aspect, &|tone| tone.evaluate(tone_ev))
    }

    /// [`weight_at`](Self::weight_at) with the tone's weight **supplied by the caller**,
    /// who is asked for it only when there is a tone switched on.
    ///
    /// `weight_at` puts the raw basis through the trapezoid, which is the reference for a
    /// tone that is neither edge-aware nor diffused. A caller holding the picture's basis
    /// can do better and hand back the prepared mask's own value at the point
    /// ([`Basis::weight_at`](crate::Basis::weight_at)), which is what the shader reads.
    pub fn weight_with(
        &self,
        x: f32,
        y: f32,
        aspect: f32,
        tone: &dyn Fn(&ZoneMask) -> f32,
    ) -> f32 {
        self.weigh(self.shape_coverage(x, y, aspect), self.live_tone().map(tone))
    }

    /// `m(p)` from its two parts, **each supplied by the caller**: the shape's coverage
    /// before the invert, and the tone's weight. `None` is a part the mask does not
    /// have, or whose tone is switched off.
    ///
    /// For a caller that has a part by other means than asking for it at a point. The
    /// curve editor's histogram has a painted shape as a picture
    /// ([`Coverage`](crate::dodgeburn::Coverage)) and a tone as a prepared mask, because
    /// it weighs tens of thousands of samples and neither can be afforded one sample at
    /// a time.
    pub fn weigh(&self, covered: Option<f32>, tone: Option<f32>) -> f32 {
        let shape = covered.map(|covered| if self.invert { 1.0 - covered } else { covered });
        match (shape, tone) {
            (Some(shape), Some(tone)) => self.rule.join(shape, tone),
            (Some(weight), None) | (None, Some(weight)) => weight,
            (None, None) => 1.0,
        }
        .clamp(0.0, 1.0)
    }
}

/// One stretch of a masked stack, ready for the GPU.
///
/// A masked stack becomes a short list of these, in order. A run of passes that apply
/// everywhere composes into **one** table, so an unmasked pass costs nothing extra,
/// and each masked pass gets a table of its own. Every table maps normalized curve
/// space to normalized curve space, `LUT_SIZE` entries, with opacity already in it:
/// the shader blends a pixel toward the table's answer by the mask and never
/// multiplies by opacity itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The instance whose mask limits this stretch, or `None` for a run that applies
    /// everywhere.
    pub masked: Option<usize>,
    pub table: Vec<f32>,
}

impl CurveInstance {
    pub fn new(name: String) -> Self {
        Self {
            name,
            curve: Curve::default(),
            opacity: 1.0,
            mask: None,
        }
    }

    /// Whether this pass changes anything: drawn, switched on, and not at zero.
    fn runs(&self) -> bool {
        self.curve.is_active() && self.opacity > 0.0
    }

    /// The mask that limits this pass, if it has one that is on.
    pub fn live_mask(&self) -> Option<&CurveMask> {
        self.mask.as_ref().filter(|mask| mask.is_live())
    }

    /// The pass at `x`, asking everything afresh: whether it runs, and its spline. The
    /// slow way, kept as the reference the bake's tests hold the fast way to.
    #[cfg(test)]
    fn apply_normalized(&self, x: f32) -> f32 {
        if !self.runs() {
            return x;
        }
        self.apply_running(&self.curve.tangents(), x)
    }

    /// The pass at `x`, **for a pass already known to run**: what a bake calls, having
    /// asked [`runs`](Self::runs) once for the pass and not once per entry. `runs` ends
    /// in `Curve::is_identity`, which builds a default curve to compare against, and
    /// asking it 65,536 times a pass was most of what was left of a bake's cost.
    fn apply_running(&self, tangents: &[f32], x: f32) -> f32 {
        let amount = self.opacity.clamp(0.0, 1.0);
        x + (self.curve.eval_with(tangents, x) - x) * amount
    }
}

/// The passes before a point of the stack, **each with its spline derived once**, for
/// applying to many tones: [`CurveStack::passes_before`].
///
/// Applying the stack to one tone asks each pass whether it runs and derives its
/// tangents, which is two allocations a pass. That is nothing for the click of a
/// sampler and was most of the cost of the curve editor's histogram, which asks about
/// two hundred thousand tones every time a point moves. This asks once and keeps the
/// answers, as [`CurveStack::bake`] does for a table. The arithmetic is the same, so a
/// tone through here and a tone through [`CurveStack::apply_before`] are equal to the
/// bit: that function is this one, made and used once.
pub struct Passes<'a> {
    /// The passes that run, in order: where each is in the stack, and its tangents.
    running: Vec<(usize, &'a CurveInstance, Vec<f32>)>,
}

impl Passes<'_> {
    /// A scene-linear tone through the passes **with every mask ignored**, each pass at
    /// full weight: what [`CurveStack::apply_before`] gives.
    ///
    /// Pass by pass in scene values, so a tone beneath the curve's window scales to
    /// meet each pass's own floor in turn.
    pub fn apply(&self, mut scene: f32) -> f32 {
        let span = HI_EV - LO_EV;
        for (_, pass, tangents) in &self.running {
            scene = if scene <= 0.0 || scene.log2() <= LO_EV {
                let at_floor = (LO_EV + pass.apply_running(tangents, 0.0) * span).exp2();
                scene * at_floor / LO_EV.exp2()
            } else {
                let t = Curve::to_normalized(scene);
                (LO_EV + pass.apply_running(tangents, t) * span).exp2()
            };
        }
        scene
    }

    /// How far the passes move a tone, in EV, with every mask ignored: the base-2 log of
    /// [`apply`](Self::apply)'s answer over the tone. `None` for a tone that has no EV,
    /// which is one that is not positive.
    ///
    /// For one pass and a tone not above the curve's window, this is read off in the
    /// curve's own space, where the pass's change *is* a difference in EV, and the tone
    /// is never raised to a power and logged back. That is the curve editor's Average,
    /// taken over every sample each time a point moves.
    pub fn change_ev(&self, scene: f32) -> Option<f32> {
        if !(scene > 0.0 && scene.is_finite()) {
            return None;
        }
        let ev = scene.log2();
        let span = HI_EV - LO_EV;
        if let [(_, pass, tangents)] = self.running.as_slice()
            && ev <= HI_EV
        {
            // Beneath the window the tone scales to meet the floor, which is the
            // change at `t = 0`: the clamp says exactly that.
            let t = ((ev - LO_EV) / span).clamp(0.0, 1.0);
            return Some((pass.apply_running(tangents, t) - t) * span);
        }
        let moved = self.apply(scene);
        (moved > 0.0 && moved.is_finite()).then(|| moved.log2() - ev)
    }

    /// A scene-linear tone through the passes **at one pixel**, each masked pass by how
    /// much of it reaches there, the way the shader does it: what
    /// [`CurveStack::apply_before_at`] gives, and its arguments.
    pub fn apply_at(
        &self,
        scene: f32,
        x: f32,
        y: f32,
        aspect: f32,
        tone: &dyn Fn(&ZoneMask) -> f32,
    ) -> f32 {
        self.apply_weighted(scene, &|_, mask| mask.weight_with(x, y, aspect, tone))
    }

    /// [`apply_at`](Self::apply_at) with each masked pass's weight **supplied by the
    /// caller**, who is told which pass of the stack is asking and handed its mask.
    ///
    /// For a caller that weighs masks by other means than a point at a time; see
    /// [`CurveMask::weigh`].
    ///
    /// **Below the window each pixel scales to meet its own floor**: the passes
    /// composed from `t = 0` with that pixel's masks. For an unmasked stack that is
    /// the one slope the table's first entry gives, as `apply_linear` has it.
    pub fn apply_weighted(&self, scene: f32, weight: &dyn Fn(usize, &CurveMask) -> f32) -> f32 {
        // Nothing acts, and a tone through no pass is the tone: not its log and back.
        if self.running.is_empty() {
            return scene;
        }
        // In normalized curve space: each masked pass blends from the tone entering it
        // toward its own answer by its mask's weight here.
        let through = |start: f32| {
            let mut t = start;
            for (index, pass, tangents) in &self.running {
                let to = pass.apply_running(tangents, t);
                let m = pass.live_mask().map_or(1.0, |mask| weight(*index, mask));
                // Exactly the pass at full weight: `t + (to - t)` is not always `to`,
                // and an unmasked pass must not drift from the table that bakes it.
                t = if m >= 1.0 { to } else { t + (to - t) * m };
            }
            t
        };
        let span = HI_EV - LO_EV;
        if scene <= 0.0 || scene.log2() <= LO_EV {
            let floor = (LO_EV + through(0.0) * span).exp2();
            return scene * floor / LO_EV.exp2();
        }
        (LO_EV + through(Curve::to_normalized(scene)) * span).exp2()
    }
}

/// The ordered Curve module: each enabled instance receives the previous one's
/// scene-linear output.
///
/// Rendering still costs one lookup. [`Self::bake`] composes the instances into the
/// same LUT the original single curve used, so adding organisational passes does not
/// add GPU nodes or dispatches.
#[derive(Debug, Clone, PartialEq)]
pub struct CurveStack {
    /// Whole-module bypass. Instance bypasses remain on [`Curve::enabled`].
    pub enabled: bool,
    pub instances: Vec<CurveInstance>,
}

impl Default for CurveStack {
    fn default() -> Self {
        Self {
            enabled: true,
            instances: vec![CurveInstance::new("Curve 1".into())],
        }
    }
}

impl From<Curve> for CurveStack {
    fn from(curve: Curve) -> Self {
        Self {
            enabled: true,
            instances: vec![CurveInstance {
                name: "Curve 1".into(),
                curve,
                opacity: 1.0,
                mask: None,
            }],
        }
    }
}

impl CurveStack {
    pub const MAX_INSTANCES: usize = 16;

    /// Whether every stored shape is linear. Bypasses and names deliberately do not
    /// enter this answer: this is the old `Curve::is_identity` question used by the
    /// pipeline readout, not the UI's "has this module been edited?" question.
    pub fn is_identity(&self) -> bool {
        self.instances.iter().all(|i| i.curve.is_identity())
    }

    pub fn is_modified(&self) -> bool {
        !self.is_identity()
    }

    /// Equality of the tables this stack bakes: its curves, its opacities, and which
    /// passes are masked. Where a mask lies is not in the answer, so dragging a mask's
    /// handle re-uploads a few bytes and never re-bakes a table.
    pub fn same_luts(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.instances.len() == other.instances.len()
            && self.instances.iter().zip(&other.instances).all(|(a, b)| {
                a.curve == b.curve
                    && a.opacity == b.opacity
                    && a.live_mask().is_some() == b.live_mask().is_some()
            })
    }

    /// Equality of the pixels this stack asks for. Names are authored state for the
    /// sidecar and history, but renaming a pass must not upload a LUT or dispatch the
    /// viewport. A mask that is off is a mask that is not there.
    pub fn same_render(&self, other: &Self) -> bool {
        self.same_luts(other)
            && self
                .instances
                .iter()
                .zip(&other.instances)
                .all(|(a, b)| a.live_mask() == b.live_mask())
    }

    /// Whether any pass that runs is limited to part of the picture, and the render
    /// therefore needs [`segments`](Self::segments) and each pixel's position.
    pub fn has_masks(&self) -> bool {
        self.instances
            .iter()
            .any(|i| i.runs() && i.live_mask().is_some())
    }

    /// The tone ranges the render has to prepare, one per pass that runs, is masked and has
    /// a tone switched on, **in pass order**. The zone worker turns each into a mask of the
    /// picture, and a segment finds its own by [`tone_index`](Self::tone_index).
    ///
    /// A range that is fully open is still listed: it selects everything, which is not the
    /// same as having no tone (Subtract it from a shape and nothing is left), so the shader
    /// has to be told it is there even though the worker has nothing to prepare for it.
    pub fn tone_masks(&self) -> Vec<ZoneMask> {
        // A bypassed module prepares nothing, as it bakes nothing.
        if !self.enabled {
            return Vec::new();
        }
        self.instances
            .iter()
            .filter(|i| i.runs())
            .filter_map(|i| i.live_mask()?.live_tone().copied())
            .collect()
    }

    /// Where pass `pass`'s tone range sits in [`tone_masks`](Self::tone_masks), if it has
    /// one switched on.
    pub fn tone_index(&self, pass: usize) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        self.instances
            .iter()
            .enumerate()
            .filter(|(_, i)| i.runs() && i.live_mask().is_some_and(|m| m.live_tone().is_some()))
            .position(|(k, _)| k == pass)
    }

    /// Whether any tone range needs a mask prepared from the picture: one that is on and
    /// not fully open. **A stack for which this is false must never cost a basis build**,
    /// which is `docs/zone-preparation-performance.md`'s rule and why it is asked before
    /// the worker is.
    pub fn needs_zones(&self) -> bool {
        self.tone_masks().iter().any(|tone| !tone.is_identity())
    }

    /// Equality of the tone ranges the render prepares. A mask's shape moving does not
    /// change them, so dragging a handle never asks the worker for anything.
    pub fn same_tone(&self, other: &Self) -> bool {
        self.tone_masks() == other.tone_masks()
    }

    pub fn is_active(&self) -> bool {
        self.enabled
            && self
                .instances
                .iter()
                .any(|i| i.curve.is_active() && i.opacity > 0.0)
    }

    pub fn add_instance(&mut self) -> Option<usize> {
        if self.instances.len() >= Self::MAX_INSTANCES {
            return None;
        }
        let mut number = self.instances.len() + 1;
        loop {
            let name = format!("Curve {number}");
            if self.instances.iter().all(|i| i.name != name) {
                self.instances.push(CurveInstance::new(name));
                return Some(self.instances.len() - 1);
            }
            number += 1;
        }
    }

    /// Compatibility and convenience for callers editing the original/first pass.
    /// New UI code names its instance explicitly; tests and older non-UI consumers
    /// that say simply "the curve" continue to mean Curve 1.
    pub fn add(&mut self, x: f32, y: f32) -> usize {
        self.instances[0].curve.add(x, y)
    }

    pub fn points(&self) -> &[[f32; 2]] {
        self.instances[0].curve.points()
    }

    pub fn remove(&mut self, i: usize) {
        self.instances[0].curve.remove(i);
    }

    pub fn eval(&self, x: f32) -> f32 {
        self.instances[0].curve.eval(x)
    }

    /// Apply the enabled instances before `stop`, used by the image sampler to find
    /// the input coordinate of the selected pass. The whole-module bypass is ignored:
    /// this asks about the stored stack being edited, including while it is previewed
    /// off, and the sampler arms the module when it commits the point.
    pub fn apply_before(&self, stop: usize, scene: f32) -> f32 {
        self.passes_before(stop).apply(scene)
    }

    /// The passes before `stop`, prepared to be applied to many tones. For one tone,
    /// [`apply_before`](Self::apply_before) and [`apply_before_at`](Self::apply_before_at)
    /// are this, made and used once. The module's bypass is ignored, as it is there.
    pub fn passes_before(&self, stop: usize) -> Passes<'_> {
        Passes {
            running: self
                .instances
                .iter()
                .enumerate()
                .take(stop)
                .filter(|(_, instance)| instance.runs())
                .map(|(index, instance)| (index, instance, instance.curve.tangents()))
                .collect(),
        }
    }

    /// Compose the ordered stack to scene-linear LUT entries. Disabled instances
    /// are skipped; a bypassed module produces the identity table even when called
    /// without first passing through `Params::effective`.
    ///
    /// **Masks are ignored**, every pass at full weight. A table is a function of the
    /// tone alone and a mask is a function of where the pixel is, so this is the stack
    /// as though nothing were masked: exact for a stack with none, and for the CPU
    /// estimates that read it (the histogram's chain, the curve editor's ghost) the
    /// stack at its strongest. [`segments`](Self::segments) is the whole stack, for
    /// the GPU, and [`apply_at`](Self::apply_at) for a pixel whose position is known.
    pub fn bake(&self) -> Vec<f32> {
        let span = HI_EV - LO_EV;
        // Which passes run, and their tangents, are decided once and not once per
        // entry: see `Curve::tangents` and `CurveInstance::apply_running`.
        let passes: Vec<(&CurveInstance, Vec<f32>)> = self
            .instances
            .iter()
            .filter(|instance| self.enabled && instance.runs())
            .map(|instance| (instance, instance.curve.tangents()))
            .collect();
        (0..LUT_SIZE)
            .map(|i| {
                let x = i as f32 / (LUT_SIZE - 1) as f32;
                // Both axes share the same log-EV normalization, so one pass's y is
                // exactly the next pass's x. Keeping the composition here avoids
                // exp2/log2 pairs and preserves the original single-curve LUT bit for
                // bit.
                let y = passes
                    .iter()
                    .fold(x, |y, (instance, tangents)| instance.apply_running(tangents, y));
                (LO_EV + y * span).exp2()
            })
            .collect()
    }

    /// How the passes that run fall into segments: for each, the pass that masks it
    /// (if one does) and the passes composed into it. Empty when there is no mask to
    /// walk, so the shader's original single-table code runs and [`bake`](Self::bake)
    /// is the whole answer.
    ///
    /// Passes that apply everywhere compose into one table between masked ones, so the
    /// order of the stack is kept and an unmasked run costs one lookup.
    fn layout(&self) -> Vec<(Option<usize>, Vec<usize>)> {
        if !self.enabled || !self.has_masks() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        for (i, instance) in self.instances.iter().enumerate() {
            if !instance.runs() {
                continue;
            }
            if instance.live_mask().is_some() {
                if !run.is_empty() {
                    out.push((None, std::mem::take(&mut run)));
                }
                out.push((Some(i), vec![i]));
            } else {
                run.push(i);
            }
        }
        if !run.is_empty() {
            out.push((None, run));
        }
        out
    }

    /// Which pass masks each segment, in order, without baking a table.
    ///
    /// What the GPU needs when a mask moves: the tables are the same and only the
    /// shapes read from them have changed, so it re-sends this and not
    /// [`segments`](Self::segments).
    pub fn segment_masks(&self) -> Vec<Option<usize>> {
        self.layout().into_iter().map(|(masked, _)| masked).collect()
    }

    /// The stack as the stretches the shader walks, tables and all, or nothing when it
    /// has no mask to walk.
    pub fn segments(&self) -> Vec<Segment> {
        self.layout()
            .into_iter()
            .map(|(masked, passes)| {
                // Derived once per pass, not once per entry: see `Curve::tangents`.
                // `layout` only lists passes that run, so the applier can skip the check.
                let running: Vec<(&CurveInstance, Vec<f32>)> = passes
                    .iter()
                    .map(|&pass| (&self.instances[pass], self.instances[pass].curve.tangents()))
                    .collect();
                Segment {
                    masked,
                    table: (0..LUT_SIZE)
                        .map(|i| {
                            let x = i as f32 / (LUT_SIZE - 1) as f32;
                            running.iter().fold(x, |y, (instance, tangents)| {
                                instance.apply_running(tangents, y)
                            })
                        })
                        .collect(),
                }
            })
            .collect()
    }

    /// The whole stack at one pixel: the CPU reference the shader must agree with.
    ///
    /// `x` and `y` are the pixel's source-normalized position, `aspect` the stored
    /// image's height over width, and `tone_ev` the basis value there in stops against
    /// middle gray, which only a mask with a tone part reads. A bypassed module is the
    /// identity, as in [`bake`](Self::bake).
    pub fn apply_at(&self, scene: f32, x: f32, y: f32, aspect: f32, tone_ev: f32) -> f32 {
        if !self.enabled {
            return scene;
        }
        self.passes_before(self.instances.len())
            .apply_at(scene, x, y, aspect, &|tone| tone.evaluate(tone_ev))
    }

    /// [`apply_at`](Self::apply_at) through the passes before `stop`, which is where
    /// the point sampler asks what tone enters the selected pass at the pixel that was
    /// clicked. The module's bypass is ignored, as in [`apply_before`](Self::apply_before).
    ///
    /// `tone` is asked for a tone range's weight at that pixel, once for each masked
    /// pass above `stop` that has one switched on. The sampler answers from the picture's
    /// basis, so a range that is edge-aware or diffused weighs what the render gives it;
    /// see [`CurveMask::weight_with`].
    pub fn apply_before_at(
        &self,
        stop: usize,
        scene: f32,
        x: f32,
        y: f32,
        aspect: f32,
        tone: &dyn Fn(&ZoneMask) -> f32,
    ) -> f32 {
        self.passes_before(stop).apply_at(scene, x, y, aspect, tone)
    }

    /// Apply a LUT baked by this stack, including the shader's linear continuation
    /// beneath the curve window.
    pub fn apply_linear(&self, lut: &[f32], v: f32) -> f32 {
        Curve::apply_lut(lut, v)
    }
}

impl Default for Curve {
    fn default() -> Self {
        Self {
            points: vec![[0.0, 0.0], [1.0, 1.0]],
            enabled: true,
        }
    }
}

impl Curve {
    pub fn points(&self) -> &[[f32; 2]] {
        &self.points
    }

    /// True when the curve is the untouched identity, so the whole module can be
    /// skipped rather than run as a near-no-op. "The module is a no-op until
    /// touched" is then literally true, not true to within LUT quantisation.
    ///
    /// **Compares the points only.** A bypassed curve is not an untouched one —
    /// this is the question "has the user drawn anything", which is what the
    /// modified dot reports and what the panel's `linear` marker means. Whether the
    /// module runs is `is_active`.
    pub fn is_identity(&self) -> bool {
        self.points == Self::default().points
    }

    /// Whether this curve changes anything: switched on, and actually drawn.
    ///
    /// The graph consults this to decide whether to emit a curve node at all, so a
    /// bypassed curve costs nothing rather than baking a LUT that is the identity.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.is_identity()
    }

    /// Insert a point, keeping the list sorted by x. Returns its index.
    ///
    /// Points are never allowed to share an x: a zero-width interval makes the
    /// secant slope infinite and the spline undefined.
    pub fn add(&mut self, x: f32, y: f32) -> usize {
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        const MIN_DX: f32 = 1.0e-3;
        if let Some(i) = self.points.iter().position(|p| (p[0] - x).abs() < MIN_DX) {
            self.points[i][1] = y;
            return i;
        }
        let i = self.points.partition_point(|p| p[0] < x);
        self.points.insert(i, [x, y]);
        i
    }

    /// Move point `i`. The endpoints are pinned in x so the window cannot collapse;
    /// interior points are fenced between their neighbours for the same reason.
    pub fn move_point(&mut self, i: usize, x: f32, y: f32) {
        const MIN_DX: f32 = 1.0e-3;
        let n = self.points.len();
        if i >= n {
            return;
        }
        let y = y.clamp(0.0, 1.0);
        if i == 0 || i == n - 1 {
            self.points[i][1] = y;
            return;
        }
        let lo = self.points[i - 1][0] + MIN_DX;
        let hi = self.points[i + 1][0] - MIN_DX;
        self.points[i] = [x.clamp(lo, hi), y];
    }

    /// Remove an interior point. The two endpoints are permanent -- without them
    /// the window has no defined ends.
    pub fn remove(&mut self, i: usize) {
        if i > 0 && i + 1 < self.points.len() {
            self.points.remove(i);
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Rebuild a curve from stored control points. `None` if there are not at
    /// least the two endpoints.
    ///
    /// **Deliberately routed through `add` and `move_point` rather than assigning
    /// the vector.** The sidecar is a text file that a user may hand-edit and that
    /// may arrive truncated or from a future version, so its contents are input,
    /// not state. Going in through the same API the editor uses means the
    /// invariants `eval` relies on — endpoints pinned at x = 0 and 1, interior
    /// points sorted and never sharing an x — hold by construction instead of by
    /// the file being well-formed. A curve that cannot be represented is clamped
    /// into one that can, not rejected: losing one control point is better than
    /// losing the edit.
    pub fn from_points(pts: &[[f32; 2]]) -> Option<Self> {
        if pts.len() < 2 {
            return None;
        }
        let mut c = Self::default();
        // Endpoints carry only y; their x is structural.
        c.move_point(0, 0.0, pts[0][1]);
        c.move_point(1, 1.0, pts[pts.len() - 1][1]);
        for p in &pts[1..pts.len() - 1] {
            c.add(p[0], p[1]);
        }
        Some(c)
    }

    /// The Fritsch-Carlson tangents, one per point: everything `eval` needs from the
    /// curve that does not depend on `x`.
    ///
    /// Split out so a bake derives them **once** and not once per entry. `eval` used to
    /// rebuild them, allocating twice, on every call, and a bake calls it `LUT_SIZE`
    /// times per curve; a stack of masked passes bakes a table for each, and dragging a
    /// curve point re-bakes them all. The arithmetic is unchanged, so the answers are
    /// bit for bit what they were.
    fn tangents(&self) -> Vec<f32> {
        let p = &self.points;
        let n = p.len();
        if n < 2 {
            return Vec::new();
        }

        // Secant slopes.
        let mut delta = Vec::with_capacity(n - 1);
        for k in 0..n - 1 {
            let dx = p[k + 1][0] - p[k][0];
            delta.push(if dx > 0.0 {
                (p[k + 1][1] - p[k][1]) / dx
            } else {
                0.0
            });
        }

        // Initial tangents: average of adjacent secants, one-sided at the ends.
        let mut m = Vec::with_capacity(n);
        m.push(delta[0]);
        for k in 1..n - 1 {
            m.push((delta[k - 1] + delta[k]) * 0.5);
        }
        m.push(delta[n - 2]);

        // Fritsch-Carlson limiting.
        for k in 0..n - 1 {
            if delta[k] == 0.0 {
                // A flat segment must stay flat, or the cubic bulges off it.
                m[k] = 0.0;
                m[k + 1] = 0.0;
                continue;
            }
            let alpha = m[k] / delta[k];
            let beta = m[k + 1] / delta[k];
            // Negative alpha/beta means the tangent disagrees in sign with the
            // secant, which is an inversion in the making; clamp it out.
            if alpha < 0.0 {
                m[k] = 0.0;
            }
            if beta < 0.0 {
                m[k + 1] = 0.0;
            }
            let s = alpha * alpha + beta * beta;
            if s > 9.0 {
                let tau = 3.0 / s.sqrt();
                m[k] = tau * alpha * delta[k];
                m[k + 1] = tau * beta * delta[k];
            }
        }
        m
    }

    /// Evaluate in normalized curve space using Fritsch-Carlson monotone cubic
    /// Hermite interpolation.
    ///
    /// The tangent limiting step is what guarantees monotonicity: wherever the
    /// initial (three-point-difference) tangents would let the cubic overshoot,
    /// they are scaled back onto the circle of radius 3 in (alpha, beta) space. A
    /// monotone input therefore cannot produce a non-monotone output, which for a
    /// tone curve means no tonal inversions.
    pub fn eval(&self, x: f32) -> f32 {
        self.eval_with(&self.tangents(), x)
    }

    /// [`eval`](Self::eval) with the tangents already in hand, from
    /// [`tangents`](Self::tangents) on this same curve.
    fn eval_with(&self, m: &[f32], x: f32) -> f32 {
        let p = &self.points;
        let n = p.len();
        if n == 0 {
            return x;
        }
        if n == 1 {
            return p[0][1];
        }
        let x = x.clamp(0.0, 1.0);

        // Locate the segment and evaluate the Hermite basis.
        if x <= p[0][0] {
            return p[0][1];
        }
        if x >= p[n - 1][0] {
            return p[n - 1][1];
        }
        let k = p
            .partition_point(|q| q[0] <= x)
            .saturating_sub(1)
            .min(n - 2);
        let h = p[k + 1][0] - p[k][0];
        if h <= 0.0 {
            return p[k][1];
        }
        let t = (x - p[k][0]) / h;
        let t2 = t * t;
        let t3 = t2 * t;
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        (h00 * p[k][1] + h10 * h * m[k] + h01 * p[k + 1][1] + h11 * h * m[k + 1]).clamp(0.0, 1.0)
    }

    /// Bake to `LUT_SIZE` entries of **scene-linear output**, indexed by normalised
    /// log2 position.
    ///
    /// Storing linear output rather than log output keeps the shader to one lerp and
    /// an exp-free path. The shader converts its input to a normalised log position,
    /// samples here, and is done.
    pub fn bake(&self) -> Vec<f32> {
        let span = HI_EV - LO_EV;
        let m = self.tangents();
        (0..LUT_SIZE)
            .map(|i| {
                let x = i as f32 / (LUT_SIZE - 1) as f32;
                let y = self.eval_with(&m, x);
                (LO_EV + y * span).exp2()
            })
            .collect()
    }

    /// Apply this curve directly. This is used for composing a stack while it is
    /// baked and for the one-shot point sampler; the GPU continues to use the LUT.
    pub fn apply(&self, v: f32) -> f32 {
        if !self.enabled {
            return v;
        }
        if v <= 0.0 {
            let at_floor = (LO_EV + self.eval(0.0) * (HI_EV - LO_EV)).exp2();
            return v * at_floor / LO_EV.exp2();
        }
        let ev = v.log2();
        if ev <= LO_EV {
            let at_floor = (LO_EV + self.eval(0.0) * (HI_EV - LO_EV)).exp2();
            return v * at_floor / LO_EV.exp2();
        }
        let y = self.eval(Self::to_normalized(v));
        (LO_EV + y * (HI_EV - LO_EV)).exp2()
    }

    /// Scene-linear value -> normalised log2 position. The inverse of what `bake`
    /// assumes, and the same mapping the curve editor's x axis uses.
    pub fn to_normalized(v: f32) -> f32 {
        ((v.max(1.0e-12).log2() - LO_EV) / (HI_EV - LO_EV)).clamp(0.0, 1.0)
    }

    /// Evaluate the curve on a scene-linear value the way the shader does, including
    /// the below-window behaviour. Used by the histogram so it tracks the viewport.
    ///
    /// Below `LO_EV` the LUT has no entries, so the shader scales linearly by the
    /// slope needed to meet the curve at the bottom of the window. For the identity
    /// curve that factor is exactly 1.0, so black stays black rather than being
    /// lifted to `2^LO_EV`.
    pub fn apply_linear(&self, lut: &[f32], v: f32) -> f32 {
        Self::apply_lut(lut, v)
    }

    fn apply_lut(lut: &[f32], v: f32) -> f32 {
        if v <= 0.0 {
            // Sub-zero values are sensor noise below the black point and are carried
            // through unrectified; see `SceneImage`.
            return v * Self::below_window_slope(lut);
        }
        let ev = v.log2();
        if ev <= LO_EV {
            return v * Self::below_window_slope(lut);
        }
        let x = ((ev - LO_EV) / (HI_EV - LO_EV)).clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32;
        let i = (x.floor() as usize).min(LUT_SIZE - 2);
        let f = x - i as f32;
        lut[i] * (1.0 - f) + lut[i + 1] * f
    }

    fn below_window_slope(lut: &[f32]) -> f32 {
        lut.first().copied().unwrap_or(0.0) / LO_EV.exp2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_stack_is_applied_in_order_and_baked_once() {
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.35, 0.55);
        let second = stack.add_instance().expect("room for a second curve");
        stack.instances[second].curve.add(0.7, 0.58);

        let scene = 0.18;
        let after_first = stack.instances[0].curve.apply(scene);
        let expected = stack.instances[1].curve.apply(after_first);
        let lut = stack.bake();
        let baked = stack.apply_linear(&lut, scene);
        assert!((baked - expected).abs() < 2.0e-5, "{baked} != {expected}");

        let mut reversed = stack.clone();
        reversed.instances.reverse();
        let other = reversed.apply_linear(&reversed.bake(), scene);
        assert!((other - baked).abs() > 1.0e-4, "order did not matter");
    }

    #[test]
    fn bypassing_one_instance_keeps_its_points_and_skips_its_effect() {
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.5, 0.7);
        let saved = stack.instances[0].curve.points().to_vec();
        stack.instances[0].curve.enabled = false;
        let v = 0.18;
        assert!((stack.apply_linear(&stack.bake(), v) - v).abs() < 1.0e-6);
        assert_eq!(stack.instances[0].curve.points(), saved);
    }

    #[test]
    fn instance_opacity_blends_in_curve_ev_space() {
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.5, 0.7);
        stack.instances[0].opacity = 0.5;

        // At x=.5 the curve asks for y=.7. Half opacity therefore lands at .6
        // in the editor's shared log-EV coordinate system.
        let scene = (LO_EV + 0.5 * (HI_EV - LO_EV)).exp2();
        let expected = (LO_EV + 0.6 * (HI_EV - LO_EV)).exp2();
        let actual = stack.apply_linear(&stack.bake(), scene);
        assert!((actual - expected).abs() < 2.0e-5, "{actual} != {expected}");

        stack.instances[0].opacity = 0.0;
        assert!((stack.apply_linear(&stack.bake(), scene) - scene).abs() < 1.0e-6);
    }

    #[test]
    fn automatic_instance_names_do_not_reuse_a_renamed_name() {
        let mut stack = CurveStack::default();
        stack.instances[0].name = "Curve 2".into();
        let i = stack.add_instance().expect("room");
        assert_eq!(stack.instances[i].name, "Curve 3");
    }

    #[test]
    fn default_is_identity() {
        let c = Curve::default();
        assert!(c.is_identity());
        for i in 0..=20 {
            let x = i as f32 / 20.0;
            assert!(
                (c.eval(x) - x).abs() < 1.0e-6,
                "identity failed at {x}: {}",
                c.eval(x)
            );
        }
    }

    #[test]
    fn identity_lut_round_trips_scene_values() {
        // The load-bearing property: an untouched curve must return the scene value
        // it was given, across the whole range including above 1.0.
        let c = Curve::default();
        let lut = c.bake();
        for v in [0.001f32, 0.01, 0.18, 0.5, 1.0, 2.0, 3.52] {
            let out = c.apply_linear(&lut, v);
            let err = (out - v).abs() / v;
            assert!(
                err < 1.0e-3,
                "identity shifted {v} to {out} (rel err {err})"
            );
        }
    }

    #[test]
    fn identity_keeps_black_at_black() {
        // Below the log window the LUT has no entries. A naive implementation
        // returns 2^LO_EV there and lifts the shadow floor; this pins that it does
        // not.
        let c = Curve::default();
        let lut = c.bake();
        assert_eq!(c.apply_linear(&lut, 0.0), 0.0);
        assert!(c.apply_linear(&lut, 1.0e-6) < 1.0e-5);
    }

    #[test]
    fn sub_zero_noise_is_not_rectified() {
        // Decode deliberately leaves below-black noise negative so SuperPixel
        // averaging stays unbiased. The curve must not be the thing that rectifies
        // it.
        let c = Curve::default();
        let lut = c.bake();
        assert!(c.apply_linear(&lut, -0.002) < 0.0);
    }

    #[test]
    fn no_overshoot_on_a_step() {
        // The reason for Fritsch-Carlson over Catmull-Rom. A near-step input makes
        // Catmull-Rom dip below the lower control point and above the upper one,
        // which reads as a tonal inversion. Monotone cubic cannot.
        let mut c = Curve::default();
        c.add(0.45, 0.05);
        c.add(0.55, 0.95);
        let mut prev = -1.0;
        for i in 0..=1000 {
            let x = i as f32 / 1000.0;
            let y = c.eval(x);
            assert!((0.0..=1.0).contains(&y), "overshoot at {x}: {y}");
            assert!(y >= prev - 1.0e-6, "non-monotone at {x}: {y} after {prev}");
            prev = y;
        }
    }

    #[test]
    fn flat_segment_stays_flat() {
        // Two points at the same height must produce a genuinely flat run, not a
        // cubic that bulges off it and then comes back.
        let mut c = Curve::default();
        c.add(0.3, 0.5);
        c.add(0.7, 0.5);
        for i in 0..=40 {
            let x = 0.3 + 0.4 * i as f32 / 40.0;
            assert!(
                (c.eval(x) - 0.5).abs() < 1.0e-5,
                "not flat at {x}: {}",
                c.eval(x)
            );
        }
    }

    #[test]
    fn a_lifted_curve_raises_midtones_without_inverting() {
        let mut c = Curve::default();
        c.add(0.5, 0.65);
        let lut = c.bake();
        assert!(!c.is_identity());
        // Monotone in the linear domain too.
        let mut prev = f32::NEG_INFINITY;
        for i in 0..200 {
            let v = (i as f32 / 200.0 * 12.0 - 10.0).exp2();
            let out = c.apply_linear(&lut, v);
            assert!(out >= prev - 1.0e-6, "linear-domain inversion at {v}");
            prev = out;
        }
        // The midpoint of the window is 2^-4; lifting its output must brighten it.
        let mid = (-4.0f32).exp2();
        assert!(c.apply_linear(&lut, mid) > mid);
    }

    #[test]
    fn points_cannot_stack_on_one_x() {
        // A zero-width interval makes the secant slope infinite.
        let mut c = Curve::default();
        c.add(0.5, 0.5);
        c.add(0.5001, 0.9);
        let xs: Vec<f32> = c.points().iter().map(|p| p[0]).collect();
        for w in xs.windows(2) {
            assert!(w[1] > w[0], "duplicate x: {xs:?}");
        }
    }

    // ── Masks ────────────────────────────────────────────────────────────────

    use crate::dodgeburn::{Linear, Radial, Shape, ZoneMask};

    /// A sky grad: full at the top edge, gone by the middle.
    fn sky_grad() -> Shape {
        Shape::Linear(Linear {
            x0: 0.5,
            y0: 0.0,
            x1: 0.5,
            y1: 0.5,
            feather: 1.0,
            ev: 0.0,
        })
    }

    fn spot() -> Shape {
        Shape::Radial(Radial {
            cx: 0.4,
            cy: 0.6,
            inner: 0.05,
            outer: 0.3,
            aspect: 1.6,
            angle: 25.0,
            feather: 0.7,
            invert: false,
            ev: 0.0,
        })
    }

    /// A stack of `n` lifted passes, each with its own shape, so no two are equal.
    fn lifted_stack(n: usize) -> CurveStack {
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.35, 0.5);
        for k in 1..n {
            let i = stack.add_instance().expect("room");
            stack.instances[i].curve.add(0.3 + 0.1 * k as f32, 0.45 + 0.05 * k as f32);
        }
        stack
    }

    /// What the shader does, from the segments alone: each segment's table in turn,
    /// blended by that segment's mask. If this agrees with `apply_at`, the table design
    /// is sound before any GPU is involved.
    fn walk(stack: &CurveStack, scene: f32, x: f32, y: f32, aspect: f32) -> f32 {
        let span = HI_EV - LO_EV;
        let segments = stack.segments();
        let table = |segment: &Segment, t: f32| {
            let at = t.clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32;
            let i = (at.floor() as usize).min(LUT_SIZE - 2);
            let f = at - i as f32;
            segment.table[i] * (1.0 - f) + segment.table[i + 1] * f
        };
        let through = |start: f32| {
            segments.iter().fold(start, |t, segment| {
                let to = table(segment, t);
                let m = segment
                    .masked
                    .and_then(|i| stack.instances[i].live_mask())
                    .map_or(1.0, |mask| mask.weight_at(x, y, aspect, 0.0));
                if m >= 1.0 { to } else { t + (to - t) * m }
            })
        };
        if scene <= 0.0 || scene.log2() <= LO_EV {
            let floor = (LO_EV + through(0.0) * span).exp2();
            return scene * floor / LO_EV.exp2();
        }
        (LO_EV + through(Curve::to_normalized(scene)) * span).exp2()
    }

    #[test]
    fn rules_join_two_weights_as_documented() {
        let (s, t) = (0.8, 0.25);
        assert!((Rule::Intersect.join(s, t) - 0.2).abs() < 1.0e-6);
        assert!((Rule::Subtract.join(s, t) - 0.6).abs() < 1.0e-6);
        assert!((Rule::Add.join(s, t) - 0.85).abs() < 1.0e-6);
        // The ends every rule agrees on: no shape and no tone, or both full.
        for rule in Rule::ALL {
            assert_eq!(rule.join(0.0, 0.0).min(0.0), 0.0, "{rule:?}");
            assert!((rule.join(1.0, 0.0) - f32::from(rule != Rule::Intersect)).abs() < 1.0e-6);
        }
        for rule in Rule::ALL {
            assert_eq!(Rule::from_key(rule.key()), Some(rule));
        }
        assert_eq!(Rule::from_key("xor"), None);
    }

    #[test]
    fn a_mask_with_nothing_in_it_or_switched_off_is_not_live() {
        let mut mask = CurveMask::of_shape(sky_grad());
        assert!(mask.is_live());
        mask.enabled = false;
        assert!(!mask.is_live(), "a switched-off mask limits nothing");
        mask.enabled = true;
        mask.shape = None;
        assert!(!mask.is_live(), "a mask with neither part limits nothing");
        assert_eq!(mask.weight_at(0.3, 0.3, 0.7, 0.0), 1.0);
        // A tone that is switched off is not a part: only an enabled one limits anything.
        mask.tone = Some(ZoneMask::default());
        assert!(!mask.is_live(), "a tone that is off limits nothing");
        mask.tone = Some(ZoneMask {
            enabled: true,
            ..ZoneMask::default()
        });
        assert!(mask.is_live(), "a tone alone is a whole mask");
    }

    #[test]
    fn a_shape_is_full_where_it_starts_and_gone_where_it_ends() {
        let mut mask = CurveMask::of_shape(sky_grad());
        let at = |mask: &CurveMask, y: f32| mask.weight_at(0.5, y, 0.667, 0.0);
        assert!((at(&mask, 0.0) - 1.0).abs() < 1.0e-6);
        assert!((at(&mask, 0.25) - 0.5).abs() < 1.0e-6);
        assert!(at(&mask, 0.5).abs() < 1.0e-6);
        assert!(at(&mask, 0.9).abs() < 1.0e-6, "and it stays gone past the end");
        mask.invert = true;
        assert!(at(&mask, 0.0).abs() < 1.0e-6, "inverted, the sky is spared");
        assert!((at(&mask, 0.9) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn a_radials_own_invert_is_not_the_masks() {
        // The mask has one way to be turned inside out, so a `GradInvert` that a
        // hand-edited file carried cannot invert the shape a second time.
        let mut shape = spot();
        let plain = CurveMask::of_shape(shape.clone());
        if let Shape::Radial(r) = &mut shape {
            r.invert = true;
        }
        let stale = CurveMask::of_shape(shape);
        for (x, y) in [(0.4, 0.6), (0.1, 0.1), (0.9, 0.4)] {
            assert_eq!(
                plain.weight_at(x, y, 0.667, 0.0),
                stale.weight_at(x, y, 0.667, 0.0)
            );
        }
        assert!((plain.weight_at(0.4, 0.6, 0.667, 0.0) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn an_unmasked_stack_has_no_segments_and_bakes_as_it_did() {
        let mut stack = lifted_stack(2);
        assert!(!stack.has_masks());
        assert!(stack.segments().is_empty());
        let before = stack.bake();

        // A mask that is off, or on an idle pass, is no mask at all.
        let mut off = CurveMask::of_shape(sky_grad());
        off.enabled = false;
        stack.instances[1].mask = Some(off);
        assert!(!stack.has_masks());
        assert!(stack.segments().is_empty());
        assert_eq!(stack.bake(), before);

        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        stack.instances[1].opacity = 0.0;
        assert!(!stack.has_masks(), "a pass that does nothing needs no mask");
    }

    #[test]
    fn bake_ignores_masks() {
        let mut stack = lifted_stack(2);
        let unmasked = stack.bake();
        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        assert_eq!(stack.bake(), unmasked);
    }

    #[test]
    fn a_masked_pass_moves_a_tone_by_the_mask_and_no_more() {
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.5, 0.7);
        stack.instances[0].opacity = 0.8;
        stack.instances[0].mask = Some(CurveMask::of_shape(sky_grad()));
        let aspect = 0.667;
        let scene = (LO_EV + 0.5 * (HI_EV - LO_EV)).exp2();
        let plain = {
            let mut p = stack.clone();
            p.instances[0].mask = None;
            p.apply_at(scene, 0.5, 0.0, aspect, 0.0)
        };

        // Where the mask is one the pass lands in full, opacity and all.
        let full = stack.apply_at(scene, 0.5, 0.0, aspect, 0.0);
        assert!((full - plain).abs() < 1.0e-6, "{full} != {plain}");
        // Where it is gone the tone is untouched.
        let gone = stack.apply_at(scene, 0.5, 0.5, aspect, 0.0);
        assert!((gone - scene).abs() < 1.0e-6, "{gone} != {scene}");
        // Halfway, the pass moves the tone half as far in the curve's own space:
        // y = x + (f(x) - x) * opacity * m, with opacity inside the table.
        let half = stack.apply_at(scene, 0.5, 0.25, aspect, 0.0);
        let (x, y) = (0.5, 0.5 + 0.8 * (stack.instances[0].curve.eval(0.5) - 0.5));
        let want = (LO_EV + (x + (y - x) * 0.5) * (HI_EV - LO_EV)).exp2();
        assert!((half - want).abs() < 1.0e-6, "{half} != {want}");
    }

    #[test]
    fn an_unmasked_run_composes_into_one_table_and_the_order_is_kept() {
        let mut stack = lifted_stack(4);
        stack.instances[2].mask = Some(CurveMask::of_shape(sky_grad()));
        let segments = stack.segments();
        assert_eq!(
            segments.iter().map(|s| s.masked).collect::<Vec<_>>(),
            vec![None, Some(2), None],
            "passes 0 and 1 are one run, 2 is masked alone, 3 follows"
        );
        assert!(segments.iter().all(|s| s.table.len() == LUT_SIZE));

        stack.instances[0].mask = Some(CurveMask::of_shape(sky_grad()));
        stack.instances[3].mask = Some(CurveMask::of_shape(spot()));
        let masked: Vec<_> = stack.segments().iter().map(|s| s.masked).collect();
        assert_eq!(masked, vec![Some(0), None, Some(2), Some(3)]);
    }

    #[test]
    fn baking_once_gives_the_answers_evaluating_each_entry_gives() {
        // The tangents are derived once per bake and not once per entry. That is a pure
        // saving only if every entry comes out as it would have by calling `eval`, so
        // this holds the bake to the slow, obviously-right route on an awkward curve:
        // a steep step, a flat run, and a pass at partial opacity.
        let mut stack = CurveStack::default();
        stack.instances[0].curve.add(0.45, 0.05);
        stack.instances[0].curve.add(0.55, 0.95);
        stack.instances[0].curve.add(0.7, 0.95);
        let second = stack.add_instance().expect("room");
        stack.instances[second].curve.add(0.3, 0.6);
        stack.instances[second].opacity = 0.6;
        let span = HI_EV - LO_EV;
        let baked = stack.bake();
        for i in (0..LUT_SIZE).step_by(257) {
            let x = i as f32 / (LUT_SIZE - 1) as f32;
            let slow = stack
                .instances
                .iter()
                .fold(x, |y, pass| pass.apply_normalized(y));
            let want = (LO_EV + slow * span).exp2();
            assert_eq!(baked[i], want, "entry {i}");
        }
        let single = stack.instances[0].curve.bake();
        for i in (0..LUT_SIZE).step_by(257) {
            let x = i as f32 / (LUT_SIZE - 1) as f32;
            let want = (LO_EV + stack.instances[0].curve.eval(x) * span).exp2();
            assert_eq!(single[i], want, "entry {i}");
        }
    }

    // ── Tone ─────────────────────────────────────────────────────────────────

    fn highlights() -> ZoneMask {
        ZoneMask {
            enabled: true,
            lo: 1.0,
            hi: 5.0,
            f_lo: 1.0,
            f_hi: 1.0,
            ..ZoneMask::default()
        }
    }

    fn tone_only() -> CurveMask {
        CurveMask {
            enabled: true,
            shape: None,
            invert: false,
            tone: Some(highlights()),
            rule: Rule::Intersect,
        }
    }

    #[test]
    fn a_tone_part_that_is_off_is_kept_and_applies_nothing() {
        let mut mask = tone_only();
        assert!(mask.is_live());
        assert!(mask.tone_weight(2.0).is_some());
        if let Some(tone) = &mut mask.tone {
            tone.enabled = false;
        }
        assert!(!mask.is_live(), "a mask with only a tone that is off limits nothing");
        assert!(mask.tone_weight(2.0).is_none());
        assert!(mask.tone.is_some(), "the range is kept for when it comes back");
        // And beside a shape it is simply not there.
        mask.shape = Some(sky_grad());
        assert!(mask.is_live());
        assert_eq!(
            mask.weight_at(0.5, 0.0, 0.667, 2.0),
            CurveMask::of_shape(sky_grad()).weight_at(0.5, 0.0, 0.667, 2.0)
        );
    }

    #[test]
    fn a_tone_alone_selects_by_brightness_and_not_by_place() {
        let mask = tone_only();
        // Bright tones select, dark ones do not, wherever the pixel is.
        for (x, y) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.9)] {
            assert!((mask.weight_at(x, y, 0.667, 3.0) - 1.0).abs() < 1.0e-6);
            assert!(mask.weight_at(x, y, 0.667, -3.0).abs() < 1.0e-6);
        }
        // A tone's own Inv turns it over.
        let mut inverted = mask.clone();
        if let Some(tone) = &mut inverted.tone {
            tone.invert = true;
        }
        assert!(inverted.weight_at(0.5, 0.5, 0.667, 3.0).abs() < 1.0e-6);
        assert!((inverted.weight_at(0.5, 0.5, 0.667, -3.0) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn the_rules_join_a_shape_and_a_tone_as_the_table_says() {
        // Sky grad half-way through, tone fully on: S = 0.5, T = 1.
        // Tone fully off: T = 0.
        let at_half = (0.5, 0.25, 0.667);
        for (rule, bright, dark) in [
            (Rule::Intersect, 0.5, 0.0),
            (Rule::Subtract, 0.0, 0.5),
            (Rule::Add, 1.0, 0.5),
        ] {
            let mask = CurveMask {
                shape: Some(sky_grad()),
                rule,
                ..tone_only()
            };
            let w = |ev| mask.weight_at(at_half.0, at_half.1, at_half.2, ev);
            assert!((w(4.0) - bright).abs() < 1.0e-5, "{rule:?} where the tone selects");
            assert!((w(-4.0) - dark).abs() < 1.0e-5, "{rule:?} where it does not");
        }
    }

    #[test]
    fn a_fully_open_tone_still_counts_as_there() {
        // Open selects everything, which is not the same as having no tone: Subtracted
        // from a shape it leaves nothing. The shader has to be told it exists.
        let mut mask = CurveMask::of_shape(sky_grad());
        mask.tone = Some(ZoneMask {
            enabled: true,
            ..ZoneMask::default()
        });
        mask.rule = Rule::Subtract;
        assert!(mask.live_tone().is_some());
        assert_eq!(mask.weight_at(0.5, 0.0, 0.667, 0.0), 0.0);
        mask.rule = Rule::Intersect;
        assert!((mask.weight_at(0.5, 0.0, 0.667, 0.0) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn the_tones_to_prepare_follow_the_passes_that_run_in_order() {
        let mut stack = lifted_stack(4);
        stack.instances[0].mask = Some(tone_only());
        stack.instances[2].mask = Some(CurveMask {
            tone: Some(ZoneMask {
                lo: -3.0,
                hi: 1.0,
                ..highlights()
            }),
            ..CurveMask::of_shape(sky_grad())
        });
        stack.instances[3].mask = Some(CurveMask::of_shape(spot()));
        let tones = stack.tone_masks();
        assert_eq!(tones.len(), 2);
        assert_eq!(stack.tone_index(0), Some(0));
        assert_eq!(stack.tone_index(1), None, "an unmasked pass has no tone");
        assert_eq!(stack.tone_index(2), Some(1));
        assert_eq!(stack.tone_index(3), None, "a shape alone has no tone");
        assert_eq!(tones[1].lo, -3.0);

        // A pass that does nothing prepares nothing, and neither does one switched off.
        stack.instances[0].opacity = 0.0;
        assert_eq!(stack.tone_masks().len(), 1);
        assert_eq!(stack.tone_index(2), Some(0), "the index follows the survivors");
    }

    #[test]
    fn only_a_tone_that_needs_the_picture_asks_for_it() {
        // **A stack with no tone range must never cost a basis build.**
        let mut stack = lifted_stack(2);
        assert!(!stack.needs_zones());
        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        assert!(!stack.needs_zones(), "a shape reads nothing from the picture");
        stack.instances[1].mask = Some(tone_only());
        assert!(stack.needs_zones());
        // Fully open selects everything and has nothing to prepare.
        stack.instances[1].mask = Some(CurveMask {
            tone: Some(ZoneMask {
                enabled: true,
                ..ZoneMask::default()
            }),
            ..tone_only()
        });
        assert!(!stack.needs_zones());
        // Off, or on a bypassed module, asks for nothing.
        stack.instances[1].mask = Some(tone_only());
        stack.enabled = false;
        assert!(!stack.needs_zones());
    }

    #[test]
    fn moving_a_shape_never_changes_the_tones_to_prepare() {
        let mut a = lifted_stack(2);
        a.instances[1].mask = Some(CurveMask {
            shape: Some(sky_grad()),
            ..tone_only()
        });
        let mut b = a.clone();
        if let Some(mask) = &mut b.instances[1].mask {
            mask.shape = Some(spot());
        }
        assert!(a.same_tone(&b), "a handle moved, the worker has nothing to do");
        if let Some(tone) = b.instances[1].mask.as_mut().and_then(|m| m.tone.as_mut()) {
            tone.lo = 0.0;
        }
        assert!(!a.same_tone(&b));
    }

    #[test]
    fn the_layout_without_tables_is_the_layout_with_them() {
        let mut stack = lifted_stack(4);
        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        stack.instances[3].mask = Some(CurveMask::of_shape(spot()));
        let cheap = stack.segment_masks();
        let baked: Vec<_> = stack.segments().iter().map(|s| s.masked).collect();
        assert_eq!(cheap, baked);
        assert_eq!(cheap, vec![None, Some(1), None, Some(3)]);
    }

    #[test]
    fn walking_the_segments_agrees_with_the_reference_everywhere() {
        let mut stack = lifted_stack(3);
        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        stack.instances[1].opacity = 0.7;
        let mut turned = CurveMask::of_shape(spot());
        turned.invert = true;
        stack.instances[2].mask = Some(turned);

        let aspect = 0.667;
        let mut worst = 0.0f32;
        // Scene values across the window, above it, below it, and not positive.
        let scenes: Vec<f32> = (0..=40)
            .map(|i| (LO_EV - 1.0 + 13.0 * i as f32 / 40.0).exp2())
            .chain([0.0, -0.002])
            .collect();
        for scene in scenes {
            for &(x, y) in &[(0.5, 0.0), (0.5, 0.2), (0.4, 0.6), (0.1, 0.9), (0.8, 0.3)] {
                let want = stack.apply_at(scene, x, y, aspect, 0.0);
                let got = walk(&stack, scene, x, y, aspect);
                let scale = want.abs().max(1.0e-6);
                worst = worst.max((got - want).abs() / scale);
            }
        }
        assert!(worst < 1.0e-3, "tables disagree with the reference: {worst}");
    }

    #[test]
    fn below_the_window_each_pixel_meets_its_own_floor() {
        // A pass that lifts the floor, masked: the lift scales a dark pixel only where
        // the mask lets the pass through.
        let mut stack = CurveStack::default();
        stack.instances[0].curve.move_point(0, 0.0, 0.3);
        stack.instances[0].mask = Some(CurveMask::of_shape(sky_grad()));
        let aspect = 0.667;
        let dark = LO_EV.exp2() * 0.25;
        let inside = stack.apply_at(dark, 0.5, 0.0, aspect, 0.0);
        let outside = stack.apply_at(dark, 0.5, 0.5, aspect, 0.0);
        assert!((outside - dark).abs() < 1.0e-9, "outside the mask, untouched");
        assert!(inside > dark * 3.0, "inside it, lifted with the floor: {inside}");
        // And the unmasked answer is the table's own slope, as `apply_linear` has it.
        stack.instances[0].mask = None;
        let lut = stack.bake();
        let unmasked = stack.apply_at(dark, 0.5, 0.5, aspect, 0.0);
        assert!((unmasked - stack.apply_linear(&lut, dark)).abs() / unmasked < 1.0e-4);
    }

    #[test]
    fn dragging_a_mask_never_rebakes_a_table() {
        let mut a = lifted_stack(2);
        a.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        let mut b = a.clone();
        if let Some(mask) = &mut b.instances[1].mask {
            mask.shape = Some(spot());
        }
        assert!(a.same_luts(&b), "where a mask lies is not in a table");
        assert!(!a.same_render(&b), "but it is in the picture");

        let mut c = a.clone();
        c.instances[1].mask = None;
        assert!(!a.same_luts(&c), "a mask appearing changes how tables are cut");

        // A mask that is off equals no mask, and renaming equals nothing.
        let mut off = a.clone();
        if let Some(mask) = &mut off.instances[1].mask {
            mask.enabled = false;
        }
        assert!(off.same_render(&c) && off.same_luts(&c));
        let mut renamed = a.clone();
        renamed.instances[1].name = "Sky".into();
        assert!(a.same_render(&renamed));
    }

    #[test]
    fn the_point_sampler_reads_the_masks_where_it_was_clicked() {
        let mut stack = lifted_stack(2);
        stack.instances[0].mask = Some(CurveMask::of_shape(sky_grad()));
        let aspect = 0.667;
        let scene = 0.18;
        // A shape reads nothing from the picture, so the tone is never asked for.
        let unasked = |_: &ZoneMask| -> f32 { panic!("no pass above has a tone range") };
        let before_second = |x, y| stack.apply_before_at(1, scene, x, y, aspect, &unasked);
        // Entering the second pass, the first has acted at the top and not below.
        assert!(before_second(0.5, 0.0) > scene * 1.05);
        assert!((before_second(0.5, 0.5) - scene).abs() < 1.0e-6);
        // Before the first pass nothing has.
        assert_eq!(
            stack.apply_before_at(0, scene, 0.5, 0.0, aspect, &unasked),
            scene
        );
    }

    #[test]
    fn the_point_sampler_weighs_a_tone_range_by_what_the_caller_read_there() {
        // The first pass is limited to the highlights. What enters the second depends on
        // how much of the first reached the pixel, and that is the caller's to say: it
        // holds the basis, and the weight it reads there is the render's.
        let mut stack = lifted_stack(2);
        stack.instances[0].mask = Some(tone_only());
        let (aspect, scene) = (0.667, 0.18);
        let entering = |weight: f32| {
            stack.apply_before_at(1, scene, 0.5, 0.5, aspect, &|tone| {
                assert_eq!(*tone, highlights(), "asked about the range the pass has");
                weight
            })
        };
        let full = stack.apply_before(1, scene);
        assert!((entering(1.0) - full).abs() < 1.0e-6, "selected: the pass in full");
        assert!((entering(0.0) - scene).abs() < 1.0e-6, "not selected: untouched");
        assert!(entering(0.5) > scene && entering(0.5) < full, "and part way between");
        // It agrees with the reference where the reference is exact: a per-pixel tone.
        assert_eq!(
            stack.apply_before_at(2, scene, 0.5, 0.5, aspect, &|tone| tone.evaluate(3.0)),
            stack.apply_at(scene, 0.5, 0.5, aspect, 3.0),
        );
    }

    #[test]
    fn passes_prepared_once_give_the_tone_the_stack_gives_one_at_a_time() {
        // The curve editor's histogram puts two hundred thousand tones through passes
        // prepared once. Each must come out as the stack gives it when asked about that
        // tone alone, to the bit, in the window, above it, below it and not positive,
        // through passes that are masked, bypassed and at part opacity.
        let mut stack = lifted_stack(4);
        stack.instances[0].opacity = 0.6;
        stack.instances[1].mask = Some(CurveMask {
            shape: Some(sky_grad()),
            ..tone_only()
        });
        stack.instances[2].curve.enabled = false;
        stack.instances[3].mask = Some(CurveMask::of_shape(spot()));
        let aspect = 0.667;
        let tones: Vec<f32> = (0..=60)
            .map(|i| (LO_EV - 2.0 + 15.0 * i as f32 / 60.0).exp2())
            .chain([0.0, -0.002])
            .collect();
        for stop in 0..=stack.instances.len() {
            let passes = stack.passes_before(stop);
            for &scene in &tones {
                assert_eq!(passes.apply(scene), stack.apply_before(stop, scene));
                for &(x, y) in &[(0.5, 0.0), (0.4, 0.6), (0.9, 0.3)] {
                    let tone = |t: &ZoneMask| t.evaluate(scene.max(1e-6).log2() + 2.47);
                    assert_eq!(
                        passes.apply_at(scene, x, y, aspect, &tone),
                        stack.apply_before_at(stop, scene, x, y, aspect, &tone),
                        "stop {stop}, tone {scene}, at ({x}, {y})"
                    );
                }
            }
        }
        // Ignoring masks, a prepared pass is the pass at full weight everywhere.
        let unmasked = {
            let mut plain = stack.clone();
            plain.instances.iter_mut().for_each(|i| i.mask = None);
            plain
        };
        // The two compose differently, pass by pass in scene values against once in
        // curve space, so they agree to rounding and not to the bit.
        let passes = stack.passes_before(4);
        for &scene in &tones {
            let (got, want) = (
                passes.apply(scene),
                unmasked.apply_at(scene, 0.5, 0.5, aspect, 0.0),
            );
            assert!((got - want).abs() <= want.abs() * 1.0e-5, "{got} against {want}");
        }

        // And the change in EV is the log of what `apply` does, for one pass and many,
        // in the window, beneath it and above it; a tone with no EV has none.
        for stop in 0..=stack.instances.len() {
            let passes = stack.passes_before(stop);
            for &scene in &tones {
                let change = passes.change_ev(scene);
                if scene > 0.0 {
                    let want = passes.apply(scene).log2() - scene.log2();
                    let got = change.expect("a positive tone has an EV");
                    assert!((got - want).abs() < 2.0e-4, "stop {stop}, {scene}: {got} / {want}");
                } else {
                    assert_eq!(change, None);
                }
            }
        }
    }

    /// A hard round dab of radius 0.2 at `(x, y)`, at full opacity; `paint` is whether
    /// its pass adds to a mask or erases from it.
    fn hard_dab(x: f32, y: f32, paint: bool) -> crate::dodgeburn::Dab {
        crate::dodgeburn::Dab {
            x,
            y,
            radius: 0.2,
            feather: 0.0,
            opacity: 1.0,
            ev: if paint { 0.25 } else { -0.25 },
            aspect: 1.0,
            angle: 0.0,
            nib: crate::dodgeburn::Nib::Round,
            falloff: crate::dodgeburn::Falloff::Core,
        }
    }

    fn painted(passes: Vec<crate::dodgeburn::Gesture>) -> CurveMask {
        CurveMask::of_shape(Shape::Brush {
            nib: crate::dodgeburn::Nib::Round,
            passes,
        })
    }

    #[test]
    fn a_painted_mask_selects_what_was_painted_and_spares_what_was_erased() {
        use crate::dodgeburn::Gesture;
        // One pass across the left, one eraser pass through its middle.
        let mask = painted(vec![
            Gesture::new(vec![hard_dab(0.3, 0.5, true), hard_dab(0.4, 0.5, true)]),
            Gesture::new(vec![hard_dab(0.5, 0.5, false)]),
        ]);
        let aspect = 1.0;
        assert!(mask.is_live());
        assert_eq!(mask.weight_at(0.2, 0.5, aspect, 0.0), 1.0, "painted");
        assert_eq!(mask.weight_at(0.45, 0.5, aspect, 0.0), 0.0, "painted, then erased");
        assert_eq!(mask.weight_at(0.9, 0.5, aspect, 0.0), 0.0, "never painted");
        // How hard the brush was set does not come into it: only that it painted.
        let mut soft = mask.clone();
        if let Some(Shape::Brush { passes, .. }) = &mut soft.shape {
            passes[0].dabs_mut().iter_mut().for_each(|d| d.ev = 2.0);
        }
        assert_eq!(soft.weight_at(0.2, 0.5, aspect, 0.0), 1.0);
        // Opacity does: a dab at half opacity is half a selection.
        let mut half = mask.clone();
        if let Some(Shape::Brush { passes, .. }) = &mut half.shape {
            passes[0].dabs_mut().iter_mut().for_each(|d| d.opacity = 0.5);
        }
        assert_eq!(half.weight_at(0.2, 0.5, aspect, 0.0), 0.5);
        // Inverted, it is everywhere but the paint; and the tone joins as with a shape.
        let mut inverted = mask.clone();
        inverted.invert = true;
        assert_eq!(inverted.weight_at(0.2, 0.5, aspect, 0.0), 0.0);
        assert_eq!(inverted.weight_at(0.9, 0.5, aspect, 0.0), 1.0);
        let toned = CurveMask {
            tone: Some(highlights()),
            ..mask
        };
        assert_eq!(toned.weight_at(0.2, 0.5, aspect, 3.0), 1.0);
        assert_eq!(toned.weight_at(0.2, 0.5, aspect, -3.0), 0.0);
    }

    #[test]
    fn a_painted_mask_with_no_strokes_holds_the_pass_back_until_it_is_painted_in() {
        // The pass is painted in, as a Dodge & Burn layer is: with nothing painted the
        // mask is live and selects nothing, so the curve has no effect anywhere.
        let mut stack = lifted_stack(1);
        stack.instances[0].mask = Some(painted(Vec::new()));
        assert!(stack.has_masks(), "an empty painted mask is still a mask");
        // Untouched to within the round trip through the curve's log space.
        let untouched = |v: f32| (v - 0.18).abs() < 1.0e-6;
        for (x, y) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.9)] {
            assert!(untouched(stack.apply_at(0.18, x, y, 0.667, 0.0)));
        }
        // One dab, and the curve is there under it and nowhere else.
        stack.instances[0].mask = Some(painted(vec![crate::dodgeburn::Gesture::new(vec![
            hard_dab(0.5, 0.5, true),
        ])]));
        assert!(stack.apply_at(0.18, 0.5, 0.5, 0.667, 0.0) > 0.18 * 1.05);
        assert!(untouched(stack.apply_at(0.18, 0.05, 0.05, 0.667, 0.0)));
        // A stroke is a change to the picture and not to any table.
        let mut more = stack.clone();
        if let Some(Shape::Brush { passes, .. }) =
            more.instances[0].mask.as_mut().and_then(|m| m.shape.as_mut())
        {
            passes.push(crate::dodgeburn::Gesture::new(vec![hard_dab(0.2, 0.2, true)]));
        }
        assert!(stack.same_luts(&more) && !stack.same_render(&more));
    }

    #[test]
    fn a_layers_mask_is_its_gradient_through_its_tone_mask() {
        // What the mask view shows for a Dodge & Burn layer has to be where the layer
        // acts: its own coverage, times its tone mask. For the two gradients that is
        // the whole of it, a Vignette included.
        use crate::dodgeburn::{Instance, Linear, Sign};
        let aspect = 0.667;
        let Shape::Radial(round) = spot() else { panic!() };
        let vignette = Shape::Radial(Radial {
            invert: true,
            ..round
        });
        let grad = Shape::Linear(Linear {
            x0: 0.2,
            y0: 0.1,
            x1: 0.7,
            y1: 0.8,
            feather: 0.6,
            ev: -1.0,
        });
        for shape in [grad, spot(), vignette] {
            let mut layer = Instance::of(Sign::Burn, "Sky".into(), shape);
            layer.mask = highlights();
            let mask = CurveMask::of_layer(&layer);
            assert!(mask.is_live());
            for (x, y) in [(0.5, 0.5), (0.3, 0.2), (0.9, 0.7), (0.1, 0.9), (0.45, 0.62)] {
                for ev in [-3.0, 0.4, 1.5, 4.0] {
                    let want = layer.coverage_at(x, y, aspect) * layer.mask.evaluate(ev);
                    let got = mask.weight_at(x, y, aspect, ev);
                    assert!((got - want).abs() < 1.0e-6, "at ({x}, {y}), {ev} EV");
                }
            }
            // With its tone mask off, the gradient alone.
            layer.mask.enabled = false;
            let mask = CurveMask::of_layer(&layer);
            assert_eq!(
                mask.weight_at(0.3, 0.2, aspect, -3.0),
                layer.coverage_at(0.3, 0.2, aspect)
            );
        }

        // A brushed layer is its tone mask and nothing else, wherever it was painted.
        let mut brushed = Instance::new(Sign::Dodge, "Face".into());
        brushed.mask = highlights();
        let mask = CurveMask::of_layer(&brushed);
        assert!(mask.shape.is_none());
        assert_eq!(mask.weight_at(0.5, 0.5, aspect, 3.0), 1.0);
        assert_eq!(mask.weight_at(0.5, 0.5, aspect, -3.0), 0.0);
        // And with no tone mask either, it has nothing to limit it: one everywhere.
        brushed.mask.enabled = false;
        let open = CurveMask::of_layer(&brushed);
        assert!(!open.is_live());
        assert_eq!(open.weight_at(0.5, 0.5, aspect, -3.0), 1.0);
    }

    #[test]
    fn a_bypassed_module_is_the_identity_at_every_pixel() {
        let mut stack = lifted_stack(2);
        stack.instances[1].mask = Some(CurveMask::of_shape(sky_grad()));
        stack.enabled = false;
        assert_eq!(stack.apply_at(0.18, 0.5, 0.0, 0.667, 0.0), 0.18);
        assert!(stack.segments().is_empty());
    }

    #[test]
    fn endpoints_survive_removal() {
        let mut c = Curve::default();
        c.add(0.5, 0.7);
        c.remove(1);
        assert!(c.is_identity());
        c.remove(0);
        c.remove(1);
        assert_eq!(c.points().len(), 2, "endpoints must be permanent");
    }
}
