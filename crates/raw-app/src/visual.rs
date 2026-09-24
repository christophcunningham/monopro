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
    assert!(develop_ready(h.state()), "{name}: the photograph never arrived");
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
    assert!(h.state().lightbox.active, "{name}: a folder should open in Lightbox");
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
