//! monopro — a monochrome-first raw viewer built on the CFA mosaic.
//!
//! The pipeline as it stands: decode -> luminance -> exposure -> contrast mask ->
//! curve -> display. The GPU half is a node graph (`raw-graph` for topology and
//! ROI, `raw-gpu` for execution), built fresh from `Params` every frame — there is
//! no node editor and no graph state to keep in sync.
//!
//! # How state flows
//!
//! `Params` is a **value**, not ambient state. Every frame, for the active tab:
//!
//! 1. snapshot the params
//! 2. let the UI mutate them freely
//! 3. diff old against new to get a `Dirty` work order
//! 4. act on the highest dirty tier; the cascade handles the rest
//! 5. hand the before/after pair to that tab's `History` for coalesced undo
//!
//! That is what makes undo, duplication and (next) sidecar serialisation fall out
//! rather than needing to be threaded through each control. Tab duplication in
//! particular is a `Params::clone` and nothing else — see `tabs`.
//!
//! # What is per-tab and what is shared
//!
//! | Per tab | Shared by the app |
//! |---|---|
//! | `Params`, `History`, `View`, status, error | compiled pipelines (`GpuContext`) |
//! | the decoded image, behind `Arc`s | the intermediate texture pool |
//! | GPU state, **while warm** | the decode queue and its cache |
//!
//! The pool being app-level is what made tabs cheap; see
//! `raw_gpu::pool`.

mod contact_sheet;
mod crop;
mod curve_presets;
mod decode;
mod dialogs;
mod export;
mod histogram;
mod hotkeys;
mod icons;
mod iptc_templates;
mod layout;
mod lightbox;
mod loupe;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(not(target_os = "macos"))]
#[path = "menu_fallback.rs"]
mod menu;
mod paint;
mod platform;
mod rename;
mod search;
mod settings;
mod snapshot;
mod tabs;
mod theme;
mod toning;
#[cfg(target_os = "macos")]
mod updater;
#[cfg(not(target_os = "macos"))]
#[path = "updater_stub.rs"]
mod updater;
mod widgets;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use raw_core::{
    CompositionParams, ContrastMaskParams, DemosaicAlgo, ExposureParams, KeystoneCrop,
    KeystoneMode, KeystoneParams, OutputParams, Params, Ratio, Sampling, SensorImage, ToneMap,
    Unit, Weighting, scene, sidecar,
};
use raw_gpu::{GpuContext, ViewGeometry, Viewport};

use decode::{BACKGROUND, ContentKey, Decoded, Done, FOREGROUND};
use layout::{Head, HeadClicks, Layout, Pane};
use tabs::{Image, Render, Tab, TabId, Tabs};

fn main() -> eframe::Result<()> {
    // Headless export: `raw-app --export <raw> <out.tif|out.png>`. No window, no
    // dialog. Exists so the export path can be exercised end to end without a GUI —
    // which is also what makes it usable for batch work.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--export") {
        if args.len() < 3 {
            eprintln!(
                "usage: raw-app --export <raw> <out.tif|out.png> \
                 [--agx] [--8bit] [--mask <spacer_pct>] [--demosaic <algo>]"
            );
            std::process::exit(2);
        }
        // `--mask <spacer>` turns Contrast Mask on at its default gamma. Exists so
        // the module can be judged on real frames, and timed, without a window.
        // The value is a percentage of the frame diagonal, matching the slider.
        let mask = args
            .iter()
            .position(|a| a == "--mask")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<f32>().ok());
        // `--demosaic <algo>` switches to full-resolution sampling with that
        // algorithm. The demosaic modes are exactly the ones that have to be
        // compared at 1:1 on real frames, which is not something a window is good
        // at doing reproducibly.
        let algo = args
            .iter()
            .position(|a| a == "--demosaic")
            .and_then(|i| args.get(i + 1))
            .map(|name| {
                DemosaicAlgo::UI_ORDER
                    .into_iter()
                    .find(|a| a.label().eq_ignore_ascii_case(name))
                    .unwrap_or_else(|| {
                        eprintln!(
                            "unknown demosaic algorithm {name:?}; have {:?}",
                            DemosaicAlgo::UI_ORDER.map(|a| a.label())
                        );
                        std::process::exit(2);
                    })
            });
        headless_export(
            &args[1],
            &args[2],
            args.iter().any(|a| a == "--agx"),
            args.iter().any(|a| a == "--8bit"),
            mask,
            algo,
        );
        return Ok(());
    }

    let options = eframe::NativeOptions {
        // macOS runs the dark app chrome under its traffic lights. Windows and Linux
        // keep their native titlebars and window controls. The platform boundary owns
        // that distinction so a new viewport cannot accidentally become borderless.
        viewport: platform::main_viewport(),
        renderer: eframe::Renderer::Wgpu,
        // egui-wgpu asks for max_texture_dimension_2d = 8192, sized for a 4K depth
        // buffer. The working image is a texture too, and in DirectMosaic it is the
        // full sensor width — 11648 on the Fuji GFX 100S — so at the default limit
        // switching sampling mode on a large sensor is a validation failure inside
        // the driver call, i.e. a crash rather than a refused mode. Ask the adapter
        // for everything it has; Metal on Apple silicon reports 16384.
        wgpu_options: wgpu_config(),
        // egui's own dither is interleaved-gradient noise keyed to SCREEN position
        // and would stack on top of the pipeline's TPDF dither, adding a second
        // noise source that crawls under the image while panning. The display
        // shader owns dithering here.
        dithering: false,
        ..Default::default()
    };
    // See `settings::app_id`. Two builds of this app running side by side must not
    // edit each other's window geometry, layout and last folder.
    eframe::run_native(
        &settings::app_id(),
        options,
        Box::new(|cc| {
            Ok(Box::new(App::new(
                cc,
                std::env::args().nth(1).map(PathBuf::from),
            )))
        }),
    )
}

/// eframe's wgpu setup, with one thing changed.
///
/// egui-wgpu asks for `max_texture_dimension_2d = 8192`, sized for a 4K depth
/// buffer. The **working image** is a texture too, and in DirectMosaic it is the
/// full sensor width — 11648 on the Fuji GFX 100S. At the default limit, switching
/// sampling mode on a large sensor is a validation failure inside the driver call:
/// a panic, not a refused mode. Ask the adapter for what it actually has; Metal on
/// Apple silicon reports 16384.
///
/// Must agree with `raw_gpu::headless_device`, or export and viewport disagree
/// about which files can be opened.
fn wgpu_config() -> egui_wgpu::WgpuConfiguration {
    let mut cfg = egui_wgpu::WgpuConfiguration::default();
    if let egui_wgpu::WgpuSetup::CreateNew(setup) = &mut cfg.wgpu_setup {
        setup.device_descriptor = Arc::new(|adapter: &wgpu::Adapter| wgpu::DeviceDescriptor {
            label: Some("monopro device"),
            required_limits: raw_gpu::limits(adapter),
            ..Default::default()
        });
    }
    cfg
}

/// Run the whole pipeline once and write a file. Same code path as the GUI export,
/// minus the window.
fn headless_export(
    input: &str,
    output: &str,
    agx: bool,
    eight: bool,
    mask: Option<f32>,
    algo: Option<DemosaicAlgo>,
) {
    let out = std::path::Path::new(output);
    let target = export::Target {
        container: match out.extension().and_then(|e| e.to_str()) {
            Some("png") => export::Container::Png,
            _ => export::Container::Tiff,
        },
        depth: if eight {
            export::Depth::Eight
        } else {
            export::Depth::Sixteen
        },
        compression: export::Compression::None,
        // The headless path writes masters. A proof is a thing you look at, and this
        // one has nobody looking at it.
        space: export::Space::Monostar,
    };

    // Start from the sidecar when there is one, so a batch export renders what was
    // actually edited rather than defaults. The flags below then override it, which
    // is what a flag on top of a saved state should do.
    let mut params = match sidecar::read(std::path::Path::new(input)) {
        sidecar::Loaded::Ok(s) => {
            println!(
                "read {} (schema {})",
                sidecar::path_for(input.as_ref()).display(),
                s.schema
            );
            s.params
        }
        sidecar::Loaded::Corrupt(e) => {
            // Exporting at defaults from a file the user believes carries their
            // edits would silently ship the wrong picture.
            eprintln!("sidecar unreadable, refusing to export at defaults: {e}");
            std::process::exit(1);
        }
        sidecar::Loaded::Absent => Params::default(),
    };
    let metadata = sidecar::effective_metadata(std::path::Path::new(input)).unwrap_or_default();
    if agx {
        params.display.enabled = true;
        params.display.tone_map = ToneMap::AGX_DEFAULT;
    }
    if let Some(spacer) = mask {
        params.contrast_mask.enabled = true;
        params.contrast_mask.spacer = spacer;
    }
    if let Some(algo) = algo {
        params.luminance.sampling = Sampling::Demosaic(algo);
    }
    // Headless follows the same bypass resolution as a live tab. In particular,
    // DISPLAY-off means Clip while retaining the required transfer function.
    params = params.effective();

    let sensor = match SensorImage::load(std::path::Path::new(input)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load failed: {e}");
            std::process::exit(1);
        }
    };
    let (sc, _) = scene::decode(&sensor, params.decode);
    let luma = scene::derive_luminance(&sc, params.luminance.sampling, params.luminance.weighting);
    // The composition from the sidecar, over the file's own orientation tag —
    // exactly what the app resolves, so a headless export is the picture the
    // viewport showed and not the negative it was cut from.
    let frame = raw_core::Frame::resolve(
        luma.output_dims,
        sensor.meta.orientation,
        &params.composition,
    );
    println!(
        "{} · {} x {}{}",
        sc.camera,
        frame.crop.w,
        frame.crop.h,
        if frame.is_uncropped() {
            String::new()
        } else {
            format!(" (cropped from {} x {})", frame.frame.w, frame.frame.h)
        }
    );

    let Some((device, queue)) = raw_gpu::headless_device() else {
        eprintln!("no usable GPU adapter");
        std::process::exit(1);
    };
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let Some((w, h, data)) = vp.export(&mut ctx, &device, &queue, &params, &frame, |d, t| {
        if t > 1 {
            println!("  tile {d}/{t}");
        }
    }) else {
        eprintln!("export render failed");
        std::process::exit(1);
    };

    // The size, the resolution and the metadata all come from the sidecar's Output
    // section, so a headless export writes the file the GUI would have written. The
    // literal `300` that used to sit here is exactly the failure the brief warned
    // about: three readouts moved with a setting and the file's own tag did not.
    // The preference, read here as the app reads it. A headless export that ignored it
    // would put the credit line back on a file someone had deliberately stripped —
    // and a batch export is exactly where that would go unnoticed.
    let include = match settings::Settings::load() {
        sidecar::Loaded::Ok(s) => s.export_metadata,
        _ => settings::Settings::default().export_metadata,
    };
    let mut spec = export::Spec::new(
        target,
        params.display.tone_map,
        params.output,
        // `effective`, so a bypassed grain does not cost the emulsion it would then
        // throw away, and a bypassed sharpen does not cost a wavelet decomposition.
        // Headless is where that would go unnoticed: a batch of forty.
        export::Tail::of(&params),
        include.then_some(&metadata),
    );
    spec.dither = params.display.dither;
    let picture = raw_core::geometry::Dims {
        w: w as usize,
        h: h as usize,
    };
    let image_d = spec.image_dims(picture);
    if image_d != picture {
        println!(
            "  resampling {w}x{h} -> {}x{} ({})",
            image_d.w,
            image_d.h,
            spec.output.scale_note(picture)
        );
    }
    let d = spec.dims(picture);
    match export::write(out, w, h, &data, &spec) {
        Ok(()) => println!(
            "wrote {output} · {} · {}x{} · {:.0} ppi{}",
            target.label(),
            d.w,
            d.h,
            spec.output.ppi,
            if spec.metadata.is_some() {
                " · metadata"
            } else {
                ""
            }
        ),
        Err(e) => {
            eprintln!("write failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Result of a background encode.
type Exported = Result<PathBuf, String>;

/// The name sheet keeps a snapshot of the selected instance from the frame Save was pressed.
/// Editing can continue behind the sheet without quietly changing what is saved.
struct CurvePresetNameDialog {
    value: String,
    instance: raw_core::CurveInstance,
    request_focus: bool,
}

/// Resolve the complete master grid, including FRAME, and enforce the allocation
/// limits on the file rather than only on the photograph inside it.
fn master_layout(
    params: &Params,
    picture: raw_core::Dims,
) -> Result<raw_core::frame::PixelLayout, String> {
    let effective = params.effective();
    let image = effective.output.target_dims(picture);
    let (w, h) = effective.output.print_inches(picture);
    let layout = effective
        .frame
        .pixel_layout(image, [w, h])
        .map_err(|e| format!("FRAME cannot be resolved: {e}"))?;
    if layout.outer.w > OutputParams::MAX_EDGE as usize
        || layout.outer.h > OutputParams::MAX_EDGE as usize
        || (layout.outer.w as u64) * (layout.outer.h as u64) > OutputParams::MAX_PIXELS
    {
        return Err(format!(
            "output would be {} x {} px — past the {} px edge / {} MP limit",
            layout.outer.w,
            layout.outer.h,
            OutputParams::MAX_EDGE,
            OutputParams::MAX_PIXELS / 1_000_000
        ));
    }
    Ok(layout)
}

struct PreparedLuma {
    source: Arc<Decoded>,
    params: raw_core::LuminanceParams,
    image: Arc<raw_core::LumaImage>,
}

const LUMA_WORKERS: usize = 1;

impl PreparedLuma {
    fn is_current(&self, tab: &Tab) -> bool {
        tab.accepts_luma(&self.source, self.params)
    }
}

struct App {
    tabs: Tabs,
    /// The browser mode. Holds its own queue and its own thumbnails; `active` is
    /// what decides whether this frame draws Develop's chrome or Lightbox's.
    lightbox: lightbox::Lightbox,
    /// Bounded, prioritised, cancellable. One decode per tab at a time; see
    /// `decode`.
    queue: decode::Queue<TabId, Done>,
    /// One full-frame luminance pass at a time. New edits supersede old work.
    luma_queue: decode::Queue<TabId, PreparedLuma>,
    /// Developed frames being rendered for the Lightbox grid. One at a time: the
    /// decode already saturates every core, and a second in flight would double the
    /// transient memory to finish the pair no sooner.
    developed_queue: decode::Queue<u64, Option<Box<DevelopedTile>>>,
    /// Weak, so it shares work between tabs without keeping a closed tab's image
    /// alive.
    cache: decode::Cache,
    /// Pipelines and the intermediate pool, shared by every tab. Created with the
    /// first image, because it needs the render state's device.
    gpu: Option<GpuContext>,
    /// Set while a file is being encoded and written off-thread.
    export_rx: Option<Receiver<Exported>>,
    export_thread: Option<std::thread::JoinHandle<()>>,
    export_owner: Option<TabId>,
    /// Deferred so the export runs outside the panel closure that requested it —
    /// it needs `&mut self` and the render state, both borrowed in there.
    /// An export was asked for, and which kind. A flag rather than a direct call:
    /// every route in — the button, the menu, `⌘E` — is inside a borrow of `self`
    /// that exporting needs mutably. See `ExportKind`.
    export_requested: Option<(TabId, ExportKind)>,
    /// Container and depth. View state, not image state: not undoable and not part
    /// of the render, so it stays off `Params`. App-level rather than per-tab
    /// because it is a preference about files, not a property of one image.
    export_target: export::Target,
    /// Shown when no tab is open.
    status: String,
    /// Where the open dialog last landed. App memory, not a setting — nobody
    /// configures this, the app just remembers.
    last_dir: Option<PathBuf>,
    /// Configured preferences, as distinct from app memory. See `settings`.
    settings: settings::Settings,
    settings_open: bool,
    /// Bring the settings window to the front on the next frame it draws.
    ///
    /// Set by every route that *opens* settings. Without it, a second press while the
    /// window is already up sets a flag that is already true and nothing happens —
    /// the window stays wherever it was, usually behind the main one.
    settings_raise: bool,
    /// Whether the settings viewport has had focus since it opened. Guards the
    /// close-on-click-away rule against the frame the window is created on.
    settings_focused: bool,
    /// The destructive Settings-wide reset is always confirmed before it runs.
    settings_reset_confirm: bool,
    /// Whether the main window had focus last frame, so a *gain* can be spotted.
    ///
    /// **The one moment a file may have changed under us.** A `.mono.xmp` written by
    /// another program — or by this app in a second window — is invisible to a grid
    /// that stats on the mode switch and never again, so an edited badge could stay
    /// wrong for as long as Lightbox stayed open. Coming back to the window is
    /// exactly when that is likely to have happened, and it is rare enough that the
    /// stat per entry `refresh_edited` costs is affordable there and would not be
    /// per frame. Starts true so launching straight into Lightbox does not count as
    /// a gain and re-stat a folder it has only just read.
    window_focused: bool,
    /// Makes scroll memory live for exactly as long as the open tabs do.
    ///
    /// A panel's scroll id also includes the active `TabId`, so returning to an open
    /// image restores that image's position while a newly opened image begins at the
    /// top. The session component prevents eframe's persisted egui memory from giving
    /// tomorrow's first tab yesterday's first tab position when `TabId(1)` is reused.
    scroll_session: u128,
    /// The Dodge & Burn brush.
    ///
    /// **App-level, not per tab.** Radius, feather, intensity and opacity are the
    /// tool in your hand, and a tool does not change when you change frames — the
    /// same argument that keeps them out of `Params` keeps them out of `Tab`. See
    /// `paint`.
    brush: paint::Brush,
    /// Which of the three shapes the next press makes. Tool state like the brush,
    /// and app-level for the same reason.
    tool: paint::Tool,
    /// An image-less tab the Develop panel draws against when nothing is open.
    ///
    /// the maintainer: the panel should show its modules at startup rather than an empty box.
    /// It is drawn **disabled** — editing parameters that belong to no picture is
    /// not a thing to allow — so this is never read back and never written to disk.
    /// See `tabs::Tab::placeholder`.
    placeholder: tabs::Tab,
    /// The Settings window's own view state — which page, and the search box.
    ///
    /// Not in `Settings` and never written to `settings.toml`: where you were
    /// standing when you closed the window is not a preference about how the app
    /// behaves. See `settings::Sheet`.
    sheet: settings::Sheet,
    /// Unsaved tabs and write errors shown by the quit sheet.
    confirm_quit: Option<Vec<String>>,
    /// The user has answered the sheet and meant it. Stops `guard_quit` asking again
    /// about the close it is itself about to send.
    quitting: bool,
    /// A brush control is being worked right now.
    ///
    /// **What makes sizing a brush not blind.** the maintainer: adjusting Radius or Aspect used
    /// to be, because reaching the slider means leaving the picture and the nib is
    /// only drawn under the pointer — so the one moment you most want to see the
    /// brush is the one moment it is not there. While this is set the nib is drawn in
    /// the middle of the viewport instead.
    ///
    /// This used to be half of a pair, with the pointer's last position inside the
    /// picture as the other half. The centre replaced it: see `paint_tool`.
    ///
    /// Latched rather than sampled: a row reports `changed` only on the frames its
    /// value actually moves, so a drag held still for two frames would blink the nib
    /// out. It stays set while the pointer is down, which is exactly as long as the
    /// gesture lasts.
    brush_adjusting: bool,
    /// Rasterised once at startup; see `icons`.
    icons: icons::Icons,
    /// The tile tree, and which panes are in it. See `layout`.
    layout: Layout,
    /// `Space` was pressed in the Lightbox, stolen from egui before it could press a
    /// button. Same mechanism and same reason as [`App::toggle_panels`]; see
    /// `raw_input_hook`.
    lightbox_space: bool,
    /// Photoshop-style temporary hand tool while a Dodge/Burn tool remains open.
    ///
    /// Kept as held state because `raw_input_hook` removes Space from egui before a
    /// previously focused button can also react to it. The press and release update
    /// this flag directly; the brush mode itself is never replaced.
    brush_pan: bool,
    /// `tab` was pressed, stolen from egui before it could navigate focus. See
    /// `raw_input_hook`.
    toggle_panels: bool,
    /// What the cursor was last over, sampled by the viewport for the footer.
    ///
    /// Carried on `App` rather than passed, because the footer is added to the
    /// layout *before* the central panel — panel order is outermost first — so it
    /// draws before the viewport has seen this frame's pointer. It therefore
    /// reports the previous frame's sample, which at 60fps is invisible and is the
    /// only ordering that does not require drawing the footer twice.
    readout: Option<Readout>,
    /// A history row was clicked this frame.
    ///
    /// On `App` rather than local to `ui` because the panel that sets it draws in the
    /// middle of the frame and the undo recording that reads it runs at the bottom —
    /// the same journey `toggle_panels` makes. It means exactly what the local
    /// `time_travelled` means for `⌘Z`: this frame's change to `params` was *travel
    /// along* the history, so recording it would push the state you left onto the very
    /// stack you left it on.
    travelled_by_click: bool,
    /// The History timeline last drawn, so a new state can put the current row at
    /// the foot of the panel. The tab id is part of the identity so revisiting one
    /// timeline does not make a different tab look newly changed.
    history_seen: Option<(tabs::TabId, u64, egui::ViewportId)>,
    /// The snapshot panel's SNAPSHOT button was pressed.
    ///
    /// A flag rather than a direct call, because capturing needs the `RenderState` to
    /// read the viewport back and a panel is handed only a `Ui`. Same journey
    /// `travelled_by_click` makes, and the same reason.
    capture_requested: Option<TabId>,
    /// The native menu bar, once `NSApp` exists to hand it to. `Default` until then,
    /// which is "no menu and every chord still on the keyboard" — so a platform
    /// without one, or a muda that refuses, runs exactly as the app did before.
    menus: menu::Menus,
    menus_installed: bool,
    /// The macOS auto-updater, installed on the first frame next to the menus —
    /// the earliest point at which the AppKit pieces it drive exist. `None`
    /// until then, and inert off macOS and outside a bundle; see `updater`.
    updates: Option<updater::Updates>,
    /// The update sheet (badge click, menu route, About button). View state, not
    /// a preference — nothing here is written to `settings.toml`.
    update_sheet_open: bool,
    /// A one-line message from a key whose feature is not built yet. Shown in the
    /// footer so an unbuilt binding reports itself rather than doing nothing.
    pending_note: Option<String>,
    /// The near-full-screen shortcut reference toggled with the period key.
    hotkey_hud: bool,
    /// Reusable individual curves live in Application Support, never in an image
    /// sidecar. The sheet is app-level because it remains open independently of the
    /// tab whose stack it captured.
    curve_presets: curve_presets::Store,
    curve_preset_name: Option<CurvePresetNameDialog>,
    /// What the thumbnail cache measured, the last time the Settings window asked.
    ///
    /// `None` is "not measured", which is the state it is put back into when the window
    /// closes and after a purge. Walking the directory is cheap but not free, and the
    /// number is only ever on screen in one place.
    cache_bytes: Option<u64>,
    /// App memory as it currently exists **on disk**.
    ///
    /// Compared against the live values to decide whether to write. Against the
    /// stored state rather than against the top of the frame, because some of it is
    /// set during construction — before any frame runs — and a frame-local
    /// comparison would never see that change.
    persisted: (export::Target, Option<PathBuf>),
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(worker) = self.export_thread.take() {
            let _ = worker.join();
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> Self {
        let mut app = Self {
            travelled_by_click: false,
            history_seen: None,
            capture_requested: None,
            cache_bytes: None,
            menus: menu::Menus::default(),
            menus_installed: false,
            updates: None,
            update_sheet_open: false,
            tabs: Tabs::new(),
            lightbox: lightbox::Lightbox::new(),
            queue: decode::Queue::new(decode::WORKERS),
            luma_queue: decode::Queue::new(LUMA_WORKERS),
            developed_queue: decode::Queue::new(1),
            cache: decode::Cache::default(),
            gpu: None,
            export_rx: None,
            export_thread: None,
            export_owner: None,
            export_requested: None,
            export_target: export::Target::default(),
            status: "drop a raw file on the window, or pass one on the command line".to_owned(),
            last_dir: None,
            settings: settings::Settings::default(),
            settings_open: false,
            settings_raise: false,
            settings_focused: false,
            window_focused: true,
            settings_reset_confirm: false,
            scroll_session: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
            lightbox_space: false,
            brush_pan: false,
            pending_note: None,
            hotkey_hud: false,
            curve_presets: curve_presets::Store::default(),
            curve_preset_name: None,
            brush: paint::Brush::default(),
            tool: paint::Tool::default(),
            placeholder: tabs::Tab::placeholder(),
            sheet: settings::Sheet::default(),
            confirm_quit: None,
            quitting: false,
            brush_adjusting: false,
            // Replaced in `new` once the context exists; a context is needed to
            // upload a texture and there is not one until then.
            icons: icons::Icons::empty(),
            layout: Layout::restore(cc.storage),
            toggle_panels: false,
            readout: None,
            persisted: (export::Target::default(), None),
        };

        // Preferences. Absent is a first run; corrupt is reported and the app still
        // starts, because a preferences file must never be the reason somebody
        // cannot open their photographs.
        match settings::Settings::load() {
            raw_core::sidecar::Loaded::Ok(s) => app.settings = s,
            raw_core::sidecar::Loaded::Absent => {}
            raw_core::sidecar::Loaded::Corrupt(e) => {
                app.status = format!("settings could not be read, using defaults — {e}");
            }
        }
        app.export_target.depth = app.settings.export_depth();

        let (curve_presets, preset_note) = curve_presets::Store::load();
        app.curve_presets = curve_presets;
        if let Some(note) = preset_note {
            app.status = note;
        }

        // Restore app memory. Every value is optional and a bad one falls back to
        // the default: this is a convenience, and it must never be the reason the
        // app will not start.
        //
        // **Seeded from what settings just produced, not from `Target::default`.** A
        // missing key has to fall back to the configured default and not past it —
        // starting from `default()` here threw away the depth set one line above, so a
        // user whose Settings said 8-bit got 16 until they had exported once and given
        // the key something to hold.
        if let Some(storage) = cc.storage {
            let mut t = app.export_target;
            if let Some(v) = storage.get_string(memory_keys::EXPORT_CONTAINER)
                && let Some(c) = export::Container::from_key(&v)
            {
                t.container = c;
            }
            if let Some(v) = storage.get_string(memory_keys::EXPORT_DEPTH)
                && let Some(d) = export::Depth::from_key(&v)
            {
                t.depth = d;
            }
            if let Some(v) = storage.get_string(memory_keys::EXPORT_COMPRESSION)
                && let Some(c) = export::Compression::from_key(&v)
            {
                t.compression = c;
            }
            app.export_target = t;
            app.last_dir = storage
                .get_string(memory_keys::LAST_DIR)
                .map(PathBuf::from)
                .filter(|p| p.is_dir());
            // `remember_lightbox_sort` becomes live here. The setting is read from
            // `settings.toml`, which was loaded above, so the preference decides
            // whether the remembered order is honoured on this launch.
            app.lightbox.restore(storage);
            // **Fresh start, after both trees have been restored rather than instead of
            // restoring them.** `Layout::restore` runs in the struct literal above,
            // before `settings.toml` has been read, so the preference cannot gate it
            // there — and gating the *read* would be the wrong shape anyway, because a
            // tree that fails to restore still has to heal rather than silently reset.
            //
            // Both modes, because "my panels are wrong" is not a complaint anyone makes
            // about one of them at a time. Favourites, the last folder and the window
            // geometry are untouched: none of them is panel layout.
            if app.settings.reset_panels_on_start {
                app.layout = Layout::default();
                app.lightbox.reset_tree();
            }
            if app.settings.remember_lightbox_sort {
                if let Some(s) = storage.get_string(memory_keys::LIGHTBOX_SORT)
                    && let Some(mode) = lightbox::Sort::from_key(&s)
                {
                    app.lightbox.sort = mode;
                }
                app.lightbox.descending = storage
                    .get_string(memory_keys::LIGHTBOX_SORT_DESC)
                    .is_some_and(|v| v == "true");
            }
        }
        // The UI must never scale. egui binds Cmd +/-/0 to its own UI zoom by
        // default, which grows the panels and chrome — exactly the wrong thing for
        // an image editor, where those keys belong to the image. Take the keys.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        theme::apply(&cc.egui_ctx);
        app.icons = icons::Icons::load(&cc.egui_ctx);

        // **The app opens in Lightbox**, which is the prototype's behaviour
        // (`monopro.py:36024`, unconditionally on first show) and the right default
        // for what this is: you arrive wanting to find a frame, not already holding
        // one. Develop with an empty viewport and a "drop a raw file on the window"
        // line is a worse first screen than a folder you can browse.
        //
        // **Unless a file was named**, in which case the answer to "which frame" was
        // given on the command line and the browser would be one keypress in the way.
        // Same reasoning for a file dropped on the icon, which arrives by this path.
        //
        // **`start_in_develop` overrides only the mode, never the folder.** The grid is
        // loaded either way — see the `else` arm — so turning the preference on does
        // not cost you the browser, it just stops it being the first thing you see.
        if let Some(p) = path {
            // `open` loads the file's own folder into the Lightbox on the way past, so
            // `l` works immediately here too. See `App::open`.
            app.open(p, &cc.egui_ctx);
        } else {
            app.lightbox.active = !app.settings.start_in_develop;
            // Start where the last session left off, so the app opens on the folder
            // you were working in rather than at home every time. `last_dir` is
            // already restored from storage above and is where every route in —
            // dialog, drop, command line — records the working folder.
            if let Some(dir) = app.last_dir.clone().filter(|d| d.is_dir()) {
                app.lightbox.open_folder(&dir);
            }
        }
        app
    }

    // ------------------------------------------------------------------ loading

    /// Open a file in a new tab, or report the cap.
    fn open(&mut self, path: PathBuf, ctx: &egui::Context) -> bool {
        let path = match dialogs::classify_open(path) {
            Ok(dialogs::OpenTarget::Raw(path)) => path,
            Ok(dialogs::OpenTarget::Folder(path)) => {
                let path = match std::fs::canonicalize(&path) {
                    Ok(path) => path,
                    Err(error) => {
                        self.status = format!("could not open {} — {error}", path.display());
                        return false;
                    }
                };
                self.last_dir = Some(path.clone());
                self.lightbox.open_folder(&path);
                self.lightbox.active = true;
                self.status = format!("browsing {}", path.display());
                return false;
            }
            Err(error) => {
                self.status = error;
                return false;
            }
        };
        // Whatever route the path arrived by — dialog, drop, or the command line —
        // this is now the folder the user is working in.
        //
        // Absolute, always. A path from the command line is usually relative, and a
        // relative path in persisted state resolves against whatever the working
        // directory happens to be next launch — which is silently wrong rather than
        // obviously wrong, because a folder of that name may well exist there.
        self.last_dir = path
            .parent()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
            .filter(|p| p.is_absolute());
        // **The folder follows the file into the Lightbox, silently.** the maintainer's ask, and
        // it closes a gap that only showed up once Develop could be reached without
        // passing through the grid: open a raw from the command line, from the dialog
        // or by dropping it on the window, press `l`, and the browser was empty or
        // still showing wherever you were last week. The file you are working on is the
        // best available answer to "which folder", and this makes the two agree.
        //
        // *Silently* means the mode does not change — `lightbox.active` is untouched.
        // Loading a folder is not a request to go and look at it.
        //
        // **The guard is not an optimisation.** `open_folder` re-lists the directory and
        // parses a sidecar per file, and it clears the selection and resets the scroll.
        // Doing that on the Lightbox's own double-click — which comes through here —
        // would throw away the ring and the scroll position of the grid you just opened
        // the file *from*, which is the one place the folder is certainly already right.
        //
        // Canonically, because the two sides arrive by different routes: `last_dir` is
        // canonicalised just above, while `lightbox.folder` is whatever path the folder
        // tree or the last session handed over. On a volume reached through a symlink
        // those spell the same directory differently, and a string comparison would
        // rescan every time.
        if let Some(dir) = self.last_dir.clone().filter(|d| d.is_dir()) {
            let same = self.lightbox.folder.as_deref().is_some_and(|cur| {
                cur == dir
                    || match (std::fs::canonicalize(cur), std::fs::canonicalize(&dir)) {
                        (Ok(a), Ok(b)) => a == b,
                        // A folder that cannot be canonicalised has been renamed or
                        // unmounted under us. Treat it as *different* so the grid
                        // reloads and shows what is actually there — two failures are
                        // not evidence of sameness.
                        _ => false,
                    }
            });
            if !same {
                self.lightbox.open_folder(&dir);
            }
        }
        // What the new tab starts from, before its own sidecar gets a say.
        //
        // Sticky — inheriting the tab you were just in — is the OFF position of
        // "reset develop settings when opening a new file", so the default is the
        // configured pipeline defaults. Either way a `.mono.xmp` overrides it in
        // `load_sidecar`, because a file's own edit outranks both.
        let mut opening = if self.settings.reset_on_open {
            self.settings.params()
        } else {
            self.tabs
                .active()
                .map(|t| t.params.clone())
                .unwrap_or_else(|| self.settings.params())
        };
        // **PPI is never sticky.** the maintainer's ask — the export resolution should come
        // from the preference, "not just remembering from last use" — and it is the
        // one field on `Params` where inheritance is wrong in principle rather than
        // merely unwanted.
        //
        // Everything else the sticky path carries is a *look*: an exposure, a grade, a
        // curve. Carrying those is the whole point, because the next frame off the
        // same roll wants the same treatment. Resolution is not a look. It is a fact
        // about the device the file is going to, and it changes when you change your
        // mind about the output, not when you change frames — so inheriting it meant
        // one image set to 720 for a proof quietly made 720 the house default until
        // the app was restarted.
        //
        // Applied to both routes rather than only the sticky one, so the field has a
        // single origin. `settings.params()` already sets it, so the reset path is
        // simply writing the same number twice.
        //
        // **The sidecar still outranks this**, in `load_sidecar` below: a file that
        // was printed at 360 comes back at 360. That is the same hierarchy the rest of
        // this function states — a file's own edit beats both the preference and the
        // tab you came from — and it is what makes the preference a *starting* value
        // rather than an override that would rewrite saved work on open.
        opening.output.ppi = self.settings.ppi();
        let Some(id) = self.tabs.open(&path) else {
            self.status = format!("{} tabs is the cap — close one first", tabs::MAX_TABS);
            return false;
        };
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.params = opening;
        }
        // **Develop comes forward with the picture.** the maintainer: opening an image sometimes
        // landed with another panel on top of its tab group, and the first thing you
        // want to see beside a frame you just opened is the panel you develop it with.
        //
        // `bring_forward` already refuses when the panels are hidden or the pane is
        // floating, which is what keeps this from undoing `tab` or dragging a popped-out
        // Develop back into the window — see `bringing_a_panel_forward_does_not_undo_tab_or_a_pop_out`.
        self.layout.bring_forward(Pane::Develop);
        // Before the decode: the sidecar decides the decode options, and it
        // outranks whatever the tab was seeded with.
        self.load_sidecar(id, &path);
        self.start_load(id, path, ctx);
        true
    }

    /// Ask for a file's scene image, from the cache if any tab already has it.
    ///
    /// The cache hit is the reason the key is content-based: opening the same raw
    /// in a second tab, or opening a duplicate's saved copy, is free rather than a
    /// second decode of identical bytes.
    fn start_load(&mut self, id: TabId, path: PathBuf, ctx: &egui::Context) {
        self.queue.cancel(id);
        self.luma_queue.cancel(id);
        let key = match ContentKey::of(&path) {
            Ok(k) => k,
            Err(e) => {
                if let Some(tab) = self.tabs.by_id_mut(id) {
                    tab.error = Some(e.to_string());
                    tab.status = "could not read the file".into();
                }
                return;
            }
        };
        let opts = self
            .tabs
            .by_id_mut(id)
            .map(|t| t.params.decode)
            .unwrap_or_default();

        // Both halves cached: nothing to queue at all.
        if let (Some(sensor), Some(decoded)) =
            (self.cache.sensor(key), self.cache.decoded(key, opts))
        {
            self.deliver(
                id,
                Image {
                    sensor,
                    decoded,
                    path,
                    key,
                },
            );
            return;
        }

        let priority = self.priority_of(id);
        let ctx = ctx.clone();
        if let Some(sensor) = self.cache.sensor(key) {
            // The file is already read; only this decode option pair is new.
            self.set_status(id, "re-decoding…");
            self.queue.submit(id, priority, move || {
                let decoded = Arc::new(decode::run_decode(&sensor, opts));
                ctx.request_repaint();
                Done::Loaded {
                    sensor,
                    decoded,
                    key,
                    path,
                }
            });
            return;
        }

        self.set_status(id, &format!("decoding {}…", file_name(&path)));
        self.queue.submit(id, priority, move || {
            let out = match SensorImage::load(&path) {
                Err(e) => Done::Failed(e.to_string()),
                Ok(sensor) => {
                    let sensor = Arc::new(sensor);
                    let decoded = Arc::new(decode::run_decode(&sensor, opts));
                    Done::Loaded {
                        sensor,
                        decoded,
                        key,
                        path,
                    }
                }
            };
            ctx.request_repaint();
            out
        });
    }

    /// Re-decode a tab's cached sensor after a decode-option change. No file read,
    /// and off the main thread so egui never blocks on a full sensor pass.
    fn start_redecode(&mut self, id: TabId, ctx: &egui::Context) {
        self.luma_queue.cancel(id);
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        let Some(img) = &tab.image else { return };
        let (sensor, key, opts) = (Arc::clone(&img.sensor), img.key, tab.params.decode);
        self.queue.cancel(id);

        // Another tab may already hold this exact decode — for example, duplicates
        // with Unity WB toggled opposite ways, flicked between.
        //
        // `Tab::decoded` invalidates the working image, so the luminance pass has
        // to follow it here just as it does on the queued path. Forgetting that is
        // what made toggling Unity WB blank the viewport whenever a duplicate was
        // open: the cache hit is only reachable when a second tab is holding the
        // other decode alive, so the path added for duplicates was the one that
        // broke with duplicates.
        if let Some(decoded) = self.cache.decoded(key, opts) {
            tab.decoded(decoded);
            self.start_luma(id, ctx);
            return;
        }
        tab.status = "re-decoding…".into();

        let priority = self.priority_of(id);
        let ctx = ctx.clone();
        self.queue.submit(id, priority, move || {
            let decoded = Arc::new(decode::run_decode(&sensor, opts));
            ctx.request_repaint();
            Done::Redecoded { decoded, key }
        });
    }

    /// Load the sidecar into a tab, **before its decode is queued**.
    ///
    /// The order is load-bearing. The sidecar carries decode options and the
    /// sampling mode, so reading it after the decode would mean the file was
    /// decoded with one set of options and then described by another — and nothing
    /// downstream would notice, because the frame's param diff runs against a
    /// snapshot taken after this point. Reading first means the decode is right the
    /// first time and no re-work is needed.
    ///
    /// Absent is the ordinary case and says nothing. Corrupt is *reported* — the
    /// image still opens, at defaults, but a user whose edits did not come back
    /// must be told rather than left to notice.
    fn load_sidecar(&mut self, id: TabId, path: &std::path::Path) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        match sidecar::read(path) {
            sidecar::Loaded::Absent => tab.saved = Some(tab.params.clone()),
            sidecar::Loaded::Ok(s) => {
                tab.params = s.params;
                tab.metadata = s.metadata;
                // An older sidecar is migrated on the next settled edit. Prototype
                // files contribute exposure only; later pipeline versions contribute
                // every field they know and new modules take their safe defaults.
                tab.saved = (s.schema == sidecar::SCHEMA_VERSION).then(|| tab.params.clone());
                if s.schema < sidecar::SCHEMA_VERSION {
                    tab.status = if s.schema < sidecar::PIPELINE_SCHEMA {
                        format!(
                            "{} · opened a v{} sidecar: exposure only",
                            tab.name, s.schema
                        )
                    } else {
                        format!(
                            "{} · opened a v{} sidecar; newer modules use their defaults",
                            tab.name, s.schema
                        )
                    };
                }
            }
            sidecar::Loaded::Corrupt(e) => {
                // Opening an unreadable sidecar is not itself an edit. Later writes
                // still refuse to replace it, even after the user changes params.
                tab.saved = Some(tab.params.clone());
                tab.error = Some(format!(
                    "the sidecar for this image could not be read, so it opened at \
                     defaults — the file has not been overwritten:\n{e}"
                ));
            }
        }
    }

    /// Write the sidecar if this tab owns one and its params have moved since the
    /// last write.
    ///
    /// Called when a gesture settles, which is the same signal that commits an undo
    /// entry — so the file is written once per gesture rather than once per frame,
    /// and the debouncing is the coalescing that already exists rather than a timer
    /// invented for this.
    fn save_sidecar(&mut self, id: TabId) -> bool {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return true;
        };
        match tab.save_sidecar() {
            Ok(()) => true,
            Err(why) => {
                tab.status = format!("could not save edits: {why}");
                false
            }
        }
    }

    /// Make a scratch duplicate permanent: copy the raw under the tab's own name
    /// and give the copy its own sidecar.
    ///
    /// **Copy-on-write where the filesystem has it.** Raws are immutable, so on
    /// APFS the copy costs nothing until one of the two files changes, which never
    /// happens. Elsewhere it falls back to a real copy.
    ///
    /// The copy is byte-identical, so its `ContentKey` matches the original's and
    /// the decode cache hands it the `SceneImage` that is already in memory rather
    /// than decoding the same bytes twice. That is what content keying was for.
    fn save_duplicate(&mut self, id: TabId) {
        let Some(tab) = self.tabs.by_id_mut(id) else {
            return;
        };
        if !tab.scratch {
            return;
        }
        let Some(img) = &tab.image else { return };
        let src = img.path.clone();
        let ext = src.extension().unwrap_or_default().to_os_string();

        // The tab's name is the new stem — that is what `_dupN` naming is for. If
        // something already sits there, take the next free number rather than
        // overwriting a file the user may have saved earlier.
        let dir = src.parent().unwrap_or(std::path::Path::new("."));
        let base = tabs::base_of(&tab.name).to_owned();
        let mut dest = dir.join(&tab.name).with_extension(&ext);
        let mut n = 1;
        while dest.exists() {
            n += 1;
            dest = dir.join(format!("{base}_dup{n}")).with_extension(&ext);
        }

        if let Err(e) = reflink_copy::reflink_or_copy(&src, &dest) {
            tab.error = Some(format!("could not copy the raw to {}: {e}", dest.display()));
            return;
        }
        if let Err(e) = sidecar::write(&dest, &tab.params, &tab.metadata) {
            // The raw copy landed but its sidecar did not, which would leave a copy
            // that renders at defaults. Take the copy back out rather than leave
            // that behind.
            let _ = std::fs::remove_file(&dest);
            tab.error = Some(format!("could not write the sidecar for the copy: {e}"));
            return;
        }

        // The tab now owns a file of its own, so it stops being scratch and starts
        // writing its own sidecar on every settled gesture like any other tab.
        tab.name = dest
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        tab.scratch = false;
        tab.saved = Some(tab.params.clone());
        if let Some(img) = &mut tab.image {
            img.path = dest.clone();
        }
        tab.status = format!("saved as {}", file_name(&dest));
    }

    /// Write app memory now, rather than waiting for an autosave that a quiet app
    /// never reaches. `flush` is what actually puts it on disk; `set_string` alone
    /// only marks the store dirty.
    fn persist(&mut self, frame: &mut eframe::Frame) {
        if let Some(storage) = frame.storage_mut() {
            <Self as eframe::App>::save(self, storage);
            storage.flush();
        }
        self.persisted = (self.export_target, self.last_dir.clone());
    }

    fn priority_of(&self, id: TabId) -> u32 {
        if self.tabs.active_id() == Some(id) {
            FOREGROUND
        } else {
            BACKGROUND
        }
    }

    fn set_status(&mut self, id: TabId, s: &str) {
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.status = s.to_owned();
        }
    }

    /// Attach a decoded image to a tab. Luminance is left for the render pass,
    /// which is the only place that has the device.
    fn deliver(&mut self, id: TabId, image: Image) {
        if let Some(tab) = self.tabs.by_id_mut(id) {
            tab.opening_path = None;
            tab.image = Some(image);
            tab.scene_gen += 1;
            tab.luma = None; // forces a re-derive against the new scene
            tab.luma_params = None;
            tab.view = tabs::View::default();
            // A different picture: a sample point from the last one addresses nothing.
            tab.loupe.reset();
            tab.update_status();
        }
    }

    fn poll_decode(&mut self, ctx: &egui::Context) {
        while let Some((id, done)) = self.queue.poll() {
            // A result for a closed tab. `TabId` is never reused, so this can only
            // be work that outran its cancellation.
            let Some(tab) = self.tabs.by_id_mut(id) else {
                continue;
            };
            match done.unwrap_or_else(Done::Failed) {
                Done::Failed(e) => {
                    tab.error = Some(e);
                    tab.status = "decode failed".into();
                }
                Done::Loaded {
                    sensor,
                    decoded,
                    key,
                    path,
                } => {
                    self.cache.put_sensor(key, &sensor);
                    let redecode = decoded.opts != tab.params.decode;
                    self.cache.put_decoded(key, &decoded);
                    tab.opening_path = None;
                    tab.scene_gen += 1;
                    tab.image = Some(Image {
                        sensor,
                        decoded,
                        path,
                        key,
                    });
                    tab.luma = None;
                    tab.view = tabs::View::default();
                    tab.loupe.reset();
                    tab.update_status();
                    if redecode {
                        // Options can change while the first file load is pending.
                        self.start_redecode(id, ctx);
                    } else {
                        self.start_luma(id, ctx);
                    }
                }
                Done::Redecoded { decoded, key } => {
                    self.cache.put_decoded(key, &decoded);
                    if decoded.opts != tab.params.decode
                        || tab.image.as_ref().is_none_or(|img| img.key != key)
                    {
                        continue;
                    }
                    tab.decoded(decoded);
                    // The same negative, re-derived: the tile is stale but the place
                    // the user is looking still means what it meant.
                    tab.loupe.forget();
                    self.start_luma(id, ctx);
                }
            }
        }
    }

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_rx else { return };
        let msg = match rx.try_recv() {
            Ok(msg) => msg,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("export worker stopped before reporting completion".into())
            }
        };
        if let Some(worker) = self.export_thread.take() {
            let _ = worker.join();
        }
        self.export_rx = None;
        // The machine is idle again as far as an install is concerned. If Sparkle
        // postponed a relaunch for this export, it resumes here — see `updater`.
        if let Some(updates) = &mut self.updates {
            updates.export_settled();
        }
        let (status, error) = match msg {
            Ok(path) => (format!("exported {}", file_name(&path)), None),
            Err(e) => (
                "export failed".to_owned(),
                Some(format!("export failed: {e}")),
            ),
        };
        self.status = error.clone().unwrap_or_else(|| status.clone());
        if error.is_some() {
            self.pending_note = error.clone();
        }
        if let Some(tab) = self
            .export_owner
            .take()
            .and_then(|id| self.tabs.by_id_mut(id))
        {
            tab.status = status;
            if error.is_some() {
                tab.error = error;
            }
        }
    }

    // -------------------------------------------------------------- tab actions

    fn duplicate(&mut self, rs: &egui_wgpu::RenderState) {
        if self.tabs.duplicate().is_none() {
            self.status = format!("{} tabs is the cap — close one first", tabs::MAX_TABS);
            return;
        }
        // The duplicate shares the working image but needs its own source texture.
        if let Some(tab) = self.tabs.active_mut() {
            warm_up(tab, rs);
        }
    }

    fn close(&mut self, index: usize, rs: &egui_wgpu::RenderState) {
        let closing_id = self.tabs.iter().nth(index).map(|tab| tab.id);
        if let Some(id) = closing_id
            && !self.save_sidecar(id)
        {
            if let Some(tab) = self.tabs.by_id_mut(id) {
                let why = format!(
                    "This tab remains open because its edits could not be saved. {}",
                    tab.status
                );
                self.pending_note = Some(why);
            }
            return;
        }
        let Some((id, render)) = self.tabs.close(index) else {
            return;
        };
        // Whatever was decoding for this tab is nobody's work now.
        self.queue.cancel(id);
        self.luma_queue.cancel(id);
        free_render(render, rs);
        // The scene may have been the last strong reference; drop the dead weak
        // entries rather than accumulating one per file for the session.
        self.cache.sweep();
        if let Some(active) = self.tabs.active_id() {
            self.queue.promote(active);
            self.luma_queue.promote(active);
        }
    }

    fn pick_and_open(&mut self, ctx: &egui::Context) {
        if self.tabs.is_full() {
            self.status = format!("{} tabs is the cap — close one first", tabs::MAX_TABS);
            return;
        }
        // The active image's folder first — that is where the next frame of a shoot
        // usually is — then wherever the dialog last landed.
        let start = self
            .tabs
            .active()
            .and_then(|t| t.image.as_ref())
            .and_then(|i| i.path.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| self.last_dir.clone())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Some(path) = dialogs::pick_raw(Some(&start)) {
            self.open(path, ctx);
        }
    }

    /// Make sure the active tab has the colour reference it is being asked for, and
    /// no registration for one it is not.
    ///
    /// Decoded on demand rather than at load: most sessions never press `j`, and
    /// the embedded JPEG of a 100 MP frame is not free.
    fn settle_preview(&mut self) {
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let want = tab.preview;

        // Stale registration: the view changed, so egui's texture goes back.
        if tab
            .preview_image
            .as_ref()
            .is_some_and(|(had, _)| *had != want)
            || want == tabs::PreviewSource::Mono
        {
            // Dropping the handle is the free.
            tab.preview_texture = None;
            tab.preview_image = None;
        }
        if want == tabs::PreviewSource::Mono || tab.preview_image.is_some() {
            return;
        }
        // Read before the borrow below: the raw-linear view has to be turned the same
        // way the render and the camera JPEG already are, and the tag lives on the tab.
        let orientation = tab.exif_orientation();
        // The sidecar's turn wins over the file's tag, so the reference views agree
        // with the render and with the grid.
        let upright_as = tab.params.composition.orientation;
        let effective = upright_as.unwrap_or(orientation);
        let Some(img) = &tab.image else { return };

        let made = match want {
            tabs::PreviewSource::Jpeg => raw_core::preview::embedded(&img.path, upright_as),
            tabs::PreviewSource::RawLinear => {
                Some(raw_core::preview::linear(&img.decoded.scene, effective))
            }
            tabs::PreviewSource::Mono => None,
        };
        match made {
            Some(p) => tab.preview_image = Some((want, std::sync::Arc::new(p))),
            // A file with no embedded preview is a property of the file, not a
            // failure. Say so and fall back to the render rather than showing an
            // empty viewport.
            None => {
                tab.preview = tabs::PreviewSource::Mono;
                tab.status = "this file carries no embedded preview".into();
            }
        }
    }

    /// Bring the focused tab's GPU state up, and take it down from any tab that is
    /// no longer entitled to it. See `tabs::WARM`.
    fn settle_warmth(&mut self, rs: &egui_wgpu::RenderState, ctx: &egui::Context) {
        for id in self.tabs.cold_with_render() {
            if let Some(tab) = self.tabs.by_id_mut(id) {
                let render = tab.render.take();
                free_render(render, rs);
            }
        }
        if let Some(id) = self.tabs.active_id() {
            self.luma_queue.promote(id);
            if self.tabs.by_id_mut(id).is_some_and(|tab| tab.needs_luma())
                && !self.luma_queue.is_busy(id)
            {
                self.start_luma(id, ctx);
            }
        }
        if let Some(tab) = self.tabs.active_mut() {
            warm_up(tab, rs);
        }
    }

    /// Export the active tab's render at full resolution.
    ///
    /// The GPU half runs here on the main thread — a handful of compute dispatches
    /// and a readback, fast enough not to be worth moving. Encoding is the slow half
    /// and goes to a worker, so the UI stays live through the part that takes time.
    ///
    /// What comes back from the GPU is **scene-referred f32**, not display pixels;
    /// the container decides its own encoding from there. See the `export` module.
    fn export(
        &mut self,
        ctx: &egui::Context,
        rs: &egui_wgpu::RenderState,
        owner: TabId,
        kind: ExportKind,
    ) {
        if self.export_rx.is_some() {
            self.pending_note = Some("An export is already being written.".into());
            return;
        }
        if let Some(tab) = self.tabs.active()
            && tab.has_image()
            && !tab.has_current_luma()
        {
            if tab.error.is_some() {
                self.pending_note = Some("The current image cannot be prepared for export.".into());
            } else {
                self.export_requested = Some((owner, kind));
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            return;
        }
        let settings = self.settings.clone();
        let proof = kind.is_proof().then(|| settings.proof_scale());
        let target = match kind {
            ExportKind::Master => self.export_target,
            ExportKind::Proof => settings.proof_target(),
        };
        let Some(gpu) = &mut self.gpu else { return };
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        // Read before the mutable borrow of `render` below.
        let Some(luma_dims) = tab.luma.as_ref().map(|l| l.output_dims) else {
            return;
        };
        // Before the file dialog, not after: being asked where to put a file and then
        // told it cannot be written is worse than being told first. The button is
        // greyed for the same reason, but ⌘E does not go through the button.
        // A proof is a fraction of the picture and cannot exceed it, so only a master
        // can be asked for something past the limits.
        if let Some(d) = tab.output_dims()
            && proof.is_none()
            && let Err(why) = master_layout(&tab.params, d)
        {
            tab.error = Some(why);
            return;
        }
        let (Some(render), Some(img)) = (&mut tab.render, &tab.image) else {
            return;
        };

        let suggested = settings.export_name(&img.path, target);
        // The configured output folder if there is one, otherwise beside the raw —
        // which is where an export belongs when nobody has said otherwise. A folder
        // that has since been deleted or unmounted falls back rather than opening a
        // dialog pointed at nothing.
        let start = settings
            .output_folder
            .clone()
            .filter(|d| d.is_dir())
            .or_else(|| img.path.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // **Quick Export skips the dialog when there is somewhere to skip it to.** The
        // decision is `Settings::quick_destination`, which is where its three ways of
        // saying no are written down — off, no usable folder, or a name already taken.
        let dest = match settings.quick_destination(&suggested) {
            Some(d) => d,
            None => {
                let Some(dest) = dialogs::save_file(
                    "Export Photograph",
                    &suggested,
                    Some(&start),
                    target.label(),
                    target.extension(),
                ) else {
                    return; // cancelled
                };
                dest
            }
        };

        // The composition as stored, not as being previewed: an export must not
        // depend on whether the crop tool happened to be open, which `render_params`
        // suppresses the crop for. `effective` still applies, because a bypassed
        // module exports as bypassed.
        let p = tab.params.effective();
        let frame =
            raw_core::Frame::resolve(luma_dims, img.sensor.meta.orientation, &p.composition);

        tab.status = "rendering export…".into();
        let Some((w, h, scene)) =
            render
                .viewport
                .export(gpu, &rs.device, &rs.queue, &p, &frame, |_, _| {})
        else {
            tab.error = Some("export render failed".into());
            return;
        };

        // Built from the same `effective` params the render used, so the file's
        // resolution tag, its size and its metadata cannot disagree with what the
        // Output panel was showing when the button was pressed.
        // Read at export time: IPTC can be edited in Lightbox while a Develop tab
        // remains open, and an old in-memory copy must never overwrite or omit it.
        let export_metadata =
            sidecar::effective_metadata(&img.path).unwrap_or_else(|_| tab.metadata.clone());
        let meta = settings.export_metadata.then_some(&export_metadata);
        let spec = match proof {
            Some(scale) => export::Spec::proof(
                target,
                p.display.tone_map,
                p.output,
                export::Tail::of(&p),
                meta,
                scale,
            ),
            None => export::Spec::new(
                target,
                p.display.tone_map,
                p.output,
                export::Tail::of(&p),
                meta,
            ),
        };
        // **The EXPORT module's dither checkbox, which is `display.dither`.** One flag,
        // one meaning — "break up the 8-bit quantisation" — governing the screen, which
        // is an 8-bit surface, and every 8-bit file. It is inert at 16 bits either way;
        // see `Spec::dither`.
        let spec = export::Spec {
            dither: p.display.dither,
            ..spec
        };
        // `Spec::dims`, not `output.target_dims`: a proof ignores the master's
        // resample, and asking the spec is the one way the status line and the
        // encoder cannot disagree about the size of the file.
        let out = spec.dims(raw_core::geometry::Dims {
            w: w as usize,
            h: h as usize,
        });
        tab.status = format!("writing {} ({}x{})…", file_name(&dest), out.w, out.h);
        let (tx, rx) = channel();
        self.export_rx = Some(rx);
        self.export_owner = Some(tab.id);
        // An update must not install (or relaunch) over a file still being
        // written. The badge's install buttons stand down and Sparkle's own
        // relaunch defers until `poll_export` settles this — see `updater`.
        if let Some(updates) = &self.updates {
            updates.set_busy(true);
        }
        let ctx = ctx.clone();
        self.export_thread = Some(std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                export::write(&dest, w, h, &scene, &spec)
                    .map(|()| dest)
                    .map_err(|e| e.to_string())
            }))
            .unwrap_or_else(|_| {
                Err("export encoder panicked; the destination was not replaced".into())
            });
            let _ = tx.send(result);
            ctx.request_repaint();
        }));
    }

    /// Apply a work order produced by `Params::diff`. Cheapest-first: a decode
    /// change supersedes a luminance re-derive, because the re-decode re-derives on
    /// completion anyway.
    fn apply(&mut self, id: TabId, dirty: raw_core::Dirty, ctx: &egui::Context) {
        if dirty.decode {
            self.start_redecode(id, ctx);
        } else if dirty.luminance {
            self.start_luma(id, ctx);
        }
        // `curve` and `render` need nothing here: the viewport does its own change
        // detection against the params it is handed, including re-baking the LUT.
    }
}

/// Re-derive luminance away from the UI thread.
impl App {
    fn start_luma(&mut self, id: TabId, ctx: &egui::Context) {
        let Some((source, params)) = self.tabs.by_id_mut(id).and_then(|tab| {
            let image = tab.image.as_ref()?;
            let request = (Arc::clone(&image.decoded), tab.params.luminance);
            tab.error = None;
            Some(request)
        }) else {
            return;
        };
        let priority = self.priority_of(id);
        let ctx = ctx.clone();
        self.luma_queue.submit(id, priority, move || {
            let image = Arc::new(scene::derive_luminance(
                &source.scene,
                params.sampling,
                params.weighting,
            ));
            ctx.request_repaint();
            PreparedLuma {
                source,
                params,
                image,
            }
        });
    }

    fn poll_luma(&mut self, rs: &egui_wgpu::RenderState) {
        while let Some((id, result)) = self.luma_queue.poll() {
            let Some(tab) = self.tabs.by_id_mut(id) else {
                continue;
            };
            let prepared = match result {
                Ok(prepared) if prepared.is_current(tab) => prepared,
                Ok(_) => continue,
                Err(why) => {
                    tab.error = Some(format!("luminance preparation failed: {why}"));
                    continue;
                }
            };
            install_luma(tab, prepared, rs);
        }
    }
}

fn fitted_picture_view(frame: &raw_core::Frame, edge: u32) -> (u32, u32, ViewGeometry) {
    let scale = (edge.max(1) as f32 / frame.crop.w.max(frame.crop.h) as f32).min(1.0);
    let out_w = (frame.crop.w as f32 * scale).round().max(1.0) as u32;
    let out_h = (frame.crop.h as f32 * scale).round().max(1.0) as u32;
    let scale = (out_w as f32 / frame.crop.w as f32).min(out_h as f32 / frame.crop.h as f32);
    (
        out_w,
        out_h,
        ViewGeometry {
            scale,
            off_x: frame.crop.x as f32,
            off_y: frame.crop.y as f32,
            overlays: raw_gpu::Overlays::NONE,
            surround: raw_gpu::Surround::NONE,
            background: 0.0,
        },
    )
}

/// One developed frame, decoded off-thread and waiting for the GPU.
struct DevelopedTile {
    path: PathBuf,
    stamp: lightbox::EditedTileStamp,
    luma: raw_core::LumaImage,
    params: raw_core::Params,
    orientation: raw_core::Orientation,
}

/// Stable key for a path, so a queued render can be cancelled and matched without
/// carrying the string through the queue.
fn path_key(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

impl App {
    /// Keep one developed-frame render in flight while Lightbox is showing them.
    ///
    /// **This is the half `store_edited_tile` cannot do.** That one writes the tile for
    /// the frame Develop is holding, which covers the frame you just edited and nothing
    /// else; a folder edited last week has no tile for any of it. So the grid asks for
    /// what it is missing and this renders it, in visible order, one at a time.
    fn poll_developed_tiles(&mut self, rs: &egui_wgpu::RenderState) {
        while let Some((_, got)) = self.developed_queue.poll() {
            let job = match got {
                Ok(Some(job)) => job,
                Ok(None) => continue,
                Err(why) => {
                    self.lightbox
                        .report_worker_failure(format!("developed thumbnail: {why}"));
                    continue;
                }
            };
            if !job.stamp.matches(&job.path) {
                self.lightbox.retry_developed_tile(&job.path);
                continue;
            }
            let Some(gpu) = &mut self.gpu else { continue };
            let frame = raw_core::Frame::resolve(
                job.luma.output_dims,
                job.orientation,
                &job.params.composition,
            );
            let (w, h, view) = fitted_picture_view(&frame, raw_core::preview::TILE_EDGE);
            // A viewport of its own, dropped at the end of the block: it holds the
            // whole luminance image on the GPU, which is why only one runs at a time.
            let mut vp = raw_gpu::Viewport::new(&rs.device, &rs.queue, &job.luma);
            vp.render(gpu, &rs.device, &rs.queue, w, h, view, &job.params, &frame);
            if let Some((tw, th, rgba)) = vp.read_back(&rs.device, &rs.queue) {
                if lightbox::store_edited_tile(&job.path, &job.stamp, tw, th, &rgba) {
                    self.lightbox.developed_tile_done(&job.path);
                } else if !job.stamp.matches(&job.path) {
                    self.lightbox.retry_developed_tile(&job.path);
                }
            }
        }

        if !self.lightbox.active
            || !self.settings.lightbox_xmp_thumbnails
            || self.developed_queue.in_flight() > 0
        {
            return;
        }
        let Some((_, path)) = self.lightbox.developed_tile_wanted() else {
            return;
        };
        self.developed_queue
            .submit(path_key(&path), decode::BACKGROUND, move || {
                let stamp = lightbox::EditedTileStamp::capture(&path)?;
                let sidecar = match raw_core::sidecar::read(&path) {
                    raw_core::sidecar::Loaded::Ok(s) => s,
                    _ => return None,
                };
                let params = sidecar.params.effective();
                let sensor = raw_core::sensor::SensorImage::load(&path).ok()?;
                let (scene, _) = raw_core::scene::decode(&sensor, params.decode);
                let luma = raw_core::scene::derive_luminance(
                    &scene,
                    params.luminance.sampling,
                    params.luminance.weighting,
                );
                Some(Box::new(DevelopedTile {
                    orientation: sensor.meta.orientation,
                    path,
                    stamp,
                    luma,
                    params,
                }))
            });
    }

    /// Hand Lightbox the picture Develop is holding, as a grid tile.
    ///
    /// **The cheap half of the edited-thumbnail feature**, and the only one that was
    /// built at first: Develop is already holding this picture on the GPU, so writing
    /// it down costs one small render. It covers the frame you just edited and nothing
    /// else — [`Self::poll_developed_tiles`] is what renders the rest of a folder.
    ///
    /// A dedicated target renders the crop fitted to `TILE_EDGE`. Reading the live
    /// viewport was tempting but wrong: a zoomed view cannot invent the rest of the
    /// picture, and its canvas and Surround are viewer state rather than authored
    /// image.
    fn store_edited_tile(&mut self, rs: &egui_wgpu::RenderState) {
        // Always keep the developed render. `lightbox_xmp_thumbnails` decides what
        // the grid displays; Contact Sheet has its own Developed / Gray / Color
        // choice and must not inherit that unrelated browser preference. Keeping the
        // derived tile is cheap here because Develop already has the complete render
        // resident, while trying to recreate it later would require decoding and
        // rendering every selected raw when the Contact Sheet dialog opens.
        let Some(gpu) = &mut self.gpu else { return };
        let Some(tab) = self.tabs.active_mut().filter(|t| t.has_image()) else {
            return;
        };
        // A replacement decode can still be pending while old pixels are visible.
        if self.queue.is_busy(tab.id) {
            return;
        }
        let Some(params) = tab.edited_tile_params() else {
            return;
        };
        let Some(img) = &tab.image else { return };
        let path = img.path.clone();
        let Some(stamp) = lightbox::EditedTileStamp::capture(&path) else {
            return;
        };
        let Some(saved) = raw_core::sidecar::read(&path).ok() else {
            return;
        };
        if saved.params != tab.params || !stamp.matches(&path) {
            return;
        }
        let Some(luma) = tab.luma.as_ref() else {
            return;
        };
        let frame = raw_core::Frame::resolve(
            luma.output_dims,
            tab.exif_orientation(),
            &params.composition,
        );
        let (out_w, out_h, view) = fitted_picture_view(&frame, raw_core::preview::TILE_EDGE);
        let Some(render) = tab.render.as_mut() else {
            return;
        };
        // This one-shot save already performs a blocking GPU readback and may
        // immediately close the tab. Finish its zone work rather than losing the
        // thumbnail when there will be no later frame to retry it.
        render.viewport.set_interactive_zones(false);
        let submitted = render.viewport.render_into(
            &mut render.edited_tile,
            gpu,
            &rs.device,
            &rs.queue,
            out_w,
            out_h,
            view,
            &params,
            &frame,
        );
        render.viewport.set_interactive_zones(true);
        if submitted && let Some((w, h, rgba)) = render.edited_tile.read_back(&rs.device, &rs.queue)
        {
            lightbox::store_edited_tile(&path, &stamp, w, h, &rgba);
        }
    }

    /// Cross into Lightbox, or back out of it.
    ///
    /// **One route, two doors.** `l` and the footer's tab strip both come here, so
    /// there is no way for the key to do something the click does not — which is the
    /// failure this app has already had once, when a panel could be closed by a
    /// control that did not know about the state another control set.
    ///
    /// Nothing about Develop's layout is saved or restored: Lightbox draws its own
    /// shell and Develop's panels are simply not drawn while it is up. See `lightbox`.
    fn set_lightbox(&mut self, active: bool) {
        self.lightbox.active = active;
        // **Coming back from Develop, re-check which frames have a sidecar.** Developing
        // one writes a `.mono.xmp` that the grid knows nothing about, so the edited rule
        // and the Developed sort were both answering from whenever the folder was last
        // read. See `Lightbox::refresh_edited` — a stat per entry, on a mode switch.
        if active {
            self.lightbox.refresh_edited();
        }
        // Compare and the loupe are viewport modes and the viewport is going away.
        // Closed rather than suspended, which is what `k` and the loupe's own key
        // already do — a mode you cannot see is a mode you cannot get out of.
        if active && let Some(t) = self.tabs.active_mut() {
            t.compare.open = false;
            t.loupe.open = false;
            t.loupe.forget();
            t.mode = tabs::Mode::View;
        }
    }
}

fn install_luma(tab: &mut Tab, prepared: PreparedLuma, rs: &egui_wgpu::RenderState) {
    let luma = prepared.image;
    // Refuse, with a message, an image this GPU cannot hold as a texture.
    // Reachable by switching a 100 MP sensor to DirectMosaic, and a panic deep in
    // wgpu validation is not how a user should learn that.
    let limit = raw_gpu::max_image_dim(&rs.device);
    if luma.output_dims.w as u32 > limit || luma.output_dims.h as u32 > limit {
        tab.error = Some(format!(
            "{} x {} exceeds this GPU's {limit} px texture limit — try SuperPixel sampling",
            luma.output_dims.w, luma.output_dims.h
        ));
        return;
    }
    tab.error = None;
    if let Some(render) = &mut tab.render {
        render.viewport.set_image(&rs.device, &rs.queue, &luma);
    }
    tab.luma = Some(luma);
    tab.luma_params = Some(prepared.params);
    tab.luma_gen += 1;
}

/// Give a tab everything it needs to draw: a current working image, and the GPU
/// state to draw it with. A no-op for a tab that already has both, which is the
/// common case.
fn warm_up(tab: &mut Tab, rs: &egui_wgpu::RenderState) {
    if tab.render.is_some() {
        return;
    }
    let Some(luma) = tab.luma.clone() else { return };
    let mut viewport = Viewport::new(&rs.device, &rs.queue, &luma);
    viewport.set_interactive_zones(true);
    tab.render = Some(Render {
        cells: Vec::new(),
        edited_tile: Default::default(),
        snapshot: Default::default(),
        viewport,
        samples: Default::default(),
        histogram: Default::default(),
        texture: None,
    });
}

/// Drop a tab's GPU state, freeing egui's texture registration with it. Letting
/// How much of the snapshot panel the foot bench reserves: the rule, its spacing either
/// side, and one button.
///
/// A constant rather than a measurement because the scroll area above has to be sized
/// *before* the bench is drawn — egui lays out top to bottom, so the height cannot be
/// asked for after the fact without drawing it twice.
const BENCH_H: f32 = 44.0;

/// The caption strip at the top of a compare cell: its label, and the handle you drag
/// to reorder it.
///
/// Shallow on purpose. It has to be tall enough to grab and to read a `v003` in, and
/// every point of it is taken from the picture underneath — which is the thing being
/// compared.
const CAPTION_H: f32 = 16.0;

/// Clear space around a fitted image, in points.
///
/// The developed render and both colour reference views have to use the same box.
/// Otherwise pressing `j` changes the apparent image size at the same moment it
/// changes the source, which makes comparison needlessly difficult.
const VIEW_FIT_MARGIN: f32 = 36.0;

/// Fit `img` inside `out`, both in physical pixels, leaving `margin` on every side.
fn fit_scale(out: egui::Vec2, img: egui::Vec2, margin: f32) -> f32 {
    let box_w = (out.x - 2.0 * margin).max(1.0);
    let box_h = (out.y - 2.0 * margin).max(1.0);
    (box_w / img.x.max(1.0)).min(box_h / img.y.max(1.0))
}

/// The box a snapshot's thumbnail is drawn in, in points.
///
/// The prototype's 54×40, near enough, and the shape matters more than the size: it is
/// a little wider than 4:3 so a landscape frame fills it and a portrait one is letter-
/// boxed rather than the other way round, which is the way round this corpus runs.
const THUMB_W: f32 = 54.0;
const THUMB_H: f32 = 40.0;

/// The largest rect of `aspect` that fits inside `outer`, centred.
///
/// **Fitted rather than stretched**, because the picture's shape is one of the things a
/// snapshot list is being read for: a row that squashed a 2:3 into the same box as a
/// 3:2 would be hiding the difference it exists to show.
fn fit_rect(outer: egui::Rect, size: egui::Vec2) -> egui::Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return outer;
    }
    let scale = (outer.width() / size.x).min(outer.height() / size.y);
    egui::Rect::from_center_size(outer.center(), size * scale)
}

impl App {
    /// `⌘K` — capture the active tab's look and its complete composed photograph.
    ///
    /// **The thumbnail is a dedicated fitted render**, never a capture of the viewer.
    /// It includes the stored orientation, straighten, keystone and crop, because
    /// those define the photograph, but ignores viewport zoom and pan along with the
    /// canvas, Surround and diagnostic overlays. Opening Crop or Keystone does not
    /// suppress the stored composition here: the snapshot records the look being
    /// saved, not the temporary full-frame tool view.
    ///
    /// The read-back is **blocking** — it maps a GPU buffer — which is why it happens
    /// once on a keypress and never in a loop. The GPU target is already bounded to
    /// [`snapshot::THUMB_MAX`], so the read-back is list-sized rather than viewport-
    /// sized; [`snapshot::reduce`] remains the guard that prevents accidental growth.
    ///
    /// A tab with no render yet still captures: the `Params` are the snapshot and the
    /// thumbnail is an `Option`. A capture that refused because the GPU was not ready
    /// would lose the look, which is the part that cannot be recovered.
    fn capture_snapshot(&mut self, ctx: &egui::Context, rs: &egui_wgpu::RenderState, owner: TabId) {
        if self
            .tabs
            .active()
            .is_some_and(|tab| tab.has_image() && !tab.has_current_luma() && tab.error.is_none())
        {
            self.capture_requested = Some(owner);
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
            return;
        }
        let Some(tab) = self.tabs.active_mut().filter(|t| t.has_image()) else {
            return;
        };
        let params = tab.params.effective();
        let frame = tab.stored_frame();
        if tab
            .render
            .as_mut()
            .is_some_and(|r| !r.viewport.zones_ready(&params))
        {
            self.capture_requested = Some(owner);
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
            return;
        }
        let thumb = if tab.has_current_luma() {
            self.gpu
                .as_mut()
                .zip(tab.render.as_mut())
                .zip(frame)
                .and_then(|((gpu, render), frame)| {
                    let (out_w, out_h, view) = fitted_picture_view(&frame, snapshot::THUMB_MAX);
                    render.viewport.render_into(
                        &mut render.snapshot,
                        gpu,
                        &rs.device,
                        &rs.queue,
                        out_w,
                        out_h,
                        view,
                        &params,
                        &frame,
                    );
                    render.snapshot.read_back(&rs.device, &rs.queue)
                })
                .and_then(|(w, h, rgba)| snapshot::reduce(w, h, &rgba))
                .map(|img| {
                    ctx.load_texture(
                        format!("snapshot-{}-{}", tab.name, tab.snapshots.len()),
                        img,
                        egui::TextureOptions::LINEAR,
                    )
                })
        } else {
            None
        };
        let label = tab.snapshots.capture(tab.params.clone(), thumb);
        tab.status = format!("captured {label}");
        // A snapshot you cannot see is a keypress that did nothing. The panel comes
        // forward on capture for the same reason a mode's panel does — see
        // `Layout::bring_forward`.
        self.layout.bring_forward(layout::Pane::Snapshots);
    }

    fn curve_preset_name_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.curve_preset_name.take() else {
            return;
        };
        let mut open = true;
        let mut accept = false;
        let mut cancel = false;
        egui::Window::new("Save Curve Preset")
            .id(egui::Id::new("curve-preset-name-dialog"))
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(theme::caption("Preset name").color(theme::DIM));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut dialog.value)
                        .desired_width(f32::INFINITY)
                        .vertical_align(egui::Align::Center)
                        .id_source("curve-preset-name"),
                );
                if dialog.request_focus {
                    response.request_focus();
                    dialog.request_focus = false;
                }
                let enter =
                    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!dialog.value.trim().is_empty(), egui::Button::new("Save"))
                        .clicked()
                        || enter
                    {
                        accept = !dialog.value.trim().is_empty();
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if accept {
            let name = self.curve_presets.unique_name(&dialog.value);
            self.curve_presets
                .presets
                .push(curve_presets::Preset::from_instance(
                    name.clone(),
                    &dialog.instance,
                ));
            match self.curve_presets.save() {
                Ok(()) => self.pending_note = Some(format!("saved Curve preset “{name}”")),
                Err(error) => {
                    self.curve_presets.presets.pop();
                    self.pending_note = Some(format!("could not save Curve preset — {error}"));
                }
            }
        } else if open && !cancel {
            self.curve_preset_name = Some(dialog);
        }
    }
}

/// `Render` fall out of scope would leak the registration for the session.
fn free_render(render: Option<Render>, rs: &egui_wgpu::RenderState) {
    let Some(render) = render else { return };
    let mut renderer = rs.renderer.write();
    if let Some(id) = render.texture {
        renderer.free_texture(&id);
    }
    // **And every compare cell.** They are registered the same way and leak the same
    // way — the reason this function exists at all — so a cell freed anywhere else
    // would be one more `TextureId` held for the session.
    for cell in render.cells {
        if let Some(id) = cell.texture {
            renderer.free_texture(&id);
        }
    }
}

/// Take a bare Tab inside a floating panel's own window.
///
/// **`raw_input_hook` cannot reach this.** eframe calls it in
/// `EpiIntegration::update`, once per frame, for the viewport it is updating — and
/// the floating panels are *immediate* viewports created inside `App::ui`, which
/// never go through that path. So with the panel's window holding OS focus, Tab went
/// straight to egui's focus system and `tab` did nothing but cycle widgets. the maintainer
/// found it; the first fix had closed only the main window's route.
///
/// There is no way to get ahead of it here — `Memory::begin_pass` has already spent
/// the key by the time this callback runs, and egui has no global switch to disable
/// Tab focus navigation (`Options` has none, and `EventFilter { tab }` is consulted
/// only for a widget that already has focus). So the key is read *after* the fact and
/// the focus it moved is handed back, which is the one thing that stops the
/// navigation accumulating press after press.
///
/// No double-fire with the root path: input is per viewport, so exactly one of the
/// two sees any given press.
///
/// **Call it before the window's widgets.** egui advances focus *as widgets are added*,
/// so asking afterwards reads the answer it has already moved on to — the text-field
/// guard below then sees the field egui just left rather than the one it was on, and
/// lets the key through. Both call sites do this, and
/// `tab_is_taken_inside_a_floating_panel_and_never_latches` fails if the surrender is
/// removed.
/// Take bare `Tab` — and, in the Lightbox, bare `Space` — out of an event stream.
///
/// Returns which of the two was **pressed**, and removes every event for them including
/// the releases: leaving a release behind for a press egui never saw is how a widget
/// ends up latched down.
///
/// Free rather than inline in `raw_input_hook` so it can be tested without an `App` and
/// a GPU. The hook is four lines of plumbing around it; this is the decision.
fn steal_bare_keys(events: &mut Vec<egui::Event>, steal_space: bool) -> (bool, bool) {
    let (mut tab, mut space) = (false, false);
    events.retain(|e| {
        let egui::Event::Key {
            key,
            modifiers,
            pressed,
            ..
        } = e
        else {
            return true;
        };
        if modifiers.any() {
            return true;
        }
        match key {
            egui::Key::Tab => {
                tab |= *pressed;
                false
            }
            egui::Key::Space if steal_space => {
                space |= *pressed;
                false
            }
            _ => true,
        }
    });
    (tab, space)
}

fn take_bare_tab(ctx: &egui::Context) -> bool {
    if ctx.text_edit_focused() {
        return false;
    }
    if !ctx.input(|i| i.key_pressed(egui::Key::Tab) && !i.modifiers.any()) {
        return false;
    }
    // Hand back the focus egui already moved, so the navigation does not accumulate
    // press after press — which is what "gets stuck cycling fields" was.
    ctx.memory_mut(|m| {
        if let Some(id) = m.focused() {
            m.surrender_focus(id);
        }
    });
    true
}

/// Gap after a tab, before the next one. Small, because a tab and its close are one
/// object and the duplicate belongs to the tab it sits against.
const GAP_TAB: f32 = 6.0;
/// Gap before `+`, which belongs to no tab.
const GAP_ICONS: f32 = 14.0;

/// A file tab: height, inner padding, and the gap between its close and its name.
const TAB_H: f32 = 20.0;
const TAB_PAD: f32 = 6.0;
const CLOSE_GAP: f32 = 3.0;
/// A name narrower than this is not worth reading; a name wider than this is not worth
/// the room. Between them the tab is exactly as wide as the filename.
const NAME_MIN: f32 = 56.0;
const NAME_MAX: f32 = 200.0;

/// What a file tab reported.
enum TabClick {
    None,
    Focus,
    Close,
}

/// One tab in the file strip: a fill, a close on the left, and the filename.
///
/// # Why it is painted rather than composed
///
/// The pieces have to sit inside one rect that is sensed as a whole — a click anywhere
/// on the tab focuses it, and the close takes its own click back out of the middle of
/// that. Allocating a `Label` and a button in a flow layout cannot express "these two
/// are one object with a background", and the background is what carries active state
/// now. This is the same construction as the tile-tree tabs in `layout::tab_ui`, which
/// is deliberate: the app should have one idea of what a tab is.
///
/// # Active is a lighter fill *and* ruby text
///
/// 7c chose "red text, not text on red", because a filled ruby swatch behind a filename
/// is a lot of ink for "this one" and fights the ruby that means interaction everywhere
/// else. That still holds and the ruby name stays. What is added is a **light/dark step
/// in the fill**, which the maintainer asked for from Photoshop's strip: it groups the close with
/// the name into one object, and it says which tab is in front from the corner of the
/// eye, without spending any colour on it.
///
/// # The close is on the left
///
/// Which is Photoshop's placement and, less obviously, the safer one: the close no
/// longer moves as the name gets longer or shorter, so the target under your cursor does
/// not change when you switch to a file with a different name length.
fn file_tab(
    ui: &mut egui::Ui,
    icons: &icons::Icons,
    label: &str,
    active: bool,
    name_cap: f32,
    hint: &str,
) -> TabClick {
    // The readout face, because a tab name is a filename — data the app read, not a name
    // it chose. It is also the only face that sets an underscore correctly; see the note
    // on `theme::UI_FACE`.
    let font = egui::FontId::new(theme::size::BODY - 1.0, egui::FontFamily::Monospace);
    let mut job = egui::text::LayoutJob::simple_singleline(
        label.to_owned(),
        font,
        egui::Color32::PLACEHOLDER,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(name_cap);
    let galley = ui.painter().layout_job(job);

    let width = TAB_PAD * 2.0 + icons::BOX + CLOSE_GAP + galley.rect.width();
    let (rect, tab) = ui.allocate_exact_size(egui::vec2(width, TAB_H), egui::Sense::click());
    let tab = tab
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(theme::tip(hint));

    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + TAB_PAD + icons::BOX * 0.5, rect.center().y),
        egui::Vec2::splat(icons::BOX),
    );
    // After the tab's own `interact`, so the later widget wins the click and the × closes
    // rather than switching to the tab under it.
    let close = ui
        .interact(close_rect, tab.id.with("close"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(theme::tip("close  ⌘W"));

    if ui.is_rect_visible(rect) {
        // Inactive tabs sit *below* the strip's own grey and the active one above it, so
        // the front tab reads as nearer without any colour being spent.
        let fill = match (active, tab.hovered()) {
            (true, _) => egui::Color32::from_gray(54),
            (false, false) => theme::CHROME_DEEP,
            (false, true) => egui::Color32::from_gray(44),
        };
        ui.painter().rect_filled(rect, 2.0, fill);
        let text = match (active, tab.hovered()) {
            (true, _) => theme::RUBY,
            (false, false) => egui::Color32::from_gray(170),
            (false, true) => egui::Color32::from_gray(215),
        };
        let hot = close.hovered();
        if hot {
            ui.painter()
                .rect_filled(close_rect, 2.0, egui::Color32::from_gray(72));
        }
        icons::paint(
            ui,
            icons,
            "close",
            "×",
            close_rect,
            if hot {
                egui::Color32::from_gray(245)
            } else {
                theme::DIM
            },
        );
        ui.painter().galley(
            egui::pos2(
                close_rect.right() + CLOSE_GAP,
                rect.center().y - galley.rect.height() * 0.5,
            ),
            galley,
            text,
        );
    }

    if close.clicked() {
        TabClick::Close
    } else if tab.clicked() {
        TabClick::Focus
    } else {
        TabClick::None
    }
}

/// Right margin on the develop panel, wide enough that the floating scrollbar has
/// somewhere to be.
///
/// The bar overlays content rather than allocating space — that is what keeps it
/// nearly invisible until you reach for it — so the content has to leave it room
/// rather than the other way round. Comfortably wider than egui's expanded bar, so
/// the module borders stay clear even while it is being dragged.
const SCROLL_GUTTER: i8 = 10;

/// The value under the cursor.
#[derive(Clone, Copy)]
enum Readout {
    /// The print, which is what the viewport is showing.
    Print {
        /// 0-100 from the developed image at working resolution. Export-size grain
        /// and output sharpening are outside this live sample.
        ///
        /// **The only tonal number here since the footer stopped carrying EV.** The
        /// scene value is still measurable — `Histogram::sample_undeveloped`, which is
        /// what the Inspector's `RAW` toggle reads — it is simply not a thing the footer
        /// reports any more.
        lstar: f32,
        /// CIELAB `a*` and `b*`, when the sampled finished image is toned.
        lab: Option<(f32, f32)>,
    },
    /// A colour reference view: the camera JPEG, or the raw linear preview.
    ///
    /// **What is displayed rules the readout**, which is the maintainer's rule and why this is
    /// a variant rather than a field added to `Print`. The pipeline is monochrome, so
    /// the print's `L*` and `EV` describe a picture that is not on screen while one of
    /// these views is up; reporting them here would be the plausible wrong number the
    /// sampler is careful about everywhere else.
    ///
    /// **Sampled as bytes, presented as the setting says.** The sample is the thing
    /// that was measured and stays raw; `ReferenceValues` decides whether the footer
    /// shows it as `L*a*b*` — the default, and the reason the readout exists, since
    /// that `L*` and the print's are the same axis and subtract — or as `R G B`.
    Reference { rgb: [u8; 3] },
}

#[derive(Clone, Copy, PartialEq)]
enum SampleTarget {
    Cursor,
    Pin(tabs::Pin),
}

#[derive(Clone, PartialEq)]
struct SampleContext {
    params: Arc<raw_core::Params>,
    frame: raw_core::Frame,
}

#[derive(Clone, Copy, PartialEq)]
struct WantedSample {
    target: SampleTarget,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

#[derive(Clone, Copy)]
struct FinishedSample {
    lstar: f32,
    lab: Option<(f32, f32)>,
}

struct PendingSample {
    context: SampleContext,
    wanted: WantedSample,
    readback: raw_gpu::PendingPatch,
}

#[derive(Default)]
pub(crate) struct FinishedSampler {
    context: Option<SampleContext>,
    wanted: Vec<WantedSample>,
    ready: Vec<(WantedSample, FinishedSample)>,
    failed: Vec<WantedSample>,
    pending: Option<PendingSample>,
}

impl FinishedSampler {
    fn prepare(
        &mut self,
        params: Arc<raw_core::Params>,
        frame: raw_core::Frame,
        area: settings::SampleArea,
        cursor: Option<(f32, f32)>,
        pins: &[tabs::Pin],
    ) {
        let context = SampleContext { params, frame };
        if self.context.as_ref() != Some(&context) {
            self.context = Some(context);
            self.ready.clear();
            self.failed.clear();
        }
        self.wanted.clear();
        if let Some((x, y)) = cursor
            && let Some(wanted) = wanted_sample(SampleTarget::Cursor, frame, x, y, area)
        {
            self.wanted.push(wanted);
        }
        self.wanted.extend(pins.iter().filter_map(|pin| {
            let (x, y) = frame.from_source(pin.x, pin.y);
            wanted_sample(SampleTarget::Pin(*pin), frame, x, y, area)
        }));
        self.ready
            .retain(|(sample, _)| self.wanted.contains(sample));
        self.failed.retain(|sample| self.wanted.contains(sample));
    }

    fn update(
        &mut self,
        viewport: &mut Viewport,
        gpu: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> bool {
        if let Some(pending) = &mut self.pending
            && let Some(result) = pending.readback.poll(device, gpu)
        {
            let pending = self
                .pending
                .take()
                .expect("the completed sample is pending");
            if self.context.as_ref() == Some(&pending.context)
                && self.wanted.contains(&pending.wanted)
            {
                match result {
                    Ok((_, _, scene)) => {
                        let value = finish_sample(&scene, &pending.context.params);
                        self.store(&pending.context, pending.wanted, value);
                    }
                    Err(_) => self.failed.push(pending.wanted),
                }
            }
        }
        if self.pending.is_none()
            && let Some(context) = self.context.clone()
            && let Some(wanted) = self.wanted.iter().copied().find(|wanted| {
                !self.ready.iter().any(|(ready, _)| ready == wanted)
                    && !self.failed.contains(wanted)
            })
            && let Some(readback) = viewport.begin_patch(
                gpu,
                device,
                queue,
                &context.params,
                &context.frame,
                wanted.x,
                wanted.y,
                wanted.w,
                wanted.h,
            )
        {
            self.pending = Some(PendingSample {
                context,
                wanted,
                readback,
            });
        }
        self.pending.is_some()
            || self.wanted.iter().any(|wanted| {
                !self.ready.iter().any(|(ready, _)| ready == wanted)
                    && !self.failed.contains(wanted)
            })
    }

    fn get(&self, target: SampleTarget) -> Option<FinishedSample> {
        self.ready
            .iter()
            .find(|(wanted, _)| wanted.target == target)
            .map(|(_, value)| *value)
    }

    fn store(
        &mut self,
        context: &SampleContext,
        wanted: WantedSample,
        value: FinishedSample,
    ) -> bool {
        if self.context.as_ref() != Some(context) || !self.wanted.contains(&wanted) {
            return false;
        }
        self.ready.retain(|(ready, _)| *ready != wanted);
        self.ready.push((wanted, value));
        true
    }
}

fn wanted_sample(
    target: SampleTarget,
    frame: raw_core::Frame,
    x: f32,
    y: f32,
    area: settings::SampleArea,
) -> Option<WantedSample> {
    let (cx, cy) = (x.floor() as i32, y.floor() as i32);
    if cx < 0 || cy < 0 || cx >= frame.frame.w as i32 || cy >= frame.frame.h as i32 {
        return None;
    }
    let radius = area.radius();
    let x0 = (cx - radius).max(0);
    let y0 = (cy - radius).max(0);
    let x1 = (cx + radius).min(frame.frame.w as i32 - 1);
    let y1 = (cy + radius).min(frame.frame.h as i32 - 1);
    Some(WantedSample {
        target,
        x: x0,
        y: y0,
        w: (x1 - x0 + 1) as u32,
        h: (y1 - y0 + 1) as u32,
    })
}

fn finish_sample(scene: &[f32], params: &raw_core::Params) -> FinishedSample {
    let mut lstar = 0.0;
    let mut lab = params.toning.is_active().then_some((0.0, 0.0));
    for &scene in scene {
        let y = raw_core::display::tone_map(scene, params.display.tone_map);
        if let Some((sum_a, sum_b)) = &mut lab {
            let toned = params.toning.evaluate(y);
            let (a, b) = toned.ab();
            let value = raw_core::colour::lab_of(toned.y, a, b);
            lstar += value[0];
            *sum_a += value[1];
            *sum_b += value[2];
        } else {
            lstar += raw_core::display::lstar_encode(y) * 100.0;
        }
    }
    let n = scene.len().max(1) as f32;
    FinishedSample {
        lstar: lstar / n,
        lab: lab.map(|(a, b)| (a / n, b / n)),
    }
}

/// The undeveloped working-image value at a source pixel, averaged in linear space.
/// Used by RAW pins, the curve point picker and optical placement. Finished-image
/// readouts use the asynchronous GPU sampler above.
///
/// # The divisor is what was read, not the window's area
///
/// Against an edge the window is partial. Dividing by `edge²` would darken every
/// sample near a border — and darken it plausibly, which is the expensive kind of
/// wrong for a readout. `n` counts the pixels that actually existed.
///
/// Returns `None` when the centre is outside the image, so that pointing off the
/// picture reports nothing rather than reporting the nearest edge.
fn sample_luma(
    luma: &raw_core::scene::LumaImage,
    sx: f32,
    sy: f32,
    area: settings::SampleArea,
) -> Option<f32> {
    let (lw, lh) = (luma.output_dims.w as i32, luma.output_dims.h as i32);
    let (cx, cy) = (sx.floor() as i32, sy.floor() as i32);
    if cx < 0 || cy < 0 || cx >= lw || cy >= lh {
        return None;
    }
    let r = area.radius();
    if r == 0 {
        return luma
            .data
            .get(cy as usize * lw as usize + cx as usize)
            .copied();
    }
    let (mut sum, mut n) = (0.0f32, 0u32);
    for y in (cy - r).max(0)..=(cy + r).min(lh - 1) {
        for x in (cx - r).max(0)..=(cx + r).min(lw - 1) {
            sum += luma.data[y as usize * lw as usize + x as usize];
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f32)
}

/// A compact display-referred proxy for FRAME's one-shot Optical placement.
///
/// The visual-centre calculation belongs to the photograph being framed, not to
/// the source sensor rectangle: crop and orientation are resolved first, and each
/// sample is passed through the same tone mapping as the histogram/readout. Keeping
/// the proxy to 128 px on its long side makes the reference algorithm effectively
/// instant while retaining the large masses that determine visual balance.
fn frame_visual_center(tab: &Tab) -> Option<[f32; 2]> {
    const LONG_SIDE: f32 = 128.0;

    let luma = tab.luma.as_ref()?;
    let frame = tab.stored_frame()?;
    let picture = frame.output_dims();
    let scale = (LONG_SIDE / picture.w.max(picture.h) as f32).min(1.0);
    let width = (picture.w as f32 * scale).round().max(1.0) as usize;
    let height = (picture.h as f32 * scale).round().max(1.0) as usize;
    let mut proxy = Vec::with_capacity(width * height);

    let sample = |sx: f32, sy: f32| {
        let (w, h) = (luma.output_dims.w, luma.output_dims.h);
        if !sx.is_finite()
            || !sy.is_finite()
            || sx < 0.0
            || sy < 0.0
            || sx > (w.saturating_sub(1)) as f32
            || sy > (h.saturating_sub(1)) as f32
        {
            return None;
        }
        let (x0, y0) = (sx.floor() as usize, sy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (tx, ty) = (sx - x0 as f32, sy - y0 as f32);
        let at = |x: usize, y: usize| luma.data[y * w + x];
        let top = at(x0, y0) * (1.0 - tx) + at(x1, y0) * tx;
        let bottom = at(x0, y1) * (1.0 - tx) + at(x1, y1) * tx;
        Some(top * (1.0 - ty) + bottom * ty)
    };

    for y in 0..height {
        for x in 0..width {
            let fx = frame.crop.x as f32 + (x as f32 + 0.5) * frame.crop.w as f32 / width as f32;
            let fy = frame.crop.y as f32 + (y as f32 + 0.5) * frame.crop.h as f32 / height as f32;
            let (sx, sy) = frame.to_source(fx, fy);
            let raw = sample(sx, sy).unwrap_or(0.0);
            let displayed = tab
                .histogram
                .sample(raw)
                .map(|(lstar, _)| lstar / 100.0)
                .unwrap_or_else(|| raw.clamp(0.0, 1.0));
            proxy.push(displayed);
        }
    }
    raw_core::frame::visual_center(&proxy, width, height)
}

/// `50mm · ISO 400 · f/2.0 · 1/250`, for the top-centre readout.
///
/// **Focal length, then ISO, aperture, shutter** — the maintainer's order, and it is the order
/// the decision is made in rather than the order EXIF lists them: you frame, then you
/// set a speed, then you expose.
///
/// # The lens name is not here, and the focal length is
///
/// the maintainer:
///
/// > I didn't need the lens name in the exif header readout — just at what mm the photo
/// > was taken. Should just read "50mm" or "28mm" or if a zoom lens "61mm".
///
/// The line carried the lens name and dropped the focal length, which is the wrong way
/// round twice over. **A lens name is an inventory fact, not an exposure one** — it is
/// the same string on every frame you shot that day, so it takes the most space in the
/// chrome and carries the least. What changes between two frames is where the zoom was,
/// and that was the part being thrown away. It also read badly at length:
/// `GF45-100mmF4 R LM OIS WR` is twenty-four characters of a four-field readout.
///
/// It cost the rule this line used to need, too. Suppressing the focal length when the
/// name already stated it — a prime writes `85mm`, a zoom writes `45-100mm` — was real
/// and worked, and with no name in the line there is nothing to duplicate, so it is
/// gone rather than kept for a case that cannot arise.
///
/// **Nothing is lost from the app.** Lightbox Metadata lists the lens by name beside
/// its own focal row, which is where an inventory fact belongs.
///
/// No labels: these are the numbers every photographer reads in this order without
/// being told which is which, and spelling them out would make a caption of something
/// that should be glanced at. A field the file did not record is dropped rather than
/// shown as a dash — an em-dash in a row of exposure values reads as "this was zero",
/// which is a different claim from "unknown".
fn exposure_line(m: &raw_core::sensor::Metadata) -> String {
    // **Present is not the same as meaningful.** A Fuji file in the corpus records
    // `aperture: Some(NaN)` and `focal_len: Some(0.0)` — an adapted manual lens the
    // body could not interrogate — and printing them gave `f/NaN · 0 mm` in the
    // chrome. A value the camera could not measure is missing, whatever the tag
    // says, so it is filtered the same way an absent one is.
    let real = |v: Option<f32>| v.filter(|x| x.is_finite() && *x > 0.0);
    [
        real(m.focal_len).map(focal_label),
        m.iso.filter(|i| *i > 0).map(|i| format!("ISO {i}")),
        real(m.aperture).map(|a| format!("f/{a:.1}")),
        m.shutter_label(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("  ·  ")
}

/// `50mm`, or `15.4mm` where the body recorded a fraction.
///
/// **No space before the unit** — the maintainer's form, and it is how a focal length is written
/// everywhere it is written by photographers rather than by instruments: it is the name
/// of a lens, not a measurement of one. `50 mm` in a row of `ISO 400 · f/2.0` reads as a
/// fifth field.
///
/// Whole numbers lose their `.0`, which is most lenses. The ones that are not — a
/// compact's 15.4, a phone's 17.7 — are real and keep their digit rather than being
/// rounded into a lens that does not exist.
///
/// Shared with the Info panel's `focal` row, which had its own `{:.0} mm` and was
/// printing that compact as a flat `15 mm`.
fn focal_label(f: f32) -> String {
    if (f - f.round()).abs() < 0.05 {
        format!("{f:.0}mm")
    } else {
        format!("{f:.1}mm")
    }
}

/// A new grain seed.
///
/// The clock rather than a counter or a crate: the only requirement is that two
/// presses give two different emulsions, and the only thing that must *not* happen is
/// every image on a machine getting the same one. `RandomState` is the standard
/// no-dependency source of entropy in `std` — it is what `HashMap` seeds itself from
/// — and hashing the instant through it means neither a fast double-press nor a
/// coarse clock can collide.
///
/// **Reduced to six digits**, because the seed is now a field you can type into and a
/// rolled one has to be a number you would be willing to write down. See
/// `raw_core::grain::SEED_MAX`.
///
/// Deliberately not a random *default*: an untouched image has seed 42 so that a file
/// with no grain block in its sidecar and a file written today grain identically.
fn fresh_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    h.finish() % (raw_core::grain::SEED_MAX + 1)
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The sampling menu. `Demosaic` appears once, carrying whichever algorithm is
/// selected; the algorithm is a second control that appears when it is chosen, the
/// same shape as the Weighted sliders and the Soft shoulder controls. Flattening
/// the algorithms into this list would put five entries that differ subtly in front
/// of a user choosing between three that differ completely.
/// **Most usable first, which is the order `DemosaicAlgo::UI_ORDER` already claims for
/// itself.** This listed SuperPixel first because it is the shipped default and the mode
/// with the strongest claim to honesty — no interpolation, no invented data. the maintainer
/// reordered it on use: Demosaic is the one that gives a full-resolution picture and
/// works correctly under every weighting, so it is what somebody opening the menu most
/// often wants. SuperPixel is the measurement, DirectMosaic is the diagnostic, and both
/// keep their place in a list read top to bottom.
///
/// The order is **not** the default — see `Sampling::default`. A menu leading with
/// something the app does not start on is fine; the alternative is a default chosen by
/// where it happened to sit in a list.
const SAMPLING_ORDER: [Sampling; 3] = [
    Sampling::Demosaic(DemosaicAlgo::Rcd),
    Sampling::SuperPixel,
    Sampling::DirectMosaic,
];

fn sampling_label(s: Sampling) -> String {
    match s {
        Sampling::SuperPixel => "SuperPixel (½ res, binned)".into(),
        Sampling::DirectMosaic => "DirectMosaic (full res, mosaic)".into(),
        Sampling::Demosaic(a) => format!("Demosaic (full res, {})", a.label()),
    }
}

/// What the tab strip was asked to do. Returned rather than done in place: the
/// strip is iterating the tab list while it draws, and closing or reordering
/// underneath that iteration is how index bugs start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripAction {
    None,
    Focus(usize),
    Close(usize),
    Open,
    Duplicate,
    /// Make the active scratch duplicate permanent.
    SaveDuplicate,
}

/// Storage keys for the app's own memory. Window geometry is eframe's and needs no
/// key here; the tile tree keeps its own keys in `layout`.
mod memory_keys {
    pub const EXPORT_CONTAINER: &str = "export.container";
    pub const EXPORT_DEPTH: &str = "export.depth";
    pub const EXPORT_COMPRESSION: &str = "export.compression";
    pub const LAST_DIR: &str = "open.last_dir";
    pub const LIGHTBOX_SORT: &str = "lightbox.sort";
    pub const LIGHTBOX_SORT_DESC: &str = "lightbox.sort_desc";
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::CHROME_DEEP.to_normalized_gamma_f32()
    }

    /// App **memory**, not settings.
    ///
    /// The distinction decides where things live: nobody configures where the
    /// window was or which folder they last opened, so those are remembered. A
    /// *setting* is something the user chooses, and a stored choice with no UI to
    /// change it is indistinguishable from one that does nothing. Preferences therefore
    /// live in `settings.toml`; this block keeps only state the app remembers for you.
    ///
    /// Written as plain strings rather than sharing the serialised Settings struct.
    /// The two files have different meanings and migration surfaces even though they
    /// sit beside each other.
    /// Take `tab` away from egui before it can spend it on focus navigation.
    ///
    /// **This has to happen here and cannot happen in `ui`.** egui's focus system
    /// reads the event list in `begin_pass`, which runs *before* the app's `ui`, and
    /// turns a bare Tab into "focus the next widget". So the key fired twice: it
    /// cycled focus through the panel's controls *and* reached our table. Worse, the
    /// focus it left behind then tripped the table's own guard — bare keys are
    /// suppressed while a widget has focus, so the second press did nothing and the
    /// panel toggle appeared to need a click on the image to "wake up". That is the
    /// bug; it was never about the image.
    ///
    /// `raw_input_hook` is the only place upstream of `begin_pass`, so the event is
    /// removed from the stream and remembered instead. Everything else about the
    /// binding stays in `hotkeys::TABLE` — this steals the key, it does not decide
    /// what the key does.
    ///
    /// Typing still wins: while a text field has focus, Tab is left alone and does
    /// whatever egui would normally do with it.
    ///
    /// **The guard has to be `text_edit_focused`, not "anything is focused".** It was
    /// the latter and that is a latch, not a guard: the first Tab that got through
    /// reached egui's focus system, egui focused a widget, and from then on this hook
    /// returned early forever — so `tab` hid the panels once and then did nothing but
    /// cycle focus, with the panels unreachable because it was the only way back.
    /// `egui_wants_keyboard_input` is the same trap under a better name; it is
    /// literally `focused().is_some()`. `text_edit_focused` asks the question the doc
    /// comment above was always describing.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if ctx.text_edit_focused() {
            self.brush_pan = false;
            return;
        }
        // **`Space` in the Lightbox and Dodge/Burn is stolen for the same reason
        // `Tab` is.** the maintainer saw
        // a flash of ruby around the footer buttons every time he pressed it to preview:
        // egui treats Space as "activate the focused widget", so the press was opening
        // the preview *and* pushing whatever button last held focus — LIGHTBOX or
        // DEVELOP, which are exactly the two that wear ruby when active. Nothing was
        // being clicked; the button was drawing itself pressed, which is worse, because
        // it looks like the mode is about to change and then does not.
        //
        // It cannot be fixed at the read site. By the time `ui` runs, `begin_pass` has
        // already handed the event to the focused widget — the same ordering that made
        // `Tab` cycle focus before reaching the table.
        let temporary_hand = self
            .tabs
            .active()
            .is_some_and(|tab| tab.mode.is_paint() || tab.mode.is_keystone());
        if !temporary_hand || !raw_input.focused {
            self.brush_pan = false;
        } else {
            // Read the transition before `steal_bare_keys` removes it. Holding Space
            // temporarily lends the primary drag to pan; the release gives it back to
            // the still-open brush. Modified Space remains somebody else's chord.
            for event in &raw_input.events {
                if let egui::Event::Key {
                    key: egui::Key::Space,
                    modifiers,
                    pressed,
                    ..
                } = event
                    && !modifiers.any()
                {
                    self.brush_pan = *pressed;
                }
            }
        }
        let (tab, space) = steal_bare_keys(
            &mut raw_input.events,
            self.lightbox.active || temporary_hand,
        );
        self.toggle_panels |= tab;
        self.lightbox_space |= space;
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(
            memory_keys::EXPORT_CONTAINER,
            self.export_target.container.key().into(),
        );
        storage.set_string(
            memory_keys::EXPORT_DEPTH,
            self.export_target.depth.key().into(),
        );
        storage.set_string(
            memory_keys::EXPORT_COMPRESSION,
            self.export_target.compression.key().into(),
        );
        if let Some(d) = &self.last_dir {
            storage.set_string(memory_keys::LAST_DIR, d.to_string_lossy().into_owned());
        }
        // **Written only when the preference is on.** Off means the app opens sorted
        // by filename every time, and leaving a stale key behind would make turning
        // the preference back on restore an order from whenever it was last off.
        if self.settings.remember_lightbox_sort {
            storage.set_string(memory_keys::LIGHTBOX_SORT, self.lightbox.sort.key().into());
            storage.set_string(
                memory_keys::LIGHTBOX_SORT_DESC,
                self.lightbox.descending.to_string(),
            );
        }
        // The one thing here that is not four plain strings. A tile tree is not a
        // shape anyone would hand-write, and `egui_tiles` derives `serde` for it, so
        // it goes through eframe's own RON helpers rather than an encoding invented
        // for it here.
        self.lightbox.save(storage);
        self.layout.save(storage);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let rs = frame.wgpu_render_state().expect("wgpu backend").clone();
        if self.gpu.is_none() {
            self.gpu = Some(GpuContext::new(&rs.device));
        }
        self.poll_decode(ui.ctx());
        self.poll_luma(&rs);
        self.poll_developed_tiles(&rs);
        self.poll_export();
        if self.export_rx.is_some()
            || self.queue.in_flight() > 0
            || self.luma_queue.in_flight() > 0
            || self.developed_queue.in_flight() > 0
        {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Sampling is shared by every panel, including Toning and the Inspector.
        if let Some(tab) = self.tabs.active_mut() {
            refresh_tab_mapping(tab);
        }
        let ctx = ui.ctx().clone();

        // A sidecar can appear while the window is in the background. See
        // `window_focused`, and `Lightbox::refresh_edited` for what the stat buys.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused && !self.window_focused && self.lightbox.active {
            self.lightbox.refresh_edited();
        }
        self.window_focused = focused;

        // (1) Snapshot. Everything below mutates the active tab's params freely;
        // the diff at the bottom works out what that cost. The id travels with it
        // because the active tab can *change* during the frame, and diffing one
        // tab's params against another's would fabricate an edit and a decode.
        let before = self.tabs.active().map(|t| (t.id, t.params.clone()));
        // `⌘Z`/`⌘⇧Z` set this below. **A history row's click cannot**, because the
        // panel draws in the middle of the frame and this is the top of it — the click
        // arrives on `self.travelled_by_click` and is read where it is used, not here.
        // Reading it here is the bug that made clicking back in time destroy the redo:
        // the flag was always still false when the recording asked.
        let mut time_travelled = false;

        for p in ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect::<Vec<_>>()
        }) {
            self.open(p, &ctx);
        }

        // Both panels at once. Hiding them one at a time would be two keys for a
        // gesture whose whole purpose is "get out of the way".
        // **Whichever mode is up owns the key.** It arrives here rather than through
        // the table because `raw_input_hook` has to take Tab before egui spends it on
        // focus — which is why the table's arm never fired for Lightbox.
        if std::mem::take(&mut self.toggle_panels) && !self.hotkey_hud {
            if self.lightbox.active {
                self.lightbox.toggle_panels();
            } else {
                self.layout.toggle_panels();
            }
        }

        theme::tooltips(&ctx, self.settings.tooltips);
        self.guard_quit(&ctx);

        // Keys handled here, before the UI, so restored params flow through exactly
        // the same diff-and-apply path as a slider drag.
        let mut action = StripAction::None;
        // One dispatch, from the table. Every binding's modifiers are matched
        // exactly and every bare key is suppressed while a text field has focus —
        // both of which used to be each call site's problem. See `hotkeys`.
        // Whether an interaction mode is claiming its own chords this frame. Only
        // the brush does today; `Mode::claims_keys` is where the next one says so.
        let modal = self.tabs.active().is_some_and(|t| t.mode.is_paint());
        // **Installed on the first frame**, which is the earliest `NSApp` exists —
        // eframe creates it during startup and offers no callback that says so.
        if !self.menus_installed {
            self.menus_installed = true;
            self.menus = menu::Menus::install("monopro");
        }
        // The updater follows the same rule as the menus above: first frame, when
        // the AppKit pieces it drives exist. It runs silent scheduled checks on
        // the stable feed and turns into a badge when it finds something; see
        // `updater`. A failed install is not fatal — the app updates by hand.
        if self.updates.is_none() {
            self.updates = Some(updater::Updates::install(&self.settings, &ctx));
        }
        // Sparkle's events drain once per frame, next to the export worker's.
        // A one-line note goes to the footer; the badge and the sheet state
        // update inside `updates`.
        let update_note = self
            .updates
            .as_mut()
            .and_then(|updates| updates.poll(&mut self.settings));
        if let Some(note) = update_note {
            self.pending_note = Some(note);
        }
        let mut actions = hotkeys::pressed(&ctx, modal, self.settings.hotkeys_enabled);
        // **A chord the menu owns must not also fire from the keyboard.** On macOS
        // AppKit consumes it before egui sees it, so this normally removes nothing —
        // it is here because a chord that slipped through would run its command twice,
        // and a doubled undo is two steps of work gone with nothing on screen to say
        // why. See `menu::Menus::claims`.
        actions.retain(|a| !self.menus.claims(*a));
        // Menu clicks join the keypresses, so both take the identical path through the
        // dispatch below — one command, one route.
        for cmd in menu::Menus::pressed() {
            match cmd {
                menu::Command::Key(a) => actions.push(a),
                menu::Command::Show(p) => self.layout.bring_forward(p),
                // Crossing modes is part of the act: asking for FOLDERS from Develop
                // can only mean you want to be looking at folders.
                menu::Command::ShowLightbox(p) => {
                    if !self.lightbox.active {
                        if let Some(id) = self.tabs.active_id() {
                            self.save_sidecar(id);
                        }
                        self.store_edited_tile(&rs);
                        self.set_lightbox(true);
                    }
                    self.lightbox.reveal_pane(p);
                }
                // The manual check opens the sheet with it, so the result —
                // including "up to date" and any failure — has somewhere to land.
                menu::Command::CheckForUpdates => {
                    if let Some(updates) = &mut self.updates {
                        updates.check_now(&mut self.settings);
                        self.update_sheet_open = true;
                    }
                }
            }
        }
        // The HUD is modal while it is visible: shortcuts are being read, not used.
        // Period and Escape are its two exits; every other binding is discarded so
        // studying the reference cannot alter the image behind it.
        if self.hotkey_hud {
            let close = actions
                .iter()
                .any(|a| matches!(a, hotkeys::Action::HotkeyHud | hotkeys::Action::ExitMode));
            actions.clear();
            if close {
                self.hotkey_hud = false;
            }
        }
        // After the HUD has discarded commands, but before any editor is drawn:
        // menu Undo/Redo belongs to focused text just like keyboard Undo/Redo.
        hotkeys::route_text_history(&ctx, &mut actions);
        // Any keypress clears the last "not built yet" note, so it reads as a reply
        // to what was just pressed rather than lingering over unrelated work.
        if !actions.is_empty() {
            self.pending_note = None;
        }
        for a in &actions {
            match a {
                hotkeys::Action::Undo | hotkeys::Action::Redo => {
                    if let Some(tab) = self.tabs.active_mut() {
                        time_travelled = if *a == hotkeys::Action::Redo {
                            tab.history.redo(&mut tab.params)
                        } else {
                            tab.history.undo(&mut tab.params)
                        };
                    }
                }
                hotkeys::Action::Export => {
                    self.export_requested =
                        self.tabs.active_id().map(|id| (id, ExportKind::Master));
                }
                hotkeys::Action::ExportProof => {
                    self.export_requested = self.tabs.active_id().map(|id| (id, ExportKind::Proof));
                }
                hotkeys::Action::OpenFile => action = StripAction::Open,
                hotkeys::Action::DuplicateTab => action = StripAction::Duplicate,
                hotkeys::Action::SaveDuplicate => action = StripAction::SaveDuplicate,
                // **Not while Settings is open**, which is the maintainer's call and fixes a
                // defect this app introduced by wiring `⌘W` into the settings
                // viewport: he pressed it meaning "put settings away" and it closed
                // the picture instead.
                //
                // The cause is that the two windows do not agree about who has the
                // key. Settings is a real OS viewport, so when it is *focused* the
                // keystroke goes to it and never reaches here — but when it is open
                // and the main window is in front, it reaches here and closes a tab.
                // Whichever window happened to be frontmost therefore decided whether
                // `⌘W` was destructive, which is not a thing a user can hold in mind.
                //
                // So `⌘W` means one thing while that window exists: nothing here. The
                // asymmetry is deliberate — an ignored key costs a second press, and
                // the alternative cost a closed image.
                hotkeys::Action::CloseTab if !self.settings_open => {
                    action = StripAction::Close(self.tabs.active_index());
                }
                hotkeys::Action::CloseTab => {
                    self.pending_note = Some("⌘W closes Settings while it is open".into());
                }
                hotkeys::Action::CycleTabs => self.tabs.step(1),
                hotkeys::Action::CycleTabsBack => self.tabs.step(-1),
                // Not the same as cycling: this toggles a *pair*, which is what
                // flicker comparison needs and what walking a strip of three or
                // more cannot do. See `docs/decisions.md`.
                hotkeys::Action::FlickTab => self.tabs.flicker(),
                // Opens it, and closes it when the main window is the one with
                // focus. The settings viewport handles its own copy of this key —
                // see `settings_window`, which is where the interesting half is.
                hotkeys::Action::Settings => {
                    self.settings_open = !self.settings_open;
                    self.settings_raise = self.settings_open;
                }
                hotkeys::Action::HotkeyHud => self.hotkey_hud = true,
                // Delivered by `raw_input_hook`, which had to take the key before
                // egui spent it on focus. Nothing reaches the table on this one.
                hotkeys::Action::TogglePanels => {}
                hotkeys::Action::PreviewOriginal => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.preview_original = !t.preview_original;
                    }
                }
                hotkeys::Action::OverexposedOverlay => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.overlays.overexposed = !t.overlays.overexposed;
                    }
                }
                hotkeys::Action::UnderexposedOverlay => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.overlays.underexposed = !t.overlays.underexposed;
                    }
                }
                hotkeys::Action::SensorClipping => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.overlays.sensor = !t.overlays.sensor;
                    }
                }
                hotkeys::Action::CyclePreviewSource => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.preview = t.preview.next();
                        // The decoded reference belongs to the view that asked for
                        // it. Dropping it here also frees the egui registration on
                        // the next frame, in `settle_preview`.
                        t.status = match t.preview {
                            tabs::PreviewSource::Mono => {
                                t.update_status();
                                std::mem::take(&mut t.status)
                            }
                            other => format!("reference view — {}", other.label()),
                        };
                    }
                }
                hotkeys::Action::Surround => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.surround = !t.surround;
                    }
                }
                hotkeys::Action::FalseColour => {
                    if let Some(t) = self.tabs.active_mut() {
                        t.overlays.false_colour = !t.overlays.false_colour;
                    }
                }
                // `c` toggles: press it again and you are back to panning. The
                // other way out is Esc, dispatched below — one key for every mode,
                // which is the affordance that makes a mode survivable.
                hotkeys::Action::Crop => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        t.mode = if t.mode.is_crop() {
                            tabs::Mode::View
                        } else {
                            tabs::Mode::crop(t.params.composition)
                        };
                    }
                }
                // `v` toggles the loupe, the same shape `c` has: press again to leave,
                // or `Esc`. **The mode and the flag move together and always through
                // `Tab::show_loupe` / the close arm here** — `loupe.open` alone would
                // put a window up that `print_loupe` closes again on the next frame,
                // because the mode is the source of truth and not the flag.
                hotkeys::Action::Loupe => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        if t.mode.is_loupe() {
                            t.mode = tabs::Mode::View;
                            t.loupe.open = false;
                            t.loupe.forget();
                        } else {
                            t.show_loupe();
                        }
                    }
                }
                hotkeys::Action::LoupeBeforeAfter => {
                    if let Some(t) = self
                        .tabs
                        .active_mut()
                        .filter(|t| t.loupe.open && t.mode.is_loupe())
                    {
                        t.loupe.before = !t.loupe.before;
                    }
                }
                // `i` toggles pin mode, the way `c` toggles crop and `k` toggles
                // compare: the key that got you in gets you out. Leaving does not
                // clear the pins — they are what the mode was for, and a mode that
                // discarded its own work on exit would make `Esc` unusable.
                hotkeys::Action::ValuePinMode => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        t.mode = if matches!(t.mode, tabs::Mode::Pin) {
                            tabs::Mode::View
                        } else {
                            // Placing a pin you cannot see would be a control that
                            // does nothing, so entering unhides.
                            t.pins.hidden = false;
                            tabs::Mode::Pin
                        };
                    }
                }
                // `⇧I` hides and shows, and works whether or not the mode is open —
                // the marks are on the picture either way, and wanting them gone
                // while you judge it is the case this exists for.
                hotkeys::Action::ToggleValuePins => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        t.pins.hidden = !t.pins.hidden;
                    }
                }
                // **`e` brings Develop forward.** the maintainer's, and it is the honest half of
                // a binding that was reserved for something bigger: `l` / `e` are the
                // Lightbox ↔ Develop mode switch, and there is no Lightbox yet — but
                // the *Develop* half already means something, because Develop is
                // tabbed with Dodge / Burn by default and `d` or `x` leaves the brush
                // in front of it. Getting back was a click on a tab and nothing else.
                //
                // `bring_forward` is the same call `d` and `x` make for the brush, so
                // this inherits its two refusals: it will not un-hide panels that
                // `tab` put away, and it will not pull back a panel someone floated
                // onto another screen. Both are already reachable; the key is for the
                // one case that is not.
                // **From Lightbox, `e` is the way back.** `l` and `e` were reserved
                // as a pair for the mode switch; inside Develop `e` brings the panel
                // forward, and from the browser the only thing it can sensibly mean
                // is the other half of the pair.
                hotkeys::Action::Develop if self.lightbox.active => self.set_lightbox(false),
                hotkeys::Action::Develop => self.layout.bring_forward(layout::Pane::Develop),
                // **In Lightbox the rotate pair turns the selection**, for display,
                // rather than the open frame's composition — same gesture, and the
                // thing it acts on is whatever the mode is about.
                hotkeys::Action::RotateLeft | hotkeys::Action::RotateRight
                    if self.lightbox.active =>
                {
                    let cw = *a == hotkeys::Action::RotateRight;
                    if let Some(why) = self.lightbox.rotate(cw) {
                        self.pending_note = Some(why);
                    }
                }
                hotkeys::Action::RotateLeft | hotkeys::Action::RotateRight => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        t.rotate(*a == hotkeys::Action::RotateRight);
                    }
                }
                hotkeys::Action::CaptureSnapshot => {
                    self.capture_requested = self.tabs.active_id();
                }
                // `k` toggles, and `Esc` closes — the pair every mode in this app
                // offers, because a view you cannot get out of with the key that got
                // you in is a trap. Nothing is put back on close: compare changes no
                // pixel, which is what `Mode::Loupe` established.
                // `1` is the single view, so the run 1–4 is one control rather than a
                // close key and three layout keys. `2`–`4` can reopen comparison after
                // `1`; without that, the first key in the run made the remaining keys
                // inert until `k` was pressed again.
                hotkeys::Action::CompareUp(n) => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        let pinned = t.snapshots.pinned_count();
                        if !t.compare.select_layout(*n as usize, pinned) {
                            t.status = "compare needs two pinned snapshots".into();
                        }
                    }
                }
                hotkeys::Action::CompareViewer => {
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        t.compare.open = !t.compare.open;
                        // **Opening starts at fit.** A grid that came back at the zoom
                        // the last comparison ended on would open on a detail of four
                        // pictures you have not chosen yet — and the first thing a
                        // comparison needs is the whole frame.
                        if t.compare.open {
                            t.compare.reset_view();
                            // Opening onto an empty grid is the one way this looks
                            // broken rather than empty, so it says which it is.
                            if t.snapshots.pinned_count() < 2 {
                                t.status = "compare needs two pinned snapshots".into();
                            }
                        }
                    }
                }
                // `d` and `x` open the brush and choose which way it works, the way
                // `c` opens the crop tool — and pressing the one you are already in
                // closes it, so the key that got you here gets you out. Pressing the
                // *other* one switches without leaving, because dodging and burning
                // the same area in turn is the ordinary way to work and going out
                // through View to do it would be friction with no purpose.
                hotkeys::Action::Dodge | hotkeys::Action::Burn => {
                    let sign = if *a == hotkeys::Action::Dodge {
                        raw_core::Sign::Dodge
                    } else {
                        raw_core::Sign::Burn
                    };
                    // **`d` always means dodge**, and never "stop dodging". It used
                    // to toggle, which is right for `c` because crop has one key and
                    // therefore has to — but D&B has two, the common gesture is
                    // flipping between them, and every third press ejecting you is
                    // the odd one out. `↵` keeps and `Esc` discards; both are bound
                    // and both are now in the mode's footer line.
                    let fallback = self.tool;
                    if self.tabs.active().is_some_and(|t| t.has_image()) {
                        self.layout.bring_forward(layout::Pane::DodgeBurn);
                    }
                    if let Some(t) = self.tabs.active_mut().filter(|t| t.has_image()) {
                        // The most recent layer of that sign, by sign alone — the
                        // one you were last working on, whether you drew it with a
                        // brush or dragged it as a gradient. The tool then follows
                        // the layer rather than the other way round.
                        let picked = paint::most_recent(&t.params.dodgeburn, sign);
                        let entered = t.params.dodgeburn.clone();
                        match picked {
                            Some(i) => {
                                let tool = paint::Tool::of(&t.params.dodgeburn.instances[i].shape);
                                t.db_active = Some(i);
                                t.mode = tabs::Mode::paint(sign, tool, entered);
                            }
                            None => {
                                // Nothing of that kind yet, so make one in whatever
                                // shape the ADD bench has up.
                                let db = &mut t.params.dodgeburn;
                                if paint::instance_for(
                                    db,
                                    &mut t.db_active,
                                    sign,
                                    fallback,
                                    self.brush.nib,
                                    true,
                                )
                                .is_some()
                                {
                                    t.mode = tabs::Mode::paint(sign, fallback, entered);
                                } else {
                                    self.pending_note = Some(format!(
                                        "{} layers is the limit",
                                        raw_core::DodgeBurnParams::MAX_INSTANCES
                                    ));
                                }
                            }
                        }
                    }
                }
                // Modal: only reachable while the brush is open, which is why `⌘D`
                // can be this and Duplicate Tab at once. See `hotkeys::Binding::modal`.
                hotkeys::Action::NewDodge | hotkeys::Action::NewBurn => {
                    let sign = if *a == hotkeys::Action::NewDodge {
                        raw_core::Sign::Dodge
                    } else {
                        raw_core::Sign::Burn
                    };
                    let tool = self.tool;
                    let nib = self.brush.nib;
                    if let Some(t) = self.tabs.active_mut() {
                        if let tabs::Mode::Paint { sign: s, grab, .. } = &mut t.mode {
                            *s = sign;
                            *grab = None;
                        }
                        let db = &mut t.params.dodgeburn;
                        if paint::instance_for(db, &mut t.db_active, sign, tool, nib, true)
                            .is_none()
                        {
                            self.pending_note = Some(format!(
                                "{} instances is the limit",
                                raw_core::DodgeBurnParams::MAX_INSTANCES
                            ));
                        }
                    }
                }
                // Held rather than pressed; see `hotkeys::held`. The press is
                // reported too and is deliberately ignored here.
                hotkeys::Action::ShowDodgeMap | hotkeys::Action::ShowBurnMap => {}
                hotkeys::Action::BrushRadius(_)
                | hotkeys::Action::BrushFeather(_)
                | hotkeys::Action::BrushIntensity(_)
                | hotkeys::Action::BrushOpacity(_) => self.brush.step(*a),
                // **Escape cancels; it does not commit.** One key out of every
                // mode, and it puts back whatever that mode was opened over — which
                // for crop is the composition as it stood. A no-op when no mode is
                // open, which is what leaves Escape to egui for closing the Settings
                // window.
                // **`Esc` closes the preview first.** One key out of every mode, and
                // in Lightbox the preview is the mode you are in — falling through to
                // Develop's tab state would leave a full-frame view up while
                // cancelling a crop you cannot see.
                hotkeys::Action::ExitMode if self.lightbox.active && self.lightbox.previewing() => {
                    self.lightbox.close_preview();
                }
                hotkeys::Action::ExitMode => {
                    if let Some(t) = self.tabs.active_mut() {
                        if let Some(before) = t.mode.cancelled() {
                            t.params.composition = before;
                        }
                        // The brush's equivalent: every pass painted since the tool
                        // opened goes back. Not the last pass — that is `⌘Z` — but
                        // the whole session, which is what "I did not want that"
                        // means when the thing you did not want is a tool you
                        // opened by accident.
                        if let Some(before) = t.mode.cancelled_strokes() {
                            t.params.dodgeburn = before.clone();
                        }
                        // The loupe has nothing to put back — it edits no pixel —
                        // so cancelling it is closing it. Doing that here rather
                        // than leaving the window up over a mode that has ended
                        // keeps "one key out of every mode" literally true.
                        if t.mode.is_loupe() {
                            t.loupe.open = false;
                            t.loupe.forget();
                        }
                        // And compare, for the same reason and with the same
                        // nothing to put back: it replaced the picture, it changed
                        // no pixel of it, so leaving is all closing means.
                        t.compare.open = false;
                        t.straighten_armed = false;
                        // Pins have nothing to put back either — they are view state
                        // and leaving keeps them, which is the point of having placed
                        // them. Only the grab in flight is dropped, so a drag
                        // interrupted by `Esc` does not resume on the next press.
                        t.pins.dragging = None;
                        t.mode = tabs::Mode::View;
                    }
                }
                // ...and `↵` applies, which is simply leaving the mode: the crop is
                // already on `params` and has been since the drag that made it.
                hotkeys::Action::CommitMode => {
                    if let Some(t) = self.tabs.active_mut() {
                        // Same for the loupe on this key, and for the same reason
                        // `Esc` closes it: there is no "apply" to distinguish from a
                        // cancel, so both ways out of a mode do the one thing there
                        // is to do. Leaving it open would strand the checkbox showing
                        // a loupe whose drag no longer works.
                        if t.mode.is_loupe() {
                            t.loupe.open = false;
                            t.loupe.forget();
                        }
                        t.straighten_armed = false;
                        t.mode = tabs::Mode::View;
                    }
                }
                // **`l` switches modes**, the other half of the pair `e` has held
                // since the Develop panel got a key of its own.
                //
                // Nothing is dismantled and nothing is put back: Lightbox draws its
                // own shell, so Develop's panels are simply not drawn while it is up
                // and are exactly where they were when it comes down. See `lightbox`.
                hotkeys::Action::Lightbox => {
                    if !self.lightbox.active {
                        if let Some(id) = self.tabs.active_id() {
                            self.save_sidecar(id);
                        }
                        self.store_edited_tile(&rs);
                    }
                    self.set_lightbox(!self.lightbox.active);
                }
                // **Ratings and labels are the grid's, and only the grid's.** They
                // act on the selected tile, so outside Lightbox there is nothing for
                // them to act on — `⌘1`-`⌘5` were kept off the tab strip for exactly
                // this, back when there was no Lightbox to give them to.
                hotkeys::Action::Rating(n) if self.lightbox.active => {
                    if let Some(why) = self.lightbox.set_rating(*n as i32) {
                        self.pending_note = Some(why);
                    }
                }
                hotkeys::Action::ColourLabel(n) if self.lightbox.active => {
                    // `⇧1`-`⇧5` are the five in `theme::LABELS` order, one-based.
                    let name = theme::LABELS
                        .get((*n as usize).saturating_sub(1))
                        .map(|(s, _)| *s);
                    if let Some(why) = self.lightbox.set_label(name) {
                        self.pending_note = Some(why);
                    }
                }
                hotkeys::Action::CopySettings if self.lightbox.active => {
                    self.pending_note = Some(match self.lightbox.copy_settings() {
                        Ok(note) | Err(note) => note,
                    });
                }
                hotkeys::Action::PasteSettings if self.lightbox.active => {
                    self.pending_note = Some(match self.lightbox.paste_settings() {
                        Ok(note) | Err(note) => note,
                    });
                }
                hotkeys::Action::RenameFiles if self.lightbox.active => {
                    self.lightbox.begin_rename();
                }
                hotkeys::Action::ContactSheet if self.lightbox.active => {
                    self.lightbox.begin_contact_sheet();
                }
                // Zoom needs the viewport's anchor, so it is handled where that
                // exists; see `viewport_panel` — except in Lightbox, where there is
                // no viewport and the same pair sizes the tiles instead. One
                // gesture, "scale what you are looking at", in both modes.
                hotkeys::Action::ZoomIn if self.lightbox.active => self.lightbox.resize(true),
                hotkeys::Action::ZoomOut if self.lightbox.active => self.lightbox.resize(false),
                hotkeys::Action::ZoomToggle
                | hotkeys::Action::ZoomFit
                | hotkeys::Action::ZoomIn
                | hotkeys::Action::ZoomOut => {}
                // Bound, and nothing implements them yet. Say so rather than doing
                // nothing, which is indistinguishable from being broken.
                other => {
                    if let Some(b) = hotkeys::TABLE.iter().find(|b| b.action == *other) {
                        self.pending_note =
                            Some(format!("{} — {} is not built yet", b.chord(), b.what));
                    }
                }
            }
        }

        egui::Panel::top("title")
            .frame(egui::Frame::NONE.fill(theme::CHROME_DEEP))
            .show(ui, |ui| {
                // On macOS this is the app chrome beneath the traffic lights. Under a
                // native Windows/Linux titlebar it becomes a mode/status strip, so it
                // names LIGHTBOX or DEVELOP rather than repeating the window title.
                // The filename lives in the footer; repeating it here just made the
                // strip say the same thing twice.
                // **Nothing about the open frame while you are browsing.** The strip
                // reports the picture Develop is holding, and in Lightbox that picture is
                // not on screen — a ruby exposure line for a frame you cannot see reads as
                // a statement about the one you are looking at.
                let exposure = if self.lightbox.active {
                    String::new()
                } else {
                    self.tabs
                        .active()
                        .and_then(|t| t.image.as_ref())
                        .map(|img| exposure_line(&img.sensor.meta))
                        .unwrap_or_default()
                };
                // **The right end of the strip is the update badge's slot.** It
                // exists only while the updater has something to say, and the
                // centre readout stays centred on the window whether or not it
                // is up. A click opens the update sheet.
                let badge = self
                    .updates
                    .as_ref()
                    .and_then(|updates| updates.badge())
                    .map(|b| widgets::UpdateBadge {
                        text: b.text,
                        detail: b.detail,
                        failed: b.failed,
                    });
                if widgets::title_strip(
                    ui,
                    platform::strip_title(self.lightbox.active),
                    &exposure,
                    badge.as_ref(),
                ) {
                    self.update_sheet_open = true;
                }
            });

        // **The tab strip is Develop's**, so it goes with the rest of it. A strip of
        // open files above a folder browser would be offering the thing you left the
        // mode to stop looking at.
        if !self.lightbox.active {
            egui::Panel::top("tabs").show(ui, |ui| {
                let a = self.tab_strip(ui);
                if a != StripAction::None {
                    action = a;
                }
            });
        }

        // Acted on here, between the strip and the panels below it. Not inside the
        // strip, because that closure is iterating the list these mutate; not after
        // the panels, because a newly focused tab would then draw one frame late —
        // and a one-frame blank is exactly what flicker comparison must not have.
        match action {
            StripAction::None => {}
            StripAction::Focus(i) => {
                self.tabs.focus(i);
                if let Some(id) = self.tabs.active_id() {
                    self.queue.promote(id);
                    self.luma_queue.promote(id);
                }
            }
            StripAction::Close(i) => self.close(i, &rs),
            StripAction::Open => self.pick_and_open(&ctx),
            StripAction::Duplicate => self.duplicate(&rs),
            StripAction::SaveDuplicate => {
                if let Some(id) = self.tabs.active_id() {
                    if self.tabs.by_id_mut(id).is_some_and(|t| t.scratch) {
                        self.save_duplicate(id);
                    } else {
                        self.save_sidecar(id);
                    }
                }
            }
        }
        // Before the viewport draws, so the tab now in front has its GPU state and
        // the tab that just left gives its own back.
        self.settle_warmth(&rs, &ctx);
        self.settle_preview();

        // Thumbnails that finished since the last frame become textures here —
        // before anything draws, so a tile that arrived is drawn on the frame it
        // arrived rather than the one after. Cheap and a no-op when the queue is
        // idle, so it is not worth guarding on the mode being up: a folder opened
        // and then left still has jobs to collect.
        self.lightbox.collect(&ctx);

        // **Lightbox's own interaction keys**, deliberately not dispatchable rows in
        // `TABLE`. Space is listed in `REFERENCE_GESTURES` because Quick Look is worth
        // discovering in the HUD; the arrows are simply what a grid is, like `Esc`
        // is a property of being in a mode rather than a configurable binding.
        //
        // Guarded on a focused text field for the same reason every bare key is:
        // typing a space into the Settings search must not open a picture.
        // **`space` is taken unconditionally, and acted on outside that guard.**
        //
        // Taken unconditionally because a flag that is set in the hook and cleared only
        // inside a guard is a latch, not a guard — the same trap `raw_input_hook`'s own
        // note describes for `text_edit_focused`. A press that arrived while a button
        // held focus would sit in the flag and fire on some unrelated later frame.
        //
        // Acted on outside it because `egui_wants_keyboard_input` is literally
        // "something has focus", and the reason Space needed a focus guard at all was
        // that egui would otherwise press the focused widget with it. The hook now takes
        // the event before egui can, so the only guard still wanted is "is the user
        // typing", which the hook applies. Clicking a footer button and then pressing
        // Space should preview the picture; it used to do nothing.
        let space = std::mem::take(&mut self.lightbox_space);
        if space && self.lightbox.active && !self.hotkey_hud {
            self.lightbox.toggle_preview();
        }
        if self.lightbox.active && !self.hotkey_hud && !ctx.egui_wants_keyboard_input() {
            let (left, right, up, down, shift) = ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::ArrowRight),
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                    i.modifiers.shift,
                )
            });
            // **`⇧` extends rather than moves**, which is the keyboard half of the
            // gesture `⇧`+click already performs. Read as a modifier on the arrow rather
            // than as four more keys, because it is one rule over all four directions.
            if left {
                if shift {
                    self.lightbox.step_selecting(false)
                } else {
                    self.lightbox.step(false)
                }
            }
            if right {
                if shift {
                    self.lightbox.step_selecting(true)
                } else {
                    self.lightbox.step(true)
                }
            }
            if up {
                if shift {
                    self.lightbox.step_row_selecting(false)
                } else {
                    self.lightbox.step_row(false)
                }
            }
            if down {
                if shift {
                    self.lightbox.step_row_selecting(true)
                } else {
                    self.lightbox.step_row(true)
                }
            }
        }

        // Panel order matters: first added is outermost. The footer spans full width,
        // so it precedes the left panel. CentralPanel always last.
        //
        // Read after the closure, not inside it: the footer holds an immutable borrow
        // of the active tab for its whole body.
        let mut restore_panels = false;
        let mut enter_lightbox = self.lightbox.active;
        let mut settings_changed = false;
        // Read before the footer body, which takes a borrow of the active tab and
        // cannot then reach `self.settings`. Same reason `settings_ppi` is bound early
        // in the develop panel.
        let print_unit = self.settings.print_unit();
        self.lightbox.set_contact_sheet_unit(print_unit);
        let mut footer_note: Option<String> = None;
        egui::Panel::bottom("footer").show(ui, |ui| {
            // Lightbox's footer is its own line, not this one with pieces removed:
            // almost nothing on the Develop footer — exposure, clipping, the pixel
            // under the cursor — has a meaning when what you are looking at is a
            // folder.
            if self.lightbox.active {
                let act = self.lightbox.footer_ui(ui, &self.icons);
                if let Some(want) = act.mode {
                    enter_lightbox = want;
                }
                // The footer's toggles and the Settings checkboxes are the same two
                // preferences, so the footer writes the setting rather than a copy of
                // it — otherwise the two controls disagree the moment either is used.
                // Same sync the prototype does by hand at `monopro.py:32115`.
                if let Some(v) = act.filenames {
                    self.settings.lightbox_filenames = v;
                    settings_changed = true;
                }
                if let Some(v) = act.frameless {
                    self.settings.frameless_tiles = v;
                    settings_changed = true;
                }
                if let Some(v) = act.grey {
                    self.settings.lightbox_gray = v;
                    settings_changed = true;
                }
                if act.note.is_some() {
                    footer_note = act.note;
                }
                return;
            }
            ui.horizontal(|ui| {
                // LEFT — what this file *is*. Identity, then the sizes that decide
                // whether it can be printed at the size you want, then how much of
                // it you are currently looking at.
                //
                // The dimensions are the **output** ones, which on SuperPixel is
                // half the sensor's. That is the honest number and it is meant to be
                // uncomfortable: a 41 MP M10-R frame prints at 13.1 x 8.7 in, not
                // 26.2 x 17.4, and the panel that hides that is the panel that lets
                // you find out at the lab. See `docs/ux-inventory.md`.
                let active = self.tabs.active();
                // **A mode says what it is and how to leave.**
                //
                // The honest cost of a mode is that it is invisible until it
                // surprises you, and this app already has the affordance that fixes
                // that: the footer reports state in ruby, and the prototype's
                // `PIN MODE · click to place · … · I to exit` is the established
                // shape. It takes the whole line and outranks the file facts,
                // exactly as `pending_note` does, because for the seconds it is up
                // it is the more useful sentence.
                let mode_hint = active.and_then(|t| t.mode.hint());
                match (&self.pending_note, mode_hint) {
                    // A bound key whose feature does not exist says so, here, and it
                    // takes the whole line — a note about what you just pressed is
                    // more use for the second it lasts than the file facts are.
                    (Some(note), _) => {
                        ui.label(theme::footer_caption(note).color(theme::RUBY));
                    }
                    (None, Some(hint)) => {
                        ui.label(theme::footer_caption(hint).color(theme::RUBY));
                    }
                    (None, None) => match active {
                        Some(t) => {
                            let mut parts = vec![t.display_name()];
                            // The CROP's dims, not the working image's. This line is
                            // what a print is ordered from, and after a crop the
                            // uncropped size is a number for a negative nobody is
                            // going to print.
                            if let Some(d) = t.output_dims() {
                                let o = &t.params.output;
                                // **The picture's pixels, not the file's.** Output's
                                // size must not leak back into the view — this line
                                // is what the histogram and the zoom are measured
                                // against, and a number that moved with the last
                                // export setting would be unreadable. The print size
                                // beside it *is* Output's, because that is the number
                                // a print is ordered from.
                                let (w, h) = (d.w, d.h);
                                let (iw, ih) = o.print_inches(d);
                                parts.push(format!("{w} x {h} px"));
                                // **One unit, the one you chose.** Both were shown, and
                                // that is right in EXIF — a reference table wants either
                                // to be findable — and wrong in a footer, which is a
                                // running line you read at a glance. The number you do
                                // not use is in the way of the one you do. the maintainer's call,
                                // and it is the same `print_unit` PIPELINE follows.
                                let (pw, ph) =
                                    (print_unit.from_inches(iw), print_unit.from_inches(ih));
                                parts.push(match print_unit {
                                    Unit::Inches => format!("{pw:.1}\" x {ph:.1}\""),
                                    Unit::Centimetres => format!("{pw:.1} x {ph:.1} cm"),
                                });
                                parts.push(format!("{:.0}ppi", o.ppi));
                                // Only when there is one, so the ordinary line does
                                // not carry a word about a feature nobody is using.
                                if o.resamples(d) {
                                    parts.push(o.scale_note(d));
                                }
                            }
                            parts.push(if t.view.fit {
                                "fit".to_owned()
                            } else {
                                tabs::zoom_label(t.view.scale)
                            });
                            ui.label(theme::footer_readout(parts.join("  ·  ")));
                        }
                        None => {
                            ui.label(theme::footer_readout(self.status.clone()));
                        }
                    },
                }

                // An overlay changes what you are looking at, and until now nothing
                // said so — you could leave false colour on, walk away, and come
                // back to a picture you would not otherwise recognise. Ruby, because
                // it is a mode you are in rather than a fact about the file.
                if let Some(t) = active {
                    let on = t.overlays;
                    let names: Vec<&str> = [
                        on.overexposed.then_some("OVEREXPOSED"),
                        on.underexposed.then_some("UNDEREXPOSED"),
                        on.false_colour.then_some("FALSE COLOR"),
                        on.sensor.then_some("SENSOR CLIPPING"),
                        // **`p` belongs in this list and was missing from it.** It is
                        // the most disguisable of the lot: an overlay looks like an
                        // overlay, but preview-original looks like your picture — just
                        // the wrong one — so leaving it on and walking away is how you
                        // come back and start re-making edits you have already made.
                        t.preview_original.then_some("PREVIEW ORIGINAL"),
                        // The two reference views, for the reason `PreviewSource::badge`
                        // gives: they are the only states in the app that put a
                        // *different picture* on the canvas, so of everything in this
                        // list they are the ones that most need to say so.
                        t.preview.badge(),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    if !names.is_empty() {
                        ui.label(
                            egui::RichText::new(format!("  ·  {}", names.join(" · ")))
                                .size(theme::size::FOOTER_CAPTION)
                                .color(theme::RUBY),
                        );
                    }
                }

                // **A second way back, because a keystroke is not enough of one.**
                // `tab` was the only route to a hidden panel, and when `tab` broke —
                // it latched on egui's focus, see `raw_input_hook` — the panels were
                // simply gone, restart after restart, because the state is persisted.
                // A key that can stop working must not be the sole inverse of a
                // gesture. Reported in the same ruby as the overlays, for the same
                // reason: it is a state you are in, not a fact about the file.
                if self.layout.panels_hidden() {
                    restore_panels = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new("  ·  PANELS HIDDEN")
                                    .size(theme::size::FOOTER_CAPTION)
                                    .color(theme::RUBY),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(theme::tip("click, or press tab, to bring them back"))
                        .clicked();
                }

                // RIGHT — what the pixel under the cursor *measures*.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // **Which mode you are in, at the outer edge, and clickable.**
                    // First in a right-to-left layout is furthest right, which is
                    // where it belongs: it is the most permanent thing on the line
                    // and the one piece of state that survives changing tabs.
                    //
                    // Read after the closure for the same reason `restore_panels` is
                    // — switching modes needs `&mut self` and this closure is
                    // holding a borrow of the active tab.
                    if let Some(want) = lightbox::mode_tabs(ui, false) {
                        enter_lightbox = want;
                    }
                    ui.separator();
                    // Say when work is outstanding somewhere other than the tab in
                    // front, so a background decode is visible rather than a
                    // mysteriously stale tab.
                    let queued = self.queue.in_flight();
                    if queued > 0 {
                        ui.label(theme::footer_caption(format!("{queued} decoding")));
                        ui.separator();
                    }
                    if let Some(t) = active {
                        // Global, and not a sample: the fraction of the whole frame
                        // at each end. It answers "am I losing anything", which is a
                        // question about the picture rather than about one pixel,
                        // and it comes free out of the bins the histogram already
                        // built.
                        //
                        // **Shown at zero too.** It used to be hidden when nothing
                        // was clipped, which meant "this frame loses nothing" and
                        // "the readout is not there" looked identical — and since
                        // clipping comes and goes as you drag exposure, the number
                        // appeared to flicker in and out at random. the maintainer reported it
                        // as a disappearance. A meter reads zero; it does not leave.
                        let (lo, hi) = t.histogram.clipped();
                        ui.label(theme::footer_readout(format!(
                            "▼{:.1}%  ▲{:.1}%",
                            lo * 100.0,
                            hi * 100.0
                        )));
                        ui.separator();
                    }
                    match self.readout {
                        // A colour reference view reports **the colour it is
                        // showing** — see `Readout::Reference`. Read right-to-left
                        // like the pair below, so they land in reading order.
                        //
                        // **The same shape as the print's readout**, on purpose:
                        // `L*` last-and-leftmost with the two chroma axes after it,
                        // in the same ink, at the same widths. That is what makes the
                        // JPEG's `L* 64` and the print's `L* 58` read as one
                        // comparison across a keypress instead of as two unrelated
                        // footers — which is the whole reason the readout is Lab.
                        //
                        // RGB is the other half of the setting, and gets no coloured
                        // ink: `a*` and `b*` earn theirs by being meaningless without
                        // a direction, and an `R` already labelled R does not.
                        Some(Readout::Reference { rgb }) => {
                            let cell = |ui: &mut egui::Ui, name: &str, text: String, ink| {
                                ui.spacing_mut().item_spacing.x = 3.0;
                                ui.label(theme::footer_readout(text).color(ink));
                                ui.label(theme::footer_readout(name).color(theme::DIM));
                            };
                            match self.settings.reference_values() {
                                settings::ReferenceValues::Lab => {
                                    // Bare `a` and `b`, `L*` keeps its star — the same
                                    // rule the print readout follows. **Both branches, or
                                    // the footer changes vocabulary when you press `j`**,
                                    // which is exactly the drift this readout exists to
                                    // avoid: its whole job is that its `L*` and the
                                    // print's are the same axis and subtract.
                                    let lab = raw_core::colour::lab_of_srgb(rgb);
                                    cell(ui, "b", format!("{:>6.1}", lab[2]), theme::LAB_B);
                                    cell(ui, "a", format!("{:>6.1}", lab[1]), theme::LAB_A);
                                    cell(
                                        ui,
                                        "L*",
                                        format!("{:>6.1}", lab[0]),
                                        ui.visuals().text_color(),
                                    );
                                }
                                settings::ReferenceValues::Rgb => {
                                    let ink = ui.visuals().text_color();
                                    cell(ui, "B", format!("{:>4}", rgb[2]), ink);
                                    cell(ui, "G", format!("{:>4}", rgb[1]), ink);
                                    cell(ui, "R", format!("{:>4}", rgb[0]), ink);
                                }
                            }
                        }
                        Some(Readout::Print { lstar, lab, .. }) => {
                            // **No EV here.** It used to lead this readout as the scene
                            // value entering the tone map, and the maintainer cut it: the footer
                            // answers *what is this pixel on the print*, and `L*` is that
                            // answer. A second number in a different space made the pair
                            // read as one measurement in two units, which is what the
                            // Inspector's `EDITED` / `RAW` toggle exists to keep apart —
                            // and that toggle is where the scene value still lives, on a
                            // pin you placed on purpose.
                            // **`a*` and `b*` only when there is colour to report**,
                            // and coloured as Lab's own axes are: `+a` runs to magenta
                            // and `+b` to yellow, so the ink says which way the number
                            // points before it is read. An untoned print has no colour,
                            // and a live `0.0` beside a live `L*` would be claiming a
                            // measurement rather than reporting an absence.
                            //
                            // Right-to-left, so these are pushed before `L*` and land
                            // after it on screen.
                            //
                            // **The name is grey and only the number carries the ink**,
                            // which is the maintainer's call and the right one: `a*` is a label
                            // like every other label in the app, and colouring it made
                            // the pair read as a coloured phrase rather than as a
                            // measurement with a coloured value.
                            //
                            // Widths are fixed so a number changing does not shove its
                            // neighbours sideways. A footer that reflows as the cursor
                            // moves is a footer nobody can read a value off.
                            let axis = |ui: &mut egui::Ui, name: &str, v: f32, ink| {
                                ui.spacing_mut().item_spacing.x = 3.0;
                                ui.label(theme::footer_readout(format!("{v:>6.1}")).color(ink));
                                ui.label(theme::footer_readout(name).color(theme::DIM));
                            };
                            // **Bare `a` and `b`, no star.** the maintainer's call for the footer
                            // specifically: `L*` earns its star because it is the axis the
                            // whole app is calibrated in and the one an `L*` elsewhere has
                            // to match, while `a*b*` here are a chroma readout on a print
                            // that is usually untoned. Three starred labels in a row read
                            // as noise. The Inspector's pin rows keep `a*` and `b*` in
                            // full, where there is room and the reader is measuring.
                            if let Some((a, b)) = lab {
                                axis(ui, "b", b, theme::LAB_B);
                                axis(ui, "a", a, theme::LAB_A);
                            }
                            ui.spacing_mut().item_spacing.x = 3.0;
                            ui.label(theme::footer_readout(format!("{lstar:>6.1}")));
                            ui.label(theme::footer_readout("L*").color(theme::DIM));
                        }
                        None => {
                            ui.label(theme::footer_caption("—"));
                        }
                    }
                });
            });
        });

        if restore_panels {
            self.layout.toggle_panels();
        }
        // The tab strip and `l` are the same act, so they go through the same call.
        if enter_lightbox != self.lightbox.active {
            if enter_lightbox {
                // The edited tile is keyed on the sidecar. Write that sidecar first,
                // otherwise a final gesture can move its timestamp immediately after
                // the preview was stored and make the new tile unreachable.
                if let Some(id) = self.tabs.active_id() {
                    self.save_sidecar(id);
                }
                self.store_edited_tile(&rs);
            }
            self.set_lightbox(enter_lightbox);
        }
        if let Some(note) = footer_note {
            self.pending_note = Some(note);
        }
        // A footer toggle is a preference change and has to survive the session, the
        // same as one made in the Settings sheet.
        if settings_changed && let Err(e) = self.settings.save() {
            self.pending_note = Some(format!("could not save settings: {e}"));
        }

        // **Panels that have been popped out.** Drawn before the tree, so their
        // windows exist for this frame; in the tree they are invisible tiles that
        // keep their place, so docking one puts it back where it left from. This is
        // the half `egui_tiles` cannot do — it has no viewport container — and it is
        // one-way by decision: see `layout`.
        //
        // `tab` takes these away too. A floating Develop still covering half the
        // screen is not "out of the way", which is the only thing that key means.
        // **Lightbox takes them too**, and for the reason `tab` does: a floating
        // Develop panel over a folder browser is the half of Develop that did not
        // get the message. The prototype preferred to leave torn-off panels up
        // (`monopro.py:35780-35800`); this app already decided what "the panels go
        // away" means and one gesture should not mean two things.
        //
        // Nothing is recorded to undo here. These windows are drawn *because* `out`
        // says so, so not drawing them is the whole of hiding them, and returning to
        // Develop puts them back untouched.
        if !self.layout.panels_hidden() && !self.lightbox.active {
            if self.layout.is_out(Pane::Info) {
                self.float_info(&ctx);
            }
            if self.layout.is_out(Pane::Snapshots) {
                self.float_snapshots(&ctx);
            }
            if self.layout.is_out(Pane::History) {
                self.float_history(&ctx);
            }
            if self.layout.is_out(Pane::Develop) {
                self.float_develop(&ctx);
            }
            if self.layout.is_out(Pane::DodgeBurn) {
                self.float_dodgeburn(&ctx);
            }
        }

        self.quit_sheet(&ctx);
        self.update_sheet(&ctx);
        let developed_previews_before = self.settings.lightbox_xmp_thumbnails;
        if self.settings_open {
            self.settings_window(&ctx);
        }
        // FOLDERS has one configurable local root; mounted volumes are supplied by
        // Lightbox independently. Push this even while Develop is showing so the
        // tree is already correct on the frame Lightbox comes forward.
        self.lightbox
            .set_folder_root(self.settings.lightbox_folder_root.as_deref());
        // Enabling the preference from Lightbox used to appear to do nothing: the
        // grid started preferring an edited cache, but no cache was made until a later
        // trip through Develop. The active tab's completed render is already resident,
        // so capture it immediately before the grid asks for its first replacement.
        if !developed_previews_before && self.settings.lightbox_xmp_thumbnails {
            self.store_edited_tile(&rs);
        }

        // The tile tree fills what the chrome left. `CentralPanel` last, as always;
        // its fill is only ever seen in the one-point seams between tiles, because
        // every pane paints its own background — a tile is handed a bare `Ui`, not a
        // `Frame`.
        if self.lightbox.active {
            // Live presentation settings are pushed in rather than read through a
            // borrow of the whole struct, which the pane closures cannot hold while
            // they also mutate the Lightbox.
            self.lightbox.show_filenames = self.settings.lightbox_filenames;
            self.lightbox.edited_mark = self.settings.lightbox_edited_mark;
            self.lightbox.frameless = self.settings.frameless_tiles;
            self.lightbox.set_grey(self.settings.lightbox_gray);
            self.lightbox
                .set_xmp_thumbnails(self.settings.lightbox_xmp_thumbnails);
            // **A setter, unlike the two plain assignments above**, because these two
            // change which entries exist rather than how they are drawn — so a change
            // has to re-read the folder. `set_listing` compares before it acts, which
            // is what keeps this from being a `read_dir` every frame.
            self.lightbox.set_listing(
                self.settings.lightbox_folders,
                self.settings.lightbox_other_files,
            );

            // **Lightbox has its own panel tree**, not Develop's. Folders, favorites
            // and EXIF are panes that can be tabbed together, split, or put on either
            // side, and the grid is what they arrange themselves around — the maintainer's
            // call, on the grounds that the moment EXIF is a panel the folder tree
            // has to be one too. See `lightbox::Pane`.
            let mut open_in_develop = None;
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(theme::CHROME))
                .show(ui, |ui| open_in_develop = self.lightbox.ui(ui, &self.icons));
            let renamed = self.lightbox.take_rename_events();
            for (id, path) in self.tabs.rename_paths(&renamed) {
                self.start_load(id, path, &ctx);
            }
            if let Some(dir) = self.lightbox.folder.clone() {
                self.last_dir = Some(dir);
            }

            // **Double-click crosses into Develop**, which is the whole point of the
            // browser: it hands one file over. Acted on out here because `open`
            // needs `&mut self` and the panel closure is holding it.
            if let Some(path) = open_in_develop
                && self.open(path, &ctx)
            {
                self.set_lightbox(false);
            }
        } else {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(theme::CHROME))
                .show(ui, |ui| self.tile_tree(ui, &ctx, &rs, &actions));
        }

        if self.hotkey_hud {
            hotkey_hud(&ctx);
        }

        // Drawn after the panel so pressing Curve's Save item can open the sheet in
        // the same frame. The stack itself was cloned at the click, so naming it is
        // view work and cannot change which edit the preset captures.
        self.curve_preset_name_dialog(&ctx);

        // Export after the panels, so it is not holding a borrow of `self` taken by
        // the panel closure that asked for it.
        // One at a time: `export_rx` being occupied means a worker is still encoding,
        // and a second dialog over the first would be two exports racing for it.
        if let Some((owner, kind)) = std::mem::take(&mut self.export_requested)
            && self.export_rx.is_none()
            && self.tabs.active_id() == Some(owner)
        {
            self.export(&ctx, &rs, owner, kind);
        }
        // The SNAPSHOT button, for the same reason and by the same route: capturing
        // reads the viewport back and so needs the `RenderState` a panel is never
        // handed. Before the diff below, so a capture and the edit that provoked it
        // land in the same frame.
        if let Some(owner) = std::mem::take(&mut self.capture_requested)
            && self.tabs.active_id() == Some(owner)
        {
            self.capture_snapshot(&ctx, &rs, owner);
        }

        // (3) Diff, (4) apply, (5) record. One place, so no control has to remember
        // what it invalidates or whether it is undoable — and skipped entirely when
        // focus moved, because a tab switch is not an edit.
        if let (Some((id, before)), Some(now)) = (before, self.tabs.active_id())
            && id == now
        {
            let after = self.tabs.active().expect("just checked").params.clone();
            let dirty = before.diff(&after);
            if dirty.any() {
                self.apply(id, dirty, &ctx);
            }
            // **Taken here, not at the top of the frame.** A history row is clicked
            // while the panels draw, which is after the top and before this — so a flag
            // read early is a flag that is always false, and the jump gets recorded as
            // a fresh edit. `History::record` clears the redo branch on any edit, so
            // the symptom was every entry ahead of the clicked row vanishing the moment
            // you travelled to it. Travelling must keep the future; only an *edit*
            // discards it.
            let travelled = time_travelled || std::mem::take(&mut self.travelled_by_click);
            if !travelled {
                // This is what coalesces a drag into one undo entry: the gesture
                // stays open until the button comes up. (`egui_is_using_pointer`
                // is 0.35's name for the old `is_using_pointer`; it is false on
                // mere hover, which is the distinction that matters here.)
                let settled = !ctx.egui_is_using_pointer();
                if let Some(tab) = self.tabs.by_id_mut(id) {
                    tab.history.record(before, &after, settled);
                }
                // The sidecar rides the same signal that commits an undo entry:
                // once per gesture, not once per frame. The debouncing a
                // write-on-edit policy would otherwise need is the coalescing that
                // already exists.
                if settled {
                    self.save_sidecar(id);
                }
            }
        }

        // App memory, written when it changes.
        //
        // **Not left to eframe's autosave**, which runs inside the event loop: this
        // app is deliberately quiet when idle — 0% CPU with an image loaded — so a
        // window that is merely sitting there never ticks the timer, and the state
        // would only reach disk if something else happened to force a frame. These
        // values change rarely, so writing them the moment they move is both cheaper
        // and more reliable than any interval.
        // The layout says so itself rather than being compared: `Behavior::on_edit`
        // fires on a drag, a resize or a tab click, which is every way the tree can
        // move, and it is cheaper and more honest than cloning a tree each frame to
        // diff it.
        if (self.export_target, self.last_dir.clone()) != self.persisted
            || std::mem::take(&mut self.layout.dirty)
        {
            self.persist(frame);
        }
    }
}

impl App {
    fn tab_strip(&mut self, ui: &mut egui::Ui) -> StripAction {
        let mut action = StripAction::None;
        let active = self.tabs.active_index();
        let full = self.tabs.is_full();
        let can_duplicate = !full && self.tabs.active().is_some_and(|t| t.has_image());

        ui.horizontal(|ui| {
            // The strip sets its own rhythm rather than inheriting the panel's, so the
            // spacing below means what it says. Grouping is the whole job here: a tab
            // and its close are one object, the duplicate belongs to the tab in front,
            // and `+` belongs to neither.
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(6.0);

            // **A tab is as wide as its name, up to a cap.** Every tab used to be the
            // same width, so one open file sat in a 260pt tab with its name in the left
            // third. The cap is still the eight-tabs budget, because eight have to fit a
            // 900pt window and a control you cannot reach is worse than a name you
            // cannot read in full — the full name is a hover away.
            let n = self.tabs.len().max(1) as f32;
            let per_tab = TAB_PAD * 2.0 + icons::BOX + CLOSE_GAP + GAP_TAB;
            let fixed = 6.0 + n * per_tab + icons::BOX + GAP_ICONS + icons::BOX;
            let cap = ((ui.available_width() - fixed) / n).clamp(NAME_MIN, NAME_MAX);

            for (i, tab) in self.tabs.iter().enumerate() {
                let busy = self.queue.is_busy(tab.id);
                // The extension is part of a filename. Leaving it off made two tabs on
                // `frame.dng` and `frame.tif` indistinguishable, and it is the extension
                // that says which pipeline a file even entered.
                let label = if busy {
                    format!("{} …", tab.display_name())
                } else {
                    tab.display_name()
                };
                let hint = if tab.scratch {
                    format!("{label}\nduplicate · scratch until saved")
                } else {
                    format!("{label}\nclick to switch · ` flicks back to the previous tab")
                };
                match file_tab(ui, &self.icons, &label, i == active, cap, &hint) {
                    TabClick::Focus => action = StripAction::Focus(i),
                    TabClick::Close => action = StripAction::Close(i),
                    TabClick::None => {}
                }

                // The duplicate sits against the tab it acts on, and only that one.
                // After the whole row it looked like a control over the strip, which is
                // not what it does — it copies the file in front.
                if i == active
                    && icons::button(ui, &self.icons, "duplicate", "▣", can_duplicate)
                        .on_hover_text(theme::tip("duplicate tab — same file, settings copied  ⌘D"))
                        .clicked()
                {
                    action = StripAction::Duplicate;
                }
                ui.add_space(GAP_TAB);
            }

            // `+` opens something that is not there yet, so it is set apart.
            ui.add_space(GAP_ICONS);
            if icons::button(ui, &self.icons, "plus", "+", !full)
                .on_hover_text(theme::tip("open a raw  ⌘O"))
                .on_disabled_hover_text(theme::tip("eight tabs is the cap"))
                .clicked()
            {
                action = StripAction::Open;
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.tabs.len() > 1 {
                    ui.label(egui::RichText::new(format!(
                        "{}/{}",
                        self.tabs.len(),
                        tabs::MAX_TABS
                    )));
                }
            });
        });
        action
    }

    /// The strip at the top of a floating panel: the app's own title bar.
    ///
    /// On macOS the OS decorations are off: a document-style titlebar eats a chunk of
    /// a tool palette that is mostly controls, so the strip supplies drag and dock.
    /// Windows and Linux retain their native titlebar and controls; this remains the
    /// pane header and way to dock it, but the OS owns window movement.
    ///
    /// **The drag region stops short of the button, and that is not tidiness.** It
    /// covered the whole strip, registered *after* the button, and a later widget at
    /// the same position wins egui's hit-test — so the dock button never saw a click
    /// and pressing it dragged the window instead. With the decorations off there is
    /// no OS close button either, so that one control was the only route back into the
    /// frame and the pop-out was effectively one-way. The comment that used to sit
    /// here claimed the buttons kept their clicks; they did not.
    fn floating_bar(icons: &icons::Icons, ui: &mut egui::Ui, name: &str, docked: &mut bool) {
        let mut button = egui::Rect::NOTHING;
        let bar = egui::Frame::new()
            .fill(theme::CHROME_DEEP)
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::header_label(ui, name);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let dock = icons::button(ui, icons, "dock", "↙", true)
                            .on_hover_text(theme::tip("put it back in the frame"));
                        button = dock.rect;
                        if dock.clicked() {
                            *docked = true;
                        }
                    });
                });
            });

        // Everything left of the button. Drag it to move the window, the way the title
        // bar we removed would have.
        let mut drag_rect = bar.response.rect;
        drag_rect.max.x = drag_rect.max.x.min(button.left() - 4.0);
        if platform::draws_custom_window_chrome()
            && drag_rect.width() > 0.0
            && ui
                .interact(
                    drag_rect,
                    ui.id().with(("drag", name)),
                    egui::Sense::click_and_drag(),
                )
                .is_pointer_button_down_on()
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    /// The whole develop column: the pinned head, then the scrolling modules.
    ///
    /// One function so the docked pane and the floating window are the same UI rather
    /// than two that drift; `head` is the only thing that differs between them. The
    /// scrollbar stays floating and hover-hidden — making it allocate its own width
    /// was tried and paid a permanent gutter for a bar you look at seconds a day — so
    /// the content keeps a right margin wide enough for it to float over nothing.
    ///
    /// # Scroll belongs to the image tab
    ///
    /// A new tab has no scroll state and therefore begins at DECODE. Returning to an
    /// already-open tab restores that tab's position instead of inheriting whichever
    /// image happened to use this panel last. Module folds remain session view state;
    /// they are a working arrangement shared by the Develop panel, not image edits.
    fn develop_column(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        let mut clicks = HeadClicks::default();
        let scroll_id = ("develop-scroll", self.scroll_session, self.tabs.active_id());
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = self.develop_head(ui, head);
                egui::ScrollArea::vertical()
                    .id_salt(scroll_id)
                    .show(ui, |ui| self.develop_panel(ui));
            });
        clicks
    }

    /// A gradient's geometry, drawn over the picture while its tool is open.
    ///
    /// **Chrome, not pixels** — drawn after the image and outside the GPU path
    /// entirely, the same way the crop handles are, which is also what keeps it
    /// crisp: it is painted in points at the window's own density rather than
    /// resampled with the picture.
    ///
    /// The filled handle is full strength (a linear's start, a radial's centre) and
    /// the hollow one is zero (the far end, the outer radius) — the same convention
    /// as the drag that placed it, which is what makes the two ends legible without
    /// a legend. Every line is drawn twice, dark under light, for the reason the
    /// brush ring is: one stroke disappears into half the pictures it is used on.
    fn gradient_overlay(
        ui: &mut egui::Ui,
        to_screen: &impl Fn((f32, f32)) -> egui::Pos2,
        from: (f32, f32),
        to: (f32, f32),
        radial: Option<raw_core::dodgeburn::Radial>,
        aspect: f32,
    ) {
        let painter = ui.painter();
        let twice = |pts: &[egui::Pos2], closed: bool| {
            for (w, c) in [(2.0, theme::INK_DARK), (1.0, theme::INK_LIGHT)] {
                let stroke = egui::Stroke::new(w, c);
                for pair in pts.windows(2) {
                    painter.line_segment([pair[0], pair[1]], stroke);
                }
                if closed && pts.len() > 2 {
                    painter.line_segment([pts[pts.len() - 1], pts[0]], stroke);
                }
            }
        };

        let (a, b) = (to_screen(from), to_screen(to));
        match radial {
            None => {
                twice(&[a, b], false);
                // The 50% line: where the transition is centred, and the single
                // most useful reference on a control whose whole subject is a soft
                // edge. It is perpendicular to the drag ON SCREEN, which is only
                // true because the weight is aspect-corrected — see
                // `raw_core::dodgeburn::Linear`. Against the prototype's
                // uncorrected arithmetic this line would be a visible lie.
                let mid = egui::pos2((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
                let d = b - a;
                let len = d.length().max(1.0);
                let n = egui::vec2(-d.y, d.x) / len * (len * 0.35).clamp(12.0, 90.0);
                twice(&[mid - n, mid + n], false);
            }
            Some(r) => {
                // The outer ellipse, and the inner one when it is not a point.
                // Sampled rather than drawn as a circle: egui has no rotated
                // ellipse, and this one is turned and squashed by two independent
                // controls.
                let ring = |radius: f32| -> Vec<egui::Pos2> {
                    let (sin, cos) = r.angle.to_radians().sin_cos();
                    (0..=64)
                        .map(|i| {
                            let t = i as f32 / 64.0 * std::f32::consts::TAU;
                            // In width units, rotated, then back into the
                            // normalised space the map expects — the same order
                            // `Radial::weight_at` undoes.
                            let (ex, ey) = (radius * t.cos(), radius * r.aspect * t.sin());
                            let (wx, wy) = (ex * cos - ey * sin, ex * sin + ey * cos);
                            to_screen((r.cx + wx, r.cy + wy / aspect.max(1e-6)))
                        })
                        .collect()
                };
                twice(&ring(r.outer), true);
                if r.inner > 1e-4 {
                    twice(&ring(r.inner), true);
                }
                twice(&[a, b], false);
            }
        }

        // The handles last, so they sit over the lines that lead to them.
        for (at, filled) in [(a, true), (b, false)] {
            painter.circle_filled(at, 5.0, theme::INK_DARK);
            if filled {
                painter.circle_filled(at, 4.0, theme::INK_LIGHT);
            } else {
                painter.circle_filled(at, 4.0, theme::CHROME);
                painter.circle_stroke(at, 4.0, egui::Stroke::new(1.0, theme::INK_LIGHT));
            }
        }
    }

    /// The Dodge & Burn panel: the brush, the instance stack, and the selected
    /// instance's tonal-range mask.
    ///
    /// **A pane of its own**, tabbed behind Develop — the maintainer's call; see
    /// `layout::Pane::DodgeBurn` for the argument, and note the one thing it costs:
    /// the develop panel no longer reads as the pipeline in order, so where this
    /// stage actually sits (after Contrast Mask, before the curve) is written down
    /// in `raw_graph::build` rather than shown by the layout.
    ///
    /// It keeps the module frame rather than becoming loose controls in a panel.
    /// The frame is what carries the bypass dot and the reset, and this is a
    /// pipeline module with both — it is only its *controls* that are a tool.
    /// The Toning pane. See `crate::toning` for the body and the argument behind it.
    fn toning_panel(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        let mut clicks = HeadClicks::default();
        let scroll_id = ("toning-scroll", self.scroll_session, self.tabs.active_id());
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = layout::panel_head(ui, &self.icons, head, "TONING", |_| {});
                if self.tabs.active().is_none_or(|t| !t.has_image()) {
                    layout::empty_state(ui, "no image open");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt(scroll_id)
                    .show(ui, |ui| self.toning_body(ui));
            });
        clicks
    }

    fn toning_body(&mut self, ui: &mut egui::Ui) {
        // Split off before the tab borrow: disjoint fields of `self`, which the borrow
        // checker allows only when both are named directly here.
        let icons = &self.icons;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        // The distribution behind the ramp is the print's, which lives on the viewport
        // because that is where the proxy it is computed from lives. Copied out before
        // the borrows below, for the reason the curve editor's is.
        let zone_params = tab.params.effective();
        let hist = tab
            .render
            .as_mut()
            .and_then(|r| {
                let ready = r.viewport.request_zone_histogram(&zone_params);
                if ready.is_none() {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(16));
                }
                ready
            })
            .unwrap_or_default();

        let (next, act) = crate::toning::body(
            ui,
            icons,
            &tab.params.toning,
            &mut tab.placement_drag,
            &hist,
        );

        let mut params = next;
        let mut edited = act.changed;
        if let Some(key) = act.remove {
            params.applied.retain(|a| a.key != key);
            edited = true;
        }
        if let Some(key) = act.add {
            // The default is the amount a darkroom would actually start at rather than
            // zero: a treatment added and doing nothing is a row that looks broken.
            params.apply(key, 0.6);
            edited = true;
        }

        // Arm the module on any real edit — see `toning::arm`. Here rather than inside
        // `body`, because it has to see the chemistry after an add or a remove.
        if edited {
            crate::toning::arm(&tab.params.toning, &mut params);
        }

        // No history call here: the frame loop diffs `tab.params` before and after the
        // panels draw and coalesces a drag into one entry. Recording again would be a
        // second entry per gesture. See the `travelled` block in `update`.
        if edited && params != tab.params.toning {
            tab.params.toning = params;
        }
    }

    fn dodgeburn_panel(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        let mut clicks = HeadClicks::default();
        let scroll_id = (
            "dodgeburn-scroll",
            self.scroll_session,
            self.tabs.active_id(),
        );
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = layout::panel_head(ui, &self.icons, head, "DODGE / BURN", |_| {});
                if self.tabs.active().is_none_or(|t| !t.has_image()) {
                    layout::empty_state(ui, "no image open");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt(scroll_id)
                    .show(ui, |ui| self.dodgeburn_body(ui));
            });
        clicks
    }

    /// The Dodge & Burn body: the layer stack, and the ADD bench at its foot.
    ///
    /// Rebuilt to the maintainer's mockup. Three things about the shape of it are his and
    /// worth not undoing:
    ///
    /// - **A layer's options live inside the layer**, revealed by selecting it and
    ///   bracketed by a red rule above and below. The first version put SHAPE and
    ///   the tone mask in fixed sections under the list, which meant the controls for
    ///   the thing you were editing were nowhere near it and there was no way to
    ///   tell, from the list, which row they belonged to.
    /// - **Creating a layer happens at the bottom**, not the top: you pick a shape,
    ///   its settings appear, and then you commit with `+DODGE` or `+BURN`. The verb
    ///   comes last.
    /// - **They are LAYERS.** The code still says `Instance`, which is the
    ///   prototype's word and the sidecar's; the panel says the word the maintainer uses.
    fn dodgeburn_body(&mut self, ui: &mut egui::Ui) {
        // Split off before the tab borrow: disjoint fields of `self`, which the borrow
        // checker allows only when both are named directly here.
        let icons = &self.icons;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        // The ruler's ghost is the pre-D&B distribution, which lives on the viewport
        // because that is where the proxy it is computed from lives. Copied out
        // before the borrows below for the reason the curve editor's is.
        let zone_params = tab.params.effective();
        let zone_hist = tab
            .render
            .as_mut()
            .and_then(|r| {
                let ready = r.viewport.request_zone_histogram(&zone_params);
                if ready.is_none() {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(16));
                }
                ready
            })
            .unwrap_or_default();

        let mut select: Option<Option<usize>> = None;
        let mut remove: Option<usize> = None;
        let mut create: Option<raw_core::Sign> = None;
        let mut pick_shape: Option<paint::Pick> = None;
        let mut commit_rename = false;
        // Set by the brush's geometric rows; see `App::brush_adjusting`.
        let mut touched = false;

        let brush = &mut self.brush;
        let tool = self.tool;
        let active = tab.db_active;
        let n = tab.params.dodgeburn.instances.len();
        let full = n >= raw_core::DodgeBurnParams::MAX_INSTANCES;

        // The bench, pinned to the foot of the pane rather than scrolling with the
        // stack: it is where you go to start something, and a control that walks off
        // the bottom as the list grows is one you hunt for.
        // `Panel`, which in egui 0.35 replaced the four `*Panel` types with one.
        egui::containers::panel::Panel::bottom("db-add")
            .frame(
                egui::Frame::NONE
                    .fill(theme::CHROME)
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 8,
                        bottom: 10,
                    }),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                theme::rule(ui, theme::DIM.gamma_multiply(0.5));
                ui.add_space(8.0);

                // **The two verbs first, as one wide filled pair.** the maintainer's mockup,
                // and the change from the previous arrangement is that they are now
                // unmistakably the *action* — everything below them describes what
                // the action will make. Before, the verbs sat inline with the shape
                // brackets and read as four peers of which two happened to commit.
                //
                // Equal widths, computed rather than measured: see `theme::wide_button`.
                let gap = 8.0;
                let w = ((ui.available_width() - gap) * 0.5).max(40.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (sign, label) in [
                        (raw_core::Sign::Dodge, "+ DODGE"),
                        (raw_core::Sign::Burn, "+ BURN"),
                    ] {
                        let dodge = sign == raw_core::Sign::Dodge;
                        // **The text is the layer hue itself**, not a second saturated
                        // pair. the maintainer's grounds are darker than his inks, so the button
                        // can be set in the same `DODGE`/`BURN` a layer's kind is
                        // written in — the button and the rows it makes now agree.
                        let fill = if dodge {
                            theme::DODGE_FILL
                        } else {
                            theme::BURN_FILL
                        };
                        let text = if dodge { theme::DODGE } else { theme::BURN };
                        if theme::wide_button(ui, label, fill, text, w, !full)
                            .on_hover_text(theme::tip(if full {
                                "eight layers is the limit"
                            } else {
                                "make a layer of the shape selected below"
                            }))
                            .clicked()
                        {
                            create = Some(sign);
                        }
                    }
                });

                ui.add_space(10.0);
                // **One row of four shapes**, not a tool row with a nib row under it.
                // See `paint::Pick` for why the two collapsed into one.
                ui.horizontal(|ui| {
                    theme::tracked(ui, "SHAPE", theme::DIM);
                    ui.label(theme::caption(">"));
                    ui.add_space(4.0);
                    let current = paint::Pick::of(tool, brush.nib);
                    for p in paint::Pick::ALL {
                        if theme::bracket(ui, p.label(), current == p, theme::size::CAPTION)
                            .on_hover_text(theme::tip(p.tooltip()))
                            .clicked()
                        {
                            pick_shape = Some(p);
                        }
                    }
                });
                ui.add_space(2.0);
                // **Nothing else lives here.** the maintainer's rule: the bench makes a layer
                // and the layer holds its own options, which is how Radial already
                // worked and is now how all four do. A shape's settings sitting under
                // the button that creates it meant the controls for the thing you were
                // editing were nowhere near it — the same complaint that moved the
                // tonal range inside the layer rows in the first place.
            });

        egui::ScrollArea::vertical().show(ui, |ui| {
            let module = widgets::Module::new("DODGE / BURN")
                .modified(tab.params.dodgeburn.is_modified())
                .switch(tab.params.dodgeburn.enabled)
                .show(ui, |ui| {
                    theme::tracked(ui, "LAYERS", theme::DIM);
                    ui.add_space(2.0);
                    if n == 0 {
                        ui.label(theme::caption("add a layer below"));
                        return;
                    }
                    // Newest first: the one you just made is the one you are working
                    // on. Stacking order is unaffected — layers sum, so there is no
                    // order to preserve.
                    // Newest is index n-1 and is drawn first, so `first` is the top
                    // of the list rather than the bottom of the stack.
                    for (drawn, i) in (0..n).rev().enumerate() {
                        let selected = active == Some(i);
                        // A hairline between rows, faint enough to be a ruling rather
                        // than a border — the maintainer asked for it after the rows ran
                        // together at eight layers. Skipped above the first row and
                        // above a selected one, which brings its own ruby rule and
                        // would otherwise sit under a second line.
                        if drawn > 0 && !selected {
                            ui.add_space(2.0);
                            theme::rule(ui, theme::DIM.gamma_multiply(0.28));
                            ui.add_space(2.0);
                        }
                        if selected {
                            ui.add_space(3.0);
                            theme::rule(ui, theme::RUBY);
                            ui.add_space(3.0);
                        }
                        Self::layer_row(
                            ui,
                            tab,
                            i,
                            selected,
                            icons,
                            &mut select,
                            &mut remove,
                            &mut commit_rename,
                        );
                        if selected {
                            Self::layer_options(ui, tab, i, &zone_hist, brush, &mut touched);
                            ui.add_space(3.0);
                            theme::rule(ui, theme::RUBY);
                            ui.add_space(3.0);
                        }
                    }
                });
            tab.params.dodgeburn.enabled ^= module.bypass;
            if module.reset {
                tab.params.dodgeburn.instances.clear();
                tab.db_active = None;
            }

            // Clicking the panel's empty space deselects, which is what collapses the
            // open layer. Read from the whole remaining area rather than from a
            // widget, so there is somewhere to click even with a full list.
            let rest = ui.available_rect_before_wrap();
            if rest.height() > 4.0
                && ui
                    .interact(rest, ui.id().with("db-deselect"), egui::Sense::click())
                    .clicked()
            {
                select = Some(None);
            }
        });

        // Latched: a row reports `changed` only on frames its value moves, so a drag
        // held still would blink the nib out. Held for as long as the pointer is down,
        // which is exactly the length of the gesture.
        self.brush_adjusting =
            touched || (self.brush_adjusting && ui.ctx().input(|i| i.pointer.any_down()));

        if commit_rename && let Some((i, name)) = tab.db_rename.take() {
            // An empty name would give a row nothing to click and nothing to read,
            // so it is refused by keeping the old one rather than by complaining.
            if let Some(inst) = tab.params.dodgeburn.instances.get_mut(i)
                && !name.trim().is_empty()
            {
                inst.name = name.trim().to_owned();
            }
        }
        if let Some(p) = pick_shape {
            // The nib rides with the pick, because for a brush the pick *is* the nib
            // — see `paint::Pick`. Set before the tool so a mode already open reads
            // the nib the user just chose rather than the previous one.
            if let Some(nib) = p.nib() {
                self.brush.nib = nib;
            }
            self.tool = p.tool();
            if let tabs::Mode::Paint { tool: m, grab, .. } = &mut tab.mode {
                *m = p.tool();
                *grab = None;
            }
        }
        if let Some(i) = select {
            tab.db_active = i;
            tab.db_rename = None;
            match i.and_then(|i| tab.params.dodgeburn.instances.get(i)) {
                // **Selecting a layer arms it.** the maintainer's call, and it is what the
                // word means: the row you have highlighted is the one a drag on the
                // picture should go to. The tool comes from the layer's own shape,
                // so there is no state where the panel says radial and a press
                // paints a brush stroke.
                Some(inst) => {
                    let (sign, tool) = (inst.sign, paint::Tool::of(&inst.shape));
                    // **And the nib, for the same reason the tool is taken.** The
                    // note below says there is no state where the panel says radial
                    // and a press paints a brush stroke; without this there was
                    // exactly that state one level down — the SHAPE row said Card
                    // while a Round layer was selected, because the row reads
                    // `Pick::of(tool, brush.nib)` and only half of that pair was being
                    // restored. Now that a layer owns its nib there is something to
                    // restore it *from*.
                    if let Some(nib) = inst.shape.nib() {
                        self.brush.nib = nib;
                    }
                    let entered = match &tab.mode {
                        // Already painting: keep the snapshot `Esc` would restore,
                        // or switching layers mid-session would quietly redefine
                        // what "discard" means.
                        tabs::Mode::Paint { entered, .. } => (**entered).clone(),
                        _ => tab.params.dodgeburn.clone(),
                    };
                    tab.mode = tabs::Mode::paint(sign, tool, entered);
                    self.tool = tool;
                }
                None => {
                    tab.db_view_mask = false;
                    if tab.mode.is_paint() {
                        tab.mode = tabs::Mode::View;
                    }
                }
            }
        }
        if let Some(i) = remove.filter(|i| *i < tab.params.dodgeburn.instances.len()) {
            tab.params.dodgeburn.instances.remove(i);
            // The selection is an index into a list that just got shorter. Cleared
            // rather than shuffled: the alternative is a silently wrong row
            // highlighted, and the next stroke picks a sensible one anyway.
            tab.db_active = None;
            tab.db_rename = None;
        }
        if let Some(sign) = create {
            let tool = self.tool;
            let nib = self.brush.nib;
            // Creating selects, and opens the tool — you made it in order to use it.
            if paint::instance_for(
                &mut tab.params.dodgeburn,
                &mut tab.db_active,
                sign,
                tool,
                nib,
                true,
            )
            .is_some()
            {
                tab.mode = tabs::Mode::paint(sign, tool, tab.params.dodgeburn.clone());
            }
        }
    }

    /// One row of the layer stack: dot, name, kind, count, opacity, delete.
    ///
    /// It does not need to know whether it is the selected one — the red rules and
    /// the expansion are drawn by the caller, which is what keeps them wrapped
    /// around the whole block rather than around the row.
    #[allow(clippy::too_many_arguments)]
    fn layer_row(
        ui: &mut egui::Ui,
        tab: &mut tabs::Tab,
        i: usize,
        selected: bool,
        icons: &icons::Icons,
        select: &mut Option<Option<usize>>,
        remove: &mut Option<usize>,
        commit_rename: &mut bool,
    ) {
        let renaming = matches!(tab.db_rename, Some((r, _)) if r == i);
        let inst = &mut tab.params.dodgeburn.instances[i];
        let is_dodge = inst.sign == raw_core::Sign::Dodge;

        ui.horizontal(|ui| {
            // The module's own dot, not a lookalike: the maintainer asked for the same
            // preview/off/on behaviour and calling the same function is the only way
            // to be sure of it. `running` is the eye; `modified` is whether it has
            // anything on it.
            if widgets::dot(ui, inst.is_active(), inst.enabled, true).clicked() {
                inst.enabled = !inst.enabled;
            }

            if renaming {
                let Some((_, buf)) = &mut tab.db_rename else {
                    return;
                };
                let edit = ui.add(egui::TextEdit::singleline(buf).desired_width(120.0).font(
                    egui::FontId::new(theme::size::BODY, egui::FontFamily::Proportional),
                ));
                edit.request_focus();
                if edit.clicked_elsewhere()
                    || edit.lost_focus()
                    || ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    *commit_rename = true;
                }
                return;
            }

            // **Name, kind and pass count are one click target, and it is a deep
            // one.** the maintainer reported the rows as hard to hit: the strip was exactly as
            // tall as its text, so the gap between two rows was dead and a click aimed
            // between the letters missed. It now claims the row's full height.
            //
            // Everything in it is left-justified, which is the app's rule and was the
            // one place breaking it.
            //
            // **The name is inert, and that is the second half of the same complaint.**
            // It used to carry `Sense::click()` so it could hear a double-click and
            // start a rename — and because a child widget wins the hit test over the
            // strip drawn under it, a single click *on the words* went to the label,
            // which does nothing with one. Measured: clicking the name gave
            // `name.clicked() == true` and `strip.clicked() == false`. So the largest
            // and most obvious target in the row — the thing you aim at — was the one
            // place that would not open it, and you had to hit the gap to its right.
            // The strip hears both gestures now and the label senses nothing.
            let row_h = 20.0;
            let strip = ui
                .allocate_ui_with_layout(
                    egui::vec2(ui.available_width() - 78.0, row_h),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let mut label = egui::RichText::new(&inst.name)
                            .size(theme::size::BODY)
                            .color(if selected { theme::BRIGHT } else { theme::NAME });
                        if selected {
                            label = label.strong();
                        }
                        ui.add(egui::Label::new(label).selectable(false));
                        ui.label(theme::caption("·"));
                        ui.label(
                            // `ui_label`, not `label`: a brush row names its nib —
                            // ROUND or CARD — because with two nibs shipped, "BRUSH"
                            // is the one thing every brush layer has in common and
                            // therefore the one thing worth not saying. `label` is the
                            // sidecar's discriminant and stays put.
                            egui::RichText::new(inst.shape.ui_label().to_uppercase())
                                .size(theme::size::CAPTION)
                                .color(if is_dodge { theme::DODGE } else { theme::BURN }),
                        );
                        // The pass count, in brackets — the maintainer's format. One face now,
                        // so it needs no boxing to sit on the same baseline as the rest.
                        let passes = inst.gestures().len();
                        if passes > 0 {
                            ui.label(theme::caption("·"));
                            ui.label(theme::caption(format!("(x{passes})")));
                        }
                    },
                )
                .response
                .interact(egui::Sense::click());
            // **One click opens the layer, and one click closes it** — the maintainer's rule.
            // It used to only ever select, so a click on an open layer did nothing and
            // the only way to collapse one was to find empty space below the list. That
            // is a different gesture for the two halves of one action.
            //
            // **The double-click branch is `else`, and that is what makes the two
            // gestures stop fighting.** Measured, because the answer decides the design:
            // egui reports `clicked` on *every* click, so the second click of a
            // double-click arrives as `clicked && double_clicked` together. Handled
            // separately, opening a layer by double-clicking it would toggle twice and
            // land back closed. As an `else` the first click opens and the second
            // renames, which is the reading the maintainer asked for — "that way double-click
            // doesn't matter".
            //
            // This is also the measurement that retires **triple-click**, which the maintainer
            // raised: the third click reports `clicked && triple_clicked`, and the
            // *second* one still reports `double_clicked` on the way past. A triple
            // click contains a double click, so it cannot disambiguate from one — it
            // adds a gesture without removing the collision it was aimed at.
            if strip.double_clicked() {
                tab.db_rename = Some((i, inst.name.clone()));
            } else if strip.clicked() {
                *select = Some(if selected { None } else { Some(i) });
            }
            strip.on_hover_cursor(egui::CursorIcon::PointingHand);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // **The tab strip's close mark, not a lookalike.** the maintainer asked for the
                // same X, and the way to be sure of that is to paint the same icon
                // through the same helper rather than to set an `x` in the same grey
                // and hope. It grows a filled square on hover exactly as the tab's
                // does, which is the affordance that says a small target is a target.
                let (close_rect, close) =
                    ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                let hot = close.hovered();
                if hot {
                    ui.painter()
                        .rect_filled(close_rect, 2.0, egui::Color32::from_gray(72));
                }
                icons::paint(
                    ui,
                    icons,
                    "close",
                    "×",
                    close_rect,
                    if hot {
                        egui::Color32::from_gray(245)
                    } else {
                        theme::DIM
                    },
                );
                if close.on_hover_text(theme::tip("delete layer")).clicked() {
                    *remove = Some(i);
                }
                ui.add_space(2.0);
                // **A DragValue, not a disclosure.** It used to be a percentage that
                // opened a slider on a second row, which the maintainer has replaced: the value
                // is always visible, always draggable and always typeable, and the row
                // stays one line without a second state to be in. That also retires
                // `db_opacity`, the toggle whose ordering bug was the last thing to go
                // wrong here.
                //
                // Narrow, because a layer row is mostly its name.
                ui.add_sized(
                    [52.0, 16.0],
                    egui::DragValue::new(&mut inst.opacity)
                        .speed(0.01)
                        .range(0.0..=1.0)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                        .custom_parser(|t| {
                            t.trim()
                                .trim_end_matches('%')
                                .parse::<f64>()
                                .ok()
                                .map(|v| v / 100.0)
                        }),
                )
                .on_hover_text(theme::tip("layer opacity"));
            });
        });
    }

    /// What a selected layer expands to show: its shape, then its tonal range.
    #[allow(clippy::too_many_arguments)]
    fn layer_options(
        ui: &mut egui::Ui,
        tab: &mut tabs::Tab,
        i: usize,
        hist: &[f32],
        brush: &mut paint::Brush,
        touched: &mut bool,
    ) {
        ui.add_space(4.0);
        let mask_active = tab.params.dodgeburn.instances[i].mask.enabled;
        let view_mask = tab.db_view_mask;
        let inst = &mut tab.params.dodgeburn.instances[i];
        let option_label_size = theme::size::SLIDER;
        // One quiet hairline separates geometry from effect strength. It is fainter
        // than the rules between layers and carries no box or extra heading, so this
        // dense control stack gains structure without gaining visual weight.
        let amount_divider = |ui: &mut egui::Ui| {
            ui.add_space(3.0);
            theme::rule(ui, theme::DIM.gamma_multiply(0.22));
            ui.add_space(3.0);
        };

        match &mut inst.shape {
            // The brush's controls, shown against the layer they will paint into.
            //
            // They are **app-level tool state**, not fields on the layer: a dab stores
            // its own radius and feather, so these describe the *next* stroke rather
            // than the layer as a whole. the maintainer asked for them here anyway and he is
            // right — where a control lives should follow what you are working on, not
            // where the value happens to be stored, and a brush layer with no controls
            // beside it was the odd one out among four shapes.
            raw_core::dodgeburn::Shape::Brush { .. } => {
                widgets::check(ui, "Angle follows stroke", &mut brush.follow).on_hover_text(
                    theme::tip(
                        "Angle the nib to the direction you drag — narrow along the \
                         path, wide across it. Shading a horizon becomes one gesture.",
                    ),
                );
                // **Radius, Feather, Aspect, Angle, Intensity, Opacity** — the maintainer's
                // order. Size and softness are the two that get moved on nearly every
                // stroke, so they lead; aspect and angle shape the nib and are set
                // once; intensity and opacity are how hard it presses and belong
                // together at the end.
                *touched |= widgets::Row::new(
                    &mut brush.radius,
                    paint::Brush::default().radius,
                    paint::Brush::RADIUS_RANGE,
                    "Radius",
                )
                .label_size(option_label_size)
                .decimals(3)
                .show(ui);
                *touched |= widgets::Row::new(
                    &mut brush.feather,
                    paint::Brush::default().feather,
                    paint::Brush::FEATHER_RANGE,
                    "Feather",
                )
                .label_size(option_label_size)
                .show(ui);
                *touched |=
                    widgets::Row::new(&mut brush.aspect, 1.0, paint::Brush::ASPECT_RANGE, "Aspect")
                        .label_size(option_label_size)
                        .show(ui);
                ui.add_enabled_ui(!brush.follow, |ui| {
                    *touched |= widgets::Row::new(
                        &mut brush.angle,
                        0.0,
                        paint::Brush::ANGLE_RANGE,
                        "Angle",
                    )
                    .label_size(option_label_size)
                    .decimals(1)
                    .suffix("°")
                    .show(ui);
                });
                amount_divider(ui);
                widgets::Row::new(
                    &mut brush.intensity,
                    paint::Brush::default().intensity,
                    paint::Brush::INTENSITY_RANGE,
                    "Intensity",
                )
                .label_size(option_label_size)
                .suffix(" EV")
                .show(ui);
                widgets::Row::new(
                    &mut brush.opacity,
                    paint::Brush::default().opacity,
                    paint::Brush::OPACITY_RANGE,
                    "Opacity",
                )
                .label_size(option_label_size)
                .show(ui);
            }
            raw_core::dodgeburn::Shape::Linear(l) => {
                widgets::Row::new(
                    &mut l.feather,
                    1.0,
                    raw_core::dodgeburn::Linear::FEATHER_RANGE,
                    "Transition",
                )
                .label_size(option_label_size)
                .show(ui);
                amount_divider(ui);
                widgets::Row::new(&mut l.ev, 0.0, -4.0..=4.0, "Strength")
                    .label_size(option_label_size)
                    .suffix(" EV")
                    .show(ui);
            }
            raw_core::dodgeburn::Shape::Radial(r) => {
                widgets::Row::new(
                    &mut r.inner,
                    0.0,
                    raw_core::dodgeburn::Radial::INNER_RANGE,
                    "Core",
                )
                .label_size(option_label_size)
                .decimals(3)
                .show(ui);
                r.inner = r.inner.min(r.outer * 0.95);
                widgets::Row::new(
                    &mut r.aspect,
                    1.0,
                    raw_core::dodgeburn::Radial::ASPECT_RANGE,
                    "Aspect",
                )
                .label_size(option_label_size)
                .show(ui);
                widgets::Row::new(
                    &mut r.angle,
                    0.0,
                    raw_core::dodgeburn::Radial::ANGLE_RANGE,
                    "Angle",
                )
                .label_size(option_label_size)
                .decimals(1)
                .suffix("°")
                .show(ui);
                widgets::Row::new(
                    &mut r.feather,
                    1.0,
                    raw_core::dodgeburn::Radial::FEATHER_RANGE,
                    "Transition",
                )
                .label_size(option_label_size)
                .show(ui);
                // Left justified, not indented under the value column. The indent
                // was trying to align this with the sliders' tracks; what it actually
                // did was leave a button floating in the middle of the panel with
                // nothing above or below it to align *to*.
                ui.horizontal(|ui| {
                    if theme::bracket(ui, "Vignette", r.invert, theme::size::CAPTION)
                        .on_hover_text(theme::tip(
                            "Turn the spotlight inside out: nothing in the middle, full \
                             strength at the edges.",
                        ))
                        .clicked()
                    {
                        r.invert = !r.invert;
                    }
                });
                amount_divider(ui);
                widgets::Row::new(&mut r.ev, 0.0, -4.0..=4.0, "Strength")
                    .label_size(option_label_size)
                    .suffix(" EV")
                    .show(ui);
            }
        }

        widgets::Row::new(
            &mut inst.contrast,
            0.0,
            raw_core::DodgeBurnParams::CONTRAST_RANGE,
            "Contrast",
        )
        .label_size(option_label_size)
        .tip("Increase or soften local detail only inside this layer's shape and Tone Mask.")
        .show(ui);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            // **A body-size label rather than a tracked section heading.** the maintainer's
            // call, and it follows what the thing is: SHAPE and LAYERS head a bench
            // you pick from, this names one control group inside a layer's own
            // settings — so it sits at the size the settings beside it are set in.
            ui.label(theme::label("TONE MASK"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::bracket(
                    ui,
                    if mask_active { "ON" } else { "OFF" },
                    mask_active,
                    theme::size::CAPTION,
                )
                .clicked()
                {
                    tab.params.dodgeburn.instances[i].mask.enabled = !mask_active;
                }
                if theme::bracket(ui, "view mask", view_mask, theme::size::CAPTION)
                    .on_hover_text(theme::tip(
                        "Show the mask instead of the picture — a shape is easier to \
                         judge on its own.",
                    ))
                    .clicked()
                {
                    tab.db_view_mask = !view_mask;
                }
            });
        });

        let m = &mut tab.params.dodgeburn.instances[i].mask;
        widgets::zone_ruler(ui, m, hist, &mut tab.zone_drag);
        ui.horizontal(|ui| {
            for (label, lo, hi, f_lo, f_hi) in raw_core::ZoneMask::PRESETS {
                let on = m.enabled && (m.lo - lo).abs() < 1e-4 && (m.hi - hi).abs() < 1e-4;
                if theme::bracket(ui, label, on, theme::size::CAPTION).clicked() {
                    (m.lo, m.hi, m.f_lo, m.f_hi) = (lo, hi, f_lo, f_hi);
                    m.enabled = true;
                }
            }
            if theme::bracket(ui, "Inv", m.invert, theme::size::CAPTION).clicked() {
                m.invert = !m.invert;
            }
        });

        let id = ui.make_persistent_id(("zone-more", i));
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                ui.label(theme::caption("Edge and diffusion"));
            })
            // **`body_unindented`.** The default body indents, and an indented row is
            // handed less width than its neighbours — so Diffusion, Region and Edge got
            // a shorter track starting further right than the Radius and Feather
            // directly above them. The disclosure triangle already says these three
            // belong to the section; the indent was saying it a second time and paying
            // for it in the one alignment the panel is built on.
            .body_unindented(|ui| {
                widgets::Row::new(
                    &mut m.blur,
                    0.0,
                    raw_core::ZoneMask::BLUR_RANGE,
                    "Diffusion",
                )
                .decimals(3)
                .show(ui);
                ui.horizontal(|ui| {
                    if theme::bracket(ui, "Edge-aware", m.edge_aware, theme::size::CAPTION)
                        .on_hover_text(theme::tip(
                            "Reads exposure REGIONALLY, so a zone selects a coherent area \
                             with clean edges — a hand under the enlarger — instead of a \
                             scatter of pixels of the right brightness.",
                        ))
                        .clicked()
                    {
                        m.edge_aware = !m.edge_aware;
                    }
                });
                ui.add_enabled_ui(m.edge_aware, |ui| {
                    widgets::Row::new(
                        &mut m.region,
                        raw_core::ZoneMask::default().region,
                        raw_core::ZoneMask::REGION_RANGE,
                        "Region",
                    )
                    .decimals(3)
                    .show(ui);
                    widgets::Row::new(
                        &mut m.edge,
                        raw_core::ZoneMask::default().edge,
                        raw_core::ZoneMask::EDGE_RANGE,
                        "Edge",
                    )
                    .suffix(" EV")
                    .show(ui);
                });
            });
    }

    /// The History panel: the undo timeline as a list you can click.
    ///
    /// **It adds no state and no capability.** `History` has held every one of these
    /// entries from the start and `⌘Z`/`⌘⇧Z` have walked them since; this makes
    /// visible what was already there, which is the whole of the maintainer's ask. The one thing
    /// it needed was a *name* per row — `Params::what_changed`, in core, because naming
    /// a change is not a UI question.
    ///
    /// # Oldest at the top, unlike Snapshots
    ///
    /// The two panels sit one behind the other and run in opposite directions, which is
    /// worth defending rather than tidying. A snapshot list is a set of things you kept
    /// and the newest is the one you are working against, so it reads newest-first. A
    /// history is a **timeline**, and a timeline read downward runs forward in time —
    /// it is how every history panel in every editor works, and reversing it would make
    /// undo travel up the list while redo travels down.
    fn history_panel(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        // **The scroll area runs to the pane's edge, and the rows are inset instead.**
        //
        // the maintainer read the old arrangement as "a second, skinnier scroll area inside the
        // panel", and that is exactly what it looked like: the frame carried
        // `outer_margin.right = SCROLL_GUTTER`, so the scroll area — and therefore its
        // bar — started ten points in from the pane. A bar floating in from the edge
        // does not read as the panel's own.
        //
        // The gutter was there so rows would not pass under a bar that egui draws
        // *over* its content. Moving it inside the scroll area keeps that and puts the
        // bar where it belongs.
        //
        // **Only this panel.** Info, Snapshots and the two others share the shape, and
        // their bodies are module frames whose width arithmetic has a documented
        // runaway — see `widgets::STROKE`. Changing those blind is how that bug
        // happened; this list is plain rows and has no such sum.
        // **The head keeps the gutter even though the scroll area gives it up.** This
        // is the correction to the change described above, and the two are easy to
        // conflate. Moving `SCROLL_GUTTER` inside the scroll area was right for the
        // *bar*; it also moved the header, because the header was inside the frame that
        // carried the margin. So HISTORY's pop-out button ended up ten points nearer the
        // pane edge than the same button on every other panel — the maintainer spotted it against
        // Snapshots and Info, and it is the only one of the six that differs. Develop,
        // Toning, Dodge/Burn, Snapshots and Info all head their panel inside this margin.
        //
        // Wrapping only the head restores the row and leaves the scroll area full-bleed,
        // which is what the note above is defending. The button is chrome that belongs to
        // the panel and should sit where the other five sit; the scrollbar belongs to the
        // list and should sit on the pane's edge.
        let mut clicks = HeadClicks::default();
        let scroll_id = ("history-scroll", self.scroll_session, self.tabs.active_id());
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = layout::panel_head(ui, &self.icons, head, "HISTORY", |_| {});
            });
        if self.tabs.active().is_none_or(|t| !t.has_image()) {
            layout::empty_state(ui, "no image open");
            return clicks;
        }
        egui::ScrollArea::vertical()
            .id_salt(scroll_id)
            // **`auto_shrink` is why the first attempt did nothing.** It defaults to
            // `TRUE` on *both* axes, so a scroll area sizes its width to its content
            // — and the content here is inset by `SCROLL_GUTTER`, so the area shrank
            // by exactly the amount the inset was meant to give back and the bar
            // landed in the same place it started. Moving the margin inside the
            // scroll area was necessary and not sufficient.
            //
            // Horizontal shrink off, vertical left on: the area should span the
            // pane's width whatever the rows measure, and still be as short as a
            // short list.
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Frame::new()
                    .outer_margin(egui::Margin {
                        right: SCROLL_GUTTER,
                        ..Default::default()
                    })
                    .show(ui, |ui| self.history_body(ui));
            });
        clicks
    }

    /// The rows, and the click that travels to one.
    fn history_body(&mut self, ui: &mut egui::Ui) {
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };

        // The whole timeline, oldest first: everything behind the current state, the
        // current state, then everything ahead of it. `future` is a stack — the last
        // element is the next redo — so it is walked in reverse to come out in time
        // order.
        let past = tab.history.past();
        let future = tab.history.future();
        let timeline = (tab.id, tab.history.revision(), ui.ctx().viewport_id());
        let follow = self.history_seen != Some(timeline);
        self.history_seen = Some(timeline);
        let here = past.len();
        let mut states: Vec<&raw_core::Params> = Vec::with_capacity(past.len() + future.len() + 1);
        states.extend(past.iter());
        states.push(&tab.params);
        states.extend(future.iter().rev());

        let mut jump: Option<i32> = None;
        let mut current_rect = None;
        for (i, state) in states.iter().enumerate() {
            // What this state was *reached by*: the difference from the one before it.
            // The first row is the state before any edit and has no predecessor, so it
            // is named for what it is rather than for a change.
            let label = match i.checked_sub(1).and_then(|p| states.get(p)) {
                None => "Opened".to_owned(),
                Some(prev) => match prev.what_changed(state) {
                    Some((module, Some(control))) => format!("{module}  ·  {control}"),
                    Some((module, None)) => module.to_owned(),
                    None => "—".to_owned(),
                },
            };
            let current = i == here;
            // **Rows ahead of you stay listed, and stay dim.** Clicking back in time
            // does not throw the future away — it is still there to walk forward into,
            // exactly as `History::future` holds it. Drawing it at the same weight as
            // the past would be the panel claiming you had done those things, when what
            // is true is that you *had* and have stepped behind them.
            let ahead = i > here;
            let row = ui.horizontal(|ui| {
                // Inset from the pane edge like every other panel's content — a mark
                // welded to the container reads as though it has fallen off it, which
                // is the same complaint `TITLE_INSET` answers for the panel names.
                ui.add_space(6.0);
                // The ruby dot: the app's own mark for "this is the one", and the same
                // `dot` the develop modules and the settings rows use.
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                if current {
                    ui.painter().circle_filled(rect.center(), 3.0, theme::RUBY);
                }
                let text = egui::RichText::new(&label)
                    .size(theme::size::CAPTION)
                    .color(match (current, ahead) {
                        (true, _) => theme::BRIGHT,
                        (_, true) => theme::DIM.gamma_multiply(0.55),
                        _ => theme::DIM,
                    });
                let row = ui
                    .add(
                        egui::Label::new(text)
                            .sense(egui::Sense::click())
                            .selectable(false),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if row.clicked() {
                    jump = Some(i as i32 - here as i32);
                }
            });
            if current && follow {
                current_rect = Some(row.response.rect);
            }
        }

        // A history is read downward. When its timeline moves, place the current
        // state at the bottom edge so the newest edit never lands below the fold.
        // Keying this on `History::revision` rather than doing it every frame leaves
        // the panel free to be scrolled for review until another state is made.
        if let Some(rect) = current_rect {
            ui.scroll_to_rect(rect, Some(egui::Align::Max));
        }

        // **After the loop, and through `History::jump`.** It lands in `tab.params` like
        // any other edit — but the frame's undo recording must not then treat the jump
        // as a *new* edit, which is what `time_travelled` already suppresses for `⌘Z`.
        if let Some(delta) = jump.filter(|d| *d != 0) {
            tab.history.jump(&mut tab.params, delta);
            self.travelled_by_click = true;
        }
    }

    /// The two wide buttons at the foot of the snapshot panel.
    ///
    /// **The same widget the `+ DODGE` / `+ BURN` bench is**, and for the same reason it
    /// was made an exception in the first place: this is the panel's *primary action*,
    /// not a selected state, and a fill is how a primary action says so. Everything
    /// that states selected-ness in this app is an outline; nothing else is filled.
    ///
    /// **Ruby, then dull ruby** — the maintainer's pairing, and the ranking is the point. Capture
    /// is what the panel is for; compare is what you do with what you captured, and it
    /// does nothing at all until two snapshots are pinned. Two buttons of equal weight
    /// would make you choose between them; these make one obviously first.
    fn snapshot_bench(ui: &mut egui::Ui, capture: &mut bool, compare: &mut bool, pinned: usize) {
        ui.add_space(8.0);
        theme::rule(ui, theme::DIM.gamma_multiply(0.28));
        ui.add_space(8.0);
        // **Inset from both edges**, which the Dodge & Burn bench gets for free from the
        // module frame it sits inside and this does not — the snapshot list is a bare
        // panel body, so a button drawn across `available_width` runs into the pane's
        // own edges. Same inset the module frame uses, so the two benches line up.
        const PAD: f32 = 9.0;
        let gap = 8.0;
        // Equal widths, computed rather than measured — see `theme::wide_button`.
        let w = ((ui.available_width() - 2.0 * PAD - gap) * 0.5).max(40.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.add_space(PAD);
            *capture |= theme::wide_button(ui, "SNAPSHOT", theme::RUBY_FILL, theme::RUBY, w, true)
                .on_hover_text(theme::tip("Capture the look on screen  ·  ⌘K"))
                .clicked();
            // Disabled below two pins rather than hidden: a button that vanished would
            // make the panel change shape as you pinned, and the greyed one is what
            // says the grid needs two.
            let ready = pinned >= 2;
            *compare |=
                theme::wide_button(ui, "COMPARE", theme::RUBY_FILL_DIM, theme::RUBY, w, ready)
                    .on_hover_text(theme::tip(if ready {
                        "Show the pinned snapshots side by side  ·  k"
                    } else {
                        "pin two snapshots to compare them"
                    }))
                    .clicked();
        });
    }

    /// The Snapshots panel: capture, the list, and the pins compare reads.
    ///
    /// **Built from the primitives and adding none.** The row is the Dodge & Burn layer
    /// row's shape — a thumbnail where the dot is, the name, then the controls right —
    /// because a list of named things you pick from is a solved problem in this app and
    /// a second answer to it would be the drift `docs/decisions.md` was written to stop.
    ///
    /// # Restore is a button, and that is a departure from the prototype
    ///
    /// The prototype restores on a **double-click of the thumbnail** and renames on a
    /// double-click of the label — one gesture meaning two things depending on which
    /// half of the row is under it. That works there and it is the wrong import here,
    /// because this app has already spent double-click twice: *reset to default* on
    /// every slider row, and *rename* on a Dodge & Burn layer. A third meaning, chosen
    /// by target, is exactly the "one gesture, several meanings" the design system
    /// exists to prevent.
    ///
    /// So restore is an explicit button wearing the small grey reset face, and
    /// double-click keeps the one meaning it already has in a list of named rows:
    /// rename. Restore is undoable — it flows through the same diff-and-record path as
    /// a slider drag — so the button is a convenience rather than a safety rail.
    fn snapshot_panel(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        let mut clicks = HeadClicks::default();
        let scroll_id = (
            "snapshots-scroll",
            self.scroll_session,
            self.tabs.active_id(),
        );
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = layout::panel_head(ui, &self.icons, head, "SNAPSHOTS", |_| {});
                if self.tabs.active().is_none_or(|t| !t.has_image()) {
                    layout::empty_state(ui, "no image open");
                    return;
                }
                // **The bench is outside the scroll area and the list is not.** Two
                // things fall out of that and both were wrong before. The buttons no
                // longer scroll away under a long list, which is what a foot bench is
                // for; and they are sized against the *panel*, not against the scroll
                // area's content — which is what made them refuse to shrink and let
                // COMPARE run off the edge, because a `ScrollArea`'s available width is
                // its content's, and the rows inside had already claimed more than the
                // pane had to give.
                let room = (ui.available_height() - BENCH_H).max(0.0);
                egui::ScrollArea::vertical()
                    .id_salt(scroll_id)
                    .max_height(room)
                    .show(ui, |ui| self.snapshot_body(ui));
                let (mut capture, mut compare) = (false, false);
                let pinned = self.tabs.active().map_or(0, |t| t.snapshots.pinned_count());
                Self::snapshot_bench(ui, &mut capture, &mut compare, pinned);
                if capture {
                    self.capture_requested = self.tabs.active_id();
                }
                if compare && let Some(t) = self.tabs.active_mut() {
                    t.compare.open = !t.compare.open;
                    if t.compare.open {
                        t.compare.reset_view();
                    }
                }
            });
        clicks
    }

    /// The list, and what each row can do.
    fn snapshot_body(&mut self, ui: &mut egui::Ui) {
        let icons = &self.icons;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };

        // Deferred, like every other list in this app: the row that says "delete me"
        // is inside a loop borrowing the list it would shorten.
        let mut restore: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut pin: Option<usize> = None;
        let mut commit_rename = false;
        let mut refused_pin = false;

        if tab.snapshots.is_empty() {
            layout::empty_state(
                ui,
                "no snapshots yet\n⌘K captures the look you are looking at",
            );
            return;
        }

        let pins = tab.snapshots.pinned_count();
        for (i, snap) in tab.snapshots.iter().enumerate() {
            let renaming = matches!(tab.snap_rename, Some((r, _)) if r == i);
            // **Keyed on the capture index, not the list position.** egui derives a
            // widget's id from where it is, so with position as the salt deleting a row
            // would hand its id — and its text-edit contents and focus — to whichever
            // row moved up into the slot. The index is the one identity a snapshot has
            // that survives both a delete and a rename.
            ui.push_id(snap.index, |ui| {
                ui.horizontal(|ui| {
                    // The thumbnail sits where a Dodge & Burn layer's dot does, and does
                    // the same job at more resolution: it is how you tell one row from
                    // another without reading.
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(THUMB_W, THUMB_H), egui::Sense::hover());
                    match &snap.thumb {
                        Some(t) => {
                            // Fitted, not stretched: the picture's shape is one of the
                            // things you are comparing.
                            let fit = fit_rect(rect, t.size_vec2());
                            ui.painter().image(
                                t.id(),
                                fit,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                        // A snapshot whose read-back failed is still a snapshot. The look
                        // is the part that cannot be recovered; the picture is not.
                        None => {
                            ui.painter()
                                .rect_filled(rect, 2.0, egui::Color32::from_gray(30));
                        }
                    }

                    if renaming {
                        let Some((_, buf)) = &mut tab.snap_rename else {
                            return;
                        };
                        let edit =
                            ui.add(egui::TextEdit::singleline(buf).desired_width(110.0).font(
                                egui::FontId::new(
                                    theme::size::BODY,
                                    egui::FontFamily::Proportional,
                                ),
                            ));
                        edit.request_focus();
                        if edit.clicked_elsewhere()
                            || edit.lost_focus()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            commit_rename = true;
                        }
                        return;
                    }

                    let name = ui.add(
                        egui::Label::new(theme::label(&snap.label))
                            .sense(egui::Sense::click())
                            .selectable(false),
                    );
                    if name.double_clicked() {
                        tab.snap_rename = Some((i, snap.label.clone()));
                    }
                    name.on_hover_cursor(egui::CursorIcon::Text);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (close_rect, close) =
                            ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                        let hot = close.hovered();
                        if hot {
                            ui.painter()
                                .rect_filled(close_rect, 2.0, egui::Color32::from_gray(72));
                        }
                        icons::paint(
                            ui,
                            icons,
                            "close",
                            "×",
                            close_rect,
                            if hot {
                                egui::Color32::from_gray(245)
                            } else {
                                theme::DIM
                            },
                        );
                        if close.on_hover_text(theme::tip("delete snapshot")).clicked() {
                            remove = Some(i);
                        }
                        // **A full grid says so on the buttons that cannot be used.** The
                        // alternative — a pin that clicks and does nothing — is the
                        // "control that looks live and does nothing" this app keeps
                        // deciding against.
                        let full = pins >= snapshot::Snapshots::MAX_CELLS && !snap.pinned;
                        // **A diamond, ruby when pinned** — the maintainer's mark, and it is the
                        // theme's own rule read literally: ruby means "something is on that
                        // would not be on by default". A diamond has no direction and no
                        // verb in it, so it reads as a state rather than as a button.
                        let diamond = if snap.pinned {
                            "diamond-filled"
                        } else {
                            "diamond"
                        };
                        if icons::flag(ui, icons, diamond, "◆", snap.pinned, !full, icons::BOX)
                            .on_hover_text(theme::tip(if full {
                                "the grid holds four — unpin one first"
                            } else {
                                "show in the compare grid  ·  k"
                            }))
                            .clicked()
                        {
                            pin = Some(i);
                        }
                        if full {
                            refused_pin = true;
                        }
                        // **Restore warns, because it is the one row control that throws
                        // work away.** Pin and delete are reversible by doing them again;
                        // restore replaces everything you have done since — recoverable
                        // with ⌘Z, which is exactly what the hint has to say, because a
                        // warning that does not name its way out is just a discouragement.
                        if icons::sized(ui, icons, "skip-back", "⏮", true, icons::BOX)
                        .on_hover_text(theme::tip(
                            "Restore look — REPLACES current develop settings.  ⌘Z puts them back.",
                        ))
                        .clicked()
                    {
                        restore = Some(i);
                    }
                    });
                });
            });
            ui.add_space(2.0);
            theme::rule(ui, theme::DIM.gamma_multiply(0.28));
            ui.add_space(2.0);
        }
        let _ = refused_pin;

        if commit_rename && let Some((i, name)) = tab.snap_rename.take() {
            // An empty name leaves the row with nothing to read, so it is refused by
            // keeping the old one rather than by complaining.
            if let Some(s) = tab.snapshots.get_mut(i)
                && !name.trim().is_empty()
            {
                s.label = name.trim().to_owned();
            }
        }
        if let Some(i) = pin {
            tab.snapshots.toggle_pin(i);
        }
        if let Some(i) = remove {
            tab.snapshots.remove(i);
            tab.snap_rename = None;
        }
        // **Last, and through `restore_look_from`.** The decode and the demosaic stay
        // this image's; everything else comes back. It lands in `tab.params` like any
        // other edit, so the frame's diff works out what it cost and the history
        // records it — which is what makes ⌘Z undo a restore.
        if let Some(i) = restore {
            let look = tab.snapshots.iter().nth(i).map(|s| s.params.clone());
            if let Some(look) = look {
                tab.params.restore_look_from(&look);
                tab.status = "restored".into();
            }
        }
    }

    /// What the Info panel needs that does not live on the tab.
    ///
    /// Read before the body runs — see [`InfoEnv`] for why it is a value and not a
    /// borrow of `self`.
    fn info_env(&self) -> InfoEnv {
        InfoEnv {
            depth: self.persisted.0.depth,
            sample: self.settings.sample_area(),
            proof: self.settings.proof_target(),
            proof_scale: self.settings.proof_scale(),
            unit: self.settings.print_unit(),
            exporting: self.export_rx.is_some(),
        }
    }

    /// Act on the Info panel's EXPORT block.
    ///
    /// The buttons set flags rather than calling straight through, for the reason
    /// `capture_requested` does: the panel draws in the middle of the frame holding a
    /// borrow of the tab, and exporting needs `&mut self`.
    fn take_info_clicks(&mut self, clicks: InfoClicks) {
        // The master wins if both arrive in one frame, which they cannot from the
        // panel — but `⌘E` and a click can land together, and losing the master to a
        // proof would be the wrong way round.
        if clicks.export {
            self.export_requested = self.tabs.active_id().map(|id| (id, ExportKind::Master));
        } else if clicks.proof {
            self.export_requested = self.tabs.active_id().map(|id| (id, ExportKind::Proof));
        }
        if clicks.settings {
            self.settings_open = true;
            self.settings_raise = true;
        }
    }

    /// The Info panel: its header, then the sections.
    fn info_panel(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        let mut clicks = HeadClicks::default();
        let mut info = InfoClicks::default();
        let env = self.info_env();
        let scroll_id = ("info-scroll", self.scroll_session, self.tabs.active_id());
        egui::Frame::new()
            .outer_margin(egui::Margin {
                right: SCROLL_GUTTER,
                ..Default::default()
            })
            .show(ui, |ui| {
                clicks = layout::panel_head(ui, &self.icons, head, "INFO", |_| {});
                // Bound out of `self` so the closure captures the field rather than
                // the whole struct — `tabs` is taken mutably two lines down.
                let icons = &self.icons;
                match self.tabs.active_mut() {
                    Some(tab) => {
                        egui::ScrollArea::vertical()
                            .id_salt(scroll_id)
                            .show(ui, |ui| info = info_body(tab, ui, env, icons));
                    }
                    None => {
                        layout::empty_state(ui, "no image open");
                    }
                }
            });
        self.take_info_clicks(info);
        clicks
    }

    /// Develop, in an OS window of its own.
    ///
    /// **An immediate viewport, not a deferred one.** A deferred viewport's callback
    /// must be `Fn + Send + Sync + 'static`, which a panel that mutates `App` cannot
    /// be; immediate ones take `FnMut` and can borrow. The cost is that it renders
    /// synchronously inside the parent frame, which for a panel of sliders is
    /// nothing. Where the backend cannot make a real window, egui falls back to an
    /// embedded one — the panel is still usable, just not detached.
    fn float_develop(&mut self, ctx: &egui::Context) {
        let mut docked = false;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-develop"),
            platform::panel_viewport("develop", [360.0, 820.0]),
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    docked = true;
                }
                self.toggle_panels |= take_bare_tab(ui.ctx());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        Self::floating_bar(&self.icons, ui, "DEVELOP", &mut docked);
                        let _ = self.develop_column(ui, Head::FLOATING);
                    });
            },
        );
        // Closing the window is docking, not hiding. A panel that vanished with no
        // way back would be a control that destroys itself.
        if docked {
            self.layout.dock(Pane::Develop);
        }
    }

    /// The brush, in an OS window of its own.
    ///
    /// The case this is *for*: painting with the panel on a second screen and the
    /// picture filling the first. It is why the brush is a pane rather than a module
    /// — a module cannot be popped out, and this is the one panel you want beside
    /// the image rather than in front of it.
    fn float_dodgeburn(&mut self, ctx: &egui::Context) {
        let mut docked = false;
        let scroll_id = (
            "dodgeburn-scroll",
            self.scroll_session,
            self.tabs.active_id(),
        );
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-dodgeburn"),
            platform::panel_viewport("dodge / burn", [340.0, 620.0]),
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    docked = true;
                }
                self.toggle_panels |= take_bare_tab(ui.ctx());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        Self::floating_bar(&self.icons, ui, "DODGE / BURN", &mut docked);
                        ui.add_space(4.0);
                        if self.tabs.active().is_none_or(|t| !t.has_image()) {
                            layout::empty_state(ui, "no image open");
                            return;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt(scroll_id)
                            .show(ui, |ui| self.dodgeburn_body(ui));
                    });
            },
        );
        if docked {
            self.layout.dock(Pane::DodgeBurn);
        }
    }

    fn float_info(&mut self, ctx: &egui::Context) {
        let mut docked = false;
        let mut info = InfoClicks::default();
        let env = self.info_env();
        let scroll_id = ("info-scroll", self.scroll_session, self.tabs.active_id());
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-info"),
            platform::panel_viewport("info", [340.0, 620.0]),
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    docked = true;
                }
                self.toggle_panels |= take_bare_tab(ui.ctx());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        Self::floating_bar(&self.icons, ui, "INFO", &mut docked);
                        ui.add_space(4.0);
                        let icons = &self.icons;
                        match self.tabs.active_mut() {
                            Some(tab) => {
                                egui::ScrollArea::vertical()
                                    .id_salt(scroll_id)
                                    .show(ui, |ui| info = info_body(tab, ui, env, icons));
                            }
                            None => {
                                layout::empty_state(ui, "no image open");
                            }
                        }
                    });
            },
        );
        self.take_info_clicks(info);
        if docked {
            self.layout.dock(Pane::Info);
        }
    }

    fn float_snapshots(&mut self, ctx: &egui::Context) {
        let mut docked = false;
        let scroll_id = (
            "snapshots-scroll",
            self.scroll_session,
            self.tabs.active_id(),
        );
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-snapshots"),
            platform::panel_viewport("snapshots", [340.0, 620.0]),
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    docked = true;
                }
                self.toggle_panels |= take_bare_tab(ui.ctx());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        Self::floating_bar(&self.icons, ui, "SNAPSHOTS", &mut docked);
                        ui.add_space(4.0);
                        if self.tabs.active().is_none_or(|t| !t.has_image()) {
                            layout::empty_state(ui, "no image open");
                            return;
                        }
                        let room = (ui.available_height() - BENCH_H).max(0.0);
                        egui::ScrollArea::vertical()
                            .id_salt(scroll_id)
                            .max_height(room)
                            .show(ui, |ui| self.snapshot_body(ui));
                        let (mut capture, mut compare) = (false, false);
                        let pinned = self.tabs.active().map_or(0, |t| t.snapshots.pinned_count());
                        Self::snapshot_bench(ui, &mut capture, &mut compare, pinned);
                        if capture {
                            self.capture_requested = self.tabs.active_id();
                        }
                        if compare && let Some(t) = self.tabs.active_mut() {
                            t.compare.open = !t.compare.open;
                            if t.compare.open {
                                t.compare.reset_view();
                            }
                        }
                    });
            },
        );
        if docked {
            self.layout.dock(Pane::Snapshots);
        }
    }

    fn float_history(&mut self, ctx: &egui::Context) {
        let mut docked = false;
        let scroll_id = ("history-scroll", self.scroll_session, self.tabs.active_id());
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-history"),
            platform::panel_viewport("history", [340.0, 620.0]),
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    docked = true;
                }
                self.toggle_panels |= take_bare_tab(ui.ctx());
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ui, |ui| {
                        Self::floating_bar(&self.icons, ui, "HISTORY", &mut docked);
                        ui.add_space(4.0);
                        if self.tabs.active().is_none_or(|t| !t.has_image()) {
                            layout::empty_state(ui, "no image open");
                            return;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt(scroll_id)
                            .show(ui, |ui| self.history_body(ui));
                    });
            },
        );
        if docked {
            self.layout.dock(Pane::History);
        }
    }

    /// The part of the develop panel that does **not** scroll.
    ///
    /// The title, the whole-panel reset, and the histogram. The histogram is pinned
    /// because it is the one thing in this panel you read *while* adjusting
    /// something else — scrolling to the exposure sliders used to take it off screen,
    /// which is the wrong way round: the control moves, the reference should not.
    fn develop_head(&mut self, ui: &mut egui::Ui, head: Head) -> HeadClicks {
        // The panel title, with the whole-panel reset beside it. No rule under it:
        // the module strokes already divide the panel, and a separator on top of
        // them is a second grammar for the same job.
        //
        // The reset is offered only when there is something to reset, the same rule
        // a module's own reset follows: a control that looks live and does nothing is
        // worse than no control.
        let mut reset = false;
        let has_image = self.tabs.active().is_some();
        let clicks = layout::panel_head(ui, &self.icons, head, "DEVELOP", |ui| {
            if has_image {
                reset = theme::reset_button(
                    ui,
                    "reset all",
                    "reset every Develop setting, including Decode and Luminance",
                )
                .clicked();
            }
        });

        let Some(tab) = self.tabs.active_mut() else {
            return clicks;
        };
        if reset {
            tab.params = Params::default();
        }

        refresh_tab_mapping(tab);
        widgets::Plain::new("TONAL DISTRIBUTION")
            .open_on_start(true)
            .show(ui, |ui| {
                if tab.luma.is_some() {
                    if tab.histogram.mode == histogram::Mode::Rgb {
                        let shown = tab.render_params();
                        if let Some(img) = &tab.image {
                            tab.histogram.refresh_rgb(&img.decoded.scene, &shown, tab.scene_gen);
                        }
                    }
                    let h = &tab.histogram;
                    let resp = match h.mode {
                        histogram::Mode::Tonal => {
                            widgets::histogram(ui, histogram::BINS, Some(&h.tonal), None)
                        }
                        histogram::Mode::Rgb => widgets::histogram(
                            ui,
                            histogram::BINS,
                            None,
                            Some([&h.rgb[0], &h.rgb[1], &h.rgb[2]]),
                        ),
                    };
                    if resp.clicked() {
                        tab.histogram.mode = tab.histogram.mode.next();
                    }
                    resp.on_hover_text(theme::tip("Click to cycle. Tonal: finished image through Contrast Mask, Dodge & Burn, curves, display mapping and toning. CFA RGB: intermediate photosite channels. Dither, output sharpening and grain are excluded. Height uses square-root scaling."));
                    let path = match tab.histogram.mode {
                        histogram::Mode::Tonal => "finished image",
                        histogram::Mode::Rgb => "photosites → display",
                    };
                    ui.label(theme::caption(format!(
                        "{path} · {}",
                        tab.histogram.mode.label()
                    )));
                } else {
                    ui.label(theme::caption("—"));
                }
            });
        clicks
    }

    fn develop_panel(&mut self, ui: &mut egui::Ui) {
        let can_export = self.tabs.active().is_some_and(|t| {
            t.has_image()
                && t.output_dims()
                    .is_none_or(|d| master_layout(&t.params, d).is_ok())
        }) && self.export_rx.is_none();
        let exporting = self.export_rx.is_some();
        let export_target = &mut self.export_target;
        let mut export_requested = false;
        let mut save_duplicate = false;
        // Composition's two buttons act on the tab, which this function has borrowed
        // mutably for its whole body — the same reason `export_requested` is a flag
        // rather than a call.
        let mut rotate: Option<bool> = None;
        // The loupe's checkbox changes the interaction mode, and the mode lives on
        // the tab this function has borrowed for its whole body — the same reason
        // `toggle_crop` below is a flag rather than a call.
        let mut toggle_loupe = false;
        let mut toggle_crop = false;
        let mut open_crop = false;
        let mut reset_crop = false;
        let mut keystone_tool: Option<KeystoneMode> = None;
        let mut reset_keystone = false;
        // FRAME's Optical button needs an immutable look at the developed picture,
        // while the module UI holds a mutable borrow of its parameters. Defer the
        // one-shot analysis until that UI borrow has ended.
        let mut optical_frame_requested = false;
        // Split off before the tab borrow: disjoint fields of `self`, which the
        // borrow checker allows only when both are named directly here.
        let icons = &self.icons;
        let curve_presets = &mut self.curve_presets;
        let curve_preset_name = &mut self.curve_preset_name;
        let pending_note = &mut self.pending_note;
        // The unit is a preference and lives in Settings, but its toggle belongs in
        // the panel where the sizes are. Read out and written back around the tab
        // borrow, the same way the composition buttons are.
        let unit = self.settings.print_unit();
        let settings_ppi = self.settings.print_ppi;
        // Read here with `settings_ppi`, before the panel body borrows what it needs.
        // See `space_defaults` below, which is where these are used.
        let settings_space_tiff = self.settings.space_for(export::Container::Tiff);
        let settings_space_png = self.settings.space_for(export::Container::Png);
        let settings_space_jpeg = self.settings.space_for(export::Container::Jpeg);
        let mut new_unit: Option<Unit> = None;

        // **With nothing open the panel still draws its modules, disabled.** It used
        // to be an empty box saying "no image open", which made the app look
        // half-built at the moment a new user first sees it — and said nothing about
        // what the app *does*. The stack is the app's description of itself.
        //
        // Disjoint fields of `self`, which the borrow checker allows only when both
        // are named directly here — the same reason `icons` is split off above.
        let live = self.tabs.active().is_some();
        let tab = match self.tabs.active_mut() {
            Some(t) => t,
            None => &mut self.placeholder,
        };
        if !live {
            // One line, and it does the whole job: every widget added to this `Ui`
            // after it is greyed and inert. Says what the panel is for without
            // claiming any of it can be used yet.
            layout::empty_state(ui, "no image open — open one to develop it");
            ui.add_space(4.0);
            ui.disable();
        }

        // **Nothing here is justified to the *panel's* right edge** — that edge is
        // where the scrollbar lives, and it is what swallowed the Contrast Mask
        // checkbox and the curve's Reset. The resets below are justified to their
        // own container's inner edge, which is inset from it, and the scrollbar now
        // allocates its own width instead of floating over the content.
        //
        // The modified flags are per *module*, which is not always one `Params`
        // group: the panel is organised by what the user is doing and `Params` by
        // what re-running it costs, so DECODE spans `decode` plus the sampling half
        // of `luminance`. That mismatch is deliberate on both sides — see the note
        // at the top of `raw_core::params`.
        let decode = widgets::Module::new("DECODE")
            .open_on_start(true)
            .modified(
                tab.params.decode != raw_core::DecodeOptions::default()
                    || tab.params.luminance.sampling != Sampling::default(),
            )
            .show(ui, |ui| {
                egui::ComboBox::from_id_salt("sampling")
                    .selected_text(sampling_label(tab.params.luminance.sampling))
                    .show_ui(ui, |ui| {
                        for s in SAMPLING_ORDER {
                            // Discriminant, not equality: Demosaic carries the
                            // algorithm, so comparing by value would deselect the entry
                            // the moment the algorithm below it changed. Same reason as
                            // the tone map.
                            let selected = std::mem::discriminant(&tab.params.luminance.sampling)
                                == std::mem::discriminant(&s);
                            if ui.selectable_label(selected, sampling_label(s)).clicked()
                                && !selected
                            {
                                // Carry the current algorithm across rather than
                                // snapping to the list entry's, so returning to
                                // Demosaic returns to the mode you left.
                                tab.params.luminance.sampling =
                                    match (s, tab.params.luminance.sampling) {
                                        (Sampling::Demosaic(_), Sampling::Demosaic(had)) => {
                                            Sampling::Demosaic(had)
                                        }
                                        (other, _) => other,
                                    };
                            }
                        }
                    });

                // The control that makes the mode mean anything, revealed only when it
                // applies — see `Weighting::Weighted` for the mistake this pattern avoids.
                if let Sampling::Demosaic(current) = tab.params.luminance.sampling {
                    ui.add_space(4.0);
                    egui::ComboBox::from_id_salt("demosaic")
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for a in DemosaicAlgo::UI_ORDER {
                                if ui
                                    .selectable_label(current == a, a.label())
                                    .on_hover_text(theme::tip(a.tooltip()))
                                    .clicked()
                                {
                                    tab.params.luminance.sampling = Sampling::Demosaic(a);
                                }
                            }
                        })
                        .response
                        .on_hover_text(theme::tip(current.tooltip()));
                }

                ui.checkbox(
                    &mut tab.params.decode.unity_wb,
                    theme::label("Unity WB (show CFA)"),
                )
                .on_hover_text(theme::tip(
                    "Diagnostic. Turns off gain equalization so the Bayer pattern shows; \
                         equalization should make it vanish.",
                ));
            });
        if decode.reset {
            tab.params.decode = raw_core::DecodeOptions::default();
            tab.params.luminance.sampling = Sampling::default();
        }

        let luminance = widgets::Module::new("LUMINANCE")
            .open_on_start(true)
            .modified(tab.params.luminance.weighting != Weighting::default())
            .show(ui, |ui| {
                egui::ComboBox::from_id_salt("weighting")
                    .selected_text(tab.params.luminance.weighting.label())
                    .show_ui(ui, |ui| {
                        for w in Weighting::UI_ORDER {
                            let selected = std::mem::discriminant(&tab.params.luminance.weighting)
                                == std::mem::discriminant(&w);
                            // The list entry for Weighted shows the generic name; the
                            // selected text above shows the actual mix.
                            let text = if matches!(w, Weighting::Weighted(..)) {
                                "Weighted (custom)".to_string()
                            } else {
                                w.label()
                            };
                            if ui.selectable_label(selected, text).clicked() {
                                // Seed from the current mix so selecting Weighted
                                // unlocks the sliders where you already are rather
                                // than jumping.
                                tab.params.luminance.weighting = match w {
                                    Weighting::Weighted(..) => {
                                        tab.params.luminance.weighting.as_weighted()
                                    }
                                    other => other,
                                };
                            }
                        }
                    });

                // The sliders that make Weighted mean anything. Without them the mode
                // was indistinguishable from Equal, because `Weighted(1,1,1)`
                // normalises to it.
                if let Some((r, g, b)) = tab.params.luminance.weighting.mix_mut() {
                    ui.add_space(4.0);
                    widgets::Slider::new(r, 1.0, 0.0..=1.0, "R")
                        .decimals(2)
                        .show(ui);
                    widgets::Slider::new(g, 1.0, 0.0..=1.0, "G")
                        .decimals(2)
                        .show(ui);
                    widgets::Slider::new(b, 1.0, 0.0..=1.0, "B")
                        .decimals(2)
                        .show(ui);
                    ui.label(theme::caption(
                        "only the ratios matter — normalized to unit sum, so the mix \
                         changes spectral character, not brightness",
                    ));
                }
            });
        if luminance.reset {
            tab.params.luminance.weighting = Weighting::default();
        }

        // From here down the modules can be switched off, and **their controls stay
        // live while bypassed**. Greying them out was the obvious alternative and is
        // wrong twice over: it defeats the point of a bypass that keeps its values —
        // setting a module up with it off, then switching on to see it, is exactly
        // the gesture this is for — and the curve editor paints itself, so
        // `add_enabled_ui` would block its interaction without dimming a pixel of
        // it. The dot is the indication. Contrast Mask did grey out, alone among the
        // modules, because until now it was the only one that could be switched off.
        let exposure = widgets::Module::new("EXPOSURE")
            .open_on_start(true)
            .modified(tab.params.exposure.is_modified())
            .switch(tab.params.exposure.enabled)
            .show(ui, |ui| {
                widgets::Slider::new(&mut tab.params.exposure.ev, 0.0, -6.0..=6.0, "Exposure")
                    .suffix(" EV")
                    .show(ui);
                // ±0.150, not ±0.050: the small range could trim sensor-noise offset
                // but not crush shadows, and crushing the black end is a print
                // decision, not a calibration one. Scene values run to ~2.0, so 0.15
                // is a real bite.
                // **`Black Corr.`, not `Black correction`.** the maintainer's abbreviation, and
                // it buys track: the label column is sized to the longest label in the
                // app, so the two longest ones were costing every slider in every module
                // several points of length. See `widgets::Row::LABEL_W`.
                widgets::Slider::new(
                    &mut tab.params.exposure.black,
                    0.0,
                    -0.15..=0.15,
                    "Black corr.",
                )
                .decimals(4)
                .show(ui);
            });
        tab.params.exposure.enabled ^= exposure.bypass;
        if exposure.reset {
            // Not the whole struct: a reset returns the values to neutral and leaves
            // the bypass where the user put it. They are two controls.
            let e = &mut tab.params.exposure;
            (e.ev, e.black) = (
                ExposureParams::default().ev,
                ExposureParams::default().black,
            );
        }

        // **Snapshotted with the switch held at one value**, so the dot cannot read as
        // an edit and arm the very module it was just used to switch off. Same shape as
        // GRAIN's `grain_before`, and the same trap.
        let mask_before = ContrastMaskParams {
            enabled: false,
            ..tab.params.contrast_mask
        };
        let mask = widgets::Module::new("CONTRAST MASK")
            .open_on_start(false)
            .modified(tab.params.contrast_mask.is_modified())
            .switch(tab.params.contrast_mask.enabled)
            .show(ui, |ui| {
                let cm = &mut tab.params.contrast_mask;
                widgets::Slider::new(
                    &mut cm.contrast,
                    ContrastMaskParams::default().contrast,
                    ContrastMaskParams::CONTRAST_RANGE,
                    "Mask contrast",
                )
                .decimals(2)
                .show(ui);
                widgets::Slider::new(
                    &mut cm.spacer,
                    ContrastMaskParams::default().spacer,
                    ContrastMaskParams::SPACER_RANGE,
                    "Spacer distance",
                )
                .decimals(2)
                .suffix(" %")
                .show(ui);
                widgets::Slider::new(
                    &mut cm.offset.0,
                    0.0,
                    ContrastMaskParams::OFFSET_RANGE,
                    "Registration X",
                )
                .decimals(1)
                .suffix(" px")
                .show(ui);
                widgets::Slider::new(
                    &mut cm.offset.1,
                    0.0,
                    ContrastMaskParams::OFFSET_RANGE,
                    "Registration Y",
                )
                .decimals(1)
                .suffix(" px")
                .show(ui);
            });
        tab.params.contrast_mask.enabled ^= mask.bypass;
        // **A slider arms the module**, the same rule GRAIN follows and for the same
        // reason: the first thing you do to a switched-off module must not be nothing.
        // the maintainer moved these four and the picture did not change, which is a control
        // that looks live and is not — and this module is the one where that is hardest
        // to spot, because the mask is a *spatial* effect and a viewer who cannot see
        // one has no way to tell "off" from "set very gently".
        //
        // After the bypass rather than before it: applying the click first and then
        // arming means a frame in which you both nudge a slider and click the dot ends
        // up on, which is the reading that matches the gesture that did more.
        let mask_edited = ContrastMaskParams {
            enabled: false,
            ..tab.params.contrast_mask
        } != mask_before;
        widgets::arm(mask_edited, &mut tab.params.contrast_mask.enabled);
        if mask.reset {
            tab.params.contrast_mask = ContrastMaskParams {
                enabled: tab.params.contrast_mask.enabled,
                ..Default::default()
            };
        }

        // Bin sampled tones after the preceding layers, never remap coarse bins.
        tab.curve_active = tab
            .curve_active
            .min(tab.params.curve.instances.len().saturating_sub(1));
        let ev_span = raw_core::curve::HI_EV - raw_core::curve::LO_EV;
        let resettable = tab
            .params
            .curve
            .instances
            .get(tab.curve_active)
            .is_some_and(|i| !i.curve.is_identity() || i.opacity != 1.0);
        let picking_curve_point = tab.mode.is_curve_point();
        let mut select_curve = None;
        let mut remove_curve = None;
        let mut commit_curve_rename = false;
        let mut toggle_curve_picker = false;
        let mut add_curve_instance = false;
        let mut save_curve_preset = false;
        let mut load_curve_preset = None;
        let mut delete_curve_preset = None;
        let curve = widgets::Module::new("CURVE")
            .open_on_start(false)
            .modified(tab.params.curve.is_modified())
            .resettable(resettable)
            .switch(tab.params.curve.enabled)
            .show(ui, |ui| {
                refresh_tab_curve_samples(tab);
                let curve_ghost = tab.histogram.curve_input(&tab.params.curve, tab.curve_active);
                ui.horizontal(|ui| {
                    theme::tracked_at(ui, "INSTANCES", theme::DIM, theme::size::SECTION - 2.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("⋯", |ui| {
                            if ui.button("Save Selected Curve as Preset…").clicked() {
                                save_curve_preset = true;
                                ui.close();
                            }
                            ui.menu_button("Load Preset", |ui| {
                                if curve_presets.presets.is_empty() {
                                    ui.label(
                                        theme::caption("No saved presets").color(theme::DIM),
                                    );
                                }
                                for (index, preset) in curve_presets.presets.iter().enumerate() {
                                    if ui.button(&preset.name).clicked() {
                                        load_curve_preset = Some(index);
                                        ui.close();
                                    }
                                }
                            });
                            ui.menu_button("Delete Preset", |ui| {
                                if curve_presets.presets.is_empty() {
                                    ui.label(
                                        theme::caption("No saved presets").color(theme::DIM),
                                    );
                                }
                                for (index, preset) in curve_presets.presets.iter().enumerate() {
                                    if ui.button(&preset.name).clicked() {
                                        delete_curve_preset = Some(index);
                                        ui.close();
                                    }
                                }
                            });
                        })
                        .response
                        .on_hover_text(theme::tip("save or add individual Curve presets"));
                        // Added after `⋯` so it lands to its left: this layout places
                        // right to left, and the reading order is `+` then the menu.
                        add_curve_instance = ui
                            .add_sized(
                                [18.0, 18.0],
                                egui::Button::new(
                                    egui::RichText::new("+")
                                        .size(theme::size::RESET + 2.0)
                                        .color(theme::DIM),
                                ),
                            )
                            .on_hover_text(theme::tip("add curve instance"))
                            .clicked();
                    });
                });
                ui.add_space(2.0);
                let n = tab.params.curve.instances.len();
                for i in 0..n {
                    let selected = i == tab.curve_active;
                    let renaming = matches!(tab.curve_rename, Some((r, _)) if r == i);
                    if selected {
                        theme::rule(ui, theme::RUBY.gamma_multiply(0.75));
                    }
                    let instance = &mut tab.params.curve.instances[i];
                    ui.horizontal(|ui| {
                        if widgets::dot(
                            ui,
                            !instance.curve.is_identity(),
                            instance.curve.enabled,
                            true,
                        )
                        .on_hover_text(theme::tip(if instance.curve.enabled {
                            "click to bypass this curve instance"
                        } else {
                            "bypassed — click to switch this curve instance back on"
                        }))
                        .clicked()
                        {
                            instance.curve.enabled = !instance.curve.enabled;
                        }

                        if renaming {
                            let Some((_, buf)) = &mut tab.curve_rename else { return };
                            let edit = ui.add(
                                egui::TextEdit::singleline(buf)
                                    .desired_width((ui.available_width() - 78.0).max(60.0))
                                    .font(egui::FontId::new(
                                        theme::size::BODY,
                                        egui::FontFamily::Proportional,
                                    )),
                            );
                            edit.request_focus();
                            if edit.clicked_elsewhere()
                                || edit.lost_focus()
                                || ui.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                commit_curve_rename = true;
                            }
                        } else {
                            let row = ui
                                .allocate_ui_with_layout(
                                    egui::vec2((ui.available_width() - 78.0).max(40.0), 20.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        let text = egui::RichText::new(&instance.name)
                                            .size(theme::size::BODY)
                                            .color(if selected {
                                                theme::BRIGHT
                                            } else {
                                                theme::NAME
                                            });
                                        ui.add(egui::Label::new(text).selectable(false));
                                    },
                                )
                                .response
                                .interact(egui::Sense::click());
                            if row.double_clicked() {
                                tab.curve_rename = Some((i, instance.name.clone()));
                            } else if row.clicked() {
                                select_curve = Some(i);
                            }
                            row.on_hover_cursor(egui::CursorIcon::PointingHand);
                        }

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if n > 1 {
                                let (close_rect, close) = ui.allocate_exact_size(
                                    egui::vec2(14.0, 14.0),
                                    egui::Sense::click(),
                                );
                                let hot = close.hovered();
                                if hot {
                                    ui.painter().rect_filled(
                                        close_rect,
                                        2.0,
                                        egui::Color32::from_gray(72),
                                    );
                                }
                                icons::paint(
                                    ui,
                                    icons,
                                    "close",
                                    "×",
                                    close_rect,
                                    if hot {
                                        egui::Color32::from_gray(245)
                                    } else {
                                        theme::DIM
                                    },
                                );
                                if close
                                    .on_hover_text(theme::tip("delete curve instance"))
                                    .clicked()
                                {
                                    remove_curve = Some(i);
                                }
                                ui.add_space(2.0);
                            }
                            ui.add_sized(
                                [52.0, 16.0],
                                egui::DragValue::new(&mut instance.opacity)
                                    .speed(0.01)
                                    .range(0.0..=1.0)
                                    .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                    .custom_parser(|t| {
                                        t.trim()
                                            .trim_end_matches('%')
                                            .parse::<f64>()
                                            .ok()
                                            .map(|v| v / 100.0)
                                    }),
                            )
                            .on_hover_text(theme::tip("curve instance opacity"));
                        });
                    });
                    if selected {
                        theme::rule(ui, theme::RUBY.gamma_multiply(0.75));
                    }
                }

                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let h = ui.spacing().interact_size.y.min(18.0);
                    ui.add_sized(
                        [widgets::Row::LABEL_W, h],
                        egui::Label::new(theme::caption("Point sampler")),
                    );
                    if icons::toggle(
                        ui,
                        icons,
                        "eyedropper",
                        "⌖",
                        picking_curve_point,
                        icons::BIG,
                    )
                    .on_hover_text(theme::tip(
                        "Then click a tone in the picture to add a point to the selected curve instance.",
                    ))
                    .clicked()
                    {
                        toggle_curve_picker = true;
                    }
                });
                ui.add_space(4.0);

                if let Some(instance) = tab.params.curve.instances.get_mut(tab.curve_active) {
                    widgets::curve_editor(
                        ui,
                        &mut instance.curve,
                        &mut tab.curve_drag,
                        &mut tab.curve_point,
                        Some(&curve_ghost),
                    );
                    if let Some((point_i, point)) = tab
                        .curve_point
                        .and_then(|i| instance.curve.points().get(i).copied().map(|p| (i, p)))
                    {
                        let mut input = raw_core::curve::LO_EV + point[0] * ev_span;
                        let mut output = raw_core::curve::LO_EV + point[1] * ev_span;
                        let can_move_input = point_i > 0 && point_i + 1 < instance.curve.points().len();
                        ui.horizontal(|ui| {
                            ui.label(theme::caption("Input:"));
                            let input_changed = ui
                                .add_enabled_ui(can_move_input, |ui| {
                                    ui.add_sized(
                                        [72.0, 18.0],
                                        egui::DragValue::new(&mut input)
                                            .speed(0.01)
                                            .range(
                                                raw_core::curve::LO_EV
                                                    ..=raw_core::curve::HI_EV,
                                            )
                                            .fixed_decimals(2)
                                            .custom_formatter(|v, _| format!("{v:+.2}"))
                                            .custom_parser(|text| {
                                                text.trim()
                                                    .trim_end_matches("EV")
                                                    .trim()
                                                    .parse::<f64>()
                                                    .ok()
                                            })
                                            .suffix(" EV"),
                                    )
                                })
                                .inner
                                .changed();
                            ui.add_space(4.0);
                            ui.label(theme::caption("Output:"));
                            let output_changed = ui
                                .add_sized(
                                    [72.0, 18.0],
                                    egui::DragValue::new(&mut output)
                                        .speed(0.01)
                                        .range(
                                            raw_core::curve::LO_EV..=raw_core::curve::HI_EV,
                                        )
                                        .fixed_decimals(2)
                                        .custom_formatter(|v, _| format!("{v:+.2}"))
                                        .custom_parser(|text| {
                                            text.trim()
                                                .trim_end_matches("EV")
                                                .trim()
                                                .parse::<f64>()
                                                .ok()
                                        })
                                        .suffix(" EV"),
                                )
                                .changed();
                            if input_changed || output_changed {
                                instance.curve.move_point(
                                    point_i,
                                    (input - raw_core::curve::LO_EV) / ev_span,
                                    (output - raw_core::curve::LO_EV) / ev_span,
                                );
                            }
                        });
                        // Read back the constrained point after numeric edits.
                        let point = instance.curve.points()[point_i];
                        let delta = (point[1] - point[0]) * ev_span;
                        let change = if delta.abs() < 0.005 {
                            "unchanged".to_owned()
                        } else {
                            format!(
                                "{:.2} stops {}",
                                delta.abs(),
                                if delta > 0.0 { "brighter" } else { "darker" },
                            )
                        };
                        ui.label(theme::caption(format!("Point: {change}")))
                            .on_hover_text(theme::tip(
                                "Output minus Input at this point, before curve opacity and other processing.",
                            ));
                    } else {
                        let change = match tab.histogram.curve_average_ev() {
                            Some(delta) if delta.abs() < 0.005 => "Average: unchanged".to_owned(),
                            Some(delta) => format!(
                                "Average: {:.2} EV {}",
                                delta.abs(),
                                if delta > 0.0 { "lighter" } else { "darker" },
                            ),
                            None => "Average: —".to_owned(),
                        };
                        ui.label(theme::caption(change)).on_hover_text(theme::tip(
                            "Average EV change across sampled positive tones for this curve layer, including opacity and bypass. Individual tones may change differently. Excludes Contrast Mask and other spatial processing. Select a point for Input / Output EV.",
                        ));
                    }
                }
                ui.label(theme::caption(
                    "click to add · right-click to delete · arrows to move",
                ));
            });
        tab.params.curve.enabled ^= curve.bypass;
        if add_curve_instance && let Some(i) = tab.params.curve.add_instance() {
            tab.curve_active = i;
            tab.curve_point = None;
            tab.curve_drag = None;
            tab.curve_rename = None;
        }
        if curve.reset
            && let Some(instance) = tab.params.curve.instances.get_mut(tab.curve_active)
        {
            let enabled = instance.curve.enabled;
            instance.curve = raw_core::Curve::default();
            instance.curve.enabled = enabled;
            instance.opacity = 1.0;
            tab.curve_point = None;
            tab.curve_drag = None;
        }
        if let Some(i) = select_curve {
            tab.curve_active = i.min(tab.params.curve.instances.len().saturating_sub(1));
            tab.curve_point = None;
            tab.curve_drag = None;
            tab.curve_rename = None;
        }
        if commit_curve_rename
            && let Some((i, name)) = tab.curve_rename.take()
            && let Some(instance) = tab.params.curve.instances.get_mut(i)
            && !name.trim().is_empty()
        {
            instance.name = name.trim().to_owned();
        }
        if let Some(i) = remove_curve.filter(|i| *i < tab.params.curve.instances.len()) {
            tab.params.curve.instances.remove(i);
            tab.curve_active = tab
                .curve_active
                .min(tab.params.curve.instances.len().saturating_sub(1));
            tab.curve_point = None;
            tab.curve_drag = None;
            tab.curve_rename = None;
        }
        if toggle_curve_picker {
            tab.mode = if picking_curve_point {
                tabs::Mode::View
            } else {
                tabs::Mode::CurvePoint
            };
        }
        if save_curve_preset
            && let Some(instance) = tab.params.curve.instances.get(tab.curve_active)
        {
            *curve_preset_name = Some(CurvePresetNameDialog {
                value: String::new(),
                instance: instance.clone(),
                request_focus: true,
            });
        }
        if let Some(index) = load_curve_preset
            && let Some(preset) = curve_presets.presets.get(index)
        {
            let preset_name = preset.name.clone();
            match preset.append_to(&mut tab.params.curve) {
                Ok(active) => {
                    tab.curve_active = active;
                    tab.curve_point = None;
                    tab.curve_drag = None;
                    tab.curve_rename = None;
                    if tab.mode.is_curve_point() {
                        tab.mode = tabs::Mode::View;
                    }
                    *pending_note = Some(format!("added Curve preset “{preset_name}”"));
                }
                Err(curve_presets::AppendError::Full) => {
                    *pending_note = Some(format!(
                        "{} Curve instances is the limit",
                        raw_core::CurveStack::MAX_INSTANCES
                    ));
                }
                Err(curve_presets::AppendError::Invalid) => {
                    *pending_note = Some("that Curve preset is invalid and was not loaded".into());
                }
            }
        }
        if let Some(index) =
            delete_curve_preset.filter(|index| *index < curve_presets.presets.len())
        {
            let removed = curve_presets.presets.remove(index);
            let name = removed.name.clone();
            match curve_presets.save() {
                Ok(()) => *pending_note = Some(format!("deleted Curve preset “{name}”")),
                Err(error) => {
                    curve_presets.presets.insert(index, removed);
                    *pending_note = Some(format!("could not delete Curve preset — {error}"));
                }
            }
        }

        let display = widgets::Module::new("TONAL TRANSFORM")
            .open_on_start(true)
            .modified(tab.params.display.is_modified())
            .switch(tab.params.display.enabled)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("tonemap")
                        .selected_text(tab.params.display.tone_map.label())
                        .show_ui(ui, |ui| {
                            for t in ToneMap::UI_ORDER {
                                // Discriminant, not equality: the shoulder carries
                                // parameters, so comparing by value would deselect the
                                // entry as soon as a slider moved.
                                let selected = std::mem::discriminant(
                                    &tab.params.display.tone_map,
                                ) == std::mem::discriminant(&t);
                                if ui.selectable_label(selected, t.label()).clicked() && !selected {
                                    tab.params.display.tone_map = t;
                                }
                            }
                        })
                        .response
                        .on_hover_text(theme::tip(
                            "Clip discards everything above scene 1.0 — up to 1.8 stops of \
                             real data. Soft shoulder keeps it, touching only the highlights. \
                             AgX rolls off highlight headroom with an S-curve, deepening \
                             shadows and adding midtone contrast while preserving middle gray.",
                        ));

                    if let Some(agx) = tab.params.display.tone_map.agx_mut()
                        && theme::bracket(
                            ui,
                            "Auto Range",
                            agx.auto_range,
                            theme::size::CAPTION,
                        )
                        .on_hover_text(theme::tip(
                            "Keep the AgX black and white window fitted to this crop after Exposure and Curve.",
                        ))
                        .clicked()
                    {
                        agx.auto_range = !agx.auto_range;
                    }
                });
                // The controls that make the shoulder mean anything. A parameterised
                // mode without them is a mode that does nothing — see
                // `Weighting::Weighted`.
                // **`Start` and `Strength`, with the word `Shoulder` said once.** the maintainer's,
                // and it is the combo directly above that makes it work: these two only
                // exist when it reads `Soft shoulder`, so the noun is already on screen
                // and the sliders can be the adjectives. Two passes of abbreviating
                // `Shoulder strength` to fit the label column were solving the wrong
                // problem — the repetition was the problem.
                if let Some((threshold, strength)) = tab.params.display.tone_map.shoulder_mut() {
                    widgets::Slider::new(threshold, 0.75, 0.2..=1.0, "Start")
                        .decimals(2)
                        .show(ui);
                    widgets::Slider::new(strength, 0.8, 0.0..=1.0, "Strength")
                        .decimals(2)
                        .show(ui);
                    ui.label(theme::caption(
                        "everything below the start is passed through untouched",
                    ));
                }
                if let Some(agx) = tab.params.display.tone_map.agx_mut() {
                    let mut changed = false;
                    ui.add_enabled_ui(!agx.auto_range, |ui| {
                        changed |= widgets::Slider::new(
                            &mut agx.white_ev,
                            raw_core::AgxParams::DEFAULT.white_ev,
                            1.0..=12.0,
                            "White EV",
                        )
                        .decimals(1)
                        .suffix(" EV")
                        .show(ui);
                        changed |= widgets::Slider::new(
                            &mut agx.black_ev,
                            raw_core::AgxParams::DEFAULT.black_ev,
                            -16.0..=-1.0,
                            "Black EV",
                        )
                        .decimals(1)
                        .suffix(" EV")
                        .show(ui);
                    });
                    changed |= widgets::Slider::new(
                        &mut agx.contrast,
                        raw_core::AgxParams::DEFAULT.contrast,
                        0.5..=12.0,
                        "Contrast",
                    )
                    .decimals(2)
                    .show(ui);

                    changed |= widgets::Slider::new(
                        &mut agx.toe_power,
                        raw_core::AgxParams::DEFAULT.toe_power,
                        0.5..=8.0,
                        "Toe",
                    )
                    .decimals(2)
                    .show(ui);
                    changed |= widgets::Slider::new(
                        &mut agx.shoulder_power,
                        raw_core::AgxParams::DEFAULT.shoulder_power,
                        0.5..=8.0,
                        "Shoulder",
                    )
                    .decimals(2)
                    .show(ui);
                    if changed {
                        *agx = agx.normalized();
                    }
                }
                widgets::Slider::new(
                    &mut tab.params.display.gamma,
                    2.2,
                    1.0..=3.0,
                    "Monitor gamma",
                )
                .decimals(2)
                .show(ui);
                // **TPDF dither is not here any more; it is in EXPORT.** the maintainer's call,
                // and the reason it reads better there is that the thing it protects is
                // an 8-bit *file*. It was in DISPLAY because the flag is
                // `display.dither` and the viewport was its only consumer, which is an
                // argument about where the field lives rather than about where the
                // control belongs.
            });
        tab.params.display.enabled ^= display.bypass;
        if display.reset {
            // Reset the adjustment values without changing the comparison switch,
            // matching the other bypassable Develop modules.
            let enabled = tab.params.display.enabled;
            tab.params.display = raw_core::DisplayParams {
                enabled,
                ..raw_core::DisplayParams::default()
            };
        }
        let auto_agx_active = matches!(
            tab.params.display.tone_map,
            ToneMap::Agx(agx) if agx.auto_range
        );
        if auto_agx_active
            && let (Some(luma), Some(frame)) = (&tab.luma, tab.frame())
            && let Some((black_ev, white_ev)) = histogram::agx_auto_range(luma, &frame, &tab.params)
            && let Some(agx) = tab.params.display.tone_map.agx_mut()
        {
            agx.black_ev = black_ev;
            agx.white_ev = white_ev;
            *agx = agx.normalized();
        }

        // ── GRAIN ─────────────────────────────────────────────────────────────
        //
        // **In DEVELOP and not a pane of its own.** Dodge & Burn earned a pane
        // because it is a *tool* — a brush, a layer stack and a mask editor, none of
        // which you look at unless you are painting. Grain is a stage: six scalars
        // and a seed, read top to bottom with everything else.
        //
        // Placed after DISPLAY rather than after CURVE, which is where "after the
        // curve" first put it. The panel reads as the order things happen in, and
        // what happens is `curve → tone map → grain → encode` — the tone map being
        // DISPLAY's. Above DISPLAY the module would be claiming to run on
        // scene-referred values, which is the one thing it does not do.
        // COMPOSITION stays last, as the handoff has it.
        //
        // Nothing here reaches the viewport: see `Params::diff`. What it reaches is
        // the loupe, which is the only place grain is visible at all.
        let grain_modified = tab.params.grain.is_modified();
        // Read before the panel draws, so an edit made inside it can be seen and arm
        // the switch — see `widgets::arm`.
        //
        // **The whole params, not `is_default()`.** The rule is "did this frame change
        // anything", and comparing against the default can only answer "is it different
        // from the factory", which is a question that stops changing after the first
        // edit. `enabled` is excluded because the dot is not an edit: it is applied
        // separately below and must not arm itself.
        let grain_before = raw_core::GrainParams {
            enabled: false,
            ..tab.params.grain
        };
        let mut reseed = false;
        // **Set by anything that changes what the file will look like below the tone
        // map**, in either of the two modules down here, and consumed once after both.
        // Neither closure can call `Tab::show_loupe` itself — `tab` is borrowed for the
        // body — which is the same reason `reseed` is a flag and not a call.
        let mut wake_loupe = false;
        let grain = widgets::Module::new("GRAIN")
            .open_on_start(false)
            .modified(grain_modified)
            .switch(tab.params.grain.enabled)
            .show(ui, |ui| {
                let g = &mut tab.params.grain;
                let d = raw_core::GrainParams::default();

                // The size slider is the one control here that is not a plain float.
                // It steps in whole pixels and lands on odd ones, because the kernel
                // is a square centred on its middle pixel — so it is read out through
                // the setter rather than written to directly, and the number on the
                // slider is always the number the emulsion runs.
                let mut size = g.size as f32;
                if widgets::Slider::new(&mut size, d.size as f32, 1.0..=20.0, "Crystal size")
                    .decimals(0)
                    .suffix(" px")
                    .show(ui)
                {
                    g.set_size(size.round().max(0.0) as u32);
                    wake_loupe = true;
                }
                wake_loupe |= widgets::Slider::new(
                    &mut g.density,
                    d.density,
                    raw_core::grain::DENSITY_RANGE,
                    "Density",
                )
                .decimals(2)
                .show(ui);

                let mut layers = g.layers as f32;
                if widgets::Slider::new(&mut layers, d.layers as f32, 5.0..=60.0, "Layers")
                    .decimals(0)
                    .show(ui)
                {
                    g.layers = layers.round().clamp(5.0, 60.0) as u32;
                    wake_loupe = true;
                }
                wake_loupe |= widgets::Slider::new(
                    &mut g.variability,
                    d.variability,
                    raw_core::grain::VARIABILITY_RANGE,
                    "Variability",
                )
                .decimals(2)
                .show(ui);
                wake_loupe |= widgets::Slider::new(
                    &mut g.sensitivity,
                    d.sensitivity,
                    raw_core::grain::SENSITIVITY_RANGE,
                    "Sensitivity",
                )
                .decimals(2)
                .suffix(" EV")
                .show(ui);

                // **The seed is not a slider**, and this is what it is instead. It is
                // per-image state with no meaningful ordering — 41 is not "less grain"
                // than 42 — so the only sensible controls over it are "give me a
                // different one" and "tell me which one this is". It rides in the
                // sidecar so that two exports of one negative are the same file.
                ui.horizontal(|ui| {
                    ui.label(theme::caption("Seed"));
                    // **Typed, not just rolled.** the maintainer asked to be able to set it
                    // himself, which is the right instinct: a seed is the identity of
                    // this negative's emulsion, and identities are things you write
                    // down and reuse. A `DragValue` because that is the app's control
                    // for a typed number — the Output module's resolution and print
                    // size are the same widget.
                    let mut seed = g.seed;
                    ui.add(
                        egui::DragValue::new(&mut seed)
                            .speed(1.0)
                            .range(0..=raw_core::grain::SEED_MAX),
                    )
                    .on_hover_text(theme::tip(
                        "The emulsion's identity. Stored with the image, so re-exporting \
                         gives the same grain — and the same number gives another frame \
                         the same crystals.",
                    ));
                    wake_loupe |= g.seed != seed;
                    g.seed = seed;
                    if icons::sized(ui, icons, "reseed", "↻", true, icons::BOX)
                        .on_hover_text(theme::tip("Roll a new seed"))
                        .clicked()
                    {
                        reseed = true;
                        wake_loupe = true;
                    }
                });
            });
        // **Measured before the bypass is applied**, so the dot cannot read as an edit
        // and arm the very module it was just used to switch off.
        let grain_edited = raw_core::GrainParams {
            enabled: false,
            ..tab.params.grain
        } != grain_before;
        tab.params.grain.enabled ^= grain.bypass;
        // **A slider arms the module**, so the first thing you do to Grain is not
        // nothing. After the bypass rather than before it: applying the click first and
        // then arming means a frame in which you both nudge a slider and click the dot
        // ends up on, which is the reading that matches the gesture that did more.
        widgets::arm(grain_edited, &mut tab.params.grain.enabled);
        // Switching a tail module ON is the moment feedback matters most, so the
        // dot counts as a touch. Switching one off is caught by the `is_active`
        // guard below rather than by a second condition here.
        wake_loupe |= grain.bypass;
        if grain.reset {
            // The seed survives a reset, deliberately. It is not a setting that can be
            // wrong — it is which emulsion this negative got — and a reset that rolled
            // it would silently change a print somebody had already approved while
            // appearing to put things back.
            tab.params.grain = raw_core::GrainParams {
                enabled: tab.params.grain.enabled,
                seed: tab.params.grain.seed,
                ..Default::default()
            };
        }
        if reseed {
            tab.params.grain.seed = fresh_seed();
        }

        // ── SHARPENING ────────────────────────────────────────────────────────
        //
        // **Below GRAIN, because that is the order it runs in**: the resize, then the
        // emulsion, then sharpening over the whole of it. The panel reads as the order
        // things happen in, and this module is genuinely last — output sharpening
        // compensates the medium, and the medium does not distinguish grain from
        // detail. `export::write` has the argument.
        //
        // Which means **the two modules above and below this line are not separable**:
        // Amount makes the grain louder and harder as well as the detail. That is
        // faithful to a print rather than a fault, and the PRINT LOUPE below is where
        // both are dialled, together.
        //
        // The resize itself is not in this panel — it is Output's, in the Info pane —
        // which is the one place this module's position is not visible from where its
        // controls are. `Radius` is the reason it matters: the unit is **output**
        // pixels, so the same number means a different physical size on a different
        // print size. That is the module working rather than a unit nobody converted,
        // and it is what the caption has to say.
        //
        // Three controls, where the prototype has six plus a mode pill. What was cut
        // and why is in `docs/decisions.md`; the short of it is that its own presets
        // never moved the radius and its band equaliser reaches scales that are local
        // contrast rather than output sharpening.
        // The same snapshot Grain takes, and for the same reason. See `widgets::arm`.
        let sharpen_before = raw_core::sharpen::SharpenParams {
            enabled: false,
            ..tab.params.sharpen
        };
        let sharpen = widgets::Module::new("SHARPENING")
            .open_on_start(false)
            .modified(tab.params.sharpen.is_modified())
            .switch(tab.params.sharpen.enabled)
            .show(ui, |ui| {
                let s = &mut tab.params.sharpen;
                let d = raw_core::sharpen::SharpenParams::default();
                wake_loupe |= widgets::Slider::new(
                    &mut s.amount,
                    d.amount,
                    raw_core::sharpen::AMOUNT_RANGE,
                    "Amount",
                )
                .decimals(2)
                .show(ui);
                wake_loupe |= widgets::Slider::new(
                    &mut s.radius,
                    d.radius,
                    raw_core::sharpen::RADIUS_RANGE,
                    "Radius",
                )
                .decimals(2)
                .suffix(" px")
                .show(ui);
                wake_loupe |= widgets::Slider::new(
                    &mut s.edges,
                    d.edges,
                    raw_core::sharpen::EDGES_RANGE,
                    "Edges",
                )
                .decimals(2)
                .show(ui);
                ui.label(theme::caption(
                    "Radius is in pixels of the FILE, not of the picture — so a smaller \
                     print sharpens finer detail at the same number.",
                ));
            });
        let sharpen_edited = raw_core::sharpen::SharpenParams {
            enabled: false,
            ..tab.params.sharpen
        } != sharpen_before;
        tab.params.sharpen.enabled ^= sharpen.bypass;
        widgets::arm(sharpen_edited, &mut tab.params.sharpen.enabled);
        // Switching a tail module ON is the moment feedback matters most, so the
        // dot counts as a touch. Switching one off is caught by the `is_active`
        // guard below rather than by a second condition here.
        wake_loupe |= sharpen.bypass;
        if sharpen.reset {
            tab.params.sharpen = raw_core::sharpen::SharpenParams {
                enabled: tab.params.sharpen.enabled,
                ..Default::default()
            };
        }

        // **The loupe opens itself the moment either tail module is touched.**
        //
        // Both are invisible to the viewport, so without this the first move of a
        // slider changes nothing you can see — which reads as a broken control rather
        // than as an export-only module, and is the single thing most likely to make
        // somebody give up on both. the maintainer asked for it and it is the right default:
        // the feedback arrives with the edit rather than after remembering to ask.
        //
        // Guarded on something being *active*, so switching the last module off does
        // not put a window up to show you nothing. Once it is up, further edits leave
        // it exactly where it is — `show_loupe` re-centres, and re-centring under a
        // drag would yank the sample away from what you were looking at.
        if wake_loupe
            && tab.has_image()
            && !tab.loupe.open
            && (tab.params.grain.is_active() || tab.params.sharpen.is_active())
        {
            tab.show_loupe();
        }
        let reveal_loupe_module = tab.loupe.take_module_reveal();

        // ── PRINT LOUPE ───────────────────────────────────────────────────────
        //
        // **Its own block, below both modules it serves**, where it used to be a
        // section inside GRAIN. Two things now run past the point the viewport can see
        // — sharpening and grain — and this one window shows both, because the tile it
        // renders is the export's whole tail. Leaving the control inside GRAIN would
        // have made the sharpening module's only feedback live under somebody else's
        // heading; duplicating it into both would be two buttons for one window.
        //
        // `Plain` rather than `Module`: there is no dot, because a loupe is not a
        // parameter. It is a tool that is open or shut, like the crop tool.
        widgets::Plain::new("PRINT LOUPE")
            .open_on_start(false)
            .open_when(reveal_loupe_module)
            .show(ui, |ui| {
                ui.label(theme::caption(
                    "sharpening and grain are applied on export only — neither is a \
                 viewport pass. The loupe is where you judge them.",
                ));
                ui.horizontal(|ui| {
                    // **An eye, not a checkbox** — the maintainer's call, and the same button the
                    // crop tool is: a tool that is *open* rather than a setting that is
                    // *true*. The loupe already behaves that way, since `Mode::Loupe`
                    // claims the drag while it is up, and a tick box was describing it
                    // as a preference.
                    if icons::toggle(ui, icons, "eye", "◉", tab.loupe.open, icons::BIG)
                        .on_hover_text(theme::tip(
                            "A 3:2 window on exact file pixels, with sharpening and grain. \
                         Drag on the picture to move what it samples.",
                        ))
                        .clicked()
                    {
                        toggle_loupe = true;
                    }
                    ui.label(theme::label("Loupe"));
                    // Enabled only while the loupe is up, because it describes what
                    // the loupe is showing and means nothing without one — the same
                    // rule the prototype's pair follows.
                    ui.add_enabled_ui(tab.loupe.open, |ui| {
                        ui.checkbox(&mut tab.loupe.before, theme::caption("Before"))
                            .on_hover_text(theme::tip(
                                "The same crop with sharpening and grain off. Both are \
                             rendered together, so switching is instant.",
                            ));
                    });
                });
                ui.add_enabled_ui(tab.loupe.open, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::caption("Magnification"));
                        let current = tab.loupe.magnification();
                        let mut selected = None;
                        for magnification in crate::loupe::Loupe::MAGNIFICATIONS {
                            let label = format!("{}%", magnification as u16 * 100);
                            if theme::bracket(
                                ui,
                                &label,
                                current == magnification,
                                theme::size::CAPTION,
                            )
                            .on_hover_text(theme::tip(
                                "Magnify exact file pixels with nearest-neighbor display",
                            ))
                            .clicked()
                            {
                                selected = Some(magnification);
                            }
                        }
                        if let Some(magnification) = selected {
                            tab.loupe.set_magnification(magnification);
                        }
                    });
                });
            });

        // **Last, per the handoff**, because it is the last thing that happens to
        // the picture: crop applies at the end of the chain, after every tone
        // decision above it has been made on the uncropped frame.
        let exif = tab.exif_orientation();
        // The crop as SET, not as drawn: with the tool open `frame` is deliberately
        // uncropped so the whole picture shows, and a readout that inherited that
        // reported the bounding box for a crop a third its size.
        let frame = tab.stored_frame();
        let in_crop = tab.mode.is_crop();
        // Snapshotted so the two controls that reshape the crop as a *consequence*
        // can tell they were touched. Neither can be done inside the closure: both
        // need the frame the change produces, which does not exist until it has.
        let was = (
            tab.params.composition.straighten,
            tab.params.composition.ratio,
        );
        let entered_composition = tab.params.composition;
        let was_keystone = tab.params.composition.keystone;
        // Set by the `↕` toggle, which changes the target shape without changing
        // which entry is selected — so the comparison below cannot see it.
        let mut reflow_ratio = false;
        let composition = widgets::Module::new("COMPOSITION")
            .open_on_start(false)
            .modified(tab.params.composition.is_modified())
            .switch(tab.params.composition.enabled)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // **Named, like every other control in the module.** Two curved
                    // arrows with nothing beside them were the one row here you had to
                    // hover to identify — the rest of the module says Ratio, Guides,
                    // Straighten. the maintainer asked for the word.
                    ui.label(theme::label("Rotate"));
                    if icons::sized(ui, icons, "rotate-left", "⟲", true, icons::BIG)
                        .on_hover_text(theme::tip("Rotate left  ⌘["))
                        .clicked()
                    {
                        rotate = Some(false);
                    }
                    if icons::sized(ui, icons, "rotate-right", "⟳", true, icons::BIG)
                        .on_hover_text(theme::tip("Rotate right  ⌘]"))
                        .clicked()
                    {
                        rotate = Some(true);
                    }
                    ui.add_space(4.0);
                    // Says both what the picture is doing and where that came from.
                    // "as shot" is not the same claim as "0°", and a panel that
                    // showed only the angle would give no way to tell a file the
                    // camera called upright from one the user turned there.
                    let o = tab.params.composition.orientation;
                    ui.label(theme::caption(match o {
                        None => format!("as shot · {}", exif.label()),
                        Some(o) => format!("{} · overrides {}", o.label(), exif.label()),
                    }));
                });
                ui.add_space(4.0);

                // No FRAME heading. The prototype has one because its COMPOSITION
                // section also carries LENS and OUTPUT and needs telling apart;
                // here the module *is* the frame, and a heading immediately under
                // the module's own title would say the same word twice.
                //
                // ── The tools ────────────────────────────────────────────────────
                //
                // **Above Ratio**, which is the maintainer's order and is the better one: Ratio
                // and Guides are settings *for* the crop, so opening the tool under the
                // things that configure it read as though they came first.
                //
                // **The straighten tool is on this row too**, which is a judgement and
                // has an argument behind it in the code rather than in taste: arming it
                // sets `open_crop`, because a line you draw on the picture can only be
                // drawn while the crop tool has the drag. It *is* a mode of the crop
                // tool, and the two buttons being one row says so. It also has to be
                // somewhere — the Straighten row below is a `widgets::Row`, and those
                // spend the module's width exactly, so a button on the end of it would
                // have to come out of the track every slider in the app shares.
                ui.horizontal(|ui| {
                    if icons::labelled_toggle(ui, icons, "crop", "#", "Crop", in_crop, icons::BIG)
                        .on_hover_text(theme::tip(
                            "Open the crop tool. The whole frame stays visible while it \
                             is open, so you can grab back what is outside the \
                             crop.  c · Return to apply · Esc to cancel",
                        ))
                        .clicked()
                    {
                        toggle_crop = true;
                    }
                    // **The prototype's Straighten tool**: the control that makes the
                    // angle usable, because nobody knows what 2.4° looks like and
                    // everybody can trace a horizon. Arms a one-shot mode — the next
                    // press on the picture draws a line instead of moving the crop, and
                    // it disarms on release.
                    let armed = tab.straighten_armed;
                    // `wide_toggle`, because this button sits immediately after a
                    // *labelled* one and a square glyph beside `⌗ Crop` reads pinched.
                    // See `icons::WIDE`.
                    if icons::wide_toggle(ui, icons, "angle", "/", armed, icons::BIG)
                        .on_hover_text(theme::tip("Draw a line to straighten"))
                        .clicked()
                    {
                        tab.straighten_armed = !armed;
                        // Arming it is only useful with the tool open, so opening it
                        // is part of the gesture rather than a thing to remember.
                        if tab.straighten_armed {
                            open_crop = true;
                        }
                    }
                    // The small reset face every other one in the panel wears, and
                    // to the right of the buttons it undoes rather than before them.
                    if theme::reset_button(ui, "reset crop", "Revert to whole frame").clicked() {
                        reset_crop = true;
                    }
                    if in_crop {
                        ui.label(theme::caption("cropping").color(theme::RUBY));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(theme::label("Ratio"));
                    egui::ComboBox::from_id_salt("crop-ratio")
                        .width(148.0)
                        .selected_text(tab.params.composition.ratio.label())
                        .show_ui(ui, |ui| {
                            // **The prototype's list, transcribed** — names, values
                            // and order. `11×14` and `Ōban` are paper the maintainer prints
                            // on, not round numbers somebody thought sensible, so a
                            // tidier list would be missing the ones that get used.
                            //
                            // The prototype carries its three groups as source
                            // comments over a flat combo; here they are drawn as
                            // unselectable headings, which is the same structure
                            // made visible. Twenty-one entries is a long way to
                            // scan otherwise.
                            for (name, r) in Ratio::PRESETS {
                                let Some(r) = *r else {
                                    ui.add_space(4.0);
                                    theme::section(ui, name);
                                    continue;
                                };
                                let on = tab.params.composition.ratio.same(r);
                                if ui.selectable_label(on, *name).clicked() {
                                    tab.params.composition.ratio = r;
                                }
                            }
                        });
                    // The prototype's `↕`: one combo of landscape ratios beside a
                    // toggle, rather than every entry listed twice. Disabled where
                    // there is nothing to stand on end — `Freehand` has no shape,
                    // and `Original` is already the picture's own way up.
                    let can_flip = tab.params.composition.ratio.can_flip();
                    let portrait = can_flip && tab.params.composition.portrait;
                    // **The device-rotate glyph, not `↕`** — see the icon's own note.
                    // It is an `icons::toggle` rather than a `Button::selected` for the
                    // same reason the crop and straighten buttons beside it are: an
                    // open tool and a chosen orientation are both "on", and the app has
                    // one way of drawing that.
                    if icons::toggle_enabled(
                        ui,
                        icons,
                        "device-rotate",
                        "↕",
                        portrait,
                        can_flip,
                        icons::BIG,
                    )
                    .on_hover_text(theme::tip("Portrait / landscape"))
                    .clicked()
                    {
                        tab.params.composition.portrait = !portrait;
                        reflow_ratio = true;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(theme::label("Guides"));
                    egui::ComboBox::from_id_salt("crop-guide")
                        .width(148.0)
                        .selected_text(tab.guide.label())
                        .show_ui(ui, |ui| {
                            for g in crop::Guide::ORDER {
                                if ui.selectable_label(tab.guide == g, g.label()).clicked() {
                                    tab.guide = g;
                                }
                            }
                        })
                        .response
                        .on_hover_text(theme::tip(
                            "Drawn inside the crop while the tool is open. Scaffolding, \
                             not a mark — none of it reaches the picture.",
                        ));
                });
                // **A row, not a section.** `STRAIGHTEN` was an all-caps heading over a
                // control with no label, which spent two lines and a type size on one
                // slider — and put the only unlabelled slider in the app under the only
                // heading with a single control beneath it. the maintainer asked for the word
                // dropped to body and set on the control's own line, which makes it an
                // ordinary `Row` and lines its number up with every other number in the
                // panel. Ten characters fits the label column with room to spare.
                widgets::Slider::new(
                    &mut tab.params.composition.straighten,
                    0.0,
                    CompositionParams::STRAIGHTEN_RANGE,
                    "Straighten",
                )
                .decimals(2)
                .suffix("°")
                .show(ui);

                ui.add_space(8.0);
                theme::section(ui, "PERSPECTIVE");
                ui.horizontal(|ui| {
                    let line_guides = matches!(
                        tab.params.composition.keystone.mode,
                        KeystoneMode::Vertical | KeystoneMode::Horizontal
                    );
                    if ui.selectable_label(line_guides, "Guides").clicked() {
                        // Vertical is only the request token here. The direction is
                        // chosen from the first line the photographer actually draws.
                        keystone_tool = Some(KeystoneMode::Vertical);
                    }
                    if ui
                        .selectable_label(
                            tab.params.composition.keystone.mode == KeystoneMode::Rectangle,
                            "Rectangle",
                        )
                        .clicked()
                    {
                        keystone_tool = Some(KeystoneMode::Rectangle);
                    }
                    if theme::reset_button(ui, "reset perspective", "Reset Perspective").clicked() {
                        reset_keystone = true;
                    }
                });
                let mut correction = tab.params.composition.keystone.correction * 100.0;
                if widgets::Slider::new(&mut correction, 80.0, 0.0..=100.0, "Correction")
                    .decimals(0)
                    .suffix("%")
                    .show(ui)
                {
                    tab.params.composition.keystone.correction = correction / 100.0;
                }
                widgets::Slider::new(
                    &mut tab.params.composition.keystone.aspect,
                    0.0,
                    KeystoneParams::ASPECT_RANGE,
                    "Aspect",
                )
                .decimals(1)
                .suffix("%")
                .show(ui);
                ui.horizontal(|ui| {
                    ui.label(theme::label("Auto crop"));
                    egui::ComboBox::from_id_salt("keystone-auto-crop")
                        .width(92.0)
                        .selected_text(tab.params.composition.keystone.crop.label())
                        .show_ui(ui, |ui| {
                            for crop in KeystoneCrop::ORDER {
                                if ui
                                    .selectable_label(
                                        tab.params.composition.keystone.crop == crop,
                                        crop.label(),
                                    )
                                    .clicked()
                                {
                                    tab.params.composition.keystone.crop = crop;
                                }
                            }
                        });
                });
                ui.label(theme::caption(
                    "Draw two parallel guides, or drag the rectangle corners.",
                ));

                if let Some(f) = &frame {
                    let d = f.output_dims();
                    // The preset's own name when one is selected, because `A4` and
                    // `Ōban` are irrational and have no honest `a:b` — and because a
                    // name is more use than a number you would have to recognise.
                    // Otherwise the measured shape; see `Ratio::aspect_label`.
                    let shape = match tab.params.composition.ratio {
                        Ratio::Free => Ratio::aspect_label(d.w as u32, d.h as u32),
                        r => r.label(),
                    };
                    ui.label(theme::caption(format!("{} x {} px  ·  {shape}", d.w, d.h)));
                }
            });
        tab.params.composition.enabled ^= composition.bypass;

        if reset_keystone {
            tab.params.composition.keystone = KeystoneParams::default();
            if tab.mode.is_keystone() {
                tab.mode = tabs::Mode::View;
            }
        } else if let Some(mode) = keystone_tool {
            let wants_lines = mode == KeystoneMode::Vertical;
            let has_lines = matches!(
                tab.params.composition.keystone.mode,
                KeystoneMode::Vertical | KeystoneMode::Horizontal
            );
            let same_tool = (wants_lines && has_lines)
                || (!wants_lines
                    && tab.params.composition.keystone.mode == KeystoneMode::Rectangle);
            if tab.mode.is_keystone() && same_tool {
                tab.mode = tabs::Mode::View;
            } else {
                let entered = tab.mode.cancelled().unwrap_or(entered_composition);
                if !same_tool {
                    if wants_lines {
                        // No direction is chosen until the first drawn line. A zero
                        // correction keeps this temporary seed a literal no-op.
                        tab.params.composition.keystone = tab
                            .params
                            .composition
                            .keystone
                            .reset_for(KeystoneMode::Vertical);
                        tab.params.composition.keystone.correction = 0.0;
                    } else {
                        tab.params.composition.keystone = tab
                            .params
                            .composition
                            .keystone
                            .reset_for(KeystoneMode::Rectangle);
                    }
                }
                tab.mode = tabs::Mode::keystone(entered);
            }
        }

        let keystone_changed = tab.params.composition.keystone != was_keystone;
        if keystone_changed && tab.params.composition.keystone.is_active() {
            let frame = raw_core::Frame::resolve(
                tab.luma
                    .as_ref()
                    .map_or(raw_core::Dims { w: 1, h: 1 }, |l| l.output_dims),
                tab.exif_orientation(),
                &tab.params.composition,
            );
            let crop_kind = tab.params.composition.keystone.crop;
            tab.params.composition.crop = frame.keystone_crop(crop_kind);
            tab.params.composition.ratio = match crop_kind {
                KeystoneCrop::Largest => raw_core::Ratio::Free,
                KeystoneCrop::Original => raw_core::Ratio::Original,
            };
        }

        // Picking a preset reshapes the crop there and then, rather than waiting for
        // the next drag to notice. A ratio that changed the *label* and nothing else
        // would be indistinguishable from one that does nothing — the same trap
        // `Weighting::Weighted` fell into.
        let ratio_changed = reflow_ratio || !tab.params.composition.ratio.same(was.1);
        if ratio_changed
            && let Some(f) = tab.stored_frame()
            && let Some(target) = f.target_ratio(
                tab.params.composition.ratio,
                tab.params.composition.portrait,
            )
        {
            tab.params.composition.crop = crop::fit_ratio(tab.params.composition.crop, target, &f);
        }

        // **Straighten auto-crops.** The corners a rotation empties are pulled out of
        // the frame automatically — see `Tab::confine_crop`, which is the one
        // implementation and is called from here, from the rotate ring and from the
        // drawn horizon. This site is only the slider.
        if tab.params.composition.straighten != was.0 {
            tab.confine_crop();
        }

        if composition.reset {
            // Orientation included: a reset returns the frame to as-shot, which is
            // the module's default. The bypass stays where the user put it, like
            // every other module's reset.
            tab.params.composition = CompositionParams {
                enabled: tab.params.composition.enabled,
                ..Default::default()
            };
        }

        // ── OUTPUT ────────────────────────────────────────────────────────────
        //
        // **Pixels, print size and PPI are three views of one decision**, and the
        // Resample switch is what says which of the three is derived. With it off the
        // pixel count is fixed and size and PPI are a reciprocal pair over it — type
        // a size and the resolution follows, type a resolution and the size follows,
        // and no pixel moves either way. With it on, size and PPI are both inputs and
        // the pixel count is the answer, which export resamples to.
        //
        // The prototype offers only the second of those, behind an Auto toggle. The
        // first is the one a photographer reaches for far more often — "what will this
        // print at?" — and it is the one that cannot damage anything, so it is the
        // default and the switch is off.
        //
        // Nothing in this module reaches the render: see `Params::diff`.
        //
        // **`Tail::of`, not `params.toning`**, because that is what the export asks —
        // a bypassed Toning module writes a greyscale file, and the readout below has
        // to say so.
        let tail = export::Tail::of(&tab.params);
        let toned = tail.toning.is_active();
        let output_needs_colour = toned || tail.frame.needs_colour();
        // The per-container space defaults, read out before the panel body takes the
        // borrows it needs — `settings_ppi` above is bound for the same reason. An array
        // rather than three names, because the only question asked of it is "what does
        // this container open at".
        let space_defaults: [(export::Container, export::Space); 3] = [
            (export::Container::Tiff, settings_space_tiff),
            (export::Container::Png, settings_space_png),
            (export::Container::Jpeg, settings_space_jpeg),
        ];
        let settings_space_for = |c: export::Container| {
            space_defaults
                .iter()
                .find(|(k, _)| *k == c)
                .map(|(_, v)| *v)
                .unwrap_or_default()
        };
        let picture = tab.output_dims();
        let out_modified = !tab.params.output.is_default();
        let output = widgets::Module::new("OUTPUT")
            .open_on_start(false)
            .modified(out_modified)
            .show(ui, |ui| {
                let o = &mut tab.params.output;
                ui.horizontal(|ui| {
                    ui.label(theme::label("Resolution"));
                    ui.add(
                        egui::DragValue::new(&mut o.ppi)
                            .speed(1.0)
                            .range(OutputParams::PPI_RANGE)
                            .fixed_decimals(0)
                            .suffix(" ppi"),
                    )
                    .on_hover_text(theme::tip(
                        "Written into the file's resolution tag. With Resample off it \
                     only changes the print size; with it on it changes how many \
                     pixels are written.",
                    ));
                });

                let Some(pic) = picture else {
                    ui.label(theme::caption("no image — size follows the picture"));
                    return;
                };

                // The size row. Both fields are always live, and editing one anchors it:
                // the other is a function of the crop's aspect and is never typed, which
                // is why a print from this panel cannot come out distorted.
                ui.horizontal(|ui| {
                    ui.label(theme::label("Size"));
                    ui.add_space(4.0);
                    // **`theme::bracket`, not a `selected` Button.** These two were the
                    // last pair in the app still showing their state as egui does it —
                    // a filled blue-grey slab — where SHAPE, the tonal-range zones,
                    // Edge-aware and Vignette all show it as a ruby outline. One
                    // toggle, one appearance; the maintainer reported the mismatch.
                    for u in Unit::UI_ORDER {
                        if theme::bracket(ui, u.label(), unit == u, theme::size::CAPTION).clicked()
                        {
                            new_unit = Some(u);
                        }
                    }
                });
                let (w_in, h_in) = o.print_inches(pic);
                let (mut w_disp, mut h_disp) = (unit.from_inches(w_in), unit.from_inches(h_in));
                // A tenth of a millimetre is below any printer's placement accuracy and
                // two decimals is what the prototype shows; the step is a quarter inch
                // because print sizes are quarters far more often than they are tenths.
                fn field(v: &mut f32, unit: Unit) -> egui::DragValue<'_> {
                    egui::DragValue::new(v)
                        .speed(0.05 * unit.per_inch())
                        .range(0.01..=400.0 * unit.per_inch())
                        .fixed_decimals(2)
                        .suffix(format!(" {}", unit.label()))
                }
                let mut edited: Option<(raw_core::Axis, f32)> = None;
                ui.horizontal(|ui| {
                    if ui.add(field(&mut w_disp, unit)).changed() {
                        edited = Some((raw_core::Axis::Width, unit.to_inches(w_disp)));
                    }
                    ui.label(theme::caption("×"));
                    if ui.add(field(&mut h_disp, unit)).changed() {
                        edited = Some((raw_core::Axis::Height, unit.to_inches(h_disp)));
                    }
                });
                // Which of the three numbers this moves depends on the mode, and that
                // decision lives in `OutputParams` so it can be tested without a UI —
                // see `set_print_size`.
                if let Some((axis, inches)) = edited {
                    o.set_print_size(pic, axis, inches);
                }

                ui.horizontal(|ui| {
                    let mut on = o.resize.is_some();
                    if ui
                        .checkbox(&mut on, theme::caption("Resample"))
                        .on_hover_text(theme::tip(
                            "Off, the file gets the picture's own pixels and the size \
                         above is a report. On, the file is resized to it and the \
                         pixel count follows.",
                        ))
                        .changed()
                    {
                        // Seeded from what the picture already is, so switching it on
                        // changes nothing until a number is moved.
                        o.set_resizing(pic, on);
                    }
                    if on {
                        egui::ComboBox::from_id_salt("resample-filter")
                            .width(96.0)
                            .selected_text(o.filter.label())
                            .show_ui(ui, |ui| {
                                for f in raw_core::Filter::UI_ORDER {
                                    if ui.selectable_label(o.filter == f, f.label()).clicked() {
                                        o.filter = f;
                                    }
                                }
                            })
                            .response
                            .on_hover_text(theme::tip(
                                "Lanczos is sharper and rings a little at hard edges; \
                             Mitchell is softer and does not. Lanczos unless a big \
                             upsample shows a halo.",
                            ));
                    }
                });

                // With resizing off, a small enough print implies a resolution past
                // anything a printer addresses, and the size field snaps to where the
                // range stops. That is the honest answer, but it is a mystery without a
                // sentence — and the way out of it is the switch immediately above.
                if o.resize.is_none()
                    && (o.ppi >= *OutputParams::PPI_RANGE.end()
                        || o.ppi <= *OutputParams::PPI_RANGE.start())
                {
                    ui.label(theme::caption(format!(
                        "at the {:.0} ppi limit — turn Resample on to go past it",
                        o.ppi
                    )));
                }

                // What will actually be written, and by how much it differs from what
                // the picture is. The one line that says whether the request is silly.
                let d = o.target_dims(pic);
                let note = format!("{} x {} px  ·  {}", d.w, d.h, o.scale_note(pic));
                if o.fits(pic) {
                    ui.label(theme::caption(note));
                } else {
                    ui.label(theme::caption(note).color(theme::RUBY));
                    ui.label(
                        theme::caption(format!(
                            "too large to write — the limits are {} px per edge and {} MP",
                            OutputParams::MAX_EDGE,
                            OutputParams::MAX_PIXELS / 1_000_000
                        ))
                        .color(theme::RUBY),
                    );
                }

                // **The colour space, and it is a control now.** It was a disabled
                // readout saying that choosing it "belongs to the output colour space
                // work, which has not happened" — that work is this. the maintainer asked for
                // eciRGB v2 and sRGB are selectable for people who do not want
                // monostar. `Space::encode` applies sRGB's own curve rather than L\*,
                // and `channels` knows which choices carry three channels. ProStar is
                // still readable as a legacy value but is intentionally absent here.
                //
                // It reads the **written** space, not the chosen one, which is the bug
                // the old readout was fixed for and would be easy to reintroduce: an
                // active toner promotes a greyscale choice to eciRGB v2 in `Spec::new`,
                // because three channels stop being optional once a pixel holds three
                // numbers. The line underneath says so when the two differ, rather than
                // leaving the picker looking overruled for no stated reason.
                ui.add_space(4.0);
                let written = export_target.written_space(output_needs_colour);
                ui.horizontal(|ui| {
                    ui.label(theme::label("Color space"));
                    egui::ComboBox::from_id_salt("output-space")
                        .width(190.0)
                        .selected_text(export_target.space.label())
                        .show_ui(ui, |ui| {
                            for k in export::Space::UI_ORDER {
                                if ui
                                    .selectable_label(export_target.space == k, k.label())
                                    .clicked()
                                {
                                    export_target.space = k;
                                }
                            }
                        });
                });
                if written != export_target.space {
                    let reason = if toned {
                        "toning carries chroma"
                    } else {
                        "the FRAME color carries chroma"
                    };
                    ui.label(theme::caption(format!(
                        "written as {} instead — {reason}",
                        written.label()
                    )));
                } else if written.is_rgb() && !toned {
                    ui.label(theme::caption(
                        "untoned, so this is the grayscale master with its channels \
                         repeated — same picture, three times the file",
                    ));
                } else if written == export::Space::Srgb {
                    ui.label(theme::caption(
                        "display-referred — a proof's space rather than a master's",
                    ));
                }
            });
        if output.reset {
            // The resolution goes back to the *configured* default rather than to
            // 300, so a user who works at 360 does not have to retype it.
            tab.params.output = OutputParams {
                ppi: settings_ppi,
                ..OutputParams::default()
            };
        }

        // ── FRAME ─────────────────────────────────────────────────────────────
        //
        // A physical canvas around the finished photograph. It is intentionally
        // below OUTPUT: the photograph's print size is one of its inputs. It is also
        // intentionally above EXPORT: `export::write` composes it only after grain,
        // toning and sharpening, so those image-making stages can never touch a frame
        // pixel or create a halo against its edge.
        let mut frame_before = tab.params.frame;
        frame_before.enabled = false;
        let frame_module = widgets::Module::new("FRAME")
            .open_on_start(false)
            .modified(tab.params.frame.is_modified())
            .switch(tab.params.frame.enabled)
            .show(ui, |ui| {
                use raw_core::frame::{Margins, Placement, Priority};

                let image_unit = unit;
                let output_params = tab.params.output;
                let f = &mut tab.params.frame;
                let unit = f.unit;
                let image_inches = picture.map(|pic| {
                    let (w, h) = output_params.print_inches(pic);
                    [w, h]
                });

                ui.horizontal(|ui| {
                    ui.label(theme::label("Drive by"));
                    for priority in Priority::UI_ORDER {
                        if theme::bracket(
                            ui,
                            priority.label(),
                            f.priority == priority,
                            theme::size::CAPTION,
                        )
                        .clicked()
                        {
                            f.priority = priority;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(theme::label("Units"));
                    for candidate in Unit::UI_ORDER {
                        if theme::bracket(
                            ui,
                            candidate.label(),
                            f.unit == candidate,
                            theme::size::CAPTION,
                        )
                        .clicked()
                        {
                            f.unit = candidate;
                        }
                    }
                });

                fn length<'a>(v: &'a mut f32, unit: Unit) -> egui::DragValue<'a> {
                    egui::DragValue::new(v)
                        .speed(0.02 * unit.per_inch())
                        .range(0.0..=raw_core::FrameParams::MAX_INCHES * unit.per_inch())
                        .fixed_decimals(2)
                        .suffix(format!(" {}", unit.label()))
                }

                match f.priority {
                    Priority::Sides => {
                        ui.horizontal(|ui| {
                            ui.label(theme::label("Sides"));
                            for (label, equal) in [("EQUAL", true), ("CUSTOM", false)] {
                                if theme::bracket(ui, label, f.equal == equal, theme::size::CAPTION)
                                    .clicked()
                                {
                                    f.equal = equal;
                                }
                            }
                        });
                        if f.equal {
                            let mut shown = unit.from_inches(f.margins.left);
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Margin"));
                                if ui.add(length(&mut shown, unit)).changed() {
                                    f.margins = Margins::all(unit.to_inches(shown));
                                }
                            });
                        } else {
                            let mut values = [
                                unit.from_inches(f.margins.left),
                                unit.from_inches(f.margins.right),
                                unit.from_inches(f.margins.top),
                                unit.from_inches(f.margins.bottom),
                            ];
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Left / Right"));
                                ui.add(length(&mut values[0], unit));
                                ui.add(length(&mut values[1], unit));
                            });
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Top / Bottom"));
                                ui.add(length(&mut values[2], unit));
                                ui.add(length(&mut values[3], unit));
                            });
                            f.margins = Margins {
                                left: unit.to_inches(values[0]),
                                right: unit.to_inches(values[1]),
                                top: unit.to_inches(values[2]),
                                bottom: unit.to_inches(values[3]),
                            };
                        }
                    }
                    Priority::Outer => {
                        let shown_size = |size: [f32; 2]| {
                            format!(
                                "{:.2} × {:.2} {}",
                                unit.from_inches(size[0]),
                                unit.from_inches(size[1]),
                                unit.label()
                            )
                        };
                        ui.horizontal(|ui| {
                            ui.label(theme::label("Frame size"));
                            egui::ComboBox::from_id_salt("frame-outer-preset")
                                .width(154.0)
                                .selected_text(if f.custom_size {
                                    "Custom".to_owned()
                                } else {
                                    shown_size(f.outer_inches)
                                })
                                .show_ui(ui, |ui| {
                                    for preset in &raw_core::FrameParams::PRESETS {
                                        let landscape =
                                            image_inches.is_some_and(|size| size[0] >= size[1]);
                                        let oriented = if landscape {
                                            [preset[1], preset[0]]
                                        } else {
                                            *preset
                                        };
                                        if ui
                                            .selectable_label(
                                                !f.custom_size && f.outer_inches == oriented,
                                                shown_size(*preset),
                                            )
                                            .clicked()
                                        {
                                            f.outer_inches = oriented;
                                            f.custom_size = false;
                                        }
                                    }
                                    if ui.selectable_label(f.custom_size, "Custom").clicked() {
                                        f.custom_size = true;
                                    }
                                });
                            if ui
                                .button("↔")
                                .on_hover_text(theme::tip("Swap frame orientation"))
                                .clicked()
                            {
                                f.outer_inches.swap(0, 1);
                            }
                        });
                        if f.custom_size {
                            let mut w = unit.from_inches(f.outer_inches[0]);
                            let mut h = unit.from_inches(f.outer_inches[1]);
                            ui.horizontal(|ui| {
                                if ui.add(length(&mut w, unit)).changed() {
                                    f.outer_inches[0] = unit.to_inches(w);
                                }
                                ui.label(theme::caption("×"));
                                if ui.add(length(&mut h, unit)).changed() {
                                    f.outer_inches[1] = unit.to_inches(h);
                                }
                            });
                        }

                        ui.horizontal(|ui| {
                            ui.label(theme::label("Placement"));
                            egui::ComboBox::from_id_salt("frame-placement")
                                .width(154.0)
                                .selected_text(f.placement.label())
                                .show_ui(ui, |ui| {
                                    for placement in Placement::UI_ORDER {
                                        if ui
                                            .selectable_label(
                                                f.placement == placement,
                                                placement.label(),
                                            )
                                            .clicked()
                                        {
                                            f.placement = placement;
                                        }
                                    }
                                });
                        });
                        if f.placement == Placement::BottomWeighted {
                            if let Some(image) = image_inches {
                                let room = (f.outer_inches[1] - image[1]).max(0.0);
                                f.bottom_weight_inches = f.bottom_weight_inches.min(room);
                            }
                            let mut weight = unit.from_inches(f.bottom_weight_inches);
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Weight"));
                                if ui.add(length(&mut weight, unit)).changed() {
                                    f.bottom_weight_inches = unit.to_inches(weight);
                                }
                                let has_room = image_inches.is_some_and(|image| {
                                    f.outer_inches[1] > image[1] + f32::EPSILON
                                });
                                if ui
                                    .add_enabled(has_room, egui::Button::new("Optical"))
                                    .on_hover_text(theme::tip(
                                        "Measure the developed photograph's visual center and set the bottom weight to center it in the frame",
                                    ))
                                    .clicked()
                                {
                                    optical_frame_requested = true;
                                }
                                if let Some(image) = image_inches {
                                    let target = (0.5
                                        + f.bottom_weight_inches / (2.0 * image[1].max(1.0e-6)))
                                        .clamp(0.5, 1.0);
                                    ui.label(theme::caption(format!("{:.1}%", target * 100.0)))
                                        .on_hover_text(theme::tip(
                                            "The visual-center position represented by this bottom weight",
                                        ));
                                }
                            });
                        }

                        // Custom placement exposes real physical margins. Opposing
                        // fields remain linked because the outer size is authoritative:
                        // adding to Left necessarily removes the same room from Right.
                        if f.placement == Placement::Custom
                            && let Some(image) = image_inches
                            && let Ok(layout) = f.layout(image)
                        {
                            let sx = (layout.outer_inches[0] - image[0]).max(0.0);
                            let sy = (layout.outer_inches[1] - image[1]).max(0.0);
                            let mut values = [
                                unit.from_inches(layout.margins.left),
                                unit.from_inches(layout.margins.right),
                                unit.from_inches(layout.margins.top),
                                unit.from_inches(layout.margins.bottom),
                            ];
                            let old = values;
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Left / Right"));
                                ui.add(length(&mut values[0], unit));
                                ui.add(length(&mut values[1], unit));
                            });
                            ui.horizontal(|ui| {
                                ui.label(theme::label("Top / Bottom"));
                                ui.add(length(&mut values[2], unit));
                                ui.add(length(&mut values[3], unit));
                            });
                            if values[0] != old[0] && sx > 0.0 {
                                f.custom_position[0] =
                                    (unit.to_inches(values[0]) / sx).clamp(0.0, 1.0);
                            } else if values[1] != old[1] && sx > 0.0 {
                                f.custom_position[0] =
                                    (1.0 - unit.to_inches(values[1]) / sx).clamp(0.0, 1.0);
                            }
                            if values[2] != old[2] && sy > 0.0 {
                                f.custom_position[1] =
                                    (unit.to_inches(values[2]) / sy).clamp(0.0, 1.0);
                            } else if values[3] != old[3] && sy > 0.0 {
                                f.custom_position[1] =
                                    (1.0 - unit.to_inches(values[3]) / sy).clamp(0.0, 1.0);
                            }
                        }
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(theme::label("Color"));
                    for (name, color) in [
                        ("White", [0xff, 0xff, 0xff]),
                        ("Black", [0x00, 0x00, 0x00]),
                    ] {
                        if theme::bracket(ui, name, f.color == color, theme::size::CAPTION)
                            .clicked()
                        {
                            f.color = color;
                        }
                    }
                    let rising_name = settings::RISING
                        .iter()
                        .find(|(_, color)| f.color == *color)
                        .map(|(name, _)| *name)
                        .unwrap_or("Rising...");
                    egui::ComboBox::from_id_salt("frame-rising-color")
                        .width(96.0)
                        .truncate()
                        .selected_text(rising_name)
                        .show_ui(ui, |ui| {
                            for (name, color) in settings::RISING {
                                if ui.selectable_label(f.color == color, name).clicked() {
                                    f.color = color;
                                }
                            }
                        })
                        .response
                        .on_hover_text(theme::tip("Measured Rising Museum Board colors"));
                    colour_picker_lab_button(ui, &mut f.color, "frame-color-picker")
                        .on_hover_text(theme::tip("Custom frame color"));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut f.trim_line, theme::label("Trim Line"))
                        .on_hover_text(theme::tip(
                            "Adds a 1 px black perimeter outside the physical frame size",
                        ));
                });

                let Some(image) = image_inches else {
                    ui.label(theme::caption("no image — frame follows OUTPUT"));
                    return;
                };
                match f.layout(image) {
                    Ok(layout) => {
                        // A compact geometry preview. The main viewer below this panel
                        // uses the photograph itself; this diagram makes the physical
                        // relationship and any bottom weighting unambiguous at a glance.
                        let width = ui.available_width().max(1.0);
                        let height = 142.0;
                        let sense =
                            if f.priority == Priority::Outer && f.placement == Placement::Custom {
                                egui::Sense::click_and_drag()
                            } else {
                                egui::Sense::hover()
                            };
                        let (box_rect, preview_response) =
                            ui.allocate_exact_size(egui::vec2(width, height), sense);
                        let pad = 8.0;
                        let avail = box_rect.shrink(pad);
                        let scale = (avail.width() / layout.outer_inches[0])
                            .min(avail.height() / layout.outer_inches[1]);
                        let outer = egui::Rect::from_center_size(
                            avail.center(),
                            egui::vec2(
                                layout.outer_inches[0] * scale,
                                layout.outer_inches[1] * scale,
                            ),
                        );
                        ui.painter().rect_filled(
                            outer,
                            0.0,
                            egui::Color32::from_rgb(f.color[0], f.color[1], f.color[2]),
                        );
                        let image_rect = egui::Rect::from_min_size(
                            egui::pos2(
                                outer.left() + layout.margins.left * scale,
                                outer.top() + layout.margins.top * scale,
                            ),
                            egui::vec2(image[0] * scale, image[1] * scale),
                        );
                        if preview_response.dragged()
                            && f.priority == Priority::Outer
                            && f.placement == Placement::Custom
                        {
                            let delta = ui.ctx().input(|i| i.pointer.delta());
                            let slack = egui::vec2(
                                (outer.width() - image_rect.width()).max(0.0),
                                (outer.height() - image_rect.height()).max(0.0),
                            );
                            if slack.x > 0.0 {
                                f.custom_position[0] =
                                    (f.custom_position[0] + delta.x / slack.x).clamp(0.0, 1.0);
                            }
                            if slack.y > 0.0 {
                                f.custom_position[1] =
                                    (f.custom_position[1] + delta.y / slack.y).clamp(0.0, 1.0);
                            }
                        }
                        if f.priority == Priority::Outer && f.placement == Placement::Custom {
                            preview_response.on_hover_cursor(egui::CursorIcon::Grab);
                        }
                        ui.painter().rect_filled(image_rect, 0.0, theme::CHROME);
                        ui.painter().rect_stroke(
                            outer,
                            0.0,
                            egui::Stroke::new(1.0, theme::DIM),
                            egui::StrokeKind::Inside,
                        );
                        if f.trim_line {
                            ui.painter().rect_stroke(
                                outer,
                                0.0,
                                egui::Stroke::new(1.0, egui::Color32::BLACK),
                                egui::StrokeKind::Outside,
                            );
                        }
                        let [ow, oh] = layout.outer_inches.map(|v| unit.from_inches(v));
                        let [iw, ih] = image.map(|v| image_unit.from_inches(v));
                        let px = picture
                            .and_then(|pic| {
                                f.pixel_layout(output_params.target_dims(pic), image).ok()
                            })
                            .map(|p| format!("  ·  {} × {} px", p.outer.w, p.outer.h))
                            .unwrap_or_default();
                        ui.label(theme::caption(format!(
                            "Image  {:.2} × {:.2} {}",
                            iw,
                            ih,
                            image_unit.label()
                        )));
                        ui.label(theme::caption(format!(
                            "Frame  {:.2} × {:.2} {}{px}",
                            ow,
                            oh,
                            unit.label()
                        )));
                    }
                    Err(why) => {
                        ui.label(
                            theme::caption(format!("does not fit — {why}")).color(theme::RUBY),
                        );
                        ui.label(theme::caption(
                            "Increase FRAME size or reduce the image size in OUTPUT.",
                        ));
                    }
                }
            });
        if optical_frame_requested
            && let Some(visual) = frame_visual_center(tab)
            && let Some(pic) = picture
        {
            let image_inches = {
                let (w, h) = tab.params.output.print_inches(pic);
                [w, h]
            };
            match tab
                .params
                .frame
                .bottom_weight_for_visual_center(image_inches, visual[1])
            {
                Ok(weight) => {
                    tab.params.frame.bottom_weight_inches = weight;
                    let shown = tab.params.frame.unit.from_inches(weight);
                    tab.status = format!(
                        "visual center {:.1}% · bottom weight {:.2} {}",
                        visual[1] * 100.0,
                        shown,
                        tab.params.frame.unit.label()
                    );
                }
                Err(why) => tab.status = format!("optical placement unavailable — {why}"),
            }
        }
        let mut frame_after = tab.params.frame;
        frame_after.enabled = false;
        // Units are presentation only; changing inches to centimetres must not arm an
        // otherwise untouched frame.
        frame_after.unit = frame_before.unit;
        let frame_edited = frame_after != frame_before;
        tab.params.frame.enabled ^= frame_module.bypass;
        widgets::arm(frame_edited, &mut tab.params.frame.enabled);
        if frame_module.reset {
            tab.params.frame = raw_core::FrameParams {
                enabled: tab.params.frame.enabled,
                ..Default::default()
            };
        }

        widgets::Plain::new("EXPORT")
            .open_on_start(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("container")
                        .width(150.0)
                        .selected_text(export_target.container.label())
                        .show_ui(ui, |ui| {
                            for c in export::Container::UI_ORDER {
                                if ui
                                    .selectable_label(export_target.container == c, c.label())
                                    .clicked()
                                {
                                    // **Changing the container reseeds the space from that
                                    // container's setting.** This is what makes the three
                                    // settings keys mean anything: a TIFF default of
                                    // eciRGB v2 and a PNG default of monostar are two
                                    // answers to "what is this file for", and switching the
                                    // container is switching the errand.
                                    //
                                    // It does overwrite a space chosen by hand for the
                                    // previous container, which is the right way round —
                                    // the alternative is a picker that silently keeps a
                                    // space the new container was never meant to carry.
                                    export_target.container = c;
                                    export_target.space = settings_space_for(c);
                                    // JPEG cannot hold 16 bits, so switching to it from a
                                    // 16-bit TIFF has to leave a legal target rather than
                                    // one the writer reinterprets. `Container::depths` is
                                    // where the rule lives; `settle` is it being applied.
                                    export_target.settle();
                                }
                            }
                        });
                    egui::ComboBox::from_id_salt("depth")
                        .width(80.0)
                        .selected_text(export_target.depth.label())
                        .show_ui(ui, |ui| {
                            for d in export::Depth::UI_ORDER {
                                // Greyed rather than hidden, so the list does not change
                                // length under the pointer and 16-bit's absence on a JPEG
                                // reads as a fact about JPEG rather than as a missing row.
                                let ok = export_target.container.supports(d);
                                let hit = ui
                                    .add_enabled_ui(ok, |ui| {
                                        ui.selectable_label(export_target.depth == d, d.label())
                                    })
                                    .inner;
                                if hit.clicked() {
                                    export_target.depth = d;
                                }
                            }
                        });
                });
                if export_target.container == export::Container::Jpeg {
                    ui.label(theme::caption(export::Container::Jpeg.proof_note()));
                }

                // PNG has no uncompressed mode, so the control would be a lie there.
                if export_target.container == export::Container::Tiff {
                    egui::ComboBox::from_id_salt("compression")
                        .width(150.0)
                        .selected_text(export_target.compression.label())
                        .show_ui(ui, |ui| {
                            for c in export::Compression::UI_ORDER {
                                if ui
                                    .selectable_label(export_target.compression == c, c.label())
                                    .clicked()
                                {
                                    export_target.compression = c;
                                }
                            }
                        })
                        .response
                        .on_hover_text(theme::tip(
                            "Both are lossless — deflate is the same coding as ZIP. \
                         Uncompressed is larger, and what fussy print RIPs and older \
                         software expect.",
                        ));
                }
                // **TPDF dither, moved here from DISPLAY.** the maintainer asked for it in this
                // module, and it belongs: what it protects is an 8-bit file, and 8-bit is a
                // choice made two rows above. One flag with one meaning — break up the
                // quantisation — governing the screen, which is an 8-bit surface, and every
                // 8-bit file this app writes.
                //
                // Disabled at 16 bits rather than hidden, because "does this master have
                // dither in it" is a question worth being able to answer by looking. The
                // rule is structural on that side — `samples16` has no dither to switch off
                // — so the control is reporting a fact rather than being greyed by policy.
                // **The "why" is on hover, not on the page.** the maintainer's, and the rule it
                // settles is one this panel needs: a caption earns its line by saying
                // something you have to know *before* you touch the control. This one
                // explains a control that is already visibly greyed — the state is on
                // screen, only the reason is missing, and a reason is what a tooltip is
                // for. Three lines of standing text under a checkbox that is off is the
                // panel talking when nobody asked.
                let dithers = export_target.depth == export::Depth::Eight;
                ui.add_enabled_ui(dithers, |ui| {
                    ui.checkbox(&mut tab.params.display.dither, theme::label("TPDF dither"))
                        .on_hover_text(theme::tip(if dithers {
                            "About one level of noise before quantizing, so a smooth gradient \
                         breaks into texture instead of steps. Every 8-bit file, and the \
                         screen."
                        } else {
                            "8-bit only. A 16-bit step is already below the visual threshold, \
                         so this would add noise and nothing else. Still governs the \
                         screen and every proof."
                        }));
                });

                // Only a scratch duplicate has anything to save: every other tab already
                // writes its sidecar on each settled gesture.
                if tab.scratch {
                    if ui
                        .add_enabled(tab.has_image(), egui::Button::new("Save duplicate…"))
                        .on_hover_text(theme::tip(
                            "Copy the raw under the tab's name, with its own sidecar. \
                         Copy-on-write where the filesystem allows, so it costs no disk \
                         space until one of the two changes.  ⌘S",
                        ))
                        .clicked()
                    {
                        save_duplicate = true;
                    }
                    ui.label(theme::caption("scratch — edits here are not saved yet"));
                    ui.add_space(8.0);
                }

                // **The panel's primary action, in the panel's primary form.** the maintainer's
                // ask, and it is the same argument `theme::wide_button` already makes for
                // `+ DODGE` and for SNAPSHOT: a fill is how the thing a module exists to
                // reach says so. Export is the end of the pipeline the whole column
                // describes, and it had been sitting there as an ordinary outlined button
                // — indistinguishable from "Save duplicate…" above it, which is a
                // housekeeping action.
                //
                // `RUBY_FILL`, not `RUBY_FILL_DIM`: the dim one is for the second of a
                // pair, and this button has no pair. Export Proof, which does, keeps it.
                let w = ui.available_width();
                if theme::wide_button(ui, "Export…", theme::RUBY_FILL, theme::RUBY, w, can_export)
                    .on_hover_text(theme::tip(
                        "Full resolution grayscale, L*-encoded and tagged monostar.icc. \
                     8-bit is dithered; 16-bit is not.  ⌘E",
                    ))
                    .clicked()
                {
                    export_requested = true;
                }
                // Under the button rather than beside it. A full-width button has no
                // "beside", and the spinner only ever appears while the button is
                // disabled — `can_export` is false for the whole of an export — so the
                // two never compete for the same row anyway.
                if exporting {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::caption("exporting…"));
                    });
                }
                // **No summary line under the button.** It said what export would write —
                // pixels, print size, ppi, space — and every one of those numbers is
                // already on the page: pixels and print size in OUTPUT directly above,
                // ppi in OUTPUT's own field, the space in OUTPUT's picker, and the whole
                // set again in the footer. the maintainer called it redundant and it was; a caption
                // that repeats the four controls it sits under is not a summary, it is the
                // panel reading itself back.
            });

        // Same rule as the settings menu: written when something moves, and a failure
        // to write a display preference is reported rather than thrown away.
        if let Some(u) = new_unit {
            self.settings.set_print_unit(u);
            if let Err(e) = self.settings.save() {
                self.status = format!("could not write settings: {e}");
            }
        }
        if export_requested {
            self.export_requested = self.tabs.active_id().map(|id| (id, ExportKind::Master));
        }
        if let Some(clockwise) = rotate
            && let Some(t) = self.tabs.active_mut()
        {
            t.rotate(clockwise);
        }
        if toggle_crop && let Some(t) = self.tabs.active_mut() {
            t.mode = if t.mode.is_crop() {
                tabs::Mode::View
            } else {
                tabs::Mode::crop(t.params.composition)
            };
        }
        // Only when it is not already open — re-entering would re-snapshot, and the
        // composition Esc puts back would become the one the tool was already in.
        if open_crop
            && let Some(t) = self.tabs.active_mut()
            && !t.mode.is_crop()
        {
            t.mode = tabs::Mode::crop(t.params.composition);
        }
        if reset_crop && let Some(t) = self.tabs.active_mut() {
            // The rectangle only. Straighten and orientation are their own controls
            // with their own resets, and a button labelled "Reset crop" that also
            // levelled the picture would be doing two things under one name.
            //
            // "Whole frame" on a *straightened* picture means the whole of what is
            // there, not the bounding box around it — the empty corners are not part
            // of the frame, they are the absence of one. `confine_crop` is what says
            // so, and at 0° it has nothing to do and `FULL` survives untouched.
            t.params.composition.crop = raw_core::Rect::FULL;
            t.confine_crop();
        }
        // The loupe is a mode as well as a window, so that a drag on the picture can
        // move the sample without also panning — one line in `Mode::claims_drag`, and
        // the same shape crop and paint already have. Closing it returns to View only
        // if the loupe is what is open: turning the checkbox off while the crop tool
        // happens to be up must not close the crop tool.
        // The open half goes through `Tab::show_loupe`, which the `v` key and the
        // auto-open share. The close half is here because it is the only caller: it has
        // a texture to drop, which opening does not.
        if toggle_loupe && let Some(t) = self.tabs.active_mut() {
            if t.loupe.open {
                t.loupe.open = false;
                if t.mode.is_loupe() {
                    t.mode = tabs::Mode::View;
                }
                t.loupe.forget();
            } else {
                t.show_loupe();
            }
        }
        if save_duplicate && let Some(id) = self.tabs.active_id() {
            self.save_duplicate(id);
        }
    }

    /// The compare grid, drawn **in the image pane's place**.
    ///
    /// the maintainer's decision, and it is the direct translation of the prototype's swapped
    /// central widget. The alternative — a sixth `Pane` — is the idiomatic tile-tree
    /// answer and is wrong here for one concrete reason: it would let you dock compare
    /// *beside* the image, which is a layout nobody wants and which would mean two live
    /// viewports. Replacing the picture also means compare follows the picture, so it
    /// still works when the image pane has been dragged to a second monitor.
    ///
    /// Returns whether it drew, so the caller can fall through to the ordinary
    /// viewport when there is nothing to compare.
    ///
    /// # One source, N targets
    ///
    /// Every cell is [`raw_gpu::Viewport::render_into`] on the tab's **own** viewport,
    /// so the luminance texture is uploaded once and the cells cost a few hundred
    /// kilobytes each. That is only sound because compare shows versions of one decode
    /// — the maintainer's scope decision — and it is what the prototype does too: "compare only
    /// ever shows snapshots of the single currently-loaded RAW".
    ///
    /// # Each cell composes itself
    ///
    /// A snapshot carries `composition`, so two cells can be different shapes, and each
    /// is fitted into its own tile rather than all four sharing one geometry. The
    /// prototype does the same and says why in a comment: the warp is applied per cell
    /// "because each snapshot's params may carry different composition state".
    fn compare_grid(&mut self, ui: &mut egui::Ui, rs: &egui_wgpu::RenderState) -> bool {
        let background = self.settings.background();
        let surround = self
            .settings
            .surround(self.tabs.active().is_some_and(|tab| tab.surround));
        let Some(gpu) = &mut self.gpu else {
            return false;
        };
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        if !tab.compare.open {
            return false;
        }
        // Two is the least that compares anything. One pinned snapshot beside nothing
        // is the live view with a border, so the grid declines and the viewport draws.
        let pinned = tab.snapshots.pinned();
        let looks: Vec<raw_core::Params> = pinned.iter().map(|s| s.params.clone()).collect();
        let names: Vec<String> = pinned.iter().map(|s| s.label.clone()).collect();
        let (n, slots) = tabs::Compare::counts(tab.compare.n_up, looks.len());
        if n < 2 {
            return false;
        }
        let Some(luma) = tab.luma.clone() else {
            return false;
        };
        // Read before the render borrow: `exif_orientation` reads the tab, and
        // `render` is a mutable borrow of one of its fields.
        let exif = tab.exif_orientation();
        let Some(render) = &mut tab.render else {
            return false;
        };

        // Layout from the requested count, not the populated count. Two pinned looks
        // in 3-up remain two narrow strips with one empty slot; in 4-up they remain
        // the top half of the square. Collapsing these back to 2-up made the `3` and
        // `4` keys appear broken whenever only two snapshots were pinned.
        let (rows, cols) = tabs::Compare::grid(slots);
        let ppp = ui.ctx().pixels_per_point();
        let area = ui.max_rect();
        const GAP: f32 = 6.0;
        let cw = (area.width() - GAP * (cols - 1) as f32) / cols as f32;
        let ch = (area.height() - GAP * (rows - 1) as f32) / rows as f32;
        let slot_rect = |i: usize| {
            let (r, c) = (i / cols, i % cols);
            egui::Rect::from_min_size(
                egui::pos2(
                    area.left() + c as f32 * (cw + GAP),
                    area.top() + r as f32 * (ch + GAP),
                ),
                egui::vec2(cw, ch),
            )
        };

        // Give unpopulated slots a quiet, visible footprint so switching among 2-,
        // 3- and 4-up reads as an actual layout change rather than pictures merely
        // becoming smaller for no apparent reason.
        for i in n..slots {
            let empty = slot_rect(i);
            ui.painter().rect_filled(empty, 0.0, theme::CHROME_DEEP);
            ui.painter().rect_stroke(
                empty,
                0.0,
                egui::Stroke::new(1.0, theme::CHROME),
                egui::StrokeKind::Inside,
            );
        }

        render.cells.resize_with(n, Default::default);
        // **The pan is registered before the cells, and that ordering is the whole
        // reason both gestures work.** Measured: where two `interact` rects overlap,
        // the *later* one wins — so with this after the cells, a drag on a cell went to
        // the pan and drag-to-reorder never fired at all. Registering it first leaves
        // the picture area to the pan and lets each cell's caption, added afterwards,
        // take the drag back over its own strip.
        let grid = ui.interact(area, ui.id().with("compare-view"), egui::Sense::drag());
        let mut restore: Option<usize> = None;
        let mut swap: Option<(usize, usize)> = None;
        let mut dropped: Option<(usize, egui::Pos2)> = None;
        let mut cell_rects: Vec<egui::Rect> = Vec::with_capacity(n);
        let (zoom, pan) = (tab.compare.zoom, tab.compare.pan);

        for (i, look) in looks.iter().take(n).enumerate() {
            let rect = slot_rect(i);
            let out_w = ((cw * ppp).round() as u32).max(1);
            let out_h = ((ch * ppp).round() as u32).max(1);

            let frame =
                raw_core::Frame::resolve(luma.output_dims, exif, &look.effective().composition);
            // The crop's extent and where it sits on the grid the view pans over —
            // the same two numbers the single view uses, and not `output_dims`, which
            // is on the far side of the frame's transform.
            let (src_w, src_h) = (frame.crop.w, frame.crop.h);
            // Fitted per cell, with the same margin the single view uses at fit, so a
            // cell reads as the picture in a window rather than as a bleed.
            const FIT_MARGIN: f32 = 12.0;
            let m = FIT_MARGIN * ppp;
            let box_w = (out_w as f32 - 2.0 * m).max(1.0);
            let box_h = (out_h as f32 - 2.0 * m).max(1.0);
            // **The shared view, applied to each cell's own picture.** `zoom` is a
            // multiple of this cell's fit and `pan` a fraction of this cell's extent,
            // so two cells cropped differently show the same *region* of what each of
            // them is — which is the only reading of "synced" that means anything once
            // a snapshot can carry a crop.
            let fit = (box_w / src_w as f32).min(box_h / src_h as f32);
            let scale = fit * zoom;
            let centre_x = frame.crop.x as f32 + src_w as f32 * (0.5 + pan.0);
            let centre_y = frame.crop.y as f32 + src_h as f32 * (0.5 + pan.1);
            let view = ViewGeometry {
                scale,
                off_x: centre_x - out_w as f32 / scale * 0.5,
                off_y: centre_y - out_h as f32 / scale * 0.5,
                background,
                overlays: raw_gpu::Overlays::NONE,
                surround,
            };

            let cell = &mut render.cells[i];
            // **Rendered only when its key moves.** A snapshot's params never change,
            // so with a still view every cell is already on the GPU and the grid costs
            // nothing per frame — which is what makes four cells affordable at all.
            let key = tabs::CellKey {
                params: look.clone(),
                out: (out_w, out_h),
                zoom,
                pan,
            };
            if cell.key.as_ref() != Some(&key) {
                let submitted = render.viewport.render_into(
                    &mut cell.gpu,
                    gpu,
                    &rs.device,
                    &rs.queue,
                    out_w,
                    out_h,
                    view,
                    look,
                    &frame,
                );
                if submitted || cell.gpu.render_error().is_some() {
                    cell.key = Some(key);
                } else {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(16));
                }
            }
            if (cell.gpu.changed || cell.texture.is_none())
                && let Some(tv) = cell.gpu.view().cloned()
            {
                let mut renderer = rs.renderer.write();
                if let Some(old) = cell.texture.take() {
                    renderer.free_texture(&old);
                }
                cell.texture = Some(renderer.register_native_texture(
                    &rs.device,
                    &tv,
                    wgpu::FilterMode::Nearest,
                ));
            }
            if let Some(message) = cell.gpu.render_error() {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    message,
                    egui::FontId::proportional(14.0),
                    egui::Color32::from_rgb(220, 120, 120),
                );
            } else if let Some(id) = cell.texture {
                let uv = cell.gpu.uv_rect();
                ui.painter().image(
                    id,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(uv[0], uv[1])),
                    egui::Color32::WHITE,
                );
            }
            // **The caption, and it is also the handle.** The grid had no labels at
            // all — four pictures of one negative with nothing to say which was `v002`
            // — and the prototype's cells carry a label bar for exactly this. Making
            // that bar the reorder handle is the prototype's arrangement too, and it is
            // what lets the picture keep the plain drag for panning.
            let bar = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), CAPTION_H));
            ui.painter()
                .rect_filled(bar, 0.0, theme::CHROME.gamma_multiply(0.85));
            ui.painter().text(
                egui::pos2(bar.left() + 6.0, bar.center().y),
                egui::Align2::LEFT_CENTER,
                &names[i],
                egui::FontId::new(theme::size::CAPTION, egui::FontFamily::Proportional),
                theme::DIM,
            );

            // Double-click the *picture* to restore. Click-only, so a drag across it
            // stays with the pan registered before the loop.
            let pic = ui.interact(
                rect,
                ui.id().with(("compare-cell", i)),
                egui::Sense::click(),
            );
            if pic.double_clicked() {
                restore = Some(i);
            }
            // **Drag one caption onto another to swap them.** A swap rather than an
            // insert-and-shift: with at most four cells the thing you mean by dragging
            // one onto another is "put these two side by side", and shifting the two
            // in between to achieve it moves cells you did not touch.
            let handle = ui
                .interact(
                    bar,
                    ui.id().with(("compare-drag", i)),
                    egui::Sense::click_and_drag(),
                )
                .on_hover_cursor(egui::CursorIcon::Grab);
            if handle.drag_stopped()
                && let Some(p) = ui.ctx().input(|inp| inp.pointer.interact_pos())
                && !rect.contains(p)
            {
                dropped = Some((i, p));
            }
            cell_rects.push(rect);
        }

        if let Some((from, p)) = dropped
            && let Some(to) = cell_rects.iter().position(|r| r.contains(p))
            && to != from
        {
            swap = Some((from, to));
        }

        // **The view is one view**, so a scroll or a drag anywhere in the grid moves
        // every tile together — which is the whole point of syncing them.
        let (mut zoom_to, mut pan_to) = (zoom, pan);
        if grid.dragged() {
            // In fractions of a cell's own extent, so the picture keeps up with the
            // pointer at any zoom and every tile moves by the same *relative* amount.
            let d = grid.drag_delta();
            pan_to.0 -= d.x / (cw * zoom).max(1.0);
            pan_to.1 -= d.y / (ch * zoom).max(1.0);
        }
        let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.0 && ui.rect_contains_pointer(area) {
            zoom_to = (zoom * (1.0 + scroll * 0.004)).clamp(1.0, tabs::Compare::MAX_ZOOM);
        }
        // Never past the edges: at fit there is nowhere to go, and the room to pan
        // grows as the zoom does. Clamped in the same normalised units the pan is in.
        let room = tabs::Compare::pan_room(zoom_to);
        pan_to.0 = pan_to.0.clamp(-room, room);
        pan_to.1 = pan_to.1.clamp(-room, room);
        if (zoom_to, pan_to) != (zoom, pan) {
            tab.compare.zoom = zoom_to;
            tab.compare.pan = pan_to;
        }

        // **Double-click restores, which is the prototype's gesture and is right here
        // even though the panel's is a button.** In the grid there is nothing else a
        // cell could mean by a click, and the picture *is* the affordance — where in
        // the list a row already had a label to double-click for a rename.
        if let Some(i) = restore
            && let Some(look) = looks.get(i)
        {
            tab.params.restore_look_from(look);
            tab.compare.open = false;
            tab.status = "restored".into();
        }
        // Reordering the *snapshots* rather than a private cell order, so the grid and
        // the panel cannot disagree about which `v003` is which. The pinned set is a
        // filter over that one list, so moving a row moves its cell.
        if let Some((a, b)) = swap {
            tab.snapshots.swap_pinned(a, b);
        }
        true
    }

    fn viewport_panel(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rs: &egui_wgpu::RenderState,
        actions: &[hotkeys::Action],
    ) {
        if self.compare_grid(ui, rs) {
            return;
        }
        // Read before the tab borrow, and handed to the shader so the fill it writes
        // matches the letterbox egui draws around it.
        let background = self.settings.background();
        // Same reason: the sampler runs inside a closure holding the tab.
        let sample = self.settings.sample_area();
        // `⇧D` / `⇧X` are holds rather than toggles, so they are read here as state
        // and OR'd into the tab's own overlays rather than stored on it — there is
        // nothing to store, and a toggle that had to be released would be a
        // different feature.
        let overlays = raw_gpu::Overlays {
            dodge_map: hotkeys::held(ctx, hotkeys::Action::ShowDodgeMap),
            burn_map: hotkeys::held(ctx, hotkeys::Action::ShowBurnMap),
            // Resolved here rather than stored: the panel names a layer by its index
            // in the full list and the shader indexes the rasterised one, so the
            // translation belongs at the boundary between them.
            zone_mask: self
                .tabs
                .active()
                .filter(|t| t.db_view_mask)
                .and_then(|t| t.db_active.and_then(|i| t.params.dodgeburn.active_index(i)))
                .map_or(-1, |i| i as i32),
            ..self.tabs.active().map(|t| t.overlays).unwrap_or_default()
        };
        let surround = self
            .settings
            .surround(self.tabs.active().is_some_and(|tab| tab.surround));
        let brush_pan = self.brush_pan;
        let Some(gpu) = &mut self.gpu else { return };
        let Some(tab) = self.tabs.active_mut() else {
            welcome(ui);
            return;
        };
        if let Some(e) = &tab.error {
            ui.centered_and_justified(|ui| {
                ui.colored_label(egui::Color32::from_rgb(220, 120, 120), e);
            });
            return;
        }
        // Taken before the mutable borrow of `render` below. Preview-original
        // substitutes here rather than anywhere that could be mistaken for an edit.
        let render_params = tab.render_params();
        let luma_gen = tab.luma_gen;
        // The geometry the whole rest of this function measures in. Resolved from
        // the same params that are about to be rendered, so the handles, the fit and
        // the pixels cannot disagree about where the frame is.
        let frame = tab.frame();
        let mode = tab.mode.clone();
        // A colour reference view. Drawn as a plain texture, entirely outside the
        // graph and the display shader — which is what keeps colour from being
        // pipeable into anything. See `raw_core::preview`.
        if let Some((_, img)) = tab.preview_image.clone() {
            // The readout is assigned here as well as on the render path, because
            // this one returns before reaching it — and a footer left holding the
            // last value the *print* reported would be describing a picture that is
            // no longer on screen.
            self.readout = draw_reference(ui, ctx, tab, &img, sample);
            return;
        }
        let (Some(render), Some(_), Some(frame)) = (&mut tab.render, &tab.luma, frame) else {
            let status = tab.status.clone();
            ui.centered_and_justified(|ui| ui.weak(status));
            return;
        };
        let vp = &mut render.viewport;

        let avail = ui.available_size();
        let ppp = ctx.pixels_per_point();
        let out_w = (avail.x * ppp).max(1.0) as u32;
        let out_h = (avail.y * ppp).max(1.0) as u32;
        // **The visible extent, which after a crop is the crop — not the stored
        // image.** Everything below is in frame pixels: the fit, the pan, the zoom
        // anchor and the readout. `vp.source_dims()` is the luminance texture and is
        // now the wrong number for every one of them; it is the thing on the far
        // side of `frame`'s transform, and only the shader speaks it.
        let (src_w, src_h) = (frame.crop.w, frame.crop.h);
        // Where that extent sits on the grid the view pans over. Zero unless cropped.
        let origin = egui::vec2(frame.crop.x as f32, frame.crop.y as f32);
        // FRAME is geometry around the photograph, not pixels in the render graph.
        // Resolve it here so Fit includes the complete export canvas while the GPU
        // continues to render only the photograph. Crop mode suppresses it: that tool
        // must show the source frame being cut, not the paper it may later sit on.
        let image_inches = render_params.output.print_inches(raw_core::Dims {
            w: src_w as usize,
            h: src_h as usize,
        });
        let export_frame = (!mode.is_crop() && render_params.frame.enabled)
            .then(|| render_params.frame.layout([image_inches.0, image_inches.1]))
            .transpose()
            .ok()
            .flatten();
        let (fit_origin, fit_w, fit_h) =
            export_frame.map_or((origin, src_w as f32, src_h as f32), |layout| {
                let sx = src_w as f32 / image_inches.0.max(f32::EPSILON);
                let sy = src_h as f32 / image_inches.1.max(f32::EPSILON);
                (
                    origin - egui::vec2(layout.margins.left * sx, layout.margins.top * sy),
                    layout.outer_inches[0] * sx,
                    layout.outer_inches[1] * sy,
                )
            });

        let (rect, response) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

        // A single anchor drives every zoom, and **what it is depends on whether the
        // image fills the viewport**.
        //
        // While the whole frame is on screen with surround around it, the anchor is
        // the viewport centre: there is nothing meaningful under the cursor to hold
        // fixed, and anchoring to it slides a floating picture sideways on the first
        // press of ⌘+ — which is the thing that felt wrong. Once the image overflows
        // the viewport the cursor is genuinely pointing *at* something, and holding
        // that point fixed is what lets you zoom into a detail rather than toward a
        // corner. So it centres until the image fills the frame, then the mouse
        // directs the zoom, which is what the prototype does.
        //
        // Both keyboard and scroll zoom read this. Two zoom paths that anchor
        // differently would be a worse surprise than either rule on its own.
        //
        // **The overflow test is against the scale being zoomed TO, not the one being
        // zoomed from**, and that distinction is the whole of a bug the maintainer reported:
        // `z` from fit stopped zooming to the mouse. At fit the image does not
        // overflow, so the old test chose the centre — but `z` jumps to 100%, which
        // on any real negative overflows enormously, and the cursor was pointing at
        // exactly the detail you wanted to land on. The rule was right and it was
        // being asked about the wrong moment.
        //
        // So the anchor is a function of the destination and each action supplies its
        // own. A press of `⌘+` from fit that still does not fill the viewport keeps
        // the centre, which is the case the rule was written for.
        let centre = egui::vec2(out_w as f32, out_h as f32) * 0.5;
        let pointer = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p))
            .map(|p| (p - rect.min) * ppp);
        let anchor_for = |target: f32| -> egui::Vec2 {
            let overflows = fit_w * target > out_w as f32 || fit_h * target > out_h as f32;
            if overflows {
                pointer.unwrap_or(centre)
            } else {
                centre
            }
        };
        // Scroll zoom is continuous and asks the same question per frame, about the
        // scale it is heading to; see its call below.

        // Keyboard view controls, from the frame's dispatched actions. They land
        // here rather than with the rest because zooming needs the anchor, which
        // only exists once the viewport rect does. egui's own Cmd +/-/0 UI zoom is
        // disabled in `App::new`, so these keys are ours.
        let view = &mut tab.view;
        let at_100 = !view.fit && (view.scale - 1.0).abs() < 0.01;
        for a in actions {
            match a {
                hotkeys::Action::ZoomToggle => {
                    if at_100 {
                        view.fit = true;
                    } else {
                        view.zoom_about(anchor_for(1.0), 1.0);
                        view.fit = false;
                    }
                }
                hotkeys::Action::ZoomFit => view.fit = true,
                // The preferred percentages, not a constant factor. See
                // `tabs::ZOOM_STEPS` — multiplying walked through 31.2% and 39.1%,
                // which are numbers nobody chose.
                hotkeys::Action::ZoomIn => {
                    let to = tabs::zoom_in_from(view.scale);
                    view.zoom_about(anchor_for(to), to);
                    view.fit = false;
                }
                hotkeys::Action::ZoomOut => {
                    let to = tabs::zoom_out_from(view.scale);
                    view.zoom_about(anchor_for(to), to);
                    view.fit = false;
                }
                _ => {}
            }
        }

        // Pan, unless a mode has claimed the drag. Dodge/Burn deliberately lends the
        // drag back while Space is held: a temporary hand tool, not a mode change, so
        // the selected layer and brush remain armed when Space comes up.
        if response.dragged() && mode.pans_with_drag(brush_pan) {
            let d = response.drag_delta() * ppp / view.scale;
            view.off -= d;
            view.fit = false;
        }
        // Inverted before it is used, not at the call site, so anything that reads
        // the wheel later reads it the way the user set it.
        let scroll = ctx.input(|i| i.smooth_scroll_delta.y)
            * if self.settings.invert_scroll {
                -1.0
            } else {
                1.0
            };
        if self.settings.scroll_zoom && scroll != 0.0 && response.hovered() {
            let to = view.scale * (1.0 + scroll * 0.002);
            view.zoom_about(anchor_for(to), to);
            view.fit = false;
        }

        // Fit last, so it wins for this frame when requested and always recomputes
        // against the current viewport size. Computed in physical pixels, so `scale`
        // means screen px per output px regardless of display density.
        if view.fit {
            // A margin at fit, so the frame edge is not welded to the app edge. It
            // shrinks the box the image is fitted *into*; the centring below still
            // uses the full viewport, so the image stays centred and simply does not
            // reach the sides. In points, not pixels, so it is the same visual gap
            // on a Retina display as on a cheap one.
            let m = VIEW_FIT_MARGIN * ppp;
            // **The mount comes out of the box too.** the maintainer's ask, and it is what
            // "fit" has to mean once there is a border: the surround is drawn *outside*
            // the image, in the same screen pixels this box is measured in, so fitting
            // the image alone put the mount in the margin and then off the edge of the
            // viewport. A 200px mount had nowhere to go at all, which is the case that
            // shows the bug plainly — you set a wide border and see three sides of it.
            //
            // Not folded into `VIEW_FIT_MARGIN`. They are different distances with
            // different owners: the margin is chrome, a constant that keeps the frame
            // off the app edge, and this is a user setting that is part of the
            // *picture* — a print
            // on a board is a thing of one size, and that whole thing is what has to
            // fit. Keeping them separate is also what makes them add correctly, which
            // is the behaviour you want: a mount does not eat the breathing room, it
            // sits inside it.
            //
            // **Physical pixels, not points**, and unlike `FIT_MARGIN` it is not scaled
            // by `ppp` — `Surround::width` reaches the shader unconverted, so this
            // reads it in the units it is actually drawn in. The two have to agree
            // exactly or the mount is clipped by however much they differ.
            let mount = surround.width.max(0.0);
            let s = fit_scale(
                egui::vec2(out_w as f32, out_h as f32),
                egui::vec2(fit_w, fit_h),
                m + mount,
            );
            view.scale = s;
            // Centre the CROP in the viewport, not the frame. `origin` is what makes
            // that true: without it a crop of one corner would be fitted correctly
            // and then drawn against the far edge of the window, because the pan
            // offset is measured from the frame's origin and the crop's is not there.
            view.off = fit_origin
                + egui::vec2(
                    (fit_w - out_w as f32 / s) * 0.5,
                    (fit_h - out_h as f32 / s) * 0.5,
                );
        }

        let (view_off, view_scale) = (view.off, view.scale);
        vp.render(
            gpu,
            &rs.device,
            &rs.queue,
            out_w,
            out_h,
            ViewGeometry {
                scale: view.scale,
                off_x: view.off.x,
                off_y: view.off.y,
                background,
                overlays,
                surround,
            },
            &render_params,
            &frame,
        );

        if vp.zone_render_pending() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
            if let Some(id) = render.texture {
                let uv = vp.uv_rect();
                ui.painter().image(
                    id,
                    rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(uv[0], uv[1])),
                    egui::Color32::WHITE,
                );
            } else {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Preparing tonal masks…",
                    egui::FontId::proportional(14.0),
                    egui::Color32::GRAY,
                );
            }
            return;
        }

        if let Some(message) = vp.render_error() {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                message,
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(220, 120, 120),
            );
            return;
        }

        if vp.target_changed || render.texture.is_none() {
            let view = vp.target_view().expect("rendered").clone();
            let mut renderer = rs.renderer.write();
            if let Some(old) = render.texture.take() {
                renderer.free_texture(&old);
            }
            // Nearest: the compute chain already produced exactly one texel per
            // physical screen pixel, so any filtering here would be a second resample
            // of an already-correct image.
            render.texture = Some(renderer.register_native_texture(
                &rs.device,
                &view,
                wgpu::FilterMode::Nearest,
            ));
        }

        let params = Arc::new(render_params.clone());
        render
            .histogram
            .prepare(Arc::clone(&params), frame, luma_gen);
        if let Some(after) = render.histogram.update(vp, gpu, &rs.device, &rs.queue) {
            ctx.request_repaint_after(after);
        }
        if let Some(bins) = render.histogram.bins().copied() {
            tab.histogram.accept_finished(bins);
        }

        // Read the export tap asynchronously. This includes the spatial graph and
        // keeps cursor motion from waiting on a GPU map operation.
        let cursor = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p))
            .and_then(|p| {
                let f = view_off + (p - rect.min) * ppp / view_scale;
                if !mode.is_crop()
                    && (f.x < origin.x
                        || f.y < origin.y
                        || f.x >= origin.x + src_w as f32
                        || f.y >= origin.y + src_h as f32)
                {
                    return None;
                }
                Some((f.x, f.y))
            });
        let pins = if tab.pins.show_raw || tab.pins.hidden {
            &[]
        } else {
            tab.pins.items.as_slice()
        };
        render.samples.prepare(params, frame, sample, cursor, pins);
        if render.samples.update(vp, gpu, &rs.device, &rs.queue) {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        self.readout = render
            .samples
            .get(SampleTarget::Cursor)
            .map(|sample| Readout::Print {
                lstar: sample.lstar,
                lab: sample.lab,
            });

        if let Some(id) = render.texture {
            // Targets are over-allocated to a 128px grid so resizing does not churn
            // texture registrations; draw only the part that was written.
            let [u, v] = vp.uv_rect();
            ui.painter().image(
                id,
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(u, v)),
                egui::Color32::WHITE,
            );
        }

        // Paint FRAME after the rendered texture. Only the four new margin strips
        // are covered; the photograph's texels remain untouched. The reference
        // Surround is moved to the *outside* of that canvas, preserving the visual
        // order: viewer background → Surround → exported FRAME → photograph.
        if let Some(layout) = export_frame {
            let image_rect = egui::Rect::from_min_size(
                rect.min + (origin - view_off) * view_scale / ppp,
                egui::vec2(src_w as f32, src_h as f32) * view_scale / ppp,
            );
            let outer = egui::Rect::from_min_max(
                image_rect.min
                    - egui::vec2(
                        image_rect.width() * layout.margins.left / image_inches.0,
                        image_rect.height() * layout.margins.top / image_inches.1,
                    ),
                image_rect.max
                    + egui::vec2(
                        image_rect.width() * layout.margins.right / image_inches.0,
                        image_rect.height() * layout.margins.bottom / image_inches.1,
                    ),
            );
            let painter = ui.painter().with_clip_rect(rect);
            let strips = |outer: egui::Rect, inner: egui::Rect| {
                [
                    egui::Rect::from_min_max(outer.min, egui::pos2(outer.right(), inner.top())),
                    egui::Rect::from_min_max(egui::pos2(outer.left(), inner.bottom()), outer.max),
                    egui::Rect::from_min_max(
                        egui::pos2(outer.left(), inner.top()),
                        egui::pos2(inner.left(), inner.bottom()),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(inner.right(), inner.top()),
                        egui::pos2(outer.right(), inner.bottom()),
                    ),
                ]
            };
            // A trim is one *export* pixel, so scale it from the photograph's
            // resolved output grid. It is clamped to one screen pixel in the
            // preview so it remains legible while the complete frame is fitted.
            let trim_outer = if render_params.frame.trim_line {
                let output_image = render_params.output.target_dims(raw_core::Dims {
                    w: src_w as usize,
                    h: src_h as usize,
                });
                let one_point = 1.0 / ppp;
                let trim_x = (image_rect.width() / output_image.w.max(1) as f32).max(one_point);
                let trim_y = (image_rect.height() / output_image.h.max(1) as f32).max(one_point);
                egui::Rect::from_min_max(
                    outer.min - egui::vec2(trim_x, trim_y),
                    outer.max + egui::vec2(trim_x, trim_y),
                )
            } else {
                outer
            };
            if surround.width > 0.0 {
                let mount = trim_outer.expand(surround.width / ppp);
                let rgb = surround
                    .rgb
                    .map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8);
                let color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                for strip in strips(mount, trim_outer) {
                    painter.rect_filled(strip, 0.0, color);
                }
            }
            let [r, g, b] = render_params.frame.color;
            let color = egui::Color32::from_rgb(r, g, b);
            for strip in strips(outer, image_rect) {
                painter.rect_filled(strip, 0.0, color);
            }
            if render_params.frame.trim_line {
                for strip in strips(trim_outer, outer) {
                    painter.rect_filled(strip, 0.0, egui::Color32::BLACK);
                }
            }
        }

        if mode.is_crop() {
            self.crop_tool(ui, ctx, rect, ppp, view_off, view_scale, &frame);
        }
        if mode.is_keystone() {
            self.keystone_tool(ui, ctx, rect, ppp, view_off, view_scale, &frame);
        }
        if mode.is_paint() {
            self.paint_tool(ui, ctx, rect, ppp, view_off, view_scale, &frame);
        }
        if mode.is_curve_point() {
            self.curve_point_tool(ui, ctx, rect, ppp, view_off, view_scale, &frame, sample);
        }
        if self.tabs.active().is_some_and(|t| t.loupe.open) {
            self.print_loupe(
                ui, ctx, rs, rect, ppp, view_off, view_scale, &frame, &response,
            );
        }
        // Last, so the pins sit over every tool's own overlay. They are a readout
        // rather than a tool, and a readout that a crop box could hide would be a
        // readout you had to close a tool to trust.
        self.pin_tool(
            ui, ctx, rect, ppp, view_off, view_scale, &frame, &response, sample,
        );
    }

    /// One-shot Curve point sampler. Its cursor is Triopro's white-point cursor:
    /// crosshair for the exact sampled pixel, eyedropper for the operation.
    #[allow(clippy::too_many_arguments)]
    fn curve_point_tool(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
        sample: settings::SampleArea,
    ) {
        if let Some(p) = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p))
        {
            ctx.set_cursor_icon(egui::CursorIcon::None);
            let painter = ui.painter_at(rect);
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(200));
            for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                let d = egui::vec2(dx, dy);
                painter.line_segment([p + d * 3.0, p + d * 8.0], stroke);
            }
            icons::paint_at(
                ui,
                &self.icons,
                "eyedropper",
                "⌖",
                egui::Rect::from_center_size(p + egui::vec2(11.0, -11.0), egui::vec2(18.0, 18.0)),
                egui::Color32::from_white_alpha(235),
                16.0,
            );
        }

        let Some(at) = ctx
            .input(|i| i.pointer.interact_pos())
            .filter(|p| rect.contains(*p))
            .filter(|_| ctx.input(|i| i.pointer.primary_released()))
        else {
            return;
        };
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(luma) = tab.luma.as_ref() else {
            return;
        };
        let f = view_off + (at - rect.min) * ppp / view_scale;
        let (sx, sy) = frame.to_source(f.x, f.y);
        let Some(raw) = sample_luma(luma, sx, sy, sample) else {
            return;
        };

        let active = tab
            .curve_active
            .min(tab.params.curve.instances.len().saturating_sub(1));
        let scene = (raw - tab.params.exposure.black) * tab.params.exposure.ev.exp2();
        let entering = tab.params.curve.apply_before(active, scene);
        let x = raw_core::Curve::to_normalized(entering);
        tab.params.curve.enabled = true;
        let Some(instance) = tab.params.curve.instances.get_mut(active) else {
            return;
        };
        // Put the point on the selected pass's current shape at the sampled input.
        // It is committed immediately (and can therefore reshape neighbouring
        // spline segments); there is no preview-only marker to confirm later.
        let y = instance.curve.eval(x);
        instance.curve.enabled = true;
        tab.curve_point = Some(instance.curve.add(x, y));
        tab.curve_drag = None;
        tab.mode = tabs::Mode::View;
    }

    /// The Inspector's pins: placing them, moving them, and drawing them.
    ///
    /// # The gestures, and why they are these gestures
    ///
    /// The prototype's, and they are already the app's: **click to place, drag to
    /// move, `⇧`-click to delete**. `⇧`-click is the one worth justifying — it is the
    /// same "modified click destroys" the Dodge & Burn layer list uses, and it is
    /// chosen over a hover-`×` because a pin is a few pixels on a picture and a
    /// close button on it would be a target smaller than the mark it removes.
    ///
    /// **All three need pin mode.** An earlier version let a pin be dragged whenever
    /// it was visible, on the reasoning that a mark you can see should be a mark you
    /// can grab. That fights pan — outside pin mode nothing has claimed the drag, so
    /// the picture pans *and* the pin moves — and one gesture doing two things is the
    /// defect, not the restriction. The mode is what makes the drag unambiguous, and
    /// it is what crop and the brush already do.
    ///
    /// # Coordinates
    ///
    /// Screen → frame → source going in, source → frame → screen coming out. Pins are
    /// stored against the **negative** — see [`tabs::Pin`] — so every read and write
    /// crosses `Frame`, and a pin stays on its subject through a rotation or a crop.
    #[allow(clippy::too_many_arguments)]
    fn pin_tool(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
        response: &egui::Response,
        sample: settings::SampleArea,
    ) {
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        if tab.pins.hidden || !tab.has_image() {
            // A drag that was in flight when the pins were hidden must not carry on
            // against marks nobody can see.
            tab.pins.dragging = None;
            return;
        }
        let placing = matches!(tab.mode, tabs::Mode::Pin);

        let to_source = |p: egui::Pos2| -> (f32, f32) {
            let f = view_off + (p - rect.min) * ppp / view_scale;
            frame.to_source(f.x, f.y)
        };
        let to_screen = |sx: f32, sy: f32| -> egui::Pos2 {
            let (fx, fy) = frame.from_source(sx, sy);
            rect.min + (egui::vec2(fx, fy) - view_off) * view_scale / ppp
        };

        // **The hit radius is in source pixels and derived from a screen distance**,
        // so grabbing a pin takes the same effort at fit as at 400%. A fixed source
        // radius would be untouchable when zoomed out and enormous when zoomed in.
        const GRAB_PX: f32 = 11.0;
        let shift = ctx.input(|i| i.modifiers.shift);

        // ── What each pin looks like, worked out before anything is hit-tested ──
        //
        // **The label box is part of the pin.** the maintainer reported reaching for a pin's
        // readout and getting the crosshair behaviour instead: the box is by far the
        // biggest thing on screen belonging to that pin, and it was inert. Laying the
        // boxes out first is what lets the same rects serve the hit test and the draw,
        // so the thing you can see and the thing you can grab cannot drift apart.
        const ARM: f32 = 7.0;
        const GAP: f32 = 2.5;
        let font = egui::FontId::new(theme::size::CAPTION, egui::FontFamily::Monospace);
        let pad = egui::vec2(5.0, 3.0);
        struct Mark {
            at: egui::Pos2,
            label: egui::Rect,
            galley: std::sync::Arc<egui::Galley>,
        }
        // Every crosshair first, because a label must clear the *other* pins' marks and
        // not only the other labels — a box parked on someone else's crosshair hides
        // the pixel that pin exists to point at.
        let ats: Vec<egui::Pos2> = tab.pins.items.iter().map(|p| to_screen(p.x, p.y)).collect();

        // ── Where each label goes, and why it is not always up-and-right ──────
        //
        // The box used to be pinned to one corner: up and right of the mark, always.
        // That is the prototype's arrangement and it is the right *first* choice — but
        // two pins near each other put one box straight over the other, and zooming out
        // makes it worse because the crosshairs converge while the boxes stay the same
        // size on screen. the maintainer asked whether they could flip sides instead, and they
        // can: the offset is the only thing that has to change, and the label rect is
        // already what both the hit test and the draw read.
        //
        // Four corners, tried in order — up-right, up-left, down-right, down-left. The
        // first is the prototype's, so a pin with room around it does not move; the
        // other three are only reached by a pin that would otherwise be buried.
        //
        // **Placement is greedy and in index order**, which matters: pin 1 keeps its
        // preferred corner and pin 2 yields. A solver that minimised total overlap
        // would shuffle boxes that were not in anyone's way whenever a distant pin
        // moved, and a label that jumps while you drag a different pin is worse than a
        // label in the second-best corner.
        const OFF: f32 = ARM + 3.0;
        let mut marks: Vec<Mark> = Vec::with_capacity(tab.pins.items.len());
        for (i, pin) in tab.pins.items.iter().enumerate() {
            let at = ats[i];
            let text = match (pin_value(tab, *pin, sample), pin_lab(tab, *pin, sample)) {
                (Some(v), Some((a, b))) => format!("{}  L {v:.1}  a {a:.1}  b {b:.1}", i + 1),
                (Some(v), None) => format!("{}  L {v:.1}", i + 1),
                (None, _) => format!("{}  L —", i + 1),
            };
            let galley = ui
                .painter()
                .layout_no_wrap(text, font.clone(), theme::BRIGHT);
            let sz = galley.size() + pad * 2.0;
            // Scoped so the borrow of `marks` ends before the push below.
            let label = {
                let taken: Vec<egui::Rect> = marks.iter().map(|m: &Mark| m.label).collect();
                pin_label_box(at, sz, OFF, &taken, &ats, i, rect)
            };
            marks.push(Mark { at, label, galley });
        }

        // The pin under a screen point: its label box, or near its crosshair. Boxes
        // are tested first and in reverse order, so the one drawn on top is the one
        // that answers — which is what "the one you clicked" means when two overlap.
        let hit = |p: egui::Pos2| -> Option<usize> {
            marks
                .iter()
                .enumerate()
                .rev()
                .find(|(_, m)| m.label.contains(p))
                .map(|(i, _)| i)
                .or_else(|| tabs::nearest_to(marks.iter().map(|m| m.at), p, GRAB_PX))
        };

        // **Placing, moving and deleting all require pin mode**, and that is a
        // correction rather than a preference. The first version allowed a drag on a
        // pin whenever the pins were visible, on the reasoning that a mark you can
        // see should be a mark you can grab. It fights pan: outside pin mode
        // `Mode::claims_drag` is false, so the viewport pans *and* the pin moves, and
        // one drag does two things. Pin mode is what makes the gesture unambiguous —
        // the same arrangement crop and the brush already have, and the prototype's.
        if !placing {
            tab.pins.dragging = None;
        }

        // ── Delete ───────────────────────────────────────────────────────────
        //
        // `⇧`+click, the "modified click destroys" the Dodge & Burn layer list uses.
        if placing
            && response.clicked()
            && shift
            && let Some(p) = ctx
                .input(|i| i.pointer.interact_pos())
                .filter(|p| rect.contains(*p))
            && let Some(i) = hit(p)
        {
            tab.pins.items.remove(i);
            tab.pins.dragging = None;
            return;
        }

        // ── Move ─────────────────────────────────────────────────────────────
        //
        // The grab is taken on the press and held across frames, like `curve_drag`:
        // re-picking the nearest pin each frame would let a fast drag hand off to a
        // pin it passed over.
        if placing
            && response.drag_started()
            && let Some(p) = ctx
                .input(|i| i.pointer.interact_pos())
                .filter(|p| rect.contains(*p))
        {
            tab.pins.dragging = hit(p);
        }
        if let Some(i) = tab.pins.dragging {
            if response.dragged()
                && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
                && let Some(pin) = tab.pins.items.get_mut(i)
            {
                let (sx, sy) = to_source(p);
                (pin.x, pin.y) = (sx, sy);
            }
            if response.drag_stopped() {
                tab.pins.dragging = None;
            }
        }

        // ── Place ────────────────────────────────────────────────────────────
        //
        // Only on a plain click in pin mode, and only where there is not already a
        // pin — clicking one you meant to grab should not stack a second on top of it.
        if placing
            && response.clicked()
            && !shift
            && let Some(p) = ctx
                .input(|i| i.pointer.interact_pos())
                .filter(|p| rect.contains(*p))
        {
            let (sx, sy) = to_source(p);
            let inside = tab.luma.as_ref().is_some_and(|l| {
                sx >= 0.0 && sy >= 0.0 && sx < l.output_dims.w as f32 && sy < l.output_dims.h as f32
            });
            if inside && hit(p).is_none() {
                tab.pins.items.push(tabs::Pin { x: sx, y: sy });
            }
        }

        // A crosshair while placing, because the gesture is *aim at a pixel* and an
        // arrow points with its tip rather than its centre. Same reason the brush
        // draws its own ring: the cursor should be the shape of what it will do.
        //
        // **And a `–` beside it when `⇧` is over a pin**, which is the maintainer's note and
        // the prototype's affordance. It matters more here than a modifier hint
        // usually does: `⇧`+click is destructive and lands on a mark a few pixels
        // wide, so *which* pin is about to go — and whether one is under the pointer
        // at all — has to be visible before the button goes down. Drawn rather than
        // set as a `CursorIcon`, because the OS set has no eyedropper-minus and
        // `NotAllowed` says the opposite of what this does.
        let pointer = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p));
        let under = pointer.and_then(hit);
        // The custom mark is painted after the pins below. Painting it here put the
        // pin's own crosshair over the delete minus at exactly the point where the
        // cursor needed to be clearest.
        let custom_cursor = if placing && response.hovered() {
            match (under, shift) {
                // **Over a pin, no modifier: it can be moved.** the maintainer's glyph, drawn
                // where the system cursor would be for the reason the rotate cursor is
                // — egui's `CursorIcon::Move` is the OS four-way *window* move and
                // reads as "drag this window", which is not what a pin does.
                (Some(_), false) => {
                    ctx.set_cursor_icon(egui::CursorIcon::None);
                    Some(false)
                }
                // **Over a pin with `⇧`: it is about to go.** A bare minus, no ring
                // — the maintainer's note, and the ring was doing nothing the colour did not
                // already do. Wider than a glyph would be so it reads at a glance.
                (Some(_), true) => {
                    ctx.set_cursor_icon(egui::CursorIcon::None);
                    Some(true)
                }
                // Empty picture: aim at a pixel. An arrow points with its tip rather
                // than its centre, which is the wrong shape for placing a sample.
                (None, _) => {
                    ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
                    None
                }
            }
        } else {
            None
        };
        let doomed = under.filter(|_| placing && response.hovered() && shift);

        // ── Draw ─────────────────────────────────────────────────────────────
        let painter = ui.painter_at(rect);
        for (i, m) in marks.iter().enumerate() {
            if !rect.contains(m.at) {
                continue;
            }
            // The one about to be deleted wears ruby, so the destructive gesture
            // names its target rather than leaving you to infer it from proximity.
            let colour = if doomed == Some(i) {
                theme::RUBY
            } else {
                tabs::pin_colour(i)
            };
            // A crosshair rather than a dot: the mark has to say *exactly* which
            // pixel is being read, and a filled dot covers the thing it measures.
            let stroke = egui::Stroke::new(1.5, colour);
            for (dx, dy) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0)] {
                let d = egui::vec2(dx, dy);
                painter.line_segment([m.at + d * GAP, m.at + d * ARM], stroke);
            }
            // The value, in a box offset diagonally from the mark so it never covers
            // the pixel it reports. Up and right by preference — the prototype's
            // arrangement — and one of the other three corners when that one is taken.
            // See where `marks` is built.
            painter.rect_filled(m.label, 2.0, theme::CHROME_DEEP.gamma_multiply(0.92));
            painter.rect_stroke(
                m.label,
                2.0,
                egui::Stroke::new(1.0, colour),
                egui::StrokeKind::Inside,
            );
            painter.galley(m.label.min + pad, m.galley.clone(), colour);
        }

        // Custom cursors last, so the Inspector pin beneath the pointer can never
        // cover the move glyph or the destructive minus.
        if let (Some(p), Some(delete)) = (pointer, custom_cursor) {
            if delete {
                painter.line_segment(
                    [p - egui::vec2(9.0, 0.0), p + egui::vec2(9.0, 0.0)],
                    egui::Stroke::new(2.5, theme::RUBY),
                );
            } else {
                icons::paint_at(
                    ui,
                    &self.icons,
                    "move",
                    "✶",
                    egui::Rect::from_center_size(p, egui::vec2(18.0, 18.0)),
                    egui::Color32::from_white_alpha(235),
                    16.0,
                );
            }
        }
    }

    /// The print loupe: the reticle on the picture, and the window over it.
    ///
    /// Runs after the image has been painted, so the loupe sits on top of it — and
    /// after the crop and paint tools for the same reason, though the modes are
    /// exclusive so the three are never up at once.
    ///
    /// **The sample point is in frame pixels**, which is the space `Frame::crop` is
    /// in and the space `Viewport::patch` takes. Screen → frame is one line and its
    /// inverse is one line; there is no source-space step here, unlike the brush,
    /// because the loupe samples the *composed* picture rather than the negative.
    #[allow(clippy::too_many_arguments)]
    fn print_loupe(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rs: &egui_wgpu::RenderState,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
        response: &egui::Response,
    ) {
        let Some(gpu) = &mut self.gpu else { return };
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };

        // **The mode is the source of truth, not the checkbox.** Opening the crop tool
        // or the brush replaces `Mode::Loupe`, and a loupe left drawing over them
        // would be a window onto a picture nobody is looking at — and would fight the
        // tool for the drag. Modes are exclusive; this is what makes the loupe one.
        if !tab.mode.is_loupe() {
            tab.loupe.open = false;
            tab.loupe.forget();
            return;
        }

        // A drag anywhere on the picture moves the sample, which is the prototype's
        // gesture. Pan has already stood down for this frame — `Mode::claims_drag` —
        // so there is nothing to arbitrate here.
        let to_frame = |p: egui::Pos2| -> (f32, f32) {
            let f = view_off + (p - rect.min) * ppp / view_scale;
            (f.x, f.y)
        };
        if (response.dragged() || response.clicked())
            && let Some(p) = ctx
                .input(|i| i.pointer.interact_pos())
                .filter(|p| rect.contains(*p))
        {
            tab.loupe.at = Some(to_frame(p));
        }

        let params = tab.params.effective();
        let place = loupe::place_magnified(tab.loupe.at, frame, &params, tab.loupe.magnification());

        // Ask for a render if what is on screen is not what the parameters describe.
        // Cheap when nothing moved: two comparisons and a return.
        if let Some(render) = &mut tab.render {
            loupe::refresh(
                &mut tab.loupe,
                ctx,
                gpu,
                &rs.device,
                &rs.queue,
                &mut render.viewport,
                &params,
                frame,
                &place,
            );
        }
        tab.loupe.poll(ctx);

        // ── The reticle ──────────────────────────────────────────────────────
        //
        // Drawn from the *placed* sample rather than from the pointer, so it stops
        // where the sample stops instead of sliding off the edge with the cursor —
        // and so it grows when Output is resampling, which is the visible statement
        // that one loupe-worth of file is more than one loupe-worth of picture.
        let (cx, cy, hx, hy) = place.reticle;
        let to_screen =
            |x: f32, y: f32| rect.min + (egui::vec2(x, y) - view_off) * view_scale / ppp;
        let centre = to_screen(cx, cy);
        let far = to_screen(cx + hx, cy + hy);
        let win_w = loupe::SIZE_W as f32 / ppp;
        let win_h = loupe::SIZE_H as f32 / ppp;

        // **The reticle's size is the sample's, and that is why it moves with zoom.**
        // It marks the region the loupe is showing, so at 100% it is exactly the
        // loupe's own size, at fit it is small, and past 100% it is larger than the
        // window it feeds. All three are truthful and only the last one caused
        // trouble: the window used to be placed a fixed gap from the reticle's
        // *centre*, so once the reticle grew past that gap the two overlapped.
        //
        // Clamped at the top end all the same. Past about twice the window the marker
        // has become a border round most of the viewport, and an outline you cannot see
        // the whole of marks nothing.
        //
        // **Clamped as a whole rather than per axis**, which is the one thing the round
        // reticle did not have to think about: one factor from the width, and the
        // height follows it. Clamping the two independently would square up a 3:2
        // marker at the ends of its range and stop it being a picture of the sample.
        let raw = egui::vec2(
            (far - centre).x.abs().max(1e-3),
            (far - centre).y.abs().max(1e-3),
        );
        let k = raw.x.clamp(6.0, win_w) / raw.x;
        let reticle = egui::Rect::from_center_size(centre, raw * k * 2.0);
        let painter = ui.painter_at(rect);
        // Amber, hairline, no fill — the prototype's, and it has to read over both a
        // white sky and a black shadow without hiding either.
        painter.rect_stroke(
            reticle,
            0.0,
            egui::Stroke::new(1.0, theme::AMBER),
            egui::StrokeKind::Inside,
        );

        // ── The window ───────────────────────────────────────────────────────
        //
        // The old placement flipped X and Y independently, so the loupe always read
        // as a box arriving from one of the sample's corners. Treat the pair as a
        // small layout instead: loupe left/right with their vertical centres aligned,
        // or loupe above/below with their horizontal centres aligned. The side with
        // the most room wins, and the shared helper keeps the tighter visible gap and
        // the viewer-edge fallback covered by geometry tests.
        let win = loupe::window_rect(rect, reticle, egui::vec2(win_w, win_h));

        match tab.loupe.texture() {
            Some(tex) => {
                // **A plain textured rect, where this used to be a mesh fan clipped to
                // a circle.** The circle needed a mesh because egui cannot clip an
                // image to an ellipse, and it cost the 21% of the tile that lay outside
                // the inscribed disc: rendered, then not drawn. A 3:2 window shows all
                // of what it renders, so the whole presentation is one call.
                //
                // What the circle was buying — a window that reads as a lens rather
                // than as another panel in a viewport full of rectangles — is carried
                // by the amber stroke and by the offset placement above, which are the
                // two things that say "this floats over the picture".
                //
                // Dimmed while a new tile is in flight, so a stale picture never
                // silently passes for a fresh one during a drag.
                let tint = if tab.loupe.rendering() {
                    egui::Color32::from_gray(140)
                } else {
                    egui::Color32::WHITE
                };
                let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
                painter.image(tex.id(), win, uv, tint);
            }

            // Nothing rendered yet. A dark panel rather than nothing at all, so the
            // first press puts something where the loupe is going to be instead of
            // leaving an outline floating over an unchanged picture.
            None => {
                painter.rect_filled(win, 0.0, egui::Color32::from_gray(26));
            }
        }
        painter.rect_stroke(
            win,
            0.0,
            egui::Stroke::new(1.0, theme::AMBER),
            egui::StrokeKind::Inside,
        );
    }

    /// The brush: where a press lands on the negative, and the pass it builds.
    ///
    /// **Dabs are recorded in SOURCE-normalised coordinates**, which is the whole
    /// coordinate contract and the one thing to get right here. Screen → frame →
    /// source → normalised, in that order, using `Frame::to_source` — the same three
    /// steps the value readout takes, and for the same reason: the stored pixel is
    /// wherever the orientation and the straighten put it. Recording in frame
    /// coordinates would look correct on an unrotated file and slide every stroke on
    /// a rotated one.
    ///
    /// The cursor is a ring the size of the brush, drawn in points at the window's
    /// own density like the crop handles — chrome, not pixels. It is the only thing
    /// that says what radius the brush currently is, and it is why `[` and `]` need
    /// no readout to be usable.
    #[allow(clippy::too_many_arguments)]
    fn paint_tool(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
    ) {
        let brush = self.brush;
        let brush_pan = self.brush_pan;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        if brush_pan {
            // A stroke cannot remain live across a pan. If Space was pressed during
            // one, close that pass where it stands; releasing Space while the mouse
            // is still down then cannot draw a long connecting line across the view.
            if let tabs::Mode::Paint { grab, .. } = &mut tab.mode {
                *grab = None;
            }
            if ctx
                .input(|i| i.pointer.latest_pos())
                .is_some_and(|p| rect.contains(p))
            {
                let dragging = ctx.input(|i| i.pointer.primary_down());
                ctx.set_cursor_icon(if dragging {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
            return;
        }
        let Some(sign) = tab.mode.painting() else {
            return;
        };
        let Some(luma) = tab.luma.as_ref() else {
            return;
        };
        let (lw, lh) = (luma.output_dims.w as f32, luma.output_dims.h as f32);
        let aspect = lh / lw;

        let pointer = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p));
        let (down, pressed, alt, shift) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.primary_pressed(),
                i.modifiers.alt,
                i.modifiers.shift,
            )
        });

        // Screen point -> the negative, normalised. Returns None outside the stored
        // image, which after a straighten is a real part of the frame: the corners
        // the rotation brought in have no negative behind them, and a dab there
        // would be recorded at a coordinate outside [0,1] that no pixel can ever
        // read.
        let to_negative = |p: egui::Pos2| -> Option<(f32, f32)> {
            let f = view_off + (p - rect.min) * ppp / view_scale;
            let (sx, sy) = frame.to_source(f.x, f.y);
            (sx >= 0.0 && sy >= 0.0 && sx < lw && sy < lh).then(|| (sx / lw, sy / lh))
        };

        // Screen point <- the negative. The inverse of `to_negative`, and what puts
        // a gradient's handles on the picture they describe.
        let to_screen = |at: (f32, f32)| -> egui::Pos2 {
            let (fx, fy) = frame.from_source(at.0 * lw, at.1 * lh);
            rect.min + (egui::vec2(fx, fy) - view_off) * view_scale / ppp
        };

        let tool = tab.mode.tool().unwrap_or_default();
        let mut grab = tab.mode.grab();

        // Which handle the press would take, if any. Hit-tested in SCREEN points so
        // the target is the same size however far you are zoomed out — a handle
        // measured in image coordinates is unhittable at fit and covers the picture
        // at 400%.
        const HANDLE_HIT: f32 = 9.0;
        let placed = tab
            .db_active
            .and_then(|i| tab.params.dodgeburn.instances.get(i))
            .and_then(|inst| paint::handles(&inst.shape, aspect));
        let over_handle = |p: egui::Pos2| -> Option<paint::End> {
            let (from, to) = placed?;
            // `To` first: on a gradient that has only just been placed the two
            // coincide, and the far end is the one the drag was about.
            [(paint::End::To, to), (paint::End::From, from)]
                .into_iter()
                .find(|(_, at)| to_screen(*at).distance(p) <= HANDLE_HIT)
                .map(|(e, _)| e)
        };

        if pressed && let Some(at) = pointer.and_then(to_negative) {
            let db = &mut tab.params.dodgeburn;
            // Grabbing a handle acts on the instance already selected and must not
            // create one — otherwise reaching for the end of a gradient would lay a
            // second gradient on top of it.
            let taken = pointer.and_then(over_handle);
            match taken {
                Some(end) => grab = Some(tabs::PaintGrab::Handle(end)),
                None => {
                    match paint::instance_for(db, &mut tab.db_active, sign, tool, brush.nib, false)
                    {
                        Some(i) => {
                            let inst = &mut db.instances[i];
                            if tool.is_gradient() {
                                // Placement and adjustment are one gesture: the press
                                // resets the shape to a zero-length one under the
                                // cursor and grabs its far end, so the drag that
                                // follows is the same drag that moves a handle.
                                let ev = sign.ev() * brush.intensity;
                                inst.shape = tool.shape(at, ev, brush.nib);
                                grab = Some(tabs::PaintGrab::Handle(paint::End::To));
                            } else {
                                let erasing = alt;
                                // `⇧`-click draws a straight pass from wherever the last
                                // one ended to here — the darkroom equivalent of a
                                // ruler, and the one gesture that is not a drag.
                                // Interpolating from the last dab rather than from the
                                // last *pointer* position is what makes it join up: the
                                // pointer has been panning and zooming since.
                                let from = shift
                                    .then(|| inst.gestures().last())
                                    .flatten()
                                    .and_then(|g| g.dabs.last())
                                    .map(|d| (d.x, d.y));
                                paint::begin(inst, &brush, from.unwrap_or(at), erasing);
                                let (last, bearing) = match from {
                                    Some(f) => paint::extend(inst, &brush, f, at, aspect, erasing),
                                    None => (at, None),
                                };
                                grab = Some(tabs::PaintGrab::Stroke {
                                    last,
                                    erasing,
                                    bearing,
                                });
                            }
                        }
                        None => {
                            self.pending_note = Some(format!(
                                "{} instances is the limit",
                                raw_core::DodgeBurnParams::MAX_INSTANCES
                            ));
                        }
                    }
                }
            }
        } else if down
            && let Some(g) = grab
            && let Some(at) = pointer.and_then(to_negative)
            && let Some(i) = tab.db_active
            && let Some(inst) = tab.params.dodgeburn.instances.get_mut(i)
        {
            match g {
                tabs::PaintGrab::Stroke {
                    last,
                    erasing,
                    bearing,
                } => {
                    let (last, laid) = paint::extend(inst, &brush, last, at, aspect, erasing);
                    // The last direction is *kept* when a frame lays no dabs, so a
                    // following nib does not snap back to axis-aligned every time
                    // the pointer pauses inside one spacing.
                    grab = Some(tabs::PaintGrab::Stroke {
                        last,
                        erasing,
                        bearing: laid.or(bearing),
                    });
                }
                tabs::PaintGrab::Handle(end) => {
                    paint::move_handle(&mut inst.shape, end, at, aspect);
                }
            }
        }
        if !down {
            // Released. The gesture is closed simply by forgetting it — there is
            // nothing to commit, because every dab and every handle move went onto
            // `params` as it happened. `History`'s existing gesture coalescing turns
            // the whole drag into one undo entry, which is exactly the prototype's
            // "⌘Z undoes the last pass" with no special case for it.
            grab = None;
        }
        if let tabs::Mode::Paint { grab: slot, .. } = &mut tab.mode {
            *slot = grab;
        }

        if tool.is_gradient() {
            if let Some((from, to)) = tab
                .db_active
                .and_then(|i| tab.params.dodgeburn.instances.get(i))
                .and_then(|inst| paint::handles(&inst.shape, aspect))
            {
                let radial = tab
                    .db_active
                    .and_then(|i| tab.params.dodgeburn.instances.get(i))
                    .and_then(|inst| match &inst.shape {
                        raw_core::dodgeburn::Shape::Radial(r) => Some(*r),
                        _ => None,
                    });
                Self::gradient_overlay(ui, &to_screen, from, to, radial, aspect);
            }
            // A crosshair, not the brush ring: a gradient has no radius, and a ring
            // sized by a control that does nothing to it would be a lie.
            if pointer.is_some() {
                ctx.set_cursor_icon(match pointer.and_then(over_handle) {
                    Some(_) => egui::CursorIcon::Grab,
                    None => egui::CursorIcon::Crosshair,
                });
            }
            return;
        }

        // The brush ring. Radius is a fraction of the negative's WIDTH, so it is
        // scaled by the width and drawn as a circle — the same aspect convention
        // the dab itself uses, seen from the other end.
        //
        // **Drawn in the middle of the viewport while a brush control is being
        // worked**, even though the pointer is then over the panel and not the
        // picture. That is the one case where "draw the cursor where the cursor is"
        // gives you nothing: reaching Radius means leaving the image, so the nib you
        // are sizing is exactly the thing you cannot see. See `App::brush_adjusting`.
        //
        // **The centre rather than where the pointer last was**, which is the maintainer's
        // call. The remembered position is wherever you happened to leave the picture
        // on the way to the panel — often a corner, sometimes under the panel you are
        // now working in — so the ghost appeared somewhere arbitrary and occasionally
        // out of sight. The centre is always visible, always the same place, and is
        // where you look while judging a size.
        let ghost = pointer.is_none();
        if let Some(p) = pointer.or(self.brush_adjusting.then(|| rect.center())) {
            // Only hide the system cursor when the pointer is actually over the
            // picture — hiding it while it is over a slider would lose it.
            if !ghost {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
            let r = brush.radius * lw * view_scale / ppp;
            // Dodge light, burn dark, eraser ruby — see `theme::INK_LIGHT`.
            let (ink, halo) = match (sign, alt) {
                (_, true) => (theme::RUBY, theme::INK_DARK),
                (raw_core::Sign::Dodge, _) => (theme::INK_LIGHT, theme::INK_DARK),
                (raw_core::Sign::Burn, _) => (theme::INK_DARK, theme::INK_LIGHT),
            };
            let painter = ui.painter();

            // **The cursor draws the nib, not a circle.** It is the only thing that
            // says what the brush currently is, and an elliptical or card-shaped
            // brush with a round cursor cannot be aimed. While the nib follows the
            // stroke it is drawn at the last direction it was laid along, so it
            // rotates as you draw rather than lying about being axis-aligned.
            let angle = match (brush.follow, tab.mode.grab()) {
                (
                    true,
                    Some(tabs::PaintGrab::Stroke {
                        bearing: Some(b), ..
                    }),
                ) => b,
                (true, _) => 0.0,
                _ => brush.angle,
            };
            let outline = |scale: f32| -> Vec<egui::Pos2> {
                let (sin, cos) = angle.to_radians().sin_cos();
                let turn = |ex: f32, ey: f32| {
                    p + egui::vec2(ex * cos - ey * sin, ex * sin + ey * cos) * scale
                };
                match brush.nib {
                    raw_core::dodgeburn::Nib::Card => vec![
                        turn(-r, -r * brush.aspect),
                        turn(r, -r * brush.aspect),
                        turn(r, r * brush.aspect),
                        turn(-r, r * brush.aspect),
                        turn(-r, -r * brush.aspect),
                    ],
                    _ => (0..=48)
                        .map(|i| {
                            let t = i as f32 / 48.0 * std::f32::consts::TAU;
                            turn(r * t.cos(), r * brush.aspect * t.sin())
                        })
                        .collect(),
                }
            };
            let ring = outline(1.0);
            for (w, c) in [(2.0, halo), (1.0, ink)] {
                painter.add(egui::Shape::line(ring.clone(), egui::Stroke::new(w, c)));
            }

            // **The feather, as the actual half-strength contour.** Sigma is not
            // that contour: at the default Feather 0.40 it lies almost on the solid
            // outer ring, which made a very soft brush look hard and made 0–0.40
            // appear to do nothing. The CPU solves the rendered profile itself, so
            // this guide cannot drift away from what a dab paints.
            if let Some(half) = raw_core::dodgeburn::Dab::half_strength_radius(brush.feather) {
                painter.add(egui::Shape::dashed_line(
                    &outline(half),
                    egui::Stroke::new(1.0, theme::FEATHER_GUIDE),
                    4.0,
                    4.0,
                ));
            }
            // A centre mark, because a soft brush at a large radius gives no clue
            // where its middle is.
            painter.circle_filled(p, 1.5, ink);
        }
    }

    /// The crop tool's frame: the overlay, the hit test, and the three drags.
    ///
    /// Drawn **after** the image and outside the GPU path entirely — handles are
    /// chrome, not pixels, the same way the curve editor's control points are. That
    /// is also what keeps them crisp: they are painted in points at the window's own
    /// density rather than resampled with the picture.
    #[allow(clippy::too_many_arguments)]
    fn crop_tool(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
    ) {
        let icons = &self.icons;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(entered) = tab.mode.cancelled() else {
            return;
        };

        // Frame pixels -> screen points. The inverse of what the viewport does to
        // place the image, so the rectangle lands exactly on the picture it describes.
        let to_screen = |fx: f32, fy: f32| -> egui::Pos2 {
            rect.min + (egui::vec2(fx, fy) - view_off) * view_scale / ppp
        };
        // Placed by the frame, never by dividing here: the crop is normalised
        // against `oriented` and has to land on `frame`, and the two are concentric
        // rather than aligned whenever the picture is straightened.
        let c = frame.place(tab.params.composition.crop);
        let screen = egui::Rect::from_min_max(
            to_screen(c.x as f32, c.y as f32),
            to_screen((c.x + c.w as i32) as f32, (c.y + c.h as i32) as f32),
        );

        let pointer = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|p| rect.contains(*p));
        let (down, pressed, shift) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.primary_pressed(),
                i.modifiers.shift,
            )
        });
        let over = pointer.map_or(crop::Zone::Outside, |p| crop::hit(screen, p));

        // Start a gesture. Which one depends on where the press landed — and on
        // whether the straighten tool is armed, which overrides all of them.
        let mut grabbed = tab.mode.grabbed();
        if pressed && let Some(p) = pointer {
            grabbed = if tab.straighten_armed {
                Some(tabs::Grab::Line { from: p, to: p })
            } else {
                match over {
                    crop::Zone::Outside => None,
                    crop::Zone::Grip(h) => Some(tabs::Grab::Grip(h)),
                    crop::Zone::Rotate(_) => Some(tabs::Grab::Rotate {
                        bearing: bearing(screen.center(), p),
                        base: tab.params.composition.straighten,
                    }),
                }
            };
        }
        if !down {
            // Released. A finished line is the one gesture that commits on release
            // rather than continuously: it is a measurement, and half of one is not
            // a smaller correction, it is a wrong one.
            if let Some(tabs::Grab::Line { from, to }) = grabbed {
                if let Some(deg) = crop::angle_of(from, to) {
                    let range = CompositionParams::STRAIGHTEN_RANGE;
                    tab.params.composition.straighten =
                        crop::snap(deg, shift).clamp(*range.start(), *range.end());
                    // The angle just moved under the crop, so the crop has to come
                    // back onto the picture. The COMPOSITION slider's own call cannot
                    // cover this one: it compares against a snapshot taken at the top
                    // of the panel body, and by the time that body next runs this
                    // write has already happened.
                    tab.confine_crop();
                }
                // One-shot, however it ended — including a drag too short to
                // measure. A tool that stayed armed after a slip would redraw the
                // angle on the next attempt to nudge an edge.
                tab.straighten_armed = false;
            }
            grabbed = None;
        }

        let delta = ctx.input(|i| i.pointer.delta());
        match grabbed {
            Some(tabs::Grab::Grip(h)) if delta != egui::Vec2::ZERO => {
                // Screen points -> frame pixels: the same scale the rectangle was
                // drawn with, inverted. Applying a screen delta to a normalised
                // rectangle would make the crop move at a speed that depended on
                // the zoom.
                let d = delta * ppp / view_scale;
                let ratio = frame.target_ratio(
                    tab.params.composition.ratio,
                    tab.params.composition.portrait,
                );
                tab.params.composition.crop =
                    crop::drag(tab.params.composition.crop, h, (d.x, d.y), ratio, frame);
            }
            Some(tabs::Grab::Rotate {
                bearing: start,
                base,
            }) => {
                if let Some(p) = pointer {
                    // The *difference* in bearing, not the bearing itself, so the
                    // picture does not snap to meet the cursor when the drag starts.
                    let range = CompositionParams::STRAIGHTEN_RANGE;
                    let deg = base + (bearing(screen.center(), p) - start);
                    tab.params.composition.straighten =
                        crop::snap(deg, shift).clamp(*range.start(), *range.end());
                    // Every frame of the drag, because every frame of it moves the
                    // angle. `confine_crop` no-ops while the corners are still on the
                    // picture, so this costs one `covers` test per frame and the box
                    // only moves when the rotation has actually taken a corner off it.
                    tab.confine_crop();
                }
            }
            Some(tabs::Grab::Line { from, .. }) => {
                if let Some(p) = pointer {
                    grabbed = Some(tabs::Grab::Line { from, to: p });
                }
            }
            _ => {}
        }
        tab.mode = tabs::Mode::Crop { grabbed, entered };

        // The cursor reports what a press would do before it is pressed, which is
        // most of what makes eight handles and a rotate ring discoverable without a
        // legend.
        let cursor = if tab.straighten_armed || matches!(grabbed, Some(tabs::Grab::Line { .. })) {
            Some(egui::CursorIcon::Crosshair)
        } else {
            match grabbed.map(zone_of).unwrap_or(over) {
                crop::Zone::Grip(h) => Some(h.cursor()),
                // **egui has no rotate cursor**, and the grab hand it fell back to
                // says "this drags something" without saying what — which in the one
                // zone whose whole job is to be distinguishable from the resize
                // beside it is the wrong thing to say. So the system cursor is
                // hidden and the icon is drawn at the pointer, which is what the
                // prototype does with a hand-built pixmap.
                crop::Zone::Rotate(_) => Some(egui::CursorIcon::None),
                crop::Zone::Outside => None,
            }
        };
        if let Some(c) = cursor {
            ctx.set_cursor_icon(c);
        }

        let levelling = matches!(
            grabbed,
            Some(tabs::Grab::Rotate { .. }) | Some(tabs::Grab::Line { .. })
        );
        // This runs *after* the frame has been dispatched, so a gesture that changed
        // the params changed them too late for the picture on screen. Panning and
        // resizing do not care — the crop is suppressed while the tool is open, so
        // only the overlay moved, and the overlay is drawn below. **Levelling does**:
        // it changes `straighten`, which is the image itself. Without this the
        // rotation would land a frame behind the cursor, and the last frame of a
        // drag would never arrive at all, because egui repaints on input and the
        // release is the final input there is.
        if levelling {
            ctx.request_repaint();
        }
        // The rotate cursor, drawn where the system one would have been.
        if matches!(grabbed.map(zone_of).unwrap_or(over), crop::Zone::Rotate(_))
            && !tab.straighten_armed
            && let Some(p) = pointer
        {
            icons::paint_at(
                ui,
                icons,
                "rotate",
                "⟳",
                egui::Rect::from_center_size(p, egui::vec2(18.0, 18.0)),
                egui::Color32::from_white_alpha(235),
                16.0,
            );
        }

        crop::overlay(
            ui.painter(),
            &crop::Overlay {
                viewport: rect,
                screen,
                hover: if grabbed.is_some() {
                    crop::Zone::Outside
                } else {
                    over
                },
                dragging: grabbed.is_some(),
                levelling,
                guide: tab.guide,
                angle: tab.params.composition.straighten,
                dims: (c.w, c.h),
                line: match grabbed {
                    Some(tabs::Grab::Line { from, to }) => Some((from, to)),
                    _ => None,
                },
            },
        );
    }

    /// Manual perspective guides. Their four points are authored in the oriented
    /// photograph and drawn over that unwarped photograph while the tool is open;
    /// leaving the tool applies the correction in one step.
    #[allow(clippy::too_many_arguments)]
    fn keystone_tool(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        view_off: egui::Vec2,
        view_scale: f32,
        frame: &raw_core::Frame,
    ) {
        let temporary_hand = self.brush_pan;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(entered) = tab.mode.cancelled() else {
            return;
        };
        let mode = tab.params.composition.keystone.mode;
        if mode == KeystoneMode::Off {
            tab.mode = tabs::Mode::View;
            return;
        }
        if temporary_hand {
            tab.mode = tabs::Mode::Keystone {
                grabbed: None,
                entered,
            };
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
            return;
        }

        let to_screen = |fx: f32, fy: f32| -> egui::Pos2 {
            rect.min + (egui::vec2(fx, fy) - view_off) * view_scale / ppp
        };
        let to_frame = |point: egui::Pos2| -> (f32, f32) {
            let f = view_off + (point - rect.min) * ppp / view_scale;
            (f.x, f.y)
        };
        let pointer = ctx
            .input(|i| i.pointer.latest_pos())
            .filter(|point| rect.contains(*point));
        let (down, pressed) =
            ctx.input(|i| (i.pointer.primary_down(), i.pointer.primary_pressed()));
        const DRAW_FIRST_LINE: usize = 4;
        const DRAW_SECOND_LINE: usize = 5;
        let mut grabbed = tab.mode.keystone_grabbed();
        let screen_points = |tab: &tabs::Tab| -> [egui::Pos2; 4] {
            std::array::from_fn(|i| {
                let point = tab.params.composition.keystone.guides[i];
                let (x, y) = frame.guide_to_frame(point);
                to_screen(x, y)
            })
        };
        let points = screen_points(tab);
        if pressed && let Some(pointer) = pointer {
            let line_mode = matches!(
                tab.params.composition.keystone.mode,
                KeystoneMode::Vertical | KeystoneMode::Horizontal
            );
            let first_drawn = tab.params.composition.keystone.guides != KeystoneParams::TARGETS;
            let waiting_for_second = line_mode
                && first_drawn
                && tab.params.composition.keystone.correction <= f32::EPSILON;
            let visible: &[usize] = match tab.params.composition.keystone.mode {
                KeystoneMode::Vertical if waiting_for_second => &[0, 3],
                KeystoneMode::Horizontal if waiting_for_second => &[0, 1],
                _ => &[0, 1, 2, 3],
            };
            let handle = visible
                .iter()
                .filter_map(|&i| {
                    let distance = points[i].distance(pointer);
                    (distance <= 12.0).then_some((i, distance))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i);
            if line_mode && (waiting_for_second || handle.is_none()) {
                let (fx, fy) = to_frame(pointer);
                let start = frame.frame_to_guide(fx, fy);
                if waiting_for_second {
                    grabbed = Some(DRAW_SECOND_LINE);
                    match tab.params.composition.keystone.mode {
                        KeystoneMode::Vertical => {
                            tab.params.composition.keystone.guides[1] = start;
                            tab.params.composition.keystone.guides[2] = start;
                        }
                        KeystoneMode::Horizontal => {
                            tab.params.composition.keystone.guides[3] = start;
                            tab.params.composition.keystone.guides[2] = start;
                        }
                        _ => {}
                    }
                } else {
                    grabbed = Some(DRAW_FIRST_LINE);
                    tab.params.composition.keystone.mode = KeystoneMode::Vertical;
                    tab.params.composition.keystone.guides = KeystoneParams::TARGETS;
                    tab.params.composition.keystone.guides[0] = start;
                    tab.params.composition.keystone.guides[3] = start;
                    tab.params.composition.keystone.correction = 0.0;
                }
            } else {
                grabbed = handle;
            }
        }
        if down && let (Some(i), Some(pointer)) = (grabbed, pointer) {
            let (fx, fy) = to_frame(pointer);
            let next = frame.frame_to_guide(fx, fy);
            match i {
                0..=3 => tab.params.composition.keystone.move_guide(i, next),
                DRAW_FIRST_LINE => {
                    let start = tab.params.composition.keystone.guides[0];
                    let (sx, sy) = frame.guide_to_frame(start);
                    let start_screen = to_screen(sx, sy);
                    let horizontal =
                        (pointer.x - start_screen.x).abs() >= (pointer.y - start_screen.y).abs();
                    tab.params.composition.keystone.guides = KeystoneParams::TARGETS;
                    tab.params.composition.keystone.guides[0] = start;
                    if horizontal {
                        tab.params.composition.keystone.mode = KeystoneMode::Horizontal;
                        tab.params.composition.keystone.guides[1] = next;
                    } else {
                        tab.params.composition.keystone.mode = KeystoneMode::Vertical;
                        tab.params.composition.keystone.guides[3] = next;
                    }
                }
                DRAW_SECOND_LINE => {
                    tab.params.composition.keystone.guides[2] = next;
                }
                _ => {}
            }
            ctx.request_repaint();
        }
        let released = (!down).then_some(grabbed).flatten();
        let mut refit = false;
        let mut commit_line_pair = false;
        match released {
            Some(DRAW_FIRST_LINE) => {
                let points = screen_points(tab);
                let length = match tab.params.composition.keystone.mode {
                    KeystoneMode::Vertical => points[0].distance(points[3]),
                    KeystoneMode::Horizontal => points[0].distance(points[1]),
                    _ => 0.0,
                };
                if length < 8.0 {
                    tab.params.composition.keystone.mode = KeystoneMode::Vertical;
                    tab.params.composition.keystone.guides = KeystoneParams::TARGETS;
                }
            }
            Some(DRAW_SECOND_LINE) => {
                let mode = tab.params.composition.keystone.mode;
                let guides = tab.params.composition.keystone.guides;
                let (first, second) = match mode {
                    KeystoneMode::Vertical => ([guides[0], guides[3]], [guides[1], guides[2]]),
                    KeystoneMode::Horizontal => ([guides[0], guides[1]], [guides[3], guides[2]]),
                    _ => ([guides[0], guides[1]], [guides[3], guides[2]]),
                };
                if tab
                    .params
                    .composition
                    .keystone
                    .set_drawn_lines(mode, first, second)
                {
                    tab.params.composition.keystone.correction = 0.8;
                    refit = true;
                    commit_line_pair = true;
                } else {
                    match mode {
                        KeystoneMode::Vertical => {
                            tab.params.composition.keystone.guides[1] = KeystoneParams::TARGETS[1];
                            tab.params.composition.keystone.guides[2] = KeystoneParams::TARGETS[2];
                        }
                        KeystoneMode::Horizontal => {
                            tab.params.composition.keystone.guides[3] = KeystoneParams::TARGETS[3];
                            tab.params.composition.keystone.guides[2] = KeystoneParams::TARGETS[2];
                        }
                        _ => {}
                    }
                }
            }
            Some(0..=3) => refit = tab.params.composition.keystone.is_active(),
            _ => {}
        }
        if !down {
            grabbed = None;
        }
        tab.mode = tabs::Mode::Keystone { grabbed, entered };

        if refit && let Some(frame) = tab.stored_frame() {
            let crop_kind = tab.params.composition.keystone.crop;
            tab.params.composition.crop = frame.keystone_crop(crop_kind);
            tab.params.composition.ratio = match crop_kind {
                KeystoneCrop::Largest => raw_core::Ratio::Free,
                KeystoneCrop::Original => raw_core::Ratio::Original,
            };
        }
        if commit_line_pair {
            // A two-line gesture is complete the moment its second measurement is
            // released. Apply it immediately, like ACR, instead of making Return a
            // third step after the photographer has already supplied both lines.
            // Rectangle stays open because its four corners remain an editing tool.
            tab.mode = tabs::Mode::View;
            ctx.request_repaint();
            return;
        }

        let points = screen_points(tab);
        let painter = ui.painter_at(rect);
        let stroke = egui::Stroke::new(1.25, theme::RUBY);
        let faint = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(220, 42, 52, 150));
        let mode = tab.params.composition.keystone.mode;
        let first_drawn = tab.params.composition.keystone.guides != KeystoneParams::TARGETS;
        let second_drawn = tab.params.composition.keystone.correction > f32::EPSILON
            || grabbed == Some(DRAW_SECOND_LINE);
        let extended_line = |a: egui::Pos2, b: egui::Pos2| {
            let delta = b - a;
            if delta.length_sq() < 1.0 {
                return;
            }
            let direction = delta.normalized();
            let reach = rect.size().length() * 2.0;
            painter.add(egui::Shape::dashed_line(
                &[a - direction * reach, b + direction * reach],
                egui::Stroke::new(1.0, egui::Color32::from_gray(135)),
                4.0,
                5.0,
            ));
            painter.line_segment([a, b], stroke);
        };
        match mode {
            KeystoneMode::Vertical => {
                if first_drawn {
                    extended_line(points[0], points[3]);
                }
                if second_drawn {
                    extended_line(points[1], points[2]);
                }
            }
            KeystoneMode::Horizontal => {
                if first_drawn {
                    extended_line(points[0], points[1]);
                }
                if second_drawn {
                    extended_line(points[3], points[2]);
                }
            }
            KeystoneMode::Rectangle => {
                for i in 0..4 {
                    painter.line_segment([points[i], points[(i + 1) % 4]], faint);
                }
            }
            KeystoneMode::Off => {}
        }
        let visible: Vec<usize> = match mode {
            KeystoneMode::Rectangle => vec![0, 1, 2, 3],
            KeystoneMode::Vertical if first_drawn && second_drawn => vec![0, 1, 2, 3],
            KeystoneMode::Vertical if first_drawn => vec![0, 3],
            KeystoneMode::Horizontal if first_drawn && second_drawn => vec![0, 1, 2, 3],
            KeystoneMode::Horizontal if first_drawn => vec![0, 1],
            _ => Vec::new(),
        };
        let hovered = pointer.and_then(|pointer| {
            visible
                .iter()
                .copied()
                .find(|&i| points[i].distance(pointer) <= 12.0)
        });
        for i in visible {
            let point = points[i];
            let active = grabbed == Some(i) || hovered == Some(i);
            painter.circle_filled(
                point,
                if active { 5.0 } else { 4.0 },
                if active { theme::RUBY } else { theme::CHROME },
            );
            painter.circle_stroke(point, if active { 5.0 } else { 4.0 }, stroke);
        }
        ctx.set_cursor_icon(if hovered.is_some() || grabbed.is_some_and(|i| i < 4) {
            egui::CursorIcon::Grab
        } else {
            egui::CursorIcon::Crosshair
        });
    }
}

/// The viewport with no tab open: the app's name, the two ways in, and the two
/// references that make the rest of the interface discoverable.
///
/// **the maintainer's wording, and the shape of it is the point.** What was there was one line
/// of status text — "drop a raw file on the window, or pass one on the command line" —
/// repeated verbatim in the footer two inches below, which made the emptiest screen in
/// the app the one that said the same thing twice. And it named the *command line*,
/// which is not a route anybody takes twice; the menu is, and the menu was the one it
/// did not mention.
///
/// The name first, because an application with nothing open should say what it is. It
/// is set in the heading size and left dim: this is a title card, not a splash screen,
/// and the instruction under it is the part that is being read. Settings and the
/// Hotkey HUD sit one line below as a quiet quick reference rather than competing
/// with the primary action.
fn welcome(ui: &mut egui::Ui) {
    let chord = |action| {
        hotkeys::TABLE
            .iter()
            .find(|binding| binding.action == action)
            .map(hotkeys::Binding::chord)
            .unwrap_or_default()
    };
    let settings = chord(hotkeys::Action::Settings);
    let hud = chord(hotkeys::Action::HotkeyHud);

    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            // Pushed down by a third rather than centred on the pane. Optical centre:
            // a two-line block sitting on the exact middle reads as low, and this pane
            // is tall.
            ui.add_space((ui.available_height() * 0.38).max(0.0));
            ui.label(egui::RichText::new("monopro").heading().color(theme::DIM));
            ui.add_space(9.0);
            ui.label(theme::caption("File > Open | ⌘ O"));
            ui.add_space(5.0);
            ui.label(theme::caption("or Drag & Drop"));
            ui.add_space(18.0);
            ui.label(theme::caption(format!("Press {settings} for Settings")));
            ui.add_space(5.0);
            ui.label(theme::caption(format!("Press {hud} for the Hotkey HUD")));
        });
    });
}

/// The bearing of `p` from `centre`, in degrees, increasing **clockwise**.
///
/// Clockwise because the screen's y axis points down, so a bare `atan2(dy, dx)`
/// already turns that way — and because it has to agree with `straighten`, which is
/// degrees clockwise. Two angle conventions in one gesture is how a rotate drag ends
/// up going the wrong way.
fn bearing(centre: egui::Pos2, p: egui::Pos2) -> f32 {
    let d = p - centre;
    d.y.atan2(d.x).to_degrees()
}

/// The zone a live grab corresponds to, for the cursor.
fn zone_of(g: tabs::Grab) -> crop::Zone {
    match g {
        tabs::Grab::Grip(h) => crop::Zone::Grip(h),
        tabs::Grab::Rotate { .. } => crop::Zone::Rotate(tabs::Handle::NW),
        tabs::Grab::Line { .. } => crop::Zone::Outside,
    }
}

/// Draw a colour reference view, with the same pan and zoom as the render.
///
/// Deliberately not through `raw-gpu`: this never becomes a node, never gets a
/// scene-referred value, and nothing downstream can reach it. It is drawn the way an
/// icon is drawn.
///
/// The mount and the diagnostic overlays do not apply here and are not drawn. Both
/// are judgements about the monochrome rendering, and neither means anything against
/// the camera's own JPEG.
fn draw_reference(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    tab: &mut Tab,
    img: &raw_core::preview::Rgb8,
    sample: settings::SampleArea,
) -> Option<Readout> {
    let handle = tab.preview_texture.get_or_insert_with(|| {
        let colour = egui::ColorImage::from_rgb([img.w, img.h], &img.data);
        ctx.load_texture("preview", colour, egui::TextureOptions::LINEAR)
    });
    let id = handle.id();

    let avail = ui.available_size();
    let ppp = ctx.pixels_per_point();
    let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

    // The reference is half resolution for the raw view and whatever size the
    // camera wrote for the JPEG, so its own dimensions drive the fit rather than
    // the working image's — otherwise the two views would not frame the same
    // picture.
    // It still uses the developed viewer's fit margin. Without that shared inset,
    // toggling the reference also changed the picture's apparent size and welded its
    // tight axis to the viewport edge.
    let out = egui::vec2(avail.x * ppp, avail.y * ppp);
    let s = fit_scale(
        out,
        egui::vec2(img.w as f32, img.h as f32),
        VIEW_FIT_MARGIN * ppp,
    );
    let size = egui::vec2(img.w as f32 * s, img.h as f32 * s) / ppp;
    let at = rect.center() - size * 0.5;

    let shown = egui::Rect::from_min_size(at, size);
    ui.painter().image(
        id,
        shown,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    // **The reference's own pixels, under the pointer.** The mapping is the inverse
    // of the fit above and nothing else: this view has no pan, no zoom and no
    // composition — it is the file as the camera or the sensor has it, drawn whole —
    // so there is no `frame` transform to undo, which is the step the print path
    // needs and this one cannot want.
    let p = ctx.input(|i| i.pointer.latest_pos())?;
    if !shown.contains(p) {
        return None;
    }
    let u = (p.x - shown.left()) / shown.width();
    let v = (p.y - shown.top()) / shown.height();
    sample_rgb(img, u * img.w as f32, v * img.h as f32, sample)
        .map(|rgb| Readout::Reference { rgb })
}

/// The reference view's colour at a pixel, averaged over the configured window.
///
/// **Averaged in the encoding, which is what the number means here.** `sample_luma`
/// averages in linear scene space because it is measuring light; this is reporting
/// what a colour picker over the displayed image would report, so it averages the
/// bytes — the same thing Photoshop's "3 by 3 average" does over a document.
///
/// The divisor is what was read rather than the window's area, so a sample at the
/// edge of the frame is not darkened by the part of the window that fell outside it.
/// Same rule as `sample_luma`, and for the same reason.
fn sample_rgb(
    img: &raw_core::preview::Rgb8,
    x: f32,
    y: f32,
    area: settings::SampleArea,
) -> Option<[u8; 3]> {
    let (w, h) = (img.w as i32, img.h as i32);
    let (cx, cy) = (x.floor() as i32, y.floor() as i32);
    if cx < 0 || cy < 0 || cx >= w || cy >= h {
        return None;
    }
    let r = area.radius();
    let (mut sum, mut n) = ([0u32; 3], 0u32);
    for yy in (cy - r).max(0)..=(cy + r).min(h - 1) {
        for xx in (cx - r).max(0)..=(cx + r).min(w - 1) {
            let i = (yy as usize * img.w + xx as usize) * 3;
            for (c, total) in sum.iter_mut().enumerate() {
                *total += u32::from(img.data[i + c]);
            }
            n += 1;
        }
    }
    (n > 0).then(|| std::array::from_fn(|c| (sum[c] / n) as u8))
}

/// Which of the two files an export is producing.
///
/// **One code path, two destinations.** The alternative — a second `export` for
/// proofs — would have duplicated the render, the dialog, the folder fallback and the
/// worker handoff, and every fix to one of those would then have had a twin waiting to
/// be forgotten. What actually differs is three things, and each is a line: which
/// target, whether the limits are checked, and whether the resample is the master's or
/// the proof's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Master,
    Proof,
}

impl ExportKind {
    fn is_proof(self) -> bool {
        matches!(self, Self::Proof)
    }
}

/// What the Info panel's EXPORT block was asked to do.
#[must_use = "the buttons report through this; drop it and the EXPORT block is dead"]
#[derive(Default)]
struct InfoClicks {
    export: bool,
    proof: bool,
    settings: bool,
}

/// What the Info panel needs from the app that is not on the tab.
///
/// Read into a value **before** the panel body runs, rather than passing `&App`: the
/// body takes `&mut Tab` out of `self.tabs`, and a second borrow of `self` for the
/// settings would be the borrow error that pushes this back to reading the tab
/// immutably and losing the pin state.
#[derive(Clone, Copy)]
struct InfoEnv {
    /// The depth the Inspector's values will be written at, from the export target.
    depth: export::Depth,
    /// How wide a window the readout averages. See [`settings::SampleArea`].
    sample: settings::SampleArea,
    /// What `Export Proof` will write. Named on the button's tooltip so the
    /// preference is visible from the control it governs.
    proof: export::Target,
    proof_scale: export::ProofScale,
    /// Inches or centimetres, for PIPELINE's print line. See [`settings::Settings`]'s
    /// `print_unit`: a property of the person rather than of the picture, so the panel
    /// reads it from settings rather than from `Params`.
    ///
    /// PIPELINE is a single running account of what will happen, so it quotes one
    /// number in the unit you chose.
    unit: Unit,
    /// An export is in flight; the primary button shows a spinner beside it.
    exporting: bool,
}

/// One `key   value` line, in the EXIF/PIPELINE two-tone face.
///
/// **Key dim and small, value in the readout face** — the prototype's split, and it
/// is what makes a column of fifteen rows scannable: the eye runs down the values
/// and the keys stay out of the way. A single `format!("{k:<9}{v}")` string, which is
/// what this used to be, sets both in one colour and turns the block into a wall.
pub(crate) fn info_row(ui: &mut egui::Ui, key: &str, value: Option<String>) {
    ui.label(
        theme::readout(key)
            .size(theme::size::CAPTION)
            .color(theme::DIM),
    );
    // **An absent value is an em-dash, not a missing row.** A row that vanished when
    // the file did not record it would make the block change height between two
    // frames off the same camera, and "this file does not say" is itself worth
    // reporting — see `sensor::measured`.
    ui.label(theme::readout(value.unwrap_or_else(|| "—".into())));
    ui.end_row();
}

/// The Info panel's body: what you have measured, what the app is doing, and what
/// leaves. File facts now have one home, Lightbox Metadata.
///
/// # The order is INSPECTOR · PIPELINE · EXPORT
///
/// # PIPELINE is the honesty panel, and it is now three stanzas
///
/// It reads the chain that actually runs, so a demosaic-then-convert path creeping
/// back in would show up here first — which is the whole reason it is worth a
/// permanent home rather than a debug print. Under CAPTURE the chain reads
/// `photosites → sampling → luminance`, in that order; the day it reads `→ RGB →`
/// anywhere above the luminance step, the regression is on screen.
///
/// It used to be a chain string plus six rows of dimensions, and the maintainer read it as
/// unhelpful. The cause was structural rather than cosmetic: **it mixed three
/// destinations in one list.** `source` and `working` are about the negative,
/// `output` is about what is on screen, `print` and `file` are about what leaves. The
/// stanzas say so, which is the "capture to screen to print" he asked for.
///
/// **The prototype's own line — `Leica M10-R RAW → Linear Rec2020` — is precisely the
/// architecture the rewrite exists to escape**, and is not ported. See
/// `docs/ux-inventory.md`.
///
/// # Why this is three functions and not one
///
/// The Inspector takes `&mut Tab` — it places and moves pins — and the other two
/// only read. One function holding `&tab.image` across the whole body cannot then
/// hand out a mutable borrow in the middle of it. Splitting at the section boundary
/// is not a workaround: **a section is the natural unit here**, each one answers a
/// different question, and the borrow checker is pointing at the same seam the panel
/// is already divided along.
fn info_body(tab: &mut Tab, ui: &mut egui::Ui, env: InfoEnv, icons: &icons::Icons) -> InfoClicks {
    if tab.image.is_none() || tab.luma.is_none() {
        widgets::Plain::new("INSPECTOR").show(ui, |ui| {
            layout::empty_state(ui, "no image open");
        });
        return InfoClicks::default();
    }
    inspector_section(tab, ui, env, icons);
    pipeline_section(tab, ui, env);
    export_section(tab, ui, env)
}

/// What is actually happening, in three stanzas: capture, screen, print.
fn pipeline_section(tab: &Tab, ui: &mut egui::Ui, env: InfoEnv) {
    let (Some(img), Some(luma)) = (&tab.image, &tab.luma) else {
        return;
    };
    let p = &tab.params;
    let source = luma.source_dims;
    let frame = tab.stored_frame();
    let out = frame.map(|f| f.output_dims()).unwrap_or(luma.output_dims);
    let o = &p.output;

    widgets::Plain::new("PIPELINE").show(ui, |ui| {
        let stanza = |ui: &mut egui::Ui, name: &str| {
            ui.label(
                theme::readout(name)
                    .size(theme::size::CAPTION)
                    .color(theme::DIM),
            );
        };
        // **Caption, matching the stanza names above them.** These used to be set at
        // the readout's default body size, which made every line of this panel a step
        // larger than the `CAPTURE` / `SCREEN` / `PRINT` markers heading them — the
        // block read as three tiny labels interrupting a column of body text rather
        // than as three stanzas. the maintainer called it; one size for the whole panel is also
        // the rule the rest of the app follows. See `docs/decisions.md`.
        let line = |ui: &mut egui::Ui, v: String| {
            ui.label(theme::readout(v).size(theme::size::CAPTION));
        };

        // ── CAPTURE ──────────────────────────────────────────────────────────
        // The chain, in the order it runs. `photosites → sampling → luminance`,
        // never `→ RGB →`.
        stanza(ui, "CAPTURE");
        line(
            ui,
            format!("{}  ·  CFA, gain-equalized", img.decoded.scene.camera),
        );
        line(
            ui,
            format!(
                "{} × {} photosites  →  {}",
                source.w,
                source.h,
                sampling_label(p.luminance.sampling)
            ),
        );
        line(
            ui,
            format!(
                "{} luminance  →  one channel, scene-linear",
                p.luminance.weighting.name()
            ),
        );
        // The grid the tone chain runs on, named only when it differs from the
        // picture — which is exactly when leaving it out would be a lie. Contrast
        // Mask's spacer is a percentage of this frame's diagonal.
        if let Some(f) = &frame
            && (!f.is_uncropped() || f.frame != luma.output_dims)
        {
            line(ui, format!("{} × {} working frame", f.frame.w, f.frame.h));
        }

        // ── SCREEN ───────────────────────────────────────────────────────────
        ui.add_space(6.0);
        stanza(ui, "SCREEN");
        line(
            ui,
            format!(
                "curve {}  →  {}",
                if p.curve.is_identity() {
                    "linear"
                } else {
                    "custom"
                },
                p.display.tone_map.label(),
            ),
        );
        line(
            ui,
            format!(
                "{} × {} px  ({:.1} MP)",
                out.w,
                out.h,
                (out.w * out.h) as f32 / 1.0e6
            ),
        );
        // **Toning is a SCREEN line, not a PRINT one**, which is the only placement
        // that keeps this panel honest. It runs *inside the display pass*, right after
        // the tone map — see `raw_gpu::exec` — so unlike grain and sharpening it is on
        // the viewport in front of you. Filing it under PRINT would say the preview is
        // not showing you the toner, and the preview is the whole reason the module
        // renders on the GPU at all.
        //
        // `is_active`, not `enabled`: the export gates on that too, and a process left
        // at its defaults with every bath at zero changes no pixel. A line claiming
        // otherwise is exactly the kind of thing this panel exists to catch.
        if p.toning.is_active() {
            line(
                ui,
                format!("{} toning  →  after the tone map", p.toning.process.label()),
            );
        }

        // ── PRINT ────────────────────────────────────────────────────────────
        ui.add_space(6.0);
        stanza(ui, "PRINT");
        line(ui, format!("{} Gray  ·  monostar", env.depth.label()));
        let d = o.target_dims(out);
        // **In or cm, whichever the settings say.** Sizes are canonically inches
        // everywhere below the UI — see `raw_core::Unit` on why — so this converts at
        // the point of display and stores nothing. It used to be hardcoded `in`, which
        // meant switching the preference moved every print size in the app except this
        // one, and the panel that exists to report what will happen was the last place
        // still quoting the other unit.
        let u = env.unit;
        let (pw, ph) = o.print_inches(out);
        let (pw, ph) = (u.from_inches(pw), u.from_inches(ph));
        line(
            ui,
            format!(
                "{} × {} px  ·  {:.0} ppi  →  {pw:.1} × {ph:.1} {}",
                d.w,
                d.h,
                o.ppi,
                u.label()
            ),
        );
        if o.resamples(out) {
            line(ui, o.scale_note(out));
        }
        // Grain and sharpening are what the print gets that the screen preview does
        // not — both run on the CPU at export and neither reaches the viewport. They
        // are listed in the order the export tail applies them, which is grain first
        // and sharpening last, over everything including the grain. See `export.rs`.
        //
        // Toning belongs to this trio chemically and is deliberately *not* here: it is
        // in SCREEN above, because it is the one of the three you can already see.
        let mut effects: Vec<&str> = Vec::new();
        if p.grain.is_active() {
            effects.push("Grain");
        }
        if p.sharpen.is_active() {
            effects.push("Sharpen");
        }
        line(
            ui,
            if effects.is_empty() {
                "—".to_owned()
            } else {
                effects.join("  ·  ")
            },
        );
    });
}

/// What leaves: two exports and the preferences that shape them.
fn export_section(tab: &Tab, ui: &mut egui::Ui, env: InfoEnv) -> InfoClicks {
    let mut clicks = InfoClicks::default();
    widgets::Plain::new("EXPORT").show(ui, |ui| {
        // **Full width and stacked**, which is the Snapshot/Compare pair turned
        // through ninety degrees: same primitive, same two grounds, and the width
        // argument that exists so a pair comes out equal is doing the same job down
        // a column. See `theme::wide_button` and `App::snapshot_bench`.
        //
        // **Each row zeroes its horizontal item spacing**, and that is the fix for
        // the maintainer's report that these would not shrink and were clipped at startup.
        //
        // `widgets::draw` calls `ui.set_width(content_w)` on a module's body, so the
        // arithmetic here has to come out at or under that width *exactly*. It did
        // not: `PAD + w` accounted for the inset but not for the default
        // `item_spacing.x` egui inserts between the space and the button, so every
        // row asked for a few points more than the box it was in. A `Ui` given more
        // content than its set width grows to fit — so the panel could never get
        // smaller, and on the first frame the overflow showed as a clipped button.
        //
        // The Dodge & Burn bench never had it because it sets `item_spacing.x`
        // itself and folds the gap into the sum. Same discipline here, with the gap
        // at zero because these buttons are stacked and there is nothing beside them.
        const PAD: f32 = 9.0;
        let w = (ui.available_width() - 2.0 * PAD).max(40.0);
        // **Both export buttons stand down while one is in flight.** `export_rx`
        // already refuses a second request, but refusing it silently is a button that
        // looks live and does nothing — and this also retires the spinner that used
        // to sit beside the first button and widen its row unpredictably.
        let ready = tab.has_image() && !env.exporting;
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(PAD);
                clicks.export |= theme::wide_button(
                    ui,
                    &format!("Export {} TIFF", env.depth.label()),
                    theme::RUBY_FILL,
                    theme::RUBY,
                    w,
                    ready,
                )
                .on_hover_text(theme::tip(if env.exporting {
                    "an export is already being written"
                } else {
                    "Full resolution grayscale, L*-encoded and tagged monostar.icc.  ⌘E"
                }))
                .clicked();
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(PAD);
                // **No file type on the label**, because the proof's format and size
                // are preferences. A button that named one would have to be
                // relabelled by a setting, and would be wrong the moment it changed.
                clicks.proof |= theme::wide_button(
                    ui,
                    "Export Proof",
                    theme::RUBY_FILL_DIM,
                    theme::RUBY,
                    w,
                    ready,
                )
                .on_hover_text(theme::tip(format!(
                    "{} · {} · {}. Set in Settings → Export.  ⌘⇧E",
                    env.proof.container.label(),
                    env.proof_scale.label(),
                    env.proof.space.label(),
                )))
                .clicked();
            });
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(PAD);
                // The reset face at panel width. `theme::reset_button` cannot do this
                // — it sizes to its text — so it is `wide_button` with a grey ground,
                // which keeps the third button in the same primitive as the two above.
                clicks.settings |=
                    theme::wide_button(ui, "Settings", theme::CHROME, theme::DIM, w, true)
                        // **`,` on its own, not `⌘,`.** The binding is a bare comma — see
                        // `hotkeys::TABLE` — and it is the one key the table refuses to let
                        // you disable, because it is the way back into the window that
                        // disables things.
                        .on_hover_text(theme::tip(
                            "Export defaults, output folder and print resolution.  ,",
                        ))
                        .clicked();
            });
        });
    });

    clicks
}

/// The INSPECTOR section: the pin controls, what is being read, and the pins.
///
/// **Named INSPECTOR, and the marks it places are still pins.** the maintainer's rename, and
/// the distinction is deliberate: the module is the thing you look at values with,
/// the pin is the mark you leave to keep looking at one. `i` and `⇧I` therefore keep
/// saying "pin", and the panel heading does not.
///
/// # A pin shows one number, and a second only once there is one
///
/// L\*, and nothing beside it while the frame is untoned — **the pipeline is one
/// channel**, so the prototype's second line of RGB values has nothing to put in it.
/// Reserving three columns and two dashes for it would have been building to a layout
/// that is not going to be used.
///
/// the maintainer's decision was that the colour readout would be *rethought* rather than
/// ported from the prototype, and toning is where that came due: `pin_lab` adds `a*`
/// and `b*` on a toned frame, which is two numbers and not three, and only on
/// `EDITED` — see its own note for why the negative does not get a colour.
///
/// `RAW` switches the number to EV — the scene value entering the tone chain, in
/// stops relative to clipping — which is the honest one-channel answer to "what is
/// this before the curve gets it".
fn inspector_section(tab: &mut Tab, ui: &mut egui::Ui, env: InfoEnv, icons: &icons::Icons) {
    widgets::Plain::new("INSPECTOR").show(ui, |ui| {
        // With EXIF moved to Lightbox, Inspector becomes Info's primary working
        // area. Keep 380 points available for its controls and pin list; content can
        // still grow beyond this when several toned pins need three readout lines.
        ui.set_min_height(380.0);
        let placing = matches!(tab.mode, tabs::Mode::Pin);
        let mut clear = false;

        // ── The control row ──────────────────────────────────────────────────
        ui.horizontal(|ui| {
            // `+` is the mode toggle, and it wears its state the way every other
            // tool-that-is-open does. It is the pointer-driven twin of `i`, which is
            // the rule the crop tool's eye and the loupe's already follow: a mode
            // must be reachable without the keyboard.
            if theme::bracket(ui, "+", placing, theme::size::SECTION)
                .on_hover_text(theme::tip("Place value pins  ·  i"))
                .clicked()
            {
                tab.mode = if placing {
                    tabs::Mode::View
                } else {
                    tabs::Mode::Pin
                };
                if !placing {
                    tab.pins.hidden = false;
                }
            }
            ui.add_space(6.0);
            // EDITED / RAW — one of the two, always. Exclusive by construction
            // rather than by a group widget: there are two states and a bool holds
            // them, so there is nothing to keep in step.
            if theme::bracket(ui, "EDITED", !tab.pins.show_raw, theme::size::CAPTION)
                .on_hover_text(theme::tip(
                    "Developed image lightness, including spatial adjustments and toning; export-size grain and sharpening are excluded",
                ))
                .clicked()
            {
                tab.pins.show_raw = false;
            }
            if theme::bracket(ui, "RAW", tab.pins.show_raw, theme::size::CAPTION)
                .on_hover_text(theme::tip(
                    "Undeveloped image lightness from the working luminance",
                ))
                .clicked()
            {
                tab.pins.show_raw = true;
            }
            ui.add_space(6.0);
            // **HIDE and clear are different actions and look it.** Hiding keeps the
            // pins and is what you want while judging a print; clearing throws them
            // away. The destructive one is the quiet text link, not the outlined
            // button — the same ranking the quit sheet uses.
            if theme::bracket(ui, "HIDE", tab.pins.hidden, theme::size::CAPTION)
                .on_hover_text(theme::tip("Keep the pins, stop drawing them  ·  ⇧I"))
                .clicked()
            {
                tab.pins.hidden = !tab.pins.hidden;
            }
            if !tab.pins.items.is_empty() {
                clear = theme::reset_button(ui, "clear", "Remove every pin").clicked();
            }
        });

        // ── What is being read ───────────────────────────────────────────────
        //
        // Three facts, because the number means nothing without them: which stage,
        // at what precision, over how wide a window. **The sample area is here
        // because its control is not** — it lives in Settings (the maintainer's call), and a
        // readout whose sample area you cannot see is a number you cannot compare
        // against another number.
        ui.label(
            theme::caption(format!(
                "{}  ·  L  ·  {}  ·  {}",
                if tab.pins.show_raw { "RAW" } else { "EDITED" },
                env.depth.label(),
                env.sample.short(),
            ))
            .color(theme::DIM),
        );
        ui.add_space(4.0);

        // ── The pins ─────────────────────────────────────────────────────────
        if tab.pins.items.is_empty() {
            // The prototype's empty state, and it is a good one: it teaches the whole
            // interaction in five lines rather than hiding it in a tooltip. Every one
            // of these is a gesture nothing on screen would otherwise suggest.
            // **Single newlines on an explicit line height, not blank lines.** This
            // was `\n\n` throughout, which puts a whole empty row between each hint —
            // at caption size that is roughly double spacing, and the five lines read
            // as five separate paragraphs floating in the panel rather than as one
            // list. the maintainer saw it as the Inspector being too spaced apart, and it is the
            // panel's default state so it is what the panel looks like most of the
            // time.
            //
            // `HINTS` is above the natural row height rather than at it: the lines are
            // five *different* gestures and want to stay countable, so they need some
            // air — just not a blank line's worth of it.
            const HINTS: f32 = 16.0;
            ui.label(
                theme::caption(
                    "Press + or 'i' to add value pins\n\
                     Click image to sample\n\
                     Drag to reposition\n\
                     Shift+click to delete\n\
                     Shift+i to hide",
                )
                .line_height(Some(HINTS)),
            );
        } else {
            let mut remove = None;
            for i in 0..tab.pins.items.len() {
                let pin = tab.pins.items[i];
                let colour = tabs::pin_colour(i);
                let value = pin_value(tab, pin, env.sample);
                // The chroma this tone came out as, when there is one. Absent for an
                // untoned print, and absent is the honest answer — a pin reporting 0°
                // would be inventing a direction for a colour that has none.
                let lab = pin_lab(tab, pin, env.sample);
                ui.horizontal(|ui| {
                    // The badge carries the number and the colour, which is the whole
                    // of how a row is matched to a mark on the picture.
                    let (badge, _) =
                        ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    ui.painter().circle_filled(badge.center(), 8.0, colour);
                    ui.painter().text(
                        badge.center(),
                        egui::Align2::CENTER_CENTER,
                        (i + 1).to_string(),
                        egui::FontId::new(9.0, egui::FontFamily::Monospace),
                        theme::INK_DARK,
                    );
                    // Keep the readout intrinsic. Giving it the whole middle of the
                    // row made the text look centred between the badge and the close
                    // mark; the measurement belongs beside its numbered badge while
                    // the remaining space belongs before the right-aligned `×`.
                    ui.add(
                        egui::Label::new(
                            theme::readout(match (value, lab) {
                                (Some(v), Some((a, b))) => {
                                    format!("L {v:.1}  a {a:.1}  b {b:.1}")
                                }
                                (Some(v), None) => format!("L {v:.1}"),
                                (None, _) => "—".into(),
                            })
                            .line_height(Some(13.0)),
                        )
                        .halign(egui::Align::LEFT),
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), ui.spacing().interact_size.y),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // **The app's close mark, not a text button.** The snapshot
                            // row and the tab strip both draw this exact control — a
                            // 14pt square that fills grey on hover and paints the
                            // `close` icon — and a third `×` in a third face would be
                            // the drift `docs/decisions.md` exists to stop.
                            let (close_rect, close) = ui
                                .allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                            let hot = close.hovered();
                            if hot {
                                ui.painter().rect_filled(
                                    close_rect,
                                    2.0,
                                    egui::Color32::from_gray(72),
                                );
                            }
                            icons::paint(
                                ui,
                                icons,
                                "close",
                                "×",
                                close_rect,
                                if hot {
                                    egui::Color32::from_gray(245)
                                } else {
                                    theme::DIM
                                },
                            );
                            if close.on_hover_text(theme::tip("delete pin")).clicked() {
                                remove = Some(i);
                            }
                        },
                    );
                });
            }
            if let Some(i) = remove {
                tab.pins.items.remove(i);
            }
        }
        if clear {
            tab.pins.items.clear();
            tab.pins.dragging = None;
        }
    });
}

fn refresh_tab_mapping(tab: &mut Tab) {
    if tab.luma.is_some() {
        tab.histogram.refresh_mapping(&tab.render_params());
    }
}

fn refresh_tab_curve_samples(tab: &mut Tab) {
    let shown = tab.render_params();
    if let (Some(luma), Some(frame)) = (&tab.luma, tab.frame()) {
        tab.histogram
            .refresh_curve_samples(luma, &shown, tab.luma_gen, &frame);
    }
}

/// What one pin reads, as `L*` on 0–100, at whichever stage the toggle is on.
///
/// **One unit for both stages.** `EDITED` is the developed image at working resolution;
/// `RAW` is what the same pixel would print as undeveloped. Both are `L*`, so the
/// two are comparable and their difference is what the developing did — see
/// `Histogram::sample_undeveloped` for why the EV that used to sit here was the
/// wrong number to put beside a lightness.
/// A pin's `a*` and `b*`, once the Chemistry has run.
///
/// **`EDITED` only.** `RAW` is what the pixel would print undeveloped, and toning is
/// something you did to it — so reporting a colour there would attribute the chemistry
/// to the negative.
fn pin_lab(tab: &Tab, pin: tabs::Pin, _sample: settings::SampleArea) -> Option<(f32, f32)> {
    if tab.pins.show_raw {
        return None;
    }
    tab.render
        .as_ref()?
        .samples
        .get(SampleTarget::Pin(pin))?
        .lab
}

fn pin_value(tab: &Tab, pin: tabs::Pin, sample: settings::SampleArea) -> Option<f32> {
    if tab.pins.show_raw {
        let raw = sample_luma(tab.luma.as_ref()?, pin.x, pin.y, sample)?;
        tab.histogram.sample_undeveloped(raw)
    } else {
        tab.render
            .as_ref()?
            .samples
            .get(SampleTarget::Pin(pin))
            .map(|value| value.lstar)
    }
}

/// Which corner one pin's readout sits in, given the boxes already placed.
///
/// Up-and-right by preference, which is the prototype's arrangement and where a pin
/// with room around it stays. When that corner is taken the box **flips** — up-left,
/// then down-right, then down-left — which is the maintainer's answer to labels burying each
/// other as the view zooms out and the crosshairs converge while the boxes do not
/// shrink.
///
/// A free function because it is pure geometry and the thing it guards against is a
/// picture nobody screenshotted: `pin_tool` needs a painter, a GPU tab and a live
/// `egui::Ui`, so the placement rule could otherwise only be checked by looking at it.
///
/// * `taken` — labels already placed this frame. Greedy in index order: an earlier pin
///   keeps its corner and a later one yields, so dragging pin 3 cannot make pin 1's
///   readout jump.
/// * `ats` / `self_i` — every crosshair, and which one belongs to this label. A box
///   over another pin's mark hides the pixel that pin is pointing at, so those count
///   as collisions too.
/// * `view` — the viewport. A corner fully on screen is preferred, but being clear of
///   the other marks outranks it: a box running past the edge is still readable, a box
///   underneath another box is not.
fn pin_label_box(
    at: egui::Pos2,
    sz: egui::Vec2,
    off: f32,
    taken: &[egui::Rect],
    ats: &[egui::Pos2],
    self_i: usize,
    view: egui::Rect,
) -> egui::Rect {
    let boxes: [egui::Rect; 4] = [
        egui::Rect::from_min_size(at + egui::vec2(off, -off - sz.y), sz),
        egui::Rect::from_min_size(at + egui::vec2(-off - sz.x, -off - sz.y), sz),
        egui::Rect::from_min_size(at + egui::vec2(off, off), sz),
        egui::Rect::from_min_size(at + egui::vec2(-off - sz.x, off), sz),
    ];
    let buried = |b: &egui::Rect| {
        taken.iter().any(|t| t.intersects(*b))
            // `expand` so a box merely *grazing* a mark still counts: a crosshair with
            // a border a pixel away is as unreadable as one underneath it.
            || ats.iter().enumerate().any(|(j, a)| j != self_i && b.expand(2.0).contains(*a))
    };
    let on_screen = |b: &egui::Rect| view.contains_rect(*b);
    boxes
        .iter()
        .find(|b| on_screen(b) && !buried(b))
        .or_else(|| boxes.iter().find(|b| !buried(b)))
        .or_else(|| boxes.iter().find(|b| on_screen(b)))
        .copied()
        // Every corner is both off-screen and buried — four pins stacked on one pixel
        // in a window smaller than a label. Nothing is readable at that point; keep the
        // preferred corner so the box is at least where the rule says it should be.
        .unwrap_or(boxes[0])
}

/// One opaque sRGB colour button with egui's visual picker and CIELAB inputs.
///
/// The popup is shared by FRAME and Surround so "Custom" means the same thing in
/// both places. L*, a* and b* are the app's existing CIELAB readout scale; changing
/// them goes through `srgb_of_lab`, which gamut-fits rather than clipping a channel.
fn colour_picker_lab_button(
    ui: &mut egui::Ui,
    rgb: &mut [u8; 3],
    id_salt: &'static str,
) -> egui::Response {
    let popup_id = ui.make_persistent_id((id_salt, "popup"));
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    let size = egui::vec2(ui.spacing().interact_size.y, ui.spacing().interact_size.y);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    let visuals = if open {
        &ui.visuals().widgets.open
    } else {
        ui.style().interact(&response)
    };
    let painted = rect.expand(visuals.expansion);
    ui.painter().rect_filled(
        painted.shrink(1.0),
        0.0,
        egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]),
    );
    ui.painter().rect_stroke(
        painted,
        0.0,
        egui::Stroke::new(1.0, visuals.bg_fill),
        egui::StrokeKind::Inside,
    );

    let mut changed = false;
    egui::Popup::menu(&response)
        .id(popup_id)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.spacing_mut().slider_width = 245.0;
            let mut color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
            if egui::color_picker::color_picker_color32(
                ui,
                &mut color,
                egui::color_picker::Alpha::Opaque,
            ) {
                *rgb = [color.r(), color.g(), color.b()];
                changed = true;
            }

            ui.add_space(5.0);
            ui.separator();
            ui.add_space(3.0);
            let mut lab = raw_core::colour::lab_of_srgb(*rgb);
            let mut lab_changed = false;
            ui.horizontal(|ui| {
                ui.label(theme::caption("LAB"));
                lab_changed |= ui
                    .add(
                        egui::DragValue::new(&mut lab[0])
                            .range(0.0..=100.0)
                            .speed(0.2)
                            .fixed_decimals(1)
                            .prefix("L* "),
                    )
                    .changed();
                lab_changed |= ui
                    .add(
                        egui::DragValue::new(&mut lab[1])
                            .range(-128.0..=127.0)
                            .speed(0.2)
                            .fixed_decimals(1)
                            .prefix("a* "),
                    )
                    .changed();
                lab_changed |= ui
                    .add(
                        egui::DragValue::new(&mut lab[2])
                            .range(-128.0..=127.0)
                            .speed(0.2)
                            .fixed_decimals(1)
                            .prefix("b* "),
                    )
                    .changed();
            });
            if lab_changed {
                *rgb = raw_core::colour::srgb_of_lab(lab);
                changed = true;
            }
        });
    if changed {
        response.mark_changed();
    }
    response
}

/// The surround's colour: absolute buttons, measured boards, hex, and custom input.
///
/// # OKHSL stays authoritative and everything else is a way of typing it
///
/// `Settings::surround_okhsl` is the stored value and does not change here. What
/// changes is that there are now four ways to reach it — a measured board, an
/// absolute chip, a hex string, and egui's picker — and **three of them speak sRGB**.
/// Each converts through `okhsl::from_srgb` on the way in, which is the split
/// `docs/ui-queue.md` settled: OKHSL authoritative, sRGB as an input and output
/// format. The reason is the one the space was chosen for — moving hue must not
/// change how bright the mount reads — and it survives only if the stored value is
/// the perceptual one.
///
/// A consequence worth expecting rather than debugging: a hex that OKHSL cannot name
/// at that lightness comes back clamped to the gamut boundary, so the field can show
/// a value one or two codes from what was typed. That is `s = 1` doing its job.
///
/// # The wheel is egui's picker, not a wheel
///
/// the maintainer asked for "an egui color picker widget with full color wheel". egui has the
/// widget and it is not a wheel: `color_picker_color32` draws a **saturation/value
/// square with a hue strip**, which is the only picker in the library. A hue wheel
/// would have to be drawn from scratch, and the square is the one every egui
/// application uses. Said here rather than silently substituted.
fn surround_colour(ui: &mut egui::Ui, s: &mut settings::Settings, d: &settings::Settings) {
    use raw_core::okhsl;

    let current = okhsl::to_srgb(okhsl::Okhsl {
        h: s.surround_okhsl[0],
        s: s.surround_okhsl[1],
        l: s.surround_okhsl[2],
    });
    let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let now: [u8; 3] = [to8(current[0]), to8(current[1]), to8(current[2])];
    let canonical_hex = format_hex_rgb(now);
    let hex_id = ui.make_persistent_id("surround-color-hex");
    let mut set: Option<[u8; 3]> = None;
    let mut picker_rgb = now;
    let near = |a: [u8; 3], b: [u8; 3]| a.into_iter().zip(b).all(|(a, b)| a.abs_diff(b) <= 2);
    let rising_name = settings::RISING
        .iter()
        .find(|(_, rgb)| near(*rgb, now))
        .map(|(name, _)| *name)
        .unwrap_or("Rising...");
    let (_, reset) = settings::item(
        ui,
        s.surround_okhsl != d.surround_okhsl,
        "Mount color",
        Some("Rising Museum Board presets or a custom color."),
        |ui| {
            // Settings controls are laid out from the right edge. Add these in the
            // reverse of their visual order so the row reads exactly like FRAME:
            // White, Black, Rising…, picker.
            ui.spacing_mut().item_spacing.x = 3.0;
            if colour_picker_lab_button(ui, &mut picker_rgb, "surround-color-picker").changed() {
                set = Some(picker_rgb);
            }
            egui::ComboBox::from_id_salt("surround-preset")
                .width(96.0)
                .truncate()
                .selected_text(settings::combo_text(rising_name))
                .show_ui(ui, |ui| {
                    settings::combo_menu(ui, |ui| {
                        for (name, rgb) in settings::RISING {
                            if ui.selectable_label(near(rgb, now), name).clicked() {
                                set = Some(rgb);
                            }
                        }
                    })
                });
            for (name, rgb) in settings::ABSOLUTE.into_iter().rev() {
                let short = if name == "100% White" {
                    "White"
                } else {
                    "Black"
                };
                if theme::bracket(ui, short, near(rgb, now), theme::size::CAPTION).clicked() {
                    set = Some(rgb);
                }
            }
        },
    );
    if reset {
        let c = okhsl::to_srgb(okhsl::Okhsl {
            h: d.surround_okhsl[0],
            s: d.surround_okhsl[1],
            l: d.surround_okhsl[2],
        });
        set = Some([to8(c[0]), to8(c[1]), to8(c[2])]);
    }

    settings::rule(ui);
    let was_focused = ui.memory(|memory| memory.has_focus(hex_id));
    let mut hex = ui
        .data_mut(|data| data.get_temp::<String>(hex_id))
        .unwrap_or_else(|| canonical_hex.clone());
    if !was_focused {
        hex.clone_from(&canonical_hex);
    }
    settings::item(ui, false, "Hex", Some("sRGB · #RRGGBB"), |ui| {
        let valid = parse_hex_rgb(&hex).is_some();
        let response = ui.add(
            egui::TextEdit::singleline(&mut hex)
                .id(hex_id)
                .char_limit(7)
                .desired_width(92.0)
                .vertical_align(egui::Align::Center)
                .text_color(if valid { theme::BRIGHT } else { theme::RUBY }),
        );
        let enter = response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if enter {
            ui.memory_mut(|memory| memory.surrender_focus(response.id));
        }
        if response.lost_focus() || enter {
            if let Some(rgb) = parse_hex_rgb(&hex) {
                set = Some(rgb);
                hex = format_hex_rgb(rgb);
            } else {
                hex.clone_from(&canonical_hex);
            }
        }
    });
    ui.data_mut(|data| data.insert_temp(hex_id, hex));

    if let Some(rgb) = set {
        let c = okhsl::from_srgb(rgb.map(|v| v as f32 / 255.0));
        s.surround_okhsl = [c.h, c.s, c.l];
        ui.data_mut(|data| data.insert_temp(hex_id, format_hex_rgb(rgb)));
    }
}

fn format_hex_rgb(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

fn parse_hex_rgb(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ])
}

/// A file size as a photographer reads it, which is never in bytes.
fn byte_label(bytes: u64) -> String {
    const MB: f64 = 1_048_576.0;
    let mb = bytes as f64 / MB;
    if mb >= 1000.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{mb:.1} MB")
    }
}

/// The Hotkey HUD occupies most of the viewport while leaving enough of the image
/// visible around it to read unmistakably as an overlay.
fn hotkey_hud_rect(screen: egui::Rect) -> egui::Rect {
    let inset = egui::vec2(
        (screen.width() * 0.045).clamp(28.0, 72.0),
        (screen.height() * 0.055).clamp(28.0, 60.0),
    );
    screen.shrink2(inset)
}

fn settings_hotkey_row(ui: &mut egui::Ui, chord: &str, what: &str, built: bool) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [58.0, 14.0],
            egui::Label::new(egui::RichText::new(chord).monospace()),
        );
        let text = egui::RichText::new(what).size(theme::size::CAPTION);
        ui.label(if built { text } else { text.color(theme::DIM) });
        if !built {
            ui.label(theme::caption("· pending"));
        }
    });
}

fn hotkey_hud_row(ui: &mut egui::Ui, chord: &str, what: &str, built: bool) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(58.0, 17.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.label(
                    egui::RichText::new(chord)
                        .size(theme::size::BODY)
                        .color(theme::BRIGHT)
                        .family(egui::FontFamily::Monospace),
                );
            },
        );
        let text = egui::RichText::new(what)
            .size(theme::size::CAPTION + 1.0)
            .color(if built { theme::NAME } else { theme::DIM });
        ui.add(egui::Label::new(text).wrap());
    });
    ui.add_space(2.0);
}

fn hotkey_hud_group(ui: &mut egui::Ui, group: hotkeys::Group) {
    theme::tracked_at(ui, group.label(), theme::RUBY, theme::size::HEADER);
    ui.add_space(6.0);

    for gesture in hotkeys::reference_gestures(group, true) {
        hotkey_hud_row(ui, gesture.chord, gesture.what, true);
    }
    for (index, binding) in hotkeys::TABLE
        .iter()
        .enumerate()
        .filter(|(_, binding)| binding.group == group)
    {
        // One visible row per action. Zoom in has two physical bindings because
        // egui names `+` differently with Shift held; the HUD should describe the
        // command once, not expose that implementation detail.
        if hotkeys::TABLE
            .iter()
            .take(index)
            .any(|earlier| earlier.action == binding.action)
        {
            continue;
        }
        hotkey_hud_row(ui, &binding.chord(), binding.what, binding.built);
    }
    for gesture in hotkeys::reference_gestures(group, false) {
        hotkey_hud_row(ui, gesture.chord, gesture.what, true);
    }
    ui.add_space(14.0);
}

fn hotkey_hud(ctx: &egui::Context) {
    let screen = ctx.content_rect();
    let rect = hotkey_hud_rect(screen);
    let content_size = rect.size() - egui::vec2(48.0, 40.0);

    // One modal owns both the veil and the reference. The first version used two
    // foreground Areas; clicking the outer one could raise its black paint above the
    // HUD on the next frame, making everything suddenly darker. A Modal paints its
    // own backdrop first and content second on one layer, so their order cannot flip.
    egui::Modal::new(egui::Id::new("hotkey-hud"))
        .backdrop_color(egui::Color32::from_black_alpha(150))
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(27, 27, 27, 248))
                .stroke(egui::Stroke::new(2.0, theme::RUBY))
                .corner_radius(2.0)
                .inner_margin(egui::Margin::symmetric(24, 20)),
        )
        .show(ctx, |ui| {
            ui.set_min_size(content_size);
            ui.set_max_size(content_size);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("HOTKEYS")
                        .size(18.0)
                        .color(theme::BRIGHT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(theme::caption("Press . or Esc to close"));
                });
            });
            ui.add_space(8.0);
            let y = ui.cursor().top();
            ui.painter().line_segment(
                [
                    egui::pos2(ui.min_rect().left(), y),
                    egui::pos2(ui.max_rect().right(), y),
                ],
                egui::Stroke::new(1.0, theme::RUBY.gamma_multiply(0.65)),
            );
            ui.add_space(14.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let groups: [&[hotkeys::Group]; 4] = [
                        &[hotkeys::Group::View, hotkeys::Group::ValuePins],
                        &[hotkeys::Group::Navigation, hotkeys::Group::Comparison],
                        &[hotkeys::Group::Composition, hotkeys::Group::DodgeBurn],
                        &[
                            hotkeys::Group::Lightbox,
                            hotkeys::Group::Files,
                            hotkeys::Group::Edit,
                        ],
                    ];
                    ui.columns(4, |columns| {
                        for (column, groups) in columns.iter_mut().zip(groups) {
                            column.set_max_width(column.available_width());
                            for group in groups {
                                hotkey_hud_group(column, *group);
                            }
                        }
                    });
                });
        });
}

impl App {
    /// Retry ordinary saves and keep in-memory work available when a write fails.
    fn guard_quit(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if self.export_rx.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_note =
                Some("An export is still being written. Quit again after it finishes.".into());
            return;
        }
        if self.quitting {
            return;
        }
        let ids: Vec<_> = self
            .tabs
            .iter()
            .filter(|t| !t.scratch)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.save_sidecar(id);
        }
        let unsaved: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| t.needs_quit_warning(self.settings.warn_unsaved_duplicates))
            .map(|t| {
                if t.scratch {
                    t.name.clone()
                } else {
                    format!("{} — {}", t.name, t.status)
                }
            })
            .collect();
        if unsaved.is_empty() {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        self.confirm_quit = Some(unsaved);
    }

    /// Saving may fail or be cancelled; only successful saves authorize quitting.
    fn quit_sheet(&mut self, ctx: &egui::Context) {
        let Some(names) = self.confirm_quit.clone() else {
            return;
        };
        let mut close = false;
        let mut save_all = false;
        let mut quit = false;
        let response = egui::Modal::new(egui::Id::new("confirm-quit")).show(ctx, |ui| {
            ui.set_width(360.0);
            theme::tracked(ui, "UNSAVED EDITS", theme::RUBY);
            ui.add_space(6.0);
            ui.label(theme::caption(
                "These edits have not been saved. Retry saving, cancel to keep working, or explicitly discard them:",
            ));
            ui.add_space(6.0);
            for n in &names {
                ui.label(theme::readout(format!("  {n}")));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Save them and quit").clicked() {
                    save_all = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui::RichText::new("Discard and quit").color(theme::RUBY))
                        .clicked()
                    {
                        quit = true;
                    }
                });
            });
        });
        close |= response.should_close();
        if save_all {
            let ids: Vec<_> = self
                .tabs
                .iter()
                .filter(|t| t.needs_quit_warning(self.settings.warn_unsaved_duplicates))
                .map(|t| (t.id, t.scratch))
                .collect();
            for (id, scratch) in ids {
                if scratch {
                    self.save_duplicate(id);
                } else {
                    self.save_sidecar(id);
                }
            }
            quit = !self
                .tabs
                .iter()
                .any(|t| t.needs_quit_warning(self.settings.warn_unsaved_duplicates));
            if !quit {
                self.confirm_quit = Some(
                    self.tabs
                        .iter()
                        .filter(|t| t.needs_quit_warning(self.settings.warn_unsaved_duplicates))
                        .map(|t| format!("{} — {}", t.name, t.status))
                        .collect(),
                );
            }
        }
        if quit {
            self.quitting = true;
            self.confirm_quit = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if close {
            self.confirm_quit = None;
        }
    }

    /// The update sheet, reached from the badge, the menu route or Settings → About.
    ///
    /// **Three choices, in order of caution.** *Update on quit* is the default and
    /// the safest — the staged installer completes after a clean quit, never under
    /// a live session. *Restart now* runs Sparkle's user-initiated install and is
    /// offered only when the update is staged and the machine idle. *Skip this
    /// version* is remembered and shown in Settings → About until the feed moves
    /// past it.
    ///
    /// While an export writes, or the quit confirmation is still unanswered, both
    /// install routes stand down: an install must never race a file being written
    /// or a quit that has not been resolved yet. Skipping stays available — it
    /// writes nothing but preferences.
    fn update_sheet(&mut self, ctx: &egui::Context) {
        if !self.update_sheet_open {
            return;
        }
        // The sheet has a reason to exist while the badge is up or there is a
        // status to report ("up to date", a failure). Otherwise it closes itself.
        let Some(updates) = &self.updates else {
            self.update_sheet_open = false;
            return;
        };
        if !updates.sheet_ready() {
            self.update_sheet_open = false;
            return;
        }
        let (version_line, notes, date, status, failed) = {
            let u = self.updates.as_ref().expect("checked above");
            (
                u.sheet_version_line(),
                u.sheet_notes().to_owned(),
                u.sheet_date().map(str::to_owned),
                u.sheet_status().map(str::to_owned),
                u.badge().is_some_and(|b| b.failed),
            )
        };
        // **Busy is the export thread or an unresolved quit confirmation.** A
        // sidecar write needs no gate here: it is synchronous, settled before the
        // sheet can be interacted with. See `guard_quit` for the quit-time pair.
        let busy = self.export_rx.is_some() || self.confirm_quit.is_some();
        let restart_ready = self
            .updates
            .as_ref()
            .is_some_and(|u| u.restart_now_ready());
        let skipped = updater::Updates::skipped_version(&self.settings).map(str::to_owned);

        let mut close = false;
        let mut update_on_quit = false;
        let mut restart_now = false;
        let mut skip = false;
        let response = egui::Modal::new(egui::Id::new("update-sheet")).show(ctx, |ui| {
            ui.set_width(420.0);
            theme::tracked(ui, "SOFTWARE UPDATE", theme::AMBER);
            ui.add_space(6.0);
            ui.label(theme::readout(version_line.clone()));
            if let Some(d) = &date {
                ui.label(theme::caption(d.clone()));
            }
            if !notes.is_empty() {
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.label(theme::label(notes.clone()));
                    });
            }
            if let Some(line) = &status {
                ui.add_space(6.0);
                ui.label(if failed {
                    theme::caption(line.clone()).color(theme::RUBY)
                } else {
                    theme::caption(line.clone())
                });
            }
            if busy {
                ui.add_space(6.0);
                ui.label(theme::caption(
                    "Wait for the export to finish — and answer the quit prompt if one is \
                     open — before installing.",
                ));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                // The default choice, first and safest. Installs after a clean
                // quit — Sparkle's staged installer does the swap at termination.
                if ui
                    .add_enabled(!busy, egui::Button::new("Update on quit"))
                    .clicked()
                {
                    update_on_quit = true;
                }
                if ui
                    .add_enabled(
                        restart_ready && !busy,
                        egui::Button::new("Restart now"),
                    )
                    .on_disabled_hover_text(if busy {
                        "an export is still being written"
                    } else {
                        "still downloading — it can restart once it is staged"
                    })
                    .clicked()
                {
                    restart_now = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Skip this version").clicked() {
                        skip = true;
                    }
                });
            });
            if let Some(v) = &skipped {
                ui.add_space(4.0);
                ui.label(theme::caption(format!("skipping monopro {v}")));
            }
        });
        if response.should_close() {
            close = true;
        }

        let mut note = None;
        if update_on_quit {
            if let Some(updates) = &mut self.updates {
                updates.choose_update_on_quit();
                note = updates.sheet_status().map(str::to_owned);
            }
            close = true;
        }
        if restart_now {
            close = true;
            if let Some(updates) = &mut self.updates
                && let Some(why) = updates.check_now(&mut self.settings)
            {
                note = Some(why);
            }
        }
        if skip {
            if let Some(updates) = &mut self.updates {
                updates.skip_this_version(&mut self.settings);
                note = updates.sheet_status().map(str::to_owned);
            }
            close = true;
        }
        if let Some(note) = note {
            self.pending_note = Some(note);
        }
        if close {
            self.update_sheet_open = false;
        }
    }

    /// The Settings window.
    ///
    /// **A real OS window**, not an egui window inside the frame. It was the latter
    /// and it was wrong in the way the prototype's was right: a settings sheet
    /// trapped inside the app has to be moved out of the way of the thing you are
    /// changing settings *for*. An immediate viewport rather than a deferred one,
    /// for the same reason the panels are — a deferred callback must be `'static`
    /// and this one mutates `App`. Where the backend cannot detach a window, egui
    /// embeds it and the behaviour is what it always was.
    ///
    /// **Nothing in here is disabled, and that is the rule now.** It was the opposite
    /// one: every specified section was present, and the ones whose feature did not
    /// exist yet carried a "not built yet" line rather than being omitted — on the
    /// argument that an absent control is indistinguishable from one nobody thought of,
    /// and that this window is also the map of what is coming.
    ///
    /// the maintainer reversed it on 2026-08-07, and the run of it decided the question. Of the
    /// five controls that ever carried the line, **four were stale** — the feature had
    /// been built and nobody came back to the label — and each was found by a reader
    /// believing a note that was months out of date. A map that is wrong four
    /// times in five is not a map. The two that were genuinely pending are in
    /// `docs/ui-queue.md`, which is where unbuilt work already lives and does not have
    /// to be maintained against the code to stay true.
    ///
    /// So: if a control is here, it works. A section with nothing working left in it
    /// goes too — an empty heading is worse than either.
    #[cfg(target_os = "macos")]
    fn settings_title_bar(ui: &mut egui::Ui, open: &mut bool) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), theme::TITLE_STRIP),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, theme::CHROME_DEEP);
        painter.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            egui::Stroke::new(1.0, egui::Color32::from_gray(52)),
        );
        painter.text(
            rect.left_center() + egui::vec2(10.0, 0.0),
            egui::Align2::LEFT_CENTER,
            "settings",
            egui::FontId::proportional(theme::size::TITLE),
            egui::Color32::from_gray(190),
        );

        let close_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 14.0, rect.center().y),
            egui::vec2(20.0, 20.0),
        );
        let close = ui
            .interact(
                close_rect,
                ui.id().with("settings-close"),
                egui::Sense::click(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if close.hovered() {
            painter.rect_filled(close_rect, 0.0, egui::Color32::from_gray(58));
        }
        painter.text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(theme::size::BODY + 1.0),
            if close.hovered() {
                theme::BRIGHT
            } else {
                theme::DIM
            },
        );
        if close.on_hover_text(theme::tip("Close Settings")).clicked() {
            *open = false;
        }

        let mut drag_rect = rect;
        drag_rect.max.x = close_rect.left() - 4.0;
        if ui
            .interact(
                drag_rect,
                ui.id().with("settings-drag"),
                egui::Sense::drag(),
            )
            .is_pointer_button_down_on()
        {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let before = self.settings.clone();
        let mut open = self.settings_open;
        let raise = std::mem::take(&mut self.settings_raise);

        // **Measured here, outside the panel, and remembered.** `read_dir` plus a
        // `stat` per tile is a few milliseconds — nothing once, and a stutter sixty
        // times a second. The panel body holds `&mut self.settings`, so it could not
        // reach this field anyway; a `Copy` value is read in and a flag comes back out,
        // which is the arrangement `InfoClicks` already uses for the same reason.
        //
        // `None` means "ask again", and it is set on close so a window reopened after
        // an hour of browsing does not quote an hour-old number.
        if self.cache_bytes.is_none() {
            self.cache_bytes = Some(lightbox::cache_bytes());
        }
        let cache_bytes = self.cache_bytes.unwrap_or(0);
        let mut purge = false;
        let mut reset_layout = false;
        let confirm_reset = self.settings_reset_confirm;
        let mut request_reset = false;
        let mut cancel_reset = false;
        let mut reset_all = false;
        let mut reveal_data = false;
        let mut check_updates_now = false;
        let mut stop_skipping = false;

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("monopro-settings"),
            platform::settings_viewport(),
            |ui, _class| {
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                open = false;
            }
            // **The comma has to be handled here as well as in the main window.**
            // Settings is a real OS viewport, so once it has focus the keystroke goes
            // to *it* and the main window's dispatch never sees it — which is why
            // the maintainer found the key opened the window but would not close it again, and
            // why it sometimes appeared not to work at all: whether it did depended on
            // which window the system thought was frontmost.
            //
            // Escape closes it too, which is what every other window in the OS does.
            //
            // Guarded on the focus test rather than on nothing: this window has text
            // fields in it — the export suffixes — and a comma typed into one of those
            // is a comma, not a command. `text_edit_focused` is the same predicate
            // `hotkeys` uses for the main window, so the two agree about what typing is.
            let typing = ui.ctx().text_edit_focused();
            if !typing
                && ui.ctx().input(|i| {
                    i.key_pressed(egui::Key::Comma) || i.key_pressed(egui::Key::Escape)
                })
            {
                open = false;
            }
            // **`⌘W` closes it too**, which is what every window on the platform
            // does — and it is not `CloseTab` here, because a settings window that
            // shut an image tab from under you would be the worst possible reading
            // of the most reflexive chord on the keyboard. Not guarded on `typing`:
            // `⌘W` is never text.
            if ui.ctx().input(|i| i.modifiers.command && i.key_pressed(egui::Key::W)) {
                open = false;
            }
            // **Clicking away closes it, and nothing is lost when it does.** the maintainer's
            // call. It is safe *because* settings are written the moment a control
            // moves — the save below is not tied to the window's lifetime — so
            // "close" here means only "put this away", which is what clicking
            // somewhere else means everywhere else.
            //
            // Guarded on having been focused at least once, because a viewport is
            // not focused on the frame it is created and closing on that would make
            // the window impossible to open. `rfd`'s folder picker blocks the thread
            // rather than running frames, so it cannot trip this either.
            let focused = ui.ctx().input(|i| i.viewport().focused).unwrap_or(true);
            if focused {
                self.settings_focused = true;
            } else if self.settings_focused {
                open = false;
            }
            // A second press of the button or the key while the window is already up
            // must bring it forward. Without this it stays where it is — often behind
            // the main window — which is exactly the maintainer's "pressing it again loses the
            // window": the flag was already true, so setting it again did nothing.
            if raise {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            if confirm_reset {
                egui::Modal::new(egui::Id::new("confirm-reset-settings")).show(ui.ctx(), |ui| {
                    ui.set_width(330.0);
                    theme::tracked(ui, "RESET ALL SETTINGS", theme::RUBY);
                    ui.add_space(6.0);
                    ui.label(theme::caption(
                        "Restore every preference to the value monopro ships with? Image edits are not affected.",
                    ));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            cancel_reset = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button(egui::RichText::new("Reset All").color(theme::RUBY))
                                .clicked()
                            {
                                reset_all = true;
                            }
                        });
                    });
                });
            }
            #[cfg(target_os = "macos")]
            egui::Panel::top("settings-title")
                .exact_size(theme::TITLE_STRIP)
                .frame(egui::Frame::new().fill(theme::CHROME_DEEP))
                .show(ui, |ui| Self::settings_title_bar(ui, &mut open));
            // **A sidebar, not one long column.** Eight groups in a single scroll
            // meant the last of them was reachable only by passing the other seven,
            // and a hairline was doing all the work of saying a subject had ended.
            // Grouping is what a settings window is for.
            let sheet = &mut self.sheet;
            sheet.begin();
            egui::containers::panel::Panel::left("settings-nav")
                .resizable(false)
                .default_size(170.0)
                .show(ui, |ui: &mut egui::Ui| {
                    ui.add_space(10.0);
                    // The search box sits above the list because it *replaces* the
                    // list's job while it has anything in it — see `Sheet::shows`.
                    ui.add(
                        egui::TextEdit::singleline(&mut sheet.query)
                            .hint_text("Search settings…")
                            .vertical_align(egui::Align::Center)
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(10.0);
                    let searching = sheet.searching();
                    // Dimmed rather than hidden while a search is live: the pages are
                    // still where they were, and removing them would make the window
                    // change shape every time the box gained a character.
                    ui.add_enabled_ui(!searching, |ui| {
                        for section in settings::Section::ALL {
                            let on = !searching && sheet.current() == section;
                            if ui.selectable_label(on, section.label()).clicked() {
                                sheet.section = Some(section);
                            }
                        }
                    });
                    if searching && ui.button("clear search").clicked() {
                        sheet.query.clear();
                    }
                });

            egui::CentralPanel::default().show(ui, |ui| {
                // **`auto_shrink` off horizontally, and a measure on the content.**
                // the maintainer resized this window to 920pt and found the scrollbar sitting in
                // the middle of it with text running past. Both are the same cause:
                // `auto_shrink` defaults to `[true, true]`, so the scroll area sized
                // itself to its *widest child* rather than to the panel — the bar landed
                // wherever that child ended, and anything wider overflowed past it.
                //
                // Vertical shrink stays on: a short page should not claim the full
                // height. Only the width was ever wrong.
                //
                // `MEASURE` then caps the column so a heading is not set across the full
                // 920 — the same argument `settings::NOTE_W` makes for a caption, applied
                // to the page. It leaves a right margin the scrollbar sits in rather than
                // on top of.
                egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(20, 8))
                    .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let s = &mut self.settings;
                // What every row's dot is measured against. One value, taken once, so
                // no row can be comparing against something slightly different.
                let d = settings::Settings::default();

                if sheet.shows(ui, settings::Section::Export, "FILE NAMING tiff png suffix filename stem name") {
                    settings::heading(ui, "FILE NAMING");
                    settings::rule(ui);
                    let (_, reset) = settings::item(
                        ui,
                        s.tiff_suffix != d.tiff_suffix,
                        "TIFF suffix",
                        Some("Added to the original filename."),
                        |ui| ui.add(egui::TextEdit::singleline(&mut s.tiff_suffix).desired_width(190.0)),
                    );
                    if reset {
                        s.tiff_suffix = d.tiff_suffix.clone();
                    }
                    settings::rule(ui);
                    let (_, reset) = settings::item(
                        ui,
                        s.png_suffix != d.png_suffix,
                        "PNG suffix",
                        Some("Added to the original filename."),
                        |ui| ui.add(egui::TextEdit::singleline(&mut s.png_suffix).desired_width(190.0)),
                    );
                    if reset {
                        s.png_suffix = d.png_suffix.clone();
                    }

                                }
                if sheet.shows(ui, settings::Section::Export, "OUTPUT folder destination colour color space print units resolution ppi depth") {
                    settings::heading(ui, "OUTPUT");
                settings::rule(ui);
                let (_, reset) = settings::item(
                    ui,
                    s.output_folder != d.output_folder,
                    "Default folder",
                    Some("Leave unset to choose on export."),
                    |ui| ui.horizontal(|ui| {
                    let path = s
                        .output_folder
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Choose…".into());
                    let shown = s
                        .output_folder
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Choose…".into());
                    if ui.button(shown).on_hover_text(theme::tip(path)).clicked()
                        && let Some(d) = dialogs::pick_folder(s.output_folder.as_deref())
                    {
                        s.output_folder = Some(d);
                    }
                    if s.output_folder.is_some() && ui.small_button("clear").clicked() {
                        s.output_folder = None;
                    }
                    }),
                );
                if reset {
                    s.output_folder = d.output_folder.clone();
                }
                settings::rule(ui);
                let (_, reset) = settings::item(ui, s.print_unit != d.print_unit, "Print units", None, |ui| {
                    let mut u = s.print_unit();
                    // Settings controls grow from the right edge, so reverse the
                    // insertion order to display the familiar `[in] [cm]` pair.
                    for k in Unit::UI_ORDER.into_iter().rev() {
                        if ui.selectable_label(u == k, k.label()).clicked() {
                            u = k;
                        }
                    }
                    s.set_print_unit(u);
                });
                if reset {
                    s.print_unit = d.print_unit.clone();
                }
                settings::rule(ui);
                let (_, reset) = settings::item(ui, s.print_ppi != d.print_ppi, "Default resolution", Some("Used when a new image opens."), |ui| {
                    ui.add(
                        egui::DragValue::new(&mut s.print_ppi)
                            .speed(1.0)
                            .range(OutputParams::PPI_RANGE)
                            .fixed_decimals(0)
                            .suffix(" ppi"),
                    );
                });
                if reset {
                    s.print_ppi = d.print_ppi;
                }

                // The master's colour space. **The RGB entries are offerable**, which
                // is what chemical toning bought: the pipeline can put three different
                // numbers in a pixel, so a container that holds three is no longer a
                // claim nobody has made.
                //
                // **Live now.** These were `pending` for as long as the export ignored
                // them and the master's space was monostar or, under a toner, eciRGB v2.
                // `Settings::space_for` is what the export module seeds from, and the
                // picker beside the container is what overrides it per image.
                //
                // Three keys rather than one, because the containers are chosen for
                // different jobs — a TIFF is the print master and a PNG is often what
                // gets handed to somebody. JPEG's is a **proof** space today; the key is
                // here so the three read as one decision.
                for (label, value, default) in [
                    ("TIFF color space", &mut s.tiff_color_space, &d.tiff_color_space),
                    ("PNG color space", &mut s.png_color_space, &d.png_color_space),
                    ("JPEG color space", &mut s.jpeg_color_space, &d.jpeg_color_space),
                ] {
                    settings::rule(ui);
                    let current = export::Space::from_key(value)
                        .unwrap_or_default()
                        .selectable();
                    let reset = settings::combo(ui, *value != *default, label, current.label(), |ui| {
                                for k in export::Space::UI_ORDER {
                                    let resp =
                                        ui.selectable_label(current == k, k.label());
                                    if resp.clicked() {
                                        *value = k.key().to_owned();
                                    }
                                    // **sRGB is offered and warned about rather than
                                    // hidden.** It was excluded on the ground that a
                                    // display-referred master is the encoding split 9b
                                    // removed arriving by the back door — which is a
                                    // good argument against *choosing* it and not one
                                    // for pretending it cannot be written. the maintainer asked
                                    // for the options; the tooltip is where the argument
                                    // goes.
                                    let tip = if k == export::Space::Srgb {
                                        "Display-referred, and a proof's space rather \
                                         than a master's — it discards the headroom a \
                                         print workflow is for. Offered because a file \
                                         going somewhere unmanaged is a real errand."
                                    } else if k.is_rgb() {
                                        "Three channels, for a toned print. Untoned, \
                                         it is the grayscale master with its channels \
                                         repeated — same picture, three times the file."
                                    } else {
                                        "Grayscale, L*. One channel, and the honest \
                                         container for an untoned print."
                                    };
                                    resp.on_hover_text(theme::tip(tip));
                                }
                    });
                    if reset {
                        *value = default.clone();
                    }
                }

                                }

                if sheet.shows(ui, settings::Section::Export, "PROOF jpeg jpg png quarter third half size format colour color space srgb monostar preview") {
                    settings::heading(ui, "PROOF");
                    settings::note(ui, "Defaults for Export Proof; master exports are unchanged.");
                    ui.add_space(4.0);

                    let mut target = s.proof_target();
                    settings::rule(ui);
                    let (_, reset) = settings::item(ui, s.proof_container != d.proof_container, "Format", None, |ui| {
                        egui::ComboBox::from_id_salt("proof-format")
                            .width(96.0)
                            .truncate()
                            .selected_text(settings::combo_text(target.container.label()))
                            .show_ui(ui, |ui| settings::combo_menu(ui, |ui| {
                                for c in export::Container::PROOF_ORDER {
                                    if ui.selectable_label(target.container == c, c.label()).clicked() {
                                        target.container = c;
                                    }
                                }
                            }));
                    });
                    if reset {
                        target.container = d.proof_target().container;
                    }

                    // **16-bit greys out for JPEG**, because baseline JPEG is 8 bits
                    // per sample. `Container::depths` is the one place that rule
                    // lives; this control asks rather than restating it.
                    settings::rule(ui);
                    let (_, reset) = settings::item(ui, s.proof_depth != d.proof_depth, "Depth", None, |ui| {
                        egui::ComboBox::from_id_salt("proof-depth")
                            .width(96.0)
                            .truncate()
                            .selected_text(settings::combo_text(target.depth.label()))
                            .show_ui(ui, |ui| settings::combo_menu(ui, |ui| {
                                for k in export::Depth::UI_ORDER {
                                    let ok = target.container.supports(k);
                                    let resp = ui.add_enabled(
                                        ok,
                                        egui::Button::selectable(target.depth == k, k.label()),
                                    );
                                    if resp.clicked() {
                                        target.depth = k;
                                    }
                                }
                            }));
                    });
                    if reset {
                        target.depth = d.proof_target().depth;
                    }

                    settings::rule(ui);
                    let (_, reset) = settings::item(ui, s.proof_space != d.proof_space, "Color space", Some("sRGB is safest for unmanaged screens."), |ui| {
                        egui::ComboBox::from_id_salt("proof-space")
                            .width(176.0)
                            .truncate()
                            .selected_text(settings::combo_text(target.space.label()))
                            .show_ui(ui, |ui| settings::combo_menu(ui, |ui| {
                                for k in export::Space::PROOF_ORDER {
                                    if ui.selectable_label(target.space == k, k.label()).clicked() {
                                        target.space = k;
                                    }
                                }
                            }));
                    });
                    if reset {
                        target.space = d.proof_target().space;
                    }

                    let mut scale = s.proof_scale();
                    settings::rule(ui);
                    let (_, reset) = settings::item(ui, s.proof_scale != d.proof_scale, "Size", Some("A fraction of the source image."), |ui| {
                        egui::ComboBox::from_id_salt("proof-scale")
                            .width(112.0)
                            .truncate()
                            .selected_text(settings::combo_text(scale.label()))
                            .show_ui(ui, |ui| settings::combo_menu(ui, |ui| {
                                for k in export::ProofScale::UI_ORDER {
                                    if ui.selectable_label(scale == k, k.label()).clicked() {
                                        scale = k;
                                    }
                                }
                            }));
                    });
                    if reset {
                        scale = d.proof_scale();
                    }

                    target.settle();
                    s.proof_container = target.container.key().into();
                    s.proof_depth = target.depth.key().into();
                    s.proof_space = target.space.key().into();
                    s.proof_scale = scale.key().into();
                }
                if sheet.shows(ui, settings::Section::Processing, "DEFAULT PIPELINE decode demosaic sampling luminance weighting mix contrast mask tone map dither") {
                    settings::heading(ui, "DEFAULT PIPELINE");
                ui.label(
                    theme::caption("what a NEW image opens at — open tabs are untouched"),
                );
                ui.add_space(4.0);

                let mut sampling = s.sampling();
                settings::rule(ui);
                let reset_sampling = settings::combo(
                    ui,
                    s.sampling != d.sampling,
                    "Decode",
                    &sampling_label(sampling),
                    |ui| {
                        for m in SAMPLING_ORDER {
                            let sel =
                                std::mem::discriminant(&sampling) == std::mem::discriminant(&m);
                            if ui.selectable_label(sel, sampling_label(m)).clicked() && !sel {
                                sampling = match m {
                                    Sampling::Demosaic(_) => Sampling::Demosaic(s.demosaic()),
                                    other => other,
                                };
                            }
                        }
                    });
                if reset_sampling {
                    sampling = d.sampling();
                }
                s.set_sampling(sampling);

                let mut algo = s.demosaic();
                settings::rule(ui);
                let reset_algo = settings::combo(ui, s.demosaic != d.demosaic, "Demosaic", algo.label(), |ui| {
                        for a in DemosaicAlgo::UI_ORDER {
                            if ui
                                .selectable_label(algo == a, a.label())
                                .on_hover_text(theme::tip(a.tooltip()))
                                .clicked()
                            {
                                algo = a;
                            }
                        }
                    },
                );
                if reset_algo {
                    algo = d.demosaic();
                }
                s.demosaic = algo.key().to_owned();

                let mut weighting = s.weighting();
                settings::rule(ui);
                let reset_weighting = settings::combo(
                    ui,
                    s.weighting != d.weighting,
                    "Luminance",
                    &weighting.label(),
                    |ui| {
                        for w in Weighting::UI_ORDER {
                            let sel =
                                std::mem::discriminant(&weighting) == std::mem::discriminant(&w);
                            let text = if matches!(w, Weighting::Weighted(..)) {
                                "Weighted (custom)".to_string()
                            } else {
                                w.label()
                            };
                            if ui.selectable_label(sel, text).clicked() {
                                weighting = match w {
                                    Weighting::Weighted(..) => weighting.as_weighted(),
                                    other => other,
                                };
                            }
                        }
                    },
                );
                if reset_weighting {
                    weighting = d.weighting();
                }
                s.set_weighting(weighting);
                if let Some((r, g, b)) = s.weighting().mix_mut().map(|(r, g, b)| (*r, *g, *b)) {
                    let mut mix = [r, g, b];
                    let mut changed = false;
                    for (i, name) in ["R", "G", "B"].iter().enumerate() {
                        changed |=
                            widgets::Row::new(&mut mix[i], 1.0, 0.0..=1.0, name).show(ui);
                    }
                    if changed {
                        s.set_weighting(Weighting::Weighted(mix[0], mix[1], mix[2]));
                    }
                }

                settings::rule(ui);
                settings::check(ui, &mut s.contrast_mask, d.contrast_mask, "Contrast Mask on");

                let mut tone = s.tone_map();
                settings::rule(ui);
                let reset_tone = settings::combo(
                    ui,
                    s.tone_map != d.tone_map,
                    "Tonal Transform",
                    tone.label(),
                    |ui| {
                        for t in ToneMap::UI_ORDER {
                            let sel = std::mem::discriminant(&tone) == std::mem::discriminant(&t);
                            if ui.selectable_label(sel, t.label()).clicked() && !sel {
                                tone = t;
                            }
                        }
                    },
                );
                if reset_tone {
                    tone = d.tone_map();
                }
                s.set_tone_map(tone);

                // **Not greyed by the export depth**, which is what it did for one
                // session and was wrong twice over. This row is the *default* a new tab
                // opens with, and a default is not doing nothing merely because today's
                // export depth would ignore it — the tab it seeds will be exported at
                // whatever depth that tab ends up choosing. And it governs the viewport,
                // which is an 8-bit surface and is always dithering or not.
                //
                // The 16-bit statement belongs on the control that is actually inert,
                // and that is the EXPORT module's checkbox, which is disabled at 16 bits
                // and says so there.
                settings::rule(ui);
                settings::check(ui, &mut s.dither, d.dither, "TPDF dither on");
                settings::note(ui, "Reduces banding on screen and in 8-bit exports.");

                let mut depth = s.export_depth();
                settings::rule(ui);
                let reset_depth = settings::combo(ui, s.export_depth != d.export_depth, "Export depth", depth.label(), |ui| {
                        for d in export::Depth::UI_ORDER {
                            if ui.selectable_label(depth == d, d.label()).clicked() {
                                depth = d;
                            }
                        }
                    },
                );
                if reset_depth {
                    depth = d.export_depth();
                }
                s.export_depth = depth.key().to_owned();

                                }
                if sheet.shows(ui, settings::Section::Viewer, "VIEWER BACKGROUND canvas background grey gray brightness") {
                    settings::heading(ui, "VIEWER BACKGROUND");
                    settings::rule(ui);
                    let (_, reset) = settings::item(
                        ui,
                        s.viewer_background != d.viewer_background,
                        "Canvas value",
                        Some("Background behind the image."),
                        |ui| {
                            widgets::settings_slider(ui, &mut s.viewer_background, d.viewer_background, 0.0..=100.0)
                        },
                    );
                    if reset {
                        s.viewer_background = d.viewer_background;
                    }

                                }

                if sheet.shows(ui, settings::Section::Viewer, "VALUE READOUT sample area size point average eyedropper sampler pin inspector tolerance reference views lab rgb colour color jpeg raw linear") {
                    settings::heading(ui, "VALUE READOUT");
                    let current = s.sample_area();
                    let reset_sample = settings::combo_width(
                        ui,
                        s.sample_area != d.sample_area,
                        "Sample area",
                        current.label(),
                        118.0,
                        |ui| {
                            for a in settings::SampleArea::UI_ORDER {
                                if ui.selectable_label(current == a, a.label()).clicked() {
                                    s.sample_area = a.key().into();
                                }
                            }
                        },
                    );
                    if reset_sample {
                        s.sample_area = d.sample_area.clone();
                    }
                    settings::note(ui, "3 × 3 is the smallest reliable DirectMosaic sample.");
                    settings::rule(ui);
                    let refv = s.reference_values();
                    let reset_reference = settings::combo_width(
                        ui,
                        s.reference_values != d.reference_values,
                        "Reference views",
                        refv.label(),
                        118.0,
                        |ui| {
                            for v in settings::ReferenceValues::UI_ORDER {
                                if ui.selectable_label(refv == v, v.label()).clicked() {
                                    s.reference_values = v.key().into();
                                }
                            }
                        },
                    );
                    if reset_reference {
                        s.reference_values = d.reference_values.clone();
                    }
                    settings::note(ui, "Controls readouts for JPEG and raw-linear views.");
                }

                if sheet.shows(ui, settings::Section::Viewer, "SURROUND mount border colour color okhsl hue width mat") {
                    settings::heading(ui, "SURROUND");
                settings::rule(ui);
                let (_, reset) = settings::item(
                    ui,
                    s.surround_width != d.surround_width,
                    "Width",
                    Some("Used whenever Surround is shown with B."),
                    |ui| {
                        widgets::settings_slider(ui, &mut s.surround_width, d.surround_width, 0.0..=300.0)
                    },
                );
                if reset {
                    s.surround_width = d.surround_width;
                }
                settings::rule(ui);
                surround_colour(ui, s, &d);
                                }
                if sheet.shows(ui, settings::Section::General, "STARTUP opening new files inherit develop lightbox launch panels layout duplicates quit warning") {
                    settings::heading(ui, "STARTUP");
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.reset_on_open,
                        d.reset_on_open,
                        "Reset edits for newly opened files",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.start_in_develop,
                        d.start_in_develop,
                        "Start in Develop",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.reset_panels_on_start,
                        d.reset_panels_on_start,
                        "Reset panel layout on launch",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.warn_unsaved_duplicates,
                        d.warn_unsaved_duplicates,
                        "Warn about unsaved duplicates on quit",
                    );
                }

                if sheet.shows(ui, settings::Section::Export, "EXPORT BEHAVIOR metadata iptc quick export dialog") {
                    settings::heading(ui, "BEHAVIOR");
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.export_metadata,
                        d.export_metadata,
                        "Include authored metadata",
                    );
                    settings::note(ui, "Includes IPTC, keywords, rating and color label.");
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.quick_export,
                        d.quick_export,
                        "Quick Export when a folder is set",
                    );
                    settings::note(ui, "Uses the dialog if no folder is set or the name is taken.");
                }

                if sheet.shows(ui, settings::Section::Lightbox, "FOLDERS root home user directory external drive card volume browse") {
                    settings::heading(ui, "FOLDERS");
                    settings::rule(ui);
                    let (_, reset) = settings::item(
                        ui,
                        s.lightbox_folder_root != d.lightbox_folder_root,
                        "Folders root",
                        Some("External drives and cards are always shown."),
                        |ui| {
                            let effective = s.lightbox_folder_root.clone().or_else(|| {
                                platform::home_dir()
                            });
                            let full = effective
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "Home".to_owned());
                            let shown = effective
                                .as_ref()
                                .and_then(|path| path.file_name())
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| {
                                    if let Some(path) = effective.as_deref() {
                                        platform::root_name(path)
                                    } else {
                                        "Home".to_owned()
                                    }
                                });
                            if ui.button(shown).on_hover_text(theme::tip(full)).clicked()
                                && let Some(root) = dialogs::pick_folder(effective.as_deref())
                            {
                                s.lightbox_folder_root = Some(root);
                            }
                        },
                    );
                    if reset {
                        s.lightbox_folder_root = d.lightbox_folder_root.clone();
                    }
                }
                if sheet.shows(ui, settings::Section::Lightbox, "TILES thumbnails edits gray grey default sort frameless filenames folders non-image files") {
                    settings::heading(ui, "TILES");
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_gray,
                        d.lightbox_gray,
                        "View images in gray by default",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_xmp_thumbnails,
                        d.lightbox_xmp_thumbnails,
                        "Show developed previews",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.remember_lightbox_sort,
                        d.remember_lightbox_sort,
                        "Remember sort order",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.frameless_tiles,
                        d.frameless_tiles,
                        "Frameless tiles at startup",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_filenames,
                        d.lightbox_filenames,
                        "Show filenames",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_edited_mark,
                        d.lightbox_edited_mark,
                        "Mark edited images",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_folders,
                        d.lightbox_folders,
                        "Show folders in the grid",
                    );
                    settings::rule(ui);
                    settings::check(
                        ui,
                        &mut s.lightbox_other_files,
                        d.lightbox_other_files,
                        "Show non-image files",
                    );
                }
                if sheet.shows(ui, settings::Section::Lightbox, "CACHE thumbnail cache purge clear disk size storage thumbcache reclaim space") {
                    settings::heading(ui, "CACHE");
                    settings::rule(ui);
                    // **A readout and one button, and no automatic ceiling.** A size
                    // limit is the other half of what a cache preference usually offers
                    // and it is deliberately not here yet: enforcing one means deciding
                    // what to evict, and the only honest key this cache has is "least
                    // recently *written*", which is not the same as least recently
                    // wanted. A wrong eviction policy costs a thumbnail re-decode every
                    // time you revisit an old folder. The button is the part that needs
                    // no policy.
                    settings::item(
                        ui,
                        false,
                        "Thumbnail cache",
                        Some("Safe to purge; thumbnails rebuild as needed."),
                        |ui| ui.horizontal(|ui| {
                        ui.label(theme::readout(lightbox::cache_label(cache_bytes)));
                        // Nothing to purge is a disabled button rather than a hidden
                        // one — the row still has to say where the cache is and that it
                        // is empty, and a control that vanishes is a control you go
                        // looking for.
                        if ui
                            .add_enabled(cache_bytes > 0, egui::Button::new("Purge now"))
                            .on_hover_text(theme::tip(
                                "Delete every cached thumbnail. Folders you revisit will \
                                 build their tiles again the first time you open them.",
                            ))
                            .clicked()
                        {
                            purge = true;
                        }
                    }),
                    );
                }
                if sheet.shows(ui, settings::Section::Controls, "INPUT scroll wheel invert zoom hotkeys keys tooltips hover") {
                    settings::heading(ui, "INPUT");
                    settings::rule(ui);
                    settings::check(ui, &mut s.invert_scroll, d.invert_scroll, "Invert scroll direction");
                    settings::rule(ui);
                    settings::check(ui, &mut s.scroll_zoom, d.scroll_zoom, "Zoom with the scroll wheel");
                    settings::rule(ui);
                    settings::check(ui, &mut s.tooltips, d.tooltips, "Show hover tooltips");
                    settings::rule(ui);
                    settings::check(ui, &mut s.hotkeys_enabled, d.hotkeys_enabled, "Hotkeys enabled");
                }

                if sheet.shows(ui, settings::Section::Controls, "HOTKEYS keys shortcuts bindings chords reference lightbox quick look loupe inspector pin delete ratings labels") {
                    settings::heading(ui, "HOTKEYS");
                for group in hotkeys::Group::ORDER {
                    ui.add_space(6.0);
                    settings::note(ui, group.label());
                    for gesture in hotkeys::reference_gestures(group, true) {
                        settings_hotkey_row(ui, gesture.chord, gesture.what, true);
                    }
                    for (i, bind) in
                        hotkeys::TABLE.iter().enumerate().filter(|(_, b)| b.group == group)
                    {
                        // One entry per *action*, not per binding. `⌘+` and `⌘⇧+`
                        // are one physical key that egui names differently depending
                        // on shift, so the table binds both — correctly — and the
                        // reference listed them as two lines that both said "Zoom
                        // in", which reads as though they did different things.
                        // Skipping later aliases is general: it will do the right
                        // thing for the next key that needs two bindings.
                        if hotkeys::TABLE.iter().take(i).any(|o| o.action == bind.action) {
                            continue;
                        }
                        settings_hotkey_row(ui, &bind.chord(), bind.what, bind.built);
                    }
                    for gesture in hotkeys::reference_gestures(group, false) {
                        settings_hotkey_row(ui, gesture.chord, gesture.what, true);
                    }
                }

                                }
                if sheet.shows(ui, settings::Section::General, "PANEL POSITIONS layout tiles docking reset arrangement") {
                    settings::heading(ui, "PANEL POSITIONS");
                    settings::note(ui, "Panel positions are remembered across restarts.");
                    if ui.button("Reset to the default layout").clicked() {
                        reset_layout = true;
                    }
                    settings::heading(ui, "SETTINGS");
                    if ui
                        .button(egui::RichText::new("Reset All Settings…").color(theme::RUBY))
                        .clicked()
                    {
                        request_reset = true;
                    }
                }

                if sheet.shows(
                    ui,
                    settings::Section::About,
                    "ACKNOWLEDGMENTS credits licences licenses typeface fonts jetbrains icons phosphor lucide software update",
                ) {
                    // **Software update lives in About** (macOS): the auto-check
                    // preference with its toggle, the manual check, and the skip
                    // the badge is honouring. Every persisted key has working UI;
                    // these two keys are that pair's UI.
                    if cfg!(target_os = "macos") {
                        settings::heading(ui, "SOFTWARE UPDATE");
                        settings::check(
                            ui,
                            &mut s.check_for_updates,
                            d.check_for_updates,
                            "Check for updates daily",
                        );
                        // The toggle's whole effect: Sparkle re-reads it the moment
                        // the settings change. Idempotent, so it may run per frame.
                        if s.check_for_updates != before.check_for_updates
                            && let Some(updates) = &self.updates
                        {
                            updates.set_auto_check(s.check_for_updates);
                        }
                        let skipped = updater::Updates::skipped_version(s).map(str::to_owned);
                        settings::note(ui, match &skipped {
                            Some(v) => format!("skipping monopro {v}"),
                            None => "The check is silent; an update announces itself \
                                as a badge in the title strip."
                                .to_owned(),
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Check for Updates…").clicked() {
                                check_updates_now = true;
                            }
                            if skipped.is_some() && ui.button("Stop skipping").clicked() {
                                stop_skipping = true;
                            }
                        });
                        settings::rule(ui);
                    }
                    settings::heading(ui, "ACKNOWLEDGMENTS");
                    for line in ACKNOWLEDGMENTS {
                        ui.label(egui::RichText::new(*line).size(theme::size::CAPTION));
                    }

                    ui.add_space(12.0);
                    if let Some(p) = settings::path() {
                        ui.label(theme::caption(format!("stored in {}", p.display())));
                    }
                    ui.add_space(8.0);
                    if ui
                        .button(format!(
                            "Show Application Data in {}",
                            platform::file_manager_name()
                        ))
                        .clicked()
                    {
                        reveal_data = true;
                    }
                }

                });
                });
            });
            },
        );

        self.settings_open = open;
        if request_reset {
            self.settings_reset_confirm = true;
        }
        if cancel_reset {
            self.settings_reset_confirm = false;
        }
        if reset_all {
            self.settings = settings::Settings::default();
            self.settings_reset_confirm = false;
            self.status = "settings restored to defaults".into();
        }
        if reveal_data
            && let Some(dir) = settings::dir()
            && let Err(e) = platform::reveal(&dir)
        {
            self.status = format!("could not show application data: {e}");
        }
        // The manual check and the un-skip, acted on out here for the same reason
        // `reveal_data` is: the panel body holds the settings borrow, and both
        // routes write through the updater (which persists its own skip copy).
        // The sheet opens with the check so its result has somewhere to land.
        if check_updates_now {
            if let Some(updates) = &mut self.updates
                && let Some(why) = updates.check_now(&mut self.settings)
            {
                self.pending_note = Some(why);
            }
            self.update_sheet_open = true;
        }
        if stop_skipping
            && let Some(updates) = &mut self.updates
        {
            updates.stop_skipping(&mut self.settings);
            self.pending_note = Some("stopped skipping — the next check offers the feed again".into());
        }
        // Acted on out here, because the panel that asked was holding the borrow the
        // purge needs — and because deleting ten thousand files in the middle of laying
        // out a window is the sort of thing that should be one statement you can find.
        // **The same two lines `reset_panels_on_start` already runs**, in place. The
        // note here used to read "not built yet — there is no path that rebuilds it in
        // place", and that was never true after the startup setting was added:
        // `Layout::default()` *is* the path, and `Lightbox::reset_tree` is its twin.
        // the maintainer asked what in Settings could be wired up; this was the answer, and it
        // was a stale label rather than a missing feature.
        //
        // Out here with `purge` rather than in the closure, for the reason that one is:
        // the panel body holds `&mut self.settings` and cannot reach another field of
        // `self`. Acting after the viewport also means the tree is replaced between
        // frames rather than while it is being laid out.
        if reset_layout {
            self.layout = Layout::default();
            self.lightbox.reset_tree();
            // `Layout::default` starts undirtied, which is right for a fresh launch and
            // wrong here: this *is* a change, and one that must survive a quit or the
            // old arrangement comes back and the button looks like it did nothing.
            self.layout.dirty = true;
            self.status = "panel layout reset".into();
        }
        if purge {
            let freed = lightbox::purge_cache();
            self.cache_bytes = None;
            self.status = format!(
                "thumbnail cache purged — {} freed",
                lightbox::cache_label(freed)
            );
        }
        // Armed again for the next open, so a window reopened later starts by
        // expecting focus rather than by closing on the frame it appears.
        if !open {
            self.settings_focused = false;
            self.settings_reset_confirm = false;
            self.cache_bytes = None;
        }
        // Written when something moves, not on a timer — same rule as app memory,
        // and for the same reason: this app is idle-quiet and never ticks one.
        if self.settings != before
            && let Err(e) = self.settings.save()
        {
            self.status = format!("could not write settings: {e}");
        }
    }
}

/// Credits, and in three cases obligations.
///
/// The demosaic suite is transcribed from GPL sources; naming them is a licence
/// term, not a courtesy. See `docs/settings-menu.md`. **Lucide is ISC**, which asks the
/// same, and the icon list stopped being Phosphor-only when float and dock were redrawn
/// — see `icons::SOURCES`.
const ACKNOWLEDGMENTS: &[&str] = &[
    "monopro — C. Cunningham, 2026",
    "github.com/christophcunningham/monopro · monopro.pages.dev",
    "",
    "Demosaic algorithms ported from RawTherapee, GPL-3.0-or-later:",
    "  RCD — Luis Sanz Rodríguez, Ingo Weyrich",
    "  AMaZE — Emil J. Martinec",
    "  Hamilton-Adams — Adams & Hamilton, Eastman Kodak",
    "",
    "Typeface: JetBrains Mono",
    "Icons: Phosphor Icons (MIT) · Lucide (ISC)",
    "",
    "Built with rawler, wgpu, egui/eframe, rayon, roxmltree,",
    "reflink-copy, png, tiff, rfd, bytemuck, pollster, thiserror, toml",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_view_fits_the_complete_stored_crop() {
        let mut params = raw_core::Params::default();
        params.composition.crop = raw_core::Rect {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        };
        let frame = raw_core::Frame::resolve(
            raw_core::Dims { w: 1200, h: 800 },
            raw_core::Orientation::default(),
            &params.composition,
        );
        let (w, h, view) = fitted_picture_view(&frame, snapshot::THUMB_MAX);
        assert_eq!((w, h), (320, 213));
        assert_eq!((view.off_x, view.off_y), (300.0, 200.0));
        assert_eq!(view.overlays, raw_gpu::Overlays::NONE);
        assert_eq!(view.surround, raw_gpu::Surround::NONE);
        assert_eq!(view.background, 0.0);
    }

    #[test]
    fn about_names_the_face_the_app_actually_embeds() {
        let text = ACKNOWLEDGMENTS.join("\n");
        assert!(text.contains("Typeface: JetBrains Mono"));
        assert!(!text.contains("Standing on:"));
    }

    /// A panel like the ones a floating window holds: a slider, a checkbox, and — the
    /// case that matters — a text field for Tab to get lost in.
    fn floating_panel(ui: &mut egui::Ui, text: &mut String, number: &mut f32) -> egui::Rect {
        ui.add(egui::Slider::new(number, 0.0..=1.0).text("v"));
        ui.checkbox(&mut false.clone(), "c");
        ui.add(egui::TextEdit::singleline(text)).rect
    }

    fn win() -> Option<egui::Rect> {
        Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(400.0, 300.0),
        ))
    }

    #[test]
    fn the_hotkey_hud_nearly_fills_the_viewport_without_losing_its_rim() {
        for size in [egui::vec2(760.0, 600.0), egui::vec2(1440.0, 900.0)] {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let hud = hotkey_hud_rect(screen);
            assert!(screen.contains_rect(hud));
            assert!(hud.width() / screen.width() > 0.85);
            assert!(hud.height() / screen.height() > 0.85);
            assert!(hud.left() > screen.left() && hud.top() > screen.top());
        }
    }

    #[test]
    fn fit_scale_leaves_the_same_margin_for_render_and_reference_views() {
        let out = egui::vec2(1600.0, 1000.0);
        for img in [
            egui::vec2(6000.0, 4000.0),
            egui::vec2(4000.0, 6000.0),
            egui::vec2(9.0, 9.0),
        ] {
            let scale = fit_scale(out, img, VIEW_FIT_MARGIN);
            let shown = img * scale;
            let slack = out - shown;
            assert!(slack.x >= 2.0 * VIEW_FIT_MARGIN - 1.0e-3);
            assert!(slack.y >= 2.0 * VIEW_FIT_MARGIN - 1.0e-3);
            assert!(
                (slack.x - 2.0 * VIEW_FIT_MARGIN).abs() < 1.0e-3
                    || (slack.y - 2.0 * VIEW_FIT_MARGIN).abs() < 1.0e-3
            );
        }
    }

    #[test]
    fn surround_hex_accepts_the_two_familiar_spellings_and_normalizes_them() {
        for (text, expected) in [
            ("#f5dcdc", [0xF5, 0xDC, 0xDC]),
            ("F5DCDC", [0xF5, 0xDC, 0xDC]),
        ] {
            let rgb = parse_hex_rgb(text).expect(text);
            assert_eq!(rgb, expected);
            assert_eq!(format_hex_rgb(rgb), "#F5DCDC");
        }
        for invalid in ["#fff", "#GG0000", "1234567", ""] {
            assert_eq!(parse_hex_rgb(invalid), None, "accepted {invalid:?}");
        }
    }

    /// **The readout leads with the focal length and never names the lens.**
    ///
    /// Against the real files rather than a fixture, because the whole question is what
    /// these bodies actually wrote — the same reason
    /// `the_leica_aperture_is_read_from_aperture_value` reads its DNG. Skips when the
    /// corpus is absent.
    ///
    /// The Fuji is the case worth having: it is the only corpus file whose lens name is
    /// long enough for its return to be obvious, and it is a zoom, so the number here is
    /// the one thing that would differ between two frames on the same lens.
    #[test]
    fn the_readout_gives_the_focal_length_and_not_the_lens() {
        let raws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        for file in [
            "canon_eos_r_54.cr3",      // EF 85mm f/1.4L IS USM, at 85
            "FujiGFX100s_Raw.raf",     // GF45-100mmF4 R LM OIS WR, at 45
            "panasonic-reference.RW2", // no lens name at all, at 15.4
        ] {
            let path = raws.join(file);
            if !path.exists() {
                continue;
            }
            let img = raw_core::sensor::SensorImage::load(&path).expect("the corpus decodes");
            let line = exposure_line(&img.meta);
            let focal = img.meta.focal_len.expect("these three record one");
            assert!(
                line.starts_with(&focal_label(focal)),
                "{file} at {focal}mm gave {line:?} — the focal length leads the line"
            );
            if let Some(lens) = img.meta.lens.as_deref().filter(|l| l.len() > 3) {
                assert!(
                    !line.contains(lens),
                    "{file} put the lens name {lens:?} back in the readout: {line:?}"
                );
            }
        }
    }

    /// `50mm`, not `50 mm` and not `50.0mm` — and the fractional lengths in the corpus
    /// keep their digit rather than being rounded into a lens that does not exist.
    #[test]
    fn a_focal_length_is_written_the_way_a_photographer_writes_it() {
        assert_eq!(focal_label(50.0), "50mm");
        assert_eq!(focal_label(28.0), "28mm");
        assert_eq!(focal_label(61.0), "61mm");
        assert_eq!(
            focal_label(15.4),
            "15.4mm",
            "the RW2's compact is not a 15mm lens"
        );
        assert_eq!(focal_label(17.7), "17.7mm");
    }

    /// **Every click reports `clicked`, and a triple click passes through
    /// `double_clicked` on its way.**
    ///
    /// Two facts about egui, and the Dodge & Burn layer row is built on both. Single
    /// click toggles the layer open; double-click renames. That only works because the
    /// rename is an `else` — the second click arrives as `clicked && double_clicked`
    /// together, so handling them separately would toggle twice and land the row back
    /// where it started.
    ///
    /// The second fact is why **triple-click cannot be the rename gesture**, which is
    /// worth pinning rather than remembering: it was the maintainer's suggestion, and the reason
    /// it fails is not a matter of taste. A triple click *contains* a double click, so
    /// it cannot be told apart from one — it would fire the rename on the way past
    /// whatever it was meant to disambiguate from.
    ///
    /// If a future egui stops reporting `clicked` on the second press, the layer row
    /// silently starts renaming without opening. This is what says so.
    #[test]
    fn a_double_click_arrives_as_a_click_as_well() {
        let ctx = egui::Context::default();
        let at = egui::pos2(50.0, 20.0);
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut seen = Vec::new();
        for pass in 0..8 {
            let events = match pass {
                1 => vec![egui::Event::PointerMoved(at), button(true)],
                3 | 5 => vec![button(true)],
                2 | 4 | 6 => vec![button(false)],
                _ => vec![],
            };
            let input = egui::RawInput {
                screen_rect: win(),
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                let r = ui.allocate_rect(
                    egui::Rect::from_min_size(egui::pos2(0.0, 10.0), egui::vec2(300.0, 20.0)),
                    egui::Sense::click(),
                );
                if r.clicked() || r.double_clicked() || r.triple_clicked() {
                    seen.push((r.clicked(), r.double_clicked(), r.triple_clicked()));
                }
            });
        }
        assert_eq!(
            seen,
            vec![
                (true, false, false),
                (true, true, false),
                (true, false, true)
            ],
            "egui's click counting changed — the layer row's open/rename split rests on \
             `clicked` firing every time, and on a triple click reporting a double on \
             the way through"
        );
    }

    fn tab_event(mods: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: mods,
        }
    }

    fn key(k: egui::Key, pressed: bool, mods: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: mods,
        }
    }

    #[test]
    fn space_is_taken_from_egui_in_the_lightbox_and_left_alone_outside_it() {
        // **Why the flash of ruby round the footer buttons.** egui reads Space as
        // "activate the focused widget" in `begin_pass`, which runs before `App::ui` —
        // so pressing Space to preview a frame also pushed whichever of LIGHTBOX or
        // DEVELOP last held focus, and those two wear ruby when active. Nothing was
        // clicked; the button drew itself pressed, which is worse than a click because
        // it looks like the mode is about to change and then does not.
        //
        // Only in the Lightbox: in Develop, Space is egui's and pressing a focused
        // button with it is the correct thing for it to do.
        let none = egui::Modifiers::default();
        let mut ev = vec![
            key(egui::Key::Space, true, none),
            key(egui::Key::Space, false, none),
        ];
        assert_eq!(steal_bare_keys(&mut ev, true), (false, true));
        assert!(
            ev.is_empty(),
            "the release was left behind and will latch a widget"
        );

        let mut ev = vec![key(egui::Key::Space, true, none)];
        assert_eq!(
            steal_bare_keys(&mut ev, false),
            (false, false),
            "taken outside the Lightbox"
        );
        assert_eq!(ev.len(), 1, "the event should have been left in the stream");

        // A modified Space is somebody else's chord and is never claimed — the same
        // rule `only_a_bare_tab_is_taken` states for Tab.
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let mut ev = vec![key(egui::Key::Space, true, shift)];
        assert_eq!(steal_bare_keys(&mut ev, true), (false, false));
        assert_eq!(ev.len(), 1);

        // And Tab still goes, with or without the Lightbox up, because hiding the
        // panels is a thing you do in both modes.
        for lb in [true, false] {
            let mut ev = vec![key(egui::Key::Tab, true, none)];
            assert_eq!(steal_bare_keys(&mut ev, lb), (true, false));
            assert!(ev.is_empty());
        }
    }

    #[test]
    fn tab_is_taken_inside_a_floating_panel_and_never_latches() {
        // **The route `raw_input_hook` cannot see.** eframe calls the hook once per
        // frame for the viewport it is updating, and a floating panel is an *immediate*
        // viewport created inside `App::ui` — so with the panel's window focused, Tab
        // reached egui's focus system and `tab` did nothing but cycle widgets. the maintainer
        // found it; the first fix had closed only the main window's route.
        //
        // Pressed six times, because once is not the bug. `take_bare_tab` cannot remove
        // the event — `Memory::begin_pass` has already spent it — so it hands the focus
        // back afterwards, and that is what stops the navigation accumulating. Without
        // it, the second press lands focus on the text field and the guard below
        // surrenders Tab to egui for good.
        let ctx = egui::Context::default();
        let (mut text, mut number) = (String::new(), 0.5f32);
        for press in 1..=6 {
            let input = egui::RawInput {
                screen_rect: win(),
                events: vec![tab_event(egui::Modifiers::default())],
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                // Before the panel's widgets, exactly as `float_develop` does it: focus
                // advances as widgets are added, so asking afterwards reads the answer
                // egui has already moved on to.
                let took = take_bare_tab(ui.ctx());
                floating_panel(ui, &mut text, &mut number);
                assert!(took, "press {press} was not taken");
                assert!(
                    !ui.ctx().text_edit_focused(),
                    "press {press} left a text field focused — the next press is lost to it"
                );
            });
        }
    }

    #[test]
    fn typing_still_wins_in_a_floating_panel() {
        // With a text field focused, Tab is a text field's key. Reached by *clicking*
        // into it, which is the only way focus can get there once Tab is being taken:
        // a click is deliberate, and Tab navigation no longer accumulates.
        let ctx = egui::Context::default();
        let (mut text, mut number) = (String::new(), 0.5f32);
        let mut field = egui::Rect::NOTHING;
        let mut took = true;
        for pass in 0..4 {
            let mut events = Vec::new();
            match pass {
                1 => {
                    events.push(egui::Event::PointerMoved(field.center()));
                    events.push(egui::Event::PointerButton {
                        pos: field.center(),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    });
                }
                2 => events.push(egui::Event::PointerButton {
                    pos: field.center(),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                }),
                3 => events.push(tab_event(egui::Modifiers::default())),
                _ => {}
            }
            let input = egui::RawInput {
                screen_rect: win(),
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                if pass == 3 {
                    assert!(
                        ui.ctx().text_edit_focused(),
                        "the click did not focus the field"
                    );
                    took = take_bare_tab(ui.ctx());
                }
                field = floating_panel(ui, &mut text, &mut number);
            });
        }
        assert!(!took, "Tab was stolen from a focused text field");
    }

    #[test]
    fn only_a_bare_tab_is_taken() {
        // Shift-Tab is egui's "focus the previous widget" and nothing here claims it, so
        // it is left alone — the same exactness `hotkeys::TABLE` applies to every chord.
        let ctx = egui::Context::default();
        let (mut text, mut number) = (String::new(), 0.5f32);
        let mut took = true;
        let input = egui::RawInput {
            screen_rect: win(),
            events: vec![tab_event(egui::Modifiers::SHIFT)],
            modifiers: egui::Modifiers::SHIFT,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            took = take_bare_tab(ui.ctx());
            floating_panel(ui, &mut text, &mut number);
        });
        assert!(!took, "shift-Tab is not the panel toggle");
    }

    #[test]
    fn the_window_device_asks_for_the_adapters_full_texture_limit() {
        // Regression, found by launching the GUI and switching sampling mode.
        // egui-wgpu's default descriptor caps max_texture_dimension_2d at 8192,
        // which is smaller than the working image of any 100 MP sensor in
        // DirectMosaic — the Fuji GFX 100S is 11648 wide — and the result is a
        // wgpu validation panic rather than a mode the app can decline.
        //
        // Tests the descriptor itself rather than a live window, so it runs
        // headless; it is the same closure eframe is handed.
        let cfg = wgpu_config();
        let egui_wgpu::WgpuSetup::CreateNew(setup) = &cfg.wgpu_setup else {
            panic!("expected eframe to be creating its own device");
        };

        let Some(adapter) = pollster::block_on(async {
            let instance =
                wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            instance.request_adapter(&Default::default()).await.ok()
        }) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };

        let asked = (setup.device_descriptor)(&adapter)
            .required_limits
            .max_texture_dimension_2d;
        assert_eq!(
            asked,
            adapter.limits().max_texture_dimension_2d,
            "the window device left texture size on the table"
        );
        assert!(
            asked >= 11648,
            "{asked} px cannot hold a GFX 100S in DirectMosaic"
        );
    }
}

#[cfg(test)]
mod sampler_tests {
    use super::*;
    use raw_core::geometry::Dims;
    use raw_core::scene::LumaImage;
    use settings::SampleArea;

    /// A `w × h` image whose pixel values are their own index, so any window's mean
    /// is arithmetic that can be checked by hand.
    fn ramp(w: usize, h: usize) -> LumaImage {
        LumaImage {
            data: (0..w * h).map(|i| i as f32).collect(),
            output_dims: Dims { w, h },
            source_dims: Dims { w, h },
            clipped: Vec::new(),
        }
    }

    #[test]
    fn finished_samples_average_final_lightness_not_scene_values() {
        let mut params = raw_core::Params::default();
        params.display.tone_map = raw_core::ToneMap::Clip;
        let scene = [0.01, 0.81];
        let got = finish_sample(&scene, &params).lstar;
        let expected = scene
            .iter()
            .map(|&v| raw_core::display::lstar_encode(v) * 100.0)
            .sum::<f32>()
            / 2.0;
        assert!((got - expected).abs() < 1.0e-5);
        let old_order = raw_core::display::lstar_encode((scene[0] + scene[1]) * 0.5) * 100.0;
        assert!(
            (got - old_order).abs() > 5.0,
            "the scene was averaged before L*"
        );
    }

    #[test]
    fn finished_toned_samples_report_lightness_and_chroma() {
        let mut params = raw_core::Params::default();
        params.display.tone_map = raw_core::ToneMap::Clip;
        params.toning.enabled = true;
        params.toning.process = raw_core::toning::Process::Vandyke;
        let got = finish_sample(&[0.3], &params);
        let (a, b) = got.lab.expect("a toned sample has chroma");
        assert!(got.lstar > 0.0 && got.lstar < 100.0);
        assert!(a.abs() + b.abs() > 1.0);
    }

    #[test]
    fn a_completed_sample_cannot_replace_a_newer_edit_or_cursor_position() {
        let dims = Dims { w: 8, h: 6 };
        let frame =
            raw_core::Frame::resolve(dims, raw_core::Orientation::Rotate0, &Default::default());
        let mut sampler = FinishedSampler::default();
        sampler.prepare(
            Arc::new(raw_core::Params::default()),
            frame,
            SampleArea::Point,
            Some((1.0, 1.0)),
            &[],
        );
        let old_context = sampler.context.clone().unwrap();
        let old_wanted = sampler.wanted[0];

        sampler.prepare(
            Arc::new(raw_core::Params::default()),
            frame,
            SampleArea::Point,
            Some((5.0, 4.0)),
            &[],
        );
        assert!(!sampler.store(
            &old_context,
            old_wanted,
            FinishedSample {
                lstar: 12.0,
                lab: None
            },
        ));
        assert!(sampler.get(SampleTarget::Cursor).is_none());

        let moved_context = sampler.context.clone().unwrap();
        let moved_wanted = sampler.wanted[0];
        let mut changed = raw_core::Params::default();
        changed.exposure.ev = 1.0;
        sampler.prepare(
            Arc::new(changed),
            frame,
            SampleArea::Point,
            Some((5.0, 4.0)),
            &[],
        );
        assert!(!sampler.store(
            &moved_context,
            moved_wanted,
            FinishedSample {
                lstar: 34.0,
                lab: None
            },
        ));
        assert!(sampler.get(SampleTarget::Cursor).is_none());
    }

    #[test]
    fn point_reads_exactly_one_pixel() {
        let img = ramp(5, 5);
        // Row 2, column 3 is index 13, and `Point` must not average it with anything.
        assert_eq!(sample_luma(&img, 3.4, 2.9, SampleArea::Point), Some(13.0));
    }

    #[test]
    fn a_window_is_centred_on_the_pixel_you_clicked() {
        // The whole reason every size is odd. A 3x3 centred on index 12 of a 5-wide
        // ramp covers 6,7,8 / 11,12,13 / 16,17,18 — mean 12, the centre itself.
        let img = ramp(5, 5);
        let got = sample_luma(&img, 2.5, 2.5, SampleArea::Three).expect("inside the image");
        assert!((got - 12.0).abs() < 1.0e-4, "3x3 centred on 12 gave {got}");
    }

    #[test]
    fn an_edge_window_divides_by_what_it_read() {
        // Top-left corner: a 3x3 has only 0,1 / 5,6 in the image. Mean is 3.0.
        // Dividing by 9 would give 1.33 — a darker reading at every border, and a
        // plausible one, which is the failure this guards.
        let img = ramp(5, 5);
        let got = sample_luma(&img, 0.5, 0.5, SampleArea::Three).expect("inside the image");
        assert!(
            (got - 3.0).abs() < 1.0e-4,
            "corner 3x3 gave {got}, expected the mean of four"
        );
    }

    #[test]
    fn a_window_larger_than_the_image_is_the_whole_image() {
        // 31x31 on a 5x5 clamps to all 25 pixels; the ramp's mean is 12.
        let img = ramp(5, 5);
        let got = sample_luma(&img, 2.5, 2.5, SampleArea::ThirtyOne).expect("inside the image");
        assert!((got - 12.0).abs() < 1.0e-4, "oversized window gave {got}");
    }

    #[test]
    fn outside_the_image_reports_nothing_rather_than_the_nearest_edge() {
        let img = ramp(5, 5);
        for (x, y) in [(-0.5, 2.0), (2.0, -0.5), (5.0, 2.0), (2.0, 5.0)] {
            assert_eq!(
                sample_luma(&img, x, y, SampleArea::Three),
                None,
                "a point off the picture at ({x}, {y}) reported a value"
            );
        }
    }

    /// The reference sampler follows the same three rules, on bytes and three
    /// channels. Worth its own test rather than trusted to resemble `sample_luma`:
    /// the two are separate functions because they average in different spaces, and
    /// resemblance is exactly what stops being checked.
    #[test]
    fn the_reference_sampler_centres_clamps_and_stays_inside() {
        // Each channel is a different ramp, so a channel swap cannot pass: R is the
        // index, G is 100 over it, B is 200.
        let (w, h) = (5usize, 5usize);
        let img = raw_core::preview::Rgb8 {
            data: (0..w * h)
                .flat_map(|i| [i as u8, i as u8 + 100, i as u8 + 200])
                .collect(),
            w,
            h,
        };

        // A point reads one pixel: row 2, column 3 is index 13.
        assert_eq!(
            sample_rgb(&img, 3.4, 2.9, SampleArea::Point),
            Some([13, 113, 213])
        );

        // A 3x3 centred on index 12 averages to the centre itself, per channel.
        assert_eq!(
            sample_rgb(&img, 2.5, 2.5, SampleArea::Three),
            Some([12, 112, 212])
        );

        // At the corner the divisor is what was read — four pixels, mean 3 — and not
        // the window's nine, which would darken every border sample plausibly.
        assert_eq!(
            sample_rgb(&img, 0.5, 0.5, SampleArea::Three),
            Some([3, 103, 203])
        );

        // Off the picture is nothing, not the nearest edge.
        for (x, y) in [(-0.5, 2.0), (2.0, -0.5), (5.0, 2.0), (2.0, 5.0)] {
            assert_eq!(
                sample_rgb(&img, x, y, SampleArea::Three),
                None,
                "({x}, {y})"
            );
        }
    }

    #[test]
    fn every_sample_size_is_odd() {
        // An even window has no centre pixel and would sit half a pixel off the
        // cursor. This is the invariant the whole ladder is chosen for.
        for a in SampleArea::UI_ORDER {
            assert_eq!(a.edge() % 2, 1, "{} has an even edge", a.label());
            assert_eq!(a.radius(), a.edge() / 2);
        }
    }

    #[test]
    fn a_three_by_three_covers_a_full_bayer_quad_in_every_phase() {
        // The claim the 3x3 default rests on: the Bayer pattern has period 2, so
        // three consecutive rows and columns span both parities whatever pixel they
        // are centred on. Checked as coverage of the four quad positions rather than
        // asserted in a comment.
        let r = SampleArea::Three.radius();
        for phase_y in 0..2 {
            for phase_x in 0..2 {
                let mut seen = [false; 4];
                for dy in -r..=r {
                    for dx in -r..=r {
                        let (py, px) = ((phase_y + dy).rem_euclid(2), (phase_x + dx).rem_euclid(2));
                        seen[(py * 2 + px) as usize] = true;
                    }
                }
                assert!(
                    seen.iter().all(|s| *s),
                    "a 3x3 at phase ({phase_x}, {phase_y}) missed part of the quad: {seen:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;
    use raw_core::composition::{CompositionParams, Orientation};
    use raw_core::geometry::Dims;
    use tabs::{Pin, Pins};

    fn pins(at: &[(f32, f32)]) -> Pins {
        Pins {
            items: at.iter().map(|&(x, y)| Pin { x, y }).collect(),
            ..Pins::default()
        }
    }

    fn at(xs: &[(f32, f32)]) -> Vec<egui::Pos2> {
        xs.iter().map(|&(x, y)| egui::pos2(x, y)).collect()
    }

    #[test]
    fn a_press_takes_the_nearest_pin_not_the_first_one_in_range() {
        // Pins overlap at low zoom, and the one whose centre is closest to the press
        // is the one that was aimed at. Taking the first hit would hand the drag to
        // whichever happened to be placed earlier.
        let p = at(&[(100.0, 100.0), (104.0, 100.0)]);
        assert_eq!(
            tabs::nearest_to(p.iter().copied(), egui::pos2(103.0, 100.0), 10.0),
            Some(1)
        );
        assert_eq!(
            tabs::nearest_to(p.iter().copied(), egui::pos2(101.0, 100.0), 10.0),
            Some(0)
        );
    }

    #[test]
    fn nothing_is_grabbed_outside_the_radius() {
        let p = at(&[(100.0, 100.0)]);
        assert_eq!(
            tabs::nearest_to(p.iter().copied(), egui::pos2(100.0, 111.0), 10.0),
            None
        );
        assert_eq!(
            tabs::nearest_to(p.iter().copied(), egui::pos2(100.0, 109.0), 10.0),
            Some(0)
        );
    }

    #[test]
    fn a_pin_survives_a_rotation_because_it_is_stored_against_the_negative() {
        // The whole reason pins are in source coordinates. A pin placed on a detail
        // must still be on that detail after the picture is turned — if it were
        // stored in frame or screen space it would slide off, which is the defect
        // the footer readout itself had before 9a.
        let source = Dims { w: 400, h: 300 };
        let upright = raw_core::Frame::resolve(
            source,
            Orientation::default(),
            &CompositionParams::default(),
        );
        let turned = raw_core::Frame::resolve(
            source,
            Orientation::default(),
            &CompositionParams {
                orientation: Some(Orientation::default().right()),
                ..Default::default()
            },
        );

        // A pin two-thirds across and a quarter down the negative.
        let (sx, sy) = (266.0, 75.0);
        // Both frames put it somewhere on screen...
        let (ax, ay) = upright.from_source(sx, sy);
        let (bx, by) = turned.from_source(sx, sy);
        // ...at different places, because the picture turned...
        assert!(
            (ax - bx).abs() > 1.0 || (ay - by).abs() > 1.0,
            "the rotation moved nothing: ({ax}, {ay}) vs ({bx}, {by})"
        );
        // ...and both map back to the same pixel of the negative, which is the claim.
        for (f, (fx, fy)) in [(&upright, (ax, ay)), (&turned, (bx, by))] {
            let (rx, ry) = f.to_source(fx, fy);
            assert!(
                (rx - sx).abs() < 0.01 && (ry - sy).abs() < 0.01,
                "round trip landed on ({rx}, {ry}), not ({sx}, {sy})"
            );
        }
    }

    #[test]
    fn pin_colours_wrap_rather_than_running_out() {
        // Nothing caps the pin count, so the tenth must have a colour. Wrapping is
        // the honest answer: two pins sharing a hue is survivable, a panic is not.
        assert_eq!(tabs::pin_colour(0), tabs::PIN_COLOURS[0]);
        assert_eq!(tabs::pin_colour(9), tabs::PIN_COLOURS[0]);
        assert_eq!(tabs::pin_colour(13), tabs::PIN_COLOURS[4]);
    }

    #[test]
    fn hiding_keeps_the_pins_and_clearing_is_the_other_button() {
        // The distinction the two controls exist to draw. A `HIDE` that discarded
        // would make the safer-looking control the destructive one.
        let mut p = pins(&[(1.0, 1.0), (2.0, 2.0)]);
        p.hidden = true;
        assert_eq!(p.items.len(), 2, "hiding threw pins away");
        p.items.clear();
        assert!(p.items.is_empty());
    }

    #[test]
    fn pin_mode_says_what_it_is_and_how_to_leave() {
        // The affordance every mode in this app carries: a mode is invisible until
        // it surprises you. See `tabs::Mode::hint`.
        let hint = tabs::Mode::Pin.hint().expect("pin mode must state itself");
        assert!(hint.starts_with("PIN MODE"), "{hint}");
        assert!(
            hint.contains("i to exit"),
            "the way out is not named: {hint}"
        );
    }

    /// The comparison the GRAIN and SHARPENING blocks make to decide "did this frame
    /// edit anything", reproduced exactly — snapshot with `enabled` normalised out,
    /// compare after the body, arm on any difference.
    ///
    /// This is the part of the fix worth pinning. `widgets::arm` is now one `if`; what
    /// was actually wrong was asking `is_default()` instead of asking whether anything
    /// had *changed*, and that question is asked here rather than in the widget.
    #[test]
    fn a_slider_arms_a_module_that_was_edited_and_then_switched_off() {
        use raw_core::sharpen::SharpenParams;
        let norm = |p: SharpenParams| SharpenParams {
            enabled: false,
            ..p
        };

        // The state the old rule could never recover from: edited before — so not
        // default — and switched off on purpose. The sidecar stores exactly this, so
        // reopening the file lands straight back in it.
        let mut params = SharpenParams {
            enabled: false,
            amount: 0.9,
            ..Default::default()
        };
        assert!(
            !params.is_default(),
            "the setup is only interesting if it is non-default"
        );

        let before = norm(params);
        params.amount = 0.95; // the slider moves
        let edited = norm(params) != before;
        widgets::arm(edited, &mut params.enabled);
        assert!(
            params.enabled,
            "moving a slider on a bypassed module must arm it"
        );

        // And the dot still holds: a frame that changes nothing must not re-arm, or the
        // switch could never be turned off at all.
        params.enabled = false;
        let before = norm(params);
        let edited = norm(params) != before;
        widgets::arm(edited, &mut params.enabled);
        assert!(
            !params.enabled,
            "a frame with no edit switched the module back on"
        );
    }

    #[test]
    fn switching_a_module_off_is_not_itself_an_edit() {
        use raw_core::sharpen::SharpenParams;
        let norm = |p: SharpenParams| SharpenParams {
            enabled: false,
            ..p
        };

        // The bypass is applied *after* the comparison, which is what stops the dot
        // reading as a change to the module and instantly re-arming what it just
        // switched off. Normalising `enabled` out of both sides is the other half.
        let mut params = SharpenParams {
            enabled: true,
            amount: 0.9,
            ..Default::default()
        };
        let before = norm(params);
        let edited = norm(params) != before;
        params.enabled ^= true; // the dot
        widgets::arm(edited, &mut params.enabled);
        assert!(
            !params.enabled,
            "the dot armed the module it was used to switch off"
        );
    }

    #[test]
    fn pin_mode_claims_the_drag_so_the_picture_does_not_pan_under_it() {
        assert!(tabs::Mode::Pin.claims_drag());
        assert!(!tabs::Mode::View.claims_drag());
    }

    #[test]
    fn space_temporarily_pans_without_leaving_an_eligible_tool() {
        let mode = tabs::Mode::paint(
            raw_core::Sign::Dodge,
            paint::Tool::Brush,
            raw_core::DodgeBurnParams::default(),
        );
        assert!(!mode.pans_with_drag(false), "a normal brush drag would pan");
        assert!(
            mode.pans_with_drag(true),
            "Space did not lend the drag to pan"
        );
        assert!(
            mode.is_paint(),
            "the temporary hand replaced the brush mode"
        );
        let keystone = tabs::Mode::keystone(raw_core::CompositionParams::default());
        assert!(
            keystone.pans_with_drag(true),
            "Space did not lend a perspective drag to pan"
        );
        assert!(
            keystone.is_keystone(),
            "the temporary hand replaced the perspective tool"
        );
        assert!(
            !tabs::Mode::Crop {
                grabbed: None,
                entered: raw_core::CompositionParams::default(),
            }
            .pans_with_drag(true),
            "Space took a drag from the crop tool"
        );
    }

    #[test]
    fn curve_sampler_is_a_visible_one_shot_mode() {
        let mode = tabs::Mode::CurvePoint;
        let hint = mode.hint().expect("the sampler must announce itself");
        assert!(hint.starts_with("CURVE SAMPLER"), "{hint}");
        assert!(hint.contains("Esc"), "the way out is not named: {hint}");
        assert!(mode.claims_drag(), "the picture would pan while sampling");
    }

    /// A roomy viewport, so nothing in these is decided by the screen edge unless the
    /// test says so.
    const VIEW: egui::Rect = egui::Rect {
        min: egui::pos2(0.0, 0.0),
        max: egui::pos2(1000.0, 1000.0),
    };
    const SZ: egui::Vec2 = egui::vec2(60.0, 18.0);
    const OFF: f32 = 10.0;

    #[test]
    fn a_lone_pin_keeps_the_corner_the_prototype_gives_it() {
        // Up and right. Nothing else in this test — but it is the case every other one
        // is a departure from, so it is worth its own assertion: a pin with room around
        // it must not move just because the flip logic exists.
        let at = egui::pos2(500.0, 500.0);
        let b = pin_label_box(at, SZ, OFF, &[], &[at], 0, VIEW);
        assert_eq!(
            b.min,
            egui::pos2(510.0, 500.0 - OFF - SZ.y),
            "the lone pin moved"
        );
    }

    #[test]
    fn a_second_pin_flips_rather_than_burying_the_first() {
        // Two crosshairs close enough that the second pin's preferred box lands on the
        // first pin's box — which is what zooming out does to every pair of pins.
        let a = egui::pos2(500.0, 500.0);
        let b = egui::pos2(515.0, 505.0);
        let ats = [a, b];
        let first = pin_label_box(a, SZ, OFF, &[], &ats, 0, VIEW);
        let second = pin_label_box(b, SZ, OFF, &[first], &ats, 1, VIEW);
        assert!(
            !first.intersects(second),
            "the second label was placed on top of the first: {first:?} vs {second:?}"
        );
        // And the first one is where it always was — greedy in index order, so the
        // earlier pin never yields to the later one.
        assert_eq!(first.min, egui::pos2(510.0, 500.0 - OFF - SZ.y));
    }

    #[test]
    fn a_label_does_not_park_on_another_pins_crosshair() {
        // The collision that is not label-on-label: pin 2's mark sits exactly where
        // pin 1's preferred box would be, so that box hides the pixel pin 2 points at.
        // Nothing is `taken` yet — only the crosshair rules the corner out.
        let a = egui::pos2(500.0, 500.0);
        let other = egui::pos2(530.0, 480.0);
        let ats = [a, other];
        let placed = pin_label_box(a, SZ, OFF, &[], &ats, 0, VIEW);
        assert!(
            !placed.expand(2.0).contains(other),
            "the label covered the other pin's mark: {placed:?} over {other:?}"
        );
    }

    #[test]
    fn a_pin_in_the_top_right_corner_turns_its_label_inward() {
        // Up-and-right runs off two edges at once here, so the only fully-visible
        // corner is down-and-left. This is the zoomed-in case — a pin near the edge of
        // the window — rather than the crowded one.
        let at = egui::pos2(995.0, 5.0);
        let b = pin_label_box(at, SZ, OFF, &[], &[at], 0, VIEW);
        assert!(
            VIEW.contains_rect(b),
            "the label hung off the screen: {b:?}"
        );
        assert!(
            b.min.x < at.x && b.min.y > at.y,
            "expected the down-left corner, got {b:?}"
        );
    }

    #[test]
    fn being_clear_of_the_other_marks_outranks_being_on_screen() {
        // Every corner that fits on screen is buried, and the one that is clear runs
        // past the edge. A box half off the window is still readable; a box under
        // another box is not — so the clear one wins.
        let at = egui::pos2(5.0, 500.0);
        // Blanket the two right-hand corners, which are the only ones fully on screen.
        let taken = [egui::Rect::from_min_size(
            egui::pos2(0.0, 400.0),
            egui::vec2(200.0, 200.0),
        )];
        let b = pin_label_box(at, SZ, OFF, &taken, &[at], 0, VIEW);
        assert!(
            !b.intersects(taken[0]),
            "took a buried corner over an off-screen one: {b:?}"
        );
    }

    /// A histogram with its mapping built from default params, which is the identity
    /// chain: no exposure, linear curve, no tone map.
    fn flat_histogram() -> histogram::Histogram {
        use raw_core::geometry::{CfaColor::*, CfaGeometry, Dims};
        use raw_core::scene::{LumaImage, SceneImage};
        use raw_core::sensor::Gains;
        let dims = Dims { w: 4, h: 4 };
        let mut h = histogram::Histogram::default();
        let luma = LumaImage {
            data: vec![0.5; 16],
            output_dims: dims,
            source_dims: dims,
            clipped: Vec::new(),
        };
        let scene = SceneImage {
            data: vec![0.5; 16],
            geom: CfaGeometry::new(4, dims, 0, 0, 4, 4, [[Red, Green], [Green, Blue]]),
            gains: Gains([1.0, 1.0, 1.0]),
            camera: "test".into(),
            clipped: Vec::new(),
        };
        let params = raw_core::Params::default();
        let frame = raw_core::Frame::resolve(
            dims,
            raw_core::composition::Orientation::default(),
            &params.composition,
        );
        h.refresh(&luma, &scene, &params, 0, &frame);
        h
    }

    #[test]
    fn both_pin_stages_report_l_star_so_the_two_can_be_compared() {
        // The defect this replaced: `RAW` reported stops-below-clipping and `EDITED`
        // reported L*, so the one question the toggle exists to answer — what did my
        // developing do to this tone — could not be answered by reading the two
        // numbers, because they were in different units and did not subtract.
        let h = flat_histogram();
        let edited = h.sample(0.5).expect("a mapping was built").0;
        let raw = h.sample_undeveloped(0.5).expect("a mapping was built");
        for (name, v) in [("edited", edited), ("raw", raw)] {
            assert!(
                (0.0..=100.0).contains(&v),
                "{name} L* was {v}, off the 0-100 scale"
            );
        }
        // On the identity chain the two stages *are* the same reading, which is the
        // sharpest statement that they share a scale.
        assert!(
            (edited - raw).abs() < 0.01,
            "an undeveloped negative read {raw} raw and {edited} edited"
        );
    }

    #[test]
    fn headroom_above_white_clamps_rather_than_reporting_l_star_past_100() {
        // Scene-linear values run above 1.0 — that is what headroom is — and an L*
        // of 140 is not a lightness. The footer's EV is what answers how far above.
        let h = flat_histogram();
        assert_eq!(h.sample_undeveloped(3.7).map(|v| v.round()), Some(100.0));
        assert_eq!(h.sample_undeveloped(0.0), Some(0.0));
    }
}

#[cfg(test)]
mod crop_gesture_tests {
    /// Drive one frame with the given pointer events and report whether egui
    /// considers the pointer to be *in use* — the signal the whole undo and
    /// sidecar-write policy rests on.
    fn using_pointer(events: Vec<egui::Event>, ctx: &egui::Context) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(400.0, 300.0),
            )),
            events,
            ..Default::default()
        };
        let mut used = false;
        let _ = ctx.run_ui(input, |ui| {
            // Exactly what `viewport_panel` allocates for the image.
            let _ = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
            used = ui.ctx().egui_is_using_pointer();
        });
        used
    }

    fn press(at: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    #[test]
    fn a_drag_on_the_image_counts_as_a_gesture_for_undo_and_the_sidecar() {
        // **The assumption the crop tool inherited without asking.**
        //
        // Undo coalescing and the sidecar write both ride `egui_is_using_pointer`:
        // one entry and one write per *gesture*, not per frame. Every edit before
        // the crop tool came from a widget — a slider, a curve point — so the
        // signal was never in question. A crop drag is not a widget: it reads the
        // raw pointer against a rect the viewport allocated.
        //
        // If this were false during the drag, every frame of it would settle, and a
        // two-second crop adjustment would push a hundred undo entries and rewrite
        // the `.mono.xmp` a hundred times. That is disk churn and an undo stack that
        // cannot undo the gesture — neither of which announces itself.
        let ctx = egui::Context::default();
        let at = egui::pos2(200.0, 150.0);

        assert!(!using_pointer(Vec::new(), &ctx), "idle must settle");
        assert!(
            using_pointer(press(at), &ctx),
            "a press on the image is not a gesture"
        );

        // Still held, one frame later, having moved — the middle of a drag.
        let moved = egui::pos2(210.0, 160.0);
        assert!(
            using_pointer(vec![egui::Event::PointerMoved(moved)], &ctx),
            "the drag settled mid-gesture; every frame would write a sidecar"
        );

        // Released: the gesture closes, and this is the frame that commits.
        let release = vec![egui::Event::PointerButton {
            pos: moved,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }];
        assert!(
            !using_pointer(release, &ctx),
            "the gesture never closed; nothing would commit"
        );
    }
}
