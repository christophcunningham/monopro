//! An in-app user driver: Sparkle's prompts handed to the host instead of drawn.
//!
//! Sparkle splits an update session between the updater, which fetches,
//! verifies, downloads and installs, and a *user driver*, which is everything
//! the user sees. `SPUStandardUserDriver` answers with AppKit windows: "You're
//! up to date!", the update alert, the download progress panel, the
//! ready-to-install prompt, error alerts. This driver answers with none of
//! them. Every call becomes a [`Prompt`] for the host, and the reply blocks
//! that some prompts carry are kept here until the host answers through
//! [`SparkleUpdater`](crate::SparkleUpdater).
//!
//! Acknowledgements (not found, error, installed) are given at once, after
//! the prompt is delivered: they only tell Sparkle the message was seen, and
//! holding them would keep the session open behind a window that does not
//! exist. Sparkle's own command-line driver answers them the same way.

use std::cell::RefCell;
use std::ffi::c_uint;
use std::rc::Rc;

use block2::{Block, RcBlock};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, NSObject, NSObjectProtocol};
use objc2::{
    define_class, extern_protocol, msg_send, ClassType, DefinedClass, MainThreadMarker,
    MainThreadOnly,
};

use crate::bindings::{SPUAppcastItem, SPUUserUpdateState};
use crate::delegate::{error_payload, update_info_from_item};
use crate::events::{ErrorPayload, UpdateInfo, UserUpdateStage, UserUpdateState};

/// Receives every prompt Sparkle would otherwise have drawn. Runs on the main
/// thread, synchronously inside Sparkle's call; keep it brief.
pub type PromptCallback = Rc<dyn Fn(Prompt)>;

/// What Sparkle wants the user to see, in place of a window.
///
/// Three prompts wait for an answer: [`Prompt::PermissionRequest`],
/// [`Prompt::UpdateFound`] and [`Prompt::ReadyToInstall`]. Until it arrives
/// Sparkle's session stays open — exactly as it would behind an unanswered
/// alert — and a later user-initiated check brings it back as
/// [`Prompt::ShowInFocus`] instead of starting another.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Prompt {
    /// Sparkle asks whether it may check automatically. Answer with
    /// `answer_permission`. Never asked when `SUEnableAutomaticChecks` is in
    /// Info.plist or the host has set the preference itself.
    PermissionRequest,
    /// A user-initiated check has started; `cancel` stops it.
    CheckStarted,
    /// An update is on offer. Answer with `answer_update`.
    UpdateFound {
        update: UpdateInfo,
        state: UserUpdateState,
    },
    /// Release notes Sparkle downloaded from the item's `releaseNotesURL`,
    /// decoded as UTF-8. Embedded notes arrive with [`Prompt::UpdateFound`].
    ReleaseNotes(String),
    ReleaseNotesFailed(ErrorPayload),
    /// A user-initiated check found nothing to offer. Already acknowledged.
    NotFound(ErrorPayload),
    /// The session failed. Already acknowledged.
    Error(ErrorPayload),
    /// The download began; `cancel` stops it until extraction starts.
    DownloadStarted,
    /// The download's expected size in bytes. May arrive more than once and
    /// may be wrong.
    DownloadExpectedLength(u64),
    /// More bytes arrived: the length of this chunk, not a running total.
    DownloadReceived(u64),
    ExtractionStarted,
    /// Extraction progress, 0.0 to 1.0.
    ExtractionProgress(f64),
    /// The update is extracted and waiting. Answer with
    /// `answer_ready_to_install`: install and relaunch now, or dismiss and let
    /// it install when the app quits.
    ReadyToInstall,
    /// Sparkle is installing, and has asked the app to quit unless it already
    /// has.
    Installing { application_terminated: bool },
    /// The install finished. Rarely seen: the updater usually dies with the
    /// app it replaced. Already acknowledged.
    Installed { relaunched: bool },
    /// Sparkle ended the session. Every unanswered prompt is void.
    Dismissed,
    /// The user checked for updates while a prompt is still waiting; bring it
    /// forward rather than expect a new one.
    ShowInFocus,
}

/// The user's answer to [`Prompt::UpdateFound`] or [`Prompt::ReadyToInstall`].
/// Mirrors `SPUUserUpdateChoice`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateChoice {
    /// For a found update: never offer this version again unless the user
    /// checks by hand, and cancel it if it was already installing. For a
    /// ready update: cancel this installation without skipping the version.
    Skip,
    /// Download if needed, then install. For an update that is already
    /// installing, or ready, this quits and relaunches the app now.
    Install,
    /// Not now. An update already installing, or ready, still installs when
    /// the app quits.
    Dismiss,
}

impl UpdateChoice {
    fn raw(self) -> isize {
        match self {
            Self::Skip => 0,
            Self::Install => 1,
            Self::Dismiss => 2,
        }
    }
}

// Sparkle's `SPUUserDriver`. Declared so the driver class registers as
// conforming to it, and so a debug build refuses to register a driver that
// lacks a method the linked framework requires.
extern_protocol!(
    #[expect(
        clippy::missing_safety_doc,
        reason = "crate-private; the macro's generated items trip the lint"
    )]
    #[name = "SPUUserDriver"]
    pub(crate) unsafe trait SPUUserDriver: NSObjectProtocol {}
);

/// `void (^)(SUUpdatePermissionResponse *)`, copied off Sparkle's stack.
type PermissionReply = RcBlock<dyn Fn(*mut AnyObject)>;
/// `void (^)(SPUUserUpdateChoice)`.
type ChoiceReply = RcBlock<dyn Fn(isize)>;

pub struct DriverIvars {
    prompts: RefCell<Option<PromptCallback>>,
    permission_reply: RefCell<Option<PermissionReply>>,
    update_reply: RefCell<Option<ChoiceReply>>,
    install_reply: RefCell<Option<ChoiceReply>>,
    /// The check's cancellation, then the download's; valid until the next
    /// stage begins.
    cancellation: RefCell<Option<RcBlock<dyn Fn()>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RustSparkleInAppUserDriver"]
    #[ivars = DriverIvars]
    pub struct InAppUserDriver;

    unsafe impl NSObjectProtocol for InAppUserDriver {}

    unsafe impl SPUUserDriver for InAppUserDriver {
        #[unsafe(method(showUpdatePermissionRequest:reply:))]
        fn show_update_permission_request(
            &self,
            _request: &NSObject,
            reply: &Block<dyn Fn(*mut AnyObject)>,
        ) {
            *self.ivars().permission_reply.borrow_mut() = Some(reply.copy());
            self.emit(Prompt::PermissionRequest);
        }

        #[unsafe(method(showUserInitiatedUpdateCheckWithCancellation:))]
        fn show_user_initiated_update_check(&self, cancellation: &Block<dyn Fn()>) {
            *self.ivars().cancellation.borrow_mut() = Some(cancellation.copy());
            self.emit(Prompt::CheckStarted);
        }

        #[unsafe(method(showUpdateFoundWithAppcastItem:state:reply:))]
        fn show_update_found(
            &self,
            item: &SPUAppcastItem,
            state: &SPUUserUpdateState,
            reply: &Block<dyn Fn(isize)>,
        ) {
            // The check that found it is over; nothing is left to cancel.
            self.ivars().cancellation.borrow_mut().take();
            *self.ivars().update_reply.borrow_mut() = Some(reply.copy());
            self.emit(Prompt::UpdateFound {
                update: update_info_from_item(item),
                state: UserUpdateState {
                    stage: UserUpdateStage::from_raw(state.stage()),
                    user_initiated: state.user_initiated(),
                },
            });
        }

        #[unsafe(method(showUpdateReleaseNotesWithDownloadData:))]
        fn show_update_release_notes(&self, download_data: &NSObject) {
            let data: Retained<NSObject> = unsafe { msg_send![download_data, data] };
            let length: usize = unsafe { msg_send![&*data, length] };
            let bytes: *const u8 = unsafe { msg_send![&*data, bytes] };
            let text = if bytes.is_null() || length == 0 {
                String::new()
            } else {
                // NSData owns `length` readable bytes at `bytes` while `data`
                // is alive, which it is for this whole statement.
                String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(bytes, length) })
                    .into_owned()
            };
            self.emit(Prompt::ReleaseNotes(text));
        }

        #[unsafe(method(showUpdateReleaseNotesFailedToDownloadWithError:))]
        fn show_update_release_notes_failed(&self, error: &NSObject) {
            self.emit(Prompt::ReleaseNotesFailed(error_payload(error)));
        }

        #[unsafe(method(showUpdateNotFoundWithError:acknowledgement:))]
        fn show_update_not_found(&self, error: &NSObject, acknowledgement: &Block<dyn Fn()>) {
            self.ivars().cancellation.borrow_mut().take();
            self.emit(Prompt::NotFound(error_payload(error)));
            acknowledgement.call(());
        }

        #[unsafe(method(showUpdaterError:acknowledgement:))]
        fn show_updater_error(&self, error: &NSObject, acknowledgement: &Block<dyn Fn()>) {
            self.ivars().cancellation.borrow_mut().take();
            self.emit(Prompt::Error(error_payload(error)));
            acknowledgement.call(());
        }

        #[unsafe(method(showDownloadInitiatedWithCancellation:))]
        fn show_download_initiated(&self, cancellation: &Block<dyn Fn()>) {
            *self.ivars().cancellation.borrow_mut() = Some(cancellation.copy());
            self.emit(Prompt::DownloadStarted);
        }

        #[unsafe(method(showDownloadDidReceiveExpectedContentLength:))]
        fn show_download_expected_length(&self, expected: u64) {
            self.emit(Prompt::DownloadExpectedLength(expected));
        }

        #[unsafe(method(showDownloadDidReceiveDataOfLength:))]
        fn show_download_received(&self, length: u64) {
            self.emit(Prompt::DownloadReceived(length));
        }

        #[unsafe(method(showDownloadDidStartExtractingUpdate))]
        fn show_extraction_started(&self) {
            // Sparkle's contract: the download may be canceled until now.
            self.ivars().cancellation.borrow_mut().take();
            self.emit(Prompt::ExtractionStarted);
        }

        #[unsafe(method(showExtractionReceivedProgress:))]
        fn show_extraction_progress(&self, progress: f64) {
            self.emit(Prompt::ExtractionProgress(progress));
        }

        #[unsafe(method(showReadyToInstallAndRelaunch:))]
        fn show_ready_to_install(&self, reply: &Block<dyn Fn(isize)>) {
            *self.ivars().install_reply.borrow_mut() = Some(reply.copy());
            self.emit(Prompt::ReadyToInstall);
        }

        #[unsafe(method(showInstallingUpdateWithApplicationTerminated:retryTerminatingApplication:))]
        fn show_installing(&self, application_terminated: bool, _retry: &Block<dyn Fn()>) {
            self.emit(Prompt::Installing {
                application_terminated,
            });
        }

        #[unsafe(method(showUpdateInstalledAndRelaunched:acknowledgement:))]
        fn show_installed(&self, relaunched: bool, acknowledgement: &Block<dyn Fn()>) {
            self.emit(Prompt::Installed { relaunched });
            acknowledgement.call(());
        }

        #[unsafe(method(dismissUpdateInstallation))]
        fn dismiss_update_installation(&self) {
            // Dropped, not called: Sparkle has torn the session down and a
            // late reply would reach a driver that no longer exists.
            self.clear_pending();
            self.emit(Prompt::Dismissed);
        }

        #[unsafe(method(showUpdateInFocus))]
        fn show_update_in_focus(&self) {
            self.emit(Prompt::ShowInFocus);
        }
    }
);

impl InAppUserDriver {
    pub(crate) fn new(mtm: MainThreadMarker, prompts: PromptCallback) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DriverIvars {
            prompts: RefCell::new(Some(prompts)),
            permission_reply: RefCell::new(None),
            update_reply: RefCell::new(None),
            install_reply: RefCell::new(None),
            cancellation: RefCell::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    // Release the borrow before calling host code: a prompt may be answered
    // from inside the callback, which takes the reply it was just given.
    fn emit(&self, prompt: Prompt) {
        let callback = self.ivars().prompts.borrow().clone();
        if let Some(callback) = callback {
            callback(prompt);
        }
    }

    fn clear_pending(&self) {
        let ivars = self.ivars();
        ivars.permission_reply.borrow_mut().take();
        ivars.update_reply.borrow_mut().take();
        ivars.install_reply.borrow_mut().take();
        ivars.cancellation.borrow_mut().take();
    }

    /// Answer the waiting [`Prompt::UpdateFound`]. `false` when none waits.
    pub(crate) fn answer_update(&self, choice: UpdateChoice) -> bool {
        // Taken before the call: the reply may re-enter this driver.
        let reply = self.ivars().update_reply.borrow_mut().take();
        reply.map(|reply| reply.call((choice.raw(),))).is_some()
    }

    /// Answer the waiting [`Prompt::ReadyToInstall`]. `false` when none waits.
    pub(crate) fn answer_ready_to_install(&self, choice: UpdateChoice) -> bool {
        let reply = self.ivars().install_reply.borrow_mut().take();
        reply.map(|reply| reply.call((choice.raw(),))).is_some()
    }

    /// Answer the waiting [`Prompt::PermissionRequest`]. `false` when none
    /// waits, or when the linked Sparkle no longer has the response class.
    pub(crate) fn answer_permission(&self, automatic_checks: bool) -> bool {
        let Some(class) = AnyClass::get(c"SUUpdatePermissionResponse") else {
            return false;
        };
        let reply = self.ivars().permission_reply.borrow_mut().take();
        let Some(reply) = reply else {
            return false;
        };
        let response: Retained<NSObject> = unsafe {
            let allocated: Allocated<NSObject> = msg_send![class, alloc];
            msg_send![
                allocated,
                initWithAutomaticUpdateChecks: automatic_checks,
                sendSystemProfile: false
            ]
        };
        reply.call((Retained::as_ptr(&response).cast_mut().cast(),));
        true
    }

    /// Cancel the running user-initiated check, or the download before it is
    /// extracted. `false` when neither can be canceled.
    pub(crate) fn cancel(&self) -> bool {
        let cancellation = self.ivars().cancellation.borrow_mut().take();
        cancellation.map(|cancel| cancel.call(())).is_some()
    }

    pub(crate) fn awaiting_update_answer(&self) -> bool {
        self.ivars().update_reply.borrow().is_some()
    }

    pub(crate) fn awaiting_install_answer(&self) -> bool {
        self.ivars().install_reply.borrow().is_some()
    }
}

/// Selectors the linked Sparkle requires of a user driver that this one does
/// not answer, plus a line if the class does not register as conforming to
/// `SPUUserDriver` at all. Empty is the only healthy answer.
///
/// A Sparkle upgrade that adds a required method builds cleanly and then
/// crashes on "unrecognized selector" the first time Sparkle sends it. Hosts
/// can call this from a test so the upgrade fails there instead.
pub fn missing_user_driver_methods() -> Vec<String> {
    let Some(protocol) = AnyProtocol::get(c"SPUUserDriver") else {
        return vec!["the linked Sparkle has no SPUUserDriver protocol".to_owned()];
    };
    let class = InAppUserDriver::class();
    let mut missing = Vec::new();
    if !class.conforms_to(protocol) {
        missing.push("InAppUserDriver does not conform to SPUUserDriver".to_owned());
    }
    let mut count: c_uint = 0;
    let descriptions = unsafe {
        objc2::ffi::protocol_copyMethodDescriptionList(
            protocol,
            Bool::YES,
            Bool::YES,
            &mut count,
        )
    };
    if descriptions.is_null() {
        return missing;
    }
    // `count` descriptions, owned by us until freed below.
    for description in unsafe { std::slice::from_raw_parts(descriptions, count as usize) } {
        if let Some(sel) = description.name {
            if class.instance_method(sel).is_none() {
                missing.push(format!("-[SPUUserDriver {}]", sel.name().to_string_lossy()));
            }
        }
    }
    unsafe { objc2::ffi::free(descriptions.cast()) };
    missing
}
