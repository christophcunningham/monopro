# monopro

A monochrome RAW processor written in Rust, for editing photographs and preparing prints.

In development. A website with further documentation and a manual is in progress.

## Pipeline

RAW decoding preserves the Bayer colour filter array. Each photosite is black-subtracted,
normalised to its channel's white level, and gain-equalised:

```text
s_c = gain_c × (raw − black) / (white_c − black)
```

Negative values and highlight headroom are retained. Sensor clipping is recorded before
gain equalisation. Monochrome reconstruction supports RCD (the default), AMaZE,
Hamilton–Adams, bilinear interpolation, 2×2 SuperPixel binning, and DirectMosaic.
The default channel weighting is `Y = ¼R + ½G + ¼B`; equal, single-channel, and custom
weights are also available. These weights operate on sensor data.

The working image is single-channel, scene-linear `f32`. Geometry, exposure, contrast
masking, dodge and burn, and curves run through a graph of WGSL compute passes using
wgpu. Inactive curve and masking stages are omitted; cached intermediates are reused.

- **Exposure:** `Y′ = (Y − black_correction) × 2^EV`.
- **Contrast mask:** Gaussian filtering in log₂ space. The blurred signal is subtracted
  around an 18% grey pivot to compress local tonal range.
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

Export supports TIFF, PNG, and JPEG. Greyscale masters can use L* encoding with the
[monostar ICC profile](profiles/MONOSTAR.md). Toning introduces colour after the
monochrome processing stages. Edits and metadata are stored in `.mono.xmp` sidecars.

## Develop modules

Decode · Luminance · Exposure · Contrast Mask · Curve · Tonal Transform · Grain ·
Sharpening · Composition · Output · Frame.

Dodge / Burn and Toning have separate panels. Inspector pins, snapshots, comparison
views, and the print loupe provide measurement and review tools.

## Lightbox

A folder-based browser with thumbnails, ratings, colour labels, sorting, filtering,
and manual ordering. Search covers filenames and IPTC metadata. Lightbox also supports
metadata editing and templates, copying develop settings, batch renaming, and PDF
contact sheets.

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
| serde | 1.0.229 | Serialisation |
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
