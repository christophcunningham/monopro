//! Visual theme: dark grey chrome, rubylith accent, one typewriter face.
//!
//! The accent is sampled from rubylith masking film — modal `#CB2E2D` over the lit
//! sheet, mean `#C93331`. It is the one saturated colour in the app and is spent
//! only on interaction state (selection, focus, active control points). Nothing
//! that describes image data is tinted with it: histograms, the curve trace, and
//! the image surround stay neutral, because a colour cast next to a monochrome
//! rendering shifts how its tonality reads.
//!
//! The image surround is deliberately NOT part of this theme. It is a shader
//! constant (`SURROUND` in display.wgsl) for the same reason — perceived tonality
//! of a monochrome print depends on its surround, so it is a fixed mid-grey rather
//! than something a user can tint.

use egui::Color32;

/// Rubylith. Sampled, not guessed.
pub const RUBY: Color32 = Color32::from_rgb(0xCB, 0x2E, 0x2D);

/// The five colour labels, in their UI and shortcut order, and their names.
///
/// Blue and yellow retain the prototype's values (`monopro.py:21720`); magenta is
/// lighter, green slightly brighter and red more vivid by design. Cyan was retired
/// from the UI in favour of a five-label run.
///
/// These earn it on a different argument from ruby's. Ruby is the app's *voice*: it
/// means "this is the state you are in", and it is the only thing that speaks. A
/// colour label is not the app speaking at all — it is the user's own mark, and its
/// meaning is whatever they decided it is. Muting them to fit the palette would be
/// the app editing somebody's filing system to match its own taste, and a label you
/// have to look twice at has failed at the one thing it does.
///
/// They are also already restrained: every one is a mid-value, slightly desaturated
/// version of its hue rather than a primary, which is why five of them sit together
/// over a grid of photographs without shouting. And they appear only as a small dot
/// on a tile — never as a ground, a rule or text.
///
/// **The names are the wire format.** They go into `xmp:Label` as written, so they
/// are lowercase English words another application can read — see
/// `raw_core::sidecar::Metadata::label`.
pub const LABELS: [(&str, Color32); 5] = [
    ("magenta", Color32::from_rgb(0xB8, 0x55, 0xB8)),
    ("blue", Color32::from_rgb(0x2F, 0x70, 0xBA)),
    ("green", Color32::from_rgb(0x16, 0x8B, 0x58)),
    ("yellow", Color32::from_rgb(0xBA, 0xBB, 0x00)),
    ("red", Color32::from_rgb(0xC4, 0x47, 0x3F)),
];

/// The colour for a label name, or `None` when another application wrote it.
///
/// An unknown word is not an error and does not get a fallback colour: it is somebody
/// else's vocabulary, and the honest thing is to show no dot rather than to guess
/// which of five it ought to be.
pub fn label_colour(name: &str) -> Option<Color32> {
    LABELS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// Ruby as a **ground** rather than as a mark: the two fills behind a wide action
/// button, and the text that sits on them.
///
/// the maintainer's, taken from the export module's format chip — a dark maroon ground carrying
/// red text, rather than the light-on-saturated pair the Dodge & Burn bench uses. The
/// difference is what the two are *for*: `+ DODGE` makes a thing and wants to be the
/// loudest control in its panel, where these two sit at the foot of a list and should
/// not out-shout the pictures above them.
///
/// **The pair ranks rather than competes.** `RUBY_FILL` is a touch more vibrant than
/// `RUBY_FILL_DIM` and no more, because capture is what the snapshot panel is *for* and
/// compare is what you do with what you captured — a difference in emphasis, not in
/// kind. Two grounds of equal weight would make you choose between them.
///
/// The text is [`RUBY`] on both, which is the one place in the app where the accent is
/// used as *text on its own ground*; it works because both grounds are far below it in
/// value, which is the same reason `DODGE_FILL` can carry `DODGE`.
pub const RUBY_FILL: Color32 = Color32::from_rgb(0x4A, 0x25, 0x23);
pub const RUBY_FILL_DIM: Color32 = Color32::from_rgb(0x38, 0x20, 0x1F);

/// The ground under a **selected chip**: `PNG`/`JPEG`, `16-bit`/`8-bit`, `Freehand`,
/// `view mask`, every small either/or in the app.
///
/// the maintainer asked for the Settings window's chip style across the Develop panel, and this
/// is that chip's ground stated as a colour. Settings gets it from
/// `Visuals::selection.bg_fill`, which is `RUBY` at 18% — **translucent**, so what you
/// actually see is ruby composited over [`CHROME`]. This is that composite, computed
/// once and stored opaque.
///
/// **Opaque rather than the factor, deliberately**, and it is the lesson [`DODGE_FILL`]
/// records applied to a second control: a translucent fill is the panel showing through
/// a tint, so its apparent colour changes with whatever it is drawn over — a chip inside
/// a module box, a chip on the title strip and a chip over an image would be three
/// different reds. Stored flat, a chip is the same chip everywhere.
///
/// **It is not `selection.bg_fill` itself, and the two are kept apart on purpose.**
/// That one has a second job — the highlight behind selected text in a `DragValue`
/// while you are retyping it — where translucency is the requirement rather than a
/// side effect, and it is tuned for it. One value serving both would mean tuning the
/// chip changed how a number reads mid-edit.
pub const RUBY_GROUND: Color32 = Color32::from_rgb(0x44, 0x27, 0x27);

/// The grain loupe's ring and its caption. The prototype's amber, transcribed. While
/// the loupe shows Before — the crop without grain and sharpening — its strokes turn
/// [`BRIGHT`] instead, so the two crops cannot be confused.
///
/// **Not [`RUBY`], deliberately.** Ruby means "something is on that would not be on
/// by default" — a mode, an overlay, a modified module — and it is the colour every
/// mark that changes the picture is drawn in. The loupe changes nothing: it is a
/// window onto pixels that already exist, and giving it ruby would put it in the same
/// visual class as a burn. Amber also survives being drawn over a blown sky and a
/// blocked shadow, which is the whole job of a reticle.
pub const AMBER: Color32 = Color32::from_rgb(0xC8, 0xA8, 0x4B);

/// The two CIELAB chroma axes, for the footer and the value pins.
///
/// **Lab's own directions, not a decorative pair.** `+a*` runs toward magenta and `+b*`
/// toward yellow, so the ink says which way a number points before it has been read —
/// and a negative `a*` in magenta ink still reads correctly as *away* from magenta,
/// because the axis is what is being named rather than the value.
///
/// They are the only saturated colours in the app besides ruby, and they earn it by
/// being the two readouts that are meaningless without a direction. Muted well below
/// full strength: this is a footer, not a warning.
pub const LAB_A: Color32 = Color32::from_rgb(0xC9, 0x8A, 0xC0);
pub const LAB_B: Color32 = Color32::from_rgb(0xD2, 0xC4, 0x72);

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

/// **One face for the whole app: JetBrains Mono.** the maintainer's decision.
///
/// Both egui families point at it, so `ui.label` and `ui.monospace` keep working and
/// no call site changes.
///
/// **It has to be fixed-pitch.** A face that kerns `_` against a letter renders a name
/// like `L1000016_dup1` with its characters piled up, and that shows in the tab strip
/// and nowhere else — `the_underscore_defect_is_gone_rather_than_avoided` is the guard.
///
/// SIL OFL 1.1, `fonts/JetBrainsMono/OFL.txt`, which permits embedding outright.
const UI_FACE: &[u8] = include_bytes!("../../../fonts/JetBrainsMono/JetBrainsMono-Regular.ttf");
/// Module titles use the same family at SemiBold. Kept as its own egui family so
/// control labels and readouts stay Regular.
const MODULE_FACE: &[u8] =
    include_bytes!("../../../fonts/JetBrainsMono/JetBrainsMono-SemiBold.ttf");

/// The type scale. Four primary sizes plus the footer's explicit one-point reductions;
/// adding another still needs an argument.
///
/// Sizes used to be picked at each call site — 9.0 for hints, 10.0 for section
/// headers, 11 and 12 in the title strip, egui's defaults everywhere else — which
/// is why the UI read as though nothing agreed with anything.
///
/// The hierarchy is the one the New Typography letterheads use, and it is worth
/// stating because it is not the obvious one: **the labels are smaller and quieter
/// than the data they label**. A module name set at 11 in dim, letter-spaced caps
/// sits *under* a 12pt slider title in the visual order, and that is correct — the
/// name is a field marker, the control is the content. Alignment and the hairline
/// containers do the work that a bigger, bolder heading would otherwise be asked to
/// do.
pub mod size {
    /// The one large thing: the app name in the title strip. Nothing else.
    pub const TITLE: f32 = 13.0;
    /// Controls — slider titles, combo entries, buttons, checkboxes. Regular case.
    pub const BODY: f32 = 12.0;
    /// Slider-row labels and their editable numeric readouts. Kept together so the
    /// two halves of one control cannot drift to different visual scales.
    pub const SLIDER: f32 = BODY - 1.0;
    /// Field markers — module names and section headers. **All caps**, letter-spaced,
    /// dim.
    ///
    /// Above `BODY` rather than below it, which reverses the letterhead reading this
    /// scale started from. The letterheads are right about *printed* fields, where
    /// the eye arrives at the top and reads down; a develop panel is scrolled and
    /// scanned, and the module name is the thing you are hunting for. the maintainer called
    /// it, and in use it is obviously correct — the names are landmarks, not
    /// captions.
    pub const HEADER: f32 = 12.5;
    /// The dim explanatory notes under a control, and anything hinting at an
    /// interaction rather than reporting a value.
    pub const CAPTION: f32 = 10.0;

    /// Footer readouts and labels. One point below body text so the bar remains
    /// subordinate to the working panels above it.
    pub const FOOTER: f32 = BODY - 1.0;
    /// Footer notes and compact state labels, one point below ordinary captions.
    pub const FOOTER_CAPTION: f32 = CAPTION - 1.0;

    /// The footer's icon buttons, and what the bar's height is built from: the mode
    /// tabs are this plus two, and the bar is those plus its frame. 21 — the app's
    /// `icons::BIG` control size — since the maintainer found the bar too thin at 18,
    /// where it came out 24 points tall; it is now 27.
    pub const FOOTER_ICON: f32 = 21.0;

    /// Section headings *inside* a module — `SHAPE`, `LAYERS`, and the `DODGE+` /
    /// `BURN+` pair that reads as one.
    ///
    /// **13pt at 1.15pt tracking**, taken from the maintainer's mockup as measured rather
    /// than eyeballed: the `.rtf` carries `\fs26` and `\expndtw23`, which are
    /// half-points and twips. Above `HEADER`, which is the module *title* — the
    /// title is set once at the top of a box and these divide the box up, so they
    /// have to survive being scanned past.
    pub const SECTION: f32 = 13.0;

    /// Letter-spacing for [`SECTION`], in points.
    ///
    /// egui has no tracking on `RichText`, so this is applied by `theme::tracked`,
    /// which lays the glyphs out itself. Worth the machinery: at 13pt an all-caps
    /// heading with no tracking reads as a word, and with it reads as a field
    /// marker, which is the whole distinction the type scale is built on.
    pub const SECTION_TRACK: f32 = 1.15;

    /// Reset buttons. Below `CAPTION`: a reset is the quietest thing in a module
    /// header and should have to be looked for.
    pub const RESET: f32 = 9.0;

    /// Tooltips.
    ///
    /// Every hint in the app goes through [`super::tip`] to get this. egui has no
    /// tooltip text style to override — a tooltip is a `Ui` drawing `Body` like any
    /// other — so the size has to be carried by the text itself, and a helper is
    /// the only way it stays consistent as hints are added.
    pub const TIP: f32 = 10.0;
}

/// Install both faces and bind the scale to egui's text styles.
///
/// Ours go in at the **front** of each family with egui's defaults left behind
/// them, so a glyph neither typewriter face has — `⧉`, `·`, an arrow, an emoji —
/// still renders instead of showing as tofu.
fn fonts(ctx: &egui::Context) {
    use std::sync::Arc;

    let mut f = egui::FontDefinitions::default();
    f.font_data
        .insert("ui".into(), Arc::new(egui::FontData::from_static(UI_FACE)));
    f.font_data.insert(
        "module".into(),
        Arc::new(egui::FontData::from_static(MODULE_FACE)),
    );
    // Both families, one face. `readout` still exists as a name so `theme::readout`
    // and every `ui.monospace` call site keep working — they now mean "a number" by
    // where they sit and how they are coloured rather than by what they are set in.
    f.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "ui".into());
    f.families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "ui".into());
    f.families
        .entry(egui::FontFamily::Name("module".into()))
        .or_default()
        .insert(0, "module".into());
    ctx.set_fonts(f);

    // `all_styles_mut`, not `style_mut`: egui keeps a light and a dark `Style` and
    // the bare setter writes only the active one. That is the same trap
    // `set_visuals_of` is guarding against below, and it fails the same way — on a
    // light-mode Mac the scale would land in a slot nothing reads.
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(size::TITLE, Proportional)),
            (TextStyle::Body, FontId::new(size::BODY, Proportional)),
            (TextStyle::Button, FontId::new(size::BODY, Proportional)),
            (TextStyle::Small, FontId::new(size::CAPTION, Proportional)),
            (TextStyle::Monospace, FontId::new(size::BODY, Monospace)),
        ]
        .into();
    });
}

/// A caption: the dim explanatory note that sits under a control.
///
/// Every one of these was `RichText::new(..).size(9.0).color(from_gray(96))`
/// written out by hand, which is how three different greys ended up doing one job.
pub fn caption(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text)
        .size(size::CAPTION)
        .color(Color32::from_gray(112))
}

/// **The name of a control**: body size, [`DIM`]. What every slider row's label is set
/// in, and now what every other control's is too.
///
/// This exists because half the rule was already written down and the other half was
/// not. The written half: *a label for a control is body; caption is for
/// explanatory prose*, and Composition's Ratio and Guides and Output's Resolution, Size
/// and Colour space were duly moved off `caption` — onto a bare `ui.label`, which takes
/// egui's `noninteractive` grey at **180**. A checkbox label takes `inactive` at **200**,
/// and **235** while the pointer is over it. Against `DIM` at 122 those are white, and
/// the maintainer read eight controls across three modules as shouting.
///
/// So the rule needed a colour in it as well as a size, and a helper is the only form a
/// rule survives in — the size alone was a rule that had to be remembered, and it was
/// remembered for five call sites and not for the greys they landed on.
///
/// A control's *state* is never carried by this. The dot, the ruby tick and the outline
/// each say "on"; the name of the thing stays the name of the thing.
pub fn label(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(size::BODY).color(DIM)
}

/// Anything the app is reporting back rather than naming: filenames, paths,
/// dimensions, EXIF, the footer. Set in the readout face.
///
/// Also the only safe way to set a filename — see the note on `UI_FACE`.
pub fn readout(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Monospace)
}

/// A footer readout, kept separate from [`readout`] so reducing the status bar does
/// not reduce filenames and measurements everywhere else.
pub fn footer_readout(text: impl Into<String>) -> egui::RichText {
    readout(text).size(size::FOOTER)
}

/// A footer note or label, one point below the ordinary caption scale.
pub fn footer_caption(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text)
        .size(size::FOOTER_CAPTION)
        .color(Color32::from_gray(112))
}

/// A named footer control or path, reduced with the rest of the status bar.
pub fn footer_label(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(size::FOOTER).color(DIM)
}

/// The small reset that sits at the right of a module header.
///
/// `label` because the panel-wide one says **"reset all"** — at the top of a column
/// of buttons that each say "reset", an identical word on the outermost one would
/// be the most destructive control in the panel wearing the same face as the least.
pub fn reset_button(ui: &mut egui::Ui, label: &str, hint: &str) -> egui::Response {
    ui.add(egui::Button::new(
        egui::RichText::new(label).size(size::RESET).color(DIM),
    ))
    .on_hover_text(tip(hint))
}

/// Panel and window chrome.
pub const CHROME: Color32 = Color32::from_gray(38);
/// The title strip, one step darker so the window edge reads as an edge.
pub const CHROME_DEEP: Color32 = Color32::from_gray(30);
/// The viewer background as egui sees it.
///
/// The image sits on a canvas drawn in two halves: this letterbox, and the fill
/// `display.wgsl` writes outside the image. They meet at the frame edge, so a
/// difference between them is a visible seam.
///
/// They used to be two hand-matched constants — `theme::SURROUND` at gray 23 and
/// `0.09` in the shader. **They are now one number**, the setting, passed to both;
/// the seam is impossible rather than merely avoided, and the constant that had to
/// be kept in step is gone.
pub fn background_of(display_encoded: f32) -> Color32 {
    let v = (display_encoded.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_gray(v)
}
/// The module ground the user has chosen, as a grey level. [`CHROME`] until the
/// settings are applied.
///
/// A static rather than a parameter because every module frame reads it and none of
/// them is handed the settings; the floating panels are separate viewports of the same
/// process and want the same value.
static MODULE_GROUND: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(38);

pub fn set_module_ground(display_encoded: f32) {
    MODULE_GROUND.store(
        background_of(display_encoded).r(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn module_ground() -> u8 {
    MODULE_GROUND.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where a grey drawn for a [`CHROME`] module lands on a module of grey `ground`.
///
/// **Every grey keeps its distance from the ground; on a light ground, the sign flips.**
/// The module's whole palette — label greys, wells, widget fills, strokes — was tuned as
/// offsets from `CHROME`, so moving the ground and keeping the offsets keeps the
/// hierarchy, and flipping them past mid-grey is what keeps the text readable when the
/// ground is lighter than the text was. Bright hues (ruby, the labels, the dodge/burn
/// pair) are inks and are left alone; dark tints are grounds and move — see below.
#[cfg(test)]
pub fn regrey(ground: u8, c: Color32) -> Color32 {
    Ground::module(ground).apply(c)
}

/// A re-grey: colours designed against grey `design`, moved to grey `to`.
///
/// Develop's modules were designed on [`CHROME`]; Lightbox's panels on
/// [`CHROME_DEEP`] and its grid on `CHROME`. The rule is the same for all of them —
/// only the grey they were drawn against differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ground {
    pub design: u8,
    pub to: u8,
}

impl Ground {
    pub fn module(to: u8) -> Self {
        Self {
            design: CHROME.r(),
            to,
        }
    }

    fn is_identity(self) -> bool {
        self.design == self.to
    }

    pub fn apply(self, c: Color32) -> Color32 {
        regrey_in(self, c)
    }
}

fn regrey_in(ground: Ground, c: Color32) -> Color32 {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    if a == 0 {
        return c;
    }
    if is_grey(c) {
        let v = move_value(ground, (r as i32 + g as i32 + b as i32) / 3);
        return Color32::from_rgba_unmultiplied(v, v, v, a);
    }
    // **A dark tint is a ground, not an ink.** `RUBY_FILL` behind the export buttons,
    // `RUBY_GROUND` behind a selected chip: each is the module grey with a little hue
    // in it, and ruby text is written on top. Left dark on a light card, that is ruby
    // on maroon inside white, which is unreadable. So its value moves the way a grey
    // would and its hue — its offset from its own mean — rides along. Bright hues are
    // inks and stay exactly as they are.
    //
    // **A pale hue on a light card is an ink that has lost its ground.** The dodge
    // and burn inks were made light to read on near-black; on white they vanish. On
    // a light ground they are mirrored the way a grey is, keeping their hue. Ruby is
    // mid-value and reads on either, so it never reaches this branch.
    let mean = (r as i32 + g as i32 + b as i32) / 3;
    if r.max(g).max(b) < 128 || (ground.to > 127 && mean > 150) {
        let to = move_value(ground, mean) as i32;
        let ch = |k: u8| (to + k as i32 - mean).clamp(0, 255) as u8;
        return Color32::from_rgba_unmultiplied(ch(r), ch(g), ch(b), a);
    }
    c
}

/// A grey level drawn against `ground.design`, moved to the same distance from
/// `ground.to` — on the other side of it when the new ground is light.
fn move_value(ground: Ground, v: i32) -> u8 {
    let d = v - ground.design as i32;
    let d = if ground.to > 127 { -d } else { d };
    (ground.to as i32 + d).clamp(0, 255) as u8
}

/// Visible and without a hue — a colour [`regrey`] moves.
fn is_grey(c: Color32) -> bool {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    a > 0 && r.max(g).max(b) - r.min(g).min(b) <= 8
}

/// Run `add` and re-grey whatever it painted for the module ground. What a module
/// card is drawn inside; anything else built on `CHROME` that should read as a
/// module — the Dodge & Burn bench — uses it too.
pub fn module_ground_ui<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    reground_ui(ui, Ground::module(module_ground()), add)
}

/// Run `add` and re-grey whatever it painted from `ground.design` to `ground.to`.
/// Shapes painted inside [`true_colour`] are left as they are.
pub fn reground_ui<R>(
    ui: &mut egui::Ui,
    ground: Ground,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let layer = ui.layer_id();
    let next = |ui: &egui::Ui, or: egui::layers::ShapeIdx| {
        ui.ctx()
            .graphics(|g| g.get(layer).map_or(or, |l| l.next_idx()))
    };
    let start = next(ui, egui::layers::ShapeIdx(0));
    let out = add(ui);
    let end = next(ui, start);
    regrey_shapes(ui.ctx(), layer, start, end, ground);
    out
}

/// Paint whose colours are **the picture's, not the chrome's** — a tone ramp, a zone
/// strip, a toned swatch, a mount colour — and must reach the screen as given.
///
/// [`module_ground_ui`] re-greys everything a module paints, which is right for text
/// and widgets and wrong for these: a grey ramp that says "this is print value 0.8"
/// would come out inverted on a light card, and a toning swatch would change colour.
/// Shapes painted inside `paint` are recorded and skipped.
pub fn true_colour<R>(ui: &egui::Ui, paint: impl FnOnce() -> R) -> R {
    let layer = ui.layer_id();
    let next = || {
        ui.ctx()
            .graphics(|g| g.get(layer).map_or(0, |l| l.next_idx().0))
    };
    let start = next();
    let out = paint();
    let end = next();
    let pass = ui.ctx().cumulative_pass_nr();
    ui.ctx().data_mut(|d| {
        let list = d.get_temp_mut_or_default::<TrueColour>(egui::Id::new(TRUE_COLOUR));
        if list.pass != pass {
            *list = TrueColour {
                pass,
                ranges: Vec::new(),
            };
        }
        list.ranges.push((layer, start, end));
    });
    out
}

const TRUE_COLOUR: &str = "theme-true-colour";

/// This pass's [`true_colour`] ranges. Keyed by pass so a range from a frame that
/// painted it outside any module cannot exempt a shape in a later frame.
#[derive(Clone, Default)]
struct TrueColour {
    pass: u64,
    ranges: Vec<(egui::LayerId, usize, usize)>,
}

/// Re-grey shapes `start..end` of `layer`. A no-op when the ground has not moved,
/// which is the case the colours were written for.
fn regrey_shapes(
    ctx: &egui::Context,
    layer: egui::LayerId,
    start: egui::layers::ShapeIdx,
    end: egui::layers::ShapeIdx,
    ground: Ground,
) {
    if ground.is_identity() {
        return;
    }
    let pass = ctx.cumulative_pass_nr();
    let exempt: Vec<(usize, usize)> = ctx.data(|d| {
        d.get_temp::<TrueColour>(egui::Id::new(TRUE_COLOUR))
            .filter(|t| t.pass == pass)
            .map(|t| {
                t.ranges
                    .iter()
                    .filter(|(l, _, _)| *l == layer)
                    .map(|(_, a, b)| (*a, *b))
                    .collect()
            })
            .unwrap_or_default()
    });
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for i in start.0..end.0 {
            if exempt.iter().any(|(a, b)| (*a..*b).contains(&i)) {
                continue;
            }
            list.mutate_shape(egui::layers::ShapeIdx(i), |c| {
                regrey_shape(ground, &mut c.shape)
            });
        }
    });
}

fn regrey_shape(ground: Ground, shape: &mut egui::Shape) {
    use egui::epaint::{ColorMode, Shape};
    let path_stroke = |s: &mut egui::epaint::PathStroke| {
        if let ColorMode::Solid(c) = &mut s.color {
            *c = regrey_in(ground, *c);
        }
    };
    match shape {
        Shape::Noop | Shape::Callback(_) => {}
        Shape::Vec(v) => v.iter_mut().for_each(|s| regrey_shape(ground, s)),
        Shape::Circle(c) => {
            c.fill = regrey_in(ground, c.fill);
            c.stroke.color = regrey_in(ground, c.stroke.color);
        }
        Shape::Ellipse(e) => {
            e.fill = regrey_in(ground, e.fill);
            e.stroke.color = regrey_in(ground, e.stroke.color);
        }
        Shape::LineSegment { stroke, .. } => stroke.color = regrey_in(ground, stroke.color),
        Shape::Path(p) => {
            p.fill = regrey_in(ground, p.fill);
            path_stroke(&mut p.stroke);
        }
        Shape::Rect(r) => {
            // **egui paints an image as a rect with a texture brush**, its fill being
            // the tint. Tinted pure white it is a picture — a thumbnail, the loupe —
            // and its pixels are image data; re-greying the white tint is what turned
            // every thumbnail black once the canvas passed mid-grey. A tinted icon
            // still follows the ground.
            let picture = r.brush.is_some() && r.fill == Color32::WHITE;
            if !picture {
                r.fill = regrey_in(ground, r.fill);
            }
            r.stroke.color = regrey_in(ground, r.stroke.color);
        }
        Shape::QuadraticBezier(b) => {
            b.fill = regrey_in(ground, b.fill);
            path_stroke(&mut b.stroke);
        }
        Shape::CubicBezier(b) => {
            b.fill = regrey_in(ground, b.fill);
            path_stroke(&mut b.stroke);
        }
        Shape::Text(t) => {
            // A galley's colours are baked into its mesh, so the only lever is the
            // override, which recolours every glyph. Taken only when every run is a
            // grey (or the fallback): a line with a ruby word in it keeps its colours.
            let fallback = t.fallback_color;
            let mut runs = t.galley.job.sections.iter().map(|s| {
                if s.format.color == Color32::PLACEHOLDER {
                    fallback
                } else {
                    s.format.color
                }
            });
            let ink = t.override_text_color.or_else(|| runs.next());
            if let Some(ink) = ink
                && is_grey(ink)
                && (t.override_text_color.is_some() || runs.all(is_grey))
            {
                t.override_text_color = Some(regrey_in(ground, ink));
            }
            t.underline.color = regrey_in(ground, t.underline.color);
        }
        Shape::Mesh(m) => {
            // A textured mesh tinted pure white is a picture — the print loupe — and
            // its pixels are image data. Anything else textured is a tinted icon.
            let picture = m.texture_id != egui::TextureId::default()
                && m.vertices.iter().all(|v| v.color == Color32::WHITE);
            if !picture {
                let m = std::sync::Arc::make_mut(m);
                for v in &mut m.vertices {
                    v.color = regrey_in(ground, v.color);
                }
            }
        }
    }
}

/// Dim label grey for section headers.
pub const DIM: Color32 = Color32::from_gray(122);

/// The two Dodge & Burn hues, for **panel text only**.
///
/// Light and desaturated so they read as small text on a near-black ground; the
/// mockup's darker, saturated pair read as muddy.
///
/// **The one exception to ruby being the app's only saturated colour, and it has a
/// boundary: panel text, nothing on the image.** A layer's kind is a fact about a list
/// entry and a list is already chrome; the brush cursor sits *on* the print while you
/// are judging its tonality, so it stays light-and-dark.
///
/// Dodge is the blue and burn the red — the darkroom's pairing: a dodge holds light
/// back and a burn adds it.
pub const DODGE: Color32 = Color32::from_rgb(0x91, 0xE3, 0xFC);
pub const BURN: Color32 = Color32::from_rgb(0xFA, 0x88, 0x82);

/// The `+ DODGE` and `+ BURN` buttons' **fill**, at rest.
///
/// **Opaque and picked, not `gamma_multiply` of the ink.** A translucent half carries
/// alpha with it, so the button was the panel showing through a tint and its colour
/// depended on what it sat over.
///
/// **They are not one factor off the ink, and that is the point.** Against `DODGE` and
/// `BURN` these land at about **0.48** and **0.39** — the burn ground is proportionally
/// darker, which gives that button the contrast the blue one gets for free from a paler
/// ink. Two hues judged separately, so they are stored as given rather than
/// reconstructed from an average that would be wrong for both.
///
/// The text on them is simply the layer hue, since these grounds sit well below it.
pub const DODGE_FILL: Color32 = Color32::from_rgb(0x48, 0x69, 0x77);
pub const BURN_FILL: Color32 = Color32::from_rgb(0x6C, 0x33, 0x2F);

/// The brightest text on the panel: a selected layer's name, and the label on a
/// filled button.
///
/// Not white. `#EEEEEE` is the maintainer's value and it is what "selected" is set in
/// throughout the Dodge & Burn panel — the shape brackets, the layer names. Pure
/// white would be the only thing on the panel brighter than the picture's own
/// highlights, which is a claim no piece of chrome should make.
pub const BRIGHT: Color32 = Color32::from_rgb(0xEE, 0xEE, 0xEE);

/// The grey a layer name is set in when it is **not** selected.
///
/// the maintainer asked for unselected names to sit "a little more grayed" against the
/// selected `BRIGHT`, so this dropped from 180 to 150: far enough that the pair reads
/// as two states at a glance, close enough that an unselected row is still a name
/// rather than a disabled control.
pub const NAME: Color32 = Color32::from_gray(150);

/// An all-caps section heading, letter-spaced.
///
/// **egui cannot track text**, so this lays the glyphs out one at a time and pads
/// between them. That is why it is a function that draws rather than a `RichText`
/// that describes: the alternative is inserting hair spaces into the string, which
/// lies to the accessibility tree and breaks the moment a heading contains a space
/// of its own.
///
/// Returns the response, so a heading can carry a click.
pub fn tracked(ui: &mut egui::Ui, label: &str, colour: Color32) -> egui::Response {
    tracked_at(ui, label, colour, size::SECTION)
}

/// A locally smaller section heading while retaining the same letter spacing.
/// Curve's instance list is subordinate to its graph, so it uses this instead of
/// changing the shared scale used by SHAPE and LAYERS throughout Develop.
pub fn tracked_at(
    ui: &mut egui::Ui,
    label: &str,
    colour: Color32,
    font_size: f32,
) -> egui::Response {
    let font = egui::FontId::new(font_size, egui::FontFamily::Proportional);
    let glyphs: Vec<(char, f32)> = ui.ctx().fonts_mut(|f| {
        label
            .chars()
            .map(|c| (c, f.glyph_width(&font, c) + size::SECTION_TRACK))
            .collect()
    });
    // The trailing letter's tracking is not part of the word; leaving it in makes a
    // right-aligned neighbour sit a point further out than it should.
    let w: f32 = glyphs.iter().map(|(_, w)| w).sum::<f32>() - size::SECTION_TRACK;
    let h = ui.ctx().fonts_mut(|f| f.row_height(&font));
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
    let painter = ui.painter_at(rect);
    let mut x = rect.left();
    for (c, adv) in glyphs {
        painter.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            c,
            font.clone(),
            colour,
        );
        x += adv;
    }
    resp
}

/// A small selectable button.
///
/// **Selected is ruby text on [`RUBY_GROUND`] inside a ruby outline** — the same chip
/// the Settings window draws. A *saturated* ruby fill was tried and is unreadable;
/// what fails is the value of the ground, not the fact of one, and [`RUBY_GROUND`]
/// says why the colour is stored twice rather than shared.
///
/// **The name is a small lie** — nothing here draws a bracket. Kept because it is what
/// this control is called across the panel, and a rename would touch nine call sites
/// to say the same thing.
pub fn bracket(ui: &mut egui::Ui, label: &str, selected: bool, size: f32) -> egui::Response {
    let text = egui::RichText::new(label).size(size);
    let ink = if selected {
        RUBY
    } else {
        Color32::from_gray(150)
    };
    let resp = ui.add(
        egui::Button::new(text.color(ink))
            // The frame is what carries the ground, so it has to be on when selected.
            // Left off otherwise: an unselected chip is a word, and eight grey boxes
            // down a module is the thing the outline-only form was right to avoid.
            .frame(selected)
            .fill(if selected {
                RUBY_GROUND
            } else {
                Color32::TRANSPARENT
            })
            .stroke(egui::Stroke::new(
                1.0,
                if selected { RUBY } else { Color32::TRANSPARENT },
            )),
    );
    if resp.hovered() && !selected {
        // Only the outline appears on hover — the ink stays put, so the row does not
        // flicker between two greys as the pointer crosses it.
        ui.painter().rect_stroke(
            resp.rect,
            0.0,
            egui::Stroke::new(1.0, Color32::from_gray(90)),
            egui::StrokeKind::Inside,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A full-width hairline.
///
/// The panel's dividers, in two weights: grey between sections, and **ruby above and
/// below the open layer** — which is how the maintainer's mockup marks a selection. A rule is
/// the right mark for it because the thing being marked is a *region* of the list,
/// not a row: the selected layer and its controls are one block, and a highlight on
/// the row alone would leave the controls looking like they belonged to whatever
/// came next.
pub fn rule(ui: &mut egui::Ui, colour: Color32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, colour),
    );
}

/// The wide filled button that makes a layer: `+ DODGE`, `+ BURN`.
///
/// **The one filled control in the app, and the exception is deliberate.** Everything
/// else states its selected-ness with an outline, because the maintainer rejected a ruby fill
/// as unreadable. This is not a selected state — it is the panel's
/// primary action, the thing the whole bench exists to reach, and a fill is how a
/// primary action says so. The two are different claims and only one of them needed
/// suppressing.
///
/// **The fill is a colour, not a factor.** It was the layer's hue at
/// `gamma_multiply(0.5)`, which multiplies alpha too — so the button was a tint with
/// the panel showing through, and its apparent colour depended on what it sat over.
/// the maintainer has given opaque grounds instead: `DODGE_FILL` and `BURN_FILL`.
///
/// Hover **lerps the ground toward the ink** rather than scaling it. Same idea as
/// before — only the one under the pointer gets louder, and the pair stays a pair —
/// but it now moves along the hue family it belongs to instead of towards white, and
/// it cannot brighten past the text it is under.
///
/// `width` is passed in rather than measured because the pair has to come out equal
/// regardless of the words on them — "+ DODGE" and "+ BURN" are not the same length,
/// and two primary actions of two different sizes would rank one above the other.
pub fn wide_button(
    ui: &mut egui::Ui,
    label: &str,
    fill: Color32,
    text: Color32,
    width: f32,
    enabled: bool,
) -> egui::Response {
    filled_action_button(
        ui,
        label,
        fill,
        text,
        egui::vec2(width, size::SECTION + 14.0),
        size::SECTION,
        enabled,
    )
}

/// A proof-style action at ordinary control height.
pub fn compact_action_button(
    ui: &mut egui::Ui,
    label: &str,
    fill: Color32,
    text: Color32,
    width: f32,
    enabled: bool,
) -> egui::Response {
    filled_action_button(
        ui,
        label,
        fill,
        text,
        egui::vec2(width, ui.spacing().interact_size.y),
        size::BODY,
        enabled,
    )
}

fn filled_action_button(
    ui: &mut egui::Ui,
    label: &str,
    fill: Color32,
    text: Color32,
    size: egui::Vec2,
    font_size: f32,
    enabled: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let ground = match (enabled, resp.hovered()) {
        // Dropped in both value and opacity, so a bench at the eight-layer cap
        // recedes into the panel rather than sitting on it greyed.
        (false, _) => fill.gamma_multiply(0.35),
        (true, false) => fill,
        (true, true) => fill.lerp_to_gamma(text, 0.28),
    };
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, ground);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::new(font_size, egui::FontFamily::Proportional),
        if enabled {
            text
        } else {
            Color32::from_gray(110)
        },
    );
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// The brush cursor, as a pair of near-black and near-white.
///
/// **Dodge and burn are told apart by VALUE, not by hue**, and that is the theme
/// rule holding rather than an accident: ruby is the app's only saturated colour
/// and is spent on interaction state, so a blue dodge and a red burn — which is
/// what the prototype's panel buttons use — would put two more hues on screen
/// beside a monochrome print. Value is also the more apt distinction: a dodge is
/// the light one because dodging lightens.
///
/// Every ring is drawn twice, one of these inside the other, so the cursor is
/// visible against a blown highlight and a blocked shadow alike. A single ring
/// disappears into half the pictures it is used on.
pub const INK_LIGHT: Color32 = Color32::from_gray(240);
pub const INK_DARK: Color32 = Color32::from_gray(20);

/// The feather ring's dashes, and the one part of the brush cursor that does **not**
/// take the sign's ink.
///
/// It used to, which meant a burn brush drew its feather in [`INK_DARK`] — black
/// dashes, which the maintainer reported. The solid outline is what has to be told apart from
/// the picture at a glance and it still carries the light/dark pair; the feather is a
/// secondary mark inside it, and a light grey reads as one rather than competing with
/// the ring it sits under.
pub const FEATHER_GUIDE: Color32 = Color32::from_gray(190);

/// Height of the strip reserved for the macOS traffic lights, in points.
pub const TITLE_STRIP: f32 = 28.0;
/// Left inset that clears the traffic lights.
pub const TITLE_INSET: f32 = 78.0;

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);

    let mut v = egui::Visuals::dark();

    v.panel_fill = CHROME;
    v.window_fill = CHROME;
    v.extreme_bg_color = Color32::from_gray(20);
    v.faint_bg_color = Color32::from_gray(46);

    v.widgets.noninteractive.bg_fill = CHROME;
    v.widgets.noninteractive.fg_stroke.color = Color32::from_gray(180);
    v.widgets.inactive.bg_fill = Color32::from_gray(58);
    v.widgets.inactive.fg_stroke.color = Color32::from_gray(200);
    v.widgets.hovered.bg_fill = Color32::from_gray(74);
    v.widgets.hovered.fg_stroke.color = Color32::from_gray(235);
    v.widgets.active.bg_fill = RUBY.gamma_multiply(0.75);
    v.widgets.active.fg_stroke.color = Color32::WHITE;

    // **Text selection, and it had to come down.** the maintainer: the ruby behind a selected
    // DragValue is too opaque to read the number through — which matters because the
    // number is selected *while you are retyping it*, so the one moment the highlight
    // appears is the one moment you need to see what is underneath. 0.40 was chosen
    // when nothing was ever selected for long.
    v.selection.bg_fill = RUBY.gamma_multiply(0.18);
    v.selection.stroke.color = RUBY;
    v.hyperlink_color = RUBY;

    // ── The controls that had a house style and did not enforce it ───────────
    //
    // the maintainer's rule: **one checkbox and one text button across the whole app**, and the
    // ones to copy are the Develop panel's checkbox and the Dodge & Burn SHAPE button.
    // Both were already what he wanted; what was missing is that they were what
    // *those panels* did rather than what the theme said, so every new control was a
    // fresh decision and the app drifted one widget at a time.
    //
    // Setting them here rather than at the call sites is the whole point. A style
    // that has to be remembered is a style that will not be.
    // Square means square in every interaction state. These visuals are shared by
    // Button, DragValue, ComboBox's button and Checkbox, so the rule reaches the
    // whole app rather than relying on four families of call sites to remember it.
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(0);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(0);
    v.widgets.active.corner_radius = egui::CornerRadius::same(0);
    v.widgets.noninteractive.corner_radius = egui::CornerRadius::same(0);

    // A button reads as a button by its outline, not by a filled block — the same
    // conclusion the bracket buttons reached when the maintainer rejected a ruby fill, applied
    // as the default rather than as one control's exception.
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, Color32::from_gray(72));
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, Color32::from_gray(110));
    // **Not ruby, and this is a correction.** `active` is the state a widget is in
    // *while the mouse is held on it*, so a ruby outline here fired on every press of
    // every button in the app — a flash of the accent colour on an ordinary click,
    // which the maintainer read as something leaking out of egui rather than as this app's own
    // styling. He was right to: ruby means **this is the state you are in** everywhere
    // else — the selected tile, the active mode, the footer's current mode — and
    // spending it on the half-second a button is held devalues it everywhere it is load
    // bearing.
    //
    // A brighter grey continues the inactive → hovered ramp, so a press still reads as
    // a press. The fill and the text below already brighten with it.
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, Color32::from_gray(150));
    v.widgets.hovered.bg_fill = Color32::from_gray(58);
    v.widgets.active.bg_fill = Color32::from_gray(64);
    v.widgets.active.fg_stroke.color = BRIGHT;

    // The window itself, so the frame is dark grey rather than the OS default.
    v.window_stroke = egui::Stroke::new(1.0, Color32::from_gray(52));

    // **The app is dark regardless of the OS appearance, and this is not a taste
    // preference.** A light UI surrounding a monochrome rendering shifts how its
    // tonality reads — the same reason the image surround is a fixed mid-grey.
    // Letting the system theme decide would mean the same file looks like a
    // different print depending on the time of day.
    //
    // Both halves are needed. `set_theme` stops egui following the system, and
    // `set_visuals_of` fills BOTH slots so nothing can fall back to a light palette:
    // egui keeps a light and a dark `Style`, and the bare `set_visuals` writes only
    // whichever is currently active. On a light-mode Mac that wrote the dark palette
    // into a slot nothing ever read, and every panel drawn from `panel_fill` came
    // out near-white while the hand-painted chrome stayed dark.
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v.clone());
    ctx.set_visuals_of(egui::Theme::Light, v);

    // `./run` deliberately launches a debug build, and egui's debug `Style` enables
    // two *painted* diagnostics by default. The important one here is
    // `warn_if_rect_changes_id`: when dynamic footer contents exchange one widget for
    // another at the same rectangle, egui draws that rectangle in pure red for one
    // frame. It looks exactly like the accent leaking into the footer and frame lines,
    // because it is painted above the finished interface. `show_unaligned` can add a
    // similar orange overlay on fractional edges. Neither belongs in an app preview;
    // diagnostics can still be enabled deliberately while debugging egui layout.
    #[cfg(debug_assertions)]
    ctx.all_styles_mut(|s| {
        s.debug.debug_on_hover = false;
        s.debug.debug_on_hover_with_all_modifiers = false;
        s.debug.hover_shows_next = false;
        s.debug.show_expand_width = false;
        s.debug.show_expand_height = false;
        s.debug.show_resize = false;
        s.debug.show_interactive_widgets = false;
        s.debug.show_widget_hits = false;
        s.debug.warn_if_rect_changes_id = false;
        s.debug.show_unaligned = false;
        s.debug.show_focused_widget = false;
    });
}

/// Extra space between the letters of a field marker, in points.
///
/// Letter-spacing used to be done by interleaving `' '` between the characters,
/// which on a **fixed-pitch** face adds a whole character width per gap — enormous,
/// and not adjustable by anything smaller than a whole space. There is no tracking
/// control in egui, so the header is painted glyph by glyph instead and this is the
/// gap, in points, that anyone can now turn.
const TRACKING: f32 = 2.0;

/// The size a field marker will occupy once it is letter-spaced.
///
/// Split out from [`header_label`] because a tile tab has to know how much space to
/// claim *before* it allocates — it allocates one rect for the title and its close
/// button together, so it cannot let the label allocate for itself.
pub fn header_size(painter: &egui::Painter, label: &str) -> egui::Vec2 {
    let n = label.chars().count();
    if n == 0 {
        return egui::Vec2::ZERO;
    }
    let galley = painter.layout_no_wrap(label.to_owned(), header_font(), DIM);
    egui::vec2(
        galley.rect.width() + TRACKING * (n - 1) as f32,
        galley.rect.height(),
    )
}

fn header_font() -> egui::FontId {
    egui::FontId::proportional(size::HEADER)
}

fn module_font() -> egui::FontId {
    egui::FontId::new(size::HEADER, egui::FontFamily::Name("module".into()))
}

/// Paint a field marker into `rect`, left-aligned and vertically centred, glyph by
/// glyph so the tracking is a number rather than a whole space character.
///
/// **Upper case is the caller's job** — every caller passes one, and uppercasing
/// here would silently mangle a name that meant its own case.
pub fn paint_header(painter: &egui::Painter, rect: egui::Rect, label: &str, colour: Color32) {
    let chars: Vec<char> = label.chars().collect();
    if chars.is_empty() {
        return;
    }
    // One measurement of the whole string, divided by the character count. Exact on
    // a fixed-pitch face, and it avoids `glyph_width`, which wants a `&mut` to the
    // font cache that `Context::fonts` does not hand out.
    let font = header_font();
    let galley = painter.layout_no_wrap(label.to_owned(), font.clone(), colour);
    let advance = galley.rect.width() / chars.len() as f32;
    let mut x = rect.left();
    for c in chars {
        painter.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            c,
            font.clone(),
            colour,
        );
        x += advance + TRACKING;
    }
}

/// A dim, letter-spaced field marker, allocated in the current layout.
///
/// Returns a clickable response so a module header can use it as its collapse
/// target.
pub fn header_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let size = header_size(ui.painter(), label);
    if size == egui::Vec2::ZERO {
        return ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover());
    }
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    paint_header(&ui.painter_at(rect), rect, label, DIM);
    resp
}

/// A module title: the ordinary field-marker treatment in JetBrains Mono SemiBold.
pub fn module_header_label(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let chars: Vec<char> = label.chars().collect();
    if chars.is_empty() {
        return ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover());
    }
    let font = module_font();
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), DIM);
    let size = egui::vec2(
        galley.rect.width() + TRACKING * (chars.len() - 1) as f32,
        galley.rect.height(),
    );
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let advance = galley.rect.width() / chars.len() as f32;
    let mut x = rect.left();
    for c in chars {
        ui.painter_at(rect).text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            c,
            font.clone(),
            DIM,
        );
        x += advance + TRACKING;
    }
    resp
}

/// A dim, letter-spaced section header, matching the prototype's small-caps labels.
pub fn section(ui: &mut egui::Ui, label: &str) {
    header_label(ui, label);
    ui.add_space(2.0);
}

/// A hint. Every tooltip in the app goes through here; see `size::TIP`.
pub fn tip(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(size::TIP)
}

/// How long the pointer must rest before a hover tooltip appears.
const TOOLTIP_DELAY: f32 = 1.3;

/// Turn hover tooltips on or off for the whole app.
///
/// **One call rather than a condition at ~120 `on_hover_text` sites.** `tip` is the
/// funnel for the *text*, but suppression has to happen where the tooltip is shown,
/// and egui offers exactly one lever for that: the delay before one appears. An
/// infinite delay is a tooltip that never arrives.
///
/// It is a hack in the sense that it says "later" rather than "no", and it is the
/// right one anyway — the alternative is a boolean threaded through every call site,
/// where the first control anybody adds will forget it.
pub fn tooltips(ctx: &egui::Context, on: bool) {
    ctx.all_styles_mut(|s| {
        s.interaction.tooltip_delay = if on { TOOLTIP_DELAY } else { f32::INFINITY };
        // egui normally skips the delay when moving quickly from one tooltip target
        // to another. A tooltip that appears immediately is still an interruption,
        // so every new target earns the same deliberate pause.
        s.interaction.tooltip_grace_time = 0.0;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay text out through a real (headless) egui context with the theme applied,
    /// and return how wide it came out.
    ///
    /// Measured by drawing an actual label rather than by reading the font cache:
    /// it is the same path the panels take, so it cannot pass while the UI shows
    /// something else. `set_fonts` takes effect at the start of the *next* frame,
    /// so this runs one throwaway frame first.
    fn width_of(text: &str, family: egui::FontFamily) -> f32 {
        let ctx = egui::Context::default();
        apply(&ctx);
        let font = egui::FontId::new(size::BODY, family);
        let mut width = 0.0;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let rich = egui::RichText::new(text).font(font.clone());
                width = ui.label(rich).rect.width();
            });
        }
        width
    }

    #[test]
    fn every_bound_family_loads_the_face_we_shipped() {
        // Parsing is the thing that can fail — an unreadable TTF makes egui fall
        // back silently, and the app would run and simply not look like itself.
        //
        // Comparing `iiii` against `WWWW` is what makes this an assertion about
        // *our* faces rather than about any font at all: egui's stock Proportional
        // is Ubuntu-Light, where those two differ by more than double. Equal widths
        // mean a fixed-pitch face is at the front of the family, which is only true
        // if the include_bytes actually parsed and was inserted at index 0.
        for family in [
            egui::FontFamily::Proportional,
            egui::FontFamily::Monospace,
            egui::FontFamily::Name("module".into()),
        ] {
            let narrow = width_of("iiii", family.clone());
            let wide = width_of("WWWW", family.clone());
            assert!(
                narrow > 0.0,
                "{family:?} laid out nothing — the face did not parse"
            );
            assert!(
                (narrow - wide).abs() < 0.5,
                "{family:?} is not fixed-pitch ({narrow} vs {wide}) — egui's default \
                 font is still in front of ours"
            );
        }
    }

    #[test]
    fn one_face_serves_both_families() {
        // **The inverse of the test this replaces**, and that is the point rather than
        // an accident. The app used to ship two typefaces on the argument that a
        // measured number should not look like a chosen name, and there was a test
        // here asserting they measured differently.
        //
        // the maintainer's decision replaced both with JetBrains Mono. What separates a readout
        // from a label is now position and colour — the control row puts every number
        // in the same column — which does the job better and costs one font instead of
        // two. So the old guard is not deleted, it is turned over: both families must
        // resolve to the *same* face, or something has quietly reinstated a second one.
        let ui = width_of("MONOPRO", egui::FontFamily::Proportional);
        let readout = width_of("MONOPRO", egui::FontFamily::Monospace);
        assert!(ui > 0.0, "nothing laid out");
        assert!(
            (ui - readout).abs() < 0.01,
            "the two families measure differently ({ui} vs {readout}) — a second face \
             has been installed into one of them"
        );
    }

    #[test]
    fn standard_controls_have_square_corners_in_every_state() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let style = ctx.style_of(egui::Theme::Dark);
        for (name, radius) in [
            ("inactive", style.visuals.widgets.inactive.corner_radius),
            ("hovered", style.visuals.widgets.hovered.corner_radius),
            ("active", style.visuals.widgets.active.corner_radius),
            (
                "noninteractive",
                style.visuals.widgets.noninteractive.corner_radius,
            ),
        ] {
            assert_eq!(radius, egui::CornerRadius::same(0), "{name} is bevelled");
        }
    }

    #[test]
    fn every_intentional_ui_symbol_has_a_glyph_in_the_real_font_stack() {
        // Symbols used as controls, shortcut marks, measurement notation, and icon
        // fallbacks. Test the composed egui family rather than JetBrains Mono alone:
        // `fonts` deliberately leaves egui's stock faces behind ours for exactly
        // these characters. A full-width lookalike that none of them owns (the Add
        // Location bug) must fail here instead of reaching the screen as a box.
        const SYMBOLS: &str = "—×°→…⌘⇧⌥⌃−↗·★☆▸⋯↕⌸▽□≡↻↺½▼▲▣↙◆⏮‰⌖◉⟲⟳✶©Ō“”’";

        let ctx = egui::Context::default();
        apply(&ctx);
        // `set_fonts` becomes active at the next pass boundary.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let font = egui::FontId::proportional(size::BODY);
        let missing: String = ctx.fonts_mut(|fonts| {
            SYMBOLS
                .chars()
                .filter(|symbol| !fonts.has_glyph(&font, *symbol))
                .collect()
        });
        assert!(
            missing.is_empty(),
            "UI symbols have no glyph in the installed font stack: {missing:?}"
        );
    }

    #[test]
    fn the_underscore_defect_is_gone_rather_than_avoided() {
        // A face that kerns `_` against a letter renders `L1000016_dup1` with its
        // characters piled up. The failure is silent everywhere except the tab strip,
        // so a font swap that reintroduced it would not be caught by eye.
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let ab = width_of("ab", family.clone());
            let underscore = width_of("a_", family.clone());
            assert!(
                (ab - underscore).abs() < 0.5,
                "{family:?} kerns the underscore ({underscore} vs {ab} for a normal \
                 pair) — filenames would render with their characters overlapping"
            );
        }
    }

    #[test]
    fn the_scale_is_bound_to_both_theme_slots() {
        // The trap `apply` documents for visuals applies to text styles too: egui
        // keeps a light and a dark `Style`, and writing only the active one leaves
        // the other at egui's defaults. `all_styles_mut` is what makes this pass.
        let ctx = egui::Context::default();
        apply(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            let body = &style.text_styles[&egui::TextStyle::Body];
            assert_eq!(body.size, size::BODY, "{theme:?} kept egui's own body size");
        }
    }

    #[test]
    fn a_module_regreys_to_its_ground_and_keeps_its_hues() {
        // At the design ground nothing moves.
        assert_eq!(regrey(CHROME.r(), DIM), DIM);
        // On a light card, text that sat above the ground now sits below it by the
        // same distance, and the card fill itself lands exactly on the ground.
        assert_eq!(regrey(230, CHROME), Color32::from_gray(230));
        let text = regrey(230, DIM);
        assert!(
            text.r() < 230 - 60,
            "{text:?} is not dark enough to read on white"
        );
        assert!(
            regrey(230, BRIGHT).r() < text.r(),
            "the hierarchy must survive the flip"
        );
        // A darker card keeps the offsets without flipping.
        assert_eq!(regrey(23, DIM).r(), DIM.r() - 15);
        // Ruby is a mid-value ink and reads on either ground.
        assert_eq!(regrey(230, RUBY), RUBY);
        // …but a dark tint is a ground: on a light card it goes light and keeps its
        // hue, so ruby text on it stays readable.
        let fill = regrey(230, RUBY_FILL);
        assert!(
            fill.r() > 200 && fill.r() > fill.g(),
            "{fill:?} should be a pale ruby"
        );
        assert!(regrey(230, RUBY_GROUND).g() > 180);
        // A pale ink goes dark on a light card and keeps its hue; on a dark card it
        // is left alone.
        let dodge = regrey(230, DODGE);
        assert!(dodge.b() < 160 && dodge.b() > dodge.r(), "{dodge:?}");
        assert_eq!(regrey(23, DODGE), DODGE);
    }

    #[test]
    fn a_picture_survives_a_light_ground() {
        // egui paints an image as a white-tinted rect with a texture brush. Re-greying
        // that tint turned every Lightbox thumbnail black past mid-grey.
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(10.0, 10.0));
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let tex = egui::TextureId::Managed(7);
        let mut picture = egui::Shape::Rect(
            egui::epaint::RectShape::filled(rect, 0.0, Color32::WHITE).with_texture(tex, uv),
        );
        regrey_shape(Ground::module(230), &mut picture);
        let egui::Shape::Rect(r) = picture else {
            unreachable!()
        };
        assert_eq!(r.fill, Color32::WHITE, "a picture's tint is not chrome");

        // A plain white rect is chrome and does move.
        let mut chrome = egui::Shape::Rect(egui::epaint::RectShape::filled(rect, 0.0, BRIGHT));
        regrey_shape(Ground::module(230), &mut chrome);
        let egui::Shape::Rect(r) = chrome else {
            unreachable!()
        };
        assert_ne!(r.fill, BRIGHT);
    }

    #[test]
    fn every_tooltip_waits_before_appearing() {
        let ctx = egui::Context::default();
        tooltips(&ctx, true);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let interaction = &ctx.style_of(theme).interaction;
            assert_eq!(interaction.tooltip_delay, 1.3);
            assert_eq!(interaction.tooltip_grace_time, 0.0);
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn a_normal_debug_run_paints_no_egui_diagnostics() {
        let ctx = egui::Context::default();
        apply(&ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let debug = ctx.style_of(theme).debug;
            assert!(!debug.debug_on_hover);
            assert!(!debug.debug_on_hover_with_all_modifiers);
            assert!(!debug.hover_shows_next);
            assert!(!debug.show_expand_width);
            assert!(!debug.show_expand_height);
            assert!(!debug.show_resize);
            assert!(!debug.show_interactive_widgets);
            assert!(!debug.show_widget_hits);
            assert!(!debug.warn_if_rect_changes_id);
            assert!(!debug.show_unaligned);
            assert!(!debug.show_focused_widget);
        }
    }
}
