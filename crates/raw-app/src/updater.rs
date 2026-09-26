//! The macOS auto-updater: Sparkle on the wire, Mole-like manners on the screen.
//!
//! # What Sparkle owns and what monopro owns
//!
//! Sparkle 2 does everything that can hurt: the feed fetch, the Ed25519 signature
//! check, the download, the staging, and the replace-the-running-app install. What
//! it does *not* do here is talk to the user. Its standard alert for scheduled
//! updates is suppressed — [`GentleReminders::should_show_scheduled_update`]
//! returns `false`, which hands presentation to us — and presentation means a
//! badge at the right end of the title strip and a sheet when it is clicked.
//! Nothing appears while the app is up to date; silence is the resting state.
//!
//! # The three choices
//!
//! - **Update on quit** (the default): Sparkle has already downloaded and staged
//!   the update (`automaticallyDownloadsUpdates`); the staged installer survives
//!   app termination and completes the swap after the process exits. Nothing is
//!   replaced underneath a running session.
//! - **Restart now**: a user-initiated check, which per Sparkle's contract goes
//!   through its own standard interface — one native confirm, then install and
//!   relaunch. Only offered when the machine is idle; see [`Updates::check_now`].
//! - **Skip this version**: written to Sparkle's own skip default *and* to
//!   `settings.toml`, and — because auto-download usually means the update is
//!   already staged for install-on-quit by the time the sheet is open —
//!   answered through Sparkle's own pending update alert with its Skip choice.
//!   That reply is what reaches the installer driver and cancels the staged
//!   installation; the user defaults alone are only consulted when filtering
//!   the feed. The preference keeps a visible control in Settings → About.
//!
//! # Busy state
//!
//! A relaunch Sparkle asks for while an export thread is still writing is
//! postponed via [`RelaunchHandler`] and resumed when the export drains — an
//! export is never cut in half by an install, and a half-replaced bundle is
//! never left behind. Failures (bad signature, dead network, refused download)
//! become a one-line status; the app keeps running on the current version, and
//! the manual DMG download is always the fallback.
//!
//! # Lifecycle
//!
//! Installed on the **first frame**, exactly like `menu::Menus::install` — that
//! is the earliest point at which `NSApp` exists, and Sparkle drives its checks
//! from AppKit's run loop. Outside a real `.app` bundle (a `cargo run` development
//! build) [`SparkleUpdater::new`] returns `None` and every entry point becomes a
//! no-op: updates are a packaged-app feature and development is the explicit
//! non-case. Events arrive on the main thread through a channel and are drained
//! once per frame by [`Updates::poll`], the same shape `poll_export` and
//! `Menus::pressed` use.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use objc2_foundation::{NSString, NSUserDefaults};
use sparkle_updater::{
    EventCallback, GentleReminders, MainThreadMarker, RelaunchContinuation, RelaunchHandler,
    SkipOutcome, SparkleUpdater, UpdateEvent, UpdaterConfig,
};

use crate::platform;
use crate::settings::{self, Settings};

/// Sparkle's own skipped-version user defaults. Written here so Sparkle's
/// checks and ours skip the same version; see `SPUSkippedUpdate`. A minor
/// version lives in the first; a major upgrade is recorded by Sparkle in the
/// other two, and clearing follows `clearSkippedUpdateForHost`: all three.
const SKIPPED_KEY: &str = "SUSkippedVersion";
const SKIPPED_MAJOR_KEY: &str = "SUSkippedMajorVersion";
const SKIPPED_MAJOR_SUBRELEASE_KEY: &str = "SUSkippedMajorSubreleaseVersion";

/// How often Sparkle's scheduled background check runs. Matches
/// `SUScheduledCheckInterval` in the packaged Info.plist; the plist is the
/// default, this is what the app asserts when it installs the updater.
const CHECK_INTERVAL_SECS: f64 = 86_400.0;

/// How many resume checks a pending skip may spend trying to reach Sparkle's
/// pending update alert. A check issued while Sparkle is mid-session is a
/// no-op; the cycle-finished event asks once more. Two bounds the retries
/// while still covering "skipped during the download" and "skipped after
/// staging" without polling the feed.
const SKIP_RESUME_ATTEMPTS: u8 = 2;
const SKIP_WARNING_STATUS: &str =
    "Skip was not confirmed; a staged update may still install when monopro quits";

/// What the updater is currently holding for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Nothing found, nothing downloading. No badge, no sheet, no noise.
    Idle,
    /// A valid update exists on the stable feed. The badge is on.
    Available,
    /// The update is downloaded, verified and staged. Install happens at quit,
    /// or on request when the machine is idle.
    Staged,
    /// The last session failed (bad signature, refused download, offline). The
    /// app keeps running on the current version and says so in one line.
    Failed,
}

/// One frame's worth of updater state, as the chrome reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Badge {
    pub text: String,
    /// The one-line status behind the badge, shown as its tooltip.
    pub detail: String,
    /// True while the last attempt failed — the badge borrows the ruby used
    /// elsewhere for states that want attention.
    pub failed: bool,
}

/// What an event from Sparkle means for the app. The mapping happens once, in
/// the event callback, so the UI never sees ObjC types.
enum Notice {
    /// A valid update is on the feed. Carries the plain-text notes, which are
    /// cached to disk the moment they arrive.
    Found {
        version: String,
        notes: Option<String>,
        date: Option<String>,
    },
    /// The feed answered and there is nothing to install.
    UpToDate,
    /// Bytes are moving.
    Downloading,
    /// The download finished and passed verification.
    Downloaded,
    /// Sparkle is about to install (and relaunch).
    Installing,
    /// A staged update will install when the app quits.
    StagedForQuit,
    /// Sparkle finished an update cycle. Carries nothing: it exists so a
    /// pending skip can ask once more for the staged update's alert after the
    /// check that staged it has ended.
    CycleFinished,
    /// A download or verification attempt failed. One line.
    Failed(String),
    /// The user answered one of Sparkle's own dialogs (the manual-check route).
    Choice {
        choice: &'static str,
        version: String,
    },
}

/// The cached found update, so an offline launch still shows the badge and the
/// notes it cached the day it could see the feed. Lives in the rebuildable
/// cache tree — losing it costs one launch of nothing, never a preference.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    version: Option<String>,
    notes: Option<String>,
    date: Option<String>,
}

impl Cache {
    fn path() -> Option<PathBuf> {
        platform::cache_dir(&settings::app_id()).map(|dir| dir.join("updates.toml"))
    }

    fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    fn store(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let body = toml::to_string(self).unwrap_or_default();
        let _ = raw_core::atomic_file::write(&path, |file| {
            use std::io::Write;
            file.write_all(body.as_bytes())
        });
    }

    fn discard() {
        if let Some(path) = Self::path() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Presentation decisions Sparkle asks the host to make.
struct Presentation {
    notices: Sender<Notice>,
}

impl GentleReminders for Presentation {
    /// **Always ours.** A scheduled check that found something must not produce a
    /// native alert mid-edit; the badge is the whole announcement. Returning
    /// `false` tells Sparkle the host takes responsibility — see the module note.
    fn should_show_scheduled_update(
        &self,
        _update: &sparkle_updater::events::UpdateInfo,
        _immediate_focus: bool,
    ) -> bool {
        false
    }

    /// Called with `handled_by_sparkle == false` exactly because the answer above
    /// took the presentation away. The item itself arrives through
    /// `didFindValidUpdate`; this only nags when that did not fire.
    fn will_show_update(
        &self,
        handled_by_sparkle: bool,
        _update: &sparkle_updater::events::UpdateInfo,
        _state: sparkle_updater::UserUpdateState,
    ) {
        if !handled_by_sparkle {
            let _ = self.notices.send(Notice::Downloading);
        }
    }
}

/// Map a Sparkle event into an app notice. Runs on the main thread, inside
/// Sparkle's delegate, between frames.
fn notice_of(event: UpdateEvent) -> Option<Notice> {
    Some(match event {
        UpdateEvent::DidFindValidUpdate(info) => Notice::Found {
            version: info.version,
            notes: info.release_notes,
            date: info.date_string,
        },
        UpdateEvent::DidNotFindUpdate(no) => {
            // "No eligible update" must be read before being called up to date:
            // skipped versions land here too, and so do OS-ineligible items.
            match no.reason {
                sparkle_updater::events::NoUpdateReason::OnLatestVersion
                | sparkle_updater::events::NoUpdateReason::OnNewerThanLatestVersion => {
                    Notice::UpToDate
                }
                _ => Notice::Failed(
                    no.recovery_suggestion
                        .unwrap_or_else(|| "no applicable update on the feed".to_owned()),
                ),
            }
        }
        UpdateEvent::WillDownloadUpdate(_) => Notice::Downloading,
        UpdateEvent::DidDownloadUpdate(_) => Notice::Downloaded,
        UpdateEvent::WillExtractUpdate(_) | UpdateEvent::DidExtractUpdate(_) => return None,
        UpdateEvent::WillInstallUpdate(_) => Notice::Installing,
        UpdateEvent::WillInstallUpdateOnQuit(_) => Notice::StagedForQuit,
        UpdateEvent::FailedToDownloadUpdate(failed) => Notice::Failed(failed.error.message),
        UpdateEvent::DidAbortWithError(payload) => {
            // A benign abort with a no-update payload is the feed saying
            // "current", not a failure — Sparkle routes that case through here.
            match payload.no_update {
                Some(no) => match no.reason {
                    sparkle_updater::events::NoUpdateReason::OnLatestVersion
                    | sparkle_updater::events::NoUpdateReason::OnNewerThanLatestVersion => {
                        Notice::UpToDate
                    }
                    _ => Notice::Failed(payload.message),
                },
                None => Notice::Failed(payload.message),
            }
        }
        UpdateEvent::UserDidMakeChoice(choice) => Notice::Choice {
            choice: match choice.choice.as_str() {
                "skip" => "skip",
                "install" => "install",
                _ => "dismiss",
            },
            version: choice.version,
        },
        UpdateEvent::WillRelaunchApplication => Notice::Installing,
        UpdateEvent::UserDidCancelDownload => Notice::Failed("download canceled".to_owned()),
        // The end of a cycle is the cue a pending skip waits for: the check
        // that downloaded and staged the update has finished, so a resumed
        // check now sees the installer in progress instead of a live session.
        UpdateEvent::DidFinishUpdateCycle(_) => Notice::CycleFinished,
        // Feed bookkeeping and scheduling telemetry change nothing on screen.
        // A wildcard, not an itemised tail: `UpdateEvent` is `#[non_exhaustive]`
        // and a future Sparkle event must not fail this build.
        UpdateEvent::DidFinishLoadingAppcast
        | UpdateEvent::WillScheduleUpdateCheck(_)
        | UpdateEvent::WillNotScheduleUpdateCheck
        | _ => return None,
    })
}

/// Write Sparkle's own skip record, so its scheduled checks stop offering the
/// version the user declined. Absent updater or failed write is harmless: the
/// settings copy still governs the badge.
fn write_sparkle_skip(version: Option<&str>) {
    let defaults = NSUserDefaults::standardUserDefaults();
    match version {
        Some(v) => {
            let key = NSString::from_str(SKIPPED_KEY);
            let value = NSString::from_str(v);
            // Unsafe per objc2: NSUserDefaults throws if the value is not a
            // property-list object. A string is one.
            unsafe { defaults.setObject_forKey(Some(&value), &key) };
        }
        None => {
            for key in [SKIPPED_KEY, SKIPPED_MAJOR_KEY, SKIPPED_MAJOR_SUBRELEASE_KEY] {
                defaults.removeObjectForKey(&NSString::from_str(key));
            }
        }
    }
}

/// The macOS updater. One per app, created on the first frame.
pub struct Updates {
    /// `None` outside an application bundle — see the module note. Every method
    /// tolerates it, so call sites never branch on it.
    updater: Option<SparkleUpdater>,
    notices: Receiver<Notice>,
    stage: Stage,
    /// The version on the other side of the badge.
    version: Option<String>,
    /// Plain-text release notes for the sheet, cached for offline launches.
    notes: String,
    /// When the feed item is dated, as offered.
    date: Option<String>,
    /// This app's own version, from the bundle.
    installed: Option<String>,
    /// Set while an export thread runs — shared with the app so Sparkle's
    /// relaunch requests defer to it.
    busy: Rc<AtomicBool>,
    /// A relaunch that was postponed because the app was busy. Resumed by
    /// [`Updates::export_settled`] when the last export drains.
    pending_relaunch: Rc<RefCell<Option<RelaunchContinuation>>>,
    /// The user chose "update on quit"; the badge softens and the sheet says so.
    update_on_quit: bool,
    /// One-line status, shown in the sheet and the About page.
    status: Option<String>,
    /// A skipped version Sparkle may still be holding — a staged install, or
    /// a pending alert whose reply is the only route to the installer driver.
    /// Cleared once Sparkle's own Skip choice has been delivered or the user
    /// explicitly stops skipping.
    skip_pending: Option<String>,
    /// Resume checks already spent on the current [`Self::skip_pending`].
    skip_attempts: u8,
    /// A skip recorded locally but not confirmed by Sparkle. A staged installer
    /// may still run at quit; a feed result alone cannot settle that question.
    skip_undelivered: Option<String>,
    /// Sparkle reported a finished download or a staged install since the last
    /// new offer. Without one, a skip has no installer to cancel, and warning
    /// that one "may still install" is a false alarm with no way to clear it.
    staged_seen: bool,
}

impl Updates {
    /// Build the updater on the first frame, next to the menu bar install.
    ///
    /// Must be called on the main thread — it is — and reads `settings.toml` so
    /// the persisted auto-check preference is the one Sparkle runs with.
    pub fn install(config: &Settings, ctx: &egui::Context) -> Self {
        let (tx, rx) = channel::<Notice>();
        let mut installed = Self {
            updater: None,
            notices: rx,
            stage: Stage::Idle,
            version: None,
            notes: String::new(),
            date: None,
            installed: None,
            busy: Rc::new(AtomicBool::new(false)),
            pending_relaunch: Rc::new(RefCell::new(None)),
            update_on_quit: false,
            status: None,
            skip_pending: None,
            skip_attempts: 0,
            skip_undelivered: None,
            staged_seen: false,
        };

        let updater = MainThreadMarker::new().and_then(|mtm| {
            SparkleUpdater::new(
                mtm,
                UpdaterConfig {
                    event_callback: Some(Self::callback(tx.clone(), ctx.clone())),
                    relaunch_handler: Some(Self::relaunch(
                        installed.busy.clone(),
                        installed.pending_relaunch.clone(),
                    )),
                    gentle_reminders: Some(Rc::new(Presentation { notices: tx })),
                },
            )
            .ok()
            .flatten()
        });
        let Some(updater) = updater else {
            // Development build, or a bundle Sparkle refuses to start in. The app
            // runs exactly as it did before the updater existed.
            return installed;
        };

        // The persisted preference is the authority, and it is re-asserted every
        // launch so the Settings control and Sparkle can never drift apart.
        let _ = updater.set_automatically_checks_for_updates(config.check_for_updates);
        let _ = updater.set_update_check_interval(CHECK_INTERVAL_SECS);
        // Update-on-quit is the default offer: download in the background, never
        // interrupt, and the swap waits for a clean quit.
        let _ = updater.set_automatically_downloads_updates(true);

        installed.installed = updater.current_version().ok();
        installed.updater = Some(updater);

        // A cached badge from a previous session reappears now — before the
        // network is consulted — so an offline launch still shows the update it
        // already knows about. A cache for the version this build *is* is stale,
        // and so is one for a version the user declined.
        let cache = Cache::load();
        match (&cache.version, &installed.installed) {
            (Some(version), Some(current))
                if version != current
                    && config.skipped_update_version.as_deref() != Some(version.as_str()) =>
            {
                installed.stage = Stage::Available;
                installed.version = Some(version.clone());
                installed.notes = cache.notes.unwrap_or_default();
                installed.date = cache.date;
            }
            (Some(_), _) => Cache::discard(),
            _ => {}
        }
        installed
    }

    /// The event callback: map to a notice, drop it in the channel, and ask for
    /// a repaint so the badge moves on the frame after the event rather than
    /// whenever the next one happens to be.
    fn callback(tx: Sender<Notice>, ctx: egui::Context) -> EventCallback {
        Rc::new(move |event| {
            if let Some(notice) = notice_of(event) {
                let _ = tx.send(notice);
                ctx.request_repaint();
            }
        })
    }

    /// The relaunch handler: Sparkle wants to restart the app while an export is
    /// still writing. Postpone until the work drains, then let it run.
    fn relaunch(
        busy: Rc<AtomicBool>,
        pending: Rc<RefCell<Option<RelaunchContinuation>>>,
    ) -> RelaunchHandler {
        Rc::new(move |_update, continuation| {
            if busy.load(Ordering::Relaxed) {
                *pending.borrow_mut() = Some(continuation);
                // The host owns the relaunch now; `export_settled` resumes it.
            } else {
                continuation.resume(
                    MainThreadMarker::new().expect("the relaunch handler runs on the main thread"),
                );
            }
        })
    }

    /// Drain Sparkle's events into visible state. Called once per frame; returns
    /// a one-line note for the footer when something deserves one.
    pub fn poll(&mut self, config: &mut Settings) -> Option<String> {
        let mut note = None;
        while let Ok(notice) = self.notices.try_recv() {
            match notice {
                Notice::Found {
                    version,
                    notes,
                    date,
                } => {
                    // The feed is offering the version the user declined. This
                    // is the resumed staged install talking: the appcast filter
                    // never sees it, so the skip has to be delivered through
                    // Sparkle's own alert reply, which is what cancels the
                    // installer. Keep any warning until Sparkle answers Skip.
                    if Self::skipped_version(config) == Some(version.as_str()) {
                        if let Some(result) = self
                            .updater
                            .as_ref()
                            .map(|updater| updater.skip_current_update())
                        {
                            self.record_skip_reply(&version, result);
                        }
                        self.stage = Stage::Idle;
                        self.version = None;
                        self.notes.clear();
                        self.date = None;
                        self.update_on_quit = false;
                        Cache::discard();
                        self.status = Some(format!("skipping monopro {version}"));
                        continue;
                    }
                    // The feed has moved past the skipped version, so the old
                    // cancel has nothing left to reach. The new offer is a
                    // fresh decision, but it does not prove an older staged
                    // installer was canceled.
                    self.skip_pending = None;
                    self.skip_attempts = 0;
                    let is_new = self.version.as_deref() != Some(version.as_str());
                    if is_new {
                        self.staged_seen = false;
                    }
                    self.stage = Stage::Available;
                    self.date = date;
                    self.update_on_quit = false;
                    self.status = None;
                    if is_new {
                        self.version = Some(version.clone());
                        self.notes = notes.unwrap_or_default();
                        Cache {
                            version: Some(version),
                            notes: (!self.notes.is_empty()).then(|| self.notes.clone()),
                            date: self.date.clone(),
                        }
                        .store();
                        note.get_or_insert_with(|| {
                            format!(
                                "monopro {} is available — see the badge for details",
                                self.version()
                            )
                        });
                    }
                }
                Notice::UpToDate => {
                    // The feed answered: anything we cached is now known stale,
                    // and a skip this feed has moved past stops mattering here.
                    if self.stage != Stage::Idle || self.version.is_some() {
                        self.stage = Stage::Idle;
                        self.version = None;
                        self.notes.clear();
                        self.date = None;
                        Cache::discard();
                    }
                    // Feed eligibility cannot prove an earlier staged installer
                    // was canceled. Keep retrying a pending skip after the
                    // cycle, and keep its warning visible in the meantime.
                    // With nothing ever staged there is nothing to cancel.
                    if let Some(pending) = &self.skip_pending {
                        if self.staged_seen {
                            self.skip_undelivered = Some(pending.clone());
                        } else {
                            self.skip_pending = None;
                            self.skip_attempts = 0;
                        }
                    }
                    if self.skip_undelivered.is_none() {
                        note.get_or_insert_with(|| "monopro is up to date".to_owned());
                        self.status = Some("monopro is up to date".to_owned());
                    }
                }
                Notice::Downloading => {
                    if self.stage != Stage::Staged {
                        self.stage = Stage::Available;
                        self.status = Some("downloading the update…".to_owned());
                    }
                }
                Notice::Downloaded => {
                    // Not staged yet: Sparkle validates the signature after the
                    // download, and a rejected update must not read as ready.
                    // `StagedForQuit` is the event that follows a valid one.
                    self.staged_seen = true;
                    if self.stage != Stage::Staged {
                        self.status = Some("verifying the update…".to_owned());
                    }
                }
                Notice::StagedForQuit => {
                    self.staged_seen = true;
                    self.stage = Stage::Staged;
                    self.status = Some("update staged — it installs when monopro quits".to_owned());
                }
                Notice::Installing => {
                    self.staged_seen = true;
                    self.stage = Stage::Staged;
                    self.status = Some("installing the update…".to_owned());
                }
                Notice::CycleFinished => {
                    // A check that was already running when the user skipped has
                    // now ended; if it staged the update, a new check resumes the
                    // installer instead of starting a session, and that is the
                    // check whose alert can be answered with Skip.
                    self.request_skip_cancel();
                }
                Notice::Choice { choice, version } => {
                    // Sparkle's own dialog is the manual-check route; mirror its
                    // skip into settings.toml so both copies agree.
                    if choice == "skip" {
                        Self::record_skip(config, &version);
                        self.skip_pending = None;
                        self.skip_attempts = 0;
                        if self.skip_undelivered.as_deref() == Some(version.as_str()) {
                            self.skip_undelivered = None;
                        }
                        self.stage = Stage::Idle;
                        self.version = None;
                        self.notes.clear();
                        self.date = None;
                        Cache::discard();
                        self.status = Some(format!("skipping monopro {version}").to_owned());
                        note.get_or_insert_with(|| self.status.clone().expect("just set"));
                    }
                }
                Notice::Failed(why) => {
                    // A failure is a state to report, never a state to die in:
                    // the running version stays whole on disk, and an update
                    // already staged stays staged — a later check failing says
                    // nothing about the update in hand.
                    if self.stage != Stage::Staged {
                        self.stage = Stage::Failed;
                    }
                    self.status = Some(why.clone());
                    note.get_or_insert_with(|| format!("update check failed — {why}"));
                }
            }
        }
        note
    }

    /// What the title strip should draw at its right end, if anything.
    pub fn badge(&self) -> Option<Badge> {
        // Checked before `version`, which a skip clears: this is the one state
        // where the app has something to say and nothing on offer.
        if let Some(version) = &self.skip_undelivered {
            return Some(Badge {
                text: format!("{version} skip unconfirmed"),
                detail: format!(
                    "Could not confirm that monopro {version} was skipped. \
                     A staged update may still install when monopro quits"
                ),
                failed: true,
            });
        }
        let version = self.version.as_deref()?;
        let text = match (self.stage, self.update_on_quit) {
            (Stage::Idle, _) => return None,
            // "On quit" is an intent, so it wins over the stage it applies to.
            (_, true) => format!("{version} on quit"),
            (Stage::Staged, false) => format!("{version} ready"),
            (Stage::Failed, _) => "update failed".to_owned(),
            (Stage::Available, false) => format!("{version} available"),
        };
        Some(Badge {
            text,
            detail: self.status.clone().unwrap_or_default(),
            failed: self.stage == Stage::Failed,
        })
    }

    /// The version the badge is pointing at, if the badge is up at all.
    fn version(&self) -> &str {
        self.version.as_deref().unwrap_or("?")
    }

    /// Whether the sheet has anything to present.
    pub fn sheet_ready(&self) -> bool {
        self.skip_undelivered.is_some() || self.version.is_some() || self.status.is_some()
    }

    pub fn skip_warning_active(&self) -> bool {
        self.skip_undelivered.is_some()
    }

    /// The sheet's version line: what is offered and what is running.
    pub fn sheet_version_line(&self) -> String {
        if let Some(version) = &self.skip_undelivered {
            return format!("monopro {version} — skip not confirmed");
        }
        match (&self.version, &self.installed) {
            (Some(next), Some(current)) => {
                format!("monopro {next} is available — you have {current}")
            }
            (Some(next), None) => format!("monopro {next} is available"),
            (None, Some(current)) => format!("monopro {current}"),
            (None, None) => "monopro".to_owned(),
        }
    }

    pub fn sheet_notes(&self) -> &str {
        &self.notes
    }

    pub fn sheet_date(&self) -> Option<&str> {
        self.date.as_deref()
    }

    /// The one-line status for the sheet and the About page.
    pub fn sheet_status(&self) -> Option<&str> {
        if self.skip_undelivered.is_some() {
            Some(SKIP_WARNING_STATUS)
        } else {
            self.status.as_deref()
        }
    }

    /// Whether "Restart now" may run: the update must be staged and the machine
    /// idle. The sheet disables the button otherwise.
    pub fn restart_now_ready(&self) -> bool {
        self.stage == Stage::Staged && !self.busy.load(Ordering::Relaxed)
    }

    /// **Update on quit.** The safest choice, and the default: the staged
    /// installer completes after a clean quit, never under a live session.
    pub fn choose_update_on_quit(&mut self) {
        self.update_on_quit = true;
        self.status = Some("monopro updates the next time you quit".to_owned());
    }

    /// **Restart now / Check for Updates.** A user-initiated check, which per
    /// Sparkle's contract goes through its standard interface: when an update
    /// is staged or found, the native dialog confirms the install and
    /// relaunches. Refused while an export writes — call sites check
    /// [`Updates::restart_now_ready`] — and a relaunch Sparkle asks for on its
    /// own while busy is postponed by the relaunch handler instead.
    ///
    /// A user-initiated check also means "offer the skipped version again",
    /// which is exactly what Sparkle does to its own skip defaults when this
    /// route runs; clearing the settings copy keeps the two in step.
    pub fn check_now(&mut self, config: &mut Settings) -> Option<String> {
        if self.updater.is_none() {
            return Some("updates are not available in this build".to_owned());
        }
        if self.busy.load(Ordering::Relaxed) {
            return Some(
                "an export is still being written — try again when it finishes".to_owned(),
            );
        }
        self.stop_skipping(config);
        match self
            .updater
            .as_ref()
            .expect("checked above")
            .check_for_updates()
        {
            Ok(()) => {
                self.status = Some("checking with the update feed…".to_owned());
                None
            }
            Err(e) => Some(format!("could not check for updates: {e}")),
        }
    }

    /// **Skip this version**, from the sheet. Records Sparkle's own skip and the
    /// settings copy in one motion, then reaches for the pending update alert
    /// Sparkle keeps while it holds a staged install: answering that with Skip
    /// is what cancels the installer before the app quits.
    pub fn skip_this_version(&mut self, config: &mut Settings) {
        let Some(version) = self.version.clone() else {
            return;
        };
        Self::record_skip(config, &version);
        self.skip_pending = Some(version.clone());
        self.skip_attempts = 0;
        self.skip_undelivered = None;
        self.stage = Stage::Idle;
        self.version = None;
        self.notes.clear();
        self.date = None;
        self.update_on_quit = false;
        Cache::discard();
        self.status = Some(format!("skipping monopro {version}"));
        self.request_skip_cancel();
    }

    /// Nudge Sparkle towards the alert a pending skip has to answer.
    ///
    /// A background check issued while a cycle is already running is a no-op;
    /// the cycle-finished notice asks again afterwards. The attempt cap keeps a
    /// skip that cannot be delivered — no alert, or an update that never
    /// staged — from polling the feed in a loop.
    fn request_skip_cancel(&mut self) {
        let Some(pending) = self.skip_pending.clone() else {
            return;
        };
        if self.skip_attempts >= SKIP_RESUME_ATTEMPTS {
            // Out of resume checks with the alert never reached. If anything
            // was staged, the app cannot establish whether it still is.
            self.skip_pending = None;
            if self.staged_seen {
                self.skip_undelivered = Some(pending);
            }
            return;
        }
        let Some(updater) = &self.updater else {
            return;
        };
        self.skip_attempts += 1;
        let _ = updater.check_for_updates_in_background();
    }

    fn record_skip_reply(&mut self, version: &str, result: sparkle_updater::Result<SkipOutcome>) {
        match result {
            Ok(SkipOutcome::Sent) => {
                self.skip_pending = None;
                self.skip_attempts = 0;
                self.skip_undelivered = None;
                self.staged_seen = false;
            }
            // A missing selector or failed call will not be fixed by polling
            // the feed again. Retain the warning even if pending was already
            // consumed by an earlier failed attempt.
            Ok(SkipOutcome::Unsupported) | Err(_) => {
                self.skip_pending = None;
                self.skip_attempts = 0;
                self.skip_undelivered = Some(version.to_owned());
            }
            // No alert yet. The cycle-finished notice asks again.
            Ok(SkipOutcome::NoAlert) => {}
        }
    }

    /// Persist the skip: Sparkle's user default first, so its scheduled checks
    /// stop offering the version, then `settings.toml` — the copy with the
    /// visible control in Settings → About.
    fn record_skip(config: &mut Settings, version: &str) {
        write_sparkle_skip(Some(version));
        config.skipped_update_version = Some(version.to_owned());
        if let Err(e) = config.save() {
            eprintln!("could not persist the skipped version: {e}");
        }
    }

    /// **Stop skipping.** Both copies cleared and any pending cancel
    /// abandoned; the next scheduled check offers whatever the feed has.
    pub fn stop_skipping(&mut self, config: &mut Settings) {
        self.skip_pending = None;
        self.skip_attempts = 0;
        self.skip_undelivered = None;
        if config.skipped_update_version.take().is_some() {
            write_sparkle_skip(None);
            if let Err(e) = config.save() {
                eprintln!("could not persist the cleared skip: {e}");
            }
        }
    }

    /// The skipped version, for the About page.
    pub fn skipped_version(config: &Settings) -> Option<&str> {
        config.skipped_update_version.as_deref()
    }

    /// The Settings toggle moved: assert it on Sparkle. Idempotent, so it may
    /// run on every settings write.
    pub fn set_auto_check(&self, enabled: bool) {
        if let Some(updater) = &self.updater {
            let _ = updater.set_automatically_checks_for_updates(enabled);
        }
    }

    /// The app began (or resumed) work that an install must not interrupt.
    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::Relaxed);
    }

    /// An export drained. If Sparkle's relaunch was postponed for it, let the
    /// install proceed now.
    pub fn export_settled(&mut self) {
        if self.busy.swap(false, Ordering::Relaxed)
            && let Some(continuation) = self.pending_relaunch.borrow_mut().take()
        {
            continuation
                .resume(MainThreadMarker::new().expect("export_settled runs on the main thread"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sparkle_updater::events::UpdateCycleInfo;

    /// A state machine with no Sparkle behind it, so `poll` is pure except for
    /// arms this test does not reach.
    fn idle_updates(skip_pending: Option<&str>, skip_attempts: u8) -> (Updates, Sender<Notice>) {
        let (tx, rx) = channel();
        (
            Updates {
                updater: None,
                notices: rx,
                stage: Stage::Idle,
                version: None,
                notes: String::new(),
                date: None,
                installed: None,
                busy: Rc::new(AtomicBool::new(false)),
                pending_relaunch: Rc::new(RefCell::new(None)),
                update_on_quit: false,
                status: None,
                skip_pending: skip_pending.map(str::to_owned),
                skip_attempts,
                skip_undelivered: None,
                staged_seen: true,
            },
            tx,
        )
    }

    #[test]
    fn cycle_finished_reaches_the_poll_as_its_own_notice() {
        let notice = notice_of(UpdateEvent::DidFinishUpdateCycle(UpdateCycleInfo {
            update_check: "background".to_owned(),
            error: None,
        }));

        assert!(matches!(notice, Some(Notice::CycleFinished)));
    }

    #[test]
    fn up_to_date_does_not_claim_a_pending_skip_was_delivered() {
        let (mut updates, tx) = idle_updates(Some("0.2.0"), 1);
        tx.send(Notice::UpToDate)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert_eq!(updates.skip_pending.as_deref(), Some("0.2.0"));
        assert_eq!(updates.skip_attempts, 1);
        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));
        assert!(updates.badge().is_some_and(|badge| badge.failed));
        assert_eq!(updates.sheet_status(), Some(SKIP_WARNING_STATUS));
    }

    #[test]
    fn stop_skipping_abandons_a_pending_skip() {
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), 1);
        updates.skip_undelivered = Some("0.2.0".to_owned());
        let mut config = Settings::default();

        updates.stop_skipping(&mut config);

        assert!(updates.skip_pending.is_none());
        assert_eq!(updates.skip_attempts, 0);
        assert!(updates.skip_undelivered.is_none());
        assert!(config.skipped_update_version.is_none());
    }

    /// The skip route runs through two selectors Sparkle does not declare in a
    /// public header. Nothing in a build fails when they move — the skip just
    /// stops arriving — so the linked framework is asked directly, here, where
    /// a Sparkle bump that breaks `skip_current_update` is a red test instead
    /// of a staged installer that runs at quit anyway. See
    /// `vendor/sparkle-updater/LOCAL-PATCH.md`.
    #[test]
    fn the_pinned_framework_still_answers_the_skip_route() {
        use objc2::runtime::AnyClass;
        use objc2::sel;

        let driver = AnyClass::get(c"SPUStandardUserDriver")
            .expect("SPUStandardUserDriver is gone from the linked Sparkle");
        assert!(
            driver.instance_method(sel!(activeUpdateAlert)).is_some(),
            "SPUStandardUserDriver no longer answers activeUpdateAlert"
        );

        let alert =
            AnyClass::get(c"SUUpdateAlert").expect("SUUpdateAlert is gone from the linked Sparkle");
        assert!(
            alert.instance_method(sel!(skipThisVersion:)).is_some(),
            "SUUpdateAlert no longer answers skipThisVersion:"
        );
    }

    #[test]
    fn a_skip_that_runs_out_of_resume_checks_warns_that_install_is_possible() {
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), SKIP_RESUME_ATTEMPTS);

        updates.request_skip_cancel();

        assert!(
            updates.skip_pending.is_none(),
            "the skip is no longer in flight"
        );
        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));
        let badge = updates
            .badge()
            .expect("an undelivered skip keeps the badge up");
        assert!(badge.failed, "an unconfirmed skip needs attention");
        assert!(badge.text.contains("0.2.0"));
        assert!(badge.detail.contains("may still install"));
    }

    #[test]
    fn repeated_unsupported_replies_keep_the_warning_until_skip_is_sent() {
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), 1);

        updates.record_skip_reply("0.2.0", Ok(SkipOutcome::Unsupported));
        assert!(updates.skip_pending.is_none());
        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));

        updates.record_skip_reply("0.2.0", Ok(SkipOutcome::Unsupported));
        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));
        assert_eq!(
            updates.sheet_version_line(),
            "monopro 0.2.0 — skip not confirmed"
        );

        updates.record_skip_reply("0.2.0", Ok(SkipOutcome::Sent));
        assert!(updates.skip_undelivered.is_none());
    }

    #[test]
    fn skipping_an_update_that_never_staged_ends_quietly_when_the_feed_is_up_to_date() {
        let (mut updates, tx) = idle_updates(Some("0.2.0"), 1);
        updates.staged_seen = false;
        tx.send(Notice::UpToDate)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert!(updates.skip_pending.is_none());
        assert!(updates.skip_undelivered.is_none());
        assert!(updates.badge().is_none());
        assert_eq!(updates.sheet_status(), Some("monopro is up to date"));
    }

    #[test]
    fn a_skip_that_never_staged_runs_out_of_resume_checks_without_a_warning() {
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), SKIP_RESUME_ATTEMPTS);
        updates.staged_seen = false;

        updates.request_skip_cancel();

        assert!(updates.skip_pending.is_none());
        assert!(updates.skip_undelivered.is_none());
        assert!(updates.badge().is_none());
    }

    #[test]
    fn an_update_rejected_after_download_reads_as_failed_not_ready() {
        // Sparkle checks the signature after the download finishes. A tampered
        // update showed "0.2.0 ready" with install buttons after being refused.
        let (mut updates, tx) = idle_updates(None, 0);
        updates.version = Some("0.2.0".to_owned());
        updates.stage = Stage::Available;
        tx.send(Notice::Downloaded)
            .expect("the receiver is in `updates`");
        tx.send(Notice::Failed("The update is improperly signed".to_owned()))
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        let badge = updates.badge().expect("a failed update keeps the badge up");
        assert!(badge.failed, "a rejected update reads as {:?}", badge.text);
        assert!(
            !updates.restart_now_ready(),
            "Restart now offered for a rejected update"
        );
    }

    #[test]
    fn a_valid_download_is_ready_once_sparkle_stages_it() {
        let (mut updates, tx) = idle_updates(None, 0);
        updates.version = Some("0.2.0".to_owned());
        updates.stage = Stage::Available;
        tx.send(Notice::Downloaded)
            .expect("the receiver is in `updates`");
        tx.send(Notice::StagedForQuit)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert_eq!(
            updates.badge().map(|b| b.text),
            Some("0.2.0 ready".to_owned())
        );
        assert!(updates.restart_now_ready());
    }

    #[test]
    fn a_download_finishing_after_the_skip_still_arms_the_warning() {
        let (mut updates, tx) = idle_updates(Some("0.2.0"), 1);
        updates.staged_seen = false;
        tx.send(Notice::Downloaded)
            .expect("the receiver is in `updates`");
        tx.send(Notice::UpToDate)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn a_feed_with_nothing_to_offer_does_not_retire_the_warning() {
        let (mut updates, tx) = idle_updates(None, 0);
        updates.skip_undelivered = Some("0.2.0".to_owned());
        tx.send(Notice::UpToDate)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert_eq!(updates.skip_undelivered.as_deref(), Some("0.2.0"));
        assert!(updates.badge().is_some_and(|badge| badge.failed));
        assert_eq!(updates.sheet_status(), Some(SKIP_WARNING_STATUS));
    }
}
