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
- A Curve instance can be limited to part of the picture. Select an instance and open
  its MASK section, which sits inside the instance between its red rules, and pick
  Linear or Radial. Picking one places a mask at once and opens it on the picture:
  Linear starts as a sky grad, full at the top edge of the picture and gone by the
  middle, and Radial as a soft ellipse in the center. Drag a handle to move it, or
  press on empty canvas to lay the shape again; Return keeps it, Esc puts everything
  back as it was, and Space pans. The section is one row of icons, the four shapes and
  then Adjust on image (a crosshair), Invert and Remove, with Transition, Core, Aspect
  and Angle under it. An instance that has a mask shows a mask icon
  beside its opacity, red while the mask is on; click it to switch the mask off and on
  without losing the shape. Masks follow the photograph through crop, rotation and
  export. The point sampler reads the masks where you click.
- A Curve instance's mask can also be limited to a range of tones, on its own or inside
  a shape. The MASK section has a TONE switch with the same ruler, Shadows, Midtones
  and Highlights presets, Inv, and Edge and diffusion that Dodge & Burn's tone mask has.
  A tone on its own is a luminosity mask across the whole frame. When a mask has both a
  shape and a tone, Join says how they combine: Intersect, Subtract or Add. The range
  reads the picture as it stood before Dodge & Burn, so burning never moves it, and
  switching the tone off keeps its range.
- A Curve instance's mask can be painted. Round and Card on the MASK row give the
  instance a painted mask and put Dodge & Burn's brush in your hand: drag to paint
  where the curve applies, Option-drag to erase, Shift-click for a straight pass, and
  the bracket keys for the brush's radius and feather. A painted mask starts empty,
  so the curve applies nowhere until it is painted in, as a Dodge & Burn layer does.
  Radius, Feather, Aspect, Angle and Opacity are in the MASK section. Picking the
  other nib keeps what you have painted. A painted mask joins a tone range as a
  Linear or Radial one does, and can be inverted.
- A tone range can be sampled from the picture, in the Curve's TONE section and in
  Dodge & Burn's TONE MASK alike. The eyedropper at the end of the Shadows, Midtones
  and Highlights row stays armed while you work. Click a tone to set the range around
  it, one zone wide with one-zone shoulders. Drag a box to span the tones inside it,
  leaving out the brightest and darkest tenth so a stray highlight does not stretch
  the range. Hold Shift to add to the range instead of replacing it. Return, or the
  eyedropper again, keeps the range; Esc puts it back as it was; Space pans. It reads
  the same image the range selects from, regionally when the range is edge-aware, so
  what you click is what gets selected. In Dodge & Burn, leaving the eyedropper
  gives the brush back.
- A tone range can be an island: only the tones connected to where you click, instead
  of every part of the picture that has them, so a range set on the sea leaves out the
  rock of the same gray above it. Island is a switch at the end of the L1 to D4 row,
  in the Curve's TONE section and in Dodge & Burn's TONE MASK alike. Turning it on arms
  the eyedropper. Click the picture to set the range around that tone and select what
  is connected to that spot; Shift-click another spot to add it, up to eight; Option-
  click a seed to take it away. The seeds show on the picture while the eyedropper is
  armed, and the switch shows how many there are. The ruler is the tolerance: narrow
  the range and the island stops sooner. Two areas that only touch along a thin strip
  count as separate, so an island does not leak through a sliver. An island follows
  the photograph through crop and rotation, and is worked out again when exposure
  changes. Switching Island off keeps the seeds.
- A mask can be seen on the picture, for a Curve instance and for a Dodge & Burn
  layer alike. The Curve's MASK header and Dodge & Burn's TONE MASK heading have the
  same two switches: "overlay" lays red film over what the mask protects and leaves
  what it applies to clear, as Photoshop shows a layer mask, and "view mask" shows
  the mask alone in place of the picture. They are one choice, since the picture
  shows one mask at a time. With neither on, a mask still shows itself while you
  shape it and goes when you let go: dragging a shape's or a gradient's handle on
  the picture shows the overlay, and dragging a handle on the tone ruler shows the
  mask alone. It works before a Curve instance has a curve or a layer has a stroke,
  and with the module bypassed.
- The / key turns the mask overlay on and off: red film over what the mask protects,
  the same switch as "overlay" in the Curve's MASK header and Dodge & Burn's TONE MASK
  heading. It shows the mask you are working on: the brush's layer while the brush is
  open, otherwise the selected Curve instance's mask, or the selected layer's.
- The Curve editor's histogram and Average are taken inside the selected instance's
  mask. The histogram behind the graph shows the tones the curve actually reaches, a
  red band along its foot marks where most of them lie, and the readout says, for
  example, "Average: 0.42 EV darker inside the mask (38% of the frame)". Instances
  above the selected one are counted where their own masks lie.
- The Curve's point sampler has a "limit to tone" option beside it. With it on, the
  click that places a point also limits the instance to the tones around the one you
  clicked, so dragging the new point moves only those tones. The range it sets is in
  the MASK section, which opens to show it.
- Photographs with a Curve mask, a tone island on a Dodge & Burn layer, or a Dodge &
  Burn stroke painted with this version's brush are saved as sidecar version 19.
  monopro 0.2.1 and earlier will refuse to open or overwrite them, so their edits are
  not lost. Every other photograph is still saved as version 18.
- A start page: with no folder chosen, Lightbox shows the monopro mark, a way into
  the folder tree and your ten most recent folders, and Develop with no image open
  shows Open Image and your ten most recent images. Both link to Settings and the
  keyboard shortcuts, over a field of the raw sensor's own mosaic.
- A Home button at the right end of the title strip, also Option-H. In Lightbox it
  clears the folder and shows the start page; in Develop it lays the start page over
  your open tabs without closing any. Home again, Esc, or clicking a tab goes back.
- Clicking the empty space under the Lightbox folder tree clears the folder and
  shows the start page.
- Each Dodge & Burn layer has a Mask button beside its opacity. It turns the
  layer's tone mask on and off, and shows red while it is on, so you can see which
  layers are limited to a tonal range without opening them.
- Ratings and color labels work in Develop. Command-1 to Command-5 and Shift-1 to
  Shift-5 rate and label the open picture, and the same key again takes it off. The
  title strip shows the stars and the label's dot after the exposure.
- Shift-F brings the Lightbox FOLDERS panel forward, from Develop as well.
- Scrolling can pan the picture instead of zooming it: Settings, Controls, Scrolling,
  Pans. Two fingers on a trackpad then move the picture around, in Develop and in the
  compare grid, and a pinch or Command-scroll zooms.
- A pinch on a trackpad zooms the picture in Develop and the compare grid, whatever
  scrolling is set to.
- Lightbox panels can be closed and popped out, as Develop's can. A panel's bar, or
  the tab strip for the panel that is showing, has a pop-out button and an x at its
  right end. A closed panel comes back from the Window menu, its key, or Tab twice; a
  popped-out panel goes back with the button on its window or by closing the window.
  The grid itself stays put.
- The arrow keys move through the folder tree once a folder has been clicked. Up and
  Down open the folder above or below, Right expands a folder or steps into it, and
  Left folds it or steps out to its parent. Clicking a tile gives the arrows back to
  the grid. Whichever has them draws its selection in ruby and the other in gray.
- Opening a module near the bottom of the Develop panel scrolls the panel smoothly, just
  far enough to show the whole module, without pushing its name off the top.
- With the Curve's point sampler armed, the graph shows where a point would land: a
  hollow ring on the curve, with a faint line down to its input, follows the tone
  under the pointer on the picture until you click.
- Each row in the start page's Recent Folders and Recent Images shows an x under the
  pointer. Click it to take that row off the list; the folder or file itself is not
  touched.

Faster
- Dragging a point or an opacity in the Curve is several times faster, most of all
  with many instances.
- Clicking from tile to tile in Lightbox is immediate. A tile is selected as you press
  it rather than when you let go, and with the METADATA panel showing, the grid no
  longer stops while the panel reads the file: a DNG took about a third of a second.
  The panel shows the last picture's rows faded until the new ones arrive, and a
  picture already looked at shows at once.

Changed
- The brush's Feather now means how much of the brush fades, as in Lightroom. A dab is
  at full strength inside the cursor's dashed inner circle and fades smoothly to
  nothing at the outer circle; Feather 0 is a hard edge and 1.0 fades from the middle
  all the way out. Before, a higher Feather painted a smaller mark, and at 1.0 the
  paint had faded out by half the brush. This is the same brush for Dodge & Burn and
  for a painted Curve mask. Strokes saved before this change keep the edge they were
  painted with, so prints already made look as they did. The brush now starts at
  Feather 0.60.
- Tone masks, in Dodge & Burn and in the Curve, have eight new presets under Shadows,
  Midtones and Highlights: L1 to L4 and D1 to D4, approximating Tony Kuyper's Lights 1
  to 4 and Darks 1 to 4. They are soft masks with no flat top, and each step selects a
  narrower part of the picture than the last.
- Dodge & Burn's shape picker has larger buttons, each with an icon beside its
  name: Round, Card, Linear and Radial, spread evenly across the panel. The area
  that holds them and the Dodge and Burn buttons is now a card like the modules
  above it.
- Icons throughout the app are drawn a little larger inside their buttons, so their
  detail is easier to see. The buttons themselves are the same size.
- The Curve's point sampler is an icon directly above the graph, without the "Point
  sampler" label, and its cursor is drawn in dark and light so it shows on a bright sky.
- Dodge & Burn's "view mask" shows a Linear or Radial layer's whole mask, the
  gradient through its tone mask, where it used to show the tone mask alone. A
  brushed layer still shows its tone mask. It also works on a layer with no strokes
  yet, which it did not before.
- Black corr. reads in L*: the lightness below which tones go to black, or, negative,
  the lightness black is lifted to, as the picture shows it at the current exposure.
  The slider is finer near zero, where a print's black point sits, and reaches L* 29
  either way instead of almost middle gray. Edits made before look exactly as they did.
- The mask overlay is solid red over what the mask protects. Before, a quarter of the
  picture showed through, which turned the red pink over a bright sky or a pale wall and
  made untouched areas look painted. Now the picture shows only as darker and lighter
  red, and a lighter, pinker film means the mask is partly there. This is the same
  overlay for Dodge & Burn and for a Curve mask.
- A new dodge or burn instance is now Command-Shift-D or Command-Shift-X while the
  brush is open, and Command-D duplicates the tab even then.
- The keyboard shortcuts reference lists Shift-S and Shift-M under Navigation, with
  the new Shift-F.
- "Use the folder tree to start" on the Lightbox start page is plain text now rather
  than a row that lit up like a link.
- Check for Updates in Settings, About answers right there: the version you have,
  then "checking", "up to date", or the version on offer with a Show Update button.
  It no longer opens the update sheet unless there is an update to show. The
  "Zoom with the scroll wheel" switch is now the Scrolling menu's Zooms and Does
  nothing.

Fixed
- A tool open on the picture no longer blinks out, or misses a press, in the moment
  tone masks are being prepared after a change.
- The print loupe shows toning. It was always gray; it now shows the toned print in
  color, Before and After both, matching the picture around it.
- Developed thumbnails in Lightbox keep their toning when GRAY is on. GRAY now grays
  only the camera's own color thumbnails. The contact sheet's Developed preview keeps a
  developed picture's toning the same way.

## 0.2.1 — 2026-09-26

This release fills out the Lightbox Metadata pane with the IPTC fields a caption
desk expects, carries the camera's EXIF into exports, redraws the Info panel's
Pipeline as a single line, and keeps software updates inside the monopro window.
Sidecars move to version 18; monopro 0.2.0 still opens them but will not save over
them.

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
- Shift+S brings up the Lightbox Search panel with the cursor in the search field,
  ready to type, and Shift+M brings up the Metadata panel. From Develop, both switch
  to Lightbox first.
- Command+A selects every image in the Lightbox grid. In a text field it still selects
  the text.

Changed
- The Info panel's Pipeline reads as one line from the camera to the file: every
  stage sits on a single rail with what it does beside it, in the order it runs. A
  stage that is off stays on the line, drawn hollow, rather than disappearing. It
  now lists Exposure, Contrast Mask and Dodge & Burn under Screen, and Crop under
  Print. Capture gives the sensor's megapixels and format, Screen its pixels, and no
  size is repeated. The Inspector above it is shorter to give it room.
- The Contact Sheet button in the Lightbox footer has its own icon, and a folder
  expanded in the Lightbox folder tree shows as an open folder.
- Print Loupe folds itself away when the loupe is turned off.
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
- The Print Loupe's outline is white while it shows Before (grain and sharpening
  off) and amber while it shows the print, so the two are never confused.
- Software updates stay inside the monopro window. Check for Updates, the update
  offer, download progress, "up to date" and update errors all appear in the
  Software Update sheet, which can also cancel a check or download in progress.
  Restart now can install an update that has not been downloaded yet.

Fixed
- Checking for updates no longer opens a separate system alert on top of the
  Software Update sheet.
- The Software Update sheet no longer offers Update on quit, Restart now and Skip
  this version when there is no update to act on.
- Squeezing Develop off the left edge no longer squeezes the image after it and
  hands the whole window to the right-hand panels.
- A panel brought back from the edge returns at no less than its default width,
  rather than at the narrower width the squeeze left it at.
- Double-clicking just beside a panel's edge, on its scroll bar for instance, resets
  the panel to its default width rather than sending it to half the window. This
  applies in Develop and in Lightbox.
- Toggling Unity WB no longer blanks the picture and collapses the histogram and Info
  panel while the image is re-derived. The previous image stays up until the new one
  is ready.
- In Settings, Lightbox, Backgrounds, the Panel and Tiles sliders no longer show as
  changed when they are at their defaults, and their reset now clears the mark. The
  default moves from 11.8 to 12, a value the slider can hold.

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
