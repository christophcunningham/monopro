//! Runs on the process main thread and invokes the real Objective-C selectors.
#![allow(dead_code)]

mod bindings;
// A custom harness does not register #[test] functions in these shared modules.
#[allow(unused_imports)]
mod callbacks;
mod delegate;
#[allow(unused_imports)]
mod events;
mod user_driver;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{msg_send, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSDictionary, NSError, NSString};

use bindings::{SPUAppcastItem, SPUUserUpdateState};
use callbacks::{GentleReminders, RelaunchContinuation};
use delegate::SparkleDelegate;
use events::{UpdateEvent, UpdateInfo, UserUpdateStage, UserUpdateState};
use user_driver::{InAppUserDriver, Prompt, UpdateChoice};

fn update_item(mtm: MainThreadMarker) -> Retained<SPUAppcastItem> {
    let version_key = NSString::from_str("sparkle:version");
    let url_key = NSString::from_str("url");
    let version = NSString::from_str("2.0");
    let url = NSString::from_str("https://example.com/App_2.0.zip");
    let enclosure = NSDictionary::from_slices(&[&*version_key, &*url_key], &[&*version, &*url]);
    let enclosure_key = NSString::from_str("enclosure");
    let dict = NSDictionary::<NSString, AnyObject>::from_slices(&[&*enclosure_key], &[&*enclosure]);
    // The deprecated dictionary initializer is confined to this fixture; it
    // constructs a real Sparkle item without requiring a network update cycle.
    unsafe { msg_send![SPUAppcastItem::alloc(mtm), initWithDictionary: &*dict] }
}

struct Reminders {
    shown: Cell<bool>,
    attention: Cell<bool>,
    finished: Cell<bool>,
}

impl GentleReminders for Reminders {
    fn should_show_scheduled_update(&self, update: &UpdateInfo, immediate_focus: bool) -> bool {
        assert_eq!(update.version, "2.0");
        assert!(!immediate_focus);
        false
    }

    fn will_show_update(
        &self,
        handled_by_sparkle: bool,
        update: &UpdateInfo,
        state: UserUpdateState,
    ) {
        assert!(!handled_by_sparkle);
        assert_eq!(update.version, "2.0");
        assert_eq!(state.stage, UserUpdateStage::Downloaded);
        assert!(state.user_initiated);
        self.shown.set(true);
    }

    fn did_receive_user_attention(&self, _update: &UpdateInfo) {
        self.attention.set(true);
    }

    fn will_finish_update_session(&self) {
        self.finished.set(true);
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("native harness runs on main thread");
    objc2::rc::autoreleasepool(|_| {
        let delegate = SparkleDelegate::new(mtm);
        let updater = NSObject::new();
        let item = update_item(mtm);
        // Sparkle's private initializer is used only by this ABI regression
        // fixture so the state can have a known nonzero stage.
        let state: Retained<SPUUserUpdateState> = unsafe {
            msg_send![SPUUserUpdateState::alloc(mtm), initWithStage: 1isize, userInitiated: true]
        };
        let seen = Rc::new(Cell::new(false));
        let captured = seen.clone();
        let reentrant_delegate = delegate.clone();
        delegate.set_event_callback(Some(Rc::new(move |event| {
            if let UpdateEvent::UserDidMakeChoice(info) = event {
                assert_eq!(info.stage, "downloaded");
                assert_eq!(info.choice, "install");
                captured.set(true);
                reentrant_delegate.set_event_callback(None);
            }
        })));
        unsafe {
            let _: () = msg_send![&*delegate, updater: &*updater, userDidMakeChoice: 1isize, forUpdate: &*item, state: &*state];
        }
        assert!(seen.get());

        let reminders = Rc::new(Reminders {
            shown: Cell::new(false),
            attention: Cell::new(false),
            finished: Cell::new(false),
        });
        let supported: bool =
            unsafe { msg_send![&*delegate, supportsGentleScheduledUpdateReminders] };
        assert!(!supported);
        delegate.set_gentle_reminders(Some(reminders.clone()));
        let supported: bool =
            unsafe { msg_send![&*delegate, supportsGentleScheduledUpdateReminders] };
        assert!(supported);
        let show: bool = unsafe {
            msg_send![&*delegate, standardUserDriverShouldHandleShowingScheduledUpdate: &*item, andInImmediateFocus: false]
        };
        assert!(!show);
        unsafe {
            let _: () = msg_send![&*delegate, standardUserDriverWillHandleShowingUpdate: false, forUpdate: &*item, state: &*state];
            let _: () =
                msg_send![&*delegate, standardUserDriverDidReceiveUserAttentionForUpdate: &*item];
            let _: () = msg_send![&*delegate, standardUserDriverWillFinishUpdateSession];
        }
        assert!(reminders.shown.get() && reminders.attention.get() && reminders.finished.get());

        let continuation: Rc<RefCell<Option<RelaunchContinuation>>> = Rc::new(RefCell::new(None));
        let captured = continuation.clone();
        delegate.set_relaunch_handler(Some(Rc::new(move |_, resume| {
            *captured.borrow_mut() = Some(resume);
        })));
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let block = RcBlock::new(move || captured.set(captured.get() + 1));
        let postponed: bool = unsafe {
            msg_send![&*delegate, updater: &*updater, shouldPostponeRelaunchForUpdate: &*item, untilInvokingBlock: &*block]
        };
        assert!(postponed);
        assert_eq!(calls.get(), 0);
        drop(block);
        continuation.borrow_mut().take().unwrap().resume(mtm);
        assert_eq!(calls.get(), 1);

        delegate.set_relaunch_handler(Some(Rc::new(move |_, continuation| {
            continuation.resume(mtm)
        })));
        let captured = calls.clone();
        let block = RcBlock::new(move || captured.set(captured.get() + 1));
        let postponed: bool = unsafe {
            msg_send![&*delegate, updater: &*updater, shouldPostponeRelaunchForUpdate: &*item, untilInvokingBlock: &*block]
        };
        assert!(!postponed);
        assert_eq!(calls.get(), 1);

        in_app_user_driver(mtm, &item, &state);
        println!("native selectors: state object, gentle reminders, deferred and synchronous relaunch, in-app user driver passed");
    });
}

/// The in-app driver against the real selectors Sparkle sends: every prompt
/// reaches the host, every reply is held until the host answers it once, and
/// nothing survives Sparkle dismissing the session.
fn in_app_user_driver(
    mtm: MainThreadMarker,
    item: &SPUAppcastItem,
    state: &SPUUserUpdateState,
) {
    let missing = user_driver::missing_user_driver_methods();
    assert!(missing.is_empty(), "the in-app driver lacks {missing:?}");

    let prompts: Rc<RefCell<Vec<Prompt>>> = Rc::new(RefCell::new(Vec::new()));
    let captured = prompts.clone();
    let driver = InAppUserDriver::new(mtm, Rc::new(move |p| captured.borrow_mut().push(p)));

    // An update found: the prompt carries the item and state, the reply
    // waits, and the answer is delivered once with Sparkle's raw choice.
    let choice = Rc::new(Cell::new(-1isize));
    let captured = choice.clone();
    let reply = RcBlock::new(move |c: isize| captured.set(c));
    unsafe {
        let _: () = msg_send![&*driver, showUpdateFoundWithAppcastItem: item, state: state, reply: &*reply];
    }
    drop(reply);
    match prompts.borrow().last() {
        Some(Prompt::UpdateFound { update, state }) => {
            assert_eq!(update.version, "2.0");
            assert_eq!(state.stage, UserUpdateStage::Downloaded);
            assert!(state.user_initiated);
        }
        other => panic!("expected UpdateFound, got {other:?}"),
    }
    assert!(driver.awaiting_update_answer());
    assert_eq!(choice.get(), -1, "nothing is answered on the host's behalf");
    assert!(driver.answer_update(UpdateChoice::Skip));
    assert_eq!(choice.get(), 0);
    assert!(!driver.answer_update(UpdateChoice::Install), "a reply is one-shot");
    assert_eq!(choice.get(), 0);

    // Ready to install: the same, through its own slot.
    let captured = choice.clone();
    let reply = RcBlock::new(move |c: isize| captured.set(c));
    unsafe {
        let _: () = msg_send![&*driver, showReadyToInstallAndRelaunch: &*reply];
    }
    assert!(matches!(prompts.borrow().last(), Some(Prompt::ReadyToInstall)));
    assert!(driver.awaiting_install_answer());
    assert!(driver.answer_ready_to_install(UpdateChoice::Install));
    assert_eq!(choice.get(), 1);

    // Not found: reported, then acknowledged without waiting on the host.
    let acknowledged = Rc::new(Cell::new(false));
    let captured = acknowledged.clone();
    let ack = RcBlock::new(move || captured.set(true));
    let domain = NSString::from_str("SUSparkleErrorDomain");
    let error: Retained<NSError> = unsafe {
        msg_send![NSError::class(), errorWithDomain: &*domain, code: 1001isize, userInfo: None::<&NSObject>]
    };
    unsafe {
        let _: () = msg_send![&*driver, showUpdateNotFoundWithError: &*error, acknowledgement: &*ack];
    }
    assert!(acknowledged.get());
    assert!(matches!(prompts.borrow().last(), Some(Prompt::NotFound(e)) if e.code == 1001));

    // A download can be canceled until extraction starts, and only once.
    let canceled = Rc::new(Cell::new(0));
    let captured = canceled.clone();
    let cancel = RcBlock::new(move || captured.set(captured.get() + 1));
    unsafe {
        let _: () = msg_send![&*driver, showDownloadInitiatedWithCancellation: &*cancel];
        let _: () = msg_send![&*driver, showDownloadDidReceiveExpectedContentLength: 2048u64];
        let _: () = msg_send![&*driver, showDownloadDidReceiveDataOfLength: 1024u64];
    }
    assert!(matches!(prompts.borrow().last(), Some(Prompt::DownloadReceived(1024))));
    assert!(driver.cancel());
    assert!(!driver.cancel());
    assert_eq!(canceled.get(), 1);
    unsafe {
        let _: () = msg_send![&*driver, showDownloadInitiatedWithCancellation: &*cancel];
        let _: () = msg_send![&*driver, showDownloadDidStartExtractingUpdate];
    }
    assert!(!driver.cancel(), "extraction ends the window for canceling");
    assert_eq!(canceled.get(), 1);

    // Dismissal voids a waiting reply without calling it.
    let captured = choice.clone();
    let reply = RcBlock::new(move |c: isize| captured.set(c + 100));
    unsafe {
        let _: () = msg_send![&*driver, showUpdateFoundWithAppcastItem: item, state: state, reply: &*reply];
        let _: () = msg_send![&*driver, dismissUpdateInstallation];
    }
    assert!(matches!(prompts.borrow().last(), Some(Prompt::Dismissed)));
    assert!(!driver.awaiting_update_answer());
    assert!(!driver.answer_update(UpdateChoice::Install));
    assert_eq!(choice.get(), 1);

    // The permission request is answered with a real Sparkle response.
    let automatic = Rc::new(Cell::new(None));
    let captured = automatic.clone();
    let reply = RcBlock::new(move |response: *mut AnyObject| {
        assert!(!response.is_null());
        let checks: bool = unsafe { msg_send![&*response, automaticUpdateChecks] };
        captured.set(Some(checks));
    });
    let request = NSObject::new();
    unsafe {
        let _: () = msg_send![&*driver, showUpdatePermissionRequest: &*request, reply: &*reply];
    }
    assert!(matches!(prompts.borrow().last(), Some(Prompt::PermissionRequest)));
    assert!(driver.answer_permission(false));
    assert_eq!(automatic.get(), Some(false));
}
