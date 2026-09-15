//! Pipeline parameters as a **value**, plus the undo history built on that.
//!
//! The handoff is explicit about why this shape and not ambient state:
//!
//! > Params must be cheaply clonable and passed as a value to the graph, not read
//! > from ambient tab state -- this is what makes undo, duplication, and
//! > serialization clean.
//!
//! So `Params` is `Clone + PartialEq`, it is passed down rather than reached for,
//! and undo is a stack of snapshots. Snapshots over an edit log was the decision:
//! snapshot undo is trivially correct, and an edit log only starts paying for
//! itself with multi-instance modules and masks.
//!
//! Dodge & Burn made deep copies expensive: 256 snapshots of 100,000 unchanged dabs
//! retained about 879 MiB. Each gesture now shares its dab payload across clones.
//! Extending a pass uses copy-on-write, so older history and snapshots stay immutable
//! while retained memory follows unique brushwork instead of snapshot count.
//!
//! The struct is grouped by **invalidation tier**, not by UI panel. That is the
//! grouping that matters: it is what `Dirty` reads to decide how much of the
//! pipeline has to re-run, and the tiers differ in cost by four orders of
//! magnitude.

use crate::composition::CompositionParams;
use crate::curve::CurveStack;
use crate::dodgeburn::DodgeBurnParams;
use crate::frame::FrameParams;
use crate::grain::GrainParams;
use crate::output::OutputParams;
use crate::scene::{DecodeOptions, Sampling, Weighting};
use crate::sharpen::SharpenParams;
use crate::toning::ToningParams;

/// Tone mapping at the display transform. Not a curve preset -- a separate stage
/// answering a different question. The curve reshapes the negative (scene-referred
/// in, scene-referred out); this fits the scene range into the display range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToneMap {
    /// Clamp to `[0, 1]` and encode. Whatever is above 1.0 is gone.
    Clip,
    /// Linear below `threshold`, exponential roll-off above it, asymptotic to 1.0.
    ///
    /// The minimal intervention: it answers "give me my highlights back" without
    /// also answering "make it filmic". Everything below the threshold is passed
    /// through *exactly*, so midtone contrast and the black end are untouched —
    /// which is the whole difference from AgX, and why it is the one to reach for
    /// when the only problem is a hot sky.
    ///
    /// In darkroom terms this is burning in the highlights, not printing on a
    /// different paper.
    Shoulder { threshold: f32, strength: f32 },
    /// AgX (Troy Sobotka / Blender), a sigmoid over a configurable log2 window.
    /// The compact parameter set is deliberately monochrome: colour-preserving
    /// inset/outset matrices and look controls have no job in a one-channel pipe.
    Agx(AgxParams),
}

/// The useful monochrome subset of darktable's AgX controls.
///
/// Black and white are stops relative to 18% grey, not absolute scene EV. Keeping
/// that convention makes the range legible to a photographer and preserves middle
/// grey while either end moves. Curve gamma remains fixed at 2.2: exposing it would
/// give two controls for roughly the same curve shape and make the module needlessly
/// technical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgxParams {
    /// Keeps the black/white window fitted to the tones entering DISPLAY. This is
    /// an editing mode, not part of the sigmoid itself; the renderer ignores it.
    pub auto_range: bool,
    pub black_ev: f32,
    pub white_ev: f32,
    pub contrast: f32,
    pub toe_power: f32,
    pub shoulder_power: f32,
}

impl AgxParams {
    pub const DEFAULT: Self = Self {
        auto_range: false,
        black_ev: -10.0,
        white_ev: 6.5,
        contrast: 3.0,
        toe_power: 1.5,
        shoulder_power: 3.3,
    };

    /// Keep the sigmoid monotone for every combination the UI or a hand-edited
    /// sidecar can produce. A contrast below either pivot-to-endpoint secant would
    /// turn a segment back on itself; raising it to the mathematical minimum is a
    /// much safer boundary than rendering a solarised curve.
    pub fn normalized(self) -> Self {
        const PIVOT_Y: f32 = 0.458_656_45;
        let black_ev = self.black_ev.clamp(-16.0, -1.0);
        let white_ev = self.white_ev.clamp(1.0, 12.0);
        let pivot_x = -black_ev / (white_ev - black_ev);
        let minimum = (PIVOT_Y / pivot_x).max((1.0 - PIVOT_Y) / (1.0 - pivot_x)) + 1.0e-3;
        Self {
            auto_range: self.auto_range,
            black_ev,
            white_ev,
            contrast: self.contrast.clamp(0.5, 12.0).max(minimum),
            toe_power: self.toe_power.clamp(0.5, 8.0),
            shoulder_power: self.shoulder_power.clamp(0.5, 8.0),
        }
    }
}

impl Default for AgxParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Default for ToneMap {
    fn default() -> Self {
        Self::SHOULDER_DEFAULT
    }
}

impl ToneMap {
    /// Where the roll-off starts by default. 0.75 is about a third of a stop below
    /// scene white, so it engages on genuinely bright material and leaves ordinary
    /// midtones alone.
    pub const SHOULDER_DEFAULT: Self = Self::Shoulder {
        threshold: 0.75,
        strength: 0.8,
    };

    pub const AGX_DEFAULT: Self = Self::Agx(AgxParams::DEFAULT);

    pub const UI_ORDER: [Self; 3] = [Self::Clip, Self::SHOULDER_DEFAULT, Self::AGX_DEFAULT];

    pub fn label(self) -> &'static str {
        match self {
            Self::Clip => "Clip (no tone map)",
            Self::Shoulder { .. } => "Soft shoulder",
            Self::Agx(..) => "AgX",
        }
    }

    /// Mutable access to the shoulder controls, for the sliders.
    ///
    /// Exists so the mode cannot repeat the `Weighting::Weighted` mistake — a
    /// parameterised variant the UI offers but provides no way to change is
    /// indistinguishable from a mode that does nothing.
    pub fn shoulder_mut(&mut self) -> Option<(&mut f32, &mut f32)> {
        match self {
            Self::Shoulder {
                threshold,
                strength,
            } => Some((threshold, strength)),
            _ => None,
        }
    }

    pub fn agx_mut(&mut self) -> Option<&mut AgxParams> {
        match self {
            Self::Agx(params) => Some(params),
            _ => None,
        }
    }
}

/// Luminance derivation: the two orthogonal axes. Gain equalisation is upstream of
/// both and lives in `DecodeOptions`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LuminanceParams {
    pub sampling: Sampling,
    pub weighting: Weighting,
}

/// `out = (in - black) * 2^stops`. Black first, so raising exposure does not
/// amplify the black offset.
///
/// Deliberately minimal: the Ansel-style module (camera-offset / manual / auto
/// modes, EXIF bias compensation, eyedropper) is a separate piece of work. The
/// EXIF that module will need is already read -- see `sensor::Metadata` -- but
/// nothing consumes it yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureParams {
    /// Module bypass. See `Curve::enabled` for why it lives on the module.
    pub enabled: bool,
    pub ev: f32,
    /// Trim on top of the sensor black level already subtracted at decode. Not only
    /// a calibration nudge: the UI range (±0.150) is wide enough to genuinely crush
    /// the shadows, which is a print decision. Allowed to go negative so shadows can
    /// lift.
    pub black: f32,
}

impl ExposureParams {
    /// Whether this changes anything: switched on, and moved off neutral.
    pub fn is_active(&self) -> bool {
        self.enabled && !self.is_default()
    }

    /// Whether the user has touched it, **ignoring the bypass**.
    ///
    /// Bypassed and modified are two independent facts and the dot reports them as
    /// two different things, so the modified test must not be able to see the
    /// switch. Comparing against `Self::default()` wholesale would make switching a
    /// neutral module off read as an edit.
    pub fn is_default(&self) -> bool {
        self.ev == Self::default().ev && self.black == Self::default().black
    }

    /// Whether the module's dot should read as modified. See [`is_modified`].
    ///
    /// Identical to `!is_default()` for this module, and deliberately expressed through
    /// the shared rule anyway: it is the same question every switchable module answers,
    /// and having one of them answer it differently is how the two drift apart.
    pub fn is_modified(&self) -> bool {
        is_modified(
            self.is_default(),
            self.is_active(),
            Self::default().is_active(),
        )
    }
}

impl Default for ExposureParams {
    fn default() -> Self {
        Self {
            enabled: true,
            ev: 0.0,
            black: 0.0,
        }
    }
}

/// Contrast Mask — the darkroom unsharp mask, not a clarity slider.
///
/// ```text
/// mask   = -blur(negative, spacer) * contrast
/// result = negative + mask                        (all in log2 EV)
/// ```
///
/// The reasoning, which should not be re-litigated casually: synthetic local
/// contrast tools modify texture *amplitude* independent of what the texture is.
/// No physical process does that, which is why they produce the "processed" look.
/// Printing through a blurred, low-contrast **positive** mask registered with the
/// negative is fundamentally a **compression** operation whose local contrast
/// increase is a byproduct.
///
/// In log2 EV the arithmetic is the same as unsharp mask. What makes it behave
/// like the darkroom technique is the **parameter ranges**: a real mask is thin
/// (~0.2-0.5 gamma) and the blur is large. Constrain the sliders to physical
/// ranges and the synthetic look becomes unreachable — which is why the ranges
/// below are enforced rather than advisory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastMaskParams {
    pub enabled: bool,
    /// Mask gamma. A real masking film is thin; above about 0.6 this stops
    /// being a mask and starts being an effect.
    pub contrast: f32,
    /// Spacer distance: the Gaussian sigma, as a **percentage of the frame
    /// diagonal**.
    ///
    /// Named for the physical spacer that separates mask from negative, which is
    /// what makes the mask unsharp in the first place.
    ///
    /// Frame-relative rather than in pixels, and the reason is a defect that was
    /// visible in the app: everything downstream measures in pixels of the
    /// LUMINANCE image, and SuperPixel is half resolution, so a 60px spacer
    /// covered twice the fraction of the frame in SuperPixel that it covered in
    /// DirectMosaic. **Changing sampling mode changed the mask's look** — which
    /// no user would predict and no darkroom analogue justifies. Percentage also
    /// makes one setting mean one thing across a 10 MP file and a 100 MP one.
    ///
    /// The **diagonal** is the reference, not the width, because it is the
    /// measure that survives a change of aspect or orientation — and because it
    /// is how film formats are compared in the first place.
    ///
    /// Pixels return as a Settings option; see `docs/settings-menu.md`.
    pub spacer: f32,
    /// Registration offset, in pixels of the luminance image.
    ///
    /// The novel control. In the darkroom the mask is separated from the
    /// negative by a spacer and misregistration was a real variable — a
    /// deliberate slip gives directional relief. Nobody ships this.
    ///
    /// Still in pixels while `spacer` is a percentage, deliberately and
    /// provisionally: a registration slip is a displacement rather than a
    /// proportion, and 1px control over it is worth having. It does inherit the
    /// same sampling-mode inconsistency the spacer just shed, so if that shows up
    /// in use, this is the next thing to convert.
    pub offset: (f32, f32),
}

impl ContrastMaskParams {
    pub const CONTRAST_RANGE: std::ops::RangeInclusive<f32> = 0.05..=0.60;
    /// Percent, not a fraction, so the stored number reads the way the slider
    /// does. Physically a spacer against a 35mm negative blurs on the order of
    /// 1-5% of the diagonal; below about 0.25% this stops being a mask and starts
    /// being a sharpening halo, which is the thing the module exists not to be.
    ///
    /// The top of the range is close to the old 200px maximum *in SuperPixel*
    /// (5% of a 24 MP quad-binned frame is ~180px), so the default working mode
    /// keeps roughly the reach it had. Full-resolution modes on a large sensor
    /// now reach considerably further, which is the point — and costs more; see
    /// `raw_graph::build`.
    pub const SPACER_RANGE: std::ops::RangeInclusive<f32> = 0.25..=5.0;
    pub const OFFSET_RANGE: std::ops::RangeInclusive<f32> = -40.0..=40.0;

    /// The spacer as a Gaussian sigma in pixels, for a frame `dims` big.
    ///
    /// `dims` is the **luminance** image, not the sensor: that is the frame the
    /// user is looking at and the space the blur actually runs in.
    /// Divide by 100 *last*: `spacer * 0.01 * diagonal` gives 49.99999 where
    /// `spacer * diagonal / 100` gives exactly 50, and this number is repeated
    /// independently by the graph (as an allocation size) and by the shader (as
    /// a sigma). They have to agree, so it is worth not shedding a bit here.
    pub fn spacer_px(&self, dims: (u32, u32)) -> f32 {
        let (w, h) = (dims.0 as f32, dims.1 as f32);
        self.spacer * (w * w + h * h).sqrt() / 100.0
    }

    /// Whether this actually does anything. A zero mask gamma is the identity,
    /// so the graph omits the whole branch rather than computing a no-op blur.
    pub fn is_active(&self) -> bool {
        self.enabled && self.contrast > 0.0 && self.spacer > 0.0
    }

    /// Whether the user has touched it, ignoring the bypass. See
    /// `ExposureParams::is_default`.
    pub fn is_default(&self) -> bool {
        Self {
            enabled: Self::default().enabled,
            ..*self
        } == Self::default()
    }

    /// Whether the module's dot should read as modified. See [`is_modified`].
    pub fn is_modified(&self) -> bool {
        is_modified(
            self.is_default(),
            self.is_active(),
            Self::default().is_active(),
        )
    }
}

/// **The dot is ruby when the module would render differently than it does at
/// defaults.**
///
/// The older rule was "the user has moved a value, ignoring the bypass", and it is
/// right for a module that is **on** by default: Exposure at 0 EV renders identically
/// whether its switch is up or down, so neither state is an edit and neither should
/// light the dot.
///
/// It is wrong for a module that is **off** by default. Contrast Mask switched on at
/// its default values changes every pixel in the frame, and under the old rule the dot
/// stayed grey until you also moved a slider — so the panel had no way of saying that
/// the rendering had changed. the maintainer found it, and asked for Contrast Mask specifically;
/// this is the general form of what he asked for, and it leaves every other module
/// exactly as it was, because for them `is_active()` cannot differ from the default's
/// unless a value has moved.
///
/// Colour still says *modified* and fill still says *running* — the two axes in
/// `widgets::Module`'s table are unchanged. This only widens what counts as modified.
pub fn is_modified(is_default: bool, is_active: bool, default_is_active: bool) -> bool {
    !is_default || is_active != default_is_active
}

impl Default for ContrastMaskParams {
    fn default() -> Self {
        // Off by default: it is a look, and every frame judged before this
        // module existed must still render the same way.
        //
        // 1.5% is where the old 60px default landed in SuperPixel on a 24 MP file
        // — the mode and the resolution most frames were judged in — so the
        // default look carries across the change of units.
        Self {
            enabled: false,
            contrast: 0.35,
            spacer: 1.5,
            offset: (0.0, 0.0),
        }
    }
}

/// The display transform. Three separate concerns, deliberately not conflated:
/// tone mapping, transfer function, and (later) the display ICC profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayParams {
    /// Bypasses the creative tone map only. Monitor gamma is a required transfer
    /// function and dither is an output property, so neither can honestly be
    /// switched off as part of a before/after comparison.
    pub enabled: bool,
    pub tone_map: ToneMap,
    /// Plain gamma, 2.2 by default. NOT called sRGB: that name imports a whole
    /// colorimetric spec of which only the EOTF is wanted.
    pub gamma: f32,
    pub dither: bool,
}

impl Default for DisplayParams {
    fn default() -> Self {
        Self {
            enabled: true,
            tone_map: ToneMap::default(),
            gamma: 2.2,
            dither: true,
        }
    }
}

impl DisplayParams {
    pub fn applied_tone_map(self) -> ToneMap {
        if self.enabled {
            self.tone_map
        } else {
            ToneMap::Clip
        }
    }

    pub fn is_modified(&self) -> bool {
        let default = Self::default();
        self.tone_map != default.tone_map
            || self.gamma != default.gamma
            || self.dither != default.dither
    }
}

/// Everything that describes how one image is rendered. Cheap to clone, compares
/// structurally, and is passed by value into the render path.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Params {
    pub decode: DecodeOptions,
    pub luminance: LuminanceParams,
    pub exposure: ExposureParams,
    pub contrast_mask: ContrastMaskParams,
    /// Local exposure: brush passes, stacked instances, tonal-range masks.
    ///
    /// **Downstream of Contrast Mask and upstream of the curve**, which is
    /// the maintainer's decision and the darkroom order: the mask is contact-printed from
    /// the negative and sandwiched with it, and only then do you dodge under the
    /// enlarger. Putting D&B first — beside the global exposure it locally
    /// modifies, which is the tempting reading — would mean burning a sky also
    /// changed the mask that sky prints through. See `raw_graph::build`.
    ///
    /// The opposite case to `output` below: this is per-image state that is
    /// *nothing but* a render input, so it is in the `render` tier and in no
    /// higher one. A brush drag re-dispatches the visible region and re-derives
    /// nothing, exactly as a crop drag does.
    pub dodgeburn: DodgeBurnParams,
    pub curve: CurveStack,
    /// Chemical toning — the process, and the ordered stack of baths.
    ///
    /// **A `render` change and no higher, and it sits beside `display` in spirit
    /// even though it is written here.** The whole model bakes to a one-dimensional
    /// table, so moving a slider re-uploads a few kilobytes and re-dispatches the
    /// visible region. Nothing is re-derived, exactly as for the curve — and for the
    /// same reason, since both are transfer functions that happen to be expensive to
    /// *build* and trivial to *apply*.
    ///
    /// It runs **after the tone map**, because a toner acts on a print rather than on
    /// a negative. On the GPU that means inside the display pass, which is where the
    /// tone map is; on the CPU export tail it is a stage of its own between grain and
    /// sharpening. Both sample the same baked table, which is what keeps them the same
    /// picture. See `raw_core::toning`.
    pub toning: ToningParams,
    /// Orientation, straighten and crop.
    ///
    /// **Grouped with `display` at the bottom, because it is a `render` change.**
    /// The struct is grouped by invalidation tier and this is the module most
    /// likely to be filed wrong: it changes the image's *dimensions*, which sounds
    /// expensive. It is not. The tone chain runs on the uncropped frame and the
    /// crop restricts only what the sink is asked to produce, so dragging a crop
    /// handle re-dispatches the visible region and re-derives nothing. If this ever
    /// looks like it needs a tier of its own, that resolution has been broken.
    pub composition: CompositionParams,
    pub display: DisplayParams,
    /// Crystallographic AgX grain. **Export-only**, and therefore in no tier of
    /// `Dirty` — the same filing as `output` below and for a stricter reason: it is
    /// not that grain *should not* reach the viewport, it is that it cannot. It runs
    /// on the CPU between the tone map and the encode, downstream of every GPU node,
    /// on a chain of sixty serial convolutions.
    ///
    /// What it does change is the **grain loupe**, which is not the viewport and does
    /// not go through `diff` — it watches these fields itself and re-renders its own
    /// tile. See `raw_app::loupe`.
    pub grain: GrainParams,
    /// Output sharpening. **Export-only**, and in no tier of `Dirty` for the same
    /// reason `output` is not: its radius is in *output* pixels, so it is defined on a
    /// grid the viewport does not render. See `raw_core::sharpen`.
    ///
    /// It sits beside `grain` rather than up with the tone modules because that is
    /// where it runs — `resample` → `grain` → **sharpen** → encode — and because the
    /// print loupe is what watches both. The output position was chosen over the
    /// prototype's scene-linear one; `docs/decisions.md` has the argument.
    pub sharpen: SharpenParams,
    /// Print resolution, output size and what metadata travels.
    ///
    /// **In `Params` but in no tier of `Dirty`.** It rides here because it is
    /// per-image, belongs in the sidecar and belongs in undo — but nothing in it can
    /// change a rendered pixel, and `diff` leaves it out of `render` on purpose. See
    /// `output`'s module note.
    pub output: OutputParams,
    /// Physical border canvas, composed after every image-making operation.
    /// Export-only, but per-image and therefore part of sidecar, history and snapshots.
    pub frame: FrameParams,
}

/// What has to re-run after a parameter change, cheapest tier last.
///
/// The tiers are not cosmetic. On a 100 MP RAF: decode is ~seconds, luminance is
/// ~hundreds of milliseconds, the curve bake is ~a millisecond, and render is a
/// GPU dispatch over the visible viewport only. Treating them alike would make
/// dragging the exposure slider re-decode the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty {
    /// Sensor -> scene. Off-thread; the whole frame is reprocessed.
    pub decode: bool,
    /// Scene -> luma, then re-upload. CPU, full frame.
    pub luminance: bool,
    /// Re-bake and re-upload the 65536-entry curve LUT.
    pub curve: bool,
    /// Re-dispatch the GPU passes. Uniform writes only.
    pub render: bool,
}

impl Dirty {
    pub const NONE: Self = Self {
        decode: false,
        luminance: false,
        curve: false,
        render: false,
    };

    /// Everything, for the first frame after a load.
    pub const ALL: Self = Self {
        decode: true,
        luminance: true,
        curve: true,
        render: true,
    };

    pub fn any(self) -> bool {
        self.decode || self.luminance || self.curve || self.render
    }

    pub fn or(self, other: Self) -> Self {
        Self {
            decode: self.decode || other.decode,
            luminance: self.luminance || other.luminance,
            curve: self.curve || other.curve,
            render: self.render || other.render,
        }
    }
}

impl Params {
    /// The same parameters with every bypassed module replaced by its default.
    ///
    /// **This is the whole implementation of module bypass**, and it is one function
    /// rather than a flag threaded through the graph, the shaders and the uniform
    /// block. A module that is switched off is asked to render as though it had
    /// never been touched, which for every module here *is* its default — exposure
    /// 0 EV, an identity curve, a mask with nothing to add. Substituting the value
    /// means nothing downstream has to learn what "off" means: the graph already
    /// omits an identity curve and an inactive mask, so a bypassed module costs no
    /// node and no dispatch, for free.
    ///
    /// The stored `Params` keeps the real values throughout, so switching a module
    /// back on returns to the edit. Only the render sees this.
    ///
    /// Decode and luminance remain structural. DISPLAY's required transfer function
    /// remains structural too, but its creative tone map can be bypassed to Clip for
    /// a before/after comparison; gamma and dither stay intact.
    pub fn effective(&self) -> Self {
        let mut p = self.clone();
        if !p.exposure.enabled {
            p.exposure = ExposureParams::default();
        }
        if !p.curve.enabled {
            p.curve = CurveStack::default();
        }
        // `ContrastMaskParams::is_active` already consults `enabled`, so the mask
        // needs no substitution — but it gets one anyway, so that "bypassed reads
        // exactly as untouched" is true of every module rather than of two out of
        // three, and so the histogram key below cannot distinguish them.
        if !p.contrast_mask.enabled {
            p.contrast_mask = ContrastMaskParams::default();
        }
        // Substituting the default here empties the instance list, which is what
        // makes a bypassed D&B cost no node and no stroke buffer rather than a
        // dispatch that multiplies every pixel by 2^0.
        if !p.dodgeburn.enabled {
            p.dodgeburn = DodgeBurnParams::default();
        }
        // Grain's substitution is what makes a bypassed module cost *nothing* rather
        // than five seconds of emulsion that is then discarded: `export::write`
        // consults `is_active`, and the default is off.
        if !p.grain.enabled {
            p.grain = GrainParams::default();
        }
        // And sharpening, for the same reason: `export::write` consults `is_active`,
        // and a bypassed module that still carried an amount would pay for a full
        // wavelet decomposition over the export buffer to reconstruct its own input.
        if !p.sharpen.enabled {
            p.sharpen = SharpenParams::default();
        }
        // And toning, on the rule this function states above: *bypassed reads exactly
        // as untouched*, of every module rather than of most of them. `is_active`
        // already consults `enabled`, so nothing renders differently either way — what
        // this buys is that the histogram key cannot tell a bypassed stack from no
        // stack, and so cannot rebuild for a change that changes nothing.
        if !p.toning.enabled {
            p.toning = ToningParams::default();
        }
        if !p.frame.enabled {
            p.frame = FrameParams::default();
        }
        if !p.display.enabled {
            p.display.tone_map = ToneMap::Clip;
        }
        // Composition substitutes its own bypass rather than a plain default,
        // because orientation has to survive it. See `CompositionParams::bypassed`.
        p.composition = p.composition.applied();
        p
    }

    /// The same parameters with the crop suppressed but everything else intact.
    ///
    /// What the crop tool renders while it is open, per the handoff:
    ///
    /// > while the crop tool is open it renders the **full uncropped frame** so the
    /// > user can see and re-grab parts outside the current crop (Capture One
    /// > behavior). The comp values stay on params; only their application is
    /// > suppressed.
    ///
    /// Note what it does *not* suppress: orientation and straighten stay applied,
    /// because you are composing against the picture the right way up. Only the
    /// rectangle stops being enforced, and the handles are drawn where it is.
    pub fn uncropped(&self) -> Self {
        let mut p = self.clone();
        p.composition.crop = crate::composition::Rect::FULL;
        p
    }

    /// Take the **look** from `from` and leave everything that describes the *file*
    /// or the *decode* where it is.
    ///
    /// What restoring a snapshot does, and one function on purpose — the definition of
    /// "what a snapshot is of" is the two lines it does *not* copy.
    ///
    /// **Written as an exclusion, not a list of inclusions**, so a module added to
    /// `Params` later is captured by default. Forgetting to add a new look to a
    /// snapshot is a silent bug; forgetting to exclude a new file property is a loud
    /// one.
    ///
    /// **`decode`** is diagnostic state — unity WB exists to make the CFA visible, and
    /// a snapshot that restored it would be restoring an inspection rather than a look.
    /// **`luminance.sampling`** is the demosaic method. The boundary runs *through*
    /// `LuminanceParams`, not around it: `weighting` is the mix a frame is printed
    /// through, as much a look as the curve, so it travels.
    pub fn restore_look_from(&mut self, from: &Self) {
        let keep_decode = self.decode;
        let keep_sampling = self.luminance.sampling;
        *self = from.clone();
        self.decode = keep_decode;
        self.luminance.sampling = keep_sampling;
    }

    /// What changed between `self` (old) and `new`, as a work order.
    ///
    /// The cascade is the point: a decode change implies a luminance re-derive
    /// implies a re-render. Callers act on the highest tier set and get the rest
    /// for free, so no call site has to remember the dependency order.
    pub fn diff(&self, new: &Self) -> Dirty {
        let decode = self.decode != new.decode;
        let luminance = decode || self.luminance != new.luminance;
        let curve = !self.curve.same_render(&new.curve);
        // **Composition, Dodge & Burn and `ratio` all sit in `render` and nowhere
        // higher**, which is the payoff of running the tone chain on the uncropped
        // frame. D&B is the one most likely to be *feared* upward — it looks like
        // painting — but the strokes are parametric, the node is pointwise, and the
        // zone masks are feed-forward, so a drag is a uniform write over the visible
        // region. `dragging_a_crop_handle_does_not_re_derive_anything` and
        // `a_brush_stroke_does_not_re_derive_anything` fail if either moves.
        //
        // **`output` and `grain` are absent, and neither is an oversight.** Output
        // describes the *file*, and a viewport that re-rendered when print size moved
        // would let output's dimensions leak back into the view; grain is export-only,
        // so reaching `render` would re-dispatch to produce an identical frame. The
        // loupe watches both directly. `an_output_change_is_not_a_render_change` fails
        // if this line grows an `|| self.output != new.output`.
        let render = luminance
            || curve
            || self.exposure != new.exposure
            || self.contrast_mask != new.contrast_mask
            || self.dodgeburn != new.dodgeburn
            || self.composition != new.composition
            || self.display != new.display
            // Toning is here and nowhere higher, for the reason the curve is: it is a
            // transfer function, and a transfer function is a table lookup however
            // elaborate the thing that built the table was. The chemistry runs on the
            // CPU once per edit and the shader never sees it.
            || self.toning != new.toning;
        Dirty {
            decode,
            luminance,
            curve,
            render,
        }
    }
}

/// Snapshot undo/redo.
///
/// Coalescing is the only subtle part. A slider drag mutates params on every frame,
/// which would otherwise push sixty history entries per second. `record` therefore
/// holds the *first* pre-edit state of a continuous interaction in `in_flight` and
/// only commits it once the interaction settles -- one undo entry per gesture,
/// which is what a user means by "undo that slider move".
#[derive(Debug, Default)]
pub struct History {
    past: Vec<Params>,
    future: Vec<Params>,
    in_flight: Option<Params>,
    /// Monotonic within the session: any change to the visible timeline bumps it.
    /// Length alone cannot drive a following History panel because committing at
    /// [`Self::CAP`] removes one state as it adds another and leaves the length equal.
    revision: u64,
}

impl History {
    /// The user-facing depth. Brush payload is shared between unchanged states;
    /// genuinely new brushwork still retains its own bytes.
    const CAP: usize = 256;

    /// Report a frame's worth of change.
    ///
    /// `before` is the state at the top of the frame, `after` the state at the
    /// bottom, and `settled` is false while the pointer is still down on a widget.
    pub fn record(&mut self, before: Params, after: &Params, settled: bool) {
        if before != *after {
            // First change of this gesture: remember where the gesture started.
            if self.in_flight.is_none() {
                self.in_flight = Some(before);
            }
            // Any new edit invalidates the redo branch.
            if !self.future.is_empty() {
                self.future.clear();
                self.bump_revision();
            }
        }
        if settled {
            self.commit();
        }
    }

    /// Close out an in-flight gesture. Idempotent.
    pub fn commit(&mut self) {
        if let Some(before) = self.in_flight.take() {
            self.past.push(before);
            if self.past.len() > Self::CAP {
                self.past.remove(0);
            }
            self.bump_revision();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty() || self.in_flight.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Step `current` back one entry. Returns false if there was nothing to undo.
    pub fn undo(&mut self, current: &mut Params) -> bool {
        // An undo mid-gesture should undo the gesture, so close it out first.
        self.commit();
        let Some(prev) = self.past.pop() else {
            return false;
        };
        self.future.push(std::mem::replace(current, prev));
        self.bump_revision();
        true
    }

    pub fn redo(&mut self, current: &mut Params) -> bool {
        self.commit();
        let Some(next) = self.future.pop() else {
            return false;
        };
        self.past.push(std::mem::replace(current, next));
        self.bump_revision();
        true
    }

    pub fn depth(&self) -> (usize, usize) {
        (self.past.len(), self.future.len())
    }

    /// Identity of the currently visible timeline for view-state consumers.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// The states behind the current one, **oldest first**.
    pub fn past(&self) -> &[Params] {
        &self.past
    }

    /// The states ahead of the current one, **nearest first** — this is a stack, so
    /// `future[0]` is the furthest away and the last element is the next redo. A panel
    /// listing a timeline wants it reversed; see `History::jump`.
    pub fn future(&self) -> &[Params] {
        &self.future
    }

    /// Move `current` `delta` entries along the timeline. Negative goes back.
    ///
    /// Clicking row *n* of a history panel is this, and it is expressed as repeated
    /// `undo`/`redo` rather than as an index into the vectors on purpose: those two
    /// already handle the in-flight gesture, the branch invalidation and the
    /// `past`/`future` bookkeeping, and a second path that reached into the vectors
    /// directly would be a second place for that to be got wrong.
    ///
    /// Stops early rather than panicking if it runs out — a click on a row that has
    /// just been trimmed by `CAP` is a click on something that is no longer there.
    pub fn jump(&mut self, current: &mut Params, delta: i32) {
        for _ in 0..delta.abs() {
            let moved = if delta < 0 {
                self.undo(current)
            } else {
                self.redo(current)
            };
            if !moved {
                break;
            }
        }
    }
}

impl Params {
    /// What one history entry changed, as **module and control**.
    ///
    /// A history row reading "Exposure" three times says nothing, so this returns both
    /// the module and the control and the panel sets them as `module · control`.
    ///
    /// **A name, not a diff.** [`Self::diff`] groups by invalidation tier, so a panel
    /// built on it would say "render" for composition, the curve and Dodge & Burn
    /// alike.
    ///
    /// **The control names are the panel's own**, deliberately — the use of a history
    /// row is to tell you which control to go back to. That duplicates the strings
    /// between here and the develop panel; naming them from `raw-app` instead would put
    /// the meaning of a history entry in the UI layer, where it cannot be tested.
    ///
    /// Module order is the panel's, so a row names the module you would scroll to.
    /// Where an edit touched two the first wins. `None` when nothing differs, which the
    /// panel still has to be able to draw.
    pub fn what_changed(&self, other: &Self) -> Option<(&'static str, Option<&'static str>)> {
        // A module's bypass is an edit in its own right and is worth naming as one:
        // "Contrast Mask · bypass" is a different thing from any of its sliders.
        fn first(pairs: &[(bool, &'static str)]) -> Option<&'static str> {
            pairs.iter().find(|(d, _)| *d).map(|(_, n)| *n)
        }
        let (a, b) = (self, other);
        if a.decode != b.decode {
            return Some((
                "Decode",
                first(&[(a.decode.unity_wb != b.decode.unity_wb, "Unity WB")]),
            ));
        }
        if a.luminance != b.luminance {
            return Some((
                "Luminance",
                first(&[
                    (a.luminance.sampling != b.luminance.sampling, "Sampling"),
                    (a.luminance.weighting != b.luminance.weighting, "Weighting"),
                ]),
            ));
        }
        if a.exposure != b.exposure {
            return Some((
                "Exposure",
                first(&[
                    (a.exposure.enabled != b.exposure.enabled, "bypass"),
                    (a.exposure.ev != b.exposure.ev, "Exposure"),
                    (a.exposure.black != b.exposure.black, "Black corr."),
                ]),
            ));
        }
        if a.contrast_mask != b.contrast_mask {
            let cm = (&a.contrast_mask, &b.contrast_mask);
            return Some((
                "Contrast Mask",
                first(&[
                    (cm.0.enabled != cm.1.enabled, "bypass"),
                    (cm.0.contrast != cm.1.contrast, "Mask contrast"),
                    (cm.0.spacer != cm.1.spacer, "Spacer distance"),
                    (cm.0.offset != cm.1.offset, "Registration"),
                ]),
            ));
        }
        if a.dodgeburn != b.dodgeburn {
            let db = (&a.dodgeburn, &b.dodgeburn);
            return Some((
                "Dodge / Burn",
                first(&[
                    (db.0.enabled != db.1.enabled, "bypass"),
                    (db.0.instances.len() != db.1.instances.len(), "layers"),
                    (
                        db.0.instances
                            .iter()
                            .zip(&db.1.instances)
                            .any(|(a, b)| a.contrast != b.contrast),
                        "Contrast",
                    ),
                ])
                // Same count, different content: a stroke, a shape or an opacity. Named
                // for the gesture rather than guessed at field level, because a dab
                // list and a mask are not controls anybody would go looking for.
                .or(Some("layer edit")),
            ));
        }
        if a.curve != b.curve {
            return Some((
                "Curve",
                first(&[
                    (a.curve.enabled != b.curve.enabled, "bypass"),
                    (
                        a.curve.instances.len() != b.curve.instances.len(),
                        "instances",
                    ),
                    (
                        a.curve
                            .instances
                            .iter()
                            .zip(&b.curve.instances)
                            .any(|(a, b)| a.name != b.name),
                        "rename",
                    ),
                    (
                        a.curve
                            .instances
                            .iter()
                            .zip(&b.curve.instances)
                            .any(|(a, b)| a.curve.enabled != b.curve.enabled),
                        "instance bypass",
                    ),
                    (
                        a.curve
                            .instances
                            .iter()
                            .zip(&b.curve.instances)
                            .any(|(a, b)| a.opacity != b.opacity),
                        "instance opacity",
                    ),
                ])
                .or(Some("points")),
            ));
        }
        if a.composition != b.composition {
            let c = (&a.composition, &b.composition);
            return Some((
                "Composition",
                first(&[
                    (c.0.enabled != c.1.enabled, "bypass"),
                    (c.0.orientation != c.1.orientation, "Rotate"),
                    (c.0.straighten != c.1.straighten, "Straighten"),
                    (
                        c.0.keystone.mode != c.1.keystone.mode
                            || c.0.keystone.guides != c.1.keystone.guides
                            || c.0.keystone.correction != c.1.keystone.correction,
                        "Keystone",
                    ),
                    (c.0.keystone.aspect != c.1.keystone.aspect, "Aspect"),
                    (
                        c.0.ratio != c.1.ratio || c.0.portrait != c.1.portrait,
                        "Ratio",
                    ),
                    (c.0.crop != c.1.crop, "Crop"),
                ]),
            ));
        }
        if a.display != b.display {
            return Some((
                "Display",
                first(&[
                    (a.display.enabled != b.display.enabled, "bypass"),
                    (a.display.tone_map != b.display.tone_map, "Tone map"),
                    (a.display.gamma != b.display.gamma, "Monitor gamma"),
                    (a.display.dither != b.display.dither, "TPDF dither"),
                ]),
            ));
        }
        if a.grain != b.grain {
            let g = (&a.grain, &b.grain);
            return Some((
                "Grain",
                first(&[
                    (g.0.enabled != g.1.enabled, "bypass"),
                    (g.0.size != g.1.size, "Crystal size"),
                    (g.0.density != g.1.density, "Density"),
                    (g.0.layers != g.1.layers, "Layers"),
                    (g.0.variability != g.1.variability, "Variability"),
                    (g.0.sensitivity != g.1.sensitivity, "Sensitivity"),
                    (g.0.seed != g.1.seed, "Seed"),
                ]),
            ));
        }
        if a.toning != b.toning {
            let t = (&a.toning, &b.toning);
            let mix =
                t.1.process
                    .mix_label()
                    .or_else(|| t.0.process.mix_label())
                    .unwrap_or("Process mix");
            return Some((
                "Toning",
                first(&[
                    (t.0.process != t.1.process, "Process"),
                    (t.0.tone != t.1.tone, "Tone"),
                    (t.0.contrast != t.1.contrast, "Contrast"),
                    (t.0.hue != t.1.hue, "Hue"),
                    (t.0.mix != t.1.mix, mix),
                    (t.0.pigment != t.1.pigment, "Pigment"),
                    (
                        t.0.chemistry_enabled != t.1.chemistry_enabled,
                        "Chemistry bypass",
                    ),
                    (t.0.applied != t.1.applied, "Chemistry"),
                    (
                        t.0.placement_enabled != t.1.placement_enabled,
                        "Placement bypass",
                    ),
                    (t.0.placement != t.1.placement, "Placement"),
                    // Last deliberately: editing any control also arms Toning. The
                    // control is the useful name unless bypass was the only change.
                    (t.0.enabled != t.1.enabled, "bypass"),
                ]),
            ));
        }
        if a.sharpen != b.sharpen {
            let s = (&a.sharpen, &b.sharpen);
            return Some((
                "Sharpening",
                first(&[
                    (s.0.enabled != s.1.enabled, "bypass"),
                    (s.0.amount != s.1.amount, "Amount"),
                    (s.0.radius != s.1.radius, "Radius"),
                    (s.0.edges != s.1.edges, "Edges"),
                ]),
            ));
        }
        if a.output != b.output {
            return Some((
                "Output",
                first(&[
                    (a.output.ppi != b.output.ppi, "Resolution"),
                    (a.output.resize != b.output.resize, "Size"),
                    (a.output.filter != b.output.filter, "Resample"),
                ]),
            ));
        }
        if a.frame != b.frame {
            let f = (&a.frame, &b.frame);
            return Some((
                "Frame",
                first(&[
                    (f.0.enabled != f.1.enabled, "bypass"),
                    (f.0.unit != f.1.unit, "Units"),
                    (f.0.priority != f.1.priority, "Drive by"),
                    (f.0.equal != f.1.equal, "Sides"),
                    (f.0.margins != f.1.margins, "Margins"),
                    (f.0.outer_inches != f.1.outer_inches, "Frame size"),
                    (f.0.custom_size != f.1.custom_size, "Frame size"),
                    (f.0.placement != f.1.placement, "Placement"),
                    (
                        f.0.bottom_weight_inches != f.1.bottom_weight_inches,
                        "Bottom weight",
                    ),
                    (f.0.custom_position != f.1.custom_position, "Position"),
                    (f.0.color != f.1.color, "Color"),
                    (f.0.trim_line != f.1.trim_line, "Trim Line"),
                ]),
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dodgeburn::{Dab, Gesture, Instance, Shape, Sign};
    use crate::scene::DemosaicAlgo;

    /// A `Params` with every field this app can reach moved off its default, so a
    /// restore that quietly drops one has somewhere to show it.
    fn edited() -> Params {
        let mut p = Params::default();
        p.decode.unity_wb = true;
        p.luminance.sampling = Sampling::Demosaic(DemosaicAlgo::default());
        p.luminance.weighting = Weighting::Weighted(0.2, 0.3, 0.5);
        p.exposure.ev = 1.75;
        p.contrast_mask.enabled = true;
        p.dodgeburn.instances.push(Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![Gesture::new(vec![Dab {
                x: 0.5,
                y: 0.5,
                radius: 0.05,
                feather: 0.4,
                opacity: 1.0,
                ev: -0.25,
                ..Dab::ROUND
            }])]),
        ));
        p.curve.enabled = false;
        p.composition.straighten = 2.5;
        p.display.gamma = 1.8;
        p.grain.enabled = true;
        p.grain.seed = 991_234;
        p.output.ppi = 240.0;
        p.frame.enabled = true;
        p
    }

    #[test]
    fn a_restored_look_brings_the_whole_look() {
        // The failure this guards is the quiet one: a module added to `Params` and
        // not added to the restore, so a snapshot silently does not carry it.
        // `restore_look_from` is written as an exclusion for exactly this reason, and
        // this asserts the default direction is "travels".
        let mut live = Params::default();
        live.restore_look_from(&edited());
        let want = edited();
        assert_eq!(live.exposure, want.exposure, "exposure did not travel");
        assert_eq!(
            live.contrast_mask, want.contrast_mask,
            "contrast mask did not travel"
        );
        assert_eq!(live.dodgeburn, want.dodgeburn, "brushwork did not travel");
        assert_eq!(live.curve, want.curve, "the curve did not travel");
        assert_eq!(
            live.composition, want.composition,
            "composition did not travel"
        );
        assert_eq!(live.display, want.display, "display did not travel");
        assert_eq!(live.grain, want.grain, "grain did not travel");
        assert_eq!(live.output, want.output, "output did not travel");
        assert_eq!(live.frame, want.frame, "frame did not travel");
    }

    #[test]
    fn a_restored_look_leaves_the_decode_diagnostics_alone() {
        // Unity WB makes the CFA pattern visible. It is something you switch on to
        // inspect the file, so a snapshot taken while inspecting must not restore it.
        let mut live = Params::default();
        let inspecting = DecodeOptions { unity_wb: true };
        live.decode = inspecting;
        live.restore_look_from(&Params::default());
        assert_eq!(
            live.decode, inspecting,
            "restoring a look changed how the file decodes"
        );
    }

    #[test]
    fn a_restored_look_keeps_this_image_s_sampling_and_takes_the_weighting() {
        // **The boundary runs through `LuminanceParams`, not around it**, which is
        // the one place this is easy to get wrong in either direction — and they are
        // two different mistakes, so both are asserted here against one restore.
        //
        // `sampling` is the demosaic method: the prototype's `decode_mode`, a
        // property of the image. `weighting` is the RGB mix the frame is printed
        // through: a colour filter, and as much a look as the curve.
        let mut live = Params::default();
        live.luminance.sampling = Sampling::DirectMosaic;
        let snap = Params {
            luminance: LuminanceParams {
                sampling: Sampling::Demosaic(DemosaicAlgo::default()),
                weighting: Weighting::Red,
            },
            ..Params::default()
        };
        live.restore_look_from(&snap);
        assert_eq!(
            live.luminance.sampling,
            Sampling::DirectMosaic,
            "the snapshot's demosaic method was restored onto the image"
        );
        assert_eq!(
            live.luminance.weighting,
            Weighting::Red,
            "the snapshot's colour filter did not travel"
        );
    }

    #[test]
    fn restoring_a_look_can_never_ask_for_a_re_decode() {
        // Not the reason for the exclusions, but a consequence worth pinning: the
        // decode tier is the slowest in `diff`, and because neither excluded field
        // can move, a restore cannot reach it however different the snapshot is.
        //
        // The luminance tier *can* still be crossed — a weighting change is a real
        // CPU re-derive — so this asserts the bound rather than that restore is free.
        //
        // **The live decode must differ from the snapshot's or this test cannot
        // fail.** The first draft set it to the same value `edited()` carries, so
        // dropping the exclusion would have assigned an identical value and `diff`
        // would have reported nothing — a test that passes for both answers. The
        // brief warns about exactly this shape and it took two minutes to write it
        // anyway, so the disagreement is asserted first and the claim second.
        let snap = edited();
        let mut live = Params::default();
        live.luminance.sampling = Sampling::DirectMosaic;
        assert_ne!(
            live.decode, snap.decode,
            "the test is not armed: nothing to restore wrongly"
        );
        assert_ne!(
            live.luminance.sampling, snap.luminance.sampling,
            "nor is the sampling"
        );

        let before = live.clone();
        live.restore_look_from(&snap);
        let dirty = before.diff(&live);
        assert!(
            !dirty.decode,
            "a restore asked for a re-decode — one of the two exclusions has moved"
        );
        assert!(
            dirty.luminance,
            "the luminance tier was not crossed, so this is asserting a bound that \
             nothing tested — the snapshot's weighting should have re-derived"
        );
    }

    #[test]
    fn a_history_row_names_the_module_you_would_scroll_to() {
        // Each module in turn, so a row that fell through to the wrong name — or to
        // `None` — shows up as the module it belongs to rather than as "something".
        let base = Params::default();
        type Edit = fn(&mut Params);
        let cases: [(Edit, &str); 9] = [
            (|p| p.decode.unity_wb = true, "Decode"),
            (|p| p.luminance.weighting = Weighting::Red, "Luminance"),
            (|p| p.exposure.ev = 1.0, "Exposure"),
            (|p| p.contrast_mask.enabled = true, "Contrast Mask"),
            (|p| p.curve.enabled = false, "Curve"),
            (|p| p.composition.straighten = 1.0, "Composition"),
            (|p| p.display.gamma = 1.9, "Display"),
            (|p| p.output.ppi = 240.0, "Output"),
            (|p| p.frame.enabled = true, "Frame"),
        ];
        for (edit, want) in cases {
            let mut after = base.clone();
            edit(&mut after);
            assert_ne!(
                after, base,
                "the fixture for {want} did not change anything"
            );
            assert_eq!(base.what_changed(&after).map(|(m, _)| m), Some(want));
        }
        assert_eq!(
            base.what_changed(&base),
            None,
            "two identical states named a change"
        );
    }

    #[test]
    fn editing_after_going_back_throws_the_future_away() {
        // Photoshop's rule, and the maintainer asked for it explicitly. Going back in time keeps
        // the entries ahead of you — that is what lets the panel list them and let you
        // walk forward again — but the moment you *edit* from there, the branch you
        // stepped off is gone. Anything else and the panel would offer a redo that
        // rebuilt a state the current one no longer descends from.
        let mut h = History::default();
        let mut live = Params::default();
        for ev in 1..=3 {
            let before = live.clone();
            live.exposure.ev = ev as f32;
            h.record(before, &live, true);
        }
        h.jump(&mut live, -2);
        assert_eq!(
            h.depth(),
            (1, 2),
            "going back must keep the future to walk into"
        );

        // Now edit from here.
        let before = live.clone();
        live.display.gamma = 1.9;
        h.record(before, &live, true);
        assert_eq!(h.depth().1, 0, "the abandoned branch survived an edit");
        assert!(
            !h.can_redo(),
            "redo would rebuild a state this one does not descend from"
        );
    }

    #[test]
    fn history_revision_moves_even_when_the_capped_depth_cannot() {
        let mut history = History::default();
        let mut live = Params::default();
        for step in 1..=History::CAP {
            let before = live.clone();
            live.exposure.ev = step as f32;
            history.record(before, &live, true);
        }
        let depth = history.depth();
        let revision = history.revision();

        let before = live.clone();
        live.exposure.ev += 1.0;
        history.record(before, &live, true);
        assert_eq!(history.depth(), depth, "the fixture did not reach the cap");
        assert_ne!(
            history.revision(),
            revision,
            "a same-length timeline change would not wake the following panel"
        );
    }

    #[test]
    fn brush_payload_is_shared_across_the_full_history_and_copies_on_write() {
        use crate::dodgeburn::DodgeBurnParams;
        use std::sync::Arc;

        fn dabs(p: &Params) -> &Arc<Vec<Dab>> {
            &p.dodgeburn.instances[0].gestures()[0].dabs
        }

        let stroke = Gesture::new(
            (0..10_000)
                .map(|i| Dab {
                    x: i as f32 / 10_000.0,
                    y: 0.5,
                    radius: 0.01,
                    feather: 0.4,
                    opacity: 1.0,
                    ev: -0.25,
                    ..Dab::ROUND
                })
                .collect(),
        );
        let mut live = Params {
            dodgeburn: DodgeBurnParams {
                enabled: true,
                instances: vec![Instance::of(
                    Sign::Burn,
                    "Burn 1".into(),
                    Shape::brush(vec![stroke]),
                )],
            },
            ..Params::default()
        };
        let original = Arc::clone(dabs(&live));
        let mut history = History::default();

        for step in 1..=History::CAP {
            let before = live.clone();
            live.exposure.ev = step as f32;
            history.record(before, &live, true);
        }
        assert!(
            history
                .past()
                .iter()
                .all(|state| Arc::ptr_eq(dabs(state), &original)),
            "retained states duplicated an unchanged brush payload"
        );

        let before = live.clone();
        live.dodgeburn.instances[0].gestures_mut().unwrap()[0]
            .dabs_mut()
            .push(Dab {
                x: 1.0,
                ..Dab::ROUND
            });
        assert!(!Arc::ptr_eq(dabs(&live), &original));
        assert_eq!(original.len(), 10_000, "copy-on-write changed an old state");
        history.record(before, &live, true);

        assert!(history.undo(&mut live));
        assert_eq!(dabs(&live).len(), 10_000);
        assert!(history.redo(&mut live));
        assert_eq!(dabs(&live).len(), 10_001);
    }

    #[test]
    fn a_history_row_names_the_control_and_not_only_the_module() {
        // the maintainer's complaint, as an assertion: three rows reading "Exposure" say nothing
        // when one of them was the black point. The control names here are the develop
        // panel's own strings, deliberately — the use of a history row is to tell you
        // which slider to go back to, so a row naming a control the panel does not have
        // is a row that sends you looking for something that is not there.
        let base = Params::default();
        type Edit = fn(&mut Params);
        let cases: [(Edit, &str, &str); 9] = [
            (|p| p.exposure.ev = 1.0, "Exposure", "Exposure"),
            (|p| p.exposure.black = 0.05, "Exposure", "Black corr."),
            (|p| p.exposure.enabled = false, "Exposure", "bypass"),
            (
                |p| p.contrast_mask.spacer = 2.0,
                "Contrast Mask",
                "Spacer distance",
            ),
            (|p| p.display.gamma = 1.9, "Display", "Monitor gamma"),
            (
                |p| p.display.dither = !p.display.dither,
                "Display",
                "TPDF dither",
            ),
            (|p| p.grain.seed = 7, "Grain", "Seed"),
            (|p| p.grain.size = 9, "Grain", "Crystal size"),
            (
                |p| p.composition.straighten = 2.0,
                "Composition",
                "Straighten",
            ),
        ];
        for (edit, module, control) in cases {
            let mut after = base.clone();
            edit(&mut after);
            assert_ne!(
                after, base,
                "the fixture for {module} · {control} changed nothing"
            );
            assert_eq!(
                base.what_changed(&after),
                Some((module, Some(control))),
                "{module} · {control} was misnamed"
            );
        }
    }

    #[test]
    fn every_toning_edit_has_the_name_of_the_control_that_made_it() {
        let base = Params::default();
        type Edit = fn(&mut Params);
        let cases: [(Edit, &str); 11] = [
            (|p| p.toning.enabled = true, "bypass"),
            (
                |p| p.toning.process = crate::toning::Process::Albumen,
                "Process",
            ),
            (|p| p.toning.tone = 1.2, "Tone"),
            (|p| p.toning.contrast = 1.2, "Contrast"),
            (|p| p.toning.hue = 12.0, "Hue"),
            (|p| p.toning.mix = 0.7, "Process mix"),
            (|p| p.toning.pigment = 0.7, "Pigment"),
            (|p| p.toning.chemistry_enabled = false, "Chemistry bypass"),
            (
                |p| {
                    p.toning.apply("selenium", 0.6);
                },
                "Chemistry",
            ),
            (|p| p.toning.placement_enabled = false, "Placement bypass"),
            (|p| p.toning.placement.move_point(0, 0.0, 0.25), "Placement"),
        ];
        for (edit, control) in cases {
            let mut after = base.clone();
            edit(&mut after);
            assert_eq!(
                base.what_changed(&after),
                Some(("Toning", Some(control))),
                "Toning · {control} was not named"
            );
        }

        // A process-specific mix uses the label the panel shows rather than the
        // generic fallback used while the current process has no mix control.
        let mut process = base.clone();
        process.toning.process = crate::toning::Process::PlatinumPalladium;
        let mut mixed = process.clone();
        mixed.toning.mix = 0.7;
        assert_eq!(
            process.what_changed(&mixed),
            Some(("Toning", Some("Platinum → Palladium")))
        );
    }

    #[test]
    fn every_module_can_name_something() {
        // The failure this catches is a module whose fields are all unnamed: it would
        // return `Some((module, None))` and the panel would draw a bare module name
        // with no control — which is what the whole change was to get away from. Each
        // module is edited in a way its own list has to be able to describe.
        let base = Params::default();
        type Edit = fn(&mut Params);
        let cases: [(Edit, &str); 5] = [
            (|p| p.decode.unity_wb = true, "Decode"),
            (|p| p.luminance.weighting = Weighting::Red, "Luminance"),
            (|p| p.curve.enabled = false, "Curve"),
            (|p| p.output.ppi = 240.0, "Output"),
            (|p| p.frame.enabled = true, "Frame"),
        ];
        for (edit, module) in cases {
            let mut after = base.clone();
            edit(&mut after);
            let got = base.what_changed(&after);
            assert_eq!(got.map(|(m, _)| m), Some(module));
            assert!(
                got.and_then(|(_, c)| c).is_some(),
                "{module} named no control"
            );
        }
    }

    #[test]
    fn jumping_back_and_forward_lands_where_the_row_was_clicked() {
        // What clicking row *n* does. Expressed as repeated undo/redo rather than as an
        // index into the vectors, so this is really asserting that the two agree about
        // the timeline — and that overshooting stops rather than panicking, which is a
        // click on a row `CAP` has since trimmed.
        let mut h = History::default();
        let mut live = Params::default();
        for ev in 1..=3 {
            let before = live.clone();
            live.exposure.ev = ev as f32;
            h.record(before, &live, true);
        }
        assert_eq!(h.depth(), (3, 0));

        h.jump(&mut live, -2);
        assert_eq!(
            live.exposure.ev, 1.0,
            "two back from the third edit is the first"
        );
        assert_eq!(h.depth(), (1, 2));

        h.jump(&mut live, 1);
        assert_eq!(
            live.exposure.ev, 2.0,
            "one forward did not land on the second"
        );

        // Past either end: stops, keeps the state it reached, and does not panic.
        h.jump(&mut live, -99);
        assert_eq!(
            live.exposure.ev, 0.0,
            "the far end is the state before any edit"
        );
        h.jump(&mut live, 99);
        assert_eq!(live.exposure.ev, 3.0, "and the other end is the last edit");
    }

    #[test]
    fn the_spacer_is_the_same_fraction_of_the_frame_at_any_resolution() {
        // The defect that made percent the unit, as an assertion. Everything
        // downstream measures in pixels of the LUMINANCE image, and SuperPixel is
        // half resolution — so with a pixel spacer, switching sampling mode
        // silently doubled the mask's reach relative to the frame. Here the
        // sigma doubles with the frame instead, which is the same physical mask.
        let cm = ContrastMaskParams {
            spacer: 1.0,
            ..Default::default()
        };
        assert!(
            (cm.spacer_px((6000, 8000)) - 100.0).abs() < 1e-3,
            "1% of a 10000px diagonal"
        );
        assert!(
            (cm.spacer_px((3000, 4000)) - 50.0).abs() < 1e-3,
            "half the frame, half the sigma"
        );
    }

    #[test]
    fn the_spacer_does_not_care_which_way_up_the_frame_is() {
        // Why the reference is the diagonal and not the width: portrait and
        // landscape are the same negative and must mask identically. A
        // percent-of-width spacer fails this, which is the trap being pinned.
        let cm = ContrastMaskParams {
            spacer: 2.5,
            ..Default::default()
        };
        assert_eq!(cm.spacer_px((6000, 8000)), cm.spacer_px((8000, 6000)));
    }

    #[test]
    fn params_are_a_value() {
        // Clone + PartialEq is the whole contract that makes undo and duplication
        // work. If this stops compiling, the design regressed.
        let a = Params::default();
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn display_change_does_not_redecode() {
        let a = Params::default();
        let mut b = a.clone();
        b.display.gamma = 2.4;
        let d = a.diff(&b);
        assert!(d.render, "gamma must re-render");
        assert!(
            !d.decode && !d.luminance && !d.curve,
            "gamma must not touch decode: {d:?}"
        );
    }

    #[test]
    fn exposure_change_is_render_only() {
        let a = Params::default();
        let mut b = a.clone();
        b.exposure.ev = 1.5;
        let d = a.diff(&b);
        assert_eq!(
            d,
            Dirty {
                decode: false,
                luminance: false,
                curve: false,
                render: true
            }
        );
    }

    #[test]
    fn curve_change_rebakes_but_does_not_rederive_luma() {
        let a = Params::default();
        let mut b = a.clone();
        b.curve.add(0.5, 0.6);
        let d = a.diff(&b);
        assert!(d.curve && d.render);
        assert!(!d.decode && !d.luminance);
    }

    #[test]
    fn naming_a_curve_is_authored_state_but_not_a_render_change() {
        let a = Params::default();
        let mut b = a.clone();
        b.curve.instances[0].name = "Portrait contrast".into();
        assert_ne!(a, b, "the name must travel through history and the sidecar");
        assert_eq!(a.diff(&b), Dirty::NONE, "a label uploaded pixels");
        assert_eq!(a.what_changed(&b), Some(("Curve", Some("rename"))));
    }

    #[test]
    fn curve_instance_opacity_rebakes_and_names_itself_in_history() {
        let mut a = Params::default();
        a.curve.instances[0].curve.add(0.5, 0.65);
        let mut b = a.clone();
        b.curve.instances[0].opacity = 0.4;
        let dirty = a.diff(&b);
        assert!(dirty.curve && dirty.render);
        assert!(!dirty.decode && !dirty.luminance);
        assert_eq!(
            a.what_changed(&b),
            Some(("Curve", Some("instance opacity")))
        );
    }

    #[test]
    fn sampling_change_rederives_luma_without_redecoding() {
        // The scene is the cache boundary: changing how luminance is derived from it
        // must not re-run decode.
        let a = Params::default();
        let mut b = a.clone();
        b.luminance.sampling = Sampling::Demosaic(DemosaicAlgo::Bilinear);
        let d = a.diff(&b);
        assert!(d.luminance && d.render);
        assert!(!d.decode, "sampling must not re-decode");
    }

    #[test]
    fn decode_change_cascades_all_the_way_down() {
        let a = Params::default();
        let mut b = a.clone();
        b.decode.unity_wb = true;
        let d = a.diff(&b);
        assert!(
            d.decode && d.luminance && d.render,
            "decode must cascade: {d:?}"
        );
    }

    #[test]
    fn dragging_a_crop_handle_does_not_re_derive_anything() {
        // The handoff's second sentence, as an assertion:
        //
        //   > the tone chain runs on the uncropped image; crop applies at the end
        //   > for display and export. This also makes crop cheap to change, since
        //   > it does not invalidate the tone chain.
        //
        // Sixty frames a second of this run during a handle drag. Reaching the
        // luminance tier would mean a full-frame CPU pass per frame on a 100 MP
        // file, which is the difference between a crop tool and an unusable one.
        let a = Params::default();
        let mut b = a.clone();
        b.composition.crop = crate::composition::Rect {
            x: 0.1,
            y: 0.1,
            w: 0.5,
            h: 0.5,
        };
        assert_eq!(
            a.diff(&b),
            Dirty {
                decode: false,
                luminance: false,
                curve: false,
                render: true
            }
        );

        // And the same for the other two, which change the frame's dimensions and
        // still cost nothing above the sink.
        let mut c = a.clone();
        c.composition.orientation = Some(crate::composition::Orientation::Rotate90);
        assert!(!a.diff(&c).luminance, "a quarter turn re-derived luminance");
        let mut d = a.clone();
        d.composition.straighten = 2.5;
        assert!(!a.diff(&d).luminance, "a straighten re-derived luminance");
    }

    #[test]
    fn a_brush_stroke_does_not_re_derive_anything() {
        // 9a's payoff, written for the module that would hurt most without it. A
        // drag deposits a dab per frame; on a 100 MP file a per-frame CPU pass
        // would make the tool unusable rather than slow.
        use crate::dodgeburn::{Dab, DodgeBurnParams, Gesture, Instance, Shape, Sign};
        let dab = |x: f32| Dab {
            x,
            y: 0.5,
            radius: 0.06,
            feather: 0.4,
            opacity: 1.0,
            ev: -0.25,
            ..Dab::ROUND
        };
        fn passes(p: &mut Params) -> &mut Vec<Gesture> {
            p.dodgeburn.instances[0]
                .gestures_mut()
                .expect("a brush instance")
        }

        let a = Params {
            dodgeburn: DodgeBurnParams {
                enabled: true,
                instances: vec![Instance::of(
                    Sign::Burn,
                    "Burn 1".into(),
                    Shape::brush(vec![Gesture::new(vec![dab(0.40)])]),
                )],
            },
            ..Default::default()
        };

        // One more dab on the gesture in flight — the thing that happens sixty
        // times a second.
        let mut b = a.clone();
        passes(&mut b)[0].dabs_mut().push(dab(0.41));
        assert_eq!(
            a.diff(&b),
            Dirty {
                decode: false,
                luminance: false,
                curve: false,
                render: true
            }
        );

        // And so must every other way the module changes: a new pass, a new
        // instance, the opacity slider, and the zone mask — whose proxy is CPU
        // work, but CPU work keyed on the signal ENTERING this stage, which none
        // of these touch. If any of them ever needs a tier of its own, the
        // feed-forward property has been broken somewhere upstream.
        let mut c = a.clone();
        passes(&mut c).push(Gesture::new(vec![dab(0.6)]));
        assert!(!a.diff(&c).luminance && a.diff(&c).render, "a second pass");

        let mut d = a.clone();
        d.dodgeburn.instances[0].opacity = 0.5;
        assert!(!a.diff(&d).luminance && a.diff(&d).render, "master opacity");

        let mut e = a.clone();
        e.dodgeburn.instances[0].mask.enabled = true;
        e.dodgeburn.instances[0].mask.hi = 0.0;
        assert!(!a.diff(&e).luminance && a.diff(&e).render, "a zone mask");

        let mut f = a.clone();
        f.dodgeburn.instances[0].contrast = 0.5;
        assert!(!a.diff(&f).luminance && a.diff(&f).render, "local contrast");
        assert_eq!(a.what_changed(&f), Some(("Dodge / Burn", Some("Contrast"))));
    }

    #[test]
    fn a_bypassed_dodge_and_burn_renders_as_though_it_were_empty() {
        // Bypass is `Params::effective` substituting the default, which for this
        // module means an empty instance list — so the graph emits no node at all
        // rather than a dispatch that multiplies every pixel by 2^0.
        use crate::dodgeburn::{Dab, DodgeBurnParams, Gesture, Instance, Shape, Sign};
        let strokes = DodgeBurnParams {
            enabled: false,
            instances: vec![Instance::of(
                Sign::Burn,
                "Burn 1".into(),
                Shape::brush(vec![Gesture::new(vec![Dab {
                    x: 0.5,
                    y: 0.5,
                    radius: 0.1,
                    feather: 0.4,
                    opacity: 1.0,
                    ev: -1.0,
                    ..Dab::ROUND
                }])]),
            )],
        };
        let p = Params {
            dodgeburn: strokes,
            ..Default::default()
        };
        assert!(
            p.effective().dodgeburn.instances.is_empty(),
            "bypassed must render as untouched"
        );
        // And the stored params keep the work, so switching back on returns to it.
        assert_eq!(p.dodgeburn.total_dabs(), 1);
    }

    #[test]
    fn a_toning_edit_is_a_render_change_and_no_more() {
        // Toning is a transfer function, and a transfer function is a table lookup
        // however elaborate the thing that built the table was. The chemistry runs on
        // the CPU once per edit; nothing upstream of the tone map can hear about it.
        //
        // The failure this guards is filing it higher out of caution — it looks like
        // colour, and colour sounds expensive. A stack edit that reached `luminance`
        // would re-derive a 100 MP frame to change a lookup table.
        let a = Params::default();
        let each: [fn(&mut Params); 3] = [
            |p| p.toning.enabled = true,
            |p| p.toning.process = crate::toning::Process::Cyanotype,
            |p| {
                p.toning.apply("selenium", 0.6);
            },
        ];
        for (i, edit) in each.into_iter().enumerate() {
            let mut b = a.clone();
            edit(&mut b);
            assert_ne!(
                a, b,
                "edit {i} changed nothing — the test would pass vacuously"
            );
            let d = a.diff(&b);
            assert!(d.render, "edit {i} must re-render");
            assert!(
                !d.decode && !d.luminance && !d.curve,
                "edit {i} reached too far: {d:?}"
            );
        }
    }

    #[test]
    fn an_output_change_is_not_a_render_change() {
        // The 9b brief: what the FILE will be must not move what the VIEW shows. Every
        // field of `OutputParams` is a file property, so none of them may reach any
        // tier — not even `render`, which is otherwise where "cheap but real" changes
        // land. A user who types a print size and watches the histogram twitch has
        // been told the two are connected, and they are not.
        let a = Params::default();
        let each: [fn(&mut Params); 3] = [
            |p| p.output.ppi = 600.0,
            |p| {
                p.output.resize = Some(crate::output::Resize {
                    inches: 8.0,
                    axis: crate::output::Axis::Height,
                })
            },
            |p| p.output.filter = crate::resample::Filter::Mitchell,
        ];
        for (i, edit) in each.into_iter().enumerate() {
            let mut b = a.clone();
            edit(&mut b);
            assert_ne!(
                a, b,
                "edit {i} changed nothing — the test would pass vacuously"
            );
            assert_eq!(a.diff(&b), Dirty::NONE, "edit {i} reached the pipeline");
        }
    }

    #[test]
    fn a_grain_change_is_not_a_render_change() {
        // Grain is export-only: it runs on the CPU after the tone map, downstream of
        // every GPU node. A slider here that reached `render` would re-dispatch the
        // whole viewport to produce a frame identical to the one already on screen —
        // and on a 100 MP file that is a visible stall in exchange for nothing.
        //
        // Every field, not just one, because the trap is a field added later and
        // filed by hand into a diff that already had a grain line in it.
        let a = Params::default();
        let each: [fn(&mut Params); 7] = [
            |p| p.grain.enabled = true,
            |p| p.grain.set_size(13),
            |p| p.grain.density = 0.6,
            |p| p.grain.layers = 60,
            |p| p.grain.variability = 1.2,
            |p| p.grain.sensitivity = -1.5,
            |p| p.grain.seed = 7,
        ];
        for (i, edit) in each.into_iter().enumerate() {
            let mut b = a.clone();
            edit(&mut b);
            assert_ne!(
                a, b,
                "edit {i} changed nothing — the test would pass vacuously"
            );
            assert_eq!(a.diff(&b), Dirty::NONE, "edit {i} reached the pipeline");
        }
    }

    #[test]
    fn a_sharpen_change_is_not_a_render_change() {
        // Output sharpening is export-only for a reason `output`'s own filing already
        // states: its radius is in OUTPUT pixels, so it is defined on a grid the
        // viewport does not render. A slider here that reached `render` would
        // re-dispatch the viewport to produce an identical frame — and worse, it would
        // make the view depend on the print size, which is the leak
        // `an_output_change_is_not_a_render_change` exists to stop.
        let a = Params::default();
        let each: [fn(&mut Params); 4] = [
            |p| p.sharpen.enabled = true,
            |p| p.sharpen.amount = 1.5,
            |p| p.sharpen.radius = 3.0,
            |p| p.sharpen.edges = 0.0,
        ];
        for (i, edit) in each.into_iter().enumerate() {
            let mut b = a.clone();
            edit(&mut b);
            assert_ne!(
                a, b,
                "edit {i} changed nothing — the test would pass vacuously"
            );
            assert_eq!(a.diff(&b), Dirty::NONE, "edit {i} reached the pipeline");
        }
    }

    #[test]
    fn a_bypassed_sharpen_costs_nothing_rather_than_running_and_being_discarded() {
        // Grain's argument exactly, with a wavelet decomposition in place of an
        // emulsion: `export::write` consults `is_active`, and a bypassed module that
        // still carried an amount would pay for four full-resolution blurs over the
        // export buffer to hand back its own input.
        let mut p = Params::default();
        p.sharpen.enabled = true;
        p.sharpen.amount = 1.5;
        p.sharpen.radius = 3.0;
        assert!(
            p.effective().sharpen.is_active(),
            "a switched-on sharpen must run"
        );

        p.sharpen.enabled = false;
        assert!(
            !p.effective().sharpen.is_active(),
            "a bypassed sharpen must not run"
        );
        assert_eq!(
            p.sharpen.amount, 1.5,
            "the stored amount survived being bypassed"
        );
        assert_eq!(p.sharpen.radius, 3.0);
    }

    #[test]
    fn a_bypassed_grain_costs_nothing_rather_than_running_and_being_discarded() {
        // Bypass is `Params::effective` substituting the default, and the default is
        // off — so `export::write` skips the emulsion entirely. Getting this wrong
        // would not show in the file, only in five seconds of an export that had no
        // grain in it anyway, which is exactly the kind of cost nobody goes looking
        // for. The stored values survive, so switching back on returns to the edit.
        let mut p = Params::default();
        p.grain.enabled = true;
        p.grain.set_size(15);
        p.grain.layers = 60;
        assert!(
            p.effective().grain.is_active(),
            "a switched-on grain must run"
        );

        p.grain.enabled = false;
        assert!(
            !p.effective().grain.is_active(),
            "a bypassed grain must not run"
        );
        assert_eq!(p.grain.size, 15, "the stored size survived being bypassed");
        assert_eq!(p.grain.layers, 60);
    }

    #[test]
    fn the_crop_tool_suppresses_the_crop_and_nothing_else() {
        // What the tool renders while it is open: the full frame, the right way up,
        // so parts outside the current crop can be seen and re-grabbed. The stored
        // rectangle is untouched — the handles are drawn where it still is.
        let mut p = Params::default();
        p.composition.orientation = Some(crate::composition::Orientation::Rotate90);
        p.composition.straighten = 3.0;
        p.composition.crop = crate::composition::Rect {
            x: 0.2,
            y: 0.2,
            w: 0.4,
            h: 0.4,
        };

        let u = p.uncropped();
        assert!(
            u.composition.crop.is_full(),
            "the crop is still being applied"
        );
        assert_eq!(
            u.composition.straighten, 3.0,
            "the picture must stay straightened"
        );
        assert_eq!(u.composition.orientation, p.composition.orientation);
        assert_eq!(
            p.composition.crop.w, 0.4,
            "the stored rectangle must survive"
        );
    }

    #[test]
    fn identical_params_are_clean() {
        let a = Params::default();
        assert!(!a.diff(&a.clone()).any());
    }

    #[test]
    fn a_bypassed_module_renders_as_its_default() {
        // The whole of module bypass, as an assertion. Nothing downstream knows
        // what "off" means; it is handed a default and behaves accordingly.
        let mut p = Params::default();
        p.exposure.ev = 2.0;
        p.exposure.black = 0.05;
        p.curve.add(0.5, 0.7);
        p.contrast_mask.enabled = true;
        p.contrast_mask.contrast = 0.5;
        p.display.tone_map = ToneMap::AGX_DEFAULT;

        p.exposure.enabled = false;
        p.curve.enabled = false;
        p.contrast_mask.enabled = false;
        p.display.enabled = false;

        let e = p.effective();
        assert!(
            e.exposure.is_default(),
            "a bypassed exposure must render as neutral"
        );
        assert!(
            e.curve.is_identity(),
            "a bypassed curve must render as a straight line"
        );
        assert!(!e.contrast_mask.is_active());
        assert_eq!(
            e.display.tone_map,
            ToneMap::Clip,
            "a bypassed display must remove the creative tone map"
        );
        assert_eq!(
            e.display.gamma, p.display.gamma,
            "display bypass must retain the required monitor transfer"
        );
        assert_eq!(
            p.display.tone_map,
            ToneMap::AGX_DEFAULT,
            "the selected display transform must survive the comparison"
        );
    }

    #[test]
    fn bypass_keeps_the_edit_it_is_bypassing() {
        // Why the dot is not a reset: switching a module back on must return to the
        // edit. If `effective` ever mutated in place this would be destructive, and
        // the A/B the control exists for would be a one-way trip.
        let mut p = Params::default();
        p.exposure.ev = 2.0;
        p.curve.add(0.5, 0.7);
        p.exposure.enabled = false;
        p.curve.enabled = false;

        let _ = p.effective();
        assert_eq!(
            p.exposure.ev, 2.0,
            "the stored value survived being bypassed"
        );
        assert!(
            !p.curve.is_identity(),
            "the stored curve survived being bypassed"
        );
    }

    #[test]
    fn bypassing_a_neutral_module_is_not_an_edit() {
        // The dot reports two independent facts — modified, and switched off — and
        // conflating them would light the modified state on a module nobody has
        // touched. `is_default` must not be able to see the switch.
        let mut p = Params::default();
        p.exposure.enabled = false;
        assert!(
            p.exposure.is_default(),
            "switching off is not a modification"
        );
        assert!(
            !p.exposure.is_active(),
            "but it does stop the module running"
        );

        let mut cm = ContrastMaskParams::default();
        cm.enabled = !cm.enabled;
        assert!(cm.is_default());
    }

    #[test]
    fn switching_on_a_module_that_defaults_off_lights_the_dot() {
        // the maintainer: "Contrast Mask is default Off — but when it is turned on, the icon does
        // not turn red. It only turns red once you tune it from default."
        //
        // He is right, and the reason is that the old rule asked "has a value moved",
        // which for a module that is off by default misses the only edit that matters.
        // Contrast Mask switched on at its defaults changes every pixel in the frame.
        let off = ContrastMaskParams::default();
        assert!(!off.is_modified(), "off and untouched is not modified");
        let on = ContrastMaskParams {
            enabled: true,
            ..Default::default()
        };
        assert!(
            on.is_modified(),
            "switched on at defaults must light the dot"
        );
        assert!(on.is_default(), "and it is still at its default values");
    }

    #[test]
    fn switching_off_a_neutral_module_still_does_not_light_the_dot() {
        // The other half of the widened rule, and the reason it is stated as "would
        // render differently than at defaults" rather than as "the switch moved".
        // Exposure at 0 EV renders the same either way, so neither state is an edit —
        // which is exactly what the narrower rule got right and must keep getting right.
        let off = ExposureParams {
            enabled: false,
            ..Default::default()
        };
        assert!(
            !off.is_modified(),
            "a bypassed neutral module is not modified"
        );
        let edited = ExposureParams {
            enabled: false,
            ev: 2.0,
            ..Default::default()
        };
        assert!(edited.is_modified(), "but a bypassed edit is");
    }

    #[test]
    fn toggling_a_bypass_re_renders() {
        // Bypass is on `Params`, so it flows through the ordinary diff — no call
        // site has to remember to invalidate anything. The curve's also re-bakes,
        // because the LUT it is switching to is a different LUT.
        let a = Params::default();
        let mut b = a.clone();
        b.exposure.enabled = false;
        assert!(a.diff(&b).render, "an exposure bypass must re-render");

        let mut c = a.clone();
        c.curve.enabled = false;
        let d = a.diff(&c);
        assert!(
            d.curve && d.render,
            "a curve bypass must re-bake and re-render: {d:?}"
        );
        assert!(!d.decode && !d.luminance, "and must not re-decode");

        let mut e = a.clone();
        e.display.enabled = false;
        let d = a.diff(&e);
        assert!(d.render, "a display bypass must re-render: {d:?}");
        assert!(
            !d.decode && !d.luminance && !d.curve,
            "display comparison is downstream of decode, luminance, and curve: {d:?}"
        );
    }

    #[test]
    fn a_drag_produces_one_undo_entry() {
        // The coalescing contract. Sixty frames of slider drag is one gesture.
        let mut h = History::default();
        let mut p = Params::default();
        for i in 1..=60 {
            let before = p.clone();
            p.exposure.ev = i as f32 * 0.01;
            h.record(before, &p, false); // pointer still down
        }
        h.record(p.clone(), &p, true); // pointer released, no further change
        assert_eq!(h.depth(), (1, 0), "a drag must be one entry");

        assert!(h.undo(&mut p));
        assert_eq!(p.exposure.ev, 0.0, "undo must return to the pre-drag state");
    }

    #[test]
    fn undo_redo_round_trips() {
        let mut h = History::default();
        let mut p = Params::default();

        let before = p.clone();
        p.exposure.ev = 2.0;
        h.record(before, &p, true);

        let before = p.clone();
        p.display.gamma = 1.8;
        h.record(before, &p, true);

        assert!(h.undo(&mut p));
        assert_eq!(p.display.gamma, 2.2);
        assert_eq!(p.exposure.ev, 2.0);

        assert!(h.undo(&mut p));
        assert_eq!(p.exposure.ev, 0.0);
        assert!(!h.can_undo());

        assert!(h.redo(&mut p));
        assert_eq!(p.exposure.ev, 2.0);
        assert!(h.redo(&mut p));
        assert_eq!(p.display.gamma, 1.8);
        assert!(!h.can_redo());
    }

    #[test]
    fn a_new_edit_clears_the_redo_branch() {
        let mut h = History::default();
        let mut p = Params::default();

        let before = p.clone();
        p.exposure.ev = 1.0;
        h.record(before, &p, true);
        h.undo(&mut p);
        assert!(h.can_redo());

        let before = p.clone();
        p.display.gamma = 2.0;
        h.record(before, &p, true);
        assert!(!h.can_redo(), "branching must drop the old redo path");
    }

    #[test]
    fn undo_mid_gesture_undoes_the_whole_gesture() {
        let mut h = History::default();
        let mut p = Params::default();
        for i in 1..=10 {
            let before = p.clone();
            p.exposure.ev = i as f32;
            h.record(before, &p, false);
        }
        // Never settled -- undo while the drag is notionally still live.
        assert!(h.undo(&mut p));
        assert_eq!(p.exposure.ev, 0.0);
    }

    #[test]
    fn no_change_records_nothing() {
        let mut h = History::default();
        let p = Params::default();
        for _ in 0..10 {
            h.record(p.clone(), &p, true);
        }
        assert_eq!(h.depth(), (0, 0));
        assert!(!h.can_undo());
    }
}
