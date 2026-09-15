//! Export encoders.
//!
//! # Where the branch is, and why
//!
//! The GPU chain hands back **scene-referred f32**, post-curve. Everything up to
//! that point is shared with the viewport; everything here is container-specific:
//!
//! ```text
//!   scene f32 ──┬── tone map → gamma → TPDF dither → 8-bit   screen
//!               └── tone map → L*    → 8 or 16-bit          file + monostar.icc
//! ```
//!
//! The two do **not** share a raw encoding, and must not. A colour-managed viewer
//! decodes both to the same linear light — that is what makes them agree. Exporting
//! the display tail instead would ship a dithered approximation of a monitor and
//! call it a master.
//!
//! This is also why L\* appears here and nowhere else. The prototype once
//! L\*-encoded for *display*, the macOS compositor decoded it as sRGB, and midtones
//! lifted ~3 L\* against a correctly tagged TIFF. The fix included renaming the
//! symbol so a stale build could not silently reintroduce it; keeping L\* inside the
//! export module is the structural version of that discipline.
//!
//! # Masters and proofs are encoded for where they are going
//!
//! **Purpose decides the encoding.** A master goes into a colour-managed print
//! workflow, so it is greyscale, L\*-encoded and tagged `monostar.icc`. A proof goes
//! to a screen nobody has profiled — mail, a browser, a phone — so it is
//! sRGB-encoded, and it carries three identical channels and `sRGB2014.icc` so that
//! the profile is valid and every application that opens it sees the intent.
//!
//! That is deliberately **not** the split Output removed, which was *depth*
//! deciding encoding — "8-bit means gamma" — and had no principle behind it. This
//! rule is one sentence and survives the colour transition unchanged.
//!
//! # One master encoding, two axes
//!
//! Every **master** is greyscale, L\*-encoded, tagged `monostar.icc`. What varies is
//! only the container and the depth:
//!
//! | | 8-bit | 16-bit |
//! |---|---|---|
//! | **TIFF** | small master, dithered | the master |
//! | **PNG** | compressed, dithered | compressed master |
//!
//! Collapsing "proof" and "master" into container × depth is what keeps this
//! honest. An earlier version had the 8-bit path go out as display-gamma RGB, which
//! meant two different encodings and two different colour behaviours to reason
//! about.
//!
//! **8-bit L\* is a genuinely good deliverable, not a degraded one.** L\* is
//! perceptually uniform by construction, so it distributes 256 codes evenly across
//! *perceived* lightness — a better use of 8 bits for a monochrome image than gamma
//! 2.2, which spends too many codes in the highlights. With TPDF dither on top, the
//! banding that normally makes 8-bit unusable for extended gradients is gone.
//!
//! `monostar.icc` is a 996-byte greyscale profile whose `kTRC` is an ICC parametric
//! type-3 curve with g=3, a=1/1.16, b=0.16/1.16, c=1/9.033, d=0.08 — exactly
//! `raw_core::display::lstar_encode` inverted, with a D50 white point. Encoder and
//! tag are a matched pair, verified by `lstar_matches_the_icc_parametric_curve`.
//!
//! **A greyscale container is the honest one while the pipeline is monochrome.**
//! Three identical channels would triple the file and claim a colour decision the
//! pipeline has not made. Toning is the stage that makes that decision, and an active
//! toner turns 1 channel into 3 — which is why `Spec::new` promotes a greyscale target
//! to eciRGB v2 rather than tagging three channels with a `GRAY` profile. The two
//! profiles are not interchangeable, and that promotion is where the difference lands.

use std::borrow::Cow;
use std::io::{BufWriter, Write};
use std::path::Path;

use raw_core::display::{lstar_encode, tone_map};
use raw_core::geometry::Dims;
use raw_core::sidecar::{self, Metadata};
use raw_core::{OutputParams, ToneMap};

/// The greyscale L\* export profile, embedded so an export never depends on a file
/// sitting next to the binary.
///
/// **These point at the repository's `profiles/` directory, not a copy under this
/// crate.** `monostar.icc` is generated — `write-profiles` writes that path and
/// `the_generator_reproduces_the_shipped_profile` guards it — so a second copy is a
/// copy that goes stale silently. It did: the app shipped the pre-regeneration
/// profile, with the non-conformant white-point Z and the short licence text, for as
/// long as the duplicate existed. One path is what makes that unrepeatable.
const MONOSTAR_ICC: &[u8] = include_bytes!("../../../profiles/monostar.icc");
const ECIRGB_ICC: &[u8] = include_bytes!("../../../profiles/eciRGB_v2_ICCv4.icc");
const PROSTAR_ICC: &[u8] = include_bytes!("../../../profiles/ProStarRGB.icc");
const SRGB_ICC: &[u8] = include_bytes!("../../../profiles/sRGB2014.icc");

/// The colour space a file is written in — its primaries, and its transfer curve.
///
/// # Three of these are one family, and that is not a coincidence
///
/// `monostar`, `eciRGB v2` and `ProStarRGB` carry an **identical transfer function**:
/// ICC parametric type 3 with `g=3, a=1/1.16, b=0.16/1.16, c=1/9.033, d=0.08` — the
/// L\* curve. Read out of the shipped profiles rather than taken on trust:
/// ProStar stores its as a 700-point table and it matches the parametric curve to
/// 0.00000 mean error.
///
/// What that buys is the colour transition, which toning has since made real. When an
/// active toner turns one channel into three, `Monostar → EciRgbV2` changes **only the
/// channel count** — the tone encoding carries over untouched, so nothing has to be
/// re-derived and no print has to be re-judged. It is why these are the two profiles
/// the maintainer chose.
///
/// | | gamut area (xy) | TRC |
/// |---|---|---|
/// | monostar | greyscale | L\* |
/// | sRGB | 0.109 | sRGB |
/// | eciRGB v2 | 0.158 | L\* |
/// | ProStarRGB | 0.277 | L\* |
///
/// `ProStarRGB` has **ProPhoto's exact primaries** with the L\* curve, which is why
/// plain ProPhoto is not offered: it would be the only entry with a different transfer
/// function, for the same gamut. Rec.2020 and Display P3 are absent for the same
/// reason, and linear Rec.2020 for a stronger one — linear light in a 16-bit *integer*
/// container spends codes uniformly in luminance and starves the shadows, which is the
/// exact problem L\* exists to solve. Linear wants float, and that is a different
/// feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Space {
    /// Greyscale, L\*. The master, and the honest container while the pipeline is
    /// monochrome — three identical channels would triple the file and claim a colour
    /// decision the pipeline has not made.
    #[default]
    Monostar,
    /// Display-referred, for proofs. See [`Space::icc`] for why this one is not always
    /// accompanied by a profile.
    Srgb,
    /// The print master once there is colour to hold: enough gamut for any inkjet,
    /// and monostar's own curve.
    EciRgbV2,
    /// ProPhoto's gamut in the L\* curve. For handing off into a ProPhoto-based
    /// workflow, not because a wider gamut is better — eciRGB v2 already exceeds any
    /// printer, and ProPhoto-gamut's blue primary sits at `y = 0.000`, outside human
    /// vision, so part of the encoding is spent on colours that do not exist.
    ProStar,
}

impl Space {
    /// Spaces offered by the export controls. `ProStar` remains a readable legacy
    /// value so old preferences and sidecars do not become invalid, but it is no
    /// longer presented as an export choice.
    pub const UI_ORDER: [Self; 3] = [Self::Monostar, Self::Srgb, Self::EciRgbV2];
    const ALL: [Self; 4] = [Self::Monostar, Self::Srgb, Self::EciRgbV2, Self::ProStar];
    /// What a **proof** may be written in. sRGB because the recipient's screen is
    /// unmanaged; monostar because sometimes you know it is not.
    pub const PROOF_ORDER: [Self; 2] = [Self::Srgb, Self::Monostar];

    pub fn label(self) -> &'static str {
        match self {
            Self::Monostar => "monostar (gray L*)",
            Self::Srgb => "sRGB",
            Self::EciRgbV2 => "eciRGB v2",
            Self::ProStar => "ProStarRGB",
        }
    }

    /// Whether this space carries three channels of *real* colour rather than a
    /// repeated scalar.
    ///
    /// **This used to gate the two RGB entries off**, on the ground that the pipeline
    /// had one channel and picking one would triple a file to claim a colour decision
    /// nobody had made. Chemical toning is that decision, so the gate is gone and the
    /// predicate keeps only its descriptive half.
    ///
    /// It is still worth having, and for a reason the gate obscured: an **untoned**
    /// frame in one of these spaces is exactly the greyscale master with its channels
    /// repeated — `raw_core::colour` guarantees that bit-exactly — so a user who picks
    /// eciRGB v2 for a neutral print pays three times the size for nothing. That is a
    /// hint to give, not a control to disable.
    pub fn is_rgb(self) -> bool {
        matches!(self, Self::EciRgbV2 | Self::ProStar)
    }

    /// Display-linear -> encoded `[0, 1]`, in this space's transfer curve.
    pub fn encode(self, v: f32) -> f32 {
        match self {
            Self::Srgb => raw_core::display::srgb_encode(v),
            _ => lstar_encode(v),
        }
    }

    /// How many channels a file in this space carries.
    ///
    /// # An sRGB proof is RGB, and pays three times the size for it
    ///
    /// the maintainer's decision, and it overrides the first version of this module. A
    /// monochrome sRGB proof *could* be written as one channel and marked with PNG's
    /// own `sRGB` chunk — smaller, and valid. It was built that way and it was the
    /// wrong trade: **`sRGB2014.icc` is an RGB profile, and tagging one channel with
    /// it is not valid**, so the marked-not-tagged version meant the file travelled
    /// carrying no ICC at all.
    ///
    /// A proof is the file that *leaves*. What matters about it is that the next
    /// application recognises the intent, and an embedded profile is the thing every
    /// application looks for — a format-native chunk is understood by decoders and
    /// routinely ignored by everything downstream of them. Three identical channels
    /// and a profile is worth three times the bytes for a file that is half size and
    /// 8-bit to begin with.
    ///
    /// The objection this reverses — that three identical channels "claim a colour
    /// decision the pipeline has not made" — is right about a **master** and does not
    /// reach a proof. A master is what a print is made from and must not overstate
    /// what is in it; a proof is a picture of the print, and its job is to survive the
    /// journey.
    pub fn channels(self) -> usize {
        match self {
            Self::Monostar => 1,
            _ => 3,
        }
    }

    /// Whether the untoned container is one channel. Use `Print::is_grey` when
    /// a print is available: toning can require three channels in a gray space.
    pub fn is_grey(self) -> bool {
        self.channels() == 1
    }

    /// The profile to embed. Every space has one, and every file carries it.
    pub fn icc(self) -> &'static [u8] {
        match self {
            Self::Monostar => MONOSTAR_ICC,
            Self::Srgb => SRGB_ICC,
            Self::EciRgbV2 => ECIRGB_ICC,
            Self::ProStar => PROSTAR_ICC,
        }
    }

    /// Stable key for persistence; see [`Container::key`].
    pub fn key(self) -> &'static str {
        match self {
            Self::Monostar => "monostar",
            Self::Srgb => "srgb",
            Self::EciRgbV2 => "ecirgb",
            Self::ProStar => "prostar",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == s)
    }

    /// Resolve a removed legacy choice to the closest remaining print space before
    /// presenting it in Settings or using it for a new export.
    pub fn selectable(self) -> Self {
        match self {
            Self::ProStar => Self::EciRgbV2,
            other => other,
        }
    }
}

/// One sample per channel, from one sample.
///
/// Three identical channels is what a monochrome picture in an RGB space *is* — see
/// [`Space::channels`] for why a proof pays that and a master does not.
fn splat<T: Copy>(samples: Vec<T>, channels: usize) -> Vec<T> {
    if channels == 1 {
        return samples;
    }
    let mut out = Vec::with_capacity(samples.len() * channels);
    for s in samples {
        out.extend(std::iter::repeat_n(s, channels));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Tiff,
    Png,
    /// **Proofs only, and 8-bit only.** Baseline JPEG is 8 bits per sample; the 12-bit
    /// extension exists and is not decoded by anything a proof would be opened in.
    /// [`Container::depths`] is where that rule lives so no call site has to remember
    /// it.
    Jpeg,
}

impl Container {
    /// The **EXPORT module's** containers, in order.
    ///
    /// **JPEG is here, and it did not used to be.** The old comment read "a master is
    /// the file a print is made from, and a lossy master is a contradiction", which is
    /// a true sentence about masters and turned out to be the wrong claim about this
    /// list: the module is where a file leaves the app, and not everything that leaves
    /// is a master. the maintainer asked for JPEG, 2026-08-06, having gone looking for it. The
    /// argument against remains on `proof_note`, where it is a caption the user reads
    /// rather than a control they cannot reach — and `Container::depths` still refuses
    /// it 16 bits, so the *shape* of the refusal survives where it is structural.
    ///
    /// Lossless first, so the order still says which of them a print is made from.
    pub const UI_ORDER: [Self; 3] = [Self::Tiff, Self::Png, Self::Jpeg];
    /// The **proof's**. PNG first, because it is the one that tells the truth.
    pub const PROOF_ORDER: [Self; 2] = [Self::Png, Self::Jpeg];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Tiff => "tif",
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tiff => "TIFF",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }

    /// Which depths this container can hold.
    ///
    /// The rule lives here rather than at the control that greys the option out, so
    /// that a second control cannot get it wrong.
    pub fn depths(self) -> &'static [Depth] {
        match self {
            Self::Jpeg => &[Depth::Eight],
            _ => &[Depth::Sixteen, Depth::Eight],
        }
    }

    pub fn supports(self, depth: Depth) -> bool {
        self.depths().contains(&depth)
    }

    /// Why you would pick this container for a proof. Shown under the control.
    ///
    /// The argument that matters here is **not** the usual lossless-versus-small one.
    /// JPEG's 8×8 DCT quantisation damages precisely what this app is careful about:
    /// smooth tonal gradation and grain. Skies and walls band along block edges, and
    /// grain — a module that costs seconds of CPU per export to get right — smears
    /// into mosquito noise. A proof judged on a JPEG can mislead you about the two
    /// things you spent the most effort on.
    pub fn proof_note(self) -> &'static str {
        match self {
            Self::Png => {
                "Lossless: the tonal gradation and the grain survive exactly, and 16-bit \
                 is available. Larger files."
            }
            Self::Jpeg => {
                "Small, and what upload forms accept. Its 8×8 blocks band smooth skies \
                 and smear grain, so judge a print on the PNG. Grayscale has no chroma \
                 to subsample, so every bit goes to luma."
            }
            Self::Tiff => "The master.",
        }
    }

    /// Stable key for persistence. **Deliberately not `label`**: a display string is
    /// free to change with the wording of the UI, and a stored value that followed it
    /// would silently stop matching what the last release wrote.
    pub fn key(self) -> &'static str {
        match self {
            Self::Tiff => "tiff",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        [Self::Tiff, Self::Png, Self::Jpeg]
            .into_iter()
            .find(|c| c.key() == s)
    }
}

/// How much smaller a proof is than the picture.
///
/// **A factor of the picture, not of the master's export size.** The master's resample
/// belongs to the master — it is the interface output sharpening will sharpen — and
/// deriving the proof from it would mean that setting a proof to ¼ changed nothing
/// about the TIFF but changing the TIFF's print size silently changed the proof. Two
/// exports, two sizes, one picture, and neither reaches into the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProofScale {
    Full,
    #[default]
    Half,
    Third,
    Quarter,
}

impl ProofScale {
    pub const UI_ORDER: [Self; 4] = [Self::Full, Self::Half, Self::Third, Self::Quarter];

    pub fn factor(self) -> f32 {
        match self {
            Self::Full => 1.0,
            Self::Half => 0.5,
            Self::Third => 1.0 / 3.0,
            Self::Quarter => 0.25,
        }
    }

    /// **Spelled `1/2`, not `½`.** the maintainer read the single-glyph fractions as
    /// illegible, and he is right: `½ ⅓ ¼` are one glyph carrying two digits and a
    /// rule, so at the size the rest of the row is set in they are a smudge — and
    /// `⅓` against `¼` is a guess. Setting these four labels larger would fix the
    /// legibility and break the row rhythm, since nothing else on the page is bigger.
    /// Full-size digits are legible at the size that was already there.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full size",
            Self::Half => "1/2 size",
            Self::Third => "1/3 size",
            Self::Quarter => "1/4 size",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Half => "half",
            Self::Third => "third",
            Self::Quarter => "quarter",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|s2| s2.key() == s)
    }

    /// The proof's pixel dimensions, given the picture's own. Never smaller than 1 px.
    pub fn dims(self, picture: Dims) -> Dims {
        if matches!(self, Self::Full) {
            return picture;
        }
        let f = self.factor();
        Dims {
            w: ((picture.w as f32 * f).round() as usize).max(1),
            h: ((picture.h as f32 * f).round() as usize).max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Eight,
    Sixteen,
}

impl Depth {
    pub const UI_ORDER: [Self; 2] = [Self::Sixteen, Self::Eight];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sixteen => "16-bit",
            Self::Eight => "8-bit",
        }
    }

    /// Stable key for persistence; see `Container::key`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Sixteen => "16",
            Self::Eight => "8",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|d| d.key() == s)
    }
}

/// TIFF compression. **Both options are lossless** — this trades file size against
/// compatibility, never quality.
///
/// Deflate is the same entropy coding as ZIP: it reconstructs bit-identical
/// samples. (JPEG-in-TIFF would be lossy; that is a different thing and is not
/// offered.) `deflate_is_bit_identical_to_uncompressed` proves it rather than
/// asserting it, because "compressed" reasonably reads as "degraded" and the claim
/// should not have to be taken on faith.
///
/// Uncompressed is still the default and worth keeping: some print RIPs and older
/// software are fussy, and an uncompressed TIFF can be memory-mapped and read
/// without decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    #[default]
    None,
    /// Deflate with a horizontal predictor.
    Deflate,
}

impl Compression {
    pub const UI_ORDER: [Self; 2] = [Self::None, Self::Deflate];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "uncompressed",
            Self::Deflate => "deflate (lossless)",
        }
    }

    /// Stable key for persistence; see `Container::key`.
    pub fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Deflate => "deflate",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|c| c.key() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub container: Container,
    pub depth: Depth,
    /// TIFF only. PNG is always deflate — the format has no uncompressed mode.
    pub compression: Compression,
    pub space: Space,
}

impl Default for Target {
    fn default() -> Self {
        Self {
            container: Container::Tiff,
            depth: Depth::Sixteen,
            compression: Compression::None,
            space: Space::Monostar,
        }
    }
}

impl Target {
    /// The default **proof**: half size, 8-bit PNG, sRGB-encoded.
    ///
    /// PNG rather than JPEG because a proof you cannot trust is not a proof; sRGB
    /// because the screen it lands on is not managed; half size because a proof is for
    /// looking at, and a 41 MP one is a download.
    pub fn proof() -> Self {
        Self {
            container: Container::Png,
            depth: Depth::Eight,
            compression: Compression::None,
            space: Space::Srgb,
        }
    }

    pub fn extension(self) -> &'static str {
        self.container.extension()
    }

    /// Force the depth into something this container can hold.
    ///
    /// Called after any change to either, so that switching a 16-bit proof to JPEG
    /// leaves a legal target rather than one the writer has to reinterpret.
    pub fn settle(&mut self) {
        if !self.container.supports(self.depth) {
            self.depth = self.container.depths()[0];
        }
    }

    /// The space this target is **actually written in**, given whether a toner is
    /// active — the promotion in [`Spec::new`], asked as a question.
    ///
    /// **Two answers to this drifted, which is what it exists to stop.** The Output
    /// panel stated the space with a hard-coded `monostar`, and `Spec::new` promoted a
    /// toned greyscale target to eciRGB v2 — so a toned export said one thing in the
    /// panel and wrote another, which is exactly the silent substitution `Spec::new`
    /// claims not to make. One function, asked by both.
    pub fn written_space(self, needs_colour: bool) -> Space {
        if needs_colour && self.space.is_grey() {
            Space::EciRgbV2
        } else {
            self.space
        }
    }

    pub fn label(self) -> String {
        let space = self.space.label();
        match self.container {
            Container::Tiff => {
                format!(
                    "{} TIFF, {}, {space}",
                    self.depth.label(),
                    self.compression.label()
                )
            }
            c => format!("{} {}, {space}", self.depth.label(), c.label()),
        }
    }
}

/// Everything about the file that is not its pixels.
///
/// Five arguments' worth of decisions bundled, because `write` had grown to seven and
/// the next one would have been the one someone passed in the wrong order. The
/// grouping is also honest: these are exactly the things the Output module sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub target: Target,
    pub tone_map: ToneMap,
    /// Print resolution, and the output size when one is asked for.
    pub output: OutputParams,
    /// The emulsion. Runs **after** `output`'s resize; see `write`.
    ///
    /// Carried here rather than read from ambient state for the same reason
    /// `tone_map` is: an export is a description of a file, and the description has
    /// to be complete at the moment the button was pressed. The render half of an
    /// export happens on the main thread and the encode half on a worker, so a spec
    /// that reached back for a parameter would be reading it a slider-drag later.
    pub grain: raw_core::GrainParams,
    /// Output sharpening. Runs **after** `output`'s resize, grain and toning; see
    /// `write`. Carried here for the same reason `grain` is.
    pub sharpen: raw_core::sharpen::SharpenParams,
    /// Chemical toning. Runs **after** `grain` and **before** `sharpen`; see `write`.
    /// Carried here for the same reason `grain` is.
    pub toning: raw_core::ToningParams,
    /// The physical canvas, composed after every image-making stage.
    pub frame: raw_core::FrameParams,
    /// TPDF dither on the 8-bit quantisation. **Ignored at 16 bits**, where there is
    /// nothing to break up.
    ///
    /// This was structural — `samples8` dithered unconditionally and `samples16` never
    /// did — on the argument that a rule no call site can get backwards is better than
    /// a flag. The rule is unchanged and still lives in one place; what changed is that
    /// 8-bit is now a *master* format here and not only a proof one, since JPEG and
    /// 8-bit PNG are selectable in the EXPORT module. An 8-bit deliverable going
    /// somewhere that will re-encode it is a case where you might not want a pixel of
    /// added noise, and there was no way to say so.
    ///
    /// Defaulted **on** by `Spec::new`, so the only way to get an undithered 8-bit file
    /// is to have asked for one.
    pub dither: bool,
    /// Set when this is a **proof**, and then it replaces `output`'s resample rather
    /// than composing with it. See [`ProofScale`].
    pub proof: Option<ProofScale>,
    /// The authored metadata to embed, or `None` to embed none.
    ///
    /// **The caller has already decided** — this being `Some` means "write this", full
    /// stop. The preference that governs it is `Settings::export_metadata`, and
    /// consulting it here as well would give two places that decide, of which the one
    /// easier to forget is the one that leaks. `Spec::new` is the single resolution
    /// and is what every caller should use.
    pub metadata: Option<Metadata>,
}

/// The CPU export tail's modules, as one argument.
///
/// **Grouped because the list had grown to eight positional arguments**, which is one
/// mis-ordered pair away from a silent bug and was already three rounds of call-site
/// churn every time the tail gained a stage. They belong together on their own terms
/// too: these are exactly the modules that run after the tone map, in this order, and
/// none of them can reach the viewport.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    pub grain: raw_core::GrainParams,
    pub sharpen: raw_core::sharpen::SharpenParams,
    pub toning: raw_core::ToningParams,
    pub frame: raw_core::FrameParams,
}

impl Tail {
    /// The tail as a set of params describes it, with each module's bypass already
    /// resolved — see `Params::effective`.
    pub fn of(p: &raw_core::Params) -> Self {
        let e = p.effective();
        Self {
            grain: e.grain,
            sharpen: e.sharpen,
            toning: e.toning,
            frame: e.frame,
        }
    }
}

impl Spec {
    /// Build a spec. Pass `None` for `meta` when the preference says metadata does not
    /// travel; metadata that is present but empty is dropped either way, so a file
    /// with nothing to say gets no packet rather than one claiming nothing.
    pub fn new(
        target: Target,
        tone_map: ToneMap,
        output: OutputParams,
        tail: Tail,
        meta: Option<&Metadata>,
    ) -> Self {
        let metadata = meta.filter(|m| !m.is_empty()).cloned();
        let Tail {
            grain,
            sharpen,
            toning,
            frame,
        } = tail;
        let mut target = target;

        // **A toned print cannot go in a greyscale container**, and this is where that
        // is resolved rather than at the writer.
        //
        // Left alone, the tail produced three real channels and the file was tagged
        // with `monostar`, which declares GRAY. Photoshop reads a colour-space mismatch
        // like that as no profile at all — the export came out untagged, and untagged
        // means read as sRGB, so the L\* encoding was decoded with the wrong curve as
        // well. One conflict, two symptoms, and the second is the one that looked like
        // a colour bug.
        //
        // **eciRGB v2 rather than an error**, because the colour transition already
        // named it the toned master — this is that decision arriving, not a new one.
        //
        // **Through [`Target::written_space`], which the Output panel asks too**, so
        // what the panel says is what gets written. It did not: the panel stated a
        // hard-coded `monostar` and a toned export wrote eciRGB v2, which is the
        // silent substitution this comment used to claim did not exist.
        target.space = target.written_space(toning.is_active() || frame.needs_colour());

        Self {
            target,
            tone_map,
            output,
            grain,
            sharpen,
            toning,
            frame,
            dither: true,
            proof: None,
            metadata,
        }
    }

    /// The same, as a proof at `scale`.
    pub fn proof(
        target: Target,
        tone_map: ToneMap,
        output: OutputParams,
        tail: Tail,
        meta: Option<&Metadata>,
        scale: ProofScale,
    ) -> Self {
        Self {
            proof: Some(scale),
            ..Self::new(target, tone_map, output, tail, meta)
        }
    }

    /// What this spec will write, given the picture's own dimensions.
    ///
    /// One function so the panel's caption and the encoder cannot disagree about the
    /// size of the file — which they would the first time one of them forgot that a
    /// proof ignores `output.resize`.
    pub fn dims(&self, picture: Dims) -> Dims {
        let image = self.image_dims(picture);
        self.frame_layout(picture)
            .map(|layout| layout.outer)
            .unwrap_or(image)
    }

    /// The photograph's grid before FRAME adds its canvas.
    pub fn image_dims(&self, picture: Dims) -> Dims {
        match self.proof {
            Some(scale) => scale.dims(picture),
            None => self.output.target_dims(picture),
        }
    }

    /// Resolve FRAME on the same physical image size OUTPUT reports. A proof has
    /// fewer pixels but the same proportions, so its margins scale with it.
    pub fn frame_layout(
        &self,
        picture: Dims,
    ) -> Result<raw_core::frame::PixelLayout, raw_core::FrameLayoutError> {
        let (w, h) = self.output.print_inches(picture);
        self.frame.pixel_layout(self.image_dims(picture), [w, h])
    }

    /// The resolution tag, as a whole number. Both containers want an integer and
    /// neither wants zero.
    fn dpi(&self) -> u32 {
        self.output.ppi.round().max(1.0) as u32
    }
}

/// Encode scene-referred f32 and write it.
///
/// `w` and `h` are the **picture's** dimensions — the crop, at 1:1. The file's may
/// differ, and `spec.output` is what decides; ask `OutputParams::target_dims` if you
/// need to know before calling.
///
/// Refuses rather than clamps when the requested size is past `OutputParams`' limits,
/// so a panel that says 72000 px does not quietly produce 30000.
pub fn write(path: &Path, w: u32, h: u32, scene: &[f32], spec: &Spec) -> std::io::Result<()> {
    let picture = Dims {
        w: w as usize,
        h: h as usize,
    };
    let frame_layout = spec
        .frame_layout(picture)
        .map_err(|e| std::io::Error::other(format!("FRAME cannot be resolved: {e}")))?;
    // A proof is bounded by the picture it came from, so only a master can ask for
    // something past the limits. The limit is on the complete file, including FRAME.
    if spec.proof.is_none()
        && (frame_layout.outer.w > OutputParams::MAX_EDGE as usize
            || frame_layout.outer.h > OutputParams::MAX_EDGE as usize
            || (frame_layout.outer.w as u64) * (frame_layout.outer.h as u64)
                > OutputParams::MAX_PIXELS)
    {
        let d = frame_layout.outer;
        return Err(std::io::Error::other(format!(
            "output would be {} x {} px, past the {} px edge / {} MP limit",
            d.w,
            d.h,
            OutputParams::MAX_EDGE,
            OutputParams::MAX_PIXELS / 1_000_000,
        )));
    }

    // Tone map FIRST, then resize, then encode. The order is the whole of
    // `raw_core::resample`'s module note: resizing L\* codes averages perceptual
    // values rather than light, and resizing scene-referred values lets a specular's
    // ringing swing far enough negative to outline the highlight in black. Between
    // the two, the signal is bounded and linear.
    let mapped: Vec<f32> = scene.iter().map(|&v| tone_map(v, spec.tone_map)).collect();
    // **A proof resizes to its own factor and ignores `output.resize` entirely.** Not
    // composing the two is the point: the master's resample is a print decision and
    // the proof's is a convenience, and a proof that inherited the print size would
    // change every time a print size did. See `ProofScale`.
    let want = spec.image_dims(picture);
    let (w, h, mapped) = if want != picture {
        let (dw, dh) = (want.w as u32, want.h as u32);
        (
            dw,
            dh,
            raw_core::resample(&mapped, w, h, dw, dh, spec.output.filter),
        )
    } else {
        (w, h, mapped)
    };

    // Then grain, **after the resize and before the encode**. Both halves of that
    // are decisions and both were nearly made the other way.
    //
    // *After the resize*, which is the maintainer's call, because **grain is a property of the
    // print and not of the negative**: a 9 px crystal is 9 px in the file whatever
    // size the file is. Graining before the resize is defensible on the other
    // reading — grain lives in the emulsion, and scanning a grainy negative down does
    // smooth it — but it costs the one thing that makes an export-only module usable.
    // The loupe shows a 400x400 crop at 1:1, so if a 2x downsample came afterwards and
    // averaged the grain away, the loupe would be showing a texture the file will not
    // have. That is the "a bug that exists only below 1:1" failure with the scale
    // factor moved from the viewport to the resampler, and the answer is the same:
    // make the thing you look at and the thing you get be the same pixels.
    //
    // *Before the encode*, because the emulsion works in light. L\* is a perceptual
    // recoding, and depositing silver into perceptual codes would put the printing
    // model's `1 - I` on the wrong curve.
    //
    // Grain is also why this function can take seconds: it is a CPU pass with a
    // serial layer chain, ~2.4 s on a 24 MP file at the default thirty layers. It
    // runs on the export worker, so the UI stays live — see `App::export`.
    let mapped = if spec.grain.is_active() {
        raw_core::grain::apply(&mapped, w as usize, h as usize, &spec.grain).image
    } else {
        mapped
    };

    // Then chemical toning, **between grain and sharpening**.
    //
    // *After grain*, because a toner reacts with the silver the emulsion actually
    // deposited. The two are not merely adjacent: `GrainResult::silver_density` is
    // emitted for this module and guarded by `silver_is_unscaled_coverage` so its
    // numbers stay usable as a reaction driver. Driving the conversion from the real
    // coverage rather than from the analytic density is a later, separable step — see
    // the Toning brief — and the order here is what makes it possible at all.
    //
    // *Before sharpening*, which is the maintainer's decision and the same argument that put
    // grain there: output sharpening compensates the medium and is the last thing that
    // happens to a print.
    //
    // **The output is OKLab, not RGB.** Toning emits a lightness and an (a, b) pair,
    // and OKLab has no primaries and no gamut, so nothing here has to choose a space.
    // The expansion to primaries happens at the encode, which is the one boundary that
    // needs one. See `raw_core::colour`.
    //
    // Untoned, this branch is not taken and the buffer stays one channel — which is
    // what keeps a greyscale master byte-for-byte what it always was.
    let toned = spec.toning.is_active().then(|| tone(&mapped, &spec.toning));

    // Then output sharpening, **last, over everything including the grain** — which is
    // the maintainer's call and the reverse of how this was first built.
    //
    // *After the resize* is what makes it output sharpening at all: the radius is in
    // output pixels, so it compensates the downsample that has just happened, for the
    // medium the file is going to. Sharpening before the resize would have the
    // resampler partly undo it, which is the reason the step exists separately from a
    // capture sharpen. This position was chosen over the prototype's scene-linear one;
    // `docs/decisions.md` has the argument and what it costs.
    //
    // *After grain*, because **output sharpening compensates the medium and the medium
    // does not discriminate**. Dot gain, paper spread and the softening of a downsample
    // act on everything in the file, and grain is in the file. Sharpening the detail but
    // not the grain would leave grain reading soft against crisp detail — a mismatch no
    // real print has, because there the grain and the detail are the same silver through
    // the same optics.
    //
    // This was first built the other way round, on the argument that decision 11's
    // "grain is a property of the print" put grain downstream of the enlarger. That
    // reads more into 11 than it says: 11 is a claim about *scale* — a 9 px crystal must
    // stay 9 px, so grain must not be resampled — and both orders satisfy it.
    //
    // **The cost, stated because it is real:** the two modules stop being separable.
    // Amount makes the grain louder and harder, so they have to be dialled together. A
    // sharper print does show its grain more, so this is faithful rather than a bug —
    // but the loupe is the only place it is visible, which is why the loupe shows the
    // whole tail and not one module of it.
    //
    // **A proof sharpens at the proof's own grid**, and gets that for free by sitting
    // below `spec.dims` — the resize above has already used the proof's factor, so the
    // radius here is in the pixels the proof will actually have. A proof that sharpened
    // at the master's scale would lie about the thing it exists to predict.
    //
    // On a toned print it sharpens the **lightness only**, which cannot fringe. See
    // `raw_core::sharpen::apply_toned`.
    let print = match toned {
        Some(lab) => Print::Lab(raw_core::sharpen::apply_toned(
            &lab,
            w as usize,
            h as usize,
            &spec.sharpen,
        )),
        None => Print::Grey(raw_core::sharpen::apply(
            &mapped,
            w as usize,
            h as usize,
            &spec.sharpen,
        )),
    };

    // FRAME is deliberately last. The photograph has already been resized, grained,
    // toned and sharpened, and only now is it copied onto the larger flat canvas.
    // Consequently no frame sample can enter the sharpening kernel, which is what
    // prevents a contrasting border from creating an edge halo.
    let print = print.framed(frame_layout, spec.frame.color);
    let (w, h) = (frame_layout.outer.w as u32, frame_layout.outer.h as u32);

    let xmp = spec.metadata.as_ref().and_then(sidecar::metadata_packet);
    raw_core::atomic_file::write(path, |file| {
        match (spec.target.container, spec.target.depth) {
            (Container::Tiff, Depth::Sixteen) => write_tiff(
                file,
                w,
                h,
                &print.samples16(spec.target.space),
                print.channels(spec.target.space),
                spec,
                xmp.as_deref(),
            ),
            (Container::Tiff, Depth::Eight) => write_tiff(
                file,
                w,
                h,
                &print.samples8(w, spec.target.space, spec.dither),
                print.channels(spec.target.space),
                spec,
                xmp.as_deref(),
            ),
            (Container::Png, _) => write_png(file, w, h, &print, spec, xmp.as_deref()),
            // Depth is not consulted: `Container::depths` has already settled it at 8, and
            // a `match` arm that pretended otherwise would be a second place to be wrong.
            (Container::Jpeg, _) => write_jpeg(file, w, h, &print, spec),
        }
    })
}

/// Toning, as the export tail runs it: one scalar in, three OKLab numbers out.
///
/// The table is the **same one the display shader samples**, baked from the same
/// parameters at the same size. That is what makes preview and export the same picture
/// rather than two implementations that agree until they do not.
fn tone(mapped: &[f32], p: &raw_core::ToningParams) -> Vec<f32> {
    let lut = p.bake_flat(raw_core::toning::LUT_ENTRIES);
    let last = raw_core::toning::LUT_ENTRIES - 1;
    let mut out = Vec::with_capacity(mapped.len() * 3);
    for &v in mapped {
        // Indexed on L*, exactly as the shader is. A table on luminance would spend
        // most of its entries on the highlights and almost none on the shadows.
        let t = raw_core::display::lstar_encode(v).clamp(0.0, 1.0) * last as f32;
        let i = (t.floor() as usize).min(last - 1);
        let f = t - i as f32;
        for k in 0..3 {
            let a = lut[i * 3 + k];
            let b = lut[(i + 1) * 3 + k];
            out.push(a + (b - a) * f);
        }
    }
    out
}

/// The finished print, in whichever form the tail produced.
///
/// A deliberate sum type rather than always carrying three channels. An untoned export
/// stays one channel from the tone map to the encode, so the greyscale master is
/// byte-for-byte the file it has always been and pays nothing for a module it is not
/// using.
enum Print {
    /// Display-referred luminance, one per pixel.
    Grey(Vec<f32>),
    /// Interleaved display-referred luminance and OKLab's two chroma axes: `y`, `a`,
    /// `b`. See `raw_core::sharpen::apply_toned` on why plane 0 is a luminance.
    Lab(Vec<f32>),
}

/// Paint the exact output-pixel perimeter requested by FRAME's Trim Line.  Keeping
/// this here, after the photograph and paper have both been copied, makes the line
/// geometry rather than another image-processing operation.
fn black_perimeter(samples: &mut [f32], width: usize, height: usize, channels: usize, trim: usize) {
    if trim == 0 || width == 0 || height == 0 {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            if x < trim || y < trim || x >= width - trim || y >= height - trim {
                let at = (y * width + x) * channels;
                samples[at..at + channels].fill(0.0);
            }
        }
    }
}

impl Print {
    /// Place the finished photograph on a flat canvas. This is a copy, not a filter:
    /// image samples are preserved exactly and only new margin samples are authored.
    fn framed(self, layout: raw_core::frame::PixelLayout, color: [u8; 3]) -> Self {
        if layout.outer == layout.image {
            return self;
        }

        let [l, a, b] = raw_core::colour::oklab_of_srgb(color);
        // The first plane in `Lab` is display-referred luminance, while `l` is OKLab
        // lightness. For a neutral those are related exactly by the cube.
        let y = l.powi(3).clamp(0.0, 1.0);
        let coloured = !(color[0] == color[1] && color[1] == color[2]);
        let at = layout.top * layout.outer.w + layout.left;

        match self {
            Self::Grey(src) if !coloured => {
                let mut out = vec![y; layout.outer.w * layout.outer.h];
                for row in 0..layout.image.h {
                    let from = row * layout.image.w;
                    let to = at + row * layout.outer.w;
                    out[to..to + layout.image.w].copy_from_slice(&src[from..from + layout.image.w]);
                }
                black_perimeter(&mut out, layout.outer.w, layout.outer.h, 1, layout.trim);
                Self::Grey(out)
            }
            Self::Grey(src) => {
                let mut out = vec![0.0; layout.outer.w * layout.outer.h * 3];
                for px in out.chunks_exact_mut(3) {
                    px.copy_from_slice(&[y, a, b]);
                }
                for row in 0..layout.image.h {
                    for col in 0..layout.image.w {
                        let value = src[row * layout.image.w + col];
                        let to = ((at + row * layout.outer.w) + col) * 3;
                        out[to..to + 3].copy_from_slice(&[value, 0.0, 0.0]);
                    }
                }
                black_perimeter(&mut out, layout.outer.w, layout.outer.h, 3, layout.trim);
                Self::Lab(out)
            }
            Self::Lab(src) => {
                let mut out = vec![0.0; layout.outer.w * layout.outer.h * 3];
                for px in out.chunks_exact_mut(3) {
                    px.copy_from_slice(&[y, a, b]);
                }
                for row in 0..layout.image.h {
                    let from = row * layout.image.w * 3;
                    let to = (at + row * layout.outer.w) * 3;
                    out[to..to + layout.image.w * 3]
                        .copy_from_slice(&src[from..from + layout.image.w * 3]);
                }
                black_perimeter(&mut out, layout.outer.w, layout.outer.h, 3, layout.trim);
                Self::Lab(out)
            }
        }
    }

    /// Samples ready for the container, at 16 bits.
    ///
    /// The greyscale arm is `splat`ed by the writers exactly as before. The toned arm
    /// expands through `raw_core::colour` — **the only place in this app that knows what
    /// primaries are** — and a neutral there is bit-exact rather than nearly so, so an
    /// untoned pixel inside a toned frame still writes three identical channels.
    fn samples16(&self, space: Space) -> Vec<u16> {
        match self {
            Self::Grey(v) => splat(samples16(v, space), space.channels()),
            Self::Lab(lab) => {
                let p = primaries(space);
                let mut out = Vec::with_capacity(lab.len());
                for px in lab.chunks_exact(3) {
                    for c in linear_rgb(px, &p) {
                        out.push((space.encode(c) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16);
                    }
                }
                out
            }
        }
    }

    /// Samples at 8 bits — see [`samples8`] for what `dither` governs and what it does
    /// not.
    ///
    /// The dither is keyed on the **pixel**, not on the sample index, so the three
    /// channels of one pixel get the same offset. Keying it per sample would decorrelate
    /// them into coloured noise, which is a chroma artefact invented by the dither
    /// rather than by anything in the picture.
    fn samples8(&self, w: u32, space: Space, dither: bool) -> Vec<u8> {
        match self {
            Self::Grey(v) => splat(samples8(v, w, space, dither), space.channels()),
            Self::Lab(lab) => {
                let p = primaries(space);
                let mut out = Vec::with_capacity(lab.len());
                for (i, px) in lab.chunks_exact(3).enumerate() {
                    let n = if dither {
                        tpdf(i as u32 % w, i as u32 / w)
                    } else {
                        0.0
                    };
                    for c in linear_rgb(px, &p) {
                        let q = space.encode(c) * 255.0 + 0.5 + n;
                        out.push(q.floor().clamp(0.0, 255.0) as u8);
                    }
                }
                out
            }
        }
    }

    /// How many channels these samples carry. The space decides for a greyscale print;
    /// a toned one is three whatever the space would have said, and
    /// `Space::needs_colour` is what stops the two disagreeing.
    fn channels(&self, space: Space) -> usize {
        match self {
            Self::Grey(_) => space.channels(),
            Self::Lab(_) => 3,
        }
    }

    fn is_grey(&self, space: Space) -> bool {
        self.channels(space) == 1
    }
}

/// One toned pixel to linear RGB in the target space.
///
/// **`px[0]` is a luminance, and OKLab wants a lightness.** `Toned::y` is display-
/// referred luminance because that is what the tone chain hands over and what the
/// density arithmetic works in; OKLab's `L` for a neutral of that luminance is its cube
/// root. Passing the luminance straight in is the mistake this comment exists to stop
/// being made twice — it made every export several stops too dark while the viewport,
/// which converts correctly, looked right. `the_export_agrees_with_the_shader` is what
/// holds them together now.
fn linear_rgb(px: &[f32], p: &raw_core::Primaries) -> [f32; 3] {
    let l = raw_core::colour::oklab_lightness(px[0]);
    raw_core::colour::oklab_to_linear(l, px[1], px[2], p)
}

/// The target space's matrix, read out of the profile the file will be tagged with.
///
/// **Falls back to sRGB's** when a space carries no primaries, which is only
/// `monostar` — and a toned print cannot be written to monostar, because
/// `Space::needs_colour` keeps a greyscale master and a hue apart. The fallback exists
/// so this returns a value rather than an `Option` nobody could act on.
fn primaries(space: Space) -> raw_core::Primaries {
    raw_core::Primaries::from_icc(space.icc())
        .or_else(|| raw_core::Primaries::from_icc(SRGB_ICC))
        .expect("sRGB's profile has primaries")
}

/// Display-referred -> 16-bit L\* samples.
///
/// The input is post-tone-map and post-resize, so it is nominally in `[0, 1]` — but
/// only nominally: a windowed filter rings a little past its input's range, which is
/// why the clamp is here and why `raw_core::resample` does not clamp for itself.
fn samples16(mapped: &[f32], space: Space) -> Vec<u16> {
    mapped
        .iter()
        .map(|&v| (space.encode(v) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16)
        .collect()
}

/// Display-referred -> 8-bit L\* samples, dithered unless told otherwise.
///
/// **`samples16` has no such argument, and that half is still structural.** The rule is
/// "dither at 8 bits, never at 16" — TPDF breaks up 8-bit truncation banding on the
/// smooth extended gradients this app is for, while at 16 bits the quantisation step is
/// already far below the visual threshold and dithering a master would only add noise.
/// Keeping the 16-bit half out of the type means no call site can get *that* backwards,
/// which was the whole of the original argument and is unaffected by the 8-bit half
/// becoming a choice. See `Spec::dither` for why it did.
///
/// It also has to happen **after** any resize, which it does by construction now that
/// the resize is upstream in `write`: dither is a quantisation step, and resampling
/// dithered samples would smear the noise into correlated blobs and defeat the point
/// of it.
fn samples8(mapped: &[f32], w: u32, space: Space, dither: bool) -> Vec<u8> {
    mapped
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let n = if dither {
                tpdf(i as u32 % w, i as u32 / w)
            } else {
                0.0
            };
            let q = space.encode(v) * 255.0 + 0.5 + n;
            q.floor().clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// Greyscale JPEG. **Proofs only, 8-bit only, and the last thing this app writes that
/// throws information away.**
///
/// # Why the quality is 92 and not adjustable
///
/// A proof has one job: to be judged. The quality slider that would let it be set to
/// 60 is a slider for making a *smaller* file, and the moment a proof is small enough
/// to mislead, it has stopped doing its job. 92 is above the knee where JPEG's 8×8
/// quantisation becomes visible in smooth gradation — which is what this app produces
/// most of — and below the point where the file stops being appreciably smaller than
/// a PNG. One number, chosen once, is the honest form of that decision; if a proof
/// needs to be trusted rather than sent, the answer is PNG and it is one control away.
///
/// # Subsampling is off, and here it is not free
///
/// An sRGB proof is three identical channels — see [`Space::channels`] — so the
/// encoder does have chroma planes, and they are exactly neutral. `F_1_1` keeps them
/// that way. 4:2:0 would cost nothing visible on a neutral image, and it is off anyway
/// because a toned proof does have real chroma to subsample: halving the resolution of
/// the hue toning just added is the wrong default to inherit.
fn write_jpeg(
    file: &mut std::fs::File,
    w: u32,
    h: u32,
    print: &Print,
    spec: &Spec,
) -> std::io::Result<()> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

    let (w16, h16) = (u16::try_from(w), u16::try_from(h));
    let (Ok(w16), Ok(h16)) = (w16, h16) else {
        // JPEG's frame header stores each dimension in **two bytes**. 65535 px is not
        // a limit this app would otherwise have, so it is reported rather than
        // clamped — a proof silently written at a different size than the one the
        // panel promised is the failure this whole module keeps deciding against.
        return Err(std::io::Error::other(format!(
            "JPEG cannot hold {w} x {h} px — its frame header stops at 65535 per edge. \
             Export the proof as PNG, or at a smaller size."
        )));
    };

    let space = spec.target.space;
    let io = |e: jpeg_encoder::EncodingError| std::io::Error::other(e.to_string());
    let mut buffered = BufWriter::new(file);
    let mut enc = Encoder::new(&mut buffered, 92);
    enc.set_sampling_factor(SamplingFactor::F_1_1);
    enc.set_density(jpeg_encoder::PixelDensity::dpi(
        spec.dpi().min(u16::MAX as u32) as u16,
    ));
    // **Always.** A proof is the file that leaves, and the profile is what the next
    // application looks for. See `Space::channels`.
    enc.add_icc_profile(space.icc()).map_err(io)?;
    // Dithered, like every other 8-bit path here: TPDF breaks up the truncation
    // banding on extended gradients. It runs *before* the DCT, so the encoder sees the
    // noise as signal and preserves some of it — which is the intent.
    let samples = print.samples8(w, space, spec.dither);
    let colour = if print.is_grey(space) {
        ColorType::Luma
    } else {
        ColorType::Rgb
    };
    enc.encode(&samples, w16, h16, colour).map_err(io)?;
    buffered.flush()
}

/// Greyscale TIFF with the L\* TRC and `monostar.icc`.
///
/// When deflate is selected it is paired with a **horizontal predictor**, which is
/// the part that actually does the work: it stores each sample as the difference
/// from its left neighbour, turning the smooth gradients this app produces into runs
/// of near-zero bytes. Deflate alone on raw 16-bit photographic samples barely helps.
/// Both stages are exactly invertible, so the decoded samples are identical either
/// way.
fn write_tiff<T>(
    file: &mut std::fs::File,
    w: u32,
    h: u32,
    samples: &[T],
    channels: usize,
    spec: &Spec,
    xmp: Option<&str>,
) -> std::io::Result<()>
where
    T: TiffSample,
    [T]: tiff::encoder::TiffValue,
{
    use tiff::encoder::{Compression as TiffComp, TiffEncoder, compression::DeflateLevel};
    use tiff::tags::Predictor;

    let io = |e: tiff::TiffError| std::io::Error::other(e.to_string());
    let mut buffered = BufWriter::new(file);
    let mut enc = TiffEncoder::new(&mut buffered).map_err(io)?;
    enc = match spec.target.compression {
        Compression::None => enc,
        Compression::Deflate => enc
            .with_compression(TiffComp::Deflate(DeflateLevel::Balanced))
            .with_predictor(Predictor::Horizontal),
    };
    // **The colour type is chosen from the samples, not from the space**, and that is
    // the whole of what stops a mistag. The failure this replaced a refusal with is the
    // same one it was guarding against: one channel of data written under a profile
    // describing three renders differently everywhere it is opened.
    //
    // It used to refuse any non-greyscale space, because there was no way to produce
    // three real channels. Now there is, so the guard moved rather than went: what must
    // agree is the sample count and the profile, and `channels` is derived from the
    // print — see `Print::channels` — while the profile is derived from the space.
    // `Space::needs_colour` is what keeps those two from ever disagreeing.
    if channels == 1 {
        finish_tiff::<T, T::Grey>(enc, w, h, samples, spec, xmp)?;
    } else {
        finish_tiff::<T, T::Rgb>(enc, w, h, samples, spec, xmp)?;
    }
    buffered.flush()
}

/// The tags and the pixel data, once the colour type is known.
///
/// Split out only so the greyscale and RGB arms share a body: they differ in one type
/// parameter and in nothing else, and writing the tag block twice is how the two would
/// drift.
fn finish_tiff<T, C>(
    mut enc: tiff::encoder::TiffEncoder<&mut BufWriter<&mut std::fs::File>>,
    w: u32,
    h: u32,
    samples: &[T],
    spec: &Spec,
    xmp: Option<&str>,
) -> std::io::Result<()>
where
    C: tiff::encoder::colortype::ColorType<Inner = T>,
    [T]: tiff::encoder::TiffValue,
{
    use tiff::tags::{ResolutionUnit, Tag};

    let io = |e: tiff::TiffError| std::io::Error::other(e.to_string());
    let space = spec.target.space;
    let mut image = enc.new_image::<C>(w, h).map_err(io)?;

    // Tags must be written before the pixel data.
    image
        .encoder()
        .write_tag(Tag::IccProfile, space.icc())
        .map_err(io)?;
    image.resolution(
        ResolutionUnit::Inch,
        tiff::encoder::Rational {
            n: spec.dpi(),
            d: 1,
        },
    );

    // Metadata twice over, and both are needed. The XMP packet is the complete
    // record — it is the only one of the two that can carry keywords and a rating at
    // all — while the three ASCII tags are what a print shop's older software and
    // every file browser actually read. Writing only the packet loses the credit line
    // in exactly the places a credit line matters.
    if let Some(x) = xmp {
        image
            .encoder()
            .write_tag(Tag::Unknown(700), x.as_bytes())
            .map_err(io)?;
    }
    if let Some(m) = &spec.metadata {
        for (tag, val) in [
            (Tag::Artist, &m.creator),
            (Tag::Copyright, &m.rights),
            (Tag::ImageDescription, &m.description),
        ] {
            if let Some(v) = val {
                // An interior NUL would terminate the field early and orphan the rest
                // of the bytes inside the IFD. Nothing in the app can produce one, but
                // a sidecar is a text file anyone may edit.
                if !v.contains('\0') {
                    image.encoder().write_tag(tag, Utf8Ascii(v)).map_err(io)?;
                }
            }
        }
    }

    image.write_data(samples).map_err(io)?;
    Ok(())
}

/// A TIFF ASCII field whose bytes are written as given.
///
/// The `tiff` crate's own `str` value refuses anything outside 7-bit ASCII, which is
/// literally what the spec says and is unusable for the field it matters most in: the
/// first copyright line anyone types starts with `©`. The refusal is not cosmetic —
/// it fails the whole export.
///
/// Every reader in practice takes these fields as UTF-8, which is what exiftool and
/// Adobe write, and the XMP packet beside them is the authoritative record either way.
/// So this writes the UTF-8 bytes with the NUL terminator the type requires, and the
/// crate's own validation is bypassed deliberately rather than by accident.
struct Utf8Ascii<'a>(&'a str);

impl tiff::encoder::TiffValue for Utf8Ascii<'_> {
    const BYTE_LEN: u8 = 1;
    const FIELD_TYPE: tiff::tags::Type = tiff::tags::Type::ASCII;

    fn count(&self) -> usize {
        self.0.len() + 1
    }

    fn data(&self) -> Cow<'_, [u8]> {
        Cow::Owned([self.0.as_bytes(), &[0]].concat())
    }
}

/// Ties a sample type to its TIFF colour type so 8- and 16-bit share one writer.
trait TiffSample: Sized {
    type Grey: tiff::encoder::colortype::ColorType<Inner = Self>;
    /// The three-channel counterpart. A toned print is RGB whatever its depth, and
    /// pairing the two here means the writer picks a colour type rather than refusing
    /// a space.
    type Rgb: tiff::encoder::colortype::ColorType<Inner = Self>;
}
impl TiffSample for u8 {
    type Grey = tiff::encoder::colortype::Gray8;
    type Rgb = tiff::encoder::colortype::RGB8;
}
impl TiffSample for u16 {
    type Grey = tiff::encoder::colortype::Gray16;
    type Rgb = tiff::encoder::colortype::RGB16;
}

/// Greyscale PNG with the L\* TRC and `monostar.icc` in an `iCCP` chunk.
fn write_png(
    file: &mut std::fs::File,
    w: u32,
    h: u32,
    print: &Print,
    spec: &Spec,
    xmp: Option<&str>,
) -> std::io::Result<()> {
    let depth = spec.target.depth;
    let space = spec.target.space;
    let mut info = png::Info::with_size(w, h);
    info.color_type = if print.is_grey(space) {
        png::ColorType::Grayscale
    } else {
        png::ColorType::Rgb
    };
    info.bit_depth = match depth {
        Depth::Eight => png::BitDepth::Eight,
        Depth::Sixteen => png::BitDepth::Sixteen,
    };
    // iCCP, always, and describing exactly the channels this file has — which is what
    // `Space::channels` is for. Deliberately no gAMA or sRGB chunk alongside it: the
    // PNG spec says a decoder that honours iCCP must ignore them, and writing a gamma
    // that contradicts the profile is how a file ends up rendering differently in two
    // viewers that are both technically correct.
    info.icc_profile = Some(Cow::Borrowed(space.icc()));
    let ppm = (spec.dpi() as f32 / 0.0254).round() as u32;
    info.pixel_dims = Some(png::PixelDimensions {
        xppu: ppm,
        yppu: ppm,
        unit: png::Unit::Meter,
    });

    // XMP travels in an *uncompressed* iTXt chunk under the keyword the XMP spec
    // fixes for PNG. Compressing it is legal and is what breaks readers: several
    // walk the chunk looking for the packet header rather than inflating first.
    if let Some(x) = xmp {
        let mut chunk = png::text_metadata::ITXtChunk::new("XML:com.adobe.xmp", x);
        chunk.compressed = false;
        info.utf8_text.push(chunk);
    }
    // PNG has no Artist or Copyright chunk, so the ASCII mirrors are tEXt under the
    // keywords the spec does register. Author and Description are two of PNG's
    // suggested keywords; Copyright is the third.
    if let Some(m) = &spec.metadata {
        for (key, val) in [
            ("Author", &m.creator),
            ("Copyright", &m.rights),
            ("Description", &m.description),
        ] {
            if let Some(v) = val {
                info.uncompressed_latin1_text
                    .push(png::text_metadata::TEXtChunk::new(key, v));
            }
        }
    }

    let bytes: Vec<u8> = match depth {
        Depth::Eight => print.samples8(w, space, spec.dither),
        // PNG stores 16-bit samples big-endian, regardless of host order.
        Depth::Sixteen => print
            .samples16(space)
            .iter()
            .flat_map(|s| s.to_be_bytes())
            .collect(),
    };

    let mut buffered = BufWriter::new(file);
    let enc = png::Encoder::with_info(&mut buffered, info)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut writer = enc.write_header()?;
    writer.write_image_data(&bytes)?;
    writer.finish()?;
    buffered.flush()
}

/// TPDF dither, mirroring `display.wgsl`. Keyed on pixel position, so it is
/// deterministic and an 8-bit export re-run produces an identical file.
fn tpdf(x: u32, y: u32) -> f32 {
    fn hash(x: u32, y: u32, salt: u32) -> u32 {
        let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ salt;
        h ^= h >> 16;
        h = h.wrapping_mul(0x7FEB_352D);
        h ^= h >> 15;
        h = h.wrapping_mul(0x846C_A68B);
        h ^= h >> 16;
        h
    }
    const INV: f32 = 1.0 / 4_294_967_296.0;
    hash(x, y, 0x68E3_1DA4) as f32 * INV + hash(x, y, 0xB529_7A4D) as f32 * INV - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("monopro-export-test");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    pub(super) fn ramp(w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .map(|i| (i % w) as f32 / (w - 1) as f32)
            .collect()
    }

    const fn t(container: Container, depth: Depth, compression: Compression) -> Target {
        Target {
            container,
            depth,
            compression,
            space: Space::Monostar,
        }
    }

    /// The same, in a named space. For the proof paths, which are the only ones that
    /// leave monostar.
    pub(super) const fn ts(container: Container, depth: Depth, space: Space) -> Target {
        Target {
            container,
            depth,
            compression: Compression::None,
            space,
        }
    }

    /// A spec that changes nothing: 300 ppi, no resize, no metadata. What every test
    /// wants unless it is about one of those, and the arguments `write` used to take
    /// positionally.
    fn spec(target: Target, tm: ToneMap) -> Spec {
        Spec::new(target, tm, OutputParams::default(), Tail::default(), None)
    }

    #[test]
    fn frame_is_composed_after_toning_and_sharpening_without_touching_either_side() {
        let mapped = vec![0.05, 0.10, 0.20, 0.35, 0.50, 0.65, 0.80, 0.90, 0.98];
        let toning = selenium();
        let toned = tone(&mapped, &toning);
        let sharpen = raw_core::sharpen::SharpenParams {
            enabled: true,
            ..Default::default()
        };
        let processed = raw_core::sharpen::apply_toned(&toned, 3, 3, &sharpen);
        let layout = raw_core::frame::PixelLayout {
            image: Dims { w: 3, h: 3 },
            outer: Dims { w: 5, h: 5 },
            left: 1,
            top: 1,
            right: 1,
            bottom: 1,
            trim: 0,
        };
        let color = [0xf5, 0xf2, 0xe8];
        let Print::Lab(framed) = Print::Lab(processed.clone()).framed(layout, color) else {
            panic!("a toned print must stay three-channel")
        };
        let [l, a, b] = raw_core::colour::oklab_of_srgb(color);
        let paper = [l.powi(3), a, b];

        for y in 0..5 {
            for x in 0..5 {
                let px = &framed[(y * 5 + x) * 3..(y * 5 + x + 1) * 3];
                if (1..4).contains(&x) && (1..4).contains(&y) {
                    let source = ((y - 1) * 3 + (x - 1)) * 3;
                    assert_eq!(px, &processed[source..source + 3], "image sample moved");
                } else {
                    assert_eq!(px, paper, "toning or sharpening reached the frame");
                }
            }
        }
    }

    #[test]
    fn trim_line_is_one_black_pixel_outside_the_physical_frame() {
        let layout = raw_core::frame::PixelLayout {
            image: Dims { w: 3, h: 3 },
            outer: Dims { w: 5, h: 5 },
            left: 1,
            top: 1,
            right: 1,
            bottom: 1,
            trim: 1,
        };
        let Print::Grey(framed) = Print::Grey(vec![0.5; 9]).framed(layout, [0xff, 0xff, 0xff])
        else {
            panic!("a neutral frame must remain greyscale")
        };

        for y in 0..5 {
            for x in 0..5 {
                let value = framed[y * 5 + x];
                if x == 0 || y == 0 || x == 4 || y == 4 {
                    assert_eq!(value, 0.0, "trim pixel must be black");
                } else {
                    assert_eq!(value, 0.5, "trim must not enter the photograph");
                }
            }
        }
    }

    #[test]
    fn a_coloured_frame_promotes_a_greyscale_master_and_changes_final_dimensions() {
        let frame = raw_core::FrameParams {
            enabled: true,
            margins: raw_core::frame::Margins::all(1.0),
            color: [0xf5, 0xdc, 0xdc],
            ..Default::default()
        };
        let spec = Spec::new(
            Target::default(),
            ToneMap::Clip,
            OutputParams {
                ppi: 10.0,
                ..Default::default()
            },
            Tail {
                frame,
                ..Default::default()
            },
            None,
        );
        assert_eq!(spec.target.space, Space::EciRgbV2);
        assert_eq!(spec.dims(Dims { w: 100, h: 80 }), Dims { w: 120, h: 100 });
        assert_eq!(
            spec.image_dims(Dims { w: 100, h: 80 }),
            Dims { w: 100, h: 80 }
        );
    }

    const ALL: [Target; 6] = [
        t(Container::Tiff, Depth::Sixteen, Compression::None),
        t(Container::Tiff, Depth::Eight, Compression::None),
        t(Container::Tiff, Depth::Sixteen, Compression::Deflate),
        t(Container::Tiff, Depth::Eight, Compression::Deflate),
        t(Container::Png, Depth::Sixteen, Compression::None),
        t(Container::Png, Depth::Eight, Compression::None),
    ];

    /// A proof spec at `scale`.
    fn proof_spec(target: Target, scale: ProofScale) -> Spec {
        Spec::proof(
            target,
            ToneMap::Clip,
            OutputParams::default(),
            Tail::default(),
            None,
            scale,
        )
    }

    #[test]
    fn the_three_l_star_profiles_share_one_transfer_curve() {
        // The whole argument for eciRGB v2 as the toned master, checked against the
        // shipped bytes rather than taken on trust: when a toner turns 1 channel into
        // 3, monostar -> eciRGB v2 must change only the channel count. If someone
        // swaps a profile for one with a different TRC, that stops being true and the
        // export silently re-tones every print.
        //
        // The parametric curve is `para` type 3 with g=3, a=1/1.16. Both are stored
        // as s15Fixed16, so the comparison is on exact bytes.
        let g = 3.0_f32;
        let a = 1.0 / 1.16_f32;
        for (name, icc, tag) in [
            ("monostar", MONOSTAR_ICC, b"kTRC"),
            ("eciRGB v2", ECIRGB_ICC, b"rTRC"),
        ] {
            let (off, _) = find_tag(icc, tag).unwrap_or_else(|| panic!("{name} has no {tag:?}"));
            assert_eq!(
                &icc[off..off + 4],
                b"para",
                "{name} is not a parametric curve"
            );
            assert_eq!(
                u16::from_be_bytes([icc[off + 8], icc[off + 9]]),
                3,
                "{name} wrong type"
            );
            let got_g = s15(&icc[off + 12..off + 16]);
            let got_a = s15(&icc[off + 16..off + 20]);
            assert!((got_g - g).abs() < 1e-4, "{name} g = {got_g}");
            assert!((got_a - a).abs() < 1e-4, "{name} a = {got_a}");
        }
    }

    fn s15(b: &[u8]) -> f32 {
        i32::from_be_bytes(b.try_into().unwrap()) as f32 / 65536.0
    }

    /// Offset and size of an ICC tag, from the tag table.
    fn find_tag(icc: &[u8], sig: &[u8; 4]) -> Option<(usize, usize)> {
        let n = u32::from_be_bytes(icc[128..132].try_into().ok()?) as usize;
        (0..n).find_map(|i| {
            let e = 132 + 12 * i;
            (&icc[e..e + 4] == sig).then(|| {
                (
                    u32::from_be_bytes(icc[e + 4..e + 8].try_into().unwrap()) as usize,
                    u32::from_be_bytes(icc[e + 8..e + 12].try_into().unwrap()) as usize,
                )
            })
        })
    }

    #[test]
    fn prostar_is_prophoto_gamut_in_the_l_star_curve() {
        // Which is why plain ProPhoto is not offered: it would be the same gamut with
        // the only different transfer function in the set. ProPhoto's primaries, D50
        // adapted, with blue at y = 0.
        let (off, _) = find_tag(PROSTAR_ICC, b"bXYZ").expect("bXYZ");
        let y = s15(&PROSTAR_ICC[off + 12..off + 16]);
        // Effectively zero — 6/65536, which is s15Fixed16 rounding around nothing. A
        // primary that contributes no luminance at all is not a colour anyone can
        // see, and that is the point: part of this gamut is outside human vision, so
        // it is offered for interoperability and not because wider is better.
        assert!(
            y < 1e-3,
            "ProStar's blue primary should carry no luminance, got {y}"
        );
        // And it is an RGB profile, so it needs colour the pipeline has not got.
        assert_eq!(&PROSTAR_ICC[16..20], b"RGB ");
        assert!(Space::ProStar.is_rgb());
        assert!(Space::EciRgbV2.is_rgb());
        assert!(!Space::Monostar.is_rgb() && !Space::Srgb.is_rgb());
    }

    #[test]
    fn prostar_remains_readable_for_old_files_but_is_not_an_export_option() {
        assert_eq!(Space::from_key("prostar"), Some(Space::ProStar));
        assert!(!Space::UI_ORDER.contains(&Space::ProStar));
        assert_eq!(Space::ProStar.selectable(), Space::EciRgbV2);
    }

    /// **Each space encodes in its own curve, and sRGB is the one that differs.**
    ///
    /// `Space::encode` routes on the variant, so a space added later falls into the
    /// `_ => lstar_encode` arm by default — which is right for the three that share
    /// monostar's curve and silently wrong for anything that does not. This is what
    /// says so. the maintainer flagged exactly this risk when asking for the RGB spaces.
    #[test]
    fn every_space_encodes_in_its_own_curve() {
        // The three that share the L* curve must agree bit for bit — that is the
        // property the whole "changes the channel count and nothing else" claim rests
        // on, and it is asserted rather than assumed.
        for v in [0.0f32, 0.02, 0.18, 0.5, 1.0] {
            let l = Space::Monostar.encode(v);
            assert_eq!(
                Space::EciRgbV2.encode(v),
                l,
                "eciRGB v2 drifted from L* at {v}"
            );
            assert_eq!(
                Space::ProStar.encode(v),
                l,
                "ProStarRGB drifted from L* at {v}"
            );
            assert_eq!(
                l,
                raw_core::display::lstar_encode(v),
                "monostar is not L* at {v}"
            );
        }

        // sRGB is display-referred and carries its own transfer function. Getting this
        // wrong produces a file that looks plausible and is off by several L*.
        //
        // **The sample points are chosen, not obvious.** The two curves *cross* near
        // linear 0.02 — L* runs below sRGB in the deep shadows and above it from about
        // 0.03 — and they converge again approaching white. At 0.02 they differ by
        // 0.003, so a test that happened to probe there would pass an implementation
        // that had them swapped. Measured: 0.05 / 0.18 / 0.5 differ by 0.020, 0.034 and
        // 0.025, which is where the two encodings actually disagree.
        for v in [0.05f32, 0.18, 0.5] {
            let srgb = Space::Srgb.encode(v);
            assert_eq!(
                srgb,
                raw_core::display::srgb_encode(v),
                "sRGB is not the sRGB curve"
            );
            assert!(
                (srgb - Space::Monostar.encode(v)).abs() > 0.01,
                "sRGB and L* agree at {v}, which means one of them is not what it says"
            );
        }

        // The ends are shared, because every curve maps black to black and white to
        // white — a difference there would be a broken curve rather than a different one.
        assert_eq!(Space::Srgb.encode(0.0), 0.0);
        assert!((Space::Srgb.encode(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_proof_ignores_the_masters_resample() {
        // The promise the two settings rest on: setting a proof to a quarter cannot
        // change what the TIFF writes, and a print size cannot change the proof.
        let picture = Dims { w: 4000, h: 3000 };
        let resized = OutputParams {
            resize: Some(raw_core::output::Resize {
                axis: raw_core::output::Axis::Width,
                inches: 10.0,
            }),
            ..Default::default()
        };
        let master = Spec::new(
            t(Container::Tiff, Depth::Sixteen, Compression::None),
            ToneMap::Clip,
            resized,
            Tail::default(),
            None,
        );
        let proof = Spec {
            proof: Some(ProofScale::Quarter),
            ..Spec::new(
                Target::proof(),
                ToneMap::Clip,
                resized,
                Tail::default(),
                None,
            )
        };
        // The master follows its resample: 10 in at 300 ppi.
        assert_eq!(master.dims(picture), Dims { w: 3000, h: 2250 });
        // The proof follows the picture, not the master.
        assert_eq!(proof.dims(picture), Dims { w: 1000, h: 750 });
    }

    #[test]
    fn every_proof_scale_is_a_fraction_of_the_picture() {
        let pic = Dims { w: 1200, h: 900 };
        assert_eq!(ProofScale::Full.dims(pic), pic);
        assert_eq!(ProofScale::Half.dims(pic), Dims { w: 600, h: 450 });
        assert_eq!(ProofScale::Third.dims(pic), Dims { w: 400, h: 300 });
        assert_eq!(ProofScale::Quarter.dims(pic), Dims { w: 300, h: 225 });
        // A one-pixel picture must not scale to zero — an encoder handed a zero
        // dimension is a panic, not an error.
        assert_eq!(
            ProofScale::Quarter.dims(Dims { w: 1, h: 1 }),
            Dims { w: 1, h: 1 }
        );
    }

    #[test]
    fn jpeg_is_eight_bit_only_and_settle_enforces_it() {
        assert_eq!(Container::Jpeg.depths(), &[Depth::Eight]);
        assert!(!Container::Jpeg.supports(Depth::Sixteen));
        assert!(Container::Png.supports(Depth::Sixteen));
        // A hand-edited settings.toml can pair 16-bit with JPEG. `settle` is what
        // stops that reaching the writer.
        let mut t = ts(Container::Jpeg, Depth::Sixteen, Space::Srgb);
        t.settle();
        assert_eq!(t.depth, Depth::Eight, "settle left an impossible target");
    }

    #[test]
    fn a_jpeg_proof_carries_its_profile_and_the_channels_its_space_declares() {
        // sRGB travels as three channels so the ICC is valid on it; monostar stays
        // one, because its profile describes one. Either way the file is tagged.
        let scene = ramp(64, 8);
        for (space, components, icc) in [
            (Space::Srgb, 3u8, SRGB_ICC),
            (Space::Monostar, 1u8, MONOSTAR_ICC),
        ] {
            let target = ts(Container::Jpeg, Depth::Eight, space);
            let path = tmp(&format!("proof.{}.jpg", space.key()));
            write(&path, 64, 8, &scene, &proof_spec(target, ProofScale::Half)).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[0..2], &[0xFF, 0xD8], "not a JPEG");
            // Half of 64x8, and the frame header says so.
            assert_eq!(dims_of(&path, Container::Jpeg), (32, 4));
            // SOF0's component count is the byte after width.
            let i = bytes.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
            assert_eq!(
                bytes[i + 9],
                components,
                "{} wrote the wrong channel count",
                space.label()
            );
            // The profile goes in an APP2 segment, split across chunks if it is large;
            // these are all small enough for one, so it appears verbatim.
            assert!(
                bytes.windows(icc.len()).any(|w| w == icc),
                "{} did not embed its profile",
                space.label()
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn an_srgb_proof_is_rgb_and_carries_the_profile() {
        // the maintainer's call, and it reverses the first version: a proof is the file that
        // *leaves*, so what matters is that the next application recognises the
        // intent. An embedded ICC is what every application looks for. Three
        // identical channels is the price, and for a half-size 8-bit file it is
        // worth paying — see `Space::channels`.
        let scene = ramp(32, 4);
        let path = tmp("proof-srgb.png");
        write(
            &path,
            32,
            4,
            &scene,
            &proof_spec(
                ts(Container::Png, Depth::Eight, Space::Srgb),
                ProofScale::Full,
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let info = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        assert_eq!(
            info.info().icc_profile.as_deref(),
            Some(SRGB_ICC),
            "an sRGB proof must travel with its profile"
        );
        assert_eq!(info.info().color_type, png::ColorType::Rgb);
        // The three channels are identical, or this is not the same picture.
        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        assert!(
            buf[..frame.buffer_size()]
                .chunks_exact(3)
                .all(|p| p[0] == p[1] && p[1] == p[2]),
            "a monochrome proof came out with unequal channels"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_master_stays_one_channel_and_a_proof_does_not_make_it_three() {
        // The objection that was right about masters and does not reach proofs: a
        // master must not overstate what is in it.
        assert!(Space::Monostar.is_grey());
        assert_eq!(Space::Monostar.channels(), 1);
        for s in [Space::Srgb, Space::EciRgbV2, Space::ProStar] {
            assert_eq!(s.channels(), 3, "{} should be RGB", s.label());
        }
    }

    /// The guard that replaced "the TIFF writer refuses a three-channel space".
    ///
    /// That refusal existed because there was no way to produce three real channels,
    /// and it said so. Three channels exist now, so the guard moved rather than went.
    /// **What must agree is the sample count and the profile** — one channel of data under a profile describing three
    /// renders differently everywhere it is opened, and so does the reverse.
    #[test]
    fn a_tiff_writes_as_many_channels_as_its_profile_describes() {
        let scene = ramp(16, 2);

        let colour_type = |path: &std::path::Path| {
            let bytes = std::fs::read(path).unwrap();
            tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes))
                .unwrap()
                .colortype()
                .unwrap()
        };

        // Greyscale: one channel, greyscale profile.
        let path = tmp("chan-grey.tif");
        write(
            &path,
            16,
            2,
            &scene,
            &spec(
                ts(Container::Tiff, Depth::Sixteen, Space::Monostar),
                ToneMap::Clip,
            ),
        )
        .unwrap();
        assert_eq!(colour_type(&path), tiff::ColorType::Gray(16));
        let _ = std::fs::remove_file(&path);

        // Toned into an RGB space: three channels, RGB profile.
        let path = tmp("chan-rgb.tif");
        let mut toned = spec(
            ts(Container::Tiff, Depth::Sixteen, Space::EciRgbV2),
            ToneMap::Clip,
        );
        toned.toning = selenium();
        write(&path, 16, 2, &scene, &toned).unwrap();
        assert_eq!(colour_type(&path), tiff::ColorType::RGB(16));
        let _ = std::fs::remove_file(&path);

        // And at eight bits, which is a different colour type on both sides.
        let path = tmp("chan-rgb8.tif");
        let mut toned = spec(
            ts(Container::Tiff, Depth::Eight, Space::EciRgbV2),
            ToneMap::Clip,
        );
        toned.toning = selenium();
        write(&path, 16, 2, &scene, &toned).unwrap();
        assert_eq!(colour_type(&path), tiff::ColorType::RGB(8));
        let _ = std::fs::remove_file(&path);
    }

    /// **The migration guarantee, on the export side.** An untoned frame written to a
    /// greyscale master must be the file it has always been — the toning module is not
    /// merely inactive, it is absent from the bytes.
    #[test]
    fn an_untoned_export_is_byte_identical() {
        let scene = ramp(24, 6);
        let target = ts(Container::Tiff, Depth::Sixteen, Space::Monostar);

        let a = tmp("untoned-a.tif");
        write(&a, 24, 6, &scene, &spec(target, ToneMap::Clip)).unwrap();

        // Enabled, but with nothing in it. `is_active` is false, so the tail never
        // leaves one channel.
        let b = tmp("untoned-b.tif");
        let mut on = spec(target, ToneMap::Clip);
        on.toning.enabled = true;
        write(&b, 24, 6, &scene, &on).unwrap();

        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// A toned export really is toned, and the neutrals inside it are still exactly
    /// neutral — which is `raw_core::colour`'s bypass showing up in a file.
    #[test]
    fn a_toned_master_has_chroma_where_the_shadows_are() {
        let scene = ramp(64, 4);
        let path = tmp("toned-master.tif");
        let target = ts(Container::Tiff, Depth::Sixteen, Space::EciRgbV2);
        let mut toned = spec(target, ToneMap::Clip);
        toned.toning = selenium();
        write(&path, 64, 4, &scene, &toned).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
            panic!("expected 16-bit samples");
        };

        // Somewhere in the frame the three channels differ — colour reached the disk.
        let spread = px
            .chunks_exact(3)
            .map(|c| c.iter().max().unwrap() - c.iter().min().unwrap())
            .max()
            .unwrap();
        assert!(
            spread > 64,
            "a selenium-toned master should carry chroma, got {spread}"
        );

        // And paper white is still exactly neutral, which is `raw_core::colour`'s
        // bypass showing up in a file rather than in a unit test.
        let white = px.chunks_exact(3).max_by_key(|c| c[0]).unwrap();
        assert_eq!(
            white[0], white[1],
            "paper white picked up a tint: {white:?}"
        );
        assert_eq!(
            white[1], white[2],
            "paper white picked up a tint: {white:?}"
        );
    }

    /// Toning at zero strength writes the same file as no toning at all, through the
    /// whole tail — resample, grain and sharpening included.
    #[test]
    fn a_bath_at_zero_strength_changes_no_byte() {
        let scene = ramp(24, 6);
        let target = ts(Container::Tiff, Depth::Sixteen, Space::Monostar);

        let a = tmp("inert-a.tif");
        write(&a, 24, 6, &scene, &spec(target, ToneMap::Clip)).unwrap();

        let b = tmp("inert-b.tif");
        let mut on = spec(target, ToneMap::Clip);
        on.toning.enabled = true;
        on.toning.apply("selenium", 0.0);
        write(&b, 24, 6, &scene, &on).unwrap();

        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// A full-strength selenium bath, for the tests above.
    fn selenium() -> raw_core::ToningParams {
        raw_core::ToningParams {
            enabled: true,
            applied: vec![raw_core::toning::Applied {
                key: "selenium",
                amount: 1.0,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_monostar_proof_is_still_tagged() {
        // The other half: when you know the recipient is colour-managed, the proof
        // carries the same profile the master does.
        let scene = ramp(32, 4);
        let path = tmp("proof-mono.png");
        write(
            &path,
            32,
            4,
            &scene,
            &proof_spec(
                ts(Container::Png, Depth::Eight, Space::Monostar),
                ProofScale::Full,
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let info = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        assert_eq!(info.info().icc_profile.as_deref(), Some(MONOSTAR_ICC));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_two_encodings_differ_and_each_matches_its_own_curve() {
        // The defect this whole split exists to avoid: an L*-encoded file rendered as
        // if it were sRGB comes out visibly light. Mid-grey is where it shows most.
        let v = 0.18_f32;
        let l = Space::Monostar.encode(v);
        let s = Space::Srgb.encode(v);
        assert!((l - raw_core::display::lstar_encode(v)).abs() < 1e-6);
        assert!((s - raw_core::display::srgb_encode(v)).abs() < 1e-6);
        // L* puts 18% grey near 50; sRGB near 46. Reading one as the other is the
        // ~3-point lift this module's header records.
        assert!(l > s, "L* should sit above sRGB at mid grey: {l} vs {s}");
        // Both are anchored at the ends, or a proof would not match its master's
        // black and white.
        for space in [Space::Monostar, Space::Srgb] {
            assert!(space.encode(0.0).abs() < 1e-6);
            assert!((space.encode(1.0) - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn the_embedded_profile_is_a_greyscale_l_star_profile() {
        // If this file is ever swapped for an RGB profile, every export silently
        // starts tagging 1-channel data with a 3-channel space.
        //
        // **Against the generator, not against a byte count.** A length was what this
        // asserted first, and a length is exactly what a stale copy of the profile
        // still satisfies — which is how the app came to embed the pre-regeneration
        // binary while `profiles/monostar.icc` had moved on. `raw_core::icc::monostar`
        // is the definition, so comparing to it is the check that cannot pass while
        // wrong.
        assert_eq!(MONOSTAR_ICC, raw_core::icc::monostar());
        assert_eq!(
            &MONOSTAR_ICC[16..20],
            b"GRAY",
            "monostar must be a GRAY profile"
        );
        assert_eq!(&MONOSTAR_ICC[12..16], b"mntr");
        let declared = u32::from_be_bytes(MONOSTAR_ICC[0..4].try_into().unwrap());
        assert_eq!(declared as usize, MONOSTAR_ICC.len());
    }

    #[test]
    fn every_target_embeds_monostar() {
        // The load-bearing claim of this module: whatever you pick, the file is
        // colour-managed. An untagged export is indistinguishable from a correct one
        // until it reaches someone else's screen.
        let scene = ramp(64, 8);
        for target in ALL {
            let path = tmp(&format!(
                "tagged.{}.{:?}.{:?}",
                target.extension(),
                target.depth,
                target.compression
            ));
            write(&path, 64, 8, &scene, &spec(target, ToneMap::Clip)).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let embedded = match target.container {
                // TIFF stores the profile verbatim.
                Container::Tiff => bytes.windows(MONOSTAR_ICC.len()).any(|w| w == MONOSTAR_ICC),
                // PNG deflates it inside iCCP, so decode to check.
                Container::Png => {
                    let dec = png::Decoder::new(std::io::Cursor::new(&bytes));
                    let reader = dec.read_info().unwrap();
                    reader.info().icc_profile.as_deref() == Some(MONOSTAR_ICC)
                }
                // Not in `ALL` — masters are never JPEG. See `Container::UI_ORDER`.
                Container::Jpeg => unreachable!("a master is never a JPEG"),
            };
            assert!(embedded, "{target:?} did not embed monostar");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn every_target_is_single_channel_greyscale() {
        let scene = ramp(32, 4);
        for target in ALL {
            let path = tmp(&format!(
                "gray.{}.{:?}.{:?}",
                target.extension(),
                target.depth,
                target.compression
            ));
            write(&path, 32, 4, &scene, &spec(target, ToneMap::Clip)).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            match target.container {
                Container::Jpeg => unreachable!("a master is never a JPEG"),
                Container::Tiff => {
                    let mut dec =
                        tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
                    let expect = match target.depth {
                        Depth::Eight => tiff::ColorType::Gray(8),
                        Depth::Sixteen => tiff::ColorType::Gray(16),
                    };
                    assert_eq!(dec.colortype().unwrap(), expect, "{target:?}");
                }
                Container::Png => {
                    let dec = png::Decoder::new(std::io::Cursor::new(&bytes));
                    let reader = dec.read_info().unwrap();
                    assert_eq!(
                        reader.info().color_type,
                        png::ColorType::Grayscale,
                        "{target:?}"
                    );
                    let expect = match target.depth {
                        Depth::Eight => png::BitDepth::Eight,
                        Depth::Sixteen => png::BitDepth::Sixteen,
                    };
                    assert_eq!(reader.info().bit_depth, expect, "{target:?}");
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn deflate_is_bit_identical_to_uncompressed() {
        // "Compressed" reads as "degraded", and it is a fair worry — JPEG-in-TIFF
        // really would throw data away. Deflate does not: it is the same entropy
        // coding as ZIP, and the horizontal predictor is exact integer differencing.
        // This proves it on real data rather than asking anyone to take it on faith.
        //
        // A ramp with fine steps and noise, so any quantisation or rounding in the
        // predictor path would show up rather than being masked by flat regions.
        let (w, h) = (301usize, 97usize);
        let scene: Vec<f32> = (0..w * h)
            .map(|i| {
                let base = (i % w) as f32 / (w - 1) as f32;
                let jitter = ((i * 2654435761) % 1024) as f32 / 1024.0 * 0.002;
                (base + jitter).clamp(0.0, 1.0)
            })
            .collect();

        for depth in [Depth::Sixteen, Depth::Eight] {
            let plain = tmp(&format!("plain.{depth:?}.tif"));
            let squashed = tmp(&format!("deflate.{depth:?}.tif"));
            write(
                &plain,
                w as u32,
                h as u32,
                &scene,
                &spec(
                    t(Container::Tiff, depth, Compression::None),
                    ToneMap::AGX_DEFAULT,
                ),
            )
            .unwrap();
            write(
                &squashed,
                w as u32,
                h as u32,
                &scene,
                &spec(
                    t(Container::Tiff, depth, Compression::Deflate),
                    ToneMap::AGX_DEFAULT,
                ),
            )
            .unwrap();

            let decode = |p: &std::path::Path| {
                let bytes = std::fs::read(p).unwrap();
                let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
                match dec.read_image().unwrap() {
                    tiff::decoder::DecodingResult::U8(v) => {
                        v.into_iter().map(u16::from).collect::<Vec<_>>()
                    }
                    tiff::decoder::DecodingResult::U16(v) => v,
                    _ => panic!("unexpected sample type"),
                }
            };
            let a = decode(&plain);
            let b = decode(&squashed);
            assert_eq!(a.len(), w * h);
            assert_eq!(a, b, "{depth:?}: deflate changed the samples");

            // ...and it is actually smaller, or there would be no reason to offer it.
            let (sa, sb) = (
                std::fs::metadata(&plain).unwrap().len(),
                std::fs::metadata(&squashed).unwrap().len(),
            );
            assert!(
                sb < sa,
                "{depth:?}: deflate ({sb}) was not smaller than plain ({sa})"
            );

            let _ = std::fs::remove_file(&plain);
            let _ = std::fs::remove_file(&squashed);
        }
    }

    #[test]
    fn sixteen_bit_keeps_precision_eight_bit_cannot() {
        // The entire point of a 16-bit master: adjacent scene values that collapse
        // onto one 8-bit code stay distinct.
        let scene = vec![0.5000f32, 0.5008];
        let path = tmp("precision.tif");
        write(
            &path,
            2,
            1,
            &scene,
            &spec(
                t(Container::Tiff, Depth::Sixteen, Compression::None),
                ToneMap::Clip,
            ),
        )
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
            panic!("expected u16");
        };
        assert_ne!(px[0], px[1], "16-bit collapsed a sub-8-bit difference");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sixteen_bit_png_is_big_endian() {
        // PNG stores 16-bit samples big-endian regardless of host order. Getting this
        // backwards produces a file that decodes to noise, so it is worth pinning.
        let scene = vec![1.0f32]; // -> L* 1.0 -> 65535 -> FF FF
        let path = tmp("endian.png");
        write(
            &path,
            1,
            1,
            &scene,
            &spec(
                t(Container::Png, Depth::Sixteen, Compression::None),
                ToneMap::Clip,
            ),
        )
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(&bytes));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..info.buffer_size()], &[0xFF, 0xFF]);

        // ...and a mid value round-trips through the decoder as big-endian.
        let scene = vec![0.25f32];
        write(
            &path,
            1,
            1,
            &scene,
            &spec(
                t(Container::Png, Depth::Sixteen, Compression::None),
                ToneMap::Clip,
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        let decoded = u16::from_be_bytes([buf[0], buf[1]]);
        let expected = (lstar_encode(0.25) * 65535.0 + 0.5) as u16;
        assert!(
            decoded.abs_diff(expected) <= 1,
            "got {decoded}, expected {expected}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_ramp_survives_every_target_monotonically() {
        let scene = ramp(256, 2);
        for target in ALL {
            let path = tmp(&format!(
                "ramp.{}.{:?}.{:?}",
                target.extension(),
                target.depth,
                target.compression
            ));
            write(&path, 256, 2, &scene, &spec(target, ToneMap::Clip)).unwrap();
            let bytes = std::fs::read(&path).unwrap();

            // Read back as f32 in [0,1] whatever the container and depth.
            let row: Vec<f32> = match target.container {
                Container::Jpeg => unreachable!("a master is never a JPEG"),
                Container::Tiff => {
                    let mut dec =
                        tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
                    match dec.read_image().unwrap() {
                        tiff::decoder::DecodingResult::U8(v) => {
                            v[..256].iter().map(|&s| s as f32 / 255.0).collect()
                        }
                        tiff::decoder::DecodingResult::U16(v) => {
                            v[..256].iter().map(|&s| s as f32 / 65535.0).collect()
                        }
                        _ => panic!("unexpected sample type"),
                    }
                }
                Container::Png => {
                    let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                        .read_info()
                        .unwrap();
                    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
                    reader.next_frame(&mut buf).unwrap();
                    match target.depth {
                        Depth::Eight => buf[..256].iter().map(|&s| s as f32 / 255.0).collect(),
                        Depth::Sixteen => buf[..512]
                            .chunks_exact(2)
                            .map(|c| u16::from_be_bytes([c[0], c[1]]) as f32 / 65535.0)
                            .collect(),
                    }
                }
            };

            // Dither makes 8-bit locally non-monotone by design, so compare coarse
            // blocks rather than adjacent samples.
            let block = |i: usize| row[i * 16..(i + 1) * 16].iter().sum::<f32>() / 16.0;
            for i in 1..16 {
                assert!(block(i) >= block(i - 1), "{target:?} inverted at block {i}");
            }
            assert!(row[0] < 0.02, "{target:?} lifted black: {}", row[0]);
            assert!(row[255] > 0.98, "{target:?} lost white: {}", row[255]);
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn eight_bit_is_dithered_and_sixteen_bit_is_not() {
        // A flat field: at 8 bits dither must break it up, at 16 bits it must be
        // pristine. Getting this backwards means either banding or a noisy master.
        let scene = vec![0.3f32; 64 * 64];

        let path = tmp("flat8.tif");
        write(
            &path,
            64,
            64,
            &scene,
            &spec(
                t(Container::Tiff, Depth::Eight, Compression::None),
                ToneMap::Clip,
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U8(px8) = dec.read_image().unwrap() else {
            panic!("expected u8");
        };
        assert!(
            px8.iter().any(|&v| v != px8[0]),
            "8-bit export was not dithered"
        );

        let path = tmp("flat16.tif");
        write(
            &path,
            64,
            64,
            &scene,
            &spec(
                t(Container::Tiff, Depth::Sixteen, Compression::None),
                ToneMap::Clip,
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U16(px16) = dec.read_image().unwrap() else {
            panic!("expected u16");
        };
        assert!(
            px16.iter().all(|&v| v == px16[0]),
            "16-bit master was dithered"
        );
    }

    #[test]
    fn the_dither_switch_reaches_the_file_at_eight_bits_and_is_inert_at_sixteen() {
        // The EXPORT module's checkbox, measured at the far end. the maintainer asked for the
        // control to move there from DISPLAY, and a control that moved without also
        // acquiring a job would be worse than the one that was in the wrong module: the
        // 8-bit path dithered unconditionally, so `dither: false` had nowhere to land.
        let scene = vec![0.3f32; 64 * 64];
        let flat = |depth: Depth, dither: bool, name: &str| -> bool {
            let path = tmp(name);
            let spec = Spec {
                dither,
                ..spec(t(Container::Tiff, depth, Compression::None), ToneMap::Clip)
            };
            write(&path, 64, 64, &scene, &spec).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
            let flat = match dec.read_image().unwrap() {
                tiff::decoder::DecodingResult::U8(p) => p.iter().all(|&v| v == p[0]),
                tiff::decoder::DecodingResult::U16(p) => p.iter().all(|&v| v == p[0]),
                _ => panic!("unexpected sample type"),
            };
            let _ = std::fs::remove_file(&path);
            flat
        };
        assert!(
            flat(Depth::Eight, false, "off8.tif"),
            "the switch did not reach the file"
        );
        assert!(
            !flat(Depth::Eight, true, "on8.tif"),
            "8-bit lost its dither"
        );
        // And 16 bits is unmoved by it in either position — that half of the rule is
        // still structural, `samples16` having no dither to switch off.
        assert!(
            flat(Depth::Sixteen, true, "on16.tif"),
            "a 16-bit master was dithered"
        );
        assert!(flat(Depth::Sixteen, false, "off16.tif"));
    }

    #[test]
    fn export_is_not_the_display_encode() {
        // These have been confused before, with a measurable ~3 L* midtone lift.
        use raw_core::DisplayParams;
        let v = 0.18f32;
        let display = DisplayParams {
            enabled: true,
            tone_map: ToneMap::Clip,
            gamma: 2.2,
            dither: false,
        };
        let master = lstar_encode(tone_map(v, ToneMap::Clip));
        let screen = raw_core::display::encode(v, &display);
        assert!(
            (master - screen).abs() > 0.02,
            "export and display encodings converged: {master} vs {screen}"
        );
    }

    // ── output size, resolution and metadata ──────────────────────────────────

    fn resized(inches: f32, axis: raw_core::Axis, ppi: f32) -> OutputParams {
        OutputParams {
            ppi,
            resize: Some(raw_core::Resize { inches, axis }),
            ..Default::default()
        }
    }

    /// Decoded dimensions, whatever the container.
    fn dims_of(path: &std::path::Path, container: Container) -> (u32, u32) {
        let bytes = std::fs::read(path).unwrap();
        match container {
            Container::Tiff => {
                let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
                dec.dimensions().unwrap()
            }
            Container::Png => {
                let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                    .read_info()
                    .unwrap();
                (reader.info().width, reader.info().height)
            }
            // The frame header: `FFC0` marker, then length, precision, height,
            // width — each big-endian u16. Parsed by hand rather than pulling in a
            // decoder, because two numbers do not justify a dependency.
            Container::Jpeg => {
                let i = bytes
                    .windows(2)
                    .position(|w| w == [0xFF, 0xC0])
                    .expect("a baseline JPEG has an SOF0 marker");
                let at = i + 5;
                let h = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
                let w = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]);
                (u32::from(w), u32::from(h))
            }
        }
    }

    #[test]
    fn a_print_size_resizes_the_file_and_keeps_the_aspect() {
        // 4 inches at 300 ppi is 1200 px from a 600 px original — a 2x upsample — and
        // the height must follow the source's 2:1 shape without being asked to.
        let scene = ramp(600, 300);
        for target in ALL {
            let path = tmp(&format!(
                "sized.{}.{:?}.{:?}",
                target.extension(),
                target.depth,
                target.compression
            ));
            let spec = Spec::new(
                target,
                ToneMap::Clip,
                resized(4.0, raw_core::Axis::Width, 300.0),
                Tail::default(),
                None,
            );
            write(&path, 600, 300, &scene, &spec).unwrap();
            assert_eq!(dims_of(&path, target.container), (1200, 600), "{target:?}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn no_print_size_writes_the_pictures_own_pixels() {
        // The default mode, and the property that makes the PPI field safe to play
        // with: whatever it is set to, the pixels are the picture's.
        let scene = ramp(64, 32);
        for ppi in [72.0, 300.0, 1440.0] {
            let path = tmp("native.tif");
            let spec = Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams {
                    ppi,
                    ..Default::default()
                },
                Tail::default(),
                None,
            );
            write(&path, 64, 32, &scene, &spec).unwrap();
            assert_eq!(
                dims_of(&path, Container::Tiff),
                (64, 32),
                "{ppi} ppi resized the file"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_ppi_setting_reaches_the_files_own_tag() {
        // The bug the brief named: `export::write` was called with a bare `300` while
        // three readouts quoted a constant, so a setting could have moved the app's
        // numbers and left the file claiming something else. Both containers, because
        // they record resolution in different units and only one of them is inches.
        for ppi in [72.0f32, 240.0, 360.0, 1200.0] {
            let scene = ramp(8, 4);
            let out = OutputParams {
                ppi,
                ..Default::default()
            };

            let path = tmp("ppi.tif");
            write(
                &path,
                8,
                4,
                &scene,
                &Spec::new(Target::default(), ToneMap::Clip, out, Tail::default(), None),
            )
            .unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
            let res = dec.get_tag(tiff::tags::Tag::XResolution).unwrap();
            let tiff::decoder::ifd::Value::Rational(n, d) = res else {
                panic!("XResolution was not a rational: {res:?}");
            };
            assert_eq!(
                (n as f32 / d as f32).round(),
                ppi,
                "TIFF tagged the wrong resolution"
            );
            let unit: u16 = dec
                .get_tag_unsigned(tiff::tags::Tag::ResolutionUnit)
                .unwrap();
            assert_eq!(unit, 2, "TIFF resolution must be per inch");
            let _ = std::fs::remove_file(&path);

            // PNG's pHYs is pixels per METRE, so this also checks the conversion
            // rather than only that something was written.
            let png_target = t(Container::Png, Depth::Eight, Compression::None);
            let path = tmp("ppi.png");
            write(
                &path,
                8,
                4,
                &scene,
                &Spec::new(png_target, ToneMap::Clip, out, Tail::default(), None),
            )
            .unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
                .read_info()
                .unwrap();
            let dims = reader.info().pixel_dims.unwrap();
            assert_eq!(dims.unit, png::Unit::Meter);
            let back = dims.xppu as f32 * 0.0254;
            assert!(
                (back - ppi).abs() < 1.0,
                "PNG says {back} ppi, wanted {ppi}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_resize_happens_after_the_tone_map_and_not_before() {
        // The ordering claim `raw_core::resample`'s module note is built on, made
        // falsifiable. With `ToneMap::Clip` a scene value of 100.0 becomes 1.0, so a
        // 2:1 downsample of [100, 0] is:
        //
        //   tone map first (correct):  clip -> [1, 0], average -> 0.5
        //   resize first    (wrong):   average -> 50, clip -> 1.0
        //
        // A factor of two in the output, from an ordering that is invisible in any
        // image whose values all sit inside [0, 1].
        let scene: Vec<f32> = (0..64)
            .map(|i| if i % 2 == 0 { 100.0 } else { 0.0 })
            .collect();
        let path = tmp("ordering.tif");
        let spec = Spec::new(
            Target::default(),
            ToneMap::Clip,
            // 32 px wide at 300 ppi is 0.1066... inches; ask in pixels-worth of inches
            // so the target is exactly half.
            resized(32.0 / 300.0, raw_core::Axis::Width, 300.0),
            Tail::default(),
            None,
        );
        write(&path, 64, 1, &scene, &spec).unwrap();
        assert_eq!(dims_of(&path, Container::Tiff), (32, 1));

        let bytes = std::fs::read(&path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
            panic!("expected u16");
        };
        // The interior samples, away from the clamped edges. L*(0.5) is 0.7607, so
        // the correct answer is nowhere near either 0 or full scale.
        let want = (lstar_encode(0.5) * 65535.0) as u16;
        for (i, &v) in px.iter().enumerate().take(24).skip(8) {
            let d = (v as i32 - want as i32).abs();
            assert!(
                d < 3000,
                "sample {i} is {v}, wanted about {want} — resize ran before the tone map"
            );
        }
    }

    #[test]
    fn a_size_past_the_limit_is_refused_and_writes_nothing() {
        // Refuse rather than clamp: a panel that says 90000 px must not produce a
        // 30000 px file. And the refusal must happen before anything is created, or
        // a failed export leaves a truncated file where a good one was.
        let scene = ramp(64, 32);
        let path = tmp("refused.tif");
        let _ = std::fs::remove_file(&path);
        let spec = Spec::new(
            Target::default(),
            ToneMap::Clip,
            resized(300.0, raw_core::Axis::Width, 300.0), // 90000 px
            Tail::default(),
            None,
        );
        let err = write(&path, 64, 32, &scene, &spec).unwrap_err();
        assert!(
            err.to_string().contains("90000"),
            "the refusal should name the size: {err}"
        );
        assert!(!path.exists(), "a refused export left a file behind");
    }

    // ── grain ─────────────────────────────────────────────────────────────────

    /// The 16-bit samples a file was written with.
    fn samples_of(path: &std::path::Path) -> Vec<u16> {
        let bytes = std::fs::read(path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
            panic!("expected u16");
        };
        px
    }

    fn grainy() -> raw_core::GrainParams {
        raw_core::GrainParams {
            enabled: true,
            size: 5,
            layers: 8,
            ..Default::default()
        }
    }

    #[test]
    fn grain_runs_after_the_resize_and_not_before_it() {
        // **the maintainer's decision, and the one thing about this module's position that a
        // reader will want to check.** Grain is a property of the print: a 5 px
        // crystal is 5 px in the file whatever size the file is. Graining the native
        // pixels and then downsampling is the other defensible reading and is what
        // makes the loupe a lie, because a 2x downsample averages the grain away and
        // the 1:1 view has promised a texture the file will not have.
        //
        // Asserted by construction rather than by measuring a texture: both candidate
        // orders are computed here, and the file must be one of them and not the
        // other. There is no room left for "close enough".
        let (w, h) = (64usize, 32usize);
        let scene = ramp(w, h);
        let out = resized(32.0 / 300.0, raw_core::Axis::Width, 300.0); // half size
        let spec = Spec::new(
            Target::default(),
            ToneMap::Clip,
            out,
            Tail {
                grain: grainy(),
                sharpen: raw_core::sharpen::SharpenParams::default(),
                ..Tail::default()
            },
            None,
        );

        let path = tmp("grain-order.tif");
        write(&path, w as u32, h as u32, &scene, &spec).unwrap();
        assert_eq!(dims_of(&path, Container::Tiff), (32, 16));
        let got = samples_of(&path);

        let mapped: Vec<f32> = scene.iter().map(|&v| tone_map(v, ToneMap::Clip)).collect();
        let encode = |v: &[f32]| -> Vec<u16> {
            v.iter()
                .map(|&x| (lstar_encode(x) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16)
                .collect()
        };

        // Resize, then grain — what this app does.
        let resized_first =
            raw_core::resample(&mapped, w as u32, h as u32, 32, 16, spec.output.filter);
        let want = encode(&raw_core::grain::apply(&resized_first, 32, 16, &grainy()).image);

        // Grain, then resize — the reading that was not taken.
        let grained_first = raw_core::grain::apply(&mapped, w, h, &grainy()).image;
        let other = encode(&raw_core::resample(
            &grained_first,
            w as u32,
            h as u32,
            32,
            16,
            spec.output.filter,
        ));

        assert_ne!(
            want, other,
            "the two orders agree — the test would pass vacuously"
        );
        assert_eq!(got, want, "the file was grained before the resize");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn grain_off_writes_exactly_the_file_it_always_did() {
        // Every negative judged before this module existed must export bit-identically.
        // A module that is off has to cost nothing *and change nothing*, and "nothing"
        // here means the same u16 in every sample, not a rounding away.
        let (w, h) = (48usize, 24usize);
        let scene = ramp(w, h);
        let a = tmp("grain-off-a.tif");
        let b = tmp("grain-off-b.tif");
        write(
            &a,
            w as u32,
            h as u32,
            &scene,
            &spec(Target::default(), ToneMap::Clip),
        )
        .unwrap();
        write(
            &b,
            w as u32,
            h as u32,
            &scene,
            &Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams::default(),
                // Switched off, but with every value moved — so a `write` that
                // consulted the values instead of the switch shows up here.
                Tail {
                    grain: raw_core::GrainParams {
                        enabled: false,
                        layers: 60,
                        density: 0.7,
                        ..grainy()
                    },
                    sharpen: raw_core::sharpen::SharpenParams::default(),
                    ..Tail::default()
                },
                None,
            ),
        )
        .unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    // ── output sharpening ─────────────────────────────────────────────────────

    fn sharp() -> raw_core::sharpen::SharpenParams {
        raw_core::sharpen::SharpenParams {
            enabled: true,
            amount: 1.5,
            ..Default::default()
        }
    }

    #[test]
    fn sharpening_runs_after_the_resize_and_after_the_grain() {
        // **The whole of this module's position, asserted by construction** — the same
        // shape as `grain_runs_after_the_resize_and_not_before_it`, because it is the
        // same kind of claim and deserves the same kind of proof.
        //
        // *After the resize* is what makes the radius mean output pixels, which is what
        // makes this output sharpening rather than a capture sharpen the resampler
        // would partly undo. *After grain* is the maintainer's call: output sharpening
        // compensates the medium, and dot gain does not discriminate between grain and
        // detail. Both candidate orders are computed here and the file must be one of
        // them and not the other — this test was written asserting the opposite one,
        // and flipping it is the whole record of the change.
        let (w, h) = (64usize, 32usize);
        let scene = ramp(w, h);
        let out = resized(32.0 / 300.0, raw_core::Axis::Width, 300.0); // half size
        let spec = Spec::new(
            Target::default(),
            ToneMap::Clip,
            out,
            Tail {
                grain: grainy(),
                sharpen: sharp(),
                ..Tail::default()
            },
            None,
        );

        let path = tmp("sharpen-order.tif");
        write(&path, w as u32, h as u32, &scene, &spec).unwrap();
        let got = samples_of(&path);

        let mapped: Vec<f32> = scene.iter().map(|&v| tone_map(v, ToneMap::Clip)).collect();
        let encode = |v: &[f32]| -> Vec<u16> {
            v.iter()
                .map(|&x| (lstar_encode(x) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16)
                .collect()
        };
        let small = raw_core::resample(&mapped, w as u32, h as u32, 32, 16, spec.output.filter);

        // Resize, grain, sharpen — what this app does.
        let grained = raw_core::grain::apply(&small, 32, 16, &grainy()).image;
        let want = encode(&raw_core::sharpen::apply(&grained, 32, 16, &sharp()));

        // Resize, sharpen, grain — the reading that was not taken.
        let sharpened = raw_core::sharpen::apply(&small, 32, 16, &sharp());
        let other = encode(&raw_core::grain::apply(&sharpened, 32, 16, &grainy()).image);

        assert_ne!(
            want, other,
            "the two orders agree — the test would pass vacuously"
        );
        assert_eq!(got, want, "the file was sharpened before the grain");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_proof_sharpens_at_its_own_scale_and_not_the_masters() {
        // A proof exists to predict a print, and a radius is in output pixels — so a
        // proof that sharpened at the master's grid would be sharpening at a scale the
        // file it is predicting will never have. It gets this right by sitting below
        // `spec.dims`, and this is the test that says so rather than the comment.
        let (w, h) = (64usize, 32usize);
        let scene = ramp(w, h);
        let master_out = resized(64.0 / 300.0, raw_core::Axis::Width, 300.0); // full size
        let spec = Spec::proof(
            Target::default(),
            ToneMap::Clip,
            master_out,
            Tail {
                grain: raw_core::GrainParams::default(),
                sharpen: sharp(),
                ..Tail::default()
            },
            None,
            ProofScale::Half,
        );
        let path = tmp("sharpen-proof.tif");
        write(&path, w as u32, h as u32, &scene, &spec).unwrap();
        let got = samples_of(&path);
        assert_eq!(
            dims_of(&path, Container::Tiff),
            (32, 16),
            "the proof is not half size"
        );

        let mapped: Vec<f32> = scene.iter().map(|&v| tone_map(v, ToneMap::Clip)).collect();
        let encode = |v: &[f32]| -> Vec<u16> {
            v.iter()
                .map(|&x| (lstar_encode(x) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16)
                .collect()
        };
        let small = raw_core::resample(&mapped, w as u32, h as u32, 32, 16, spec.output.filter);
        let want = encode(&raw_core::sharpen::apply(&small, 32, 16, &sharp()));

        // The other reading: sharpen the master's grid, then resample down to the
        // proof. Same picture, a radius that means something else.
        let big = raw_core::sharpen::apply(&mapped, w, h, &sharp());
        let other = encode(&raw_core::resample(
            &big,
            w as u32,
            h as u32,
            32,
            16,
            spec.output.filter,
        ));

        assert_ne!(
            want, other,
            "the two readings agree — the test would pass vacuously"
        );
        assert_eq!(got, want, "the proof sharpened at the master's scale");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sharpening_off_writes_exactly_the_file_it_always_did() {
        // Grain's guarantee, and it has to hold for the same reason: every negative
        // judged before this module existed exports bit-identically. Off costs nothing
        // *and changes nothing*, and "nothing" is the same u16 in every sample.
        let (w, h) = (48usize, 24usize);
        let scene = ramp(w, h);
        let a = tmp("sharpen-off-a.tif");
        let b = tmp("sharpen-off-b.tif");
        write(
            &a,
            w as u32,
            h as u32,
            &scene,
            &spec(Target::default(), ToneMap::Clip),
        )
        .unwrap();
        write(
            &b,
            w as u32,
            h as u32,
            &scene,
            &Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams::default(),
                // Switched off with every value moved, so a `write` that consulted the
                // values instead of the switch shows up here.
                Tail {
                    sharpen: raw_core::sharpen::SharpenParams {
                        enabled: false,
                        radius: 4.0,
                        ..sharp()
                    },
                    ..Tail::default()
                },
                None,
            ),
        )
        .unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn sharpening_on_changes_the_file_and_stays_in_range() {
        // The other half, which the test above would otherwise pass with the sharpen
        // call deleted. The range matters here for a reason particular to this module:
        // a band gain overshoots either side of an edge by design, so the values it
        // hands on are *expected* past [0, 1] and `samples16`'s clamp is what catches
        // them. A kernel that returned 1.4 would print a flat white rim rather than
        // fail.
        let (w, h) = (48usize, 24usize);
        let scene = ramp(w, h);
        let clean = tmp("sharpen-clean.tif");
        let sharpened = tmp("sharpen-on.tif");
        write(
            &clean,
            w as u32,
            h as u32,
            &scene,
            &spec(Target::default(), ToneMap::Clip),
        )
        .unwrap();
        write(
            &sharpened,
            w as u32,
            h as u32,
            &scene,
            &Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams::default(),
                Tail {
                    grain: raw_core::GrainParams::default(),
                    sharpen: sharp(),
                    ..Tail::default()
                },
                None,
            ),
        )
        .unwrap();
        let (a, b) = (samples_of(&clean), samples_of(&sharpened));
        assert_ne!(a, b, "sharpening changed nothing");
        let _ = std::fs::remove_file(&clean);
        let _ = std::fs::remove_file(&sharpened);
    }

    #[test]
    fn grain_on_changes_the_file_and_stays_in_range() {
        // The other half of the test above, which would otherwise pass with the grain
        // call deleted. And the range, because grain sits between the tone map and the
        // encode: `samples16` clamps, so a kernel that returned 1.4 would come back as
        // a flat white patch rather than as a failure.
        let (w, h) = (48usize, 24usize);
        let scene = ramp(w, h);
        let clean = tmp("grain-clean.tif");
        let grained = tmp("grain-on.tif");
        write(
            &clean,
            w as u32,
            h as u32,
            &scene,
            &spec(Target::default(), ToneMap::Clip),
        )
        .unwrap();
        write(
            &grained,
            w as u32,
            h as u32,
            &scene,
            &Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams::default(),
                Tail {
                    grain: grainy(),
                    sharpen: raw_core::sharpen::SharpenParams::default(),
                    ..Tail::default()
                },
                None,
            ),
        )
        .unwrap();
        let (a, b) = (samples_of(&clean), samples_of(&grained));
        assert_ne!(a, b, "grain changed nothing");
        // Shadows move most — the printing model — so the difference must not be a
        // uniform offset, which is what a broken composite would look like.
        let moved = a.iter().zip(&b).filter(|(x, y)| x != y).count();
        assert!(
            moved > a.len() / 4,
            "only {moved} of {} samples moved",
            a.len()
        );
        let _ = std::fs::remove_file(&clean);
        let _ = std::fs::remove_file(&grained);
    }

    fn sample_metadata() -> Metadata {
        Metadata {
            creator: Some("Example Photographer".into()),
            rights: Some("© 2026 Example Photographer".into()),
            title: Some("Test title".into()),
            headline: Some("Test headline".into()),
            description: Some("Test frame".into()),
            city: Some("New York".into()),
            subject: vec!["monochrome".into(), "test".into()],
            rating: Some(4),
            label: Some("green".into()),
            ..Default::default()
        }
    }

    #[test]
    fn metadata_travels_only_when_asked() {
        // The half that matters: turning it off must actually strip every trace, not
        // just the packet. The preference that decides is `Settings::export_metadata`,
        // which defaults on — `the_defaults_change_nothing` covers that end.
        let scene = ramp(8, 4);
        let meta = sample_metadata();
        for target in ALL {
            for want in [true, false] {
                let path = tmp(&format!(
                    "meta.{}.{want}.{:?}",
                    target.extension(),
                    target.depth
                ));
                let out = OutputParams::default();
                let m = want.then_some(&meta);
                write(
                    &path,
                    8,
                    4,
                    &scene,
                    &Spec::new(target, ToneMap::Clip, out, Tail::default(), m),
                )
                .unwrap();
                let bytes = std::fs::read(&path).unwrap();
                // A byte search, deliberately: it catches the string wherever it
                // ended up, so a container that embedded it somewhere unexpected
                // still counts, and one that leaked it after the flag was cleared is
                // still caught.
                let found = bytes
                    .windows(b"Example Photographer".len())
                    .any(|w| w == b"Example Photographer");
                assert_eq!(found, want, "{target:?} metadata={want}");
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    #[test]
    fn a_tiff_carries_both_the_xmp_packet_and_the_ascii_tags() {
        // Two records of the same thing, and both are needed: the packet is the only
        // one that can hold keywords and a rating, and the tags are what the software
        // at a print shop actually reads.
        let scene = ramp(8, 4);
        let path = tmp("meta-full.tif");
        write(
            &path,
            8,
            4,
            &scene,
            &Spec::new(
                Target::default(),
                ToneMap::Clip,
                OutputParams::default(),
                Tail::default(),
                Some(&sample_metadata()),
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        use tiff::tags::Tag;
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Artist).unwrap(),
            "Example Photographer"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::Copyright).unwrap(),
            "© 2026 Example Photographer"
        );
        assert_eq!(
            dec.get_tag_ascii_string(Tag::ImageDescription).unwrap(),
            "Test frame"
        );

        // The packet, and the two fields that exist nowhere else.
        let xmp = String::from_utf8_lossy(&bytes);
        assert!(
            xmp.contains("xmp:Rating=\"4\""),
            "the rating did not travel"
        );
        assert!(xmp.contains("monochrome"), "the keywords did not travel");
        assert!(xmp.contains("Test title"), "the title did not travel");
        assert!(
            xmp.contains("photoshop:Headline") && xmp.contains("Test headline"),
            "the IPTC headline did not travel"
        );
        assert!(
            xmp.contains("photoshop:City") && xmp.contains("New York"),
            "the IPTC location did not travel"
        );
        // ...and it must NOT carry how the picture was developed.
        assert!(
            !xmp.contains("monopro:"),
            "the export packet leaked develop params"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_png_carries_the_packet_uncompressed_under_the_registered_keyword() {
        // Compressing the XMP is legal and is what breaks readers that scan for the
        // packet header rather than inflating first.
        let scene = ramp(8, 4);
        let path = tmp("meta.png");
        let target = t(Container::Png, Depth::Eight, Compression::None);
        write(
            &path,
            8,
            4,
            &scene,
            &Spec::new(
                target,
                ToneMap::Clip,
                OutputParams::default(),
                Tail::default(),
                Some(&sample_metadata()),
            ),
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        let itxt = &reader.info().utf8_text;
        let packet = itxt
            .iter()
            .find(|c| c.keyword == "XML:com.adobe.xmp")
            .expect("no XMP chunk");
        assert!(!packet.compressed, "the XMP chunk was compressed");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("xmp:Rating=\"4\""));
        // And the tEXt mirrors, since PNG has no Artist chunk.
        let keys: Vec<&str> = reader
            .info()
            .uncompressed_latin1_text
            .iter()
            .map(|c| c.keyword.as_str())
            .collect();
        for k in ["Author", "Copyright", "Description"] {
            assert!(
                keys.contains(&k),
                "PNG is missing the {k} tEXt chunk: {keys:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_empty_metadata_block_writes_no_packet_at_all() {
        // The flag being on is not a reason to write an empty rdf:Description. A file
        // with no metadata should have none, not an XMP packet claiming nothing.
        let scene = ramp(8, 4);
        let path = tmp("meta-empty.tif");
        write(&path, 8, 4, &scene, &spec(Target::default(), ToneMap::Clip)).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.windows(8).any(|w| w == b"xmpmeta"),
            "wrote a packet for nothing"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod toned_container {
    use super::tests::*;
    use super::*;

    fn selenium() -> raw_core::ToningParams {
        raw_core::ToningParams {
            enabled: true,
            applied: vec![raw_core::toning::Applied {
                key: "selenium",
                amount: 1.0,
            }],
            ..Default::default()
        }
    }

    /// **A toned print cannot go in a greyscale container.**
    ///
    /// Left unresolved, the tail wrote three real channels and tagged them `monostar`,
    /// which declares GRAY. Photoshop reads that mismatch as no profile at all, so the
    /// file came out untagged — and untagged means read as sRGB, so the L\* encoding was
    /// decoded with the wrong curve too. One conflict, two symptoms.
    #[test]
    fn toning_promotes_a_greyscale_master_to_the_toned_one() {
        let grey = ts(Container::Tiff, Depth::Sixteen, Space::Monostar);

        let plain = Spec::new(
            grey,
            ToneMap::Clip,
            OutputParams::default(),
            Tail::default(),
            None,
        );
        assert_eq!(
            plain.target.space,
            Space::Monostar,
            "an untoned master must not move"
        );

        let toned = Spec::new(
            grey,
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: selenium(),
                ..Tail::default()
            },
            None,
        );
        assert_eq!(
            toned.target.space,
            Space::EciRgbV2,
            "the toned master is eciRGB v2"
        );
    }

    /// And the resolution is visible: `Spec::target` is what the Output panel reports,
    /// so what it says is what gets written. A substitution made inside the writer
    /// would be one nobody could see until they opened the file.
    #[test]
    fn the_promotion_is_reported_not_hidden() {
        let toned = Spec::new(
            ts(Container::Tiff, Depth::Sixteen, Space::Monostar),
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: selenium(),
                ..Tail::default()
            },
            None,
        );
        assert!(
            toned.target.label().contains("eciRGB"),
            "{}",
            toned.target.label()
        );
    }

    /// And the Output panel states the same space the writer uses.
    ///
    /// **The panel cannot be tested here, so the function it asks is.** It used to
    /// carry its own hard-coded `monostar`, which was right until toning landed and
    /// then said `monostar` over an eciRGB v2 file. Both sides now call
    /// `Target::written_space`, and this fails if `Spec::new` ever stops agreeing
    /// with it — which is the only way they could part again.
    #[test]
    fn the_panels_answer_is_the_writers_answer() {
        for space in [Space::Monostar, Space::EciRgbV2, Space::ProStar] {
            for (toned, tail) in [
                (false, Tail::default()),
                (
                    true,
                    Tail {
                        toning: selenium(),
                        ..Tail::default()
                    },
                ),
            ] {
                let target = ts(Container::Tiff, Depth::Sixteen, space);
                let written = Spec::new(target, ToneMap::Clip, OutputParams::default(), tail, None)
                    .target
                    .space;
                assert_eq!(
                    target.written_space(toned),
                    written,
                    "{space:?}, toned={toned}: the panel would state one space and the \
                     writer would use another"
                );
            }
        }
    }

    /// End to end: the file carries an RGB profile over RGB data, which is the whole
    /// point of the promotion.
    #[test]
    fn a_toned_master_is_tagged_with_a_matching_profile() {
        let scene = ramp(24, 4);
        let path = tmp("toned-tagged.tif");
        let spec = Spec::new(
            ts(Container::Tiff, Depth::Sixteen, Space::Monostar),
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: selenium(),
                ..Tail::default()
            },
            None,
        );
        write(&path, 24, 4, &scene, &spec).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(dec.colortype().unwrap(), tiff::ColorType::RGB(16));

        // The embedded profile declares RGB, not GRAY. Bytes 16..20 of an ICC header
        // are its data colour space.
        let icc: Vec<u8> = dec
            .get_tag(tiff::tags::Tag::IccProfile)
            .unwrap()
            .into_u8_vec()
            .unwrap();
        assert_eq!(
            &icc[16..20],
            b"RGB ",
            "the profile does not describe the data"
        );
    }
}

#[cfg(test)]
mod colour_round_trip {
    use super::tests::*;
    use super::*;

    fn toned() -> raw_core::ToningParams {
        raw_core::ToningParams {
            enabled: true,
            process: raw_core::toning::Process::Albumen,
            applied: vec![raw_core::toning::Applied {
                key: "gold-gp1",
                amount: 0.7,
            }],
            ..Default::default()
        }
    }

    /// What the model says this scene value should encode to, computed independently of
    /// `write` — the model, then the boundary, then the space's own transfer curve.
    fn want(scene: f32, spec: &Spec) -> [u16; 3] {
        let space = spec.target.space;
        let y = raw_core::display::tone_map(scene, spec.tone_map);
        let t = spec.toning.evaluate(y);
        let (a, b) = t.ab();
        let p = raw_core::Primaries::from_icc(space.icc()).expect("an RGB profile");
        let lin =
            raw_core::colour::oklab_to_linear(raw_core::colour::oklab_lightness(t.y), a, b, &p);
        [0, 1, 2].map(|i| (space.encode(lin[i]) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16)
    }

    fn decode16(path: &std::path::Path) -> Vec<u16> {
        let bytes = std::fs::read(path).unwrap();
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap();
        match dec.read_image().unwrap() {
            tiff::decoder::DecodingResult::U16(v) => v,
            other => panic!("expected 16-bit, got {other:?}"),
        }
    }

    /// **The whole chain, end to end, in eciRGB v2.**
    ///
    /// Every earlier test checked one link: the model, or the boundary, or the tag. This
    /// walks a scene value all the way to a byte on disk and compares it against the
    /// model computed independently — which is the only check that catches a link being
    /// right on its own and wrong in company. Both of the export bugs found so far were
    /// of exactly that kind.
    #[test]
    fn a_toned_eci_master_matches_the_model_pixel_for_pixel() {
        let scene = ramp(32, 4);
        let path = tmp("rt-eci.tif");
        let spec = Spec::new(
            ts(Container::Tiff, Depth::Sixteen, Space::EciRgbV2),
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: toned(),
                ..Tail::default()
            },
            None,
        );
        write(&path, 32, 4, &scene, &spec).unwrap();
        let px = decode16(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(px.len(), 32 * 4 * 3, "three channels per pixel");
        for x in [0usize, 7, 16, 31] {
            let got = [px[x * 3], px[x * 3 + 1], px[x * 3 + 2]];
            let expect = want(scene[x], &spec);
            for c in 0..3 {
                let apart = (got[c] as i32 - expect[c] as i32).abs();
                // **Not exact, and it should not be.** The file is written from the
                // baked table with a linear interpolation between entries; `want`
                // evaluates the model itself. They agree to within the table's own
                // resolution, which at `LUT_ENTRIES` steps of L\* is a couple of codes
                // at 16 bits — well under a code at 8, which is what anybody looks at.
                assert!(
                    apart <= 3,
                    "x={x} channel {c}: file {} against the model's {} ({got:?} vs {expect:?})",
                    got[c],
                    expect[c]
                );
            }
        }
    }

    /// And in **ProStarRGB**, which is the same path through a different matrix — so a
    /// hardcoded set of primaries hiding behind the profile read would show up here.
    #[test]
    fn a_toned_prostar_master_matches_the_model_too() {
        let scene = ramp(32, 4);
        let path = tmp("rt-prostar.tif");
        let spec = Spec::new(
            ts(Container::Tiff, Depth::Sixteen, Space::ProStar),
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: toned(),
                ..Tail::default()
            },
            None,
        );
        write(&path, 32, 4, &scene, &spec).unwrap();
        let px = decode16(&path);
        let _ = std::fs::remove_file(&path);

        for x in [3usize, 19, 30] {
            let got = [px[x * 3], px[x * 3 + 1], px[x * 3 + 2]];
            let expect = want(scene[x], &spec);
            for c in 0..3 {
                assert!(
                    (got[c] as i32 - expect[c] as i32).abs() <= 3,
                    "x={x} channel {c}: {got:?} against {expect:?}"
                );
            }
        }
    }

    /// **The two spaces really are different files**, which is the point of offering
    /// both. Identical bytes would mean the primaries were never applied.
    #[test]
    fn the_two_rgb_spaces_do_not_produce_the_same_bytes() {
        let scene = ramp(32, 4);
        let render = |space| {
            let path = tmp(&format!("rt-diff-{space:?}.tif"));
            let spec = Spec::new(
                ts(Container::Tiff, Depth::Sixteen, space),
                ToneMap::Clip,
                OutputParams::default(),
                Tail {
                    toning: toned(),
                    ..Tail::default()
                },
                None,
            );
            write(&path, 32, 4, &scene, &spec).unwrap();
            let px = decode16(&path);
            let _ = std::fs::remove_file(&path);
            px
        };
        assert_ne!(render(Space::EciRgbV2), render(Space::ProStar));
    }

    /// **An sRGB proof of a toned print**, which is the other half of what gets written
    /// — and the only path that uses a transfer curve other than L\*.
    #[test]
    fn an_srgb_proof_carries_the_toning_and_the_srgb_curve() {
        let scene = ramp(32, 4);
        let path = tmp("rt-proof.png");
        let spec = Spec::new(
            ts(Container::Png, Depth::Sixteen, Space::Srgb),
            ToneMap::Clip,
            OutputParams::default(),
            Tail {
                toning: toned(),
                ..Tail::default()
            },
            None,
        );
        write(&path, 32, 4, &scene, &spec).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut reader = png::Decoder::new(std::io::Cursor::new(&bytes))
            .read_info()
            .unwrap();
        assert_eq!(reader.info().color_type, png::ColorType::Rgb);
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let px: Vec<u16> = buf[..info.buffer_size()]
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect();

        for x in [5usize, 20, 29] {
            let got = [px[x * 3], px[x * 3 + 1], px[x * 3 + 2]];
            let expect = want(scene[x], &spec);
            for c in 0..3 {
                assert!(
                    (got[c] as i32 - expect[c] as i32).abs() <= 3,
                    "x={x} channel {c}: {got:?} against {expect:?}"
                );
            }
        }
    }

    /// **An unstained paper is bit-exact white in a colour container**, all the way to
    /// the file — `raw_core::colour`'s matrix bypass showing up where it matters.
    ///
    /// A *stained* paper is not, and must not be: an albumen sheet is cream, and that
    /// cream is half of why its highlights read as yolk. So this asks the question of a
    /// process whose paper is neutral, and asks the opposite question of one whose is
    /// not — the pair is what says the tint is a decision rather than a rounding.
    #[test]
    fn an_unstained_paper_writes_white_and_a_stained_one_does_not() {
        let scene = vec![4.0f32; 16]; // well past clipping, so the tone map gives 1.0
        let render = |process| {
            let path = tmp(&format!("rt-white-{process:?}.tif"));
            let spec = Spec::new(
                ts(Container::Tiff, Depth::Sixteen, Space::EciRgbV2),
                ToneMap::Clip,
                OutputParams::default(),
                Tail {
                    toning: raw_core::ToningParams { process, ..toned() },
                    ..Tail::default()
                },
                None,
            );
            write(&path, 4, 4, &scene, &spec).unwrap();
            let px = decode16(&path);
            let _ = std::fs::remove_file(&path);
            px
        };

        // Gelatin silver's paper is neutral, so its white goes through the bypass and
        // comes out three identical maxima.
        for (i, p) in render(raw_core::toning::Process::GelatinSilver)
            .chunks_exact(3)
            .enumerate()
        {
            assert_eq!(p[0], p[1], "pixel {i} is not neutral: {p:?}");
            assert_eq!(p[1], p[2], "pixel {i} is not neutral: {p:?}");
            assert_eq!(
                p[0], 65535,
                "an unstained paper should be white, got {}",
                p[0]
            );
        }

        // Albumen's is cream, and the file says so.
        let cream = render(raw_core::toning::Process::Albumen);
        let p = &cream[..3];
        assert!(p[0] != p[2], "a stained paper came out neutral: {p:?}");
    }
}
