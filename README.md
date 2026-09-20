# monopro

A monochrome RAW processor written in Rust, for editing photographs and preparing prints.

In development. A website with further documentation and a manual is in progress.

## Getting started

[Download for macOS](https://github.com/christophcunningham/monopro/releases/download/v0.1.0/monopro-0.1.0.dmg)
— version 0.1.0, Apple Silicon and Intel. Open the `.dmg`, drag **monopro.app** to
**Applications**, eject the disk image, and launch the installed app.

This is a development build and is not notarized by Apple. See the
[release notes](https://github.com/christophcunningham/monopro/releases/tag/v0.1.0)
for first-launch instructions and known limitations.

Windows and Linux packages are in preparation. [Build from source](#build) to run
the current source version.

In Lightbox, select a folder of RAW images in the folder tree. Double-click an image
to open it in Develop. Use the modules, Dodge / Burn brushes, and Toning to edit
before exporting. Use Inspector to read values, snapshots to compare edits, and
history to undo or redo changes.

Press `,` for Settings and `.` for the hotkey reference.

On macOS the app checks the stable release feed once a day, quietly: nothing
appears while it is current, a badge appears at the right of the title strip
when an update exists, and clicking it offers *Update on quit* (default),
*Restart now*, or *Skip this version*. Settings → About has the preference and
the manual check.

## Lightbox

A folder-based browser with thumbnails, ratings, color labels, sorting, filtering,
and manual ordering. Search covers filenames and IPTC metadata. Lightbox also supports
metadata editing and templates, copying develop settings, batch renaming, and PDF
contact sheets.

## Develop modules

Decode · Luminance · Exposure · Contrast Mask · Curve · Tonal Transform · Grain ·
Sharpening · Composition · Output · Frame · Export

Dodge / Burn and Toning have separate panels. Inspector pins, snapshots, comparison
views, and the print loupe provide measurement and review tools.

## Pipeline

RAW decoding preserves the Bayer color filter array. Each photosite is black-subtracted,
normalized to its channel's white level, and gain-equalized:

```text
s_c = gain_c × (raw − black) / (white_c − black)
```

Negative values and highlight headroom are retained. Sensor clipping is recorded before
gain equalization. Monochrome reconstruction supports RCD (the default), AMaZE,
Hamilton–Adams, bilinear interpolation, 2×2 SuperPixel binning, and DirectMosaic.
The default channel weighting is `Y = ¼R + ½G + ¼B`; equal, single-channel, and custom
weights are also available. These weights operate on sensor data.

The working image is single-channel, scene-linear `f32`. Geometry, exposure, contrast
masking, dodge and burn, and curves run through a graph of WGSL compute passes using
wgpu. Inactive curve and masking stages are omitted; cached intermediates are reused.

- **Exposure:** `Y′ = (Y − black_correction) × 2^EV`.
- **Contrast mask:** Gaussian filtering in log₂ space. The blurred signal is subtracted
  around an 18% gray pivot to compress local tonal range.
- **Dodge / burn:** layered exposure adjustments with brush masks, tonal-range masks,
  and local contrast controls.
- **Curves:** Fritsch–Carlson monotone cubic interpolation in a log₂ exposure domain.
  The curve stack is composed into a 65,536-entry lookup table.
- **Tonal transform:** clipping, an exponential soft shoulder, or a monochrome AgX
  sigmoid maps scene values into the output range.

The print path applies tone mapping, resizing, grain, toning, output sharpening,
framing, and encoding, in that order. Grain, toning, and sharpening run on the CPU;
the print loupe previews them at the selected output scale. Grain uses stochastic
silver-halide crystal synthesis. Toning models material conversion and optical density;
its coefficients are currently adjusted by eye rather than measured.

Export supports TIFF, PNG, and JPEG. Grayscale masters can use L* encoding with the
[monostar ICC profile](profiles/MONOSTAR.md). Toning introduces color after the
monochrome processing stages. Edits and metadata are stored in `.mono.xmp` sidecars.

## Dependencies

Direct external Rust dependencies, at the versions resolved in [Cargo.lock](Cargo.lock).
The lockfile also records transitive dependencies.

| Dependency | Version | Use |
|---|---|---|
| rawler | 0.7.2 | RAW decoding |
| wgpu | 29.0.4 | GPU processing |
| eframe, egui, egui-wgpu | 0.35.0 | Interface and rendering |
| egui_tiles | 0.16.0 | Panel layout |
| rayon | 1.12.0 | CPU parallelism |
| bytemuck | 1.25.2 | GPU data layout |
| pollster | 0.4.0 | Async task blocking |
| image | 0.25.10 | Image decoding and resizing |
| png | 0.18.1 | PNG output |
| tiff | 0.11.3 | TIFF output |
| jpeg-encoder | 0.7.1 | JPEG output |
| kamadak-exif | 0.6.1 | EXIF reading |
| pdf-writer | 0.15.0 | Contact sheets |
| ttf-parser | 0.25.1 | Font metrics and embedding |
| resvg | 0.47.0 | SVG icons |
| rfd | 0.17.2 | File dialogs |
| reflink-copy | 0.1.30 | File duplication |
| serde | 1.0.229 | Serialization |
| toml | 0.9.12+spec-1.1.0 | Settings |
| roxmltree | 0.21.1 | XMP parsing |
| thiserror | 2.0.19 | Error types |
| muda | 0.19.3 | macOS menus |
| libc | 0.2.189 | macOS system interface |
| windows-sys | 0.61.2 | Windows file operations |

Bundled assets: JetBrains Mono, Phosphor and Lucide icons, and ICC profiles.
Their notices are included in [fonts](fonts/JetBrainsMono), [icons](icons), and
[profiles](profiles). Grain follows Aurélien Pierre's crystallographic synthesis;
AgX follows Troy Sobotka's tone-mapping work.

## Where it keeps things

| File or directory | Contents |
|---|---|
| `<stem>.mono.xmp` | one image's edit, beside the raw |
| `app.ron`, `settings.toml`, presets | durable application data |
| `thumbcache/` | rebuildable Lightbox tiles in the OS cache location |

| OS | Durable data | Rebuildable cache |
|---|---|---|
| macOS | `~/Library/Application Support/monopro` | `~/Library/Caches/monopro` |
| Windows | `%APPDATA%\monopro\data` | `%LOCALAPPDATA%\monopro\cache` |
| Linux | `${XDG_DATA_HOME:-~/.local/share}/monopro` | `${XDG_CACHE_HOME:-~/.cache}/monopro` |

`MONOPRO_APP_ID` overrides the final application name in both roots, which is what
`./run --profile` uses to keep two builds from editing each other's memory or cache.

## Build

Rust 1.92 or newer, edition 2024.

```sh
cargo run --release --locked --bin monopro
```

macOS has been exercised locally. Windows and Linux support is implemented; native
package and desktop validation remain pending. See [packaging](packaging/README.md),
[platform status](docs/cross-platform-release.md), and the
[release checklist](docs/release-smoke-test.md).

[Keyboard shortcuts](docs/hotkeys.md) · [Optional test fixtures](docs/private-fixtures.md)

## Licence

[GPL-3.0-or-later](LICENSE).
