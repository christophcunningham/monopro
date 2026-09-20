//! The updater boundary for platforms that have no updater yet.
//!
//! Windows ships an Inno Setup EXE and Linux an AppImage (see
//! `packaging/README.md`); neither has a feed or an installer this app drives
//! today, and the Sparkle machinery behind the macOS half is macOS-only by
//! nature. Every entry point the macOS module exposes exists here as a no-op,
//! so `main.rs` reads identically on both sides of the boundary: the badge is
//! never shown, the sheet never opens, and "Check for Updates" reports plainly
//! that there is nothing to check with.

use crate::settings::Settings;

/// What the macOS module calls a stage. Always idle here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Stage {
    Idle,
    Available,
    Staged,
    Failed,
}

/// The badge the title strip would draw. Never constructed here.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub struct Badge {
    pub text: String,
    pub detail: String,
    pub failed: bool,
}

pub struct Updates;

impl Updates {
    pub fn install(_config: &Settings, _ctx: &egui::Context) -> Self {
        Self
    }

    /// No events to drain, nothing to report.
    pub fn poll(&mut self, _config: &mut Settings) -> Option<String> {
        None
    }

    pub fn badge(&self) -> Option<Badge> {
        None
    }

    pub fn sheet_ready(&self) -> bool {
        false
    }

    pub fn sheet_version_line(&self) -> String {
        "monopro".to_owned()
    }

    pub fn sheet_notes(&self) -> &str {
        ""
    }

    pub fn sheet_date(&self) -> Option<&str> {
        None
    }

    pub fn sheet_status(&self) -> Option<&str> {
        None
    }

    pub fn restart_now_ready(&self) -> bool {
        false
    }

    pub fn choose_update_on_quit(&mut self) {}

    pub fn check_now(&mut self, _config: &mut Settings) -> Option<String> {
        Some("updates are not available on this platform yet".to_owned())
    }

    pub fn skip_this_version(&mut self, _config: &mut Settings) {}

    pub fn stop_skipping(&mut self, _config: &mut Settings) {}

    pub fn skipped_version(_config: &Settings) -> Option<&str> {
        None
    }

    pub fn set_auto_check(&self, _enabled: bool) {}

    pub fn set_busy(&self, _busy: bool) {}

    pub fn export_settled(&mut self) {}
}
