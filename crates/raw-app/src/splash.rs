//! The start page: what the app shows when nothing is in front of you.
//!
//! Three places draw it. Lightbox with no folder chosen puts it where the grid would
//! be; Develop with no image open puts it in the viewport; and the Home button at the
//! right of the title strip lays it over Develop's open tabs without closing any of
//! them. Each mode has its own page, because each is a different errand: Lightbox
//! offers folders and Develop offers images. Both stand on the same ground and under
//! the same header.
//!
//! # The ground is a sensor
//!
//! A flat field of the Bayer mosaic at 200%: every photosite two points square, red,
//! green, green, blue, drawn as grays. That is what a raw file is before this app has
//! done anything to it — the CFA mosaic `SensorImage` holds — and so it is a texture
//! that belongs to monopro and nothing else.
//!
//! It is drawn from one tile, 128 points square, whose texels are **device pixels**:
//! the tile is rasterized for the display's scale and laid down on whole pixels, so
//! every texel lands on the pixel it was made for. Relying on a sampler's repeat mode
//! was the first version, and it is not safe — egui's predictable filtering, which
//! the visual tests run under, clamps every lookup and ignores the sampler — so the
//! field is instead the tile laid edge to edge as one mesh, a few hundred quads on the
//! largest window.
//!
//! The grays are the maintainer's, chosen on the design canvas: green 43, red 31,
//! blue 23. Green is the light cell because on a real sensor it collects about twice
//! what red and blue do, which is why an undemosaiced raw reads as a checkerboard.
//! There is no fade toward the middle. An earlier study faded the pattern out under
//! the text, and the maintainer preferred the field flat.
//!
//! **It does not take the viewer's ground.** The start page is a title card. It
//! looks the same whichever grays the panels and canvas are set to, the way a
//! window's title strip does.
//!
//! # Recent files are memory, not settings
//!
//! [`Recent`] is the ten folders and ten images opened most lately. Nobody configures
//! it, so it lives in eframe's storage beside `open.last_dir` rather than in
//! `settings.toml`; see `App::save`. A path that has gone by the next launch is
//! dropped when the list is read back, so the page never offers a row that cannot
//! open.

use std::path::{Path, PathBuf};

use egui::{Color32, FontId, Pos2, Rect, Sense, Vec2, pos2, vec2};

use crate::hotkeys;
use crate::icons::{self, Icons};
use crate::theme;

/// The mosaic's grays: green, red and blue photosites. See the module note.
pub const MOSAIC: [u8; 3] = [43, 31, 23];

/// One photosite, in points. Two, so the field reads as texture rather than as tiles.
const CELL: f32 = 2.0;
/// The tile the field is laid from, in points. A whole number of 2 × 2 cells, and a
/// whole number of device pixels at every scale macOS and Windows offer.
const TILE: f32 = 128.0;

/// The mark: the app icon's pixel eye, one cell per photosite.
///
/// An outer ring, and a pupil whose four sides touch only at their corners. Painted
/// as a mesh of cells rather than loaded from an SVG, because the icon pipeline
/// rasterizes a square and this is 13 × 9, and because cells painted edge to edge
/// with no antialiasing stay crisp at any size a whole number of points divides.
pub const MARK: [&str; 9] = [
    "...#######...",
    "..##.....##..",
    ".##..###..##.",
    "##..#...#..##",
    "#...#...#...#",
    "##..#...#..##",
    ".##..###..##.",
    "..##.....##..",
    "...#######...",
];

/// The content column's width, in points. Zed's welcome page is about this wide,
/// and it is the width at which a folder name and its location share one row.
const COLUMN: f32 = 440.0;
/// A clickable row's height.
const ROW_H: f32 = 28.0;
/// The mark's cell in the header: 13 × 9 cells at 4 points is 52 × 36.
const MARK_CELL: f32 = 4.0;
/// The header's height, which is the mark's.
const HEADER_H: f32 = 9.0 * MARK_CELL;
/// Space between the header and each group below it.
const GAP: f32 = 30.0;
/// A group's name and the rule beside it.
const TITLE_H: f32 = 20.0;
/// Between a group's name and its first row.
const TITLE_GAP: f32 = 4.0;
/// A caption line under a row.
const CAPTION_H: f32 = 18.0;
/// The rule above the bottom links, and the space under it.
const NAV_RULE: f32 = 11.0;
/// Inset of every row's contents from the column's edge.
const INSET: f32 = 8.0;
/// Where a row's label starts: past the inset, a 14 point icon and a gap.
const LABEL_X: f32 = INSET + 14.0 + 10.0;

/// Letter spacing for the group names, in points.
const TRACK: f32 = 1.5;

/// A row's name at rest. Brighter than `NAME`, dimmer than `BRIGHT`, which it takes
/// under the pointer.
const TEXT: Color32 = Color32::from_gray(200);
/// The rule beside a group name, and the one above the links: the panel rule.
const RULE: Color32 = Color32::from_gray(52);
/// A keycap's outline: the widget fill, so it reads as a key and not as a button.
const KEY_EDGE: Color32 = Color32::from_gray(58);
/// The dot between the bottom links.
const DOT: Color32 = Color32::from_gray(74);

/// How many of each [`Recent`] keeps, and the page lists.
pub const RECENT: usize = 10;

const FOLDERS_KEY: &str = "recent.folders";
const IMAGES_KEY: &str = "recent.images";

/// Folders and images opened lately, newest first. See the module note.
#[derive(Debug, Default)]
pub struct Recent {
    pub folders: Vec<PathBuf>,
    pub images: Vec<PathBuf>,
    /// Changed since it was last written, so `App` knows to persist.
    pub dirty: bool,
}

impl Recent {
    /// Read the lists back, dropping any path that has gone since they were written.
    pub fn restore(storage: &dyn eframe::Storage) -> Self {
        let read = |key| eframe::get_value::<Vec<PathBuf>>(storage, key).unwrap_or_default();
        Self {
            folders: keep(read(FOLDERS_KEY), Path::is_dir),
            images: keep(read(IMAGES_KEY), Path::is_file),
            dirty: false,
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, FOLDERS_KEY, &self.folders);
        eframe::set_value(storage, IMAGES_KEY, &self.images);
    }

    /// A folder was opened. Cheap when it is already first, so it can run every frame.
    pub fn folder(&mut self, dir: &Path) {
        self.dirty |= touch(&mut self.folders, dir);
    }

    /// An image was opened.
    pub fn image(&mut self, path: &Path) {
        self.dirty |= touch(&mut self.images, path);
    }
}

fn keep(list: Vec<PathBuf>, exists: fn(&Path) -> bool) -> Vec<PathBuf> {
    list.into_iter()
        .filter(|p| exists(p))
        .take(RECENT)
        .collect()
}

/// Put `path` first, once, and hold the list to [`RECENT`]. Whether anything moved.
fn touch(list: &mut Vec<PathBuf>, path: &Path) -> bool {
    if list.first().is_some_and(|p| p == path) {
        return false;
    }
    list.retain(|p| p != path);
    list.insert(0, path.to_path_buf());
    list.truncate(RECENT);
    true
}

/// Which page to draw.
pub enum Page<'a> {
    /// Lightbox with no folder chosen.
    Lightbox { folders: &'a [PathBuf] },
    /// Develop. `back` names the open image when the page is laid over the tabs by
    /// Home, and is `None` when there is no image open at all.
    Develop {
        images: &'a [PathBuf],
        back: Option<&'a str>,
    },
}

/// What a click on the page asks for. `App` carries it out after the frame's panels
/// have drawn, because most of it needs more of the app than a page is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Go {
    /// Open this folder in Lightbox.
    Folder(PathBuf),
    /// Open this image in Develop, or bring its tab forward.
    Image(PathBuf),
    /// Bring the folder tree forward.
    Browse,
    /// The open dialog.
    Open,
    Lightbox,
    Develop,
    Settings,
    Shortcuts,
    /// Close the page Home laid over the tabs.
    Back,
}

/// Draw the page over the whole of `ui`'s rect.
pub fn show(ui: &mut egui::Ui, icons: &Icons, page: Page<'_>) -> Option<Go> {
    let area = ui.max_rect();
    paint_mosaic(ui, area);
    let mut go = None;

    if let Page::Develop {
        back: Some(name), ..
    } = page
    {
        let label = format!("Back to {name}");
        let size = link_size(ui, &label, "esc");
        let at = pos2(area.right() - 10.0 - size.x, area.top() + 10.0);
        if link(ui, Rect::from_min_size(at, size), &label, "esc").clicked() {
            go = Some(Go::Back);
        }
    }

    let width = COLUMN.min(area.width() - 32.0).max(0.0);
    let height = height_of(&page);
    // Centered, but never above the top: a short window keeps the header and loses
    // the last rows instead.
    let top = (area.center().y - height * 0.5).max(area.top() + 16.0);
    let column = Rect::from_min_size(
        pos2(area.center().x - width * 0.5, top),
        vec2(width, height),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(column)
            .layout(egui::Layout::top_down(egui::Align::Min)),
        |ui| {
            ui.set_clip_rect(area);
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            header(ui);
            ui.add_space(GAP);
            let picked = match page {
                Page::Lightbox { folders } => lightbox_page(ui, icons, folders),
                Page::Develop { images, .. } => develop_page(ui, icons, images),
            };
            go = go.take().or(picked);
        },
    );
    go
}

/// The column's height, so it can be centered before it is laid out.
fn height_of(page: &Page<'_>) -> f32 {
    let (start, listed) = match page {
        Page::Lightbox { folders } => (ROW_H, folders.len()),
        Page::Develop { images, .. } => (ROW_H + CAPTION_H, images.len()),
    };
    let recent = if listed == 0 {
        CAPTION_H
    } else {
        listed as f32 * ROW_H
    };
    HEADER_H
        + GAP
        + TITLE_H
        + TITLE_GAP
        + start
        + GAP
        + TITLE_H
        + TITLE_GAP
        + recent
        + GAP
        + NAV_RULE
        + ROW_H
}

fn lightbox_page(ui: &mut egui::Ui, icons: &Icons, folders: &[PathBuf]) -> Option<Go> {
    let mut go = None;
    title(ui, "START");
    if row(
        ui,
        icons,
        "tree-view",
        "Use the folder tree to start",
        Meta::None,
    )
    .clicked()
    {
        go = Some(Go::Browse);
    }
    ui.add_space(GAP);
    title(ui, "RECENT FOLDERS");
    if folders.is_empty() {
        caption(ui, "Folders you open are listed here");
    }
    for dir in folders {
        let name = dir.file_name().map_or_else(
            || dir.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let place = dir.parent().map(tilde).unwrap_or_default();
        if row(ui, icons, "folder", &name, Meta::Text(&place)).clicked() {
            go = Some(Go::Folder(dir.clone()));
        }
    }
    ui.add_space(GAP);
    let picked = links(
        ui,
        &[
            ("Develop", hotkeys::Action::Develop, Go::Develop),
            ("Settings", hotkeys::Action::Settings, Go::Settings),
            (
                "Keyboard Shortcuts",
                hotkeys::Action::HotkeyHud,
                Go::Shortcuts,
            ),
        ],
    );
    go.or(picked)
}

fn develop_page(ui: &mut egui::Ui, icons: &Icons, images: &[PathBuf]) -> Option<Go> {
    let mut go = None;
    title(ui, "START");
    let open = chord(hotkeys::Action::OpenFile);
    if row(ui, icons, "camera-plus", "Open Image", Meta::Key(&open)).clicked() {
        go = Some(Go::Open);
    }
    caption(ui, "or drop a raw file on the window");
    ui.add_space(GAP);
    title(ui, "RECENT IMAGES");
    if images.is_empty() {
        caption(ui, "Images you open are listed here");
    }
    for path in images {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let folder = path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if row(ui, icons, "file-image", &name, Meta::Text(&folder)).clicked() {
            go = Some(Go::Image(path.clone()));
        }
    }
    ui.add_space(GAP);
    let picked = links(
        ui,
        &[
            ("Lightbox", hotkeys::Action::Lightbox, Go::Lightbox),
            ("Settings", hotkeys::Action::Settings, Go::Settings),
            (
                "Keyboard Shortcuts",
                hotkeys::Action::HotkeyHud,
                Go::Shortcuts,
            ),
        ],
    );
    go.or(picked)
}

/// The mark and the name beside it.
fn header(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
    // On whole device pixels, or every cell edge is a soft one.
    let ppp = ui.ctx().pixels_per_point();
    let mark = pos2(
        ((rect.left() + INSET) * ppp).round() / ppp,
        (rect.top() * ppp).round() / ppp,
    );
    let painter = ui.painter();
    paint_mark(painter, mark, MARK_CELL, theme::BRIGHT);

    let name = painter.layout_no_wrap(
        "monopro".to_owned(),
        FontId::proportional(30.0),
        theme::BRIGHT,
    );
    let sub = painter.layout_no_wrap(
        format!("monochrome raw processor · {}", env!("CARGO_PKG_VERSION")),
        FontId::proportional(theme::size::FOOTER),
        theme::DIM,
    );
    let gap = 4.0;
    let x = mark.x + 13.0 * MARK_CELL + 18.0;
    let y = rect.center().y - (name.size().y + gap + sub.size().y) * 0.5;
    painter.galley(pos2(x, y), name.clone(), theme::BRIGHT);
    painter.galley(pos2(x, y + name.size().y + gap), sub, theme::DIM);
}

/// A group's name in tracked caps, and a rule running from it to the column's edge.
fn title(ui: &mut egui::Ui, label: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_H), Sense::hover());
    let painter = ui.painter();
    let font = FontId::proportional(theme::size::CAPTION);
    let mut x = rect.left() + INSET;
    for c in label.chars() {
        let glyph = painter.layout_no_wrap(c.to_string(), font.clone(), theme::DIM);
        let w = glyph.size().x;
        painter.galley(
            pos2(x, rect.center().y - glyph.size().y * 0.5),
            glyph,
            theme::DIM,
        );
        x += w + TRACK;
    }
    let y = painter.round_to_pixel_center(rect.center().y);
    painter.hline(
        (x + 10.0 - TRACK)..=(rect.right() - INSET),
        y,
        egui::Stroke::new(1.0, RULE),
    );
    ui.add_space(TITLE_GAP);
}

fn caption(ui: &mut egui::Ui, text: &str) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), CAPTION_H), Sense::hover());
    ui.painter().text(
        pos2(rect.left() + LABEL_X, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::proportional(theme::size::CAPTION),
        theme::DIM,
    );
}

/// What sits at the right end of a row.
enum Meta<'a> {
    None,
    /// Where the thing is, in the dim gray.
    Text(&'a str),
    /// The key that does the same thing.
    Key(&'a str),
}

/// A full-width row: icon, name, and a place or a key at the right.
fn row(
    ui: &mut egui::Ui,
    icons: &Icons,
    icon: &str,
    label: &str,
    meta: Meta<'_>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let hot = resp.hovered();
    let painter = ui.painter();
    if hot {
        painter.rect_filled(rect, 2.0, theme::CHROME_DEEP);
    }
    let glyph = Rect::from_center_size(
        pos2(rect.left() + INSET + 7.0, rect.center().y),
        Vec2::splat(14.0),
    );
    icons::paint_at(
        ui,
        icons,
        icon,
        "·",
        glyph,
        if hot { theme::BRIGHT } else { theme::NAME },
        13.0,
    );

    let mut right = rect.right() - INSET;
    match meta {
        Meta::None => {}
        Meta::Text(text) => {
            let font = FontId::proportional(theme::size::FOOTER);
            let fitted = crate::lightbox::elide(ui, text, &font, rect.width() * 0.45);
            let galley = painter.layout_no_wrap(fitted, font, theme::DIM);
            let at = pos2(
                right - galley.size().x,
                rect.center().y - galley.size().y * 0.5,
            );
            right = at.x - 16.0;
            painter.galley(at, galley, theme::DIM);
        }
        Meta::Key(key) => {
            let size = keycap_size(ui, key);
            let at =
                Rect::from_min_size(pos2(right - size.x, rect.center().y - size.y * 0.5), size);
            keycap(ui, at, key);
            right = at.left() - 16.0;
        }
    }

    let font = FontId::proportional(theme::size::BODY);
    let left = rect.left() + LABEL_X;
    let fitted = crate::lightbox::elide(ui, label, &font, right - left);
    let ink = if hot { theme::BRIGHT } else { TEXT };
    let galley = painter.layout_no_wrap(fitted, font, ink);
    painter.galley(
        pos2(left, rect.center().y - galley.size().y * 0.5),
        galley,
        ink,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The bottom row: a rule, then each link with its key, separated by dots.
fn links(ui: &mut egui::Ui, items: &[(&str, hotkeys::Action, Go)]) -> Option<Go> {
    let (rule, _) = ui.allocate_exact_size(vec2(ui.available_width(), NAV_RULE), Sense::hover());
    let y = ui.painter().round_to_pixel_center(rule.top());
    ui.painter()
        .hline(rule.x_range(), y, egui::Stroke::new(1.0, RULE));
    let (strip, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let mut go = None;
    let mut x = strip.left();
    for (i, (label, action, target)) in items.iter().enumerate() {
        if i > 0 {
            ui.painter().text(
                pos2(x + 4.0, strip.center().y),
                egui::Align2::LEFT_CENTER,
                "•",
                FontId::proportional(theme::size::BODY),
                DOT,
            );
            x += 16.0;
        }
        let key = chord(*action);
        let size = link_size(ui, label, &key);
        if link(
            ui,
            Rect::from_min_size(pos2(x, strip.top()), size),
            label,
            &key,
        )
        .clicked()
        {
            go = Some(target.clone());
        }
        x += size.x;
    }
    go
}

fn link_size(ui: &egui::Ui, label: &str, key: &str) -> Vec2 {
    let text = ui
        .painter()
        .layout_no_wrap(
            label.to_owned(),
            FontId::proportional(theme::size::BODY),
            TEXT,
        )
        .size()
        .x;
    vec2(INSET + text + 8.0 + keycap_size(ui, key).x + INSET, ROW_H)
}

/// A word and its key, as one target.
fn link(ui: &mut egui::Ui, rect: Rect, label: &str, key: &str) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(("splash-link", label)), Sense::click());
    let hot = resp.hovered();
    let painter = ui.painter();
    if hot {
        painter.rect_filled(rect, 2.0, theme::CHROME_DEEP);
    }
    let ink = if hot { theme::BRIGHT } else { TEXT };
    let galley = painter.layout_no_wrap(
        label.to_owned(),
        FontId::proportional(theme::size::BODY),
        ink,
    );
    let text_at = pos2(rect.left() + INSET, rect.center().y - galley.size().y * 0.5);
    let text_w = galley.size().x;
    painter.galley(text_at, galley, ink);
    let size = keycap_size(ui, key);
    keycap(
        ui,
        Rect::from_min_size(
            pos2(text_at.x + text_w + 8.0, rect.center().y - size.y * 0.5),
            size,
        ),
        key,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn keycap_size(ui: &egui::Ui, key: &str) -> Vec2 {
    let w = ui
        .painter()
        .layout_no_wrap(
            key.to_owned(),
            FontId::proportional(theme::size::CAPTION),
            theme::DIM,
        )
        .size()
        .x;
    vec2((w + 8.0).max(16.0), 16.0)
}

fn keycap(ui: &egui::Ui, rect: Rect, key: &str) {
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, KEY_EDGE),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        key,
        FontId::proportional(theme::size::CAPTION),
        theme::DIM,
    );
}

/// The key bound to `action`, as it is printed on a keyboard: `⌘O`, not `⌘o`.
pub(crate) fn chord(action: hotkeys::Action) -> String {
    hotkeys::TABLE
        .iter()
        .find(|binding| binding.action == action)
        .map(|binding| binding.chord().to_uppercase())
        .unwrap_or_default()
}

/// A folder's location, with the home folder written `~`.
fn tilde(dir: &Path) -> String {
    if let Some(home) = crate::platform::home_dir()
        && let Ok(rest) = dir.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    dir.display().to_string()
}

/// The field of photosites under the page.
///
/// Tiles start on multiples of [`TILE`] measured from the screen's origin, so the
/// cells sit on one grid whatever shape the pane is, and a tile's corner is always a
/// whole device pixel.
fn paint_mosaic(ui: &egui::Ui, rect: Rect) {
    let ppp = ui.ctx().pixels_per_point();
    let texture = mosaic_texture(ui.ctx(), ppp);
    let mut mesh = egui::Mesh::with_texture(texture.id());
    let unit = Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
    let mut y = (rect.top() / TILE).floor() * TILE;
    while y < rect.bottom() {
        let mut x = (rect.left() / TILE).floor() * TILE;
        while x < rect.right() {
            mesh.add_rect_with_uv(
                Rect::from_min_size(pos2(x, y), Vec2::splat(TILE)),
                unit,
                Color32::WHITE,
            );
            x += TILE;
        }
        y += TILE;
    }
    ui.painter_at(rect).add(egui::Shape::mesh(mesh));
}

/// The tile, rasterized for `ppp` and kept in egui's memory until the scale changes.
fn mosaic_texture(ctx: &egui::Context, ppp: f32) -> egui::TextureHandle {
    let id = egui::Id::new("splash-mosaic");
    if let Some((at, texture)) = ctx.data(|d| d.get_temp::<(f32, egui::TextureHandle)>(id))
        && at == ppp
    {
        return texture;
    }
    let texture = ctx.load_texture(
        "splash-mosaic",
        mosaic_tile(ppp),
        egui::TextureOptions::NEAREST,
    );
    ctx.data_mut(|d| d.insert_temp(id, (ppp, texture.clone())));
    texture
}

/// One [`TILE`] of photosites, one texel per device pixel.
///
/// Row by row, red first: the RGGB phase most sensors in the corpus share.
fn mosaic_tile(ppp: f32) -> egui::ColorImage {
    let [g, r, b] = MOSAIC.map(Color32::from_gray);
    let quad = [[r, g], [g, b]];
    let n = (TILE * ppp).round().max(1.0) as usize;
    // Which cell a texel's center falls in, counted in points.
    let cell = |i: usize| (((i as f32 + 0.5) / ppp / CELL) as usize) % 2;
    let pixels = (0..n * n).map(|i| quad[cell(i / n)][cell(i % n)]).collect();
    egui::ColorImage::new([n, n], pixels)
}

/// The mark's cells as one mesh, with its top-left at `origin`.
///
/// A mesh rather than `rect_filled`, because egui feathers every filled rect and two
/// feathered edges meeting leave a faint seam between cells. Mesh quads have no
/// feathering, so a row of cells is one solid run.
pub fn paint_mark(painter: &egui::Painter, origin: Pos2, cell: f32, color: Color32) {
    let mut mesh = egui::Mesh::default();
    for (y, line) in MARK.iter().enumerate() {
        for (x, c) in line.chars().enumerate() {
            if c == '#' {
                let min = origin + vec2(x as f32 * cell, y as f32 * cell);
                mesh.add_colored_rect(Rect::from_min_size(min, Vec2::splat(cell)), color);
            }
        }
    }
    painter.add(egui::Shape::mesh(mesh));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn opening_again_moves_a_path_to_the_front_without_a_duplicate() {
        let mut list = paths(&["/a", "/b", "/c"]);
        assert!(touch(&mut list, Path::new("/c")));
        assert_eq!(list, paths(&["/c", "/a", "/b"]));
    }

    #[test]
    fn the_path_already_first_changes_nothing() {
        let mut recent = Recent {
            folders: paths(&["/a", "/b"]),
            ..Recent::default()
        };
        recent.folder(Path::new("/a"));
        assert!(
            !recent.dirty,
            "a folder that stays open must not rewrite storage"
        );
        recent.folder(Path::new("/b"));
        assert!(recent.dirty);
        assert_eq!(recent.folders, paths(&["/b", "/a"]));
    }

    #[test]
    fn the_list_keeps_the_ten_newest() {
        let mut list = Vec::new();
        for i in 0..14 {
            touch(&mut list, Path::new(&format!("/{i}")));
        }
        assert_eq!(list.len(), RECENT);
        assert_eq!(list.first(), Some(&PathBuf::from("/13")));
        assert_eq!(list.last(), Some(&PathBuf::from("/4")));
    }

    #[test]
    fn a_path_that_has_gone_is_not_offered() {
        let here = std::env::temp_dir();
        let gone = here.join("monopro-recent-that-does-not-exist");
        let kept = keep(vec![gone, here.clone()], Path::is_dir);
        assert_eq!(kept, vec![here]);
    }

    #[test]
    fn the_mark_is_the_icons_eye_and_is_symmetric() {
        assert!(MARK.iter().all(|row| row.len() == 13));
        for row in MARK {
            assert_eq!(row, row.chars().rev().collect::<String>(), "left to right");
        }
        for (top, bottom) in MARK.iter().zip(MARK.iter().rev()) {
            assert_eq!(top, bottom, "top to bottom");
        }
        // The pupil is hollow: its middle three by three is empty.
        for row in &MARK[3..6] {
            assert_eq!(&row[5..8], "...");
        }
    }

    #[test]
    fn a_tile_is_whole_cells_on_whole_pixels_at_every_common_scale() {
        let [g, r, b] = MOSAIC.map(Color32::from_gray);
        for ppp in [1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            let tile = mosaic_tile(ppp);
            let n = tile.size[0];
            assert_eq!(
                n as f32,
                TILE * ppp,
                "{ppp}: a tile is a whole number of pixels"
            );
            // The tile's own edges continue the pattern, so tiles meet without a seam:
            // the first cell is red and the last is blue, whatever the scale.
            assert_eq!(tile.pixels[0], r, "{ppp}");
            assert_eq!(tile.pixels[n * n - 1], b, "{ppp}");
            // Two points in, the next cell along is green.
            let step = (CELL * ppp).ceil() as usize;
            assert_eq!(tile.pixels[step], g, "{ppp}");
        }
    }

    #[test]
    fn green_is_the_light_cell_and_blue_the_dark_one() {
        let [g, r, b] = MOSAIC;
        assert!(g > r && r > b);
    }

    #[test]
    fn keys_are_printed_the_way_a_keyboard_prints_them() {
        assert_eq!(chord(hotkeys::Action::OpenFile), "⌘O");
        assert_eq!(chord(hotkeys::Action::Lightbox), "L");
        assert_eq!(chord(hotkeys::Action::Home), "⌥H");
    }

    #[test]
    fn a_home_folder_location_is_written_with_a_tilde() {
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        assert_eq!(tilde(&home), "~");
        let below = Path::new("Pictures").join("2026");
        assert_eq!(tilde(&home.join(&below)), format!("~/{}", below.display()));
        assert_eq!(tilde(Path::new("/Volumes/Archive")), "/Volumes/Archive");
    }

    #[test]
    fn the_column_is_as_tall_as_what_it_lists() {
        let ten = paths(&["/1", "/2", "/3", "/4", "/5", "/6", "/7", "/8", "/9", "/10"]);
        let full = height_of(&Page::Lightbox { folders: &ten });
        let empty = height_of(&Page::Lightbox { folders: &[] });
        assert_eq!(full - empty, 10.0 * ROW_H - CAPTION_H);
        let develop = height_of(&Page::Develop {
            images: &ten,
            back: None,
        });
        assert_eq!(
            develop - full,
            CAPTION_H,
            "Develop's start has a caption under it"
        );
    }
}
