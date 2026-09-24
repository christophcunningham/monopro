//! SVG icons from `icons/`, rasterised once at startup. Phosphor, plus a Lucide pair
//! for float and dock — see [`SOURCES`].
//!
//! egui does not draw SVG, so something has to. The alternative considered was
//! painting these by hand — `plus.svg` is two lines and `copy-simple.svg` is a rect
//! and a polyline, so three or four `painter` calls each — which costs no
//! dependency at all. It was rejected because it does not scale past the handful
//! already in use: `icons/` holds fifty files and how icon-heavy this UI becomes is
//! deliberately still open. Paying for a loader once keeps that decision open;
//! hand-drawing would quietly close it by making every new icon a small programming
//! task.
//!
//! `resvg` is taken with **default features off**, which drops text shaping, font
//! discovery and raster-image decoding — none of which a stroked 24×24 glyph needs.
//!
//! Everything is rasterised **white and tinted at draw time**. One texture then
//! serves the dim, hovered and disabled states, which is both fewer textures and
//! the only way the colours stay in step with the theme.

use std::collections::HashMap;

use resvg::usvg;

/// Rasterised at well above `BOX` so it stays sharp on a Retina display and on
/// whatever fractional scaling a external monitor asks for. A 48×48 RGBA texture is
/// 9 KB; there is nothing to save by being clever here.
const RASTER: usize = 48;

/// The second size, for icons the Lightbox grid draws as the whole subject of a tile.
///
/// **48 is a button size, and a grid tile is not a button.** `RASTER` was chosen when
/// every icon in the app sat in a 15pt box, where 48 physical pixels is already
/// generous. A folder tile on the widest rung draws its glyph at up to 96 *points* —
/// 192 device pixels on a Retina display — so the 48px texture is being blown up four
/// times, which is exactly what the maintainer saw. The SVG is vector right up until `load`
/// rasterises it, and then it is a bitmap like any other.
///
/// 256 covers 96pt at 2x with room over. Only [`LARGE`] pays for it: ~22 icons at
/// 256 KB each is under 6 MB against a tile cache whose own ceiling is 350 MB.
const RASTER_LARGE: usize = 256;

/// Which icons get the second, larger texture — the ones the grid draws big.
///
/// Derived from the name rather than listed, so the file-type set added next does not
/// have to remember to join. Everything else is a control and stays at [`RASTER`].
fn wants_large(name: &str) -> bool {
    name.starts_with("file") || name.starts_with("folder")
}

/// Every icon the app ships, by the name call sites ask for.
///
/// A named list rather than an array literal inside `load`, so
/// `every_shipped_icon_rasterises_to_something_visible` can check the same set the app
/// draws. A parse failure
/// is a *silent* downgrade to the text glyph — the app still runs and simply stops
/// looking like itself — which is exactly the kind of thing a test has to catch.
///
/// **Two families, and the mix is chosen rather than accidental.** The controls that were
/// here first are Phosphor; float and dock are Lucide, picked by the maintainer and drawn as a box
/// with an arrow leaving it rather than as four arrows spreading. They sit together
/// because both are stroked outlines at a similar relative weight — Phosphor 16/256,
/// Lucide 1.25/24, so the Lucide pair is marginally the finer of the two. Both families
/// are credited in `ACKNOWLEDGMENTS`; Lucide is ISC, where attribution is a licence term
/// rather than a courtesy.
const SOURCES: &[(&str, &str)] = &[
    ("plus", include_str!("../../../icons/plus.svg")),
    // Curve point sampler. The same Phosphor eyedropper and the same cursor drawing
    // Triopro uses for its white-point sampler.
    ("eyedropper", include_str!("../../../icons/eyedropper.svg")),
    ("duplicate", include_str!("../../../icons/copy-simple.svg")),
    ("close", include_str!("../../../icons/x.svg")),
    (
        "float",
        include_str!("../../../icons/square-arrow-out-up-right.svg"),
    ),
    (
        "dock",
        include_str!("../../../icons/square-arrow-out-down-left.svg"),
    ),
    // Composition. Back to Phosphor for all three: `arrow-arc-left` and
    // `arrow-arc-right` are a genuine mirrored pair, which matters more here than
    // anywhere else in the app — two rotate buttons drawn from different glyphs
    // would read as two different operations rather than as one in two directions.
    (
        "rotate-left",
        include_str!("../../../icons/arrow-arc-left.svg"),
    ),
    (
        "rotate-right",
        include_str!("../../../icons/arrow-arc-right.svg"),
    ),
    ("crop", include_str!("../../../icons/crop.svg")),
    // The rotate-ring cursor. egui's `CursorIcon` has no rotate, so the system
    // cursor is hidden in that zone and this is drawn at the pointer instead —
    // the same thing the prototype does with a hand-built `QCursor` pixmap.
    ("rotate", include_str!("../../../icons/arrow-clockwise.svg")),
    // The draw-a-horizon button. An angle is what the tool measures.
    ("angle", include_str!("../../../icons/angle.svg")),
    // Stand the crop ratio on end. **the maintainer's glyph, replacing a bare `↕`.**
    //
    // The arrow was a fair first reading of the prototype's control and it says the
    // wrong thing: `↕` is the mark for *resize vertically*, which is what the edge
    // handles on the crop box do, so the one button that swaps the frame's two
    // dimensions wore the sign of the gesture that changes one of them. A device
    // turning on its side is the operation itself — 4:5 becomes 5:4 — and it is the
    // mark every phone and tablet uses for exactly this.
    (
        "device-rotate",
        include_str!("../../../icons/device-rotate.svg"),
    ),
    // Roll a new grain seed. The two-arrow cycle rather than the single curved
    // arrow already registered as `rotate`: that one turns the *picture* and reusing
    // it here would put one glyph on two unrelated actions.
    //
    // **`arrows-clockwise`, which the maintainer asked for twice.** The first time the set did
    // not contain it and this took the counter-clockwise plural instead, on the
    // argument that a refresh mark reads as "again" rather than as a direction. That
    // argument was fine and it was answering a question nobody asked; he has since
    // added the file.
    (
        "reseed",
        include_str!("../../../icons/arrows-clockwise.svg"),
    ),
    // The grain loupe. An eye, because the loupe shows rather than changes — it is
    // the one control in the module that reaches no pixel of the export.
    ("eye", include_str!("../../../icons/eye.svg")),
    // A pinned snapshot. the maintainer's glyph, and a diamond is right for a mark rather than
    // a button: it has no direction and no verb in it, so it reads as a state. The
    // filled partner is the active mark; colour alone left the ruby diamond looking
    // like a hover treatment rather than a pin that had actually been set.
    ("diamond", include_str!("../../../icons/diamond.svg")),
    (
        "diamond-filled",
        include_str!("../../../icons/diamond-filled.svg"),
    ),
    // Restore a snapshot. A transport mark rather than an arrow-arc, which is already
    // the rotate pair — this one means "go back to that", not "turn the picture".
    ("skip-back", include_str!("../../../icons/skip-back.svg")),
    // The move cursor, for a value pin under the pointer. the maintainer's glyph. Drawn at the
    // pointer rather than set as a `CursorIcon` for the reason `rotate` is: egui's set
    // has `Move`, but it is the OS four-way *window* move on macOS and reads as "drag
    // this window", which is not what a pin does.
    (
        "move",
        include_str!("../../../icons/arrows-out-cardinal.svg"),
    ),
    // The Lightbox footer's two display toggles, and these are the prototype's own
    // glyphs — `monopro.py:175` embeds exactly these three SVGs inline for this pair.
    //
    // Filenames is one icon that only changes tint, because the control has one
    // meaning in two states. Frameless is **two** icons, solid and dashed, because
    // there the state *is* a border: a dashed rectangle says "the frame is off" in
    // the shape of the thing it turns off, which no amount of tinting says.
    // The sort direction pair, and the filter mark. A genuine mirrored pair again,
    // for the same reason the rotate arrows are: two directions of one control drawn
    // from unrelated glyphs would read as two controls.
    (
        "sort-ascending",
        include_str!("../../../icons/sort-ascending.svg"),
    ),
    (
        "sort-descending",
        include_str!("../../../icons/sort-descending.svg"),
    ),
    ("funnel", include_str!("../../../icons/funnel-simple.svg")),
    ("tree-view", include_str!("../../../icons/tree-view.svg")),
    // Rename one frame or a selection from the Lightbox footer.
    ("text-aa", include_str!("../../../icons/text-aa.svg")),
    // Text alignment, wherever it is chosen. Three icons rather than a combo: the
    // options are a closed set of three whose shapes *are* the answer, so a word in a
    // dropdown is a slower way of saying what a picture of ragged lines says at once.
    (
        "text-align-left",
        include_str!("../../../icons/text-align-left.svg"),
    ),
    (
        "text-align-center",
        include_str!("../../../icons/text-align-center.svg"),
    ),
    (
        "text-align-right",
        include_str!("../../../icons/text-align-right.svg"),
    ),
    // Whether four margins are one number. The broken chain is the *off* state, which
    // is the way round that reads: a whole chain says these are held together.
    ("link", include_str!("../../../icons/link.svg")),
    ("link-break", include_str!("../../../icons/link-break.svg")),
    // the maintainer's folder glyphs, and the file marks for what the grid cannot draw.
    ("folder", include_str!("../../../icons/folder-simple.svg")),
    (
        "folder-plus",
        include_str!("../../../icons/folder-simple-plus.svg"),
    ),
    ("folders", include_str!("../../../icons/folders.svg")),
    ("file", include_str!("../../../icons/file.svg")),
    ("file-image", include_str!("../../../icons/file-image.svg")),
    // **The file-type set, for the Lightbox's "show other files" setting.** the maintainer
    // added these; `crate::lightbox::icon_for` is the extension-to-name mapping and
    // `every_file_icon_the_grid_asks_for_is_loaded` is what keeps the two in step —
    // a name this list does not have degrades to the text glyph silently, which on a
    // grid tile would be an empty card rather than a visibly missing icon.
    //
    // Names are `file-<ext-family>` so the mapping reads as itself. Where one glyph
    // serves several extensions the family is named for the glyph rather than for any
    // one of them: `file-code` takes the languages Phosphor has no separate mark for.
    ("file-txt", include_str!("../../../icons/file-txt.svg")),
    ("file-md", include_str!("../../../icons/file-md.svg")),
    ("file-pdf", include_str!("../../../icons/file-pdf.svg")),
    ("file-doc", include_str!("../../../icons/file-doc.svg")),
    ("file-xls", include_str!("../../../icons/file-xls.svg")),
    ("file-zip", include_str!("../../../icons/file-zip.svg")),
    (
        "file-archive",
        include_str!("../../../icons/file-archive.svg"),
    ),
    ("file-audio", include_str!("../../../icons/file-audio.svg")),
    ("file-cloud", include_str!("../../../icons/file-cloud.svg")),
    ("file-code", include_str!("../../../icons/file-code.svg")),
    ("file-html", include_str!("../../../icons/file-html.svg")),
    ("file-js", include_str!("../../../icons/file-js.svg")),
    ("file-py", include_str!("../../../icons/file-py.svg")),
    ("file-rs", include_str!("../../../icons/file-rs.svg")),
    ("file-c", include_str!("../../../icons/file-c.svg")),
    ("file-cpp", include_str!("../../../icons/file-cpp.svg")),
    ("file-sql", include_str!("../../../icons/file-sql.svg")),
    ("file-svg", include_str!("../../../icons/file-svg.svg")),
    ("file-jpg", include_str!("../../../icons/file-jpg.svg")),
    ("file-png", include_str!("../../../icons/file-png.svg")),
    ("article", include_str!("../../../icons/article.svg")),
    ("rectangle", include_str!("../../../icons/rectangle.svg")),
    (
        "rectangle-dashed",
        include_str!("../../../icons/rectangle-dashed.svg"),
    ),
];

pub struct Icons {
    small: HashMap<&'static str, egui::TextureHandle>,
    /// The [`RASTER_LARGE`] copies, for the names [`wants_large`] admits. Consulted by
    /// `paint_at` only when the small one would actually be upscaled.
    large: HashMap<&'static str, egui::TextureHandle>,
}

impl Icons {
    /// No icons at all, for the moment between `App`'s literal and the context
    /// existing. Every button falls back to its text glyph, so this is a valid
    /// state rather than a placeholder that must be replaced.
    pub fn empty() -> Self {
        Self {
            small: HashMap::new(),
            large: HashMap::new(),
        }
    }

    pub fn load(ctx: &egui::Context) -> Self {
        let mut small = HashMap::new();
        let mut large = HashMap::new();
        for (name, src) in SOURCES {
            // A missing or unparseable icon must not stop the app — the same rule
            // `Loaded<T>` applies to everything read from disk. The button falls
            // back to its text glyph.
            if let Some(img) = rasterise_at(src, RASTER) {
                small.insert(
                    *name,
                    ctx.load_texture(*name, img, egui::TextureOptions::LINEAR),
                );
            }
            if wants_large(name)
                && let Some(img) = rasterise_at(src, RASTER_LARGE)
            {
                large.insert(
                    *name,
                    ctx.load_texture(format!("{name}-large"), img, egui::TextureOptions::LINEAR),
                );
            }
        }
        Self { small, large }
    }

    /// The texture to draw `name` at `px` **device** pixels.
    ///
    /// The large copy only when the small one would be enlarged, so nothing that fits
    /// in 48 pays the memory or gets a needlessly soft downscale.
    fn best(&self, name: &str, px: f32) -> Option<&egui::TextureHandle> {
        if px > RASTER as f32
            && let Some(t) = self.large.get(name)
        {
            return Some(t);
        }
        self.small.get(name)
    }
}

/// Whether the app ships an icon under this name.
///
/// **Test support, and `#[cfg(test)]` because nothing at runtime should need to ask.**
/// Every call site in the app writes a literal name and `paint_at` degrades safely if
/// one is wrong; the exception is `lightbox::icon_for`, which picks a name from a file
/// extension, and the exception is exactly why this exists —
/// `every_file_icon_the_grid_asks_for_is_loaded` walks that mapping against this. A
/// name that is not here would be a text glyph on a grid tile that has no text, which
/// is a blank card rather than a visibly wrong one. `SOURCES` stays private.
#[cfg(test)]
pub fn shipped(name: &str) -> bool {
    SOURCES.iter().any(|(n, _)| *n == name)
}

/// The button-sized raster. Test-only since `load` names its own edge — kept because
/// `every_shipped_icon_rasterises_to_something_visible` checks the size it produces.
#[cfg(test)]
fn rasterise(src: &str) -> Option<egui::ColorImage> {
    rasterise_at(src, RASTER)
}

/// [`rasterise`] at an explicit edge, so one SVG can serve a button and a grid tile.
fn rasterise_at(src: &str, raster: usize) -> Option<egui::ColorImage> {
    // `currentColor` inherits from a CSS `color` the document never sets, and usvg
    // resolves it to black — which on this UI is invisible. White, then tinted.
    let src = src.replace("currentColor", "#ffffff");
    let tree = usvg::Tree::from_str(&src, &usvg::Options::default()).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(raster as u32, raster as u32)?;
    let scale = raster as f32 / tree.size().width().max(1.0);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia writes premultiplied alpha. Reading it as unmultiplied darkens
    // every antialiased edge, which on a 1.5px stroke is most of the glyph.
    Some(egui::ColorImage::from_rgba_premultiplied(
        [raster, raster],
        pixmap.data(),
    ))
}

/// The glyph, and the box it sits in.
///
/// Two numbers rather than one derived from the other: the padding between them is
/// the point. The first version fitted the button tightly to the glyph, which made
/// a row of controls that looked crowded against the filenames beside them.
pub const BOX: f32 = 15.0;
const GLYPH: f32 = 9.0;

/// An icon button, falling back to `glyph` if the icon is missing.
///
/// Painted by hand rather than through `egui::Button::image`, for one reason: the
/// tint has to depend on hover, and a `Button`'s image is built before there is a
/// response to ask. The first attempt read `ui.ui_contains_pointer()`, which asks
/// about the *containing* `Ui` — so hovering any tab lit every icon in the strip at
/// once. Allocating the rect first gives a response to read, and the glyph goes
/// white under the pointer.
pub fn button(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    enabled: bool,
) -> egui::Response {
    sized(ui, icons, name, glyph, enabled, BOX)
}

/// Bigger, for the buttons that are a control rather than a corner affordance.
///
/// The tab strip's close and duplicate are marks you aim at once you have already
/// decided; Composition's rotate, crop and straighten are the module's *controls*,
/// sat among sliders and combo boxes, and at `BOX` they read as afterthoughts beside
/// them. the maintainer asked for them larger, and the glyph keeps its proportion of the box
/// so nothing has to be re-tuned per icon.
pub const BIG: f32 = 21.0;

/// The ink an icon button is drawn in, which is **three states and not four**.
///
/// `HOT` is the light grey the glyph takes under the pointer, and it is also what an
/// icon that is *on* is drawn in. the maintainer asked for that, and it settles something the
/// ruby outline alone was leaving half-said: an outlined button whose glyph stayed at
/// `DIM` had its state in the frame and not in the mark, so at a glance down a column of
/// tools the outline was the only thing carrying it. On and hovered look the same on
/// purpose — hover is a *promise* of the state, so the two agreeing is the affordance
/// working rather than an ambiguity.
const HOT: egui::Color32 = egui::Color32::from_gray(245);

/// What an icon button is, in the only three states it can be in.
///
/// One value rather than the `enabled` and `active` pair it replaces, which had a
/// fourth combination — disabled *and* on — that no caller can produce and no painter
/// knew what to do with.
#[derive(Clone, Copy, PartialEq)]
enum State {
    Off,
    On,
    Disabled,
}

fn on(active: bool) -> State {
    if active { State::On } else { State::Off }
}

fn ink(state: State, hot: bool) -> egui::Color32 {
    match state {
        State::Disabled => crate::theme::DIM.gamma_multiply(0.35),
        State::On => HOT,
        State::Off if hot => HOT,
        State::Off => crate::theme::DIM,
    }
}

/// An icon button that is also a **state**: outlined in ruby while it is on, and its
/// glyph in [`HOT`] rather than `DIM`.
///
/// One definition rather than two, because a tool that is open and a tool that is
/// armed are the same claim and have to look the same. Drawn as an outline rather
/// than a fill so the glyph keeps its own tint and the button does not become a
/// different shape when it lights.
pub fn toggle(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    active: bool,
    box_size: f32,
) -> egui::Response {
    button_in(ui, icons, name, glyph, None, on(active), box_size, 0.0)
}

/// [`toggle`] that can also be greyed out.
///
/// **`enabled` wins over `active`**, which is the one combination the [`State`] enum
/// says no caller can produce. It still cannot in practice — Composition's portrait
/// toggle reads `can_flip && portrait`, so a disabled one is never on — but a helper
/// that takes both flags has to answer the question, and "disabled" is the honest
/// answer: a control you cannot reach should not be advertising a state you cannot
/// change.
pub fn toggle_enabled(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    active: bool,
    enabled: bool,
    box_size: f32,
) -> egui::Response {
    let state = if enabled { on(active) } else { State::Disabled };
    button_in(ui, icons, name, glyph, None, state, box_size, 0.0)
}

/// How much wider than tall a bare-glyph toggle is allowed to be, in points.
///
/// **A square box is the wrong shape next to a labelled one.** Composition's row runs
/// `⌗ Crop` then the straighten `∠`, and the first is a wide button with its glyph
/// padded away from its own edges by the word beside it. The second, at the same
/// `box_size`, is a glyph pressed against both walls of a square — so the two read as
/// a button and a slightly panicky icon, which is the thing the maintainer saw. Six points is
/// the smallest amount that reads as deliberate.
///
/// Split evenly either side by [`button_in`], so the glyph stays centred.
pub const WIDE: f32 = 6.0;

/// [`toggle`], a little wider than it is tall. See [`WIDE`].
pub fn wide_toggle(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    active: bool,
    box_size: f32,
) -> egui::Response {
    button_in(ui, icons, name, glyph, None, on(active), box_size, WIDE)
}

/// An icon that is a **state**, not a button: [`crate::theme::RUBY`] when it is on.
///
/// The pin. Distinct from [`toggle`], which is a *tool* that is open and wears a ruby
/// outline with its glyph in [`HOT`] — here the glyph itself carries the state and
/// there is no outline, because what is being marked is the row rather than the
/// control. It is the theme's own rule read literally: ruby means "something is on that
/// would not be on by default", and a pin is exactly that.
pub fn flag(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    on: bool,
    enabled: bool,
    box_size: f32,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(box_size, box_size),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let hot = enabled && resp.hovered();
    let colour = match (enabled, on, hot) {
        (false, _, _) => crate::theme::DIM.gamma_multiply(0.35),
        (_, true, _) => crate::theme::RUBY,
        (_, false, true) => HOT,
        _ => crate::theme::DIM,
    };
    if hot {
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_gray(64));
    }
    paint_at(ui, icons, name, glyph, rect, colour, box_size * GLYPH / BOX);
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// [`toggle`] with a word beside the glyph.
///
/// **the maintainer asked for the crop tool to say `Crop`**, and an icon that names itself is
/// worth more than the two points it costs: `⌗` is a tool you have to have been told
/// about, where `⌗ Crop` is one you can read. The pair share a frame, a fill and an
/// outline, so it is one button rather than an icon that happens to sit near a label.
pub fn labelled_toggle(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    label: &str,
    active: bool,
    box_size: f32,
) -> egui::Response {
    button_in(
        ui,
        icons,
        name,
        glyph,
        Some(label),
        on(active),
        box_size,
        0.0,
    )
}

pub fn sized(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    enabled: bool,
    box_size: f32,
) -> egui::Response {
    let state = if enabled { State::Off } else { State::Disabled };
    button_in(ui, icons, name, glyph, None, state, box_size, 0.0)
}

/// Every icon button in the app, in one place: claim a box, fill it on hover, tint the
/// glyph, and outline it in ruby if it is a toggle that is on.
///
/// `pad_x` is extra width, split evenly either side of the content — see [`WIDE`].
#[expect(
    clippy::too_many_arguments,
    reason = "the one drawing routine behind every icon button; each argument is a variation some caller uses"
)]
fn button_in(
    ui: &mut egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    label: Option<&str>,
    state: State,
    box_size: f32,
    pad_x: f32,
) -> egui::Response {
    let enabled = state != State::Disabled;
    // The word is measured before the box is claimed, because it is inside the button
    // rather than beside it — a label that allocated for itself would be a second
    // widget with its own hover, and the fill would stop halfway along the thing you
    // are pointing at.
    let pad = 5.0;
    let text = label.map(|l| {
        let font = egui::FontId::new(crate::theme::size::BODY, egui::FontFamily::Proportional);
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(l.to_owned(), font, crate::theme::DIM));
        (galley.size().x, galley)
    });
    // **The word gets the same `pad` after it as before it.** the maintainer's note on the Crop
    // button: with only the leading gap counted, the ruby outline and the hover fill
    // stopped on the `p`, so the one button in the module that carries a word was also
    // the one whose frame looked like a mistake. The glyph never had this problem
    // because `BOX` is bigger than `GLYPH` — the padding was built into the box. This
    // gives the text the same courtesy.
    let w = box_size + text.as_ref().map_or(0.0, |(tw, _)| tw + pad * 2.0) + pad_x;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(w, box_size),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let hot = enabled && resp.hovered();
    let colour = ink(state, hot);
    if hot {
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_gray(64));
    }
    let box_rect = egui::Rect::from_min_size(
        rect.min + egui::vec2(pad_x * 0.5, 0.0),
        egui::vec2(box_size, box_size),
    );
    paint_at(
        ui,
        icons,
        name,
        glyph,
        box_rect,
        colour,
        box_size * GLYPH / BOX,
    );
    if let Some((_, galley)) = text {
        let at = egui::pos2(
            box_rect.right() + pad,
            rect.center().y - galley.size().y * 0.5,
        );
        ui.painter().galley(at, galley, colour);
    }
    if state == State::On {
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, crate::theme::RUBY),
            egui::StrokeKind::Inside,
        );
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// Paint an icon centred in `rect`, tinted, with no allocation and no interaction.
///
/// The counterpart to [`button`] for a caller that has already claimed and sensed
/// its own rect: a tile tab allocates the title and its close together and senses
/// the whole thing for a drag, so the close cannot be a widget that allocates for
/// itself part-way through.
pub fn paint(
    ui: &egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    rect: egui::Rect,
    colour: egui::Color32,
) {
    paint_at(ui, icons, name, glyph, rect, colour, GLYPH);
}

/// [`paint`], at an explicit glyph size.
pub fn paint_at(
    ui: &egui::Ui,
    icons: &Icons,
    name: &str,
    glyph: &str,
    rect: egui::Rect,
    colour: egui::Color32,
    size: f32,
) {
    let box_ = egui::Rect::from_center_size(rect.center(), egui::vec2(size, size));
    // **Device pixels, not points.** A 96pt glyph on a Retina panel is 192 real pixels,
    // and it is the real number that decides whether the 48px texture is being enlarged.
    let px = size * ui.ctx().pixels_per_point();
    match icons.best(name, px) {
        Some(tex) => {
            egui::Image::new(tex)
                .fit_to_exact_size(egui::vec2(size, size))
                .tint(colour)
                .paint_at(ui, box_);
        }
        None => {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(size + 2.0),
                colour,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_icon_rasterises_to_something_visible() {
        // **A failed parse is silent.** `load` skips the icon and the button falls back
        // to its text glyph, so the app runs and simply stops looking like itself —
        // nobody files that, they just see a `⇱` where an icon should be. Adding an icon
        // in a new family, or with a `viewBox` usvg reads differently, is exactly when
        // this fires.
        for (name, src) in SOURCES {
            let img = rasterise(src)
                .unwrap_or_else(|| panic!("{name} did not parse — the button will show text"));
            assert_eq!(img.size, [RASTER, RASTER], "{name} came out the wrong size");
            let ink = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(
                ink > 20,
                "{name} rasterised to {ink} visible pixels — effectively blank"
            );
        }
    }

    /// **The grid draws its glyphs at up to 96pt, which is 192 device pixels.** At
    /// `RASTER` that is a fourfold enlargement of a bitmap, which is what the maintainer saw as
    /// "low resolution SVGs" — the vector is spent the moment `load` rasterises it.
    ///
    /// A miss here is invisible: `best` falls back to the small texture and the tile
    /// still draws, just softly. So the set has to be asserted rather than eyeballed.
    #[test]
    fn every_glyph_the_grid_draws_big_has_a_large_raster() {
        for (name, src) in SOURCES {
            if !wants_large(name) {
                continue;
            }
            let img = rasterise_at(src, RASTER_LARGE)
                .unwrap_or_else(|| panic!("{name} has no large raster — tiles will be soft"));
            assert_eq!(img.size, [RASTER_LARGE, RASTER_LARGE], "{name}");
            let ink = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(
                ink > 200,
                "{name} rasterised to {ink} visible pixels at {RASTER_LARGE}"
            );
        }
        // The two the Lightbox reaches for by name, so a rename cannot quietly drop them
        // out of the large set.
        assert!(wants_large("folder"));
        assert!(wants_large("file"));
        assert!(wants_large("file-txt"));
        // And a control that has no business paying for 256px.
        assert!(!wants_large("close"));
        assert!(!wants_large("crop"));
    }

    #[test]
    fn icons_are_rasterised_white_so_the_theme_can_tint_them() {
        // Everything is drawn white and tinted at draw time, which is what lets one
        // texture serve the dim, hovered and disabled states — and the only way the
        // colours stay in step with the theme. `currentColor` resolves to *black* in
        // usvg, because the document never sets a CSS `color`, and black on this UI is
        // invisible. The substitution in `rasterise` is load-bearing, not a nicety.
        let img = rasterise(SOURCES[0].1).expect("plus parses");
        let lit: Vec<_> = img.pixels.iter().filter(|p| p.a() > 200).collect();
        assert!(!lit.is_empty(), "nothing was drawn opaquely");
        for p in lit {
            // Premultiplied, so at full alpha an opaque white pixel is 255 across.
            assert!(
                p.r() > 200 && p.g() > 200 && p.b() > 200,
                "an icon rasterised dark: {p:?}"
            );
        }
    }
}
