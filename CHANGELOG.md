# Changelog

User-visible changes, newest first. Every change a person using monopro would notice
gets a line under **Unreleased** in the same commit that makes it. Internal refactors,
tests and CI do not.

To release: rename **Unreleased** to the version and date, start a fresh
**Unreleased** above it, and copy the section's text into the Sparkle notes file
`monopro-<version>-macos.txt` (see [packaging](packaging/README.md#auto-update-sparkle-stable-channel)).
The notes are plain text, so write entries that read correctly without Markdown:
no links or code spans in the bullets themselves.

Group entries under the headings the release notes use (New, Changed, Faster, Fixed, Cameras),
and leave out any heading with nothing under it. Known limitations that are not
changes belong in [docs/known-limitations.md](docs/known-limitations.md).

## Unreleased

New
- Lightbox panels tuck off the edge of the window the way Develop's do: drag a
  side column past its minimum width, and click the strip it leaves to bring it back.
- The Metadata pane has buttons under the template menu to step to the previous or
  next image, and Copy to Next, which copies every IPTC field to the next image and
  moves to it. Date Created stays with its own frame.
- New IPTC fields: Keywords, Digital Source Type (a menu of IPTC's own terms),
  Copyright Owner and Contact Email, and under More: Alt Text (Accessibility), Person
  Shown, Description Writer and Data Mining (a menu of the PLUS terms, including the
  AI training prohibitions). Copyright Notice has a menu of standard notices (All
  rights reserved, the Creative Commons licenses, CC0) that fill the field and stay
  editable.
- Entering a Creator fills in Copyright Owner and Credit Line when they are empty,
  and keeps them in step when the Creator changes, unless you have written something
  else there.
- Date Created shows the camera's capture time when nothing else has set it, and is
  written into exports.
- Exports can carry the camera's EXIF: camera, lens, exposure, ISO, focal length and
  capture time, in TIFF, PNG and JPEG alike. On by default, under Settings, Export,
  Behavior, Include camera EXIF. Location, serial numbers and the owner's name are
  never copied.
- The edited mark on Lightbox tiles can be drawn in any of the Dixon China Marker
  colors, under Settings, Lightbox, Edited mark color. Dixon Yellow 73 is the new
  default, a touch warmer than the old yellow.

Changed
- The Metadata pane lists IPTC fields in caption-desk order: Description, City,
  State, Country, Keywords, Digital Source Type, Creator, Copyright Owner, Copyright
  Notice, Credit Line, Contact Email, Date Created. Title, Headline, Source and
  Instructions are under More, with the new fields there.
- Creator is also written as IPTC's Image Creator, so licensing tools find it.
- The Lightbox sidebar starts at the same width as Develop's Info panel. Saved
  Lightbox panel widths are reset once by this update.
- Metadata templates never save or apply Date Created.
- Sidecars are now version 18. monopro 0.2.0 declines to overwrite one rather than
  save it without the new IPTC fields; sidecars from 0.2.0 open here as before.
- The footer is a little taller, with larger buttons, stars and label dots.
- Hotkeys are now called keyboard shortcuts: the overlay on the period key is
  Keyboard Shortcuts, and the list in Settings, Controls is headed Keymap.
- The open file's tab name is plain text rather than red; the lighter tab still marks
  which file is in front.
- American spelling throughout: Center in contact sheet captions, Digitized in
  Digital Source Type, canceled in update messages.

Fixed
- Squeezing Develop off the left edge no longer squeezes the image after it and
  hands the whole window to the right-hand panels.
- A panel brought back from the edge returns at no less than its default width,
  rather than at the narrower width the squeeze left it at.
- Double-clicking just beside a panel's edge, on its scroll bar for instance, resets
  the panel to its default width rather than sending it to half the window. This
  applies in Develop and in Lightbox.

## 0.2.0 — 2026-09-25

This release separates masters from proofs and lets Lightbox notice files that
arrive in an open folder. Exporting works differently: a master is now always a
16-bit TIFF, and the proof settings have moved from Settings into the EXPORT
module in Develop.

New
- monopro runs from the command line without opening a window. monopro render
  writes each raw as its sidecar describes it, exactly as the Export buttons would,
  as a master or a proof, one file or a whole folder. monopro info prints what a
  raw is and what its sidecar holds. Run monopro help for the options.
- Settings can set the Panel and Module backgrounds beside the viewer background,
  with a switch to have panels follow the viewer. Lightbox has its own Viewer
  background, Panel and Tiles values, which match Develop's by default.
- Text and controls turn dark on a light background, so every background setting
  stays readable. Pictures, tone ramps and colours you picked are left as they are.
- Dragging a side panel past its minimum width tucks it off the edge of the window,
  leaving a strip that brings it back.
- Lightbox notices files added to or removed from the open folder while it is open:
  it looks again whenever you come back to the window or switch into Lightbox,
  without reloading the thumbnails already shown or losing your selection and
  place. Cmd+R refreshes on demand and says what changed, and re-reads the folder
  tree too.

Changed
- Masters and proofs are defined by format. Export Master always writes a 16-bit
  uncompressed TIFF; there is no depth or compression to choose. Anything else is
  a proof: a PNG or JPEG, at full resolution or a half, third or quarter of it.
- The proof's format, depth, color space, size and dither are set in the Develop
  EXPORT module, beside the buttons, instead of in Settings. The Info panel's
  buttons are now Export Master and Export Proof.
- Dither is split in two. The screen's is a Viewer setting (Dither the screen),
  and an 8-bit proof has its own switch in the EXPORT module. A master is never
  dithered. The old per-photo dither switch is gone, and photos that had it off
  dither on screen again.
- Settings → Export names the suffixes Master suffix and Proof suffix, and keeps
  one Master color space; the PNG and JPEG color space rows are gone.
- Tab-bar arrows step one tab at a time instead of a third of the bar.
- Shorter Settings labels, with redundant captions removed. Settings sliders use
  the same handles as Develop.

Fixed
- Histogram fills sit cleanly under their outline, without jagged edges or flicker.

## 0.1.1 — 2026-09-23

This release adds automatic updates on macOS. From now on, monopro checks for a
new version once a day and shows a badge in the title strip when one is ready.
Settings > About has the preference and a manual check.

Faster
- Contrast Mask is much faster at 100% and higher zoom, and no longer fails at
  the highest zoom levels.
- Moving the cursor no longer redraws the whole image.
- Exposure and mask edits no longer run an unneeded blur before rendering.
- Histograms rebuild only when their own inputs change.

Fixed
- Rotating in Lightbox now turns the crop with the picture.
- Quick Look shows a rotated frame turned straight away.
- Compare leaves out modules that were switched off when a snapshot was taken.
- Framed proofs are held to the same size limits as full exports.
- Up and Down arrows now nudge every numeric field.
- Dodge & Burn tonal selection matches the render with Contrast Mask on.
- Undo while typing in a text field undoes the text, not the photograph.
- PNG export with non-Latin captions no longer fails.
- JPEG export now includes the requested metadata.

Cameras
- Updated RAW decoding (rawler 0.8), with support for newer Bayer cameras.
  Older Panasonic RW2 files now use the correct black level, so their deepest
  shadows render slightly darker.
- Fujifilm X-Trans cameras (X-series) are not supported in Develop yet; Lightbox
  shows their previews.

## 0.1.0 — 2026-09-16

First public development release, for macOS (Apple Silicon and Intel).
