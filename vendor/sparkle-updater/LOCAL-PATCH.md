# Local patch: an in-app user driver

This directory is upstream [`sparkle-updater` 0.1.0][crate] (MIT, see
`LICENSE-MIT`), vendored so monopro can replace Sparkle's standard user driver
with one of its own. It is wired in with `[patch.crates-io]` in the workspace
`Cargo.toml`; keep it in step with the pinned Sparkle framework version
(2.9.6, `packaging/macos/Sparkle`).

[crate]: https://crates.io/crates/sparkle-updater/0.1.0

## Why

Upstream starts Sparkle through `SPUStandardUpdaterController`, whose
`SPUStandardUserDriver` answers every prompt with an AppKit window. The gentle
reminders delegate can keep a *scheduled* update quiet, but a user-initiated
check always goes through the standard interface: "Check for Updates" put
Sparkle's "You're up to date!" alert, its update alert and its progress panel on
top of monopro's own update sheet, two windows for one question.

The standard driver also holds the reply block for the update it is presenting,
and the public API has no way to answer it. monopro's "Skip this version" needs
exactly that reply — it is what cancels an update already staged for install on
quit — so an earlier version of this patch reached into the driver's private
`activeUpdateAlert` and sent `skipThisVersion:` to it. With the driver ours, the
reply is ours too, and that private route is gone.

## Delta

- `src/user_driver.rs` (new): `InAppUserDriver`, an Objective-C class
  implementing Sparkle's public `SPUUserDriver` protocol. Each call becomes a
  `Prompt` for the host's `PromptCallback`; the reply blocks for an update
  found, ready to install and the permission request are kept until the host
  answers, and cancellation blocks until they lapse. Acknowledgements (not found,
  error, installed) are given as soon as the prompt is delivered, as Sparkle's
  own command-line driver gives them. `dismissUpdateInstallation` drops every
  held block without calling it. `missing_user_driver_methods()` lists any
  selector the linked framework requires that the driver does not answer.
- `src/updater.rs`: `UpdaterConfig::prompt_callback`. When it is `Some`, Sparkle
  starts as a bare `SPUUpdater` (`initWithHostBundle:applicationBundle:
  userDriver:delegate:`) with the in-app driver, and `SparkleUpdater` gains
  `answer_update`, `answer_ready_to_install`, `answer_permission`, `cancel`,
  `awaiting_update_answer` and `awaiting_install_answer`. When it is `None`
  nothing changes: the standard controller and its windows, as upstream. The
  wrapper now holds the `SPUUpdater` directly and calls it instead of going
  through the controller, which only forwarded. `skip_current_update` and
  `SkipOutcome`, the earlier private-API patch, are removed.
- `src/bindings.rs`: the `SPUUpdater` initializer above.
- `src/delegate.rs`: `update_info_from_item` and `error_payload` are
  `pub(crate)`, for the driver's prompts.
- `src/error.rs`: `Error::NoInAppDriver`, returned by the answer methods when
  Sparkle is using its standard driver.
- `src/lib.rs`: the new module and its public items.
- `src/native_tests.rs`: the driver against the real selectors — prompts
  delivered, replies held and answered once, acknowledgements immediate,
  cancellation lapsing at extraction, dismissal voiding a held reply, and a real
  `SUUpdatePermissionResponse` built for the permission reply.
- `README.md`: a paragraph on `prompt_callback`.

Everything else is byte-for-byte upstream. The driver uses only Sparkle's public
headers; the one class it looks up by name, `SUUpdatePermissionResponse`, is
public too. A Sparkle upgrade that adds a required method to `SPUUserDriver`
would build and then crash on the first prompt using it, so
`crates/raw-app/src/updater.rs` asserts `missing_user_driver_methods()` is empty
against the linked framework in a macOS test, and a debug build refuses to
register the class at all.
