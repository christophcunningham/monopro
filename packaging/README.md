# Native release packages

These definitions produce one native artifact per supported desktop operating system.
They are deliberately host-native: a package is built, signed and tested on the OS that
will run it.

| Host | Artifact | Architectures | Minimum release baseline |
|---|---|---|---|
| macOS | signed and notarized DMG | Apple Silicon + Intel | macOS 11.0 |
| Windows | signed Inno Setup EXE | x86-64 | Windows 11 25H2 (build 26200) |
| Linux | AppImage | x86-64 | Ubuntu 24.04 LTS, Wayland and X11 |

The baselines are packaging inputs, not evidence that the release is ready. Every
artifact still has to pass the real-machine matrix in
[`docs/cross-platform-release.md`](../docs/cross-platform-release.md) and the
[`packaged release smoke test`](../docs/release-smoke-test.md).

Baseline choices were recorded 2026-09-13. Windows 11 25H2 is a serviced Home/Pro
release in Microsoft's [lifecycle table](https://learn.microsoft.com/en-us/lifecycle/products/windows-11-home-and-pro),
and Ubuntu 24.04 LTS has standard maintenance through May 2029 in Canonical's
[release cycle](https://ubuntu.com/about/release-cycle). Revisit these dates before
each public release rather than allowing a package constant to become a support policy
by accident.

## macOS

Run on macOS with both Rust targets installed:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./package                         # local, ad-hoc-signed test package
./package --signed                # public release package
```

`--signed` requires a Developer ID Application identity in
`MONOPRO_MAC_SIGNING_IDENTITY` and a notarytool keychain profile in
`MONOPRO_NOTARY_PROFILE`. It signs the application and DMG, waits for Apple's
notarization result, staples the ticket, then verifies the bundle, disk image,
architectures, metadata and checksum. `--arm64` may be added for a local
single-architecture build; public releases are universal. Each successful run writes a
`.sha256` file beside the DMG.

### Auto-update (Sparkle, stable channel)

macOS builds carry a [Sparkle](https://sparkle-project.org) 2 updater with a
Mole-like surface: a silent daily background check that shows nothing when up to
date, a badge at the right end of the title strip when an update exists, and a
sheet offering *Update on quit* (default), *Restart now* (idle only) and *Skip
this version*. The binding is [`sparkle-updater`
0.1.0](https://crates.io/crates/sparkle-updater), chosen over
`slint-ui/sparklers` because it registers as the user-driver delegate and can
keep Sparkle's own alerts off scheduled checks. It is vendored at
`vendor/sparkle-updater` with one local addition — `skip_current_update`, which
answers Sparkle's pending alert with its own Skip choice so a skipped update
that was already staged for install-on-quit is canceled. Upstream 0.1.0 cannot
reach that reply; `vendor/sparkle-updater/LOCAL-PATCH.md` records the delta and
the private APIs it depends on. Feed hosting is a single stable, unversioned URL
— the `releases/latest/download/appcast.xml` alias of the release assets —
because the URL is baked into each shipped bundle.

Everything Sparkle needs is vendored and pinned in `packaging/macos/Sparkle/`:

- `Sparkle.framework` — 2.9.6, from `Sparkle-2.9.6.tar.xz`, SHA-256
  `52bf9e88cdd972fc0c81501377a880e90d47031bd8ca5462488f843e2609e192`. `./package`
  embeds it in `Contents/Frameworks` (preserving its symlink tree with `ditto`)
  and signs it explicitly before the outer bundle pass.
- `sparkle-bin/` — `sign_update`, `generate_appcast`, `generate_keys` and
  `BinaryDelta` from the same archive, used by `./package` and
  `packaging/macos/appcast`.
- `LICENSE` — Sparkle's MIT licence text.

Sparkle 2.10 and later require macOS 12; 2.9.6 keeps the macOS 11.0 baseline.

Key setup, once per maintainer:

```sh
packaging/macos/Sparkle/sparkle-bin/generate_keys   # EdDSA key into the login keychain
```

The public key it prints goes into `packaging/macos/sparkle-ed25519-pub.txt`
(one line, no whitespace) or the `MONOPRO_SPARKLE_ED_PUBLIC_KEY` environment
variable; the private key stays in the keychain (export it with
`generate_keys -x` only to move machines, and pass that file as
`MONOPRO_SPARKLE_KEY_FILE` on machines where it is imported). `--signed` fails
closed when no public key is available. Local ad-hoc packages without a key
embed `SUEnableAutomaticChecks=false` and never check; the Settings toggle can
still enable a deliberate local test.

Publishing a release:

1. `./package --signed` produces the DMG (for humans) and the notarized ZIP of
   the signed app (the enclosure Sparkle downloads), plus
   `monopro-<version>-macos.zip.ed.sig` — the EdDSA signature over the ZIP
   bytes from `sign_update`.
2. Publish both assets on the GitHub release, together with release notes as a
   plain-text file named after the ZIP (`monopro-<version>-macos.txt`).
3. Extend the feed with `packaging/macos/appcast <archives-dir>
   [--ed-key-file <key>]`: a staging directory holding the new ZIP, its notes
   file, and the previous release's `appcast.xml`. The tool embeds the notes
   into each item, signs enclosures, and prunes versions older than the newest;
   upload the resulting `appcast.xml` as a release asset so the stable alias
   always serves the newest feed. **Every release must be generated with the
   same key and embedded notes** — an item's notes are only embedded when its
   entry is first written.

Installation remains drag-to-Applications from the DMG. Sparkle updates replace
the running application in place after a clean quit; a staged installer never
touches a live session, and a relaunch Sparkle asks for while an export writes
is postponed until the export drains. User settings and caches are intentionally
retained.

## Windows

Run in PowerShell on x64 Windows with the stable Rust MSVC toolchain, Windows SDK
`signtool.exe`, and Inno Setup 6.3 or newer on `PATH`:

```powershell
rustup target add x86_64-pc-windows-msvc
.\packaging\windows\package.ps1 -Unsigned   # local package validation
.\packaging\windows\package.ps1             # signed public release package
```

The signed path requires the certificate's SHA-1 thumbprint in
`MONOPRO_CERT_SHA1` and an RFC 3161 timestamp service URL in
`MONOPRO_TIMESTAMP_URL`. Inno Setup invokes the repository's signing wrapper for the
application, installer and uninstaller. The stable AppId makes installing a newer
version an in-place upgrade. Packaging verifies the x86-64 payload, installer version,
expected signature state and checksum. Each successful run writes a `.sha256` file
beside the installer.

The installer includes font, icon, profile, Sparkle framework, and
`sparkle-updater` notices under `licenses/`.

Windows package builds remap source and user-directory paths before compilation;
the validator rejects executables that still contain those build-machine paths.

The default is a per-user install under the user's Program Files folder, with an
optional installer prompt for an all-users install. Apps & Features removes the
program. Settings and caches are retained across upgrades and uninstall.

## Linux

Run on an x86-64 Ubuntu 24.04 LTS host with `appstreamcli`,
`desktop-file-validate`, FUSE 2 compatibility, and `linuxdeploy` (including its
AppImage output plugin) available:

```sh
rustup target add x86_64-unknown-linux-gnu
sudo apt install appstream desktop-file-utils libfuse2t64
./packaging/linux/fetch-linuxdeploy
export LINUXDEPLOY="$PWD/dist/tools/linuxdeploy-x86_64.AppImage"
./packaging/linux/package
```

The fetch helper pins both the reviewed linuxdeploy commit and its SHA-256. If the
upstream continuous asset changes, the build fails until both are deliberately reviewed
and updated. Packaging extracts the result without FUSE, validates its desktop and
AppStream metadata, checks its x86-64 payload and dynamic links, and writes an adjacent
checksum. `./packaging/linux/verify-containers` additionally checks that payload in
Fedora 44 and current Arch containers when Docker is available. The Arch result covers
userspace ABI risk for Omarchy and CachyOS; it cannot cover their compositor, portal or
GPU behavior.

The build host is part of the ABI baseline: do not build a public AppImage on a newer
distribution. The AppImage carries the executable, desktop entry, icon and GPL license;
`linuxdeploy` gathers non-base shared-library dependencies. It intentionally does not
sandbox the application because monopro must browse local, removable and network
filesystems.

Make the AppImage executable and run it in place. Upgrade by replacing the file;
uninstall by deleting it and removing any launcher integration created by the user's
desktop integration tool. Settings and caches are retained.

## Hosted automation

`Native CI` compiles, lints and runs non-display tests on pinned macOS, Windows and
Ubuntu hosted runners for every push and pull request. `Package candidates` is a manual
workflow with a platform selector: choose one platform or `all`. It runs the package
validators and retains the artifacts for 14 days. Windows artifacts also include
the matching source archive, its checksum, and the source commit identifier.

Candidate workflow artifacts are not public releases. GitHub's Windows Server runner is
not Windows 11 25H2, hosted runners do not provide the required interactive desktop/GPU
coverage, and signing credentials are intentionally absent. Run the documented smoke
test with the exact candidate checksums, then rebuild signed/notarized artifacts from
the tested commit.
