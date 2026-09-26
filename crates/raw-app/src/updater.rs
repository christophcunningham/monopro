//! The macOS auto-updater: Sparkle on the wire, monopro on the screen.
//!
//! # What Sparkle owns and what monopro owns
//!
//! Sparkle 2 does everything that can hurt: the feed fetch, the Ed25519 signature
//! check, the download, the staging, and the replace-the-running-app install. It
//! draws nothing. The binding runs it with an in-app user driver instead of
//! Sparkle's standard one, so every prompt that would have been an AppKit window —
//! "You're up to date!", the update alert, download progress, ready to install,
//! errors — arrives here as a [`Prompt`] and is shown in the update sheet, and the
//! sheet's buttons are the answers Sparkle waits for. A scheduled check announces
//! a new version as a badge at the right end of the title strip and nothing else.
//! Nothing appears while the app is up to date; silence is the resting state.
//!
//! # The three choices
//!
//! - **Update on quit** (the default): Sparkle usually has the update downloaded and
//!   staged already (`automaticallyDownloadsUpdates`), and the staged installer
//!   survives app termination and completes the swap after the process exits.
//!   When Sparkle is holding an offer, the choice is its answer: *Dismiss* for an
//!   update already installing, which still installs at quit; *Install* for one
//!   not downloaded yet, then *Dismiss* at ready-to-install, which leaves the
//!   installer waiting for the quit. Nothing is replaced under a running session.
//! - **Restart now**: *Install*, to the offer or to ready-to-install, and Sparkle
//!   relaunches into the update — after downloading it, if it has to. With no
//!   prompt waiting, a user-initiated check brings the staged update back as one.
//!   Only offered when the machine is idle; see [`Updates::restart_now`].
//! - **Skip this version**: written to Sparkle's own skip default *and* to
//!   `settings.toml`, and answered as Sparkle's *Skip* — the reply that reaches
//!   the installer driver and cancels an update already staged for install on
//!   quit. The user default alone is only consulted when filtering the feed. With
//!   no prompt waiting, a background check resumes the staged update to get one.
//!   The preference keeps a visible control in Settings → About.
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
//! non-case. Sparkle's events and prompts arrive on the main thread through one
//! channel and are drained once per frame by [`Updates::poll`], the same shape
//! `poll_export` and `Menus::pressed` use.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use objc2_foundation::{NSString, NSUserDefaults};
use sparkle_updater::{
    EventCallback, MainThreadMarker, Prompt, PromptCallback, RelaunchContinuation,
    RelaunchHandler, SparkleUpdater, UpdateChoice, UpdateEvent, UpdaterConfig, UserUpdateStage,
};

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

/// How many resume checks a pending skip or restart may spend trying to get
/// Sparkle to present the staged update. A check issued while Sparkle is
/// mid-session is a no-op; the cycle-finished event asks once more. Two bounds
/// the retries while still covering "chosen during the download" and "chosen
/// after staging" without polling the feed.
const RESUME_ATTEMPTS: u8 = 2;
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

/// The Sparkle prompt the in-app driver is holding for an answer. Sparkle's
/// session stays open until it gets one, as it would behind an open alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiting {
    Nothing,
    /// An update on offer, and how far Sparkle has already taken it.
    Offer(UserUpdateStage),
    /// Downloaded and extracted: install and relaunch now, or at quit.
    ReadyToInstall,
}

/// Bytes of a download in flight, for the sheet's percentage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Download {
    received: u64,
    /// `None` until Sparkle says, and Sparkle may be wrong.
    expected: Option<u64>,
}

/// What an event or prompt from Sparkle means for the app. The mapping happens
/// once, in the callbacks, so the UI never sees ObjC types.
enum Notice {
    /// A valid update is on the feed. Carries the plain-text notes, which are
    /// cached to disk the moment they arrive.
    Found {
        version: String,
        notes: Option<String>,
        date: Option<String>,
    },
    /// Sparkle is holding this update for an answer — what its standard
    /// driver would have drawn as the update alert.
    Offered {
        version: String,
        notes: Option<String>,
        date: Option<String>,
        stage: UserUpdateStage,
    },
    /// The feed answered and there is nothing to install.
    UpToDate,
    /// A user-initiated check is under way and may be canceled.
    Checking,
    /// Bytes are moving. `cancelable` when the in-app driver can stop them —
    /// a download the user started, not one Sparkle runs on its own.
    Downloading { cancelable: bool },
    DownloadExpected(u64),
    DownloadReceived(u64),
    /// The download finished and passed verification.
    Downloaded,
    /// The download is being unpacked; it can no longer be canceled.
    Extracting,
    /// Unpacked and waiting: install and relaunch now, or when the app quits.
    ReadyToInstall,
    /// Sparkle is about to install (and relaunch).
    Installing,
    /// A staged update will install when the app quits.
    StagedForQuit,
    /// The user stopped a download. Not a failure: the offer stands, and the
    /// next check tries again.
    Canceled,
    /// Sparkle ended the session; nothing it was holding can be answered now.
    SessionEnded,
    /// Sparkle finished an update cycle. Carries nothing: it exists so a
    /// pending skip or restart can ask once more for the staged update after
    /// the check that staged it has ended.
    CycleFinished,
    /// A download or verification attempt failed. One line.
    Failed(String),
    /// Sparkle recorded an answer to one of its prompts.
    Choice {
        choice: &'static str,
        version: String,
    },
    /// Sparkle asks whether it may check automatically. `settings.toml` has
    /// the answer; the packaged Info.plist normally keeps this from arising.
    PermissionRequested,
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
    #[cfg(not(test))]
    fn path() -> Option<PathBuf> {
        crate::platform::cache_dir(&settings::app_id()).map(|dir| dir.join("updates.toml"))
    }

    // A test offering "0.3.0" must not leave a badge for the real app to find
    // at its next launch; each test thread's storage is its own.
    #[cfg(test)]
    fn path() -> Option<PathBuf> {
        settings::dir().map(|dir| dir.join("updates.toml"))
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

/// Map a Sparkle delegate event into an app notice. Runs on the main thread,
/// inside Sparkle's delegate, between frames.
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
        UpdateEvent::WillDownloadUpdate(_) => Notice::Downloading { cancelable: false },
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
        UpdateEvent::UserDidCancelDownload => Notice::Canceled,
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

/// Map a prompt from the in-app user driver into an app notice. Same thread,
/// same channel as [`notice_of`].
fn notice_of_prompt(prompt: Prompt) -> Option<Notice> {
    Some(match prompt {
        Prompt::PermissionRequest => Notice::PermissionRequested,
        Prompt::CheckStarted => Notice::Checking,
        Prompt::UpdateFound { update, state } => Notice::Offered {
            version: update.version,
            notes: update.release_notes,
            date: update.date_string,
            stage: state.stage,
        },
        Prompt::DownloadStarted => Notice::Downloading { cancelable: true },
        Prompt::DownloadExpectedLength(bytes) => Notice::DownloadExpected(bytes),
        Prompt::DownloadReceived(bytes) => Notice::DownloadReceived(bytes),
        Prompt::ExtractionStarted => Notice::Extracting,
        Prompt::ReadyToInstall => Notice::ReadyToInstall,
        Prompt::Installing { .. } => Notice::Installing,
        Prompt::Dismissed => Notice::SessionEnded,
        // Sparkle reports the same outcomes to the delegate — `DidNotFindUpdate`
        // and `DidAbortWithError` — and those carry the reason. The driver has
        // already acknowledged them.
        Prompt::NotFound(_) | Prompt::Error(_) => return None,
        // The appcast embeds its notes, and they arrive with the offer.
        Prompt::ReleaseNotes(_) | Prompt::ReleaseNotesFailed(_) => return None,
        // Every route that checks by hand already opens the sheet, and an
        // install that finished belongs to the process that replaced this one.
        // A wildcard for the same reason as `notice_of`'s.
        Prompt::ShowInFocus | Prompt::ExtractionProgress(_) | Prompt::Installed { .. } | _ => {
            return None;
        }
    })
}

/// Write Sparkle's own skip record, so its scheduled checks stop offering the
/// version the user declined. Absent updater or failed write is harmless: the
/// settings copy still governs the badge.
fn write_sparkle_skip(version: Option<&str>) {
    // The test runner has a defaults domain of its own, and a skip written
    // there would outlive the test in ~/Library/Preferences.
    if cfg!(test) {
        return;
    }
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

/// The calls the state machine makes into Sparkle. A trait so the tests can
/// stand in for a framework that only runs inside a signed bundle.
trait Sparkle {
    /// Answer the waiting offer. `false` when Sparkle is no longer holding one.
    fn answer_update(&self, choice: UpdateChoice) -> bool;
    /// Answer the waiting ready-to-install prompt. `false` likewise.
    fn answer_ready_to_install(&self, choice: UpdateChoice) -> bool;
    fn answer_permission(&self, automatic_checks: bool);
    /// A user-initiated check: resumes a staged update as an offer, or asks
    /// the feed. Sparkle shows nothing; the prompts come back here.
    fn check(&self) -> Result<(), String>;
    /// A scheduled-style check, which honors Sparkle's own skip record.
    fn check_in_background(&self);
    /// Stop a user-initiated check, or a download before it is unpacked.
    fn cancel(&self) -> bool;
    fn set_auto_check(&self, enabled: bool);
}

impl Sparkle for SparkleUpdater {
    fn answer_update(&self, choice: UpdateChoice) -> bool {
        SparkleUpdater::answer_update(self, choice).unwrap_or(false)
    }

    fn answer_ready_to_install(&self, choice: UpdateChoice) -> bool {
        SparkleUpdater::answer_ready_to_install(self, choice).unwrap_or(false)
    }

    fn answer_permission(&self, automatic_checks: bool) {
        let _ = SparkleUpdater::answer_permission(self, automatic_checks);
    }

    fn check(&self) -> Result<(), String> {
        self.check_for_updates().map_err(|e| e.to_string())
    }

    fn check_in_background(&self) {
        let _ = self.check_for_updates_in_background();
    }

    fn cancel(&self) -> bool {
        SparkleUpdater::cancel(self).unwrap_or(false)
    }

    fn set_auto_check(&self, enabled: bool) {
        let _ = self.set_automatically_checks_for_updates(enabled);
    }
}

/// The macOS updater. One per app, created on the first frame.
pub struct Updates {
    /// `None` outside an application bundle — see the module note. Every method
    /// tolerates it, so call sites never branch on it.
    updater: Option<Box<dyn Sparkle>>,
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
    /// The user chose "restart now" and Sparkle has not relaunched yet: the
    /// next offer or ready-to-install prompt is answered *Install*.
    restart_requested: bool,
    /// The prompt Sparkle is waiting on, if any.
    waiting: Waiting,
    /// A user-initiated check or download is running and can be stopped.
    cancelable: bool,
    /// The download in flight, while Sparkle reports one to the driver.
    download: Option<Download>,
    /// One-line status, shown in the sheet and the About page.
    status: Option<String>,
    /// A skipped version Sparkle may still be holding — a staged install, or
    /// an offer whose *Skip* answer is the only route to the installer driver.
    /// Cleared once that answer has been delivered or the user explicitly
    /// stops skipping.
    skip_pending: Option<String>,
    /// Resume checks already spent on the current [`Self::skip_pending`] or
    /// [`Self::restart_requested`].
    resume_attempts: u8,
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
        let mut installed = Self::detached(rx);

        let updater = MainThreadMarker::new().and_then(|mtm| {
            SparkleUpdater::new(
                mtm,
                UpdaterConfig {
                    event_callback: Some(Self::callback(tx.clone(), ctx.clone())),
                    relaunch_handler: Some(Self::relaunch(
                        installed.busy.clone(),
                        installed.pending_relaunch.clone(),
                    )),
                    // Only Sparkle's standard driver consults gentle reminders;
                    // the in-app driver below hands over every prompt instead.
                    gentle_reminders: None,
                    prompt_callback: Some(Self::prompts(tx, ctx.clone())),
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
        installed.updater = Some(Box::new(updater));

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

    /// The resting state, with no Sparkle behind it yet.
    fn detached(notices: Receiver<Notice>) -> Self {
        Self {
            updater: None,
            notices,
            stage: Stage::Idle,
            version: None,
            notes: String::new(),
            date: None,
            installed: None,
            busy: Rc::new(AtomicBool::new(false)),
            pending_relaunch: Rc::new(RefCell::new(None)),
            update_on_quit: false,
            restart_requested: false,
            waiting: Waiting::Nothing,
            cancelable: false,
            download: None,
            status: None,
            skip_pending: None,
            resume_attempts: 0,
            skip_undelivered: None,
            staged_seen: false,
        }
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

    /// The prompt callback: the in-app driver's half of the same channel. The
    /// replies stay with the driver until `poll` or the sheet answers them.
    fn prompts(tx: Sender<Notice>, ctx: egui::Context) -> PromptCallback {
        Rc::new(move |prompt| {
            if let Some(notice) = notice_of_prompt(prompt) {
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

    /// Drain Sparkle's events and prompts into visible state, answering any
    /// prompt a choice the user already made applies to. Called once per
    /// frame; returns a one-line note for the footer when something deserves
    /// one.
    pub fn poll(&mut self, config: &mut Settings) -> Option<String> {
        let mut note = None;
        while let Ok(notice) = self.notices.try_recv() {
            match notice {
                Notice::Found {
                    version,
                    notes,
                    date,
                } => {
                    self.found(config, version, notes, date, &mut note);
                }
                Notice::Offered {
                    version,
                    notes,
                    date,
                    stage,
                } => {
                    // Sparkle's own found-update event usually came first; the
                    // bookkeeping is idempotent, and a resumed staged install
                    // may arrive here without one.
                    self.waiting = Waiting::Offer(stage);
                    self.cancelable = false;
                    self.download = None;
                    let skipped = self.found(config, version, notes, date, &mut note);
                    if matches!(
                        stage,
                        UserUpdateStage::Downloaded | UserUpdateStage::Installing
                    ) {
                        self.staged_seen = true;
                    }
                    if !skipped {
                        // The offer replaces "checking…": the sheet shows the
                        // version and its notes, and the choices speak for it.
                        self.status = (stage == UserUpdateStage::Installing).then(|| {
                            "update staged — it installs when monopro quits".to_owned()
                        });
                        if stage == UserUpdateStage::Installing {
                            self.stage = Stage::Staged;
                        }
                    }
                    self.answer_offer_if_chosen();
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
                            self.resume_attempts = 0;
                        }
                    }
                    if self.skip_undelivered.is_none() {
                        note.get_or_insert_with(|| "monopro is up to date".to_owned());
                        self.status = Some("monopro is up to date".to_owned());
                    }
                }
                Notice::Checking => {
                    self.cancelable = true;
                    self.status = Some("checking with the update feed…".to_owned());
                }
                Notice::Downloading { cancelable } => {
                    self.cancelable |= cancelable;
                    if self.download.is_none() {
                        self.download = Some(Download::default());
                    }
                    if self.stage != Stage::Staged {
                        self.stage = Stage::Available;
                        self.status = Some(self.download_status());
                    }
                }
                Notice::DownloadExpected(bytes) => {
                    if bytes > 0 {
                        self.download.get_or_insert_with(Download::default).expected = Some(bytes);
                    }
                }
                Notice::DownloadReceived(bytes) => {
                    let download = self.download.get_or_insert_with(Download::default);
                    download.received = download.received.saturating_add(bytes);
                    if self.stage != Stage::Staged {
                        self.status = Some(self.download_status());
                    }
                }
                Notice::Downloaded => {
                    // Not staged yet: Sparkle validates the signature after the
                    // download, and a rejected update must not read as ready.
                    // `StagedForQuit` or `ReadyToInstall` follows a valid one.
                    self.staged_seen = true;
                    self.download = None;
                    if self.stage != Stage::Staged {
                        self.status = Some("verifying the update…".to_owned());
                    }
                }
                Notice::Extracting => {
                    self.cancelable = false;
                    self.download = None;
                    if self.stage != Stage::Staged {
                        self.status = Some("preparing the update…".to_owned());
                    }
                }
                Notice::ReadyToInstall => {
                    self.waiting = Waiting::ReadyToInstall;
                    self.cancelable = false;
                    self.download = None;
                    self.staged_seen = true;
                    self.stage = Stage::Staged;
                    self.status = Some(if self.update_on_quit {
                        "update staged — it installs when monopro quits".to_owned()
                    } else {
                        "update ready — restart now, or it installs when monopro quits".to_owned()
                    });
                    self.answer_ready_if_chosen();
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
                Notice::Canceled => self.canceled(),
                Notice::SessionEnded => {
                    // Whatever Sparkle was holding is void now; a restart it
                    // never reached is not carried into a later session.
                    self.waiting = Waiting::Nothing;
                    self.cancelable = false;
                    self.download = None;
                    self.restart_requested = false;
                }
                Notice::CycleFinished => {
                    // A check that was already running when the user chose has
                    // now ended; if it staged the update, a new check resumes
                    // the installer instead of starting a session, and that is
                    // the check whose offer can take the answer.
                    self.request_resume();
                }
                Notice::Choice { choice, version } => {
                    // Sparkle recorded a skip — ours, delivered as its answer.
                    // Mirror it into settings.toml so both copies agree.
                    if choice == "skip" {
                        Self::record_skip(config, &version);
                        self.skip_pending = None;
                        self.resume_attempts = 0;
                        if self.skip_undelivered.as_deref() == Some(version.as_str()) {
                            self.skip_undelivered = None;
                        }
                        self.clear_offer();
                        self.status = Some(format!("skipping monopro {version}"));
                        note.get_or_insert_with(|| self.status.clone().expect("just set"));
                    }
                }
                Notice::Failed(why) => {
                    // A failure is a state to report, never a state to die in:
                    // the running version stays whole on disk, and an update
                    // already staged stays staged — a later check failing says
                    // nothing about the update in hand.
                    self.cancelable = false;
                    self.download = None;
                    if self.stage != Stage::Staged {
                        self.stage = Stage::Failed;
                    }
                    self.status = Some(why.clone());
                    note.get_or_insert_with(|| format!("update check failed — {why}"));
                }
                Notice::PermissionRequested => {
                    if let Some(updater) = &self.updater {
                        updater.answer_permission(config.check_for_updates);
                    }
                }
            }
        }
        note
    }

    /// The found-update bookkeeping, shared by Sparkle's event and its offer.
    /// Returns `true` for the version the user declined, which is recorded as
    /// a skip still to be delivered instead of being shown.
    fn found(
        &mut self,
        config: &Settings,
        version: String,
        notes: Option<String>,
        date: Option<String>,
        note: &mut Option<String>,
    ) -> bool {
        // The feed is offering the version the user declined. This is the
        // resumed staged install talking: the appcast filter never sees it,
        // so the skip has to be delivered as the answer to Sparkle's offer,
        // which is what cancels the installer. Keep any warning until then.
        if Self::skipped_version(config) == Some(version.as_str()) {
            if self.skip_pending.is_none() {
                self.skip_pending = Some(version.clone());
                self.resume_attempts = 0;
            }
            self.clear_offer();
            self.status = Some(format!("skipping monopro {version}"));
            return true;
        }
        // The feed has moved past the skipped version, so the old cancel has
        // nothing left to reach. The new offer is a fresh decision, but it
        // does not prove an older staged installer was canceled.
        self.skip_pending = None;
        self.resume_attempts = 0;
        let is_new = self.version.as_deref() != Some(version.as_str());
        if is_new {
            self.staged_seen = false;
            self.update_on_quit = false;
            self.restart_requested = false;
        }
        if self.stage != Stage::Staged {
            self.stage = Stage::Available;
        }
        self.date = date;
        if is_new || matches!(self.stage, Stage::Failed | Stage::Idle) {
            self.status = None;
        }
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
        false
    }

    /// Forget the update on offer: no badge, no notes, no cached copy.
    fn clear_offer(&mut self) {
        self.stage = Stage::Idle;
        self.version = None;
        self.notes.clear();
        self.date = None;
        self.update_on_quit = false;
        self.restart_requested = false;
        self.download = None;
        Cache::discard();
    }

    fn download_status(&self) -> String {
        match self.download {
            Some(Download {
                received,
                expected: Some(expected),
            }) => format!(
                "downloading the update… {}%",
                (received.saturating_mul(100) / expected).min(100)
            ),
            _ => "downloading the update…".to_owned(),
        }
    }

    /// Answer a waiting offer with the choice the user already made, if any.
    /// With none, the offer waits for the sheet.
    fn answer_offer_if_chosen(&mut self) {
        let Waiting::Offer(stage) = self.waiting else {
            return;
        };
        let choice = if self.skip_pending.is_some() {
            UpdateChoice::Skip
        } else if self.restart_requested && !self.busy.load(Ordering::Relaxed) {
            UpdateChoice::Install
        } else if self.update_on_quit {
            // Already installing: it installs at quit whatever the answer, and
            // *Dismiss* keeps it that way. Otherwise it has to be fetched
            // first, and ready-to-install is answered *Dismiss* in turn.
            if stage == UserUpdateStage::Installing {
                UpdateChoice::Dismiss
            } else {
                UpdateChoice::Install
            }
        } else {
            return;
        };
        self.answer_offer(choice);
    }

    /// Answer a waiting ready-to-install prompt with the choice already made.
    fn answer_ready_if_chosen(&mut self) {
        if self.waiting != Waiting::ReadyToInstall {
            return;
        }
        let choice = if self.skip_pending.is_some() {
            // Cancels this installation. The version itself is already in
            // Sparkle's skip record, written by `record_skip`.
            UpdateChoice::Skip
        } else if self.restart_requested && !self.busy.load(Ordering::Relaxed) {
            UpdateChoice::Install
        } else if self.update_on_quit {
            UpdateChoice::Dismiss
        } else {
            return;
        };
        self.answer_ready(choice);
    }

    fn answer_offer(&mut self, choice: UpdateChoice) {
        self.waiting = Waiting::Nothing;
        let delivered = self
            .updater
            .as_ref()
            .is_some_and(|updater| updater.answer_update(choice));
        if choice == UpdateChoice::Skip {
            self.skip_answered(delivered);
        }
    }

    fn answer_ready(&mut self, choice: UpdateChoice) {
        self.waiting = Waiting::Nothing;
        let delivered = self
            .updater
            .as_ref()
            .is_some_and(|updater| updater.answer_ready_to_install(choice));
        if choice == UpdateChoice::Skip {
            self.skip_answered(delivered);
        }
    }

    /// A *Skip* answer reached Sparkle, which cancels the installer — or it
    /// did not, because Sparkle ended the session first, and the cycle-finished
    /// notice asks for another offer to answer.
    fn skip_answered(&mut self, delivered: bool) {
        if delivered {
            self.skip_pending = None;
            self.resume_attempts = 0;
            self.skip_undelivered = None;
            self.staged_seen = false;
        }
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

    /// Whether an update is on offer, so the sheet shows the three choices.
    /// A check that found nothing, or is still running, has only a status.
    pub fn has_offer(&self) -> bool {
        self.version.is_some() && self.stage != Stage::Idle
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

    /// Whether "Restart now" may run: the machine is idle, and the update is
    /// staged or Sparkle is holding it for an answer — one not yet downloaded
    /// downloads first. The sheet disables the button otherwise.
    pub fn restart_now_ready(&self) -> bool {
        (self.stage == Stage::Staged || self.waiting != Waiting::Nothing)
            && !self.busy.load(Ordering::Relaxed)
    }

    /// Whether the sheet may offer "Cancel": a user-initiated check, or a
    /// download that has not been unpacked yet.
    pub fn cancelable(&self) -> bool {
        self.cancelable
    }

    /// **Update on quit.** The safest choice, and the default: the staged
    /// installer completes after a clean quit, never under a live session.
    pub fn choose_update_on_quit(&mut self) {
        self.update_on_quit = true;
        self.restart_requested = false;
        self.status = Some("monopro updates the next time you quit".to_owned());
        match self.waiting {
            Waiting::ReadyToInstall => self.answer_ready_if_chosen(),
            Waiting::Offer(stage) => {
                if stage != UserUpdateStage::Installing {
                    self.status = Some("downloading the update…".to_owned());
                }
                self.answer_offer_if_chosen();
            }
            // Staged, or being staged by Sparkle's automatic download: the
            // installer runs at quit without anything more from us.
            Waiting::Nothing => {}
        }
    }

    /// **Restart now.** *Install*, to whichever prompt Sparkle is holding;
    /// with none, a user-initiated check resumes the staged update as an offer
    /// and `poll` answers it. Refused while an export writes — the sheet also
    /// checks [`Updates::restart_now_ready`] — and a relaunch Sparkle asks for
    /// on its own while busy is postponed by the relaunch handler instead.
    pub fn restart_now(&mut self) -> Option<String> {
        if self.updater.is_none() {
            return Some("updates are not available in this build".to_owned());
        }
        if self.busy.load(Ordering::Relaxed) {
            return Some(
                "an export is still being written — try again when it finishes".to_owned(),
            );
        }
        self.restart_requested = true;
        self.update_on_quit = false;
        self.resume_attempts = 0;
        self.status = Some("installing the update…".to_owned());
        match self.waiting {
            Waiting::ReadyToInstall => self.answer_ready_if_chosen(),
            Waiting::Offer(stage) => {
                if stage != UserUpdateStage::Installing {
                    self.status = Some("downloading the update…".to_owned());
                }
                self.answer_offer_if_chosen();
            }
            Waiting::Nothing => {
                self.resume_attempts = 1;
                if let Err(e) = self.updater.as_ref().expect("checked above").check() {
                    self.restart_requested = false;
                    return Some(format!("could not restart into the update: {e}"));
                }
            }
        }
        None
    }

    /// **Check for Updates.** A user-initiated check, which Sparkle presents
    /// through the in-app driver: its progress, its result and any offer all
    /// land in the sheet. Refused while an export writes.
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
        match self.updater.as_ref().expect("checked above").check() {
            Ok(()) => {
                // Sparkle is already holding a prompt: the check brings that
                // forward instead of asking the feed, and the sheet shows it.
                if self.waiting == Waiting::Nothing {
                    self.status = Some("checking with the update feed…".to_owned());
                }
                None
            }
            Err(e) => Some(format!("could not check for updates: {e}")),
        }
    }

    /// **Cancel**, from the sheet: stop the running check or download. The
    /// app stays on the current version; the next scheduled check tries again.
    pub fn cancel(&mut self) {
        let canceled = self
            .updater
            .as_ref()
            .is_some_and(|updater| updater.cancel());
        self.cancelable = false;
        if canceled {
            self.canceled();
        }
    }

    /// Back to the offer as it stood before the check or download began —
    /// a choice the user withdrew, which the badge must not call a failure.
    fn canceled(&mut self) {
        self.cancelable = false;
        self.restart_requested = false;
        self.download = None;
        if self.stage != Stage::Staged {
            self.stage = if self.version.is_some() {
                Stage::Available
            } else {
                Stage::Idle
            };
        }
        self.status = Some("canceled".to_owned());
    }

    /// **Skip this version**, from the sheet. Records Sparkle's own skip and the
    /// settings copy in one motion, then answers Sparkle's offer with *Skip* —
    /// the answer that cancels a staged installer before the app quits. With no
    /// offer waiting, a background check resumes the staged update to get one.
    pub fn skip_this_version(&mut self, config: &mut Settings) {
        let Some(version) = self.version.clone() else {
            return;
        };
        Self::record_skip(config, &version);
        self.skip_pending = Some(version.clone());
        self.resume_attempts = 0;
        self.skip_undelivered = None;
        self.clear_offer();
        self.status = Some(format!("skipping monopro {version}"));
        match self.waiting {
            Waiting::Offer(_) => self.answer_offer_if_chosen(),
            Waiting::ReadyToInstall => self.answer_ready_if_chosen(),
            Waiting::Nothing => self.request_resume(),
        }
    }

    /// Nudge Sparkle towards the offer a pending skip or restart has to answer.
    ///
    /// A check issued while a cycle is already running is a no-op; the
    /// cycle-finished notice asks again afterwards. The attempt cap keeps a
    /// choice that cannot be delivered — no offer, or an update that never
    /// staged — from polling the feed in a loop.
    fn request_resume(&mut self) {
        if self.waiting != Waiting::Nothing {
            return;
        }
        if let Some(pending) = self.skip_pending.clone() {
            if self.resume_attempts >= RESUME_ATTEMPTS {
                // Out of resume checks with no offer reached. If anything was
                // staged, the app cannot establish whether it still is.
                self.skip_pending = None;
                if self.staged_seen {
                    self.skip_undelivered = Some(pending);
                }
                return;
            }
            let Some(updater) = &self.updater else {
                return;
            };
            self.resume_attempts += 1;
            // In the background, so Sparkle's own skip record still filters
            // the feed; a staged update resumes regardless.
            updater.check_in_background();
        } else if self.restart_requested {
            if self.resume_attempts >= RESUME_ATTEMPTS {
                self.restart_requested = false;
                if self.stage == Stage::Staged {
                    self.status = Some(
                        "could not restart into the update — it installs when monopro quits"
                            .to_owned(),
                    );
                }
                return;
            }
            let Some(updater) = &self.updater else {
                return;
            };
            self.resume_attempts += 1;
            let _ = updater.check();
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
        self.resume_attempts = 0;
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
            updater.set_auto_check(enabled);
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
    use std::cell::Cell;

    /// Every call the state machine made into Sparkle, in order.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Call {
        AnswerUpdate(UpdateChoice),
        AnswerReady(UpdateChoice),
        Permission(bool),
        Check,
        Background,
        Cancel,
    }

    /// Stands in for Sparkle. `delivers` is whether a prompt is still there to
    /// answer — `false` is Sparkle having ended the session first.
    struct Recorder {
        calls: RefCell<Vec<Call>>,
        delivers: Cell<bool>,
    }

    impl Sparkle for Rc<Recorder> {
        fn answer_update(&self, choice: UpdateChoice) -> bool {
            self.calls.borrow_mut().push(Call::AnswerUpdate(choice));
            self.delivers.get()
        }
        fn answer_ready_to_install(&self, choice: UpdateChoice) -> bool {
            self.calls.borrow_mut().push(Call::AnswerReady(choice));
            self.delivers.get()
        }
        fn answer_permission(&self, automatic_checks: bool) {
            self.calls
                .borrow_mut()
                .push(Call::Permission(automatic_checks));
        }
        fn check(&self) -> Result<(), String> {
            self.calls.borrow_mut().push(Call::Check);
            Ok(())
        }
        fn check_in_background(&self) {
            self.calls.borrow_mut().push(Call::Background);
        }
        fn cancel(&self) -> bool {
            self.calls.borrow_mut().push(Call::Cancel);
            true
        }
        fn set_auto_check(&self, _enabled: bool) {}
    }

    /// A state machine with no Sparkle behind it, so `poll` is pure except for
    /// arms this test does not reach.
    fn idle_updates(skip_pending: Option<&str>, resume_attempts: u8) -> (Updates, Sender<Notice>) {
        let (tx, rx) = channel();
        let mut updates = Updates::detached(rx);
        updates.skip_pending = skip_pending.map(str::to_owned);
        updates.resume_attempts = resume_attempts;
        updates.staged_seen = true;
        (updates, tx)
    }

    /// The same, with a recorder standing in for Sparkle.
    fn recorded_updates() -> (Updates, Sender<Notice>, Rc<Recorder>) {
        let (mut updates, tx) = idle_updates(None, 0);
        updates.staged_seen = false;
        let recorder = Rc::new(Recorder {
            calls: RefCell::new(Vec::new()),
            delivers: Cell::new(true),
        });
        updates.updater = Some(Box::new(recorder.clone()));
        (updates, tx, recorder)
    }

    fn offer(version: &str, stage: UserUpdateStage) -> Notice {
        Notice::Offered {
            version: version.to_owned(),
            notes: Some("Faster previews.".to_owned()),
            date: None,
            stage,
        }
    }

    fn calls(recorder: &Recorder) -> Vec<Call> {
        recorder.calls.borrow().clone()
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
        assert_eq!(updates.resume_attempts, 1);
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
        assert_eq!(updates.resume_attempts, 0);
        assert!(updates.skip_undelivered.is_none());
        assert!(config.skipped_update_version.is_none());
    }

    /// Sparkle draws nothing only while every method it requires of a user
    /// driver is answered by the in-app one. A Sparkle bump that adds one
    /// builds cleanly and then crashes on the first prompt that uses it, so
    /// the linked framework is asked directly, here, where the bump is a red
    /// test instead. See `vendor/sparkle-updater/LOCAL-PATCH.md`.
    #[test]
    fn the_linked_sparkle_asks_nothing_the_in_app_driver_cannot_answer() {
        let missing = sparkle_updater::missing_user_driver_methods();
        assert!(missing.is_empty(), "the in-app user driver lacks {missing:?}");
    }

    #[test]
    fn a_skip_that_runs_out_of_resume_checks_warns_that_install_is_possible() {
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), RESUME_ATTEMPTS);

        updates.request_resume();

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
        let (mut updates, _tx) = idle_updates(Some("0.2.0"), RESUME_ATTEMPTS);
        updates.staged_seen = false;

        updates.request_resume();

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

    #[test]
    fn a_manual_check_that_finds_nothing_offers_no_install_choices() {
        // The sheet read "monopro is up to date" above Update on quit, Restart
        // now and Skip this version, with nothing for any of them to act on.
        let (mut updates, tx, _recorder) = recorded_updates();
        updates.check_now(&mut Settings::default());
        tx.send(Notice::Checking)
            .expect("the receiver is in `updates`");
        tx.send(Notice::UpToDate)
            .expect("the receiver is in `updates`");
        tx.send(Notice::SessionEnded)
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert!(updates.sheet_ready(), "the result has somewhere to land");
        assert!(!updates.has_offer());
        assert!(!updates.cancelable());
        assert_eq!(updates.sheet_status(), Some("monopro is up to date"));
    }

    #[test]
    fn an_offer_waits_for_the_sheet_and_update_on_quit_downloads_then_defers() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::NotDownloaded))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        assert!(calls(&recorder).is_empty(), "nothing answered for the user");
        assert!(updates.has_offer());
        assert!(updates.restart_now_ready(), "an offer can download and install");

        updates.choose_update_on_quit();
        assert_eq!(calls(&recorder), [Call::AnswerUpdate(UpdateChoice::Install)]);

        tx.send(Notice::Downloading { cancelable: true })
            .expect("the receiver is in `updates`");
        tx.send(Notice::ReadyToInstall)
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        assert_eq!(
            calls(&recorder),
            [
                Call::AnswerUpdate(UpdateChoice::Install),
                Call::AnswerReady(UpdateChoice::Dismiss),
            ],
            "ready to install is answered Dismiss, leaving it for the quit"
        );
        assert_eq!(
            updates.badge().map(|b| b.text),
            Some("0.3.0 on quit".to_owned())
        );
    }

    #[test]
    fn update_on_quit_dismisses_an_update_that_is_already_installing() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        updates.choose_update_on_quit();

        assert_eq!(calls(&recorder), [Call::AnswerUpdate(UpdateChoice::Dismiss)]);
    }

    #[test]
    fn restart_now_resumes_a_staged_update_and_installs_it() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(Notice::Found {
            version: "0.3.0".to_owned(),
            notes: None,
            date: None,
        })
        .expect("the receiver is in `updates`");
        tx.send(Notice::StagedForQuit)
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());
        assert!(updates.restart_now_ready());

        assert_eq!(updates.restart_now(), None);
        assert_eq!(calls(&recorder), [Call::Check], "no prompt, so ask for one");

        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        assert_eq!(
            calls(&recorder),
            [Call::Check, Call::AnswerUpdate(UpdateChoice::Install)]
        );
    }

    #[test]
    fn restart_now_for_an_offer_not_downloaded_installs_once_it_is_ready() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::NotDownloaded))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        updates.restart_now();
        tx.send(Notice::ReadyToInstall)
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        assert_eq!(
            calls(&recorder),
            [
                Call::AnswerUpdate(UpdateChoice::Install),
                Call::AnswerReady(UpdateChoice::Install),
            ]
        );
    }

    #[test]
    fn restart_now_waits_while_an_export_is_writing() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());
        updates.set_busy(true);

        assert!(!updates.restart_now_ready());
        assert!(updates.restart_now().is_some());
        assert!(calls(&recorder).is_empty());
    }

    #[test]
    fn skip_is_delivered_as_sparkles_answer_to_the_waiting_offer() {
        let (mut updates, tx, recorder) = recorded_updates();
        let mut config = Settings::default();
        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut config);

        updates.skip_this_version(&mut config);

        assert_eq!(calls(&recorder), [Call::AnswerUpdate(UpdateChoice::Skip)]);
        assert!(updates.skip_pending.is_none(), "the skip was delivered");
        assert!(!updates.skip_warning_active());
        assert!(updates.badge().is_none());
        assert_eq!(config.skipped_update_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn a_skip_with_no_offer_waiting_resumes_the_staged_update_to_answer_it() {
        let (mut updates, tx, recorder) = recorded_updates();
        let mut config = Settings::default();
        tx.send(Notice::Found {
            version: "0.3.0".to_owned(),
            notes: None,
            date: None,
        })
        .expect("the receiver is in `updates`");
        tx.send(Notice::StagedForQuit)
            .expect("the receiver is in `updates`");
        updates.poll(&mut config);

        updates.skip_this_version(&mut config);
        assert_eq!(calls(&recorder), [Call::Background]);

        // Sparkle resumes the staged install as a scheduled offer.
        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut config);

        assert_eq!(
            calls(&recorder),
            [Call::Background, Call::AnswerUpdate(UpdateChoice::Skip)]
        );
        assert!(updates.badge().is_none(), "the skipped version is not shown");
        assert!(!updates.skip_warning_active());
    }

    #[test]
    fn a_skip_sparkle_could_no_longer_receive_is_retried_after_the_cycle() {
        let (mut updates, tx, recorder) = recorded_updates();
        let mut config = Settings::default();
        tx.send(offer("0.3.0", UserUpdateStage::Installing))
            .expect("the receiver is in `updates`");
        updates.poll(&mut config);
        recorder.delivers.set(false);

        updates.skip_this_version(&mut config);
        assert_eq!(updates.skip_pending.as_deref(), Some("0.3.0"));

        tx.send(Notice::SessionEnded)
            .expect("the receiver is in `updates`");
        tx.send(Notice::CycleFinished)
            .expect("the receiver is in `updates`");
        updates.poll(&mut config);

        assert_eq!(
            calls(&recorder),
            [Call::AnswerUpdate(UpdateChoice::Skip), Call::Background]
        );
    }

    #[test]
    fn an_offer_sparkle_withdrew_is_not_answered() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::NotDownloaded))
            .expect("the receiver is in `updates`");
        tx.send(Notice::SessionEnded)
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        updates.choose_update_on_quit();

        assert!(calls(&recorder).is_empty());
    }

    #[test]
    fn a_download_reports_its_progress_and_can_be_canceled() {
        let (mut updates, tx, recorder) = recorded_updates();
        tx.send(offer("0.3.0", UserUpdateStage::NotDownloaded))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());
        updates.choose_update_on_quit();

        tx.send(Notice::Downloading { cancelable: true })
            .expect("the receiver is in `updates`");
        tx.send(Notice::DownloadExpected(200))
            .expect("the receiver is in `updates`");
        tx.send(Notice::DownloadReceived(50))
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());

        assert_eq!(updates.sheet_status(), Some("downloading the update… 25%"));
        assert!(updates.cancelable());

        updates.cancel();

        assert_eq!(calls(&recorder).last(), Some(&Call::Cancel));
        assert!(!updates.cancelable());
        assert_eq!(updates.sheet_status(), Some("canceled"));

        // Sparkle confirms through its delegate; that is not a failed update.
        tx.send(Notice::Canceled)
            .expect("the receiver is in `updates`");
        updates.poll(&mut Settings::default());
        let badge = updates.badge().expect("the offer stands");
        assert!(!badge.failed, "a canceled download reads as {:?}", badge.text);
        assert!(updates.has_offer());
    }

    #[test]
    fn an_offer_replaces_the_checking_status() {
        let (mut updates, tx, _recorder) = recorded_updates();
        updates.version = Some("0.3.0".to_owned());
        updates.stage = Stage::Available;
        tx.send(Notice::Checking)
            .expect("the receiver is in `updates`");
        tx.send(offer("0.3.0", UserUpdateStage::NotDownloaded))
            .expect("the receiver is in `updates`");

        updates.poll(&mut Settings::default());

        assert_eq!(updates.sheet_status(), None);
        assert!(updates.has_offer());
    }

    #[test]
    fn sparkles_permission_question_is_answered_from_settings() {
        let (mut updates, tx, recorder) = recorded_updates();
        let mut config = Settings {
            check_for_updates: false,
            ..Settings::default()
        };
        tx.send(Notice::PermissionRequested)
            .expect("the receiver is in `updates`");

        updates.poll(&mut config);

        assert_eq!(calls(&recorder), [Call::Permission(false)]);
    }
}
