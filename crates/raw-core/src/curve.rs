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
}

impl CurveInstance {
    pub fn new(name: String) -> Self {
        Self {
            name,
            curve: Curve::default(),
            opacity: 1.0,
        }
    }

    fn apply_normalized(&self, x: f32) -> f32 {
        if !self.curve.is_active() || self.opacity <= 0.0 {
            return x;
        }
        let amount = self.opacity.clamp(0.0, 1.0);
        x + (self.curve.eval(x) - x) * amount
    }

    fn apply_scene(&self, scene: f32) -> f32 {
        if !self.curve.is_active() || self.opacity <= 0.0 {
            return scene;
        }
        let span = HI_EV - LO_EV;
        if scene <= 0.0 || scene.log2() <= LO_EV {
            let at_floor = (LO_EV + self.apply_normalized(0.0) * span).exp2();
            return scene * at_floor / LO_EV.exp2();
        }
        (LO_EV + self.apply_normalized(Curve::to_normalized(scene)) * span).exp2()
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

    /// Equality of the pixels this stack asks for. Names are authored state for the
    /// sidecar and history, but renaming a pass must not upload a LUT or dispatch the
    /// viewport.
    pub fn same_render(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.instances.len() == other.instances.len()
            && self
                .instances
                .iter()
                .zip(&other.instances)
                .all(|(a, b)| a.curve == b.curve && a.opacity == b.opacity)
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
    pub fn apply_before(&self, stop: usize, mut scene: f32) -> f32 {
        for instance in self.instances.iter().take(stop) {
            if instance.curve.is_active() && instance.opacity > 0.0 {
                scene = instance.apply_scene(scene);
            }
        }
        scene
    }

    /// Compose the ordered stack to scene-linear LUT entries. Disabled instances
    /// are skipped; a bypassed module produces the identity table even when called
    /// without first passing through `Params::effective`.
    pub fn bake(&self) -> Vec<f32> {
        let span = HI_EV - LO_EV;
        (0..LUT_SIZE)
            .map(|i| {
                let x = i as f32 / (LUT_SIZE - 1) as f32;
                let mut y = x;
                if self.enabled {
                    for instance in &self.instances {
                        if instance.curve.is_active() && instance.opacity > 0.0 {
                            // Both axes share the same log-EV normalisation, so one
                            // pass's y is exactly the next pass's x. Keeping the
                            // composition here avoids exp2/log2 pairs and preserves
                            // the original single-curve LUT bit for bit.
                            y = instance.apply_normalized(y);
                        }
                    }
                }
                (LO_EV + y * span).exp2()
            })
            .collect()
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

    /// Evaluate in normalised curve space using Fritsch-Carlson monotone cubic
    /// Hermite interpolation.
    ///
    /// The tangent limiting step is what guarantees monotonicity: wherever the
    /// initial (three-point-difference) tangents would let the cubic overshoot,
    /// they are scaled back onto the circle of radius 3 in (alpha, beta) space. A
    /// monotone input therefore cannot produce a non-monotone output, which for a
    /// tone curve means no tonal inversions.
    pub fn eval(&self, x: f32) -> f32 {
        let p = &self.points;
        let n = p.len();
        if n == 0 {
            return x;
        }
        if n == 1 {
            return p[0][1];
        }
        let x = x.clamp(0.0, 1.0);

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
        (0..LUT_SIZE)
            .map(|i| {
                let x = i as f32 / (LUT_SIZE - 1) as f32;
                let y = self.eval(x);
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
