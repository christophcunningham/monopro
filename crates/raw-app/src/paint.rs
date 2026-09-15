//! The Dodge & Burn brush: the tool's own settings, where dabs land, and which
//! instance a pass goes onto.
//!
//! # Why this is not in `viewport_panel`
//!
//! The same reason `crop` is not: laying dabs along a drag is arithmetic with an
//! off-by-one in it, and the modal key set is a table of ranges. Kept here both are
//! pure functions with tests that name the cases; folded into the viewport they
//! would be forty untested lines inside a function that also owns zoom, pan and the
//! GPU dispatch.
//!
//! # The brush is not in `Params`
//!
//! Radius, feather, intensity and opacity describe **the next dab**, not the
//! picture. They are not undoable, not written to the sidecar, and not copied by a
//! duplicate — they are the tool in your hand, and it does not change when you
//! change frames. The prototype reaches the same answer by keeping them on the
//! panel rather than in its `ProcessingParams`.
//!
//! One brush for the whole app rather than one per tab, for that reason: a cursor
//! is not per-document either.

use raw_core::dodgeburn::{
    Dab, DodgeBurnParams, Gesture, Instance, Linear, Nib, Radial, Shape, Sign,
};

/// Which of the three shapes the next press will make.
///
/// The prototype's `Brush | Linear | Radial` row, kept as a row: it is a *tool*
/// selector, so it belongs beside the brush controls rather than being three
/// separate buttons that each also create an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    #[default]
    Brush,
    Linear,
    Radial,
}

/// What the ADD bench offers: **one row of four shapes**, not a tool row and a nib
/// row under it.
///
/// The mockup collapses them — `Round | Card | Linear | Radial` — and it is the
/// better model. A nib was only ever a property of the brush *because* the brush is
/// the thing that has a nib, but nobody choosing what to make thinks "a brush, and
/// then which kind": they think "a round one" or "a card". Two rows made the user
/// perform a taxonomy the panel could have performed for them.
///
/// So this is the picker's vocabulary and [`Tool`] stays the interaction's. The
/// mapping between them lives here and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Round,
    Card,
    Linear,
    Radial,
}

impl Pick {
    pub const ALL: [Self; 4] = [Self::Round, Self::Card, Self::Linear, Self::Radial];

    pub fn label(self) -> &'static str {
        match self {
            Self::Round => "Round",
            Self::Card => "Card",
            Self::Linear => "Linear",
            Self::Radial => "Radial",
        }
    }

    /// The tool this picks, and the nib it implies where there is one.
    pub fn tool(self) -> Tool {
        match self {
            Self::Round | Self::Card => Tool::Brush,
            Self::Linear => Tool::Linear,
            Self::Radial => Tool::Radial,
        }
    }

    pub fn nib(self) -> Option<Nib> {
        match self {
            Self::Round => Some(Nib::Round),
            Self::Card => Some(Nib::Card),
            _ => None,
        }
    }

    /// Which entry is lit, given the tool that is up and the nib the brush carries.
    ///
    /// The brush's nib is what disambiguates the two brush entries, which is why this
    /// takes both — a tool alone cannot say whether `Round` or `Card` is current.
    pub fn of(tool: Tool, nib: Nib) -> Self {
        match (tool, nib) {
            (Tool::Brush, Nib::Round) => Self::Round,
            (Tool::Brush, Nib::Card) => Self::Card,
            (Tool::Linear, _) => Self::Linear,
            (Tool::Radial, _) => Self::Radial,
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Round => "A disc or ellipse",
            Self::Card => "A card with a straight edge",
            Self::Linear => "Feather gradient: drag from 100% to 0%.",
            Self::Radial => "Spotlight: drag from the center outwards.",
        }
    }
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Self::Brush => "Brush",
            Self::Linear => "Linear",
            Self::Radial => "Radial",
        }
    }

    pub fn is_gradient(self) -> bool {
        self != Self::Brush
    }

    /// The tool that edits this shape.
    ///
    /// **The layer decides the tool, not the other way round.** Selecting a layer
    /// arms whatever it is made of, so there is no state in which the radial tool is
    /// up and a brush layer is selected — which was reachable before and meant a
    /// press on the picture quietly started a second layer instead of adding to the
    /// one highlighted in the panel.
    pub fn of(shape: &Shape) -> Self {
        match shape {
            Shape::Brush { .. } => Self::Brush,
            Shape::Linear(_) => Self::Linear,
            Shape::Radial(_) => Self::Radial,
        }
    }

    /// A fresh shape of this kind, placed as a zero-length drag at `at`.
    ///
    /// Zero length on purpose: the press makes the shape and the drag gives it its
    /// extent, so between the two there is a real instance with no size — which
    /// `Linear::weight_at` reads as full strength everywhere and `Radial` as a
    /// point. Both are stable and neither divides by zero.
    /// `nib` is used only by [`Tool::Brush`]; a gradient has none.
    pub fn shape(self, at: (f32, f32), ev: f32, nib: Nib) -> Shape {
        match self {
            Self::Brush => Shape::Brush {
                nib,
                passes: Vec::new(),
            },
            Self::Linear => Shape::Linear(Linear {
                x0: at.0,
                y0: at.1,
                x1: at.0,
                y1: at.1,
                feather: 1.0,
                ev,
            }),
            Self::Radial => Shape::Radial(Radial {
                cx: at.0,
                cy: at.1,
                inner: 0.0,
                outer: 0.0,
                aspect: 1.0,
                angle: 0.0,
                feather: 1.0,
                invert: false,
                ev,
            }),
        }
    }

    /// The default name for an instance of this kind — `Burn 1`, `Linear Burn 2`.
    pub fn name_for(self, sign: Sign, existing: &[Instance]) -> String {
        let kind = match self {
            Self::Brush => sign.label().to_owned(),
            _ => format!("{} {}", self.label(), sign.label()),
        };
        (1..)
            .map(|n| format!("{kind} {n}"))
            .find(|n| !existing.iter().any(|i| i.name == *n))
            .expect("the naturals are not exhausted by a finite instance list")
    }
}

/// The two ends of a gradient, as the handles present them.
///
/// `From` is the filled handle — full strength for a linear, the centre for a
/// radial. `To` is the hollow one: zero strength, or a point on the outer radius.
/// The same convention as the drag that placed it, which is what makes the handles
/// legible without a legend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    From,
    To,
}

/// Where a gradient's two handles are, in source-normalised coordinates.
///
/// For a radial, `to` is the point at `outer` along the ellipse's own long axis —
/// so dragging it sets the radius *and* the rotation, and the angle control needs
/// no separate widget. At `aspect == 1.0` the rotation is invisible, which is
/// correct: a circle has no orientation.
pub fn handles(shape: &Shape, aspect: f32) -> Option<((f32, f32), (f32, f32))> {
    match shape {
        Shape::Brush { .. } => None,
        Shape::Linear(l) => Some(((l.x0, l.y0), (l.x1, l.y1))),
        Shape::Radial(r) => {
            let a = r.angle.to_radians();
            // `outer` is in width units, so the y step has to be divided back out
            // of the aspect to land in y-normalised space.
            let (dx, dy) = (r.outer * a.cos(), r.outer * a.sin() / aspect.max(1e-6));
            Some(((r.cx, r.cy), (r.cx + dx, r.cy + dy)))
        }
    }
}

/// Move one end of a gradient to `at`.
///
/// Dragging a radial's outer handle sets **both** the radius and the angle, from
/// the distance and the bearing — which is the whole of what that handle means, and
/// is why the angle slider is a refinement rather than the only way to turn one.
pub fn move_handle(shape: &mut Shape, end: End, at: (f32, f32), aspect: f32) {
    match shape {
        Shape::Brush { .. } => {}
        Shape::Linear(l) => match end {
            End::From => (l.x0, l.y0) = at,
            End::To => (l.x1, l.y1) = at,
        },
        Shape::Radial(r) => match end {
            End::From => {
                // Moving the centre carries the shape with it rather than
                // resizing it — a centre handle that also changed the radius
                // would be two controls on one grab.
                (r.cx, r.cy) = at;
            }
            End::To => {
                let (dx, dy) = (at.0 - r.cx, (at.1 - r.cy) * aspect);
                r.outer = (dx * dx + dy * dy).sqrt().max(1e-3);
                r.angle = dy.atan2(dx).to_degrees();
                // The full-strength boundary cannot sit outside the zero one.
                r.inner = r.inner.min(r.outer * 0.95);
            }
        },
    }
}

/// The tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Brush {
    /// Fraction of the frame width, like [`Dab::radius`].
    pub radius: f32,
    pub feather: f32,
    /// Round, or a card with a straight edge.
    pub nib: Nib,
    /// The nib's own height/width ratio. 1.0 is round.
    pub aspect: f32,
    /// Rotation in degrees, used when [`Brush::follow`] is off.
    pub angle: f32,
    /// **Take the angle from the direction of the drag instead.**
    ///
    /// This is what turns an elongated nib into a *nib*: narrow along the path and
    /// wide across it, so shading along a shoreline or a horizon is one gesture
    /// instead of forty. the maintainer asked for it as an option rather than a mode, which
    /// is what it is — one more control beside the angle it overrides.
    ///
    /// It changes nothing about the pass model: each dab carries its own angle and
    /// they still composite by max within a gesture, so a swept nib lays a
    /// continuous ribbon rather than a sum.
    pub follow: bool,
    /// EV per pass. The number `Intensity` shows, and it means exactly that: one
    /// click deposits this much, and a drag deposits it once however slowly the
    /// hand moves. See `raw_core::dodgeburn`.
    pub intensity: f32,
    pub opacity: f32,
}

impl Default for Brush {
    fn default() -> Self {
        // The prototype's opening values, which are a soft mid-sized wand at a
        // quarter stop — small enough that a first stroke is a suggestion rather
        // than a statement, which is the right way round for a tool you cannot see
        // the effect of until you have used it.
        Self {
            radius: 0.06,
            feather: 0.40,
            nib: Nib::Round,
            aspect: 1.0,
            angle: 0.0,
            follow: false,
            intensity: 0.25,
            opacity: 1.0,
        }
    }
}

impl Brush {
    pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 0.005..=0.5;
    pub const FEATHER_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;
    pub const INTENSITY_RANGE: std::ops::RangeInclusive<f32> = 0.01..=2.0;
    pub const OPACITY_RANGE: std::ops::RangeInclusive<f32> = 0.05..=1.0;
    pub const ASPECT_RANGE: std::ops::RangeInclusive<f32> = 0.1..=6.0;
    pub const ANGLE_RANGE: std::ops::RangeInclusive<f32> = -90.0..=90.0;

    /// One dab of this brush at `at`, for an instance of `sign`.
    ///
    /// `erasing` flips the EV against the instance, which is the whole of what an
    /// eraser is: a pass of the opposite sign, subtracting a pass locally, clamped
    /// by the instance so it can reach zero and no further.
    /// `bearing` is the direction the caller laid this dab along, in degrees. Used
    /// only when the nib is [`Brush::follow`]ing the stroke; otherwise the brush's
    /// own angle stands.
    ///
    /// **`nib` is passed in rather than read off the brush**, and it is always the
    /// *layer's*. The nib is a property of the layer now, so a dab that took the
    /// brush's own would be the one way the two could disagree — which is exactly how
    /// mixed layers used to happen. Every caller has the instance in hand, so there is
    /// nowhere this can be got wrong.
    pub fn dab(
        &self,
        at: (f32, f32),
        sign: Sign,
        erasing: bool,
        bearing: Option<f32>,
        nib: Nib,
    ) -> Dab {
        let direction = if erasing { -sign.ev() } else { sign.ev() };
        Dab {
            x: at.0,
            y: at.1,
            radius: self.radius,
            feather: self.feather,
            opacity: self.opacity,
            ev: direction * self.intensity,
            aspect: self.aspect,
            angle: bearing.filter(|_| self.follow).unwrap_or(self.angle),
            nib,
        }
    }

    /// How far apart dabs are laid along a drag, in width-normalised units.
    ///
    /// A quarter of the radius: close enough that the maxima of consecutive dabs
    /// overlap well inside the falloff, so a stroke reads as a stroke rather than
    /// as beads, and far enough apart that a slow drag across a wide brush does not
    /// deposit thousands of records to describe one line.
    ///
    /// **The narrow axis** — and the brush-shapes brief got the reason
    /// backwards, which is worth recording because the wrong reason produces the
    /// right formula and a test that cannot see the difference.
    ///
    /// The brief said "on a nib following the stroke, the dimension along the path is
    /// the narrow one". It is the *wide* one: `follow` sets the angle to the bearing,
    /// so the nib's own x-axis — half-extent `radius`, independent of aspect — lies
    /// along the path, and `aspect` widens it *across*. A following nib therefore
    /// never needs less spacing than a round one.
    ///
    /// The case that does is a **fixed** angle, where the drag can run along the
    /// nib's narrow axis: a 1:5 card held vertically and dragged sideways presents
    /// `radius * aspect` along the path, and spacing from `radius` alone leaves gaps
    /// between consecutive dabs — a stroke that beads.
    ///
    /// So the rule is right and it is conservative: at worst it lays a few more
    /// records than strictly needed for a following nib. The cheap error is the one
    /// to make.
    pub fn spacing(&self) -> f32 {
        (self.radius * self.aspect.clamp(0.01, 1.0) * 0.25).max(1e-4)
    }

    /// Move one of the four controls, from the brush actions in `hotkeys::TABLE`.
    ///
    /// **The four bracket pairs are modal**, and that is Dodge & Burn's hotkey
    /// decision. The prototype wants `[`/`]` for radius, `⌘[`/`⌘]` for intensity,
    /// `⇧[`/`⇧]` for feather and `⌥[`/`⌥]` for opacity. Two of those four could not
    /// be global: `⌘[` and `⌘]` are Rotate left and right, taken in 9a, and a rotate
    /// firing mid-stroke is the surprise the mode enum exists to prevent. So all
    /// four are modal, under one rule — *while the brush is open, the bracket keys
    /// belong to the brush* — implemented by `hotkeys::pressed` and documented by
    /// the mode's own footer line.
    ///
    /// Radius steps **multiplicatively** and the other three linearly. A radius is a
    /// size, and a fixed increment that is a sensible nudge at 30% of the frame is
    /// the entire brush at 1%.
    pub fn step(&mut self, action: crate::hotkeys::Action) {
        use crate::hotkeys::Action as A;
        let clamp = |v: f32, r: &std::ops::RangeInclusive<f32>| v.clamp(*r.start(), *r.end());
        let d = |up: bool| if up { 0.05 } else { -0.05 };
        match action {
            A::BrushRadius(up) => {
                let factor = if up { 1.2 } else { 1.0 / 1.2 };
                self.radius = clamp(self.radius * factor, &Self::RADIUS_RANGE);
            }
            A::BrushFeather(up) => self.feather = clamp(self.feather + d(up), &Self::FEATHER_RANGE),
            A::BrushIntensity(up) => {
                self.intensity = clamp(self.intensity + d(up), &Self::INTENSITY_RANGE);
            }
            A::BrushOpacity(up) => self.opacity = clamp(self.opacity + d(up), &Self::OPACITY_RANGE),
            _ => {}
        }
    }
}

/// Dab centres from `from` to `to`, at `spacing`, **excluding `from`**.
///
/// Excluding, because `from` already has a dab on it — it is where the last one
/// landed. Including it would deposit two records at the same point on every frame
/// of a drag, which the max-within-a-pass rule makes invisible and the sidecar
/// makes expensive.
///
/// Distances are measured in width-normalised units with `dy` aspect-corrected, the
/// same currency [`Dab::magnitude_at`] works in — so spacing is a real distance on
/// the negative and a stroke is not laid more thinly across a landscape frame than
/// down it.
///
/// The final point is included **only if it is at least a full spacing from the
/// last one**, so the sequence is uniform rather than ending in a stub. The
/// leftover is not lost: the caller carries `from` forward as the last dab it
/// actually placed, so the next frame continues from there.
pub fn lay(from: (f32, f32), to: (f32, f32), aspect: f32, spacing: f32) -> Vec<(f32, f32)> {
    let dx = to.0 - from.0;
    let dy = (to.1 - from.1) * aspect;
    let dist = (dx * dx + dy * dy).sqrt();
    if !dist.is_finite() || dist < spacing {
        return Vec::new();
    }
    let n = (dist / spacing).floor() as usize;
    // A pathological case rather than a real one — a spacing of 1e-4 across the
    // whole frame is ten thousand dabs — but a drag that somehow arrived with a
    // near-zero spacing must not allocate without bound.
    let n = n.min(4096);
    (1..=n)
        .map(|i| {
            let t = i as f32 * spacing / dist;
            (from.0 + dx * t, from.1 + (to.1 - from.1) * t)
        })
        .collect()
}

/// The most recent layer of this kind, which is what `d` and `x` bring forward.
///
/// **Sign only, not shape.** the maintainer's call: `d` means *dodge*, and the most recent
/// dodge is the one you were last working on whether you drew it with a brush or
/// dragged it as a gradient. The tool then follows the layer, via [`Tool::of`].
pub fn most_recent(db: &DodgeBurnParams, sign: Sign) -> Option<usize> {
    db.instances.iter().rposition(|inst| inst.sign == sign)
}

/// Which instance the next pass goes onto, creating one if there is nothing
/// suitable. Returns its index.
///
/// `active` is the panel's selection and is updated to match.
///
/// The rule, in order: keep the selection if it is already this kind, otherwise
/// take the most recent instance of this kind, otherwise make one. Which is what
/// "press `x` to burn" should do — it should not silently start a second Burn 2
/// beside the Burn 1 you were working on, and it should not paint a burn onto a
/// dodge.
pub fn instance_for(
    db: &mut DodgeBurnParams,
    active: &mut Option<usize>,
    sign: Sign,
    tool: Tool,
    nib: Nib,
    force_new: bool,
) -> Option<usize> {
    // Kind as well as sign, since 10b: pressing `x` with the radial tool selected
    // must not paint a burn onto the linear you were editing.
    //
    // **And the nib as well as the kind**, which is what stops a layer ever holding
    // two. The panel offers *one row of four shapes* — see `Pick`, which collapsed the
    // tool and the nib into one choice on the argument that nobody picking what to make
    // thinks "a brush, and separately a nib". This matched on `Tool` alone, so the code
    // deciding "does this layer suit what I picked" was still working in the three-way
    // split the UI had abandoned: choosing Card with a Round layer selected extended the
    // Round layer. the maintainer reported the model, not the bug — *"I don't think we can have
    // mixed-nib layers"* — and this is the line that makes it true.
    let suits = |inst: &Instance| {
        inst.sign == sign
            && match (&inst.shape, tool) {
                (Shape::Brush { nib: had, .. }, Tool::Brush) => *had == nib,
                (Shape::Linear(_), Tool::Linear) | (Shape::Radial(_), Tool::Radial) => true,
                _ => false,
            }
    };
    if !force_new {
        if let Some(i) = *active
            && db.instances.get(i).is_some_and(suits)
        {
            return Some(i);
        }
        if let Some(i) = db.instances.iter().rposition(suits) {
            *active = Some(i);
            return Some(i);
        }
    }
    // The cap is a real refusal, not a silent no-op: the shader binds one mask row
    // per instance and the panel is a list you have to read. Returning `None` lets
    // the caller say so in the status line.
    if db.instances.len() >= DodgeBurnParams::MAX_INSTANCES {
        return None;
    }
    let name = tool.name_for(sign, &db.instances);
    // A gradient is created empty and placed by the drag that follows; `Tool::shape`
    // makes the zero-length form the press leaves behind.
    db.instances
        .push(Instance::of(sign, name, tool.shape((0.5, 0.5), 0.0, nib)));
    let i = db.instances.len() - 1;
    *active = Some(i);
    Some(i)
}

/// Open a new pass on `inst` with one dab at `at`. A no-op on a gradient, which has
/// no passes to open.
///
/// The first dab of a following nib has no direction yet — nothing has moved — so it
/// takes the brush's own angle and the second dab onwards take the drag's.
pub fn begin(inst: &mut Instance, brush: &Brush, at: (f32, f32), erasing: bool) {
    let nib = inst.shape.nib().unwrap_or_default();
    let dab = brush.dab(at, inst.sign, erasing, None, nib);
    if let Some(g) = inst.gestures_mut() {
        g.push(Gesture::new(vec![dab]));
    }
}

/// Extend the pass in flight with every dab between `from` and `to`.
///
/// Returns the last centre actually placed — or `from` when the pointer has not yet
/// moved a full spacing — and the bearing it laid them along, for the cursor.
pub fn extend(
    inst: &mut Instance,
    brush: &Brush,
    from: (f32, f32),
    to: (f32, f32),
    aspect: f32,
    erasing: bool,
) -> ((f32, f32), Option<f32>) {
    let sign = inst.sign;
    let nib = inst.shape.nib().unwrap_or_default();
    // The bearing of the drag, in the same width-normalised space the nib is
    // measured in — so a nib following a diagonal on a wide frame lies along the
    // path as it appears, rather than as the coordinates happen to be scaled.
    let bearing = {
        let (dx, dy) = (to.0 - from.0, (to.1 - from.1) * aspect);
        (dx * dx + dy * dy > 1e-12).then(|| dy.atan2(dx).to_degrees())
    };
    let dabs: Vec<Dab> = lay(from, to, aspect, brush.spacing())
        .iter()
        .map(|at| brush.dab(*at, sign, erasing, bearing, nib))
        .collect();
    let last = dabs.last().map_or(from, |d| (d.x, d.y));
    if let Some(g) = inst.gestures_mut().and_then(|g| g.last_mut()) {
        g.dabs_mut().extend(dabs);
    }
    (last, bearing)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQUARE: f32 = 1.0;

    #[test]
    fn a_slow_drag_and_a_fast_one_deposit_the_same_dabs() {
        // The property that makes `spacing` worth having at all: dabs are a
        // function of distance, not of how many frames the drag took. One long
        // step and eight short ones over the same line must land in the same
        // places, or a stroke's density would depend on the frame rate.
        let brush = Brush {
            radius: 0.08,
            ..Default::default()
        };
        let spacing = brush.spacing();

        let fast = lay((0.1, 0.5), (0.5, 0.5), SQUARE, spacing);

        let mut slow = Vec::new();
        let mut at = (0.1f32, 0.5f32);
        for i in 1..=8 {
            let to = (0.1 + 0.05 * i as f32, 0.5);
            let placed = lay(at, to, SQUARE, spacing);
            if let Some(last) = placed.last() {
                at = *last;
            }
            slow.extend(placed);
        }

        assert_eq!(fast.len(), slow.len(), "{} vs {}", fast.len(), slow.len());
        for (a, b) in fast.iter().zip(&slow) {
            assert!((a.0 - b.0).abs() < 1e-5, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn laying_never_repeats_the_point_it_started_from() {
        // `from` already carries a dab. Emitting it again would put two records on
        // the same spot every frame of a drag — invisible, because the max rule
        // hides it, and straight into the sidecar.
        let out = lay((0.2, 0.2), (0.8, 0.2), SQUARE, 0.02);
        assert!(
            out.iter().all(|p| p.0 > 0.2),
            "the start point was re-emitted: {out:?}"
        );
        assert!((out[0].0 - 0.22).abs() < 1e-5);
    }

    #[test]
    fn a_pointer_that_has_barely_moved_lays_nothing() {
        assert!(lay((0.5, 0.5), (0.5001, 0.5), SQUARE, 0.02).is_empty());
        // And an unmoved pointer, which is every frame of a click-and-hold.
        assert!(lay((0.5, 0.5), (0.5, 0.5), SQUARE, 0.02).is_empty());
    }

    #[test]
    fn spacing_is_a_distance_on_the_negative_not_in_normalised_units() {
        // The aspect trap again, at the other end of the tool. On a 2:1 frame a
        // vertical drag of 0.4 covers 0.2 of the WIDTH, so it must lay half as many
        // dabs as a horizontal drag of 0.4 does. Without the correction a vertical
        // stroke comes out twice as dense as a horizontal one on the same picture.
        let landscape = 0.5;
        let across = lay((0.3, 0.5), (0.7, 0.5), landscape, 0.02);
        let down = lay((0.5, 0.3), (0.5, 0.7), landscape, 0.02);
        assert_eq!(across.len(), 20);
        assert_eq!(
            down.len(),
            10,
            "a vertical drag covers half the width-distance"
        );
    }

    #[test]
    fn an_elongated_nib_lays_a_continuous_stroke_rather_than_beads() {
        // **A FIXED angle, and the drag runs along the nib's narrow axis.** The
        // first version of this test used a following nib, which cannot bead — see
        // `Brush::spacing` — so it passed with the spacing rule deliberately broken
        // and was measuring nothing. It was caught by breaking the rule on purpose
        // and watching it not fail, which is the whole reason for doing that.
        //
        // A 1:10 card turned upright and dragged sideways presents `radius * aspect`
        // along the path — a full extent of 0.02 against a naive spacing of 0.025,
        // so consecutive dabs miss each other. The margin matters: at 1:5 the naive
        // spacing still overlaps and the test passes against the bug, which is what
        // the first two attempts at this did.
        let brush = Brush {
            radius: 0.1,
            aspect: 0.1,
            angle: 90.0,
            feather: 0.0,
            nib: raw_core::dodgeburn::Nib::Card,
            follow: false,
            ..Brush::default()
        };
        let mut inst = Instance::new(Sign::Burn, "Burn 1".into());
        begin(&mut inst, &brush, (0.15, 0.5), false);
        extend(&mut inst, &brush, (0.15, 0.5), (0.85, 0.5), SQUARE, false);

        for i in 0..=120 {
            let x = 0.2 + i as f32 * 0.6 / 120.0;
            assert!(
                inst.ev_at(x, 0.5, SQUARE).abs() > 0.0,
                "the stroke beaded at x={x}"
            );
        }
    }

    #[test]
    fn spacing_follows_the_narrow_axis() {
        // The rule stated directly, so the reason survives even if the stroke test
        // above is ever loosened. A nib four times taller than it is wide is laid at
        // a quarter the spacing of a round one of the same radius — because a
        // following nib travels on its narrow axis.
        let round = Brush {
            radius: 0.08,
            aspect: 1.0,
            ..Brush::default()
        };
        let flat = Brush {
            aspect: 0.25,
            ..round
        };
        let tall = Brush {
            aspect: 4.0,
            ..round
        };
        assert!((flat.spacing() - round.spacing() * 0.25).abs() < 1e-6);
        // And a nib wider than it is tall does NOT get four times the spacing: the
        // clamp keeps it at the round figure, because the narrow axis is the width
        // and the width is what `radius` already measures.
        assert_eq!(tall.spacing(), round.spacing());
    }

    #[test]
    fn a_following_nib_turns_with_the_drag_and_a_fixed_one_does_not() {
        let follow = Brush {
            aspect: 0.3,
            follow: true,
            angle: 12.0,
            ..Brush::default()
        };
        let fixed = Brush {
            follow: false,
            ..follow
        };

        let mut a = Instance::new(Sign::Burn, "a".into());
        begin(&mut a, &follow, (0.2, 0.2), false);
        extend(&mut a, &follow, (0.2, 0.2), (0.8, 0.8), SQUARE, false);
        let laid = a.gestures()[0].dabs.last().expect("dabs");
        assert!(
            (laid.angle - 45.0).abs() < 1e-3,
            "a diagonal drag is 45°, got {}",
            laid.angle
        );

        let mut b = Instance::new(Sign::Burn, "b".into());
        begin(&mut b, &fixed, (0.2, 0.2), false);
        extend(&mut b, &fixed, (0.2, 0.2), (0.8, 0.8), SQUARE, false);
        assert_eq!(
            b.gestures()[0].dabs.last().expect("dabs").angle,
            12.0,
            "the brush's own angle"
        );
    }

    #[test]
    fn a_following_nib_measures_its_bearing_in_pixels_not_in_coordinates() {
        // The aspect trap once more, at the last place it appears. A drag across a
        // 2:1 frame that moves the same *number of pixels* in x and y is a 45° drag
        // on the print, and the nib has to lie along it — not along the 63° the raw
        // normalised coordinates would report.
        let landscape = 0.5;
        let brush = Brush {
            aspect: 0.3,
            follow: true,
            ..Brush::default()
        };
        let mut inst = Instance::new(Sign::Burn, "a".into());
        // 0.4 of the width across, and the same distance in pixels down — which on a
        // 2:1 frame is 0.8 of the height.
        begin(&mut inst, &brush, (0.3, 0.1), false);
        extend(&mut inst, &brush, (0.3, 0.1), (0.7, 0.9), landscape, false);
        let angle = inst.gestures()[0].dabs.last().expect("dabs").angle;
        assert!(
            (angle - 45.0).abs() < 1e-3,
            "expected 45° on the print, got {angle}"
        );
    }

    #[test]
    fn the_first_dab_of_a_following_stroke_has_no_direction_to_take() {
        // Nothing has moved yet, so it uses the brush's own angle. A version that
        // reached for a bearing here would divide by a zero-length drag.
        let brush = Brush {
            follow: true,
            angle: 20.0,
            ..Brush::default()
        };
        let mut inst = Instance::new(Sign::Burn, "a".into());
        begin(&mut inst, &brush, (0.5, 0.5), false);
        assert_eq!(inst.gestures()[0].dabs[0].angle, 20.0);
    }

    #[test]
    fn an_eraser_dab_opposes_its_instance_and_nothing_else() {
        let b = Brush {
            intensity: 0.4,
            ..Default::default()
        };
        assert_eq!(
            b.dab((0.5, 0.5), Sign::Burn, false, None, Nib::Round).ev,
            -0.4
        );
        assert_eq!(
            b.dab((0.5, 0.5), Sign::Burn, true, None, Nib::Round).ev,
            0.4
        );
        assert_eq!(
            b.dab((0.5, 0.5), Sign::Dodge, false, None, Nib::Round).ev,
            0.4
        );
        assert_eq!(
            b.dab((0.5, 0.5), Sign::Dodge, true, None, Nib::Round).ev,
            -0.4
        );
    }

    #[test]
    fn pressing_burn_keeps_the_burn_you_were_working_on() {
        // Not "make a new one every time", which is the obvious implementation and
        // would litter the panel with Burn 1..Burn 9 over one session.
        let mut db = DodgeBurnParams::default();
        let mut active = None;

        let a = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        assert_eq!((a, db.instances.len()), (Some(0), 1));
        let b = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        assert_eq!(
            (b, db.instances.len()),
            (Some(0), 1),
            "the same burn, not a second one"
        );

        // Switching to dodge makes one, and switching back returns to the burn.
        let d = instance_for(
            &mut db,
            &mut active,
            Sign::Dodge,
            Tool::Brush,
            Nib::Round,
            false,
        );
        assert_eq!((d, db.instances.len()), (Some(1), 2));
        let back = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        assert_eq!((back, db.instances.len()), (Some(0), 2));
        assert_eq!(active, Some(0), "and the panel selection follows");
    }

    #[test]
    fn asking_for_a_new_instance_always_makes_one() {
        let mut db = DodgeBurnParams::default();
        let mut active = None;
        instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            true,
        );
        assert_eq!(db.instances.len(), 2);
        assert_eq!(active, Some(1));
        assert_eq!(db.instances[1].name, "Burn 2");
    }

    #[test]
    fn the_tool_picks_the_instance_as_much_as_the_sign_does() {
        // The 10b addition to the rule. Pressing `x` with the radial tool up must
        // not paint a burn onto the linear you were editing — kind and sign both
        // have to match, or the selection follows only half of what you chose.
        let mut db = DodgeBurnParams::default();
        let mut active = None;
        instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        let g = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Radial,
            Nib::Round,
            false,
        );
        assert_eq!(
            (g, db.instances.len()),
            (Some(1), 2),
            "a burn brush is not a burn radial"
        );
        assert_eq!(db.instances[1].name, "Radial Burn 1");
        // And back to the brush returns to the brush.
        assert_eq!(
            instance_for(
                &mut db,
                &mut active,
                Sign::Burn,
                Tool::Brush,
                Nib::Round,
                false
            ),
            Some(0)
        );
    }

    /// **A Round pick never lands on a Card layer.** The rule that makes mixed-nib
    /// layers unreachable rather than merely unusual.
    ///
    /// This is `the_tool_picks_the_instance_as_much_as_the_sign_does` one level down,
    /// and it had been missing for the same reason the bug existed: `Pick` collapsed
    /// the tool and the nib into one row of four shapes, and `instance_for` was still
    /// matching on the three-way `Tool`. the maintainer reported the model — *"I don't think we
    /// can have mixed-nib layers"* — which was a description of what he expected rather
    /// than of what the code did.
    #[test]
    fn the_nib_picks_the_instance_as_much_as_the_tool_does() {
        let mut db = DodgeBurnParams::default();
        let mut active = None;

        let round = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Round,
            false,
        );
        assert_eq!((round, db.instances.len()), (Some(0), 1));
        // Same nib, same layer — `pressing_burn_keeps_the_burn_you_were_working_on` still holds.
        assert_eq!(
            instance_for(
                &mut db,
                &mut active,
                Sign::Burn,
                Tool::Brush,
                Nib::Round,
                false
            ),
            Some(0),
        );

        // A different nib is a different layer, not a second nib in this one.
        let card = instance_for(
            &mut db,
            &mut active,
            Sign::Burn,
            Tool::Brush,
            Nib::Card,
            false,
        );
        assert_eq!(
            (card, db.instances.len()),
            (Some(1), 2),
            "a round burn is not a card burn"
        );
        assert_eq!(db.instances[1].shape.nib(), Some(Nib::Card));

        // And going back returns to the round one rather than making a third.
        assert_eq!(
            instance_for(
                &mut db,
                &mut active,
                Sign::Burn,
                Tool::Brush,
                Nib::Round,
                false
            ),
            Some(0),
        );
        assert_eq!(db.instances.len(), 2);
        assert_eq!(active, Some(0), "and the panel selection follows");
    }

    /// Every dab a layer holds carries the layer's nib, whatever the brush says.
    ///
    /// The second half of "mixed layers are unrepresentable": the variant makes the
    /// *layer* single-nibbed, and this makes the dabs agree with it. A `Brush` still has
    /// a `nib` field — it is what the SHAPE row reads and what a new layer is made from
    /// — so without this a brush left on Card could still lay card dabs into a round
    /// layer through any path that did not go via `instance_for`.
    #[test]
    fn a_dab_takes_its_layer_s_nib_and_not_the_brush_s() {
        let card_brush = Brush {
            nib: Nib::Card,
            ..Brush::default()
        };
        let mut round_layer = Instance::of(
            Sign::Burn,
            "a".into(),
            Shape::Brush {
                nib: Nib::Round,
                passes: Vec::new(),
            },
        );

        begin(&mut round_layer, &card_brush, (0.5, 0.5), false);
        extend(
            &mut round_layer,
            &card_brush,
            (0.5, 0.5),
            (0.8, 0.5),
            SQUARE,
            false,
        );

        let dabs: Vec<_> = round_layer
            .gestures()
            .iter()
            .flat_map(|g| g.dabs.iter())
            .collect();
        assert!(dabs.len() > 1, "the drag laid something to check");
        assert!(
            dabs.iter().all(|d| d.nib == Nib::Round),
            "a card brush painted card dabs into a round layer",
        );
    }

    #[test]
    fn a_radial_handle_carries_the_radius_and_the_rotation_together() {
        // One grab, two properties, because that is what the point means: it is on
        // the ellipse's long axis, so where you put it *is* the radius and *is* the
        // angle. The slider exists to refine, not as the only way to turn one.
        let mut shape = Tool::Radial.shape((0.5, 0.5), -1.0, Nib::Round);
        move_handle(&mut shape, End::To, (0.75, 0.5), SQUARE);
        let Shape::Radial(r) = &shape else {
            panic!("radial")
        };
        assert!((r.outer - 0.25).abs() < 1e-5, "{}", r.outer);
        assert!(
            r.angle.abs() < 1e-3,
            "a drag along x is no rotation: {}",
            r.angle
        );

        move_handle(&mut shape, End::To, (0.5, 0.75), SQUARE);
        let Shape::Radial(r) = &shape else {
            panic!("radial")
        };
        assert!((r.outer - 0.25).abs() < 1e-5);
        assert!(
            (r.angle - 90.0).abs() < 1e-3,
            "straight down is a quarter turn: {}",
            r.angle
        );
    }

    #[test]
    fn a_radial_handle_round_trips_through_the_geometry() {
        // `handles` and `move_handle` are inverses, and they have to be or a handle
        // would jump away from the cursor the moment you grabbed it. The aspect
        // correction is in both, which is exactly where a sign error hides.
        let aspect = 0.625;
        let mut shape = Tool::Radial.shape((0.4, 0.55), -1.0, Nib::Round);
        for at in [(0.7, 0.55), (0.4, 0.2), (0.65, 0.8), (0.2, 0.3)] {
            move_handle(&mut shape, End::To, at, aspect);
            let (_, to) = handles(&shape, aspect).expect("a gradient");
            assert!((to.0 - at.0).abs() < 1e-4, "x: {to:?} vs {at:?}");
            assert!((to.1 - at.1).abs() < 1e-4, "y: {to:?} vs {at:?}");
        }
    }

    #[test]
    fn moving_a_radial_centre_carries_the_shape_rather_than_resizing_it() {
        let aspect = 0.75;
        let mut shape = Tool::Radial.shape((0.5, 0.5), -1.0, Nib::Round);
        move_handle(&mut shape, End::To, (0.8, 0.5), aspect);
        let Shape::Radial(before) = shape else {
            panic!("radial")
        };
        move_handle(&mut shape, End::From, (0.2, 0.3), aspect);
        let Shape::Radial(after) = shape else {
            panic!("radial")
        };
        assert_eq!((after.cx, after.cy), (0.2, 0.3));
        assert_eq!((after.outer, after.angle), (before.outer, before.angle));
    }

    #[test]
    fn the_instance_cap_refuses_rather_than_silently_doing_nothing() {
        let mut db = DodgeBurnParams::default();
        let mut active = None;
        for _ in 0..DodgeBurnParams::MAX_INSTANCES {
            assert!(
                instance_for(
                    &mut db,
                    &mut active,
                    Sign::Burn,
                    Tool::Brush,
                    Nib::Round,
                    true
                )
                .is_some()
            );
        }
        assert_eq!(
            instance_for(
                &mut db,
                &mut active,
                Sign::Burn,
                Tool::Brush,
                Nib::Round,
                true
            ),
            None
        );
        assert_eq!(db.instances.len(), DodgeBurnParams::MAX_INSTANCES);
    }

    use crate::hotkeys::Action as A;

    #[test]
    fn each_bracket_pair_moves_its_own_control_and_no_other() {
        // Four pairs on two keys is exactly the shape that comes out cross-wired.
        // One assertion per pair, each checking that the other three did not move.
        // Mid-range on every axis, not the default: the default opacity is 1.0,
        // which is the top of its range, so `]` clamps and *nothing* moves — a
        // pass that would have read as a cross-wiring failure. The first version of
        // this test did exactly that.
        let base = Brush {
            radius: 0.06,
            feather: 0.5,
            intensity: 0.5,
            opacity: 0.5,
            ..Brush::default()
        };
        for (action, name) in [
            (A::BrushRadius(true), "radius"),
            (A::BrushIntensity(true), "intensity"),
            (A::BrushFeather(true), "feather"),
            (A::BrushOpacity(true), "opacity"),
        ] {
            let mut b = base;
            b.step(action);
            let moved: Vec<&str> = [
                ("radius", b.radius != base.radius),
                ("intensity", b.intensity != base.intensity),
                ("feather", b.feather != base.feather),
                ("opacity", b.opacity != base.opacity),
            ]
            .iter()
            .filter(|(_, m)| *m)
            .map(|(n, _)| *n)
            .collect();
            assert_eq!(moved, vec![name], "the wrong control moved");
        }
    }

    #[test]
    fn the_brackets_go_both_ways_and_stop_at_the_ends() {
        let mut b = Brush::default();
        let start = b.radius;
        b.step(A::BrushRadius(true));
        assert!(b.radius > start);
        b.step(A::BrushRadius(false));
        assert!((b.radius - start).abs() < 1e-6, "and back to where it was");

        for _ in 0..200 {
            b.step(A::BrushRadius(false));
        }
        assert_eq!(b.radius, *Brush::RADIUS_RANGE.start());
        for _ in 0..400 {
            b.step(A::BrushRadius(true));
        }
        assert_eq!(b.radius, *Brush::RADIUS_RANGE.end());
    }

    #[test]
    fn an_action_that_is_not_the_brushs_leaves_it_alone() {
        let mut b = Brush::default();
        b.step(A::Undo);
        b.step(A::RotateLeft);
        assert_eq!(b, Brush::default());
    }
}
