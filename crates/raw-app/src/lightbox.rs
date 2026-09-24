//! The browser mode: a folder tree down the left, a grid of camera thumbnails
//! filling the rest.
//!
//! # It is a browser, not an editor
//!
//! Nothing here changes a pixel. Lightbox picks files, and later stages will record
//! what you think of them; everything it draws is a camera thumbnail. If something
//! in this module starts reaching for a develop control, it has taken a wrong turn.
//!
//! # Why it is a mode and not a panel
//!
//! Entering takes the Develop panels off the screen entirely rather than docking a
//! grid beside them. The shell is FastRawViewer's — tree left, grid right, nothing
//! else — and the two arrangements have no overlap worth reconciling.
//!
//! **The restore is free, and that is by construction.** This module never touches
//! [`crate::layout::Layout`]. Develop's chrome is not dismantled on entry, it is
//! simply not drawn; so returning puts every panel, split and tab back exactly where
//! it was because nothing moved. The one thing that does need saying explicitly is
//! floating panels: those are OS windows, and a window nobody drew is still a window
//! on screen, so `main` skips their viewports while this mode is up.
//!
//! That follows the rule `tab` already set — a floating Develop still covering half
//! the screen is not "out of the way". The prototype instead preferred to leave
//! torn-off panels floating over the Lightbox (`monopro.py:35780-35800`); this app
//! has a settled answer for what "hide the panels" means and one gesture should not
//! mean two things, so the prototype is not followed here.
//!
//! # Speed is the build constraint
//!
//! A directory must appear *now*. The rules that come from it:
//!
//! - **Never decode a raw to draw a tile.** [`raw_core::preview::tile`] pulls the
//!   camera's own embedded JPEG. A file that has none gets a placeholder, not a
//!   decode.
//! - **Only visible rows are drawn or requested**, so a folder of five thousand
//!   costs the same as a folder of fifty until you scroll.
//! - **Scrolling cancels what you scrolled past**, which is the queue's job and the
//!   reason it was written generic over its key.
//! - **A tile is written to disk once.** The second visit to a folder reads JPEGs at
//!   a few hundred microseconds each and never opens a raw at all.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::decode;
use crate::theme;

/// A rung of the size ladder.
///
/// **The small end is a target width; the big end is a column count**, and the split
/// is the point. A width ladder cannot promise a column count: 760 points is two
/// columns on a 1450 pt grid and three on a 2560 one, so "let me see two across"
/// could not be asked for on a wide screen. A column count promises it at any width.
///
/// The other direction is equally true, which is why the small end is not columns —
/// "twelve across" means a different sized tile on every window, where 120 points is
/// a contact sheet everywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rung {
    /// About this wide; the grid divides the row to fit.
    Width(f32),
    /// Exactly this many across, whatever that makes them.
    Columns(usize),
}

/// The size ladder.
///
/// The first six are the prototype's (`monopro.py:25423`) and are a good ladder —
/// each step roughly a third larger than the last, which is about the smallest change
/// that reads as a change. The last two are the maintainer's ask, and they are counts rather
/// than widths for the reason [`Rung`] gives.
pub const SIZES: [Rung; 8] = [
    Rung::Width(120.0),
    Rung::Width(160.0),
    Rung::Width(210.0),
    Rung::Width(260.0),
    Rung::Width(320.0),
    Rung::Width(400.0),
    Rung::Columns(2),
    Rung::Columns(1),
];

/// The default: large enough to judge a frame, small enough to see a shoot.
pub const DEFAULT_SIZE: usize = 2;

/// Gap between tiles, and the grid's margin. The prototype's `QGridLayout` numbers
/// (`monopro.py:23346`), which are tighter than they look written down: at 3 points
/// the grid reads as a contact sheet rather than as a row of separate cards.
const SPACING: f32 = 3.0;
const MARGIN: f32 = 5.0;

/// Clear space between the image and every edge of its image region.
///
/// This was 4 points in the prototype. The extra 5 points gives each frame a more
/// deliberate contact-sheet margin without moving the filename or rating rows.
const IMAGE_PAD: f32 = 9.0;

/// Height reserved under the image for the stars row and the colour label dot.
///
/// Nothing draws in it yet — that furniture is the tile's own stage. It is reserved
/// now rather than later so the card geometry does not change under the maintainer's feet when
/// it lands: the prototype's row is five 18-point stars against a 14-point dot
/// (`monopro.py:22722-22742`), and 20 points is what that comes to.
const META_ROW_H: f32 = 20.0;

/// Height of the filename line, when it is shown.
///
/// **A second row above the marks, not a fight with them.** Both used to want the one
/// 20 pt strip and the stars won; the maintainer's answer is that the picture gives up the
/// height instead. It costs the most where it hurts least — a landscape frame is
/// letterboxed in a portrait card anyway, and a vertical simply sits a little smaller.
const NAME_ROW_H: f32 = 13.0;

/// Empty rating marks need to remain quieter than a chosen amber star while still
/// reading as five available targets on the dark contact sheet.
const EMPTY_STAR: egui::Color32 = egui::Color32::from_gray(86);

/// Raw formats — the ones `rawler` decodes, and the ones
/// [`raw_core::preview::tile`] can pull an embedded thumbnail from.
pub(crate) const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "ari", "arw", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "erf", "fff", "iiq", "kdc",
    "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "raf", "raw", "rw2", "rwl", "sr2", "srf",
    "srw", "x3f",
];

/// Ordinary picture formats, thumbnailed with the `image` crate rather than rawler.
///
/// **the maintainer asked why a browser would refuse to show a JPEG, and there is no good
/// answer.** The original rule — raws only, so a folder of raws with the odd export
/// in it shows the shoot — was solving a sorting problem with a blindfold. A browser
/// that cannot show you a file you can see in the Finder is the browser being wrong;
/// if exports get in the way, that is what the Type sort and the filters are for.
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "tif", "tiff", "webp", "bmp", "gif"];

fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

pub(crate) fn is_raw(path: &Path) -> bool {
    RAW_EXTENSIONS.contains(&ext_of(path).as_str())
}

fn is_image(path: &Path) -> bool {
    IMAGE_EXTENSIONS.contains(&ext_of(path).as_str())
}

/// Anything the grid will list.
pub(crate) fn is_listed(path: &Path) -> bool {
    is_raw(path) || is_image(path)
}

/// Files the filename index may find even when Develop cannot decode them.
///
/// PSD/PSB are valuable finished-image assets to locate, but neither rawler nor the
/// grid's image decoder can render them. Keeping this broader than [`is_listed`]
/// makes them searchable without falsely presenting them as developable pictures.
pub(crate) fn is_searchable(path: &Path) -> bool {
    is_listed(path) || matches!(ext_of(path).as_str(), "psd" | "psb")
}

/// How many thumbnails may be made at once.
///
/// **It follows the machine now, between four and eight.** It was a flat four, and the
/// argument for that number was memory: a thumbnail job holds one decoded embedded
/// JPEG, which on a 100 MP body is 8256x5504 — 136 MB of RGB8 — *plus the copy
/// `to_rgb8` made*. That copy is gone (`into_rgb8` consumes the decode instead), so the
/// worst case per job has halved, and the ceiling that four was protecting is twice as
/// far away as it was.
///
/// The work is a JPEG decode and a box filter, both CPU-bound, so it scales with cores
/// until it runs out of them. Two are left for the UI thread and the GPU queue, because
/// a grid that fills fast and scrolls in jerks is not faster in the way that matters.
///
/// **Still capped at eight**, and the cap is the memory argument surviving in its
/// weakened form: eight 100 MP frames in flight is about 1.1 GB of transient
/// allocation, which is a lot to ask of a machine that may not have been the one this
/// was measured on. A core count is not a memory budget, and past eight the disk is
/// usually the limit anyway.
///
/// Higher than the decode queue's two for the reason it always was: the jobs are
/// shorter and hold an order of magnitude less, a scene decode carrying a full sensor
/// *and* a full scene where this carries one JPEG.
fn thumb_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2))
        .unwrap_or(4)
        .clamp(4, 8)
}

/// Rows of tiles requested beyond the visible ones, above and below.
///
/// One screen's worth in each direction. Enough that an ordinary scroll lands on
/// tiles that are already there, few enough that flinging through a large folder
/// cancels most of what it passes rather than queueing all of it.
const OVERSCAN_ROWS: usize = 2;

/// Textures kept before the least recently seen are dropped.
///
/// A 512 px tile is about 700 KB on the GPU, so this is a ~350 MB ceiling. The
/// *disk* cache is untouched by eviction — dropping a texture costs a few hundred
/// microseconds to re-read, not a raw decode.
const TEXTURE_CAP: usize = 500;

// ------------------------------------------------------------------- the panels

/// The panes Lightbox can show.
///
/// **Its own tree, not Develop's.** the maintainer's call, and the reason is grouping: EXIF
/// wants to be a panel, and the moment one of them is a panel they all should be —
/// otherwise the folder tree is a fixed wall down the left that the EXIF panel has to
/// work around. As panes they can be tabbed together, split, or put on either side,
/// and the arrangement is yours rather than the one this file happened to choose.
///
/// A *second* tree rather than adding variants to [`crate::layout::Pane`], because
/// the two modes share no pane at all: there is no viewport here and no grid there,
/// and one tree holding both would have to hide half of itself in either mode. Two
/// trees also keep the promise the mode switch already makes — Develop's arrangement
/// is untouched while you are in here, because this does not touch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pane {
    Grid,
    Folders,
    Search,
    Favorites,
    Exif,
}

impl Pane {
    /// Every pane, in the order a rebuilt default lays them out.
    pub const ALL: [Self; 5] = [
        Self::Grid,
        Self::Folders,
        Self::Search,
        Self::Favorites,
        Self::Exif,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Grid => "GRID",
            Self::Folders => "FOLDERS",
            Self::Search => "SEARCH",
            Self::Favorites => "FAVORITES",
            // Keep the serialized `Exif` identity so existing Lightbox layouts
            // restore, but name the expanded camera + IPTC panel for what it is.
            Self::Exif => "METADATA",
        }
    }
}

/// The default arrangement: the three lists tabbed down the left, the grid filling
/// the rest.
///
/// **Tabbed rather than stacked**, which is what makes the default useful: three
/// panels sharing one column each get a third of the height, and a folder tree in a
/// third of a screen is a scroll bar with a hint of tree. Tabbed, each gets the whole
/// column, and dragging one out to split them is a gesture away.
fn default_tree() -> egui_tiles::Tree<Pane> {
    let mut tiles = egui_tiles::Tiles::default();
    let folders = tiles.insert_pane(Pane::Folders);
    let search = tiles.insert_pane(Pane::Search);
    let favorites = tiles.insert_pane(Pane::Favorites);
    let exif = tiles.insert_pane(Pane::Exif);
    let grid = tiles.insert_pane(Pane::Grid);

    // **Folders over Favorites, three parts to one** — the maintainer's arrangement. Favorites
    // is a short list you glance at, where a folder tree is the thing you work in, so
    // they stack rather than share a tab bar: tabbed, going to a favorite meant losing
    // sight of where you were in the tree.
    //
    // EXIF stays tabbed *with* Folders, because those two genuinely compete for the
    // same space — you are either finding a frame or reading one.
    let mut top = egui_tiles::Tabs::new(vec![folders, search, exif]);
    top.active = Some(folders);
    let top = tiles.insert_container(egui_tiles::Container::Tabs(top));

    let mut left = egui_tiles::Linear::new(egui_tiles::LinearDir::Vertical, vec![top, favorites]);
    left.shares.set_share(top, 3.0);
    left.shares.set_share(favorites, 1.0);
    let left = tiles.insert_container(egui_tiles::Container::Linear(left));

    let mut row = egui_tiles::Linear::new(egui_tiles::LinearDir::Horizontal, vec![left, grid]);
    row.shares.set_share(left, 240.0);
    row.shares.set_share(grid, 900.0);

    let root = tiles.insert_container(egui_tiles::Container::Linear(row));
    egui_tiles::Tree::new("monopro-lightbox", root, tiles)
}

/// Whether a restored tree is one this build can run.
///
/// The same rule `layout::heal` applies, and for the same reason: a stored layout is
/// inherited forever unless something checks it, so a tree missing a pane or carrying
/// one twice is thrown away for the default rather than run. One lost arrangement on
/// an upgrade beats a mode that opens wrong every time.
fn tree_is_sound(tree: &egui_tiles::Tree<Pane>) -> bool {
    if tree.root.is_none() {
        return false;
    }
    let mut seen: Vec<Pane> = tree
        .tiles
        .tiles()
        .filter_map(|t| match t {
            egui_tiles::Tile::Pane(p) => Some(*p),
            _ => None,
        })
        .collect();
    seen.sort_by_key(|p| format!("{p:?}"));
    seen.dedup();
    seen.len() == Pane::ALL.len()
}

// --------------------------------------------------------------- sort and filter

/// How the grid is ordered. The prototype's seven (`monopro.py:25660`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    #[default]
    Filename,
    /// Capture date, from the file's EXIF. See [`Lightbox::sweep_dates`] — this is
    /// the only sort that costs anything to compute.
    Captured,
    /// When this app last wrote a sidecar, which is "when did I work on it".
    Developed,
    Rating,
    Label,
    /// Extension, so a folder of mixed bodies groups by camera without needing to
    /// know anything about cameras.
    Kind,
    /// Yours. See [`Lightbox::reorder`].
    Manual,
}

impl Sort {
    pub const ALL: [Self; 7] = [
        Self::Filename,
        Self::Captured,
        Self::Developed,
        Self::Rating,
        Self::Label,
        Self::Kind,
        Self::Manual,
    ];

    /// Which end a fresh choice of this sort should start at.
    ///
    /// Captured is deliberately **not** in here: a shoot read oldest-first is the same
    /// order the filenames are in, which is what makes Date feel like a correction to
    /// Name rather than a different list.
    pub fn natural_descending(self) -> bool {
        matches!(self, Self::Rating | Self::Developed)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Filename => "NAME",
            Self::Captured => "DATE",
            Self::Developed => "DEVELOPED",
            Self::Rating => "RATING",
            Self::Label => "LABEL",
            Self::Kind => "TYPE",
            Self::Manual => "MANUAL",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Filename => "filename",
            Self::Captured => "captured",
            Self::Developed => "developed",
            Self::Rating => "rating",
            Self::Label => "label",
            Self::Kind => "kind",
            Self::Manual => "manual",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.key() == s)
    }
}

/// What the grid is allowed to show.
///
/// All of these are *and*-ed, and all of them default to off — an empty filter shows
/// the folder, which is the state you want to be able to get back to in one gesture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filters {
    /// Files from folders below this one, too.
    pub subfolders: bool,
    /// Only files with no rating. **Mutually exclusive with `stars`** by meaning
    /// rather than by enforcement: unrated is `rating == 0` and the threshold is
    /// `rating >= n`, so asking for both shows nothing. The footer greys the
    /// threshold while this is on rather than letting you build an empty view.
    pub unrated: bool,
    /// Show only files rated at least this. Zero is off.
    pub stars: i32,
    /// Show only files carrying one of these labels. Empty is off.
    pub labels: Vec<String>,
}

impl Filters {
    pub fn any(&self) -> bool {
        self.subfolders || self.unrated || self.stars > 0 || !self.labels.is_empty()
    }

    /// Whether one file survives. `subfolders` is not asked here — it decides what
    /// is *listed*, not what is shown, and is applied when the folder is read.
    fn admits(&self, e: &Entry) -> bool {
        // **A folder is never filtered out.** It carries no rating and no label, so
        // every one of the tests below would reject it — and a two-star filter that
        // quietly removed the way *out* of the folder would be the grid trapping you
        // in it. Folders are navigation rather than content, and the filters are about
        // content. See `Settings::lightbox_folders`.
        if e.kind == Kind::Folder {
            return true;
        }
        if self.unrated && e.rating != 0 {
            return false;
        }
        if self.stars > 0 && e.rating < self.stars {
            return false;
        }
        if !self.labels.is_empty()
            && !e
                .label
                .as_deref()
                .is_some_and(|l| self.labels.iter().any(|w| w == l))
        {
            return false;
        }
        true
    }
}

// ------------------------------------------------------------------ the entries

/// What one tile in the grid actually is.
///
/// The grid held nothing but pictures until the two listing settings arrived, and the
/// distinction has to be a *stored* property rather than something re-derived from the
/// path at each use. Three call sites need it every frame — the thumbnail request, the
/// sort, and the tile's own drawing — and asking the filesystem sixty times a second
/// whether something is a directory is the kind of thing `Entry::edited` already
/// documents not doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A raw or an ordinary picture. **The only kind that gets a thumbnail**, and the
    /// only kind a double click takes into Develop.
    Picture,
    /// A subfolder, listed when `Settings::lightbox_folders` is on. Double-clicking one
    /// navigates into it rather than opening anything.
    Folder,
    /// Any other file, listed when `Settings::lightbox_other_files` is on and drawn as
    /// its type's icon. Inert: there is nothing to decode and nothing to develop.
    Other,
}

/// Does this sidecar mean the frame has been **developed**?
///
/// One place, because the answer is asked at listing time and again on every mode
/// switch, and the two disagreeing is precisely the failure the mark had: a rating
/// wrote a sidecar, one site read "a sidecar exists" as "worked on", and the amber
/// rule appeared on frames nobody had opened.
///
/// **Corrupt still counts.** The badge says something is there that this app wrote and
/// cannot now read, which is a fact worth surfacing — and `sidecar::write` refuses to
/// overwrite a corrupt file, so hiding the mark would hide the one file that needs
/// looking at, with no other sign that anything is wrong.
fn developed(side: &raw_core::sidecar::Loaded<raw_core::sidecar::Sidecar>) -> bool {
    match side {
        raw_core::sidecar::Loaded::Absent => false,
        raw_core::sidecar::Loaded::Corrupt(_) => true,
        raw_core::sidecar::Loaded::Ok(s) => s.is_developed(),
    }
}

/// One file in the current folder.
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// Picture, folder, or something else. See [`Kind`].
    pub kind: Kind,
    /// This file has been **developed** — its sidecar carries render edits, not just a
    /// rating. See [`developed`], which is the one place that decides.
    ///
    /// Sampled when the folder is read rather than checked per frame: one small XML
    /// parse per file that has a sidecar, which is nothing against a directory listing
    /// and a great deal against sixty frames a second.
    pub edited: bool,
    /// `xmp:Rating`, 0–5. Zero is unrated and is not written.
    pub rating: i32,
    /// `xmp:Label`. A string, because the format's is — see
    /// `raw_core::sidecar::Metadata::label`. A word this app does not know is
    /// another application's and is displayed as no dot, not as a wrong one.
    pub label: Option<String>,
    /// Lowercase extension, for the Type sort and for choosing a file icon.
    ///
    /// Named `ext` since [`Kind`] arrived; it was `kind`, and two fields called that
    /// meaning different things is how a sort ends up grouping by the wrong one.
    /// Empty for a folder.
    pub ext: String,
    /// Which way up to show this frame, replacing the file's own EXIF tag.
    ///
    /// **The same value Develop composes with**, read from the sidecar, because a
    /// frame turned in the grid and the same frame open in Develop have to agree —
    /// and once a tile can be a *developed* render they are the same picture. `None`
    /// is as shot.
    ///
    /// Turning one still does not make it look edited: `Sidecar::is_developed`
    /// ignores orientation for exactly this reason.
    pub orientation: Option<raw_core::Orientation>,
    /// When this app last wrote a sidecar — the sidecar's own mtime, which costs
    /// nothing because the folder read already stats it. `None` for a file nobody
    /// has worked on, and those sort together at the end.
    pub developed: Option<std::time::SystemTime>,
    /// Capture date, from EXIF. **`None` until somebody asks for it**: reading it
    /// means faulting the whole raw through the page cache, so it is swept once when
    /// the Date sort is first chosen and then cached on disk forever. See
    /// [`Lightbox::sweep_dates`].
    pub captured: Option<std::time::SystemTime>,
}

/// Build the grid's view of one filesystem entry. Search and folder browsing must
/// agree here: a four-star frame found by name should still arrive four-starred.
fn entry_for(path: PathBuf, kind: Kind) -> Entry {
    let picture = kind == Kind::Picture;
    let side = if picture && path.exists() {
        raw_core::sidecar::read(&path)
    } else {
        raw_core::sidecar::Loaded::Absent
    };
    let edited = developed(&side);
    let loaded = side.ok();
    let orientation = loaded
        .as_ref()
        .and_then(|s| s.params.composition.orientation);
    let meta = loaded.map(|s| s.metadata);
    Entry {
        name: name_of(&path),
        kind,
        edited,
        rating: meta
            .as_ref()
            .and_then(|m| m.rating)
            .unwrap_or(0)
            .clamp(0, 5),
        orientation,
        label: meta.and_then(|m| m.label),
        ext: if picture || kind == Kind::Other {
            path.extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase()
        } else {
            String::new()
        },
        developed: picture
            .then(|| {
                std::fs::metadata(raw_core::sidecar::path_for(&path))
                    .ok()
                    .and_then(|m| m.modified().ok())
            })
            .flatten(),
        captured: None,
        path,
    }
}

/// A search hit is deliberately cheap to materialise. A broad query can return
/// thousands of paths; synchronously parsing thousands of sidecars would turn a
/// fast filename index into a slow metadata catalogue. Metadata enrichment belongs
/// on its own worker in the next search slice.
fn search_entry(path: PathBuf) -> Entry {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    Entry {
        name: name_of(&path),
        path,
        kind: if matches!(ext.as_str(), "psd" | "psb") {
            Kind::Other
        } else {
            Kind::Picture
        },
        edited: false,
        rating: 0,
        label: None,
        ext,
        orientation: None,
        developed: None,
        captured: None,
    }
}

/// Which thumbnail a queued job belongs to.
///
/// `Copy`, because [`decode::Queue`] requires it — which is also why this is an
/// index and a generation rather than a path. **The generation is what makes
/// changing folders safe**: jobs from the old folder keep running (the queue cancels
/// at task boundaries, not mid-work), and their results arrive keyed to a generation
/// that no longer matches and are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    generation: u32,
    idx: u32,
    /// The full-frame preview rather than a tile.
    ///
    /// Part of the key so both can be in flight for the same file at once: opening
    /// the preview must not cancel or replace the tile behind it, and the two are
    /// different sizes of the same picture.
    full: bool,
}

/// Full Quick Look textures kept warm: the frame in hand and one step either way.
///
/// At the screen-preview edge these are roughly 8–12 MB each, so three buys instant
/// arrowing without quietly turning Lightbox into an unbounded full-frame cache.
const PREVIEW_CACHE_CAP: usize = 3;

struct PreviewTexture {
    texture: egui::TextureHandle,
    /// Frame it was last used on, for the three-entry LRU.
    seen: u64,
}

/// What a tile has.
enum Tile {
    /// Queued or running.
    Pending,
    Ready {
        texture: egui::TextureHandle,
        /// Frame it was last drawn on, for eviction.
        seen: u64,
    },
    /// The file carries no embedded preview, or could not be read. **Not an error**
    /// — a raw without a preview is a property of the file, and the grid says so
    /// with a placeholder rather than an alert.
    Missing,
}

// -------------------------------------------------------------------- the tree

/// The folder tree's own state: what is open, and what each open folder contains.
///
/// Listings are cached because a tree redraws every frame and `read_dir` on a
/// network volume is not free. They are read once when a folder is first expanded,
/// and again after the Refresh command (`Lightbox::refresh`), which forgets them.
/// There is no watcher: a watcher cannot see changes another machine makes on a
/// network volume, which is where it would be needed most.
pub struct FolderTree {
    /// The configured local root. `None` resolves to Home; external volumes are not
    /// stored here because they are discovered live whenever the panel is drawn.
    local_root: Option<PathBuf>,
    roots: Vec<PathBuf>,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
}

impl FolderTree {
    fn new() -> Self {
        Self {
            local_root: None,
            roots: folder_roots(None),
            expanded: HashSet::new(),
            children: HashMap::new(),
        }
    }

    fn set_local_root(&mut self, root: Option<&Path>) {
        let root = root.map(Path::to_path_buf);
        if self.local_root == root {
            return;
        }
        self.local_root = root;
        self.refresh_roots();
    }

    /// Refresh the top-level devices without throwing away expanded folders.
    ///
    /// The platform's mount list changes while the app is running when an SD card or
    /// drive is inserted or ejected. A root list captured only at startup would make
    /// the new device invisible until monopro was relaunched.
    fn refresh_roots(&mut self) {
        self.roots = folder_roots(self.local_root.as_deref());
    }

    /// Subdirectories of `dir`, read once and remembered.
    ///
    /// Hidden folders are skipped: a photo tree has no use for `.DS_Store`'s
    /// neighbours, and `~/Library` alone would double the visible size of home.
    fn children_of(&mut self, dir: &Path) -> &[PathBuf] {
        if !self.children.contains_key(dir) {
            let mut kids: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .filter(|p| {
                    !p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with('.'))
                })
                // `/Volumes/Macintosh HD` is a symlink back to `/`. Without this,
                // expanding Macintosh HD → Volumes offers a route back to the root
                // and lets the tree recurse forever one click at a time.
                .filter(|p| !is_startup_disk_alias(p))
                .collect();
            kids.sort_by_key(|p| name_of(p).to_lowercase());
            self.children.insert(dir.to_path_buf(), kids);
        }
        &self.children[dir]
    }

    /// Open every folder on the way to `dir`, so revealing a path shows it.
    fn reveal(&mut self, dir: &Path) {
        for a in dir.ancestors().skip(1) {
            self.expanded.insert(a.to_path_buf());
        }
    }
}

fn home() -> Option<PathBuf> {
    crate::platform::home_dir()
}

/// The places a photographer can begin browsing: one chosen local root and every
/// currently mounted external volume. The local root defaults to Home; the system
/// root only appears separately when it is explicitly chosen.
fn folder_roots(configured: Option<&Path>) -> Vec<PathBuf> {
    let local = configured
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(home)
        .unwrap_or_else(|| PathBuf::from("/"));
    let mut roots = vec![local];

    for volume in crate::platform::external_roots()
        .into_iter()
        .filter(|path| !is_startup_disk_alias(path))
    {
        if !roots.contains(&volume) {
            roots.push(volume);
        }
    }
    roots
}

fn folder_root_name(path: &Path) -> String {
    crate::platform::root_name(path)
}

fn is_startup_disk_alias(path: &Path) -> bool {
    path != Path::new("/")
        && std::fs::canonicalize(path).is_ok_and(|target| target == Path::new("/"))
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Compact location context for the footer: this folder plus at most two parents.
/// The tree already shows the full hierarchy; the footer only needs enough path to
/// distinguish shoots with the same leaf name without swallowing the control row.
fn footer_folder_path(path: &Path) -> String {
    let mut names: Vec<String> = path
        .ancestors()
        .filter_map(|ancestor| ancestor.file_name())
        .take(3)
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    names.reverse();
    if names.is_empty() {
        folder_root_name(path)
    } else {
        names.join("/")
    }
}

/// The intentionally narrow clipboard behind Lightbox's Copy/Paste Settings.
///
/// This is not a sidecar clipboard: carrying metadata with it would replace the
/// destination's authorship, IPTC, rating, and label. Dodge/Burn is excluded too:
/// painted local work belongs to the source image's geometry, not to a reusable
/// Develop recipe.
#[derive(Clone)]
struct CopiedDevelop {
    params: raw_core::Params,
    source: String,
}

struct RenameCaches {
    source: PathBuf,
    camera: Option<PathBuf>,
    captured: Option<PathBuf>,
    edited: Option<PathBuf>,
}

// --------------------------------------------------------------- the mode itself

pub struct Lightbox {
    /// Whether the mode is up. `main` reads this to decide what to draw.
    pub active: bool,
    /// Canvas, panel and card greys, from `Settings::lightbox_grounds`. Set by
    /// `main` every frame, so a Settings change is live.
    pub grounds: [u8; 3],
    /// Index into [`SIZES`].
    pub size: usize,
    pub folder: Option<PathBuf>,
    pub folders: FolderTree,

    entries: Vec<Entry>,
    generation: u32,
    tiles: HashMap<u32, Tile>,
    queue: decode::Queue<Key, Option<raw_core::preview::Rgb8>>,
    /// Quick Look cannot wait behind a screenful of thumbnail jobs. One dedicated
    /// worker also bounds its peak memory: full previews are much larger than tiles.
    preview_queue: decode::Queue<Key, Option<raw_core::preview::Rgb8>>,
    cache_dir: Option<PathBuf>,
    /// Developed frames still waiting for their tile to be rendered from the sidecar.
    ///
    /// Rebuilt whenever the answer could have changed — a folder read, a sidecar
    /// appearing, the preference going on. Drained by `App`, which owns the GPU this
    /// needs and hands each finished tile back through [`Self::developed_tile_done`].
    developed_wanted: std::collections::VecDeque<u32>,
    frame: u64,
    /// Set when the folder changes, so the grid jumps back to the top rather than
    /// keeping a scroll offset that means nothing in the new folder.
    reset_scroll: bool,
    /// The grid's width as last laid out, so `resize` can tell whether a rung would
    /// change anything before it steps onto it. Zero until the first frame.
    last_avail: f32,
    /// The column count the grid was last laid out at.
    ///
    /// Changing it — the size ladder, or `tab` taking the panels away — changes how
    /// tall the whole grid is, and a scroll offset held in points then means a
    /// different row. the maintainer pressed `tab` and the view jumped to the end of the
    /// folder, which is that: fewer rows, same offset, clamped to the bottom.
    last_cols: usize,
    /// Where `⇧`+arrow has walked to, which is **not** the anchor.
    ///
    /// `⇧`+click keeps `selected` where it is and puts the run in `batch` — the anchor
    /// is what a run is measured *from*, so it must not move. `⇧`+arrow has to work the
    /// same way, which means the moving end needs somewhere of its own to live.
    ///
    /// `None` is "wherever `selected` is", so a fresh grid needs no initialisation and a
    /// plain arrow can reset the pair by clearing this.
    head: Option<u32>,
    /// A folder tile was double-clicked, and the grid should move into it.
    ///
    /// Recorded during the tile loop and acted on after it, because `open_folder`
    /// rebuilds the very `entries` that loop is walking. The same arrangement
    /// `App::take_info_clicks` and the grid's own `open` already use.
    enter: Option<PathBuf>,
    /// Set when the keyboard moved the selection, or Quick Look closed, so the grid
    /// scrolls to the selected tile.
    ///
    /// Only the keyboard: a click is already on screen by definition, and scrolling
    /// to a tile you just pointed at would jump the grid under your hand.
    follow: bool,
    /// The tile with the ring around it — the anchor every other selection gesture
    /// is relative to. An index rather than a path, because the folder it indexes
    /// into is retired wholesale when you leave it.
    selected: Option<u32>,
    /// Filenames under the tiles. Mirrors `Settings::lightbox_filenames`, pushed in
    /// once a frame rather than read through a borrow of the whole settings struct.
    pub show_filenames: bool,
    /// Border, and later the stars and label, hidden. Mirrors
    /// `Settings::frameless_tiles`. **Independent of the filename toggle**, which is
    /// the prototype's arrangement and the useful one: frameless with names on is a
    /// contact sheet, and frameless with names off is a wall of pictures.
    pub frameless: bool,
    /// Draw tiles from what Develop rendered, where there is one. Mirrors
    /// `Settings::lightbox_xmp_thumbnails`. The cached developed render itself is
    /// kept independently so Contact Sheet's Developed choice remains authoritative.
    pub xmp_thumbnails: bool,
    /// Draw the amber edited rule. Mirrors `Settings::lightbox_edited_mark`, pushed in
    /// once a frame like the other display flags — it changes how a tile is *drawn* and
    /// so costs nothing to write every frame.
    pub edited_mark: bool,
    /// Subfolders as tiles. Mirrors `Settings::lightbox_folders`.
    ///
    /// **Private, with [`Lightbox::set_listing`] as the way in**, unlike the two
    /// display mirrors above. Those change how an entry is *drawn* and can be written
    /// every frame for free; this one changes which entries exist, so writing it has to
    /// re-read the folder. A public field would make that the caller's job to remember.
    show_folders: bool,
    /// Files the grid cannot draw. Mirrors `Settings::lightbox_other_files`. Private
    /// for the same reason as `show_folders`.
    show_other_files: bool,
    /// Draw the thumbnails as gray. Mirrors `Settings::lightbox_gray`; mutation goes
    /// through [`Lightbox::set_grey`] because changing it invalidates every thumbnail
    /// texture already in hand.
    grey: bool,

    pub sort: Sort,
    pub descending: bool,
    pub filters: Filters,
    /// Indices into `entries`, sorted and filtered — the order actually drawn.
    ///
    /// The grid iterates this, never `entries`, so a thumbnail keyed by an entry's
    /// index survives every change of sort without being asked for again.
    visible: Vec<u32>,
    /// This folder's manual order, as filenames. Empty until something is dragged.
    manual: Vec<String>,
    /// The tile being dragged, and where it would land — `None` when nothing is.
    drag: Option<(u32, usize)>,
    /// The multi-selection, as entry indices.
    ///
    /// **Separate from `selected`, which stays the anchor.** The two are different
    /// things and the prototype draws them differently: the anchor is where a
    /// `⇧`-click measures its range from, and the batch is what an action applies to.
    /// Collapsing them would make shift-clicking backwards impossible to express.
    batch: std::collections::BTreeSet<u32>,

    /// This mode's own panel arrangement. See [`Pane`].
    pub tree: egui_tiles::Tree<Pane>,
    /// The last measured tile sizes, used to keep a panel's width when it is moved
    /// from one side of the grid to the other.
    panel_sizes: HashMap<egui_tiles::TileId, egui::Vec2>,
    /// A drop changed the tree's shape, so the next layout pass must restore panel
    /// widths and leave the grid to absorb the difference.
    tree_restructured: bool,
    restore_panel_pixels: bool,
    /// `tab` has taken the panels away and left the grid.
    panels_hidden: bool,
    /// Folders you keep. Order is the order you added them.
    pub favorites: Vec<PathBuf>,
    /// The full-frame preview: which entry it is showing, and its texture.
    ///
    /// **`Space` opens it, `Space` or `Esc` closes it** — the pair every mode in this
    /// app offers, because a view you cannot leave by the key that opened it is a
    /// trap. Nothing is put back on close because nothing was taken: it is a way of
    /// looking, and it changes no pixel.
    preview: Option<u32>,
    /// Current, previous, and next full previews, retained even while Quick Look is
    /// closed. Keying by generation prevents an index from the last folder being
    /// mistaken for the same index in this one.
    preview_textures: HashMap<Key, PreviewTexture>,
    preview_failed: std::collections::HashSet<Key>,
    /// The last selected frame requested at foreground priority. Selection churn can
    /// then cancel obsolete foreground work without touching useful neighbour loads.
    preview_focus: Option<Key>,
    /// EXIF for the selected file, and which file it belongs to.
    ///
    /// Cached because reading it faults the whole raw through the page cache — the
    /// same cost the Date sort pays, for one file rather than a folder. Re-read only
    /// when the selection moves.
    exif: Option<(PathBuf, Vec<Facts>)>,
    /// A contextual filesystem action failed; the Lightbox footer carries the error.
    action_note: Option<String>,
    /// Develop settings copied from one tile, kept across folder changes for the
    /// duration of this app session.
    copied_develop: Option<CopiedDevelop>,
    /// The in-app single/batch rename sheet and the completed path changes waiting
    /// for App to carry them into any open Develop tabs.
    rename_dialog: Option<crate::rename::Dialog>,
    rename_events: Vec<crate::rename::Event>,
    /// The PDF contact-sheet planner. Like Rename, it is a modal over Lightbox
    /// because its scope and order come from the grid that opened it.
    contact_sheet_dialog: Option<crate::contact_sheet::Dialog>,
    /// The display unit Contact Sheet inherits from Settings when it opens.
    contact_sheet_unit: raw_core::Unit,
    /// Editable IPTC state for the current single or multi-selection.
    iptc: Option<IptcDraft>,
    /// Reusable IPTC overlays live in Application Support, never beside a photo.
    iptc_templates: crate::iptc_templates::Store,
    iptc_template_selected: Option<usize>,
    iptc_template_name: Option<TemplateNameDialog>,
    iptc_template_editor: bool,
    /// The rebuildable filename/path index and the SEARCH pane's session state.
    search: crate::search::SearchIndex,
    search_query: String,
    search_showing: bool,
    search_total: usize,
    search_focus: bool,
    /// Search entries whose source drive is not currently mounted.
    search_offline: HashSet<u32>,
    /// IPTC context for results that matched metadata rather than only their path.
    search_metadata: HashMap<u32, String>,
    /// A re-read of the open folder running off the main thread. See [`Self::refresh`].
    relist: Option<Relist>,
    /// When the last re-read began, so switching back and forth between apps does
    /// not queue one per switch.
    relist_at: Option<std::time::Instant>,
}

/// A re-read of the open folder, in flight on its own thread.
///
/// Carries what the listing was read *under*, because the app can change the folder,
/// the listing settings or the entries themselves while it runs, and a listing of a
/// state that no longer holds must not be applied to the one that does.
struct Relist {
    dir: PathBuf,
    /// `Lightbox::generation` when the read began. Everything the app does to the
    /// entries itself — open, rename, search — retires the generation, so a match
    /// here means nothing has touched them since.
    generation: u32,
    /// Subfolders, folders, other files: the three things that decide what a
    /// listing contains.
    flags: (bool, bool, bool),
    /// Asked for by the Refresh command, which reports what it found; the automatic
    /// re-read on focus stays silent.
    manual: bool,
    rx: std::sync::mpsc::Receiver<Vec<(PathBuf, Kind)>>,
}

/// What a re-read changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub added: usize,
    pub removed: usize,
}

impl Reconciled {
    fn note(self) -> String {
        match (self.added, self.removed) {
            (0, 0) => "Folder is up to date".to_owned(),
            (a, 0) => format!("{a} new {}", files(a)),
            (0, r) => format!("{r} {} gone", files(r)),
            (a, r) => format!("{a} new, {r} gone"),
        }
    }
}

fn files(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

/// How soon an automatic re-read may follow the last one. Switching to another app
/// and straight back is a gesture, not a request to read a network folder twice.
const RELIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

impl Default for Lightbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Lightbox {
    pub fn new() -> Self {
        let (iptc_templates, template_note) = crate::iptc_templates::Store::load();
        Self {
            active: false,
            size: DEFAULT_SIZE,
            folder: None,
            folders: FolderTree::new(),
            entries: Vec::new(),
            generation: 0,
            tiles: HashMap::new(),
            queue: decode::Queue::new(thumb_workers()),
            preview_queue: decode::Queue::new(1),
            cache_dir: cache_dir(),
            developed_wanted: std::collections::VecDeque::new(),
            frame: 0,
            reset_scroll: false,
            relist: None,
            relist_at: None,
            last_avail: 0.0,
            selected: None,
            head: None,
            enter: None,
            last_cols: 0,
            follow: false,
            show_filenames: true,
            frameless: false,
            xmp_thumbnails: false,
            edited_mark: true,
            show_folders: false,
            show_other_files: false,
            grey: true,
            grounds: [
                theme::CHROME.r(),
                theme::CHROME_DEEP.r(),
                theme::CHROME_DEEP.r(),
            ],
            sort: Sort::default(),
            descending: false,
            filters: Filters::default(),
            visible: Vec::new(),
            manual: Vec::new(),
            drag: None,
            batch: Default::default(),
            tree: default_tree(),
            panel_sizes: HashMap::new(),
            tree_restructured: false,
            restore_panel_pixels: true,
            panels_hidden: false,
            favorites: Vec::new(),
            preview: None,
            preview_textures: HashMap::new(),
            preview_failed: std::collections::HashSet::new(),
            preview_focus: None,
            exif: None,
            action_note: template_note,
            copied_develop: None,
            rename_dialog: None,
            rename_events: Vec::new(),
            contact_sheet_dialog: None,
            contact_sheet_unit: raw_core::Unit::default(),
            iptc: None,
            iptc_templates,
            iptc_template_selected: None,
            iptc_template_name: None,
            iptc_template_editor: false,
            search: crate::search::SearchIndex::new(),
            search_query: String::new(),
            search_showing: false,
            search_total: 0,
            search_focus: false,
            search_offline: HashSet::new(),
            search_metadata: HashMap::new(),
        }
    }

    /// Put back the arrangement and the favorites from the last session.
    pub fn restore(&mut self, storage: &dyn eframe::Storage) {
        if let Some(tree) = eframe::get_value::<egui_tiles::Tree<Pane>>(storage, "lightbox.tree")
            && tree_is_sound(&tree)
        {
            self.tree = tree;
        }
        self.panel_sizes = eframe::get_value(storage, "lightbox.pixel_sizes").unwrap_or_default();
        self.restore_panel_pixels = true;
        self.favorites =
            eframe::get_value::<Vec<PathBuf>>(storage, "lightbox.favorites").unwrap_or_default();
    }

    /// Re-check which files have a sidecar, and drop the tiles of any that changed.
    ///
    /// **Called when the Lightbox comes forward**, because `edited` is sampled once when
    /// the folder is read and developing a frame writes a sidecar behind the grid's
    /// back. the maintainer saw the yellow edited rule take a long time to appear after coming
    /// back from Develop; what it was actually waiting for was the next full re-read of
    /// the folder, which might not happen at all.
    ///
    /// **A `stat` first, and a parse only for what moved.** The mark no longer answers
    /// "is there a sidecar" — a rating writes one too, and `Sidecar::is_developed` is
    /// what tells the two apart — so it cannot be settled by `exists()` any more. But
    /// parsing every entry on every mode switch would make coming back from Develop
    /// cost a thousand XML parses in a folder of a thousand, to learn that nine hundred
    /// and ninety-nine of them are exactly as they were.
    ///
    /// So: the stat still does the change *detection*, on mtime and existence, and the
    /// parse runs only for the entries whose sidecar actually moved. Coming back from
    /// Develop that is one file.
    ///
    /// If anything changed, the thumbnail generation is retired and the grid is
    /// cleared. Clearing only the changed tile is not enough: a camera-thumbnail job
    /// for that index may already be running, and without a new generation it can
    /// arrive after the clear and put the colour thumbnail straight back. Retiring
    /// the whole generation also means every pending marker is retired with its job;
    /// the visible tiles are requested again on the next frame.
    /// `developed` is refreshed with it, or a Developed sort would still be ordering by
    /// a time from before the edit.
    pub fn refresh_edited(&mut self) {
        let mut stale: Vec<u32> = Vec::new();
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.kind != Kind::Picture {
                continue;
            }
            let side = raw_core::sidecar::path_for(&e.path);
            let when = std::fs::metadata(&side)
                .ok()
                .and_then(|m| m.modified().ok());
            if e.developed == when {
                continue;
            }
            e.developed = when;
            e.edited = developed(&raw_core::sidecar::read(&e.path));
            stale.push(i as u32);
        }
        if !stale.is_empty() {
            self.generation = self.generation.wrapping_add(1);
            self.tiles.clear();
        }
        // A sidecar that appeared is a frame that now wants a rendered tile.
        self.rebuild_developed_wanted();
    }

    /// Put the pane arrangement back to the one the app ships with.
    ///
    /// The tree only — favourites are not layout, and a person who wants their panels
    /// back where they started is not asking to lose their folders.
    pub fn reset_tree(&mut self) {
        self.tree = default_tree();
        self.restore_panel_pixels = true;
        self.panel_sizes.clear();
        self.tree_restructured = false;
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "lightbox.tree", &self.tree);
        let mut sizes = self.panel_sizes.clone();
        for id in self.tree.tiles.tile_ids() {
            if let Some(rect) = self.tree.tiles.rect(id) {
                sizes.insert(id, rect.size());
            }
        }
        eframe::set_value(storage, "lightbox.pixel_sizes", &sizes);
        eframe::set_value(storage, "lightbox.favorites", &self.favorites);
    }

    fn restore_panel_widths(&mut self, width: f32, default: f32) {
        if !std::mem::take(&mut self.restore_panel_pixels) {
            return;
        }
        let Some(root) = self.tree.root else {
            return;
        };
        self.panel_sizes.insert(root, egui::vec2(width, 0.0));
        let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
            self.tree.tiles.get(root)
        else {
            return;
        };
        for child in &row.children {
            self.panel_sizes
                .entry(*child)
                .or_insert(egui::vec2(default, 0.0));
        }
        self.keep_panel_widths();
    }

    fn reset_panel_edge(&mut self, at: egui::Pos2, default: f32) -> bool {
        let Some(grid) = self.tree.tiles.find_pane(&Pane::Grid) else {
            return false;
        };
        let mut path = vec![grid];
        while let Some(parent) = self.tree.tiles.parent_of(*path.last().unwrap()) {
            path.push(parent);
        }
        self.capture_panel_sizes();
        for id in self.tree.tiles.tile_ids().collect::<Vec<_>>() {
            let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
                self.tree.tiles.get(id)
            else {
                continue;
            };
            if row.dir != egui_tiles::LinearDir::Horizontal {
                continue;
            }
            for pair in row.children.windows(2) {
                let (Some(left), Some(right)) =
                    (self.tree.tiles.rect(pair[0]), self.tree.tiles.rect(pair[1]))
                else {
                    continue;
                };
                let seam = egui::Rect::from_min_max(
                    egui::pos2(left.right() - 4.0, left.top().max(right.top())),
                    egui::pos2(right.left() + 4.0, left.bottom().min(right.bottom())),
                );
                if seam.contains(at) && path.contains(&pair[0]) != path.contains(&pair[1]) {
                    let panel = if path.contains(&pair[0]) {
                        pair[1]
                    } else {
                        pair[0]
                    };
                    self.panel_sizes
                        .insert(panel, egui::vec2(default, left.height()));
                    self.keep_panel_widths();
                    return true;
                }
            }
        }
        false
    }

    /// Draw the whole mode. Returns a file to open in Develop, if one was chosen.
    ///
    /// The tree is taken out of `self` for the call and put back after, because the
    /// behavior needs `&mut Lightbox` and the tree lives on it — `Tree::ui` borrows
    /// the tree mutably at the same time. A cheap move of a small struct, and the
    /// alternative is putting the tree somewhere it does not belong.
    pub fn ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) -> Option<PathBuf> {
        self.commit_blurred_iptc(ui.ctx());
        self.poll_search();
        if self.search.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(40));
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            self.reveal_pane(Pane::Search);
            self.search_focus = true;
        }
        // `tab` leaves the grid and nothing else. Drawn directly rather than by
        // emptying the tree, so the arrangement that comes back is the one that left.
        if self.panels_hidden {
            let open = self.grid_ui(ui, icons);
            self.preload_selected();
            self.preview_ui(ui);
            self.rename_dialog_ui(ui.ctx());
            self.contact_sheet_dialog_ui(ui.ctx(), icons);
            return open;
        }

        // **Put back anything a drag lost.** egui_tiles removes a pane dropped
        // somewhere that is not a drop target, and the maintainer watched panels vanish. A
        // layout you can destroy by aiming badly is not a layout you can rearrange
        // with confidence — so a missing pane is re-attached at the root rather than
        // mourned. `layout::heal` makes the same promise for Develop by rebuilding
        // the whole default; this is the gentler version, because here only the
        // dropped pane is in doubt.
        self.heal_tree();
        self.tree.simplify(&crate::layout::SIMPLIFY);
        if std::mem::take(&mut self.tree_restructured) {
            self.keep_panel_widths();
        } else {
            self.capture_panel_sizes();
        }
        let sidebar_width = ([Pane::Folders, Pane::Search, Pane::Exif]
            .iter()
            .map(|p| theme::header_size(ui.painter(), p.label()).x + 16.0)
            .sum::<f32>()
            .ceil()
            + 2.0)
            .max(240.0);
        self.restore_panel_widths(ui.available_width(), sidebar_width);
        let mut tree = std::mem::replace(&mut self.tree, egui_tiles::Tree::empty("lightbox-swap"));
        let tabbed: Vec<egui_tiles::TileId> = tree
            .tiles
            .tiles()
            .filter_map(|t| match t {
                egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs)) => {
                    Some(tabs.children.clone())
                }
                _ => None,
            })
            .flatten()
            .collect();
        // **Before the pass that would otherwise draw a dropped pane one point wide.**
        // This is the whole of "snapping a Lightbox panel to the right made it
        // disappear": a pane dropped into the root row gets no share entry, `Shares`
        // answers 1.0, and 1.0 against `left: 240` and `grid: 900` is a hairline. See
        // `layout::normalise_shares`, which Develop has been calling all along.
        crate::layout::normalise_shares(&mut tree);
        let tree_id = tree.id();
        let mut panes = Panes {
            lb: self,
            icons,
            tabbed,
            tree_id,
            open: None,
            nav: None,
            dropped: false,
        };
        tree.ui(&mut panes, ui);
        let (open, nav, dropped) = (panes.open, panes.nav, panes.dropped);
        self.tree = tree;
        if let Some(at) = ui.input(|i| {
            i.pointer
                .button_double_clicked(egui::PointerButton::Primary)
                .then(|| i.pointer.interact_pos())
                .flatten()
        }) && self.reset_panel_edge(at, sidebar_width)
        {
            ui.ctx().request_repaint();
        }
        if dropped {
            self.tree_restructured = true;
            ui.ctx().request_repaint();
        }

        if let Some(dir) = nav {
            self.open_folder(&dir);
        }
        // Selection itself begins Quick Look's load. By the time `Space` is pressed,
        // the disk read is commonly already complete rather than only just starting.
        self.preload_selected();
        // Over the panels, after them, and only when it is up.
        self.preview_ui(ui);
        self.rename_dialog_ui(ui.ctx());
        self.contact_sheet_dialog_ui(ui.ctx(), icons);
        open
    }

    /// `tab`: take the panels away, or bring them back.
    ///
    /// The same gesture Develop has and the same meaning — see the picture with
    /// nothing around it. Kept as a flag rather than by emptying the tree, so what
    /// comes back is exactly the arrangement that went away.
    pub fn toggle_panels(&mut self) {
        self.panels_hidden = !self.panels_hidden;
    }

    pub fn previewing(&self) -> bool {
        self.preview.is_some()
    }

    /// `Space`: open the full frame, or close it.
    pub fn toggle_preview(&mut self) {
        if self.preview.take().is_some() {
            // Quick Look can walk far beyond the row that was visible when it opened.
            // Returning to a grid without the frame you were just judging is a lost
            // selection even though the ring still exists off screen. Reuse the same
            // one-shot reveal that keyboard navigation already uses.
            self.follow = true;
            return;
        }
        // Nothing selected yet means the first tile, so `Space` in a folder you have
        // only just opened shows you something rather than nothing.
        if let Some(idx) = self.selected.or_else(|| self.visible.first().copied()) {
            self.selected = Some(idx);
            self.preview = Some(idx);
        }
    }

    pub fn close_preview(&mut self) {
        if self.preview.take().is_some() {
            // Covers Escape and clicking the preview as well as any future close
            // affordance. The next grid pass consumes this flag exactly once.
            self.follow = true;
        }
    }

    /// `←` / `→`: the next frame, in the grid or in the preview.
    ///
    /// **Walks the visible order, not the folder**, so stepping through a filtered
    /// four-star selection visits four frames rather than the whole shoot.
    /// `↑` / `↓`: a whole row, which is what a grid means by up and down.
    ///
    /// The stride is the column count as last laid out, so it follows the tile size
    /// and the window rather than a number written here.
    pub fn step_row(&mut self, down: bool) {
        let cols = self.columns_now().max(1);
        for _ in 0..cols {
            self.step(down);
        }
    }

    /// `⇧`+arrow: walk the moving end and take the run behind it.
    ///
    /// **The same gesture `⇧`+click already performs**, driven from the keyboard rather
    /// than the pointer, and it has to agree with it or the grid has two ideas of what
    /// extending a selection means: the anchor stays put, and `batch` becomes every tile
    /// between the anchor and where you have walked to.
    ///
    /// Walking back over your own path shrinks the run rather than growing it, because
    /// the run is always recomputed from the two ends rather than accumulated.
    pub fn step_selecting(&mut self, forward: bool) {
        // Nothing selected yet, so there is no anchor to measure a run from. Behave as a
        // plain step, which is what puts the anchor down.
        let Some(anchor) = self.selected else {
            self.step(forward);
            return;
        };
        let from = self.head.unwrap_or(anchor);
        let Some(at) = self.visible.iter().position(|i| *i == from) else {
            return;
        };
        let next = if forward {
            (at + 1).min(self.visible.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        let Some(&idx) = self.visible.get(next) else {
            return;
        };
        self.head = Some(idx);
        self.follow = true;
        if let Some(a) = self.visible.iter().position(|i| *i == anchor) {
            self.batch = self.visible[a.min(next)..=a.max(next)]
                .iter()
                .copied()
                .collect();
        }
    }

    pub fn step_row_selecting(&mut self, down: bool) {
        let cols = self.columns_now().max(1);
        for _ in 0..cols {
            self.step_selecting(down);
        }
    }

    /// Which tile the grid scrolls to when `follow` is set.
    ///
    /// The moving end, not the anchor — during a `⇧`+arrow run the anchor is standing
    /// still and scrolling to it would drag the view *backwards* as the selection grew.
    fn follow_target(&self) -> Option<u32> {
        self.head.or(self.selected)
    }

    pub fn step(&mut self, forward: bool) {
        // **A plain arrow collapses the selection**, which is what a plain click already
        // does — see `tile_ui`. Leaving the batch behind would mean an arrow press that
        // moved the ring while a five-tile selection stayed lit somewhere else, and the
        // next rating would land on all six.
        self.batch.clear();
        self.head = None;
        let Some(current) = self.selected else {
            self.selected = self.visible.first().copied();
            self.follow = true;
            if self.preview.is_some() {
                self.preview = self.selected;
            }
            return;
        };
        let Some(at) = self.visible.iter().position(|i| *i == current) else {
            return;
        };
        let next = if forward {
            (at + 1).min(self.visible.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        if let Some(&idx) = self.visible.get(next) {
            self.selected = Some(idx);
            self.follow = true;
            if self.preview.is_some() {
                self.preview = Some(idx);
            }
        }
    }

    /// The full frame, over everything.
    ///
    /// Drawn last and over the whole mode rather than as a pane, because that is what
    /// it is for: the panels are how you *find* a frame and this is looking at one.
    /// A preview inside a pane would be the grid with fewer pictures in it.
    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.preview else { return };
        let rect = ui.max_rect();

        // Opaque, and it eats the clicks: while this is up the grid underneath is not
        // something you can interact with by accident.
        ui.painter().rect_filled(rect, 0.0, theme::CHROME_DEEP);
        let r = ui.interact(rect, ui.id().with("lb-preview"), egui::Sense::click());
        if r.clicked() {
            self.close_preview();
            return;
        }

        self.request_full(idx);
        self.prefetch_neighbours(idx);

        // The preview arrives the right way up — `preview::screen` applies the
        // sidecar's orientation — so nothing here turns it.
        let fitted = |size: egui::Vec2, box_: egui::Vec2, enlarge: bool| {
            let fit = size;
            let scale = (box_.x / fit.x).min(box_.y / fit.y);
            if enlarge { scale } else { scale.min(1.0) }
        };

        let preview_key = self.full_key(idx);
        let preview_texture = self.preview_textures.get_mut(&preview_key).map(|ready| {
            ready.seen = self.frame;
            ready.texture.clone()
        });
        match preview_texture {
            Some(texture) => {
                let size = texture.size_vec2();
                let pad = 24.0;
                let box_ = egui::vec2(rect.width() - 2.0 * pad, rect.height() - 2.0 * pad);
                // **Never enlarged past its own size.** A 1600 px embedded preview
                // blown up to a 4K window is a soft picture presented as a look at the
                // frame, which is the one thing this view must not do.
                let scale = fitted(size, box_, false);
                let draw = egui::Rect::from_center_size(rect.center(), size * scale);
                egui::Image::new(&texture).paint_at(ui, draw);
            }
            // **The tile stands in while the full frame loads.** the maintainer found the
            // wait long, and it is: opening a raw for its full preview mmaps the whole
            // file with `populate()`, so a 90 MB frame that is not in the page cache
            // is disk-bound before any decoding starts. The grid already holds a 512
            // px version of exactly this picture — showing it immediately turns a
            // blank pause into a soft image that sharpens, which is the difference
            // between waiting and watching something arrive.
            _ => match self.tiles.get(&idx) {
                Some(Tile::Ready { texture, .. }) => {
                    let size = texture.size_vec2();
                    let pad = 24.0;
                    let box_ = egui::vec2(rect.width() - 2.0 * pad, rect.height() - 2.0 * pad);
                    // Enlarged here, unlike the finished frame — this one is standing
                    // in for something sharper and is honest about being a placeholder.
                    let scale = fitted(size, box_, true);
                    let draw = egui::Rect::from_center_size(rect.center(), size * scale);
                    egui::Image::new(texture).paint_at(ui, draw);
                }
                _ => {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "…",
                        egui::FontId::proportional(24.0),
                        theme::DIM,
                    );
                }
            },
        }

        if let Some(e) = self.entries.get(idx as usize) {
            ui.painter().text(
                egui::pos2(rect.center().x, rect.max.y - 14.0),
                egui::Align2::CENTER_CENTER,
                &e.name,
                egui::FontId::proportional(11.0),
                theme::NAME,
            );
        }
    }

    /// Ask for the full-size frame, unless it is already here or already asked for.
    fn request_full(&mut self, idx: u32) {
        let key = self.full_key(idx);
        if self.preview_focus != Some(key)
            && let Some(old) = self.preview_focus.replace(key)
        {
            self.preview_queue.cancel(old);
        }
        self.request_full_at(idx, decode::FOREGROUND);
    }

    /// Start the selected frame before Quick Look opens. Kept separate from
    /// `preview_ui` so a plain click or arrow key is enough to pay the I/O cost.
    fn preload_selected(&mut self) {
        if let Some(idx) = self.selected {
            self.request_full(idx);
        }
    }

    fn full_key(&self, idx: u32) -> Key {
        Key {
            generation: self.generation,
            idx,
            full: true,
        }
    }

    /// **Load the frames either side of the one being looked at.**
    ///
    /// The preview's remaining cost is a decode that cannot be avoided, so the way to
    /// make it disappear is to have already paid it. While you are looking at one
    /// frame the queue is idle and the next thing you are most likely to do is press
    /// an arrow — so the neighbours are fetched behind it, at background priority so
    /// they can never delay the frame you actually asked for.
    ///
    /// One each way rather than a window: two spare full previews is ~22 MB of
    /// texture, and the third one out is a frame you would have to press twice to
    /// reach.
    fn prefetch_neighbours(&mut self, idx: u32) {
        let Some(at) = self.visible.iter().position(|i| *i == idx) else {
            return;
        };
        let before = at.checked_sub(1).and_then(|i| self.visible.get(i)).copied();
        let after = self.visible.get(at + 1).copied();
        for n in [after, before].into_iter().flatten() {
            self.request_full_at(n, decode::BACKGROUND);
        }
    }

    fn request_full_at(&mut self, idx: u32, priority: u32) {
        if self.search_offline.contains(&idx) {
            return;
        }
        let key = self.full_key(idx);
        if self.preview_failed.contains(&key) {
            return;
        }
        if self.preview_textures.contains_key(&key) {
            return;
        }
        if self.preview_queue.is_busy(key) {
            if priority == decode::FOREGROUND {
                self.preview_queue.promote(key);
            }
            return;
        }
        let Some((path, upright_as)) = self
            .entries
            .get(idx as usize)
            .map(|e| (e.path.clone(), e.orientation))
        else {
            return;
        };
        // Foreground: it is the only thing on screen, so nothing outranks it.
        self.preview_queue.submit(key, priority, move || {
            if is_raw(&path) {
                raw_core::preview::screen(&path, raw_core::preview::SCREEN_EDGE, upright_as)
            } else {
                image_screen(&path, raw_core::preview::SCREEN_EDGE)
            }
        });
    }

    /// Re-attach any pane that is no longer in the tree.
    fn heal_tree(&mut self) {
        let present: Vec<Pane> = self
            .tree
            .tiles
            .tiles()
            .filter_map(|t| match t {
                egui_tiles::Tile::Pane(p) => Some(*p),
                _ => None,
            })
            .collect();
        let missing: Vec<Pane> = Pane::ALL
            .into_iter()
            .filter(|p| !present.contains(p))
            .collect();
        if missing.is_empty() {
            return;
        }
        // The root may itself have gone if the last pane was dragged out of it.
        let Some(root) = self.tree.root else {
            self.tree = default_tree();
            self.panel_sizes.clear();
            self.tree_restructured = false;
            return;
        };
        for pane in missing {
            let id = self.tree.tiles.insert_pane(pane);
            // Into the root container if it can hold children, beside the root
            // otherwise. Either way it is on screen and can be dragged where it
            // belongs, which is the whole point.
            if let Some(egui_tiles::Tile::Container(c)) = self.tree.tiles.get_mut(root) {
                c.add_child(id);
            } else {
                let row =
                    egui_tiles::Linear::new(egui_tiles::LinearDir::Horizontal, vec![root, id]);
                let new_root = self
                    .tree
                    .tiles
                    .insert_container(egui_tiles::Container::Linear(row));
                self.tree.root = Some(new_root);
            }
        }
        self.tree_restructured = true;
    }

    /// Remember the sizes from the frame before a structural edit. `Tree::ui` clears
    /// its rects while drawing, so these must be captured ahead of the drop.
    fn capture_panel_sizes(&mut self) {
        let live: Vec<egui_tiles::TileId> = self.tree.tiles.tile_ids().collect();
        for id in &live {
            if let Some(rect) = self.tree.tiles.rect(*id) {
                self.panel_sizes.insert(*id, rect.size());
            }
        }
        self.panel_sizes.retain(|id, _| live.contains(id));
    }

    /// Panels retain their previous width after a left/right drop; the thumbnail grid
    /// is the flexible part of the Lightbox. Without this, a detached Search pane gets
    /// the tiler's implicit equal share and consumes roughly a third of the window.
    fn keep_panel_widths(&mut self) {
        let Some(grid) = self.tree.tiles.find_pane(&Pane::Grid) else {
            return;
        };
        let mut grid_path = vec![grid];
        while let Some(parent) = self
            .tree
            .tiles
            .parent_of(*grid_path.last().expect("seeded"))
        {
            grid_path.push(parent);
        }

        for id in self.tree.tiles.tile_ids().collect::<Vec<_>>() {
            let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
                self.tree.tiles.get(id)
            else {
                continue;
            };
            if row.dir != egui_tiles::LinearDir::Horizontal {
                continue;
            }
            let children: Vec<_> = row
                .children
                .iter()
                .copied()
                .filter(|child| self.tree.is_visible(*child))
                .collect();
            let Some(flex) = children.iter().position(|child| grid_path.contains(child)) else {
                continue;
            };
            let Some(extent) = self.panel_sizes.get(&id).map(|size| size.x) else {
                continue;
            };
            let available = extent - crate::layout::GAP * (children.len() - 1) as f32;
            let remembered_panel = children
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != flex)
                .find_map(|(_, child)| self.panel_sizes.get(child).map(|size| size.x))
                .unwrap_or(240.0);
            let mut widths: Vec<f32> = children
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    self.panel_sizes.get(child).map_or_else(
                        || {
                            if index == flex {
                                available / children.len() as f32
                            } else {
                                remembered_panel
                            }
                        },
                        |size| size.x,
                    )
                })
                .collect();
            let panels: f32 = widths
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != flex)
                .map(|(_, width)| *width)
                .sum();
            let for_grid = available - panels;
            if !for_grid.is_finite() || for_grid < crate::layout::MIN_PANE {
                continue;
            }
            widths[flex] = for_grid;

            let solved: Vec<_> = children.iter().copied().zip(widths).collect();
            let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
                self.tree.tiles.get_mut(id)
            else {
                continue;
            };
            for (child, width) in solved {
                row.shares.set_share(child, width);
            }
        }
    }

    /// Make a pane visible and current — the Window menu's job.
    pub fn reveal_pane(&mut self, want: Pane) {
        self.panels_hidden = false;
        self.heal_tree();
        let id = self
            .tree
            .tiles
            .iter()
            .find(|(_, t)| matches!(t, egui_tiles::Tile::Pane(p) if *p == want))
            .map(|(id, _)| *id);
        if let Some(id) = id {
            self.tree.make_active(|tile, _| tile == id);
        }
    }

    pub fn is_favorite(&self, dir: &Path) -> bool {
        self.favorites.iter().any(|f| f == dir)
    }

    /// Add or remove the current folder from the favorites.
    pub fn toggle_favorite(&mut self, dir: &Path) {
        if let Some(i) = self.favorites.iter().position(|f| f == dir) {
            self.favorites.remove(i);
        } else {
            self.favorites.push(dir.to_path_buf());
        }
    }

    /// Re-read the current folder — after a filter that changes what is listed.
    pub fn reopen(&mut self) {
        if self.search_showing {
            self.search.query(&self.search_query);
            return;
        }
        if let Some(dir) = self.folder.clone() {
            self.open_folder(&dir);
        }
    }

    /// Apply filters that only decide what is *shown*, without touching the disk.
    pub fn refilter(&mut self) {
        self.reindex();
    }

    /// Set the single local root shown by FOLDERS. Mounted volumes remain separate
    /// live roots, so choosing Pictures here cannot hide an attached card.
    pub fn set_folder_root(&mut self, root: Option<&Path>) {
        self.folders.set_local_root(root);
    }

    /// Switch between camera thumbnails and Develop's own renders.
    ///
    /// The tiles in hand were made the other way, so they go — and their generation
    /// goes with them. A clear by itself leaves queued camera-thumbnail jobs valid;
    /// one arriving a frame later can otherwise repopulate the grid with the exact
    /// colour tile this switch just retired. The replacements come back off the disk
    /// cache in well under a millisecond each and only the visible ones are asked for.
    pub fn set_xmp_thumbnails(&mut self, on: bool) {
        if self.xmp_thumbnails != on {
            self.xmp_thumbnails = on;
            self.generation = self.generation.wrapping_add(1);
            self.tiles.clear();
            self.rebuild_developed_wanted();
        }
    }

    /// List the developed frames that have no rendered tile yet.
    ///
    /// **One `stat` per developed frame, and only when the answer could have changed.**
    /// Checking per frame would be two metadata calls per entry per repaint; checking
    /// on a folder read, on a sidecar appearing, and on the preference going on covers
    /// every way an answer moves.
    pub fn rebuild_developed_wanted(&mut self) {
        self.developed_wanted.clear();
        if !self.xmp_thumbnails {
            return;
        }
        let Some(cache) = self.cache_dir.clone() else {
            return;
        };
        // Visible order, so the frames on screen are rendered before the rest of the
        // folder rather than in whatever order the directory was read.
        for idx in self.visible.clone() {
            let Some(e) = self.entries.get(idx as usize) else {
                continue;
            };
            if e.kind != Kind::Picture || !e.edited || !is_raw(&e.path) {
                continue;
            }
            let missing = edited_cache_name(&e.path, &cache).is_none_or(|f| !f.exists());
            if missing {
                self.developed_wanted.push_back(idx);
            }
        }
    }

    /// The next developed frame needing a render, if any.
    ///
    /// **Re-checks what the list was built from**, because a folder can be re-read,
    /// re-sorted or re-filtered between building and draining, and an index means
    /// something else by then. The filter in `rebuild_developed_wanted` is the cheap
    /// pass; this one is the guarantee.
    pub fn developed_tile_wanted(&mut self) -> Option<(u32, PathBuf)> {
        while let Some(idx) = self.developed_wanted.pop_front() {
            if let Some(e) = self.entries.get(idx as usize)
                && e.kind == Kind::Picture
                && e.edited
            {
                return Some((idx, e.path.clone()));
            }
        }
        None
    }

    /// An edit changed while its tile was rendering. Retry only this visible path.
    pub fn retry_developed_tile(&mut self, path: &Path) {
        if !self.xmp_thumbnails {
            return;
        }
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.path == path && e.edited && is_raw(&e.path))
            .map(|i| i as u32)
            && self.visible.contains(&idx)
            && !self.developed_wanted.contains(&idx)
        {
            self.developed_wanted.push_back(idx);
        }
    }

    /// A tile has been written for `path`; show it.
    ///
    /// Takes the path rather than the index because the folder may have been re-read,
    /// re-sorted or re-filtered while the decode was in flight, and an index means
    /// something else by then.
    pub fn developed_tile_done(&mut self, path: &Path) {
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.path == path)
            .map(|i| i as u32)
        {
            self.queue.cancel(Key {
                generation: self.generation,
                idx,
                full: false,
            });
            self.tiles.remove(&idx);
        }
    }

    /// Switch between color and gray thumbnails without allowing an old queued
    /// decode to repopulate the grid in the previous mode.
    pub fn set_grey(&mut self, on: bool) {
        if self.grey != on {
            self.grey = on;
            self.generation = self.generation.wrapping_add(1);
            self.tiles.clear();
        }
    }

    /// Contact Sheet stores geometry canonically in inches but presents the same
    /// unit selected for print sizes in Settings.
    pub fn set_contact_sheet_unit(&mut self, unit: raw_core::Unit) {
        self.contact_sheet_unit = unit;
    }

    /// The two listing settings, applied together and only when they change.
    ///
    /// **Re-reads the folder, which is why this is a setter and not a pair of public
    /// fields.** `show_filenames` and `frameless` decide how an entry is drawn and are
    /// written every frame for nothing; these decide which entries exist at all, so a
    /// change means the directory has to be walked again. Pushed once a frame from
    /// `App::ui` beside the other mirrors, and the equality test is what keeps that
    /// from being a `read_dir` per frame.
    ///
    /// The selection and the scroll go with the reload — `open_folder` clears both.
    /// That is correct here in a way it is not for the folder-following in `App::open`:
    /// the set of tiles genuinely changed, so an index into the old one means nothing.
    pub fn set_listing(&mut self, folders: bool, others: bool) {
        if self.show_folders == folders && self.show_other_files == others {
            return;
        }
        self.show_folders = folders;
        self.show_other_files = others;
        if let Some(dir) = self.folder.clone() {
            self.open_folder(&dir);
        }
    }

    pub fn rung(&self) -> Rung {
        SIZES[self.size.min(SIZES.len() - 1)]
    }

    /// `⌘+` / `⌘−` while the grid is up.
    ///
    /// The same pair that zooms the image in Develop, which is deliberate: there is
    /// no viewport to zoom here, and "scale what you are looking at" is one gesture
    /// whichever mode you are in.
    ///
    /// **A press that changes nothing is skipped.** Because the ladder sets a target
    /// and the cards then fill the row, two adjacent rungs can land on the same
    /// column count — at 1292 points of grid, 260 and 320 both give four columns at
    /// an identical 318 point card. The prototype steps into that and the grid sits
    /// still; here the step continues until the layout actually moves.
    ///
    /// The rungs themselves are untouched, so every width the prototype can produce
    /// is still reachable — what changes is only that the key always does something.
    /// Stepping past a rung is the lesser of the two surprises: the alternative is a
    /// key that looks broken on some window widths and not others.
    pub fn resize(&mut self, larger: bool) {
        let before = self.columns_now();
        loop {
            let next = if larger {
                (self.size + 1).min(SIZES.len() - 1)
            } else {
                self.size.saturating_sub(1)
            };
            if next == self.size {
                break; // the end of the ladder
            }
            self.size = next;
            // Before the first frame there is no measured width, so one step is all
            // that can be justified.
            if self.last_avail <= 0.0 || self.columns_now() != before {
                break;
            }
        }
    }

    fn columns_now(&self) -> usize {
        if self.last_avail <= 0.0 {
            return 0;
        }
        grid_metrics(self.last_avail, self.rung(), 0.0).0
    }

    /// Set a rating on the selection, and write it.
    ///
    /// **The sidecar is the truth, so the write happens now** rather than on a timer
    /// or at quit: a rating is one small file and the whole point of it living beside
    /// the image is that closing the laptop cannot lose it. The in-memory `Entry` is
    /// updated first so the stars redraw on this frame whatever the disk does.
    ///
    /// Returns a message when the write failed, for the footer to carry.
    pub fn set_rating(&mut self, rating: i32) -> Option<String> {
        let selected = self.selection();
        if selected.len() == 1 {
            return self.set_rating_at(selected[0], rating);
        }
        // **An assignment while the selection disagrees, a toggle once it agrees.**
        // The first half is the rule this had and it is still right: if two of five
        // frames carry three stars, choosing three means all five end at three rather
        // than those two being cleared. But that argument only ever spoke to the
        // *mixed* case, and taken as the whole rule it left no way to take a rating
        // off a selection at all — pressing the same key again did nothing, and so did
        // clicking the star. The only route out was the context menu's `None`, which
        // is not where anybody reaches. So once every selected frame already carries
        // the rating being asked for, the same key clears them, exactly as it does on
        // a single frame.
        let uniform = !selected.is_empty()
            && selected.iter().all(|idx| {
                self.entries
                    .get(*idx as usize)
                    .is_some_and(|e| e.rating == rating)
            });
        let next = if uniform && rating != 0 {
            0
        } else {
            rating.clamp(0, 5)
        };
        let mut errors = Vec::new();
        for idx in selected {
            let Some(e) = self.entries.get_mut(idx as usize) else {
                continue;
            };
            e.rating = next;
            if let Some(error) = Self::commit(e) {
                errors.push(error);
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    /// Rate one named tile, **without selecting it**.
    ///
    /// The star row is drawn on the tile it rates, so a click there already says which
    /// frame it means and does not need the selection moved to say it again. It used to:
    /// `set_rating` reads `self.selected`, so the handler set that first and the ruby
    /// ring appeared on every tile you rated. the maintainer found it distracting, and he is
    /// right that it is two gestures wearing one — rating is a judgement about a
    /// picture, selecting is choosing what the *next* command acts on.
    ///
    /// The keyboard path is unchanged and still goes through `set_rating`, because there
    /// the selection is the only thing that says which frame you mean.
    pub fn set_rating_at(&mut self, idx: u32, rating: i32) -> Option<String> {
        let e = self.entries.get_mut(idx as usize)?;
        // Pressing the star you are already on clears it, which is the prototype's
        // behaviour and the one every browser has: `⌘3` twice is how you take three
        // stars off without counting down.
        e.rating = if e.rating == rating {
            0
        } else {
            rating.clamp(0, 5)
        };
        Self::commit(e)
    }

    /// Set a colour label on the selection, and write it. `None` clears it.
    pub fn set_label(&mut self, label: Option<&str>) -> Option<String> {
        let selected = self.selection();
        if selected.len() == 1 {
            return self.set_label_at(selected[0], label);
        }
        // The same rule as `set_rating`, and for the same reason: one gesture means
        // one thing whether it lands on one frame or forty. `⇧2` twice takes the
        // label off a selection that all carries it.
        let next = label.map(str::to_owned);
        let uniform = !selected.is_empty()
            && selected.iter().all(|idx| {
                self.entries
                    .get(*idx as usize)
                    .is_some_and(|e| e.label == next)
            });
        let assigned = if uniform && next.is_some() {
            None
        } else {
            next
        };
        let mut errors = Vec::new();
        for idx in selected {
            let Some(e) = self.entries.get_mut(idx as usize) else {
                continue;
            };
            e.label = assigned.clone();
            if let Some(error) = Self::commit(e) {
                errors.push(error);
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    /// Label one named tile, without selecting it. See [`Self::set_rating_at`].
    pub fn set_label_at(&mut self, idx: u32, label: Option<&str>) -> Option<String> {
        let e = self.entries.get_mut(idx as usize)?;
        let next = label.map(str::to_owned);
        e.label = if e.label == next { None } else { next };
        Self::commit(e)
    }

    /// Turn the selection a quarter turn, for display, and write it.
    ///
    /// Acts on **every selected tile**, which is what makes multi-select worth having
    /// — a batch of portraits shot on their side is the case this exists for.
    pub fn rotate(&mut self, clockwise: bool) -> Option<String> {
        let mut errors = Vec::new();
        let mut touched = false;
        for idx in self.selection() {
            let Some(e) = self.entries.get(idx as usize) else {
                continue;
            };
            if e.kind != Kind::Picture {
                continue;
            }
            let path = e.path.clone();
            match write_turn(&path, clockwise) {
                Ok(next) => {
                    if let Some(e) = self.entries.get_mut(idx as usize) {
                        e.orientation = Some(next);
                    }
                    self.tiles.remove(&idx);
                    // The full-preview key carries no orientation, and selection has
                    // usually preloaded this frame at the old one.
                    let full = self.full_key(idx);
                    self.preview_textures.remove(&full);
                    self.preview_queue.cancel(full);
                    touched = true;
                }
                Err(e) => errors.push(e),
            }
        }
        if touched {
            // A turned frame's cached tile is keyed on its orientation, so the grid
            // has to ask again; a developed one has to be re-rendered.
            self.rebuild_developed_wanted();
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    /// Every selected tile: the multi-selection if there is one, else the anchor.
    ///
    /// One accessor rather than two paths, so an action can never act on the anchor
    /// while the screen shows five tiles ringed.
    pub fn selection(&self) -> Vec<u32> {
        if !self.batch.is_empty() {
            let mut v: Vec<u32> = self.batch.iter().copied().collect();
            v.sort_unstable();
            return v;
        }
        self.selected.into_iter().collect()
    }

    fn selection_in_visible_order(&self) -> Vec<u32> {
        let chosen: HashSet<u32> = self.selection().into_iter().collect();
        self.visible
            .iter()
            .copied()
            .filter(|index| chosen.contains(index))
            .collect()
    }

    /// Open the single or batch rename sheet for the visible selection.
    ///
    /// `selection()` is deliberately *not* used for ordering here: it sorts storage
    /// indices, while a numbered rename has to follow the contact sheet the person is
    /// looking at — including Manual order, search relevance, and reverse sorts.
    pub fn begin_rename(&mut self) {
        if self.rename_dialog.is_some() {
            return;
        }
        let ordered = self.selection_in_visible_order();
        if ordered.is_empty() {
            self.action_note = Some("Select a file to rename".to_owned());
            return;
        }

        let mut sources = Vec::with_capacity(ordered.len());
        for index in ordered {
            let Some(entry) = self.entries.get(index as usize) else {
                continue;
            };
            if entry.kind == Kind::Folder {
                self.action_note = Some("Folder renaming is not available here".to_owned());
                return;
            }
            if self.search_offline.contains(&index) || !entry.path.exists() {
                self.action_note = Some(format!("{} is not currently available", entry.name));
                return;
            }
            sources.push(crate::rename::Source {
                path: entry.path.clone(),
                captured: entry.captured,
            });
        }
        if !sources.is_empty() {
            self.preview = None;
            self.rename_dialog = Some(crate::rename::Dialog::new(sources));
        }
    }

    /// Open the contact-sheet planner with three deliberately different scopes.
    /// Selected preserves the visible selection order, Visible preserves the
    /// filtered grid, and Entire Folder ignores filters rather than quietly turning
    /// a filtered contact sheet into a claim about the complete folder.
    pub fn begin_contact_sheet(&mut self) {
        if self.contact_sheet_dialog.is_some() {
            return;
        }
        let source = |entry: &Entry| crate::contact_sheet::Source {
            path: entry.path.clone(),
            name: entry.name.clone(),
            rating: entry.rating,
            label: entry.label.clone(),
            orientation: entry.orientation,
        };
        let selected = self
            .selection_in_visible_order()
            .into_iter()
            .filter_map(|index| self.entries.get(index as usize))
            .filter(|entry| entry.kind == Kind::Picture && entry.path.exists())
            .map(source)
            .collect();
        let visible = self
            .visible
            .iter()
            .filter_map(|index| self.entries.get(*index as usize))
            .filter(|entry| entry.kind == Kind::Picture && entry.path.exists())
            .map(source)
            .collect();
        let folder = self
            .entries
            .iter()
            .filter(|entry| entry.kind == Kind::Picture && entry.path.exists())
            .map(source)
            .collect();
        let sources = crate::contact_sheet::Sources {
            selected,
            visible,
            folder,
            cache: self.cache_dir.clone(),
        };
        if sources.visible.is_empty() && sources.folder.is_empty() {
            self.action_note =
                Some("There are no photographs to place in a contact sheet".to_owned());
            return;
        }
        self.preview = None;
        self.contact_sheet_dialog = Some(crate::contact_sheet::Dialog::new(
            sources,
            self.contact_sheet_unit,
        ));
    }

    /// Path changes completed by the rename sheet, consumed once by App so open
    /// Develop tabs can keep following the same photographs.
    pub fn take_rename_events(&mut self) -> Vec<crate::rename::Event> {
        std::mem::take(&mut self.rename_events)
    }

    fn rename_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.rename_dialog.take() else {
            return;
        };
        match dialog.show(ctx) {
            None => self.rename_dialog = Some(dialog),
            Some(crate::rename::DialogResponse::Cancel) => {}
            Some(crate::rename::DialogResponse::Rename(rows)) => {
                let caches = self.rename_caches(&rows);
                match crate::rename::execute(&rows) {
                    Ok(events) => {
                        let count = events.len();
                        self.migrate_rename_caches(&events, caches);
                        self.apply_rename_events(&events);
                        self.rename_events.extend(events);
                        self.action_note = Some(if count == 1 {
                            "Renamed 1 file".to_owned()
                        } else {
                            format!("Renamed {count} files")
                        });
                    }
                    Err(error) => {
                        dialog.set_error(error);
                        self.rename_dialog = Some(dialog);
                    }
                }
            }
        }
    }

    fn contact_sheet_dialog_ui(&mut self, ctx: &egui::Context, icons: &crate::icons::Icons) {
        let Some(mut dialog) = self.contact_sheet_dialog.take() else {
            return;
        };
        match dialog.show(ctx, icons) {
            None => self.contact_sheet_dialog = Some(dialog),
            Some(crate::contact_sheet::DialogResponse::Cancel) => {}
            Some(crate::contact_sheet::DialogResponse::Exported(path)) => {
                self.action_note = Some(format!("Exported contact sheet to {}", path.display()));
            }
        }
    }

    fn rename_caches(&self, rows: &[crate::rename::Row]) -> Vec<RenameCaches> {
        let Some(cache) = &self.cache_dir else {
            return Vec::new();
        };
        rows.iter()
            .filter_map(|row| {
                let metadata = std::fs::metadata(&row.source).ok()?;
                let camera = cache.join(cache_name(&row.source, &metadata));
                Some(RenameCaches {
                    source: row.source.clone(),
                    captured: Some(camera.with_extension("when")),
                    camera: Some(camera),
                    edited: edited_cache_name(&row.source, cache),
                })
            })
            .collect()
    }

    fn migrate_rename_caches(&self, events: &[crate::rename::Event], caches: Vec<RenameCaches>) {
        let Some(cache) = &self.cache_dir else { return };
        for old in caches {
            let Some(event) = events.iter().find(|event| event.old == old.source) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&event.new) else {
                continue;
            };
            let camera = cache.join(cache_name(&event.new, &metadata));
            move_cache(old.camera, Some(camera.clone()));
            move_cache(old.captured, Some(camera.with_extension("when")));
            move_cache(old.edited, edited_cache_name(&event.new, cache));
        }
    }

    fn apply_rename_events(&mut self, events: &[crate::rename::Event]) {
        let changes: HashMap<PathBuf, PathBuf> = events
            .iter()
            .map(|event| (event.old.clone(), event.new.clone()))
            .collect();
        let current = self.folder.as_deref();

        for entry in &mut self.entries {
            let Some(new) = changes.get(&entry.path) else {
                continue;
            };
            entry.path = new.clone();
            entry.name = name_of(new);
            entry.ext = ext_of(new);
        }

        if let Some(dir) = current {
            for name in &mut self.manual {
                if let Some(new) = changes.get(&dir.join(&*name)) {
                    *name = name_of(new);
                }
            }
        }
        if let Some(dir) = &self.folder {
            write_order(dir, &self.manual);
        }

        self.generation = self.generation.wrapping_add(1);
        self.tiles.clear();
        self.preview = None;
        self.preview_textures.clear();
        self.preview_focus = None;
        self.exif = None;
        self.iptc = None;
        self.reindex();
        self.search.reindex();
    }

    /// Copy the selected image's transferable Develop state, excluding metadata and
    /// image-local Dodge/Burn work.
    pub fn copy_settings(&mut self) -> Result<String, String> {
        let Some(idx) = self.selected else {
            return Err("Select an edited image to copy its settings".to_owned());
        };
        self.copy_settings_at(idx)
    }

    /// Copy from one named tile. The context menu uses this so the tile under the
    /// pointer is unambiguous even when a batch is selected.
    fn copy_settings_at(&mut self, idx: u32) -> Result<String, String> {
        let Some(entry) = self.entries.get(idx as usize) else {
            return Err("That image is no longer in the Lightbox".to_owned());
        };
        if entry.kind != Kind::Picture {
            return Err("Develop settings can only be copied from an image".to_owned());
        }
        let source = entry.name.clone();
        let mut params = match raw_core::sidecar::read(&entry.path) {
            raw_core::sidecar::Loaded::Ok(sidecar) if sidecar.is_developed() => sidecar.params,
            raw_core::sidecar::Loaded::Ok(_) | raw_core::sidecar::Loaded::Absent => {
                return Err(format!("{source} has no Develop settings to copy"));
            }
            raw_core::sidecar::Loaded::Corrupt(why) => {
                return Err(format!("could not copy settings from {source} — {why}"));
            }
        };
        params.dodgeburn = Default::default();
        self.copied_develop = Some(CopiedDevelop {
            params,
            source: source.clone(),
        });
        Ok(format!("Copied Develop settings from {source}"))
    }

    pub fn has_copied_settings(&self) -> bool {
        self.copied_develop.is_some()
    }

    /// Paste the copied Develop state onto every selected picture while preserving
    /// each destination's own metadata overlay and Dodge/Burn layers.
    pub fn paste_settings(&mut self) -> Result<String, String> {
        let Some(copied) = self.copied_develop.clone() else {
            return Err("Copy settings from an edited image first".to_owned());
        };
        let selected = self.selection();
        if selected.is_empty() {
            return Err("Select one or more destination images".to_owned());
        }

        let mut applied = 0usize;
        let mut errors = Vec::new();
        for idx in selected {
            let Some(entry) = self.entries.get(idx as usize) else {
                continue;
            };
            if entry.kind != Kind::Picture {
                continue;
            }
            let path = entry.path.clone();
            let name = entry.name.clone();
            let (metadata, dodgeburn) = match raw_core::sidecar::read(&path) {
                raw_core::sidecar::Loaded::Ok(sidecar) => {
                    (sidecar.metadata, sidecar.params.dodgeburn)
                }
                raw_core::sidecar::Loaded::Absent => (Default::default(), Default::default()),
                raw_core::sidecar::Loaded::Corrupt(why) => {
                    errors.push(format!(
                        "{name}: refusing to overwrite a corrupt sidecar — {why}"
                    ));
                    continue;
                }
            };
            let mut params = copied.params.clone();
            params.dodgeburn = dodgeburn;
            if let Err(error) = raw_core::sidecar::write(&path, &params, &metadata) {
                errors.push(format!("{name}: {error}"));
                continue;
            }
            if let Some(entry) = self.entries.get_mut(idx as usize) {
                entry.edited = params != raw_core::Params::default();
                entry.developed = std::fs::metadata(raw_core::sidecar::path_for(&path))
                    .ok()
                    .and_then(|metadata| metadata.modified().ok());
            }
            applied += 1;
        }

        if applied > 0 {
            self.generation = self.generation.wrapping_add(1);
            self.tiles.clear();
        }
        if !errors.is_empty() {
            return Err(format!(
                "Pasted onto {applied} image{}; {}",
                if applied == 1 { "" } else { "s" },
                errors.join("; ")
            ));
        }
        if applied == 0 {
            return Err("The selection contains no images".to_owned());
        }
        Ok(format!(
            "Pasted settings from {} onto {applied} image{}",
            copied.source,
            if applied == 1 { "" } else { "s" }
        ))
    }

    fn clear_selection(&mut self) {
        self.selected = None;
        self.batch.clear();
        self.head = None;
    }

    /// Push one entry's metadata to disk.
    ///
    /// **It does not touch `edited`, and that is the maintainer's correction.** It used to set
    /// it, on the reasoning that "it has a sidecar now, so it is worked on" — which
    /// conflated the two unrelated reasons a sidecar exists. Rating a frame is
    /// cataloguing it; the amber rule answers "which of these have I *developed*", and
    /// a star lighting it made the mark useless on exactly the pass where it is most
    /// wanted, the one where you go through a folder starring things. See
    /// `Sidecar::is_developed`.
    fn commit(e: &mut Entry) -> Option<String> {
        match write_metadata(&e.path, e.rating, e.label.as_deref()) {
            Ok(()) => None,
            Err(err) => Some(format!("{}: {err}", e.name)),
        }
    }

    /// Point the grid at a folder.
    ///
    /// Everything from the previous folder is dropped and its generation retired —
    /// see [`Key`]. The sort and filters carry over, because they are how *you* look
    /// at a folder rather than a property of the one you left.
    /// Whether the grid has finished arriving: at least one tile drawn and none
    /// still queued. `visual` waits on this so a Lightbox frame is not captured
    /// half-filled.
    #[cfg(test)]
    pub fn tiles_settled(&self) -> bool {
        !self.tiles.is_empty() && !self.tiles.values().any(|t| matches!(t, Tile::Pending))
    }

    pub fn open_folder(&mut self, dir: &Path) {
        let mut entries: Vec<Entry> = list_entries(
            dir,
            self.filters.subfolders,
            self.show_folders,
            self.show_other_files,
        )
        .into_iter()
        .map(|(p, kind)| entry_for(p, kind))
        .collect();
        // A stable base order under every sort, so two files that tie anywhere else
        // still come out the same way twice.
        entries.sort_by_cached_key(|e| e.name.to_lowercase());
        self.entries = entries;
        self.generation = self.generation.wrapping_add(1);
        self.tiles.clear();
        // The selection is an index into the folder you just left. Keeping it would
        // put the ring on whatever happens to be in that position here.
        self.selected = None;
        self.search_showing = false;
        self.search_offline.clear();
        self.search_metadata.clear();
        self.folder = Some(dir.to_path_buf());
        self.reset_scroll = true;
        self.folders.reveal(dir);
        self.folders.expanded.insert(dir.to_path_buf());
        self.manual = read_order(dir);
        self.reindex();
        if self.sort == Sort::Captured {
            self.sweep_dates();
        }
    }

    /// Re-read the open folder in the background, and fold what changed into the grid
    /// when it arrives. See [`Self::reconcile`] for what "fold in" preserves.
    ///
    /// **Automatic** (`manual == false`) on the window coming back to the front and on
    /// switching into Lightbox — the moments files are most likely to have arrived from
    /// somewhere else — and at most once per [`RELIST_INTERVAL`]. **Manual** is the
    /// Refresh command: it is never throttled, reports what it found, and also forgets
    /// the folder tree's cached subfolders so they are read again as they are drawn.
    ///
    /// Off the main thread because a folder on a network volume can take a noticeable
    /// fraction of a second to list, and the first of these runs on every window focus.
    /// Nothing happens while search results are showing: those entries are not the
    /// folder's.
    pub fn refresh(&mut self, manual: bool) {
        let Some(dir) = self.folder.clone() else {
            return;
        };
        if self.search_showing || self.relist.is_some() {
            return;
        }
        if !manual
            && self
                .relist_at
                .is_some_and(|at| at.elapsed() < RELIST_INTERVAL)
        {
            return;
        }
        if manual {
            self.folders.children.clear();
        }
        self.relist_at = Some(std::time::Instant::now());
        let flags = (
            self.filters.subfolders,
            self.show_folders,
            self.show_other_files,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let read = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(list_entries(&read, flags.0, flags.1, flags.2));
        });
        self.relist = Some(Relist {
            dir,
            generation: self.generation,
            flags,
            manual,
            rx,
        });
    }

    /// Apply a finished re-read, if it still describes the grid on screen.
    fn poll_relist(&mut self, ctx: &egui::Context) {
        use std::sync::mpsc::TryRecvError;
        let Some(relist) = &self.relist else {
            return;
        };
        let listing = match relist.rx.try_recv() {
            Ok(listing) => listing,
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(TryRecvError::Disconnected) => {
                self.relist = None;
                return;
            }
        };
        let Some(relist) = self.relist.take() else {
            return;
        };
        // Stale is dropped rather than applied: the next focus or Refresh reads again,
        // while a listing from before a rename would put the old name back.
        let current = self.folder.as_deref() == Some(relist.dir.as_path())
            && self.generation == relist.generation
            && relist.flags
                == (
                    self.filters.subfolders,
                    self.show_folders,
                    self.show_other_files,
                )
            && !self.search_showing
            && self.drag.is_none();
        if !current {
            return;
        }
        let change = self.reconcile(listing);
        if relist.manual {
            self.action_note = Some(change.note());
        }
    }

    /// Bring `entries` into line with a fresh listing of the same folder **without
    /// disturbing anything that is still there**.
    ///
    /// Every thumbnail, the selection, the batch, Quick Look and the scroll are keyed
    /// by an entry's index, so `open_folder`'s answer — rebuild and clear — would blank
    /// the grid and lose your place on every window focus. Instead:
    ///
    /// - nothing added or removed changes nothing, which is almost every call;
    /// - surviving entries keep their relative order and new ones go on the end, so
    ///   the display order still comes from the sort alone (`entries` is never the
    ///   display order; see [`Self::reindex`]);
    /// - every index-keyed field is carried across one remap, and whatever pointed at
    ///   a removed file is dropped;
    /// - the generation is retired, so a thumbnail job still running under an old
    ///   index cannot land on the entry that now holds it. Only tiles still *pending*
    ///   are lost to that, and the grid asks for those again as it draws.
    ///
    /// Sidecar changes are not this function's business — `refresh_edited` covers
    /// them and runs at the same moments.
    fn reconcile(&mut self, listing: Vec<(PathBuf, Kind)>) -> Reconciled {
        let listed: HashSet<&Path> = listing.iter().map(|(p, _)| p.as_path()).collect();
        let mut added: Vec<(PathBuf, Kind)> = {
            let known: HashSet<&Path> = self.entries.iter().map(|e| e.path.as_path()).collect();
            listing
                .iter()
                .filter(|(p, _)| !known.contains(p.as_path()))
                .cloned()
                .collect()
        };
        let removed = self
            .entries
            .iter()
            .filter(|e| !listed.contains(e.path.as_path()))
            .count();
        if added.is_empty() && removed == 0 {
            return Reconciled::default();
        }

        // Old index -> new index, `None` for a file that is gone.
        let mut remap: Vec<Option<u32>> = Vec::with_capacity(self.entries.len());
        let mut kept: Vec<Entry> = Vec::with_capacity(self.entries.len() + added.len());
        for entry in std::mem::take(&mut self.entries) {
            if listed.contains(entry.path.as_path()) {
                remap.push(Some(kept.len() as u32));
                kept.push(entry);
            } else {
                remap.push(None);
            }
        }
        // The same base order `open_folder` gives a whole folder, among the newcomers.
        added.sort_by_cached_key(|(p, _)| name_of(p).to_lowercase());
        let result = Reconciled {
            added: added.len(),
            removed,
        };
        kept.extend(added.into_iter().map(|(p, kind)| entry_for(p, kind)));
        self.entries = kept;
        let map = |i: u32| remap.get(i as usize).copied().flatten();

        // Queued jobs that never started would be wasted work under the old
        // generation; stop them before retiring it.
        for (idx, tile) in &self.tiles {
            if matches!(tile, Tile::Pending) {
                self.queue.cancel(Key {
                    generation: self.generation,
                    idx: *idx,
                    full: false,
                });
            }
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let rekey = |key: Key| {
            Some(Key {
                generation,
                idx: map(key.idx)?,
                ..key
            })
        };

        self.tiles = std::mem::take(&mut self.tiles)
            .into_iter()
            .filter(|(_, tile)| !matches!(tile, Tile::Pending))
            .filter_map(|(idx, tile)| Some((map(idx)?, tile)))
            .collect();
        self.preview_textures = std::mem::take(&mut self.preview_textures)
            .into_iter()
            .filter_map(|(key, texture)| Some((rekey(key)?, texture)))
            .collect();
        self.preview_failed.clear();
        self.preview_focus = self.preview_focus.and_then(rekey);
        self.selected = self.selected.and_then(map);
        self.head = self.head.and_then(map);
        self.preview = self.preview.and_then(map);
        self.batch = self.batch.iter().filter_map(|i| map(*i)).collect();
        self.drag = None;

        // Also rebuilds `developed_wanted`, newcomers included, from the new indices.
        self.reindex();
        if self.sort == Sort::Captured {
            self.sweep_dates();
        }
        result
    }

    /// Rebuild the display order from the sort and the filters.
    ///
    /// **`entries` is never reordered**, only `visible` is. Tiles are keyed by an
    /// entry's index, so a thumbnail survives a change of sort without being asked
    /// for again — which is what makes flipping between orders instant rather than a
    /// second pass over the folder.
    fn reindex(&mut self) {
        if self.search_showing {
            // Search records arrive in relevance order. Folder filters depend on
            // metadata the filename index intentionally has not parsed, and applying
            // them here would make an active star filter erase every result.
            self.visible = (0..self.entries.len() as u32).collect();
            self.reset_scroll = true;
            return;
        }
        let mut v: Vec<u32> = (0..self.entries.len() as u32)
            .filter(|i| self.filters.admits(&self.entries[*i as usize]))
            .collect();

        match self.sort {
            // Folder reads begin in filename order, but a rename changes the name in
            // place so the stable storage indices no longer imply that order.
            Sort::Filename => {
                v.sort_by_cached_key(|i| self.entries[*i as usize].name.to_lowercase())
            }
            Sort::Manual => {
                // Preserve the first saved occurrence, including malformed orders
                // with duplicates. New files follow saved files in storage order.
                let mut ranks = HashMap::with_capacity(self.manual.len());
                for (rank, name) in self.manual.iter().enumerate() {
                    ranks.entry(name.as_str()).or_insert(rank);
                }
                v.sort_by_key(|i| {
                    (
                        ranks
                            .get(self.entries[*i as usize].name.as_str())
                            .copied()
                            .unwrap_or(usize::MAX),
                        *i,
                    )
                });
            }
            Sort::Captured => v.sort_by_key(|i| self.entries[*i as usize].captured),
            Sort::Developed => v.sort_by_key(|i| self.entries[*i as usize].developed),
            Sort::Rating => v.sort_by_key(|i| self.entries[*i as usize].rating),
            Sort::Label => v.sort_by_key(|i| {
                let e = &self.entries[*i as usize];
                let rank = e
                    .label
                    .as_deref()
                    .and_then(|l| theme::LABELS.iter().position(|(n, _)| *n == l));
                (rank.unwrap_or(usize::MAX), e.label.clone())
            }),
            Sort::Kind => v.sort_by_key(|i| self.entries[*i as usize].ext.clone()),
        }
        if self.descending {
            v.reverse();
        }
        // **"No answer" goes last, whichever way the sort runs.**
        //
        // The rule was written down here — "unlabelled last, which is where no answer
        // belongs under every one of these" — and then only Label implemented it, and
        // only in one direction: it carried the flag inside its key, so `reverse` sent
        // its unlabelled frames to the *top*. Rating and Developed did not implement it
        // at all, which is why choosing either put every frame the maintainer cared about at the
        // bottom behind a wall of noughts.
        //
        // Applied after the reversal for the same reason the folders block below is,
        // and by the same means: `sort_by_key` is stable, so this lifts one group out
        // and leaves both in the order the sort just put them.
        let unanswered = |e: &Entry| match self.sort {
            Sort::Rating => e.rating <= 0,
            Sort::Developed => e.developed.is_none(),
            Sort::Label => e.label.is_none(),
            Sort::Captured => e.captured.is_none(),
            Sort::Filename | Sort::Kind | Sort::Manual => false,
        };
        v.sort_by_key(|i| unanswered(&self.entries[*i as usize]));
        // **Folders first, after the sort and after the reversal.** Every file browser
        // does this and the reason is navigation rather than taste: the way out of a
        // folder should be in the same place whichever order you are looking at its
        // contents in, and flipping to descending should not send it to the bottom.
        //
        // `sort_by_key` is stable, so this lifts the folders as a block and leaves both
        // groups in the order the sort just put them.
        if self.show_folders {
            v.sort_by_key(|i| self.entries[*i as usize].kind != Kind::Folder);
        }
        self.visible = v;
        self.reset_scroll = true;
        // The work list is in visible order, so re-sorting or re-filtering re-orders it.
        self.rebuild_developed_wanted();
    }

    /// Change the sort, or flip its direction.
    ///
    /// **Choosing a sort also chooses the direction it is usually wanted in**, the way
    /// a file browser gives Name A–Z and Date newest-first without being asked. Rating
    /// and Developed are the two where ascending is the wrong end of the answer: you
    /// sort by rating to see the good frames and by developed to see what you last
    /// worked on, and both of those live at the *high* end. Flipping afterwards still
    /// works and is remembered across a restart; this only decides where a fresh
    /// choice lands.
    pub fn set_sort(&mut self, sort: Sort) {
        self.sort = sort;
        self.descending = sort.natural_descending();
        if sort == Sort::Captured {
            self.sweep_dates();
        }
        self.reindex();
    }

    pub fn set_descending(&mut self, descending: bool) {
        self.descending = descending;
        self.reindex();
    }

    /// Read capture dates for the whole folder, once.
    ///
    /// **This is the expensive one and it is only paid on request.** `RawSource::new`
    /// mmaps with `populate()`, so asking a raw for its EXIF faults the entire file
    /// through the page cache — on a folder of 500 that is every byte of every raw.
    /// So: nothing happens until you choose the Date sort, the answer is cached on
    /// disk beside the tiles, and a second visit costs a small file read per image.
    ///
    /// Done on the calling thread with rayon rather than through the queue, because
    /// a half-sorted grid that settles as answers arrive is worse than a grid that
    /// waits: the whole point of a sort is that the order means something.
    fn sweep_dates(&mut self) {
        use rayon::prelude::*;
        let want: Vec<usize> = (0..self.entries.len())
            .filter(|i| self.entries[*i].captured.is_none())
            .collect();
        if want.is_empty() {
            return;
        }
        let cache = self.cache_dir.clone();
        let found: Vec<(usize, Option<std::time::SystemTime>)> = want
            .par_iter()
            .map(|i| (*i, captured_at(&self.entries[*i].path, cache.as_deref())))
            .collect();
        for (i, when) in found {
            self.entries[i].captured = when;
        }
    }

    /// Move the dragged file to sit before `before` in the manual order.
    ///
    /// **Dragging switches the sort to Manual rather than refusing**, which is
    /// Bridge's behaviour and the maintainer's call: a drag is an unambiguous statement about
    /// where you want something, and a browser that ignores it because you happen to
    /// be sorted by rating is arguing with you.
    ///
    /// The order is a list of *names* and it lives in this app's cache, never in the
    /// photo directory — see [`write_order`].
    pub fn reorder(&mut self, dragged: u32, before: usize) {
        // **A drag moves everything that is selected**, not just the tile under the
        // pointer. the maintainer found three highlighted and one moving, which is the gesture
        // disagreeing with the screen. The dragged tile joins the set if it is not in
        // it, because dragging something unselected is a statement about that thing.
        let mut moving = self.selection();
        if !moving.contains(&dragged) {
            moving = vec![dragged];
        }
        // In visible order, so a block dropped somewhere keeps its own arrangement.
        moving.sort_by_key(|i| {
            self.visible
                .iter()
                .position(|v| v == i)
                .unwrap_or(usize::MAX)
        });
        let names: Vec<String> = moving
            .iter()
            .filter_map(|i| self.entries.get(*i as usize))
            .map(|e| e.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }

        // **Seed from what is on screen whenever the drag is arriving from another
        // sort**, not only when there is no order yet. A folder arranged by hand last
        // week still has that order on disk, and switching to Manual re-applied it —
        // so a drag under Date or Rating rearranged the whole folder around the one
        // tile that moved, and left you somewhere else in it. Taking the visible order
        // as the starting point means the only thing that moves is what you dragged.
        if self.manual.is_empty() || self.sort != Sort::Manual {
            self.manual = self
                .visible
                .iter()
                .filter_map(|i| self.entries.get(*i as usize))
                .map(|e| e.name.clone())
                .collect();
        }
        // Any file the order has not heard of goes on the end before we move
        // anything, so positions mean the same thing throughout.
        for e in &self.entries {
            if !self.manual.contains(&e.name) {
                self.manual.push(e.name.clone());
            }
        }

        // The landing place is named before anything moves, because taking the block
        // out shifts every index after it.
        let target_name = self
            .visible
            .get(before)
            .and_then(|i| self.entries.get(*i as usize))
            .map(|e| e.name.clone())
            .filter(|n| !names.contains(n));
        self.manual.retain(|n| !names.contains(n));
        let at = target_name
            .and_then(|t| self.manual.iter().position(|n| *n == t))
            .unwrap_or(self.manual.len());
        for (k, n) in names.into_iter().enumerate() {
            self.manual.insert((at + k).min(self.manual.len()), n);
        }

        self.sort = Sort::Manual;
        self.descending = false;
        if let Some(dir) = &self.folder {
            write_order(dir, &self.manual);
        }
        self.reindex();
        // A drag is a deliberate act on one tile; keeping the scroll where it was is
        // the difference between reordering a shoot and losing your place in it.
        self.reset_scroll = false;
    }

    pub fn report_worker_failure(&mut self, message: String) {
        self.action_note = Some(message);
    }

    /// Accept only the current folder generation and keep failures out of busy state.
    pub fn collect(&mut self, ctx: &egui::Context) {
        self.poll_relist(ctx);
        self.preview_failed
            .retain(|key| key.generation == self.generation);
        if self.queue.in_flight() > 0 || self.preview_queue.in_flight() > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // Full previews are collected first. They have their own worker precisely so
        // a screenful of thumbnail reads cannot sit in front of the frame the person
        // has selected, and retaining three is what makes neighbour prefetch real.
        while let Some((key, got)) = self.preview_queue.poll() {
            if key.generation != self.generation {
                continue;
            }
            let got = match got {
                Ok(got) => got,
                Err(why) => {
                    self.preview_failed.insert(key);
                    self.report_worker_failure(format!("preview: {why}"));
                    continue;
                }
            };
            if let Some(img) = got {
                let colour = if self.grey {
                    grey_image(&img)
                } else {
                    egui::ColorImage::from_rgb([img.w, img.h], &img.data)
                };
                let name = format!("lbfull{}:{}", key.generation, key.idx);
                let texture = ctx.load_texture(name, colour, egui::TextureOptions::LINEAR);
                self.preview_textures.insert(
                    key,
                    PreviewTexture {
                        texture,
                        seen: self.frame,
                    },
                );
                self.evict_preview_textures();
            }
        }

        while let Some((key, got)) = self.queue.poll() {
            if key.generation != self.generation {
                continue;
            }
            debug_assert!(!key.full, "full previews belong on the Quick Look queue");
            let got = match got {
                Ok(got) => got,
                Err(why) => {
                    self.report_worker_failure(format!("thumbnail: {why}"));
                    None
                }
            };
            match got {
                Some(img) => {
                    let colour = if self.grey {
                        grey_image(&img)
                    } else {
                        egui::ColorImage::from_rgb([img.w, img.h], &img.data)
                    };
                    let name = format!("lb{}:{}", key.generation, key.idx);
                    let texture = ctx.load_texture(name, colour, egui::TextureOptions::LINEAR);
                    self.tiles.insert(
                        key.idx,
                        Tile::Ready {
                            texture,
                            seen: self.frame,
                        },
                    );
                }
                None => {
                    self.tiles.insert(key.idx, Tile::Missing);
                }
            }
        }
    }

    /// Keep Quick Look's working set to current, previous, and next.
    ///
    /// LRU is the fallback at a filtered edge or while a new neighbour is landing,
    /// but proximity wins first: a two-steps-away texture must not evict the frame
    /// one arrow press behind merely because both arrived on the same UI frame.
    fn evict_preview_textures(&mut self) {
        while self.preview_textures.len() > PREVIEW_CACHE_CAP {
            let focus = self.preview.or(self.selected);
            let mut working_set = Vec::with_capacity(PREVIEW_CACHE_CAP);
            if let Some(idx) = focus {
                working_set.push(self.full_key(idx));
                if let Some(at) = self.visible.iter().position(|visible| *visible == idx) {
                    if let Some(before) =
                        at.checked_sub(1).and_then(|i| self.visible.get(i)).copied()
                    {
                        working_set.push(self.full_key(before));
                    }
                    if let Some(after) = self.visible.get(at + 1).copied() {
                        working_set.push(self.full_key(after));
                    }
                }
            }
            let victim = self
                .preview_textures
                .iter()
                .filter(|(key, _)| !working_set.contains(key))
                .min_by_key(|(_, ready)| ready.seen)
                .map(|(key, _)| *key)
                .or_else(|| {
                    let protected = focus.map(|idx| self.full_key(idx));
                    self.preview_textures
                        .iter()
                        .filter(|(key, _)| Some(**key) != protected)
                        .min_by_key(|(_, ready)| ready.seen)
                        .map(|(key, _)| *key)
                });
            let Some(victim) = victim else { break };
            self.preview_textures.remove(&victim);
        }
    }

    /// Ask for a thumbnail, unless it is already asked for or already here.
    fn request(&mut self, idx: u32, priority: u32) {
        if self.tiles.contains_key(&idx) {
            return;
        }
        if self.search_offline.contains(&idx) {
            self.tiles.insert(idx, Tile::Missing);
            return;
        }
        let Some(entry) = self.entries.get(idx as usize) else {
            return;
        };
        // **Only a picture has a thumbnail to ask for.** A folder and a `.txt` are drawn
        // from an icon, and queueing them would be four workers opening files that
        // cannot produce an image — every one of them a guaranteed `Tile::Missing`, and
        // in a documents folder with "other files" on, enough of them to hold up the
        // pictures that *can* be drawn.
        if entry.kind != Kind::Picture {
            return;
        }
        let key = Key {
            generation: self.generation,
            idx,
            full: false,
        };
        let path = entry.path.clone();
        let cache = self.cache_dir.clone();
        let edited = self.xmp_thumbnails;
        let upright_as = entry.orientation;
        self.tiles.insert(idx, Tile::Pending);
        self.queue.submit(key, priority, move || {
            thumbnail(&path, cache.as_deref(), edited, upright_as)
        });
    }

    /// Drop the textures nobody has looked at for longest.
    ///
    /// Only textures. A dropped tile is re-read from the disk cache in well under a
    /// millisecond, so this trades a ceiling on GPU memory for something the user
    /// cannot perceive.
    fn evict(&mut self) {
        let ready = self
            .tiles
            .values()
            .filter(|t| matches!(t, Tile::Ready { .. }))
            .count();
        if ready <= TEXTURE_CAP {
            return;
        }
        let mut seen: Vec<(u64, u32)> = self
            .tiles
            .iter()
            .filter_map(|(i, t)| match t {
                Tile::Ready { seen, .. } => Some((*seen, *i)),
                _ => None,
            })
            .collect();
        seen.sort_unstable();
        for (_, idx) in seen.into_iter().take(ready - TEXTURE_CAP) {
            self.tiles.remove(&idx);
        }
    }
}

// ------------------------------------------------------------------- the worker

/// Where cached tiles live.
///
/// In the OS cache tree. Unlike `app.ron`, preferences, presets and manual ordering,
/// every file here is rebuildable; putting it in durable application data makes cache
/// cleanup and roaming-backup behavior wrong on Windows and Linux.
fn cache_dir() -> Option<PathBuf> {
    let dir = crate::settings::cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// The cache filename for a file as it is right now.
///
/// **Path, size and mtime** — not the content key `decode` uses. That one exists to
/// notice that two paths hold the same bytes, which is worth ~100 ms of hashing when
/// it saves a 600 MB decode and is absurd when it saves a 50 KB JPEG read. Here the
/// question is only "is this the same file I saw last time", and three cheap
/// `stat` fields answer it.
///
/// A false *hit* would need a file to be edited in place keeping its size and mtime;
/// a false *miss* costs one thumbnail. The asymmetry is why this is fine.
/// [`cache_name`] for a frame that may have been turned.
///
/// The orientation is part of what the tile *looks like*, so it has to be part of the
/// key — otherwise turning a frame serves the tile made before it was turned, forever.
fn turned_cache_name(
    path: &Path,
    meta: &std::fs::Metadata,
    upright_as: Option<raw_core::Orientation>,
) -> String {
    match upright_as {
        None => cache_name(path, meta),
        Some(o) => {
            let mut n = cache_name(path, meta);
            n.truncate(n.len() - 4);
            format!("{n}-o{}.jpg", o as u8)
        }
    }
}

fn cache_name(path: &Path, meta: &std::fs::Metadata) -> String {
    // FNV-1a, the same hash `decode::ContentKey` uses, for the same reason: a cache
    // key needs to be well-mixed, not cryptographic.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    // **The version comes first, and bumping it retires every tile on disk.**
    //
    // The key answers "is this the same file", and that is not the whole question —
    // it is "would this file produce the same tile", and the code that makes the
    // tile is half of that. Version 1 tiles were stored the way the sensor read
    // them, so every portrait frame in them is lying on its side; nothing about the
    // *file* changed when that was fixed, so without this they would be served
    // sideways forever.
    //
    // Bump on any change to what a tile looks like: orientation, size, the filter.
    const TILE_CACHE_VERSION: u8 = 2;
    h ^= TILE_CACHE_VERSION as u64;
    h = h.wrapping_mul(0x1000_0000_01b3);
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    };
    eat(path.as_os_str().as_encoded_bytes());
    eat(&meta.len().to_le_bytes());
    if let Ok(t) = meta.modified()
        && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
    {
        eat(&d.as_nanos().to_le_bytes());
    }
    format!("{h:016x}.jpg")
}

/// Snapshot of the raw and sidecar identity used to render an edited tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditedTileStamp(String);

impl EditedTileStamp {
    pub fn capture(image: &Path) -> Option<Self> {
        let side = raw_core::sidecar::path_for(image);
        let raw = cache_name(image, &std::fs::metadata(image).ok()?);
        let side = cache_name(&side, &std::fs::metadata(&side).ok()?);
        // Version 4 includes sidecar size as well as its full modification time.
        Some(Self(format!("{}-e4-{side}", raw.trim_end_matches(".jpg"))))
    }

    pub fn matches(&self, image: &Path) -> bool {
        Self::capture(image).as_ref() == Some(self)
    }
}

fn edited_cache_name(image: &Path, cache: &Path) -> Option<PathBuf> {
    Some(cache.join(EditedTileStamp::capture(image)?.0))
}

fn move_cache(source: Option<PathBuf>, destination: Option<PathBuf>) {
    let (Some(source), Some(destination)) = (source, destination) else {
        return;
    };
    if !source.exists() || source == destination {
        return;
    }
    if destination.exists() {
        let _ = std::fs::remove_file(source);
    } else {
        let _ = std::fs::rename(source, destination);
    }
}

/// Publish pixels only under the source stamp captured before their render.
/// Returns false when inputs changed or the optional disk cache could not be written.
pub fn store_edited_tile(
    image: &Path,
    stamp: &EditedTileStamp,
    w: u32,
    h: u32,
    rgba: &[u8],
) -> bool {
    let Some(cache) = cache_dir() else {
        return false;
    };
    if !stamp.matches(image) {
        return false;
    }
    let file = cache.join(&stamp.0);
    if w == 0 || h == 0 || rgba.len() < (w * h * 4) as usize {
        return false;
    }

    let edge = raw_core::preview::TILE_EDGE;
    let scale = (edge as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * scale) as u32).max(1),
        ((h as f32 * scale) as u32).max(1),
    );
    let Some(src) = image::RgbaImage::from_raw(w, h, rgba.to_vec()) else {
        return false;
    };
    let small = image::imageops::thumbnail(&src, tw, th);
    let rgb: Vec<u8> = small
        .pixels()
        .flat_map(|p| [p.0[0], p.0[1], p.0[2]])
        .collect();

    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, 88);
    if enc
        .encode(&rgb, tw as u16, th as u16, jpeg_encoder::ColorType::Rgb)
        .is_err()
        || !stamp.matches(image)
    {
        return false;
    }
    // Never choose the filename again after rendering: even an edit arriving
    // during the write can only leave an unused tile under its original stamp.
    raw_core::atomic_file::write(&file, |file| {
        use std::io::Write;
        file.write_all(&out)
    })
    .is_ok()
        && stamp.matches(image)
}

/// One thumbnail: the disk cache if it has one, the raw's embedded JPEG otherwise.
///
/// Runs on a queue worker, so it takes owned data and touches nothing shared.
fn thumbnail(
    path: &Path,
    cache: Option<&Path>,
    prefer_edited: bool,
    upright_as: Option<raw_core::Orientation>,
) -> Option<raw_core::preview::Rgb8> {
    // **The edited tile first, when it is wanted and there is one.** This is the
    // prototype's second pass, except the render happened in Develop rather than
    // here — see [`store_edited_tile`]. A file nobody has opened since the feature
    // existed simply has none, and shows the camera's thumbnail as before.
    if prefer_edited
        && let Some(dir) = cache
        && let Some(file) = edited_cache_name(path, dir)
        && let Ok(bytes) = std::fs::read(&file)
        && let Some(img) = decode_jpeg(&bytes)
    {
        return Some(img);
    }

    let cached = cache
        .zip(std::fs::metadata(path).ok())
        .map(|(dir, m)| dir.join(turned_cache_name(path, &m, upright_as)));

    if let Some(file) = &cached
        && let Ok(bytes) = std::fs::read(file)
        && let Some(img) = decode_jpeg(&bytes)
    {
        return Some(img);
    }

    // A raw hands over its embedded JPEG; an ordinary picture is simply decoded and
    // reduced. Same box filter at the same size, so a folder of both looks like one
    // grid rather than two.
    let img = if is_raw(path) {
        raw_core::preview::tile(path, upright_as)
    } else {
        image_tile(path)
    }?;

    // Write the cache, and do not care if it fails. A read-only cache directory, a
    // full disk or a race with another instance all mean the same thing here: the
    // next visit pays for the tile again, which is slow rather than wrong.
    if let Some(file) = &cached {
        let mut out = Vec::new();
        let enc = jpeg_encoder::Encoder::new(&mut out, 88);
        if enc
            .encode(
                &img.data,
                img.w as u16,
                img.h as u16,
                jpeg_encoder::ColorType::Rgb,
            )
            .is_ok()
        {
            // Write and rename, so a killed process cannot leave a half-written JPEG
            // behind that every later run would read as a valid tile.
            let _ = raw_core::atomic_file::write(file, |destination| {
                std::io::Write::write_all(destination, &out)
            });
        }
    }

    Some(img)
}

/// Contact sheets share Lightbox's edited-tile contract but may ask for more pixels
/// than a browser tile. A developed preview is intentionally never invented at a
/// higher resolution than the cached render; camera previews and ordinary pictures
/// can be decoded to the requested edge for the final PDF.
pub(crate) fn contact_thumbnail(
    path: &Path,
    cache: Option<&Path>,
    prefer_edited: bool,
    edge: u32,
    upright_as: Option<raw_core::Orientation>,
) -> Option<raw_core::preview::Rgb8> {
    if prefer_edited
        && let Some(dir) = cache
        && let Some(file) = edited_cache_name(path, dir)
        && let Ok(bytes) = std::fs::read(file)
        && let Some(image) = decode_jpeg(&bytes)
    {
        return Some(image);
    }
    if edge <= raw_core::preview::TILE_EDGE {
        return thumbnail(path, cache, false, upright_as);
    }
    if is_raw(path) {
        raw_core::preview::screen(path, edge, upright_as)
    } else {
        image_screen(path, edge)
    }
}

/// Every raw in a folder, and optionally in everything below it.
///
/// **Depth-limited and hidden-skipping.** A photo tree is wide rather than deep, and
/// the guard is against the accident: pointing the sweep at your home folder should
/// cost a bounded walk rather than an unbounded one through every bundle and cache
/// on the disk. Files come back in whatever order the filesystem gives; the caller
/// sorts.
/// What the grid should show, and what each thing is.
///
/// # The two `show_` flags are listing rules, not view filters
///
/// They decide what goes into `entries` rather than what survives `Filters`, which is
/// the same split `subfolders` already sits on. That is why turning one on has to
/// re-read the folder — see `Lightbox::set_listing` — and it is the right side of the
/// split: a folder tile is not a picture that has been hidden, it is a different kind
/// of thing that was not being collected.
///
/// # Folders are listed from *this* directory only
///
/// Never recursed into, even when `subfolders` is on. Recursive listing flattens a tree
/// into one grid of pictures, and adding the folders back would show you both the
/// container and its contents at the same level — the same frames twice over, once
/// inside a tile you can open and once beside it. So the folder tiles are what sits
/// directly here, which is also the only set double-clicking into makes sense for.
fn list_entries(dir: &Path, subfolders: bool, folders: bool, others: bool) -> Vec<(PathBuf, Kind)> {
    const MAX_DEPTH: usize = 8;
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for p in rd.flatten().map(|e| e.path()) {
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if p.is_dir() {
                // Listed only at depth 0 — see the note above.
                if folders && !hidden && depth == 0 {
                    out.push((p.clone(), Kind::Folder));
                }
                let child = depth + 1;
                if subfolders && !hidden && child <= MAX_DEPTH {
                    stack.push((p, child));
                }
            } else if is_listed(&p) {
                out.push((p, Kind::Picture));
            } else if others && !hidden {
                // **The sidecars stay out of it**, and this is the one exclusion worth
                // making by hand. `.mono.xmp` is this app's own bookkeeping; listing it
                // beside the frame it belongs to would double every worked-on picture
                // in the grid and invite somebody to open one.
                if !is_sidecar(&p) {
                    out.push((p, Kind::Other));
                }
            }
        }
    }
    out
}

/// What the thumbnail cache is occupying on disk, in bytes.
///
/// **Walked rather than tracked.** A running total kept in memory would need every
/// write, eviction and failed write to remember to update it, and would still be wrong
/// after a crash or after somebody emptied the folder in the Finder. The directory is
/// flat and its entries are small — a folder of ten thousand tiles is one `read_dir`
/// and ten thousand `stat`s, a few milliseconds — and it is read when the Settings page
/// asks, not per frame. See [`App::cache_size`], which caches the answer for the life
/// of the sheet.
pub fn cache_bytes() -> u64 {
    let Some(dir) = cache_dir() else { return 0 };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Delete every cached tile, and report how many bytes went.
///
/// # What this is safe to do
///
/// Everything in here is **derived and rebuildable**: camera thumbnails, the edited
/// tiles Develop writes on its way out, and the `.when` capture dates. Nothing in this
/// directory is the only copy of anything. That is what makes a purge button reasonable
/// at all — the cost of pressing it is that the next visit to a folder is as slow as
/// the first one was, and no more.
///
/// # What it deliberately does not do
///
/// It does not remove the directory, only its files, so `cache_dir`'s
/// `create_dir_all` is not racing a purge to recreate it. And it skips subdirectories
/// entirely — nothing writes one today, and a recursive delete rooted at a path built
/// from `settings::dir()` is a great deal more dangerous than this feature is worth.
///
/// Individual failures are counted and skipped rather than aborting: a tile held open
/// by something else should not stop the other nine thousand going.
pub fn purge_cache() -> u64 {
    let Some(dir) = cache_dir() else { return 0 };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut freed = 0;
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        if std::fs::remove_file(e.path()).is_ok() {
            freed += meta.len();
        }
    }
    freed
}

/// `4.2 GB`, `812 MB`, `0 bytes` — the same shape `byte_label` gives a file size.
pub fn cache_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if bytes == 0 {
        "empty".to_owned()
    } else if b < KB {
        format!("{bytes} bytes")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

/// Whether this is a sidecar this app wrote. Compared against the real suffix rather
/// than against `xmp`, so another application's plain `.xmp` still lists.
fn is_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_lowercase().ends_with(".mono.xmp"))
}

/// The icon name for a file, from its extension. See `icons::SOURCES`.
///
/// **Every arm here must name an icon that list actually loads**, which is what
/// `every_file_icon_the_grid_asks_for_is_loaded` checks — a missing texture falls back
/// to a text glyph, and on a grid tile with no text that is a blank card rather than a
/// visibly wrong one.
///
/// Unknown extensions get the plain `file` sheet, which is the honest answer: the grid
/// is saying "something is here and it is not a picture", and inventing a glyph for a
/// type nobody anticipated would say more than it knows.
pub fn icon_for(ext: &str) -> &'static str {
    FILE_ICONS
        .iter()
        .find(|(exts, _)| exts.contains(&ext))
        .map(|(_, icon)| *icon)
        .unwrap_or(FALLBACK_ICON)
}

/// Extensions to icon name, in the order they are searched.
///
/// **A table rather than a `match`**, so `every_file_icon_the_grid_asks_for_is_loaded`
/// can walk it. The same rule as [`icons::SOURCES`] being a named list: a mapping the
/// test cannot enumerate is a mapping where the next entry added is the one that names
/// a texture nobody loaded.
///
/// The linear scan is over twenty groups of short strings, most of them rejected on the
/// first byte, and only for tiles that are *not* pictures. Against one `read_dir` per
/// folder it does not register.
type IconGroup = (&'static [&'static str], &'static str);
const FILE_ICONS: &[IconGroup] = &[
    (&["txt", "text", "log", "rtf"], "file-txt"),
    (&["md", "markdown"], "file-md"),
    (&["pdf"], "file-pdf"),
    (&["doc", "docx", "pages", "odt"], "file-doc"),
    (&["xls", "xlsx", "numbers", "ods", "csv", "tsv"], "file-xls"),
    (&["zip"], "file-zip"),
    (
        &["gz", "tar", "bz2", "xz", "7z", "rar", "dmg"],
        "file-archive",
    ),
    (
        &["wav", "aif", "aiff", "mp3", "m4a", "flac", "aac", "ogg"],
        "file-audio",
    ),
    (&["html", "htm", "xhtml"], "file-html"),
    (&["js", "mjs", "cjs", "jsx", "ts", "tsx"], "file-js"),
    (&["py", "pyw"], "file-py"),
    (&["rs"], "file-rs"),
    (&["c", "h"], "file-c"),
    (&["cpp", "cc", "cxx", "hpp", "hh"], "file-cpp"),
    (&["sql", "db", "sqlite"], "file-sql"),
    (&["svg"], "file-svg"),
    // **`icloud` is a placeholder, not a document.** macOS leaves a
    // `.filename.icloud` stub for evicted files; the grid showing a cloud says the
    // bytes are not here, which is exactly what you need to know before wondering why a
    // folder looks empty.
    (&["icloud"], "file-cloud"),
    // The remaining code-ish types Phosphor has no separate mark for.
    (
        &[
            "json", "toml", "yaml", "yml", "xml", "ron", "sh", "zsh", "bash", "rb", "go", "java",
            "swift", "kt", "lua", "pl", "php", "css", "scss", "ini", "cfg", "conf", "wgsl", "glsl",
            "metal",
        ],
        "file-code",
    ),
];

/// What an extension nobody anticipated gets. The plain sheet, which says "something is
/// here and it is not a picture" and claims nothing further.
const FALLBACK_ICON: &str = "file";

/// A file's capture time, from the disk cache if it has been asked before.
///
/// The cache is a two-line text file beside the tile, keyed the same way, so it is
/// retired by the same version bump and by the same file change. Kept separate from
/// the JPEG rather than embedded in it because a tile and a date are wanted at
/// different moments: the grid needs tiles for what is on screen, and a Date sort
/// needs dates for everything.
fn captured_at(path: &Path, cache: Option<&Path>) -> Option<std::time::SystemTime> {
    let file = cache
        .zip(std::fs::metadata(path).ok())
        .map(|(dir, m)| dir.join(cache_name(path, &m)).with_extension("when"));

    if let Some(f) = &file
        && let Ok(text) = std::fs::read_to_string(f)
    {
        // An empty file is a remembered "this raw declares no capture date", which is
        // worth caching too — otherwise every Date sort re-reads every such file.
        return text
            .trim()
            .parse::<u64>()
            .ok()
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
    }

    let when = raw_core::sensor::capture_time(path);
    if let Some(f) = &file {
        let text = when
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        let _ = raw_core::atomic_file::write(f, |destination| {
            std::io::Write::write_all(destination, text.as_bytes())
        });
    }
    when
}

/// What the camera recorded, as rows for the shared EXIF `key / value` grid.
/// Goes through `raw_core::sensor::Metadata` rather than reaching for rawler here,
/// so tags are interpreted in the one place that already does it. Missing values are
/// retained as `None`; the shared row renderer turns those into stable em-dash rows.
#[derive(Clone)]
pub struct Facts {
    pub rows: Vec<(String, Option<String>)>,
}

const IPTC_FIELDS: usize = raw_core::sidecar::IptcField::ALL.len();

/// Text being edited in the IPTC panel. A multi-selection stores the common value
/// for each field and marks the ones that differ, so changing one field applies only
/// that field to the whole selection.
struct IptcDraft {
    paths: Vec<PathBuf>,
    values: [String; IPTC_FIELDS],
    mixed: [bool; IPTC_FIELDS],
    dirty: [bool; IPTC_FIELDS],
    ids: [Option<egui::Id>; IPTC_FIELDS],
}

impl IptcDraft {
    fn load(paths: Vec<PathBuf>) -> Self {
        let records: Vec<raw_core::sidecar::Metadata> = paths
            .iter()
            .map(|path| raw_core::sidecar::effective_metadata(path).unwrap_or_default())
            .collect();
        let values = std::array::from_fn(|i| {
            records
                .first()
                .and_then(|metadata| metadata.iptc(raw_core::sidecar::IptcField::ALL[i]))
                .unwrap_or("")
                .to_owned()
        });
        let mixed = std::array::from_fn(|i| {
            let field = raw_core::sidecar::IptcField::ALL[i];
            records
                .iter()
                .skip(1)
                .any(|metadata| metadata.iptc(field).unwrap_or("") != values[i].as_str())
        });
        Self {
            paths,
            values,
            mixed,
            dirty: [false; IPTC_FIELDS],
            ids: [None; IPTC_FIELDS],
        }
    }
}

#[derive(Clone, Copy)]
enum TemplateNamePurpose {
    SaveCurrent,
    Rename(usize),
}

struct TemplateNameDialog {
    purpose: TemplateNamePurpose,
    value: String,
    request_focus: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TemplateFieldMode {
    Unchanged,
    Set,
    Clear,
}

impl TemplateFieldMode {
    const ALL: [Self; 3] = [Self::Unchanged, Self::Set, Self::Clear];

    fn label(self) -> &'static str {
        match self {
            Self::Unchanged => "Leave unchanged",
            Self::Set => "Set value",
            Self::Clear => "Clear existing",
        }
    }
}

/// What the camera and file recorded, in Develop EXIF's order and vocabulary.
/// Missing values stay in the grid as em dashes through `info_row`, so switching
/// frames never moves every row below the absent fact. Print and Name are the two
/// deliberate omissions: the browser has no print decision, and the filename is
/// already the Metadata pane's heading.
fn read_exif(path: &Path) -> Vec<Facts> {
    let raw = is_raw(path);
    // **Two readers, one set of rows.** rawler answers for a raw and nothing else, so
    // an ordinary picture used to show its size and its file name and stop — every
    // camera row blank whether or not the file recorded one, which is the failure this
    // panel is least able to explain. The rows below do not know which reader answered.
    let probed = if raw {
        raw_core::sensor::probe(path)
    } else {
        raw_core::sensor::probe_rendered(path)
    };
    let camera = probed
        .as_ref()
        .map(|(camera, _)| camera.clone())
        .filter(|camera| !camera.is_empty());
    let metadata = probed.as_ref().map(|(_, metadata)| metadata);

    // Two ways to a size, and the cheap one is picked per format. Neither decodes
    // pixels, and neither runs until a tile is selected.
    let dims = if raw {
        raw_core::sensor::raw_dimensions(path)
    } else {
        image::image_dimensions(path)
            .ok()
            .map(|(w, h)| (w as usize, h as usize))
    };
    let pixels = dims.map(|(w, h)| {
        let mp = (w * h) as f32 / 1.0e6;
        format!("{w} × {h}  ({mp:.1} MP)")
    });
    let file = std::fs::metadata(path)
        .ok()
        .map(|metadata| crate::byte_label(metadata.len()));
    let format = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_uppercase);

    // A raw has no source colour space. For rendered pictures, read the embedded
    // profile name without decoding the image; an untagged file reports `none`.
    use image::ImageDecoder as _;
    let color_space = (!raw)
        .then(|| {
            image::ImageReader::open(path)
                .ok()?
                .with_guessed_format()
                .ok()?
                .into_decoder()
                .ok()
                .and_then(|mut decoder| decoder.icc_profile().ok().flatten())
                .and_then(|icc| raw_core::colour::icc_description(&icc))
                .or_else(|| Some("none".to_owned()))
        })
        .flatten();

    let mut rows = vec![
        ("Camera".to_owned(), camera),
        ("Lens".to_owned(), metadata.and_then(|m| m.lens.clone())),
        (
            "Date/Time".to_owned(),
            metadata.and_then(|m| m.date_time.clone()),
        ),
        (
            "Focal".to_owned(),
            metadata
                .and_then(|m| m.focal_len)
                .filter(|focal| focal.is_finite() && *focal > 0.0)
                .map(crate::focal_label),
        ),
        (
            "ISO".to_owned(),
            metadata
                .and_then(|m| m.iso)
                .filter(|iso| *iso > 0)
                .map(|iso| iso.to_string()),
        ),
        (
            "Aperture".to_owned(),
            metadata
                .and_then(|m| m.aperture)
                .filter(|aperture| aperture.is_finite() && *aperture > 0.0)
                .map(|aperture| format!("f/{aperture:.1}")),
        ),
        (
            "Shutter".to_owned(),
            metadata.and_then(|m| m.shutter_label()),
        ),
        (
            "Metering".to_owned(),
            metadata.and_then(|m| m.metering.map(str::to_owned)),
        ),
        (
            "WB".to_owned(),
            metadata.and_then(|m| m.white_balance.map(str::to_owned)),
        ),
        ("Pixels".to_owned(), pixels),
        ("File".to_owned(), file),
        ("Format".to_owned(), format),
        ("Color Space".to_owned(), color_space),
    ];
    if let Some(bias) = metadata
        .and_then(|m| m.exposure_bias)
        .filter(|bias| bias.abs() > 1.0e-3)
    {
        rows.push(("Exp Comp".to_owned(), Some(format!("{bias:+.2} EV"))));
    }
    vec![Facts { rows }]
}

/// Where a folder's manual order is kept.
///
/// **In this app's cache, keyed by the folder's path** — never in the photo
/// directory. the maintainer's call, against a `monopro:SortOrder` per sidecar: the order is
/// not worth writing a sidecar for every file in a folder the first time somebody
/// drags one, including files nobody has ever edited.
///
/// It sharpens the rule the sidecar follows rather than bending it. **A sidecar
/// carries what you decided about a picture; the cache carries how you arranged a
/// folder.** A rating is a judgement about an image and belongs to it wherever it
/// goes; a manual order is a property of one folder on one machine at one moment and
/// is meaningless the instant the set changes. So losing it when the folder is copied
/// is the honest behaviour rather than a shortfall — and "a file with no sidecar is a
/// file nobody has edited" stays a statement worth trusting.
fn order_path(dir: &Path) -> Option<PathBuf> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dir.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    let d = crate::settings::dir()?.join("order");
    std::fs::create_dir_all(&d).ok()?;
    Some(d.join(format!("{h:016x}.txt")))
}

/// Filenames, one per line, in the order they should appear.
///
/// Names rather than paths, so the file is readable and so a folder that moved keeps
/// its arrangement under its new path once you drag one thing. Nothing validates
/// them against the folder: a name that is no longer there simply never matches.
fn read_order(dir: &Path) -> Vec<String> {
    order_path(dir)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| {
            t.lines()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
fn write_order(dir: &Path, order: &[String]) {
    let Some(p) = order_path(dir) else { return };
    let text = order.join("\n");
    let _ = raw_core::atomic_file::write(&p, |destination| {
        std::io::Write::write_all(destination, text.as_bytes())
    });
}

/// Write a rating and a label back beside the image.
///
/// **Only `<stem>.mono.xmp` is ever written.** The conservative half of the
/// two-sidecar question, and now the chosen one: a foreign `<stem>.xmp` from Bridge
/// or Lightroom is read for what it carries and never touched. Writing one would mean
/// this app editing a file another application owns, on a machine where that
/// application may also be running, with no way to merge what it did not understand.
///
/// **Read, amend, write** — never a fresh sidecar from these two fields alone. The
/// file holds develop parameters, a curve, dodge and burn instances and whatever
/// another application put in the standard blocks; a rating is a small change to a
/// large document, and treating it as the whole document would silently discard
/// somebody's work. This is the same promise `Metadata`'s own note makes.
fn write_metadata(image: &Path, rating: i32, label: Option<&str>) -> std::io::Result<()> {
    use raw_core::sidecar::Loaded;

    let (params, mut metadata) = match raw_core::sidecar::read(image) {
        Loaded::Ok(s) => (s.params, s.metadata),
        // No sidecar yet. A rating is reason enough to make one, at default params —
        // which is honestly what "rated, not developed" is.
        Loaded::Absent => (Default::default(), Default::default()),
        // **Refuse rather than overwrite.** A sidecar that will not parse holds work
        // this app cannot see, and writing a two-field replacement over it would
        // destroy exactly what the corruption is hiding. The caller says so and the
        // rating does not take.
        Loaded::Corrupt(why) => {
            return Err(std::io::Error::other(format!(
                "sidecar will not parse, refusing to overwrite it — {why}"
            )));
        }
    };

    // Zero is "unrated" and is written as *absent* rather than as `0`. That is what
    // the tag means: `xmp:Rating="0"` is a rating of nought, and taking the last star
    // back off should leave no statement behind rather than a different one.
    metadata.rating = (rating > 0).then_some(rating);
    metadata.cleared.retain(|key| key != "rating");
    if rating == 0 {
        metadata.cleared.push("rating".to_owned());
    }
    metadata.label = label.map(str::to_owned);
    metadata.cleared.retain(|key| key != "label");
    if label.is_none() {
        metadata.cleared.push("label".to_owned());
    }
    raw_core::sidecar::write(image, &params, &metadata)
}

/// The mark on a frame that has been worked on. the maintainer's colour and weight.
///
/// Outside the picture's edge rather than inside it, so the rule does not cover a
/// pixel of the photograph it is marking.
const EDITED_INK: egui::Color32 = egui::Color32::from_rgb(0xE9, 0xC5, 0x00);
const EDITED_STROKE: f32 = 1.6;

/// Shorten text to fit a width, keeping both ends.
///
/// The middle goes because the ends are what tell two frames in a shoot apart —
/// `sample-session_0008_architecture.dng` and `…0009…` differ in the middle of a name
/// whose head and tail are identical, so a plain truncation would make every tile in
/// a folder read the same.
fn elide(ui: &egui::Ui, text: &str, font: &egui::FontId, width: f32) -> String {
    let measure = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_owned(), font.clone(), theme::NAME)
            .rect
            .width()
    };
    if width <= 0.0 || measure(text) <= width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    while keep > 4 {
        keep -= 2;
        let head: String = chars[..keep / 2].iter().collect();
        let tail: String = chars[chars.len() - (keep - keep / 2)..].iter().collect();
        let candidate = format!("{head}…{tail}");
        if measure(&candidate) <= width {
            return candidate;
        }
    }
    "…".to_owned()
}

/// Quarter turns as radians, clockwise.
///
/// egui rotates clockwise for a positive angle, which is `composition::Orientation`'s
/// convention too — so a turn count means the same thing here as everywhere else.
/// The file's own idea of which way up it is, for a frame nobody has turned.
fn as_shot(path: &Path) -> raw_core::Orientation {
    let probe = if is_raw(path) {
        raw_core::sensor::probe(path)
    } else {
        raw_core::sensor::probe_rendered(path)
    };
    probe.map_or_else(Default::default, |(_, m)| m.orientation)
}

/// Turn the frame a quarter turn in its sidecar, crop included, keeping every other
/// edit and all metadata. Returns the new orientation.
fn write_turn(path: &Path, clockwise: bool) -> Result<raw_core::Orientation, String> {
    let (mut params, metadata) = match raw_core::sidecar::read(path) {
        raw_core::sidecar::Loaded::Ok(s) => (s.params, s.metadata),
        raw_core::sidecar::Loaded::Absent => Default::default(),
        // Refusing here is the same rule the rest of the app follows: a file we
        // cannot read is one we must not overwrite.
        raw_core::sidecar::Loaded::Corrupt(e) => {
            return Err(format!("{}: {e}", name_of(path)));
        }
    };
    // A quarter turn from where it is now, which for a frame nobody has turned means
    // from the file's own tag. That probe is paid only by an unturned frame.
    let exif = if params.composition.orientation.is_some() {
        raw_core::Orientation::default()
    } else {
        as_shot(path)
    };
    params.composition.turn(clockwise, exif);
    raw_core::sidecar::write(path, &params, &metadata)
        .map_err(|e| format!("{}: {e}", name_of(path)))?;
    Ok(params.composition.orientation(exif))
}

/// A camera thumbnail, drawn grey.
///
/// **Rec.709 luma on the camera's JPEG, and it is a way of looking rather than a
/// measurement.** This is not the app's luminance model and must never be mistaken
/// for it: the pipeline's grey comes from photosites through a chosen luminance
/// mode, where this is the camera's own rendered, matrixed, tone-curved RGB flattened
/// by the standard display coefficients. It answers "roughly how does this tone out"
/// while browsing, which is a question about which frame to open, and it reaches no
/// pixel of anything.
///
/// The same containment the reference views have: 8-bit, screen-only, and nothing
/// downstream can consume it.
fn grey_image(img: &raw_core::preview::Rgb8) -> egui::ColorImage {
    let mut px = Vec::with_capacity(img.pixels());
    for c in img.data.chunks_exact(3) {
        let y = (0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32).round() as u8;
        px.push(egui::Color32::from_gray(y));
    }
    egui::ColorImage {
        size: [img.w, img.h],
        pixels: px,
        source_size: egui::vec2(img.w as f32, img.h as f32),
    }
}

/// A tile from an ordinary picture file.
///
/// `image::open` sniffs the format from the bytes rather than trusting the extension,
/// which is what a browser wants — a `.jpg` that is really a PNG still draws.
///
/// **EXIF orientation is not applied here**, unlike the raw path: the `image` crate
/// does not read it and the browser would need its own parser. A phone JPEG shot in
/// portrait may therefore lie on its side, and the rotate buttons are the answer until
/// that is worth building.
fn image_tile(path: &Path) -> Option<raw_core::preview::Rgb8> {
    image_screen(path, raw_core::preview::TILE_EDGE)
}

/// An ordinary picture reduced to fit a box. See [`image_tile`].
fn image_screen(path: &Path, edge: u32) -> Option<raw_core::preview::Rgb8> {
    use image::ImageDecoder;

    // **The tag is honoured here too.** A phone JPEG shot in portrait is stored
    // landscape with an orientation tag, exactly as a raw's embedded preview is —
    // `image` 0.25 reads it off the decoder and can apply it, so there is no reason
    // for the browser to show one kind of file sideways and not the other.
    //
    // Read *before* decoding, because taking the decoder consumes it.
    let reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut img = image::DynamicImage::from_decoder(decoder).ok()?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }

    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let rgb = if w.max(h) > edge {
        let scale = edge as f32 / w.max(h) as f32;
        let tw = ((w as f32 * scale).round() as u32).max(1);
        let th = ((h as f32 * scale).round() as u32).max(1);
        // `raw_core::preview::box_down`, not `imageops::thumbnail`, for the reason
        // stated there — and *here* as well as on the raw path, because an 80 MB TIFF
        // is exactly the size where the difference is worth the most. A folder of
        // scans is otherwise the slowest thing this grid lists.
        raw_core::preview::box_down(&img.into_rgb8(), tw, th)
    } else {
        img.to_rgb8()
    };
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Some(raw_core::preview::Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Option<raw_core::preview::Rgb8> {
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Some(raw_core::preview::Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    })
}

// ------------------------------------------------------------------------- the UI

/// Columns, card width and card height for a viewport.
///
/// **The ladder picks the column count, not the card width** — the prototype's
/// arithmetic at `monopro.py:23622-23629`. Divide the viewport by the target width to
/// get the columns, then share the width back out evenly so the cards reach both
/// edges. A fixed width would leave a ragged right margin that grew and shrank as the
/// window was dragged, which is the one thing a contact sheet must not do; the size
/// slider is choosing "about this big", and the grid is what makes it exact.
fn grid_metrics(available_width: f32, rung: Rung, furniture_h: f32) -> (usize, f32, f32) {
    let avail = (available_width - 2.0 * MARGIN).max(1.0);
    let cols = match rung {
        Rung::Width(w) => ((avail / w).floor() as usize).max(1),
        Rung::Columns(n) => n.max(1),
    };
    let tile_w = ((avail - SPACING * (cols - 1) as f32) / cols as f32).max(80.0);
    // A square image well gives landscape and portrait frames the same maximum
    // displayed dimension. Filename and marks are furniture below that square, not
    // extra room for a portrait photograph.
    (cols, tile_w, tile_w.round() + furniture_h)
}

/// The two-tab mode switch, drawn at the right-hand end of the footer.
///
/// Returns the mode that was asked for, or `None` if neither was clicked.
///
/// **A mode reachable only by a key is a mode nobody finds.** The prototype's
/// `ModeTabStrip` (`monopro.py:26831`) is a pair of checkable buttons in the status
/// bar, and it is the affordance — `L` is the shortcut *for* it, not the only way in.
/// Built here rather than left to a later stage because the alternative is shipping a
/// mode with no visible door.
///
/// The active tab is **ruby, not the prototype's amber**. `PALETTE["accent"]` is
/// `#c8a96e` there and every checked control wears it; this app has one settled rule
/// for the footer — it reports state in ruby — and a second accent for the same job
/// would be two colours meaning "this is on". The 2 point rule over the active tab is
/// the prototype's and is kept: it is what makes the pair read as tabs rather than as
/// two words.
pub fn mode_tabs(ui: &mut egui::Ui, lightbox_active: bool) -> Option<bool> {
    const TAB_W: f32 = 72.0;
    const H: f32 = 20.0;

    let (rect, _) = ui.allocate_exact_size(egui::vec2(TAB_W * 2.0, H), egui::Sense::hover());
    let mut want = None;

    for (i, (label, is_lightbox)) in [("LIGHTBOX", true), ("DEVELOP", false)]
        .into_iter()
        .enumerate()
    {
        let tab = egui::Rect::from_min_size(
            egui::pos2(rect.min.x + i as f32 * TAB_W, rect.min.y),
            egui::vec2(TAB_W, H),
        );
        let r = ui.interact(tab, ui.id().with(("mode", i)), egui::Sense::click());
        let on = is_lightbox == lightbox_active;

        let colour = if on {
            theme::RUBY
        } else if r.hovered() {
            theme::BRIGHT
        } else {
            theme::DIM
        };
        if on {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(tab.min, egui::vec2(TAB_W, 2.0)),
                0.0,
                theme::RUBY,
            );
        }
        ui.painter().text(
            tab.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(8.0),
            colour,
        );
        if r.clicked() {
            want = Some(is_lightbox);
        }
        r.on_hover_cursor(egui::CursorIcon::PointingHand);
    }
    want
}

/// Draws Lightbox's panes, and carries out what they ask for.
///
/// Borrows the whole [`Lightbox`] because every pane reads from it and three of them
/// write to it. The results a caller needs — a file to open, a folder to go to — come
/// back on the struct rather than as a return value, because `egui_tiles` gives the
/// behavior no way to return anything from `pane_ui`.
struct Panes<'a> {
    lb: &'a mut Lightbox,
    icons: &'a crate::icons::Icons,
    /// Tiles that sit in a tab bar, and therefore already have a handle.
    tabbed: Vec<egui_tiles::TileId>,
    /// The tree's own egui id, which the drag handle has to sense under.
    tree_id: egui::Id,
    open: Option<PathBuf>,
    nav: Option<PathBuf>,
    dropped: bool,
}

impl Panes<'_> {
    /// A pane's header and contents, on the ground `pane_ui` has already painted.
    /// Returns whether the header started a drag.
    fn pane_body(&mut self, ui: &mut egui::Ui, tile_id: egui_tiles::TileId, pane: &Pane) -> bool {
        // **A pane that is not in a tab bar needs a handle, or it cannot be moved.**
        // the maintainer found FAVORITES and EXIF ungrabbable once stacked: a tab is its own
        // drag handle, and a lone pane has none. This is the header Develop's panes
        // already use, so the gesture is the same one in both modes — drag the title.
        let mut dragged = false;
        if *pane != Pane::Grid && !self.tabbed.contains(&tile_id) {
            // **The same bar a tabbed pane wears**, so a panel looks like a panel
            // whether or not it happens to share a tab strip: the tab bar's ground,
            // its hairline, and the name in ruby because this pane is the one you are
            // looking at. the maintainer found a lone pane wearing a plain grey caption where
            // Develop's panels carry a ruby header, and that was two idioms for one
            // thing.
            // **The handle senses under the tile's own egui id**, not one of its own.
            // `layout.rs:836` writes this down and I did not read it: `set_dragged_id`
            // writes into `interact_widgets`, which egui recomputes from hit-testing
            // every pass — so a title with its own id has egui naming *the title* as
            // the dragged widget from the second frame, `is_being_dragged(tile_id)`
            // goes false, and the drag evaporates one frame after it starts. A preview
            // that never appears and a drop that never lands, which is what "the panel
            // disappears if I dock it right" was.
            let handle = tile_id.egui_id(self.tree_id);
            let bar_h = theme::size::HEADER + 12.0;
            let bar = egui::Rect::from_min_size(
                ui.max_rect().min,
                egui::vec2(ui.max_rect().width(), bar_h),
            );
            ui.painter().rect_filled(bar, 0.0, theme::CHROME_DEEP);
            // Every panel begins with the same top rule. Favorites used to be the
            // only pane with one, which made stacked Folders and Search look joined.
            ui.painter().line_segment(
                [
                    egui::pos2(bar.min.x, bar.min.y + 0.5),
                    egui::pos2(bar.max.x, bar.min.y + 0.5),
                ],
                crate::layout::panel_rule(),
            );
            ui.painter().line_segment(
                [
                    egui::pos2(bar.min.x, bar.max.y - 0.5),
                    egui::pos2(bar.max.x, bar.max.y - 0.5),
                ],
                crate::layout::panel_rule(),
            );
            let text = ui.painter().layout_no_wrap(
                pane.label().to_owned(),
                egui::FontId::proportional(theme::size::HEADER),
                theme::RUBY,
            );
            let at = egui::pos2(bar.min.x + 10.0, bar.center().y);
            ui.painter().galley(
                egui::pos2(at.x, at.y - text.rect.height() * 0.5),
                text.clone(),
                theme::RUBY,
            );
            let hit_rect = egui::Rect::from_min_size(
                egui::pos2(at.x, at.y - text.rect.height() * 0.5),
                text.rect.size(),
            );
            let hit = ui.interact(hit_rect, handle, egui::Sense::drag());
            // **Started, not "is being dragged".** `egui_tiles` begins the drag when
            // it sees this once; saying it again on every frame of the drag restarts
            // it, and a drag that keeps restarting never reaches a drop.
            dragged = hit.drag_started();
            hit.on_hover_cursor(egui::CursorIcon::Grab);
            ui.advance_cursor_after_rect(bar);
        }

        match *pane {
            Pane::Grid => {
                if let Some(p) = self.lb.grid_ui(ui, self.icons) {
                    self.open = Some(p);
                }
            }
            Pane::Folders => {
                if let Nav::Open(dir) = self.lb.tree_ui(ui, self.icons) {
                    self.nav = Some(dir);
                }
            }
            Pane::Search => self.lb.search_ui(ui, self.icons),
            Pane::Favorites => {
                if let Some(dir) = self.lb.favorites_ui(ui, self.icons) {
                    self.nav = Some(dir);
                }
            }
            Pane::Exif => self.lb.exif_ui(ui),
        }
        dragged
    }
}

impl egui_tiles::Behavior<Pane> for Panes<'_> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tile_id: egui_tiles::TileId,
        pane: &mut Pane,
    ) -> egui_tiles::UiResponse {
        // Every pane paints its own ground: a tile is handed a bare `Ui` with no
        // `Frame` around it, so without this the central panel's fill shows through.
        //
        // **The grounds are settings**, the grid on the canvas value and the rest on
        // the panel value. Everything a pane draws was designed on the old fixed grey
        // (`CHROME` for the grid, `CHROME_DEEP` for the panels) and is re-greyed from
        // it — the same rule Develop's modules use, so text flips dark on a light
        // ground. Thumbnails are pictures and are never touched.
        let [canvas, panel, _] = self.lb.grounds;
        let ground = if *pane == Pane::Grid {
            theme::Ground {
                design: theme::CHROME.r(),
                to: canvas,
            }
        } else {
            theme::Ground {
                design: theme::CHROME_DEEP.r(),
                to: panel,
            }
        };
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_gray(ground.to));
        let dragged = theme::reground_ui(ui, ground, |ui| self.pane_body(ui, tile_id, pane));
        if dragged {
            egui_tiles::UiResponse::DragStarted
        } else {
            egui_tiles::UiResponse::None
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        pane.label().into()
    }

    fn tab_ui(
        &mut self,
        tiles: &mut egui_tiles::Tiles<Pane>,
        ui: &mut egui::Ui,
        id: egui::Id,
        tile_id: egui_tiles::TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Response {
        let name = tiles.get_pane(&tile_id).map_or("—", |pane| pane.label());
        let title = theme::header_size(ui.painter(), name);
        let pad = self.tab_title_spacing(ui.visuals());
        let (_, rect) = ui.allocate_space(egui::vec2(title.x + 2.0 * pad, ui.available_height()));
        let draggable = self.is_tile_draggable(tiles, tile_id);
        let sense = if draggable {
            egui::Sense::click_and_drag()
        } else {
            egui::Sense::click()
        };
        let mut tab = ui.interact(rect, id, sense);
        if draggable {
            tab = tab.on_hover_cursor(self.tab_hover_cursor_icon());
        }
        if !ui.is_rect_visible(rect) || state.is_being_dragged {
            return tab;
        }

        crate::layout::paint_tab_rules(ui, rect);
        let colour = match (state.active, tab.hovered()) {
            (true, _) => theme::RUBY,
            (false, false) => theme::DIM,
            (false, true) => egui::Color32::from_gray(210),
        };
        let text_rect = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(pad, 0.0),
            egui::vec2(title.x, rect.height()),
        );
        theme::paint_header(&ui.painter_at(rect), text_rect, name, colour);
        tab
    }

    fn simplification_options(&self) -> egui_tiles::SimplificationOptions {
        crate::layout::SIMPLIFY
    }

    fn on_edit(&mut self, action: egui_tiles::EditAction) {
        self.dropped |= action == egui_tiles::EditAction::TileDropped;
    }

    // The rest is Develop's tab styling, repeated rather than shared because the
    // trait is generic over the pane type and there is no way to inherit an impl.
    // The *values* are shared — one grey for every dividing line in the app, ruby for
    // interaction and nothing else.
    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        theme::size::HEADER + 12.0
    }

    fn tab_bar_color(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::CHROME_DEEP
    }

    fn top_bar_right_ui(
        &mut self,
        tiles: &egui_tiles::Tiles<Pane>,
        ui: &mut egui::Ui,
        _tile_id: egui_tiles::TileId,
        tabs: &egui_tiles::Tabs,
        scroll_offset: &mut f32,
    ) {
        let required: f32 = tabs
            .children
            .iter()
            .filter(|id| tiles.is_visible(**id))
            .filter_map(|id| tiles.get_pane(id))
            .map(|pane| {
                theme::header_size(ui.painter(), pane.label()).x
                    + 2.0 * self.tab_title_spacing(ui.visuals())
            })
            .sum();
        if ui.available_width() >= required {
            *scroll_offset = 0.0;
        }
    }

    fn tab_bg_color(
        &self,
        _visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<Pane>,
        _tile_id: egui_tiles::TileId,
        _state: &egui_tiles::TabState,
    ) -> egui::Color32 {
        egui::Color32::TRANSPARENT
    }

    fn tab_outline_stroke(
        &self,
        _visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<Pane>,
        _tile_id: egui_tiles::TileId,
        _state: &egui_tiles::TabState,
    ) -> egui::Stroke {
        egui::Stroke::NONE
    }

    fn tab_bar_hline_stroke(&self, _visuals: &egui::Visuals) -> egui::Stroke {
        crate::layout::panel_rule()
    }

    fn tab_text_color(
        &self,
        _visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<Pane>,
        _tile_id: egui_tiles::TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Color32 {
        if state.active {
            theme::RUBY
        } else {
            theme::DIM
        }
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        crate::layout::GAP
    }

    fn min_size(&self) -> f32 {
        crate::layout::MIN_PANE
    }
}

/// What the footer was asked for this frame.
///
/// Returned rather than applied, for the reason the tree's [`Nav`] is: the display
/// toggles live in `Settings`, which is `App`'s and has to be written to disk when
/// they change. A UI type reaching into the preferences file is how a control ends
/// up with its own private copy of a setting.
#[derive(Default)]
pub struct FooterActions {
    /// `Some(true)` to be in Lightbox, `Some(false)` for Develop.
    pub mode: Option<bool>,
    pub filenames: Option<bool>,
    pub frameless: Option<bool>,
    pub grey: Option<bool>,
    /// Something failed, and the footer's own line should carry it.
    pub note: Option<String>,
}

/// What the tree asked for this frame.
pub enum Nav {
    None,
    Open(PathBuf),
}

impl Lightbox {
    /// Search controls live beside Folders because both are ways of deciding what
    /// the one shared grid shows. The index itself owns no UI state beyond the list
    /// of locations, so closing and reopening this pane cannot interrupt its worker.
    pub fn search_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, "SEARCH");
        });
        ui.add_space(5.0);

        let mut changed = false;
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let width = (ui.available_width() - 38.0).max(60.0);
            let response = ui.add_sized(
                [width, 25.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .hint_text("Filename, path, or metadata")
                    .vertical_align(egui::Align::Center)
                    .id_source("lightbox-search-query"),
            );
            if std::mem::take(&mut self.search_focus) {
                response.request_focus();
            }
            changed |= response.changed();
            if response.has_focus()
                && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.search_query.clear();
                changed = true;
                response.surrender_focus();
            }
            if !self.search_query.is_empty() && ui.small_button("×").clicked() {
                self.search_query.clear();
                changed = true;
            }
        });

        if changed {
            if self.search_query.trim().is_empty() {
                self.restore_browse_view();
            } else {
                self.search_showing = true;
                self.search.query(&self.search_query);
            }
        } else if !self.search_showing && !self.search_query.trim().is_empty() {
            // Returning to the Search tab after browsing a result or a folder should
            // put the existing query back in the grid without requiring a keystroke.
            self.search_showing = true;
            self.search.query(&self.search_query);
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, "LOCATIONS");
        });
        ui.add_space(3.0);

        let locations = self.search.locations().to_vec();
        let mut enable = None;
        let mut remove = None;
        let mut reveal = None;
        let mut reindex = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height((ui.available_height() - 64.0).max(60.0))
            .show(ui, |ui| {
                for location in locations {
                    let mut on = location.enabled;
                    let (row, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 32.0),
                        egui::Sense::click(),
                    );

                    // Reserve the right edge before laying out the name. A drive's
                    // long volume name may use every point offered to it, but it may
                    // never push the removal control off the row.
                    const CLOSE: f32 = 14.0;
                    let close_rect = egui::Rect::from_center_size(
                        egui::pos2(row.max.x - CLOSE, row.center().y),
                        egui::Vec2::splat(CLOSE),
                    );
                    let content = egui::Rect::from_min_max(
                        row.min,
                        egui::pos2(close_rect.left() - 4.0, row.max.y),
                    );
                    let mut row_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt(("search-location", location.id))
                            .max_rect(content)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    row_ui.add_space(5.0);
                    if row_ui.checkbox(&mut on, "").changed() {
                        enable = Some((location.id, on));
                    }
                    let dot = if location.online() {
                        egui::Color32::from_rgb(78, 160, 105)
                    } else {
                        theme::DIM
                    };
                    let (r, _) =
                        row_ui.allocate_exact_size(egui::vec2(7.0, 14.0), egui::Sense::hover());
                    row_ui.painter().circle_filled(r.center(), 3.0, dot);
                    row_ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(location.name())
                                .size(10.0)
                                .color(theme::NAME),
                        );
                        let state = if location.online() {
                            "online"
                        } else {
                            "offline"
                        };
                        ui.label(
                            egui::RichText::new(format!("{} files · {state}", location.count))
                                .size(9.0)
                                .color(theme::DIM),
                        );
                    });

                    let close = ui
                        .interact(
                            close_rect,
                            ui.id().with(("search-location-x", location.id)),
                            egui::Sense::click(),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(theme::tip("Remove from Search"));
                    if close.hovered() {
                        ui.painter()
                            .rect_filled(close_rect, 2.0, egui::Color32::from_gray(72));
                    }
                    crate::icons::paint(
                        ui,
                        icons,
                        "close",
                        "×",
                        close_rect,
                        if close.hovered() {
                            egui::Color32::from_gray(245)
                        } else {
                            theme::DIM
                        },
                    );
                    if close.clicked() {
                        remove = Some(location.id);
                    }

                    let response = response.on_hover_text(location.path.display().to_string());
                    response.context_menu(|ui| {
                        if ui.button("Reindex").clicked() {
                            reindex = true;
                            ui.close();
                        }
                        if ui
                            .button(format!("Show in {}", crate::platform::file_manager_name()))
                            .clicked()
                        {
                            reveal = Some(location.path.clone());
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Remove from Search").clicked() {
                            remove = Some(location.id);
                            ui.close();
                        }
                    });
                    ui.add_space(3.0);
                }
            });

        if let Some((id, on)) = enable {
            self.search.set_enabled(id, on);
        }
        if let Some(id) = remove {
            self.search.remove_location(id);
        }
        if reindex {
            self.search.reindex();
        }
        if let Some(path) = reveal
            && let Err(err) = crate::platform::reveal(&path)
        {
            self.action_note = Some(format!(
                "Show in {}: {err}",
                crate::platform::file_manager_name()
            ));
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if ui.button("+ ADD LOCATION").clicked()
                && let Some(path) = crate::dialogs::pick_folder(self.folder.as_deref())
            {
                self.search.add_location(path);
            }
        });
        ui.add_space(5.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let status = if self.search.scanning() {
                format!("INDEXING · {} files ready", self.search.indexed_count())
            } else if self.search.querying() {
                "SEARCHING…".to_owned()
            } else if self.search.locations().is_empty() {
                "ADD A FOLDER OR DRIVE TO BEGIN".to_owned()
            } else {
                format!("UP TO DATE · {} files", self.search.indexed_count())
            };
            ui.label(egui::RichText::new(status).size(9.0).color(theme::DIM));
        });
    }

    fn poll_search(&mut self) {
        let results = self.search.poll();
        if let Some(error) = self.search.take_error() {
            self.action_note = Some(error);
        }
        let Some(results) = results else {
            return;
        };
        if results.generation == 0 || !self.search_showing || self.search_query.trim().is_empty() {
            return;
        }
        self.search_total = results.total;
        self.entries.clear();
        self.search_offline.clear();
        self.search_metadata.clear();
        for result in results.matches {
            let idx = self.entries.len() as u32;
            if !result.online {
                self.search_offline.insert(idx);
            }
            if let Some(metadata) = result.metadata {
                self.search_metadata.insert(idx, metadata);
            }
            self.entries.push(search_entry(result.path));
        }
        self.generation = self.generation.wrapping_add(1);
        self.tiles.clear();
        self.selected = None;
        self.batch.clear();
        self.manual.clear();
        self.reset_scroll = true;
        self.reindex();
    }

    fn restore_browse_view(&mut self) {
        self.search_showing = false;
        self.search_total = 0;
        self.search_offline.clear();
        self.search_metadata.clear();
        if let Some(folder) = self.folder.clone() {
            self.open_folder(&folder);
        } else {
            self.entries.clear();
            self.visible.clear();
            self.tiles.clear();
        }
    }

    /// The folder tree. Drawn by `main` into a left `SidePanel`.
    pub fn tree_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) -> Nav {
        let mut nav = Nav::None;
        // An SD card can appear after this window opened. Refresh only the small root
        // list; expanded directory listings remain cached as before.
        self.folders.refresh_roots();
        // **Inset like every other heading in this mode.** FAVORITES and EXIF both wrap
        // theirs in exactly this, and FOLDERS was the one that did not — so the word sat
        // on the pane's edge while the tree under it was indented. Same complaint
        // `layout::empty_state` already records for the Develop side, and the same fix:
        // the panel's own margin, applied to the thing that names it.
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::section(ui, "FOLDERS");
        });
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let roots = self.folders.roots.clone();
                for root in roots {
                    self.folder_row(ui, icons, &root, 0, &mut nav);
                }
            });
        nav
    }

    /// One folder and, if it is open, its children.
    ///
    /// Hand-drawn rather than `CollapsingHeader` for two reasons: the disclosure
    /// triangle and the label are separate hit targets here — clicking the name
    /// *opens* the folder in the grid, clicking the triangle only expands it, and
    /// those are different acts — and the row has to be a full-width highlight for
    /// the current folder, which a collapsing header's layout does not give.
    fn folder_row(
        &mut self,
        ui: &mut egui::Ui,
        icons: &crate::icons::Icons,
        dir: &Path,
        depth: usize,
        nav: &mut Nav,
    ) {
        let indent = 12.0 * depth as f32;
        let open = self.folders.expanded.contains(dir);
        let current = self.folder.as_deref() == Some(dir);
        let row_h = 20.0;

        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), row_h),
            egui::Sense::hover(),
        );

        // The triangle first, as its own target.
        let tri_w = 16.0;
        let tri_rect =
            egui::Rect::from_min_size(rect.min + egui::vec2(indent, 0.0), egui::vec2(tri_w, row_h));
        let tri = ui.interact(tri_rect, ui.id().with(("tri", dir)), egui::Sense::click());

        let label_rect = egui::Rect::from_min_max(egui::pos2(tri_rect.max.x, rect.min.y), rect.max);
        let hit = ui.interact(label_rect, ui.id().with(("row", dir)), egui::Sense::click());

        if current {
            ui.painter().rect_filled(rect, 2.0, theme::RUBY_FILL_DIM);
        } else if hit.hovered() || tri.hovered() {
            ui.painter().rect_filled(rect, 2.0, theme::CHROME_DEEP);
        }

        // A disclosure triangle is a shape, not an icon — the same call the rotate
        // and move cursors make. It is drawn only when there is something under it.
        let has_kids = !self.folders.children_of(dir).is_empty();
        if has_kids {
            let c = tri_rect.center();
            let r = 3.5;
            let colour = if tri.hovered() {
                theme::BRIGHT
            } else {
                theme::DIM
            };
            let pts = if open {
                vec![
                    egui::pos2(c.x - r, c.y - r * 0.6),
                    egui::pos2(c.x + r, c.y - r * 0.6),
                    egui::pos2(c.x, c.y + r * 0.8),
                ]
            } else {
                vec![
                    egui::pos2(c.x - r * 0.6, c.y - r),
                    egui::pos2(c.x + r * 0.8, c.y),
                    egui::pos2(c.x - r * 0.6, c.y + r),
                ]
            };
            ui.painter()
                .add(egui::Shape::convex_polygon(pts, colour, egui::Stroke::NONE));
        }

        // **the maintainer's folder glyph**, tinted like the name it belongs to rather than
        // drawn as a separate mark — the icon and the word are one label.
        let colour = if current { theme::BRIGHT } else { theme::NAME };
        let glyph = egui::Rect::from_center_size(
            egui::pos2(label_rect.min.x + 9.0, rect.center().y),
            egui::Vec2::splat(13.0),
        );
        crate::icons::paint_at(ui, icons, "folder", "▸", glyph, colour, 13.0);
        ui.painter().text(
            egui::pos2(label_rect.min.x + 20.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            folder_root_name(dir),
            // the maintainer: half a point down. The tree is a list you read past rather than
            // read, and it was sitting level with the headings above it.
            egui::FontId::proportional(11.5),
            colour,
        );

        if tri.clicked() {
            if open {
                self.folders.expanded.remove(dir);
            } else {
                self.folders.expanded.insert(dir.to_path_buf());
            }
        }
        // Clicking the name opens it *and* expands it — you asked to go there, and
        // arriving with the folder still shut would be a second click for nothing.
        if hit.clicked() {
            self.folders.expanded.insert(dir.to_path_buf());
            *nav = Nav::Open(dir.to_path_buf());
        }
        hit.context_menu(|ui| {
            if ui
                .button(format!("Show in {}", crate::platform::file_manager_name()))
                .clicked()
            {
                if let Err(e) = crate::platform::reveal(dir) {
                    self.action_note = Some(format!(
                        "could not show {} in {}: {e}",
                        name_of(dir),
                        crate::platform::file_manager_name()
                    ));
                }
                ui.close();
            }
        });

        if self.folders.expanded.contains(dir) {
            for kid in self.folders.children_of(dir).to_vec() {
                self.folder_row(ui, icons, &kid, depth + 1, nav);
            }
        }
    }

    /// The favorites pane: folders you keep, and the way back to them.
    ///
    /// Returns a folder to go to. **A single click, not a double** — unlike the tree,
    /// where a click both opens and expands, a favorite has nothing to expand and the
    /// only thing it can mean is "take me there".
    pub fn favorites_ui(
        &mut self,
        ui: &mut egui::Ui,
        icons: &crate::icons::Icons,
    ) -> Option<PathBuf> {
        let mut go = None;
        let here = self.folder.clone();

        // **One header row that always says the same thing.** It used to be three
        // states — add, remove, or nothing — so the control at the top of the pane
        // changed identity depending on where you happened to be standing, and the
        // word next to it changed with it. Removing a favourite is now the `×` on its
        // own row, which is where you were already looking to remove it, and this is
        // left doing one job.
        //
        // Greyed rather than hidden when there is nothing to add — no folder open, or
        // one already kept — because a control that vanishes is a control you go
        // hunting for. Same rule the Inspector's `clear` follows.
        ui.add_space(6.0);
        let addable = here.as_deref().is_some_and(|d| !self.is_favorite(d));
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let hint = match &here {
                None => "no folder open",
                Some(d) if self.is_favorite(d) => "this folder is already a favorite",
                Some(_) => "Keep this folder in favorites",
            };
            if crate::icons::sized(ui, icons, "plus", "+", addable, 16.0)
                .on_hover_text(theme::tip(hint))
                .clicked()
                && let Some(dir) = &here
            {
                self.toggle_favorite(&dir.clone());
            }
            let ink = if addable { theme::NAME } else { theme::DIM };
            ui.label(theme::caption("Add current folder").color(ink));
        });
        ui.add_space(4.0);

        if self.favorites.is_empty() {
            crate::layout::empty_state(ui, "No favorites yet\nopen a folder and keep it");
            return None;
        }

        let mut drop = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for fav in self.favorites.clone() {
                    let current = here.as_deref() == Some(fav.as_path());
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 20.0),
                        egui::Sense::hover(),
                    );
                    let r = ui.interact(rect, ui.id().with(("fav", &fav)), egui::Sense::click());

                    if current {
                        ui.painter().rect_filled(rect, 2.0, theme::RUBY_FILL_DIM);
                    } else if r.hovered() {
                        ui.painter().rect_filled(rect, 2.0, theme::CHROME);
                    }
                    // A folder that has gone is shown struck through rather than hidden:
                    // it is a favorite you made, and silently dropping it would look like
                    // the app losing your list.
                    let gone = !fav.is_dir();
                    let ink = if gone {
                        egui::Color32::from_gray(80)
                    } else if current {
                        theme::BRIGHT
                    } else {
                        theme::NAME
                    };
                    ui.painter().text(
                        egui::pos2(rect.min.x + 8.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        name_of(&fav),
                        egui::FontId::proportional(12.0),
                        ink,
                    );
                    if gone {
                        let y = rect.center().y;
                        let w = ui
                            .painter()
                            .layout_no_wrap(name_of(&fav), egui::FontId::proportional(12.0), ink)
                            .rect
                            .width();
                        ui.painter().line_segment(
                            [
                                egui::pos2(rect.min.x + 8.0, y),
                                egui::pos2(rect.min.x + 8.0 + w, y),
                            ],
                            egui::Stroke::new(1.0, ink),
                        );
                    }
                    // **The `×` on the right of the row it removes.** the maintainer's ask, and it is
                    // where the tab strip and the Inspector's pin rows already put one — so
                    // "remove this thing" is one mark in one place, rather than a mode the
                    // header was in.
                    //
                    // Sensed before the row's own hover text is attached, so clicking it
                    // cannot also open the folder underneath.
                    const CLOSE: f32 = 14.0;
                    let close_rect = egui::Rect::from_center_size(
                        egui::pos2(rect.max.x - CLOSE, rect.center().y),
                        egui::Vec2::splat(CLOSE),
                    );
                    let close = ui.interact(
                        close_rect,
                        ui.id().with(("fav-x", &fav)),
                        egui::Sense::click(),
                    );
                    if r.hovered() || close.hovered() {
                        let hot = close.hovered();
                        if hot {
                            ui.painter()
                                .rect_filled(close_rect, 2.0, egui::Color32::from_gray(72));
                        }
                        crate::icons::paint(
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
                    }
                    // Read before the response is consumed by `on_hover_text`.
                    let (took_click, over_close) = (close.clicked(), close.hovered());
                    if close
                        .on_hover_text(theme::tip("Remove from favorites"))
                        .clicked()
                    {
                        drop = Some(fav.clone());
                    }

                    let r = r.on_hover_text(theme::tip(fav.to_string_lossy()));
                    // The `×` wins the click; opening the folder you just removed would be
                    // the row doing two things with one press.
                    if r.clicked() && !gone && !took_click && !over_close {
                        go = Some(fav.clone());
                    }
                    r.context_menu(|ui| {
                        if ui.button("Remove from favorites").clicked() {
                            drop = Some(fav.clone());
                            ui.close();
                        }
                    });
                }
            });
        if let Some(d) = drop {
            self.favorites.retain(|f| *f != d);
        }
        go
    }

    fn iptc_paths(&self) -> Vec<PathBuf> {
        self.selection()
            .into_iter()
            .filter_map(|idx| self.entries.get(idx as usize))
            .filter(|entry| entry.kind != Kind::Folder)
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Commit fields that no longer own keyboard focus. This runs even while EXIF
    /// is hidden, so clicking another pane still means "leave and save".
    fn commit_blurred_iptc(&mut self, ctx: &egui::Context) {
        let focused = ctx.memory(|memory| memory.focused());
        let slots: Vec<usize> = self
            .iptc
            .as_ref()
            .map(|draft| {
                (0..IPTC_FIELDS)
                    .filter(|&i| draft.dirty[i] && draft.ids[i] != focused)
                    .collect()
            })
            .unwrap_or_default();
        self.commit_iptc_slots(&slots);
    }

    fn commit_iptc_slots(&mut self, slots: &[usize]) {
        if slots.is_empty() {
            return;
        }
        let Some(draft) = self.iptc.as_ref() else {
            return;
        };
        let paths = draft.paths.clone();
        let updates: Vec<(usize, String)> = slots
            .iter()
            .copied()
            .map(|i| (i, draft.values[i].clone()))
            .collect();
        let mut errors = Vec::new();
        let mut saved = Vec::new();

        for path in &paths {
            let (params, mut metadata) = match raw_core::sidecar::read(path) {
                raw_core::sidecar::Loaded::Ok(sidecar) => (sidecar.params, sidecar.metadata),
                raw_core::sidecar::Loaded::Absent => (Default::default(), Default::default()),
                raw_core::sidecar::Loaded::Corrupt(why) => {
                    errors.push(format!("{}: {why}", name_of(path)));
                    continue;
                }
            };
            for (i, value) in &updates {
                metadata.set_iptc(raw_core::sidecar::IptcField::ALL[*i], value.clone());
            }
            match raw_core::sidecar::write(path, &params, &metadata) {
                Ok(()) => saved.push(path.clone()),
                Err(err) => errors.push(format!("{}: {err}", name_of(path))),
            }
        }

        let search_updates: Vec<_> = updates
            .iter()
            .map(|(i, value)| (raw_core::sidecar::IptcField::ALL[*i], value.clone()))
            .collect();
        self.search.update_iptc(&saved, &search_updates);

        if let Some(draft) = self.iptc.as_mut() {
            for &i in slots {
                draft.dirty[i] = false;
                draft.mixed[i] = false;
            }
        }
        self.exif = None;
        if !errors.is_empty() {
            self.action_note = Some(format!("IPTC was not saved — {}", errors.join("; ")));
        }
    }

    fn save_iptc_templates(&mut self) {
        if let Err(error) = self.iptc_templates.save() {
            self.action_note = Some(format!("Metadata templates were not saved — {error}"));
        }
    }

    fn apply_iptc_template(&mut self, index: usize) {
        let dirty: Vec<usize> = self
            .iptc
            .as_ref()
            .map(|draft| {
                draft
                    .dirty
                    .iter()
                    .enumerate()
                    .filter_map(|(i, dirty)| dirty.then_some(i))
                    .collect()
            })
            .unwrap_or_default();
        self.commit_iptc_slots(&dirty);

        let Some(template) = self.iptc_templates.templates.get(index).cloned() else {
            return;
        };
        let paths = self.iptc_paths();
        let mut errors = Vec::new();
        let mut saved = Vec::new();
        let mut search_updates = Vec::new();
        for field in raw_core::sidecar::IptcField::ALL {
            match template.action(field) {
                Some(crate::iptc_templates::Action::Set { value }) => {
                    search_updates.push((field, value.clone()));
                }
                Some(crate::iptc_templates::Action::Clear) => {
                    search_updates.push((field, String::new()));
                }
                None => {}
            }
        }
        for path in &paths {
            let (params, mut metadata) = match raw_core::sidecar::read(path) {
                raw_core::sidecar::Loaded::Ok(sidecar) => (sidecar.params, sidecar.metadata),
                raw_core::sidecar::Loaded::Absent => (Default::default(), Default::default()),
                raw_core::sidecar::Loaded::Corrupt(why) => {
                    errors.push(format!("{}: {why}", name_of(path)));
                    continue;
                }
            };
            for field in raw_core::sidecar::IptcField::ALL {
                match template.action(field) {
                    Some(crate::iptc_templates::Action::Set { value }) => {
                        metadata.set_iptc(field, value.clone());
                    }
                    Some(crate::iptc_templates::Action::Clear) => {
                        metadata.set_iptc(field, String::new());
                    }
                    None => {}
                }
            }
            match raw_core::sidecar::write(path, &params, &metadata) {
                Ok(()) => saved.push(path.clone()),
                Err(error) => errors.push(format!("{}: {error}", name_of(path))),
            }
        }
        self.search.update_iptc(&saved, &search_updates);

        self.iptc = Some(IptcDraft::load(paths));
        self.exif = None;
        self.action_note = if errors.is_empty() {
            Some(format!("Applied metadata template “{}”", template.name))
        } else {
            Some(format!(
                "Metadata template was not applied to every image — {}",
                errors.join("; ")
            ))
        };
    }

    fn iptc_template_controls(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = self
            .iptc_templates
            .templates
            .iter()
            .map(|template| template.name.clone())
            .collect();
        if self
            .iptc_template_selected
            .is_some_and(|index| index >= names.len())
        {
            self.iptc_template_selected = None;
        }
        let selected_text = self
            .iptc_template_selected
            .and_then(|index| names.get(index))
            .map(String::as_str)
            .unwrap_or("Choose template");
        let mut picked = None;
        let mut apply = false;
        let mut save_current = false;
        let mut edit = false;
        let mut rename = false;
        let mut delete = false;

        ui.horizontal(|ui| {
            ui.add_space(8.0);
            // Keep the row inside a docked Metadata pane even with its scrollbar
            // showing. Apply, the menu, their gaps and the right inset together need
            // a little more than the old 92-point allowance.
            ui.spacing_mut().item_spacing.x = 5.0;
            egui::ComboBox::from_id_salt("lightbox-iptc-template")
                .selected_text(selected_text)
                .width((ui.available_width() - 108.0).max(76.0))
                .show_ui(ui, |ui| {
                    if names.is_empty() {
                        ui.label(theme::caption("No saved templates").color(theme::DIM));
                    }
                    for (index, name) in names.iter().enumerate() {
                        if ui
                            .selectable_label(self.iptc_template_selected == Some(index), name)
                            .clicked()
                        {
                            picked = Some(index);
                            ui.close();
                        }
                    }
                });
            apply = ui
                .add_enabled(
                    self.iptc_template_selected.is_some(),
                    egui::Button::new("Apply"),
                )
                .on_hover_text(theme::tip("Apply to the selected image or images"))
                .clicked();
            ui.menu_button("⋯", |ui| {
                if ui.button("Save Current as Template…").clicked() {
                    save_current = true;
                    ui.close();
                }
                ui.add_enabled_ui(self.iptc_template_selected.is_some(), |ui| {
                    if ui.button("Edit Template…").clicked() {
                        edit = true;
                        ui.close();
                    }
                    if ui.button("Rename…").clicked() {
                        rename = true;
                        ui.close();
                    }
                    if ui.button("Delete").clicked() {
                        delete = true;
                        ui.close();
                    }
                });
            });
        });

        if let Some(index) = picked {
            self.iptc_template_selected = Some(index);
        }
        if apply && let Some(index) = self.iptc_template_selected {
            self.apply_iptc_template(index);
        }
        if save_current {
            self.iptc_template_name = Some(TemplateNameDialog {
                purpose: TemplateNamePurpose::SaveCurrent,
                value: String::new(),
                request_focus: true,
            });
        }
        if edit {
            self.iptc_template_editor = true;
        }
        if rename
            && let Some(index) = self.iptc_template_selected
            && let Some(template) = self.iptc_templates.templates.get(index)
        {
            self.iptc_template_name = Some(TemplateNameDialog {
                purpose: TemplateNamePurpose::Rename(index),
                value: template.name.clone(),
                request_focus: true,
            });
        }
        if delete
            && let Some(index) = self.iptc_template_selected
            && index < self.iptc_templates.templates.len()
        {
            self.iptc_templates.templates.remove(index);
            self.iptc_template_selected = None;
            self.iptc_template_editor = false;
            self.save_iptc_templates();
        }
    }

    fn iptc_template_name_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.iptc_template_name.take() else {
            return;
        };
        let title = match dialog.purpose {
            TemplateNamePurpose::SaveCurrent => "Save Metadata Template",
            TemplateNamePurpose::Rename(_) => "Rename Metadata Template",
        };
        let mut open = true;
        let mut accept = false;
        let mut cancel = false;
        egui::Window::new(title)
            .id(egui::Id::new("iptc-template-name-dialog"))
            .collapsible(false)
            .resizable(false)
            .default_width(320.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(theme::caption("Template name").color(theme::DIM));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut dialog.value)
                        .desired_width(f32::INFINITY)
                        .vertical_align(egui::Align::Center)
                        .id_source("iptc-template-name"),
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
            match dialog.purpose {
                TemplateNamePurpose::SaveCurrent => {
                    let name = self.iptc_templates.unique_name(&dialog.value, None);
                    let (values, mixed) = self
                        .iptc
                        .as_ref()
                        .map(|draft| (draft.values.clone(), draft.mixed))
                        .unwrap_or_default();
                    let template =
                        crate::iptc_templates::Template::from_values(name, &values, &mixed);
                    self.iptc_templates.templates.push(template);
                    self.iptc_template_selected =
                        Some(self.iptc_templates.templates.len().saturating_sub(1));
                    self.save_iptc_templates();
                }
                TemplateNamePurpose::Rename(index) => {
                    let name = self.iptc_templates.unique_name(&dialog.value, Some(index));
                    if let Some(template) = self.iptc_templates.templates.get_mut(index) {
                        template.name = name;
                        self.save_iptc_templates();
                    }
                }
            }
        } else if open && !cancel {
            self.iptc_template_name = Some(dialog);
        }
    }

    fn iptc_template_editor(&mut self, ctx: &egui::Context) {
        if !self.iptc_template_editor {
            return;
        }
        let Some(index) = self.iptc_template_selected else {
            self.iptc_template_editor = false;
            return;
        };
        let Some(template) = self.iptc_templates.templates.get(index) else {
            self.iptc_template_editor = false;
            return;
        };
        let title = format!("Edit Metadata Template — {}", template.name);
        let mut modes: [TemplateFieldMode; IPTC_FIELDS] =
            std::array::from_fn(
                |i| match template.action(raw_core::sidecar::IptcField::ALL[i]) {
                    Some(crate::iptc_templates::Action::Set { .. }) => TemplateFieldMode::Set,
                    Some(crate::iptc_templates::Action::Clear) => TemplateFieldMode::Clear,
                    None => TemplateFieldMode::Unchanged,
                },
            );
        let mut values: [String; IPTC_FIELDS] =
            std::array::from_fn(
                |i| match template.action(raw_core::sidecar::IptcField::ALL[i]) {
                    Some(crate::iptc_templates::Action::Set { value }) => value.clone(),
                    _ => String::new(),
                },
            );
        let current = self
            .iptc
            .as_ref()
            .map(|draft| (draft.values.clone(), draft.mixed));
        let mut open = true;
        let mut changed = false;
        egui::Window::new(title)
            .id(egui::Id::new("iptc-template-editor"))
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    theme::caption(
                        "Leave unchanged preserves each image’s existing value. Clear existing removes it.",
                    )
                    .color(theme::DIM),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .max_height(480.0)
                    .show(ui, |ui| {
                        for (i, field) in
                            raw_core::sidecar::IptcField::ALL.into_iter().enumerate()
                        {
                            ui.horizontal(|ui| {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(118.0, 24.0),
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        ui.label(theme::caption(field.label()).color(theme::DIM));
                                    },
                                );
                                let before = modes[i];
                                egui::ComboBox::from_id_salt(("iptc-template-field", field.key()))
                                    .selected_text(modes[i].label())
                                    .width(118.0)
                                    .show_ui(ui, |ui| {
                                        for mode in TemplateFieldMode::ALL {
                                            ui.selectable_value(&mut modes[i], mode, mode.label());
                                        }
                                    });
                                if before != modes[i] {
                                    changed = true;
                                    if modes[i] == TemplateFieldMode::Set
                                        && values[i].is_empty()
                                        && let Some((current_values, mixed)) = &current
                                        && !mixed[i]
                                    {
                                        values[i] = current_values[i].clone();
                                    }
                                }
                                if modes[i] == TemplateFieldMode::Set {
                                    changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(&mut values[i])
                                                .desired_width(f32::INFINITY)
                                                .vertical_align(egui::Align::Center),
                                        )
                                        .changed();
                                }
                            });
                            ui.add_space(3.0);
                        }
                    });
            });
        self.iptc_template_editor = open;

        if changed {
            if let Some(template) = self.iptc_templates.templates.get_mut(index) {
                for (i, field) in raw_core::sidecar::IptcField::ALL.into_iter().enumerate() {
                    let action = match modes[i] {
                        TemplateFieldMode::Unchanged => None,
                        TemplateFieldMode::Set => Some(crate::iptc_templates::Action::Set {
                            value: values[i].clone(),
                        }),
                        TemplateFieldMode::Clear => Some(crate::iptc_templates::Action::Clear),
                    };
                    template.set_action(field, action);
                }
            }
            self.save_iptc_templates();
        }
    }

    /// Camera and file facts remain read-only; IPTC beneath them is an authored
    /// sidecar overlay. A multi-selection edits only the field that was touched.
    pub fn exif_ui(&mut self, ui: &mut egui::Ui) {
        let Some(path) = self
            .selected
            .and_then(|i| self.entries.get(i as usize))
            .map(|e| e.path.clone())
        else {
            crate::layout::empty_state(ui, "Select a frame");
            return;
        };

        if self.exif.as_ref().is_none_or(|(p, _)| *p != path) {
            self.exif = Some((path.clone(), read_exif(&path)));
        }

        let paths = self.iptc_paths();
        if self
            .iptc
            .as_ref()
            .is_some_and(|draft| draft.paths != paths && draft.dirty.iter().any(|dirty| *dirty))
        {
            let slots: Vec<usize> = self
                .iptc
                .as_ref()
                .map(|draft| {
                    draft
                        .dirty
                        .iter()
                        .enumerate()
                        .filter_map(|(i, dirty)| dirty.then_some(i))
                        .collect()
                })
                .unwrap_or_default();
            self.commit_iptc_slots(&slots);
        }
        if self.iptc.as_ref().is_none_or(|draft| draft.paths != paths) {
            self.iptc = Some(IptcDraft::load(paths));
        }

        let sections = self
            .exif
            .as_ref()
            .map(|(_, sections)| sections.clone())
            .unwrap_or_default();
        let selected_count = self
            .iptc
            .as_ref()
            .map(|draft| draft.paths.len())
            .unwrap_or(0);
        let mut left_fields = Vec::new();

        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let name = if selected_count > 1 {
                        format!("{selected_count} SELECTED")
                    } else {
                        name_of(&path)
                    };
                    ui.label(
                        theme::readout(name)
                            .size(theme::size::BODY - 1.0)
                            .color(theme::NAME),
                    );
                });
                ui.add_space(4.0);
                if sections.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(theme::caption("this file records nothing").color(theme::DIM));
                    });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    theme::section(ui, "EXIF");
                });
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    egui::Grid::new("lightbox-exif-rows")
                        .num_columns(2)
                        .spacing([10.0, 3.0])
                        .show(ui, |ui| {
                            for (key, value) in sections.iter().flat_map(|section| &section.rows) {
                                crate::info_row(ui, key, value.clone());
                            }
                        });
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    theme::section(ui, "IPTC");
                    if selected_count > 1 {
                        ui.label(
                            theme::caption(format!("applies to {selected_count} selected"))
                                .color(theme::DIM),
                        );
                    }
                });
                ui.add_space(3.0);
                self.iptc_template_controls(ui);
                ui.add_space(6.0);
                let Some(draft) = self.iptc.as_mut() else {
                    return;
                };
                for (i, field) in raw_core::sidecar::IptcField::ALL.into_iter().enumerate() {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);
                            ui.label(theme::caption(field.label()).color(theme::DIM));
                        });
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);
                            // Match the left inset on the right, plus a little room
                            // for the vertical scrollbar so the field stroke never
                            // disappears under the pane edge.
                            let width = (ui.available_width() - 18.0).max(72.0);
                            let edit = if field.multiline() {
                                egui::TextEdit::multiline(&mut draft.values[i]).desired_rows(3)
                            } else {
                                egui::TextEdit::singleline(&mut draft.values[i])
                                    .vertical_align(egui::Align::Center)
                            }
                            .desired_width(width)
                            .hint_text(if draft.mixed[i] {
                                "Multiple values"
                            } else {
                                ""
                            })
                            .id_source(("lightbox-iptc", field.key()));
                            let response = ui.add_sized(
                                [width, if field.multiline() { 54.0 } else { 24.0 }],
                                edit,
                            );
                            draft.ids[i] = Some(response.id);
                            if response.changed() {
                                draft.dirty[i] = true;
                                draft.mixed[i] = false;
                            }
                            if response.lost_focus() && draft.dirty[i] {
                                left_fields.push(i);
                            }
                        });
                        ui.add_space(4.0);
                    });
                }
            });
        self.commit_iptc_slots(&left_fields);
        self.iptc_template_name_dialog(ui.ctx());
        self.iptc_template_editor(ui.ctx());
    }

    /// Lightbox's browsing controls, grouped in the order their meaning unfolds:
    /// include/rename, arrange, then filter.
    ///
    /// **A menu for the sort and marks for the filters**, which is not an arbitrary
    /// split: the sort is one choice out of seven and only its current value matters,
    /// where the filters are four independent switches whose *combination* is what
    /// you need to see at a glance. Seven chips would take the width of the bar to
    /// say one word, and a filter behind a menu is a filter you forget is on.
    fn footer_controls_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) {
        // Subfolders changes what is *listed*, so it re-reads the folder rather than
        // re-filtering what is already in hand.
        if crate::icons::toggle(ui, icons, "tree-view", "⌸", self.filters.subfolders, 18.0)
            .on_hover_text(theme::tip("Include subfolders"))
            .clicked()
        {
            self.filters.subfolders = !self.filters.subfolders;
            self.reopen();
        }

        let any = !self.selection().is_empty();
        if crate::icons::sized(ui, icons, "text-aa", "Aa", any, 18.0)
            .on_hover_text(theme::tip("Rename selected files · ⇧⌘R"))
            .clicked()
        {
            self.begin_rename();
        }

        let has_pictures = self.visible.iter().any(|index| {
            self.entries
                .get(*index as usize)
                .is_some_and(|entry| entry.kind == Kind::Picture)
        });
        if crate::icons::sized(ui, icons, "file-pdf", "PDF", has_pictures, 18.0)
            .on_hover_text(theme::tip("Contact sheet PDF · ⇧⌘P"))
            .clicked()
        {
            self.begin_contact_sheet();
        }

        ui.separator();

        ui.menu_button(
            egui::RichText::new(self.sort.label())
                .size(8.0)
                .color(theme::NAME),
            |ui| {
                for mode in Sort::ALL {
                    if ui
                        .selectable_label(self.sort == mode, mode.label())
                        .clicked()
                    {
                        self.set_sort(mode);
                        ui.close();
                    }
                }
            },
        )
        .response
        .on_hover_text(theme::tip("Sort order"));

        let glyph = if self.descending {
            "sort-descending"
        } else {
            "sort-ascending"
        };
        if crate::icons::sized(ui, icons, glyph, "↕", true, 18.0)
            .on_hover_text(theme::tip(if self.descending {
                "Descending"
            } else {
                "Ascending"
            }))
            .clicked()
        {
            self.set_descending(!self.descending);
        }

        ui.separator();

        // **Unrated and a star threshold cannot both be on.** They are `rating == 0`
        // and `rating >= n`, so the pair asks for nothing at all. Rather than let you
        // build an empty view and wonder, choosing one clears the other.
        if theme::bracket(ui, "UNRATED", self.filters.unrated, 8.0)
            .on_hover_text(theme::tip("Only files with no rating"))
            .clicked()
        {
            self.filters.unrated = !self.filters.unrated;
            if self.filters.unrated {
                self.filters.stars = 0;
            }
            self.refilter();
        }

        // The threshold, as five stars you click. Reads the same way as the row on a
        // tile, which is the point: the same mark means the same thing in both places,
        // and here it means "this many or more".
        let dim = self.filters.unrated;
        for n in 1..=5i32 {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(12.0, ui.available_height()),
                egui::Sense::hover(),
            );
            let hit = ui.interact(rect, ui.id().with(("fstar", n)), egui::Sense::click());
            let on = !dim && n <= self.filters.stars;
            let ink = if dim {
                egui::Color32::from_gray(54)
            } else if on {
                theme::AMBER
            } else if hit.hovered() {
                theme::DIM
            } else {
                EMPTY_STAR
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                if on { "★" } else { "☆" },
                egui::FontId::proportional(10.0),
                ink,
            );
            if hit.clicked() && !dim {
                // The star you are already on turns the filter off, so the control
                // that set it is the control that clears it.
                self.filters.stars = if self.filters.stars == n { 0 } else { n };
                self.refilter();
            }
        }

        // A dot per label, and any of them may be on at once — a colour filter is
        // naturally "show me these", not "show me this one".
        for (name, colour) in theme::LABELS {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(14.0, ui.available_height()),
                egui::Sense::hover(),
            );
            let hit = ui.interact(rect, ui.id().with(("flabel", name)), egui::Sense::click());
            let on = self.filters.labels.iter().any(|l| l == name);
            let c = rect.center();
            if on {
                ui.painter().circle_filled(c, 5.0, colour);
            } else {
                ui.painter().circle_stroke(
                    c,
                    4.5,
                    egui::Stroke::new(
                        1.0,
                        if hit.hovered() {
                            colour
                        } else {
                            colour.gamma_multiply(0.45)
                        },
                    ),
                );
            }
            if hit.clicked() {
                if on {
                    self.filters.labels.retain(|l| l != name);
                } else {
                    self.filters.labels.push(name.to_owned());
                }
                self.refilter();
            }
        }

        // One way back to the whole folder, and it only appears when there is
        // something to undo. A filter you cannot clear in one gesture is a filter you
        // clear by restarting.
        if self.filters.any() {
            let subfolders = self.filters.subfolders;
            if crate::icons::sized(ui, icons, "funnel", "▽", true, 18.0)
                .on_hover_text(theme::tip("Clear the filters"))
                .clicked()
            {
                self.filters = Filters::default();
                if subfolders {
                    self.reopen();
                } else {
                    self.refilter();
                }
            }
        }
    }

    /// The footer line while the mode is up.
    ///
    /// Where it is, how much is in it, and how to leave. The Develop footer's
    /// readouts are all about one picture and none of them survive the crossing.
    ///
    /// Returns what was asked for rather than doing it — see [`FooterActions`].
    pub fn footer_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) -> FooterActions {
        let mut act = FooterActions {
            note: self.action_note.take(),
            ..Default::default()
        };
        ui.horizontal(|ui| {
            if self.search_showing {
                let selected = self.selected;
                let path = self
                    .selected
                    .and_then(|idx| self.entries.get(idx as usize))
                    .map(|entry| entry.path.display().to_string());
                let metadata = selected.and_then(|idx| self.search_metadata.get(&idx));
                let text = path.as_deref().unwrap_or("SEARCH · select a result");
                // A search result has no single enclosing folder, so its full path
                // is the location context. Keep the footer stable on long paths and
                // put the unabridged value on hover.
                let width = (ui.available_width() * 0.34).clamp(180.0, 520.0);
                let response = ui.add_sized(
                    [width, ui.available_height()],
                    egui::Label::new(theme::footer_label(text)).truncate(),
                );
                if let Some(path) = path {
                    response.on_hover_text(theme::footer_label(path));
                }
                ui.separator();
                if let Some(metadata) = metadata {
                    let text = format!("MATCH · {metadata}");
                    ui.add_sized(
                        [
                            (ui.available_width() * 0.22).clamp(140.0, 320.0),
                            ui.available_height(),
                        ],
                        egui::Label::new(theme::footer_caption(&text)).truncate(),
                    )
                    .on_hover_text(theme::footer_label(text));
                    ui.separator();
                }
            } else {
                match &self.folder {
                    Some(dir) => {
                        let label = footer_folder_path(dir);
                        ui.label(theme::footer_label(label))
                            .on_hover_text(theme::footer_label(dir.display().to_string()));
                        ui.separator();
                    }
                    None => {
                        ui.label(theme::footer_caption("no folder").color(theme::DIM));
                        ui.separator();
                    }
                }
            }

            // **Fixed width, so the bar does not jump.** The prototype's note at
            // `monopro.py:25654`: a count that reflows the line every time a
            // thumbnail lands makes the whole footer twitch while a folder fills.
            let n = if self.search_showing {
                self.search_total.max(self.entries.len())
            } else {
                self.entries.len()
            };
            let shown = self.visible.len();
            let outstanding = self.queue.in_flight();
            // **What a filter hides is said, not left to be noticed.** "12 of 340" is
            // the difference between a filter and a folder that turned out to be
            // nearly empty, and it is the only readout that can tell you which.
            // **A multi-selection outranks the filter here.** Both want to say "N of
            // T", and while several tiles are ringed the question you are asking is how
            // many you have picked. One selected is not a selection worth reporting —
            // there is always exactly one — so that falls back to the folder.
            let picked = self.batch.len();
            let count = match (shown, n) {
                (_, 0) => "no files".to_owned(),
                _ if picked > 1 => format!("{picked} of {n} files"),
                (s, t) if s == t && t == 1 => "1 file".to_owned(),
                (s, t) if s == t => format!("{t} files"),
                (s, t) => format!("{s} of {t} files"),
            };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(88.0, ui.available_height()),
                egui::Sense::hover(),
            );
            ui.painter().text(
                egui::pos2(rect.min.x, rect.center().y),
                egui::Align2::LEFT_CENTER,
                count,
                egui::FontId::proportional(10.0),
                theme::DIM,
            );

            if outstanding > 0 {
                ui.separator();
                ui.label(theme::footer_caption(format!("{outstanding} loading")));
            }

            if !self.search_showing {
                ui.separator();
                self.footer_controls_ui(ui, icons);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // The same slot the Develop footer puts its own name in, so the one
                // thing that changed when you pressed `l` changed in place.
                act.mode = mode_tabs(ui, true);
                // No size readout. It was added to stop the footer lying about the
                // card width, and the maintainer's answer at the screen was that the honest
                // version of a number nobody needs is no number: the grid is in
                // front of you and you can see how big it is.
                ui.separator();

                // **The two display toggles**, the prototype's pair. Frameless swaps
                // its glyph rather than only its tint: the state it reports *is* a
                // border, so a dashed rectangle says "off" in the shape of the thing
                // being turned off. Filenames has no such shape and only tints.
                let frameless_glyph = if self.frameless {
                    "rectangle-dashed"
                } else {
                    "rectangle"
                };
                if crate::icons::toggle(ui, icons, frameless_glyph, "□", self.frameless, 18.0)
                    .on_hover_text(theme::tip("Frameless tiles — hide the border"))
                    .clicked()
                {
                    act.frameless = Some(!self.frameless);
                }
                if crate::icons::toggle(ui, icons, "article", "≡", self.show_filenames, 18.0)
                    .on_hover_text(theme::tip("Filenames under the tiles"))
                    .clicked()
                {
                    act.filenames = Some(!self.show_filenames);
                }
                // **A word rather than an icon**, which is the prototype's choice
                // (`monopro.py:25766`) and the right one: the button reports which of
                // two states you are in, and no glyph says "grey" as plainly as the
                // word does. Named for what you are looking at now, not for what
                // pressing it would do.
                // **The size slider.** `⌘+` / `⌘−` walk the same ladder; this is the
                // control they are a shortcut for. A slider over the *rung* rather
                // than over points, because the rungs are the only sizes there are.
                let mut rung = self.size as f32;
                let top = (SIZES.len() - 1) as f32;
                let slider = crate::widgets::bare_slider(
                    ui,
                    &mut rung,
                    DEFAULT_SIZE as f32,
                    0.0..=top,
                    84.0,
                    Some(SIZES.len()),
                );
                let want = (rung.round() as usize).min(SIZES.len() - 1);
                if want != self.size {
                    self.size = want;
                }
                slider.on_hover_text(theme::tip("Tile size · double-click to reset"));
                ui.separator();

                // **Rotate the selection, for display only** — Lightbox changes no
                // pixel, and this writes a turn count the grid draws by. The same
                // mirrored pair Composition uses: one operation in two directions, and
                // a second set of glyphs would read as a different operation.
                // **Right is added first**, which puts it on the right: this is a
                // right-to-left layout, so the order here is the reverse of the order
                // on screen. Added the other way round the arrows pointed *at* each
                // other, where Composition's pair points away — the mirrored glyphs
                // only read as one control in two directions if they are the way
                // round the eye expects.
                let any = !self.selection().is_empty();
                if crate::icons::sized(ui, icons, "rotate-right", "↻", any, 18.0)
                    .on_hover_text(theme::tip("Rotate the selection right, for display"))
                    .clicked()
                {
                    act.note = self.rotate(true);
                }
                if crate::icons::sized(ui, icons, "rotate-left", "↺", any, 18.0)
                    .on_hover_text(theme::tip("Rotate the selection left, for display"))
                    .clicked()
                {
                    act.note = self.rotate(false);
                }
                ui.separator();

                // **American, like every other label.** That is settled:
                // the user reads "color" and "gray", while identifiers, persistence
                // keys and comments keep the British form — which is why `self.grey`
                // and `theme::label_colour` are spelt the way they are two lines from
                // a button that is not.
                if theme::bracket(ui, if self.grey { "GRAY" } else { "COLOR" }, self.grey, 8.0)
                    .on_hover_text(theme::tip("Show the thumbnails as gray"))
                    .clicked()
                {
                    act.grey = Some(!self.grey);
                }
            });
        });
        act
    }

    /// The grid. Drawn by `main` into the central panel.
    ///
    /// Returns a file to open in Develop when one is double-clicked. Returned rather
    /// than acted on for the reason the tree's `Nav` is: opening a file is `App`'s,
    /// and this type has no business knowing how a tab is made.
    pub fn grid_ui(&mut self, ui: &mut egui::Ui, icons: &crate::icons::Icons) -> Option<PathBuf> {
        self.frame = self.frame.wrapping_add(1);
        let mut open = None;

        if self.folder.is_none() {
            crate::layout::empty_state(ui, "⌘O  Open a folder\nor browse the folder panel");
            return None;
        }
        if self.entries.is_empty() {
            crate::layout::empty_state(ui, "No raw files in this folder");
            return None;
        }
        if self.visible.is_empty() {
            // Distinguished from an empty folder, because the fix is different: one
            // is "there is nothing here" and the other is "you asked to see none of
            // it", and a view that cannot tell you which looks broken.
            crate::layout::empty_state(ui, "No files match the filter");
            return None;
        }

        let furniture_h = if self.frameless { 0.0 } else { META_ROW_H }
            + if self.show_filenames { NAME_ROW_H } else { 0.0 };
        let (cols, tile_w, tile_h) = grid_metrics(ui.available_width(), self.rung(), furniture_h);
        self.last_avail = ui.available_width();
        // **Keep your place when the shape changes.** The anchor is the selected tile
        // if there is one and the first visible tile otherwise — either way it is what
        // you were looking at, and scrolling to it after a reflow is the difference
        // between resizing the grid and being thrown to the end of it.
        if cols != self.last_cols {
            if self.last_cols != 0 {
                self.follow = true;
                if self.selected.is_none() {
                    self.selected = self.visible.first().copied();
                }
            }
            self.last_cols = cols;
        }
        let cell_w = tile_w + SPACING;
        let cell_h = tile_h + SPACING;

        let rows = self.visible.len().div_ceil(cols);

        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.reset_scroll) {
            scroll = scroll.vertical_scroll_offset(0.0);
        }

        let pointer = ui.input(|input| input.pointer.latest_pos());
        let background_clicked = ui.input(|input| input.pointer.primary_clicked());
        let mut over_tile = false;
        let scroll_output = scroll.show_viewport(ui, |ui, viewport| {
            ui.set_height(rows as f32 * cell_h + 2.0 * MARGIN);
            let top = ui.min_rect().min;

            // **Only the visible band is drawn.** This is what makes the folder size
            // stop mattering: the work per frame is a screenful, not a directory.
            let first_row = ((viewport.min.y / cell_h).floor().max(0.0)) as usize;
            let last_row = (((viewport.max.y / cell_h).ceil()) as usize).min(rows);

            let follow = std::mem::take(&mut self.follow);
            // **The tile being scrolled to has to be drawn to be scrolled to.** When
            // the selection has just moved by keyboard it may be outside the visible
            // band entirely — a page away, if you held an arrow — so the band is
            // stretched to include it for this one frame.
            let mut first_row = first_row;
            let mut last_row = last_row;
            if follow
                && let Some(sel) = self.selected
                && let Some(slot) = self.visible.iter().position(|i| *i == sel)
            {
                let row = slot / cols;
                first_row = first_row.min(row);
                last_row = last_row.max(row + 1);
            }
            let want_first = first_row.saturating_sub(OVERSCAN_ROWS);
            let want_last = (last_row + OVERSCAN_ROWS).min(rows);

            // `slot` is the position on screen; `idx` is which entry sits there. The
            // two are only equal under an unfiltered filename sort, and keeping them
            // apart is what lets a thumbnail survive a change of order.
            for row in want_first..want_last {
                for col in 0..cols {
                    let slot = row * cols + col;
                    let Some(idx) = self.visible.get(slot).copied() else {
                        break;
                    };

                    // Visible tiles jump the queue; the overscan waits behind them.
                    let priority = if (first_row..last_row).contains(&row) {
                        decode::FOREGROUND
                    } else {
                        decode::BACKGROUND
                    };
                    self.request(idx, priority);

                    if row < first_row || row >= last_row {
                        continue; // requested, but off screen — nothing to paint
                    }

                    let pos = top
                        + egui::vec2(MARGIN + col as f32 * cell_w, MARGIN + row as f32 * cell_h);
                    let rect = egui::Rect::from_min_size(pos, egui::vec2(tile_w, tile_h));
                    over_tile |= pointer.is_some_and(|position| rect.contains(position));
                    if let Some(p) = self.tile_ui(ui, icons, rect, idx, slot) {
                        open = Some(p);
                    }
                    // **The grid follows the keyboard.** Walking off the bottom of the
                    // screen and losing the ring is the arrow keys working and the view
                    // not — so the selected tile is scrolled into sight on the frame it
                    // moves, and only then.
                    if follow && self.follow_target() == Some(idx) {
                        if cols <= 2 {
                            // At one or two across the tile is a viewing surface, not
                            // a contact-sheet thumbnail. Put the whole card in view and
                            // align its top with the viewer so each arrow press lands in
                            // the same place instead of merely exposing an edge.
                            ui.scroll_to_rect(rect, Some(egui::Align::Min));
                        } else {
                            ui.scroll_to_rect(rect.expand(cell_h * 0.5), None);
                        }
                    }
                }
            }

            // Everything outside the band is work nobody is waiting for. Cancelling
            // rather than letting it drain is what keeps a fling through a large
            // folder responsive. The band is in slots, so it is turned back into
            // entry indices before anything is dropped.
            let keep: HashSet<u32> = self
                .visible
                .get(want_first * cols..(want_last * cols).min(self.visible.len()))
                .unwrap_or(&[])
                .iter()
                .copied()
                .collect();
            self.cancel_outside(&keep);
        });

        // Empty grid space is a real target: it means "nothing selected". This is
        // useful in its own right and lets a manually arranged contact sheet be
        // captured without a ruby selection ring. Tile rectangles win above, and
        // `inner_rect` excludes the scrollbar so dragging it never clears selection.
        if background_clicked
            && pointer.is_some_and(|position| scroll_output.inner_rect.contains(position))
            && !over_tile
        {
            self.clear_selection();
        }

        // A drag that ended outside any tile, or on the one it started from, still
        // has to be cleared or the insertion mark would stay on screen.
        if self.drag.is_some()
            && !ui.input(|i| i.pointer.any_down())
            && let Some((dragged, before)) = self.drag.take()
        {
            self.reorder(dragged, before);
        }

        // Now that nothing is holding an index into `entries`, a double-clicked folder
        // can replace the list. Before `evict`, so the outgoing folder's textures are
        // the ones dropped rather than the new folder's first screenful.
        if let Some(dir) = self.enter.take() {
            self.open_folder(&dir);
        }

        self.evict();
        open
    }

    /// Drop requests for tiles that have scrolled away.
    fn cancel_outside(&mut self, keep: &HashSet<u32>) {
        let stale: Vec<u32> = self
            .tiles
            .iter()
            .filter(|(i, t)| matches!(t, Tile::Pending) && !keep.contains(i))
            .map(|(i, _)| *i)
            .collect();
        for idx in stale {
            self.queue.cancel(Key {
                generation: self.generation,
                idx,
                full: false,
            });
            // Forget it, so scrolling back asks again. A cancelled job that is
            // already running still delivers, and `collect` will take it — this only
            // stops the ones that never started.
            self.tiles.remove(&idx);
        }
    }

    /// One card: the picture, what is known about it, and what it does when clicked.
    ///
    /// Returns a path when the card is double-clicked, which is the gesture that
    /// takes a file into Develop. Single click selects — the two are the same pair
    /// the tab strip and the folder tree already use, so nothing new is being taught.
    fn tile_ui(
        &mut self,
        ui: &mut egui::Ui,
        icons: &crate::icons::Icons,
        card: egui::Rect,
        idx: u32,
        slot: usize,
    ) -> Option<PathBuf> {
        let mut open = None;
        let r = ui.interact(
            card,
            ui.id().with(("tile", idx)),
            egui::Sense::click_and_drag(),
        );

        if r.clicked() {
            // **`⌘` adds one, `⇧` takes a run, a plain click starts again** — the
            // gestures every file browser has. The anchor is what a run is measured
            // from, which is why it is kept apart from the batch.
            let (cmd, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
            if shift && let Some(anchor) = self.selected {
                let a = self.visible.iter().position(|i| *i == anchor);
                let b = self.visible.iter().position(|i| *i == idx);
                if let (Some(a), Some(b)) = (a, b) {
                    self.batch = self.visible[a.min(b)..=a.max(b)].iter().copied().collect();
                }
                // The clicked end is where a following `⇧`+arrow carries on from, so the
                // pointer and the keyboard extend one selection rather than two.
                self.head = Some(idx);
            } else if cmd {
                // The anchor joins the batch the first time you extend it, or
                // `⌘`-clicking a second tile would silently drop the first.
                if self.batch.is_empty()
                    && let Some(a) = self.selected
                {
                    self.batch.insert(a);
                }
                if !self.batch.remove(&idx) {
                    self.batch.insert(idx);
                }
                self.selected = Some(idx);
                self.head = None;
            } else {
                self.batch.clear();
                self.selected = Some(idx);
                self.head = None;
            }
        }

        // **Drag to reorder.** Which half of the tile the pointer is over decides
        // whether the dragged file lands before or after it, so a drop between two
        // tiles means what it looks like.
        if r.drag_started() {
            self.selected = Some(idx);
            self.drag = Some((idx, slot));
        }
        if let Some((dragged, _)) = self.drag
            && dragged != idx
            && r.contains_pointer()
        {
            let after = ui
                .input(|i| i.pointer.latest_pos())
                .is_some_and(|p| p.x > card.center().x);
            self.drag = Some((dragged, if after { slot + 1 } else { slot }));
        }
        // The insertion mark, on the edge the drop would land against.
        if let Some((dragged, before)) = self.drag
            && dragged != idx
            && (before == slot || before == slot + 1)
        {
            let x = if before == slot {
                card.min.x - SPACING * 0.5
            } else {
                card.max.x + SPACING * 0.5
            };
            ui.painter().line_segment(
                [egui::pos2(x, card.min.y), egui::pos2(x, card.max.y)],
                egui::Stroke::new(2.0, theme::RUBY),
            );
        }
        if r.drag_stopped()
            && let Some((dragged, before)) = self.drag.take()
        {
            self.reorder(dragged, before);
        }
        // **What a double click means depends on what the tile is.** A picture goes to
        // Develop, a folder is navigated into, and anything else does nothing — there
        // is no decode for a `.txt` and pretending otherwise would open an empty tab.
        //
        // The folder is *recorded* rather than opened here: `open_folder` rebuilds
        // `entries`, and this runs inside `grid_ui`'s loop over that very list. It is
        // acted on once the loop is done, the same journey `open` already makes.
        if r.double_clicked() {
            self.selected = Some(idx);
            if self.search_offline.contains(&idx) {
                if let Some(e) = self.entries.get(idx as usize) {
                    self.action_note = Some(format!(
                        "{} is on a drive that is not connected",
                        e.path.display()
                    ));
                }
            } else if let Some(e) = self.entries.get(idx as usize) {
                match e.kind {
                    Kind::Picture => open = Some(e.path.clone()),
                    Kind::Folder => self.enter = Some(e.path.clone()),
                    Kind::Other => {}
                }
            }
        }

        let selected = self.selected == Some(idx);
        let in_batch = self.batch.contains(&idx);
        // The card is taller than the image it holds. Below sit the filename and the
        // marks, each on its own row and each only when it is asked for — see
        // [`META_ROW_H`] and [`NAME_ROW_H`]. The picture takes whatever is left.
        let marks = !self.frameless;
        let named = self.show_filenames;
        let below = if marks { META_ROW_H } else { 0.0 } + if named { NAME_ROW_H } else { 0.0 };
        let img_rect =
            egui::Rect::from_min_max(card.min, egui::pos2(card.max.x, card.max.y - below));

        // **Three states, three weights.** Selected is a ruby ring, the same ink the
        // footer uses for "this is the state you are in". Hover is a lift in the
        // fill only — a second ring would compete with the selection at exactly the
        // moment you are about to change it.
        //
        // **Frameless drops the box, not the feedback.** The prototype's mode hides
        // the border so the grid reads as pictures rather than as cards; a selection
        // you could not see would be a different thing entirely, so the ring stays
        // and only the resting fill goes.
        let fill = if in_batch {
            theme::RUBY_FILL_DIM
        } else if r.hovered() && !selected {
            theme::CHROME
        } else {
            theme::CHROME_DEEP
        };
        // **The card has its own ground, the module value**, independent of the
        // canvas it sits on. The three fills above were written against
        // `CHROME_DEEP`; moved to the card grey here, and kept out of the grid's own
        // re-grey so the canvas value does not move them a second time.
        let fill = theme::Ground {
            design: theme::CHROME_DEEP.r(),
            to: self.grounds[2],
        }
        .apply(fill);
        if !self.frameless || r.hovered() || in_batch {
            theme::true_colour(ui, || ui.painter().rect_filled(img_rect, 2.0, fill));
        }
        // **Three states, three weights, and they stack.** Batch membership is a
        // ground, the anchor is a full ruby ring, and a batch member that is not the
        // anchor gets a thinner one — so a five-tile selection reads as five tiles
        // with one of them being where the next `⇧`-click will measure from.
        if selected {
            ui.painter().rect_stroke(
                card,
                2.0,
                egui::Stroke::new(1.5, theme::RUBY),
                egui::StrokeKind::Inside,
            );
        } else if in_batch {
            ui.painter().rect_stroke(
                card,
                2.0,
                egui::Stroke::new(1.0, theme::RUBY.gamma_multiply(0.6)),
                egui::StrokeKind::Inside,
            );
        }

        // **A folder or a document is drawn from an icon, and never asks the queue.**
        // `request` has already declined to queue it, so its slot in `tiles` stays
        // empty forever — which is why this returns before the match below rather than
        // adding an arm to it. The match is about *thumbnail state*, and these two
        // kinds have none.
        let kind = self
            .entries
            .get(idx as usize)
            .map(|e| e.kind)
            .unwrap_or(Kind::Picture);
        if kind != Kind::Picture {
            let name = match kind {
                Kind::Folder => "folder",
                // The extension decides the sheet. `icon_for` falls back to the plain
                // one, so an unrecognised type is still a tile rather than a blank.
                _ => self
                    .entries
                    .get(idx as usize)
                    .map(|e| icon_for(&e.ext))
                    .unwrap_or("file"),
            };
            // Sized against the tile rather than fixed, so the glyph keeps its
            // proportion up and down the whole size ladder — at the two-column rungs a
            // 24 px icon in a 600 px card would be a speck.
            let side = (img_rect.height() * 0.42).clamp(18.0, 96.0);
            let at = egui::Rect::from_center_size(img_rect.center(), egui::Vec2::splat(side));
            // Dimmer than a photograph, and deliberately: these are the things the grid
            // is *not* here to show you. A folder reads a little brighter than a
            // document because it is the one you can act on.
            let ink = if selected {
                theme::BRIGHT
            } else if kind == Kind::Folder {
                theme::NAME
            } else {
                theme::DIM
            };
            crate::icons::paint_at(ui, icons, name, "▢", at, ink, side * 0.9);
            // The name always shows for these, whatever the filename setting says. A
            // picture is recognisable without its name and a folder is not — an
            // unlabelled row of identical folder glyphs is a row of nothing.
            if !named {
                let row = egui::Rect::from_min_max(
                    egui::pos2(card.min.x, img_rect.max.y - NAME_ROW_H),
                    egui::pos2(card.max.x, img_rect.max.y),
                );
                let font = egui::FontId::proportional(9.0);
                if let Some(e) = self.entries.get(idx as usize) {
                    let text = elide(ui, &e.name, &font, row.width() - 6.0);
                    ui.painter().with_clip_rect(row).text(
                        row.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        font,
                        if selected { theme::BRIGHT } else { theme::NAME },
                    );
                }
            }
        }

        let frame = self.frame;
        match self.tiles.get_mut(&idx) {
            Some(Tile::Ready { texture, seen }) => {
                *seen = frame;
                // A quarter turn swaps which way the picture has to fit, so the
                // *turned* size is what the scale is solved against — otherwise a
                // rotated landscape frame is fitted as though it were still wide and
                // runs off the top and bottom of its tile.
                let size = texture.size_vec2();
                // The tile arrives the right way up, so nothing here turns it: the
                // camera preview is oriented by `preview::tile` and a developed render
                // by the composition it was rendered through.
                let fit = size;
                // The scale target subtracts both margins. Forgetting the second one
                // clips the photograph against the far edge of its tile.
                let box_ = egui::vec2(
                    img_rect.width() - 2.0 * IMAGE_PAD,
                    img_rect.height() - 2.0 * IMAGE_PAD,
                );
                let scale = (box_.x / fit.x).min(box_.y / fit.y);
                let draw = egui::Rect::from_center_size(img_rect.center(), size * scale);
                egui::Image::new(&*texture).paint_at(ui, draw);
                let shown = draw;

                // **The edited mark is a rule around the picture**, the maintainer's, and it
                // reads as a contact sheet: a frame drawn on the image itself rather
                // than a dot floating over a corner of it. Against `draw` and not the
                // card, so it hugs the photograph through the letterboxing — which is
                // what makes a grid of them look like a sheet rather than like a grid
                // of boxes with marks in them.
                if self.edited_mark && self.entries.get(idx as usize).is_some_and(|e| e.edited) {
                    ui.painter().rect_stroke(
                        shown,
                        0.0,
                        egui::Stroke::new(EDITED_STROKE, EDITED_INK),
                        egui::StrokeKind::Outside,
                    );
                }
            }
            Some(Tile::Missing) => {
                ui.painter().text(
                    img_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    if self.search_offline.contains(&idx) {
                        "drive offline"
                    } else {
                        "no preview"
                    },
                    egui::FontId::proportional(11.0),
                    theme::DIM,
                );
            }
            // Pending, or evicted and about to be asked for again. A quiet box: a
            // spinner per tile in a grid of two hundred is a screen full of motion
            // that says nothing.
            _ => {}
        }

        // **The strip carries the filename or the marks, and the marks win.** The
        // prototype gives them a row each and a taller card; here one 20 point strip
        // is what the card reserves, so when both are asked for the stars take it —
        // a rating is a judgement you made and the filename is on the file.
        // **The filename first, then the marks.** The name describes the picture above
        // it and the marks are what you did to it, so the name sits closer to what it
        // names.
        let mut y = img_rect.max.y;
        if named && let Some(e) = self.entries.get(idx as usize) {
            let row = egui::Rect::from_min_max(
                egui::pos2(card.min.x, y),
                egui::pos2(card.max.x, y + NAME_ROW_H),
            );
            // **Clipped and shortened to the card.** A long name used to run out of
            // the tile and across its neighbours; the middle goes because the ends are
            // what tell two frames in a shoot apart.
            let font = egui::FontId::proportional(9.0);
            let ink = if selected { theme::BRIGHT } else { theme::NAME };
            let text = elide(ui, &e.name, &font, row.width() - 6.0);
            ui.painter().with_clip_rect(row).text(
                row.center(),
                egui::Align2::CENTER_CENTER,
                text,
                font,
                ink,
            );
            y = row.max.y;
        }
        if marks {
            let row = egui::Rect::from_min_max(egui::pos2(card.min.x, y), card.max);
            self.marks_ui(ui, row, idx);
        }

        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);

        // The right-click menu. Rating and label, checked against what this file
        // already carries — a menu that cannot tell you the current state makes you
        // guess whether you already did the thing.
        r.context_menu(|ui| {
            // Right-clicking a member of a batch keeps the batch, so Rating and Label
            // apply to everything visibly selected. Right-clicking anywhere else
            // begins a new one-file selection, as Finder does.
            if !self.batch.contains(&idx) {
                self.batch.clear();
            }
            self.selected = Some(idx);
            self.head = None;
            let (rating, label, path, can_copy) = self
                .entries
                .get(idx as usize)
                .map(|e| {
                    (
                        e.rating,
                        e.label.clone(),
                        Some(e.path.clone()),
                        e.kind == Kind::Picture && e.edited,
                    )
                })
                .unwrap_or((0, None, None, false));

            if ui
                .button(format!("Show in {}", crate::platform::file_manager_name()))
                .clicked()
            {
                if let Some(path) = path
                    && let Err(e) = crate::platform::reveal(&path)
                {
                    self.action_note = Some(format!(
                        "could not show {} in {}: {e}",
                        name_of(&path),
                        crate::platform::file_manager_name()
                    ));
                }
                ui.close();
            }
            let rename_count = self.selection().len();
            let rename_label = if rename_count > 1 {
                format!("Rename {rename_count} Files…    ⌘⇧R")
            } else {
                "Rename…    ⌘⇧R".to_owned()
            };
            if ui.button(rename_label).clicked() {
                self.begin_rename();
                ui.close();
            }
            if ui.button("Contact Sheet…    ⌘⇧P").clicked() {
                self.begin_contact_sheet();
                ui.close();
            }
            ui.separator();

            if ui
                .add_enabled(can_copy, egui::Button::new("Copy Settings    ⌘⇧C"))
                .clicked()
            {
                self.action_note = Some(match self.copy_settings_at(idx) {
                    Ok(note) | Err(note) => note,
                });
                ui.close();
            }
            if ui
                .add_enabled(
                    self.has_copied_settings(),
                    egui::Button::new("Paste Settings    ⌘⇧V"),
                )
                .clicked()
            {
                self.action_note = Some(match self.paste_settings() {
                    Ok(note) | Err(note) => note,
                });
                ui.close();
            }
            ui.separator();

            ui.menu_button("Rating", |ui| {
                for n in 1..=5 {
                    if ui
                        .selectable_label(rating == n, "★".repeat(n as usize))
                        .clicked()
                    {
                        self.set_rating(n);
                        ui.close();
                    }
                }
                if ui.selectable_label(rating == 0, "None").clicked() {
                    self.set_rating(0);
                    ui.close();
                }
            });
            ui.menu_button("Label", |ui| {
                if ui.selectable_label(label.is_none(), "None").clicked() {
                    self.set_label(None);
                    ui.close();
                }
                for (name, colour) in theme::LABELS {
                    let on = label.as_deref() == Some(name);
                    if ui
                        .selectable_label(on, egui::RichText::new(name).color(colour))
                        .clicked()
                    {
                        self.set_label(Some(name));
                        ui.close();
                    }
                }
            });
        });

        open
    }

    /// The stars and the label dot, in the card's reserved strip.
    ///
    /// Both are live controls, not a readout: clicking a star sets that rating and
    /// clicking the dot cycles the label, which is the prototype's arrangement
    /// (`monopro.py:22984-22993`) and the reason the row is worth its height.
    fn marks_ui(&mut self, ui: &mut egui::Ui, strip: egui::Rect, idx: u32) {
        let Some((rating, label)) = self
            .entries
            .get(idx as usize)
            .map(|e| (e.rating, e.label.clone()))
        else {
            return;
        };

        const STAR_W: f32 = 13.0;
        let y = strip.center().y;
        let left = strip.min.x + 4.0;

        for n in 1..=5i32 {
            let r = egui::Rect::from_center_size(
                egui::pos2(left + STAR_W * (n as f32 - 0.5), y),
                egui::vec2(STAR_W, strip.height()),
            );
            let hit = ui.interact(r, ui.id().with(("star", idx, n)), egui::Sense::click());
            let on = n <= rating;
            // An unset star is drawn, not omitted — a row that grows as you rate it
            // gives you no target to click for the fourth star.
            let ink = if on {
                theme::AMBER
            } else if hit.hovered() {
                theme::DIM
            } else {
                EMPTY_STAR
            };
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                if on { "★" } else { "☆" },
                egui::FontId::proportional(11.0),
                ink,
            );
            if hit.clicked() {
                if self.batch.contains(&idx) {
                    self.set_rating(n);
                } else {
                    self.set_rating_at(idx, n);
                }
            }
        }

        // The dot, at the far end. Unlabelled is an outline rather than nothing, for
        // the same reason an unset star is drawn: it has to be clickable.
        let dot = egui::pos2(strip.max.x - 9.0, y);
        let hit = ui.interact(
            egui::Rect::from_center_size(dot, egui::vec2(16.0, strip.height())),
            ui.id().with(("label", idx)),
            egui::Sense::click(),
        );
        match label.as_deref().and_then(theme::label_colour) {
            Some(c) => {
                ui.painter().circle_filled(dot, 5.0, c);
            }
            None => {
                ui.painter().circle_stroke(
                    dot,
                    4.5,
                    egui::Stroke::new(
                        1.0,
                        if hit.hovered() {
                            theme::DIM
                        } else {
                            egui::Color32::from_gray(74)
                        },
                    ),
                );
            }
        }
        if hit.clicked() {
            // **No selection change**, same as the stars above. This line was doing
            // nothing functional even before that: the cycle writes `entries[idx]`
            // directly rather than going through `set_label`, so the selection it moved
            // was never read — it only lit the ruby ring.
            //
            // Cycle: none → magenta → blue → green → yellow → red → none. A foreign label counts
            // as "not one of ours" and the first click takes you to the start of this
            // app's run rather than deleting a word it did not write.
            let next = match label
                .as_deref()
                .and_then(|l| theme::LABELS.iter().position(|(n, _)| *n == l))
            {
                None => Some(theme::LABELS[0].0),
                Some(i) if i + 1 < theme::LABELS.len() => Some(theme::LABELS[i + 1].0),
                Some(_) => None,
            };
            if self.batch.contains(&idx) {
                // A selected run receives one answer. `set_label` deliberately treats
                // a batch as an assignment, so mixed labels converge on this next one.
                self.set_label(next);
            } else if let Some(e) = self.entries.get_mut(idx as usize) {
                // One tile keeps the cycle's exact next value rather than the toggle
                // semantics used by a direct keyboard/menu assignment.
                e.label = next.map(str::to_owned);
                Self::commit(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic filenames isolate ordering cost; opening uses empty JPEG files
    /// and measures enumeration/entry metadata, not decoding or thumbnail work.
    #[test]
    #[ignore = "performance measurement; run explicitly with --release --nocapture"]
    fn measure_folder_open_and_manual_order() {
        use std::{hint::black_box, time::Instant};
        let mut lb = Lightbox::new();
        for count in [1_000, 10_000] {
            lb.entries = (0..count)
                .map(|i| {
                    entry_for(
                        PathBuf::from(format!("/measurement/{i:05}.jpg")),
                        Kind::Picture,
                    )
                })
                .collect();
            lb.manual = lb.entries.iter().rev().map(|e| e.name.clone()).collect();
            lb.sort = Sort::Manual;
            let mut times = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                lb.reindex();
                black_box(&lb.visible);
                times.push(start.elapsed());
            }
            times.sort();
            eprintln!("manual order {count}: median {:?}", times[2]);
        }
        let root = crate::settings::dir().unwrap().join("folder-measurement");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..1_000 {
            std::fs::write(root.join(format!("{i:05}.jpg")), b"").unwrap();
        }
        lb.sort = Sort::Filename;
        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            lb.open_folder(&root);
            black_box(&lb.visible);
            times.push(start.elapsed());
        }
        times.sort();
        eprintln!("warm folder open 1000 empty JPEGs: median {:?}", times[2]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unchanged_tile_is_readable_and_a_changed_raw_rejects_it() {
        let dir = crate::settings::dir().unwrap();
        let raw = dir.join("unchanged.dng");
        std::fs::write(&raw, b"raw").unwrap();
        std::fs::write(raw_core::sidecar::path_for(&raw), b"edits").unwrap();
        let stamp = EditedTileStamp::capture(&raw).unwrap();
        assert!(store_edited_tile(&raw, &stamp, 1, 1, &[255, 0, 0, 255]));
        let file = edited_cache_name(&raw, &cache_dir().unwrap()).unwrap();
        assert_eq!(image::open(file).unwrap().width(), 1);
        std::fs::write(&raw, b"replaced raw").unwrap();
        let file = edited_cache_name(&raw, &cache_dir().unwrap()).unwrap();
        assert!(!store_edited_tile(&raw, &stamp, 1, 1, &[255, 0, 0, 255]));
        assert!(!file.exists());
    }

    #[test]
    fn stale_tile_retries_are_deduplicated_and_leave_other_folders_alone() {
        let mut lb = grid_of(2);
        lb.xmp_thumbnails = true;
        lb.entries[0].edited = true;
        let path = lb.entries[0].path.clone();
        lb.retry_developed_tile(&path);
        lb.retry_developed_tile(&path);
        assert_eq!(lb.developed_wanted.len(), 1);
        lb.retry_developed_tile(Path::new("/another/folder/photo.dng"));
        assert_eq!(lb.developed_wanted.len(), 1);
        assert_eq!(lb.developed_tile_wanted().unwrap().1, path);
    }

    #[test]
    fn publishing_an_edited_tile_cancels_the_older_camera_thumbnail_request() {
        let mut lb = grid_of(1);
        let key = Key {
            generation: lb.generation,
            idx: 0,
            full: false,
        };
        let path = lb.entries[0].path.clone();
        lb.queue.submit(key, decode::BACKGROUND, || None);
        lb.tiles.insert(0, Tile::Pending);
        lb.developed_tile_done(&path);
        assert!(!lb.queue.is_busy(key));
        lb.collect(&egui::Context::default());
        assert!(!lb.tiles.contains_key(&0));
    }

    #[test]
    fn a_panicking_preview_is_reported_without_retrying_every_frame() {
        let mut lb = grid_of(1);
        let key = lb.full_key(0);
        lb.preview_queue
            .submit(key, decode::BACKGROUND, || panic!("broken preview"));
        let ctx = egui::Context::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lb.preview_queue.in_flight() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "preview remained busy"
            );
            lb.collect(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(lb.action_note.as_ref().unwrap().contains("broken preview"));
        lb.request_full_at(0, decode::FOREGROUND);
        assert!(!lb.preview_queue.is_busy(key));
        lb.generation += 1;
        lb.collect(&ctx);
        assert!(lb.preview_failed.is_empty());
    }

    #[test]
    fn an_old_render_cannot_be_stored_under_newer_sidecar_edits() {
        let dir = crate::settings::dir().unwrap();
        let raw = dir.join("changing.dng");
        let side = raw_core::sidecar::path_for(&raw);
        std::fs::write(&raw, b"raw").unwrap();
        std::fs::write(&side, b"old edits").unwrap();
        let stamp = EditedTileStamp::capture(&raw).unwrap();
        std::fs::write(&side, b"newer and different edits").unwrap();
        let current = edited_cache_name(&raw, &cache_dir().unwrap()).unwrap();
        store_edited_tile(&raw, &stamp, 1, 1, &[255, 0, 0, 255]);
        assert!(!current.exists(), "old pixels were labelled as newer edits");
    }

    #[test]
    fn lightbox_defaults_and_saved_widths_are_pixels() {
        let mut lb = Lightbox::default();
        let check = |lb: &Lightbox, expected: f32, extent: f32| {
            let root = lb.tree.root.unwrap();
            let egui_tiles::Tile::Container(egui_tiles::Container::Linear(row)) =
                lb.tree.tiles.get(root).unwrap()
            else {
                panic!();
            };
            let sum: f32 = row.children.iter().map(|c| row.shares[*c]).sum();
            let actual = row.shares[row.children[0]] / sum * (extent - crate::layout::GAP);
            assert!((actual - expected).abs() < 0.01);
        };
        lb.restore_panel_widths(2000.0, 280.0);
        check(&lb, 280.0, 2000.0);
        let root = lb.tree.root.unwrap();
        let egui_tiles::Tile::Container(egui_tiles::Container::Linear(row)) =
            lb.tree.tiles.get(root).unwrap()
        else {
            panic!();
        };
        let panel = row.children[0];
        lb.panel_sizes.insert(panel, egui::vec2(390.0, 900.0));
        lb.restore_panel_pixels = true;
        lb.restore_panel_widths(2400.0, 280.0);
        check(&lb, 390.0, 2400.0);
        lb.reset_tree();
        lb.restore_panel_widths(1800.0, 280.0);
        check(&lb, 280.0, 1800.0);
    }

    #[test]
    fn footer_location_keeps_the_folder_and_only_two_parents() {
        assert_eq!(
            footer_folder_path(Path::new("/example/Pictures/Photos/Ingest/session")),
            "Photos/Ingest/session"
        );
        assert_eq!(footer_folder_path(Path::new("/shoot")), "shoot");
        assert_eq!(
            footer_folder_path(Path::new("/")),
            crate::platform::root_name(Path::new("/"))
        );
    }

    #[test]
    fn the_folder_tree_defaults_to_home_without_a_second_startup_disk_route() {
        let tree = FolderTree::new();
        assert_eq!(
            folder_root_name(Path::new("/")),
            crate::platform::root_name(Path::new("/"))
        );
        if let Some(home) = home() {
            assert_eq!(
                tree.roots.first(),
                Some(&home),
                "Home is not the local root"
            );
            assert!(
                !tree.roots.contains(&PathBuf::from("/")),
                "the startup disk duplicates every folder below Home"
            );
        }
    }

    #[test]
    fn choosing_the_startup_disk_makes_it_the_one_local_root() {
        let roots = folder_roots(Some(Path::new("/")));
        assert_eq!(roots.first(), Some(&PathBuf::from("/")));
        assert_eq!(
            roots
                .iter()
                .filter(|root| root.as_path() == Path::new("/"))
                .count(),
            1
        );
    }

    #[test]
    fn the_size_ladder_only_goes_where_it_has_rungs() {
        let mut lb = Lightbox::new();
        lb.size = 0;
        lb.resize(false);
        assert_eq!(
            lb.size, 0,
            "smaller than the smallest is still the smallest"
        );
        for _ in 0..20 {
            lb.resize(true);
        }
        assert_eq!(
            lb.size,
            SIZES.len() - 1,
            "larger than the largest is still the largest"
        );
        assert_eq!(lb.rung(), *SIZES.last().unwrap());
    }

    #[test]
    fn the_ladder_reaches_two_columns_on_a_wide_grid() {
        // the maintainer asked for a two-column grid. The prototype's ladder stops at 400,
        // which on his 1450 pt grid is three across with nowhere further to go, so
        // the answer is more ladder rather than a second control meaning the same
        // thing.
        // Checked across the widths a grid actually gets — panels open, panels
        // hidden, and a narrow window — because the ladder is a *target width* and
        // what it buys in columns depends on how much room there is.
        // **Two columns at every width**, which is what a count buys and a target
        // width cannot: 760 points would be two across at 1450 and three at 2560.
        for wide in [1200.0, 1450.0, 1990.0, 2560.0, 3440.0] {
            let counts: Vec<usize> = SIZES
                .iter()
                .map(|r| grid_metrics(wide, *r, 0.0).0)
                .collect();
            assert!(
                counts.contains(&2),
                "no rung gives two columns at {wide} pt: {counts:?}"
            );
            assert!(counts.contains(&1), "nor one: {counts:?}");
            assert!(
                *counts.first().unwrap() >= 9,
                "the small end should still be a contact sheet"
            );
        }
    }

    #[test]
    fn the_ladder_climbs() {
        // Each rung bigger than the last — which now has to be checked as *columns*,
        // because the ladder mixes widths and counts and only the resulting grid can
        // be compared across the two kinds.
        // Non-increasing rather than strictly falling: on a narrow grid a width rung
        // and a count rung can land on the same number — at 1200 pt, 400 points is
        // already two across. That collision is not a fault, it is the case `resize`
        // exists to step over, and `every_press_moves_the_grid` is what pins it.
        for wide in [1200.0, 1450.0, 1990.0, 2560.0] {
            let counts: Vec<usize> = SIZES
                .iter()
                .map(|r| grid_metrics(wide, *r, 0.0).0)
                .collect();
            assert!(
                counts.windows(2).all(|w| w[1] <= w[0]),
                "at {wide}: {counts:?} does not fall"
            );
            assert!(
                counts.first() > counts.last(),
                "at {wide}: the ladder goes nowhere"
            );
        }
    }

    #[test]
    fn every_press_moves_the_grid() {
        // The defect this exists to prevent: at 1292 points, rungs 260 and 320 both
        // give four columns at an identical card, so a press of `⌘+` changed the
        // number in the footer and nothing on screen.
        let avail = 1292.0;
        assert_eq!(
            grid_metrics(avail, Rung::Width(260.0), 0.0).0,
            grid_metrics(avail, Rung::Width(320.0), 0.0).0,
            "the collision this guards"
        );

        let mut lb = Lightbox::new();
        lb.last_avail = avail;

        // Walk the whole ladder up, then the whole way down, and require the column
        // count to change on every single press.
        lb.size = 0;
        let mut seen = vec![lb.columns_now()];
        while lb.size < SIZES.len() - 1 {
            let before = lb.columns_now();
            lb.resize(true);
            let after = lb.columns_now();
            assert_ne!(
                before, after,
                "growing from rung {} changed nothing",
                lb.size
            );
            seen.push(after);
        }
        // Columns fall as the cards grow, and never repeat.
        for pair in seen.windows(2) {
            assert!(pair[1] < pair[0], "columns went {pair:?}");
        }

        while lb.size > 0 {
            let before = lb.columns_now();
            lb.resize(false);
            assert_ne!(
                before,
                lb.columns_now(),
                "shrinking from rung {} changed nothing",
                lb.size
            );
        }
        assert_eq!(lb.size, 0, "and it walks all the way back to the smallest");
    }

    #[test]
    fn without_a_measured_width_a_press_is_a_single_step() {
        // Before the first frame there is no geometry to consult, so the skip has
        // nothing to go on and must not loop to the end of the ladder.
        let mut lb = Lightbox::new();
        assert_eq!(lb.last_avail, 0.0);
        lb.size = 1;
        lb.resize(true);
        assert_eq!(lb.size, 2, "one rung, not a run to the top");
    }

    #[test]
    fn a_card_leaves_a_square_image_well_above_its_furniture() {
        // The filename and marks make the card taller than the photograph area, but
        // they must not make a portrait photograph larger than a landscape one.
        let (_, w, h) = grid_metrics(1000.0, Rung::Width(210.0), META_ROW_H + NAME_ROW_H);
        assert!(h > w, "a {w}x{h} card has no room for its rows");
        assert!(
            (h - META_ROW_H - NAME_ROW_H - w.round()).abs() < 0.01,
            "the image well is not square: {w}x{}",
            h - META_ROW_H - NAME_ROW_H
        );
    }

    #[test]
    fn the_default_card_is_210_by_243_before_row_fill() {
        // Ten points are the grid's two outer margins, leaving the default rung's
        // exact target width available to its single column.
        let (cols, w, h) = grid_metrics(220.0, SIZES[DEFAULT_SIZE], META_ROW_H + NAME_ROW_H);
        assert_eq!(cols, 1);
        assert_eq!((w, h), (210.0, 243.0));
        assert_eq!(
            IMAGE_PAD, 9.0,
            "the image has the requested 5-point extra margin"
        );
    }

    #[test]
    fn the_cards_fill_the_row_rather_than_leaving_a_ragged_edge() {
        // The prototype's arithmetic, checked at the width it was written for: the
        // ladder value is a target that picks the column count, and the cards then
        // share the row out between them.
        let (cols, w, _) = grid_metrics(1000.0, Rung::Width(210.0), META_ROW_H + NAME_ROW_H);
        assert_eq!(
            cols, 4,
            "990 points of viewport at a 210 target is four columns"
        );

        // Every card, plus the gaps between them, is the whole viewport back again.
        let used = w * cols as f32 + SPACING * (cols - 1) as f32;
        assert!(
            (used - (1000.0 - 2.0 * MARGIN)).abs() < 1.0,
            "{used} does not fill 990"
        );
        assert!(
            w > 210.0,
            "sharing the row out makes the cards larger than the target, not smaller"
        );
    }

    #[test]
    fn a_narrow_panel_still_gets_one_column() {
        // Dragging the tree wide enough to squeeze the grid must not produce zero
        // columns and a division by it.
        let (cols, w, h) = grid_metrics(60.0, Rung::Width(400.0), META_ROW_H + NAME_ROW_H);
        assert_eq!(cols, 1);
        assert!(
            w >= 80.0 && h > 0.0,
            "the floor keeps a card visible: {w}x{h}"
        );
    }

    #[test]
    fn every_rung_of_the_ladder_lays_out() {
        // No size may produce a degenerate grid at any plausible window width.
        for target in SIZES {
            for width in [320.0, 800.0, 1440.0, 2560.0, 3840.0] {
                let (cols, w, h) = grid_metrics(width, target, META_ROW_H + NAME_ROW_H);
                assert!(cols >= 1, "{target:?} at {width}: no columns");
                assert!(w >= 80.0, "{target:?} at {width}: card {w} too small");
                assert!(h > w, "{target:?} at {width}: card {w}x{h} is not portrait");
            }
        }
    }

    #[test]
    fn only_raws_reach_the_grid() {
        assert!(
            is_raw(Path::new("/x/a.DNG")),
            "extension matching is case-insensitive"
        );
        assert!(is_raw(Path::new("/x/a.rw2")));
        assert!(!is_raw(Path::new("/x/a.jpg")), "an export is not a shoot");
        assert!(
            !is_raw(Path::new("/x/a.mono.xmp")),
            "a sidecar is not a picture"
        );
        assert!(!is_raw(Path::new("/x/noextension")));
    }

    #[test]
    fn changing_folders_retires_the_old_generation() {
        // The property that makes cancellation safe: a result from the folder you
        // just left cannot be mistaken for one from the folder you are in.
        let mut lb = Lightbox::new();
        let before = lb.generation;
        lb.open_folder(Path::new("/nonexistent-folder-for-a-test"));
        assert_ne!(lb.generation, before);
        assert!(
            lb.entries.is_empty(),
            "a folder that is not there is empty, not an error"
        );
        assert!(
            lb.folder.is_some(),
            "and it is still the folder you asked for"
        );
    }

    /// A folder with one raw, one document, one subfolder and one sidecar in it.
    fn mixed_folder() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "monopro-listing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subfolder")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("a.dng"), b"not really a raw").unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        std::fs::write(dir.join("a.mono.xmp"), b"<x/>").unwrap();
        dir
    }

    #[test]
    fn the_grid_lists_pictures_only_until_the_two_settings_say_otherwise() {
        let dir = mixed_folder();
        let names = |v: Vec<(std::path::PathBuf, Kind)>| -> Vec<String> {
            let mut n: Vec<String> = v
                .into_iter()
                .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };

        // The default, and the behaviour every existing install has.
        assert_eq!(names(list_entries(&dir, false, false, false)), ["a.dng"]);

        // Folders on: the subfolder appears, the hidden one does not.
        assert_eq!(
            names(list_entries(&dir, false, true, false)),
            ["a.dng", "subfolder"]
        );

        // Other files on: the document appears — and **the sidecar does not**, which
        // is the one exclusion made by hand. Listing `a.mono.xmp` beside `a.dng` would
        // double every worked-on picture in the grid.
        assert_eq!(
            names(list_entries(&dir, false, false, true)),
            ["a.dng", "notes.txt"]
        );

        assert_eq!(
            names(list_entries(&dir, false, true, true)),
            ["a.dng", "notes.txt", "subfolder"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subfolder_is_listed_once_even_when_the_grid_is_recursing() {
        // `subfolders` flattens the tree into one grid of pictures. Listing the folders
        // as well at every depth would show the container *and* its contents side by
        // side — the same frames twice. So folder tiles are depth 0 only.
        let dir = mixed_folder();
        std::fs::write(dir.join("subfolder").join("deep.dng"), b"raw").unwrap();
        std::fs::create_dir_all(dir.join("subfolder").join("deeper")).unwrap();

        let got = list_entries(&dir, true, true, false);
        let folders: Vec<_> = got.iter().filter(|(_, k)| *k == Kind::Folder).collect();
        assert_eq!(
            folders.len(),
            1,
            "expected only the top-level folder, got {folders:?}"
        );
        assert!(folders[0].0.ends_with("subfolder"));
        // And the recursion still did its job on the pictures.
        assert_eq!(got.iter().filter(|(_, k)| *k == Kind::Picture).count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_file_icon_the_grid_asks_for_is_loaded() {
        // **A name the icon set does not have is a blank tile, silently.** `icons::load`
        // skips what it cannot find and `paint_at` falls back to a text glyph — which on
        // a button is a visible wrong character and on a grid tile with no text is
        // nothing at all. This walks the same table `icon_for` reads.
        for (exts, icon) in FILE_ICONS {
            assert!(
                crate::icons::shipped(icon),
                "icon_for maps {exts:?} to {icon:?}, which icons::SOURCES does not ship"
            );
            for ext in *exts {
                assert_eq!(icon_for(ext), *icon);
            }
        }
        assert!(crate::icons::shipped(FALLBACK_ICON));
        assert!(
            crate::icons::shipped("folder"),
            "folder tiles have no glyph"
        );
        // An extension nobody anticipated still gets a tile rather than a blank.
        assert_eq!(icon_for("qqq"), FALLBACK_ICON);
        assert_eq!(icon_for(""), FALLBACK_ICON);
    }

    #[test]
    fn a_folder_survives_a_filter_that_no_folder_could_satisfy() {
        // A folder has no rating, so every test in `admits` would throw it out — and a
        // two-star filter that removed the way *out* of the folder would be the grid
        // trapping you in it.
        let folder = Entry {
            path: PathBuf::from("/x/sub"),
            name: "sub".into(),
            kind: Kind::Folder,
            edited: false,
            rating: 0,
            label: None,
            ext: String::new(),
            developed: None,
            captured: None,
            orientation: None,
        };
        let picture = Entry {
            kind: Kind::Picture,
            ..folder_like()
        };

        let strict = Filters {
            stars: 3,
            ..Default::default()
        };
        assert!(
            strict.admits(&folder),
            "a two-star filter hid the way out of the folder"
        );
        assert!(
            !strict.admits(&picture),
            "and it still filters the pictures"
        );

        let labelled = Filters {
            labels: vec!["red".into()],
            ..Default::default()
        };
        assert!(labelled.admits(&folder));
        assert!(!labelled.admits(&picture));
    }

    /// An unrated, unlabelled picture — what the filters above are meant to reject.
    fn folder_like() -> Entry {
        Entry {
            path: PathBuf::from("/x/a.dng"),
            name: "a.dng".into(),
            kind: Kind::Picture,
            edited: false,
            rating: 0,
            label: None,
            ext: "dng".into(),
            developed: None,
            captured: None,
            orientation: None,
        }
    }

    /// `n` plain picture entries named `a`, `b`, `c`… in a grid with nothing filtered.
    fn grid_of(n: usize) -> Lightbox {
        let mut lb = Lightbox::new();
        lb.entries = (0..n)
            .map(|i| {
                let name = ((b'a' + i as u8) as char).to_string();
                Entry {
                    name: format!("{name}.dng"),
                    ..folder_like()
                }
            })
            .collect();
        lb.reindex();
        lb
    }

    #[test]
    fn a_rename_swap_preserves_manual_order_and_distinct_entries() {
        let mut lb = grid_of(2);
        let folder = crate::settings::dir().unwrap();
        lb.folder = Some(folder.clone());
        for (entry, name) in lb.entries.iter_mut().zip(["A.dng", "B.dng"]) {
            entry.path = folder.join(name);
            entry.name = name.into();
        }
        lb.manual = vec!["B.dng".into(), "A.dng".into()];
        lb.apply_rename_events(&[
            crate::rename::Event {
                old: folder.join("A.dng"),
                new: folder.join("B.dng"),
            },
            crate::rename::Event {
                old: folder.join("B.dng"),
                new: folder.join("A.dng"),
            },
        ]);
        assert_eq!(lb.manual, ["A.dng", "B.dng"]);
        assert_eq!(lb.entries[0].path, folder.join("B.dng"));
        assert_eq!(lb.entries[1].path, folder.join("A.dng"));
        assert_eq!(read_order(&folder), lb.manual);
    }

    #[test]
    fn a_rating_comes_off_a_whole_selection_the_second_time_it_is_pressed() {
        // the maintainer, at the screen: with several frames selected there was no way to take a
        // rating off — not with the key, not by clicking the star. The batch path was an
        // assignment and only an assignment, so pressing the same number again wrote the
        // same number. The mixed case is why it was an assignment, and that half stays.
        let mut lb = grid_of(4);
        lb.batch = [0u32, 1, 2].into_iter().collect();

        lb.set_rating(3);
        assert_eq!(
            [
                lb.entries[0].rating,
                lb.entries[1].rating,
                lb.entries[2].rating
            ],
            [3, 3, 3],
            "the batch did not take the rating"
        );

        // Now they agree, so the same key is the toggle it is on a single frame.
        lb.set_rating(3);
        assert_eq!(
            [
                lb.entries[0].rating,
                lb.entries[1].rating,
                lb.entries[2].rating
            ],
            [0, 0, 0],
            "a selection that all carried three stars could not be cleared"
        );

        // And the case the assignment rule exists for. One frame differs, so three
        // stars means three stars everywhere rather than clearing the two that agree.
        lb.entries[0].rating = 3;
        lb.entries[1].rating = 3;
        lb.entries[2].rating = 1;
        lb.set_rating(3);
        assert_eq!(
            [
                lb.entries[0].rating,
                lb.entries[1].rating,
                lb.entries[2].rating
            ],
            [3, 3, 3],
            "a mixed selection was toggled instead of assigned"
        );

        // The frame outside the selection is untouched throughout.
        assert_eq!(lb.entries[3].rating, 0);
    }

    #[test]
    fn a_colour_label_comes_off_a_whole_selection_the_same_way() {
        // The same gesture has to mean the same thing, or `⇧2` would clear a label on
        // one tile and be inert on five. See `set_label`.
        let mut lb = grid_of(3);
        lb.batch = [0u32, 1].into_iter().collect();

        lb.set_label(Some("red"));
        assert_eq!(lb.entries[0].label.as_deref(), Some("red"));
        assert_eq!(lb.entries[1].label.as_deref(), Some("red"));

        lb.set_label(Some("red"));
        assert_eq!(
            lb.entries[0].label, None,
            "a shared label could not be cleared"
        );
        assert_eq!(lb.entries[1].label, None);

        lb.entries[0].label = Some("red".to_owned());
        lb.entries[1].label = Some("blue".to_owned());
        lb.set_label(Some("red"));
        assert_eq!(lb.entries[0].label.as_deref(), Some("red"));
        assert_eq!(
            lb.entries[1].label.as_deref(),
            Some("red"),
            "a mixed selection was toggled instead of assigned"
        );
    }

    #[test]
    fn rating_a_tile_does_not_select_it() {
        // Two gestures wearing one. The star row is drawn *on* the tile it rates, so a
        // click there already says which frame it means — moving the selection to say it
        // again lit the ruby ring on every tile the maintainer rated. `set_rating_at` is what
        // separates them; `set_rating` still reads the selection, because from the
        // keyboard that is the only thing saying which frame you mean.
        let mut lb = grid_of(4);
        lb.selected = Some(0);

        lb.set_rating_at(2, 4);
        assert_eq!(
            lb.entries[2].rating, 4,
            "the clicked tile was not the one rated"
        );
        assert_eq!(lb.selected, Some(0), "rating moved the selection");

        lb.set_label_at(3, Some("red"));
        assert_eq!(lb.entries[3].label.as_deref(), Some("red"));
        assert_eq!(lb.selected, Some(0), "labelling moved the selection");

        // The keyboard path still goes through the selection, which is the whole reason
        // the pair exists rather than one function.
        lb.set_rating(5);
        assert_eq!(
            lb.entries[0].rating, 5,
            "the keyboard path stopped following the ring"
        );
    }

    #[test]
    fn shift_arrow_extends_the_run_and_leaves_the_anchor_alone() {
        let mut lb = grid_of(6);
        lb.selected = Some(2);

        lb.step_selecting(true);
        assert_eq!(
            lb.selected,
            Some(2),
            "the anchor moved — a run has nothing to measure from"
        );
        assert_eq!(
            lb.batch,
            [2, 3].into_iter().collect(),
            "expected the run 2..=3"
        );

        lb.step_selecting(true);
        assert_eq!(
            lb.batch,
            [2, 3, 4].into_iter().collect(),
            "the run did not grow"
        );

        // **Walking back shrinks it**, because the run is recomputed from the two ends
        // rather than accumulated. Growing on the way out and staying grown on the way
        // back is the bug this guards.
        lb.step_selecting(false);
        assert_eq!(
            lb.batch,
            [2, 3].into_iter().collect(),
            "walking back did not shrink the run"
        );

        // And past the anchor it extends the other way.
        for _ in 0..3 {
            lb.step_selecting(false);
        }
        assert_eq!(lb.selected, Some(2), "the anchor still must not have moved");
        assert_eq!(
            lb.batch,
            [0, 1, 2].into_iter().collect(),
            "expected the run 0..=2"
        );
    }

    #[test]
    fn a_plain_arrow_collapses_the_selection() {
        // A plain click clears the batch; a plain arrow has to agree with it, or one
        // press moves the ring while five tiles stay lit somewhere else and the next
        // rating lands on all six.
        let mut lb = grid_of(6);
        lb.selected = Some(1);
        lb.step_selecting(true);
        lb.step_selecting(true);
        assert_eq!(lb.batch.len(), 3);

        lb.step(true);
        assert!(lb.batch.is_empty(), "a plain arrow left the batch behind");
        assert_eq!(
            lb.selected,
            Some(2),
            "and it should still have moved the ring"
        );
        assert!(
            lb.head.is_none(),
            "the moving end must go back to the anchor"
        );
    }

    #[test]
    fn shift_arrow_with_nothing_selected_puts_the_anchor_down() {
        // There is no run to measure without an anchor, so the first press behaves as a
        // plain step rather than doing nothing.
        let mut lb = grid_of(4);
        assert_eq!(lb.selected, None);
        lb.step_selecting(true);
        assert_eq!(lb.selected, lb.visible.first().copied());
    }

    #[test]
    fn the_grid_scrolls_to_the_moving_end_not_the_anchor() {
        // `follow` scrolls to `follow_target`. During a shift-run the anchor stands
        // still, so following it would drag the view backwards as the selection grew.
        let mut lb = grid_of(6);
        lb.selected = Some(0);
        lb.step_selecting(true);
        lb.step_selecting(true);
        assert_eq!(
            lb.follow_target(),
            Some(2),
            "the view would follow the anchor"
        );
    }

    #[test]
    fn folders_sort_to_the_front_in_both_directions() {
        // The point of lifting them after the reversal: the way out of a folder should
        // be in the same corner whichever order you are reading its contents in.
        let mut lb = Lightbox::new();
        lb.show_folders = true;
        lb.entries = vec![
            Entry {
                name: "b.dng".into(),
                ext: "b".into(),
                ..folder_like()
            },
            Entry {
                path: PathBuf::from("/x/sub"),
                name: "sub".into(),
                kind: Kind::Folder,
                ext: String::new(),
                ..folder_like()
            },
            Entry {
                name: "a.dng".into(),
                ext: "a".into(),
                ..folder_like()
            },
        ];
        lb.sort = Sort::Filename;

        lb.descending = false;
        lb.reindex();
        assert_eq!(
            lb.entries[lb.visible[0] as usize].kind,
            Kind::Folder,
            "ascending"
        );

        lb.descending = true;
        lb.reindex();
        assert_eq!(
            lb.entries[lb.visible[0] as usize].kind,
            Kind::Folder,
            "descending"
        );
    }

    #[test]
    fn a_cache_size_reads_as_something_a_person_can_act_on() {
        assert_eq!(cache_label(0), "empty");
        assert_eq!(cache_label(512), "512 bytes");
        assert_eq!(cache_label(2048), "2 KB");
        assert_eq!(cache_label(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(cache_label(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn purging_takes_the_files_and_leaves_the_directory() {
        // The directory has to survive, because `cache_dir` calls `create_dir_all` and
        // a purge racing that would be a cache that sometimes cannot be written to.
        // Subdirectories are skipped entirely — nothing writes one, and a recursive
        // delete under a path built from `settings::dir()` is not worth the feature.
        let Some(dir) = cache_dir() else { return };
        let marker = dir.join("purge-test.jpg");
        let keep = dir.join("purge-test-subdir");
        std::fs::write(&marker, vec![0u8; 4096]).unwrap();
        std::fs::create_dir_all(&keep).unwrap();

        assert!(
            cache_bytes() >= 4096,
            "the file we just wrote is not being counted"
        );
        let freed = purge_cache();
        assert!(
            freed >= 4096,
            "freed {freed}, expected at least the 4 KB we wrote"
        );
        assert!(!marker.exists(), "the tile survived the purge");
        assert!(dir.exists(), "the cache directory itself was removed");
        assert!(keep.exists(), "a subdirectory was removed");

        let _ = std::fs::remove_dir_all(&keep);
    }

    #[test]
    fn the_cache_name_follows_the_file() {
        // Same file, same name; a touched file, a different one. This is the whole
        // contract of the disk cache.
        let dir = std::env::temp_dir().join(format!("monopro-lb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.dng");
        std::fs::write(&f, b"one").unwrap();

        let m1 = std::fs::metadata(&f).unwrap();
        let n1 = cache_name(&f, &m1);
        assert_eq!(
            n1,
            cache_name(&f, &std::fs::metadata(&f).unwrap()),
            "stable for an unchanged file"
        );

        std::fs::write(&f, b"different length").unwrap();
        let n2 = cache_name(&f, &std::fs::metadata(&f).unwrap());
        assert_ne!(n1, n2, "an edited file must not read its old tile");

        // A different path is a different tile even with identical contents.
        let g = dir.join("b.dng");
        std::fs::write(&g, b"different length").unwrap();
        assert_ne!(n2, cache_name(&g, &std::fs::metadata(&g).unwrap()));

        assert!(n1.ends_with(".jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_round_trips_and_the_second_visit_is_the_cheap_one() {
        // The whole reason the disk cache exists, end to end: extract a tile from a
        // real raw, write it, read it back, and get the same picture. The JPEG hop
        // is the part that could silently produce a transposed or recoloured tile
        // and still "work".
        //
        // Skips when the raws are not there, like its neighbours in `raw-core`.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(dir.as_path()) else {
            return;
        };
        let Some(raw) = entries
            .flatten()
            .map(|e| e.path())
            .find(|p| is_raw(p) && raw_core::preview::tile(p, None).is_some())
        else {
            return;
        };

        let cache = std::env::temp_dir().join(format!("monopro-lb-cache-{}", std::process::id()));
        std::fs::create_dir_all(&cache).unwrap();

        let t = std::time::Instant::now();
        let cold =
            thumbnail(&raw, Some(&cache), false, None).expect("a tile from a raw that has one");
        let cold_ms = t.elapsed().as_secs_f64() * 1e3;

        let files: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "the cold pass writes exactly one tile");
        assert!(
            files[0].path().extension().is_some_and(|e| e == "jpg"),
            "and no .part is left behind"
        );

        let t = std::time::Instant::now();
        let warm = thumbnail(&raw, Some(&cache), false, None).expect("the cached tile");
        let warm_ms = t.elapsed().as_secs_f64() * 1e3;

        assert_eq!(
            (cold.w, cold.h),
            (warm.w, warm.h),
            "the cache must not reshape the tile"
        );
        assert_eq!(warm.data.len(), warm.pixels() * 3);

        // JPEG is lossy, so this is a likeness test, not an equality one. A wrong
        // orientation or a channel swap moves the mean far past this.
        let mean: f64 = cold
            .data
            .iter()
            .zip(warm.data.iter())
            .map(|(a, b)| a.abs_diff(*b) as f64)
            .sum::<f64>()
            / cold.data.len() as f64;
        assert!(
            mean < 6.0,
            "cached tile differs by {mean:.1}/255 — that is not the same picture"
        );

        eprintln!(
            "{}: cold {cold_ms:.1} ms, warm {warm_ms:.1} ms",
            raw.file_name().unwrap().to_string_lossy()
        );

        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn the_selection_does_not_follow_you_into_another_folder() {
        // It is an index, and the folder it indexes into is retired wholesale on the
        // way out — so keeping it would put the ring on whatever happens to sit in
        // that position in the folder you arrived at.
        let mut lb = Lightbox::new();
        lb.entries = vec![
            Entry {
                path: PathBuf::from("/x/a.dng"),
                name: "a.dng".into(),
                edited: false,
                rating: 0,
                label: None,
                ext: "dng".into(),
                kind: Kind::Picture,
                developed: None,
                captured: None,
                orientation: None,
            },
            Entry {
                path: PathBuf::from("/x/b.dng"),
                name: "b.dng".into(),
                edited: false,
                rating: 0,
                label: None,
                ext: "dng".into(),
                kind: Kind::Picture,
                developed: None,
                captured: None,
                orientation: None,
            },
        ];
        lb.selected = Some(1);
        lb.open_folder(Path::new("/nonexistent-folder-for-a-test"));
        assert_eq!(lb.selected, None);
    }

    #[test]
    fn the_edited_badge_follows_the_sidecar() {
        // The badge means "worked on", and what it reads is whether a `.mono.xmp`
        // sits beside the file — `raw_core::sidecar::path_for`'s convention, not a
        // second guess at the naming.
        let dir = std::env::temp_dir().join(format!("monopro-lb-edited-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let worked = dir.join("worked.dng");
        let untouched = dir.join("untouched.dng");
        std::fs::write(&worked, b"raw").unwrap();
        std::fs::write(&untouched, b"raw").unwrap();
        std::fs::write(raw_core::sidecar::path_for(&worked), b"<xmp/>").unwrap();

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(lb.entries.len(), 2, "both raws are listed");
        let badge = |n: &str| {
            lb.entries
                .iter()
                .find(|e| e.name == n)
                .expect("listed")
                .edited
        };
        assert!(badge("worked.dng"), "a file with a sidecar is marked");
        assert!(!badge("untouched.dng"), "and one without is not");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ refresh

    /// A folder of raw-named files, opened in a fresh Lightbox.
    fn opened(tag: &str, names: &[&str]) -> (Lightbox, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("monopro-lb-relist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), b"not really a raw").unwrap();
        }
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        (lb, dir)
    }

    fn relisted(lb: &mut Lightbox, dir: &Path) -> Reconciled {
        lb.reconcile(list_entries(
            dir,
            false,
            lb.show_folders,
            lb.show_other_files,
        ))
    }

    fn index_of(lb: &Lightbox, name: &str) -> u32 {
        lb.entries
            .iter()
            .position(|e| e.name == name)
            .expect("entry is listed") as u32
    }

    fn name_at(lb: &Lightbox, idx: u32) -> &str {
        &lb.entries[idx as usize].name
    }

    #[test]
    fn a_refresh_that_finds_nothing_new_touches_nothing() {
        let (mut lb, dir) = opened("same", &["a.dng", "b.dng", "c.dng"]);
        lb.tiles.insert(0, Tile::Missing);
        lb.tiles.insert(1, Tile::Pending);
        lb.selected = Some(2);
        let before = lb.generation;

        assert_eq!(relisted(&mut lb, &dir), Reconciled::default());
        assert_eq!(
            lb.generation, before,
            "no change must not retire a single tile"
        );
        assert_eq!(lb.tiles.len(), 2, "not even the pending one");
        assert_eq!(lb.selected, Some(2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_files_join_without_moving_anything_already_there() {
        let (mut lb, dir) = opened("added", &["a.dng", "b.dng", "c.dng"]);
        for i in 0..3 {
            lb.tiles.insert(i, Tile::Missing);
        }
        lb.selected = Some(index_of(&lb, "b.dng"));
        std::fs::write(dir.join("0-first-by-name.dng"), b"x").unwrap();

        let change = relisted(&mut lb, &dir);
        assert_eq!(
            change,
            Reconciled {
                added: 1,
                removed: 0
            }
        );
        assert_eq!(
            lb.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["a.dng", "b.dng", "c.dng", "0-first-by-name.dng"],
            "survivors keep their index; the newcomer goes on the end"
        );
        assert_eq!(lb.tiles.len(), 3, "every thumbnail already drawn is kept");
        assert_eq!(name_at(&lb, lb.selected.unwrap()), "b.dng");
        assert_eq!(
            lb.visible.first().map(|i| name_at(&lb, *i)),
            Some("0-first-by-name.dng"),
            "and the sort, not the storage order, decides where it shows"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_removed_file_takes_its_own_state_and_the_rest_follow_their_files() {
        let (mut lb, dir) = opened("removed", &["a.dng", "b.dng", "c.dng", "d.dng"]);
        let [a, b, c, d] = ["a.dng", "b.dng", "c.dng", "d.dng"].map(|n| index_of(&lb, n));
        for i in [a, b, c, d] {
            lb.tiles.insert(i, Tile::Missing);
        }
        lb.selected = Some(b);
        lb.head = Some(d);
        lb.preview = Some(c);
        lb.batch = [a, b, d].into_iter().collect();
        std::fs::remove_file(dir.join("b.dng")).unwrap();

        let change = relisted(&mut lb, &dir);
        assert_eq!(
            change,
            Reconciled {
                added: 0,
                removed: 1
            }
        );
        assert_eq!(lb.entries.len(), 3);
        assert_eq!(lb.tiles.len(), 3, "b's tile went with it");
        assert_eq!(lb.selected, None, "the selection was the file that left");
        assert_eq!(name_at(&lb, lb.head.unwrap()), "d.dng");
        assert_eq!(name_at(&lb, lb.preview.unwrap()), "c.dng");
        let mut batch: Vec<&str> = lb.batch.iter().map(|i| name_at(&lb, *i)).collect();
        batch.sort();
        assert_eq!(batch, ["a.dng", "d.dng"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_retires_the_generation_and_asks_again_for_pending_tiles() {
        // A job running under an old index would land on whichever file holds that
        // index now. Retiring the generation is what makes `collect` drop it.
        let (mut lb, dir) = opened("pending", &["a.dng", "b.dng"]);
        lb.tiles.insert(0, Tile::Missing);
        lb.tiles.insert(1, Tile::Pending);
        let before = lb.generation;
        std::fs::remove_file(dir.join("a.dng")).unwrap();

        relisted(&mut lb, &dir);
        assert_ne!(lb.generation, before);
        assert!(
            lb.tiles.is_empty(),
            "a's tile left with a, and b's pending one is forgotten so the grid asks again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drive a background re-read to completion the way the frame loop does.
    fn finish_relist(lb: &mut Lightbox) {
        let ctx = egui::Context::default();
        let start = std::time::Instant::now();
        while lb.relist.is_some() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(10),
                "re-read hung"
            );
            lb.collect(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn refresh_finds_new_files_in_the_background_and_says_so() {
        let (mut lb, dir) = opened("manual", &["a.dng"]);
        std::fs::write(dir.join("b.dng"), b"x").unwrap();
        std::fs::write(dir.join("c.dng"), b"x").unwrap();

        lb.refresh(true);
        finish_relist(&mut lb);
        assert_eq!(lb.entries.len(), 3);
        assert_eq!(lb.action_note.as_deref(), Some("2 new files"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listing_read_before_the_app_changed_the_entries_is_dropped() {
        // A rename, a search or a new folder all retire the generation. A listing
        // read before one of them describes a folder that is no longer on screen —
        // applied, it would put a renamed file's old name back.
        let (mut lb, dir) = opened("stale", &["a.dng"]);
        std::fs::write(dir.join("b.dng"), b"x").unwrap();
        lb.refresh(true);
        lb.generation = lb.generation.wrapping_add(1);
        finish_relist(&mut lb);
        assert_eq!(lb.entries.len(), 1, "the stale listing was not applied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn automatic_refreshes_are_spaced_out_and_the_command_is_not() {
        let (mut lb, dir) = opened("throttle", &["a.dng"]);
        lb.refresh(false);
        finish_relist(&mut lb);
        lb.refresh(false);
        assert!(
            lb.relist.is_none(),
            "a second focus straight after is not a second read"
        );
        lb.refresh(true);
        assert!(lb.relist.is_some(), "asking for it is always honoured");
        finish_relist(&mut lb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder with one raw-named file in it, for the metadata tests.
    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("monopro-lb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("frame.dng");
        std::fs::write(&raw, b"not really a raw").unwrap();
        (dir, raw)
    }

    #[test]
    fn lightbox_exif_uses_the_develop_rows_without_print_or_name() {
        let dir =
            std::env::temp_dir().join(format!("monopro-lb-exif-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        image::RgbImage::new(2, 1).save(&path).unwrap();

        let facts = read_exif(&path);
        let labels: Vec<&str> = facts
            .iter()
            .flat_map(|facts| facts.rows.iter().map(|(label, _)| label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                "Camera",
                "Lens",
                "Date/Time",
                "Focal",
                "ISO",
                "Aperture",
                "Shutter",
                "Metering",
                "WB",
                "Pixels",
                "File",
                "Format",
                "Color Space",
            ]
        );
        assert!(!labels.contains(&"Print"));
        assert!(!labels.contains(&"Name"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_panel_dropped_into_the_lightbox_row_arrives_at_a_usable_width() {
        // the maintainer: snapping Folders — or any Lightbox panel — to the right edge made it
        // disappear. It was not disappearing; it was arriving a hairline wide.
        // `Tiles::insert_at` gives a dropped tile no share and `Shares` answers 1.0 for
        // a tile it has no entry for, so against this tree's shipped `left: 240` and
        // `grid: 900` the new pane got one part in eleven hundred. Develop has called
        // `normalise_shares` since the same bug was found there; the Lightbox never did.
        let mut tree = default_tree();
        let root = tree.root.expect("a root");

        // **The normalise runs before the drop, not after**, and that is the whole
        // mechanism: scaling preserves ratios, so it cannot rescue a share that is
        // already 1-in-1141. What it does is keep the row's shares *averaging* 1.0, so
        // that the 1.0 `Shares` invents for a share-less tile means "one of n" instead
        // of "a thousandth". Hence the call site sits ahead of `tree.ui`.
        crate::layout::normalise_shares(&mut tree);

        // What a drop on the right edge does, as `egui_tiles` does it: append to the
        // root row without a share.
        let orphan = tree.tiles.insert_pane(Pane::Exif);
        let egui_tiles::Tile::Container(egui_tiles::Container::Linear(row)) =
            tree.tiles.get_mut(root).expect("a root row")
        else {
            panic!("the root is not a linear container")
        };
        row.children.push(orphan);

        let share_of = |tree: &egui_tiles::Tree<Pane>, id| {
            let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
                tree.tiles.get(tree.root.expect("a root"))
            else {
                panic!("no root row")
            };
            let total: f32 = row.children.iter().map(|c| row.shares[*c]).sum();
            row.shares[id] / total
        };

        let got = share_of(&tree, orphan);

        // The bug, measured on the same tree without the normalise, so this test would
        // fail without the fix rather than merely pass with it.
        let mut unfixed = default_tree();
        let stray = unfixed.tiles.insert_pane(Pane::Exif);
        let unfixed_root = unfixed.root.expect("a root");
        if let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
            unfixed.tiles.get_mut(unfixed_root)
        {
            row.children.push(stray);
        }
        assert!(
            share_of(&unfixed, stray) < 0.005,
            "the premise is wrong: the dropped pane was visible without the fix"
        );

        assert!(
            got > 0.15,
            "a dropped panel came back at {:.1}% of the row",
            got * 100.0
        );
        // And the shipped proportions survive it: normalising scales, it does not
        // equalise, so the folder column does not jump to a third of the window.
        let folders = tree
            .tiles
            .iter()
            .find_map(|(id, t)| matches!(t, egui_tiles::Tile::Container(_)).then_some(*id))
            .expect("a container");
        let _ = folders;
        let grid = tree
            .tiles
            .iter()
            .find_map(|(id, t)| matches!(t, egui_tiles::Tile::Pane(Pane::Grid)).then_some(*id))
            .expect("the grid");
        assert!(
            share_of(&tree, grid) > share_of(&tree, orphan),
            "the grid lost its lead"
        );
    }

    #[test]
    fn a_lightbox_panel_moved_right_matches_the_left_panel_width() {
        // The settled shape in the maintainer's screenshot: Metadata on the left, the grid in
        // the middle and Search newly detached on the right. egui_tiles initially
        // gives those three equal shares; Lightbox must put both side panels back at
        // the width they had before the drop and take the difference from the grid.
        let mut tiles = egui_tiles::Tiles::default();
        let left = tiles.insert_pane(Pane::Exif);
        let grid = tiles.insert_pane(Pane::Grid);
        let right = tiles.insert_pane(Pane::Search);
        let row =
            egui_tiles::Linear::new(egui_tiles::LinearDir::Horizontal, vec![left, grid, right]);
        let root = tiles.insert_container(egui_tiles::Container::Linear(row));

        let mut lb = Lightbox::new();
        lb.tree = egui_tiles::Tree::new("lightbox-width-test", root, tiles);
        lb.panel_sizes.insert(root, egui::vec2(1400.0, 900.0));
        lb.panel_sizes.insert(left, egui::vec2(240.0, 900.0));
        lb.panel_sizes.insert(grid, egui::vec2(1159.0, 900.0));
        lb.panel_sizes.insert(right, egui::vec2(240.0, 900.0));
        lb.keep_panel_widths();

        let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(row))) =
            lb.tree.tiles.get(root)
        else {
            panic!("the root stopped being a row")
        };
        let total: f32 = row.children.iter().map(|child| row.shares[*child]).sum();
        let available = 1400.0 - 2.0 * crate::layout::GAP;
        let width = |child| row.shares[child] / total * available;
        assert!(
            (width(left) - width(right)).abs() < 1.0,
            "Metadata is {:.1}pt but Search is {:.1}pt",
            width(left),
            width(right)
        );
        assert!(
            (width(right) - 240.0).abs() < 1.0,
            "Search landed at {:.1}pt rather than its remembered width",
            width(right)
        );
        assert!(width(grid) > width(right) * 3.0, "the grid did not flex");
    }

    #[test]
    fn a_rating_survives_a_restart() {
        // The contract of the settled decision: the sidecar is the truth, so
        // re-reading the folder from disk is the whole test — no cache is consulted
        // and none is allowed to answer.
        let (dir, _raw) = scratch("rating");
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        assert_eq!(lb.set_rating(4), None, "the write should succeed");
        assert_eq!(lb.set_label(Some("green")), None);

        // A different Lightbox, as a fresh launch would have.
        let mut later = Lightbox::new();
        later.open_folder(&dir);
        let e = &later.entries[0];
        assert_eq!(e.rating, 4);
        assert_eq!(e.label.as_deref(), Some("green"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copied_develop_settings_apply_to_a_batch_without_copying_metadata_or_dodge_burn() {
        let dir = folder_of(
            "copy-develop",
            &[
                ("a-source.dng", 5, Some("red")),
                ("b-target.dng", 2, Some("blue")),
                ("c-target.dng", 3, Some("green")),
            ],
        );
        let source = dir.join("a-source.dng");
        let mut source_params = raw_core::Params::default();
        source_params.exposure.ev = 1.25;
        source_params
            .dodgeburn
            .instances
            .push(raw_core::Instance::new(
                raw_core::Sign::Dodge,
                "Source dodge".into(),
            ));
        let source_metadata = raw_core::sidecar::Metadata {
            rating: Some(5),
            label: Some("red".into()),
            creator: Some("Source creator".into()),
            ..Default::default()
        };
        raw_core::sidecar::write(&source, &source_params, &source_metadata).unwrap();

        for (name, creator, local_name) in [
            ("b-target.dng", "First destination", "Target burn B"),
            ("c-target.dng", "Second destination", "Target burn C"),
        ] {
            let path = dir.join(name);
            let mut sidecar = raw_core::sidecar::read(&path)
                .ok()
                .expect("metadata sidecar");
            sidecar
                .params
                .dodgeburn
                .instances
                .push(raw_core::Instance::new(
                    raw_core::Sign::Burn,
                    local_name.into(),
                ));
            let metadata = raw_core::sidecar::Metadata {
                creator: Some(creator.into()),
                ..sidecar.metadata
            };
            raw_core::sidecar::write(&path, &sidecar.params, &metadata).unwrap();
        }

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        assert!(lb.copy_settings().is_ok());
        lb.selected = Some(1);
        lb.batch = [1, 2].into_iter().collect();
        assert!(lb.paste_settings().is_ok());

        for (name, rating, label, creator, local_name) in [
            (
                "b-target.dng",
                2,
                "blue",
                "First destination",
                "Target burn B",
            ),
            (
                "c-target.dng",
                3,
                "green",
                "Second destination",
                "Target burn C",
            ),
        ] {
            let sidecar = raw_core::sidecar::read(&dir.join(name))
                .ok()
                .expect("pasted sidecar");
            let mut expected_params = source_params.clone();
            expected_params.dodgeburn = Default::default();
            expected_params
                .dodgeburn
                .instances
                .push(raw_core::Instance::new(
                    raw_core::Sign::Burn,
                    local_name.into(),
                ));
            assert_eq!(sidecar.params, expected_params);
            assert_eq!(sidecar.metadata.rating, Some(rating));
            assert_eq!(sidecar.metadata.label.as_deref(), Some(label));
            assert_eq!(sidecar.metadata.creator.as_deref(), Some(creator));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_iptc_field_applies_to_the_selection_without_flattening_other_fields() {
        let dir = folder_of("iptc-batch", &[("a.dng", 0, None), ("b.dng", 0, None)]);
        for (name, caption) in [("a.dng", "First caption"), ("b.dng", "Second caption")] {
            let path = dir.join(name);
            let metadata = raw_core::sidecar::Metadata {
                description: Some(caption.into()),
                ..Default::default()
            };
            raw_core::sidecar::write(&path, &Default::default(), &metadata).unwrap();
        }

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        lb.batch = [0, 1].into_iter().collect();
        let paths = lb.iptc_paths();
        lb.iptc = Some(IptcDraft::load(paths));
        let creator = raw_core::sidecar::IptcField::Creator as usize;
        let draft = lb.iptc.as_mut().unwrap();
        draft.values[creator] = "Example Photographer".into();
        draft.dirty[creator] = true;
        lb.commit_iptc_slots(&[creator]);

        for (name, caption) in [("a.dng", "First caption"), ("b.dng", "Second caption")] {
            let sidecar = raw_core::sidecar::read(&dir.join(name))
                .ok()
                .expect("sidecar parses");
            assert_eq!(
                sidecar.metadata.creator.as_deref(),
                Some("Example Photographer")
            );
            assert_eq!(sidecar.metadata.description.as_deref(), Some(caption));
            assert!(
                !sidecar.is_developed(),
                "cataloguing metadata lit the developed mark"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_metadata_template_sets_clears_and_preserves_fields_across_a_selection() {
        use crate::iptc_templates::{Action, Store, Template};

        let dir = folder_of(
            "iptc-template-batch",
            &[("a.dng", 0, None), ("b.dng", 0, None)],
        );
        for (name, city) in [("a.dng", "New York"), ("b.dng", "Paris")] {
            let path = dir.join(name);
            let mut params = raw_core::Params::default();
            params.exposure.ev = 0.75;
            let metadata = raw_core::sidecar::Metadata {
                creator: Some("Old credit".into()),
                description: Some("Remove this caption".into()),
                city: Some(city.into()),
                ..Default::default()
            };
            raw_core::sidecar::write(&path, &params, &metadata).unwrap();
        }

        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            raw_core::sidecar::IptcField::Creator.key().into(),
            Action::Set {
                value: "Studio Name".into(),
            },
        );
        fields.insert(
            raw_core::sidecar::IptcField::Description.key().into(),
            Action::Clear,
        );

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        lb.batch = [0, 1].into_iter().collect();
        lb.iptc_templates = Store {
            templates: vec![Template {
                name: "Desk credit".into(),
                fields,
            }],
        };
        lb.apply_iptc_template(0);

        for (name, city) in [("a.dng", "New York"), ("b.dng", "Paris")] {
            let sidecar = raw_core::sidecar::read(&dir.join(name))
                .ok()
                .expect("sidecar parses");
            assert_eq!(sidecar.params.exposure.ev, 0.75, "develop state moved");
            assert_eq!(sidecar.metadata.creator.as_deref(), Some("Studio Name"));
            assert_eq!(sidecar.metadata.description, None);
            assert!(
                sidecar
                    .metadata
                    .cleared
                    .iter()
                    .any(|key| key == raw_core::sidecar::IptcField::Description.key()),
                "the embedded caption could reappear"
            );
            assert_eq!(sidecar.metadata.city.as_deref(), Some(city));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_exif_pane_keeps_its_saved_identity_but_is_named_metadata() {
        assert_eq!(Pane::Exif.label(), "METADATA");
    }

    #[test]
    fn the_edited_mark_answers_developed_and_not_merely_catalogued() {
        // the maintainer: a star should not put the amber rule on a frame. It did, because
        // `edited` was reading "a sidecar exists" and a rating writes one — so the mark
        // lit on exactly the pass it is least use during, going through a folder
        // rating things. It has to mean *developed*, which is the sidecar's `params`
        // against their defaults. See `Sidecar::is_developed`.
        let (dir, raw) = scratch("editedmark");

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        assert!(!lb.entries[0].edited, "an untouched frame starts unmarked");

        // Rating and labelling write a sidecar and must leave the mark off — both in
        // the live grid, which `commit` updates, and on a fresh read of the folder.
        assert_eq!(lb.set_rating(4), None);
        assert_eq!(lb.set_label(Some("green")), None);
        assert!(!lb.entries[0].edited, "a star lit the edited mark");
        let mut fresh = Lightbox::new();
        fresh.open_folder(&dir);
        assert_eq!(fresh.entries[0].rating, 4, "the rating did not survive");
        assert!(
            !fresh.entries[0].edited,
            "a star lit the edited mark after a re-read"
        );

        // A develop edit does light it, keeping the rating that was already there —
        // which is the case that would break if the two facts were read from one bit.
        let mut params = raw_core::Params::default();
        params.exposure.ev = 1.25;
        let meta = raw_core::sidecar::Metadata {
            rating: Some(4),
            label: Some("green".into()),
            ..Default::default()
        };
        raw_core::sidecar::write(&raw, &params, &meta).unwrap();

        let mut after = Lightbox::new();
        after.open_folder(&dir);
        assert!(after.entries[0].edited, "a developed frame is not marked");
        assert_eq!(after.entries[0].rating, 4, "developing dropped the rating");

        // And `refresh_edited` — the mode-switch path, which stats first and parses
        // only what moved — has to reach the same answer as the full re-read.
        let mut switched = Lightbox::new();
        switched.open_folder(&dir);
        switched.entries[0].edited = false;
        switched.entries[0].developed = None;
        switched.refresh_edited();
        assert!(
            switched.entries[0].edited,
            "the mode-switch path disagrees with the listing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_star_comes_off_without_leaving_a_nought() {
        // `xmp:Rating="0"` is a rating of nought, which is a different statement
        // from "unrated". Taking the star back off must leave the attribute absent.
        let (dir, raw) = scratch("unrate");
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        lb.set_rating(3);
        lb.set_rating(3); // the same star again clears it
        assert_eq!(lb.entries[0].rating, 0);

        let text = std::fs::read_to_string(raw_core::sidecar::path_for(&raw)).unwrap();
        assert!(
            !text.contains("xmp:Rating"),
            "an unrated file still declares a rating:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_label_is_carried_rather_than_normalised() {
        // The format's label is free text and other applications write their own
        // vocabulary. This app shows no dot for a word it does not know — and must
        // not rewrite it to one of its six on the way past.
        let (dir, raw) = scratch("foreign");
        let side = raw_core::sidecar::path_for(&raw);
        let mut meta = raw_core::sidecar::Metadata {
            label: Some("Zweite Wahl".into()),
            ..Default::default()
        };
        raw_core::sidecar::write(&raw, &Default::default(), &meta).unwrap();

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(
            lb.entries[0].label.as_deref(),
            Some("Zweite Wahl"),
            "read as found"
        );
        assert!(
            theme::label_colour("Zweite Wahl").is_none(),
            "and no dot is invented for it"
        );

        // Rating it must not disturb the label somebody else wrote.
        lb.selected = Some(0);
        lb.set_rating(2);
        meta = raw_core::sidecar::read(&raw)
            .ok()
            .expect("still parses")
            .metadata;
        assert_eq!(
            meta.label.as_deref(),
            Some("Zweite Wahl"),
            "the foreign label was rewritten"
        );
        assert_eq!(meta.rating, Some(2));
        assert!(side.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sidecar_that_will_not_parse_is_refused_rather_than_replaced() {
        // It holds work this app cannot see. Writing a two-field replacement over it
        // would destroy exactly what the corruption is hiding.
        let (dir, raw) = scratch("corrupt");
        let side = raw_core::sidecar::path_for(&raw);
        std::fs::write(&side, b"<xmp>truncated...").unwrap();

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert!(
            lb.entries[0].edited,
            "present-and-unreadable is still worked on"
        );

        lb.selected = Some(0);
        assert!(
            lb.set_rating(5).is_some(),
            "the write should have been refused, with a reason"
        );
        assert_eq!(
            std::fs::read_to_string(&side).unwrap(),
            "<xmp>truncated...",
            "the unreadable sidecar was overwritten anyway"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_five_labels_are_ordered_distinct_and_named_for_the_wire() {
        // The names go into `xmp:Label` verbatim, so they have to be stable, unique
        // and readable by another application.
        let mut seen = std::collections::HashSet::new();
        for (name, colour) in theme::LABELS {
            assert!(seen.insert(name), "{name} is in the palette twice");
            assert_eq!(
                name.to_lowercase(),
                name,
                "{name} must be lowercase for the wire"
            );
            assert_eq!(theme::label_colour(name), Some(colour));
        }
        assert_eq!(seen.len(), 5);
        assert_eq!(
            theme::LABELS.map(|(name, _)| name),
            ["magenta", "blue", "green", "yellow", "red"]
        );
        assert_eq!(
            theme::label_colour("cyan"),
            None,
            "retired cyan metadata should remain harmless foreign text"
        );
    }

    /// A folder of named raws with ratings and labels, for the sort and filter tests.
    fn folder_of(tag: &str, files: &[(&str, i32, Option<&str>)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("monopro-lb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, rating, label) in files {
            let p = dir.join(name);
            std::fs::write(&p, b"raw").unwrap();
            if *rating > 0 || label.is_some() {
                let meta = raw_core::sidecar::Metadata {
                    rating: (*rating > 0).then_some(*rating),
                    label: label.map(str::to_owned),
                    ..Default::default()
                };
                raw_core::sidecar::write(&p, &Default::default(), &meta).unwrap();
            }
        }
        dir
    }

    fn names(lb: &Lightbox) -> Vec<String> {
        lb.visible
            .iter()
            .map(|i| lb.entries[*i as usize].name.clone())
            .collect()
    }

    #[test]
    fn sorting_reorders_the_view_and_never_the_folder() {
        let dir = folder_of(
            "sort",
            &[("c.dng", 5, None), ("a.dng", 1, None), ("b.dng", 3, None)],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(
            names(&lb),
            ["a.dng", "b.dng", "c.dng"],
            "filename is the default"
        );

        // **Choosing Rating starts at the high end**, because that is the end the
        // question is asked from. the maintainer found the old behaviour by choosing the sort
        // and watching every frame he cared about go to the bottom.
        lb.set_sort(Sort::Rating);
        assert_eq!(names(&lb), ["c.dng", "b.dng", "a.dng"], "5, 3, 1");
        lb.set_descending(false);
        assert_eq!(names(&lb), ["a.dng", "b.dng", "c.dng"], "and flips");

        // `entries` is the folder as read and must never be permuted — the tile cache
        // is keyed by its indices, so a sort that moved it would scramble thumbnails.
        let underlying: Vec<&str> = lb.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            underlying,
            ["a.dng", "b.dng", "c.dng"],
            "the entry list was reordered"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unanswered_frame_sorts_last_whichever_way_the_sort_runs() {
        // The rule was written in `reindex` and only Label implemented it, and only
        // ascending — its key carried the flag, so reversing sent the unlabelled to the
        // top. Rating and Developed did not implement it at all, which is what put a
        // wall of unrated frames above everything the maintainer had actually rated.
        let dir = folder_of(
            "unanswered",
            &[
                ("plain.dng", 0, None),
                ("low.dng", 2, Some("blue")),
                ("high.dng", 5, Some("red")),
            ],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);

        lb.set_sort(Sort::Rating);
        assert_eq!(names(&lb), ["high.dng", "low.dng", "plain.dng"]);
        lb.set_descending(false);
        assert_eq!(
            names(&lb),
            ["low.dng", "high.dng", "plain.dng"],
            "the unrated frame came up with the reversal"
        );

        lb.set_sort(Sort::Label);
        assert_eq!(*names(&lb).last().unwrap(), "plain.dng");
        lb.set_descending(true);
        assert_eq!(
            *names(&lb).last().unwrap(),
            "plain.dng",
            "the unlabelled frame came up with the reversal"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_filters_select_what_they_say() {
        let dir = folder_of(
            "filter",
            &[
                ("none.dng", 0, None),
                ("two.dng", 2, Some("red")),
                ("four.dng", 4, Some("green")),
                ("five.dng", 5, None),
            ],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(lb.visible.len(), 4);

        lb.filters.stars = 4;
        lb.refilter();
        assert_eq!(names(&lb), ["five.dng", "four.dng"], "four stars and up");

        lb.filters.stars = 0;
        lb.filters.unrated = true;
        lb.refilter();
        assert_eq!(names(&lb), ["none.dng"]);

        lb.filters.unrated = false;
        lb.filters.labels = vec!["red".into(), "green".into()];
        lb.refilter();
        assert_eq!(
            names(&lb),
            ["four.dng", "two.dng"],
            "either label, not both"
        );

        lb.filters = Filters::default();
        lb.refilter();
        assert_eq!(lb.visible.len(), 4, "clearing gets the whole folder back");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrated_and_a_star_threshold_cannot_both_be_asked_for() {
        // They are `== 0` and `>= n`. Together they select nothing, which is a view
        // you can build and then not understand — so the footer clears one when you
        // choose the other, and this records why that is not fussiness.
        let f = Filters {
            unrated: true,
            stars: 3,
            ..Default::default()
        };
        let rated = Entry {
            path: PathBuf::from("/x/a.dng"),
            name: "a.dng".into(),
            edited: true,
            rating: 5,
            label: None,
            ext: "dng".into(),
            kind: Kind::Picture,
            orientation: None,
            developed: None,
            captured: None,
        };
        assert!(!f.admits(&rated), "a five-star file is not unrated");
        let unrated = Entry { rating: 0, ..rated };
        assert!(
            !f.admits(&unrated),
            "and an unrated one is below the threshold"
        );
    }

    #[test]
    fn a_manual_order_survives_a_restart_and_stays_out_of_the_photo_folder() {
        let dir = folder_of(
            "manual",
            &[("a.dng", 0, None), ("b.dng", 0, None), ("c.dng", 0, None)],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(names(&lb), ["a.dng", "b.dng", "c.dng"]);

        // Drag the last to the front. Dragging under any other sort switches the mode
        // to Manual rather than refusing — Bridge's behaviour, and the maintainer's call.
        lb.reorder(2, 0);
        assert_eq!(lb.sort, Sort::Manual, "a drag switches the sort");
        assert_eq!(names(&lb), ["c.dng", "a.dng", "b.dng"]);

        // A fresh launch, reading only from disk.
        let mut later = Lightbox::new();
        later.sort = Sort::Manual;
        later.open_folder(&dir);
        assert_eq!(
            names(&later),
            ["c.dng", "a.dng", "b.dng"],
            "the order did not survive"
        );

        // **And nothing was written into the photo directory.** The order lives in
        // the app's cache; the only files here should be raws and sidecars.
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            assert!(
                n.ends_with(".dng") || n.ends_with(".mono.xmp"),
                "{n} was written into somebody's photo folder"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manual_order_forgives_a_folder_that_moved_on() {
        // The consequence the settled decision names: the cache is keyed by folder,
        // so a folder whose contents changed has an order with holes in it. New files
        // sort to the end, missing ones are dropped on read, and neither is an error
        // to report.
        let dir = folder_of("holes", &[("a.dng", 0, None), ("b.dng", 0, None)]);
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.reorder(1, 0);
        assert_eq!(names(&lb), ["b.dng", "a.dng"]);

        std::fs::remove_file(dir.join("a.dng")).unwrap();
        std::fs::write(dir.join("z.dng"), b"raw").unwrap();
        lb.open_folder(&dir);
        assert_eq!(
            names(&lb),
            ["b.dng", "z.dng"],
            "known first, newcomers after"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_subfolder_sweep_reaches_down_and_the_plain_listing_does_not() {
        let dir = folder_of("deep", &[("top.dng", 0, None)]);
        let sub = dir.join("below");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("under.dng"), b"raw").unwrap();
        // Hidden folders are skipped — a photo tree has no use for what is in them.
        let hidden = dir.join(".cache");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("nope.dng"), b"raw").unwrap();

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(names(&lb), ["top.dng"], "off by default");

        lb.filters.subfolders = true;
        lb.reopen();
        assert_eq!(
            names(&lb),
            ["top.dng", "under.dng"],
            "and no hidden folder was walked"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_arrangement_holds_every_pane_exactly_once() {
        // What `tree_is_sound` checks on restore, checked against the default that
        // restore falls back to — a default failing its own soundness test would make
        // a bad stored tree unrecoverable.
        let tree = default_tree();
        assert!(tree_is_sound(&tree));

        let panes: Vec<Pane> = tree
            .tiles
            .tiles()
            .filter_map(|t| match t {
                egui_tiles::Tile::Pane(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert_eq!(
            panes.len(),
            Pane::ALL.len(),
            "a pane is missing or duplicated"
        );
        for want in Pane::ALL {
            assert!(
                panes.contains(&want),
                "{want:?} is not in the default layout"
            );
        }
    }

    #[test]
    fn a_stored_arrangement_this_build_cannot_run_is_thrown_away() {
        // The rule `layout::heal` sets: a stored layout is inherited forever unless
        // something checks it, so a tree this build cannot run is replaced by the
        // default rather than run badly. One arrangement lost on an upgrade beats a
        // mode that opens wrong every time.
        let mut tiles = egui_tiles::Tiles::default();
        let only = tiles.insert_pane(Pane::Grid);
        let lonely = egui_tiles::Tree::new("t", only, tiles);
        assert!(
            !tree_is_sound(&lonely),
            "a tree holding one pane is not runnable"
        );
        assert!(
            !tree_is_sound(&egui_tiles::Tree::<Pane>::empty("t")),
            "nor an empty one"
        );
    }

    #[test]
    fn a_pane_dropped_into_nowhere_comes_back() {
        // egui_tiles removes a pane dropped somewhere that is not a drop target, and
        // the maintainer watched panels vanish. The drag itself is fixed — the handle now
        // senses under the tile's own id, which is what makes the drop land — but a
        // layout you can destroy by aiming badly needs a floor under it as well.
        let mut lb = Lightbox::new();
        let id = lb
            .tree
            .tiles
            .iter()
            .find(|(_, t)| matches!(t, egui_tiles::Tile::Pane(p) if *p == Pane::Favorites))
            .map(|(id, _)| *id)
            .expect("favorites is in the default layout");
        lb.tree.tiles.remove(id);
        assert!(!tree_is_sound(&lb.tree), "the tree really is short a pane");

        lb.heal_tree();
        assert!(tree_is_sound(&lb.tree), "the pane did not come back");

        // And it came back where it can be seen and moved, not into limbo.
        let present: Vec<Pane> = lb
            .tree
            .tiles
            .tiles()
            .filter_map(|t| match t {
                egui_tiles::Tile::Pane(p) => Some(*p),
                _ => None,
            })
            .collect();
        assert!(present.contains(&Pane::Favorites));
        assert_eq!(
            present.len(),
            Pane::ALL.len(),
            "healing duplicated something"
        );
    }

    #[test]
    fn an_edited_tile_is_keyed_on_the_sidecar_and_preferred_when_asked_for() {
        // The whole point of the key: a raw does not change when you edit it, so a
        // tile keyed on the raw alone would be stale forever. The sidecar's mtime is
        // what moves.
        let dir = folder_of("editedtile", &[("a.dng", 0, None)]);
        let raw = dir.join("a.dng");
        let cache = dir.join("cache");
        std::fs::create_dir_all(&cache).unwrap();

        // No sidecar yet: nothing to key on, and nothing edited to show.
        let bare = folder_of("editedtile2", &[("b.dng", 0, None)]);
        assert!(edited_cache_name(&bare.join("b.dng"), &cache).is_none());

        // `folder_of` wrote a sidecar for a rated file; rate this one to get one.
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        lb.set_rating(3);

        let first = edited_cache_name(&raw, &cache).expect("there is a sidecar now");
        assert!(
            first.to_string_lossy().contains("-e4-"),
            "the name does not carry the edited-tile version and sidecar stamp"
        );

        // Store a tile, and it comes back in preference to the camera's.
        let (w, h) = (8u32, 4u32);
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 30, 30, 255]).collect();
        std::fs::write(&first, {
            // Go through the real writer, but into this test's cache directory: the
            // production one is keyed off the app's storage dir.
            let scale = 1.0f32;
            let _ = scale;
            let src = image::RgbaImage::from_raw(w, h, rgba.clone()).unwrap();
            let small = image::imageops::thumbnail(&src, w, h);
            let rgb: Vec<u8> = small
                .pixels()
                .flat_map(|p| [p.0[0], p.0[1], p.0[2]])
                .collect();
            let mut out = Vec::new();
            let enc = jpeg_encoder::Encoder::new(&mut out, 88);
            enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                .unwrap();
            out
        })
        .unwrap();

        let got = thumbnail(&raw, Some(&cache), true, None).expect("the edited tile");
        assert_eq!(
            (got.w, got.h),
            (w as usize, h as usize),
            "the camera tile was used instead"
        );
        // Red-ish, which the camera's thumbnail of a file containing the word "raw"
        // could not be — it has none.
        assert!(
            got.data[0] > got.data[1],
            "that is not the tile that was stored"
        );

        // And with the preference off, the edited tile is ignored entirely.
        assert!(
            thumbnail(&raw, Some(&cache), false, None).is_none(),
            "a fake raw has no camera tile"
        );

        // Touching the sidecar retires the tile: a new key, so a new file.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        lb.set_rating(5);
        let second = edited_cache_name(&raw, &cache).expect("still a sidecar");
        assert_ne!(first, second, "editing again did not retire the old tile");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn enabling_developed_previews_retires_camera_tiles_immediately() {
        let mut lb = Lightbox::new();
        let camera_generation = lb.generation;
        lb.tiles.insert(0, Tile::Missing);
        lb.set_xmp_thumbnails(true);
        assert!(lb.xmp_thumbnails);
        assert_ne!(
            lb.generation, camera_generation,
            "a queued camera thumbnail could still be accepted"
        );
        assert!(
            lb.tiles.is_empty(),
            "the grid kept the camera-mode tile after the preference changed"
        );
    }

    #[test]
    fn changing_gray_mode_retires_color_tiles_and_pending_decodes() {
        let mut lb = Lightbox::new();
        assert!(lb.grey, "a fresh Lightbox should follow its gray default");
        lb.tiles.insert(0, Tile::Pending);
        let gray_generation = lb.generation;

        lb.set_grey(false);

        assert!(!lb.grey);
        assert_ne!(
            lb.generation, gray_generation,
            "a queued gray thumbnail could still be accepted in color mode"
        );
        assert!(lb.tiles.is_empty());
    }

    #[test]
    fn returning_from_develop_retires_pending_camera_tiles() {
        let mut lb = Lightbox::new();
        let mut entry = entry_for(
            PathBuf::from("/a/frame-that-is-not-here.dng"),
            Kind::Picture,
        );
        // Pretend the folder was read while a sidecar existed. Its disappearance is
        // enough to exercise the same invalidation path as a newly written sidecar.
        entry.developed = Some(std::time::UNIX_EPOCH);
        lb.entries.push(entry);
        lb.tiles.insert(0, Tile::Pending);
        let before = lb.generation;

        lb.refresh_edited();

        assert_ne!(
            lb.generation, before,
            "the pending camera request could still be delivered"
        );
        assert!(lb.tiles.is_empty());
    }

    #[test]
    fn a_sidecar_written_while_the_grid_was_open_still_marks_the_frame_edited() {
        // The badge used to be answered from whenever the folder was last read, and
        // the only thing that re-read it was crossing back from Develop. A sidecar
        // written by another program — or by this app in a second window — therefore
        // stayed invisible for as long as Lightbox stayed up. The window regaining
        // focus now runs this; the test covers what that call has to find.
        let dir = std::env::temp_dir().join(format!("monopro-badge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let image = dir.join("frame.dng");

        let mut lb = Lightbox::new();
        lb.entries.push(entry_for(image.clone(), Kind::Picture));
        lb.refresh_edited();
        assert!(
            !lb.entries[0].edited,
            "no sidecar, yet the frame reads edited"
        );

        let mut params = raw_core::Params::default();
        params.exposure.ev = 1.0;
        raw_core::sidecar::write(&image, &params, &Default::default()).expect("sidecar");

        lb.refresh_edited();
        assert!(
            lb.entries[0].edited,
            "a sidecar that appeared after the folder was read went unnoticed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_developed_frames_without_a_tile_are_queued_for_rendering() {
        // The list is what makes browsing a folder of old edits work at all: the
        // capture on leaving Develop only ever covers the frame you just had open, so
        // everything edited on another day needs rendering from its sidecar.
        let dir = std::env::temp_dir().join(format!("monopro-want-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");

        let mut params = raw_core::Params::default();
        params.exposure.ev = 1.0;
        let developed = dir.join("edited.dng");
        std::fs::write(&developed, b"raw").unwrap();
        raw_core::sidecar::write(&developed, &params, &Default::default()).unwrap();

        // Rated but not edited: a sidecar exists, and it must not be queued.
        let rated = dir.join("rated.dng");
        std::fs::write(&rated, b"raw").unwrap();
        let meta = raw_core::sidecar::Metadata {
            rating: Some(3),
            ..Default::default()
        };
        raw_core::sidecar::write(&rated, &raw_core::Params::default(), &meta).unwrap();

        let plain = dir.join("plain.dng");
        std::fs::write(&plain, b"raw").unwrap();

        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.set_xmp_thumbnails(true);

        let mut queued: Vec<String> = Vec::new();
        while let Some((_, p)) = lb.developed_tile_wanted() {
            queued.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
        assert_eq!(
            queued,
            ["edited.dng"],
            "only the developed frame should be queued"
        );

        // Off, and nothing is wanted at all — the render is what the preference buys.
        lb.set_xmp_thumbnails(false);
        assert!(lb.developed_tile_wanted().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_favorite_is_kept_once_and_can_be_taken_back() {
        let mut lb = Lightbox::new();
        let a = Path::new("/x/shoot");
        assert!(!lb.is_favorite(a));

        lb.toggle_favorite(a);
        assert!(lb.is_favorite(a));
        assert_eq!(lb.favorites.len(), 1);

        // Keeping it twice is the same act as taking it back: one button whose
        // meaning depends on whether the folder is already in the list.
        lb.toggle_favorite(a);
        assert!(!lb.is_favorite(a));
        assert!(lb.favorites.is_empty());
    }

    #[test]
    fn rotating_writes_the_sidecar_without_marking_the_frame_edited() {
        // **the maintainer's call, 2026-09-06, reversing the earlier one.** The turn used to
        // live in the folder cache so that arranging a folder left no xmp beside the
        // picture. That held while every tile was a camera JPEG the grid rotated
        // itself — and stopped holding the moment a tile could be a *developed*
        // render, which carries Develop's orientation baked in and was then turned a
        // second time. One rotation, in the sidecar, read by both.
        //
        // The rule it was protecting survives instead in `Sidecar::is_developed`,
        // which ignores orientation: a turned frame has a sidecar and is still not
        // an edited one.
        let dir = folder_of("turn", &[("a.dng", 0, None)]);
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);

        assert_eq!(lb.rotate(true), None);
        assert_eq!(
            lb.entries[0].orientation,
            Some(raw_core::Orientation::Rotate90)
        );
        lb.rotate(true);
        lb.rotate(true);
        lb.rotate(true);
        assert_eq!(
            lb.entries[0].orientation,
            Some(raw_core::Orientation::Rotate0),
            "four turns is where you started"
        );
        lb.rotate(false);
        assert_eq!(
            lb.entries[0].orientation,
            Some(raw_core::Orientation::Rotate270),
            "and it goes the other way too"
        );

        assert!(!lb.entries[0].edited, "turning a frame made it look edited");

        // A fresh launch, reading only from disk.
        let mut later = Lightbox::new();
        later.open_folder(&dir);
        assert_eq!(
            later.entries[0].orientation,
            Some(raw_core::Orientation::Rotate270),
            "the turn did not survive"
        );
        assert!(!later.entries[0].edited);

        // And it is the value Develop composes with, which is the whole point.
        let side = raw_core::sidecar::read(&dir.join("a.dng"));
        let params = side.ok().expect("a sidecar").params;
        assert_eq!(
            params.composition.orientation,
            Some(raw_core::Orientation::Rotate270)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_action_applies_to_the_whole_selection() {
        // What multi-select is for. `selection` is one accessor so an action can
        // never act on the anchor while the screen shows several tiles ringed.
        let dir = folder_of(
            "batch",
            &[("a.dng", 0, None), ("b.dng", 0, None), ("c.dng", 0, None)],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);

        lb.selected = Some(1);
        assert_eq!(
            lb.selection(),
            vec![1],
            "with no batch, the anchor is the selection"
        );

        lb.batch = [0, 2].into_iter().collect();
        assert_eq!(lb.selection(), vec![0, 2], "with a batch, the batch is");

        lb.rotate(true);
        let turned = Some(raw_core::Orientation::Rotate90);
        assert_eq!(lb.entries[0].orientation, turned);
        assert_eq!(
            lb.entries[1].orientation, None,
            "the anchor was not in the batch and must not turn"
        );
        assert_eq!(lb.entries[2].orientation, turned);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drag_out_of_another_sort_moves_only_what_was_dragged() {
        // the maintainer: rearranging while sorted by anything else threw the whole folder
        // around and lost his place in it. A manual order from an earlier session was
        // still on disk, and switching to Manual re-applied it wholesale.
        let dir = folder_of(
            "dragsort",
            &[
                ("a.dng", 1, None),
                ("b.dng", 5, None),
                ("c.dng", 3, None),
                ("d.dng", 2, None),
            ],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);

        // An arrangement from before, deliberately unlike every other order.
        lb.manual = vec![
            "d.dng".to_owned(),
            "c.dng".to_owned(),
            "b.dng".to_owned(),
            "a.dng".to_owned(),
        ];

        lb.set_sort(Sort::Rating);
        let before = names(&lb);
        assert_eq!(before, ["b.dng", "c.dng", "d.dng", "a.dng"], "5, 3, 2, 1");

        // Drag the last tile to the front. Everything else must stay where it was.
        let dragged = lb.visible[3];
        lb.selected = Some(dragged);
        lb.reorder(dragged, 0);

        assert_eq!(lb.sort, Sort::Manual);
        assert_eq!(
            names(&lb),
            ["a.dng", "b.dng", "c.dng", "d.dng"],
            "the stale manual order was re-applied instead of the visible one"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_rename_numbers_in_visible_not_storage_order() {
        let mut lb = grid_of(4);
        lb.visible = vec![3, 1, 0, 2];
        lb.batch = [0, 2, 3].into_iter().collect();
        assert_eq!(lb.selection(), vec![0, 2, 3], "storage order is stable");
        assert_eq!(
            lb.selection_in_visible_order(),
            vec![3, 0, 2],
            "rename order must be the contact sheet order"
        );
    }

    #[test]
    fn ratings_and_labels_assign_one_value_to_the_whole_selection() {
        let dir = folder_of(
            "batch-marks",
            &[
                ("a.dng", 3, Some("blue")),
                ("b.dng", 1, None),
                ("c.dng", 0, Some("red")),
            ],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        lb.batch = [0, 1, 2].into_iter().collect();

        assert_eq!(lb.set_rating(3), None);
        assert_eq!(
            lb.entries
                .iter()
                .map(|entry| entry.rating)
                .collect::<Vec<_>>(),
            [3, 3, 3],
            "an existing three-star file was toggled off instead of assigned"
        );

        assert_eq!(lb.set_label(Some("magenta")), None);
        assert!(
            lb.entries
                .iter()
                .all(|entry| entry.label.as_deref() == Some("magenta"))
        );
        assert_eq!(lb.set_label(None), None);
        assert!(lb.entries.iter().all(|entry| entry.label.is_none()));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clearing_the_grid_selection_removes_anchor_batch_and_keyboard_head() {
        let mut lb = Lightbox::new();
        lb.selected = Some(2);
        lb.batch = [0, 1, 2].into_iter().collect();
        lb.head = Some(1);

        lb.clear_selection();

        assert_eq!(lb.selected, None);
        assert!(lb.batch.is_empty());
        assert_eq!(lb.head, None);
        assert!(lb.selection().is_empty());
    }

    #[test]
    fn stepping_walks_the_visible_order_and_stops_at_the_ends() {
        // Filtered, so the walk has to follow what is on screen rather than the
        // folder — four-star stepping should visit four frames, not the whole shoot.
        let dir = folder_of(
            "step",
            &[("a.dng", 5, None), ("b.dng", 1, None), ("c.dng", 5, None)],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.filters.stars = 5;
        lb.refilter();
        assert_eq!(names(&lb), ["a.dng", "c.dng"]);

        lb.step(true);
        assert_eq!(
            lb.selected,
            Some(0),
            "no selection yet means the first visible"
        );
        lb.step(true);
        assert_eq!(names(&lb)[1], "c.dng");
        assert_eq!(
            lb.selected,
            Some(2),
            "b.dng is filtered out and must be skipped"
        );
        lb.step(true);
        assert_eq!(lb.selected, Some(2), "the end is the end, not a wrap");
        lb.step(false);
        assert_eq!(lb.selected, Some(0));
        lb.step(false);
        assert_eq!(lb.selected, Some(0), "and the start holds too");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_opens_the_preview_and_space_closes_it() {
        let dir = folder_of("space", &[("a.dng", 0, None), ("b.dng", 0, None)]);
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert!(!lb.previewing());

        // Nothing selected yet: it opens on the first frame rather than doing nothing.
        lb.toggle_preview();
        assert!(lb.previewing());
        assert_eq!(lb.selected, Some(0));

        lb.step(true);
        assert_eq!(lb.preview, Some(1), "the preview follows the step");

        // The grid normally consumes `follow` while Quick Look is drawn beneath the
        // overlay. Model that frame so this assertion proves closing itself requests
        // the reveal rather than inheriting an earlier arrow's request.
        lb.follow = false;
        lb.toggle_preview();
        assert!(!lb.previewing(), "the key that opened it closes it");
        assert_eq!(
            lb.selected,
            Some(1),
            "and the selection stays where you left it"
        );
        assert!(
            lb.follow,
            "closing Quick Look did not ask the grid to reveal the selection"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selecting_starts_quick_look_on_its_own_queue() {
        let dir = folder_of("preview-preload", &[("a.dng", 0, None)]);
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);

        lb.preload_selected();

        let key = lb.full_key(0);
        assert!(
            lb.preview_queue.is_busy(key),
            "selection did not begin the full preview"
        );
        assert!(
            !lb.queue.is_busy(key),
            "Quick Look was put back behind thumbnail work"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotating_in_lightbox_turns_the_crop_with_the_picture() {
        use raw_core::composition::{Ratio, Rect};
        let dir = folder_of("turn-crop", &[("a.dng", 0, None)]);
        let path = dir.join("a.dng");
        let mut params = raw_core::Params::default();
        params.composition.orientation = Some(raw_core::Orientation::Rotate0);
        // The top-left quarter: after a clockwise turn it is the top-right one.
        params.composition.crop = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 0.5,
        };
        params.composition.ratio = Ratio::Fixed(1.5);
        raw_core::sidecar::write(&path, &params, &Default::default()).expect("sidecar");
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);

        assert_eq!(lb.rotate(true), None);

        let raw_core::sidecar::Loaded::Ok(after) = raw_core::sidecar::read(&path) else {
            panic!("the sidecar did not survive the turn");
        };
        assert_eq!(
            after.params.composition.crop,
            Rect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 0.5,
            },
            "the crop stayed where it was while the picture turned under it"
        );
        assert!(
            after.params.composition.portrait,
            "a locked 3:2 did not stand on its end with the frame"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_look_after_a_rotation_shows_the_turned_frame_not_the_preloaded_one() {
        // Selecting a tile preloads its full preview at the old orientation, and the
        // preview key has no orientation in it. Space straight after a turn showed
        // that stale frame until something happened to retire the generation.
        let ctx = egui::Context::default();
        let dir = folder_of("turn-look", &[("a.dng", 0, None)]);
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        lb.selected = Some(0);
        let key = lb.full_key(0);
        let image = egui::ColorImage {
            size: [1, 1],
            pixels: vec![egui::Color32::BLACK],
            source_size: egui::vec2(1.0, 1.0),
        };
        let texture = ctx.load_texture("unturned", image, egui::TextureOptions::LINEAR);
        lb.preview_textures
            .insert(key, PreviewTexture { texture, seen: 0 });

        assert_eq!(lb.rotate(true), None);

        assert!(
            !lb.preview_textures.contains_key(&key),
            "the preview loaded before the turn is still what Space would show"
        );

        lb.preview_queue.submit(key, decode::BACKGROUND, || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            None
        });
        lb.rotate(true);
        assert!(
            !lb.preview_queue.is_busy(key),
            "a load begun at the old orientation was left to land"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_look_keeps_three_textures_and_survives_close() {
        let ctx = egui::Context::default();
        let mut lb = grid_of(4);
        lb.generation = 7;
        lb.selected = Some(1);
        lb.preview = Some(1);

        for idx in 0..4 {
            let image = egui::ColorImage {
                size: [1, 1],
                pixels: vec![egui::Color32::from_gray(idx as u8)],
                source_size: egui::vec2(1.0, 1.0),
            };
            let texture = ctx.load_texture(
                format!("preview-test-{idx}"),
                image,
                egui::TextureOptions::LINEAR,
            );
            lb.preview_textures.insert(
                lb.full_key(idx),
                PreviewTexture {
                    texture,
                    seen: idx as u64,
                },
            );
        }

        lb.evict_preview_textures();
        assert_eq!(lb.preview_textures.len(), PREVIEW_CACHE_CAP);
        assert!(
            lb.preview_textures.contains_key(&lb.full_key(1)),
            "the selected frame was evicted because it was oldest"
        );
        assert!(
            lb.preview_textures.contains_key(&lb.full_key(0))
                && lb.preview_textures.contains_key(&lb.full_key(2)),
            "an immediate neighbour was evicted"
        );
        assert!(
            !lb.preview_textures.contains_key(&lb.full_key(3)),
            "a two-steps-away frame displaced a direct neighbour"
        );

        lb.close_preview();
        assert!(
            lb.follow,
            "a non-Space Quick Look close did not reveal the selection"
        );
        assert!(
            lb.preview_textures.contains_key(&lb.full_key(1)),
            "closing Quick Look discarded the warm frame"
        );
    }

    #[test]
    fn a_drag_moves_everything_that_is_selected() {
        // the maintainer found three tiles highlighted and one moving — the gesture disagreeing
        // with the screen. The block keeps its own order when it lands.
        let dir = folder_of(
            "dragmany",
            &[
                ("a.dng", 0, None),
                ("b.dng", 0, None),
                ("c.dng", 0, None),
                ("d.dng", 0, None),
            ],
        );
        let mut lb = Lightbox::new();
        lb.open_folder(&dir);
        assert_eq!(names(&lb), ["a.dng", "b.dng", "c.dng", "d.dng"]);

        // Take c and d, and drop them before a.
        lb.selected = Some(2);
        lb.batch = [2, 3].into_iter().collect();
        lb.reorder(2, 0);
        assert_eq!(names(&lb), ["c.dng", "d.dng", "a.dng", "b.dng"]);

        // Dragging something *outside* the selection is a statement about that one
        // thing, so the batch is not dragged along behind it.
        lb.selected = Some(0);
        lb.batch = [0, 2].into_iter().collect();
        lb.reorder(1, 0); // b.dng, which is not in the batch
        assert_eq!(names(&lb)[0], "b.dng");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_grid_lists_pictures_and_not_only_raws() {
        // the maintainer: there is no good reason for a browser to refuse a JPEG.
        assert!(is_listed(Path::new("/x/a.DNG")));
        assert!(is_listed(Path::new("/x/a.jpg")));
        assert!(
            is_listed(Path::new("/x/a.PNG")),
            "extension matching is case-insensitive"
        );
        assert!(is_listed(Path::new("/x/a.tiff")));
        assert!(is_raw(Path::new("/x/a.rw2")) && !is_image(Path::new("/x/a.rw2")));
        assert!(is_image(Path::new("/x/a.webp")) && !is_raw(Path::new("/x/a.webp")));
        // Still not everything: a sidecar is not a picture, and neither is a note.
        assert!(!is_listed(Path::new("/x/a.mono.xmp")));
        assert!(!is_listed(Path::new("/x/notes.txt")));
        assert!(!is_listed(Path::new("/x/noextension")));
    }

    #[test]
    fn every_sort_mode_round_trips_through_storage() {
        // What `remember_lightbox_sort` writes has to come back as the same mode.
        for mode in Sort::ALL {
            assert_eq!(Sort::from_key(mode.key()), Some(mode), "{mode:?}");
        }
        assert_eq!(Sort::from_key("something else"), None);
    }

    #[test]
    fn a_tile_that_is_here_is_not_asked_for_again() {
        let mut lb = Lightbox::new();
        lb.entries = vec![Entry {
            path: PathBuf::from("/x/a.dng"),
            name: "a.dng".into(),
            edited: false,
            rating: 0,
            label: None,
            ext: "dng".into(),
            kind: Kind::Picture,
            developed: None,
            captured: None,
            orientation: None,
        }];
        lb.request(0, decode::FOREGROUND);
        assert!(matches!(lb.tiles.get(&0), Some(Tile::Pending)));
        // Second ask is a no-op: the guard is what stops the grid queueing the same
        // tile once per frame for as long as it is on screen.
        lb.request(0, decode::FOREGROUND);
        assert_eq!(lb.tiles.len(), 1);
    }
}
