//! `<stem>.mono.xmp` — the per-image sidecar.
//!
//! The format is the Python prototype's, preserved: real XMP, one
//! `rdf:Description`, monopro's own namespace for develop parameters as
//! attributes, `rdf:Seq` for the curve, and `dc:`/`xmp:` for metadata so it
//! travels to other applications.
//!
//! # The reading discipline, which is the whole point
//!
//! **Absent, older, or corrupt must never stop anything.** A missing sidecar is an
//! unedited image, not an error. A field this version does not know is skipped. A
//! field that fails to parse leaves its default and does not take the rest of the
//! file with it. A file that will not parse at all opens the image at defaults and
//! *says so* — silently discarding somebody's edits is worse than the parse failure
//! that caused it.
//!
//! That is [`Loaded`], one type rather than three conventions — `settings.toml` needs
//! the same three outcomes.
//!
//! # Why a hand-written field table and not a derive
//!
//! The sidecar is a **compatibility surface**. A derive would make the wire format a
//! consequence of the struct layout, so renaming a field or reordering an enum would
//! silently change what old files mean. The table makes every such change a visible
//! edit in one place.
//!
//! It also lets the enums be encoded the way this app actually wants them, which no
//! derive would guess: a **discriminant attribute plus its payload attributes,
//! written whether or not that variant is selected**. `Weighting="photosite"` still
//! carries `WeightingMix`, and `ToneMap="clip"` still carries the shoulder controls.
//! The file is then self-describing whichever variant is live, so a reader never has
//! to invent a payload it was not given.
//!
//! What that deliberately does *not* do is remember a payload across a mode change.
//! `Weighted(r, g, b)` **is** the mix, so leaving it discards the mix in `Params`
//! itself; the attribute then holds the selected mode's own weights. The sidecar
//! mirrors `Params` exactly and must not pretend to hold state the app does not
//! have — `Sampling::Demosaic(algo)` is the one that genuinely does remember, and
//! it round-trips because the algorithm lives in the variant.
//!
//! # Schema versions
//!
//! [`SCHEMA_VERSION`] is 17. Versions 1 and 2 are **prototype** sidecars, and this
//! app is a different pipeline: it reads the CFA mosaic where the prototype read
//! LibRaw's demosaiced tristimulus, so almost nothing in a v2 file means the same
//! thing here. Pretending otherwise would open an image with settings that look
//! carried over and render differently.
//!
//! So a v1/v2 sidecar contributes **exposure in EV and the metadata**, and nothing
//! else. A stop is a stop in any pipeline, and `dc:`/`xmp:` are not ours to
//! interpret. Everything else opens at defaults.
//!
//! **The cut is at [`PIPELINE_SCHEMA`], not [`SCHEMA_VERSION`].** They were the same
//! number until the version first moved, at which point `schema < SCHEMA_VERSION` would
//! have read every v3 sidecar on disk as a prototype file and stripped it back to its
//! exposure. Two different questions: *is this my pipeline* decides whether the field
//! table runs at all; *which version of it* is only a claim about what may be missing.
//!
//! **An added attribute does not bump the version.** `ExposureEnabled` and
//! `CurveEnabled` arrived after v3 shipped and the number stayed at 3, because a
//! version is a claim about whether a file can be *understood*, not a changelog. An
//! attribute whose absence reads as its default is understood by both directions —
//! an older file omits it and means the same thing, an older reader skips it and
//! loses only the bypass. Bumping would tell a reader to distrust a file it can
//! read perfectly well. The number moves when a value changes meaning, as it did
//! from the prototype's pipeline to this one.
//!
//! Composition is the case that moved it: a v3 reader handed a v4 file would ignore
//! `Crop` and show the whole frame, silently — precisely the "looks carried over and
//! renders differently" failure versions exist to make visible.

use std::io;
use std::path::{Path, PathBuf};

use crate::composition::{
    CompositionParams, KeystoneCrop, KeystoneMode, KeystoneParams, Orientation, Point, Ratio, Rect,
};
use crate::curve::{Curve, CurveInstance, CurveStack};
use crate::frame::{
    FrameParams, Margins as FrameMargins, Placement as FramePlacement, Priority as FramePriority,
};
use crate::grain;
use crate::output::{Axis as OutAxis, OutputParams, Resize, Unit};
use crate::params::{
    AgxParams, ContrastMaskParams, DisplayParams, ExposureParams, LuminanceParams, Params, ToneMap,
};
use crate::resample::Filter;
use crate::scene::{DecodeOptions, DemosaicAlgo, Sampling, Weighting};
use crate::sharpen;

/// What this app writes today. The module note says *when* a version moves; this is the
/// precedent for what has counted, and what an older reader does with each.
///
/// | | added | an older reader |
/// |---|---|---|
/// | 4 | composition | ignores `Crop`, shows the whole frame |
/// | 5 | Dodge & Burn | drops every stroke — a print nobody made |
/// | 6 | brush shapes | drops shaped-nib strokes; round dabs still read |
/// | 7 | grain | same print on screen, softer file on export |
/// | 8 | output sharpening | as 7 |
/// | 9 | chemical toning | **neutral print where a toned one was meant** |
/// | 10 | the Curve stack | keeps one pass, discards the rest |
/// | 11 | per-pass Curve opacity | treats every pass as 100% |
/// | 12 | per-layer D&B contrast | keeps the exposure, drops the shaping |
/// | 13 | the export FRAME | exports without the authored border |
/// | 14 | Softness as a direct % | — (migrated on read) |
/// | 15 | Softness becomes Recovery | — (no numeric change) |
/// | 16 | retires highlight reconstruction and TCA | — (attributes ignored) |
/// | 17 | manual keystone | ignores the projective map |
///
/// **9 is the one to reason from.** It is the harshest since 5, and the first whose
/// *order* is load-bearing: the Chemistry stack is an `rdf:Seq` because gold after
/// sulphide is red and gold before it is blue-black, so a reader treating it as a bag
/// produces a plausible picture that is not the one that was made.
pub const SCHEMA_VERSION: u32 = 17;

/// The first version that describes **this** pipeline rather than the prototype's.
///
/// Below it, a sidecar contributes exposure and metadata only. At or above it the
/// whole field table runs, and anything the file does not carry takes its default —
/// which is how a v3 file opens with composition at "as shot, uncropped" rather
/// than with a complaint. See the module note on why this is not `SCHEMA_VERSION`.
pub const PIPELINE_SCHEMA: u32 = 3;

const MONOPRO_NS: &str = "http://monopro.app/ns/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const PHOTOSHOP_NS: &str = "http://ns.adobe.com/photoshop/1.0/";
const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// What came back from trying to load something persisted.
///
/// Shared with the settings file when that arrives: the three outcomes are the
/// same, and so is the rule that none of them may stop the app.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded<T> {
    /// Nothing on disk. Not an error — an unedited image, or a first run.
    Absent,
    Ok(T),
    /// Present and unreadable. The caller opens at defaults **and reports this**,
    /// because the alternative is quietly throwing away work.
    Corrupt(String),
}

impl<T> Loaded<T> {
    pub fn ok(self) -> Option<T> {
        match self {
            Self::Ok(v) => Some(v),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Corrupt(e) => Some(e),
            _ => None,
        }
    }
}

/// The editable IPTC text fields, in their stable UI and storage order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum IptcField {
    Creator,
    Copyright,
    Title,
    Headline,
    Description,
    Credit,
    Source,
    City,
    State,
    Country,
    Instructions,
}

impl IptcField {
    pub const ALL: [Self; 11] = [
        Self::Creator,
        Self::Copyright,
        Self::Title,
        Self::Headline,
        Self::Description,
        Self::Credit,
        Self::Source,
        Self::City,
        Self::State,
        Self::Country,
        Self::Instructions,
    ];

    pub const fn key(self) -> &'static str {
        match self {
            Self::Creator => "creator",
            Self::Copyright => "copyright",
            Self::Title => "title",
            Self::Headline => "headline",
            Self::Description => "description",
            Self::Credit => "credit",
            Self::Source => "source",
            Self::City => "city",
            Self::State => "state",
            Self::Country => "country",
            Self::Instructions => "instructions",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Creator => "Creator",
            Self::Copyright => "Copyright",
            Self::Title => "Title",
            Self::Headline => "Headline",
            Self::Description => "Description / Caption",
            Self::Credit => "Credit",
            Self::Source => "Source",
            Self::City => "City",
            Self::State => "State / Province",
            Self::Country => "Country",
            Self::Instructions => "Instructions",
        }
    }

    pub const fn multiline(self) -> bool {
        matches!(self, Self::Description | Self::Instructions)
    }
}

/// Standard metadata, kept so it travels to other applications — and read back
/// verbatim on write so this app never destroys what another one wrote.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub creator: Option<String>,
    pub rights: Option<String>,
    pub title: Option<String>,
    pub headline: Option<String>,
    pub description: Option<String>,
    pub credit: Option<String>,
    pub source: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub instructions: Option<String>,
    pub subject: Vec<String>,
    /// `xmp:Rating`. Lightbox's, but stored here from the start so a rating set in
    /// another application survives a develop write.
    pub rating: Option<i32>,
    /// `xmp:Label` — a colour label, as a **string**, which is the format's own
    /// choice and not a convenience here.
    pub label: Option<String>,
    /// Fields deliberately cleared in monopro, rather than merely absent from this
    /// sidecar. This is the overlay's tombstone: without it, clearing an embedded
    /// caption would make the embedded caption reappear on the next read.
    pub cleared: Vec<String>,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn iptc(&self, field: IptcField) -> Option<&str> {
        match field {
            IptcField::Creator => self.creator.as_deref(),
            IptcField::Copyright => self.rights.as_deref(),
            IptcField::Title => self.title.as_deref(),
            IptcField::Headline => self.headline.as_deref(),
            IptcField::Description => self.description.as_deref(),
            IptcField::Credit => self.credit.as_deref(),
            IptcField::Source => self.source.as_deref(),
            IptcField::City => self.city.as_deref(),
            IptcField::State => self.state.as_deref(),
            IptcField::Country => self.country.as_deref(),
            IptcField::Instructions => self.instructions.as_deref(),
        }
    }

    /// Change one authored field. An empty string is a deliberate clear, recorded
    /// separately from absence so an embedded value cannot leak back through.
    pub fn set_iptc(&mut self, field: IptcField, value: String) {
        let value = value.trim().to_owned();
        let cleared = value.is_empty();
        let slot = match field {
            IptcField::Creator => &mut self.creator,
            IptcField::Copyright => &mut self.rights,
            IptcField::Title => &mut self.title,
            IptcField::Headline => &mut self.headline,
            IptcField::Description => &mut self.description,
            IptcField::Credit => &mut self.credit,
            IptcField::Source => &mut self.source,
            IptcField::City => &mut self.city,
            IptcField::State => &mut self.state,
            IptcField::Country => &mut self.country,
            IptcField::Instructions => &mut self.instructions,
        };
        *slot = (!cleared).then_some(value);
        self.cleared.retain(|key| key != field.key());
        if cleared {
            self.cleared.push(field.key().to_owned());
        }
    }

    /// Overlay monopro-authored values on metadata embedded in the source. The
    /// result contains no internal clear markers and is safe to display or export.
    pub fn merged(embedded: &Self, authored: &Self) -> Self {
        let mut out = embedded.clone();
        for field in IptcField::ALL {
            if authored.cleared.iter().any(|key| key == field.key()) {
                clear_iptc(&mut out, field);
            } else if let Some(value) = authored.iptc(field) {
                set_iptc_value(&mut out, field, Some(value.to_owned()));
            }
        }
        if !authored.subject.is_empty() {
            out.subject = authored.subject.clone();
        }
        if authored.cleared.iter().any(|key| key == "subject") {
            out.subject.clear();
        }
        if authored.cleared.iter().any(|key| key == "rating") {
            out.rating = None;
        } else if let Some(rating) = authored.rating {
            out.rating = Some(rating);
        }
        if authored.cleared.iter().any(|key| key == "label") {
            out.label = None;
        } else if let Some(label) = &authored.label {
            out.label = Some(label.clone());
        }
        out.cleared.clear();
        out
    }
}

fn set_iptc_value(metadata: &mut Metadata, field: IptcField, value: Option<String>) {
    match field {
        IptcField::Creator => metadata.creator = value,
        IptcField::Copyright => metadata.rights = value,
        IptcField::Title => metadata.title = value,
        IptcField::Headline => metadata.headline = value,
        IptcField::Description => metadata.description = value,
        IptcField::Credit => metadata.credit = value,
        IptcField::Source => metadata.source = value,
        IptcField::City => metadata.city = value,
        IptcField::State => metadata.state = value,
        IptcField::Country => metadata.country = value,
        IptcField::Instructions => metadata.instructions = value,
    }
}

fn clear_iptc(metadata: &mut Metadata, field: IptcField) {
    set_iptc_value(metadata, field, None);
}

/// One sidecar's contents.
#[derive(Debug, Clone, PartialEq)]
pub struct Sidecar {
    pub params: Params,
    pub metadata: Metadata,
    /// The version the file declared. Below [`PIPELINE_SCHEMA`] means only exposure
    /// and metadata were taken from it; see the module note.
    pub schema: u32,
}

impl Sidecar {
    /// Whether this file has been **developed**, as opposed to merely catalogued.
    ///
    /// A sidecar exists for two unrelated reasons: someone rated or labelled the frame,
    /// or someone edited it. Its *presence* answers neither on its own, and the
    /// Lightbox's amber rule was reading presence — so a star put an "I have been
    /// through this one" mark on a frame nobody had opened, which is the opposite of
    /// what the mark is for. the maintainer's, 2026-08-07.
    ///
    /// The test is the render params against their defaults, because that is exactly
    /// "would this file come out of the pipeline differently than an untouched one".
    /// Metadata is excluded by construction: `rating` and `label` live on
    /// [`Metadata`], not on `Params`.
    pub fn is_developed(&self) -> bool {
        // **Orientation does not count.** Turning a frame the right way up is
        // arranging, not editing, and it is the one edit Lightbox can make without
        // opening Develop — so a folder someone only straightened out must not come
        // back looking developed. It still lives in the sidecar, because Develop and
        // the grid have to agree about which way up the picture is.
        let mut bare = self.params.clone();
        bare.composition.orientation = None;
        bare != Params::default()
    }
}

/// `<stem>.mono.xmp`, alongside the source.
pub fn path_for(image: &Path) -> PathBuf {
    let stem = image.file_stem().unwrap_or_default();
    let mut name = stem.to_os_string();
    name.push(".mono.xmp");
    image.with_file_name(name)
}

// ------------------------------------------------------------------- the table

/// How one enum is stored: a discriminant string, and the payload it carries.
///
/// Both halves are always written. See the module note on why.
fn sampling_str(s: Sampling) -> &'static str {
    match s {
        Sampling::SuperPixel => "superpixel",
        Sampling::DirectMosaic => "directmosaic",
        Sampling::Demosaic(_) => "demosaic",
    }
}

fn weighting_str(w: Weighting) -> &'static str {
    match w {
        Weighting::Photosite => "photosite",
        Weighting::Equal => "equal",
        Weighting::Red => "red",
        Weighting::Green => "green",
        Weighting::Blue => "blue",
        Weighting::Weighted(..) => "weighted",
    }
}

fn tone_map_str(t: ToneMap) -> &'static str {
    match t {
        ToneMap::Clip => "clip",
        ToneMap::Shoulder { .. } => "shoulder",
        ToneMap::Agx(..) => "agx",
    }
}

/// The **absolute** orientation, or `as-shot`.
///
/// Absolute, never a turn to apply on top of the EXIF tag, and that is the whole
/// decision: a stored delta would compose with the tag again on every reopen, so a
/// file rotated once would be rotated twice the second time it was opened and four
/// times the fourth. `as-shot` is the absence of an override rather than a
/// particular angle, which is why it is a discriminant here and an `Option` in
/// `CompositionParams`.
fn orientation_str(o: Option<Orientation>) -> &'static str {
    match o {
        None => "as-shot",
        Some(Orientation::Rotate0) => "0",
        Some(Orientation::Rotate90) => "90",
        Some(Orientation::Rotate180) => "180",
        Some(Orientation::Rotate270) => "270",
    }
}

/// The **value**, not an index into the preset table and not its name.
///
/// The prototype stores the index, which is what makes its list unreorderable: an
/// entry added in the middle silently changes what every older sidecar means. A
/// number is self-describing — a reader that has never heard of Ōban still opens the
/// file with the right crop, and the label is recovered by matching the value back
/// against the table. Six of the presets are irrational, so a `w:h` pair could not
/// carry them anyway.
fn ratio_str(r: Ratio) -> String {
    match r {
        Ratio::Free => "free".into(),
        Ratio::Original => "original".into(),
        Ratio::Fixed(v) => num(v),
    }
}

fn parse_ratio(s: &str) -> Option<Ratio> {
    match s {
        "free" => Some(Ratio::Free),
        "original" => Some(Ratio::Original),
        _ => {
            let v: f32 = s.trim().parse().ok()?;
            // Zero, negative and NaN are not ratios. Refused here rather than
            // divided by later, so a hand-edited file cannot produce a NaN crop.
            (v.is_finite() && v > 0.0).then_some(Ratio::Fixed(v))
        }
    }
}

fn keystone_mode_str(mode: KeystoneMode) -> &'static str {
    match mode {
        KeystoneMode::Off => "off",
        KeystoneMode::Vertical => "vertical",
        KeystoneMode::Horizontal => "horizontal",
        KeystoneMode::Rectangle => "rectangle",
    }
}

fn parse_keystone_mode(s: &str) -> Option<KeystoneMode> {
    match s {
        "off" => Some(KeystoneMode::Off),
        "vertical" => Some(KeystoneMode::Vertical),
        "horizontal" => Some(KeystoneMode::Horizontal),
        "rectangle" => Some(KeystoneMode::Rectangle),
        _ => None,
    }
}

fn keystone_crop_str(crop: KeystoneCrop) -> &'static str {
    match crop {
        KeystoneCrop::Largest => "largest",
        KeystoneCrop::Original => "original",
    }
}

fn parse_keystone_crop(s: &str) -> Option<KeystoneCrop> {
    match s {
        "largest" => Some(KeystoneCrop::Largest),
        "original" => Some(KeystoneCrop::Original),
        _ => None,
    }
}

// ------------------------------------------------------------------- writing

/// Format a float the way the prototype's `%.8g` does for these magnitudes, which
/// is also the shortest form that reads back bit-identical.
fn num(v: f32) -> String {
    format!("{v}")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ------------------------------------------------------------------ dodge & burn

/// A dab's numbers, at five decimal places with the trailing zeros cut.
///
/// Every other number in this file goes through `num`, which prints an `f32` at
/// full precision. That is right for a value a person might type and wrong for a
/// list of thousands: `0.51234567` costs nine characters to place a dab to a
/// ten-millionth of a frame width, which on a 6000px negative is a thousandth of a
/// pixel. Five places is 0.06px there — under any plausible display of it — and
/// costs seven characters at worst.
///
/// The saving is real and was measured rather than assumed; see
/// `a_thousand_dabs_is_a_sensible_sized_sidecar`.
fn dab_num(v: f32) -> String {
    let s = format!("{v:.5}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" {
        "0".to_owned()
    } else {
        s.to_owned()
    }
}

/// The module's element.
///
/// **Packed, following `CurvePoints` rather than inventing a second shape** for a
/// variable-length payload — but packed one level further, because the payload has
/// a level `CurvePoints` does not. A pass is one `rdf:li` holding its dabs as
/// space-separated six-tuples, so the XML overhead is per *pass* and not per dab.
/// One element per dab would be honest XMP and roughly four times the file.
///
/// The nesting is the model exactly: instances hold passes, passes hold dabs. An
/// instance's own settings ride as attributes on its `rdf:li`, which is the XMP
/// shorthand for a struct and keeps the scalar fields in the same visual shape as
/// the ones on `rdf:Description` above.
/// Chemical toning: the process as attributes, the Chemistry stack as an ordered
/// sequence.
///
/// **`rdf:Seq` and not `rdf:Bag`**, and that is the whole of what this format has to
/// get right about toning. The stack is ordered and the order changes the picture —
/// gold after sulphide is red, gold before it is blue-black — so a container that
/// documented itself as unordered would be inviting a reader to sort it.
///
/// Written only when there is something to write, like Dodge & Burn's block, so an
/// untoned file is the file it always was.
fn write_toning(o: &mut String, t: &crate::toning::ToningParams) {
    use crate::toning::Process;

    if t.is_default() {
        return;
    }
    let process = match t.process {
        Process::GelatinSilver => "gelatin-silver",
        Process::SaltPrint => "salt",
        Process::Albumen => "albumen",
        Process::CollodionPop => "collodion",
        Process::Kallitype => "kallitype",
        Process::Vandyke => "vandyke",
        Process::PlatinumPalladium => "platinum-palladium",
        Process::Ziatype => "ziatype",
        Process::Cyanotype => "cyanotype",
        Process::Carbon => "carbon",
        Process::Tintype => "tintype",
        Process::Daguerreotype => "daguerreotype",
    };
    o.push_str(&format!(
        "   <monopro:Toning rdf:parseType=\"Resource\"\n     monopro:Enabled=\"{}\" \
         monopro:ChemistryEnabled=\"{}\" monopro:PlacementEnabled=\"{}\" \
         monopro:Process=\"{process}\"\n     monopro:Tone=\"{}\" monopro:Hue=\"{}\" \
         monopro:Mix=\"{}\" monopro:Pigment=\"{}\" monopro:Contrast=\"{}\"\n     >\n",
        t.enabled,
        t.chemistry_enabled,
        t.placement_enabled,
        num(t.tone),
        num(t.hue),
        num(t.mix),
        num(t.pigment),
        num(t.contrast),
    ));

    // **Keyed, not positional.** A treatment is matched to its process's table by name,
    // so adding one to a process — or reordering the list to read better — cannot make
    // an old sidecar apply the wrong chemistry.
    if !t.applied.is_empty() {
        o.push_str("    <monopro:Treatments>\n     <rdf:Seq>\n");
        for a in &t.applied {
            o.push_str(&format!(
                "      <rdf:li rdf:parseType=\"Resource\" monopro:Key=\"{}\" \
                 monopro:Amount=\"{}\"/>\n",
                a.key,
                num(a.amount)
            ));
        }
        o.push_str("     </rdf:Seq>\n    </monopro:Treatments>\n");
    }

    // The placement curve, in the shape `CurvePoints` already uses.
    o.push_str("    <monopro:Placement>\n     <rdf:Seq>\n");
    for pt in t.placement.points() {
        o.push_str(&format!(
            "      <rdf:li>{},{}</rdf:li>\n",
            num(pt[0]),
            num(pt[1])
        ));
    }
    o.push_str("     </rdf:Seq>\n    </monopro:Placement>\n");
    o.push_str("   </monopro:Toning>\n");
}

/// Read the toning block back. Absent reads as the default, which is **off** — so
/// every file written before this module existed opens untoned, which is what it is.
fn read_toning(desc: &roxmltree::Node) -> crate::toning::ToningParams {
    use crate::toning::{Process, ToningParams, flat_placement};

    let mut out = ToningParams::default();
    let Some(node) = desc
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "Toning")))
    else {
        return out;
    };
    let get = |n: &str| node.attribute((MONOPRO_NS, n));
    let f = |n: &str, d: f32| get(n).and_then(finite).unwrap_or(d);

    out.enabled = get("Enabled").map(|v| v == "true").unwrap_or(false);
    // These arrived after the Toning block. Absence means engaged, matching the old
    // always-on submodules, so existing sidecars keep rendering exactly as before.
    out.chemistry_enabled = get("ChemistryEnabled").map(|v| v == "true").unwrap_or(true);
    out.placement_enabled = get("PlacementEnabled").map(|v| v == "true").unwrap_or(true);
    // An unrecognised process reads as gelatin silver rather than refusing the file. A
    // process is a paper: the picture is still the picture, and showing it on the
    // default paper beats showing none.
    out.process = match get("Process") {
        Some("salt") => Process::SaltPrint,
        Some("albumen") => Process::Albumen,
        Some("collodion") => Process::CollodionPop,
        Some("kallitype") => Process::Kallitype,
        Some("vandyke") => Process::Vandyke,
        Some("platinum-palladium") => Process::PlatinumPalladium,
        Some("ziatype") => Process::Ziatype,
        Some("cyanotype") => Process::Cyanotype,
        Some("carbon") => Process::Carbon,
        Some("tintype") => Process::Tintype,
        Some("daguerreotype") => Process::Daguerreotype,
        _ => Process::GelatinSilver,
    };
    out.tone = f("Tone", out.tone).clamp(0.0, 2.0);
    out.hue = f("Hue", out.hue).clamp(-60.0, 60.0);
    out.mix = f("Mix", out.mix).clamp(0.0, 1.0);
    out.pigment = f("Pigment", out.pigment).clamp(0.0, 1.0);
    out.contrast = f("Contrast", out.contrast).clamp(0.0, 3.0);

    if let Some(seq) = node
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "Treatments")))
        .and_then(|t| t.children().find(|n| n.has_tag_name((RDF_NS, "Seq"))))
    {
        for li in seq.children().filter(|n| n.has_tag_name((RDF_NS, "li"))) {
            let key = li.attribute((MONOPRO_NS, "Key")).unwrap_or_default();
            // **A key this process does not offer is dropped, not guessed.** Every other
            // field here is a number in a range and has a sensible fallback; a treatment
            // is the whole of what was done, so inventing one puts chemistry in the
            // picture nobody asked for. Dropping is wrong in the direction of doing less.
            let Some(t) = out.process.treatments().iter().find(|t| t.key == key) else {
                continue;
            };
            let amount = li
                .attribute((MONOPRO_NS, "Amount"))
                .and_then(finite)
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            out.applied
                .push(crate::toning::Applied { key: t.key, amount });
        }
    }

    if let Some(seq) = node
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "Placement")))
        .and_then(|t| t.children().find(|n| n.has_tag_name((RDF_NS, "Seq"))))
    {
        let pts: Vec<[f32; 2]> = seq
            .children()
            .filter(|n| n.has_tag_name((RDF_NS, "li")))
            .filter_map(|li| {
                let text = li.text()?;
                let (x, y) = text.split_once(',')?;
                Some([finite(x)?, finite(y)?])
            })
            .collect();
        out.placement = crate::curve::Curve::from_points(&pts).unwrap_or_else(flat_placement);
    }

    out
}

fn write_dodgeburn(o: &mut String, db: &crate::dodgeburn::DodgeBurnParams) {
    if db.instances.is_empty() {
        return;
    }
    o.push_str("   <monopro:DodgeBurn>\n    <rdf:Seq>\n");
    for inst in &db.instances {
        let m = &inst.mask;
        o.push_str("     <rdf:li rdf:parseType=\"Resource\"\n");
        o.push_str(&format!("       monopro:Name=\"{}\"\n", escape(&inst.name)));
        o.push_str(&format!(
            "       monopro:Sign=\"{}\" monopro:Opacity=\"{}\" monopro:Contrast=\"{}\" monopro:Enabled=\"{}\"\n",
            match inst.sign {
                crate::dodgeburn::Sign::Dodge => "dodge",
                crate::dodgeburn::Sign::Burn => "burn",
            },
            num(inst.opacity),
            num(inst.contrast),
            inst.enabled
        ));
        o.push_str(&format!(
            "       monopro:MaskEnabled=\"{}\" monopro:MaskLo=\"{}\" monopro:MaskHi=\"{}\"\n",
            m.enabled,
            num(m.lo),
            num(m.hi)
        ));
        o.push_str(&format!(
            "       monopro:MaskFeatherLo=\"{}\" monopro:MaskFeatherHi=\"{}\" monopro:MaskInvert=\"{}\"\n",
            num(m.f_lo),
            num(m.f_hi),
            m.invert
        ));
        o.push_str(&format!(
            "       monopro:MaskBlur=\"{}\" monopro:MaskEdgeAware=\"{}\" monopro:MaskRegion=\"{}\" monopro:MaskEdge=\"{}\"\n",
            num(m.blur),
            m.edge_aware,
            num(m.region),
            num(m.edge)
        ));
        // The shape, and its geometry. Written for every instance including a
        // brush, so the discriminant is always present and a reader never has to
        // infer the kind from which optional group happens to be there — the same
        // rule `the_payload_attributes_are_always_present` states for the enums on
        // `rdf:Description`.
        o.push_str(&format!("       monopro:Shape=\"{}\"", inst.shape.label()));
        match &inst.shape {
            // **The nib is written on the layer**, beside the discriminant rather than
            // only inside every dab. It is what makes a layer with no strokes yet still
            // a Card layer — which is a real state, since a layer exists from the moment
            // you make it. A v8 reader that does not know the attribute ignores it and
            // falls back to the dabs, which is what it did before.
            crate::dodgeburn::Shape::Brush { nib, .. } => {
                o.push_str(&format!(" monopro:Nib=\"{}\"", nib.key()));
            }
            crate::dodgeburn::Shape::Linear(l) => o.push_str(&format!(
                "\n       monopro:GradFrom=\"{},{}\" monopro:GradTo=\"{},{}\" \
                 monopro:GradFeather=\"{}\" monopro:GradEV=\"{}\"",
                dab_num(l.x0),
                dab_num(l.y0),
                dab_num(l.x1),
                dab_num(l.y1),
                num(l.feather),
                num(l.ev)
            )),
            crate::dodgeburn::Shape::Radial(r) => o.push_str(&format!(
                "\n       monopro:GradCentre=\"{},{}\" monopro:GradInner=\"{}\" \
                 monopro:GradOuter=\"{}\"\n       monopro:GradAspect=\"{}\" \
                 monopro:GradAngle=\"{}\" monopro:GradInvert=\"{}\" \
                 monopro:GradFeather=\"{}\" monopro:GradEV=\"{}\"",
                dab_num(r.cx),
                dab_num(r.cy),
                num(r.inner),
                num(r.outer),
                num(r.aspect),
                num(r.angle),
                r.invert,
                num(r.feather),
                num(r.ev)
            )),
        }
        o.push_str(">\n");
        // Only a brush has passes, and an empty `<Passes>` on a gradient would be
        // a claim that it is a brush with none.
        if matches!(inst.shape, crate::dodgeburn::Shape::Brush { .. }) {
            o.push_str("      <monopro:Passes>\n       <rdf:Seq>\n");
            for g in inst.gestures() {
                o.push_str("        <rdf:li>");
                for (i, d) in g.dabs.iter().enumerate() {
                    if i > 0 {
                        o.push(' ');
                    }
                    o.push_str(&format!(
                        "{},{},{},{},{},{}",
                        dab_num(d.x),
                        dab_num(d.y),
                        dab_num(d.radius),
                        dab_num(d.feather),
                        dab_num(d.opacity),
                        dab_num(d.ev)
                    ));
                    // **Six numbers for a round dab, nine for a shaped one.** The common
                    // case does not grow the file, and a reader tells the two apart by
                    // counting rather than by a flag — which is what makes a v5 file's
                    // six-tuples still mean exactly what they meant.
                    if d.nib != crate::dodgeburn::Nib::Round
                        || (d.aspect - 1.0).abs() > 1e-5
                        || d.angle != 0.0
                    {
                        o.push_str(&format!(
                            ",{},{},{}",
                            dab_num(d.aspect),
                            dab_num(d.angle),
                            u8::from(d.nib == crate::dodgeburn::Nib::Card)
                        ));
                    }
                }
                o.push_str("</rdf:li>\n");
            }
            o.push_str("       </rdf:Seq>\n      </monopro:Passes>\n");
        }
        o.push_str("     </rdf:li>\n");
    }
    o.push_str("    </rdf:Seq>\n   </monopro:DodgeBurn>\n");
}

fn read_dodgeburn(desc: &roxmltree::Node, enabled: bool) -> crate::dodgeburn::DodgeBurnParams {
    use crate::dodgeburn::{
        Dab, DodgeBurnParams, Gesture, Instance, Linear, Radial, Shape, Sign, ZoneMask,
    };

    let mut out = DodgeBurnParams {
        enabled,
        instances: Vec::new(),
    };
    let Some(db) = desc
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "DodgeBurn")))
    else {
        return out;
    };
    let Some(seq) = db.children().find(|n| n.has_tag_name((RDF_NS, "Seq"))) else {
        return out;
    };

    for li in seq.children().filter(|n| n.has_tag_name((RDF_NS, "li"))) {
        let get = |n: &str| li.attribute((MONOPRO_NS, n));
        let f = |n: &str, d: f32| get(n).and_then(finite).unwrap_or(d);
        let b = |n: &str, d: bool| get(n).map(|v| v == "true").unwrap_or(d);

        let sign = if get("Sign") == Some("dodge") {
            Sign::Dodge
        } else {
            Sign::Burn
        };
        let default = ZoneMask::default();
        let mut inst = Instance {
            name: get("Name").unwrap_or("Burn").to_owned(),
            opacity: f("Opacity", 1.0),
            contrast: f("Contrast", 0.0).clamp(
                *DodgeBurnParams::CONTRAST_RANGE.start(),
                *DodgeBurnParams::CONTRAST_RANGE.end(),
            ),
            enabled: b("Enabled", true),
            mask: ZoneMask {
                enabled: b("MaskEnabled", false),
                lo: f("MaskLo", default.lo),
                hi: f("MaskHi", default.hi),
                f_lo: f("MaskFeatherLo", default.f_lo),
                f_hi: f("MaskFeatherHi", default.f_hi),
                invert: b("MaskInvert", false),
                blur: f("MaskBlur", default.blur),
                edge_aware: b("MaskEdgeAware", default.edge_aware),
                region: f("MaskRegion", default.region),
                edge: f("MaskEdge", default.edge),
            },
            ..Instance::new(sign, String::new())
        };

        // The shape. An unrecognised or absent discriminant reads as a brush, which
        // is what a v5 file means — it had no shapes but the one.
        let pair = |n: &str| -> Option<(f32, f32)> {
            let v = get(n)?;
            let (a, b) = v.split_once(',')?;
            let (a, b) = (a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?);
            (a.is_finite() && b.is_finite()).then_some((a, b))
        };
        match get("Shape") {
            // A gradient with no geometry is not a gradient. Rather than invent one
            // in the middle of the frame, the instance stays a brush with nothing on
            // it — visible in the panel, changing no pixel, and obviously wrong to
            // the person looking at it.
            Some("linear") => {
                if let (Some((x0, y0)), Some((x1, y1))) = (pair("GradFrom"), pair("GradTo")) {
                    inst.shape = Shape::Linear(Linear {
                        x0,
                        y0,
                        x1,
                        y1,
                        feather: f("GradFeather", 1.0),
                        ev: f("GradEV", 0.0),
                    });
                }
            }
            Some("radial") => {
                if let Some((cx, cy)) = pair("GradCentre") {
                    inst.shape = Shape::Radial(Radial {
                        cx,
                        cy,
                        inner: f("GradInner", 0.0),
                        outer: f("GradOuter", 0.25),
                        aspect: f("GradAspect", 1.0),
                        angle: f("GradAngle", 0.0),
                        feather: f("GradFeather", 1.0),
                        invert: b("GradInvert", false),
                        ev: f("GradEV", 0.0),
                    });
                }
            }
            _ => {}
        }

        let passes = li
            .children()
            .find(|n| n.has_tag_name((MONOPRO_NS, "Passes")))
            .and_then(|p| p.children().find(|n| n.has_tag_name((RDF_NS, "Seq"))));
        if let Some(passes) = passes {
            for pass in passes.children().filter(|n| n.has_tag_name((RDF_NS, "li"))) {
                let dabs: Vec<Dab> = pass
                    .text()
                    .unwrap_or_default()
                    .split_ascii_whitespace()
                    .filter_map(|tuple| {
                        let n: Vec<f32> = tuple
                            .split(',')
                            .map(str::parse::<f32>)
                            .map_while(|v| v.ok().filter(|v| v.is_finite()))
                            .collect();
                        // **Six is a round dab and nine is a shaped one.** Anything
                        // else is not a dab this version understands, and guessing
                        // at it would place a mark nobody asked for — the same rule
                        // `parse4` applies to the crop rectangle. It is also what
                        // makes the format extensible: a v5 reader handed a nine
                        // drops it visibly rather than misreading six of it.
                        if n.len() != 6 && n.len() != 9 {
                            return None;
                        }
                        Some(Dab {
                            x: n[0],
                            y: n[1],
                            radius: n[2],
                            feather: n[3],
                            opacity: n[4],
                            ev: n[5],
                            aspect: if n.len() == 9 { n[6] } else { 1.0 },
                            angle: if n.len() == 9 { n[7] } else { 0.0 },
                            nib: if n.len() == 9 && n[8] != 0.0 {
                                crate::dodgeburn::Nib::Card
                            } else {
                                crate::dodgeburn::Nib::Round
                            },
                        })
                    })
                    .collect();
                // An empty pass is dropped rather than kept: it renders nothing,
                // and carrying it would make `⌘Z` undo a stroke that was never
                // visible.
                if !dabs.is_empty()
                    && let Some(g) = inst.gestures_mut()
                {
                    g.push(Gesture::new(dabs));
                }
            }
        }

        // **The layer's nib, and where it comes from for a file that predates it.**
        //
        // Written as `monopro:Nib` since the nib moved onto the layer. A file written
        // before that has the attribute missing and the answer in its dabs — so the
        // fallback reads the first one, which is exactly what the panel used to derive
        // and therefore what those files have always meant.
        //
        // A *mixed* layer can only come from such a file, because the app can no longer
        // make one. It reads as its first dab's nib and the strokes keep their own
        // shapes, because a `Dab` still carries its nib and the renderer still reads it.
        // That is the honest outcome: the layer is relabelled, and not one pixel of what
        // was painted moves.
        let painted = inst
            .gestures()
            .iter()
            .flat_map(|g| g.dabs.iter())
            .next()
            .map(|d| d.nib);
        if let Shape::Brush { nib, .. } = &mut inst.shape {
            *nib = get("Nib")
                .and_then(crate::dodgeburn::Nib::from_key)
                .or(painted)
                .unwrap_or_default();
        }
        out.instances.push(inst);
        if out.instances.len() == DodgeBurnParams::MAX_INSTANCES {
            break;
        }
    }
    out
}

/// Build the XML. Separated from the write so a test can read it without a
/// filesystem.
pub fn to_xml(params: &Params, metadata: &Metadata, source_name: &str) -> String {
    let mut o = String::new();
    let p = params;

    o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    o.push_str("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n");
    o.push_str(&format!(" <rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n"));
    o.push_str("  <rdf:Description rdf:about=\"\"\n");
    o.push_str(&format!("    xmlns:monopro=\"{MONOPRO_NS}\"\n"));
    o.push_str(&format!("    xmlns:dc=\"{DC_NS}\"\n"));
    o.push_str(&format!("    xmlns:xmp=\"{XMP_NS}\"\n"));
    o.push_str(&format!("    xmlns:photoshop=\"{PHOTOSHOP_NS}\"\n"));

    let mut attr = |name: &str, value: String| {
        o.push_str(&format!("   monopro:{name}=\"{value}\"\n"));
    };

    attr("SchemaVersion", SCHEMA_VERSION.to_string());
    attr("SourceFile", escape(source_name));

    // Decode.
    attr("UnityWB", p.decode.unity_wb.to_string());

    // Luminance. Discriminant plus payload, both always present.
    attr("Sampling", sampling_str(p.luminance.sampling).to_owned());
    attr(
        "DemosaicAlgo",
        (match p.luminance.sampling {
            Sampling::Demosaic(a) => a,
            _ => DemosaicAlgo::default(),
        })
        .key()
        .to_owned(),
    );
    attr("Weighting", weighting_str(p.luminance.weighting).to_owned());
    {
        let [r, g, b] = p.luminance.weighting.as_weighted().weights();
        attr("WeightingMix", format!("{},{},{}", num(r), num(g), num(b)));
    }

    // Exposure. The bypass is stored with the values it bypasses, so switching a
    // module off is an edit that survives a reload like any other — and the values
    // survive with it, which is what makes it an A/B rather than a reset.
    attr("ExposureEnabled", p.exposure.enabled.to_string());
    attr("ExposureEV", num(p.exposure.ev));
    attr("BlackCorrection", num(p.exposure.black));

    // The curve's bypass. The points themselves are an element, written below.
    attr("CurveEnabled", p.curve.enabled.to_string());

    // Contrast Mask. The spacer is stored as the PERCENTAGE it physically is,
    // never as pixels — a Settings toggle may later show it in pixels, and the
    // display unit must not change what the file means.
    attr("ContrastMaskEnabled", p.contrast_mask.enabled.to_string());
    attr("MaskContrast", num(p.contrast_mask.contrast));
    attr("MaskSpacerPercent", num(p.contrast_mask.spacer));
    attr("MaskRegistrationX", num(p.contrast_mask.offset.0));
    attr("MaskRegistrationY", num(p.contrast_mask.offset.1));

    // Dodge & Burn's bypass. The instances themselves are an element, written
    // below — the same split as the curve's.
    attr("DodgeBurnEnabled", p.dodgeburn.enabled.to_string());

    // Composition. The crop is stored as FRACTIONS of the straightened frame,
    // never as pixels, for the reason the spacer is a percentage: SuperPixel is
    // half resolution, and a crop in pixels would cover a different part of the
    // picture after a change of sampling mode.
    attr("CompositionEnabled", p.composition.enabled.to_string());
    attr(
        "Orientation",
        orientation_str(p.composition.orientation).to_owned(),
    );
    attr("Straighten", num(p.composition.straighten));
    attr(
        "KeystoneMode",
        keystone_mode_str(p.composition.keystone.mode).to_owned(),
    );
    attr(
        "KeystoneGuides",
        p.composition
            .keystone
            .guides
            .iter()
            .flat_map(|point| [num(point.x), num(point.y)])
            .collect::<Vec<_>>()
            .join(","),
    );
    attr("KeystoneCorrection", num(p.composition.keystone.correction));
    attr("KeystoneAspect", num(p.composition.keystone.aspect));
    attr(
        "KeystoneCrop",
        keystone_crop_str(p.composition.keystone.crop).to_owned(),
    );
    {
        let c = p.composition.crop;
        attr(
            "Crop",
            format!("{},{},{},{}", num(c.x), num(c.y), num(c.w), num(c.h)),
        );
    }
    attr("CropRatio", ratio_str(p.composition.ratio));
    attr("CropPortrait", p.composition.portrait.to_string());

    // Display.
    attr("DisplayEnabled", p.display.enabled.to_string());
    attr("ToneMap", tone_map_str(p.display.tone_map).to_owned());
    {
        let (t, s) = match p.display.tone_map {
            ToneMap::Shoulder {
                threshold,
                strength,
            } => (threshold, strength),
            _ => match ToneMap::SHOULDER_DEFAULT {
                ToneMap::Shoulder {
                    threshold,
                    strength,
                } => (threshold, strength),
                _ => unreachable!("SHOULDER_DEFAULT is a Shoulder"),
            },
        };
        attr("ShoulderThreshold", num(t));
        attr("ShoulderStrength", num(s));
    }
    {
        let agx = match p.display.tone_map {
            ToneMap::Agx(agx) => agx.normalized(),
            _ => AgxParams::DEFAULT,
        };
        attr("AgxAutoRange", agx.auto_range.to_string());
        attr("AgxBlackEv", num(agx.black_ev));
        attr("AgxWhiteEv", num(agx.white_ev));
        attr("AgxContrast", num(agx.contrast));
        attr("AgxToePower", num(agx.toe_power));
        attr("AgxShoulderPower", num(agx.shoulder_power));
    }
    attr("Gamma", num(p.display.gamma));
    // No `Dither`: screen dither is a viewer preference and 8-bit files take the
    // proof's, so there is nothing per image to store. Files that carry the attribute
    // still open; it is ignored.

    // Grain. The size is written as the ODD value that runs, never as whatever was
    // typed — see `GrainParams::set_size`; a file that said 6 and printed 7 would put
    // the disagreement somewhere nobody can see it.
    //
    // **The seed is the point of writing any of this.** It is per-image state with no
    // sensible default beyond "the one this negative had", and two exports of one
    // negative are only the same picture because this line exists.
    attr("GrainEnabled", p.grain.enabled.to_string());
    attr("GrainSize", p.grain.effective_size().to_string());
    attr("GrainDensity", num(p.grain.density));
    attr("GrainLayers", p.grain.layers.to_string());
    attr("GrainVariability", num(p.grain.variability));
    attr("GrainSensitivity", num(p.grain.sensitivity));
    attr("GrainSeed", p.grain.seed.to_string());

    // Output sharpening. The radius is in OUTPUT pixels and is stored as such, which is
    // the one thing a reader has to know about it: the same number means a different
    // physical size on a print of a different size, and that is the module working
    // rather than a unit nobody converted.
    attr("SharpenEnabled", p.sharpen.enabled.to_string());
    attr("SharpenAmount", num(p.sharpen.amount));
    attr("SharpenRadius", num(p.sharpen.radius));
    attr("SharpenEdges", num(p.sharpen.edges));

    // Output. The print size is stored in INCHES and as one anchored edge, never as a
    // width and a height, for the same reason the crop is stored as fractions: the
    // unanchored edge is a function of the crop's aspect, and writing both would let a
    // hand-edited file claim a print that is not the shape of the picture. The unit
    // the UI happens to be showing is a display preference and is not written here.
    //
    // `PrintSize` absent means "no resize" — which is also what a v3 or v4 file says
    // by omitting it, so an older sidecar opens writing its own pixels.
    attr("PrintPPI", num(p.output.ppi));
    if let Some(r) = p.output.resize {
        attr("PrintSize", num(r.inches));
        attr(
            "PrintAxis",
            match r.axis {
                OutAxis::Width => "w".to_owned(),
                OutAxis::Height => "h".to_owned(),
            },
        );
    }
    attr("ResampleFilter", p.output.filter.key().to_owned());

    // FRAME. Every physical quantity is written canonically in inches; `FrameUnit`
    // records only how this image's module presents those values. Switching units can
    // therefore never resize a print by round-trip rounding.
    attr("FrameEnabled", p.frame.enabled.to_string());
    attr("FrameUnit", p.frame.unit.key().to_owned());
    attr("FramePriority", p.frame.priority.key().to_owned());
    attr("FrameEqual", p.frame.equal.to_string());
    attr(
        "FrameMargins",
        format!(
            "{},{},{},{}",
            num(p.frame.margins.left),
            num(p.frame.margins.top),
            num(p.frame.margins.right),
            num(p.frame.margins.bottom)
        ),
    );
    attr(
        "FrameOuter",
        format!(
            "{},{}",
            num(p.frame.outer_inches[0]),
            num(p.frame.outer_inches[1])
        ),
    );
    attr("FrameCustomSize", p.frame.custom_size.to_string());
    attr("FramePlacement", p.frame.placement.key().to_owned());
    attr("FrameBottomWeight", num(p.frame.bottom_weight_inches));
    attr(
        "FramePosition",
        format!(
            "{},{}",
            num(p.frame.custom_position[0]),
            num(p.frame.custom_position[1])
        ),
    );
    attr(
        "FrameColor",
        format!(
            "{},{},{}",
            p.frame.color[0], p.frame.color[1], p.frame.color[2]
        ),
    );
    attr("FrameTrimLine", p.frame.trim_line.to_string());

    if let Some(r) = metadata.rating {
        o.push_str(&format!("   xmp:Rating=\"{r}\"\n"));
    }
    if let Some(l) = &metadata.label {
        o.push_str(&format!("   xmp:Label=\"{}\"\n", escape(l)));
    }
    if !metadata.cleared.is_empty() {
        o.push_str(&format!(
            "   monopro:MetadataCleared=\"{}\"\n",
            escape(&metadata.cleared.join(","))
        ));
    }

    o.push_str("   >\n");

    write_curves(&mut o, &p.curve);

    write_dodgeburn(&mut o, &p.dodgeburn);
    write_toning(&mut o, &p.toning);

    // Metadata, in the shapes XMP defines for them.
    if let Some(v) = &metadata.creator {
        o.push_str(&format!(
            "   <dc:creator><rdf:Seq><rdf:li>{}</rdf:li></rdf:Seq></dc:creator>\n",
            escape(v)
        ));
    }
    for (tag, val) in [
        ("title", &metadata.title),
        ("rights", &metadata.rights),
        ("description", &metadata.description),
    ] {
        if let Some(v) = val {
            o.push_str(&format!(
                "   <dc:{tag}><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:{tag}>\n",
                escape(v)
            ));
        }
    }
    if !metadata.subject.is_empty() {
        o.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for s in &metadata.subject {
            o.push_str(&format!("     <rdf:li>{}</rdf:li>\n", escape(s)));
        }
        o.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }
    for (tag, val) in [
        ("Headline", &metadata.headline),
        ("Credit", &metadata.credit),
        ("Source", &metadata.source),
        ("City", &metadata.city),
        ("State", &metadata.state),
        ("Country", &metadata.country),
        ("Instructions", &metadata.instructions),
    ] {
        if let Some(v) = val {
            o.push_str(&format!(
                "   <photoshop:{tag}>{}</photoshop:{tag}>\n",
                escape(v)
            ));
        }
    }

    o.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n");
    o
}

/// A standalone XMP packet carrying **only** the authored metadata, for embedding in
/// an exported file. `None` when there is nothing to say.
///
/// Deliberately not `to_xml` with the params left out. A sidecar describes how to
/// develop a negative and is addressed to this application; an exported TIFF is a
/// finished print and is addressed to everyone else, and shipping `monopro:MaskSpacer`
/// inside it would be both meaningless to the reader and a small unasked-for
/// disclosure of how the picture was made.
///
/// The fields are the ones a person filled in on purpose. `dc:subject` and
/// `xmp:Rating` are here and cannot be anywhere else — TIFF has ASCII tags for the
/// other three, and none for these two — which is the reason a packet is written at
/// all rather than tags alone.
pub fn metadata_packet(metadata: &Metadata) -> Option<String> {
    let mut external = metadata.clone();
    external.cleared.clear();
    if external.is_empty() {
        return None;
    }
    let metadata = &external;
    let mut o = String::new();
    o.push_str("<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n");
    o.push_str("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n");
    o.push_str(&format!(" <rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n"));
    o.push_str(&format!(
        "  <rdf:Description rdf:about=\"\"\n   xmlns:dc=\"{DC_NS}\"\n   xmlns:xmp=\"{XMP_NS}\"\n   xmlns:photoshop=\"{PHOTOSHOP_NS}\"\n"
    ));
    if let Some(r) = metadata.rating {
        o.push_str(&format!("   xmp:Rating=\"{r}\"\n"));
    }
    if let Some(l) = &metadata.label {
        o.push_str(&format!("   xmp:Label=\"{}\"\n", escape(l)));
    }
    o.push_str("   >\n");
    if let Some(v) = &metadata.creator {
        o.push_str(&format!(
            "   <dc:creator><rdf:Seq><rdf:li>{}</rdf:li></rdf:Seq></dc:creator>\n",
            escape(v)
        ));
    }
    for (tag, val) in [
        ("title", &metadata.title),
        ("rights", &metadata.rights),
        ("description", &metadata.description),
    ] {
        if let Some(v) = val {
            o.push_str(&format!(
                "   <dc:{tag}><rdf:Alt><rdf:li xml:lang=\"x-default\">{}</rdf:li></rdf:Alt></dc:{tag}>\n",
                escape(v)
            ));
        }
    }
    if !metadata.subject.is_empty() {
        o.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for s in &metadata.subject {
            o.push_str(&format!("     <rdf:li>{}</rdf:li>\n", escape(s)));
        }
        o.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }
    for (tag, val) in [
        ("Headline", &metadata.headline),
        ("Credit", &metadata.credit),
        ("Source", &metadata.source),
        ("City", &metadata.city),
        ("State", &metadata.state),
        ("Country", &metadata.country),
        ("Instructions", &metadata.instructions),
    ] {
        if let Some(v) = val {
            o.push_str(&format!(
                "   <photoshop:{tag}>{}</photoshop:{tag}>\n",
                escape(v)
            ));
        }
    }
    o.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>");
    Some(o)
}

/// Write the sidecar for `image`.
///
/// **Atomic**: written to a temporary file beside the target and renamed over it,
/// so an interrupted write cannot leave a half-file where a good one was. The
/// sidecar is the only record of an edit; losing it to a crash during save would be
/// the worst possible moment to lose it.
pub fn write(image: &Path, params: &Params, metadata: &Metadata) -> io::Result<()> {
    if let Loaded::Corrupt(why) = read(image) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, why));
    }
    let dest = path_for(image);
    let name = image
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let xml = to_xml(params, metadata, &name);

    crate::atomic_file::write(&dest, |file| {
        use std::io::Write;
        file.write_all(xml.as_bytes())
    })
}

// ------------------------------------------------------------------- reading

/// Read the sidecar for `image`.
pub fn read(image: &Path) -> Loaded<Sidecar> {
    let path = path_for(image);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Loaded::Absent,
        Err(e) => Loaded::Corrupt(format!("{}: {e}", path.display())),
        Ok(text) => from_xml(&text),
    }
}

/// Standard XMP metadata embedded in the source, if its format exposes a packet.
/// It is always read-only; authored changes belong to `<stem>.mono.xmp`.
pub fn embedded_metadata(image: &Path) -> Metadata {
    crate::sensor::xmp_packet(image)
        .and_then(|packet| from_xml(&packet).ok())
        .map(|sidecar| sidecar.metadata)
        .unwrap_or_default()
}

/// The source's embedded metadata with monopro's sidecar applied as a field-level
/// overlay. This is the record the UI shows and export embeds.
pub fn effective_metadata(image: &Path) -> Result<Metadata, String> {
    let embedded = embedded_metadata(image);
    match read(image) {
        Loaded::Absent => Ok(embedded),
        Loaded::Ok(sidecar) => Ok(Metadata::merged(&embedded, &sidecar.metadata)),
        Loaded::Corrupt(why) => Err(why),
    }
}

/// Parse sidecar text. Public so a test — and a future Lightbox that reads many
/// sidecars without touching `Params` — can use it directly.
pub fn from_xml(text: &str) -> Loaded<Sidecar> {
    let doc = match roxmltree::Document::parse(text) {
        Ok(d) => d,
        Err(e) => return Loaded::Corrupt(format!("not valid XML: {e}")),
    };
    let Some(desc) = doc
        .descendants()
        .find(|n| n.has_tag_name((RDF_NS, "Description")))
    else {
        return Loaded::Corrupt("no rdf:Description element".into());
    };

    let get = |name: &str| desc.attribute((MONOPRO_NS, name));
    let schema = match get("SchemaVersion") {
        None => 0,
        Some(value) => match value.parse::<u32>() {
            Ok(schema) => schema,
            Err(_) => return Loaded::Corrupt("invalid sidecar schema version".into()),
        },
    };

    if schema > SCHEMA_VERSION {
        return Loaded::Corrupt(format!(
            "sidecar schema {schema} requires a newer monopro (supports {SCHEMA_VERSION})"
        ));
    }
    let mut params = Params::default();
    let metadata = read_metadata(&desc);

    // A prototype sidecar. Only exposure survives the change of pipeline; see the
    // module note — and note the constant, which is not `SCHEMA_VERSION`.
    if schema < PIPELINE_SCHEMA {
        if let Some(v) = get("ExposureEV").and_then(finite) {
            params.exposure.ev = v;
        }
        return Loaded::Ok(Sidecar {
            params,
            metadata,
            schema,
        });
    }

    // Every field below follows the same rule: absent or unparseable leaves the
    // default in place and does not disturb anything else.
    let f = |name: &str| get(name).and_then(finite);
    let b = |name: &str| get(name).map(|v| v == "true");
    let u = |name: &str| get(name).and_then(|v| v.parse::<u32>().ok());

    params.decode = DecodeOptions {
        unity_wb: b("UnityWB").unwrap_or(params.decode.unity_wb),
    };

    let algo = get("DemosaicAlgo")
        .and_then(DemosaicAlgo::from_key)
        .unwrap_or_default();
    let sampling = match get("Sampling") {
        Some("superpixel") => Sampling::SuperPixel,
        Some("directmosaic") => Sampling::DirectMosaic,
        Some("demosaic") => Sampling::Demosaic(algo),
        _ => params.luminance.sampling,
    };
    let mix = get("WeightingMix").and_then(parse3);
    let weighting = match get("Weighting") {
        Some("photosite") => Weighting::Photosite,
        Some("equal") => Weighting::Equal,
        Some("red") => Weighting::Red,
        Some("green") => Weighting::Green,
        Some("blue") => Weighting::Blue,
        // The mix is stored whichever mode was selected, so returning to Weighted
        // returns to the mix that was left there.
        Some("weighted") => match mix {
            Some([r, g, b]) => Weighting::Weighted(r, g, b),
            None => Weighting::Photosite.as_weighted(),
        },
        _ => params.luminance.weighting,
    };
    params.luminance = LuminanceParams {
        sampling,
        weighting,
    };

    params.exposure = ExposureParams {
        // Absent reads as on, which is what every sidecar written before the bypass
        // existed means: those files describe a module that ran.
        enabled: b("ExposureEnabled").unwrap_or(ExposureParams::default().enabled),
        ev: f("ExposureEV").unwrap_or(params.exposure.ev),
        black: f("BlackCorrection").unwrap_or(params.exposure.black),
    };

    let d = ContrastMaskParams::default();
    params.contrast_mask = ContrastMaskParams {
        enabled: b("ContrastMaskEnabled").unwrap_or(d.enabled),
        contrast: f("MaskContrast").unwrap_or(d.contrast),
        spacer: f("MaskSpacerPercent").unwrap_or(d.spacer),
        offset: (
            f("MaskRegistrationX").unwrap_or(d.offset.0),
            f("MaskRegistrationY").unwrap_or(d.offset.1),
        ),
    };

    // Composition. A v3 file carries none of this and every field below falls to
    // its default, which is "as shot, unstraightened, uncropped" — the ordinary
    // case rather than the exception.
    let dc = CompositionParams::default();
    params.composition = CompositionParams {
        enabled: b("CompositionEnabled").unwrap_or(dc.enabled),
        orientation: match get("Orientation") {
            // Absent and "as-shot" are the same statement, and both must be, or a
            // v3 file would read as a deliberate override of nothing.
            None | Some("as-shot") => None,
            Some("0") => Some(Orientation::Rotate0),
            Some("90") => Some(Orientation::Rotate90),
            Some("180") => Some(Orientation::Rotate180),
            Some("270") => Some(Orientation::Rotate270),
            // An unrecognised value is not an instruction. Fall back to the file's
            // own tag rather than inventing a turn.
            Some(_) => None,
        },
        straighten: f("Straighten")
            .filter(|v| v.is_finite())
            .map(|v| {
                v.clamp(
                    *CompositionParams::STRAIGHTEN_RANGE.start(),
                    *CompositionParams::STRAIGHTEN_RANGE.end(),
                )
            })
            .unwrap_or(dc.straighten),
        keystone: {
            let dk = KeystoneParams::default();
            let guides = get("KeystoneGuides")
                .and_then(|value| {
                    let values = value
                        .split(',')
                        .map(str::trim)
                        .map(str::parse::<f32>)
                        .collect::<Result<Vec<_>, _>>()
                        .ok()?;
                    (values.len() == 8 && values.iter().all(|v| v.is_finite())).then(|| {
                        std::array::from_fn(|i| Point {
                            x: values[i * 2].clamp(0.0, 1.0),
                            y: values[i * 2 + 1].clamp(0.0, 1.0),
                        })
                    })
                })
                .unwrap_or(dk.guides);
            KeystoneParams {
                mode: get("KeystoneMode")
                    .and_then(parse_keystone_mode)
                    .unwrap_or(dk.mode),
                guides,
                correction: f("KeystoneCorrection")
                    .filter(|v| v.is_finite())
                    .map(|v| {
                        v.clamp(
                            *KeystoneParams::CORRECTION_RANGE.start(),
                            *KeystoneParams::CORRECTION_RANGE.end(),
                        )
                    })
                    .unwrap_or(dk.correction),
                aspect: f("KeystoneAspect")
                    .filter(|v| v.is_finite())
                    .map(|v| {
                        v.clamp(
                            *KeystoneParams::ASPECT_RANGE.start(),
                            *KeystoneParams::ASPECT_RANGE.end(),
                        )
                    })
                    .unwrap_or(dk.aspect),
                crop: get("KeystoneCrop")
                    .and_then(parse_keystone_crop)
                    .unwrap_or(dk.crop),
            }
        },
        // `clamped` is what stops a hand-edited file producing a crop that is
        // off the frame or of zero size; see `composition::Rect::clamped`.
        // `sane` is what stops a hand-edited file producing a crop of zero size;
        // the *frame* bound is applied at `Frame::place`, because a straightened
        // crop is entitled to sit outside the unit square.
        crop: get("Crop")
            .and_then(parse4)
            .map(|[x, y, w, h]| Rect { x, y, w, h }.sane())
            .unwrap_or(dc.crop),
        ratio: get("CropRatio").and_then(parse_ratio).unwrap_or(dc.ratio),
        portrait: b("CropPortrait").unwrap_or(dc.portrait),
    };

    let dd = DisplayParams::default();
    let shoulder = match ToneMap::SHOULDER_DEFAULT {
        ToneMap::Shoulder {
            threshold,
            strength,
        } => (threshold, strength),
        _ => unreachable!("SHOULDER_DEFAULT is a Shoulder"),
    };
    let tone_map = match get("ToneMap") {
        Some("clip") => ToneMap::Clip,
        Some("agx") => ToneMap::Agx(
            AgxParams {
                auto_range: b("AgxAutoRange").unwrap_or(AgxParams::DEFAULT.auto_range),
                black_ev: f("AgxBlackEv").unwrap_or(AgxParams::DEFAULT.black_ev),
                white_ev: f("AgxWhiteEv").unwrap_or(AgxParams::DEFAULT.white_ev),
                contrast: f("AgxContrast").unwrap_or(AgxParams::DEFAULT.contrast),
                toe_power: f("AgxToePower").unwrap_or(AgxParams::DEFAULT.toe_power),
                shoulder_power: f("AgxShoulderPower").unwrap_or(AgxParams::DEFAULT.shoulder_power),
            }
            .normalized(),
        ),
        Some("shoulder") => ToneMap::Shoulder {
            threshold: f("ShoulderThreshold").unwrap_or(shoulder.0),
            strength: f("ShoulderStrength").unwrap_or(shoulder.1),
        },
        _ => dd.tone_map,
    };
    params.display = DisplayParams {
        enabled: b("DisplayEnabled").unwrap_or(dd.enabled),
        tone_map,
        gamma: f("Gamma").unwrap_or(dd.gamma),
        dither: dd.dither,
    };

    // Grain. Absent reads as the default, which is **off** — so every file written
    // before this module existed opens describing the print it has always described.
    //
    // Every value is clamped into its range on the way in, `size` through the same
    // odd-enforcing path the setter uses. A hand-edited sidecar is a supported way to
    // reach this app, and a file claiming 400 layers should open at 60 rather than
    // spend four minutes per export.
    let dg = crate::grain::GrainParams::default();
    let mut grain = crate::grain::GrainParams {
        enabled: b("GrainEnabled").unwrap_or(dg.enabled),
        density: f("GrainDensity")
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(*grain::DENSITY_RANGE.start(), *grain::DENSITY_RANGE.end()))
            .unwrap_or(dg.density),
        layers: u("GrainLayers")
            .map(|v| v.clamp(*grain::LAYERS_RANGE.start(), *grain::LAYERS_RANGE.end()))
            .unwrap_or(dg.layers),
        variability: f("GrainVariability")
            .filter(|v| v.is_finite())
            .map(|v| {
                v.clamp(
                    *grain::VARIABILITY_RANGE.start(),
                    *grain::VARIABILITY_RANGE.end(),
                )
            })
            .unwrap_or(dg.variability),
        sensitivity: f("GrainSensitivity")
            .filter(|v| v.is_finite())
            .map(|v| {
                v.clamp(
                    *grain::SENSITIVITY_RANGE.start(),
                    *grain::SENSITIVITY_RANGE.end(),
                )
            })
            .unwrap_or(dg.sensitivity),
        seed: get("GrainSeed")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(dg.seed),
        ..dg
    };
    // Through the setter, so a file carrying an even size arrives snapped rather than
    // snapped later by the kernel behind the panel's back.
    grain.set_size(u("GrainSize").unwrap_or(dg.size));
    params.grain = grain;

    // Output sharpening. Absent reads as the default, which is **off**, for grain's
    // reason exactly: a file written before this module existed describes a print with
    // no sharpening in it and must keep describing that print. Clamped on the way in
    // for grain's other reason — a hand-edited sidecar is a supported route into this
    // app, and an amount of 50 should arrive at 2 rather than solarise the file.
    let ds = crate::sharpen::SharpenParams::default();
    let clamp = |v: Option<f32>, r: &std::ops::RangeInclusive<f32>, d: f32| {
        v.filter(|v| v.is_finite())
            .map(|v| v.clamp(*r.start(), *r.end()))
            .unwrap_or(d)
    };
    params.sharpen = crate::sharpen::SharpenParams {
        enabled: b("SharpenEnabled").unwrap_or(ds.enabled),
        amount: clamp(f("SharpenAmount"), &sharpen::AMOUNT_RANGE, ds.amount),
        radius: clamp(f("SharpenRadius"), &sharpen::RADIUS_RANGE, ds.radius),
        edges: clamp(f("SharpenEdges"), &sharpen::EDGES_RANGE, ds.edges),
    };

    // Output. `PrintSize` absent is the ordinary case and means "write the picture's
    // own pixels" — which is exactly what a file written before Composition says.
    let dout = OutputParams::default();
    params.output = OutputParams {
        ppi: f("PrintPPI")
            .filter(|v| v.is_finite())
            .map(|v| {
                v.clamp(
                    *OutputParams::PPI_RANGE.start(),
                    *OutputParams::PPI_RANGE.end(),
                )
            })
            .unwrap_or(dout.ppi),
        // A non-finite or non-positive size is not an instruction to resample to
        // nothing; it is a broken file, and the safe reading is the one that leaves
        // every pixel alone.
        resize: f("PrintSize")
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|inches| Resize {
                inches,
                axis: if get("PrintAxis") == Some("h") {
                    OutAxis::Height
                } else {
                    OutAxis::Width
                },
            }),
        filter: get("ResampleFilter")
            .and_then(Filter::from_key)
            .unwrap_or(dout.filter),
    };

    // FRAME. Absent is the off default, so every pre-v13 image keeps exporting the
    // same dimensions it always did. Invalid authored geometry is sanitised here and
    // a too-small fixed frame is reported by layout rather than silently resizing the
    // photograph to make it fit.
    let df = FrameParams::default();
    let margins = get("FrameMargins")
        .and_then(parse4)
        .map(|v| FrameMargins {
            left: v[0],
            top: v[1],
            right: v[2],
            bottom: v[3],
        })
        .unwrap_or(df.margins)
        .sane();
    let outer = get("FrameOuter")
        .and_then(parse2)
        .map(|v| {
            v.map(|n| {
                if n.is_finite() {
                    n.clamp(0.0, FrameParams::MAX_INCHES)
                } else {
                    0.0
                }
            })
        })
        .unwrap_or(df.outer_inches);
    let position = get("FramePosition")
        .and_then(parse2)
        .map(|v| {
            v.map(|n| {
                if n.is_finite() {
                    n.clamp(0.0, 1.0)
                } else {
                    0.5
                }
            })
        })
        .unwrap_or(df.custom_position);
    params.frame = FrameParams {
        enabled: b("FrameEnabled").unwrap_or(df.enabled),
        unit: get("FrameUnit").and_then(Unit::from_key).unwrap_or(df.unit),
        priority: get("FramePriority")
            .and_then(FramePriority::from_key)
            .unwrap_or(df.priority),
        equal: b("FrameEqual").unwrap_or(df.equal),
        margins,
        outer_inches: outer,
        custom_size: b("FrameCustomSize").unwrap_or(df.custom_size),
        placement: get("FramePlacement")
            .and_then(FramePlacement::from_key)
            .unwrap_or(df.placement),
        bottom_weight_inches: f("FrameBottomWeight")
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, FrameParams::MAX_INCHES))
            .unwrap_or(df.bottom_weight_inches),
        custom_position: position,
        color: get("FrameColor").and_then(parse_u8_3).unwrap_or(df.color),
        trim_line: b("FrameTrimLine").unwrap_or(df.trim_line),
    };

    params.curve = if let Some(instances) = read_curves(&desc) {
        CurveStack {
            enabled: true,
            instances,
        }
    } else if let Some(pts) = read_curve(&desc)
        && let Some(c) = Curve::from_points(&pts)
    {
        // The pre-stack shape becomes Curve 1 exactly; no visual migration.
        CurveStack {
            enabled: true,
            instances: vec![CurveInstance {
                name: "Curve 1".into(),
                curve: c,
                opacity: 1.0,
            }],
        }
    } else {
        CurveStack::default()
    };
    params.curve.enabled = b("CurveEnabled").unwrap_or(CurveStack::default().enabled);

    // Absent reads as on, which is what a v4 file means: it was written before the
    // module existed, so it describes a picture with no strokes on it, and a
    // switch that arrived later cannot have been thrown.
    params.toning = read_toning(&desc);

    params.dodgeburn = read_dodgeburn(
        &desc,
        b("DodgeBurnEnabled").unwrap_or(crate::dodgeburn::DodgeBurnParams::default().enabled),
    );

    Loaded::Ok(Sidecar {
        params,
        metadata,
        schema,
    })
}

fn finite(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

fn parse3(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split(',').map(|v| v.trim().parse::<f32>());
    let (r, g, b) = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
    if it.next().is_some() || ![r, g, b].iter().all(|v| v.is_finite()) {
        None
    } else {
        Some([r, g, b])
    }
}

fn parse2(s: &str) -> Option<[f32; 2]> {
    let mut it = s.split(',').map(str::trim);
    let out = [finite(it.next()?)?, finite(it.next()?)?];
    it.next().is_none().then_some(out)
}

fn parse_u8_3(s: &str) -> Option<[u8; 3]> {
    let mut it = s.split(',').map(str::trim);
    let out = [
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ];
    it.next().is_none().then_some(out)
}

fn parse4(s: &str) -> Option<[f32; 4]> {
    let mut it = s.split(',').map(|v| v.trim().parse::<f32>());
    let mut out = [0.0f32; 4];
    for slot in &mut out {
        *slot = it.next()?.ok()?;
    }
    // All four or none: a partial rectangle is not a rectangle, and taking three
    // of its numbers would place the crop somewhere nobody asked for.
    if it.next().is_some() || !out.iter().all(|v| v.is_finite()) {
        None
    } else {
        Some(out)
    }
}

fn read_curve(desc: &roxmltree::Node) -> Option<Vec<[f32; 2]>> {
    let cp = desc
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "CurvePoints")))?;
    let seq = cp.children().find(|n| n.has_tag_name((RDF_NS, "Seq")))?;
    let pts: Vec<[f32; 2]> = seq
        .children()
        .filter(|n| n.has_tag_name((RDF_NS, "li")))
        .filter_map(|li| {
            let t = li.text()?;
            let (x, y) = t.split_once(',')?;
            Some([finite(x)?, finite(y)?])
        })
        .collect();
    (pts.len() >= 2).then_some(pts)
}

fn write_curves(o: &mut String, curves: &CurveStack) {
    o.push_str("   <monopro:Curves>\n    <rdf:Seq>\n");
    for instance in &curves.instances {
        o.push_str(&format!(
            "     <rdf:li monopro:Name=\"{}\" monopro:Opacity=\"{}\" monopro:Enabled=\"{}\">\n",
            escape(&instance.name),
            num(instance.opacity),
            instance.curve.enabled,
        ));
        o.push_str("      <monopro:CurvePoints>\n       <rdf:Seq>\n");
        for pt in instance.curve.points() {
            o.push_str(&format!(
                "        <rdf:li>{},{}</rdf:li>\n",
                num(pt[0]),
                num(pt[1])
            ));
        }
        o.push_str("       </rdf:Seq>\n      </monopro:CurvePoints>\n     </rdf:li>\n");
    }
    o.push_str("    </rdf:Seq>\n   </monopro:Curves>\n");
}

fn read_curves(desc: &roxmltree::Node) -> Option<Vec<CurveInstance>> {
    let node = desc
        .children()
        .find(|n| n.has_tag_name((MONOPRO_NS, "Curves")))?;
    let seq = node.children().find(|n| n.has_tag_name((RDF_NS, "Seq")))?;
    let instances: Vec<CurveInstance> = seq
        .children()
        .filter(|n| n.has_tag_name((RDF_NS, "li")))
        .filter_map(|li| {
            let points = read_curve(&li)?;
            let mut curve = Curve::from_points(&points)?;
            curve.enabled = li
                .attribute((MONOPRO_NS, "Enabled"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(true);
            let name = li
                .attribute((MONOPRO_NS, "Name"))
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or("Curve")
                .to_owned();
            let opacity = li
                .attribute((MONOPRO_NS, "Opacity"))
                .and_then(finite)
                .filter(|v| v.is_finite())
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            Some(CurveInstance {
                name,
                curve,
                opacity,
            })
        })
        .take(CurveStack::MAX_INSTANCES)
        .collect();
    (!instances.is_empty()).then_some(instances)
}

fn read_metadata(desc: &roxmltree::Node) -> Metadata {
    // `rdf:Seq`, `rdf:Alt` and `rdf:Bag` differ in meaning but not in how the text
    // is reached, so one helper reads all three.
    let list = |tag: &str| -> Vec<String> {
        desc.children()
            .find(|n| n.has_tag_name((DC_NS, tag)))
            .into_iter()
            .flat_map(|n| n.children().collect::<Vec<_>>())
            .flat_map(|c| c.children().collect::<Vec<_>>())
            .filter(|n| n.has_tag_name((RDF_NS, "li")))
            .filter_map(|n| n.text().map(str::to_owned))
            .collect()
    };
    let simple = |ns: &str, tag: &str| -> Option<String> {
        desc.attribute((ns, tag))
            .map(str::to_owned)
            .or_else(|| {
                desc.children()
                    .find(|node| node.has_tag_name((ns, tag)))
                    .and_then(|node| node.text())
                    .map(str::to_owned)
            })
            .filter(|value| !value.is_empty())
    };
    Metadata {
        creator: list("creator").into_iter().next(),
        rights: list("rights").into_iter().next(),
        title: list("title").into_iter().next(),
        headline: simple(PHOTOSHOP_NS, "Headline"),
        description: list("description").into_iter().next(),
        credit: simple(PHOTOSHOP_NS, "Credit"),
        source: simple(PHOTOSHOP_NS, "Source"),
        city: simple(PHOTOSHOP_NS, "City"),
        state: simple(PHOTOSHOP_NS, "State"),
        country: simple(PHOTOSHOP_NS, "Country"),
        instructions: simple(PHOTOSHOP_NS, "Instructions"),
        subject: list("subject"),
        rating: desc
            .attribute((XMP_NS, "Rating"))
            .and_then(|v| v.parse().ok()),
        // Not matched against this app's own six names. An unrecognised label is
        // another application's vocabulary, and it is carried through as found — the
        // tile shows no dot for a word it does not know, and a write puts the word
        // back exactly as it was.
        label: desc
            .attribute((XMP_NS, "Label"))
            .map(str::to_owned)
            .filter(|s| !s.is_empty()),
        cleared: desc
            .attribute((MONOPRO_NS, "MetadataCleared"))
            .into_iter()
            .flat_map(|keys| keys.split(','))
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dither_switch_from_an_older_file_is_ignored() {
        // Dither stopped being per image: the screen follows a viewer preference and
        // 8-bit files the proof's. A file written while it was per image must still
        // open, and its old switch must not quietly turn the screen's dither off.
        let old = to_xml(&Params::default(), &Metadata::default(), "frame.dng").replace(
            "monopro:Gamma=",
            "monopro:Dither=\"false\"\n   monopro:Gamma=",
        );
        assert!(
            old.contains("Dither=\"false\""),
            "the fixture carries the old attribute"
        );
        let Loaded::Ok(read) = from_xml(&old) else {
            panic!("a file with the retired attribute still opens");
        };
        assert!(read.params.display.dither);
        assert!(!to_xml(&read.params, &read.metadata, "frame.dng").contains("Dither"));
    }
    use crate::dodgeburn::{Dab, Gesture, Instance, Linear, Radial, Shape, Sign, ZoneMask};

    fn roundtrip(p: &Params) -> Params {
        let xml = to_xml(p, &Metadata::default(), "test.dng");
        match from_xml(&xml) {
            Loaded::Ok(s) => s.params,
            other => panic!("did not read back: {other:?}"),
        }
    }

    #[test]
    fn future_sidecars_are_not_loaded_or_overwritten() {
        let image = std::env::temp_dir().join(format!("monopro-future-{}.dng", std::process::id()));
        let xml = to_xml(&Params::default(), &Metadata::default(), "future.dng").replace(
            &format!("SchemaVersion=\"{SCHEMA_VERSION}\""),
            "SchemaVersion=\"999\"",
        );
        std::fs::write(path_for(&image), &xml).unwrap();
        let result = write(&image, &Params::default(), &Metadata::default());
        let preserved = std::fs::read_to_string(path_for(&image)).unwrap();
        std::fs::remove_file(path_for(&image)).unwrap();
        assert!(matches!(from_xml(&xml), Loaded::Corrupt(_)));
        assert!(result.is_err());
        assert_eq!(preserved, xml);
    }

    #[test]
    fn non_finite_exposure_uses_the_default_in_current_and_legacy_files() {
        for schema in [2, SCHEMA_VERSION] {
            for value in ["NaN", "inf", "-inf", "1e999"] {
                let xml = format!(
                    r#"<rdf:RDF xmlns:rdf="{RDF_NS}" xmlns:monopro="{MONOPRO_NS}"><rdf:Description monopro:SchemaVersion="{schema}" monopro:ExposureEV="{value}"/></rdf:RDF>"#
                );
                let sidecar = from_xml(&xml).ok().unwrap();
                assert_eq!(sidecar.params.exposure.ev, Params::default().exposure.ev);
            }
        }
        assert!(parse2("NaN,1").is_none());
        assert!(parse3("1,inf,0").is_none());
    }

    #[test]
    fn defaults_round_trip_exactly() {
        let p = Params::default();
        assert_eq!(roundtrip(&p), p);
    }

    #[test]
    fn retired_decode_attributes_are_not_written_and_cannot_reenable_their_paths() {
        let p = Params::default();
        let xml = to_xml(&p, &Metadata::default(), "test.dng");
        for name in [
            "HighlightReconstruction",
            "HighlightSoft",
            "HighlightWhite",
            "TcaCorrect",
        ] {
            assert!(!xml.contains(name), "current schema still wrote {name}");
        }

        // A schema-15 sidecar may still carry every retired attribute. They are
        // deliberately unknown to the active parameter model: the file opens, its
        // remaining edits survive, and neither removed rendering path can return.
        let legacy = xml
            .replace(
                &format!("SchemaVersion=\"{SCHEMA_VERSION}\""),
                "SchemaVersion=\"15\"",
            )
            .replace(
                "   monopro:UnityWB=\"false\"\n",
                concat!(
                    "   monopro:UnityWB=\"false\"\n",
                    "   monopro:HighlightReconstruction=\"true\"\n",
                    "   monopro:HighlightSoft=\"60\"\n",
                    "   monopro:HighlightWhite=\"1000\"\n",
                    "   monopro:TcaCorrect=\"true\"\n",
                ),
            );
        let Loaded::Ok(sidecar) = from_xml(&legacy) else {
            panic!("a legacy sidecar with retired attributes must still open")
        };
        assert_eq!(sidecar.schema, 15);
        assert_eq!(sidecar.params, p);
    }

    #[test]
    fn the_order_names_bypasses_and_points_of_every_curve_round_trip() {
        let mut p = Params::default();
        p.curve.instances[0].name = "Shadows".into();
        p.curve.instances[0].curve.add(0.25, 0.38);
        let second = p.curve.add_instance().expect("room for Curve 2");
        p.curve.instances[second].name = "Print highlights".into();
        p.curve.instances[second].curve.add(0.82, 0.72);
        p.curve.instances[second].curve.enabled = false;
        p.curve.instances[second].opacity = 0.42;

        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        assert!(xml.contains("<monopro:Curves>"), "{xml}");
        assert_eq!(roundtrip(&p).curve, p.curve);
    }

    #[test]
    fn a_curve_stack_from_before_opacity_reads_every_instance_at_full_strength() {
        let mut p = Params::default();
        p.curve.instances[0].curve.add(0.4, 0.6);
        let xml = to_xml(&p, &Metadata::default(), "t.dng").replace(" monopro:Opacity=\"1\"", "");
        assert!(
            !xml.contains("monopro:Opacity"),
            "fixture still carries opacity"
        );
        let Loaded::Ok(sidecar) = from_xml(&xml) else {
            panic!("legacy stack did not read")
        };
        assert_eq!(sidecar.params.curve.instances[0].opacity, 1.0);
    }

    #[test]
    fn every_field_round_trips() {
        // One value per field, all different from the default, so a field the
        // writer forgot shows up as a mismatch rather than as a coincidence.
        let mut p = Params::default();
        p.decode.unity_wb = true;
        p.luminance.sampling = Sampling::Demosaic(DemosaicAlgo::Amaze);
        p.luminance.weighting = Weighting::Weighted(0.7, 0.2, 0.1);
        p.exposure.enabled = false;
        p.exposure.ev = 1.625;
        p.exposure.black = -0.0125;
        p.curve.enabled = false;
        p.contrast_mask.enabled = true;
        p.contrast_mask.contrast = 0.375;
        p.contrast_mask.spacer = 2.25;
        p.contrast_mask.offset = (3.5, -4.25);
        p.display.enabled = false;
        p.display.tone_map = ToneMap::Shoulder {
            threshold: 0.625,
            strength: 0.375,
        };
        p.display.gamma = 2.4;
        p.composition.enabled = false;
        p.composition.orientation = Some(Orientation::Rotate270);
        p.composition.straighten = -2.75;
        p.composition.keystone = KeystoneParams {
            mode: KeystoneMode::Rectangle,
            guides: [
                Point { x: 0.17, y: 0.21 },
                Point { x: 0.83, y: 0.16 },
                Point { x: 0.76, y: 0.86 },
                Point { x: 0.24, y: 0.79 },
            ],
            correction: 0.73,
            aspect: 6.25,
            crop: KeystoneCrop::Original,
        };
        p.composition.crop = Rect {
            x: 0.125,
            y: 0.0625,
            w: 0.5,
            h: 0.375,
        };
        // A real preset, taken from the table rather than retyped — a literal here
        // would drift from the list it is meant to be exercising.
        p.composition.ratio = Ratio::Fixed(std::f32::consts::SQRT_2);
        p.composition.portrait = true;
        p.curve.add(0.3, 0.55);
        p.curve.add(0.7, 0.8);
        p.output.ppi = 360.0;
        p.output.resize = Some(Resize {
            inches: 11.25,
            axis: OutAxis::Height,
        });
        p.output.filter = Filter::Mitchell;
        p.frame.enabled = true;
        p.frame.unit = Unit::Centimetres;
        p.frame.priority = FramePriority::Outer;
        p.frame.equal = false;
        p.frame.margins = FrameMargins {
            left: 1.25,
            top: 1.5,
            right: 1.75,
            bottom: 2.0,
        };
        p.frame.outer_inches = [22.0, 28.0];
        p.frame.custom_size = true;
        p.frame.placement = FramePlacement::Custom;
        p.frame.bottom_weight_inches = 1.375;
        p.frame.custom_position = [0.375, 0.625];
        p.frame.color = [231, 225, 210];
        p.frame.trim_line = true;
        p.sharpen.enabled = true;
        p.sharpen.amount = 1.25;
        p.sharpen.radius = 1.75;
        p.sharpen.edges = 0.75;
        p.grain.enabled = true;
        p.grain.set_size(13);
        p.grain.density = 0.4375;
        p.grain.layers = 45;
        p.grain.variability = 0.625;
        p.grain.sensitivity = -1.25;
        p.grain.seed = 1_234_567_890;
        // Dodge & Burn: two instances of different signs, one with passes and a
        // bounded mask, one empty and switched off — so a writer that dropped the
        // empty instance, or the second one, or the mask, shows up here.
        //
        // Every number is exact in five decimal places, which is what `dab_num`
        // writes. A dab at 1/3 would fail this test for a reason that is a
        // property of the format rather than a bug in it; see
        // `dab_positions_round_trip_to_a_thousandth_of_a_pixel`.
        p.dodgeburn.enabled = false;
        p.dodgeburn.instances = vec![
            Instance {
                opacity: 0.75,
                contrast: 0.375,
                mask: ZoneMask {
                    enabled: true,
                    lo: -2.5,
                    hi: 1.25,
                    f_lo: 0.5,
                    f_hi: 1.5,
                    invert: true,
                    blur: 0.025,
                    edge_aware: false,
                    region: 0.075,
                    edge: 1.25,
                },
                ..Instance::of(
                    Sign::Burn,
                    "Burn 1".into(),
                    Shape::brush(vec![
                        Gesture::new(vec![
                            Dab {
                                x: 0.25,
                                y: 0.5,
                                radius: 0.0625,
                                feather: 0.4,
                                opacity: 1.0,
                                ev: -0.25,
                                ..Dab::ROUND
                            },
                            Dab {
                                x: 0.375,
                                y: 0.5,
                                radius: 0.0625,
                                feather: 0.4,
                                opacity: 0.5,
                                ev: -0.25,
                                ..Dab::ROUND
                            },
                        ]),
                        Gesture::new(vec![Dab {
                            x: 0.75,
                            y: 0.25,
                            radius: 0.125,
                            feather: 0.0,
                            opacity: 1.0,
                            ev: 0.5,
                            ..Dab::ROUND
                        }]),
                    ]),
                )
            },
            Instance {
                enabled: false,
                ..Instance::new(Sign::Dodge, "Dodge 1".into())
            },
            // A gradient of each kind, so the shape discriminant and both geometry
            // groups are exercised by the same assertion that covers every scalar.
            Instance::of(
                Sign::Burn,
                "Linear Burn 1".into(),
                Shape::Linear(Linear {
                    x0: 0.125,
                    y0: 0.25,
                    x1: 0.75,
                    y1: 0.625,
                    feather: 0.5,
                    ev: -0.75,
                }),
            ),
            Instance::of(
                Sign::Dodge,
                "Radial Dodge 1".into(),
                Shape::Radial(Radial {
                    cx: 0.375,
                    cy: 0.5,
                    inner: 0.125,
                    outer: 0.625,
                    aspect: 1.5,
                    angle: -22.5,
                    feather: 0.75,
                    invert: true,
                    ev: 1.25,
                }),
            ),
        ];

        assert_eq!(roundtrip(&p), p);
    }

    #[test]
    fn a_pass_is_one_element_and_the_dabs_inside_it_are_not() {
        // The format decision, as an assertion. The payload nests one level deeper
        // than `CurvePoints` and is packed at the innermost level, so XML overhead
        // is paid per PASS. An element per dab would be equally honest XMP and
        // roughly two and a half times the file — about 55 bytes of tags and
        // indentation against one space. If someone "tidies" this into an
        // `<rdf:li>` per dab, this test is why not.
        let mut p = Params::default();
        p.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![Gesture::new(
                (0..20)
                    .map(|i| Dab {
                        x: i as f32 / 40.0,
                        y: 0.5,
                        radius: 0.05,
                        feather: 0.4,
                        opacity: 1.0,
                        ev: -0.25,
                        ..Dab::ROUND
                    })
                    .collect(),
            )]),
        )];
        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        assert_eq!(
            xml.matches("<rdf:li>").count(),
            1 + p.curve.points().len(),
            "one li per pass"
        );
        assert_eq!(roundtrip(&p), p);
    }

    #[test]
    fn a_thousand_dabs_is_a_sensible_sized_sidecar() {
        // The brief asks for this measured rather than assumed. A thousand current
        // shaped dabs is 36 KB in memory; the question was what it costs as XML text.
        //
        // Measured: packed six-tuples at five decimal places come out at **36 KB**,
        // against the 60 KB the brief estimated. Comfortably smaller than the
        // embedded JPEG preview of any raw this will sit beside. The bound below is
        // generous on purpose — it is here to catch a format change that doubles
        // the file, not to pin a byte count that will drift with the sample data.
        let mut p = Params::default();
        p.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(
                (0..20)
                    .map(|g| {
                        Gesture::new(
                            (0..50)
                                .map(|i| Dab {
                                    x: 0.1 + (g * 50 + i) as f32 * 0.0008,
                                    y: 0.5 + (i as f32 * 0.37).sin() * 0.2,
                                    radius: 0.0625,
                                    feather: 0.4,
                                    opacity: 1.0,
                                    ev: -0.25,
                                    ..Dab::ROUND
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            ),
        )];
        assert_eq!(p.dodgeburn.total_dabs(), 1000);

        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        assert!(
            xml.len() < 40_000,
            "a thousand dabs wrote {} bytes of XML",
            xml.len()
        );
        assert!(
            xml.len() > 20_000,
            "suspiciously small — did the dabs get written at all?"
        );
    }

    #[test]
    fn dab_positions_round_trip_to_a_thousandth_of_a_pixel() {
        // What five decimal places actually buys, stated in the units that matter.
        // On a 6000px negative, 1e-5 of the width is 0.06px — far below anything
        // that can be seen, and far below the sub-pixel phase the view transform
        // already introduces.
        let mut p = Params::default();
        let awkward = [1.0 / 3.0, 0.123_456_79, 0.999_99, 1e-6];
        p.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![Gesture::new(
                awkward
                    .iter()
                    .map(|&x| Dab {
                        x,
                        y: x,
                        radius: 0.05,
                        feather: 0.4,
                        opacity: 1.0,
                        ev: -0.25,
                        ..Dab::ROUND
                    })
                    .collect(),
            )]),
        )];
        let back = roundtrip(&p);
        for (a, b) in p.dodgeburn.instances[0].gestures()[0]
            .dabs
            .iter()
            .zip(back.dodgeburn.instances[0].gestures()[0].dabs.iter())
        {
            assert!((a.x - b.x).abs() <= 1e-5, "{} -> {}", a.x, b.x);
            assert!((a.y - b.y).abs() <= 1e-5, "{} -> {}", a.y, b.y);
        }
    }

    #[test]
    fn a_hand_edited_pass_cannot_produce_a_dab_that_is_not_one() {
        // The same rule the crop rectangle follows: a tuple that is not six finite
        // numbers is dropped, rather than half-read into a mark nobody placed.
        let with = to_xml(&Params::default(), &Metadata::default(), "t.dng").replace(
            "  </rdf:Description>",
            "   <monopro:DodgeBurn>\n    <rdf:Seq>\n     <rdf:li rdf:parseType=\"Resource\" monopro:Name=\"Burn 1\" monopro:Sign=\"burn\">\n      <monopro:Passes>\n       <rdf:Seq>\n        <rdf:li>0.5,0.5,0.05,0.4,1,-0.25 0.6,0.6,0.05 0.7,0.7,0.05,0.4,1,-0.25,9 nonsense 0.8,0.8,0.05,0.4,1,NaN</rdf:li>\n       </rdf:Seq>\n      </monopro:Passes>\n     </rdf:li>\n    </rdf:Seq>\n   </monopro:DodgeBurn>\n  </rdf:Description>",
        );
        let s = from_xml(&with)
            .ok()
            .expect("a bad tuple must not lose the file");
        let dabs = &s.params.dodgeburn.instances[0].gestures()[0].dabs;
        assert_eq!(
            dabs.len(),
            1,
            "only the well-formed tuple survives: {dabs:?}"
        );
        assert_eq!(dabs[0].x, 0.5);
    }

    #[test]
    fn a_sidecar_from_before_the_gradients_reads_its_instances_as_brushes() {
        // A v5 file has instances but no `Shape` attribute, because there was only
        // one shape when it was written. Absent must read as a brush — anything
        // else would turn every stroke set on disk into a gradient with no geometry.
        let mut p = Params::default();
        p.dodgeburn.instances = vec![Instance::of(
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
        )];
        let xml = to_xml(&p, &Metadata::default(), "t.dng").replace(" monopro:Shape=\"brush\"", "");
        let s = from_xml(&xml).ok().expect("a v5 file must open");
        assert!(matches!(
            s.params.dodgeburn.instances[0].shape,
            Shape::Brush { .. }
        ));
        assert_eq!(
            s.params.dodgeburn.instances[0].dab_count(),
            1,
            "and it kept its dabs"
        );
    }

    /// The layer's nib survives a write and a read — **including on a layer with no
    /// strokes on it**, which is the whole reason it is stored on the layer.
    #[test]
    fn a_card_layer_is_still_a_card_layer_after_a_round_trip() {
        let mut p = Params::default();
        p.dodgeburn.instances = vec![
            // Empty. Nothing in the passes to derive a nib from, so if the attribute
            // is not written this comes back Round and the panel says the wrong word.
            Instance::of(
                Sign::Burn,
                "Burn 1".into(),
                Shape::Brush {
                    nib: crate::dodgeburn::Nib::Card,
                    passes: Vec::new(),
                },
            ),
            Instance::of(
                Sign::Dodge,
                "Dodge 1".into(),
                Shape::brush(vec![Gesture::new(vec![Dab {
                    x: 0.5,
                    y: 0.5,
                    radius: 0.05,
                    feather: 0.4,
                    opacity: 1.0,
                    ev: 0.25,
                    ..Dab::ROUND
                }])]),
            ),
        ];
        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        let s = from_xml(&xml).ok().expect("round trip");
        let got: Vec<_> = s
            .params
            .dodgeburn
            .instances
            .iter()
            .map(|i| i.shape.ui_label())
            .collect();
        assert_eq!(got, ["Card", "Round"]);
    }

    /// A file written before the nib moved onto the layer has the answer in its dabs.
    ///
    /// This is the migration, and it is a read-side default rather than a rewrite: the
    /// attribute is absent, so the first dab decides. A **mixed** layer can only come
    /// from such a file — the app can no longer make one — and it is relabelled by its
    /// first stroke while every dab keeps the shape it was painted with.
    #[test]
    fn a_sidecar_written_before_the_layer_owned_its_nib_reads_the_nib_off_its_dabs() {
        let card = Dab {
            x: 0.5,
            y: 0.5,
            radius: 0.05,
            feather: 0.4,
            opacity: 1.0,
            ev: -0.25,
            nib: crate::dodgeburn::Nib::Card,
            ..Dab::ROUND
        };
        let mut p = Params::default();
        // Deliberately inconsistent, which is what the old format allowed: the layer
        // says Round (its default) and the strokes are cards.
        p.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![Gesture::new(vec![card])]),
        )];

        let xml = to_xml(&p, &Metadata::default(), "t.dng").replace(" monopro:Nib=\"round\"", "");
        assert!(
            !xml.contains("monopro:Nib"),
            "the fixture must actually be missing it"
        );

        let s = from_xml(&xml).ok().expect("an older file must open");
        let inst = &s.params.dodgeburn.instances[0];
        assert_eq!(
            inst.shape.ui_label(),
            "Card",
            "the dabs said card, so the layer is a card"
        );
        assert_eq!(
            inst.gestures()[0].dabs[0].nib,
            crate::dodgeburn::Nib::Card,
            "and not one stroke moved",
        );
    }

    #[test]
    fn a_gradient_with_no_geometry_does_not_become_one_in_the_middle_of_the_frame() {
        // A hand-edited or truncated file. Inventing a gradient would put a mark on
        // the picture nobody placed; the instance stays a brush with nothing on it,
        // which is visible in the panel and obviously wrong to whoever looks.
        let bad = concat!(
            "   <monopro:DodgeBurn>\n    <rdf:Seq>\n",
            "     <rdf:li rdf:parseType=\"Resource\" monopro:Name=\"Linear Burn 1\" ",
            "monopro:Sign=\"burn\" monopro:Shape=\"linear\">\n",
            "     </rdf:li>\n    </rdf:Seq>\n   </monopro:DodgeBurn>\n  </rdf:Description>"
        );
        let xml = to_xml(&Params::default(), &Metadata::default(), "t.dng")
            .replace("  </rdf:Description>", bad);
        let s = from_xml(&xml)
            .ok()
            .expect("a bad gradient must not lose the file");
        let inst = &s.params.dodgeburn.instances[0];
        assert!(
            matches!(inst.shape, Shape::Brush { .. }),
            "{:?}",
            inst.shape
        );
        assert!(!inst.is_active(), "and it renders nothing");
    }

    #[test]
    fn a_sidecar_from_before_dodge_and_burn_opens_with_none_of_it() {
        // A v4 file describes a picture with no strokes on it. It must not open
        // with the module switched off — that would be an edit the file does not
        // record — nor with anything painted on it.
        let mut p = Params::default();
        p.exposure.ev = 0.75;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|l| !l.contains("monopro:DodgeBurnEnabled="))
            .collect::<Vec<_>>()
            .join("\n");
        let s = from_xml(&xml).ok().expect("a v4 file must open");
        assert!(s.params.dodgeburn.instances.is_empty());
        assert!(
            s.params.dodgeburn.enabled,
            "absent must read as the module's default, which is on"
        );
        assert_eq!(s.params.exposure.ev, 0.75);
    }

    #[test]
    fn a_v3_sidecar_keeps_everything_it_had_when_the_version_moved_to_four() {
        // The regression the version bump nearly shipped. The prototype test was
        // `schema < SCHEMA_VERSION`, which was right for exactly as long as there
        // was one version of this pipeline's format — the moment the number moved,
        // every v3 file on disk would have been read as a prototype sidecar and
        // stripped back to its exposure. Every one of the maintainer's edits, silently.
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = 2.25;
        p.luminance.sampling = Sampling::Demosaic(DemosaicAlgo::Amaze);
        p.display.gamma = 2.4;
        p.curve.add(0.4, 0.6);

        // A v3 file is a v4 file with the composition attributes removed and the
        // version number put back.
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|l| {
                ![
                    "CompositionEnabled",
                    "Orientation",
                    "Straighten",
                    "Crop",
                    "CropRatio",
                ]
                .iter()
                .any(|a| l.contains(&format!("monopro:{a}=")))
            })
            .collect::<Vec<_>>()
            .join("\n")
            // Against the constant, not against the literal it happened to be
            // when this was written: the whole claim is that a v3 file survives
            // *every* future bump, and a hardcoded "4" would have made the test
            // stop exercising it the first time one landed. It did, at 5.
            .replace(
                &format!("SchemaVersion=\"{SCHEMA_VERSION}\""),
                "SchemaVersion=\"3\"",
            );

        let s = from_xml(&xml).ok().expect("a v3 file must still open");
        assert_eq!(s.schema, 3);
        assert_eq!(s.params.exposure.ev, 1.5, "a v3 file lost its exposure");
        assert_eq!(
            s.params.contrast_mask.spacer, 2.25,
            "a v3 file lost its contrast mask"
        );
        assert_eq!(
            s.params.luminance.sampling,
            Sampling::Demosaic(DemosaicAlgo::Amaze)
        );
        assert_eq!(s.params.display.gamma, 2.4);
        assert!(!s.params.curve.is_identity(), "a v3 file lost its curve");
        // And composition opens at its default, which is the ordinary case: as
        // shot, unstraightened, uncropped.
        assert_eq!(s.params.composition, CompositionParams::default());
        assert_eq!(
            s.params.composition.orientation, None,
            "a v3 file must read as as-shot"
        );
    }

    #[test]
    fn a_file_written_before_grain_existed_opens_with_grain_off() {
        // The only reading that leaves an existing negative exporting the file it has
        // always exported. Grain absent must not mean "grain at defaults" — the
        // defaults are a real emulsion, and switching one on across every frame in
        // the corpus is not a thing a version bump gets to do.
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|l| !l.contains("monopro:Grain"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !xml.contains("Grain"),
            "the grain block survived being stripped"
        );

        let s = from_xml(&xml)
            .ok()
            .expect("a pre-grain file must still open");
        assert_eq!(s.params.grain, crate::grain::GrainParams::default());
        assert!(
            !s.params.grain.is_active(),
            "an old file must open with grain off"
        );
        assert_eq!(
            s.params.exposure.ev, 1.5,
            "stripping grain lost the exposure"
        );
    }

    #[test]
    fn a_file_written_before_sharpening_existed_opens_with_it_off() {
        // Grain's argument, and it holds for the same reason: the defaults are a real
        // sharpen, and switching one on across every frame in the corpus is not a thing
        // a version bump gets to do. Every file this app has written so far is silent
        // on the subject, and silence has to keep meaning the print it has always
        // described.
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|l| !l.contains("monopro:Sharpen"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !xml.contains("Sharpen"),
            "the sharpening block survived being stripped"
        );

        let s = from_xml(&xml)
            .ok()
            .expect("a pre-sharpening file must still open");
        assert_eq!(s.params.sharpen, crate::sharpen::SharpenParams::default());
        assert!(
            !s.params.sharpen.is_active(),
            "an old file must open with sharpening off"
        );
        assert_eq!(
            s.params.exposure.ev, 1.5,
            "stripping sharpening lost the exposure"
        );
    }

    #[test]
    fn a_hand_edited_sharpen_block_is_clamped_rather_than_believed() {
        // The same supported route in, and the same answer. An amount of 50 is not a
        // sharpen, it is a solarisation, and a radius of 400 would ask for scales this
        // module deliberately does not have.
        let mut p = Params::default();
        p.sharpen.enabled = true;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .replace("SharpenAmount=\"0.75\"", "SharpenAmount=\"50\"")
            .replace("SharpenRadius=\"1\"", "SharpenRadius=\"400\"")
            .replace("SharpenEdges=\"0.5\"", "SharpenEdges=\"-3\"");
        let s = from_xml(&xml).ok().expect("should read").params.sharpen;
        assert_eq!(s.amount, *sharpen::AMOUNT_RANGE.end());
        assert_eq!(s.radius, *sharpen::RADIUS_RANGE.end());
        assert_eq!(s.edges, *sharpen::EDGES_RANGE.start());
    }

    #[test]
    fn a_hand_edited_grain_block_is_clamped_rather_than_believed() {
        // A sidecar is a supported way to reach this app, so it is also a way to ask
        // for four hundred layers of emulsion — which is four minutes an export, on a
        // module whose whole cost model assumes sixty. Everything is clamped on the
        // way in, and the size comes in through the odd-enforcing setter so the panel
        // and the kernel cannot end up disagreeing about a number the file chose.
        //
        // **Every substitution below is checked to have landed**, and that is not
        // belt and braces: the density case originally clamped to the top of the
        // range, which stopped being a test the moment the default became the top of
        // the range — the `replace` would have silently found nothing and the
        // assertion would have passed on the default. It clamps at the bottom now,
        // and `edited` fails loudly if a literal ever stops matching again.
        let mut xml = to_xml(&Params::default(), &Metadata::default(), "t.dng");
        let mut edited = |from: &str, to: &str| {
            assert!(
                xml.contains(from),
                "the writer no longer emits {from} — this test is stale"
            );
            xml = xml.replace(from, to);
        };
        edited("GrainLayers=\"30\"", "GrainLayers=\"400\"");
        edited("GrainSize=\"3\"", "GrainSize=\"8\"");
        edited("GrainDensity=\"0.56\"", "GrainDensity=\"0.0001\"");
        edited("GrainSensitivity=\"0\"", "GrainSensitivity=\"-40\"");

        let g = from_xml(&xml).ok().expect("should read").params.grain;
        assert_eq!(g.layers, *grain::LAYERS_RANGE.end());
        assert_eq!(
            g.size, 9,
            "an even size must arrive snapped, not snapped later"
        );
        assert_eq!(g.density, *grain::DENSITY_RANGE.start());
        assert_eq!(g.sensitivity, *grain::SENSITIVITY_RANGE.start());
    }

    #[test]
    fn the_seed_survives_a_reload_so_two_exports_are_one_picture() {
        // The whole reason the seed is in the file. It is not a setting with a
        // sensible default — it is the identity of this negative's emulsion, and a
        // reload that lost it would silently re-grain a print somebody had already
        // approved.
        let mut p = Params::default();
        p.grain.enabled = true;
        p.grain.seed = 18_446_744_073_709_551_615; // u64::MAX, which an i32 field would eat
        assert_eq!(roundtrip(&p).grain.seed, p.grain.seed);
    }

    #[test]
    fn the_stored_orientation_is_absolute_so_reopening_cannot_rotate_twice() {
        // Why the attribute is `90` and not `+1 turn`. A delta would compose with
        // the EXIF tag again on every open: a file rotated once would come back
        // rotated twice, and again on the next open, which is a corruption that
        // looks like a rendering bug.
        let mut p = Params::default();
        p.composition.orientation = Some(Orientation::Rotate90);
        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        assert!(xml.contains("Orientation=\"90\""), "not absolute:\n{xml}");

        let s = from_xml(&xml).ok().expect("should read");
        // Whatever the file's own tag says, the override wins and is the same turn
        // it was when it was written.
        let o = s.params.composition.orientation;
        assert_eq!(o, Some(Orientation::Rotate90));
        assert_eq!(
            s.params.composition.orientation(Orientation::Rotate270),
            Orientation::Rotate90,
            "the EXIF tag was composed with the override"
        );
    }

    #[test]
    fn as_shot_survives_the_round_trip_as_as_shot() {
        // The distinction that makes the dot honest: "the user has not decided" is
        // not the same value as "the user chose 0°", and a file that confused them
        // would open a portrait frame on its side.
        let p = Params::default();
        let xml = to_xml(&p, &Metadata::default(), "t.dng");
        assert!(xml.contains("Orientation=\"as-shot\""), "{xml}");
        assert_eq!(
            from_xml(&xml)
                .ok()
                .expect("read")
                .params
                .composition
                .orientation,
            None
        );

        let mut q = Params::default();
        q.composition.orientation = Some(Orientation::Rotate0);
        let xml = to_xml(&q, &Metadata::default(), "t.dng");
        let back = from_xml(&xml)
            .ok()
            .expect("read")
            .params
            .composition
            .orientation;
        assert_eq!(
            back,
            Some(Orientation::Rotate0),
            "a deliberate 0° became as-shot"
        );
    }

    #[test]
    fn a_hand_edited_crop_cannot_produce_a_rectangle_that_is_not_one() {
        // The sidecar is a text file a user may edit, and these four numbers become
        // an allocation size and a divisor. Every bad form must land on something
        // renderable rather than on a zero or a NaN.
        for bad in [
            "0.5,0.5,0.9,0.9",   // runs off the frame
            "0.1,0.1,0,0",       // no area
            "-1,-1,3,3",         // outside on both sides
            "0.1,0.1,0.5",       // one number short
            "0.1,0.1,0.5,0.5,9", // one too many
            "nan,0.1,0.5,0.5",
            "not a rectangle",
        ] {
            let xml = to_xml(&Params::default(), &Metadata::default(), "t.dng")
                .replace("Crop=\"0,0,1,1\"", &format!("Crop=\"{bad}\""));
            let s = from_xml(&xml)
                .ok()
                .unwrap_or_else(|| panic!("{bad} stopped the read"));
            let stored = s.params.composition.crop;
            assert!(
                stored.w > 0.0 && stored.h > 0.0,
                "{bad} gave an empty crop: {stored:?}"
            );

            // Asserted on the PLACED rectangle, not on the stored fractions. A
            // straightened crop is entitled to sit outside the unit square —
            // reaching into the corners the rotation emptied is the whole of "drag
            // back out and reclaim them" — so the bound that has to hold is the
            // frame's, and it is `Frame::place` that applies it. These four numbers
            // become an allocation size and a divisor there and nowhere earlier.
            let f = crate::composition::Frame::resolve(
                crate::Dims { w: 600, h: 400 },
                crate::composition::Orientation::Rotate0,
                &s.params.composition,
            );
            let c = f.crop;
            assert!(c.w >= 1 && c.h >= 1, "{bad} gave an empty crop: {c:?}");
            assert!(c.x >= 0 && c.y >= 0, "{bad} gave a negative origin: {c:?}");
            assert!(
                c.x + c.w as i32 <= 600 && c.y + c.h as i32 <= 400,
                "{bad} escaped the frame: {c:?}"
            );
        }
    }

    #[test]
    fn an_out_of_range_straighten_is_clamped_rather_than_believed() {
        // 400 degrees is not a horizon correction, and the bounding box it implies
        // is a frame nobody asked for. The range is enforced on the way in, the same
        // way the mask's ranges are enforced rather than advisory.
        let xml = to_xml(&Params::default(), &Metadata::default(), "t.dng")
            .replace("Straighten=\"0\"", "Straighten=\"400\"");
        let s = from_xml(&xml).ok().expect("should read");
        assert_eq!(s.params.composition.straighten, 45.0);
    }

    #[test]
    fn a_sidecar_written_before_bypass_existed_reads_as_switched_on() {
        // Why the schema version did not move. Every v3 file written before the
        // bypass omits these two attributes and describes a module that ran, so
        // absent must read as on — and a bypassed module's *values* are still
        // there to be read, which is what stops an old file losing its exposure.
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        p.curve.add(0.4, 0.6);
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|l| !l.contains("ExposureEnabled") && !l.contains("CurveEnabled"))
            .collect::<Vec<_>>()
            .join("\n");

        let Loaded::Ok(s) = from_xml(&xml) else {
            panic!("did not read back:\n{xml}")
        };
        assert!(
            s.params.exposure.enabled,
            "an absent bypass must read as on"
        );
        assert!(s.params.curve.enabled);
        assert_eq!(
            s.params.exposure.ev, 1.5,
            "and must not cost the file its values"
        );
        assert!(!s.params.curve.is_identity());
    }

    #[test]
    fn a_sidecar_from_before_display_bypass_opens_display_switched_on() {
        let mut p = Params::default();
        p.display.tone_map = ToneMap::AGX_DEFAULT;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .lines()
            .filter(|line| !line.contains("DisplayEnabled"))
            .collect::<Vec<_>>()
            .join("\n");

        let Loaded::Ok(s) = from_xml(&xml) else {
            panic!("legacy display sidecar did not read:\n{xml}")
        };
        assert!(
            s.params.display.enabled,
            "the absence of the new switch must preserve the old rendered look"
        );
        assert_eq!(s.params.display.tone_map, ToneMap::AGX_DEFAULT);
    }

    #[test]
    fn the_payload_attributes_are_always_present() {
        // The file is self-describing whichever variant is selected, so a reader
        // never has to invent a payload it was not given. Note what this does NOT
        // claim: `Weighting::Green` carries no remembered custom mix, because
        // `Weighted(r,g,b)` *is* the mix — switching away from it discards it in
        // `Params` itself. The sidecar mirrors `Params` exactly and must not
        // pretend to hold state the app does not have.
        let mut p = Params::default();
        p.luminance.weighting = Weighting::Green;
        p.display.tone_map = ToneMap::Clip;
        let xml = to_xml(&p, &Metadata::default(), "t.dng");

        assert!(xml.contains("Weighting=\"green\""));
        assert!(
            xml.contains("WeightingMix=\"0,1,0\""),
            "no mix written:\n{xml}"
        );
        assert!(xml.contains("ToneMap=\"clip\""));
        assert!(
            xml.contains("ShoulderThreshold=\"0.75\""),
            "no shoulder written:\n{xml}"
        );
        assert!(
            xml.contains("AgxWhiteEv=\"6.5\"") && xml.contains("AgxToePower=\"1.5\""),
            "no AgX payload written:\n{xml}"
        );
        // Selecting Demosaic is a separate control from which algorithm, so the
        // algorithm is stored even while another sampling mode is active — that one
        // IS remembered, because `Sampling::Demosaic(a)` keeps it and the UI
        // carries it across a mode change.
        // The shipped default, whatever it is — this test is about the attribute being
        // *present*, not about which mode it names. It said `superpixel` and had to be
        // edited when the default moved, which is the assertion holding the wrong thing.
        assert!(xml.contains(&format!(
            "Sampling=\"{}\"",
            match Params::default().luminance.sampling {
                Sampling::SuperPixel => "superpixel",
                Sampling::DirectMosaic => "directmosaic",
                Sampling::Demosaic(_) => "demosaic",
            }
        )));
        assert!(xml.contains("DemosaicAlgo=\"rcd\""));
    }

    #[test]
    fn a_payload_variant_round_trips_its_own_payload() {
        for w in [Weighting::Weighted(0.6, 0.3, 0.1), Weighting::Photosite] {
            for t in [
                ToneMap::Shoulder {
                    threshold: 0.5,
                    strength: 0.25,
                },
                ToneMap::Agx(AgxParams {
                    auto_range: true,
                    black_ev: -8.5,
                    white_ev: 5.0,
                    contrast: 3.5,
                    toe_power: 2.0,
                    shoulder_power: 4.0,
                }),
            ] {
                let mut p = Params::default();
                p.luminance.weighting = w;
                p.display.tone_map = t;
                assert_eq!(roundtrip(&p), p, "{w:?} + {t:?}");
            }
        }
    }

    #[test]
    fn a_missing_field_leaves_its_default() {
        // The compatibility rule: a file written by an older version must open
        // cleanly, with anything it did not know about at the current default.
        let xml = r#"<?xml version="1.0"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:monopro="http://monopro.app/ns/1.0/"
   monopro:SchemaVersion="3"
   monopro:ExposureEV="2.5"
   />
 </rdf:RDF>
</x:xmpmeta>"#;
        let s = from_xml(xml).ok().expect("should read");
        assert_eq!(s.params.exposure.ev, 2.5);
        assert_eq!(s.params.display.gamma, Params::default().display.gamma);
        assert_eq!(s.params.contrast_mask, ContrastMaskParams::default());
    }

    #[test]
    fn agx_sidecars_from_before_the_controls_keep_the_old_curve() {
        let xml = to_xml(&Params::default(), &Metadata::default(), "t.dng")
            .replace("ToneMap=\"shoulder\"", "ToneMap=\"agx\"")
            .lines()
            .filter(|line| !line.contains("AgxBlackEv") && !line.contains("AgxWhiteEv"))
            .filter(|line| !line.contains("AgxContrast") && !line.contains("AgxToePower"))
            .filter(|line| !line.contains("AgxShoulderPower"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = from_xml(&xml).ok().expect("old AgX sidecar should read");
        assert_eq!(s.params.display.tone_map, ToneMap::AGX_DEFAULT);
    }

    #[test]
    fn one_unparseable_field_does_not_lose_the_others() {
        // A hand-edited file with one bad number must not cost the whole edit.
        let mut p = Params::default();
        p.exposure.ev = 1.5;
        p.display.gamma = 2.4;
        let xml = to_xml(&p, &Metadata::default(), "t.dng")
            .replace("Gamma=\"2.4\"", "Gamma=\"two point four\"");
        let s = from_xml(&xml).ok().expect("should still read");
        assert_eq!(
            s.params.exposure.ev, 1.5,
            "a bad gamma took the exposure with it"
        );
        assert_eq!(s.params.display.gamma, Params::default().display.gamma);
    }

    #[test]
    fn malformed_xml_is_reported_rather_than_swallowed() {
        // Corrupt must be distinguishable from absent: one means "open at
        // defaults", the other means "open at defaults AND tell the user their
        // edits did not load".
        match from_xml("<x:xmpmeta><unclosed>") {
            Loaded::Corrupt(e) => assert!(!e.is_empty()),
            other => panic!("expected Corrupt, got {other:?}"),
        }
        match from_xml("<?xml version=\"1.0\"?><nothing/>") {
            Loaded::Corrupt(e) => assert!(e.contains("Description")),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn a_prototype_sidecar_contributes_exposure_and_nothing_else() {
        // Schema 2 is the Python prototype, which is a different pipeline — its
        // tone chain ran on demosaiced tristimulus. Carrying its curve or tone mode
        // across would look like the settings transferred and render differently.
        // A stop is a stop, so exposure comes over.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:monopro="http://monopro.app/ns/1.0/"
   monopro:SchemaVersion="2"
   monopro:SourceFile="fuji-reference.RAF"
   monopro:DecodeMode="ahd"
   monopro:ExposureEV="1.63"
   monopro:ToneMode="linear"
   monopro:CallierQ="1"
   >
   <monopro:CurvePoints>
    <rdf:Seq>
     <rdf:li>0,0</rdf:li>
     <rdf:li>0.5,0.75</rdf:li>
     <rdf:li>1,1</rdf:li>
    </rdf:Seq>
   </monopro:CurvePoints>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        let s = from_xml(xml).ok().expect("a v2 file must still open");
        assert_eq!(s.schema, 2);
        assert!((s.params.exposure.ev - 1.63).abs() < 1e-6);
        assert!(
            s.params.curve.is_identity(),
            "the prototype's curve was carried into a different tone chain"
        );
        assert_eq!(s.params.luminance.sampling, Sampling::default());
    }

    #[test]
    fn metadata_survives_a_write() {
        // The handoff's reason for dc:/xmp: at all — it travels to other apps, so
        // this app must not destroy it when it saves a develop change.
        let meta = Metadata {
            creator: Some("Example & Co <photo>".into()),
            rights: Some("All rights reserved".into()),
            title: Some("Example scene".into()),
            headline: Some("Winter light".into()),
            description: Some("Example description".into()),
            credit: Some("Example Photographer".into()),
            source: Some("Example archive".into()),
            city: Some("Example City".into()),
            state: Some("Example City".into()),
            country: Some("United States".into()),
            instructions: Some("Contact before publication".into()),
            subject: vec!["architecture".into(), "daylight".into()],
            rating: Some(4),
            // A word this app's own palette does not contain, deliberately: the label
            // is free text in the format and a foreign vocabulary has to round-trip
            // untouched rather than be normalised to one of six.
            label: Some("Zweite Wahl".into()),
            cleared: Vec::new(),
        };
        let xml = to_xml(&Params::default(), &meta, "t.dng");
        let s = from_xml(&xml).ok().expect("should read");
        assert_eq!(s.metadata, meta, "metadata did not survive:\n{xml}");
    }

    #[test]
    fn authored_metadata_overlays_field_by_field_and_can_clear_embedded_text() {
        let embedded = Metadata {
            creator: Some("Embedded creator".into()),
            title: Some("Embedded title".into()),
            description: Some("Embedded caption".into()),
            city: Some("Paris".into()),
            rating: Some(4),
            ..Default::default()
        };
        let mut authored = Metadata::default();
        authored.set_iptc(IptcField::Creator, "Authored creator".into());
        authored.set_iptc(IptcField::Description, String::new());

        let merged = Metadata::merged(&embedded, &authored);
        assert_eq!(merged.creator.as_deref(), Some("Authored creator"));
        assert_eq!(merged.title.as_deref(), Some("Embedded title"));
        assert_eq!(merged.city.as_deref(), Some("Paris"));
        assert_eq!(merged.rating, Some(4));
        assert_eq!(merged.description, None, "the cleared caption came back");
        assert!(
            merged.cleared.is_empty(),
            "internal tombstones leaked outward"
        );

        let xml = to_xml(&Params::default(), &authored, "t.dng");
        let reread = from_xml(&xml).ok().expect("sidecar parses").metadata;
        assert!(reread.cleared.iter().any(|key| key == "description"));
        assert_eq!(Metadata::merged(&embedded, &reread), merged);
    }

    #[test]
    fn the_sidecar_sits_next_to_its_image() {
        assert_eq!(
            path_for(Path::new("/raws/L1000016.DNG")),
            PathBuf::from("/raws/L1000016.mono.xmp")
        );
        // A name with dots must not lose everything after the first one.
        assert_eq!(
            path_for(Path::new("/raws/2026-03-11.frame.RAF")),
            PathBuf::from("/raws/2026-03-11.frame.mono.xmp")
        );
    }

    #[test]
    fn a_corrupt_curve_cannot_build_an_invalid_one() {
        // The sidecar is a text file a user may edit. Points out of order, out of
        // range, or duplicated must not produce a Curve that `eval` cannot handle.
        let xml = to_xml(&Params::default(), &Metadata::default(), "t.dng").replace(
            "<rdf:li>0,0</rdf:li>",
            "<rdf:li>0,0</rdf:li>\n<rdf:li>0.9,0.1</rdf:li>\n<rdf:li>0.2,5</rdf:li>\n\
             <rdf:li>0.2,0.3</rdf:li>",
        );
        let s = from_xml(&xml).ok().expect("should read");
        let pts = s.params.curve.points();
        assert_eq!(pts[0][0], 0.0, "the first point must stay pinned at x=0");
        assert_eq!(
            pts[pts.len() - 1][0],
            1.0,
            "the last point must stay pinned at x=1"
        );
        for w in pts.windows(2) {
            assert!(w[0][0] < w[1][0], "points came back out of order: {pts:?}");
        }
        for p in pts {
            assert!((0.0..=1.0).contains(&p[1]), "a y escaped the window: {p:?}");
        }
        // And it must still evaluate.
        assert!(s.params.curve.eval(0.5).is_finite());
    }
    #[test]
    fn optional_external_sidecars_open() {
        // Optional external fixtures, supplied explicitly by the test runner.
        // No personal directory is inspected by default.
        //
        // **Deliberately does not assert a schema.** These are live files that this
        // app rewrites: opening one and moving a slider upgrades it from 2 to 3,
        // which is the migration working. An earlier version of this test pinned
        // schema 2 and started failing the moment the feature it was testing did
        // its job.
        let Some(dir) = std::env::var_os("MONOPRO_TEST_SIDECARS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut seen = 0;
        for e in entries.flatten() {
            let p = e.path();
            if !p.to_string_lossy().ends_with(".mono.xmp") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            seen += 1;
            match from_xml(&text) {
                Loaded::Ok(s) => {
                    // **`PIPELINE_SCHEMA`, not `SCHEMA_VERSION`** — the two are not the
                    // same question and this line asked the wrong one until output
                    // sharpening moved the version to 8. Below `PIPELINE_SCHEMA` a file
                    // describes the *prototype's* pipeline and contributes exposure
                    // only; between it and `SCHEMA_VERSION` a file describes this app's
                    // pipeline at an older revision and carries a perfectly real curve.
                    // the maintainer's own v7 files are the second kind, and the moment the
                    // version moved past them they started failing a test that meant to
                    // be about the first kind.
                    if s.schema < PIPELINE_SCHEMA {
                        // A prototype file contributes exposure and nothing else.
                        assert!(
                            s.params.curve.is_identity(),
                            "{} carried a v{} curve into a different tone chain",
                            p.display(),
                            s.schema
                        );
                    }
                }
                other => panic!("{} did not open: {other:?}", p.display()),
            }
        }
        eprintln!("checked {seen} real sidecars");
    }

    #[test]
    fn a_real_write_lands_beside_the_image_and_reads_back() {
        // Exercises the atomic write, the path derivation and the reader together —
        // the round-trip tests above all stay in memory.
        let dir = std::env::temp_dir().join(format!("monopro-sc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let image = dir.join("L1000016.DNG");

        let mut p = Params::default();
        p.exposure.ev = 1.625;
        p.luminance.sampling = Sampling::Demosaic(DemosaicAlgo::Amaze);
        p.curve.add(0.4, 0.6);
        let meta = Metadata {
            rating: Some(3),
            ..Default::default()
        };

        write(&image, &p, &meta).expect("write");
        let written = path_for(&image);
        assert!(written.exists(), "no sidecar at {}", written.display());
        // The temporary must not survive the rename.
        assert!(
            !dir.join("L1000016.mono.xmp.tmp").exists(),
            "a .tmp was left behind"
        );

        match read(&image) {
            Loaded::Ok(s) => {
                assert_eq!(s.params, p);
                assert_eq!(s.metadata, meta);
                assert_eq!(s.schema, SCHEMA_VERSION);
            }
            other => panic!("did not read back: {other:?}"),
        }

        if std::env::var("MONOPRO_SHOW_XMP").is_ok() {
            eprintln!("{}", std::fs::read_to_string(&written).expect("read"));
        }

        // An image with no sidecar is Absent, not an error.
        assert_eq!(read(&dir.join("nothing.DNG")), Loaded::Absent);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod toning_persistence {
    use super::*;
    use crate::toning::{Applied, Process, ToningParams};

    fn trip(p: &Params) -> Params {
        let xml = to_xml(p, &Metadata::default(), "test.dng");
        match from_xml(&xml) {
            Loaded::Ok(s) => s.params,
            other => panic!("did not read back: {other:?}"),
        }
    }

    fn loaded() -> ToningParams {
        ToningParams {
            enabled: true,
            process: Process::Kallitype,
            tone: 1.4,
            hue: -22.0,
            mix: 0.35,
            pigment: 0.7,
            contrast: 1.35,
            chemistry_enabled: false,
            applied: vec![
                Applied {
                    key: "gold-gp1",
                    amount: 0.8,
                },
                Applied {
                    key: "palladium",
                    amount: 0.45,
                },
            ],
            placement: crate::curve::Curve::from_points(&[[0.0, 1.0], [0.4, 0.3], [1.0, 0.9]])
                .unwrap(),
            placement_enabled: false,
        }
    }

    /// Every field, every treatment, one value each and all different from the default —
    /// so a field the writer forgot shows up as a mismatch rather than a coincidence.
    #[test]
    fn the_whole_process_round_trips() {
        let p = Params {
            toning: loaded(),
            ..Params::default()
        };
        assert_eq!(trip(&p).toning, p.toning);
    }

    /// **Treatments are matched by key, not by position.** A process's list is written
    /// to read well and will be reordered; a positional encoding would silently apply
    /// the wrong chemistry to every file written before the reorder.
    #[test]
    fn a_treatment_is_matched_by_name() {
        let p = Params {
            toning: ToningParams {
                enabled: true,
                applied: vec![Applied {
                    key: "sepia",
                    amount: 0.9,
                }],
                ..Default::default()
            },
            ..Params::default()
        };
        let xml = to_xml(&p, &Metadata::default(), "test.dng");
        assert!(xml.contains("monopro:Key=\"sepia\""), "{xml}");
        assert_eq!(trip(&p).toning.amount("sepia"), 0.9);
    }

    /// A file with no toning block opens untoned, which is what it is.
    #[test]
    fn a_file_with_no_toning_block_opens_untoned() {
        let p = Params::default();
        let xml = to_xml(&p, &Metadata::default(), "test.dng");
        assert!(
            !xml.contains("monopro:Toning"),
            "an untoned file should carry no block"
        );
        assert_eq!(trip(&p).toning, ToningParams::default());
    }

    /// Sidecars written before the submodule switches existed had both paths always
    /// engaged. Missing attributes must retain that meaning.
    #[test]
    fn an_older_toning_block_keeps_chemistry_and_placement_engaged() {
        let p = Params {
            toning: loaded(),
            ..Params::default()
        };
        let xml = to_xml(&p, &Metadata::default(), "test.dng")
            .replace(" monopro:ChemistryEnabled=\"false\"", "")
            .replace(" monopro:PlacementEnabled=\"false\"", "");
        let Loaded::Ok(s) = from_xml(&xml) else {
            panic!("should still load")
        };
        assert!(s.params.toning.chemistry_enabled);
        assert!(s.params.toning.placement_enabled);
    }

    /// A treatment the process does not offer is **dropped, not guessed**. Every other
    /// field is a number in a range with a sensible fallback; a treatment is the whole
    /// of what was done, so inventing one puts chemistry in the picture nobody asked
    /// for. Dropping is wrong in the direction of doing less.
    #[test]
    fn a_treatment_the_process_cannot_run_is_dropped() {
        let p = Params {
            toning: ToningParams {
                enabled: true,
                process: Process::GelatinSilver,
                applied: vec![Applied {
                    key: "selenium",
                    amount: 0.7,
                }],
                ..Default::default()
            },
            ..Params::default()
        };
        // Same file, read as a cyanotype — which has no silver for selenium to reach.
        let xml = to_xml(&p, &Metadata::default(), "test.dng")
            .replace("\"gelatin-silver\"", "\"cyanotype\"");
        let Loaded::Ok(s) = from_xml(&xml) else {
            panic!("should still load")
        };
        assert_eq!(s.params.toning.process, Process::Cyanotype);
        assert!(
            s.params.toning.applied.is_empty(),
            "{:?}",
            s.params.toning.applied
        );
    }

    /// An unrecognised *process* falls back to gelatin silver rather than refusing the
    /// file — the opposite call to the treatment above, and deliberately: a process is a
    /// paper, the picture is still the picture, and showing it on the default paper
    /// beats showing none.
    #[test]
    fn an_unknown_process_falls_back_to_the_default_paper() {
        let p = Params {
            toning: ToningParams {
                enabled: true,
                process: Process::Cyanotype,
                ..Default::default()
            },
            ..Params::default()
        };
        let xml =
            to_xml(&p, &Metadata::default(), "test.dng").replace("\"cyanotype\"", "\"wothlytype\"");
        let Loaded::Ok(s) = from_xml(&xml) else {
            panic!("should still load")
        };
        assert_eq!(s.params.toning.process, Process::GelatinSilver);
        assert!(
            s.params.toning.enabled,
            "and the rest of the block still applies"
        );
    }

    /// Toning still requires schema 9 or later: a v8 reader shows a neutral print
    /// where a toned one was intended. Later features may move the current number.
    #[test]
    fn the_schema_version_moved_with_the_block() {
        let p = Params {
            toning: loaded(),
            ..Params::default()
        };
        let xml = to_xml(&p, &Metadata::default(), "test.dng");
        assert!(
            xml.contains(&format!("monopro:SchemaVersion=\"{SCHEMA_VERSION}\"")),
            "{xml}"
        );
    }
}
