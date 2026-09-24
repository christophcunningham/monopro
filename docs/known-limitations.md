# Known limitations

Things monopro does not do, or does not do yet, written down so they are stated
rather than rediscovered. This is also the source for the website's "what it
isn't" page.

**Record a limitation here rather than deleting the check that exposes it.** When a
platform check, test or smoke-test item turns out to be something monopro cannot
fix (an OS behaviour, a missing driver feature, an upstream decoder gap), keep the
check, mark it as expected, and add an entry below that says what happens, why,
and what would change it. When a limitation is lifted, remove its entry in the same
commit and add a line to [CHANGELOG.md](../CHANGELOG.md).

## By design

These are scope decisions, not gaps.

- **Monochrome only.** Every stage up to the print path works on one scene-linear
  channel. Colour enters only through Toning, after the monochrome processing.
  There is no colour editing mode.
- **Folders and sidecars, no catalog.** Lightbox browses the file system directly,
  and each edit lives in a `.mono.xmp` beside its raw. There is no library database
  to import into or keep in sync.
- **Linux ships as an AppImage, not a sandboxed package.** Removable drives,
  network mounts and arbitrary photo folders are core workflows, and a Flatpak or
  Snap sandbox would get in the way of all three.

## Cameras and files

- **Develop opens 2×2 Bayer sensors only.** Fujifilm X-Trans bodies (X-series,
  such as the X-T5 or X-T30 III) are not supported in Develop; Lightbox browses
  them from their embedded previews. Fujifilm GFX bodies use Bayer sensors and are
  supported. Any other non-Bayer layout is treated the same way. Lifting this needs
  an X-Trans demosaic path in `raw-core`.
- **Camera coverage follows rawler.** A body rawler 0.8 cannot decode does not open
  in Develop, and a file without an embedded preview shows a placeholder in
  Lightbox rather than a thumbnail.
- **Nikon High Efficiency★ NEF is not decoded.** rawler 0.8 refuses NEF files
  recorded with High Efficiency★ compression, a raw option on recent Z bodies, so
  they fail in Develop and in `monopro render`. Plain High Efficiency is untested.
  Conventionally compressed NEF, such as the D850's, decodes.

## Lightbox

- **An open folder does not notice new or deleted files.** A folder's contents are
  read when it is opened or first expanded in the tree. Files copied in afterwards
  appear only after opening the folder again. Edit badges are the exception: they
  are re-checked on switching into Lightbox and whenever the window comes back to
  the front.

## Toning

- **Toning coefficients are set by eye, not measured.** The model of material
  conversion and optical density is physical, but its coefficients have not been
  fitted to measured prints.

## Platforms

- **Only macOS has been released or exercised.** Windows and Linux build and pass
  CI's compile, lint and non-display tests, but no packaged build has been run on
  either. The first-release matrix and what still has to be verified are in
  [cross-platform-release.md](cross-platform-release.md).
- **The macOS build is not notarized.** The first launch needs **Open Anyway** in
  System Settings → Privacy & Security. Updates installed by the app afterwards do
  not. Lifting this needs a Developer ID; the `./package --signed` path already
  exists.
- **Automatic updates are macOS only.** Windows and Linux compile against an inert
  stand-in and never check for updates.
- **No native menu bar on Windows or Linux.** Every shortcut is handled inside the
  window instead. An attached native menu is optional release work.
- **x86-64 only on Windows and Linux.** ARM builds of either are outside the first
  release.
