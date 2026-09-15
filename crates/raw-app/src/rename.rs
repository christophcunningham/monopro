//! Collision-safe file renaming for Lightbox.
//!
//! A photograph is not one pathname in monopro: its authored `.mono.xmp` sidecar,
//! an optional conventional `.xmp`, open Develop tabs, and Lightbox's filename-keyed
//! arrangement all follow it. This module owns the filesystem half. Lightbox and App
//! consume the returned [`Event`]s to update their in-memory views.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use crate::theme;

/// Every compact rename field uses the same vertically-centred single-line editor.
/// Keeping this in one constructor prevents a newly added batch field from silently
/// falling back to egui's top-aligned text inside our taller input boxes.
fn rename_edit(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).vertical_align(egui::Align::Center)
}

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub captured: Option<std::time::SystemTime>,
}

/// One batch of capture dates, as the worker hands them back: the row each answer
/// belongs to, and `None` where the file carried no date. Named because the bare type
/// is three levels of generic and reads as noise at the field that holds it.
type DateReply = Vec<(usize, Option<std::time::SystemTime>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sequence,
    Replace,
    Pattern,
}

pub enum DialogResponse {
    Cancel,
    Rename(Vec<Row>),
}

/// The in-app rename sheet. The extension is never editable: this is a filename
/// operation, not a file-conversion operation, and changing `.dng` to `.jpg` would
/// only make an unreadable raw whose name lies about it.
pub struct Dialog {
    sources: Vec<Source>,
    mode: Mode,
    single_name: String,
    sequence_base: String,
    sequence_start: u32,
    sequence_digits: usize,
    find: String,
    replace: String,
    pattern: String,
    request_focus: bool,
    error: Option<String>,
    date_loading: bool,
    date_rx: Option<mpsc::Receiver<DateReply>>,
}

impl Dialog {
    pub fn new(sources: Vec<Source>) -> Self {
        let first = sources
            .first()
            .and_then(|source| source.path.file_stem())
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let common = common_base(&first);
        Self {
            sources,
            mode: Mode::Sequence,
            single_name: first.clone(),
            sequence_base: if common.is_empty() { first } else { common },
            sequence_start: 1,
            sequence_digits: 4,
            find: String::new(),
            replace: String::new(),
            pattern: "{original}_{sequence}".to_owned(),
            request_focus: true,
            error: None,
            date_loading: false,
            date_rx: None,
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Option<DialogResponse> {
        self.poll_dates(ctx);
        if self.uses_capture_date() {
            self.begin_dates(ctx);
        }

        let mut answer = None;
        let batch = self.sources.len() > 1;
        let desired = if batch {
            egui::vec2(720.0, 500.0)
        } else {
            egui::vec2(520.0, 230.0)
        };
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));

        egui::Modal::new(egui::Id::new("lightbox-rename"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(theme::CHROME)
                    .stroke(egui::Stroke::new(2.0, theme::RUBY))
                    .corner_radius(2.0)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ctx, |ui| {
                ui.set_min_size(desired);
                ui.set_max_size(desired);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(if batch {
                            format!("RENAME {} FILES", self.sources.len())
                        } else {
                            "RENAME FILE".to_owned()
                        })
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

                if batch {
                    self.batch_ui(ui);
                } else {
                    self.single_ui(ui);
                }

                let preview = self.rows();
                if batch {
                    ui.add_space(14.0);
                    self.preview_ui(ui, preview.as_ref().ok());
                }
                let validation = preview.as_ref().err().cloned();
                let problem = self.error.as_ref().or(validation.as_ref());
                if let Some(problem) = problem {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(problem).color(theme::RUBY));
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            answer = Some(DialogResponse::Cancel);
                        }
                        let enabled = preview.is_ok() && !self.date_loading;
                        if ui
                            .add_enabled(enabled, egui::Button::new("Rename"))
                            .clicked()
                            || (enter && enabled)
                        {
                            answer = Some(DialogResponse::Rename(preview.unwrap()));
                        }
                    });
                });
            });

        if escape {
            Some(DialogResponse::Cancel)
        } else {
            answer
        }
    }

    fn single_ui(&mut self, ui: &mut egui::Ui) {
        let extension = self.sources[0]
            .path
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default();
        ui.label(theme::caption("Filename"));
        ui.horizontal(|ui| {
            let response = ui.add_sized(
                [ui.available_width() - 70.0, 26.0],
                rename_edit(&mut self.single_name),
            );
            if std::mem::take(&mut self.request_focus) {
                response.request_focus();
            }
            ui.label(egui::RichText::new(extension).color(theme::DIM));
        });
        ui.add_space(8.0);
        ui.label(theme::caption("The file type stays unchanged."));
    }

    fn batch_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.mode, Mode::Sequence, "Sequence");
            ui.selectable_value(&mut self.mode, Mode::Replace, "Find / Replace");
            ui.selectable_value(&mut self.mode, Mode::Pattern, "Pattern");
        });
        ui.add_space(12.0);

        egui::Grid::new("rename-controls")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| match self.mode {
                Mode::Sequence => {
                    ui.label("Base name");
                    ui.add_sized([360.0, 24.0], rename_edit(&mut self.sequence_base));
                    ui.end_row();
                    ui.label("Start at");
                    ui.add(egui::DragValue::new(&mut self.sequence_start).range(0..=999_999));
                    ui.end_row();
                    ui.label("Digits");
                    ui.add(egui::DragValue::new(&mut self.sequence_digits).range(1..=8));
                    ui.end_row();
                }
                Mode::Replace => {
                    ui.label("Find");
                    ui.add_sized([360.0, 24.0], rename_edit(&mut self.find));
                    ui.end_row();
                    ui.label("Replace with");
                    ui.add_sized([360.0, 24.0], rename_edit(&mut self.replace));
                    ui.end_row();
                }
                Mode::Pattern => {
                    ui.label("Pattern");
                    ui.add_sized([460.0, 24.0], rename_edit(&mut self.pattern));
                    ui.end_row();
                    ui.label("Tokens");
                    ui.horizontal_wrapped(|ui| {
                        for token in ["{original}", "{date}", "{time}", "{sequence}", "{folder}"] {
                            if ui.small_button(token).clicked() {
                                self.pattern.push_str(token);
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Sequence start");
                    ui.add(egui::DragValue::new(&mut self.sequence_start).range(0..=999_999));
                    ui.end_row();
                    ui.label("Sequence digits");
                    ui.add(egui::DragValue::new(&mut self.sequence_digits).range(1..=8));
                    ui.end_row();
                }
            });
    }

    fn preview_ui(&self, ui: &mut egui::Ui, rows: Option<&Vec<Row>>) {
        ui.label(theme::caption("PREVIEW · CURRENT LIGHTBOX ORDER"));
        egui::Frame::new()
            .fill(theme::CHROME_DEEP)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(210.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.date_loading {
                            ui.label(theme::caption("Reading capture dates…"));
                        }
                        if let Some(rows) = rows {
                            egui::Grid::new("rename-preview")
                                .num_columns(3)
                                .spacing([12.0, 4.0])
                                .show(ui, |ui| {
                                    for row in rows {
                                        ui.label(
                                            egui::RichText::new(file_name(&row.source))
                                                .color(theme::NAME),
                                        );
                                        ui.label(egui::RichText::new("→").color(theme::DIM));
                                        ui.label(
                                            egui::RichText::new(file_name(&row.destination))
                                                .color(theme::BRIGHT),
                                        );
                                        ui.end_row();
                                    }
                                });
                        }
                    });
            });
    }

    fn rows(&self) -> Result<Vec<Row>, String> {
        let mut rows = Vec::with_capacity(self.sources.len());
        let mut names = HashSet::new();
        for (index, source) in self.sources.iter().enumerate() {
            let original = source
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stem = if self.sources.len() == 1 {
                self.single_name.clone()
            } else {
                match self.mode {
                    Mode::Sequence => format!(
                        "{}_{:0width$}",
                        self.sequence_base,
                        self.sequence_start.saturating_add(index as u32),
                        width = self.sequence_digits
                    ),
                    Mode::Replace => {
                        if self.find.is_empty() {
                            return Err("Enter text to find".to_owned());
                        }
                        original.replace(&self.find, &self.replace)
                    }
                    Mode::Pattern => self.expand_pattern(source, index, &original)?,
                }
            };
            validate_filename(&stem)?;
            let destination = destination_for(&source.path, &stem);
            let key = normalised(&destination);
            if !names.insert(key) {
                return Err(format!(
                    "More than one image would become {}",
                    file_name(&destination)
                ));
            }
            rows.push(Row {
                source: source.path.clone(),
                destination,
            });
        }
        if rows.iter().all(|row| row.source == row.destination) {
            return Err("No filenames would change".to_owned());
        }
        let references: Vec<&Row> = rows.iter().collect();
        validate_rows(&references)?;
        Ok(rows)
    }

    fn expand_pattern(
        &self,
        source: &Source,
        index: usize,
        original: &str,
    ) -> Result<String, String> {
        let folder = source
            .path
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (date, time) = source
            .captured
            .map(date_and_time)
            .unwrap_or_else(|| (String::new(), String::new()));
        if self.uses_capture_date() && date.is_empty() {
            return Err(if self.date_loading {
                "Reading capture dates…".to_owned()
            } else {
                format!("{} has no capture date", file_name(&source.path))
            });
        }
        let sequence = format!(
            "{:0width$}",
            self.sequence_start.saturating_add(index as u32),
            width = self.sequence_digits
        );
        let mut out = String::new();
        let mut rest = self.pattern.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let token_start = &rest[open..];
            let Some(close) = token_start.find('}') else {
                return Err("Pattern has an unfinished token".to_owned());
            };
            let token = &token_start[..=close];
            out.push_str(match token {
                "{original}" => original,
                "{date}" => &date,
                "{time}" => &time,
                "{sequence}" => &sequence,
                "{folder}" => &folder,
                _ => return Err(format!("Unknown token {token}")),
            });
            rest = &token_start[close + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn uses_capture_date(&self) -> bool {
        self.sources.len() > 1
            && self.mode == Mode::Pattern
            && (self.pattern.contains("{date}") || self.pattern.contains("{time}"))
    }

    fn begin_dates(&mut self, ctx: &egui::Context) {
        if self.date_loading
            || self.date_rx.is_some()
            || self.sources.iter().all(|s| s.captured.is_some())
        {
            return;
        }
        let jobs: Vec<(usize, PathBuf)> = self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, source)| source.captured.is_none())
            .map(|(index, source)| (index, source.path.clone()))
            .collect();
        let (tx, rx) = mpsc::channel();
        self.date_loading = true;
        self.date_rx = Some(rx);
        std::thread::Builder::new()
            .name("monopro-rename-dates".into())
            .spawn(move || {
                use rayon::prelude::*;
                let found = jobs
                    .into_par_iter()
                    .map(|(index, path)| (index, raw_core::sensor::capture_time(&path)))
                    .collect();
                let _ = tx.send(found);
            })
            .ok();
        ctx.request_repaint_after(std::time::Duration::from_millis(40));
    }

    fn poll_dates(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.date_rx else { return };
        match rx.try_recv() {
            Ok(found) => {
                for (index, captured) in found {
                    if let Some(source) = self.sources.get_mut(index) {
                        source.captured = captured;
                    }
                }
                self.date_loading = false;
                self.date_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(40));
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.date_loading = false;
                self.date_rx = None;
            }
        }
    }
}

fn common_base(stem: &str) -> String {
    let trimmed = stem.trim_end_matches(|character: char| character.is_ascii_digit());
    trimmed.trim_end_matches(['_', '-', ' ']).to_owned()
}

fn destination_for(source: &Path, stem: &str) -> PathBuf {
    let mut name = OsString::from(stem);
    if let Some(extension) = source.extension() {
        name.push(".");
        name.push(extension);
    }
    source.with_file_name(name)
}

fn date_and_time(when: std::time::SystemTime) -> (String, String) {
    let seconds = when
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let days = seconds.div_euclid(86_400);
    let clock = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = clock / 3_600;
    let minute = (clock % 3_600) / 60;
    let second = clock % 60;
    (
        format!("{year:04}{month:02}{day:02}"),
        format!("{hour:02}{minute:02}{second:02}"),
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }).div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[derive(Debug, Clone)]
pub struct Row {
    pub source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub old: PathBuf,
    pub new: PathBuf,
}

#[derive(Debug)]
struct AuthoredSidecar {
    original: PathBuf,
    temporary: PathBuf,
    destination_image: PathBuf,
    sidecar: raw_core::sidecar::Sidecar,
}

#[derive(Debug)]
struct Move {
    original: PathBuf,
    temporary: PathBuf,
    destination: PathBuf,
    finalised: bool,
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Validate and perform a whole rename as one staged operation.
///
/// Every source is first moved to a unique temporary name beside itself. Only after
/// all of those moves succeed do final names land. That makes swaps safe and gives a
/// failed batch one rollback path instead of leaving its latter half untouched and
/// its former half renamed.
pub fn execute(rows: &[Row]) -> Result<Vec<Event>, String> {
    let rows: Vec<&Row> = rows
        .iter()
        .filter(|row| row.source != row.destination)
        .collect();
    if rows.is_empty() {
        return Err("No filenames would change".to_owned());
    }
    validate_rows(&rows)?;

    let mut moves = Vec::new();
    let mut authored = Vec::new();
    let mut move_sources = HashSet::new();

    for row in &rows {
        push_move(
            &mut moves,
            &mut move_sources,
            row.source.clone(),
            row.destination.clone(),
        )?;

        let own = raw_core::sidecar::path_for(&row.source);
        if own.exists() {
            let sidecar = match raw_core::sidecar::read(&row.source) {
                raw_core::sidecar::Loaded::Ok(sidecar) => sidecar,
                raw_core::sidecar::Loaded::Absent => {
                    return Err(format!("{} disappeared during rename", own.display()));
                }
                raw_core::sidecar::Loaded::Corrupt(why) => {
                    return Err(format!(
                        "{} has an unreadable monopro sidecar — {why}",
                        file_name(&row.source)
                    ));
                }
            };
            if !move_sources.insert(normalised(&own)) {
                return Err(format!("{} is included twice", own.display()));
            }
            authored.push(AuthoredSidecar {
                temporary: temporary_for(&own),
                original: own,
                destination_image: row.destination.clone(),
                sidecar,
            });
        }

        // A conventional Adobe-compatible sidecar follows the raw too. monopro does
        // not interpret or rewrite it; preserving its bytes is the respectful rule.
        let foreign = row.source.with_extension("xmp");
        if foreign.exists()
            && foreign != row.source
            && foreign != raw_core::sidecar::path_for(&row.source)
        {
            push_move(
                &mut moves,
                &mut move_sources,
                foreign,
                row.destination.with_extension("xmp"),
            )?;
        }
    }

    validate_destinations(&moves, &authored)?;

    // Phase one: take every source out of the destination namespace.
    for (staged_moves, movement) in moves.iter().enumerate() {
        if let Err(error) = rename_noreplace(&movement.original, &movement.temporary) {
            let recovery = rollback_staging(&moves[..staged_moves], &authored, 0);
            return Err(format!(
                "could not prepare {} for rename: {error}{recovery}",
                movement.original.display()
            ));
        }
    }
    for (staged_sidecars, sidecar) in authored.iter().enumerate() {
        if let Err(error) = rename_noreplace(&sidecar.original, &sidecar.temporary) {
            let recovery = rollback_staging(&moves, &authored, staged_sidecars);
            return Err(format!(
                "could not prepare {} for rename: {error}{recovery}",
                sidecar.original.display()
            ));
        }
    }

    // Phase two: land source images and foreign companions under final names.
    for at in 0..moves.len() {
        if let Err(error) = rename_noreplace(&moves[at].temporary, &moves[at].destination) {
            let recovery = rollback_final(&mut moves, &authored, &[]);
            return Err(format!(
                "could not rename {}: {error}{recovery}",
                moves[at].original.display()
            ));
        }
        moves[at].finalised = true;
    }

    // Rewrite authored sidecars rather than merely moving them: SourceFile is part
    // of monopro's XMP and must say the new filename while every edit remains intact.
    let mut written_sidecars = Vec::new();
    for sidecar in &authored {
        if let Err(error) = write_renamed_sidecar(sidecar) {
            let recovery = rollback_final(&mut moves, &authored, &written_sidecars);
            return Err(format!(
                "could not rewrite the sidecar for {}: {error}{recovery}",
                sidecar.destination_image.display()
            ));
        }
        written_sidecars.push(raw_core::sidecar::path_for(&sidecar.destination_image));
    }
    for sidecar in &authored {
        let _ = std::fs::remove_file(&sidecar.temporary);
    }

    Ok(rows
        .into_iter()
        .map(|row| Event {
            old: row.source.clone(),
            new: row.destination.clone(),
        })
        .collect())
}

fn validate_rows(rows: &[&Row]) -> Result<(), String> {
    let changed_sources: HashSet<String> = rows.iter().map(|row| normalised(&row.source)).collect();
    let mut destinations = HashSet::new();
    for row in rows {
        if !row.source.exists() {
            return Err(format!("{} is no longer available", row.source.display()));
        }
        if row.source.parent() != row.destination.parent() {
            return Err("Rename cannot move files between folders".to_owned());
        }
        let Some(name) = row.destination.file_name().and_then(|name| name.to_str()) else {
            return Err("A generated filename is not valid Unicode".to_owned());
        };
        validate_filename(name)?;
        let destination = normalised(&row.destination);
        if !destinations.insert(destination.clone()) {
            return Err(format!("More than one image would become {name}"));
        }
        if row.destination.exists() && !changed_sources.contains(&destination) {
            return Err(format!("{name} already exists"));
        }
    }
    Ok(())
}

fn validate_filename(name: &str) -> Result<(), String> {
    if name.trim().is_empty() || matches!(name, "." | "..") {
        return Err("A filename cannot be empty".to_owned());
    }
    if name != name.trim() {
        return Err(format!("“{name}” begins or ends with a space"));
    }
    if name.contains(['/', ':', '\0']) {
        return Err(format!(
            "“{name}” contains a character macOS cannot use here"
        ));
    }
    if name.len() > 240 {
        return Err(format!("“{name}” is too long"));
    }
    Ok(())
}

fn push_move(
    moves: &mut Vec<Move>,
    sources: &mut HashSet<String>,
    original: PathBuf,
    destination: PathBuf,
) -> Result<(), String> {
    if !sources.insert(normalised(&original)) {
        return Err(format!("{} is included twice", original.display()));
    }
    moves.push(Move {
        temporary: temporary_for(&original),
        original,
        destination,
        finalised: false,
    });
    Ok(())
}

fn validate_destinations(moves: &[Move], authored: &[AuthoredSidecar]) -> Result<(), String> {
    let sources: HashSet<String> = moves
        .iter()
        .map(|movement| normalised(&movement.original))
        .chain(authored.iter().map(|sidecar| normalised(&sidecar.original)))
        .collect();
    let mut destinations = HashSet::new();
    for movement in moves {
        let destination = &movement.destination;
        let label = file_name(destination);
        let key = normalised(destination);
        if !destinations.insert(key.clone()) {
            return Err(format!("More than one companion would become {label}"));
        }
        if destination.exists() && !sources.contains(&key) {
            return Err(format!("{label} already exists"));
        }
    }
    for sidecar in authored {
        let destination = raw_core::sidecar::path_for(&sidecar.destination_image);
        let key = normalised(&destination);
        if !destinations.insert(key.clone()) {
            return Err(format!(
                "More than one companion would become {}",
                file_name(&destination)
            ));
        }
        if destination.exists() && !sources.contains(&key) {
            return Err(format!("{} already exists", file_name(&destination)));
        }
    }
    Ok(())
}

fn recovery_notice(errors: Vec<String>) -> String {
    if errors.is_empty() {
        String::new()
    } else {
        format!(
            "\nRollback incomplete. Keep the staging files for recovery:\n{}",
            errors.join("\n")
        )
    }
}

fn restore(source: &Path, destination: &Path, errors: &mut Vec<String>) -> bool {
    if let Err(error) = rename_noreplace(source, destination) {
        errors.push(format!(
            "{} -> {}: {error}",
            source.display(),
            destination.display()
        ));
        false
    } else {
        true
    }
}

fn rollback_staging(
    moves: &[Move],
    authored: &[AuthoredSidecar],
    staged_sidecars: usize,
) -> String {
    let mut errors = Vec::new();
    for sidecar in authored[..staged_sidecars].iter().rev() {
        restore(&sidecar.temporary, &sidecar.original, &mut errors);
    }
    for movement in moves.iter().rev() {
        restore(&movement.temporary, &movement.original, &mut errors);
    }
    recovery_notice(errors)
}

fn rollback_final(moves: &mut [Move], authored: &[AuthoredSidecar], written: &[PathBuf]) -> String {
    let mut errors = Vec::new();
    for path in written {
        if let Err(error) = std::fs::remove_file(path) {
            errors.push(format!("could not remove {}: {error}", path.display()));
        }
    }
    // A batch may be a name swap. Going directly from each final destination back
    // to its original would overwrite the other half of the swap, so first vacate
    // the entire final namespace into the already unique staging names.
    for movement in moves.iter_mut() {
        if movement.finalised && restore(&movement.destination, &movement.temporary, &mut errors) {
            movement.finalised = false;
        }
    }
    for movement in moves.iter().rev() {
        if !movement.finalised {
            restore(&movement.temporary, &movement.original, &mut errors);
        }
    }
    for sidecar in authored.iter().rev() {
        restore(&sidecar.temporary, &sidecar.original, &mut errors);
    }
    recovery_notice(errors)
}

fn write_renamed_sidecar(sidecar: &AuthoredSidecar) -> std::io::Result<()> {
    use std::io::Write;
    let destination = raw_core::sidecar::path_for(&sidecar.destination_image);
    let temporary = temporary_for(&destination);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let xml = raw_core::sidecar::to_xml(
        &sidecar.sidecar.params,
        &sidecar.sidecar.metadata,
        &file_name(&sidecar.destination_image),
    );
    let written = file
        .write_all(xml.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| rename_noreplace(&temporary, &destination));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn temporary_for(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    loop {
        let serial = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".monopro-rename-{}-{serial}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn normalised(path: &Path) -> String {
    // Existing names are aliases only when the filesystem says so. Lowercasing
    // two distinct files on case-sensitive storage would permit an overwrite.
    #[cfg(unix)]
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::MetadataExt;
        return format!("file:{}:{}", meta.dev(), meta.ino());
    }
    format!("path:{}", path.to_string_lossy().to_lowercase())
}

#[cfg(target_os = "macos")]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
    };
    let source = path(source)?;
    let destination = path(destination)?;
    // SAFETY: both paths are live, NUL-terminated C strings. RENAME_EXCL makes
    // the existence check and move one filesystem operation, including rollback.
    if unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Renames stay within one directory. Refuse unsupported filesystems rather
    // than falling back to a move that could overwrite an unrelated file.
    std::fs::hard_link(source, destination)?;
    if let Err(error) = std::fs::remove_file(source) {
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "monopro-rename-{tag}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_destination_appearing_after_validation_is_not_overwritten() {
        let dir = temp("late-collision");
        let source = dir.join("source.dng");
        let destination = dir.join("destination.dng");
        std::fs::write(&source, b"source").unwrap();
        let row = Row {
            source: source.clone(),
            destination: destination.clone(),
        };
        validate_rows(&[&row]).unwrap();
        std::fs::write(&destination, b"arrived later").unwrap();
        assert!(rename_noreplace(&source, &destination).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(std::fs::read(&destination).unwrap(), b"arrived later");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn case_only_renames_follow_filesystem_identity() {
        use std::io::Write;
        let dir = temp("case");
        let lower = dir.join("a.dng");
        let upper = dir.join("A.dng");
        std::fs::write(&lower, b"lower").unwrap();
        let row = Row {
            source: lower.clone(),
            destination: upper.clone(),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&upper)
        {
            Ok(mut f) => {
                f.write_all(b"upper").unwrap();
                drop(f);
                assert_ne!(normalised(&lower), normalised(&upper));
                assert!(execute(&[row]).is_err());
                assert_eq!(std::fs::read(lower).unwrap(), b"lower");
                assert_eq!(std::fs::read(upper).unwrap(), b"upper");
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                execute(&[row]).unwrap();
                assert_eq!(std::fs::read(upper).unwrap(), b"lower");
            }
            Err(e) => panic!("cannot create case fixture: {e}"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rollback_reports_a_collision_and_keeps_both_files() {
        let dir = temp("rollback");
        let original = dir.join("A.dng");
        let destination = dir.join("B.dng");
        let temporary = dir.join("staging");
        std::fs::write(&destination, b"photograph").unwrap();
        std::fs::write(&temporary, b"unrelated file").unwrap();
        let mut moves = [Move {
            original: original.clone(),
            destination: destination.clone(),
            temporary: temporary.clone(),
            finalised: true,
        }];
        let notice = rollback_final(&mut moves, &[], &[]);
        assert!(notice.contains("Rollback incomplete"));
        assert!(notice.contains(&destination.display().to_string()));
        assert!(!original.exists());
        assert_eq!(std::fs::read(destination).unwrap(), b"photograph");
        assert_eq!(std::fs::read(temporary).unwrap(), b"unrelated file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_new_sidecar_at_the_destination_is_not_overwritten() {
        let dir = temp("sidecar-collision");
        let destination_image = dir.join("B.dng");
        let destination = raw_core::sidecar::path_for(&destination_image);
        std::fs::write(&destination, b"arrived later").unwrap();
        let sidecar = raw_core::sidecar::from_xml(&raw_core::sidecar::to_xml(
            &Default::default(),
            &Default::default(),
            "A.dng",
        ))
        .ok()
        .unwrap();
        let authored = AuthoredSidecar {
            original: dir.join("A.mono.xmp"),
            temporary: dir.join("old-backup"),
            destination_image,
            sidecar,
        };
        assert!(write_renamed_sidecar(&authored).is_err());
        assert_eq!(std::fs::read(destination).unwrap(), b"arrived later");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_rename_carries_and_rewrites_both_sidecars() {
        let dir = temp("companions");
        let old = dir.join("L100001.dng");
        let new = dir.join("session_0001.dng");
        std::fs::write(&old, b"raw").unwrap();
        std::fs::write(old.with_extension("xmp"), b"foreign").unwrap();
        let params = raw_core::Params {
            exposure: raw_core::ExposureParams {
                ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        };
        raw_core::sidecar::write(&old, &params, &Default::default()).unwrap();

        let events = execute(&[Row {
            source: old.clone(),
            destination: new.clone(),
        }])
        .unwrap();

        assert_eq!(
            events,
            [Event {
                old: old.clone(),
                new: new.clone()
            }]
        );
        assert!(!old.exists() && new.exists());
        assert_eq!(
            std::fs::read(new.with_extension("xmp")).unwrap(),
            b"foreign"
        );
        let xml = std::fs::read_to_string(raw_core::sidecar::path_for(&new)).unwrap();
        assert!(xml.contains("SourceFile=\"session_0001.dng\""));
        assert!(!raw_core::sidecar::path_for(&old).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_batch_can_swap_names_without_overwriting() {
        let dir = temp("swap");
        let a = dir.join("a.dng");
        let b = dir.join("b.dng");
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&b, b"B").unwrap();

        execute(&[
            Row {
                source: a.clone(),
                destination: b.clone(),
            },
            Row {
                source: b.clone(),
                destination: a.clone(),
            },
        ])
        .unwrap();

        assert_eq!(std::fs::read(&a).unwrap(), b"B");
        assert_eq!(std::fs::read(&b).unwrap(), b"A");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn collisions_are_refused_before_any_file_moves() {
        let dir = temp("collision");
        let a = dir.join("a.dng");
        let occupied = dir.join("occupied.dng");
        std::fs::write(&a, b"A").unwrap();
        std::fs::write(&occupied, b"untouched").unwrap();

        assert!(
            execute(&[Row {
                source: a.clone(),
                destination: occupied.clone(),
            }])
            .is_err()
        );
        assert_eq!(std::fs::read(&a).unwrap(), b"A");
        assert_eq!(std::fs::read(&occupied).unwrap(), b"untouched");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sequence_names_follow_the_sources_order_and_keep_each_extension() {
        let dir = temp("sequence");
        let first = dir.join("later.jpg");
        let second = dir.join("earlier.dng");
        std::fs::write(&first, b"one").unwrap();
        std::fs::write(&second, b"two").unwrap();
        let mut dialog = Dialog::new(vec![
            Source {
                path: first,
                captured: None,
            },
            Source {
                path: second,
                captured: None,
            },
        ]);
        dialog.sequence_base = "sailing".into();
        dialog.sequence_start = 7;
        dialog.sequence_digits = 2;

        let rows = dialog.rows().unwrap();
        assert_eq!(rows[0].destination, dir.join("sailing_07.jpg"));
        assert_eq!(rows[1].destination, dir.join("sailing_08.dng"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pattern_tokens_do_not_reinterpret_tokens_inside_the_original_name() {
        let dir = temp("pattern");
        let first = dir.join("{sequence}_negative.dng");
        let second = dir.join("second.dng");
        std::fs::write(&first, b"raw").unwrap();
        std::fs::write(&second, b"raw").unwrap();
        let mut dialog = Dialog::new(vec![
            Source {
                path: first,
                captured: Some(std::time::UNIX_EPOCH),
            },
            Source {
                path: second,
                captured: Some(std::time::UNIX_EPOCH),
            },
        ]);
        dialog.mode = Mode::Pattern;
        dialog.pattern = "{date}_{time}_{original}_{sequence}".into();
        dialog.sequence_start = 3;
        dialog.sequence_digits = 2;

        let rows = dialog.rows().unwrap();
        assert_eq!(
            file_name(&rows[0].destination),
            "19700101_000000_{sequence}_negative_03.dng"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
