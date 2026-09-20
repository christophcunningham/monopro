# Packaged release smoke test

Run the functional checks against the exact unsigned/ad-hoc candidate artifacts produced
by the package workflow. They are preflight artifacts, so A2 remains open. Before
publication, rebuild signed/notarized artifacts from the same commit, verify their new
checksums, and repeat A1–A4 plus the platform's install/upgrade/uninstall flow. This is
the evidence CI cannot supply: a person using the real desktop, GPU, dialogs and
filesystems. One unchecked required row blocks publication.

## Test record

Copy this block into the release issue for every environment tested:

```text
Artifact filename:
Artifact SHA-256:
Commit:
Phase: functional candidate / final signed artifact
Tester and date:
Computer / CPU:
GPU and driver:
OS and exact version/build:
Desktop/session (Linux):
Display count, scale and HDR state:
Local filesystem:
External filesystem/device:
Network filesystem/server:
Result: PASS / FAIL
Failed check IDs and issue links:
```

Verify the artifact SHA-256 against its adjacent `.sha256` file before beginning.
Never substitute a locally rebuilt binary halfway through a record.

## Required environment matrix

| ID | Environment | Required session and coverage |
|---|---|---|
| M1 | Current macOS, Apple Silicon | Metal; Retina scaling; packaged DMG |
| M2 | macOS 11+, Intel | Metal; universal binary's x86-64 half |
| W1 | Windows 11 25H2 x86-64 | DirectX 12; 100% and fractional display scale |
| U1 | Ubuntu 24.04 x86-64 | GNOME Wayland; Vulkan |
| U2 | Ubuntu 24.04 x86-64 | GNOME on Xorg; Vulkan |
| F1 | Current Fedora x86-64 | GNOME Wayland; Vulkan |
| O1 | Current Omarchy x86-64 | Hyprland Wayland; portal file chooser |
| C1 | Current CachyOS x86-64 | Its default Wayland desktop; Vulkan |

At least one Windows/Linux pass must use Intel graphics, one must use AMD, and one must
use NVIDIA. These may be distributed across the rows. Omarchy and CachyOS are covered
by the Arch-container ABI check in CI, but that check does not satisfy O1 or C1 because
it has no compositor, portal or GPU.

For an AppImage, make it executable first. If it reports a FUSE error, record that fact,
install the distribution's FUSE 2 compatibility package, and repeat. Record both the
initial experience and the package installed (`fuse-libs` on Fedora, `fuse2` on
Arch-family systems).

## Test material

Use copies, never the only copy of a photograph:

- a small supported RAW with no sidecar;
- a supported RAW with an existing `.mono.xmp` edit;
- the largest supported sensor file available;
- a folder containing RAW, JPEG, TIFF, subfolders and a deliberately unsupported file;
- a writable removable exFAT volume and a writable SMB network share;
- a read-only folder and a path that can be disconnected while monopro is open;
- one non-bundled system font for the contact sheet.

## A. Package lifecycle

- [ ] **A1** The downloaded filename, version and architecture match the release record.
- [ ] **A2** The final publication package passes the OS signature/notarization UI with no bypass instructions.
- [ ] **A3** A clean install or first launch creates no files outside the documented app-data and cache roots.
- [ ] **A4** The launcher, application icon, window title and OS app switcher identity all say `monopro`.
- [ ] **A5** Install the previous public version, change a preference, then upgrade in place. The preference and edits survive and only one application entry remains.
- [ ] **A6** Uninstall/delete the application. Program files disappear; settings, cache and photo sidecars remain. Reinstall and confirm settings are recovered.
- [ ] **A7** (macOS only) With a staged update, quit the app. The install completes after termination and the relaunched app reports the new version. Quitting while an export writes is refused with a clear note, and no install touches the running bundle.
- [ ] **A8** (macOS only) The update sheet's release notes render as plain text, survive an offline relaunch from cache, and "Skip this version" sticks across restarts until Settings → About clears it. **Test the staged case**: let the badge say the update is ready, then skip, quit, and confirm the skipped version is not installed — the skip has to cancel the installer Sparkle staged, not just darken the badge.
- [ ] **A9** (macOS only) A corrupted or mismatched update signature reports a one-line failure and leaves the running application untouched.

For macOS, test drag-to-Applications. For Windows, test the default per-user install,
Apps & Features uninstall, and one administrator-approved all-users install. For Linux,
test run-in-place, launcher integration if used, replacement upgrade and deletion.

## B. Window and input shell

- [ ] **B1** Main and Settings windows expose working native close, minimize, maximize/fullscreen, move and resize controls.
- [ ] **B2** Windows snap and Linux compositor tiling work; macOS traffic lights and fullscreen behave normally.
- [ ] **B3** At 100%, 150%, 200% and a second monitor with a different scale, text and image pixels remain sharp and controls remain reachable.
- [ ] **B4** Detached panels can open, move between monitors, regain focus and close without taking down the main window.
- [ ] **B5** Command shortcuts work on macOS; Ctrl shortcuts work on Windows/Linux; typing in a text field does not trigger an application command.
- [ ] **B6** Window geometry, panel layout and last folder survive a normal restart and recover sensibly after the previous monitor is removed.

## C. Dialogs, drops and desktop integration

- [ ] **C1** Open RAW shows the desktop's native/portal chooser, starts in an existing useful folder and filters to supported RAW formats.
- [ ] **C2** Choose Folder opens Lightbox at that folder. Cancelling either chooser changes no tab, folder or status.
- [ ] **C3** Export and contact-sheet save dialogs enforce the selected extension and ask before replacing an existing file.
- [ ] **C4** Drag one RAW onto the window: it opens in Develop and its folder is available in Lightbox.
- [ ] **C5** Drag a folder: it opens in Lightbox. Drag an unsupported file and a missing path: each reports a clear error and creates no tab.
- [ ] **C6** Launch from the file manager with a RAW and from a terminal with a relative RAW path. Both open the right file and remember an absolute folder.
- [ ] **C7** “Show in Finder/File Explorer/File Manager” reveals the selected file or folder.
- [ ] **C8** On Omarchy, time C1–C3 and record the portal backend. A chooser that hangs, appears behind the app or cannot select a folder fails O1.

## D. Filesystems and persistence

Repeat D1–D7 on the local filesystem, removable exFAT volume and SMB share.

- [ ] **D1** Browse a large folder, search it, close and reopen the app, and recover the same location without a rescan failure.
- [ ] **D2** Save settings twice with different values. The second value survives restart and no `.tmp`/`.part` file remains.
- [ ] **D3** Edit a RAW, save its sidecar twice, restart, and recover the second edit exactly. The RAW bytes and timestamp do not change.
- [ ] **D4** Rate, label and manually reorder photographs; restart and confirm all three persist.
- [ ] **D5** Perform a case-only rename, a two-file name swap and a rename whose destination appears after confirmation. No file or sidecar is lost or overwritten.
- [ ] **D6** Disconnect the removable/network location while browsing and while opening a file. The app remains responsive, reports the unavailable path and can continue after reconnection.
- [ ] **D7** Attempt sidecar, rename and export operations in the read-only folder. Each fails clearly; existing files remain byte-identical.
- [ ] **D8** Purge thumbnails in Settings. The documented cache root is emptied without touching settings, presets or sidecars.

## E. Photograph and GPU pipeline

- [ ] **E1** Open the small and largest RAWs; orientation, crop and preview dimensions are correct and an oversized image produces a useful limit message rather than a crash.
- [ ] **E2** Exercise exposure, contrast mask, Dodge & Burn, curves, toning, grain, sharpening and crop/rotation. The preview updates without stale tiles or black frames.
- [ ] **E3** The finished-image histogram, clipping counts, cursor value and pinned samples follow those edits and the crop.
- [ ] **E4** Hide/show panels, resize continuously, switch tabs rapidly and enter the print loupe. Memory remains bounded and the GPU does not reset.
- [ ] **E5** Suspend/resume, lock/unlock and hot-plug a display while an edited image is open. The device either recovers or reports a clean actionable failure.
- [ ] **E6** Export 16-bit TIFF master and proof outputs. Reopen them elsewhere and verify dimensions, bit depth, grayscale profile and visual orientation.
- [ ] **E7** Export a PDF contact sheet using a non-bundled system font. Verify page count, filenames, embedded font and print appearance.

## F. Completion rule

- [ ] **F1** Every required environment has a test record tied to its exact candidate checksum.
- [ ] **F2** Every required check is marked pass, or an approved release exception links to a bounded issue and is stated in the public release notes.
- [ ] **F3** Signed/notarized release artifacts are rebuilt from the tested commit, their package validators pass, their final checksums are recorded, and A1–A4 plus lifecycle checks pass on them.
- [ ] **F4** A second person verifies the checksums and reviews all failures/exceptions before publication.

CI compilation, unsigned candidate artifacts and container ABI checks are preparation,
not substitutes for this record. A release is ready only after F1–F4 are true.
