# Architecture

A map of the source, and where each kind of change belongs. Line counts are
approximate and only there to show where the weight is. The module doc comment at
the top of each file is the fuller account; this page is for finding the right file.

## Four crates, one direction

```text
raw-core ──► raw-graph ──► raw-gpu ──► raw-app (the `monopro` binary)
    └────────────────────────────────────┘
```

| Crate | Owns | Never touches |
|---|---|---|
| `raw-core` | decoding, every parameter, the sidecar, all CPU image stages | the GPU, the UI |
| `raw-graph` | which stages run, in what order, over which region | wgpu (so its region arithmetic is tested even without a GPU) |
| `raw-gpu` | the device, the texture pool, the WGSL passes, readback | egui |
| `raw-app` | the window, every panel, export encoding, settings, Lightbox, the command line | pixel maths that could live in `raw-core` |

Anything that can be tested without a window belongs as far left as it can go.

## The picture's path

```text
file ─► SensorImage ─► SceneImage ─► LumaImage ─────────────► GPU graph ─────────────► screen
        sensor.rs      scene.rs      scene.rs +               raw-graph builds it,       display.wgsl
        (rawler)       black, white, demosaic.rs              raw-gpu runs it:
                       gain                                   exposure, contrast mask,
                                                              dodge & burn, curve
                                                                    │
                                                                    └─► export tap ─► CPU tail ─► file
                                                                        (scene f32)   resample, grain,
                                                                                      toning, sharpen,
                                                                                      frame, encode
                                                                                      (export.rs)
```

Three rules hold the path together:

- **`Params` is a value.** `raw_graph::build` makes a new graph from it on every
  change, and nothing edits a graph in place. That's why undo is a stack of `Params`,
  a duplicate tab is a clone, and the sidecar is a flat list of settings.
- **`Params::effective()` resolves bypasses.** The viewport, export, compare cells
  and `monopro render` all render from it, so a module that is switched off stays off
  everywhere.
- **One export spec.** `export::Spec::for_params` is the only place an edit becomes a
  file description, and the Export button and `monopro render` both call it.
  L\* encoding exists only inside `export.rs`.

## Source map

### raw-core: decode, parameters, CPU stages

| File | Lines | What it is |
|---|---:|---|
| `sensor.rs` | 940 | Stage 1: the u16 CFA mosaic through rawler, EXIF, and refusing non-Bayer sensors |
| `geometry.rs` | 180 | CFA geometry: crop origin and pattern phase, the source of off-by-two bugs |
| `scene.rs` | 820 | Stages 2–3: black, white and gain equalisation, then luminance and the sampling modes |
| `demosaic.rs` | 2240 | Full-resolution demosaics (RCD, AMaZE, Hamilton–Adams, bilinear), ported and GPL |
| `params.rs` | 2280 | Every pipeline parameter as one value, plus undo history and `effective()` |
| `sidecar.rs` | 3570 | `<stem>.mono.xmp`, written by hand because it is a compatibility surface |
| `composition.rs` | 2390 | Orientation, straighten, keystone, crop, and `Frame` (which pixels become the picture) |
| `curve.rs` | 710 | Monotone cubic curve in log₂ EV, baked into a lookup table |
| `dodgeburn.rs` | 1970 | Dodge & Burn layers as parametric records |
| `zone.rs` | 730 | Zone masks: the proxy they are computed on, and tonal selection |
| `display.rs` | 780 | The display transform and L\*: the reference `display.wgsl` mirrors, and what the histogram calls |
| `output.rs` | 620 | Print size, resolution and the pixel limits (`MAX_EDGE`, `MAX_PIXELS`) |
| `resample.rs` | 520 | Resizing to output size, at the start of the export tail |
| `grain.rs` | 1430 | Silver-halide grain synthesis (Pierre) |
| `toning.rs` | 1710 | Chemical toning as a composition of species |
| `sharpen.rs` | 710 | Output sharpening with an à-trous wavelet |
| `frame.rs` | 620 | The physical canvas around an exported print |
| `colour.rs`, `okhsl.rs` | 1340 | OKLab to tagged RGB; OKHSL for the surround colour |
| `icc.rs` | 450 | Generating `monostar.icc` |
| `preview.rs` | 830 | The embedded colour preview, for reference views only (not the pipeline) |
| `atomic_file.rs` | 120 | Replacing a file without ever exposing half of one |
| `camera_exif.rs` | 450 | The camera EXIF an export carries, and the list of what never travels |

Examples in `examples/` are measurement tools (`inspect`, `sharpen-sweep`,
`demosaic-compare`, …). `tests/grain_cost.rs` is an ignored benchmark.

### raw-graph: topology and regions

| File | Lines | What it is |
|---|---:|---|
| `lib.rs` | 1430 | `build(params)` → `Graph`; stage order; region propagation in both directions |
| `node.rs` | 370 | The closed set of stages, and the apron each one reads beyond its output |
| `roi.rs` | 430 | Regions and aprons |

### raw-gpu: device work

| File | Lines | What it is |
|---|---:|---|
| `lib.rs` | 2530 | `GpuContext`, `Viewport` (upload, render, `export`, patch readback, histogram) |
| `exec.rs` | 580 | Compiled passes and the executor that walks a plan |
| `pool.rs` | 210 | The shared intermediate texture pool |
| `limits.rs` | 90 | Checking allocations before wgpu sees them, and `limits(adapter)` |
| `zones.rs` | 220 | Bounded off-thread zone preparation |
| `*.wgsl` | — | One file per pass: `sample`, `exposure`, `log2`/`exp2`, `blur`, `contrast_mask`, `mask_input`, `dodge_burn`, `curve`, `display` |

GPU integration tests in `tests/render.rs` need a real adapter; CI compiles them
but runs only `--lib`.

### raw-app: the application

| File | Lines | What it is |
|---|---:|---|
| `main.rs` | 14700 | `App`: startup, loading, the frame loop, every Develop panel and tool (map below) |
| `lightbox.rs` | 9460 | The browser: folder tree, grid, thumbnails, sorting, filtering, ratings, rotation |
| `export.rs` | 4040 | Containers, encodings, `Spec`, `write` (the CPU tail and the encoders) |
| `layout.rs` | 3150 | The `egui_tiles` docking tree; the image is one pane in it |
| `tabs.rs` | 2860 | Develop tabs: images, renders, pins, compare |
| `contact_sheet.rs` | 2990 | Contact-sheet planning and PDF |
| `widgets.rs` | 2730 | Shared controls: the module frame, sliders, switches |
| `settings.rs` | 2130 | `settings.toml`, its sections and the storage roots |
| `hotkeys.rs` | 2020 | Every key binding as data (`TABLE`); the menu is built from it |
| `theme.rs` | 1430 | Colours, the one typeface, sizes, re-greying on light grounds |
| `crop.rs` | 1700 | The crop tool's hit-testing, drag arithmetic and overlay |
| `paint.rs` | 1180 | The Dodge & Burn brush |
| `rename.rs`, `search.rs` | 2440 | Lightbox batch rename, and filename/IPTC search |
| `histogram.rs` | 1170 | The tonal distribution panel |
| `updater.rs` / `updater_stub.rs` | 1940 | Sparkle on macOS, answered by the update sheet; an inert stand-in elsewhere |
| `loupe.rs` | 960 | The print loupe: the export tail on one tile |
| `decode.rs` | 860 | The off-thread decode queue and cache |
| `cli.rs` | 700 | `monopro render` and `monopro info` |
| `toning.rs`, `snapshot.rs` | 940 | The Toning pane; snapshots and the compare grid |
| `menu.rs` / `menu_fallback.rs` | 590 | The macOS menu bar; the no-menu boundary elsewhere |
| `platform.rs`, `dialogs.rs` | 520 | File-manager and volume differences; native dialogs and path checks |
| `icons.rs` | 760 | SVG icons rasterised at startup |
| `curve_presets.rs`, `iptc_templates.rs` | 530 | Presets stored with the app, not beside images |
| `visual.rs` | 260 | Test only: frames of the real app drawn without a window |

#### Inside `main.rs`

`main.rs` is the file most changes touch, and it is laid out in this order:

| Where | What |
|---|---|
| top, `fn main` | runs `cli` if the arguments are a command, otherwise starts eframe with `wgpu_config` |
| `struct App`, `App::new` | all state; settings, presets and window memory restored |
| `loading` section | `open`, `start_load`, decode polling, sidecar load/save, `export`, `apply` (the diff-and-apply path every edit goes through) |
| `impl eframe::App` → `ui` | the frame: hotkeys, menu and updater polling, then Lightbox or the tile tree |
| panels | `develop_panel` (every module's controls), `dodgeburn_body`, `toning_body`, `history_body`, `snapshot_body`, `info_panel` and their floating versions |
| canvas tools | `viewport_panel`, then one function per tool: `curve_point_tool`, `pin_tool`, `print_loupe`, `paint_tool`, `crop_tool`, `keystone_tool` |
| Info sections | `pipeline_section`, `export_section`, `inspector_section` |
| sheets and windows | `guard_quit`, `quit_sheet`, `update_sheet`, `settings_window` |
| tests | `tests`, `sampler_tests`, `pin_tests`, `crop_gesture_tests` |

When `main.rs` is split, those rows are the natural seams: `develop_panel`,
the canvas tools and `settings_window` are each large enough to be modules.

## Where a change belongs

| To… | Change |
|---|---|
| add or change a develop control | the parameter in `raw-core/params.rs`; its sidecar field in `sidecar.rs` (bump `SCHEMA_VERSION`, keep old files opening); the control in `develop_panel` |
| add a GPU stage | a node in `raw-graph/node.rs` and its place in `build`; a `.wgsl` pass and its wiring in `raw-gpu`; if it changes the display transform, `display.rs` first, since the shader mirrors it |
| add a stage after the tone map | `raw-core` for the maths, `export::write` for its place in the tail, `loupe.rs` so the loupe shows it |
| add an export format or encoding | `export.rs`: `Container`/`Space`, `Spec`, `write` |
| add a preference | a field in `settings.rs` with a default, and its row in `settings_window` |
| add a shortcut | a row in `hotkeys::TABLE`; the macOS menu and the hotkey reference follow it |
| change what Lightbox lists or shows | `lightbox.rs` |
| handle a platform difference | `platform.rs` (file manager, volumes, window shell) or `dialogs.rs` (dialogs, opened paths) |
| add to the command line | `cli.rs`, reusing the export path, never a second one |
| check a UI change by eye | add or reuse a scene in `visual.rs` and run `cargo test -p raw-app visual -- --ignored` |
| record a user-visible change | a line under **Unreleased** in `CHANGELOG.md` |
| record something monopro can't do | `docs/known-limitations.md` |

For a first reading, follow one edit from file to print:

1. `raw-app/src/main.rs`, `fn main`: a command or the window.
2. `struct App` and its `loading` section: how a file opens, and `apply`, which every
   edit goes through.
3. `raw-core/src/params.rs`: what an edit is.
4. `raw-graph/src/lib.rs`, `build`: how an edit becomes stages.
5. `raw-gpu/src/lib.rs`, `Viewport`: how those stages run and reach the screen.
6. `raw-app/src/export.rs`, `write`: the CPU tail and the encoders.

## Tests

- `cargo test --workspace` runs everything that needs neither a GPU nor photographs.
- GPU tests (`raw-gpu/tests/render.rs`) and camera tests skip or are ignored without
  an adapter or the private corpus (`docs/private-fixtures.md`).
- `visual.rs` scenes are ignored by default and write PNGs rather than asserting on
  pixels.
- CI (`.github/workflows/native-ci.yml`) runs clippy with warnings denied and the
  tests on macOS, Windows and Linux, plus a weekly advisory check (`deny.toml`).
