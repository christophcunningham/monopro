//! Frames of the real app, drawn with no window.
//!
//!     cargo test -p raw-app visual -- --ignored
//!
//! writes one PNG per scene to `target/visual/`, or to `$MONOPRO_VISUAL_DIR` when it
//! is set. A before-and-after pair is two runs into two folders:
//!
//!     MONOPRO_VISUAL_DIR=/somewhere/before cargo test -p raw-app visual -- --ignored
//!     # make the change
//!     MONOPRO_VISUAL_DIR=/somewhere/after  cargo test -p raw-app visual -- --ignored
//!
//! **This exists so a change to the interface can be looked at by whoever made it**,
//! including a coding agent, which otherwise cannot see its own work. It is not a
//! pixel-exact regression suite: nothing is compared against a stored image, and the
//! tests fail only when a scene cannot be drawn at all.
//!
//! # What makes a frame here the frame the window draws
//!
//! egui_kittest's defaults describe a different machine, and each one would quietly
//! change the picture rather than fail. Every one is overridden:
//!
//! - **2 points per pixel**, a Retina display. At kittest's 1.0 every glyph is
//!   hinted and rasterised at a size the app is never seen at.
//! - **macOS**, not kittest's Linux, so shortcut labels read ⌘ and not Ctrl.
//! - **the app's own device**, `crate::device_descriptor`, on the real adapter.
//!   kittest prefers a CPU adapter at egui's 8192 texture limit, and the working
//!   image of a large sensor does not fit in that.
//! - **the real `App::new`**, so fonts and theme are installed by `theme.rs` exactly
//!   as at launch. Nothing here sets a font or a style.
//!
//! Settings come from a `settings.toml` written into this thread's isolated test
//! storage before the app is built (see `settings::dir`), so a scene never reads the
//! preferences of whoever runs it. The folder tree still lists the machine's own
//! volumes, which is what the real panel shows.
//!
//! Two things are absent from every frame: the native menu bar, which AppKit only
//! builds on the main thread and which is not drawn by egui anyway, and the update
//! badge, because a test must never start the updater. See `App::ui`.
//!
//! Photographs come from the private corpus, `raws/` or `../raws` beside the
//! workspace (see `docs/private-fixtures.md`). A scene that needs one and cannot find
//! it is skipped with a note rather than failed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_kittest::Harness;

use crate::App;
use crate::settings::Settings;

/// The window size the scenes are drawn at, in points: a 14" MacBook Pro's default
/// scaled resolution, less nothing — the app runs full-window there.
const SIZE: egui::Vec2 = egui::vec2(1512.0, 945.0);

/// Long enough for a 100 MP decode on a laptop. A scene that is still not ready
/// after this is drawn anyway and reported, since a half-loaded frame is still
/// worth looking at.
const PATIENCE: Duration = Duration::from_secs(90);

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("MONOPRO_VISUAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("target/visual"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn corpus() -> Option<PathBuf> {
    let root = workspace();
    [root.join("raws"), root.join("../raws")]
        .into_iter()
        .find(|d| d.is_dir())
}

fn raw(name: &str) -> Option<PathBuf> {
    corpus().map(|d| d.join(name)).filter(|p| p.is_file())
}

/// The default grounds, and a light set: canvas, panels and modules all near
/// white, which is where the re-greying of text and controls is most likely to
/// have missed something.
#[derive(Clone, Copy)]
enum Ground {
    Dark,
    Light,
}

impl Ground {
    fn suffix(self) -> &'static str {
        match self {
            Self::Dark => "",
            Self::Light => "-light",
        }
    }

    fn settings(self) -> Settings {
        let mut s = Settings::default();
        if let Self::Light = self {
            s.viewer_background = 88.0;
            s.panel_matches_viewer = true;
            s.module_background = 94.0;
            s.lightbox_matches_viewer = true;
        }
        s
    }
}

/// **One scene at a time.** The app assumes one window per process, and some of its
/// drawing state is process-wide — `theme::set_module_ground` is a static — so two
/// scenes drawn in parallel paint each other's grounds. The first light frame came
/// out with dark module cards for exactly that reason. Every scene holds this for
/// its whole run.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    // A scene that panicked still leaves the next one a usable app.
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build the app as a launch with `path` on the command line would.
fn launch(path: Option<PathBuf>, ground: Ground) -> Harness<'static, App> {
    ground
        .settings()
        .save()
        .expect("write settings into the isolated test storage");
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(crate::device_descriptor);
    Harness::builder()
        .with_size(SIZE)
        .with_pixels_per_point(2.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .with_max_steps(64)
        .wgpu_setup(egui_wgpu::WgpuSetup::CreateNew(setup))
        .build_eframe(move |cc| App::new(cc, path))
}

/// Step the app in real time until `ready`, then a little longer so fades, the
/// histogram and anything scheduled off the back of the last result have landed.
///
/// Real time because the work is on real threads: decoding, thumbnails and the
/// luminance pass all report back through `request_repaint`, and a harness that
/// only steps as fast as it can would outrun them.
fn settle(h: &mut Harness<'static, App>, name: &str, ready: impl Fn(&App) -> bool) {
    let start = Instant::now();
    while !ready(h.state()) {
        if start.elapsed() > PATIENCE {
            eprintln!("{name}: not ready after {PATIENCE:?}; drawing it as it is");
            break;
        }
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    for _ in 0..30 {
        h.step();
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn save(h: &mut Harness<'static, App>, name: &str) {
    let path = out_dir().join(format!("{name}.png"));
    h.render()
        .unwrap_or_else(|e| panic!("{name}: render failed: {e}"))
        .save(&path)
        .unwrap_or_else(|e| panic!("{name}: cannot write {}: {e}", path.display()));
    eprintln!("{name}: {}", path.display());
}

fn develop_ready(app: &App) -> bool {
    app.tabs
        .active()
        .is_some_and(|t| t.has_image() && t.has_current_luma())
}

/// The corpus, cloned into a folder of this scene's own, so the Lightbox reads a
/// folder nothing else is writing to — scenes run in parallel. Clones are free on
/// APFS; `reflink_or_copy` copies elsewhere.
fn corpus_folder(scene: &str) -> Option<PathBuf> {
    let src = corpus()?;
    let dir = workspace().join("target/visual-fixtures").join(scene);
    std::fs::create_dir_all(&dir).ok()?;
    for entry in std::fs::read_dir(&src).ok()?.flatten() {
        let from = entry.path();
        let to = dir.join(entry.file_name());
        if from.is_file() && !to.exists() {
            reflink_copy::reflink_or_copy(&from, &to).ok()?;
        }
    }
    Some(dir)
}

fn scene_develop(ground: Ground) {
    let _turn = one_at_a_time();
    let name = format!("develop{}", ground.suffix());
    let Some(path) = raw("L1000016.DNG") else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    let mut h = launch(Some(path), ground);
    settle(&mut h, &name, develop_ready);
    assert!(
        develop_ready(h.state()),
        "{name}: the photograph never arrived"
    );
    save(&mut h, &name);
}

fn scene_lightbox(ground: Ground) {
    let _turn = one_at_a_time();
    let name = format!("lightbox{}", ground.suffix());
    let Some(folder) = corpus_folder(&name) else {
        eprintln!("{name}: skipped, no corpus folder");
        return;
    };
    let mut h = launch(Some(folder), ground);
    assert!(
        h.state().lightbox.active,
        "{name}: a folder should open in Lightbox"
    );
    settle(&mut h, &name, |app| app.lightbox.tiles_settled());
    save(&mut h, &name);
}

#[test]
#[ignore = "draws with the GPU; cargo test -p raw-app visual -- --ignored"]
fn empty() {
    let _turn = one_at_a_time();
    let mut h = launch(None, Ground::Dark);
    settle(&mut h, "empty", |_| true);
    save(&mut h, "empty");
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn develop() {
    scene_develop(Ground::Dark);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn develop_light() {
    scene_develop(Ground::Light);
}

/// Develop with the EXPORT module open: the master button, and the proof's format,
/// depth, space, size and dither. Tall, so the whole column is on screen.
fn scene_export(ground: Ground) {
    let _turn = one_at_a_time();
    let name = format!("export{}", ground.suffix());
    let Some(path) = raw("L1000016.DNG") else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    crate::widgets::open_for_a_test("EXPORT");
    ground
        .settings()
        .save()
        .expect("write settings into the isolated test storage");
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(crate::device_descriptor);
    let mut h = Harness::builder()
        .with_size(egui::vec2(SIZE.x, 1600.0))
        .with_pixels_per_point(2.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .with_max_steps(64)
        .wgpu_setup(egui_wgpu::WgpuSetup::CreateNew(setup))
        .build_eframe(move |cc| App::new(cc, Some(path)));
    settle(&mut h, &name, develop_ready);
    save(&mut h, &name);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn export() {
    scene_export(Ground::Dark);
}

/// One corpus raw, cloned into a folder of this scene's own, for a scene that edits
/// the picture. **The app saves a sidecar beside whatever it opened** when the
/// parameters differ from the saved ones, so a scene that moves a slider on the
/// corpus file itself leaves a `.mono.xmp` in the photographer's own folder and
/// changes how the file opens in their app. A copy takes the sidecar instead.
fn raw_copy(file: &str, scene: &str) -> Option<PathBuf> {
    let from = raw(file)?;
    let dir = workspace().join("target/visual-fixtures").join(scene);
    std::fs::create_dir_all(&dir).ok()?;
    let to = dir.join(file);
    if !to.exists() {
        reflink_copy::reflink_or_copy(&from, &to).ok()?;
    }
    // **A scene starts from the photograph and nothing else.** The folder outlives the
    // run and the app saves a sidecar into it, so without this the second run opens the
    // first one's edits and draws a picture that depends on what ran before it.
    let _ = std::fs::remove_file(raw_core::sidecar::path_for(&to));
    Some(to)
}

/// Develop with CURVE open and a mask on the picture: the Mask flag on each row, the
/// shapes of the selected one, the MASK group, and the tool's handles over the
/// photograph. Tall, so the whole module is on screen.
///
/// The first pass is masked by a sky grad and left unselected, so its icon shows what a
/// closed row says; the second is the selected one, with the tool open on a radial and a
/// tone range inside it.
fn scene_curve_mask(ground: Ground, look: Option<raw_gpu::MaskLook>) {
    use crate::paint::Pick;
    use raw_core::curve::CurveMask;

    let _turn = one_at_a_time();
    let name = format!(
        "curve-mask{}{}",
        match look {
            None => "",
            Some(raw_gpu::MaskLook::Overlay) => "-overlay",
            Some(raw_gpu::MaskLook::Mask) => "-view",
        },
        ground.suffix()
    );
    let Some(path) = raw_copy("L1000016.DNG", &name) else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    crate::widgets::open_for_a_test("CURVE");
    ground
        .settings()
        .save()
        .expect("write settings into the isolated test storage");
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(crate::device_descriptor);
    let mut h = Harness::builder()
        .with_size(egui::vec2(SIZE.x, 1900.0))
        .with_pixels_per_point(2.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .with_max_steps(64)
        .wgpu_setup(egui_wgpu::WgpuSetup::CreateNew(setup))
        .build_eframe(move |cc| App::new(cc, Some(path)));
    settle(&mut h, &name, develop_ready);
    assert!(
        develop_ready(h.state()),
        "{name}: the photograph never arrived"
    );

    {
        let tab = h.state_mut().tabs.active_mut().expect("a tab");
        let frame = tab.frame().expect("a frame");
        let size = tab
            .luma
            .as_ref()
            .map(|l| (l.output_dims.w as f32, l.output_dims.h as f32))
            .expect("the working image");
        let sky = crate::curve_mask::starting_shape(Pick::Linear, &frame, size);
        let spot = crate::curve_mask::starting_shape(Pick::Radial, &frame, size);
        let curve = &mut tab.params.curve;
        curve.instances[0].curve.add(0.5, 0.68);
        curve.instances[0].mask = Some(CurveMask::of_shape(sky));
        let second = curve.add_instance().expect("room for a second curve");
        curve.instances[second].curve.add(0.4, 0.3);
        curve.instances[second].opacity = 0.7;
        curve.instances[second].mask = Some(CurveMask::of_shape(spot));
        // A tone range inside the radial, so the Tone section, its ruler and the Join chips
        // are all on screen.
        if let Some(mask) = &mut curve.instances[second].mask {
            mask.tone = Some(raw_core::dodgeburn::ZoneMask {
                enabled: true,
                lo: -2.0,
                hi: 1.0,
                f_lo: 1.0,
                f_hi: 1.25,
                ..Default::default()
            });
        }
        tab.curve_active = second;
        tab.mask_pin = look.map(|look| (crate::tabs::MaskOwner::Curve, look));
        // The tool's handles over the plain picture; a pinned look is drawn without
        // them, as it is when the mask is looked at and not adjusted.
        if look.is_none() {
            tab.mode = crate::tabs::Mode::curve_mask(second, tab.params.curve.clone());
        }
    }
    settle(&mut h, &name, |_| true);
    save(&mut h, &name);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask() {
    scene_curve_mask(Ground::Dark, None);
}

/// The same scene with the mask kept on the picture, each way: red film over what the
/// selected instance's mask protects, and the mask alone in the picture's place.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask_overlay() {
    scene_curve_mask(Ground::Dark, Some(raw_gpu::MaskLook::Overlay));
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask_view() {
    scene_curve_mask(Ground::Dark, Some(raw_gpu::MaskLook::Mask));
}

/// A tone island: a range set on the sea and limited to what is connected to its seed,
/// so the cliff's rock of the same tones is left out. The eyedropper is armed, which is
/// when the seed is drawn on the picture, and the film is up to show what the island
/// kept.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask_island() {
    use crate::tabs::ToneTarget;

    let _turn = one_at_a_time();
    let name = "curve-mask-island";
    let Some(path) = raw_copy("L1000016.DNG", name) else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    crate::widgets::open_for_a_test("CURVE");
    Ground::Dark
        .settings()
        .save()
        .expect("write settings into the isolated test storage");
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(crate::device_descriptor);
    let mut h = Harness::builder()
        .with_size(egui::vec2(SIZE.x, 1900.0))
        .with_pixels_per_point(2.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .with_max_steps(64)
        .wgpu_setup(egui_wgpu::WgpuSetup::CreateNew(setup))
        .build_eframe(move |cc| App::new(cc, Some(path)));
    settle(&mut h, name, develop_ready);
    assert!(develop_ready(h.state()), "{name}: the photograph never arrived");

    {
        let tab = h.state_mut().tabs.active_mut().expect("a tab");
        tab.params.curve.instances[0].curve.add(0.45, 0.6);
        crate::curve_mask::flip_tone(&mut tab.params.curve.instances[0]);
        let tone = tab
            .tone_range_mut(ToneTarget::Curve(0))
            .expect("the tone part just switched on");
        // The sea's tones, read per pixel, and one seed out on the water.
        tone.lo = -1.4;
        tone.hi = -0.4;
        tone.edge_aware = false;
        tone.island.enabled = true;
        assert!(tone.island.set((0.4, 0.77)));
        tab.mask_pin = Some((crate::tabs::MaskOwner::Curve, raw_gpu::MaskLook::Overlay));
        tab.ask_tone_sampler(ToneTarget::Curve(0), crate::widgets::SamplerAsk::Arm);
        // The section was drawn closed while the curve had no mask; open it, so the
        // range and its Island switch are on screen.
        tab.curve_mask_reveal = Some(0);
    }
    settle(&mut h, name, |_| true);
    save(&mut h, name);
}

/// A painted mask: the brush open on a curve whose mask is strokes, with the film up so
/// what has been painted shows. Two passes and an eraser pass through the first, so the
/// MASK section's brush controls, the cursor's absence at rest and the histogram inside
/// the paint are all on screen.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask_painted() {
    use raw_core::curve::CurveMask;
    use raw_core::dodgeburn::{Dab, Gesture, Nib, Shape};

    let _turn = one_at_a_time();
    let name = "curve-mask-painted";
    let Some(path) = raw_copy("L1000016.DNG", name) else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    crate::widgets::open_for_a_test("CURVE");
    Ground::Dark
        .settings()
        .save()
        .expect("write settings into the isolated test storage");
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(crate::device_descriptor);
    let mut h = Harness::builder()
        .with_size(egui::vec2(SIZE.x, 1900.0))
        .with_pixels_per_point(2.0)
        .with_os(egui::os::OperatingSystem::Mac)
        .with_max_steps(64)
        .wgpu_setup(egui_wgpu::WgpuSetup::CreateNew(setup))
        .build_eframe(move |cc| App::new(cc, Some(path)));
    settle(&mut h, name, develop_ready);
    assert!(develop_ready(h.state()), "{name}: the photograph never arrived");

    {
        let stroke = |from: (f32, f32), to: (f32, f32), paint: bool| {
            // As close together as the brush lays them, a quarter of the radius, so the
            // pass reads as a stroke and not as beads.
            Gesture::new(
                (0..=40)
                    .map(|i| {
                        let t = i as f32 / 40.0;
                        Dab {
                            x: from.0 + (to.0 - from.0) * t,
                            y: from.1 + (to.1 - from.1) * t,
                            radius: 0.07,
                            feather: 0.5,
                            opacity: 1.0,
                            ev: if paint { 1.0 } else { -1.0 },
                            ..Dab::ROUND
                        }
                    })
                    .collect(),
            )
        };
        let tab = h.state_mut().tabs.active_mut().expect("a tab");
        let curve = &mut tab.params.curve;
        curve.instances[0].curve.add(0.45, 0.6);
        curve.instances[0].mask = Some(CurveMask::of_shape(Shape::Brush {
            nib: Nib::Round,
            passes: vec![
                stroke((0.2, 0.35), (0.75, 0.3), true),
                stroke((0.3, 0.7), (0.8, 0.75), true),
                stroke((0.5, 0.2), (0.45, 0.5), false),
            ],
        }));
        tab.curve_active = 0;
        tab.mask_pin = Some((crate::tabs::MaskOwner::Curve, raw_gpu::MaskLook::Overlay));
        // The section was drawn closed before the mask was put here; open it, as a pick
        // would have found it.
        tab.curve_mask_reveal = Some(0);
        tab.mode = crate::tabs::Mode::curve_mask(0, tab.params.curve.clone()).with_brush(true);
    }
    settle(&mut h, name, |_| true);
    save(&mut h, name);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn curve_mask_light() {
    scene_curve_mask(Ground::Light, None);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn lightbox() {
    scene_lightbox(Ground::Dark);
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn lightbox_light() {
    scene_lightbox(Ground::Light);
}

/// The Metadata pane on a selected frame: the IPTC fields in the maintainer's order,
/// and the stepping row under the template menu.
fn scene_lightbox_metadata(ground: Ground) {
    let _turn = one_at_a_time();
    let name = format!("lightbox-metadata{}", ground.suffix());
    let Some(folder) = corpus_folder(&name) else {
        eprintln!("{name}: skipped, no corpus folder");
        return;
    };
    let mut h = launch(Some(folder), ground);
    settle(&mut h, &name, |app| app.lightbox.tiles_settled());
    let lightbox = &mut h.state_mut().lightbox;
    lightbox.step(true);
    lightbox.step(true);
    lightbox.reveal_pane(crate::lightbox::Pane::Exif);
    lightbox.expand_iptc_more();
    settle(&mut h, &name, |app| app.lightbox.tiles_settled());
    save(&mut h, &name);

    // And scrolled to the end, for the fields below the fold.
    h.event(egui::Event::PointerMoved(egui::pos2(150.0, 500.0)));
    for _ in 0..6 {
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -400.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        });
        h.step();
    }
    settle(&mut h, &name, |app| app.lightbox.tiles_settled());
    save(&mut h, &format!("{name}-scrolled"));
}

#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn lightbox_metadata() {
    scene_lightbox_metadata(Ground::Dark);
}

/// Dodge & Burn with a stack of layers: the SHAPE row's glyphs, and each layer's mask
/// flag, lit on the two that have a tone mask. The selected layer is open, so its
/// TONE MASK shows the same state as its flag.
///
/// **The layers go on a copy of the corpus raw**, in a folder of this scene's own. A
/// changed stack saves a sidecar beside the image it belongs to, and on the corpus
/// file that would be a stray edit in the maintainer's own photographs.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn dodgeburn() {
    scene_dodgeburn(None);
}

/// The same stack with the selected layer's mask kept on the picture as red film: the
/// radial through its tone mask, drawn by the code that draws a Curve's.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn dodgeburn_overlay() {
    scene_dodgeburn(Some(raw_gpu::MaskLook::Overlay));
}

fn scene_dodgeburn(look: Option<raw_gpu::MaskLook>) {
    use raw_core::Sign;
    use raw_core::dodgeburn::{Instance, Linear, Nib, Radial, Shape, ZoneMask};

    let _turn = one_at_a_time();
    let name = if look.is_some() {
        "dodgeburn-overlay"
    } else {
        "dodgeburn"
    };
    let Some(src) = raw("L1000016.DNG") else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    let dir = workspace().join("target/visual-fixtures").join(name);
    std::fs::create_dir_all(&dir).expect("create the scene's fixture folder");
    let path = dir.join("L1000016.DNG");
    if !path.exists() {
        reflink_copy::reflink_or_copy(&src, &path).expect("copy the corpus raw");
    }
    // The last run's layers would otherwise open underneath this run's.
    let _ = std::fs::remove_file(raw_core::sidecar::path_for(&path));

    let mut h = launch(Some(path), Ground::Dark);
    settle(&mut h, name, develop_ready);
    {
        let app = h.state_mut();
        app.layout.bring_forward(crate::layout::Pane::DodgeBurn);
        let tab = app.tabs.active_mut().expect("the photograph's tab");
        let (_, lo, hi, f_lo, f_hi) = ZoneMask::PRESETS[2];
        let highlights = ZoneMask {
            enabled: true,
            lo,
            hi,
            f_lo,
            f_hi,
            ..ZoneMask::default()
        };
        let mut sky = Instance::of(
            Sign::Burn,
            "Sky".into(),
            Shape::Linear(Linear {
                x0: 0.5,
                y0: 0.0,
                x1: 0.5,
                y1: 0.45,
                feather: 1.0,
                ev: -0.8,
            }),
        );
        sky.mask = highlights;
        let mut face = Instance::of(
            Sign::Dodge,
            "Face".into(),
            Shape::Radial(Radial {
                cx: 0.5,
                cy: 0.45,
                inner: 0.05,
                outer: 0.25,
                aspect: 1.0,
                angle: 0.0,
                feather: 1.0,
                invert: false,
                ev: 0.4,
            }),
        );
        face.mask = highlights;
        let edge = Instance::of(
            Sign::Burn,
            "Edge".into(),
            Shape::Brush {
                nib: Nib::Card,
                passes: Vec::new(),
            },
        );
        let db = &mut tab.params.dodgeburn;
        db.instances = vec![sky, face, edge];
        // Newest is drawn first, so the open layer is the middle row. With the film up it
        // is the sky's instead: a grad through the highlights, which selects something
        // to see, where the radial in the middle of the frame has no highlights in it.
        tab.db_active = Some(if look.is_some() { 0 } else { 1 });
        tab.mask_pin = look.map(|look| (crate::tabs::MaskOwner::Layer, look));
    }
    settle(&mut h, name, |_| true);
    save(&mut h, name);
}

/// Recent folders and images for the start page to list: the workspace's own folders
/// and the corpus, so every row is a path that exists on the machine drawing it.
fn seed_recent(app: &mut App) {
    let root = workspace();
    app.recent.folders = ["crates", "docs", "icons", "packaging", "fonts"]
        .iter()
        .map(|d| root.join(d))
        .chain(corpus())
        .filter(|d| d.is_dir())
        .filter_map(|d| std::fs::canonicalize(d).ok())
        .collect();
    let mut images: Vec<PathBuf> = corpus()
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    images.sort();
    images.truncate(crate::splash::RECENT);
    app.recent.images = images;
}

/// Lightbox with no folder chosen: the start page where the grid would be.
#[test]
#[ignore = "draws with the GPU; cargo test -p raw-app visual -- --ignored"]
fn start_lightbox() {
    let _turn = one_at_a_time();
    let mut h = launch(None, Ground::Dark);
    seed_recent(h.state_mut());
    h.state_mut().lightbox.close_folder();
    settle(&mut h, "start-lightbox", |_| true);
    save(&mut h, "start-lightbox");
}

/// Develop with no image open.
#[test]
#[ignore = "draws with the GPU; cargo test -p raw-app visual -- --ignored"]
fn start_develop() {
    let _turn = one_at_a_time();
    let mut h = launch(None, Ground::Dark);
    seed_recent(h.state_mut());
    h.state_mut().set_lightbox(false);
    settle(&mut h, "start-develop", |_| true);
    save(&mut h, "start-develop");
}

/// Home over an open image: the page laid over Develop, with the tab still open.
#[test]
#[ignore = "draws with the GPU and the private corpus; see the module note"]
fn start_home() {
    let _turn = one_at_a_time();
    let name = "start-home";
    let Some(path) = raw("L1000016.DNG") else {
        eprintln!("{name}: skipped, no corpus raw L1000016.DNG");
        return;
    };
    let mut h = launch(Some(path), Ground::Dark);
    settle(&mut h, name, develop_ready);
    seed_recent(h.state_mut());
    h.state_mut().go_home();
    assert!(
        h.state().home_showing(),
        "{name}: Home should lay the page over the tab"
    );
    settle(&mut h, name, |_| true);
    save(&mut h, name);
    assert_eq!(
        h.state().tabs.len(),
        1,
        "{name}: Home must not close the tab"
    );
}
