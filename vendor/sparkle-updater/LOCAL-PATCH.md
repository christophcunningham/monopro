# Local patch: `skip_current_update`

This directory is upstream [`sparkle-updater` 0.1.0][crate] (MIT, see
`LICENSE-MIT`), vendored so monopro can add one binding the released crate does
not expose. It is wired in with `[patch.crates-io]` in the workspace
`Cargo.toml`; keep it in step with the pinned Sparkle framework version
(2.9.6, `packaging/macos/Sparkle`).

[crate]: https://crates.io/crates/sparkle-updater/0.1.0

## Why

Sparkle's standard user driver holds the reply block for a pending update
alert. For a scheduled update the gentle-reminders delegate can take over
presentation, which leaves that reply waiting while Sparkle has already
downloaded and staged the update for install-on-quit. The public API has no way
to answer the pending alert, so an app-side "Skip this version" only wrote
`SUSkippedVersion`; the staged installer still ran at quit, and the resumed
session re-offered the version.

`SparkleUpdater::skip_current_update` closes that gap by invoking the Skip
action on the standard user driver's active alert, which is the same action the
alert's "Skip This Version" button runs. Through the reply block it records
Sparkle's own skip and, when an update is already staged, reaches
`SPUCoreBasedUpdateDriver`'s skip branch and cancels the staged installation.

## Delta

- `src/updater.rs`: added `skip_current_update()` and its doc comment. It asks
  the controller for its `userDriver` (public property), the driver for its
  `activeUpdateAlert` (declared in the framework's shipped `PrivateHeaders`),
  and sends `skipThisVersion:` to the alert. Returns `Ok(false)` when there is
  no pending alert.

Everything else is byte-for-byte upstream. The patch depends on Sparkle 2.9.6
internals (`SPUStandardUserDriver.activeUpdateAlert`, `SUUpdateAlert`'s
`skipThisVersion:` action); the vendored framework is checksum-pinned, so the
pair moves together.
