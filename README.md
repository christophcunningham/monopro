<h1 align="center">monopro</h1>

<p align="center">
  <strong>A linear-light black-and-white darkroom for RAW — from the sensor's raw photon count to an inkjet-ready plate.</strong>
</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-0.1.0-7c5cff">
  <img alt="Platform" src="https://img.shields.io/badge/platform-macOS%2012%2B%20·%20Apple%20Silicon-2a9d8f">
  <img alt="License" src="https://img.shields.io/badge/license-GPLv3-blue">
</p>

monopro is a free, open-source macOS application for **black-and-white RAW
processing and fine-art printmaking**. It decodes your RAW file with *no* tone
curve, *no* auto-brightness, and *no* aesthetic guesswork — what comes out of
LibRaw is the sensor's actual photon count, proportional to scene luminance. A
surface that reflects twice as much light produces a value exactly twice as
large.

Everything after that — channel mixing, enlarger character, exposure, contrast
grade, dodging and burning, historic toning, silver-grain simulation, output
sharpening — happens in **linear light**, the way light actually behaves. The
perceptual L\* encoding is applied only once, at export, so the file you hand to
your printer is as faithful to the scene as the camera allowed.

> [!NOTE]
> **monopro is a darkroom, not a filter pack.** The mental model is the
> enlarger and the wet print: the RAW is your negative, the controls are your
> enlarger head and your paper grade, and the export is the print. Every control
> below maps to a physical step a printer would recognise. Pressing `/` in the
> app opens an **About & Workflow** panel that documents every control in
> context — that panel is the real manual; this README is the map.

---

## Table of contents

- [What you'll need](#what-youll-need)
- [Download & install](#download--install)
  - [The one-line install](#the-one-line-install)
  - [What the setup script does](#what-the-setup-script-does)
  - [Running from source manually](#running-from-source-manually)
  - [The eciRGB v2 profile](#the-ecirgb-v2-profile)
  - [Rust extensions (optional acceleration)](#rust-extensions-optional-acceleration)
- [The pipeline](#the-pipeline)
- [Develop controls](#develop-controls)
- [Dodge & Burn](#dodge--burn)
- [Grain](#grain)
- [Toning](#toning)
- [Output sharpening](#output-sharpening)
- [The viewer](#the-viewer)
- [Test Strip](#test-strip)
- [Snapshots & Compare](#snapshots--compare)
- [Lightbox](#lightbox)
- [Export](#export)
- [Presets](#presets)
- [Preferences](#preferences)
- [Hotkeys](#hotkeys)
- [Supported cameras](#supported-cameras)
- [For developers](#for-developers)
- [License](#license)
- [Acknowledgements](#acknowledgements)

---

## What you'll need

monopro is built for a specific, deliberate setup:

1. **A Mac** — Apple Silicon, macOS 12 (Monterey) or later. The optional Rust
   extensions use Metal GPU acceleration via `wgpu`.
2. **Python 3.9** — specifically 3.9. The Rust extensions are compiled against
   the Python 3.9 ABI, so other minor versions will fail at the build step. The
   setup script checks this and installs `python@3.9` via Homebrew if needed.
3. **A RAW file** — DNG, ARW, NEF, CR2/CR3, RAF, RW2, 3FR, or IIQ. Any 8- or
   16-bit TIFF opens directly too.
4. *(Optional)* **The eciRGB v2 ICCv4 profile**, if you plan to export toned
   (RGB) prints. It's a free download from [eci.org](https://www.eci.org) and is
   only required when toning is active — see
   [The eciRGB v2 profile](#the-ecirgb-v2-profile).

For printmaking, monopro pairs naturally with a fine-art inkjet workflow: a
pigment printer, the **monostar** grayscale profile (generated at runtime, no
download needed), and your own paper.

---

## Download & install

### The one-line install

```bash
git clone https://github.com/christophcunningham/monopro.git
cd monopro
chmod +x setup_monopro.sh && ./setup_monopro.sh
```

When it finishes, `monopro.app` is built and the project folder opens in Finder.
**Drag `monopro.app` into `/Applications`** and you're done — the app is
self-contained (it carries its own Python environment and the compiled Rust
extensions inside the bundle), so it runs without Terminal from then on.

> [!TIP]
> **First launch on a Mac that didn't build the app?** macOS may warn that the
> developer can't be verified. The setup script ad-hoc-signs the bundle and
> clears the quarantine flag, but if you receive a built `.app` via AirDrop or
> zip, **right-click `monopro.app` → Open**, then click **Open** in the dialog.
> If that still won't clear, run once in Terminal:
> ```bash
> xattr -dr com.apple.quarantine /Applications/monopro.app
> ```

### What the setup script does

`setup_monopro.sh` is idempotent — safe to re-run. In one pass it:

1. **Checks for Python 3.9** (installs `python@3.9` via Homebrew if missing) and
   warns if a different minor version is active.
2. **Creates a virtual environment** at `./venv` and installs every Python
   dependency (rawpy, NumPy, Pillow, SciPy, PyQt6, exifread, colour-science,
   maturin).
3. **Installs the Rust toolchain** via rustup if `cargo` isn't found, then writes
   the macOS linker flags (`-undefined dynamic_lookup`) the PyO3 build needs.
4. **Builds both Rust extensions** (`monopro_sharpen` and `monopro_core`) with
   maturin and installs them into the venv.
5. **Installs the eciRGB v2 profile** to `~/Library/ColorSync/Profiles/` if it
   finds it in the project folder.
6. **Converts the app icon** to `.icns` and **builds `monopro.app`** — a fully
   self-contained bundle with the venv, the extensions, and the ICC profile all
   baked in — then ad-hoc-signs it and clears the quarantine flag.
7. Drops a `monopro.command` double-click launcher for development.

### Running from source manually

If you'd rather skip the bundle and run the script directly:

```bash
pip install rawpy "numpy<2" Pillow scipy PyQt6 exifread colour-science
python3 monopro.py
```

The app runs fully without the Rust extensions — sharpening controls appear but
are disabled, and NumPy fallbacks handle everything else.

### The eciRGB v2 profile

monopro **never silently substitutes** a colour profile. For toned (RGB) export
it requires the genuine ECI-sourced **eciRGB v2 ICCv4** file. It is *not*
redistributable here, so you download it once from
[eci.org/en/downloads](https://www.eci.org/en/downloads) and place it at:

```
monopro/profiles/eciRGB_v2_ICCv4.icc
```

If toning is active and that file is absent, export **hard-fails with a clear
message** rather than guessing — this is by design, so a toned plate is always
backed by the real working space your retoucher's Photoshop expects.

> [!NOTE]
> Grayscale export needs **no download**. The **monostar** profile — an original
> ICC v4.4 grayscale profile with a parametric L\* tone curve, released CC0 — is
> generated at runtime and embedded directly. The Display P3 profile used for
> JPEG proofs is bundled or pulled from macOS ColorSync automatically.

### Rust extensions (optional acceleration)

Two Rust crates (built via [maturin](https://www.maturin.rs/), installed into the
venv as Python packages) accelerate the hot paths. The setup script builds them
for you; to rebuild by hand:

```bash
cd monopro_ext/monopro_sharpen && python -m maturin develop --release
cd ../monopro_core           && python -m maturin develop --release
```

**`monopro_sharpen`** — sharpening, Metal GPU acceleration, grain, and D&B
compositing:

| Function | What it does |
|----------|--------------|
| `sharpen_u16` | À-trous wavelet sharpening on uint16 grayscale |
| `gpu_channel_mix` | Metal-accelerated luminance channel combine |
| `gpu_lut_apply` | Metal-accelerated 65 536-entry tone-curve LUT apply |
| `render_ev_map_rust` | Rayon-parallel EV-map render for dodge & burn |
| `fast_db_composite_rust` | Fused pre-D&B cache + EV composite for interactive painting |
| `grainify_rust` | Crystallographic silver-grain simulation |

**`monopro_core`** — the display pipeline:

| Function | What it does |
|----------|--------------|
| `lstar_u16_to_rgb8` | Rayon-parallel uint16 → L\*-encoded uint8 RGB for the screen |

When present, the GPU functions replace their NumPy equivalents in the hot path —
a real difference on 40 MP+ files.

---

## The pipeline

Everything between decode and export happens in **Linear Rec2020 float32, D65**.
sRGB never appears in the pipeline — only in a JPEG proof export.

```
RAW / DNG / TIFF
  │
  ▼  rawpy / LibRaw   ·   gamma=(1,1) · no auto-bright · highlight blend · output_color=Rec2020
  │                       decode mode: AHD · DHT · DCB · VNG · superpixel
  │                       optional lensfun profile correction (decode-time)
  │
  ▼  Linear Rec2020 float32
  │
  ├─  Composition        crop / rotate (non-destructive quad) + parametric distortion
  │
  ├─  Luminance channel  Weighted (Rec2020) · Equal · Red · Green · Blue · Infrared ~720nm · Emulsion
  │
  ├─  Callier Q          enlarger contrast character (condenser ↔ diffusion)
  │
  ├─  Exposure           middle-gray placement in linear light (±4 EV)
  │
  ├─  Scene Range        dynamic-range window the tone curve maps
  │
  ├─  Dodge / Burn       local EV adjustments — brush, linear, or radial instances
  │
  ├─  Black / White pt   hard input clip
  │
  ├─  Tone mapping       Linear · Multigrade (incl. split grade) · AgX · Photogravure · Custom spline
  │
  ├─  Toe / Shoulder     soft roll-off shaping at the extremes
  │
  ├─  Shadow Lift / Hilite Trim   output density-range compression
  │
  ├─  Invert             plate positive
  │
  ├─  Grain              optional crystallographic AgX emulsion simulation
  │
  ▼  uint16 grayscale
  │
  ├─  Output sharpening  à-trous wavelet (monopro_sharpen) — output-only by default
  │
  ├─  Toning             optional gradient-map of a historic process
  │
  ▼  L* encode  (CIE 1976)
  │
  ├─  Grayscale export:  16-bit TIFF  ·  monostar ICC (L* Gray)
  └─  Toned export:      16-bit RGB TIFF  ·  eciRGB v2 ICCv4
```

Working-space coefficients are BT.2020: **R 0.2627 · G 0.6780 · B 0.0593**. The
LAB readout reports CIE L\*a\*b\* D50. AgX runs scene-referred and bypasses the
0–1 clip; Callier Q sits right after luminance extraction.

---

## Develop controls

### Decode (RAW only)

The demosaic algorithm is a **decode-time** property — it changes how the Bayer
data becomes RGB, so it lives outside the look and is preserved across a
parameter reset (and stored per-image, not in develop presets).

| Mode | Character |
|------|-----------|
| **AHD** | Adaptive Homogeneity-Directed. Balanced default. |
| **DHT** | Maximum fine-detail acutance. |
| **DCB** | Alternative detail rendering. |
| **VNG** | Smoothest, lowest-artifact — good for high ISO. |
| **superpixel** | LibRaw half-size: each Bayer quad bins to one RGB pixel. No demosaic, fast, half resolution. |

Available modes depend on what your installed LibRaw provides; `superpixel` is
always present.

### Lens correction

When **Lens correction** is on, monopro looks up your lens in the **lensfun**
database (via the system `liblensfun`) and applies profile-based geometric,
vignetting, and TCA correction at decode time, on the full-resolution RGB. It's
**off by default even when a profile exists** — the lens is part of the optical
chain, and its character may be intentional. Like decode mode, it's a
decode-time property: preserved across reset, stored per-image, never in a
develop preset.

For lenses lensfun doesn't know, the composition stage exposes a **parametric
Brown-Conrady radial distortion** with **Strength** and **Zoom** sliders.

### Luminance channel

How the three colour channels collapse into one grayscale value.

| Mode | Character |
|------|-----------|
| **Weighted (Rec2020)** | BT.2020 coefficients. Physically accurate. Default. |
| **Equal (R=G=B)** | Flat reference. |
| **Red (filter)** | Red only — brightens warm tones, darkens skies and foliage. Like a deep red filter on the lens. |
| **Green (filter)** | Green only — natural midtone separation; favoured for portraits and landscape. |
| **Blue (filter)** | Blue only — darkens warm tones, lifts sky and haze. |
| **Infrared (~720nm)** | Approximates a 720 nm longpass filter: heavy red bias, near-zero blue. Foliage bright, skies dark. |
| **Emulsion** | Convolves a real emulsion's spectral sensitivity curve with the BT.2020 channel response — see below. |

#### Emulsion / spectral panel

monopro draws an estimated **spectral power distribution** for the open image
from its linear RGB content, and lets you render the grayscale *through a named
emulsion's actual sensitivity curve* rather than fixed channel weights. The
built-in curves are based on published sensitometry:

| Emulsion | Response |
|----------|----------|
| **Panchromatic** | Full visible spectrum, peak ~550 nm. Most modern B&W film. |
| **Ortho Plus** | Orthochromatic — sharp cutoff ~585 nm, no red. Reds dark, sky bright. |
| **Albumen** | Silver-chloride in egg-white binder (~1850–1890). UV-dominant with a blue-green tail. |
| **Salt Print** | Talbot's native silver-chloride response. Peaks ~380 nm, broad tail. |
| **Carbon** | Dichromate sensitizer — broad UV-into-blue with a midtone shoulder. |
| **Pt/Pd** | Ferric-oxalate — peak 350–365 nm UVA, drops sharply by ~420 nm. |

Choosing an emulsion overlays its curve on the spectral plot so you can see
exactly which wavelengths your tones are being weighted by.

### Callier Q

Models the contrast difference between a **condenser** and a **diffusion**
enlarger, applied as a power function to the luminance after channel extraction.
Above 1.0 stretches highlight separation and compresses shadows (condenser);
below 1.0 opens shadows and compresses highlights (diffusion). 1.0 is neutral.
Range 0.70–1.50.

### Exposure

Sets the middle-gray point in linear light, ±4 EV. The most-used control — it
places the tone curve's midpoint relative to the scene.

### Scene Range

The dynamic-range *window* the tone curve maps — how many stops above and below
middle gray the curve sees. Unlike Black/White point (which clip), Scene Range
tells the tone mapping how wide a scene to compress. Wider preserves more
shadow/highlight detail at the cost of local contrast; narrower raises contrast
but lets the extremes clip.

### Black point / White point

Hard input clips applied after Scene Range and before the curve. Black Point
truncates below a floor (cut deep-shadow noise); White Point truncates above a
ceiling (discard blown highlights cleanly). These are destructive — data outside
the clip is gone.

### Tone mapping

| Mode | Description |
|------|-------------|
| **Linear** | No curve. Pure physical luminance. |
| **Multigrade** | Variable-contrast paper simulation, grades 00–5. **Split grade** mode controls highlight and shadow grades independently with a luminance crossover point — exactly like a two-exposure split-grade print. |
| **AgX** | Scene-referred display transform (Troy Sobotka / Blender 4). A log2 sigmoid across 16.5 stops — shadow and highlight detail held simultaneously. |
| **Photogravure** | Ferric-chloride etch simulation with dedicated **Plate Character** controls (below). |
| **Custom** | A user-drawn spline. Drag control points freely. |

#### Plate Character (Photogravure only)

These shape the photogravure curve directly and have no effect in other modes:

- **Shadow Comp** — compresses shadow density (slows the etch in dense areas).
- **Hilite Ext** — extends highlight gradation, holding separation in high values.
- **Midtone Ctr** — adds or removes midtone contrast.

### Toe / Shoulder

Post-curve shaping that softens the roll-off at the extremes *without* flattening
local contrast — it reshapes the transition rate, like choosing a paper with a
longer toe or applying a highlight mask. **Toe** softens entry into the shadows;
**Shoulder** softens the exit into the highlights.

### Shadow Lift / Hilite Trim

Output density-range controls applied after the curve and Toe/Shoulder, both
leaving the midpoint untouched:

- **Shadow Lift** — raises the black floor, like a paper pre-flash. Strongest at
  pure black, tapering to zero at the midpoint. Opens the shadows.
- **Hilite Trim** — pulls the maximum white down with a smoothstep shoulder.
  Softens the highlights.

Use Toe/Shoulder for subtle shaping; use Lift/Trim for stronger flattening or to
target a specific D-min / D-max.

---

## Dodge & Burn

Local exposure adjustments in linear light — multiple named, stackable
instances, each one entirely non-destructive. An instance can be one of three
kinds:

- **Brush** — freehand strokes with adjustable radius, intensity (EV), feather,
  and opacity.
- **Linear** — a single linear gradient (a graduated-filter-style ramp).
- **Radial** — a single radial gradient (a vignette / spotlight falloff).

Each instance has its own master opacity and an eye toggle to include or exclude
it from the render. Hold `⇧X` / `⇧D` to flash the burn / dodge mask overlay.
Brush size, intensity, feather, and opacity are all adjustable live with the
bracket keys (see [Hotkeys](#hotkeys)). Painting is interactive thanks to a
pre-D&B cache and a fused Rust compositor.

---

## Grain

Crystallographic AgX emulsion simulation (Pierre 2023), modelling the emulsion
as a stack of silver-crystal layers — the parameters map to physical emulsion
properties. Grain is applied in linear light after inversion, before the uint16
cast and output sharpening.

| Control | Range | Description |
|---------|-------|-------------|
| Size | 1–20 px | Crystal kernel size. Larger = coarser. |
| Density | 0.05–0.75 | Surface-filling ratio — how much of the emulsion is covered. |
| Layers | 5–60 | Crystal layers. More = denser, more continuous. |
| Variability | 0.0–2.0 | Log-normal size spread. Higher = more organic. |
| Sensitivity | −2.0 to +2.0 EV | Tonal bias — negative weights grain into the shadows, positive into the highlights. |
| Seed | integer | RNG seed. Same seed + parameters = identical grain across renders. |

A **live grain loupe** previews grain at full resolution on a crop without
forcing a full re-render — enable it with the grain preview toggle (press `Esc`
to dismiss it). Grain settings can be saved as named **grain presets** and set
as a default to apply on open.

---

## Toning

Gradient-map toning simulating historic processes. Shadow and highlight colours
are applied independently, each with its own opacity, and a balance slider sets
the crossover.

| Process | Character |
|---------|-----------|
| **Silver** | Near-neutral gelatin-silver reference with micro-cool shadows. |
| **Albumen** | Purplish-brown shadows, warm parchment highlights. |
| **Salt** | Reddish-brown shadows, warm tan highlights. Fox Talbot's process. |
| **Collodion** | Warm-brown shadows, cool off-white highlights. Wet plate / POP. |
| **Selenium** | Eggplant/purple shadows, near-neutral highlights. |
| **Gold** | Blue-black shadows, cool blue-white highlights. |
| **Pt/Pd** | A blend slider — full left is cool platinum, full right is warm palladium. |
| **Cyanotype** | Deep Prussian-blue shadows, pale blue-white highlights. |
| **Custom** | Pick shadow and highlight colours yourself. |

When toning is active, **export switches automatically to 16-bit RGB TIFF with
eciRGB v2 ICCv4 embedded.**

---

## Output sharpening

À-trous wavelet sharpening via `monopro_sharpen`, applied to the uint16 result
after inversion and grain, just before export. It's **output-only by default** —
it does not touch the live preview unless you check *Preview (live)*.

| Control | Range | Description |
|---------|-------|-------------|
| Passes | 0–8 | Iterations. 0 = off. |
| Radius | 0.5–8.0 px | Wavelet band centre — the spatial scale of the halo. |
| Spread | 0.0–3.0 | Scale-space envelope width — how broadly the effect spreads across frequencies. |

| Preset | Passes | Radius | Spread | Use |
|--------|--------|--------|--------|-----|
| None | 0 | — | — | No sharpening |
| Sharp | 2 | 1.0 | 0.5 | Most prints |
| Sharper | 3 | 1.0 | 1.0 | Fine detail, high-frequency subjects |

Requires `monopro_sharpen`; controls show but are disabled without it.

---

## The viewer

- **Output modes** — **Proof** (standard inkjet view, L\* encoded), **Plate
  Simulation** (a raking-light depth render — raised areas warm, recessed cool,
  with adjustable relief depth and light direction), and **Split View** (linear
  input left, processed output right, draggable divider).
- **Colour views** — press `J` to cycle the processed gray view → the embedded
  camera JPEG → the native Linear Rec2020 colour (no processing). Useful for
  judging the camera's own rendering against yours.
- **False-colour exposure map** (`F`) and **clipping overlays** for over- and
  under-exposure (below).
- **100% crop rendering** — at fit-to-view the app shows a fast downscaled
  preview; at 100%+ it fires a full-resolution render of just the visible slice,
  running the whole pipeline at native sensor resolution on that region (capped
  at a maximum megapixel count to keep timing consistent). Panning or zooming
  cancels an in-flight render and restarts once the view settles.
- **L\*a\*b\* pins** (`I` to place, `⇧I` to hide) for sampling exact values.
- **Surround** (`B`) — draws a configurable border around the image to judge it
  against a chosen paper white or tone; colour and width are set in Preferences.

### Clipping overlays

Two independent, simultaneously-active overlays.

**Overexposed (`O`)** — three-tier highlight check:

| Indicator | Colour | Condition |
|-----------|--------|-----------|
| Near-clip | Magenta checkerboard | Any channel ≥ 230 |
| Hard-clip | Red solid | Any channel ≥ 248 |
| Blown | Black solid | All channels = 255 |

**Underexposed (`U`)** — three-tier shadow check:

| Indicator | Colour | Condition |
|-----------|--------|-----------|
| Near-crush | Cyan checkerboard | Luminance ≤ 12 |
| Hard-crush | Blue solid | Luminance ≤ 7 |
| Pure black | White solid | Luminance = 0 |

Priority when both are on: white > black > blue > red > cyan > magenta.

---

## Test Strip

A darkroom-style multi-band comparison. monopro renders the current image
through N pipeline variants side by side, each stepping a single parameter across
a range — exactly like laying a test strip across a sheet under the enlarger.

Toggle it from the develop panel; choose the parameter, step size, band count
(3–9), and orientation. It re-renders live as you adjust, and exports as JPEG or
TIFF. Steppable parameters: **Exposure, Callier Q, Contrast Grade, Shadow Lift,
Hilite Trim, Scene Wht, Scene Blk, Toe, Shoulder.**

---

## Snapshots & Compare

**Snapshots** capture a full develop + toning state with a thumbnail at any
point. Press `⌘K` to capture; the Snapshots panel lists every capture for the
open file. Pin up to four to make them available in the compare grid;
double-click a thumbnail to restore that state; drag to reorder.

**Compare grid** (`K`) shows pinned snapshots side by side in **2-up / 3-up /
4-up** layouts (`2` / `3` / `4`), with zoom and pan synced across all cells.
Close with `⌘W` or `Esc`.

---

## Lightbox

A full-screen grid browser for the folder of the open file. Press `L` to enter,
`L` again to return.

- **Grid** — thumbnail cards with star rating, colour label, and an edit
  indicator; adjust the column count with `⌘−` / `⌘+`.
- **Full-image preview** — `Space` on a card opens a full-resolution preview;
  `←` / `→` navigate, `Space` or `Esc` returns.
- **Ratings** — `⌘1–5` set a star rating (press the same again to clear).
- **Colour labels** — `⇧1–6`: blue · cyan · green · yellow · red · magenta
  (press the same again to clear).
- **Folders & Favourites sidebar** — a folder tree plus a favourites list for
  fast navigation between shoots.
- **Batch copy/paste** of develop settings across selected files.
- **EXIF panel**, plus **sort and filter** by filename, capture date, rating, or
  last-developed, and by label or rating.
- **Rotate** a selected image with `R` (CW) / `⇧R` (CCW).
- **XMP sidecars** — ratings, labels, and develop settings write to per-file
  `.mono.xmp` sidecars and persist across sessions.

---

## Export

| Format | Encoding | ICC | When |
|--------|----------|-----|------|
| 16-bit TIFF | L\* (CIE 1976) | monostar (L\* Gray) | No toning |
| 16-bit RGB TIFF | L\* (CIE 1976) | eciRGB v2 ICCv4 | Toning active |
| JPEG proof | Display RGB | Display P3 | Always ⅓ scale, quality 92 |

Plate Simulation can be exported on its own as a full-resolution Display-P3
JPEG. Output DPI is configurable (72–1440). Set fixed output folders in
Preferences to skip the save dialog entirely (Quick Export).

---

## Presets

- **Develop presets** save the full look: exposure, Callier Q, scene range,
  black/white point, luminance channel, tone mode (and Multigrade/Plate
  Character settings), toe/shoulder, shadow lift/hilite trim, invert, grain,
  sharpening, and DPI. Stored at `~/.monopro/develop_presets.json`.
- **Toning presets** save the process, shadow/highlight colours, per-side
  opacity, balance, and Pt/Pd blend. Stored at `~/.monopro/toning_presets.json`.
- **Grain presets** save the full grain parameter set.

Any preset can be set as a **default to apply automatically** when opening a new
file, in Preferences.

---

## Preferences

Open with `⌘,` or `.`; changes apply immediately. Preferences live at
`~/.monopro/prefs.json`.

**Output & naming** — fixed TIFF and JPEG output folders (empty = ask every
time), configurable filename suffixes, and **Quick Export** to skip the save
dialog when a folder is set.

**Default pipeline on open** — a develop preset, luminance channel, tone mode,
toning preset, grain preset, sharpening level, and DPI to apply to every newly
opened file; plus whether to fully reset parameters and clear snapshots on open.

**Viewer** — independent background grays for the **main viewer, compare grid,
and lightbox**; the **Surround** colour and width (the `B` border); and the
cursor colour-readout **space** (L\*a\*b\* · Rec2020 · sRGB · Display P3 · eciRGB
v2 · ProPhoto) and **source** (processed output vs. raw linear input).

**Lightbox** — show/hide filenames on tiles, use XMP-edited previews vs.
originals, a frameless tile mode, favourites, and optional sort-mode restore.

---

## Hotkeys

### Develop

| Key | Action |
|-----|--------|
| `⌘O` | Open RAW or TIFF |
| `⌘E` | Export 16-bit TIFF |
| `⌘⇧E` | Export JPEG proof |
| `⌘,` or `.` | Preferences |
| `⌘R` | Reprocess |
| `⌘F` | Toggle fullscreen |
| `⌘0` | Fit to window |
| `⌘+` / `⌘−` | Zoom in / out |
| `⌘Z` / `⌘⇧Z` | Undo / redo |
| `Z` | Toggle 100% zoom at cursor |
| `Space` (hold) | Hand / pan |
| `B` | Toggle Surround border |
| `L` | Toggle Lightbox / Develop |
| `F` | False-colour exposure map |
| `O` / `U` | Overexposed / underexposed clipping overlay |
| `P` | Preview bypass — linear input vs. develop |
| `⌥I` | Invert (plate positive) |
| `I` | Place L\*a\*b\* pin |
| `⇧I` | Hide / show pins |
| `J` | Cycle colour views: Gray → Embedded JPEG → Linear Rec2020 colour |
| `S` | Toggle Split View |
| `Y` | Cycle luminance channel |
| `T` | Cycle tone mapping mode |
| `C` | Toggle crop / rotate tool |
| `⌘K` | Capture snapshot |
| `K` | Toggle compare grid |
| `2` / `3` / `4` | Compare-grid layout (2-up / 3-up / 4-up) |
| `⌘W` | Close compare grid |
| `⌘⇧S` | Save develop preset |
| `⌘⇧T` | Save toning preset |
| `/` | About & Workflow |
| `,` | Hotkey HUD |

### Crop / Rotate

| Key | Action |
|-----|--------|
| `C` | Toggle crop tool (commits on exit) |
| `↵` | Commit crop |
| `Esc` | Cancel crop — restore the previous frame |
| `S` (hold) | Straighten — drag to align a horizon |
| `G` | Toggle rule-of-thirds grid |

### Dodge & Burn

| Key | Action |
|-----|--------|
| `X` | Enter Burn mode (press again to exit) |
| `D` | Enter Dodge mode (press again to exit) |
| `⌘X` / `⌘D` | New Burn / Dodge instance and start painting |
| `⇧X` / `⇧D` (hold) | Flash the Burn / Dodge mask overlay |
| `[` / `]` | Brush radius − / + |
| `⌘[` / `⌘]` | Brush intensity (EV) − / + |
| `⇧[` / `⇧]` | Brush feather − / + |
| `⌥[` / `⌥]` | Brush opacity − / + |

### Lightbox

| Key | Action |
|-----|--------|
| `L` | Toggle Lightbox / Develop |
| `Space` | Full-image preview toggle |
| `←` / `→` | Navigate images in preview |
| `Esc` | Exit preview |
| `⌘1–5` | Star rating (same again clears) |
| `⇧1–6` | Colour label: blue · cyan · green · yellow · red · magenta |
| `R` / `⇧R` | Rotate selected CW / CCW 90° |
| `⌘−` / `⌘+` | Fewer / more grid columns |

> [!TIP]
> On any slider: **double-click** resets to default, **right-click** types an
> exact value, and the **scroll wheel** fine-adjusts.

---

## Supported cameras

monopro decodes through LibRaw (800+ cameras). Primary RAW formats: **DNG, ARW,
NEF, CR2/CR3, RAF, RW2, 3FR, IIQ.** Any 8- or 16-bit **TIFF** opens directly.

---

## For developers

> Most users want the [one-line install](#the-one-line-install). Read on only to
> modify the source.

monopro is a single-file PyQt6 application (`monopro.py`, ~28 k lines) with two
optional Rust/PyO3 extensions. The pipeline is linear-light throughout (Linear
Rec2020 float32), with L\* encoding applied only at export.

### Repository layout

```
monopro/
├── monopro.py                  ← the application
├── monopro_ext/                ← Rust workspace
│   ├── Cargo.toml              ← workspace manifest
│   ├── .cargo/config.toml      ← macOS linker flags (-undefined dynamic_lookup)
│   ├── monopro_sharpen/        ← sharpening · GPU · grain · D&B
│   │   ├── src/lib.rs
│   │   └── Cargo.toml
│   └── monopro_core/           ← display-pipeline acceleration
│       ├── src/lib.rs
│       └── Cargo.toml
├── profiles/
│   ├── eciRGB_v2_ICCv4.icc     ← you supply this (eci.org) — toned export
│   └── DisplayP3-v4.icc        ← JPEG-proof profile
├── setup_monopro.sh            ← idempotent setup + .app builder
├── monopro_app_icon.png
└── README.md
```

`monopro.icns`, `monopro.command`, `venv/`, the Rust `target/` directories, and
`monopro.app` are all generated locally and gitignored. The **monostar** ICC
profile is generated at runtime, not stored.

### Key dependencies

**Python:** PyQt6, NumPy (`<2`), rawpy/LibRaw, Pillow, SciPy, exifread,
colour-science, maturin. **Rust:** PyO3 (Bound API), ndarray + rayon, wgpu
(Metal backend), rand/rand_distr. Build the extensions with maturin from each
crate directory (not the workspace root); macOS PyO3 builds need
`RUSTFLAGS="-undefined dynamic_lookup"`, which the workspace `.cargo/config.toml`
supplies.

---

## License

monopro is free software under the **GNU General Public License v3.0**. You're
free to use, study, share, and modify it; a distributed modified version must
stay free under the same licence. GPLv3 is the right fit because monopro builds
on GPL-licensed components (PyQt6) and references GPL source for reference
(LibRaw, and demosaic/grain algorithms studied from darktable/Ansel).

The **monostar** grayscale ICC profile is released separately under **CC0** —
see [`MONOSTAR.md`](MONOSTAR.md).

---

## Acknowledgements

monopro stands on a lot of excellent open work: **LibRaw** for RAW decoding,
**lensfun** for lens-correction profiles, **ArgyllCMS** and the CGATS ecosystem
for the measurement side, and **colour-science** for spectral data. The AgX
display transform is the work of **Troy Sobotka** (and its Blender 4
integration); the crystallographic grain model follows **Aurélien Pierre**'s 2023
algorithm; emulsion sensitivity curves draw on published sensitometry by **Mike
Ware, Sandy King, James Reilly,** and others. The darkroom philosophy throughout
owes an obvious debt to a century of wet-process printmakers.

<sub>v0.1.0 · C. Cunningham · New York · github.com/christophcunningham</sub>
