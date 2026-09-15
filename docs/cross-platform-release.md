# Cross-platform release gate

Recorded 2026-09-12. Public release requires monopro to be operational and tested on
macOS, Windows and Linux. The portable Rust core is not sufficient evidence: each OS
needs its own build, package and runtime verification.

## Current state

- Only macOS is built and exercised. Native package definitions now exist for all three
  systems, but no Windows or Linux artifact has been built or exercised on its target
  OS.
- egui, eframe and wgpu provide a suitable shared UI and GPU foundation. The image
  pipeline, WGSL passes and most application widgets should remain common code.
- Native CI and candidate-package jobs are defined, along with a recordable manual
  smoke-test gate. They become evidence only after running on the hosted/native systems;
  a successful macOS build still says nothing about DirectX 12, Vulkan or the native
  integrations on the other systems.

## Implementation record

**2026-09-13 — first portability slice.** The full workspace now passes `cargo check
--workspace --all-targets` for `x86_64-pc-windows-gnu` and
`x86_64-unknown-linux-gnu`, as well as compiling locally on macOS. This is compile
evidence only, not a packaged-app or runtime pass.

- `muda` is now a macOS-only dependency. Windows and Linux use a dependency-free menu
  boundary that leaves every shortcut in egui; they no longer build an unattached menu
  and then suppress the Ctrl chords it claimed.
- Home-directory lookup, mounted-device roots, root labels and reveal-in-file-manager
  behavior now go through one platform adapter. Finder-specific UI copy is confined to
  macOS; Windows uses File Explorer and Linux uses the system file manager through
  `xdg-open`.
- Contact-sheet font discovery now searches Windows and Linux system/user font
  locations as well as the existing macOS locations. Bundled JetBrains Mono remains
  first and is still the no-system-font fallback.

Native Windows/Linux menus remain open; the safe egui shortcut route is the release
behavior until an attached menu is implemented and exercised. Mounted-device discovery
and reveal behavior likewise still require real-machine tests on both systems.

**2026-09-13 — native window shell slice.** Main, detached-panel and Settings viewport
construction now goes through the platform adapter. macOS keeps the full-size hidden
titlebar and custom drag chrome already exercised there. Windows and Linux explicitly
retain native decorations, window movement and close/minimize/maximize behavior; the
custom Settings titlebar is not drawn beneath them. The shared 28-point app strip names
the active mode on those systems instead of repeating `monopro` under the native title.
Builder tests lock the distinction, and all three target configurations are clippy-clean.
Actual Win32, Wayland and X11 window behavior remains visually and interactively
unchecked until real-machine smoke tests.

**2026-09-13 — packaging and support-baseline slice.** Native package definitions and
an exact first-release matrix now live in `packaging/`. Public artifacts are universal
macOS DMG, x64 Windows Inno Setup EXE, and x64 Linux AppImage. The Windows definition
has a stable upgrade identity, per-user/all-users install modes, Apps & Features
uninstall, version metadata, license and icon. The Linux AppDir carries its desktop
entry, AppStream metadata, icon and license; `linuxdeploy` collects non-base shared
libraries before producing the AppImage. The shared main viewport now embeds the app
icon for the Windows taskbar and Linux desktop shell as well.

Release signing fails closed: `./package --signed` requires Developer ID and notarytool
credentials, while the Windows script requires Authenticode certificate and timestamp
inputs unless its explicit local-only `-Unsigned` switch is used. These paths are
definitions, not release evidence: Apple notarization has not been exercised with
release credentials, and the Windows and Linux package scripts cannot be run or
runtime-tested from this macOS checkout. The ad-hoc-signed arm64 macOS path did produce
a 0.1.0 application and DMG locally; its Mach-O architecture, bundle plist, embedded
ICNS and on-disk signature were inspected successfully. The universal, Developer ID and
notarization paths remain release-host checks.

**2026-09-13 — storage, dialogs and path-intake slice.** Durable state continues to
follow eframe's native data root (`Application Support`, roaming AppData, or XDG data),
while rebuildable thumbnails now use the native cache root (`Library/Caches`, local
AppData, or `XDG_CACHE_HOME`). Named profiles remain isolated in both trees. The README
records the exact paths rather than describing only the macOS installation.

Every durable or rebuildable app-owned write now goes through the shared atomic-file
publisher. Windows uses `ReplaceFileW` when a destination already exists, fixing the
second-and-later save failure inherent in Unix-style rename-overwrite assumptions.
Settings, curve presets, IPTC templates, search locations/index, manual ordering and
thumbnail metadata all share that behavior.

Native dialogs now have one boundary with existing-directory fallback, RAW filters and
enforced export extensions. Dialog, command-line and drop paths are classified before
tabs or remembered state change: missing/unreadable and non-RAW files report one clear
status, while a dropped or command-line folder opens in Lightbox. This is automated
policy evidence only; portal/native dialog appearance, parent-window modality and
desktop drag/drop still require real Win32, Wayland and X11 interaction tests.

**2026-09-15 — automated release-gate slice.** `Native CI` now defines native hosted
compile, clippy and non-display test jobs on macOS 15 arm64, Windows Server 2025 x64 and
Ubuntu 24.04 x64 using the declared Rust 1.92 minimum. The manually dispatched package
workflow creates ad-hoc/unsigned candidates for all three platforms and retains them
with SHA-256 records. Package verification checks macOS bundle/DMG identity and
architectures, the Windows PE architecture/version/signature state, and the Linux
AppImage payload, desktop/AppStream metadata and dynamic links. The Linux job also
extracts the same AppImage under Fedora 44 and current Arch containers; this is ABI
evidence for Fedora and the Arch foundations of Omarchy/CachyOS, not desktop evidence.

The release UI work is now an explicit checklist in `docs/release-smoke-test.md`, with
artifact identity, environment/hardware records and stable check IDs covering package
lifecycle, native windows, dialogs/drop, local/removable/network filesystems, GPU work
and exports. Hosted runners cannot complete that checklist: Windows Server is not the
supported Windows client release, container checks have no Wayland/X11 portal, and CI
GPUs do not represent the Metal/DX12/Vulkan support matrix. No candidate workflow run or
manual record has yet been made, so none of these definitions is release evidence yet.

## Supported first-release matrix

| Platform | Artifact | CPU | Minimum OS / desktop |
|---|---|---|---|
| macOS | signed, notarized DMG | Apple Silicon and Intel | macOS 11.0 |
| Windows | signed Inno Setup EXE | x86-64 | Windows 11 25H2, build 26200 |
| Linux | AppImage | x86-64 | Ubuntu 24.04 LTS; Wayland and X11 |

The Linux artifact must be built on Ubuntu 24.04, not merely tested there, so it does
not acquire a newer glibc requirement. An AppImage is preferred over a sandboxed store
package because removable volumes, network mounts and arbitrary photo directories are
core workflows. Upgrade is replace-and-relaunch on macOS/Linux and an in-place install
under the stable Windows AppId. Uninstall removes the application but deliberately
retains settings and caches on every platform. ARM Windows and ARM Linux are outside
the first-release matrix.

## Settled window direction

Keep one shared design inside the window and a thin platform-specific native shell.
Do not make the main window borderless merely to force identical square corners.
Replacing native decorations would also require reliable close, minimize, maximize,
drag, resize, snap, fullscreen, DPI and accessibility behavior across AppKit, Win32,
Wayland and X11.

The hidden/full-size titlebar options are now confined to macOS. Windows and Linux
retain their native titlebars, and the 28-point app strip beneath them is an intentional
mode/status strip that does not repeat the application title. Window corner shape
remains the OS or compositor's decision. The opaque dark clear color and egui-drawn
controls are shared safely.

## Known portability work

1. **Native menus and shortcuts.** The safe fallback is built: macOS alone installs and
   claims a native menu, while Windows/Linux retain shortcuts in egui. An attached
   native menu on those systems remains optional release work; if added, claim a chord
   only after installation succeeds. Verify Command on macOS and Ctrl conventions on
   Windows/Linux.
2. **Filesystem presentation.** The platform boundary is built for home folders,
   mounted drives, root labels and reveal-in-file-manager behavior. Verify it against
   removable and network drives on each target, especially Linux mount conventions.
3. **Font discovery.** Windows and Linux locations are built, with bundled JetBrains
   Mono retained as the reliable fallback. Verify enumeration and PDF embedding with a
   non-bundled font on each target.
4. **File operations.** Exercise sidecars, atomic publication, case-only and swap
   renames, hard-link fallback, removable media and network paths on NTFS and common
   Linux filesystems. Do not infer their behavior from APFS tests.
5. **GPU compatibility.** Validate the complete editing and export pipeline on Metal,
   DirectX 12 and Vulkan, including lower texture limits, large sensors, readbacks,
   spatial edits and device-loss/error reporting.
6. **Packaging.** Definitions, formats and support baselines are settled and checked in.
   Build them on native hosts, exercise the signing/notarization paths, record checksums
   and run install/upgrade/uninstall smoke tests before publishing any artifact.
7. **Storage and dialogs.** Platform roots, atomic replacement, dialog fallback/filter
   policy and dropped-path classification are built. Verify the actual native dialogs,
   persistence, drag/drop and external/network volumes under each OS's permissions and
   path conventions.

## Release verification

Automated CI compiles and tests on each supported OS family and builds inspectable
candidate packages. Run `docs/release-smoke-test.md` against the exact candidate
checksums on real or remote interactive machines; it covers:

- first launch, install/upgrade/uninstall and settings persistence;
- open, browse, search, rename, sidecar save and reveal in the native file manager;
- menus, shortcuts, window controls, DPI scaling, multiple windows and drag/drop;
- RAW decode, GPU editing, finished-image sampling, proof/master export and PDF contact
  sheets, including an external drive;
- clean failure messages for unsupported GPUs, permissions and unavailable paths.

The first-release versions, Linux distribution/display-server baseline and CPU
architectures are defined above. “Cross-platform” is complete only when the packaged
artifacts pass this matrix; compiling shared source or checking package definitions on
macOS is not the release criterion.
