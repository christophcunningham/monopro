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

## The brush

- **Strokes painted before Feather meant the width of the fade keep their old edge.**
  That edge narrowed as Feather rose, so those strokes are smaller and softer than the
  same settings paint now. A layer painted partly before and partly after this change
  shows both edges, and nothing converts the old strokes; repaint them to change them.
  This is so that prints already made do not change when the app is updated.

## Curve masks

- **The Develop RGB histogram and the Frame module's Optical placement ignore Curve
  masks.** They read the curve through a table that has no position to read a mask
  at, so they show each masked instance at full strength, as though it applied
  everywhere. The Curve editor's own histogram and Average do not: they are taken
  inside the selected instance's mask, with the instances above it weighed where
  their masks lie.
- **The Curve editor's histogram inside a mask is an estimate from 50,000 samples**,
  a quarter of what the unmasked one uses, so that it can follow a handle as it is
  dragged. A mask that selects a very small part of the frame has few samples in it,
  and its histogram is correspondingly rough.
- **A brushed Dodge & Burn layer's overlay and "view mask" show its tone mask, not
  where it has been painted.** They show where painting can reach. A Linear or
  Radial layer shows its whole mask, the gradient through its tone mask. To see what
  has been painted, hold Shift-D or Shift-X for the dodge and burn maps.
- **`⇧`-click with the tone eyedropper does nothing on a range that is still open.**
  A range open at both ends already includes every tone, so there is nothing to add to
  it. Click without `⇧` to start a range, then `⇧`-click to widen it.
- **Where a tone range's edge falls is worked out at the proxy's resolution, 480 pixels
  on the long edge.** The range is cut at every pixel, so its edge is smooth at any
  zoom, but the tones it is cut from are averaged over a proxy pixel. Against a hard
  edge in the picture, the mask's edge can sit up to one proxy pixel away from it,
  about 16 pixels on a 40 MP negative, and shows as a soft rim of the adjustment along
  that edge. Lifting this needs the edge placed from the full-resolution picture.
- **The tone eyedropper samples at the proxy's resolution, 480 pixels on the long
  edge.** That is the image a tone range reads, so the sample is what the mask will be
  compared against, but a subject only a few pixels across in a large frame is averaged
  with its surroundings. The Inspector's sample-area setting does not apply to it.
- **A tone island follows connected tones, not subjects.** It grows from each seed
  through neighboring pixels the range selects at half strength or more, and stops
  where the tone leaves the range. Two things of the same tone that meet along a wide
  edge are one island; narrow the range on the ruler, or turn off Edge-aware so the
  range reads pixels and not regions, to separate them. There is no subject detection.
- **A tone island does not cross a connection narrower than about one percent of the
  frame**, so it does not leak through a sliver. The cost is that a subject whose parts
  are joined only thinly, a rock face divided by its cracks, is selected one part at a
  time: `⇧`-click the next part to add it. Seeds only add; nothing takes a part of an
  island away except narrowing the range.
- **A tone island is worked out at the proxy's resolution, 480 pixels on the long
  edge**, like the range it limits. A gap or a subject smaller than a pixel of that is
  not seen, and the island's edge is as soft as the proxy makes it.
- **A tone island is slower to follow than a plain range.** Working one out takes 7 to
  8 ms where a range takes under half a millisecond. It happens off the frame loop, so
  the interface does not stall, but the mask trails a dragged ruler handle by about a
  frame.
- **An island holds eight seeds.** `⇧`-click past that says so and adds nothing. A
  second Curve instance, or a second layer, takes more.
- **A seed stays where it was placed when the range moves.** If the ruler is moved, or
  exposure changes, so that the tone under a seed is no longer in the range, that seed
  selects nothing until it is. With every seed in that state the island is empty, and
  the mask selects nothing.
- **One shape per instance.** A mask is painted (Round or Card), or a Linear, or a
  Radial, and not two of them. A second masked instance is not a substitute, because
  the Curve then applies twice where the two masks overlap.
- **A painted mask has no effect to see until its instance has a curve.** Painting
  shows in the picture as the curve applying. On an instance with no curve yet the
  overlay comes on by itself when the mask is made, so the strokes show as clear film.
- **The Curve editor's histogram reads a painted mask from a picture of it 480
  pixels on the long edge**, so an edge painted with a hard brush is a few pixels
  soft there. The render is exact.
- **A photograph with a Curve mask, a tone island on a Dodge & Burn layer, or a Dodge
  & Burn stroke painted with this version's brush cannot be opened by monopro 0.2.1 or
  earlier.** Those builds would otherwise save it back without the mask, the island
  or the strokes, so sidecar version 19 makes them refuse the file. Every other
  photograph is still version 18. Since every new stroke counts, a photograph dodged
  or burned in this version is a 19.

## Lightbox

- **No live file watching.** The open folder is re-read when the window comes back
  to the front, when you switch into Lightbox, and on ⌘R. Files that arrive while
  monopro stays in front, such as tethered captures, appear on the next of those.
  A file watcher would not see changes another machine makes on a network volume,
  which is where it would matter most.
- **Quick Look shows the camera's preview.** Space opens the camera's embedded JPEG
  full-frame, not your edit, so a developed or toned picture looks as it was shot
  there. The grid's developed previews are the place to see edits in Lightbox.

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
- **Installing an update can still show system windows.** Every update prompt is in
  monopro's own sheet, but the install itself runs in Sparkle's installer after
  monopro quits. If monopro's folder is not writable by the current user, macOS asks
  for an administrator password, and a slow install may show Sparkle's own progress
  window until the new version opens.
- **No native menu bar on Windows or Linux.** Every shortcut is handled inside the
  window instead. An attached native menu is optional release work.
- **x86-64 only on Windows and Linux.** ARM builds of either are outside the first
  release.
