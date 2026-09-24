//! `settings.toml` — application preferences.
//!
//! Three kinds of persisted state exist in this app and they are not the same
//! thing. Keeping them apart is the point:
//!
//! | | What it is | Where |
//! |---|---|---|
//! | `<stem>.mono.xmp` | one image's edit | beside the raw |
//! | `app.ron` | **memory** — where the window was, what folder you were in | eframe's storage dir |
//! | `settings.toml` | **choices** — what you configured | beside `app.ron` |
//!
//! Memory is remembered; settings are chosen. Nobody configures where the window
//! was, and a stored choice with no UI to change it is indistinguishable from one
//! that does nothing — which is why every field here has a working control in the
//! menu. Future preferences remain in the documentation until their behaviour exists.
//!
//! # Why TOML and serde here, and not for the sidecar
//!
//! The sidecar is a **compatibility surface**: other applications read its `dc:` and
//! `xmp:` fields, and old files have to keep meaning what they meant. Its wire
//! format is therefore chosen, and hand-written, so that renaming a Rust field
//! cannot silently change what an old file means.
//!
//! This file is read by nothing but this app, and its job is to be **hand-editable**.
//! Those are different requirements, and TOML with `#[serde(default)]` on every field
//! meets them: an older file is missing keys and gets defaults, a newer one has extra
//! keys and they are ignored.
//!
//! # Enums are stored as stable keys, not as Rust variants
//!
//! `sampling = "superpixel"`, not a derived representation of `Sampling`. Two
//! reasons. Renaming or reordering a variant must not change the file, and — the one
//! that actually bites — `Sampling::Demosaic(algo)` carries a payload that is a
//! *separate setting* here, so the derived shape would be wrong as well as fragile.
//! `Container::key` established the pattern; this follows it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use raw_core::scene::{DemosaicAlgo, Sampling, Weighting};
use raw_core::sidecar::Loaded;
use raw_core::{OutputParams, Params, ToneMap, Unit};
use serde::{Deserialize, Serialize};

use crate::export;

/// File name inside eframe's storage directory.
///
/// **Beside `app.ron`, deliberately.** On macOS that is Application Support rather
/// than a config directory, which is not where a hand-editable preferences file
/// would ideally live — but one findable place for everything this app persists
/// beats two correct-but-separate ones, and eframe already chose the first.
const FILE: &str = "settings.toml";

/// The name eframe keys the storage directory on — and therefore the identity of
/// everything this app remembers: `app.ron`, `settings.toml`, the thumbnail cache.
///
/// **`MONOPRO_APP_ID` overrides it**, which exists for one specific and recurring
/// need: running two builds of this app side by side without them editing each
/// other's memory. A checkout on a branch shares the default name with the checkout
/// on `main`, so the branch quietly rewrites the window geometry, panel layout and
/// last folder of the build you were comparing it against.
///
/// An environment variable rather than a flag because it has to be read before
/// `run_native` and before any `App` exists to parse arguments, and because the
/// intent is "this whole process runs in that profile" rather than a per-launch
/// choice. Unset, everything behaves exactly as it did.
pub fn app_id() -> String {
    std::env::var("MONOPRO_APP_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "monopro".to_owned())
}

#[cfg(not(test))]
pub fn dir() -> Option<PathBuf> {
    eframe::storage_dir(&app_id())
}

// Each test thread owns its storage, including when a real app profile is selected.
#[cfg(test)]
pub fn dir() -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Storage(PathBuf);
    impl Storage {
        fn new() -> Self {
            loop {
                let serial = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "monopro-test-storage-{}-{serial}",
                    std::process::id()
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("cannot create isolated test storage: {e}"),
                }
            }
        }
    }
    impl Drop for Storage {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    thread_local! {
        static STORAGE: Storage = Storage::new();
    }
    Some(STORAGE.with(|storage| storage.0.clone()))
}

#[cfg(test)]
mod profile_tests {
    #[test]
    fn purging_another_tests_cache_cannot_touch_this_tests_files() {
        let root = super::dir().unwrap();
        assert_ne!(Some(root.clone()), eframe::storage_dir(&super::app_id()));
        let cache = root.join("thumbcache");
        std::fs::create_dir_all(&cache).unwrap();
        let marker = cache.join("keep.jpg");
        std::fs::write(&marker, b"keep").unwrap();
        let other = std::thread::spawn(|| {
            crate::lightbox::purge_cache();
            super::dir().unwrap()
        })
        .join()
        .unwrap();
        assert_ne!(root, other);
        assert_eq!(std::fs::read(marker).unwrap(), b"keep");
    }

    /// The default has to survive, because every existing install depends on it:
    /// change it and the app forgets its window, its panels and its preferences.
    ///
    /// Reading the real variable rather than setting it — `set_var` is unsafe and
    /// process-global, and a sibling test running in parallel would see it.
    #[test]
    fn the_default_profile_is_the_name_every_install_already_uses() {
        if std::env::var("MONOPRO_APP_ID").is_ok() {
            return; // running under an override, which is itself the feature working
        }
        assert_eq!(super::app_id(), "monopro");
        assert!(super::path().is_some_and(|p| p.ends_with("settings.toml")));
    }
}

pub fn path() -> Option<PathBuf> {
    dir().map(|d| d.join(FILE))
}

/// Rebuildable thumbnails live in the platform cache tree in production. Tests keep
/// them under their thread-local storage root so parallel cache tests cannot touch the
/// user's cache or one another.
#[cfg(not(test))]
pub fn cache_dir() -> Option<PathBuf> {
    crate::platform::cache_dir(&app_id())
}

#[cfg(test)]
pub fn cache_dir() -> Option<PathBuf> {
    dir().map(|root| root.join("thumbcache"))
}

/// 0.09 display-encoded, on the 0-100 scale. Also the panels' default, so out of
/// the box the panels and the canvas are one ground.
const DEFAULT_VIEWER_BACKGROUND: f32 = 9.0;

/// `theme::CHROME` (grey 38) on the 0-100 scale.
const DEFAULT_MODULE_BACKGROUND: f32 = 15.0;

/// Everything the Settings menu configures.
///
/// `#[serde(default)]` on the struct is what makes a partial file legal: any key the
/// running version does not find takes the value from `Default`, so an older
/// `settings.toml` opens cleanly and a newer one does not break an older build.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // -- 1. File naming
    /// Appended to the stem on export. Empty means no suffix.
    ///
    /// The defaults say what the file *is*: `_monopro` is the master, `_monoproof` a
    /// proof. A master is always a TIFF and a proof always a PNG or JPEG, so the keys
    /// keep their container names from when the choice was per container; renaming
    /// them would reset every custom suffix. That the two differ is the point — a TIFF and a PNG exported from one
    /// frame land in the same folder, and identical stems would make the second
    /// silently offer to overwrite nothing while looking like it might.
    pub tiff_suffix: String,
    pub png_suffix: String,

    // -- 2. Output
    pub output_folder: Option<PathBuf>,
    /// The colour space a **master** is written in. See [`Settings::master_space`].
    ///
    /// Named for TIFF because a master is one; the key predates masters being TIFF
    /// only, when PNG and JPEG had keys of their own. Those are gone — a PNG or JPEG
    /// is a proof, and a proof has `proof_space`.
    pub tiff_color_space: String,

    // -- 3. Default pipeline options, as stable keys. See `Settings::params`.
    pub sampling: String,
    pub demosaic: String,
    pub weighting: String,
    /// The custom mix, kept so selecting `weighted` has something to mean.
    pub weighting_mix: [f32; 3],
    pub contrast_mask: bool,
    pub tone_map: String,
    /// `in` or `cm`. **A display preference, not a stored size** — every print size in
    /// the app is canonically inches, and this only decides how they are shown and
    /// typed. It lives here rather than in `Params` because it is a property of the
    /// person, not of the picture: nobody wants one image measured in inches and the
    /// next in centimetres.
    pub print_unit: String,
    /// The default print resolution a **new** image opens at. Per-image after that.
    pub print_ppi: f32,

    // -- 4. Viewer background, and the surround
    /// 0–100. The canvas the image sits on, *not* the border around it.
    pub viewer_background: f32,
    /// 0–100, the same scale as `viewer_background`. The side panels' ground when
    /// `panel_matches_viewer` is off.
    pub panel_background: f32,
    /// Whether the side panels follow `viewer_background` instead of
    /// `panel_background`.
    pub panel_matches_viewer: bool,
    /// 0–100, the same scale as `viewer_background`. The ground of every module
    /// card, independent of both the canvas and the panels. The default is
    /// `theme::CHROME`, which is what the modules were designed on.
    pub module_background: f32,
    /// The border around the image — a mount, the way a print sits on one. Whether it
    /// is shown belongs to the image tab; Settings owns only the reference mount's
    /// geometry and colour.
    pub surround_width: f32,
    /// OKHSL: hue in degrees, saturation and lightness in `[0, 1]`. Perceptually
    /// uniform in lightness, so moving hue does not change how bright the mount
    /// reads against the print.
    pub surround_okhsl: [f32; 3],
    /// TPDF dither on the **screen**. The viewer draws into an 8-bit texture, where a
    /// smooth gradient bands visibly although the file does not; about one level of
    /// noise, locked to the photograph's pixels, breaks the bands up. A property of
    /// the monitor rather than of any picture, so it is a preference and not in the
    /// sidecar. Files have their own: see `proof_dither`.
    pub screen_dither: bool,

    // -- 5. Behaviour
    /// **On means today's behaviour.** Off makes a new file inherit the develop
    /// settings of the tab that was active when it opened — but never over its own
    /// sidecar.
    pub reset_on_open: bool,
    /// Whether authored IPTC, subject keywords, rating and label travel with an
    /// exported file.
    ///
    /// **On by default.** These are fields a person filled in on purpose; nobody types
    /// a copyright line and then wants it stripped on the way out. The privacy argument
    /// for defaulting off applies to what a *camera* wrote unasked, GPS above all, and
    /// none of that is in the sidecar.
    ///
    /// **A preference, not a per-image parameter.** It began in `OutputParams` and
    /// the maintainer moved it here, which is right: whether your credit line travels is a
    /// standing decision about how you work, and answering it per picture would mean
    /// answering it again for every picture. Same reasoning as the print unit.
    pub export_metadata: bool,
    /// Draw the amber rule around a tile whose frame has been worked on. **Live.**
    ///
    /// **On by default**, because "which of these have I already been through" is the
    /// question a contact sheet exists to answer, and the rule is how it answers. Off
    /// for the pass where that is not the question — judging a folder on the pictures
    /// alone, with nothing on the sheet that is about your own history with it.
    ///
    /// It hides the mark, not the fact: the sidecar is still there, the EXIF pane still
    /// reports it, and the Edited filter still finds it.
    pub lightbox_edited_mark: bool,
    /// Show a worked-on frame's edit in the grid instead of the camera's JPEG. **Live.**
    ///
    /// The doc here said "still unbuilt" long after it stopped being true, which is the
    /// hazard of a comment describing a plan. It was unbuilt in the shape it was first
    /// conceived — *render the tile from the sidecar while browsing* — and that shape
    /// was correctly refused on cost: a full `SensorImage::load`, a scene decode, a
    /// luminance derivation and a GPU pass per tile, against ~54 ms for the camera's
    /// embedded JPEG.
    ///
    /// **It shipped by a different route.** Develop already holds the picture on the GPU
    /// when it leaves a frame, so it writes the tile then and the grid simply prefers
    /// that file when this preference is on. The derived tile is kept regardless of this
    /// setting because Contact Sheet has its own Developed choice. No render at browse
    /// time, no device per worker; this setting controls only what the Lightbox grid shows.
    pub lightbox_xmp_thumbnails: bool,
    /// Show Lightbox thumbnails in gray. **Live**, and also a footer toggle.
    ///
    /// On by default because this is a monochrome editor: the contact sheet should
    /// open in the same visual language as Develop. Turning it off is remembered,
    /// so color remains one click away without becoming a session-only surprise.
    pub lightbox_gray: bool,
    /// Lightbox follows the viewer's canvas, panel and module values. On by default so
    /// the two modes look like one app; off, the three below take over.
    pub lightbox_matches_viewer: bool,
    /// 0–100. The grid behind the thumbnails.
    pub lightbox_canvas: f32,
    /// 0–100. Folders, Search, Favorites and EXIF.
    pub lightbox_panel: f32,
    /// 0–100. The thumbnail cards.
    pub lightbox_module: f32,
    /// The one local folder shown at the top level of FOLDERS. `None` means the
    /// current user's Home folder, which keeps a settings file portable between
    /// machines and accounts. Mounted external drives and cards are discovered
    /// separately and remain visible regardless of this choice.
    pub lightbox_folder_root: Option<PathBuf>,
    /// Export without the save dialog when an output folder is configured. **Live.**
    ///
    /// Gated on the folder existing, because the preference means "do not ask again once
    /// I have told you where these go" rather than "never ask" — with nowhere to write,
    /// a dialog is the only honest thing to show. It also falls back to the dialog when
    /// the export name is already taken: the confirmation being skipped is the dialog's
    /// own, so the overwrite check has to move rather than vanish.
    pub quick_export: bool,
    /// Whether the Lightbox sort survives a restart. **Live.**
    pub remember_lightbox_sort: bool,
    /// Frameless tiles at startup. **Live**, and also a footer toggle — the two write
    /// the same value rather than each keeping a copy.
    pub frameless_tiles: bool,
    /// Filenames under the Lightbox tiles. **Live**, with a footer toggle beside the
    /// frameless one.
    pub lightbox_filenames: bool,
    /// Show the current folder's subfolders as tiles in the grid. **Live.**
    ///
    /// **Off by default, and it reverses a decision rather than filling a gap.**
    /// The Lightbox brief's open question 3 cut folder tiles, on the argument that the
    /// FOLDERS panel is the place you navigate and a grid mixing two kinds of thing is
    /// harder to scan than one that does not. That argument still holds for the
    /// default; what it does not justify is refusing the arrangement to somebody who
    /// works the other way, from a folder of shoots rather than a folder of frames.
    ///
    /// A folder tile is exempt from the filters — see `Filters::admits`. It has no
    /// rating to satisfy them with, and a two-star filter that silently removed the
    /// way out of the folder would be the grid trapping you in it.
    pub lightbox_folders: bool,
    /// List files the grid cannot draw, as their file-type icon. **Live.**
    ///
    /// **Off by default**, because the browser is for finding a frame and a shoot
    /// folder full of `.txt` release forms is noise while you are doing that. On, it
    /// answers the other complaint the listing rule already conceded once: *a browser
    /// that cannot show you a file you can see in the Finder is the browser being
    /// wrong*. That reasoning admitted JPEGs and PNGs; this finishes it.
    ///
    /// These tiles never enter Develop — there is nothing to decode — so a double
    /// click on one does nothing. See `Kind::Other`.
    pub lightbox_other_files: bool,
    /// Start every session with the shipped panel layout — "fresh start". **Live.**
    ///
    /// **On by default, which is the maintainer's call and reverses the one this shipped with.**
    /// The argument for off was that remembering where you put your panels is the correct
    /// behaviour; what that misses is which state a *remembered* layout actually is. A
    /// panel width is not a preference somebody set on purpose — it is wherever a drag
    /// last happened to end, often mid-experiment, and restoring it means the app opens
    /// looking like the middle of a session you have already forgotten. the maintainer described
    /// exactly that: launching and loading a photo, and finding the panel widths and the
    /// active tab from whatever he was testing last.
    ///
    /// Defaulting on makes the app open the same way every time, which is the thing a
    /// layout you did not choose cannot do. Turning it off is how you keep an arrangement
    /// you *did* choose.
    ///
    /// It resets **both** trees — Develop's and the Lightbox's — because "my panels are
    /// wrong" is not a complaint anyone makes about one mode at a time. That includes the
    /// active tab: `default_tree` opens on Develop and Info, so a session left in Toning
    /// does not come back in it. Favourites, the last folder and the window's own size
    /// and position all survive — none of those is panel layout.
    pub reset_panels_on_start: bool,
    /// Open the app in Develop rather than in the Lightbox. **Live.**
    ///
    /// **Off by default, because the default is a decision and not an accident.** The
    /// app opens in the Lightbox on the prototype's reasoning — you arrive wanting to
    /// find a frame, not already holding one — and that stays the shipped answer. This
    /// is for the other way of working: if you come back to the same picture across
    /// several sittings, the browser is a keypress in the way every single launch.
    ///
    /// It has no effect when a file is named on the command line or dropped on the
    /// icon. That already opens in Develop, so the preference has nothing to decide.
    pub start_in_develop: bool,

    // -- 6. Input
    /// Flip the scroll wheel. **Not a per-gesture setting** — it flips whatever
    /// scroll does, which today is zoom and tomorrow may be more, so it stays a
    /// statement about the wheel rather than about zooming.
    pub invert_scroll: bool,
    /// Whether the wheel zooms at all. Off leaves the keys, which are exact.
    pub scroll_zoom: bool,
    /// The whole hotkey table, on or off.
    ///
    /// **`,` is exempt and cannot be disabled.** Turning every key off from a window
    /// you reached with a key, in an app with no menu bar until the OS menu lands,
    /// would leave no way back into this window — and the settings file is not
    /// somewhere a user should have to go with a text editor. The same rule the
    /// PANELS HIDDEN footer exists for: *a key that can stop working must not be the
    /// sole inverse of a gesture.*
    pub hotkeys_enabled: bool,
    /// Hover tooltips. On is the app as built; off is for someone who knows it.
    pub tooltips: bool,
    /// Ask before quitting with a scratch duplicate that has never been saved.
    ///
    /// A duplicate writes no sidecar by design — two tabs on one raw would otherwise
    /// overwrite each other — so its edits live only in memory and closing the app
    /// is the one action that silently discards them.
    pub warn_unsaved_duplicates: bool,
    /// Whether the app checks the stable update feed by itself. **macOS only;**
    /// live — see `updater`. The check is silent: nothing appears unless an
    /// update is found, and then the badge is the whole announcement.
    pub check_for_updates: bool,
    /// The update the user chose to skip, as an exact feed version. **macOS
    /// only;** live — see `updater`. Mirrored into Sparkle's own
    /// `SUSkippedVersion` user default so both copies offer the same answer;
    /// this one is what the badge reads and Settings → About shows.
    pub skipped_update_version: Option<String>,
    /// The window the value readout averages over, as a stable key. See
    /// [`SampleArea`].
    pub sample_area: String,
    /// What the footer reports while a **colour reference view** is on screen, as a
    /// stable key. See [`ReferenceValues`].
    pub reference_values: String,

    // -- 7. The proof
    /// `png` or `jpeg`. See `export::Container::proof_note`.
    pub proof_container: String,
    /// `srgb` or `monostar`. See `export::Space`.
    pub proof_space: String,
    /// `full`, `half`, `third` or `quarter` of the picture. See `export::ProofScale`.
    pub proof_scale: String,
    /// 8 or 16. Forced to 8 whenever the container is JPEG.
    pub proof_depth: String,
    /// TPDF dither on an **8-bit** proof. A master is 16-bit and never dithered, and a
    /// 16-bit proof ignores this: a 16-bit step is already below what anyone can see.
    pub proof_dither: bool,
}

/// A proof's format, size and dither — the preferences the EXPORT module shows.
///
/// Global rather than per image, like every other export preference: a proof is a
/// way of handing pictures to someone, and that does not change from frame to frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProofPrefs {
    pub target: export::Target,
    pub scale: export::ProofScale,
    pub dither: bool,
}

/// How wide a window the footer readout and the Inspector's pins average over.
///
/// # Why this control exists at all
///
/// **On DirectMosaic a single-pixel sample is not a luminance measurement.** Every
/// output pixel there is its own gain-equalised photosite scaled by
/// `weight[c] / density[c]`, and the CFA pattern surviving on saturated colour is the
/// *point* of the mode — see `raw_core::scene::direct_mosaic`. So a 1×1 sample reads
/// whichever CFA colour that pixel happens to be: one channel of the scene, not the
/// scene's luminance. the maintainer reported it as "point sample seems a bit too exact on
/// DirectMosaic mode", which is exactly the symptom.
///
/// The other two modes do not have the problem, and that is the tell. **SuperPixel**
/// already averages a full 2×2 quad into each output pixel, so its 1×1 sample *is* a
/// quad mean; **Demosaic** interpolates, so every pixel already carries all three
/// colours. DirectMosaic is the only mode where the smallest sample is degenerate.
///
/// # Why the sizes are odd, and why 3 is enough to fix it
///
/// **Odd, because an odd square has an exact centre pixel** — the sample is centred
/// on the pixel you clicked. An even window has no centre and would sit half a pixel
/// off the cursor, with which half decided by a rounding convention nobody should
/// have to learn. This is Photoshop's rule too, and it is why its ladder is
/// `3 · 5 · 11 · 31 · 51 · 101` rather than powers of two.
///
/// **A 3×3 window covers a full Bayer quad in every phase.** The pattern has period
/// 2, and three consecutive rows and columns span both row parities and both column
/// parities — so a 3×3 always contains at least one of each CFA colour whatever pixel
/// it is centred on. It is the smallest odd window that does, which makes it the
/// minimum honest sample on a mosaic rather than a matter of taste.
///
/// The ladder stops at 31. Photoshop's 51 and 101 exist for cloning and healing
/// workflows this app does not have, and a 101×101 window on a 41 MP frame is a 1%
/// patch of the picture — a different measurement from the one this control is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleArea {
    /// One pixel, exactly. Kept because some work genuinely wants the literal value
    /// of one photosite — checking whether a specific pixel is clipped, above all.
    Point,
    /// **The default.** Costs almost nothing in SuperPixel and Demosaic, where the
    /// pixel is already an average or an interpolation, and is the difference between
    /// a number and a wrong number in DirectMosaic.
    #[default]
    Three,
    Five,
    Eleven,
    ThirtyOne,
}

impl SampleArea {
    pub const UI_ORDER: [Self; 5] = [
        Self::Point,
        Self::Three,
        Self::Five,
        Self::Eleven,
        Self::ThirtyOne,
    ];

    /// The window's edge in pixels. Always odd; `Point` is 1.
    pub fn edge(self) -> i32 {
        match self {
            Self::Point => 1,
            Self::Three => 3,
            Self::Five => 5,
            Self::Eleven => 11,
            Self::ThirtyOne => 31,
        }
    }

    /// How far the window reaches from the centre pixel. `edge / 2`, which is what
    /// makes the centre exact.
    pub fn radius(self) -> i32 {
        self.edge() / 2
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Three => "3 × 3",
            Self::Five => "5 × 5",
            Self::Eleven => "11 × 11",
            Self::ThirtyOne => "31 × 31",
        }
    }

    /// The compact form, for the Inspector's caption line where it sits beside two
    /// other facts and must not take the row.
    pub fn short(self) -> &'static str {
        match self {
            Self::Point => "1px",
            Self::Three => "3×3",
            Self::Five => "5×5",
            Self::Eleven => "11×11",
            Self::ThirtyOne => "31×31",
        }
    }

    /// Stable key for persistence; see `export::Container::key`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Three => "3",
            Self::Five => "5",
            Self::Eleven => "11",
            Self::ThirtyOne => "31",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|a| a.key() == s)
    }
}

impl Default for Settings {
    fn default() -> Self {
        let proof = export::Target::proof();
        // Every pipeline default is the current shipped default, so turning the menu
        // on changes nothing until something is moved.
        let p = Params::default();
        Self {
            tiff_suffix: "_monopro".into(),
            png_suffix: "_monoproof".into(),
            output_folder: None,
            tiff_color_space: "monostar".into(),
            sampling: sampling_key(p.luminance.sampling).into(),
            demosaic: DemosaicAlgo::default().key().into(),
            weighting: weighting_key(p.luminance.weighting).into(),
            weighting_mix: [1.0, 1.0, 1.0],
            contrast_mask: p.contrast_mask.enabled,
            tone_map: tone_map_key(p.display.tone_map).into(),
            print_unit: Unit::default().key().into(),
            print_ppi: p.output.ppi,
            viewer_background: DEFAULT_VIEWER_BACKGROUND,
            panel_background: DEFAULT_VIEWER_BACKGROUND,
            panel_matches_viewer: false,
            module_background: DEFAULT_MODULE_BACKGROUND,
            surround_width: 85.0,
            // Mathematical white, matching the direct White button in the Viewer
            // page and the Frame module. Rising boards remain measured alternatives.
            surround_okhsl: [0.0, 0.0, 1.0],
            screen_dither: true,
            reset_on_open: true,
            export_metadata: true,
            lightbox_xmp_thumbnails: false,
            lightbox_gray: true,
            lightbox_matches_viewer: true,
            // Lightbox's own look before these were settings: the grid on `CHROME`,
            // panels and cards on `CHROME_DEEP`.
            lightbox_canvas: 15.0,
            lightbox_panel: 11.8,
            lightbox_module: 11.8,
            lightbox_folder_root: None,
            quick_export: false,
            remember_lightbox_sort: true,
            lightbox_filenames: true,
            lightbox_edited_mark: true,
            lightbox_folders: false,
            lightbox_other_files: false,
            reset_panels_on_start: true,
            start_in_develop: false,
            invert_scroll: false,
            scroll_zoom: true,
            hotkeys_enabled: true,
            tooltips: true,
            warn_unsaved_duplicates: true,
            check_for_updates: true,
            skipped_update_version: None,
            frameless_tiles: false,
            sample_area: SampleArea::default().key().into(),
            reference_values: ReferenceValues::default().key().into(),
            // From `Target::proof` rather than spelled out again, so the default
            // proof is defined once and in the module that knows what a proof is.
            proof_container: proof.container.key().into(),
            proof_space: proof.space.key().into(),
            proof_depth: proof.depth.key().into(),
            proof_scale: export::ProofScale::default().key().into(),
            proof_dither: true,
        }
    }
}

impl Settings {
    /// The proof target, resolved from its stable keys and made legal.
    ///
    /// **`settle` is not optional.** A stored 16-bit depth beside a JPEG container is
    /// a state the UI cannot produce but a hand-edited `settings.toml` can, and the
    /// writer must never be handed one — see `export::Container::depths`.
    pub fn proof_target(&self) -> export::Target {
        let mut t = export::Target {
            container: export::Container::from_key(&self.proof_container)
                .unwrap_or(export::Container::Png),
            depth: export::Depth::from_key(&self.proof_depth).unwrap_or(export::Depth::Eight),
            compression: export::Compression::None,
            // A proof is sRGB or monostar, and that is a decision about *proofs*
            // rather than about the pipeline: the recipient's screen is unmanaged, so a
            // wide-gamut proof would be read as sRGB and come back desaturated. The
            // filter used to say "the pipeline cannot render this"; it now says what it
            // always meant.
            space: export::Space::from_key(&self.proof_space)
                .filter(|s| export::Space::PROOF_ORDER.contains(s))
                .unwrap_or(export::Space::Srgb),
        };
        t.settle();
        t
    }

    pub fn proof_scale(&self) -> export::ProofScale {
        export::ProofScale::from_key(&self.proof_scale).unwrap_or_default()
    }

    /// Everything about a proof, as one value the EXPORT module edits and hands back.
    pub fn proof_prefs(&self) -> ProofPrefs {
        ProofPrefs {
            target: self.proof_target(),
            scale: self.proof_scale(),
            dither: self.proof_dither,
        }
    }

    /// Store `p`, made legal first: a 16-bit JPEG is a state a control can pass
    /// through on its way somewhere and the writer must never be handed.
    pub fn set_proof_prefs(&mut self, p: ProofPrefs) {
        let mut target = p.target;
        target.settle();
        self.proof_container = target.container.key().into();
        self.proof_depth = target.depth.key().into();
        self.proof_space = target.space.key().into();
        self.proof_scale = p.scale.key().into();
        self.proof_dither = p.dither;
    }

    /// The sample window, resolved from its stable key.
    ///
    /// Falls back to the default rather than to `Point` if the file holds a key this
    /// build does not know: an unreadable preference should give you the good
    /// behaviour, not the one the control exists to avoid.
    pub fn sample_area(&self) -> SampleArea {
        SampleArea::from_key(&self.sample_area).unwrap_or_default()
    }

    pub fn reference_values(&self) -> ReferenceValues {
        ReferenceValues::from_key(&self.reference_values).unwrap_or_default()
    }
}

/// What the footer reports under a colour reference view — the camera JPEG or the raw
/// linear preview.
///
/// **Lab by default, and that is the whole point of the readout.** These two views are
/// the colour the camera saw, and the question they are on screen to answer is what
/// that colour comes out as in grey. `L*` here and `L*` on the print are the same
/// axis, so the two subtract — which is exactly the argument that put `RAW` and
/// `EDITED` into one unit rather than leaving one of them in EV.
///
/// RGB is the other thing a person might want from a picture with colour in it —
/// checking a channel, reading a value a browser would show — and it is one setting
/// rather than a second readout, because both answer *what is this pixel* and only one
/// of them can be in the footer at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReferenceValues {
    #[default]
    Lab,
    Rgb,
}

impl ReferenceValues {
    pub const UI_ORDER: [Self; 2] = [Self::Lab, Self::Rgb];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lab => "L*a*b*",
            Self::Rgb => "RGB",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Lab => "lab",
            Self::Rgb => "rgb",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|v| v.key() == s)
    }
}

// -------------------------------------------------------------- stable keys

fn sampling_key(s: Sampling) -> &'static str {
    match s {
        Sampling::SuperPixel => "superpixel",
        Sampling::DirectMosaic => "directmosaic",
        Sampling::Demosaic(_) => "demosaic",
    }
}

fn weighting_key(w: Weighting) -> &'static str {
    match w {
        Weighting::Photosite => "photosite",
        Weighting::Equal => "equal",
        Weighting::Red => "red",
        Weighting::Green => "green",
        Weighting::Blue => "blue",
        Weighting::Weighted(..) => "weighted",
    }
}

fn tone_map_key(t: ToneMap) -> &'static str {
    match t {
        ToneMap::Clip => "clip",
        ToneMap::Shoulder { .. } => "shoulder",
        ToneMap::Agx(..) => "agx",
    }
}

impl Settings {
    /// The sampling mode a new image opens at, with the configured algorithm
    /// already inside it — the two are separate settings and one `Sampling`.
    pub fn sampling(&self) -> Sampling {
        let algo = DemosaicAlgo::UI_ORDER
            .into_iter()
            .find(|a| a.key() == self.demosaic)
            .unwrap_or_default();
        match self.sampling.as_str() {
            "superpixel" => Sampling::SuperPixel,
            "directmosaic" => Sampling::DirectMosaic,
            "demosaic" => Sampling::Demosaic(algo),
            // **`Sampling::default()`, not a named mode.** This arm is the hand-edited
            // typo, and it belongs wherever the shipped default is rather than on
            // whichever variant happened to be written here when the default last moved.
            // It *was* `SuperPixel`, which is how a typo in `settings.toml` would have
            // quietly kept giving the old default after this one changed.
            _ => Sampling::default(),
        }
    }

    pub fn set_sampling(&mut self, s: Sampling) {
        self.sampling = sampling_key(s).into();
        if let Sampling::Demosaic(a) = s {
            self.demosaic = a.key().into();
        }
    }

    pub fn demosaic(&self) -> DemosaicAlgo {
        DemosaicAlgo::UI_ORDER
            .into_iter()
            .find(|a| a.key() == self.demosaic)
            .unwrap_or_default()
    }

    pub fn weighting(&self) -> Weighting {
        let [r, g, b] = self.weighting_mix;
        match self.weighting.as_str() {
            "equal" => Weighting::Equal,
            "red" => Weighting::Red,
            "green" => Weighting::Green,
            "blue" => Weighting::Blue,
            "weighted" => Weighting::Weighted(r, g, b),
            _ => Weighting::Photosite,
        }
    }

    pub fn set_weighting(&mut self, w: Weighting) {
        self.weighting = weighting_key(w).into();
        if let Weighting::Weighted(r, g, b) = w {
            self.weighting_mix = [r, g, b];
        }
    }

    pub fn tone_map(&self) -> ToneMap {
        match self.tone_map.as_str() {
            "clip" => ToneMap::Clip,
            "shoulder" => ToneMap::SHOULDER_DEFAULT,
            "agx" => ToneMap::AGX_DEFAULT,
            // Keep a hand-edited typo aligned with the shipped default. This
            // matters whenever the default moves without changing the file format.
            _ => ToneMap::default(),
        }
    }

    pub fn set_tone_map(&mut self, t: ToneMap) {
        self.tone_map = tone_map_key(t).into();
    }

    pub fn print_unit(&self) -> Unit {
        Unit::from_key(&self.print_unit).unwrap_or_default()
    }

    /// Where Quick Export would write `suggested`, or `None` to show the dialog.
    ///
    /// **The whole decision, in one place and away from `rfd`.** The three ways it says
    /// no are each a different reason and each is a way the feature could be wrong:
    ///
    /// - the preference is off, which is the ordinary case;
    /// - there is no output folder, or the one configured has been deleted or unmounted
    ///   — "do not ask again once I have told you where these go" has nothing to mean
    ///   without somewhere to go, and writing beside the raw instead would be a
    ///   different feature nobody asked for;
    /// - the name is already taken. The confirmation being skipped is the save dialog's
    ///   own overwrite prompt, so the check moves here rather than disappearing. Quick
    ///   means fewer keystrokes, not fewer chances to notice.
    pub fn quick_destination(&self, suggested: &str) -> Option<PathBuf> {
        if !self.quick_export {
            return None;
        }
        let dir = self.output_folder.as_deref().filter(|d| d.is_dir())?;
        let dest = dir.join(suggested);
        (!dest.exists()).then_some(dest)
    }

    /// The space a master is written in.
    ///
    /// An unrecognised key falls back to `Space::default()` rather than to a named
    /// space, on the rule `sampling` follows: a hand-edited typo should land wherever
    /// the shipped default is, not on whichever variant was written here last.
    pub fn master_space(&self) -> export::Space {
        export::Space::from_key(&self.tiff_color_space)
            .unwrap_or_default()
            .selectable()
    }

    pub fn set_master_space(&mut self, space: export::Space) {
        self.tiff_color_space = space.key().into();
    }

    /// What a master is: a 16-bit uncompressed TIFF in [`Self::master_space`].
    pub fn master_target(&self) -> export::Target {
        export::Target::master(self.master_space())
    }

    pub fn set_print_unit(&mut self, u: Unit) {
        self.print_unit = u.key().into();
    }

    /// The develop state a **new** image opens at.
    ///
    /// Applied at `Tabs::open`, never by redefining `Params::default` — that is
    /// still what Develop's "Reset All" means and what every test compares against.
    pub fn params(&self) -> Params {
        let mut p = Params::default();
        p.luminance.sampling = self.sampling();
        p.luminance.weighting = self.weighting();
        p.contrast_mask.enabled = self.contrast_mask;
        p.display.tone_map = self.tone_map();
        // Only the resolution. A print *size* is a decision about one picture and
        // must not be inherited by the next file that opens — a preference that
        // silently resampled every new image would be the worst kind.
        p.output.ppi = self.ppi();
        p
    }

    /// The configured export resolution, clamped into the range the control allows.
    ///
    /// **Clamped because the file can say anything.** `settings.toml` is a text file a
    /// user may edit, and an out-of-range value taken literally would put a new image
    /// somewhere its own `DragValue` cannot reach — a number you can only remove by
    /// going back to the text file.
    ///
    /// Split out of [`params`](Self::params) when `open_path` needed the same value:
    /// PPI is applied to a new tab on **both** routes in, sticky and reset, so the
    /// clamp had to stop being a detail of one of them.
    pub fn ppi(&self) -> f32 {
        self.print_ppi.clamp(
            *OutputParams::PPI_RANGE.start(),
            *OutputParams::PPI_RANGE.end(),
        )
    }

    /// The suffix for a container, as configured.
    ///
    /// **JPEG shares the PNG suffix**, because the two are the same *deliverable* —
    /// the proof — and the suffix names what a file is rather than how it is encoded.
    /// `_monoproof.png` and `_monoproof.jpg` beside each other is exactly right; two
    /// suffixes for one purpose would be a third preference to keep in step.
    pub fn suffix(&self, c: export::Container) -> &str {
        match c {
            export::Container::Tiff => &self.tiff_suffix,
            export::Container::Png | export::Container::Jpeg => &self.png_suffix,
        }
    }

    /// What to call an export of `source`, before the user gets a say.
    ///
    /// Sanitised, because this is a filename built from a user-typed string: a
    /// suffix containing a path separator would silently move the file somewhere
    /// else, which is the one way a naming preference could lose someone's export.
    pub fn export_name(&self, source: &Path, target: export::Target) -> String {
        let stem = source.file_stem().map(|s| s.to_string_lossy().into_owned());
        let stem = stem.unwrap_or_else(|| "export".into());
        let suffix: String = self
            .suffix(target.container)
            .chars()
            .filter(|c| !"/\\:".contains(*c))
            .collect();
        format!("{stem}{suffix}.{}", target.extension())
    }

    /// The mount, resolved to what the renderer wants.
    ///
    /// Width and the tab's enabled state collapse into one number here: zero width
    /// *is* off, so the renderer cannot be handed disagreeing state.
    pub fn surround(&self, enabled: bool) -> raw_gpu::Surround {
        if !enabled {
            return raw_gpu::Surround::NONE;
        }
        let [h, s, l] = self.surround_okhsl;
        raw_gpu::Surround {
            width: self.surround_width.max(0.0),
            rgb: raw_core::okhsl::to_srgb(raw_core::okhsl::Okhsl { h, s, l }),
        }
    }

    /// The viewer background as a display-encoded value in `[0, 1]`, which is the
    /// unit both the shader and the egui letterbox want.
    pub fn background(&self) -> f32 {
        (self.viewer_background / 100.0).clamp(0.0, 1.0)
    }

    /// Lightbox's canvas, panel and card grounds, in the same unit as
    /// [`Settings::background`] — the viewer's when `lightbox_matches_viewer` is on.
    pub fn lightbox_grounds(&self) -> [f32; 3] {
        if self.lightbox_matches_viewer {
            [
                self.background(),
                self.panel_background(),
                self.module_background(),
            ]
        } else {
            [
                self.lightbox_canvas,
                self.lightbox_panel,
                self.lightbox_module,
            ]
            .map(|v| (v / 100.0).clamp(0.0, 1.0))
        }
    }

    /// The module cards' ground, in the same unit as [`Settings::background`].
    pub fn module_background(&self) -> f32 {
        (self.module_background / 100.0).clamp(0.0, 1.0)
    }

    /// The side panels' ground, in the same unit as [`Settings::background`]: the
    /// viewer background when `panel_matches_viewer` is on, else its own value.
    pub fn panel_background(&self) -> f32 {
        let v = if self.panel_matches_viewer {
            self.viewer_background
        } else {
            self.panel_background
        };
        (v / 100.0).clamp(0.0, 1.0)
    }

    // ------------------------------------------------------------ persistence

    /// Read `settings.toml`.
    ///
    /// Absent is a first run, not an error. **Corrupt is reported and the app still
    /// starts** — a preferences file that fails to parse must never be the reason
    /// somebody cannot open their photographs.
    pub fn load() -> Loaded<Self> {
        let Some(path) = path() else {
            return Loaded::Absent;
        };
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Absent,
            Err(e) => Loaded::Corrupt(format!("{}: {e}", path.display())),
            Ok(text) => match toml::from_str(&text) {
                Ok(s) => Loaded::Ok(s),
                Err(e) => Loaded::Corrupt(format!("{}: {e}", path.display())),
            },
        }
    }

    /// Write it. Atomic, for the same reason the sidecar is: a half-written
    /// preferences file would be a corrupt one on the next launch.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = path() else {
            return Err(std::io::Error::other("no storage directory"));
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        write_atomically(&path, &text)
    }
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    raw_core::atomic_file::write(path, |file| file.write_all(text.as_bytes()))
}

// ------------------------------------------------------------------------- UI

/// A section marker in the Settings window.
///
/// A hairline above it rather than space alone. This window is one long column of
/// unrelated switches — naming, output, defaults, view, lightbox — and the only
/// thing that tells you a subject has ended is the rule. The develop panel uses
/// boxes for the same job; here a rule is enough, because a settings section has no
/// state of its own to contain.
/// The Settings window's sidebar sections, in the maintainer's order.
///
/// **A sidebar rather than one long column**, which is what it was. The column had
/// eight headings and no way to reach the eighth but scrolling past the other seven,
/// and the rule between sections was doing the whole job of saying a subject had
/// ended. Grouping is what a settings window is *for*; a scroll is not grouping.
///
/// The order follows the work: app behavior, viewing, browsing, processing,
/// delivery, input, then application information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    General,
    Viewer,
    Lightbox,
    Processing,
    Export,
    Controls,
    About,
}

impl Section {
    pub const ALL: [Self; 7] = [
        Self::General,
        Self::Viewer,
        Self::Lightbox,
        Self::Processing,
        Self::Export,
        Self::Controls,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Viewer => "Viewer",
            Self::Lightbox => "Lightbox",
            Self::Processing => "Processing",
            Self::Export => "Export",
            Self::Controls => "Controls",
            Self::About => "About",
        }
    }
}

/// The Settings window's own view state: which section is showing, and what is in the
/// search box.
///
/// Not part of [`Settings`] and never written to `settings.toml`. Which page you last
/// had open is not a preference about how the app behaves; it is where you happened
/// to be standing.
#[derive(Debug, Default)]
pub struct Sheet {
    pub section: Option<Section>,
    pub query: String,
    /// The last section whose caption was drawn during a search, so a run of matches
    /// from one section is captioned once rather than per row.
    drawn: Option<Section>,
}

impl Sheet {
    /// The section shown when nothing has been chosen yet.
    pub fn current(&self) -> Section {
        self.section.unwrap_or(Section::General)
    }

    pub fn searching(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Start a frame. Clears the per-frame caption state.
    pub fn begin(&mut self) {
        self.drawn = None;
    }

    /// Whether a row belongs on screen right now, drawing its section caption if a
    /// search has pulled it out of its own page.
    ///
    /// **Searching abandons the sidebar rather than filtering inside it.** A search
    /// that only looked in the page you were already on would find nothing precisely
    /// when you most need it — you search because you do not know where the thing
    /// lives. So a query draws every match from every section, each run captioned
    /// with where it came from, and the sidebar goes quiet until the box is cleared.
    ///
    /// Matching is a case-insensitive substring over the row's label and its section
    /// name, so "export" finds the Export page's rows and "ppi" finds the one row.
    pub fn shows(&mut self, ui: &mut egui::Ui, section: Section, label: &str) -> bool {
        let hit = self.matches(section, label);
        // The caption is the only part that needs a `Ui`, and it is drawn once per run
        // of matches from the same section rather than per row.
        if hit && self.searching() && self.drawn != Some(section) {
            self.drawn = Some(section);
            ui.add_space(10.0);
            crate::theme::section(ui, &section.label().to_uppercase());
            ui.add_space(2.0);
        }
        hit
    }

    /// Whether a row belongs on screen, with no drawing and no `Ui`.
    ///
    /// **Split out so the rule can be tested.** Everything interesting about
    /// searching is here — which page wins, what a query matches, what an empty one
    /// means — and none of it needs a frame. What is left in [`Self::shows`] is a caption.
    pub fn matches(&self, section: Section, label: &str) -> bool {
        if !self.searching() {
            return self.current() == section;
        }
        let q = self.query.trim().to_lowercase();
        label.to_lowercase().contains(&q) || section.label().to_lowercase().contains(&q)
    }
}

/// The modified mark: a ruby dot in the row's left gutter, or the gutter alone.
///
/// **the maintainer asked for a dot beside anything changed from its default**, and the reason
/// it earns its space is that this window is mostly switches that look identical
/// whichever way they are set. You cannot tell, from a checkbox, whether you turned it
/// on or it was born that way — so a settings window without this is a window you have
/// to remember.
///
/// Ruby, and the same ruby the develop modules use, because it is the same claim:
/// *this differs from the default*. The gutter is allocated whether or not the dot is
/// drawn, so a page does not re-flow as values move on and off their defaults.
///
/// `modified` is passed rather than computed here because only the call site knows
/// which field the row is about. `Settings::default()` is the thing to compare
/// against and the comparison is one expression each time.
/// The Rising Museum Board colours, as **spectrophotometer-measured sRGB**.
///
/// the maintainer's measurements, carried over from the prototype where they are recorded as
/// *SpectraShop, 2°/D65, i1Pro 45:0*. The whites and tints are **one L\* point below
/// the raw measurement** — his correction, and it is deliberate: a board measured
/// 45:0 reads slightly brighter than the same board seen on a wall, and the mount is
/// there to be judged against, not to be reproduced.
///
/// These are the ten the maintainer asked for, in his order — broadest to narrowest spectral
/// response, which for a mount is also lightest to darkest. The board numbers from
/// the Rising chart are kept beside them because the range is not contiguous: 007,
/// 010 and 011 exist and are not wanted.
pub const RISING: [(&str, [u8; 3]); 10] = [
    ("Polar White", [0xf0, 0xf0, 0xee]), // 001
    ("White", [0xf6, 0xf5, 0xee]),       // 002
    ("Warm White", [0xf5, 0xf1, 0xe5]),  // 003
    ("Antique", [0xf5, 0xee, 0xdc]),     // 004
    ("Olde White", [0xf6, 0xf0, 0xdb]),  // 005
    ("Cream", [0xf6, 0xef, 0xd4]),       // 006
    ("Natural", [0xf8, 0xed, 0xd1]),     // 008
    ("Zinc", [0xe5, 0xd8, 0xbd]),        // 009
    ("Medium Gray", [0xa0, 0x99, 0x93]), // 012
    ("Black", [0x34, 0x33, 0x33]),       // 013
];

/// The two chips that are not a board.
///
/// **Kept apart from [`RISING`] rather than appended to it**, which is the
/// prototype's arrangement and is right: every Rising colour is a thing you can buy
/// and cut, and these two are the mathematical ends. Mixing them into the same row
/// would imply a board that does not exist.
pub const ABSOLUTE: [(&str, [u8; 3]); 2] = [
    ("100% White", [0xff, 0xff, 0xff]),
    ("100% Black", [0x00, 0x00, 0x00]),
];

/// The modified gutter's width, including the space after it.
///
/// Named because two things have to agree about it: [`item`], which draws in it, and
/// [`note`], which has to step over it.
pub const GUTTER: f32 = 9.0;
/// Width reserved for the control side of every Settings row.
///
/// Keeping this fixed is what removes the rag: labels may have different lengths,
/// but sliders, fields and menus all finish on the same right edge.
pub const CONTROL_W: f32 = 210.0;

/// The three fixed regions in one Settings row.
///
/// These are calculated before any child widget is drawn. That matters because egui
/// normally lets a combo box grow to fit its selected text and lets a checkbox take
/// only its intrinsic width; when those widgets participate in the row's layout they
/// can move their own column. A settings sheet needs the opposite rule: the column is
/// fixed, and the widget fits inside it.
#[derive(Debug, Clone, Copy)]
struct ItemRects {
    gutter: egui::Rect,
    label: egui::Rect,
    control: egui::Rect,
}

fn item_rects(row: egui::Rect, gap: f32) -> ItemRects {
    let content_left = row.left() + GUTTER + gap;
    let room_after_gutter = (row.right() - content_left).max(1.0);
    let control_w = CONTROL_W
        .min((row.width() * 0.44).max(150.0))
        // Keep a useful label column in a window at its minimum width.
        .min((room_after_gutter - gap - 120.0).max(80.0));
    let control = egui::Rect::from_min_max(
        egui::pos2(row.right() - control_w, row.top()),
        row.right_bottom(),
    );
    let label = egui::Rect::from_min_max(
        egui::pos2(content_left, row.top()),
        egui::pos2((control.left() - gap).max(content_left), row.bottom()),
    );
    let gutter = egui::Rect::from_min_size(row.min, egui::vec2(GUTTER, row.height()));
    ItemRects {
        gutter,
        label,
        control,
    }
}

#[cfg(test)]
mod item_layout_tests {
    use super::*;

    #[test]
    fn every_control_column_finishes_on_the_same_right_edge() {
        for width in [430.0, 560.0, 760.0] {
            let row = egui::Rect::from_min_size(egui::pos2(17.0, 20.0), egui::vec2(width, 40.0));
            let rects = item_rects(row, 8.0);
            assert_eq!(rects.control.right(), row.right());
            assert!(rects.label.right() < rects.control.left());
            assert!(rects.label.width() >= 120.0);
        }
    }

    #[test]
    fn a_widgets_intrinsic_width_cannot_move_the_settings_columns() {
        let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(520.0, 40.0));
        let first = item_rects(row, 8.0);
        let second = item_rects(row, 8.0);
        assert_eq!(first.label, second.label);
        assert_eq!(first.control, second.control);
    }
}

/// The explanatory line under a control, **indented to the control's own column**.
///
/// the maintainer's note, with a guideline drawn down the screenshot to show where. Every
/// control in this window sits behind a 9pt modified gutter, so its label starts
/// inset; a caption drawn at the container's edge started further left than the thing
/// it was explaining, and the eye had two left margins to choose between. Stepping
/// over the same gutter gives the page one.
///
/// The section headings stay at the edge, and that is the point of the arrangement
/// rather than an exception to it: a heading names a *group*, so it should sit outside
/// the column the group is laid out in.
/// How wide a caption is allowed to get before it wraps.
///
/// **A measure, not a margin.** The settings window is 920pt and a caption set across
/// all of it is forty words on one line, which is past the span an eye tracks back
/// from without losing the row — the reason every book has a margin it does not need
/// structurally. the maintainer asked for the break at about here.
///
/// It is a *maximum*: a narrower window wraps sooner, because `available_width` is
/// the other half of the `min` below.
const NOTE_W: f32 = 450.0;

pub fn note(ui: &mut egui::Ui, text: impl Into<String>) {
    // **Not a `horizontal` with a space in front of it**, which is what this was and
    // is why the first attempt did not wrap at all: egui's horizontal layout sets its
    // children to *extend* rather than wrap, because a row that grew downwards would
    // push everything beside it out of line. Any width cap inside one is therefore
    // advisory and the text runs off the edge regardless.
    //
    // A frame's left margin gives the same indent inside the parent's **vertical**
    // layout, where a label wraps the way a label normally does.
    let inset = GUTTER + ui.spacing().item_spacing.x;
    let w = (ui.available_width() - inset).clamp(120.0, NOTE_W);
    egui::Frame::new()
        .outer_margin(egui::Margin {
            left: inset as i8,
            ..Default::default()
        })
        .show(ui, |ui| {
            ui.set_max_width(w);
            ui.label(crate::theme::caption(text));
        });
}

/// One compact Settings row: description on the left, control on the right.
///
/// The return value is `(control result, reset clicked)`. Callers that own a value
/// other than a simple boolean can restore its default without this layout helper
/// knowing anything about the setting's type.
pub fn item<R>(
    ui: &mut egui::Ui,
    modified: bool,
    title: &str,
    caption: Option<&str>,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> (R, bool) {
    let row_h = if caption.is_some() {
        40.0
    } else {
        ui.spacing().interact_size.y
    };
    let row_w = ui.available_width().max(1.0);
    let (row, _) = ui.allocate_exact_size(egui::vec2(row_w, row_h), egui::Sense::hover());
    let rects = item_rects(row, ui.spacing().item_spacing.x);

    if modified {
        ui.painter()
            .circle_filled(rects.gutter.center(), 3.0, crate::theme::RUBY);
    }

    let mut label_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("settings-label", title))
            .max_rect(rects.label)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    let mut reset = false;
    label_ui.horizontal(|ui| {
        ui.label(crate::theme::label(title));
        if modified {
            reset = ui
                .small_button(
                    egui::RichText::new("↺")
                        .size(crate::theme::size::CAPTION)
                        .color(crate::theme::DIM),
                )
                .on_hover_text(crate::theme::tip("Restore the default"))
                .clicked();
        }
    });
    if let Some(caption) = caption {
        label_ui.label(crate::theme::caption(caption));
    }

    let mut control_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("settings-control", title))
            .max_rect(rects.control)
            .layout(egui::Layout::right_to_left(egui::Align::Min)),
    );
    let value = control(&mut control_ui);
    (value, reset)
}

/// The compact egui-style switch used for boolean preferences.
fn toggle_switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    let desired = egui::vec2(34.0, 18.0);
    let (rect, mut response) = ui.allocate_exact_size(desired, egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }

    let t = ui.ctx().animate_bool_responsive(response.id, *value);
    let hovered = response.hovered();
    let track = if *value {
        if hovered {
            crate::theme::RUBY
        } else {
            crate::theme::RUBY_FILL
        }
    } else if hovered {
        egui::Color32::from_gray(70)
    } else {
        egui::Color32::from_gray(54)
    };
    ui.painter().rect(
        rect,
        0.0,
        track,
        egui::Stroke::new(1.0, egui::Color32::from_gray(88)),
        egui::StrokeKind::Inside,
    );
    let knob_size = 12.0;
    let knob_x = egui::lerp((rect.left() + 3.0)..=(rect.right() - 3.0 - knob_size), t);
    let knob = egui::Rect::from_min_size(
        egui::pos2(knob_x, rect.top() + 3.0),
        egui::vec2(knob_size, knob_size),
    );
    // **The knob wears the slider handle's bezel**: the same 1pt corner and 1pt
    // outline, so a switch and a slider read as one family of grips. The track keeps
    // its square corners — the bezel belongs to the thing you take hold of.
    ui.painter().rect(
        knob,
        1.0,
        if *value {
            // Settings switches use a restrained 78.4% white rather than the app's
            // 93.3% bright text white. The square remains legible without reading as
            // a tiny luminous button inside the ruby track.
            egui::Color32::from_gray(200)
        } else {
            crate::theme::DIM
        },
        egui::Stroke::new(1.0, egui::Color32::from_gray(150)),
        egui::StrokeKind::Inside,
    );
    response
}

/// A right-justified switch in a row with its own modified gutter.
///
/// Exists so the eight switches on the Behaviour page do not each need a `horizontal`
/// wrapped round them by hand — and so the gutter width is stated once rather than
/// eight times and left to drift.
pub fn check(ui: &mut egui::Ui, value: &mut bool, default: bool, label: &str) {
    let (_, reset) = item(ui, *value != default, label, None, |ui| {
        toggle_switch(ui, value)
    });
    if reset {
        *value = default;
    }
}

/// A combo box with its **label on the left**, in the modified gutter's column.
///
/// the maintainer's general rule, and it is the develop panel's row read across to this
/// window: *label, then the control*. Boolean rows follow it too: their switch is in
/// this same right-justified control column.
///
/// `egui::ComboBox::from_label` puts the label on the **right**, which is why the
/// Pipeline Defaults page read as a column of unlabelled boxes with words trailing
/// them. There is no option to move it: the label is drawn after the button by
/// construction, so the fix is to draw it ourselves and give the box an id instead of
/// a label.
///
/// The label column is `Row::LABEL_W` so a combo lines up with the sliders above and
/// below it rather than starting wherever its own text happens to end.
pub fn combo<R>(
    ui: &mut egui::Ui,
    modified: bool,
    label: &str,
    selected: &str,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    combo_width(ui, modified, label, selected, PIPELINE_COMBO_W, body)
}

/// The default menu width in Settings, named for the page that establishes it.
/// Every DEFAULT PIPELINE menu uses this exact measure, so `Decode`, `Demosaic`,
/// `Luminance`, `Tonal Transform`, and `Export depth` cannot acquire five different widths
/// as their vocabularies change.
pub const PIPELINE_COMBO_W: f32 = CONTROL_W - 16.0;

/// Settings menus are deliberately one point quieter than their row labels, matching
/// the slider label/readout scale in Develop.
pub fn combo_text(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).size(crate::theme::size::SLIDER)
}

/// Apply the compact Settings type size inside a combo's popup as well as on its
/// closed button. `selected_text` alone only changes the latter.
pub fn combo_menu<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        let font = egui::FontId::new(crate::theme::size::SLIDER, egui::FontFamily::Proportional);
        ui.style_mut()
            .text_styles
            .insert(egui::TextStyle::Body, font.clone());
        ui.style_mut()
            .text_styles
            .insert(egui::TextStyle::Button, font);
        body(ui)
    })
    .inner
}

/// A Settings combo with an explicit button width, still anchored to the common
/// right edge. Short vocabularies such as `3 × 3` should not look like text fields.
pub fn combo_width<R>(
    ui: &mut egui::Ui,
    modified: bool,
    label: &str,
    selected: &str,
    width: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> bool {
    let (_, reset) = item(ui, modified, label, None, |ui| {
        egui::ComboBox::from_id_salt(label)
            .width(width)
            .truncate()
            .selected_text(combo_text(selected))
            .show_ui(ui, |ui| combo_menu(ui, body));
    });
    reset
}

pub fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(14.0);
    let y = ui.cursor().top();
    let x = ui.max_rect().x_range();
    ui.painter().line_segment(
        [egui::pos2(x.min, y), egui::pos2(x.max, y)],
        egui::Stroke::new(1.0, egui::Color32::from_gray(52)),
    );
    ui.add_space(8.0);
    crate::theme::section(ui, text);
    ui.add_space(2.0);
    // A heading opens a group, so the next `rule` has nothing to divide.
    ui.data_mut(|d| d.insert_temp(rule_id(), false));
}

/// Where [`rule`] remembers whether it has anything to divide yet.
///
/// A fixed `Id` rather than one per group: the window draws top to bottom in one pass
/// and only ever has one group open, so a single slot is the whole state. `insert_temp`
/// rather than persisted — this is a fact about *this frame's* layout and carrying it
/// to the next run would be meaningless.
fn rule_id() -> egui::Id {
    egui::Id::new("settings-rule")
}

/// **A hairline between two settings**, the way Zed's settings window divides its own.
///
/// the maintainer asked for it after reading that window: a page of switches and combos with
/// nothing between them reads as one undifferentiated block, and the eye has to work
/// out for itself where a control ends and its neighbour begins. It is the same
/// argument the develop panel's boxed modules already make, one level further down —
/// the difference being that a box says *these belong together* and a rule says only
/// *this one has finished*, which is the weaker claim and the right one here.
///
/// # Call it before every setting, including the first
///
/// **It draws nothing the first time after a [`heading`]**, which is what makes it
/// safe to call uniformly. A rule immediately under a section heading would divide a
/// heading from its own first control — the one place on the page where two things are
/// certain to belong together — and the alternative, remembering to skip the first at
/// every call site, is a rule that has to be kept rather than one that holds. Fifty
/// call sites is exactly the scale at which "remember to" stops working.
///
/// This also gets the **search view** right for free. A query redraws the page as
/// whichever groups matched, so which setting is first in a group is not a fact about
/// the source order — it is decided per frame, by `Sheet::shows`, and only something
/// reading the frame as it is drawn can know it.
///
/// # Quieter than the group rule
///
/// Gray 46 against [`heading`]'s 52, on the panel's 38. Both are hairlines and the
/// hierarchy between them has to come from somewhere; here it is value plus the 14pt
/// of air a heading takes and this does not. Inverting them — a bold rule between two
/// checkboxes and a faint one between subjects — would make the page read as many
/// groups of one.
pub fn rule(ui: &mut egui::Ui) {
    let divides = ui.data_mut(|d| d.get_temp::<bool>(rule_id()).unwrap_or(false));
    ui.data_mut(|d| d.insert_temp(rule_id(), true));
    if !divides {
        return;
    }
    ui.add_space(6.0);
    let y = ui.cursor().top();
    let x = ui.max_rect().x_range();
    ui.painter().line_segment(
        [egui::pos2(x.min, y), egui::pos2(x.max, y)],
        egui::Stroke::new(1.0, egui::Color32::from_gray(46)),
    );
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(s: &Settings) -> Settings {
        let text = toml::to_string_pretty(s).expect("serialise");
        toml::from_str(&text).expect("deserialise")
    }

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        assert_eq!(roundtrip(&s), s);
    }

    #[test]
    fn repeated_settings_writes_replace_the_complete_previous_file() {
        let path = dir().unwrap().join("replacement-settings.toml");
        write_atomically(&path, "first = true\n").unwrap();
        write_atomically(&path, "second = true\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second = true\n");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .contains("monopro-write"))
                .count(),
            0,
            "atomic publication left a temporary file behind"
        );
    }

    #[test]
    fn every_field_round_trips() {
        let s = Settings {
            tiff_suffix: "_master".into(),
            png_suffix: "_proof".into(),
            output_folder: Some("/Volumes/Prints".into()),
            tiff_color_space: "ecirgb".into(),
            sampling: "superpixel".into(),
            demosaic: "amaze".into(),
            weighting: "weighted".into(),
            weighting_mix: [0.7, 0.2, 0.1],
            contrast_mask: true,
            tone_map: "agx".into(),
            screen_dither: false,
            proof_dither: false,
            print_unit: "cm".into(),
            print_ppi: 360.0,
            viewer_background: 42.5,
            panel_background: 20.0,
            panel_matches_viewer: true,
            module_background: 80.0,
            surround_width: 60.0,
            surround_okhsl: [35.0, 0.15, 0.9],
            reset_on_open: false,
            export_metadata: false,
            lightbox_xmp_thumbnails: true,
            lightbox_gray: false,
            lightbox_matches_viewer: false,
            lightbox_canvas: 50.0,
            lightbox_panel: 60.0,
            lightbox_module: 70.0,
            lightbox_folder_root: Some("/example/Pictures".into()),
            quick_export: true,
            remember_lightbox_sort: false,
            frameless_tiles: true,
            lightbox_filenames: false,
            lightbox_edited_mark: false,
            lightbox_folders: true,
            lightbox_other_files: true,
            reset_panels_on_start: false,
            start_in_develop: true,
            invert_scroll: true,
            scroll_zoom: false,
            hotkeys_enabled: false,
            tooltips: false,
            warn_unsaved_duplicates: false,
            check_for_updates: false,
            skipped_update_version: Some("0.0.9".into()),
            sample_area: "11".into(),
            reference_values: "rgb".into(),
            proof_container: "jpeg".into(),
            proof_space: "monostar".into(),
            proof_scale: "quarter".into(),
            proof_depth: "8".into(),
        };
        assert_eq!(roundtrip(&s), s);
    }

    #[test]
    fn an_unknown_sample_area_key_falls_back_to_the_default_not_to_point() {
        // A preference this build cannot read should give you the good behaviour,
        // not the one the control exists to avoid: `Point` is the degenerate sample
        // on DirectMosaic, which is the whole reason the setting was added.
        let s = Settings {
            sample_area: "17".into(),
            ..Settings::default()
        };
        assert_eq!(s.sample_area(), SampleArea::Three);
        assert_eq!(Settings::default().sample_area(), SampleArea::Three);
    }

    #[test]
    fn a_hand_edited_sixteen_bit_jpeg_proof_is_settled_before_it_reaches_the_writer() {
        // The UI cannot produce this pairing; a text editor can. `proof_target` is
        // the boundary, and baseline JPEG has no 16-bit mode to fall back on.
        let s = Settings {
            proof_container: "jpeg".into(),
            proof_depth: "16".into(),
            ..Settings::default()
        };
        let t = s.proof_target();
        assert_eq!(t.container, export::Container::Jpeg);
        assert_eq!(t.depth, export::Depth::Eight);
    }

    #[test]
    fn a_proof_space_that_needs_colour_falls_back_rather_than_writing_three_grey_channels() {
        // eciRGB v2 is not on the proof list, so this only arrives by hand-editing —
        // and writing it would triple the file to claim a colour decision the
        // pipeline has not made.
        let s = Settings {
            proof_space: "ecirgb".into(),
            ..Settings::default()
        };
        assert_eq!(s.proof_target().space, export::Space::Srgb);
    }

    #[test]
    fn the_default_proof_is_a_half_size_srgb_png() {
        let s = Settings::default();
        let t = s.proof_target();
        assert_eq!(t.container, export::Container::Png);
        assert_eq!(t.depth, export::Depth::Eight);
        assert_eq!(t.space, export::Space::Srgb);
        assert_eq!(s.proof_scale(), export::ProofScale::Half);
        assert!(
            s.proof_dither,
            "an 8-bit proof is dithered unless asked otherwise"
        );
        // And the master is untouched by any of it.
        assert_eq!(
            s.master_target(),
            export::Target::master(export::Space::Monostar)
        );
    }

    #[test]
    fn every_proof_key_round_trips() {
        for c in export::Container::PROOF_ORDER {
            assert_eq!(export::Container::from_key(c.key()), Some(c));
        }
        for s in export::Space::UI_ORDER {
            assert_eq!(export::Space::from_key(s.key()), Some(s));
        }
        for s in export::ProofScale::UI_ORDER {
            assert_eq!(export::ProofScale::from_key(s.key()), Some(s));
        }
    }

    #[test]
    fn a_legacy_prostar_preference_opens_at_the_remaining_print_space() {
        let settings = Settings {
            tiff_color_space: "prostar".into(),
            ..Settings::default()
        };
        assert_eq!(settings.master_space(), export::Space::EciRgbV2);
    }

    #[test]
    fn every_sample_area_key_round_trips() {
        for a in SampleArea::UI_ORDER {
            assert_eq!(
                SampleArea::from_key(a.key()),
                Some(a),
                "{} lost its key",
                a.label()
            );
        }
    }

    #[test]
    fn the_defaults_change_nothing() {
        // The rule the whole menu ships under: turning it on must not alter a single
        // rendering until something is moved.
        assert_eq!(Settings::default().params(), Params::default());
        assert!(Settings::default().screen_dither);
        // Authored metadata travels unless someone says otherwise: these are fields a
        // person filled in on purpose. See `Settings::export_metadata`.
        assert!(Settings::default().export_metadata);
    }

    #[test]
    fn a_fresh_lightbox_opens_in_gray() {
        assert!(Settings::default().lightbox_gray);
    }

    #[test]
    fn the_default_surround_is_absolute_white() {
        let surround = Settings::default().surround(true);
        assert_eq!(surround.width, 85.0);
        for channel in surround.rgb {
            assert!(
                (channel - 1.0).abs() < 1.0e-4,
                "not white: {:?}",
                surround.rgb
            );
        }
    }

    /// **What a new user actually gets**, end to end, with no `settings.toml` on disk.
    ///
    /// Four hops, and each is a place the shipped default could be lost without any of
    /// the others noticing: `Settings::default` derives its key from `Params::default`,
    /// `sampling()` parses that key back with the algorithm from a *second* field, and
    /// `params()` is what `App::open` seeds a new tab from. A missing file leaves
    /// `Settings::default()` in place — `Loaded::Absent` is a no-op in `App::new` — so
    /// this is the whole of a first launch.
    /// Quick Export's three ways of declining, each for a different reason.
    #[test]
    fn quick_export_writes_straight_through_only_when_it_safely_can() {
        let dir = std::env::temp_dir().join(format!(
            "monopro-quick-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut s = Settings {
            output_folder: Some(dir.clone()),
            ..Default::default()
        };

        // Off is off, however good the folder is.
        assert_eq!(
            s.quick_destination("a_mono.tif"),
            None,
            "the preference was ignored"
        );

        s.quick_export = true;
        assert_eq!(
            s.quick_destination("a_mono.tif"),
            Some(dir.join("a_mono.tif")),
            "a set folder and a free name is the case the feature exists for"
        );

        // **A name already taken falls back to the dialog.** The prompt being skipped is
        // the dialog's own overwrite warning, so skipping the dialog must not skip that
        // too — this is the assertion standing between Quick Export and a silently
        // destroyed master.
        std::fs::write(dir.join("taken_mono.tif"), b"already here").unwrap();
        assert_eq!(
            s.quick_destination("taken_mono.tif"),
            None,
            "it would have overwritten"
        );

        // No folder configured, and a folder that has gone. Both mean there is nowhere
        // to assume, and neither should quietly write beside the raw instead.
        s.output_folder = None;
        assert_eq!(s.quick_destination("a_mono.tif"), None);
        s.output_folder = Some(dir.join("not-here"));
        assert_eq!(
            s.quick_destination("a_mono.tif"),
            None,
            "a missing folder was used anyway"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_install_opens_on_the_shipped_default() {
        let fresh = Settings::default();

        // The stored keys, which are what a first `settings.toml` will be written with.
        assert_eq!(fresh.sampling, "demosaic");
        assert_eq!(fresh.demosaic, "rcd");

        // Parsed back, including the algorithm from its separate field.
        assert_eq!(fresh.sampling(), Sampling::Demosaic(DemosaicAlgo::Rcd));

        // And what a new image is actually seeded with.
        assert_eq!(
            fresh.params().luminance.sampling,
            Sampling::Demosaic(DemosaicAlgo::Rcd)
        );

        // Stated twice on purpose: the concrete value above is the tripwire that makes
        // moving the default a deliberate act, and this is the one that keeps the chain
        // honest if it moves again.
        assert_eq!(fresh.sampling(), Sampling::default());
        assert_eq!(fresh.params().luminance.sampling, Sampling::default());
    }

    #[test]
    fn lightbox_follows_the_viewer_until_told_not_to() {
        let mut s = Settings {
            viewer_background: 30.0,
            panel_background: 40.0,
            module_background: 50.0,
            lightbox_canvas: 10.0,
            lightbox_panel: 20.0,
            lightbox_module: 90.0,
            ..Settings::default()
        };
        assert!(s.lightbox_matches_viewer, "matching is the default");
        assert_eq!(s.lightbox_grounds(), [0.3, 0.4, 0.5]);
        s.lightbox_matches_viewer = false;
        assert_eq!(s.lightbox_grounds(), [0.1, 0.2, 0.9]);
    }

    #[test]
    fn an_older_file_missing_keys_opens_at_defaults() {
        // The compatibility rule. A file written before a setting existed must not
        // fail to parse, and must not take the settings around it down with it.
        let text = r#"
            tiff_suffix = "_master"
            viewer_background = 30.0
        "#;
        let s: Settings = toml::from_str(text).expect("a partial file must parse");
        assert_eq!(s.tiff_suffix, "_master");
        assert_eq!(s.viewer_background, 30.0);
        assert_eq!(s.screen_dither, Settings::default().screen_dither);
        assert_eq!(s.sampling(), Sampling::default());
        assert_eq!(
            s.lightbox_folder_root, None,
            "an existing install should adopt Home as its folder root"
        );
    }

    #[test]
    fn proof_preferences_round_trip_and_are_made_legal() {
        let mut s = Settings::default();
        let mut p = s.proof_prefs();
        p.target.container = export::Container::Jpeg;
        p.target.depth = export::Depth::Sixteen;
        p.scale = export::ProofScale::Full;
        p.dither = false;
        s.set_proof_prefs(p);
        let back = s.proof_prefs();
        assert_eq!(back.target.container, export::Container::Jpeg);
        assert_eq!(
            back.target.depth,
            export::Depth::Eight,
            "a JPEG proof is 8-bit"
        );
        assert_eq!(back.scale, export::ProofScale::Full);
        assert!(!back.dither);
    }

    #[test]
    fn a_file_from_before_masters_were_tiff_only_still_opens() {
        // The keys retired when a master became a 16-bit TIFF and dither split into
        // screen and proof. They are ignored, and the screen keeps dithering even
        // where the old per-image default had been switched off.
        let text = r#"
            tiff_color_space = "ecirgb"
            png_color_space = "monostar"
            jpeg_color_space = "srgb"
            export_depth = "8"
            dither = false
        "#;
        let s: Settings = toml::from_str(text).expect("retired keys must be ignored");
        assert_eq!(s.master_space(), export::Space::EciRgbV2);
        assert!(s.screen_dither);
        assert_eq!(
            s.master_target(),
            export::Target::master(export::Space::EciRgbV2)
        );
    }

    #[test]
    fn a_newer_file_with_unknown_keys_still_opens() {
        // The other direction: a setting added in a later version must not stop an
        // earlier build from reading the rest.
        let text = r#"
            screen_dither = false
            some_setting_from_the_future = "hello"
        "#;
        let s: Settings = toml::from_str(text).expect("unknown keys must be ignored");
        assert!(!s.screen_dither);
    }

    #[test]
    fn an_unrecognised_enum_key_falls_back_rather_than_failing() {
        // A hand-edited typo should cost that one setting, not the file.
        let text = r#"
            sampling = "supperpixel"
            weighting = "chartreuse"
            tone_map = "filmic"
            tiff_color_space = "pantone"
        "#;
        let s: Settings = toml::from_str(text).expect("parse");
        assert_eq!(s.sampling(), Sampling::default());
        assert_eq!(s.weighting(), Weighting::Photosite);
        assert_eq!(s.tone_map(), ToneMap::default());
        assert_eq!(s.master_space(), export::Space::default());
    }

    #[test]
    fn the_demosaic_algorithm_survives_leaving_demosaic_mode() {
        // Sampling and the algorithm are two settings, so choosing SuperPixel must
        // not forget which algorithm Demosaic would use — the same property the
        // develop panel's combo has, and the sidecar's.
        let mut s = Settings::default();
        s.set_sampling(Sampling::Demosaic(DemosaicAlgo::Amaze));
        s.set_sampling(Sampling::SuperPixel);
        assert_eq!(s.demosaic(), DemosaicAlgo::Amaze);
        s.set_sampling(Sampling::Demosaic(s.demosaic()));
        assert_eq!(s.sampling(), Sampling::Demosaic(DemosaicAlgo::Amaze));
    }

    #[test]
    fn malformed_toml_is_corrupt_not_a_panic() {
        assert!(toml::from_str::<Settings>("this is not toml = = =").is_err());
    }

    #[test]
    fn the_background_default_is_where_the_view_already_was() {
        // The canvas used to be two hand-matched constants: `theme::SURROUND` at
        // gray 23, and 0.09 in `display.wgsl`. Both are gone, replaced by this one
        // setting — so the seam between the letterbox and the shader fill is now
        // impossible rather than avoided. The default has to land where those
        // constants were, or turning the menu on would change the view.
        let bg = Settings::default().background();
        assert!((bg - 0.09).abs() < 1e-6, "background default drifted: {bg}");
        assert_eq!(
            (bg * 255.0).round() as u8,
            23,
            "no longer the grey it always was"
        );
    }

    #[test]
    fn an_export_is_named_after_its_source_plus_the_suffix() {
        let s = Settings::default();
        let src = Path::new("/raws/L1000016.DNG");
        let tiff = export::Target {
            container: export::Container::Tiff,
            depth: export::Depth::Sixteen,
            compression: export::Compression::None,
            space: export::Space::Monostar,
        };
        let png = export::Target {
            container: export::Container::Png,
            ..tiff
        };
        assert_eq!(s.export_name(src, tiff), "L1000016_monopro.tif");
        assert_eq!(s.export_name(src, png), "L1000016_monoproof.png");
    }

    #[test]
    fn the_two_containers_do_not_collide() {
        // A TIFF and a PNG of one frame land in the same folder. Equal stems would
        // put them one keystroke apart in a save dialog with no hint which is which.
        let s = Settings::default();
        assert_ne!(s.tiff_suffix, s.png_suffix);
    }

    #[test]
    fn an_empty_suffix_is_allowed() {
        let s = Settings {
            tiff_suffix: String::new(),
            ..Settings::default()
        };
        let t = export::Target {
            container: export::Container::Tiff,
            depth: export::Depth::Sixteen,
            compression: export::Compression::None,
            space: export::Space::Monostar,
        };
        assert_eq!(s.export_name(Path::new("/raws/a.dng"), t), "a.tif");
    }

    #[test]
    fn a_suffix_cannot_move_the_file_somewhere_else() {
        // The one way a naming preference could lose an export: a separator in the
        // suffix would make the "name" a path. It is a hand-typed field, so this is
        // reachable by typo, not only by mischief.
        let s = Settings {
            tiff_suffix: "/../../etc/passwd".into(),
            ..Settings::default()
        };
        let t = export::Target {
            container: export::Container::Tiff,
            depth: export::Depth::Sixteen,
            compression: export::Compression::None,
            space: export::Space::Monostar,
        };
        let name = s.export_name(Path::new("/raws/a.dng"), t);
        assert!(
            !name.contains('/') && !name.contains('\\'),
            "{name} is a path, not a name"
        );
    }
}

#[cfg(test)]
mod sheet_tests {
    use super::{Section, Sheet};

    #[test]
    fn every_section_is_reachable_and_named() {
        // The sidebar is built from `ALL`, so a section missing from it is a page with
        // no way in — the failure the sidebar exists to fix, reintroduced.
        //
        // the maintainer's order, which is the order you meet these things: how it looks, how
        // it behaves, what it does to a picture, what it writes, then the keys.
        let names: Vec<&str> = Section::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(
            names,
            [
                "General",
                "Viewer",
                "Lightbox",
                "Processing",
                "Export",
                "Controls",
                "About"
            ]
        );
    }

    #[test]
    fn with_no_search_a_page_shows_only_its_own_rows() {
        let mut sheet = Sheet::default();
        // General is where an unset sidebar starts, so the window is never blank.
        assert!(sheet.matches(Section::General, "STARTUP launch"));
        assert!(!sheet.matches(Section::Export, "FILE NAMING tiff"));

        sheet.section = Some(Section::Export);
        assert!(sheet.matches(Section::Export, "FILE NAMING tiff"));
        assert!(!sheet.matches(Section::Viewer, "SURROUND mount"));
    }

    #[test]
    fn a_search_reaches_across_pages() {
        // **The reason search ignores the sidebar.** You search because you do not
        // know where the thing lives, so one that only looked in the page you happened
        // to be standing on would fail exactly when it is needed. Standing on
        // General, a query for a row that lives under Export must still find it.
        let sheet = Sheet {
            query: "tiff".into(),
            ..Default::default()
        };
        assert_eq!(
            sheet.current(),
            Section::General,
            "still standing on General"
        );
        assert!(sheet.matches(Section::Export, "FILE NAMING tiff png suffix"));
        assert!(!sheet.matches(Section::Viewer, "SURROUND mount colour"));
    }

    #[test]
    fn a_search_matches_the_section_name_too() {
        // Typing the name of a page is a legitimate way to ask for the page, and it is
        // what somebody does when they know the group but not the row.
        let sheet = Sheet {
            query: "Lightbox".into(),
            ..Default::default()
        };
        assert!(sheet.matches(Section::Lightbox, "TILES filenames folders"));
        assert!(!sheet.matches(Section::Export, "FILE NAMING tiff"));
    }

    #[test]
    fn searching_is_case_and_whitespace_insensitive() {
        for q in ["PPI", "  ppi  ", "Ppi"] {
            let sheet = Sheet {
                query: q.into(),
                ..Default::default()
            };
            assert!(
                sheet.matches(Section::Export, "OUTPUT resolution ppi"),
                "{q:?} missed"
            );
        }
    }

    #[test]
    fn a_box_with_only_spaces_is_not_a_search() {
        // Otherwise clearing the field one character at a time would leave the window
        // blank at the last space rather than returning to the page you were on.
        let sheet = Sheet {
            query: "   ".into(),
            ..Default::default()
        };
        assert!(!sheet.searching());
        assert!(
            sheet.matches(Section::General, "STARTUP launch"),
            "back to the page"
        );
        assert!(!sheet.matches(Section::Export, "FILE NAMING tiff"));
    }
}
