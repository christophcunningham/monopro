//! Contact-sheet planning and PDF export for Lightbox.
//!
//! The sheet is deliberately a modal rather than another Settings page: it is a
//! one-off document assembled from the order currently visible in Lightbox. The
//! dialog previews layout quickly from browser-size images; the export thread does
//! its own decode and PDF work so a large folder never freezes the grid.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Content, Finish, Name, Pdf, Rect, Ref, Str, TextStr};

use crate::{export::Space, iptc_templates, theme};
use raw_core::{Unit, sidecar::IptcField};

const UI_FACE: &[u8] = include_bytes!("../../../fonts/JetBrainsMono/JetBrainsMono-Regular.ttf");
const MIN_TEXT_PT: f32 = 5.0;

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub name: String,
    pub rating: i32,
    pub label: Option<String>,
    /// Lightbox's display rotation, in clockwise quarter turns.
    pub orientation: Option<raw_core::Orientation>,
}

#[derive(Debug, Clone)]
pub struct Sources {
    pub selected: Vec<Source>,
    pub visible: Vec<Source>,
    pub folder: Vec<Source>,
    pub cache: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Selected,
    Visible,
    Folder,
}

impl Scope {
    const ALL: [Self; 3] = [Self::Selected, Self::Visible, Self::Folder];

    fn label(self) -> &'static str {
        match self {
            Self::Selected => "Selected",
            Self::Visible => "Visible",
            Self::Folder => "Entire Folder",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Preview {
    Developed,
    Gray,
    Color,
}

impl Preview {
    const ALL: [Self; 3] = [Self::Developed, Self::Gray, Self::Color];

    fn label(self) -> &'static str {
        match self {
            Self::Developed => "Developed",
            Self::Gray => "Gray",
            Self::Color => "Color",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paper {
    Letter,
    Tabloid,
    A4,
    A3,
    EightTen,
    ThirteenNineteen,
    Custom,
}

impl Paper {
    const ALL: [Self; 7] = [
        Self::Letter,
        Self::Tabloid,
        Self::A4,
        Self::A3,
        Self::EightTen,
        Self::ThirteenNineteen,
        Self::Custom,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Letter => "US Letter",
            Self::Tabloid => "Tabloid · 11 × 17",
            Self::A4 => "A4",
            Self::A3 => "A3",
            Self::EightTen => "8 × 10",
            Self::ThirteenNineteen => "13 × 19",
            Self::Custom => "Custom",
        }
    }

    fn inches(self, custom: [f32; 2]) -> [f32; 2] {
        match self {
            Self::Letter => [8.5, 11.0],
            Self::Tabloid => [11.0, 17.0],
            Self::A4 => [8.2677, 11.6929],
            Self::A3 => [11.6929, 16.5354],
            Self::EightTen => [8.0, 10.0],
            Self::ThirteenNineteen => [13.0, 19.0],
            Self::Custom => custom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Portrait,
    Landscape,
}

#[derive(Debug, Clone, Copy)]
struct PageMargins {
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

impl PageMargins {
    fn uniform(inches: f32) -> Self {
        Self {
            top: inches,
            bottom: inches,
            left: inches,
            right: inches,
        }
    }

    fn set_all(&mut self, inches: f32) {
        *self = Self::uniform(inches);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextPosition {
    Left,
    Center,
    Right,
}

// No `ALL` and no `label()` on either of these. The two icon rows that choose them
// carry the order and the wording, and the wording is better there: a header's tip
// can say "Upper left" where a shared `label()` could only say "Left".

/// Which band the page number prints in, or neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageNumber {
    Off,
    Header,
    Footer,
}

impl PageNumber {
    const ALL: [Self; 3] = [Self::Off, Self::Header, Self::Footer];

    fn label(self) -> &'static str {
        match self {
            Self::Off => "None",
            Self::Header => "Header",
            Self::Footer => "Footer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptionSource {
    None,
    Filename,
    Sequence,
    Custom,
}

impl CaptionSource {
    const ALL: [Self; 4] = [Self::None, Self::Filename, Self::Sequence, Self::Custom];

    fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Filename => "Filename",
            Self::Sequence => "Sequence number",
            Self::Custom => "Custom text",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextAlignment {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone)]
struct FontChoice {
    label: String,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Settings {
    scope: Scope,
    preview: Preview,
    paper: Paper,
    orientation: Orientation,
    unit: Unit,
    custom_inches: [f32; 2],
    margins_inches: PageMargins,
    margins_linked: bool,
    vertical_spacing_inches: f32,
    horizontal_spacing_inches: f32,
    columns: usize,
    rows: usize,
    rotate_verticals: bool,
    black_background: bool,
    caption_source: CaptionSource,
    include_file_extension: bool,
    caption_pattern: String,
    caption_align: TextAlignment,
    caption_gap: f32,
    sequence_start: u32,
    sequence_digits: usize,
    rating: bool,
    color_label: bool,
    header: String,
    header_position: TextPosition,
    header_size: f32,
    header_first_page_only: bool,
    footer: String,
    footer_position: TextPosition,
    footer_size: f32,
    footer_first_page_only: bool,
    page_number: PageNumber,
    page_number_position: TextPosition,
    page_number_size: f32,
    caption_font_index: usize,
    header_font_index: usize,
    footer_font_index: usize,
    page_number_font_index: usize,
    caption_size: f32,
    profile: Space,
    resolution: u16,
    metadata_template: Option<usize>,
}

impl Settings {
    fn page_points(&self) -> [f32; 2] {
        let mut inches = self.paper.inches(self.custom_inches);
        if self.orientation == Orientation::Landscape {
            inches.swap(0, 1);
        }
        [inches[0] * 72.0, inches[1] * 72.0]
    }
}

pub enum DialogResponse {
    Cancel,
    Exported(PathBuf),
}

struct PreviewResult {
    generation: u64,
    images: Vec<Option<raw_core::preview::Rgb8>>,
}

pub struct Dialog {
    sources: Sources,
    settings: Settings,
    fonts: Vec<FontChoice>,
    iptc_templates: iptc_templates::Store,
    iptc_error: Option<String>,
    error: Option<String>,
    exporting: bool,
    export_rx: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    preview_rx: Option<mpsc::Receiver<PreviewResult>>,
    preview_generation: u64,
    preview_textures: Vec<Option<egui::TextureHandle>>,
    /// Which page the preview is showing. The export writes them all; this is only
    /// the one being looked at.
    preview_page: usize,
    preview_signature: Option<(Scope, Preview, usize, usize, bool, usize)>,
}

impl Dialog {
    pub fn new(sources: Sources, unit: Unit) -> Self {
        let scope = if sources.selected.is_empty() {
            Scope::Visible
        } else {
            Scope::Selected
        };
        let (iptc_templates, iptc_error) = iptc_templates::Store::load();
        Self {
            sources,
            settings: Settings {
                scope,
                preview: Preview::Developed,
                paper: Paper::Letter,
                orientation: Orientation::Portrait,
                unit,
                custom_inches: [8.5, 11.0],
                margins_inches: PageMargins::uniform(0.4),
                margins_linked: true,
                vertical_spacing_inches: 0.16,
                horizontal_spacing_inches: 0.16,
                columns: 5,
                rows: 7,
                rotate_verticals: false,
                black_background: false,
                // **None by default.** A contact sheet's job is the pictures; a
                // filename under every one of forty frames is a page of text with
                // photographs in it. the maintainer's call — the caption is a thing you turn on
                // when you need to read something back, not the resting state.
                caption_source: CaptionSource::None,
                include_file_extension: false,
                caption_pattern: "{original}".to_owned(),
                caption_align: TextAlignment::Center,
                caption_gap: 3.0,
                sequence_start: 1,
                sequence_digits: 3,
                rating: false,
                color_label: false,
                header: String::new(),
                header_position: TextPosition::Left,
                header_size: 6.0,
                header_first_page_only: false,
                footer: String::new(),
                footer_position: TextPosition::Left,
                footer_size: 6.0,
                footer_first_page_only: false,
                page_number: PageNumber::Off,
                page_number_position: TextPosition::Right,
                page_number_size: 6.0,
                caption_font_index: 0,
                header_font_index: 0,
                footer_font_index: 0,
                page_number_font_index: 0,
                caption_size: 6.0,
                profile: Space::Srgb,
                resolution: 200,
                metadata_template: None,
            },
            fonts: system_fonts(),
            iptc_templates,
            iptc_error,
            error: None,
            exporting: false,
            export_rx: None,
            preview_rx: None,
            preview_generation: 0,
            preview_textures: Vec::new(),
            preview_page: 0,
            preview_signature: None,
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        icons: &crate::icons::Icons,
    ) -> Option<DialogResponse> {
        if let Some(answer) = self.poll_export(ctx) {
            return Some(answer);
        }
        self.poll_preview(ctx);
        self.request_preview(ctx);

        let mut answer = None;
        let escape = ctx.input(|input| input.key_pressed(egui::Key::Escape));
        egui::Modal::new(egui::Id::new("lightbox-contact-sheet"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(theme::CHROME)
                    .stroke(egui::Stroke::new(2.0, theme::RUBY))
                    .corner_radius(2.0)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_min_size(egui::vec2(1060.0, 720.0));
                ui.set_max_size(egui::vec2(1060.0, 720.0));
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("CONTACT SHEET")
                            .size(18.0)
                            .color(theme::BRIGHT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("×").clicked() {
                            answer = Some(DialogResponse::Cancel);
                        }
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(12.0);

                ui.columns_const(|[controls, preview]| {
                    egui::ScrollArea::vertical()
                        .id_salt("contact-sheet-controls")
                        .show(controls, |ui| self.controls_ui(ui, icons));
                    self.preview_ui(preview);
                });

                if let Some(error) = &self.error {
                    ui.label(egui::RichText::new(error).color(theme::RUBY));
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            answer = Some(DialogResponse::Cancel);
                        }
                        if theme::compact_action_button(
                            ui,
                            if self.exporting {
                                "Exporting…"
                            } else {
                                "Export PDF…"
                            },
                            theme::RUBY_FILL_DIM,
                            theme::RUBY,
                            104.0,
                            !self.exporting,
                        )
                        .clicked()
                        {
                            self.begin_export();
                        }
                    });
                });
            });

        if escape && !self.exporting {
            Some(DialogResponse::Cancel)
        } else {
            answer
        }
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) {
        ui.set_width(470.0);
        section(ui, "CONTENT");
        combo(ui, "Scope", self.settings.scope.label(), |ui| {
            for scope in Scope::ALL {
                let count = self.items_for(scope).len();
                if ui
                    .selectable_value(
                        &mut self.settings.scope,
                        scope,
                        format!("{} · {count}", scope.label()),
                    )
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        combo(ui, "Preview", self.settings.preview.label(), |ui| {
            for mode in Preview::ALL {
                if ui
                    .selectable_value(&mut self.settings.preview, mode, mode.label())
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        if self.settings.preview == Preview::Color && self.settings.profile == Space::Monostar {
            self.settings.profile = Space::Srgb;
        }
        toggle(ui, "Rotate verticals", &mut self.settings.rotate_verticals);

        section(ui, "PAGE");
        combo(ui, "Paper", self.settings.paper.label(), |ui| {
            for paper in Paper::ALL {
                if ui
                    .selectable_value(&mut self.settings.paper, paper, paper.label())
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        row(ui, "Orientation", |ui| {
            let landscape = self.settings.orientation == Orientation::Landscape;
            if crate::icons::toggle(
                ui,
                icons,
                "device-rotate",
                "↕",
                landscape,
                crate::icons::BIG,
            )
            .on_hover_text(theme::tip(if landscape {
                "Landscape · click for portrait"
            } else {
                "Portrait · click for landscape"
            }))
            .clicked()
            {
                self.settings.orientation = if landscape {
                    Orientation::Portrait
                } else {
                    Orientation::Landscape
                };
            }
            ui.label(if landscape { "Landscape" } else { "Portrait" });
        });
        if self.settings.paper == Paper::Custom {
            pair_inches(
                ui,
                "Size",
                &mut self.settings.custom_inches,
                1.0..=100.0,
                self.settings.unit,
            );
        }
        row(ui, "Link margins", |ui| {
            let linked = self.settings.margins_linked;
            if crate::icons::toggle(
                ui,
                icons,
                if linked { "link" } else { "link-break" },
                if linked { "=" } else { "≠" },
                linked,
                crate::icons::BIG,
            )
            .on_hover_text(theme::tip(if linked {
                "One margin on all four sides · click to set them separately"
            } else {
                "Four margins · click to hold them together"
            }))
            .clicked()
            {
                self.settings.margins_linked = !linked;
            }
        });
        let unit = self.settings.unit;
        // **Linked is one number, and now looks like one.** Four identical fields under
        // a checkbox that makes three of them echoes is the panel disagreeing with
        // itself; unlinked they are two pairs, and a pair belongs on a row.
        if self.settings.margins_linked {
            let mut all = self.settings.margins_inches.top;
            if value_inches(ui, "Margins", &mut all, 0.0..=3.0, unit) {
                self.settings.margins_inches.set_all(all);
            }
        } else {
            let margins = &mut self.settings.margins_inches;
            duo_inches(
                ui,
                "Margins",
                [("Top", &mut margins.top), ("Bottom", &mut margins.bottom)],
                0.0..=3.0,
                unit,
            );
            // Blank label: the second row of one control, not a control of its own.
            duo_inches(
                ui,
                "",
                [("Left", &mut margins.left), ("Right", &mut margins.right)],
                0.0..=3.0,
                unit,
            );
        }
        duo_inches(
            ui,
            "Spacing",
            [
                ("Vert", &mut self.settings.vertical_spacing_inches),
                ("Horz", &mut self.settings.horizontal_spacing_inches),
            ],
            0.0..=2.0,
            unit,
        );
        row(ui, "Grid", |ui| {
            ui.add(crate::widgets::bounded_number(
                &mut self.settings.columns,
                1..=12,
                1.0,
            ));
            ui.label("×");
            ui.add(crate::widgets::bounded_number(
                &mut self.settings.rows,
                1..=12,
                1.0,
            ));
        });
        let cell = PageLayout::new(&self.settings, self.settings.page_points()).cell_size();
        row(ui, "Cell size", |ui| {
            let factor = self.settings.unit.per_inch();
            ui.label(format!(
                "{:.2} × {:.2} {}",
                cell[0] / 72.0 * factor,
                cell[1] / 72.0 * factor,
                self.settings.unit.label()
            ));
        });
        toggle(ui, "Black background", &mut self.settings.black_background);

        section(ui, "CAPTIONS");
        combo(ui, "Content", self.settings.caption_source.label(), |ui| {
            for source in CaptionSource::ALL {
                ui.selectable_value(&mut self.settings.caption_source, source, source.label());
            }
        });
        if self.settings.caption_source == CaptionSource::Filename {
            toggle(
                ui,
                "File extension",
                &mut self.settings.include_file_extension,
            );
        }
        if self.settings.caption_source == CaptionSource::Custom {
            row(ui, "Custom", |ui| {
                ui.add_sized(
                    [300.0, 24.0],
                    egui::TextEdit::singleline(&mut self.settings.caption_pattern)
                        .vertical_align(egui::Align::Center),
                );
            });
            row(ui, "Tokens", |ui| {
                ui.horizontal_wrapped(|ui| {
                    for token in ["{filename}", "{original}", "{sequence}", "{folder}"] {
                        if ui.small_button(token).clicked() {
                            self.settings.caption_pattern.push_str(token);
                        }
                    }
                });
            });
        }
        if matches!(
            self.settings.caption_source,
            CaptionSource::Sequence | CaptionSource::Custom
        ) {
            row(ui, "Sequence", |ui| {
                ui.add(crate::widgets::bounded_number(
                    &mut self.settings.sequence_start,
                    0..=999_999,
                    1.0,
                ));
                ui.label("start");
                ui.add(crate::widgets::bounded_number(
                    &mut self.settings.sequence_digits,
                    1..=8,
                    1.0,
                ));
                ui.label("digits");
            });
        }
        align_row(
            ui,
            icons,
            "Alignment",
            &mut self.settings.caption_align,
            &[
                (TextAlignment::Left, "text-align-left", "L", "Left"),
                (TextAlignment::Center, "text-align-center", "C", "Center"),
                (TextAlignment::Right, "text-align-right", "R", "Right"),
            ],
        );
        toggle(ui, "Rating", &mut self.settings.rating);
        toggle(ui, "Color label", &mut self.settings.color_label);
        let caption_font = self
            .fonts
            .get(self.settings.caption_font_index)
            .map_or("JetBrains Mono", |font| font.label.as_str());
        combo_width(ui, "Caption font", caption_font, FONT_WIDTH, |ui| {
            for (index, font) in self.fonts.iter().enumerate() {
                if ui
                    .selectable_value(&mut self.settings.caption_font_index, index, &font.label)
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        value(
            ui,
            "Text size",
            &mut self.settings.caption_size,
            MIN_TEXT_PT..=24.0,
            "pt",
        );
        value(ui, "Gap", &mut self.settings.caption_gap, 0.0..=24.0, "pt");

        section(ui, "PAGE TEXT");
        ui.label(theme::caption("HEADER"));
        row(ui, "Text", |ui| {
            ui.add_sized(
                [300.0, 62.0],
                egui::TextEdit::multiline(&mut self.settings.header),
            );
        });
        row(ui, "Tokens", |ui| {
            for token in ["{page}", "{pages}"] {
                if ui.small_button(token).clicked() {
                    self.settings.header.push_str(token);
                }
            }
        });
        // Position rather than alignment, so the tips name the corner the block is
        // anchored to — the distinction `docs/lightbox-inspect.md` records after the
        // first mockup put the footer halfway up the page.
        align_row(
            ui,
            icons,
            "Header position",
            &mut self.settings.header_position,
            &[
                (TextPosition::Left, "text-align-left", "L", "Upper left"),
                (
                    TextPosition::Center,
                    "text-align-center",
                    "C",
                    "Upper center",
                ),
                (TextPosition::Right, "text-align-right", "R", "Upper right"),
            ],
        );
        let header_font = self
            .fonts
            .get(self.settings.header_font_index)
            .map_or("JetBrains Mono", |font| font.label.as_str());
        combo_width(ui, "Header font", header_font, FONT_WIDTH, |ui| {
            for (index, font) in self.fonts.iter().enumerate() {
                if ui
                    .selectable_value(&mut self.settings.header_font_index, index, &font.label)
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        value(
            ui,
            "Header size",
            &mut self.settings.header_size,
            MIN_TEXT_PT..=36.0,
            "pt",
        );
        toggle(
            ui,
            "First page only",
            &mut self.settings.header_first_page_only,
        );
        ui.add_space(8.0);
        ui.label(theme::caption("FOOTER"));
        row(ui, "Text", |ui| {
            ui.add_sized(
                [300.0, 62.0],
                egui::TextEdit::multiline(&mut self.settings.footer),
            );
        });
        row(ui, "Tokens", |ui| {
            for token in ["{page}", "{pages}"] {
                if ui.small_button(token).clicked() {
                    self.settings.footer.push_str(token);
                }
            }
        });
        align_row(
            ui,
            icons,
            "Footer position",
            &mut self.settings.footer_position,
            &[
                (TextPosition::Left, "text-align-left", "L", "Lower left"),
                (
                    TextPosition::Center,
                    "text-align-center",
                    "C",
                    "Lower center",
                ),
                (TextPosition::Right, "text-align-right", "R", "Lower right"),
            ],
        );
        let footer_font = self
            .fonts
            .get(self.settings.footer_font_index)
            .map_or("JetBrains Mono", |font| font.label.as_str());
        combo_width(ui, "Footer font", footer_font, FONT_WIDTH, |ui| {
            for (index, font) in self.fonts.iter().enumerate() {
                if ui
                    .selectable_value(&mut self.settings.footer_font_index, index, &font.label)
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        value(
            ui,
            "Footer size",
            &mut self.settings.footer_size,
            MIN_TEXT_PT..=36.0,
            "pt",
        );
        toggle(
            ui,
            "First page only",
            &mut self.settings.footer_first_page_only,
        );
        ui.add_space(8.0);
        ui.label(theme::caption("PAGE NUMBER"));
        combo(ui, "Placement", self.settings.page_number.label(), |ui| {
            for placement in PageNumber::ALL {
                if ui
                    .selectable_value(&mut self.settings.page_number, placement, placement.label())
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        if self.settings.page_number != PageNumber::Off {
            let header = self.settings.page_number == PageNumber::Header;
            // The row shows where the number *will* print, not what was chosen before
            // the band's text turned up and claimed that corner.
            let mut position = page_number_position(&self.settings);
            align_row_except(
                ui,
                icons,
                "Alignment",
                &mut position,
                &if header {
                    [
                        (TextPosition::Left, "text-align-left", "L", "Upper left"),
                        (
                            TextPosition::Center,
                            "text-align-center",
                            "C",
                            "Upper center",
                        ),
                        (TextPosition::Right, "text-align-right", "R", "Upper right"),
                    ]
                } else {
                    [
                        (TextPosition::Left, "text-align-left", "L", "Lower left"),
                        (
                            TextPosition::Center,
                            "text-align-center",
                            "C",
                            "Lower center",
                        ),
                        (TextPosition::Right, "text-align-right", "R", "Lower right"),
                    ]
                },
                page_number_blocked(&self.settings).map(|taken| {
                    (
                        taken,
                        if header {
                            "The header text is here"
                        } else {
                            "The footer text is here"
                        },
                    )
                }),
            );
            self.settings.page_number_position = position;
            let number_font = self
                .fonts
                .get(self.settings.page_number_font_index)
                .map_or("JetBrains Mono", |font| font.label.as_str());
            combo_width(ui, "Number font", number_font, FONT_WIDTH, |ui| {
                for (index, font) in self.fonts.iter().enumerate() {
                    if ui
                        .selectable_value(
                            &mut self.settings.page_number_font_index,
                            index,
                            &font.label,
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                }
            });
            value(
                ui,
                "Number size",
                &mut self.settings.page_number_size,
                MIN_TEXT_PT..=36.0,
                "pt",
            );
        }

        section(ui, "OUTPUT");
        combo(ui, "Profile", self.settings.profile.label(), |ui| {
            for space in Space::UI_ORDER {
                if self.settings.preview == Preview::Color && space == Space::Monostar {
                    continue;
                }
                if ui
                    .selectable_value(&mut self.settings.profile, space, space.label())
                    .clicked()
                {
                    ui.close();
                }
            }
        });
        row(ui, "Resolution", |ui| {
            ui.add(crate::widgets::bounded_number(
                &mut self.settings.resolution,
                72..=600,
                1.0,
            ));
            ui.label("ppi");
        });
        if self.iptc_templates.templates.is_empty() {
            row(ui, "Metadata", |ui| {
                ui.label(theme::caption("No IPTC templates saved"));
            });
        } else {
            let selected = self
                .settings
                .metadata_template
                .and_then(|index| self.iptc_templates.templates.get(index))
                .map_or("None", |template| template.name.as_str());
            combo(ui, "Metadata", selected, |ui| {
                if ui
                    .selectable_value(&mut self.settings.metadata_template, None, "None")
                    .clicked()
                {
                    ui.close();
                }
                for (index, template) in self.iptc_templates.templates.iter().enumerate() {
                    if ui
                        .selectable_value(
                            &mut self.settings.metadata_template,
                            Some(index),
                            &template.name,
                        )
                        .clicked()
                    {
                        ui.close();
                    }
                }
            });
        }
        if let Some(error) = &self.iptc_error {
            row(ui, "", |ui| {
                ui.label(egui::RichText::new(error).color(theme::RUBY));
            });
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let per_page = self.per_page();
        let count = self.items().len();
        let pages = self.page_count();
        self.preview_page = self.preview_page.min(pages - 1);
        // Not `page`: that name is taken below by the sheet's size in points.
        let page_index = self.preview_page;
        // **The sheet is as many pages as it takes, and all of them are reachable.**
        // The preview drew the first and said `FIRST PAGE`, which read as a limit.
        ui.horizontal(|ui| {
            let heading = if pages > 1 {
                format!("PAGE {} OF {pages} · {count} IMAGES", page_index + 1)
            } else {
                format!("ONE PAGE · {count} IMAGES")
            };
            theme::section(ui, &heading);
            if pages > 1 {
                ui.add_space(8.0);
                if ui
                    .add_enabled(page_index > 0, egui::Button::new("‹"))
                    .on_hover_text(theme::tip("previous page"))
                    .clicked()
                {
                    self.preview_page = page_index - 1;
                }
                if ui
                    .add_enabled(page_index + 1 < pages, egui::Button::new("›"))
                    .on_hover_text(theme::tip("next page"))
                    .clicked()
                {
                    self.preview_page = page_index + 1;
                }
            }
        });
        ui.add_space(16.0);
        let page = self.settings.page_points();
        let available = ui.available_size() - egui::vec2(24.0, 56.0);
        let scale = (available.x / page[0]).min(available.y / page[1]);
        let size = egui::vec2(page[0] * scale, page[1] * scale);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let paper = if self.settings.black_background {
            egui::Color32::BLACK
        } else {
            egui::Color32::WHITE
        };
        let ink = if self.settings.black_background {
            egui::Color32::WHITE
        } else {
            egui::Color32::BLACK
        };
        ui.painter().rect_filled(rect, 0.0, paper);
        let layout = PageLayout::new(&self.settings, page);
        // This page's slice. `preview_textures` holds the same slice, in the same
        // order, because the request and the draw take their page from one place.
        let items = &self.items()[(self.preview_page * per_page).min(count)..];
        let shown = items.len().min(per_page);
        for (index, cell) in layout.cells().take(shown).enumerate() {
            let cell = map_rect(cell, page, rect);
            let image_rect = cell.shrink(2.0);
            let visible_image_rect = if let Some(Some(texture)) = self.preview_textures.get(index) {
                let source = texture.size_vec2();
                let fit = fit_rect(source, image_rect);
                ui.painter().image(
                    texture.id(),
                    fit,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
                fit
            } else {
                ui.painter()
                    .rect_filled(image_rect, 0.0, egui::Color32::from_gray(96));
                image_rect
            };
            // **The caption hangs off the picture, not off the cell.** A cell is
            // squarish and a frame is not, so a fitted landscape sits with slack under
            // it — and a caption anchored to the cell floated below that slack, which
            // read as a gap the Gap control could not close because it was not one.
            // Captions on a row of like frames still line up; on mixed shapes they now
            // follow their own picture, which is what a contact sheet is for.
            let caption_height = layout.caption * scale;
            let anchor = visible_image_rect.bottom();
            let caption_top = anchor + self.settings.caption_gap * scale;
            let caption_rect = egui::Rect::from_min_max(
                egui::pos2(visible_image_rect.left(), caption_top),
                egui::pos2(visible_image_rect.right(), anchor + caption_height),
            );
            let mark_size = (self.settings.caption_size * scale).max(3.0);
            let label_width = if self.settings.color_label
                && label_color(items[index].label.as_deref()).is_some()
            {
                mark_size + 2.0
            } else {
                0.0
            };
            let rating_width = if self.settings.rating {
                items[index].rating.clamp(0, 5) as f32 * mark_size * 0.72
            } else {
                0.0
            };
            let marks_width = label_width + rating_width;
            let (text_left, text_right, marks_left) = match self.settings.caption_align {
                TextAlignment::Left => (
                    caption_rect.left(),
                    caption_rect.right() - marks_width,
                    caption_rect.right() - marks_width,
                ),
                TextAlignment::Center => (
                    caption_rect.left() + marks_width,
                    caption_rect.right() - marks_width,
                    caption_rect.right() - marks_width,
                ),
                TextAlignment::Right => (
                    caption_rect.left() + marks_width,
                    caption_rect.right(),
                    caption_rect.left(),
                ),
            };
            let text_rect = egui::Rect::from_min_max(
                egui::pos2(text_left.min(text_right), caption_rect.top()),
                egui::pos2(text_right.max(text_left), caption_rect.bottom()),
            );
            let text = caption_text(&items[index], index, &self.settings);
            if !text.is_empty() {
                let (x, align) = match self.settings.caption_align {
                    TextAlignment::Left => (text_rect.left(), egui::Align2::LEFT_TOP),
                    TextAlignment::Center => (text_rect.center().x, egui::Align2::CENTER_TOP),
                    TextAlignment::Right => (text_rect.right(), egui::Align2::RIGHT_TOP),
                };
                ui.painter().with_clip_rect(text_rect).text(
                    // No inset. Zero has to mean zero, or the control is describing
                    // something other than the distance it is named for.
                    egui::pos2(x, text_rect.top()),
                    align,
                    text,
                    egui::FontId::monospace(mark_size),
                    ink,
                );
            }
            let mut mark_x = marks_left;
            if self.settings.rating {
                let radius = mark_size * 0.18;
                for _ in 0..items[index].rating.clamp(0, 5) {
                    ui.painter().circle_filled(
                        egui::pos2(mark_x + radius, caption_rect.top() + mark_size * 0.45),
                        radius,
                        ink,
                    );
                    mark_x += mark_size * 0.72;
                }
            }
            if self.settings.color_label
                && let Some((_, color)) = theme::LABELS
                    .iter()
                    .find(|(name, _)| Some(*name) == items[index].label.as_deref())
            {
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        egui::pos2(mark_x, caption_rect.top()),
                        egui::vec2(mark_size * 0.75, mark_size * 0.75),
                    ),
                    0.0,
                    *color,
                );
            }
        }
        let of = PageOf {
            index: self.preview_page,
            count: pages,
        };
        paint_page_text(ui, rect, page, &self.settings, true, of);
        paint_page_text(ui, rect, page, &self.settings, false, of);
    }

    fn items(&self) -> &[Source] {
        self.items_for(self.settings.scope)
    }

    fn items_for(&self, scope: Scope) -> &[Source] {
        match scope {
            Scope::Selected => &self.sources.selected,
            Scope::Visible => &self.sources.visible,
            Scope::Folder => &self.sources.folder,
        }
    }

    fn per_page(&self) -> usize {
        (self.settings.columns.max(1) * self.settings.rows.max(1)).max(1)
    }

    /// How many pages the export will write. At least one, so an empty scope still
    /// draws a sheet of paper rather than nothing.
    fn page_count(&self) -> usize {
        self.items().len().div_ceil(self.per_page()).max(1)
    }

    fn request_preview(&mut self, ctx: &egui::Context) {
        let page = self.preview_page.min(self.page_count().saturating_sub(1));
        let signature = (
            self.settings.scope,
            self.settings.preview,
            self.settings.columns,
            self.settings.rows,
            self.settings.rotate_verticals,
            page,
        );
        if self.preview_signature == Some(signature) || self.preview_rx.is_some() {
            return;
        }
        self.preview_signature = Some(signature);
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let generation = self.preview_generation;
        let per_page = self.per_page();
        let items: Vec<Source> = self
            .items()
            .iter()
            .skip(page * per_page)
            .take(per_page)
            .cloned()
            .collect();
        let cache = self.sources.cache.clone();
        let mode = self.settings.preview;
        let rotate_verticals = self.settings.rotate_verticals;
        let (tx, rx) = mpsc::channel();
        self.preview_rx = Some(rx);
        std::thread::spawn(move || {
            let images = items
                .iter()
                .map(|source| load_preview(source, cache.as_deref(), mode, 512, rotate_verticals))
                .collect();
            let _ = tx.send(PreviewResult { generation, images });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(40));
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.preview_rx else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.preview_rx = None;
                if result.generation != self.preview_generation {
                    self.preview_signature = None;
                    return;
                }
                self.preview_textures = result
                    .images
                    .into_iter()
                    .enumerate()
                    .map(|(index, image)| {
                        image.map(|image| {
                            let color = egui::ColorImage::from_rgb([image.w, image.h], &image.data);
                            ctx.load_texture(
                                format!("contact-sheet-{index}"),
                                color,
                                egui::TextureOptions::LINEAR,
                            )
                        })
                    })
                    .collect();
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(40));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.preview_rx = None;
                self.preview_signature = None;
            }
        }
    }

    fn begin_export(&mut self) {
        let Some(destination) = crate::dialogs::save_file(
            "Export Contact Sheet",
            "contact-sheet.pdf",
            None,
            "PDF",
            "pdf",
        ) else {
            return;
        };
        let items = self.items().to_vec();
        if items.is_empty() {
            self.error = Some("There are no photographs in this scope".to_owned());
            return;
        }
        let settings = self.settings.clone();
        let cache = self.sources.cache.clone();
        let fonts = [
            self.fonts[self.settings.caption_font_index].clone(),
            self.fonts[self.settings.header_font_index].clone(),
            self.fonts[self.settings.footer_font_index].clone(),
            self.fonts[self.settings.page_number_font_index].clone(),
        ];
        let metadata = self
            .settings
            .metadata_template
            .and_then(|index| self.iptc_templates.templates.get(index))
            .cloned();
        let (tx, rx) = mpsc::channel();
        self.exporting = true;
        self.export_rx = Some(rx);
        std::thread::spawn(move || {
            let result = write_pdf(
                &destination,
                &items,
                &settings,
                cache.as_deref(),
                &fonts,
                metadata.as_ref(),
            )
            .map(|()| destination);
            let _ = tx.send(result);
        });
    }

    fn poll_export(&mut self, ctx: &egui::Context) -> Option<DialogResponse> {
        let Some(rx) = &self.export_rx else {
            return None;
        };
        match rx.try_recv() {
            Ok(Ok(path)) => Some(DialogResponse::Exported(path)),
            Ok(Err(error)) => {
                self.exporting = false;
                self.export_rx = None;
                self.error = Some(error);
                None
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(40));
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.exporting = false;
                self.export_rx = None;
                self.error = Some("Contact sheet export stopped unexpectedly".to_owned());
                None
            }
        }
    }
}

fn section(ui: &mut egui::Ui, label: &str) {
    ui.add_space(15.0);
    ui.separator();
    ui.add_space(8.0);
    theme::section(ui, label);
    ui.add_space(6.0);
}

const LABEL_WIDTH: f32 = 126.0;
/// Wide enough for the longest value any of these lists holds — `Sequence number`,
/// and `Entire Folder · 720` with its count — and no wider. The combo used to run to
/// the panel edge at 300, which made five short words look like five long fields and
/// left the eye no column to travel down.
const CONTROL_WIDTH: f32 = 160.0;
/// The exception, and the only one: a font list holds names nobody chose for brevity.
const FONT_WIDTH: f32 = 240.0;

fn row<R>(ui: &mut egui::Ui, label: &str, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_sized([LABEL_WIDTH, 22.0], egui::Label::new(label));
        contents(ui)
    })
    .inner
}

fn combo(ui: &mut egui::Ui, label: &str, selected: &str, contents: impl FnOnce(&mut egui::Ui)) {
    combo_width(ui, label, selected, CONTROL_WIDTH, contents);
}

fn combo_width(
    ui: &mut egui::Ui,
    label: &str,
    selected: &str,
    width: f32,
    contents: impl FnOnce(&mut egui::Ui),
) {
    row(ui, label, |ui| {
        egui::ComboBox::from_id_salt(("contact", label))
            .selected_text(selected)
            .width(width)
            .show_ui(ui, contents);
    });
}

/// A closed set of alignments, drawn as its own shapes.
///
/// Returns nothing: the caller's value is set in place, the way every other control
/// in this dialog works. The glyph fallbacks are the same three characters the icons
/// draw, so a build with no SVGs still says which is which.
fn align_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    label: &str,
    value: &mut T,
    options: &[(T, &str, &str, &str)],
) {
    align_row_except(ui, icons, label, value, options, None);
}

/// [`align_row`] with one choice greyed out, and its tip saying why.
fn align_row_except<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    label: &str,
    value: &mut T,
    options: &[(T, &str, &str, &str)],
    blocked: Option<(T, &str)>,
) {
    row(ui, label, |ui| {
        for (option, icon, glyph, tip) in options {
            let taken = blocked.filter(|(which, _)| which == option);
            if crate::icons::toggle_enabled(
                ui,
                icons,
                icon,
                glyph,
                *value == *option,
                taken.is_none(),
                crate::icons::BIG,
            )
            .on_hover_text(theme::tip(taken.map_or(*tip, |(_, why)| why)))
            .clicked()
            {
                *value = *option;
            }
        }
    });
}

/// Two length fields on one row, each with its own short label.
///
/// The margins and the two spacings were six rows of one number, which is what made
/// PAGE a column you scroll rather than a panel you read. They are three pairs, and
/// pairs belong beside each other.
fn duo_inches(
    ui: &mut egui::Ui,
    label: &str,
    fields: [(&str, &mut f32); 2],
    range_inches: std::ops::RangeInclusive<f32>,
    unit: Unit,
) -> bool {
    let factor = unit.per_inch();
    let range = *range_inches.start() * factor..=*range_inches.end() * factor;
    row(ui, label, |ui| {
        let mut changed = false;
        for (name, inches) in fields {
            ui.label(theme::caption(name));
            let mut shown = *inches * factor;
            if ui
                .add(
                    egui::DragValue::new(&mut shown)
                        .range(range.clone())
                        .speed(0.05),
                )
                .changed()
            {
                *inches = shown / factor;
                changed = true;
            }
        }
        ui.label(unit.label());
        changed
    })
}

fn value(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: &str,
) {
    row(ui, label, |ui| {
        ui.add(egui::DragValue::new(value).range(range).speed(0.05));
        ui.label(unit);
    });
}

fn value_inches(
    ui: &mut egui::Ui,
    label: &str,
    inches: &mut f32,
    range_inches: std::ops::RangeInclusive<f32>,
    unit: Unit,
) -> bool {
    let factor = unit.per_inch();
    let mut shown = *inches * factor;
    let range = *range_inches.start() * factor..=*range_inches.end() * factor;
    row(ui, label, |ui| {
        let changed = ui
            .add(egui::DragValue::new(&mut shown).range(range).speed(0.05))
            .changed();
        if changed {
            *inches = shown / factor;
        }
        ui.label(unit.label());
        changed
    })
}

fn pair_inches(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut [f32; 2],
    range_inches: std::ops::RangeInclusive<f32>,
    unit: Unit,
) {
    let factor = unit.per_inch();
    let mut shown = [values[0] * factor, values[1] * factor];
    let range = *range_inches.start() * factor..=*range_inches.end() * factor;
    row(ui, label, |ui| {
        let first = ui.add(egui::DragValue::new(&mut shown[0]).range(range.clone()));
        ui.label("×");
        let second = ui.add(egui::DragValue::new(&mut shown[1]).range(range));
        if first.changed() || second.changed() {
            *values = [shown[0] / factor, shown[1] / factor];
        }
        ui.label(unit.label());
    });
}

fn toggle(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    row(ui, label, |ui| {
        ui.checkbox(value, "");
    });
}

#[derive(Debug, Clone, Copy)]
struct PageLayout {
    page: [f32; 2],
    grid: Rect,
    columns: usize,
    rows: usize,
    top_gutter: f32,
    side_gutter: f32,
    caption: f32,
}

impl PageLayout {
    fn new(settings: &Settings, page: [f32; 2]) -> Self {
        let margins = settings.margins_inches;
        let header_height = band_height(settings, true);
        let footer_height = band_height(settings, false);
        let mut grid = Rect::new(
            margins.left * 72.0,
            margins.bottom * 72.0,
            page[0] - margins.right * 72.0,
            page[1] - margins.top * 72.0,
        );
        grid.y1 += footer_height;
        grid.y2 -= header_height;
        let caption = if settings.caption_source != CaptionSource::None
            || settings.rating
            || settings.color_label
        {
            settings.caption_size * 1.35 + settings.caption_gap
        } else {
            0.0
        };
        Self {
            page,
            grid,
            columns: settings.columns.max(1),
            rows: settings.rows.max(1),
            top_gutter: settings.vertical_spacing_inches * 72.0,
            side_gutter: settings.horizontal_spacing_inches * 72.0,
            caption,
        }
    }

    fn cell_size(self) -> [f32; 2] {
        let width = (self.grid.x2 - self.grid.x1 - self.side_gutter * (self.columns - 1) as f32)
            / self.columns as f32;
        let height = (self.grid.y2 - self.grid.y1 - self.top_gutter * (self.rows - 1) as f32)
            / self.rows as f32;
        [width.max(0.0), height.max(0.0)]
    }

    fn cells(self) -> impl Iterator<Item = Rect> {
        let [w, h] = self.cell_size();
        (0..self.rows * self.columns).map(move |index| {
            let row = index / self.columns;
            let column = index % self.columns;
            let x = self.grid.x1 + column as f32 * (w + self.side_gutter);
            let y = self.grid.y2 - (row + 1) as f32 * h - row as f32 * self.top_gutter;
            Rect::new(x, y + self.caption, x + w, y + h)
        })
    }
}

fn text_block_height(text: &str, size: f32) -> f32 {
    if text.is_empty() {
        0.0
    } else {
        text.lines().count().max(1) as f32 * size * 1.25 + 8.0
    }
}

/// Which sheet of the run is being drawn, and how many there are: what `{page}` and
/// `{pages}` resolve against, and what first-page-only is asked about.
#[derive(Debug, Clone, Copy)]
struct PageOf {
    index: usize,
    count: usize,
}

/// The height a band takes out of the grid — whichever of its two tenants is taller.
///
/// Taken on every page even where the text is first-page-only, and even where `{page}`
/// will make the band empty on this one: a band that came and went would resize the
/// cells from sheet to sheet, and frames that change size between sheets are not a
/// contact sheet.
fn band_height(settings: &Settings, header: bool) -> f32 {
    let (text, size) = if header {
        (&settings.header, settings.header_size)
    } else {
        (&settings.footer, settings.footer_size)
    };
    let number = if settings.page_number == band_of(header) {
        text_block_height("0", settings.page_number_size)
    } else {
        0.0
    };
    text_block_height(text, size).max(number)
}

fn band_of(header: bool) -> PageNumber {
    if header {
        PageNumber::Header
    } else {
        PageNumber::Footer
    }
}

/// What the band prints on this page: nothing past the first if it is first-page-only,
/// and `{page}` / `{pages}` filled in.
fn band_text(settings: &Settings, header: bool, page: PageOf) -> String {
    let (text, first_only) = if header {
        (&settings.header, settings.header_first_page_only)
    } else {
        (&settings.footer, settings.footer_first_page_only)
    };
    if first_only && page.index > 0 {
        return String::new();
    }
    text.replace("{page}", &(page.index + 1).to_string())
        .replace("{pages}", &page.count.to_string())
}

/// The corner the page number may not take: the one its band's own text is anchored
/// to. Two blocks on the same corner print on top of each other.
fn page_number_blocked(settings: &Settings) -> Option<TextPosition> {
    match settings.page_number {
        PageNumber::Header if !settings.header.is_empty() => Some(settings.header_position),
        PageNumber::Footer if !settings.footer.is_empty() => Some(settings.footer_position),
        _ => None,
    }
}

/// Where the page number prints. A stored corner that the band's text has since claimed
/// steps aside — the control greys that icon out, and the sheet has to agree with the
/// control even when the text arrived after the choice did.
fn page_number_position(settings: &Settings) -> TextPosition {
    let wanted = settings.page_number_position;
    if page_number_blocked(settings) == Some(wanted) {
        [
            TextPosition::Left,
            TextPosition::Center,
            TextPosition::Right,
        ]
        .into_iter()
        .find(|free| *free != wanted)
        .unwrap_or(wanted)
    } else {
        wanted
    }
}

fn map_rect(rect: Rect, page: [f32; 2], target: egui::Rect) -> egui::Rect {
    let x1 = target.left() + rect.x1 / page[0] * target.width();
    let x2 = target.left() + rect.x2 / page[0] * target.width();
    let y1 = target.bottom() - rect.y2 / page[1] * target.height();
    let y2 = target.bottom() - rect.y1 / page[1] * target.height();
    egui::Rect::from_min_max(egui::pos2(x1, y1), egui::pos2(x2, y2))
}

fn fit_rect(source: egui::Vec2, bounds: egui::Rect) -> egui::Rect {
    let scale = (bounds.width() / source.x).min(bounds.height() / source.y);
    let size = source * scale;
    egui::Rect::from_center_size(bounds.center(), size)
}

fn paint_page_text(
    ui: &mut egui::Ui,
    page_rect: egui::Rect,
    page: [f32; 2],
    settings: &Settings,
    header: bool,
    of: PageOf,
) {
    let (position, size) = if header {
        (settings.header_position, settings.header_size)
    } else {
        (settings.footer_position, settings.footer_size)
    };
    let text = band_text(settings, header, of);
    let number = (settings.page_number == band_of(header)).then(|| (of.index + 1).to_string());
    if text.is_empty() && number.is_none() {
        return;
    }
    let scale = page_rect.width() / page[0];
    let inset = |inches: f32| inches * 72.0 * page_rect.width() / page[0];
    let left_inset = inset(settings.margins_inches.left);
    let right_inset = inset(settings.margins_inches.right);
    let vertical_inset = if header {
        settings.margins_inches.top
    } else {
        settings.margins_inches.bottom
    } * 72.0
        * page_rect.height()
        / page[1];
    let ink = if settings.black_background {
        egui::Color32::WHITE
    } else {
        egui::Color32::BLACK
    };
    // Centred between the margins, not on the page. The PDF path draws into a
    // margin-to-margin rect and centres inside it, and the two have to agree.
    let anchor = |position: TextPosition| match position {
        TextPosition::Left => (page_rect.left() + left_inset, egui::Align2::LEFT_TOP),
        TextPosition::Center => (
            (page_rect.left() + left_inset + page_rect.right() - right_inset) * 0.5,
            egui::Align2::CENTER_TOP,
        ),
        TextPosition::Right => (page_rect.right() - right_inset, egui::Align2::RIGHT_TOP),
    };
    let draw = |lines: &[&str], position, size: f32| {
        let line_height = size * 1.25 * scale;
        let (x, align) = anchor(position);
        // A footer grows upward from the bottom margin; a header downward from the top.
        let first = if header {
            page_rect.top() + vertical_inset
        } else {
            page_rect.bottom() - vertical_inset - line_height * lines.len() as f32
        };
        for (line, text) in lines.iter().enumerate() {
            ui.painter().text(
                egui::pos2(x, first + line as f32 * line_height),
                align,
                *text,
                egui::FontId::monospace((size * scale).max(3.0)),
                ink,
            );
        }
    };
    if !text.is_empty() {
        draw(&text.lines().collect::<Vec<_>>(), position, size);
    }
    if let Some(number) = &number {
        draw(
            &[number.as_str()],
            page_number_position(settings),
            settings.page_number_size,
        );
    }
}

fn load_preview(
    source: &Source,
    cache: Option<&Path>,
    mode: Preview,
    edge: u32,
    rotate_verticals: bool,
) -> Option<raw_core::preview::Rgb8> {
    let mut image = crate::lightbox::contact_thumbnail(
        &source.path,
        cache,
        mode == Preview::Developed,
        edge,
        source.orientation,
    )?;
    if mode != Preview::Color {
        for pixel in image.data.chunks_exact_mut(3) {
            let gray =
                (0.2126 * pixel[0] as f32 + 0.7152 * pixel[1] as f32 + 0.0722 * pixel[2] as f32)
                    .round() as u8;
            pixel.fill(gray);
        }
    }
    Some(orient_preview(image, rotate_verticals))
}

/// The one turn the *document* makes. The photograph arrives the right way up —
/// `contact_thumbnail` applies the sidecar's orientation — so this is layout only.
fn orient_preview(
    image: raw_core::preview::Rgb8,
    rotate_verticals: bool,
) -> raw_core::preview::Rgb8 {
    if rotate_verticals && image.h > image.w {
        rotate_preview(image, 3)
    } else {
        image
    }
}

fn rotate_preview(image: raw_core::preview::Rgb8, turns: u8) -> raw_core::preview::Rgb8 {
    let turns = turns % 4;
    if turns == 0 || image.w == 0 || image.h == 0 {
        return image;
    }
    let (out_w, out_h) = if turns % 2 == 1 {
        (image.h, image.w)
    } else {
        (image.w, image.h)
    };
    let mut data = vec![0; out_w * out_h * 3];
    for y in 0..image.h {
        for x in 0..image.w {
            let (out_x, out_y) = match turns {
                1 => (image.h - 1 - y, x),
                2 => (image.w - 1 - x, image.h - 1 - y),
                3 => (y, image.w - 1 - x),
                _ => unreachable!(),
            };
            let source = (y * image.w + x) * 3;
            let destination = (out_y * out_w + out_x) * 3;
            data[destination..destination + 3].copy_from_slice(&image.data[source..source + 3]);
        }
    }
    raw_core::preview::Rgb8 {
        w: out_w,
        h: out_h,
        data,
    }
}

fn system_fonts() -> Vec<FontChoice> {
    static FONTS: std::sync::OnceLock<Vec<FontChoice>> = std::sync::OnceLock::new();
    FONTS.get_or_init(discover_system_fonts).clone()
}

fn discover_system_fonts() -> Vec<FontChoice> {
    let mut fonts = vec![FontChoice {
        label: "JetBrains Mono".to_owned(),
        path: None,
    }];
    let mut found = BTreeMap::<String, PathBuf>::new();
    for root in crate::platform::font_dirs() {
        visit_fonts(&root, &mut found, 0);
    }
    fonts.extend(found.into_iter().map(|(label, path)| FontChoice {
        label,
        path: Some(path),
    }));
    fonts
}

fn visit_fonts(root: &Path, found: &mut BTreeMap<String, PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_fonts(&path, found, depth + 1);
            continue;
        }
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ttf"))
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(face) = ttf_parser::Face::parse(&bytes, 0) else {
            continue;
        };
        if face.tables().glyf.is_none() {
            continue;
        }
        let label = face
            .names()
            .into_iter()
            .find(|name| name.name_id == ttf_parser::name_id::FULL_NAME)
            .and_then(|name| name.to_string())
            .or_else(|| {
                path.file_stem()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        if let Some(label) = label {
            found.entry(label).or_insert(path);
        }
    }
}

fn write_pdf(
    destination: &Path,
    items: &[Source],
    settings: &Settings,
    cache: Option<&Path>,
    fonts: &[FontChoice; 4],
    metadata: Option<&iptc_templates::Template>,
) -> Result<(), String> {
    let font_bytes = [
        read_font(&fonts[0])?,
        read_font(&fonts[1])?,
        read_font(&fonts[2])?,
        read_font(&fonts[3])?,
    ];
    let faces = [
        parse_font(&font_bytes[0], &fonts[0])?,
        parse_font(&font_bytes[1], &fonts[1])?,
        parse_font(&font_bytes[2], &fonts[2])?,
        parse_font(&font_bytes[3], &fonts[3])?,
    ];
    let page_size = settings.page_points();
    let per_page = settings.columns.max(1) * settings.rows.max(1);
    let page_count = items.len().div_ceil(per_page);
    let mut refs = Refs::new();
    let catalog = refs.next();
    let pages = refs.next();
    let document_info = refs.next();
    let font_refs: [FontRefs; 4] = std::array::from_fn(|_| FontRefs {
        font: refs.next(),
        cid: refs.next(),
        descriptor: refs.next(),
        file: refs.next(),
        cmap: refs.next(),
    });
    let icc_id = refs.next();
    let page_ids: Vec<Ref> = (0..page_count).map(|_| refs.next()).collect();
    let content_ids: Vec<Ref> = (0..page_count).map(|_| refs.next()).collect();
    let image_ids: Vec<Ref> = (0..items.len()).map(|_| refs.next()).collect();
    let image_names: Vec<Vec<u8>> = (0..items.len())
        .map(|index| format!("I{index}").into_bytes())
        .collect();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog).pages(pages);
    pdf.pages(pages)
        .kids(page_ids.iter().copied())
        .count(page_count as i32);
    write_document_info(&mut pdf, document_info, metadata);
    for index in 0..4 {
        embed_font(
            &mut pdf,
            font_refs[index],
            &font_bytes[index],
            &faces[index],
            items,
            settings,
            index,
        );
    }

    let profile = settings.profile;
    let mut icc = pdf.icc_profile(icc_id, profile.icc());
    icc.n(profile.channels() as i32);
    if profile.channels() == 1 {
        icc.alternate().device_gray();
        icc.range([0.0, 1.0]);
    } else {
        icc.alternate().srgb();
        icc.range([0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    }
    icc.finish();

    for page_index in 0..page_count {
        let start = page_index * per_page;
        let end = (start + per_page).min(items.len());
        let layout = PageLayout::new(settings, page_size);
        let mut page = pdf.page(page_ids[page_index]);
        page.media_box(Rect::new(0.0, 0.0, page_size[0], page_size[1]));
        page.parent(pages);
        page.contents(content_ids[page_index]);
        {
            let mut resources = page.resources();
            {
                let mut page_fonts = resources.fonts();
                for index in 0..4 {
                    page_fonts.pair(Name(PDF_FONT_NAMES[index]), font_refs[index].font);
                }
            }
            let mut objects = resources.x_objects();
            for item_index in start..end {
                objects.pair(Name(&image_names[item_index]), image_ids[item_index]);
            }
        }
        page.finish();

        let mut content = Content::new();
        if settings.black_background {
            content.set_fill_gray(0.0);
            content.rect(0.0, 0.0, page_size[0], page_size[1]);
            content.fill_nonzero();
        }
        let ink = if settings.black_background { 1.0 } else { 0.0 };
        for (offset, cell) in layout.cells().take(end - start).enumerate() {
            let item_index = start + offset;
            let item = &items[item_index];
            let Some(image) = load_preview(
                item,
                cache,
                settings.preview,
                export_edge(cell, settings.resolution),
                settings.rotate_verticals,
            ) else {
                continue;
            };
            let encoded = encode_for_profile(&image, profile)?;
            let mut xobject = pdf.image_xobject(image_ids[item_index], &encoded.data);
            xobject.filter(pdf_writer::Filter::DctDecode);
            xobject.width(image.w as i32);
            xobject.height(image.h as i32);
            xobject.bits_per_component(8);
            xobject.color_space().icc_based(icc_id);
            xobject.finish();

            let fitted = fit_pdf(image.w as f32, image.h as f32, cell);
            content.save_state();
            content.transform([
                fitted.x2 - fitted.x1,
                0.0,
                0.0,
                fitted.y2 - fitted.y1,
                fitted.x1,
                fitted.y1,
            ]);
            content.x_object(Name(&image_names[item_index]));
            content.restore_state();

            // Anchored to the fitted picture, matching the preview. See the note
            // there: `cell.y1` put the caption under the letterbox rather than under
            // the photograph.
            let caption_y = fitted.y1 - settings.caption_gap - settings.caption_size;
            let label_width =
                if settings.color_label && label_color(item.label.as_deref()).is_some() {
                    settings.caption_size + 3.0
                } else {
                    0.0
                };
            let rating_width = if settings.rating {
                item.rating.clamp(0, 5) as f32 * settings.caption_size * 0.893
            } else {
                0.0
            };
            let marks_width = label_width + rating_width;
            let (text_left, text_right, marks_left) = match settings.caption_align {
                TextAlignment::Left => {
                    (fitted.x1, fitted.x2 - marks_width, fitted.x2 - marks_width)
                }
                TextAlignment::Center => (
                    fitted.x1 + marks_width,
                    fitted.x2 - marks_width,
                    fitted.x2 - marks_width,
                ),
                TextAlignment::Right => (fitted.x1 + marks_width, fitted.x2, fitted.x1),
            };
            let caption = caption_text(item, item_index, settings);
            if !caption.is_empty() {
                let available = (text_right - text_left).max(0.0);
                show_text_aligned(
                    &mut content,
                    (PDF_FONT_NAMES[0], &faces[0]),
                    &truncate_to_width(&caption, &faces[0], settings.caption_size, available),
                    settings.caption_size,
                    Rect::new(text_left, caption_y, text_left + available, caption_y),
                    settings.caption_align,
                    ink,
                );
            }
            if settings.rating && item.rating > 0 {
                draw_rating(
                    &mut content,
                    item.rating.clamp(0, 5) as usize,
                    settings.caption_size,
                    marks_left,
                    caption_y,
                    ink,
                );
            }
            if settings.color_label
                && let Some(color) = label_color(item.label.as_deref())
            {
                content.set_fill_rgb(color[0], color[1], color[2]);
                content.rect(
                    marks_left + rating_width,
                    caption_y,
                    settings.caption_size * 0.75,
                    settings.caption_size * 0.75,
                );
                content.fill_nonzero();
            }
        }
        let of = PageOf {
            index: page_index,
            count: page_count,
        };
        let number_font = (PDF_FONT_NAMES[3], &faces[3]);
        write_page_text(
            &mut content,
            [(PDF_FONT_NAMES[1], &faces[1]), number_font],
            settings,
            &layout,
            true,
            ink,
            of,
        );
        write_page_text(
            &mut content,
            [(PDF_FONT_NAMES[2], &faces[2]), number_font],
            settings,
            &layout,
            false,
            ink,
            of,
        );
        pdf.stream(content_ids[page_index], &content.finish());
    }

    std::fs::write(destination, pdf.finish())
        .map_err(|error| format!("Could not write {}: {error}", destination.display()))
}

const PDF_FONT_NAMES: [&[u8]; 4] = [b"F1", b"F2", b"F3", b"F4"];

#[derive(Clone, Copy)]
struct FontRefs {
    font: Ref,
    cid: Ref,
    descriptor: Ref,
    file: Ref,
    cmap: Ref,
}

fn read_font(font: &FontChoice) -> Result<Vec<u8>, String> {
    match &font.path {
        Some(path) => {
            std::fs::read(path).map_err(|error| format!("Could not read {}: {error}", font.label))
        }
        None => Ok(UI_FACE.to_vec()),
    }
}

fn parse_font<'a>(bytes: &'a [u8], font: &FontChoice) -> Result<ttf_parser::Face<'a>, String> {
    ttf_parser::Face::parse(bytes, 0)
        .map_err(|_| format!("{} is not a readable TrueType font", font.label))
}

fn embed_font(
    pdf: &mut Pdf,
    refs: FontRefs,
    bytes: &[u8],
    face: &ttf_parser::Face<'_>,
    items: &[Source],
    settings: &Settings,
    index: usize,
) {
    let postscript = postscript_name(face).unwrap_or_else(|| format!("ContactSheetFont{index}"));
    let postscript_pdf = pdf_name(&format!("{postscript}-{index}"));
    let info = SystemInfo {
        registry: Str(b"Adobe"),
        ordering: Str(b"Identity"),
        supplement: 0,
    };
    pdf.type0_font(refs.font)
        .base_font(Name(postscript_pdf.as_bytes()))
        .encoding_predefined(Name(b"Identity-H"))
        .descendant_font(refs.cid)
        .to_unicode(refs.cmap);

    let used = used_glyphs(items, settings, face);
    let scale = 1000.0 / face.units_per_em() as f32;
    let mut cid = pdf.cid_font(refs.cid);
    cid.subtype(CidFontType::Type2)
        .base_font(Name(postscript_pdf.as_bytes()))
        .system_info(info)
        .font_descriptor(refs.descriptor)
        .default_width(0.0)
        .cid_to_gid_map_predefined(Name(b"Identity"));
    {
        let mut widths = cid.widths();
        for glyph in &used {
            widths.same(
                *glyph,
                *glyph,
                face.glyph_hor_advance(ttf_parser::GlyphId(*glyph))
                    .unwrap_or(0) as f32
                    * scale,
            );
        }
    }
    cid.finish();

    let bbox = face.global_bounding_box();
    let mut flags = FontFlags::NON_SYMBOLIC;
    if face.is_monospaced() {
        flags |= FontFlags::FIXED_PITCH;
    }
    if face.is_italic() {
        flags |= FontFlags::ITALIC;
    }
    pdf.font_descriptor(refs.descriptor)
        .name(Name(postscript_pdf.as_bytes()))
        .flags(flags)
        .bbox(Rect::new(
            bbox.x_min as f32 * scale,
            bbox.y_min as f32 * scale,
            bbox.x_max as f32 * scale,
            bbox.y_max as f32 * scale,
        ))
        .italic_angle(face.italic_angle())
        .ascent(face.ascender() as f32 * scale)
        .descent(face.descender() as f32 * scale)
        .cap_height(face.capital_height().unwrap_or(face.ascender()) as f32 * scale)
        .stem_v(80.0)
        .font_file2(refs.file);
    pdf.stream(refs.file, bytes)
        .pair(Name(b"Length1"), bytes.len() as i32);
    let cmap_name = format!("ContactSheetUnicode{index}");
    let mut cmap = UnicodeCmap::new(Name(cmap_name.as_bytes()), info);
    for (glyph, character) in glyph_map(items, settings, face) {
        cmap.pair(glyph, character);
    }
    pdf.cmap(refs.cmap, &cmap.finish());
}

fn write_document_info(pdf: &mut Pdf, id: Ref, template: Option<&iptc_templates::Template>) {
    let mut info = pdf.document_info(id);
    info.creator(TextStr("monopro"))
        .producer(TextStr("monopro"));
    let Some(template) = template else {
        return;
    };
    if let Some(value) = template_value(template, IptcField::Title) {
        info.title(TextStr(value));
    }
    if let Some(value) = template_value(template, IptcField::Creator) {
        info.author(TextStr(value));
    }
    if let Some(value) = template_value(template, IptcField::Description)
        .or_else(|| template_value(template, IptcField::Headline))
    {
        info.subject(TextStr(value));
    }
    // Everything the three fields above did not take, in the pane's order. A
    // vocabulary value in its published words: the value itself is a URI nobody reads.
    let keywords = IptcField::ALL
        .into_iter()
        .filter(|field| {
            !field.per_image()
                && !matches!(
                    field,
                    IptcField::Title
                        | IptcField::Creator
                        | IptcField::Description
                        | IptcField::Headline
                )
        })
        .filter_map(|field| {
            let value = template_value(template, field)?;
            let value = field.term_label(value).unwrap_or(value);
            Some(format!("{}: {value}", field.label()))
        })
        .collect::<Vec<_>>()
        .join("; ");
    if !keywords.is_empty() {
        info.keywords(TextStr(&keywords));
    }
}

fn template_value(template: &iptc_templates::Template, field: IptcField) -> Option<&str> {
    match template.action(field) {
        Some(iptc_templates::Action::Set { value }) if !value.trim().is_empty() => {
            Some(value.trim())
        }
        _ => None,
    }
}

struct EncodedImage {
    data: Vec<u8>,
}

fn encode_for_profile(
    image: &raw_core::preview::Rgb8,
    profile: Space,
) -> Result<EncodedImage, String> {
    let (pixels, color_type) = if profile.channels() == 1 {
        let pixels = image
            .data
            .chunks_exact(3)
            .map(|pixel| {
                let encoded = (0.2126 * pixel[0] as f32
                    + 0.7152 * pixel[1] as f32
                    + 0.0722 * pixel[2] as f32)
                    / 255.0;
                (profile.encode(srgb_decode(encoded)) * 255.0).round() as u8
            })
            .collect();
        (pixels, jpeg_encoder::ColorType::Luma)
    } else if profile == Space::Srgb {
        (image.data.clone(), jpeg_encoder::ColorType::Rgb)
    } else {
        (convert_srgb(image, profile), jpeg_encoder::ColorType::Rgb)
    };
    let mut data = Vec::new();
    jpeg_encoder::Encoder::new(&mut data, 92)
        .encode(&pixels, image.w as u16, image.h as u16, color_type)
        .map_err(|error| format!("Could not encode a contact-sheet image: {error}"))?;
    Ok(EncodedImage { data })
}

fn convert_srgb(image: &raw_core::preview::Rgb8, target: Space) -> Vec<u8> {
    let source = raw_core::Primaries::from_icc(Space::Srgb.icc()).expect("sRGB has primaries");
    let target_primaries =
        raw_core::Primaries::from_icc(target.icc()).expect("RGB profile has primaries");
    let matrix = source.to_xyz_d50();
    image
        .data
        .chunks_exact(3)
        .flat_map(|pixel| {
            let linear = [
                srgb_decode(pixel[0] as f32 / 255.0),
                srgb_decode(pixel[1] as f32 / 255.0),
                srgb_decode(pixel[2] as f32 / 255.0),
            ];
            let xyz = [
                matrix[0][0] * linear[0] + matrix[0][1] * linear[1] + matrix[0][2] * linear[2],
                matrix[1][0] * linear[0] + matrix[1][1] * linear[1] + matrix[1][2] * linear[2],
                matrix[2][0] * linear[0] + matrix[2][1] * linear[1] + matrix[2][2] * linear[2],
            ];
            target_primaries
                .linear_rgb(xyz)
                .map(|value| (target.encode(value.clamp(0.0, 1.0)) * 255.0).round() as u8)
        })
        .collect()
}

fn srgb_decode(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn export_edge(cell: Rect, resolution: u16) -> u32 {
    let inches = ((cell.x2 - cell.x1).max(cell.y2 - cell.y1) / 72.0).max(0.1);
    (inches * resolution as f32).ceil().clamp(128.0, 4096.0) as u32
}

fn fit_pdf(width: f32, height: f32, bounds: Rect) -> Rect {
    let scale = ((bounds.x2 - bounds.x1) / width).min((bounds.y2 - bounds.y1) / height);
    let w = width * scale;
    let h = height * scale;
    let x = bounds.x1 + ((bounds.x2 - bounds.x1) - w) * 0.5;
    let y = bounds.y1 + ((bounds.y2 - bounds.y1) - h) * 0.5;
    Rect::new(x, y, x + w, y + h)
}

fn draw_rating(
    content: &mut Content,
    rating: usize,
    size: f32,
    left: f32,
    baseline: f32,
    ink: f32,
) {
    let radius = size * 0.38;
    let step = radius * 2.35;
    let start = left + radius;
    content.set_fill_gray(ink);
    for star in 0..rating {
        let cx = start + star as f32 * step;
        let cy = baseline + size * 0.45;
        for point in 0..10 {
            let angle = -std::f32::consts::FRAC_PI_2 + point as f32 * std::f32::consts::PI / 5.0;
            let r = if point % 2 == 0 {
                radius
            } else {
                radius * 0.42
            };
            let (x, y) = (cx + angle.cos() * r, cy + angle.sin() * r);
            if point == 0 {
                content.move_to(x, y);
            } else {
                content.line_to(x, y);
            }
        }
        content.close_path();
        content.fill_nonzero();
    }
}

fn show_text(
    content: &mut Content,
    font: (&[u8], &ttf_parser::Face<'_>),
    text: &str,
    size: f32,
    x: f32,
    y: f32,
    ink: f32,
) {
    let (font_name, face) = font;
    let encoded = encode_text(text, face);
    content.set_fill_gray(ink);
    content.begin_text();
    content.set_font(Name(font_name), size);
    content.next_line(x, y);
    content.show(Str(&encoded));
    content.end_text();
}

fn show_text_aligned(
    content: &mut Content,
    font: (&[u8], &ttf_parser::Face<'_>),
    text: &str,
    size: f32,
    line: Rect,
    alignment: TextAlignment,
    ink: f32,
) {
    let (font_name, face) = font;
    let width = text_width(text, face, size);
    let x = match alignment {
        TextAlignment::Left => line.x1,
        TextAlignment::Center => line.x1 + ((line.x2 - line.x1) - width) * 0.5,
        TextAlignment::Right => line.x2 - width,
    };
    show_text(
        content,
        (font_name, face),
        text,
        size,
        x.max(line.x1),
        line.y1,
        ink,
    );
}

fn write_page_text(
    content: &mut Content,
    fonts: [(&[u8], &ttf_parser::Face<'_>); 2],
    settings: &Settings,
    layout: &PageLayout,
    header: bool,
    ink: f32,
    of: PageOf,
) {
    let (position, size) = if header {
        (settings.header_position, settings.header_size)
    } else {
        (settings.footer_position, settings.footer_size)
    };
    let text = band_text(settings, header, of);
    let number = (settings.page_number == band_of(header)).then(|| (of.index + 1).to_string());
    if text.is_empty() && number.is_none() {
        return;
    }
    let mut write = |font: (&[u8], &ttf_parser::Face<'_>), lines: &[&str], position, size: f32| {
        let leading = size * 1.25;
        let first_y = if header {
            layout.page[1] - settings.margins_inches.top * 72.0 - size
        } else {
            settings.margins_inches.bottom * 72.0 + leading * (lines.len() - 1) as f32
        };
        for (index, line) in lines.iter().enumerate() {
            let y = first_y - index as f32 * leading;
            show_text_aligned(
                content,
                font,
                line,
                size,
                Rect::new(
                    settings.margins_inches.left * 72.0,
                    y,
                    layout.page[0] - settings.margins_inches.right * 72.0,
                    y,
                ),
                match position {
                    TextPosition::Left => TextAlignment::Left,
                    TextPosition::Center => TextAlignment::Center,
                    TextPosition::Right => TextAlignment::Right,
                },
                ink,
            );
        }
    };
    if !text.is_empty() {
        write(fonts[0], &text.lines().collect::<Vec<_>>(), position, size);
    }
    if let Some(number) = &number {
        write(
            fonts[1],
            &[number.as_str()],
            page_number_position(settings),
            settings.page_number_size,
        );
    }
}

fn caption_text(source: &Source, index: usize, settings: &Settings) -> String {
    match settings.caption_source {
        CaptionSource::None => String::new(),
        CaptionSource::Filename if settings.include_file_extension => source.name.clone(),
        CaptionSource::Filename => source.path.file_stem().map_or_else(
            || source.name.clone(),
            |stem| stem.to_string_lossy().into_owned(),
        ),
        CaptionSource::Sequence => sequence_text(index, settings),
        CaptionSource::Custom => expand_caption_tokens(source, index, settings),
    }
}

fn sequence_text(index: usize, settings: &Settings) -> String {
    format!(
        "{:0width$}",
        settings.sequence_start.saturating_add(index as u32),
        width = settings.sequence_digits
    )
}

fn expand_caption_tokens(source: &Source, index: usize, settings: &Settings) -> String {
    let original = source
        .path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let folder = source
        .path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let sequence = sequence_text(index, settings);
    settings
        .caption_pattern
        .replace("{filename}", &source.name)
        .replace("{original}", &original)
        .replace("{sequence}", &sequence)
        .replace("{folder}", &folder)
}

fn text_width(text: &str, face: &ttf_parser::Face<'_>, size: f32) -> f32 {
    let units = face.units_per_em() as f32;
    text.chars()
        .filter_map(|character| face.glyph_index(character))
        .map(|glyph| face.glyph_hor_advance(glyph).unwrap_or(0) as f32 / units * size)
        .sum()
}

fn truncate_to_width(text: &str, face: &ttf_parser::Face<'_>, size: f32, width: f32) -> String {
    if text_width(text, face, size) <= width {
        return text.to_owned();
    }
    let ellipsis = "…";
    let allowance = (width - text_width(ellipsis, face, size)).max(0.0);
    let mut answer = String::new();
    for character in text.chars() {
        answer.push(character);
        if text_width(&answer, face, size) > allowance {
            answer.pop();
            break;
        }
    }
    answer.push_str(ellipsis);
    answer
}

fn encode_text(text: &str, face: &ttf_parser::Face<'_>) -> Vec<u8> {
    text.chars()
        .flat_map(|character| {
            face.glyph_index(character)
                .unwrap_or(ttf_parser::GlyphId(0))
                .0
                .to_be_bytes()
        })
        .collect()
}

fn glyph_map(
    items: &[Source],
    settings: &Settings,
    face: &ttf_parser::Face<'_>,
) -> BTreeMap<u16, char> {
    let mut map = BTreeMap::new();
    let captions: String = items
        .iter()
        .enumerate()
        .map(|(index, item)| caption_text(item, index, settings))
        .collect();
    for character in captions
        .chars()
        .chain(settings.header.chars())
        .chain(settings.footer.chars())
        // The digits are always embedded: `{page}` and the page number itself put
        // numerals on the sheet that no string in `settings` contains.
        .chain("0123456789…".chars())
    {
        if let Some(glyph) = face.glyph_index(character) {
            map.entry(glyph.0).or_insert(character);
        }
    }
    map
}

fn used_glyphs(
    items: &[Source],
    settings: &Settings,
    face: &ttf_parser::Face<'_>,
) -> BTreeSet<u16> {
    glyph_map(items, settings, face).into_keys().collect()
}

fn postscript_name(face: &ttf_parser::Face<'_>) -> Option<String> {
    face.names()
        .into_iter()
        .find(|name| name.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .and_then(|name| name.to_string())
}

fn pdf_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn label_color(label: Option<&str>) -> Option<[f32; 3]> {
    let (_, color) = theme::LABELS
        .iter()
        .find(|(name, _)| Some(*name) == label)?;
    Some([
        color.r() as f32 / 255.0,
        color.g() as f32 / 255.0,
        color.b() as f32 / 255.0,
    ])
}

struct Refs(i32);

impl Refs {
    fn new() -> Self {
        Self(1)
    }

    fn next(&mut self) -> Ref {
        let reference = Ref::new(self.0);
        self.0 += 1;
        reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_footer_reserve_the_actual_page_edges() {
        let mut settings = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings;
        settings.header = "Contact Sheet".to_owned();
        settings.footer = "EXAMPLE\nExample photo series".to_owned();
        let page = settings.page_points();
        let layout = PageLayout::new(&settings, page);
        assert!(
            layout.grid.y1 > settings.margins_inches.bottom * 72.0 + settings.footer_size * 2.0
        );
        assert!(layout.grid.y2 < page[1] - settings.margins_inches.top * 72.0);
    }

    fn bare_settings() -> Settings {
        Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings
    }

    #[test]
    fn a_first_page_only_header_leaves_the_sheets_behind_it_bare() {
        let mut settings = bare_settings();
        settings.header = "Example photo series".to_owned();
        settings.header_first_page_only = true;
        let page = |index| PageOf { index, count: 3 };
        assert_eq!(band_text(&settings, true, page(0)), "Example photo series");
        assert_eq!(band_text(&settings, true, page(1)), "");
        settings.header_first_page_only = false;
        assert_eq!(band_text(&settings, true, page(1)), "Example photo series");
    }

    #[test]
    fn the_band_keeps_its_height_on_the_pages_that_print_nothing_in_it() {
        let mut settings = bare_settings();
        settings.header = "Example photo series".to_owned();
        let full = PageLayout::new(&settings, settings.page_points()).cell_size();
        settings.header_first_page_only = true;
        assert_eq!(
            PageLayout::new(&settings, settings.page_points()).cell_size(),
            full,
            "cells that resize between sheets are not a contact sheet"
        );
    }

    #[test]
    fn the_page_token_becomes_the_sheet_it_is_printed_on() {
        let mut settings = bare_settings();
        settings.footer = "{page} of {pages}".to_owned();
        assert_eq!(
            band_text(&settings, false, PageOf { index: 1, count: 4 }),
            "2 of 4"
        );
    }

    #[test]
    fn a_page_number_alone_still_claims_its_band() {
        let mut settings = bare_settings();
        let bare = PageLayout::new(&settings, settings.page_points()).grid;
        settings.page_number = PageNumber::Footer;
        let claimed = PageLayout::new(&settings, settings.page_points()).grid;
        assert!(claimed.y1 > bare.y1);
        assert_eq!(claimed.y2, bare.y2, "the header band is not the footer's");
    }

    #[test]
    fn the_page_number_steps_off_the_corner_its_bands_text_holds() {
        let mut settings = bare_settings();
        settings.page_number = PageNumber::Footer;
        settings.page_number_position = TextPosition::Left;
        assert_eq!(page_number_blocked(&settings), None);
        assert_eq!(page_number_position(&settings), TextPosition::Left);
        settings.footer = "EXAMPLE".to_owned();
        settings.footer_position = TextPosition::Left;
        assert_eq!(page_number_blocked(&settings), Some(TextPosition::Left));
        assert_ne!(page_number_position(&settings), TextPosition::Left);
        // The header's text is in the other band, so it blocks nothing.
        settings.page_number = PageNumber::Header;
        assert_eq!(page_number_blocked(&settings), None);
        assert_eq!(page_number_position(&settings), TextPosition::Left);
    }

    #[test]
    fn text_sizes_reach_five_points() {
        assert_eq!(MIN_TEXT_PT, 5.0);
    }

    #[test]
    fn the_paper_menu_has_tabloid_and_no_eleven_by_fourteen() {
        let labels: Vec<&str> = Paper::ALL.into_iter().map(Paper::label).collect();
        assert!(labels.iter().any(|label| label.starts_with("Tabloid")));
        assert!(!labels.contains(&"11 × 14"));
        assert_eq!(Paper::Tabloid.inches([1.0, 1.0]), [11.0, 17.0]);
    }

    #[test]
    fn contact_sheet_lengths_follow_the_print_unit_without_changing_inches() {
        let inches = 0.4;
        let shown = inches * Unit::Centimetres.per_inch();
        assert!((shown - 1.016).abs() < 1.0e-6);
        assert!((shown / Unit::Centimetres.per_inch() - inches).abs() < 1.0e-6);
    }

    #[test]
    fn custom_captions_expand_the_rename_style_tokens() {
        let mut settings = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings;
        settings.caption_source = CaptionSource::Custom;
        settings.caption_pattern = "{sequence} · {folder}/{original} · {filename}".to_owned();
        settings.sequence_start = 7;
        settings.sequence_digits = 3;
        let source = Source {
            path: PathBuf::from("/shoot/raw/frame-01.dng"),
            name: "frame-01.dng".to_owned(),
            rating: 0,
            label: None,
            orientation: None,
        };
        assert_eq!(
            caption_text(&source, 1, &settings),
            "008 · raw/frame-01 · frame-01.dng"
        );
    }

    #[test]
    fn filename_captions_omit_the_extension_until_requested() {
        let mut settings = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings;
        let source = Source {
            path: PathBuf::from("/shoot/frame-01.dng"),
            name: "frame-01.dng".to_owned(),
            rating: 0,
            label: None,
            orientation: None,
        };
        // Set rather than inherited: captions default to None now, and a test about
        // the extension rule should say which source it is asking about instead of
        // leaning on a default that means something else.
        settings.caption_source = CaptionSource::Filename;
        assert_eq!(caption_text(&source, 0, &settings), "frame-01");
        settings.include_file_extension = true;
        assert_eq!(caption_text(&source, 0, &settings), "frame-01.dng");
    }

    #[test]
    fn contact_sheet_rotation_turns_pixels_and_dimensions() {
        let image = raw_core::preview::Rgb8 {
            w: 2,
            h: 1,
            data: vec![255, 0, 0, 0, 255, 0],
        };
        let turned = rotate_preview(image, 1);
        assert_eq!((turned.w, turned.h), (1, 2));
        assert_eq!(turned.data, vec![255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn rotate_verticals_turns_portraits_counter_clockwise_only() {
        let portrait = raw_core::preview::Rgb8 {
            w: 1,
            h: 2,
            data: vec![255, 0, 0, 0, 255, 0],
        };
        let turned = orient_preview(portrait, true);
        assert_eq!((turned.w, turned.h), (2, 1));
        assert_eq!(turned.data, vec![255, 0, 0, 0, 255, 0]);

        let landscape = raw_core::preview::Rgb8 {
            w: 2,
            h: 1,
            data: vec![255, 0, 0, 0, 255, 0],
        };
        let unchanged = orient_preview(landscape.clone(), true);
        assert_eq!(unchanged.data, landscape.data);
        assert_eq!((unchanged.w, unchanged.h), (2, 1));
    }

    #[test]
    fn the_default_grid_is_five_columns_by_seven_rows() {
        let dialog = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        );
        assert_eq!((dialog.settings.columns, dialog.settings.rows), (5, 7));
        let layout = PageLayout::new(&dialog.settings, dialog.settings.page_points());
        assert_eq!(layout.cells().count(), 35);
    }

    #[test]
    fn vertical_and_horizontal_cell_spacing_are_independent() {
        let mut settings = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings;
        settings.caption_source = CaptionSource::None;
        settings.vertical_spacing_inches = 0.25;
        settings.horizontal_spacing_inches = 0.5;
        let layout = PageLayout::new(&settings, settings.page_points());
        let cells: Vec<Rect> = layout.cells().collect();
        assert!((cells[1].x1 - cells[0].x2 - 36.0).abs() < 0.001);
        assert!((cells[0].y1 - cells[5].y2 - 18.0).abs() < 0.001);
    }

    #[test]
    fn four_page_margins_bound_the_grid_independently() {
        let mut settings = Dialog::new(
            Sources {
                selected: Vec::new(),
                visible: Vec::new(),
                folder: Vec::new(),
                cache: None,
            },
            Unit::Inches,
        )
        .settings;
        settings.margins_inches = PageMargins {
            top: 0.25,
            bottom: 0.5,
            left: 0.75,
            right: 1.0,
        };
        let page = settings.page_points();
        let layout = PageLayout::new(&settings, page);
        assert!((layout.grid.x1 - 54.0).abs() < 0.001);
        assert!((layout.grid.x2 - (page[0] - 72.0)).abs() < 0.001);
        assert!((layout.grid.y1 - 36.0).abs() < 0.001);
        assert!((layout.grid.y2 - (page[1] - 18.0)).abs() < 0.001);
    }

    #[test]
    fn exported_pdf_contains_images_embedded_font_and_two_line_footer() {
        let root =
            std::env::temp_dir().join(format!("monopro-contact-sheet-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut items = Vec::new();
        for index in 0..6 {
            let path = root.join(format!("frame-{index}.jpg"));
            let width = if index % 2 == 0 { 180 } else { 120 };
            let height = if index % 2 == 0 { 120 } else { 180 };
            let pixels: Vec<u8> = (0..width * height)
                .flat_map(|pixel| {
                    let value = ((pixel + index * 31) % 220 + 20) as u8;
                    [value, value.saturating_add(12), value.saturating_sub(12)]
                })
                .collect();
            let mut encoded = Vec::new();
            jpeg_encoder::Encoder::new(&mut encoded, 90)
                .encode(
                    &pixels,
                    width as u16,
                    height as u16,
                    jpeg_encoder::ColorType::Rgb,
                )
                .unwrap();
            std::fs::write(&path, encoded).unwrap();
            items.push(Source {
                path,
                name: format!("20260830_contact_{index:04}.jpg"),
                rating: index % 6,
                label: (index == 2).then(|| "magenta".to_owned()),
                orientation: None,
            });
        }
        let dialog = Dialog::new(
            Sources {
                selected: items.clone(),
                visible: items.clone(),
                folder: items.clone(),
                cache: None,
            },
            Unit::Inches,
        );
        let mut settings = dialog.settings;
        settings.preview = Preview::Gray;
        settings.header = "CONTACT SHEET · {page} of {pages}".to_owned();
        settings.footer = "EXAMPLE\nExample photo series".to_owned();
        settings.page_number = PageNumber::Footer;
        settings.rating = true;
        settings.color_label = true;
        settings.rotate_verticals = true;
        settings.black_background = true;
        let mut values: [String; IptcField::ALL.len()] = Default::default();
        let mixed = [false; IptcField::ALL.len()];
        values[IptcField::Creator as usize] = "Example Photographer".to_owned();
        values[IptcField::Title as usize] = "Example photo series".to_owned();
        values[IptcField::Copyright as usize] = "EXAMPLE".to_owned();
        let metadata =
            iptc_templates::Template::from_values("Contact Sheet".to_owned(), &values, &mixed);
        let keep = std::env::var_os("MONOPRO_CONTACT_SHEET_QA").is_some();
        let destination = if keep {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tmp/pdfs/monopro-contact-sheet-qa.pdf");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            path
        } else {
            root.join("contact-sheet.pdf")
        };
        let fonts = [
            dialog.fonts[0].clone(),
            dialog.fonts[0].clone(),
            dialog.fonts[0].clone(),
            dialog.fonts[0].clone(),
        ];
        write_pdf(
            &destination,
            &items,
            &settings,
            None,
            &fonts,
            Some(&metadata),
        )
        .unwrap();
        let bytes = std::fs::read(&destination).unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
        assert!(bytes.len() > 10_000);
        assert!(bytes.windows(6).any(|window| window == b"/Title"));
        assert!(bytes.windows(7).any(|window| window == b"/Author"));
        assert!(bytes.windows(3).any(|window| window == b"/F2"));
        assert!(bytes.windows(3).any(|window| window == b"/F3"));
        assert!(bytes.windows(3).any(|window| window == b"/F4"));
        if !keep {
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
