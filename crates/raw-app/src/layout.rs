//! The docking layout: one `egui_tiles` tree, and the image is a pane in it.
//!
//! # Why one tree, with the image inside it
//!
//! The alternative was to leave the image in the `CentralPanel` and put tiles in the
//! side panels only — closer to Qt, where docks surround an immovable central
//! widget. It needs *two* trees, left and right, and a pane cannot move between two
//! trees; dragging Develop from the left of the image to the right is one of the
//! things this exists to do. So: one tree, and the image is a pane. `viewport_panel`
//! already sizes itself from `ui.available_size()`, so it works in an arbitrary tile
//! rect unchanged. This is the shape Rerun uses, and `egui_tiles` is the crate Rerun
//! built.
//!
//! # What the tree does not do
//!
//! `egui_tiles` has no viewport container and no concept of an OS window — it draws
//! inside one `Ui` in one context. So it replaces the *docked* half of the earliere 7c
//! and leaves the floating half as a separate mechanism. Tearing a pane out is easy:
//! it goes invisible in the tree, keeps its place there, and is drawn by
//! `float_develop` / `float_info` instead. Dragging a floating window back **in** is
//! the hard direction — a native window drag is invisible to the main window's egui
//! context — and by the maintainer's decision it is not attempted. Float is a one-way pop-out
//! with a dock button, exactly as 7c shipped it.
//!
//! # Where the layout lives
//!
//! App memory, not settings and not `Params`. The tree is `serde`-serialisable, so it
//! persists through eframe storage alongside window geometry — the same
//! decision about which of the three files owns what. Not the sidecar: where you put
//! a panel is not a property of the image.

use egui_tiles::{Container, Linear, LinearDir, SimplificationOptions, Tile, TileId, UiResponse};

use crate::{App, hotkeys, icons, theme};

/// One tile in the tree.
///
/// `Snapshots` will join this when the compare viewer is built (`k` / `⌘K`, deferred
/// by the maintainer — see `docs/ux-inventory.md`); the slot is left obvious rather than
/// discovered later. Adding a variant resets the stored layout once, on purpose:
/// see [`Layout::heal`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Pane {
    Image,
    Develop,
    /// The Dodge & Burn brush, its instances and their tonal-range masks.
    ///
    /// **A pane of its own rather than a module inside Develop**, which is the maintainer's
    /// call and the prototype's arrangement — its top-level tabs are DEVELOP and
    /// DODGE/BURN. `docs/ux-inventory.md` left this open on the grounds that the
    /// prototype's tabs are *modes* while this app's are *images*, so it could not
    /// simply be ported.
    ///
    /// What settles it is that this pane is a **tool**, not a stage: four brush
    /// controls, a list of instances and a mask editor, none of which you are
    /// looking at unless you are painting. As a module it made the Develop panel
    /// long and put a scroll between Contrast Mask and the curve. As a pane it
    /// defaults to a tab beside DEVELOP and can be dragged out, stacked under it, or
    /// put on another screen — which is more than the prototype can do with it.
    ///
    /// The cost, stated because it was the argument for the other arrangement: the
    /// develop panel no longer reads as the pipeline in order, and D&B's place
    /// between Contrast Mask and the curve is now something you have to know rather
    /// than see. `raw_graph::build` is where it is written down.
    DodgeBurn,
    /// Chemical toning: the process, and the ordered stack of baths.
    ///
    /// **Behind Dodge & Burn**, which is the maintainer's placement, and the argument written
    /// into `DodgeBurn` applies unchanged: this is a *tool with its own workspace* —
    /// a list, an editor and a graph — and none of it is something you are looking at
    /// unless you are toning. As a module inside Develop it would put a scroll between
    /// Contrast Mask and the curve, which is the mistake that pane was created to fix.
    Toning,
    Info,
    /// Captured looks, and the pins that decide what the compare grid shows.
    ///
    /// **Tabbed with Info rather than given a column**, which is the same argument
    /// that put Dodge & Burn beside Develop: a fourth column would come out of the
    /// image, and the image is the thing the program is for. Info's side is the right
    /// home of the two — the left column is *controls*, things you set, and both Info
    /// and this are things you *read* and occasionally act on.
    Snapshots,
    /// The undo timeline, made visible.
    ///
    /// **Behind Snapshots**, which is the maintainer's placement and the right one: the two are
    /// the same question at two scales. A snapshot is a version you *chose* to keep and
    /// a history entry is one the app kept for you, so they belong on one side of the
    /// window, one in front of the other.
    History,
}

impl Pane {
    /// Every pane the tree must contain, exactly once.
    pub const ALL: [Self; 7] = [
        Self::Image,
        Self::Develop,
        Self::DodgeBurn,
        Self::Toning,
        Self::Info,
        Self::Snapshots,
        Self::History,
    ];

    /// Everything that is not the image. `tab` hides and restores exactly these.
    pub const PANELS: [Self; 6] = [
        Self::Develop,
        Self::DodgeBurn,
        Self::Toning,
        Self::Info,
        Self::Snapshots,
        Self::History,
    ];

    /// Its share of a horizontal row at the default window width.
    ///
    /// Used both for the starting layout and to rebuild a container whose shares have
    /// stopped being widths — so a healed layout comes back looking like the one the
    /// app ships with rather than as equal columns.
    fn default_share(self) -> f32 {
        share(match self {
            Self::Image => IMAGE_W,
            // The same width as Develop: they share a tab strip by default, so a
            // different share would make the column jump when you switched.
            Self::Develop | Self::DodgeBurn | Self::Toning => DEVELOP_W,
            // These three share the narrower readout column. Keeping one width for
            // the whole tab group prevents a jump when its active pane changes.
            Self::Info | Self::Snapshots | Self::History => INFO_W,
        })
    }

    /// Upper case, because it is set as a field marker — see `theme::paint_header`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "IMAGE",
            Self::Develop => "DEVELOP",
            // The prototype's own label for the tab, slash and all.
            Self::DodgeBurn => "DODGE / BURN",
            Self::Toning => "TONING",
            Self::Info => "INFO",
            Self::Snapshots => "SNAPSHOTS",
            Self::History => "HISTORY",
        }
    }
}

/// **The image pane must not be closable.** Everything else in the app can be
/// brought back with `tab`; closing the image would close the thing the program is
/// for, and there is nothing else in the frame to reopen it from.
///
/// A free function rather than only a `Behavior` method so it can be tested without
/// an `App` to hang a behavior off.
fn closable(pane: Pane) -> bool {
    pane != Pane::Image
}

/// The starting split, in points, at the 1400pt default window width.
///
/// `pub(crate)` because `widgets::Row`'s column budget is solved against the width a
/// module body has at this panel size, and `the_row_budget_fits_the_default_panel_without_shortening_the_track`
/// has to read it rather than quote it — a develop panel that started narrower would
/// shorten every slider in the app, and nothing else would say so.
pub(crate) const DEVELOP_W: f32 = 350.0;
/// Fit the complete tab headers, including close buttons and a small rounding margin.
///
/// `pub(crate)` because the Lightbox's sidebar starts at this width too — the
/// maintainer's call, so the two modes' reading columns match when you switch between
/// them.
pub(crate) const INFO_W: f32 = 304.0;
const IMAGE_W: f32 = 746.0;

/// Those three, which is the default window width.
const LAYOUT_W: f32 = DEVELOP_W + IMAGE_W + INFO_W;

/// A width in points as a **normalised share**: scaled so the shares in a container
/// average 1.0.
///
/// **Not the raw point width, and this is the difference between working and
/// unusable.** Shares are relative, so points appear to work — and they read
/// beautifully, `set_share(develop, 320.0)`. But `Shares` returns **1.0** for any tile
/// it has no entry for, and `Tiles::insert_at` never creates one: a tile that arrives
/// in a container by being dropped there simply has no share. Beside siblings of 320
/// and 780 that default is 1/1101 of the window — a pane 1.3 points wide, which is
/// indistinguishable from having lost it. It happened to Info in testing.
///
/// On this scale the same implicit default means "an equal share", which is what
/// `egui_tiles` designed it to mean. [`Layout::normalise_shares`] keeps every
/// container on it.
///
/// # The constant is the number of COLUMNS, not the number of panes
///
/// It used to be `Pane::ALL.len()`, which was six and worked, and became seven when
/// Toning was added — at which point `a_pane_dropped_into_a_row_does_not_land_one_
/// point_wide` failed at 175pt against a 180pt minimum. The two numbers were never the
/// same thing; they were briefly equal enough.
///
/// Shares are relative, so scaling every explicit share by a constant changes nothing
/// on screen. **The only thing this constant controls is what an implicit 1.0 is worth
/// beside them** — how wide a freshly-dropped pane lands. That has to be measured
/// against the row it lands in, and the row is three columns wide. Sized by the pane
/// count instead, every pane added anywhere in the app makes a dropped pane narrower,
/// silently, until one day it is under the minimum.
fn share(points: f32) -> f32 {
    COLUMNS as f32 * points / LAYOUT_W
}

/// Columns in the default row: controls, image, readouts. See [`share`].
const COLUMNS: usize = 3;

/// How far apart two shares in one container may be before the smaller one has
/// stopped being a width. See [`Layout::heal`].
///
/// **A ratio, not a fraction of the container.** A fraction was the first attempt and
/// it fires on a layout that is perfectly good: `min_size` stops a drag at 180pt, and
/// on a 3440pt ultrawide 180 is 5% of the window — so any threshold low enough to
/// catch a real sliver is high enough to reset a panel somebody had deliberately
/// dragged narrow. The widest legitimate spread is a panel at the stop beside a 5K
/// image, around 26:1. A stored share that has actually gone wrong is orders of
/// magnitude past that.
const SHARE_SPREAD: f32 = 100.0;

/// **These decide whether the tree fights the user**, so they are set out in full
/// rather than left to `Default` even where they agree with it — and they are one
/// constant rather than a method body, because the tests assert behaviour that
/// follows from them and a second copy would let the two drift.
///
/// - Empty containers and empty tab bars are pruned. Drag the last panel out of a
///   column and the column has to go with it, or it keeps its slot forever.
/// - Single-child containers collapse, and nested linear containers of the same
///   direction join. Without both, every drag deepens the tree by a level and the
///   shares of a wrapper nobody can see start deciding the layout.
/// - `prune_single_child_tabs` means a tab bar naming only itself goes away when the
///   pane beside it is dragged off. Wrong settings here make a panel appear to snap
///   back after being moved, which reads as a broken drag rather than as a policy.
/// - `all_panes_must_have_tabs` is **off**. With it on, every pane wears a tab bar —
///   including the image, which would put a permanent strip reading "IMAGE" over the
///   photograph, and the design pass spent three sittings taking chrome like that
///   away. So a tab bar appears only where panes are genuinely stacked, and a lone
///   pane is dragged by its own title instead. Nothing is lost by it: the drop zone
///   that stacks two panes is `tab_bar_height` deep whether or not a bar is drawn.
pub(crate) const SIMPLIFY: SimplificationOptions = SimplificationOptions {
    prune_empty_tabs: true,
    prune_empty_containers: true,
    prune_single_child_tabs: true,
    prune_single_child_containers: true,
    all_panes_must_have_tabs: false,
    join_nested_linear_containers: true,
};

/// Scale every linear container's shares so they average 1.0. See
/// `Layout::normalise_shares` for why it runs every frame.
///
/// **Free and generic, because the Lightbox has the same tree and had the same bug.**
/// the maintainer: dragging Folders — or any Lightbox panel — to the right edge made it vanish.
/// It was not vanishing; it was arriving one point wide. `Tiles::insert_at` puts a tile
/// into a container without giving it a share and `Shares` answers 1.0 for a tile it
/// has no entry for, so a pane dropped into the Lightbox's root row landed at 1.0
/// against `left: 240` and `grid: 900` — one part in eleven hundred, which is a
/// hairline against the window edge and reads as gone.
///
/// Develop never showed it because this function has been running there since the
/// problem was found; the fix simply never crossed over. One implementation now, called
/// from both, which is the only way the two stay fixed together.
pub(crate) fn normalise_shares<T>(tree: &mut egui_tiles::Tree<T>) {
    for (_, tile) in tree.tiles.iter_mut() {
        let Tile::Container(Container::Linear(row)) = tile else {
            continue;
        };
        let n = row.children.len();
        if n == 0 {
            continue;
        }
        let total: f32 = row.children.iter().map(|c| row.shares[*c]).sum();
        if !total.is_finite() || total <= 0.0 {
            continue; // `Layout::clamp_shares` deals with that
        }
        let scale = n as f32 / total;
        // Near enough already; leave the numbers alone so a share the user set by
        // dragging is not rewritten on every frame of the drag.
        if (scale - 1.0).abs() < 0.001 {
            continue;
        }
        for child in row.children.clone() {
            row.shares.set_share(child, row.shares[child] * scale);
        }
    }
}

/// No pane may be dragged narrower than this, in points.
///
/// One number for both axes, because that is all `egui_tiles` offers — the old
/// `size_range(240..=560)` per panel is gone with the panels. 180 is chosen so that
/// three panes side by side still fit the 900pt minimum window, and so that a panel
/// squeezed to the stop is cramped rather than a sliver.
pub(crate) const MIN_PANE: f32 = 180.0;

/// How far past its stop a side column's seam has to be dragged before the column
/// tucks away. Far enough that meeting the stop is not the same gesture as leaving.
const TUCK_PAST: f32 = 60.0;

/// The width of the strip a tucked column leaves at the window's edge.
const TUCK_STRIP: f32 = 6.0;

/// The seam between two tiles, in points. One constant rather than a literal in
/// `Behavior::gap_width`, because `keep_panel_sizes` has to subtract exactly the space
/// the layout will spend on seams or every panel drifts by a point per neighbour.
pub(crate) const GAP: f32 = 1.0;

/// The one hairline used to frame panel title bars and separate adjacent names.
pub(crate) fn panel_rule() -> egui::Stroke {
    egui::Stroke::new(1.0, egui::Color32::from_gray(52))
}

/// Finish a tab bar like a browser strip: one rule over the whole panel and a short
/// vertical rule before every tab after the first.
pub(crate) fn paint_tab_rules(ui: &egui::Ui, tab: egui::Rect) {
    let bar = ui.max_rect();
    let painter = ui.painter();
    painter.line_segment(
        [
            egui::pos2(bar.left(), bar.top() + 0.5),
            egui::pos2(bar.right(), bar.top() + 0.5),
        ],
        panel_rule(),
    );
    if tab.left() > bar.left() + 1.0 {
        painter.line_segment(
            [
                egui::pos2(tab.left() + 0.5, tab.top() + 6.0),
                egui::pos2(tab.left() + 0.5, tab.bottom() - 6.0),
            ],
            panel_rule(),
        );
    }
}

/// A size along a linear container's own axis.
fn along(size: egui::Vec2, dir: LinearDir) -> f32 {
    match dir {
        LinearDir::Horizontal => size.x,
        LinearDir::Vertical => size.y,
    }
}

/// Where the panels are: the tree, plus which panes are in the frame at all and
/// which have been popped into their own OS window.
pub struct Layout {
    pub tree: egui_tiles::Tree<Pane>,
    /// The panels currently in the frame. Closing a tab takes one out; `tab` takes
    /// all of them out and puts all of them back, which is what makes closing a
    /// panel reversible without inventing a second control for it.
    ///
    /// Not the tree's own visibility flags, which are *derived* from this and from
    /// `out` every frame. Two authorities on whether a panel is showing is how a
    /// panel ends up floating and docked at once.
    shown: Vec<Pane>,
    /// Popped out into a window of its own. Deliberately not persisted — where a
    /// window was is eframe's business, and a panel that came back detached with its
    /// window somewhere off-screen would be a panel you cannot find.
    out: Vec<Pane>,
    /// Something moved, so the tree is worth writing. Set from
    /// `Behavior::on_edit` — a drag, a resize or a tab click — and by the controls
    /// here. Cheaper and more honest than diffing the tree every frame.
    pub dirty: bool,
    /// The *shape* changed — a pane was dropped somewhere, closed, popped out or put
    /// back — as opposed to merely resized. Only these need [`Layout::keep_panel_sizes`];
    /// a resize drag already writes shares from measured widths and must not be
    /// second-guessed.
    restructured: bool,
    /// What each tile measured last frame. See [`Layout::keep_panel_sizes`].
    measured: std::collections::HashMap<TileId, egui::Vec2>,
    restore_pixels: bool,
    /// Edge columns dragged off the side of the window. Hidden like a closed panel but
    /// reached by the strip left at the edge, not by `tab`. See [`Tuck`].
    tuck: Tuck,
}

/// Edge columns dragged off the side of the window, and the width each goes back to.
///
/// **One implementation for both trees**, for the reason `normalise_shares` is one:
/// the Lightbox has the same row of panels around a flexible middle, and a fix made
/// to one copy never reached the other. The flexible tile — the image here, the grid
/// there — is passed in rather than known, and everything else is the same gesture.
///
/// Not persisted: a column that came back tucked after a restart would look like a
/// column that had gone missing.
#[derive(Default)]
pub(crate) struct Tuck {
    tucked: Vec<(TileId, f32)>,
    /// Where the edge seams were, and how wide their columns, when the pointer last
    /// went down. A tuck only fires for a drag that *started* on that column's seam —
    /// dragging the other seam across the window must not take this column with it.
    press: [Option<(f32, f32)>; 2],
}

impl Tuck {
    pub(crate) fn contains(&self, id: TileId) -> bool {
        self.tucked.iter().any(|(t, _)| *t == id)
    }

    /// Each tucked column and the width it goes back to.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (TileId, f32)> + '_ {
        self.tucked.iter().copied()
    }

    /// Tuck `id`. False if it already was.
    pub(crate) fn push(&mut self, id: TileId, width: f32) -> bool {
        if self.contains(id) {
            return false;
        }
        self.tucked.push((id, width));
        true
    }

    /// Bring `id` back, returning the width it left at.
    pub(crate) fn remove(&mut self, id: TileId) -> Option<f32> {
        let i = self.tucked.iter().position(|(t, _)| *t == id)?;
        Some(self.tucked.remove(i).1)
    }

    /// Every tucked column, taken out of the tuck.
    pub(crate) fn take(&mut self) -> Vec<(TileId, f32)> {
        std::mem::take(&mut self.tucked)
    }

    /// The tucked column that holds `tile`, taken out of the tuck — for a panel that is
    /// asked for by name while its column is off the edge.
    pub(crate) fn release_containing<P>(
        &mut self,
        tree: &egui_tiles::Tree<P>,
        tile: TileId,
    ) -> Option<(TileId, f32)> {
        let mut at = Some(tile);
        while let Some(id) = at {
            if let Some(width) = self.remove(id) {
                return Some((id, width));
            }
            at = tree.tiles.parent_of(id);
        }
        None
    }

    /// The root row's first and last children — `[left, right]` — when they are not
    /// the column holding `flex`. Tucked ones included, so a tucked column is still
    /// found to bring it back. Anything but a horizontal root has no side columns.
    pub(crate) fn edge_columns<P>(
        &self,
        tree: &egui_tiles::Tree<P>,
        flex: TileId,
    ) -> [Option<(TileId, egui::Rect)>; 2] {
        let none = [None, None];
        let Some(root) = tree.root else {
            return none;
        };
        let Some(Tile::Container(Container::Linear(row))) = tree.tiles.get(root) else {
            return none;
        };
        if row.dir != LinearDir::Horizontal || row.children.len() < 2 {
            return none;
        }
        let holds_flex = |mut id: TileId| loop {
            if id == flex {
                return true;
            }
            match tree.tiles.parent_of(id) {
                Some(p) if p != root => id = p,
                _ => return false,
            }
        };
        // Only children that are showing or tucked count as the edge: a closed column
        // at the end of the row is not what is at the window's edge.
        let on_edge: Vec<TileId> = row
            .children
            .iter()
            .copied()
            .filter(|c| self.contains(*c) || tree.is_visible(*c))
            .collect();
        let pick = |id: Option<&TileId>| {
            let id = *id?;
            if holds_flex(id) {
                return None;
            }
            Some((id, tree.tiles.rect(id).unwrap_or(egui::Rect::NOTHING)))
        };
        [pick(on_edge.first()), pick(on_edge.last())]
    }

    /// Hide every tucked column, and let go of any that is no longer on the window's
    /// edge — one that has since been dropped somewhere else, or whose tile is gone.
    ///
    /// After the caller has settled its own visibility, since this only ever hides.
    pub(crate) fn settle<P>(&mut self, tree: &mut egui_tiles::Tree<P>, flex: TileId) {
        let edges = self.edge_columns(tree, flex);
        self.tucked
            .retain(|(id, _)| edges.iter().any(|e| e.map(|(t, _)| t) == Some(*id)));
        for (id, _) in &self.tucked {
            tree.set_visible(*id, false);
        }
    }

    /// **Drag a side column past its stop and it tucks away.** `min_size` holds the
    /// seam at [`MIN_PANE`]; carry on dragging toward the window's edge by
    /// [`TUCK_PAST`] and the column goes, leaving the strip [`Tuck::strips`] draws.
    ///
    /// Returns true on the frame a column tucks, and **the caller must then end the
    /// drag** (`Context::stop_dragging`). `egui_tiles` keys each seam by its index among
    /// the *visible* children, so once the left column is hidden, seam 0 is the one
    /// between the image and the right column — and the drag still held on seam 0
    /// carries on there. That was the bug: squeeze Develop off the left and the image
    /// was squeezed after it, handing the whole window to Info.
    ///
    /// `defaults` are the shipped widths, `[left, right]`, and **a column comes back no
    /// narrower than its default** — wider only if it was wider when it left. The
    /// maintainer's call: the width a collapse begins from is usually the one the
    /// squeeze toward the edge has already eaten into, not one anybody chose, and a
    /// panel that reopens narrow reads as a panel that reopened broken.
    pub(crate) fn drag<P>(
        &mut self,
        tree: &egui_tiles::Tree<P>,
        flex: TileId,
        input: &egui::InputState,
        resizing: bool,
        defaults: [f32; 2],
    ) -> bool {
        let edges = self.edge_columns(tree, flex);
        if input.pointer.primary_pressed() {
            // Seam and width as they were at the press, before the drag moved them.
            self.press = [0, 1].map(|side| {
                edges[side]
                    .filter(|(_, r)| r.is_positive())
                    .map(|(_, r)| (if side == 0 { r.right() } else { r.left() }, r.width()))
            });
        }
        if !resizing || !input.pointer.primary_down() {
            return false;
        }
        let (Some(origin), Some(at)) = (input.pointer.press_origin(), input.pointer.interact_pos())
        else {
            return false;
        };
        let mut fired = false;
        for (side, (edge, press)) in edges.into_iter().zip(self.press).enumerate() {
            let (Some((id, rect)), Some((seam, width))) = (edge, press) else {
                continue;
            };
            if (origin.x - seam).abs() > 6.0 || rect.width() > MIN_PANE + 1.0 {
                continue;
            }
            let past = if side == 0 {
                rect.right() - at.x
            } else {
                at.x - rect.left()
            };
            if past > TUCK_PAST {
                fired |= self.push(id, width.max(defaults[side]));
            }
        }
        if fired {
            self.press = [None; 2];
        }
        fired
    }

    /// The strip a tucked column leaves at the window's edge. A click or a drag on it
    /// brings the column back, and the column and its width are returned so the
    /// caller can restore it.
    pub(crate) fn strips<P>(
        &mut self,
        tree: &egui_tiles::Tree<P>,
        flex: TileId,
        ui: &mut egui::Ui,
        area: egui::Rect,
    ) -> Option<(TileId, f32)> {
        let edges = self.edge_columns(tree, flex);
        let mut back = None;
        for (side, edge) in edges.into_iter().enumerate() {
            let Some((id, _)) = edge else {
                continue;
            };
            if !self.contains(id) {
                continue;
            }
            let rect = if side == 0 {
                egui::Rect::from_min_size(area.left_top(), egui::vec2(TUCK_STRIP, area.height()))
            } else {
                egui::Rect::from_min_size(
                    egui::pos2(area.right() - TUCK_STRIP, area.top()),
                    egui::vec2(TUCK_STRIP, area.height()),
                )
            };
            let resp = ui
                .interact(
                    rect,
                    ui.id().with(("tuck", side)),
                    egui::Sense::click_and_drag(),
                )
                .on_hover_cursor(if side == 0 {
                    egui::CursorIcon::ResizeEast
                } else {
                    egui::CursorIcon::ResizeWest
                })
                .on_hover_text(theme::tip("Click or drag to bring the panel back"));
            let ink = if resp.hovered() || resp.dragged() {
                theme::DIM
            } else {
                egui::Color32::from_gray(72)
            };
            ui.painter().rect_filled(rect, 0.0, theme::CHROME_DEEP);
            let line = if side == 0 {
                rect.right() - 1.5
            } else {
                rect.left() + 1.5
            };
            ui.painter().line_segment(
                [
                    egui::pos2(line, rect.top()),
                    egui::pos2(line, rect.bottom()),
                ],
                egui::Stroke::new(3.0, ink),
            );
            if (resp.clicked() || resp.drag_started())
                && let Some(width) = self.remove(id)
            {
                back = Some((id, width));
            }
        }
        back
    }
}

mod memory_keys {
    pub const TREE: &str = "layout.tree";
    pub const PANELS: &str = "layout.panels";
    pub const PIXELS: &str = "layout.pixel_sizes";
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            tree: default_tree(),
            shown: Pane::PANELS.to_vec(),
            out: Vec::new(),
            dirty: false,
            restructured: false,
            measured: Default::default(),
            restore_pixels: true,
            tuck: Tuck::default(),
        }
    }
}

/// (Develop / Dodge&Burn) | image | Info — the three panels, with the
/// brush tabbed behind Develop where the prototype puts it.
///
/// **Tabbed rather than a fourth column**, and rather than stacked under Develop.
/// Two of the three arrangements were offered and this is the one the maintainer picked: the
/// brush and the tone chain are different jobs and you are rarely doing both at
/// once, so the default should show one at a time — and because it is a real tile,
/// anyone who wants both drags the tab out once and the layout remembers.
fn default_tree() -> egui_tiles::Tree<Pane> {
    let mut tiles = egui_tiles::Tiles::default();
    let develop = tiles.insert_pane(Pane::Develop);
    let dodgeburn = tiles.insert_pane(Pane::DodgeBurn);
    let toning = tiles.insert_pane(Pane::Toning);
    let image = tiles.insert_pane(Pane::Image);
    let info = tiles.insert_pane(Pane::Info);
    let snapshots = tiles.insert_pane(Pane::Snapshots);
    let history = tiles.insert_pane(Pane::History);

    // Develop is active, so an untouched layout opens on the tone chain rather than
    // on a tool with nothing in it.
    let mut column = egui_tiles::Tabs::new(vec![develop, dodgeburn, toning]);
    column.active = Some(develop);
    let column = tiles.insert_container(Container::Tabs(column));

    // Info is active for the same reason Develop is: an untouched layout should open
    // on the readout that always has something in it, not on a list that is empty
    // until the first `⌘K`.
    let mut right = egui_tiles::Tabs::new(vec![info, snapshots, history]);
    right.active = Some(info);
    let right = tiles.insert_container(Container::Tabs(right));

    let mut row = Linear::new(LinearDir::Horizontal, vec![column, image, right]);
    row.shares.set_share(column, Pane::Develop.default_share());
    row.shares.set_share(image, Pane::Image.default_share());
    row.shares.set_share(right, Pane::Info.default_share());

    let root = tiles.insert_container(Container::Linear(row));
    egui_tiles::Tree::new("monopro-layout", root, tiles)
}

impl Layout {
    /// Restore the layout from app memory, falling back to the default whenever what
    /// came back cannot be run.
    ///
    /// A layout is a convenience and must never be the reason the app will not start
    /// — the same rule `Loaded<T>` applies to everything else read from disk.
    pub fn restore(storage: Option<&dyn eframe::Storage>) -> Self {
        let Some(storage) = storage else {
            return Self::default();
        };
        let Some(tree) = eframe::get_value::<egui_tiles::Tree<Pane>>(storage, memory_keys::TREE)
        else {
            return Self::default();
        };
        let shown = eframe::get_value::<Vec<Pane>>(storage, memory_keys::PANELS)
            .unwrap_or_else(|| Pane::PANELS.to_vec());
        let mut layout = Self {
            tree,
            shown,
            out: Vec::new(),
            dirty: false,
            restructured: false,
            measured: eframe::get_value(storage, memory_keys::PIXELS).unwrap_or_default(),
            restore_pixels: true,
            tuck: Tuck::default(),
        };
        if !layout.heal() {
            return Self::default();
        }
        layout
    }

    /// Apply pixel widths once the actual window width is known. The image
    /// receives the remainder; saved ratios never scale the side panels on launch.
    fn restore_pixel_widths(&mut self, width: f32) {
        if !std::mem::take(&mut self.restore_pixels) {
            return;
        }
        let Some(root) = self.tree.root else {
            return;
        };
        let Some(image) = self.tree.tiles.find_pane(&Pane::Image) else {
            return;
        };
        let mut path = vec![image];
        while let Some(parent) = self.tree.tiles.parent_of(*path.last().unwrap()) {
            path.push(parent);
        }
        let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get(root) else {
            return;
        };
        if row.dir != LinearDir::Horizontal {
            return;
        }
        let children: Vec<_> = row
            .children
            .iter()
            .copied()
            .filter(|c| self.tree.is_visible(*c))
            .collect();
        let Some(flex) = children.iter().position(|c| path.contains(c)) else {
            return;
        };
        let available = width - GAP * children.len().saturating_sub(1) as f32;
        let mut sizes: Vec<_> = children
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == flex {
                    0.0
                } else {
                    self.measured
                        .get(c)
                        .map(|s| s.x)
                        .filter(|w| w.is_finite() && *w >= MIN_PANE)
                        .unwrap_or(if i < flex { DEVELOP_W } else { INFO_W })
                }
            })
            .collect();
        let total: f32 = sizes.iter().sum();
        if available - total < MIN_PANE {
            return;
        }
        sizes[flex] = available - total;
        if let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get_mut(root) {
            for (child, size) in children.into_iter().zip(sizes) {
                row.shares.set_share(child, size);
            }
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, memory_keys::TREE, &self.tree);
        eframe::set_value(storage, memory_keys::PANELS, &self.shown);
        let mut measured = self.measured.clone();
        for id in self.tree.tiles.tile_ids() {
            if let Some(rect) = self.tree.tiles.rect(id) {
                measured.insert(id, rect.size());
            }
        }
        // A tuck is not persisted, so a column quit while tucked comes back shown —
        // at the width it would have come back at, not the squeezed one it left at.
        for (id, width) in self.tuck.iter() {
            let h = measured.get(&id).map_or(0.0, |s| s.y);
            measured.insert(id, egui::vec2(width, h));
        }
        eframe::set_value(storage, memory_keys::PIXELS, &measured);
    }

    /// Make a restored tree safe to run, or report that it cannot be.
    ///
    /// **A stored layout is inherited forever unless something heals it.** That is
    /// the lesson of the 7b panel runaway: the develop panel grew until it filled the
    /// window, and then came back that way every launch, because the bad width was
    /// persisted. The tree itself cannot run away — `Tree::ui` divides
    /// `available_rect` top-down rather than measuring content — but a *share* is a
    /// stored number with the same property, so a pane stored at one part in a
    /// thousand would be a permanent sliver with a resize handle too small to grab.
    ///
    /// Returns false when the tree is beyond repair, which means one thing: the set
    /// of panes is not the set this build knows. A missing `Image` leaves no
    /// viewport; a duplicate `Develop` gives two panes one identity; a pane added in
    /// a later version has nowhere sensible to go. Rebuilding the default is a
    /// layout lost once, on an upgrade, rather than an app that starts wrong.
    fn heal(&mut self) -> bool {
        if self.tree.root.is_none() {
            return false;
        }
        let mut found: Vec<Pane> = Vec::new();
        for tile in self.tree.tiles.tiles() {
            if let Tile::Pane(pane) = tile {
                if found.contains(pane) {
                    return false; // the same pane twice
                }
                found.push(*pane);
            }
        }
        if found.len() != Pane::ALL.len() || Pane::ALL.iter().any(|p| !found.contains(p)) {
            return false;
        }
        self.shown.retain(|p| Pane::PANELS.contains(p));
        self.clamp_shares();
        self.normalise_shares();
        true
    }

    /// Scale every linear container's shares so they average 1.0.
    ///
    /// **Run every frame, not only on restore**, because the state this repairs is
    /// created by an ordinary drop: `Tiles::insert_at` puts a tile in a container
    /// without giving it a share, and `Shares` answers 1.0 for a tile it has no entry
    /// for. Keeping the scale near 1.0 is what makes that implicit default mean "an
    /// equal share" instead of "one part in a thousand". A pane dropped into a row of
    /// three lands at roughly a third of it, which is the right answer and is arrived
    /// at by doing nothing.
    ///
    /// Scaling every share by one factor preserves their ratios, so this cannot fight
    /// a resize drag in progress — the widths it computes are unchanged, and only the
    /// numbers behind them move.
    fn normalise_shares(&mut self) {
        normalise_shares(&mut self.tree);
    }

    /// Rebuild a linear container's shares whenever one of them has stopped being a
    /// width. All of them, not just the offender: the sum is what a share means, so
    /// repairing one in place would leave the rest describing a different total.
    ///
    /// Rebuilt from each pane's own default rather than to equal shares, so a healed
    /// layout comes back looking like the one the app ships with. A child that is a
    /// container gets an equal share, because there is no sensible width to ask a
    /// subtree for.
    fn clamp_shares(&mut self) {
        // Two passes: the replacement shares are read from the tiles, and the write
        // needs the container mutably.
        let mut fixes: Vec<(TileId, Vec<(TileId, f32)>)> = Vec::new();
        for (id, tile) in self.tree.tiles.iter() {
            let Tile::Container(Container::Linear(row)) = tile else {
                continue;
            };
            let shares = || row.children.iter().map(|c| row.shares[*c]);
            let bad = shares().any(|s| !s.is_finite() || s <= 0.0)
                || shares().fold(0.0f32, f32::max)
                    > shares().fold(f32::INFINITY, f32::min) * SHARE_SPREAD;
            if !bad {
                continue;
            }
            let rebuilt = row
                .children
                .iter()
                .map(|child| {
                    let share = match self.tree.tiles.get(*child) {
                        Some(Tile::Pane(pane)) => pane.default_share(),
                        _ => 1.0,
                    };
                    (*child, share)
                })
                .collect();
            fixes.push((*id, rebuilt));
        }
        for (id, rebuilt) in fixes {
            let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get_mut(id) else {
                continue;
            };
            for (child, share) in rebuilt {
                row.shares.set_share(child, share);
            }
        }
    }

    /// Remember what every tile measured, for [`Self::keep_panel_sizes`].
    ///
    /// A tile with no rect this frame — one that is hidden — keeps its last known size
    /// rather than being forgotten, so a panel that is hidden and shown again comes back
    /// the width it was. Entries for tiles that no longer exist are dropped.
    fn capture_sizes(&mut self) {
        let live: Vec<TileId> = self.tree.tiles.tile_ids().collect();
        for id in &live {
            if let Some(rect) = self.tree.tiles.rect(*id) {
                self.measured.insert(*id, rect.size());
            }
        }
        self.measured.retain(|id, _| live.contains(id));
    }

    /// **Panels have widths; the image is what flexes.**
    ///
    /// the maintainer, on docking Info onto Develop: the left column got wider, and he wanted it
    /// to stay the width Develop already was. The cause is not a bug — it is what a
    /// linear container does. Info's share left the row, so the total fell and *both*
    /// survivors grew in proportion, including the one he was aiming at.
    ///
    /// The rule here is the one that makes structural changes feel deliberate: after a
    /// drop, a close or a pop-out, every panel keeps the size it had and the pane holding
    /// the image absorbs the difference. Nothing you were not dragging changes size.
    ///
    /// # Why the sizes come from the frame *before*
    ///
    /// `Tiles::rects` is cleared at the start of every `Tree::ui`, and the drop is applied
    /// *inside* that call, on pointer release — so by the time the tree has a new shape,
    /// the sizes that shape replaced are already gone. They are captured at the top of
    /// each frame instead, and the frame that restores them skips the capture so it reads
    /// pre-drop sizes rather than the ones the drop just produced. See `tile_tree`.
    ///
    /// # Why setting shares to point sizes is correct here
    ///
    /// Shares are relative, and `Shares::split` divides the container's real extent by
    /// their sum — so shares proportional to the sizes we want, summing to the extent we
    /// measured, come out as exactly those sizes. `normalise_shares` then rescales them
    /// back to the standard scale, which preserves ratios and so preserves the widths.
    fn keep_panel_sizes(&mut self) {
        let Some(image) = self.tree.tiles.find_pane(&Pane::Image) else {
            return;
        };
        // The image's tile and every container above it: whichever child of a row leads
        // to the image is the one allowed to change size.
        let mut on_image_path = vec![image];
        while let Some(parent) = self
            .tree
            .tiles
            .parent_of(*on_image_path.last().expect("seeded"))
        {
            on_image_path.push(parent);
        }

        for id in self.tree.tiles.tile_ids().collect::<Vec<_>>() {
            let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get(id) else {
                continue;
            };
            let dir = row.dir;
            // Only what is on screen: `split` divides the extent between visible children,
            // so a hidden one is not competing for it.
            let children: Vec<TileId> = row
                .children
                .iter()
                .copied()
                .filter(|c| self.tree.is_visible(*c))
                .collect();
            if children.len() < 2 {
                continue;
            }
            let Some(extent) = self.measured.get(&id).map(|s| along(*s, dir)) else {
                continue;
            };
            let avail = extent - GAP * (children.len() - 1) as f32;
            let Some(flex) = children.iter().position(|c| on_image_path.contains(c)) else {
                // No image beneath this row, so nothing here is the flexible one and
                // there is no honest way to choose. Leave the proportions alone.
                continue;
            };

            // A child with no remembered size is one that has just arrived from a
            // container measured on the other axis. On a horizontal image row it is
            // still a side panel, so give it the same measure as either shipped side;
            // an equal share here is what made a right-edge drop arrive far wider than
            // the panel it was sitting beside. Vertical splits have no comparable
            // standard height and keep the proportional fallback.
            let fallback = avail / children.len() as f32;
            let mut sizes: Vec<f32> = children
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    self.measured.get(c).map_or_else(
                        || {
                            if dir == LinearDir::Horizontal && i != flex {
                                DEVELOP_W
                            } else {
                                fallback
                            }
                        },
                        |s| along(*s, dir),
                    )
                })
                .collect();
            let others: f32 = sizes
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != flex)
                .map(|(_, s)| *s)
                .sum();
            let for_image = avail - others;
            if !for_image.is_finite() || for_image < MIN_PANE {
                // The window cannot honour every panel's width without starving the
                // picture. Proportional is the right answer then — the alternative is a
                // sliver of image, which is worse than a panel that changed size.
                continue;
            }
            sizes[flex] = for_image;

            // **Every child, not only the visible ones**, and this is the other half of
            // the docking bug. Writing point-scale sizes for the visible children while
            // leaving a hidden one on the normalised scale puts two scales in one
            // container — around 0.7 next to 1083 — and `normalise_shares` faithfully
            // preserves that ratio, so the moment the hidden pane came back it was a
            // fraction of a point wide. A hidden child gets its remembered size, which
            // costs nothing (`split` only divides between visible children) and is what
            // brings it back the width it left at.
            let solved: Vec<(TileId, f32)> = children.iter().copied().zip(sizes).collect();
            let hidden: Vec<(TileId, f32)> = {
                let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get(id) else {
                    continue;
                };
                row.children
                    .iter()
                    .copied()
                    .filter(|c| !children.contains(c))
                    .map(|c| {
                        (
                            c,
                            self.measured.get(&c).map_or_else(
                                || {
                                    if dir == LinearDir::Horizontal {
                                        DEVELOP_W
                                    } else {
                                        fallback
                                    }
                                },
                                |s| along(*s, dir),
                            ),
                        )
                    })
                    .collect()
            };

            let Some(Tile::Container(Container::Linear(row))) = self.tree.tiles.get_mut(id) else {
                continue;
            };
            for (child, size) in solved.into_iter().chain(hidden) {
                row.shares.set_share(child, size);
            }
        }
    }

    /// Everything the tree needs settled before it draws.
    ///
    /// A method on `Layout` rather than inline in `tile_tree` because the tests drive the
    /// tree directly, and a harness that skipped this was testing a frame the app never
    /// runs — which is exactly how `keep_panel_sizes` came to be written and then
    /// measured as having no effect.
    ///
    /// **Order matters, and which frame it happens on matters more.** A drop is applied
    /// inside the *previous* `Tree::ui`, after that frame was already laid out, so the new
    /// shape is drawn for the first time now and correcting the shares here means it is
    /// never drawn wrong. `capture_sizes` is skipped on that frame on purpose: the sizes
    /// it would record are the ones the drop produced, and what `keep_panel_sizes` needs
    /// are the ones from before it.
    pub fn before_ui(&mut self) {
        self.tree.simplify(&SIMPLIFY);
        // **Visibility first, and this order is load-bearing.** `keep_panel_sizes` asks
        // which children are on screen; with `settle_visibility` after it, that answer was
        // *last* frame's — so on the frame a panel docked back from floating it was still
        // marked invisible, was left out of the solve, and arrived a fraction of a point
        // wide against the window edge.
        self.settle_visibility();
        if std::mem::take(&mut self.restructured) {
            self.keep_panel_sizes();
        } else {
            self.capture_sizes();
        }
        // Before the pass that could otherwise draw a dropped pane one point wide.
        self.normalise_shares();
    }

    /// `tab`: hide every panel, or bring every panel back.
    ///
    /// Both at once, because the point of the key is to see the picture with nothing
    /// around it — hiding them one at a time would be two keys for a gesture whose
    /// whole purpose is "get out of the way". Restoring *all* of them is also what
    /// makes a closed panel reachable again: close Info, and `tab` twice brings it
    /// back. There is no state in which a panel is gone with no way to it.
    pub fn toggle_panels(&mut self) {
        self.restructured = true;
        if self.shown.is_empty() {
            self.shown = Pane::PANELS.to_vec();
            // "Every panel back" includes the ones tucked at the edge.
            for (id, width) in self.tuck.take() {
                self.remember_width(id, width);
            }
        } else {
            self.shown.clear();
        }
        self.dirty = true;
    }

    /// Popped out into a window of its own — the *state*, which survives `tab`.
    pub fn is_out(&self, pane: Pane) -> bool {
        self.out.contains(&pane)
    }

    /// `tab` has taken the panels away.
    ///
    /// **A popped-out window goes with them**, which is what 7c did and is right: the
    /// whole purpose of the key is "get out of the way", and a floating Develop still
    /// covering half the screen would not be out of the way. It is hidden, not docked
    /// — press `tab` again and the window comes back where it was.
    pub fn panels_hidden(&self) -> bool {
        self.shown.is_empty()
    }

    /// Pop a panel into its own window.
    ///
    /// The image never floats. It has no header to pop out from, so this cannot be
    /// reached for it; the guard is here because "the image stays in the frame" is
    /// the same invariant [`closable`] states, and it should be stated once.
    fn float(&mut self, pane: Pane) {
        self.restructured = true;
        if closable(pane) && !self.out.contains(&pane) {
            self.out.push(pane);
        }
    }

    /// Put it back in the frame, where it left from.
    ///
    /// Closing the window is docking, not hiding: a panel that vanished with no way
    /// back would be a control that destroys itself.
    pub fn dock(&mut self, pane: Pane) {
        self.restructured = true;
        self.out.retain(|p| *p != pane);
        if !self.shown.contains(&pane) {
            self.shown.push(pane);
            self.dirty = true;
        }
    }

    /// Make `pane` the visible one in whatever tab strip it is in, and bring it
    /// back if it was closed.
    ///
    /// **Why a mode needs this.** The brush is tabbed behind Develop by default, so
    /// pressing `d` or `x` would otherwise start a tool whose entire panel is
    /// hidden — the picture would take a stroke and nothing on screen would say
    /// where it went. A mode that opens has to make its own controls reachable.
    ///
    /// Deliberately does **not** un-float or un-hide: a panel someone popped onto
    /// another screen is already reachable, and `tab` hiding the panels is a
    /// statement about the whole frame that one key press should not overrule.
    pub fn bring_forward(&mut self, pane: Pane) {
        // **Both guards before anything else**, because `panels_hidden` is
        // `shown.is_empty()` — reopening the panel first is what makes it stop being
        // hidden, so a version that pushed and then checked would un-hide every time.
        // `bringing_a_panel_forward_does_not_undo_tab_or_a_pop_out` caught exactly
        // that.
        if self.panels_hidden() || self.out.contains(&pane) {
            return;
        }
        // A *closed* panel does come back: it has no window and no other way to
        // reach it, so a mode opening onto it would otherwise be invisible.
        if !self.shown.contains(&pane) {
            self.shown.push(pane);
            self.dirty = true;
            self.restructured = true;
        }
        if let Some(tile) = self.tree.tiles.find_pane(&pane) {
            // A mode opening onto a tucked column has to bring the column out, for
            // the same reason it reopens a closed one.
            if let Some((id, width)) = self.tuck.release_containing(&self.tree, tile) {
                self.remember_width(id, width);
            }
            self.tree.make_active(|id, _| id == tile);
        }
    }

    fn close(&mut self, pane: Pane) {
        self.restructured = true;
        if closable(pane) {
            self.shown.retain(|p| *p != pane);
            self.dirty = true;
        }
    }

    /// Push `shown` and `out` down into the tree, and take any container whose
    /// children have all gone with them.
    ///
    /// The second half is not optional. A `Tabs` container holding only hidden panes
    /// still draws its bar and still claims its slot, so pressing `tab` with Develop
    /// and Info stacked would leave an empty strip where the panels were instead of
    /// giving the space to the picture.
    fn settle_visibility(&mut self) {
        for pane in Pane::ALL {
            let visible =
                pane == Pane::Image || (self.shown.contains(&pane) && !self.out.contains(&pane));
            if let Some(id) = self.tree.tiles.find_pane(&pane) {
                self.tree.set_visible(id, visible);
            }
        }
        if let Some(root) = self.tree.root {
            settle_container(&mut self.tree, root);
        }
        if let Some(image) = self.tree.tiles.find_pane(&Pane::Image) {
            self.tuck.settle(&mut self.tree, image);
        }
    }

    #[cfg(test)]
    /// The side columns, `[left, right]`, including tucked ones. See
    /// [`Tuck::edge_columns`].
    fn edge_columns(&self) -> [Option<(TileId, egui::Rect)>; 2] {
        match self.tree.tiles.find_pane(&Pane::Image) {
            Some(image) => self.tuck.edge_columns(&self.tree, image),
            None => [None, None],
        }
    }

    /// The width a hidden tile should come back at, for `keep_panel_sizes`.
    fn remember_width(&mut self, id: TileId, width: f32) {
        let h = self.measured.get(&id).map_or(0.0, |s| s.y);
        self.measured.insert(id, egui::vec2(width, h));
        self.restructured = true;
        self.dirty = true;
    }

    #[cfg(test)]
    fn tuck(&mut self, id: TileId, width: f32) {
        if self.tuck.push(id, width) {
            self.restructured = true;
            self.dirty = true;
        }
    }

    #[cfg(test)]
    fn untuck(&mut self, id: TileId) {
        if let Some(width) = self.tuck.remove(id) {
            self.remember_width(id, width);
        }
    }

    /// See [`Tuck::drag`]. True when a column tucked, which ends the drag.
    fn tuck_from_drag(&mut self, input: &egui::InputState, resizing: bool) -> bool {
        let Some(image) = self.tree.tiles.find_pane(&Pane::Image) else {
            return false;
        };
        let fired = self
            .tuck
            .drag(&self.tree, image, input, resizing, [DEVELOP_W, INFO_W]);
        if fired {
            self.restructured = true;
            self.dirty = true;
        }
        fired
    }

    /// See [`Tuck::strips`].
    fn tuck_strips(&mut self, ui: &mut egui::Ui, area: egui::Rect) {
        let Some(image) = self.tree.tiles.find_pane(&Pane::Image) else {
            return;
        };
        if let Some((id, width)) = self.tuck.strips(&self.tree, image, ui, area) {
            self.remember_width(id, width);
        }
    }

    /// The panes a tab bar is already naming. Their bodies drop their own title
    /// rather than saying it twice.
    fn tabbed(&self) -> Vec<TileId> {
        let mut out = Vec::new();
        for (_, tile) in self.tree.tiles.iter() {
            if let Tile::Container(Container::Tabs(tabs)) = tile {
                out.extend(tabs.children.iter().copied());
            }
        }
        out
    }
}

/// A container is visible when any of its children are. Depth first, because the
/// answer for a container depends on the answer for everything under it.
fn settle_container(tree: &mut egui_tiles::Tree<Pane>, id: TileId) -> bool {
    let children = match tree.tiles.get(id) {
        Some(Tile::Container(container)) => container.children_vec(),
        _ => return tree.is_visible(id),
    };
    let mut any = false;
    for child in children {
        any |= settle_container(tree, child);
    }
    tree.set_visible(id, any);
    any
}

/// How a panel's header row is drawn this frame.
///
/// The same body serves a docked pane, a tabbed pane and a floating window, and each
/// wants a different amount of chrome around it: a tab bar has already said the name,
/// and a window that is already popped out has no use for a pop-out button.
#[derive(Clone, Copy)]
pub struct Head {
    /// Draw the panel's name, sensed under this id so it is the handle the tree
    /// drags the pane by. `None` when a tab bar above is already naming it — the tab
    /// is the handle then — or in a floating window, which has no tree to drag in.
    ///
    /// **The id is not free to choose.** It must be `TileId::egui_id(tree_id)`, the
    /// same id `egui_tiles` senses its tabs under, because that is the id
    /// `is_being_dragged` looks for. See [`panel_head`].
    pub handle: Option<egui::Id>,
    /// Offer the pop-out button.
    pub float: bool,
}

impl Head {
    /// A floating window: `floating_bar` above it has already named it, it is as
    /// popped out as it can get, and there is no tile to drag.
    pub const FLOATING: Self = Self {
        handle: None,
        float: false,
    };
}

/// What a panel header reported this frame.
#[derive(Default, Clone, Copy)]
pub struct HeadClicks {
    /// A drag started on the title, which is this pane's handle in the tree.
    pub dragged: bool,
    /// The pop-out button was clicked.
    pub float: bool,
}

/// The title row every docked panel carries: its name, and the controls that act on
/// the panel as a whole.
///
/// **The title is the drag handle**, in the manner of the Qt dock the prototype
/// drags by its title bar. Sensing the whole row was tried first and is what
/// `floating_bar` does, but a row that senses drags on top of the buttons in it is
/// one misread click away from resetting a panel you meant to move. The title is
/// unambiguous, and when the pane is tabbed the tab is the handle instead.
///
/// `right` adds anything that belongs between the pop-out button and the title —
/// Develop's whole-panel reset. Drawn right to left, like every other trailing
/// control in the app.
///
/// # The handle must sense under the tile's own egui id
///
/// This is the whole trick, and getting it wrong makes the drag look implemented and
/// do nothing. `Ui::set_dragged_id`, which is what `egui_tiles` calls when `pane_ui`
/// answers `DragStarted`, writes into `interact_widgets` — and egui **recomputes that
/// from its own hit-testing every pass**. So a title that is an ordinary `Label` with
/// its own id has egui reporting *the label* as the dragged widget from the second
/// frame onward, `is_being_dragged(tile_id)` goes false, and the drag evaporates one
/// frame after it began: a preview that never appears and a drop that never lands.
///
/// Sensing the title under `TileId::egui_id(tree_id)` makes egui's own answer the one
/// `egui_tiles` is asking for, and the drag needs no override to survive. It is
/// exactly what the crate's own `tab_ui` does, which is why dragging a *tab* worked
/// while dragging a lone panel did not. `the_panel_handle_senses_under_the_tile_id`
/// pins it.
/// How far the panel title is inset from the pane's left edge.
///
/// Matched to the inset a module frame gives its own contents, so the title lines up
/// with the controls under it rather than sitting proud of them.
const TITLE_INSET: f32 = 8.0;

/// What a panel says when there is nothing to describe — **inset like everything else
/// in it.**
///
/// A helper rather than five `ui.label` calls, because five was exactly the number that
/// let the margin be added to the title and missed on the empty state. `panel_head`
/// already carries a note about this complaint: the maintainer reported the panel *names* sitting
/// on the pane's edge, and the fix inset the title. The captions that replace a panel's
/// body were flush against the same edge for the same reason and were not spotted,
/// because you only see them with no image open — which is the one state you stop
/// looking at once the app has a picture in it.
///
/// `TITLE_INSET`, so the sentence lines up with the panel name directly above it.
pub fn empty_state(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(TITLE_INSET);
        ui.label(theme::caption(text));
    });
}

pub fn panel_head(
    ui: &mut egui::Ui,
    icons: &icons::Icons,
    head: Head,
    name: &str,
    right: impl FnOnce(&mut egui::Ui),
) -> HeadClicks {
    let mut out = HeadClicks::default();
    // A lone docked pane has no tab bar to frame it. Tabbed panes get this rule from
    // `paint_tab_rules`; floating panes already sit inside a native window frame.
    if head.handle.is_some() {
        let rect = ui.available_rect_before_wrap();
        ui.painter().line_segment(
            [
                egui::pos2(rect.left(), rect.top() + 0.5),
                egui::pos2(rect.right(), rect.top() + 0.5),
            ],
            panel_rule(),
        );
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // **A left margin on the title.** the maintainer: the panel names sit too close to the
        // edge. They did — the horizontal starts at the panel's own inner edge, and a
        // heading welded to it reads as though it has fallen off rather than as
        // though it labels what is below. The panel's other content is inset by its
        // module frames; only the head was flush.
        ui.add_space(TITLE_INSET);
        if let Some(handle) = head.handle {
            // Painted first, then interacted under the tile's id. Not
            // `Label::sense`, which would register the label's own id instead.
            let title = ui.add(egui::Label::new(egui::RichText::new(name).heading()));
            let drag = ui
                .interact(title.rect, handle, egui::Sense::click_and_drag())
                .on_hover_cursor(egui::CursorIcon::Grab)
                .on_hover_text(theme::tip(
                    "drag to move — either side of the image, above or below another \
                     panel, or onto one to stack them as tabs",
                ));
            out.dragged = drag.drag_started();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if head.float
                && icons::button(ui, icons, "float", "↗", true)
                    .on_hover_text(theme::tip("open in its own window"))
                    .clicked()
            {
                out.float = true;
            }
            right(ui);
        });
    });
    ui.add_space(6.0);
    out
}

// ---------------------------------------------------------------------------
// The behaviour: how the tree draws, and what it looks like
// ---------------------------------------------------------------------------

/// Borrows the whole app for the length of one tree pass.
///
/// The tree itself is moved out of `App` first — `pane_ui` needs `&mut App` and the
/// tree lives on it — and moved back afterwards. `Tree::empty` is the placeholder,
/// and it allocates nothing.
struct Panes<'a> {
    app: &'a mut App,
    ctx: &'a egui::Context,
    rs: &'a egui_wgpu::RenderState,
    actions: &'a [hotkeys::Action],
    /// The tree's own id, which the drag handles have to be keyed from. Carried
    /// because the tree itself is moved out of `App` for the pass.
    tree_id: egui::Id,
    /// Panes a tab bar is already naming.
    tabbed: Vec<TileId>,
    /// Collected during the pass and acted on after it, because both of them mutate
    /// the layout the tree pass is holding.
    float: Option<Pane>,
    close: Option<Pane>,
    edited: bool,
    dropped: bool,
    /// A seam was dragged this frame. See [`Layout::tuck_from_drag`].
    resizing: bool,
    /// Which of the two tone/brush panes the tree actually drew this frame. Read by
    /// [`App::tile_tree`], which drops paint mode when the Dodge/Burn pane is neither
    /// drawn nor floating — the branch on `brush_gone`. **Untested**: the rule lives
    /// in `App`, and every test here drives a bare `Layout`.
    drew_develop: bool,
    drew_dodgeburn: bool,
}

/// What chrome a docked pane's header should carry, and the id its handle must sense
/// under.
///
/// A free function so the headless tests build a `Head` the same way the app does —
/// the id here is the load-bearing part, and a test that derived it independently
/// would pass while the app was broken. See [`panel_head`].
fn head_for(tree_id: egui::Id, tabbed: &[TileId], tile_id: TileId) -> Head {
    Head {
        handle: (!tabbed.contains(&tile_id)).then(|| tile_id.egui_id(tree_id)),
        float: true,
    }
}

impl egui_tiles::Behavior<Pane> for Panes<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut Pane) -> UiResponse {
        let head = head_for(self.tree_id, &self.tabbed, tile_id);
        let panel = theme::background_of(self.app.settings.panel_background());
        match *pane {
            Pane::Image => {
                // The letterbox and the shader's fill are one colour with a seam
                // between them if they disagree, so both read the same setting. The
                // pane paints it because a tile is handed a bare `Ui` — there is no
                // `Frame` around it the way `CentralPanel` had one.
                let bg = theme::background_of(self.app.settings.background());
                ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
                self.app.viewport_panel(ui, self.ctx, self.rs, self.actions);
                UiResponse::None
            }
            Pane::Develop => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                self.drew_develop = true;
                let clicks = self.app.develop_column(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::Develop));
                drag(clicks.dragged)
            }
            Pane::DodgeBurn => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                self.drew_dodgeburn = true;
                let clicks = self.app.dodgeburn_panel(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::DodgeBurn));
                drag(clicks.dragged)
            }
            Pane::Toning => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                let clicks = self.app.toning_panel(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::Toning));
                drag(clicks.dragged)
            }
            Pane::Info => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                let clicks = self.app.info_panel(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::Info));
                drag(clicks.dragged)
            }
            Pane::Snapshots => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                let clicks = self.app.snapshot_panel(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::Snapshots));
                drag(clicks.dragged)
            }
            Pane::History => {
                ui.painter().rect_filled(ui.max_rect(), 0.0, panel);
                let clicks = self.app.history_panel(ui, head);
                self.float = self.float.or(clicks.float.then_some(Pane::History));
                drag(clicks.dragged)
            }
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        pane.label().into()
    }

    fn is_tab_closable(&self, tiles: &egui_tiles::Tiles<Pane>, tile_id: TileId) -> bool {
        tiles.get_pane(&tile_id).copied().is_some_and(closable)
    }

    /// Closing a panel **hides** it rather than removing it from the tree, so it
    /// keeps its place and `tab` can bring it back. Returning false is what aborts
    /// the removal `egui_tiles` would otherwise do.
    fn on_tab_close(&mut self, tiles: &mut egui_tiles::Tiles<Pane>, tile_id: TileId) -> bool {
        self.close = tiles.get_pane(&tile_id).copied();
        false
    }

    /// A tab title is a field marker, letter-spaced through `theme::paint_header`,
    /// and **the active tab is ruby text, not text on ruby** — the rule the image tab
    /// strip already sets. A filled swatch behind a name is a lot of ink for "this
    /// one", and it fights the ruby that means *interaction* everywhere else.
    ///
    /// Reimplemented rather than themed through the colour hooks because neither the
    /// tracking nor the app's own close icon can be reached from them; the default
    /// draws a plain galley and two hand-drawn lines. The interaction contract is
    /// `egui_tiles`' and is kept exactly: the tab senses `click_and_drag` under the
    /// `id` it was handed, and the close senses afterwards so it takes the click back.
    fn tab_ui(
        &mut self,
        tiles: &mut egui_tiles::Tiles<Pane>,
        ui: &mut egui::Ui,
        id: egui::Id,
        tile_id: TileId,
        state: &egui_tiles::TabState,
    ) -> egui::Response {
        let name = tiles.get_pane(&tile_id).map_or("—", |p| p.label());
        let title = theme::header_size(ui.painter(), name);
        let pad = self.tab_title_spacing(ui.visuals());
        let close_w = if state.closable {
            icons::BOX + pad
        } else {
            0.0
        };
        let (_, rect) = ui.allocate_space(egui::vec2(
            title.x + 2.0 * pad + close_w,
            ui.available_height(),
        ));

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

        // A gap where the tab was, while it is being dragged.
        if !ui.is_rect_visible(rect) || state.is_being_dragged {
            return tab;
        }

        paint_tab_rules(ui, rect);

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

        if state.closable {
            let close_rect = egui::Rect::from_center_size(
                egui::pos2(rect.right() - pad - icons::BOX * 0.5, rect.center().y),
                egui::Vec2::splat(icons::BOX),
            );
            // After the tab's own `interact`, so the later widget wins the click and
            // the × closes rather than starting a drag of the tab under it.
            let close = ui
                // Keyed off the tab's own id rather than the `Ui`'s running auto-id,
                // so it is the same button frame to frame however the bar is scrolled.
                .interact(close_rect, id.with("close"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(theme::tip("close panel — tab brings them back"));
            let hot = close.hovered();
            if hot {
                ui.painter()
                    .rect_filled(close_rect, 2.0, egui::Color32::from_gray(64));
            }
            let tint = if hot {
                egui::Color32::from_gray(245)
            } else {
                theme::DIM
            };
            icons::paint(ui, &self.app.icons, "close", "×", close_rect, tint);
            if close.clicked() && self.on_tab_close(tiles, tile_id) {
                tiles.remove(tile_id);
            }
        }
        tab
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SIMPLIFY
    }

    fn top_bar_right_ui(
        &mut self,
        tiles: &egui_tiles::Tiles<Pane>,
        ui: &mut egui::Ui,
        _tile_id: TileId,
        tabs: &egui_tiles::Tabs,
        scroll_offset: &mut f32,
    ) {
        let pad = self.tab_title_spacing(ui.visuals());
        let required: f32 = tabs
            .children
            .iter()
            .filter(|id| tiles.is_visible(**id))
            .filter_map(|id| tiles.get_pane(id))
            .map(|pane| {
                theme::header_size(ui.painter(), pane.label()).x
                    + 2.0 * pad
                    + if closable(*pane) {
                        icons::BOX + pad
                    } else {
                        0.0
                    }
            })
            .sum();
        if ui.available_width() >= required {
            *scroll_offset = 0.0;
        }
    }

    fn tab_bar_height(&self, _style: &egui::Style) -> f32 {
        // Deep enough for a HEADER-sized field marker with the panel head's own 6pt
        // of air either side of it, so a tab bar and a panel title sit at the same
        // height whichever a pane happens to have.
        theme::size::HEADER + 12.0
    }

    fn tab_bar_color(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::CHROME_DEEP
    }

    /// Transparent, always: the active tab is said in ruby text. See `tab_ui`.
    fn tab_bg_color(
        &self,
        _visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<Pane>,
        _tile_id: TileId,
        _state: &egui_tiles::TabState,
    ) -> egui::Color32 {
        egui::Color32::TRANSPARENT
    }

    fn tab_outline_stroke(
        &self,
        _visuals: &egui::Visuals,
        _tiles: &egui_tiles::Tiles<Pane>,
        _tile_id: TileId,
        _state: &egui_tiles::TabState,
    ) -> egui::Stroke {
        egui::Stroke::NONE
    }

    /// The hairline the module frame and the title strip already use. One grey for
    /// every dividing line in the app.
    fn tab_bar_hline_stroke(&self, _visuals: &egui::Visuals) -> egui::Stroke {
        panel_rule()
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        GAP
    }

    fn min_size(&self) -> f32 {
        MIN_PANE
    }

    /// The seam between two panes, and the handle that moves it. Ruby while it is
    /// being dragged, because ruby means interaction here and nothing else.
    fn resize_stroke(&self, _style: &egui::Style, state: egui_tiles::ResizeState) -> egui::Stroke {
        match state {
            egui_tiles::ResizeState::Idle => egui::Stroke::new(1.0, egui::Color32::from_gray(52)),
            egui_tiles::ResizeState::Hovering => egui::Stroke::new(1.0, theme::DIM),
            egui_tiles::ResizeState::Dragging => egui::Stroke::new(1.0, theme::RUBY),
        }
    }

    /// Where the pane will land. A quiet wash rather than egui's default half-opaque
    /// selection colour, which over a monochrome rendering reads as a colour cast on
    /// the picture rather than as a hint about the drag.
    fn drag_preview_color(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::RUBY.gamma_multiply(0.18)
    }

    fn drag_preview_stroke(&self, _visuals: &egui::Visuals) -> egui::Stroke {
        egui::Stroke::new(1.0, theme::RUBY)
    }

    fn dragged_overlay_color(&self, _visuals: &egui::Visuals) -> egui::Color32 {
        theme::CHROME.gamma_multiply(0.6)
    }

    /// The label that follows the cursor while a pane is in flight. The default is
    /// `Frame::popup`, which is egui's own rounded light-bordered box.
    fn drag_ui(&mut self, tiles: &egui_tiles::Tiles<Pane>, ui: &mut egui::Ui, tile_id: TileId) {
        let name = self.tab_title_for_tile(tiles, tile_id).text().to_owned();
        egui::Frame::new()
            .fill(theme::CHROME_DEEP)
            .stroke(egui::Stroke::new(1.0, theme::RUBY))
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                theme::header_label(ui, &name);
            });
    }

    fn on_edit(&mut self, action: egui_tiles::EditAction) {
        self.edited = true;
        // A resize writes shares from measured widths and is the user setting a width
        // directly; only a drop changes which children a container has.
        self.dropped |= action == egui_tiles::EditAction::TileDropped;
        self.resizing |= action == egui_tiles::EditAction::TileResized;
    }

    fn on_seam_double_click(
        &mut self,
        tiles: &egui_tiles::Tiles<Pane>,
        shares: &mut egui_tiles::Shares,
        dir: LinearDir,
        children: &[TileId],
        pair: [TileId; 2],
    ) -> bool {
        tiles.find_pane(&Pane::Image).is_some_and(|image| {
            reset_seam(
                tiles,
                shares,
                dir,
                children,
                pair,
                image,
                [DEVELOP_W, INFO_W],
            )
            .is_some()
        })
    }
}

/// **Double-click a seam and the side panel on it goes back to its shipped width**;
/// the tile on the path to `flex` — the image, or the Lightbox's grid — takes up the
/// difference. `defaults` are `[panel left of flex, panel right of flex]`.
///
/// Called from `Behavior::on_seam_double_click`, which `egui_tiles` asks with the
/// seam egui itself hit (see `vendor/egui_tiles/LOCAL-PATCH.md`). This used to run
/// after the tree instead, re-deriving the seam from the pointer; egui's hit test
/// reaches a few points further than that did, so a double-click on a panel's scroll
/// bar landed on the seam, missed the reset, and got upstream's even split — the
/// panel went to half the window.
///
/// `None` for a seam with the flexible tile on neither side, or on both, which is
/// left to upstream's evening-out.
pub(crate) fn reset_seam<P>(
    tiles: &egui_tiles::Tiles<P>,
    shares: &mut egui_tiles::Shares,
    dir: LinearDir,
    children: &[TileId],
    [left, right]: [TileId; 2],
    flex: TileId,
    defaults: [f32; 2],
) -> Option<(TileId, f32)> {
    if dir != LinearDir::Horizontal {
        return None;
    }
    let mut flex_path = vec![flex];
    while let Some(parent) = tiles.parent_of(*flex_path.last().expect("seeded")) {
        flex_path.push(parent);
    }
    let flex_left = flex_path.contains(&left);
    if flex_left == flex_path.contains(&right) {
        return None;
    }
    let (panel, flex_side, desired) = if flex_left {
        (right, left, defaults[1])
    } else {
        (left, right, defaults[0])
    };
    let total = tiles.rect(left)?.width() + tiles.rect(right)?.width();
    let width = desired.min(total - MIN_PANE).max(MIN_PANE);
    for child in children {
        let size = if *child == panel {
            width
        } else if *child == flex_side {
            total - width
        } else {
            tiles.rect(*child)?.width()
        };
        shares.set_share(*child, size);
    }
    Some((panel, width))
}

fn drag(started: bool) -> UiResponse {
    if started {
        UiResponse::DragStarted
    } else {
        UiResponse::None
    }
}

impl App {
    /// Draw the tile tree, and act on what it reported.
    ///
    /// The tree is moved out of `self` for the duration: `pane_ui` needs the whole
    /// app, and the tree is part of it.
    pub fn tile_tree(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        rs: &egui_wgpu::RenderState,
        actions: &[hotkeys::Action],
    ) {
        // Simplify before reading the tree rather than only inside `Tree::ui`, which
        // does it too. The frame after a drop, the tree still carries the wrapper
        // container the drop left behind — so a pane that is about to stop being
        // tabbed would draw one frame with both a tab bar and its own title.
        self.layout.before_ui();
        let tabbed = self.layout.tabbed();

        self.layout.restore_pixel_widths(ui.available_width());
        let mut tree = std::mem::replace(
            &mut self.layout.tree,
            egui_tiles::Tree::empty("monopro-layout-placeholder"),
        );
        let tree_id = tree.id();
        let mut panes = Panes {
            app: self,
            ctx,
            rs,
            actions,
            tree_id,
            tabbed,
            float: None,
            close: None,
            edited: false,
            dropped: false,
            resizing: false,
            drew_develop: false,
            drew_dodgeburn: false,
        };
        let area = ui.available_rect_before_wrap();
        tree.ui(&mut panes, ui);
        let (float, close, edited, dropped) =
            (panes.float, panes.close, panes.edited, panes.dropped);
        let (drew_develop, drew_dodgeburn) = (panes.drew_develop, panes.drew_dodgeburn);
        let resizing = panes.resizing;
        self.layout.tree = tree;
        // Outside `input`: ending the drag writes to the context `input` is reading.
        if ui.input(|i| self.layout.tuck_from_drag(i, resizing)) {
            ui.ctx().stop_dragging();
        }
        self.layout.tuck_strips(ui, area);

        // **Leaving the brush behind leaves paint mode.** the maintainer's ask, and the default
        // layout is why it is needed: Develop and Dodge & Burn are two tabs of one
        // tile, so clicking DEVELOP puts the brush panel away — and paint mode stayed
        // on behind it. You then had a picture that took brush strokes with nothing on
        // screen saying so, and no obvious way back out, which is precisely the
        // failure `Mode::hint` exists to prevent. The hint was even still in the
        // footer; it just no longer had a panel to belong to.
        //
        // **The test is "the brush is not on screen", not "the Develop tab was
        // clicked."** Written the second way it would fire when the two panes are
        // dragged apart and both are visible — a layout the tiling exists to allow —
        // and paint mode would be unusable for anyone who arranged it that way. Asking
        // what was actually drawn also covers closing the panel and hiding all panels
        // with `tab`, which want the same answer for the same reason.
        //
        // `out` is checked because a floated Dodge & Burn draws in its own viewport
        // and so never reaches `pane_ui`; it is still on screen and still the brush.
        //
        // **`Mode::View`, which is `↵` and not `Esc`** — navigating away keeps the
        // strokes. Discarding them would make a tab click the most destructive control
        // in the window.
        let brush_gone = !drew_dodgeburn && !self.layout.out.contains(&Pane::DodgeBurn);
        if drew_develop
            && brush_gone
            && let Some(t) = self.tabs.active_mut()
            && t.mode.is_paint()
        {
            t.mode = crate::tabs::Mode::View;
        }

        if let Some(pane) = float {
            self.layout.float(pane);
        }
        if let Some(pane) = close {
            self.layout.close(pane);
        }
        self.layout.dirty |= edited;
        self.layout.restructured |= dropped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree with everything the app puts round a pane except the pane's own body,
    /// which needs a GPU. Same `Behavior`, same `panel_head`, same
    /// `SimplificationOptions` — so what these tests pin is what ships.
    struct Bare {
        tree_id: egui::Id,
        icons: icons::Icons,
        tabbed: Vec<TileId>,
        dropped: bool,
        resizing: bool,
    }

    impl Bare {
        fn new(tree: &egui_tiles::Tree<Pane>) -> Self {
            Self {
                tree_id: tree.id(),
                icons: icons::Icons::empty(),
                tabbed: Vec::new(),
                dropped: false,
                resizing: false,
            }
        }
    }

    impl egui_tiles::Behavior<Pane> for Bare {
        fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut Pane) -> UiResponse {
            if *pane == Pane::Image {
                ui.label(pane.label());
                return UiResponse::None;
            }
            // The real header, through the real function and the real id derivation.
            // Anything less and the drag contract would be tested against a copy of
            // itself.
            let head = head_for(self.tree_id, &self.tabbed, tile_id);
            let clicks = panel_head(ui, &self.icons, head, pane.label(), |_| {});
            // Enough content to be measured, and in the develop pane the same module
            // frame the real one draws — see `the_tree_does_not_run_away`.
            if *pane == Pane::Develop {
                let _ = crate::widgets::Module::new("DECODE")
                    .modified(true)
                    .show(ui, |ui| {
                        ui.label("a control");
                    });
                let _ = crate::widgets::Module::new("CURVE")
                    .modified(true)
                    .switch(true)
                    .show(ui, |ui| {
                        ui.label("a much much much wider control");
                    });
            }
            drag(clicks.dragged)
        }

        fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
            pane.label().into()
        }

        fn on_edit(&mut self, action: egui_tiles::EditAction) {
            self.dropped |= action == egui_tiles::EditAction::TileDropped;
            self.resizing |= action == egui_tiles::EditAction::TileResized;
        }

        fn simplification_options(&self) -> SimplificationOptions {
            // The real ones, so what these tests pin is what ships.
            SIMPLIFY
        }

        // The real stop, or a squeeze would never reach the point where a column tucks.
        fn min_size(&self) -> f32 {
            MIN_PANE
        }

        fn on_seam_double_click(
            &mut self,
            tiles: &egui_tiles::Tiles<Pane>,
            shares: &mut egui_tiles::Shares,
            dir: LinearDir,
            children: &[TileId],
            pair: [TileId; 2],
        ) -> bool {
            tiles.find_pane(&Pane::Image).is_some_and(|image| {
                reset_seam(
                    tiles,
                    shares,
                    dir,
                    children,
                    pair,
                    image,
                    [DEVELOP_W, INFO_W],
                )
                .is_some()
            })
        }

        fn gap_width(&self, _style: &egui::Style) -> f32 {
            GAP
        }
    }

    /// The pane of a tile, for assertions.
    fn pane_of(tree: &egui_tiles::Tree<Pane>, id: TileId) -> Option<Pane> {
        tree.tiles.get_pane(&id).copied()
    }

    fn tile_of(tree: &egui_tiles::Tree<Pane>, pane: Pane) -> TileId {
        tree.tiles
            .find_pane(&pane)
            .expect("every pane is in the tree")
    }

    /// The image, with Develop and Info stacked as tabs beside it.
    ///
    /// Built rather than dragged: the drag itself is `egui_tiles`' and is exercised
    /// by the app, but the *shape* it produces is what the simplification options
    /// decide, and that is ours.
    fn stacked() -> Layout {
        let mut tiles = egui_tiles::Tiles::default();
        let image = tiles.insert_pane(Pane::Image);
        let develop = tiles.insert_pane(Pane::Develop);
        let info = tiles.insert_pane(Pane::Info);
        // Along for the ride: `heal` rejects a tree whose pane set is not
        // `Pane::ALL`, so a fixture that omitted one would be rejected for a reason
        // that has nothing to do with what it is testing.
        let brush = tiles.insert_pane(Pane::DodgeBurn);
        let toning = tiles.insert_pane(Pane::Toning);
        let snapshots = tiles.insert_pane(Pane::Snapshots);
        let history = tiles.insert_pane(Pane::History);
        let tabs = tiles.insert_tab_tile(vec![develop, info]);
        let root =
            tiles.insert_horizontal_tile(vec![image, tabs, brush, toning, snapshots, history]);
        Layout {
            tree: egui_tiles::Tree::new("stacked", root, tiles),
            shown: Pane::PANELS.to_vec(),
            out: Vec::new(),
            dirty: false,
            restructured: false,
            measured: Default::default(),
            restore_pixels: true,
            tuck: Tuck::default(),
        }
    }

    #[test]
    fn side_panel_widths_fit_their_tab_headers() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            for (panes, width) in [
                ([Pane::Develop, Pane::DodgeBurn, Pane::Toning], DEVELOP_W),
                ([Pane::Info, Pane::Snapshots, Pane::History], INFO_W),
            ] {
                let required: f32 = panes
                    .iter()
                    .map(|pane| {
                        theme::header_size(ui.painter(), pane.label()).x + 3.0 * 8.0 + icons::BOX
                    })
                    .sum();
                assert!(
                    width >= required.ceil(),
                    "{:?} needs {required}, has {width}",
                    panes[0]
                );
                assert!(
                    width - required < 4.0,
                    "keep the default close to the measured tab width"
                );
            }
        });
    }

    #[test]
    fn the_default_layout_holds_every_pane_exactly_once() {
        let mut layout = Layout::default();
        assert!(
            layout.heal(),
            "the default layout must be one the app can run"
        );
        // And in the order they shipped: develop, image, info — with the
        // brush tabbed behind develop, which Dodge & Burn added and the one
        // place the default tree is not a flat row.
        let root = layout.tree.root.expect("a root");
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("the default root is a horizontal row");
        };
        assert_eq!(row.children.len(), 3, "three columns, not four");
        let Some(Tile::Container(Container::Tabs(tabs))) = layout.tree.tiles.get(row.children[0])
        else {
            panic!("the first column is a tab strip");
        };
        let tabbed: Vec<_> = tabs
            .children
            .iter()
            .filter_map(|c| pane_of(&layout.tree, *c))
            .collect();
        assert_eq!(tabbed, vec![Pane::Develop, Pane::DodgeBurn, Pane::Toning]);
        assert_eq!(
            tabs.active.and_then(|a| pane_of(&layout.tree, a)),
            Some(Pane::Develop),
            "an untouched layout opens on the tone chain, not on a tool with nothing in it"
        );
        // The image, then the right-hand column — a tab strip too since the snapshot
        // list arrived, for the same reason the left one is: Snapshots is a panel you
        // read, so it shares Info's side rather than taking a fourth column out of the
        // picture.
        let rest: Vec<_> = row.children[1..]
            .iter()
            .filter_map(|c| pane_of(&layout.tree, *c))
            .collect();
        assert_eq!(
            rest,
            vec![Pane::Image],
            "only the image is a bare pane in the row"
        );
        let Some(Tile::Container(Container::Tabs(right))) = layout.tree.tiles.get(row.children[2])
        else {
            panic!("the third column is a tab strip");
        };
        let tabbed: Vec<_> = right
            .children
            .iter()
            .filter_map(|c| pane_of(&layout.tree, *c))
            .collect();
        assert_eq!(tabbed, vec![Pane::Info, Pane::Snapshots, Pane::History]);
        assert_eq!(
            right.active.and_then(|a| pane_of(&layout.tree, a)),
            Some(Pane::Info),
            "an untouched layout opens on the readout, not on a list that is empty \
             until the first capture"
        );
    }

    #[test]
    fn opening_a_mode_brings_its_panel_to_the_front() {
        // The brush is tabbed behind Develop, so pressing `d` without this starts a
        // tool whose entire panel is hidden: the picture takes a stroke and nothing
        // on screen says where it went.
        let mut layout = Layout::default();
        let active = |l: &Layout| {
            let root = l.tree.root.expect("a root");
            let Some(Tile::Container(Container::Linear(row))) = l.tree.tiles.get(root) else {
                panic!("a row")
            };
            let Some(Tile::Container(Container::Tabs(tabs))) = l.tree.tiles.get(row.children[0])
            else {
                panic!("a tab strip")
            };
            tabs.active.and_then(|a| pane_of(&l.tree, a))
        };
        assert_eq!(
            active(&layout),
            Some(Pane::Develop),
            "an untouched layout opens on Develop"
        );

        layout.bring_forward(Pane::DodgeBurn);
        assert_eq!(active(&layout), Some(Pane::DodgeBurn));
        layout.bring_forward(Pane::Develop);
        assert_eq!(active(&layout), Some(Pane::Develop), "and back again");
    }

    #[test]
    fn bringing_a_panel_forward_does_not_undo_tab_or_a_pop_out() {
        // Two things it must NOT do. A panel someone put on another screen is
        // already reachable, and `tab` hiding the panels is a statement about the
        // whole frame that one key press should not overrule.
        let mut floated = Layout::default();
        floated.float(Pane::DodgeBurn);
        floated.bring_forward(Pane::DodgeBurn);
        assert!(
            floated.is_out(Pane::DodgeBurn),
            "it was dragged back off its own screen"
        );

        let mut hidden = Layout::default();
        hidden.toggle_panels();
        hidden.bring_forward(Pane::DodgeBurn);
        assert!(hidden.panels_hidden(), "the panels came back on their own");
    }

    #[test]
    fn the_image_pane_cannot_be_closed() {
        // Everything else can be brought back with `tab`. Closing the image would
        // close the thing the program is for, and nothing in the frame could reopen
        // it — the tab strip is about files and the footer is a readout.
        assert!(!closable(Pane::Image));
        assert!(closable(Pane::Develop));
        assert!(closable(Pane::Info));

        // And the close is refused even if it is somehow asked for.
        let mut layout = Layout::default();
        layout.close(Pane::Image);
        layout.settle_visibility();
        let image = tile_of(&layout.tree, Pane::Image);
        assert!(layout.tree.is_visible(image), "the image pane went away");
    }

    #[test]
    fn hiding_the_panels_is_one_gesture_and_reversible() {
        // `tab` hides both, and there is no state in which a panel is gone with no
        // way back — the key that hid them is the key that returns them, and
        // floating is separate from hidden so a popped-out panel is not lost by
        // pressing it.
        let mut layout = Layout::default();
        assert_eq!(
            layout.shown.len(),
            Pane::PANELS.len(),
            "panels start in the frame"
        );
        layout.float(Pane::Develop);
        layout.toggle_panels();
        assert!(layout.panels_hidden(), "the picture is on its own");
        assert!(
            layout.is_out(Pane::Develop),
            "hiding must not silently dock a floating panel"
        );
        layout.toggle_panels();
        assert_eq!(
            layout.shown.len(),
            Pane::PANELS.len(),
            "the same key brings them back"
        );
        assert!(
            layout.is_out(Pane::Develop),
            "and brings the floating one back floating"
        );
    }

    #[test]
    fn a_tucked_column_hides_and_comes_back_at_its_width() {
        let mut layout = Layout::default();
        layout.before_ui();
        let [Some((left, _)), Some((right, _))] = layout.edge_columns() else {
            panic!("the default layout has a column on each side");
        };
        layout.tuck(left, 312.0);
        layout.before_ui();
        assert!(!layout.tree.is_visible(left), "tucked means off screen");
        assert!(layout.tree.is_visible(right), "and only that side");
        assert_eq!(
            layout.edge_columns()[0].map(|(id, _)| id),
            Some(left),
            "a tucked column is still found, or its strip could not bring it back"
        );
        layout.untuck(left);
        layout.before_ui();
        assert!(layout.tree.is_visible(left));
        assert_eq!(
            layout.measured[&left].x, 312.0,
            "back at the width it left at"
        );
    }

    #[test]
    fn tab_and_a_mode_both_bring_a_tucked_column_back() {
        let mut layout = Layout::default();
        layout.before_ui();
        let [Some((left, _)), _] = layout.edge_columns() else {
            panic!("the default layout has a left column");
        };
        layout.tuck(left, 300.0);
        layout.toggle_panels();
        layout.toggle_panels();
        layout.before_ui();
        assert!(layout.tree.is_visible(left), "`tab` restores every panel");

        layout.tuck(left, 300.0);
        layout.before_ui();
        layout.bring_forward(Pane::DodgeBurn);
        layout.before_ui();
        assert!(
            layout.tree.is_visible(left),
            "a mode opening onto a tucked panel brings it out"
        );
    }

    /// Hold the primary button down at `from` and drag it through `path`, a frame per
    /// point, then let go at the last one.
    fn drag_seam(ctx: &egui::Context, layout: &mut Layout, from: egui::Pos2, path: &[f32]) {
        frame(
            ctx,
            layout,
            vec![egui::Event::PointerMoved(from), press(from, true)],
        );
        let mut at = from;
        for x in path {
            at = egui::pos2(*x, from.y);
            frame(ctx, layout, vec![egui::Event::PointerMoved(at)]);
        }
        frame(ctx, layout, vec![press(at, false)]);
        frame(ctx, layout, vec![]);
        frame(ctx, layout, vec![]);
    }

    fn width_of(layout: &Layout, id: TileId) -> f32 {
        layout.tree.tiles.rect(id).map_or(0.0, |r| r.width())
    }

    #[test]
    fn squeezing_a_column_off_the_edge_leaves_the_other_side_alone() {
        // **The maintainer's report**: squeeze Develop off the left and Info grew to fill
        // the window. `egui_tiles` keys each seam by its index among the *visible*
        // children, so the moment Develop tucked, seam 0 was the image | Info seam —
        // and the drag still held on seam 0 carried on there, squeezing the image
        // against its own stop and handing everything to Info.
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        let [Some((left, rect)), Some((right, _))] = layout.edge_columns() else {
            panic!("the default layout has a column on each side");
        };
        let info_before = width_of(&layout, right);
        let seam = egui::pos2(rect.right() + GAP / 2.0, rect.center().y);

        // Well past the stop, then on to the window's edge, all in one held drag.
        let path: Vec<f32> = (1..=12).map(|i| seam.x - 40.0 * i as f32).collect();
        let path: Vec<f32> = path.into_iter().map(|x| x.max(2.0)).collect();
        drag_seam(&ctx, &mut layout, seam, &path);

        assert!(
            !layout.tree.is_visible(left),
            "the column should have tucked"
        );
        assert!(
            (width_of(&layout, right) - info_before).abs() < 2.0,
            "Info went from {info_before} to {} — the drag carried on into its seam",
            width_of(&layout, right)
        );
        let image = tile_of(&layout.tree, Pane::Image);
        assert!(
            width_of(&layout, image) > WINDOW.x - info_before - 40.0,
            "the image did not take the tucked column's room: {}",
            width_of(&layout, image)
        );

        // And it comes back at the width it had before the drag began.
        layout.untuck(left);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        assert!(
            (width_of(&layout, left) - rect.width()).abs() < 2.0,
            "came back at {} rather than {}",
            width_of(&layout, left),
            rect.width()
        );
    }

    #[test]
    fn a_column_squeezed_before_it_tucks_comes_back_at_its_default_width() {
        // Squeeze to the stop, let go, then drag again to tuck it: the width at the
        // second press is the stop, which is not a width anybody chose. Bringing the
        // panel back there is what "super narrow" was — and the same for a squeeze
        // that stopped short of the stop, which is what "narrower" was the next time.
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        let [Some((left, rect)), _] = layout.edge_columns() else {
            panic!("a left column");
        };
        let seam = egui::pos2(rect.right() + GAP / 2.0, rect.center().y);
        let to_stop = seam.x - (rect.width() - MIN_PANE) - 20.0;
        drag_seam(&ctx, &mut layout, seam, &[seam.x - 40.0, to_stop]);
        assert!(
            layout.tree.is_visible(left),
            "meeting the stop is not tucking"
        );
        let squeezed = width_of(&layout, left);
        assert!(squeezed < MIN_PANE + 2.0, "not at the stop: {squeezed}");

        let rect = layout.tree.tiles.rect(left).unwrap();
        let seam = egui::pos2(rect.right() + GAP / 2.0, rect.center().y);
        drag_seam(&ctx, &mut layout, seam, &[seam.x - 40.0, seam.x - 120.0]);
        assert!(
            !layout.tree.is_visible(left),
            "the second drag should tuck it"
        );

        layout.untuck(left);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        assert!(
            (width_of(&layout, left) - DEVELOP_W).abs() < 2.0,
            "came back at {}",
            width_of(&layout, left)
        );
    }

    #[test]
    fn a_column_reopens_no_narrower_than_its_default_and_no_narrower_than_it_was() {
        // Narrowed to 250 and then collapsed: back at the default. Widened to 470 and
        // then collapsed: back at 470. Reopened the way a person does, by clicking the
        // strip at the edge.
        for (moved, expected) in [(-100.0, DEVELOP_W), (120.0, DEVELOP_W + 120.0)] {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut layout = Layout::default();
            frame(&ctx, &mut layout, vec![]);
            frame(&ctx, &mut layout, vec![]);
            let [Some((left, rect)), _] = layout.edge_columns() else {
                panic!("a left column");
            };
            let seam = egui::pos2(rect.right() + GAP / 2.0, rect.center().y);
            drag_seam(
                &ctx,
                &mut layout,
                seam,
                &[seam.x + moved / 2.0, seam.x + moved],
            );
            let rect = layout.tree.tiles.rect(left).unwrap();
            let seam = egui::pos2(rect.right() + GAP / 2.0, rect.center().y);
            let path: Vec<f32> = (1..=12)
                .map(|i| (seam.x - 60.0 * i as f32).max(2.0))
                .collect();
            drag_seam(&ctx, &mut layout, seam, &path);
            assert!(!layout.tree.is_visible(left), "it should have tucked");

            let image = layout
                .tree
                .tiles
                .rect(tile_of(&layout.tree, Pane::Image))
                .unwrap();
            let strip = egui::pos2(image.left() + TUCK_STRIP / 2.0, image.center().y);
            for _ in 0..30 {
                frame(&ctx, &mut layout, vec![]);
            }
            frame(
                &ctx,
                &mut layout,
                vec![egui::Event::PointerMoved(strip), press(strip, true)],
            );
            frame(&ctx, &mut layout, vec![press(strip, false)]);
            frame(&ctx, &mut layout, vec![]);
            frame(&ctx, &mut layout, vec![]);
            assert!(
                layout.tree.is_visible(left),
                "the strip should bring it back"
            );
            assert!(
                (width_of(&layout, left) - expected).abs() < 2.0,
                "moved {moved}pt then tucked: came back at {} rather than {expected}",
                width_of(&layout, left)
            );
        }
    }

    #[test]
    fn a_closed_panel_comes_back_with_tab() {
        // What makes the close button safe to offer: closing hides a panel and keeps
        // its place in the tree, and `tab` restores *all* of them rather than
        // whichever were showing last. Without that, closing Info would be a control
        // with no inverse.
        let mut layout = Layout::default();
        layout.close(Pane::Info);
        layout.settle_visibility();
        let info = tile_of(&layout.tree, Pane::Info);
        assert!(!layout.tree.is_visible(info));
        assert!(
            layout.tree.tiles.find_pane(&Pane::Info).is_some(),
            "and it kept its place"
        );

        layout.toggle_panels(); // everything off
        layout.toggle_panels(); // everything on
        layout.settle_visibility();
        assert!(layout.tree.is_visible(tile_of(&layout.tree, Pane::Info)));
    }

    #[test]
    fn closing_a_floating_panel_docks_it_rather_than_losing_it() {
        // Closing the OS window is the only obvious way to get rid of it, and if that
        // hid the panel instead of docking it, the control would destroy itself — the
        // window is gone and the panel is nowhere.
        let mut layout = Layout::default();
        layout.float(Pane::Develop);
        layout.float(Pane::Info);
        layout.dock(Pane::Develop);
        assert!(
            !layout.is_out(Pane::Develop),
            "closing the window docks the panel"
        );
        assert!(
            layout.shown.contains(&Pane::Develop),
            "and does not hide it"
        );
        assert!(layout.is_out(Pane::Info), "the other panel is untouched");
    }

    #[test]
    fn hiding_every_panel_takes_their_container_with_them() {
        // A `Tabs` container holding only hidden panes still draws its bar and still
        // claims its slot. Stack Develop and Info, press `tab`, and without this the
        // picture would gain an empty strip instead of the space.
        let mut layout = stacked();
        layout.toggle_panels();
        layout.settle_visibility();
        for (id, tile) in layout.tree.tiles.iter() {
            let holds_image = match tile {
                Tile::Pane(p) => *p == Pane::Image,
                Tile::Container(_) => {
                    let image = tile_of(&layout.tree, Pane::Image);
                    descends_from(&layout.tree, *id, image)
                }
            };
            assert_eq!(
                layout.tree.is_visible(*id),
                holds_image,
                "{tile:?} is visible without anything visible in it"
            );
        }
    }

    fn descends_from(tree: &egui_tiles::Tree<Pane>, ancestor: TileId, of: TileId) -> bool {
        let mut walk = Some(of);
        while let Some(id) = walk {
            if id == ancestor {
                return true;
            }
            walk = tree.tiles.parent_of(id);
        }
        false
    }

    #[test]
    fn stacking_two_panes_makes_tabs_and_unstacking_takes_them_away() {
        // The simplification options, stated as behaviour. Wrong settings here make
        // panels snap back after being moved, which reads as a broken drag rather
        // than as a policy — and `prune_single_child_tabs` with
        // `all_panes_must_have_tabs` off is exactly what decides it.
        let mut layout = stacked();
        let (develop, info) = (
            tile_of(&layout.tree, Pane::Develop),
            tile_of(&layout.tree, Pane::Info),
        );

        // Two panes stacked stay stacked: nothing here prunes a tab bar that is
        // naming more than itself.
        layout.tree.simplify(&SIMPLIFY);
        let parent = layout.tree.tiles.parent_of(develop).expect("a parent");
        assert!(
            matches!(
                layout.tree.tiles.get(parent),
                Some(Tile::Container(Container::Tabs(_)))
            ),
            "a stack of two panes was taken apart"
        );
        assert_eq!(layout.tree.tiles.parent_of(info), Some(parent));

        // Drag Develop back out: the leftover single-child tab bar collapses, so Info
        // does not keep a tab bar naming only itself.
        let root = layout.tree.root.expect("a root");
        layout.tree.move_tile_to_container(develop, root, 0, false);
        layout.tree.simplify(&SIMPLIFY);
        let info_parent = layout.tree.tiles.parent_of(info).expect("a parent");
        assert!(
            !matches!(
                layout.tree.tiles.get(info_parent),
                Some(Tile::Container(Container::Tabs(_)))
            ),
            "a lone pane kept a tab bar of its own"
        );
        assert!(layout.heal(), "the tree is still one the app can run");
    }

    /// `eframe::Storage` over a map, so persistence can be exercised without a
    /// state directory or a window.
    #[derive(Default)]
    struct Memory(std::collections::HashMap<String, String>);

    impl eframe::Storage for Memory {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn remove_string(&mut self, key: &str) {
            self.0.remove(key);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn a_moved_panel_is_still_moved_after_a_restart() {
        // Layout is app memory: not settings, and not the sidecar — where you put a
        // panel is not a property of the image. This is the whole claim, end to end,
        // through the same RON helpers eframe uses.
        let mut saved = stacked();
        saved.close(Pane::Info);

        let mut storage = Memory::default();
        saved.save(&mut storage);
        let restored = Layout::restore(Some(&storage));

        assert!(
            !restored.shown.contains(&Pane::Info),
            "a closed panel came back open"
        );
        let develop = tile_of(&restored.tree, Pane::Develop);
        let parent = restored.tree.tiles.parent_of(develop).expect("a parent");
        assert!(
            matches!(
                restored.tree.tiles.get(parent),
                Some(Tile::Container(Container::Tabs(_)))
            ),
            "the stack did not survive the round trip"
        );
    }

    #[test]
    fn nothing_stored_is_a_first_run_rather_than_a_failure() {
        let empty = Memory::default();
        let layout = Layout::restore(Some(&empty));
        assert_eq!(layout.shown, Pane::PANELS.to_vec());
        // Panes, the row that holds them, and the tab strip Develop and the brush
        // share. Counted rather than named so a structural change to the default
        // has to be acknowledged here.
        // Panes, the row, and **two** tab strips: Develop/DodgeBurn on the left,
        // Info/Snapshots/History on the right.
        assert_eq!(Pane::ALL.len() + 3, layout.tree.tiles.tiles().count());
    }

    #[test]
    fn a_stored_layout_that_will_not_parse_is_a_first_run_too() {
        // A layout is a convenience and must never be the reason the app will not
        // start — the same rule `Loaded<T>` applies to everything read from disk.
        // The realistic way here is an upgrade that changed the wire shape.
        let mut storage = Memory::default();
        eframe::Storage::set_string(&mut storage, memory_keys::TREE, "not a tree".into());
        let layout = Layout::restore(Some(&storage));
        assert!(layout.tree.tiles.find_pane(&Pane::Image).is_some());
    }

    #[test]
    fn a_tree_the_app_cannot_run_falls_back_to_the_default() {
        // The three ways a stored tree can come back wrong. Each is a layout lost
        // once rather than an app that starts without a viewport.
        let mut missing = Layout::default();
        let image = tile_of(&missing.tree, Pane::Image);
        missing.tree.remove_recursively(image);
        assert!(
            !missing.heal(),
            "a tree with no image pane must be rejected"
        );

        let mut doubled = Layout::default();
        let root = doubled.tree.root.expect("a root");
        let extra = doubled.tree.tiles.insert_pane(Pane::Develop);
        doubled.tree.move_tile_to_container(extra, root, 0, false);
        assert!(
            !doubled.heal(),
            "two panes with one identity must be rejected"
        );

        let mut empty = Layout::default();
        empty.tree = egui_tiles::Tree::empty("nothing");
        assert!(!empty.heal(), "an empty tree must be rejected");
    }

    #[test]
    fn a_sliver_share_heals_on_the_next_launch() {
        // The 7b lesson generalised: a bad stored number is inherited forever unless
        // something repairs it. The develop panel once grew until it filled the window
        // and came back that way every launch. A share of one part in a thousand is
        // the same failure with the sign flipped — a pane too thin to see, with a
        // resize handle too thin to grab.
        let mut layout = Layout::default();
        let develop = tile_of(&layout.tree, Pane::Develop);
        let root = layout.tree.root.expect("a root");
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get_mut(root) else {
            panic!("the default root is a row");
        };
        row.shares.set_share(develop, share(IMAGE_W) / 2000.0);

        assert!(layout.heal());
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("still a row");
        };
        let widest = row
            .children
            .iter()
            .map(|c| row.shares[*c])
            .fold(0.0f32, f32::max);
        let narrowest = row
            .children
            .iter()
            .map(|c| row.shares[*c])
            .fold(f32::INFINITY, f32::min);
        assert!(
            widest <= narrowest * SHARE_SPREAD,
            "a sliver survived healing: {narrowest} beside {widest}"
        );
    }

    #[test]
    fn a_panel_dragged_to_the_stop_on_an_ultrawide_is_left_alone() {
        // The bound this exists to get right. `min_size` stops a drag at 180pt, and
        // 180 beside a 5K image is a spread of about 26:1 — a layout somebody chose,
        // not a stored number that has gone wrong. An earlier "below 5% of the
        // container" rule would have reset it on every launch, which is the same
        // class of bug as not healing at all: the app quietly refusing the layout you
        // asked for.
        let mut layout = Layout::default();
        let (develop, info, image) = (
            tile_of(&layout.tree, Pane::Develop),
            tile_of(&layout.tree, Pane::Info),
            tile_of(&layout.tree, Pane::Image),
        );
        let root = layout.tree.root.expect("a root");
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get_mut(root) else {
            panic!("a row");
        };
        // A 5120pt window with both panels squeezed to the stop: a ~26:1 spread.
        const WIDE: f32 = 5120.0;
        row.shares.set_share(develop, 3.0 * MIN_PANE / WIDE);
        row.shares.set_share(info, 3.0 * MIN_PANE / WIDE);
        row.shares
            .set_share(image, 3.0 * (WIDE - 2.0 * MIN_PANE) / WIDE);

        assert!(layout.heal());
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("a row");
        };
        // Normalising rescales, so compare the ratio rather than the number.
        let total: f32 = row.children.iter().map(|c| row.shares[*c]).sum();
        assert!(
            (row.shares[develop] / total - MIN_PANE / WIDE).abs() < 0.002,
            "a deliberate narrow panel was reset: {} of the row",
            row.shares[develop] / total
        );
    }

    #[test]
    fn a_pane_dropped_into_a_row_does_not_land_one_point_wide() {
        // **How Info was "lost forever" in testing.** `Tiles::insert_at` puts a
        // dropped tile in a container without giving it a share, and `Shares` answers
        // 1.0 for a tile it has no entry for. The first version used point widths as
        // shares — 320, 780, 300, which reads beautifully — so that implicit 1.0 was
        // one part in eleven hundred: a pane 1.3 points wide, next to a resize handle
        // too thin to find. The panel was not hidden and not closed. It was there.
        //
        // Normalised shares make the same default mean "an equal share", which is
        // what `egui_tiles` intends by it.
        let mut layout = Layout::default();
        let root = layout.tree.root.expect("a root");
        let orphan = layout.tree.tiles.insert_pane(Pane::Info);
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get_mut(root) else {
            panic!("a row");
        };
        row.children.push(orphan); // exactly what a drop leaves behind: no share
        layout.normalise_shares();

        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("a row");
        };
        let total: f32 = row.children.iter().map(|c| row.shares[*c]).sum();
        let got = row.shares[orphan] / total * LAYOUT_W;
        assert!(
            got > MIN_PANE,
            "a dropped pane landed {got:.1}pt wide, under the {MIN_PANE}pt minimum"
        );
    }

    #[test]
    fn a_share_that_is_not_a_number_heals_too() {
        let mut layout = Layout::default();
        let image = tile_of(&layout.tree, Pane::Image);
        let root = layout.tree.root.expect("a root");
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get_mut(root) else {
            panic!("a row");
        };
        row.shares.set_share(image, f32::NAN);
        assert!(layout.heal());
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("a row");
        };
        assert!(row.children.iter().all(|c| row.shares[*c].is_finite()));
    }

    #[test]
    fn startup_reset_and_restore_keep_pixel_widths_on_wide_windows() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        for width in [1800.0, 2400.0] {
            // A fresh default is also what the Settings reset button installs.
            layout = Layout::default();
            frame_sized(&ctx, &mut layout, vec![], egui::vec2(width, 900.0));
            let root = layout.tree.root.unwrap();
            let Tile::Container(Container::Linear(row)) = layout.tree.tiles.get(root).unwrap()
            else {
                panic!();
            };
            assert!(
                (layout.tree.tiles.rect(row.children[0]).unwrap().width() - DEVELOP_W).abs() < 1.0
            );
            assert!(
                (layout.tree.tiles.rect(row.children[2]).unwrap().width() - INFO_W).abs() < 1.0
            );
        }
        let mut storage = Memory::default();
        layout.save(&mut storage);
        let mut restored = Layout::restore(Some(&storage));
        frame_sized(&ctx, &mut restored, vec![], egui::vec2(2000.0, 900.0));
        let root = restored.tree.root.unwrap();
        let Tile::Container(Container::Linear(row)) = restored.tree.tiles.get(root).unwrap() else {
            panic!();
        };
        assert!(
            (restored.tree.tiles.rect(row.children[0]).unwrap().width() - DEVELOP_W).abs() < 1.0
        );
        assert!((restored.tree.tiles.rect(row.children[2]).unwrap().width() - INFO_W).abs() < 1.0);
    }

    /// A double-click, as egui sees one: two clicks at `at`, well clear in time of any
    /// click before them — a third inside the window would make it a triple.
    fn double_click(ctx: &egui::Context, layout: &mut Layout, at: egui::Pos2, size: egui::Vec2) {
        for _ in 0..60 {
            frame_sized(ctx, layout, vec![], size);
        }
        frame_sized(
            ctx,
            layout,
            vec![egui::Event::PointerMoved(at), press(at, true)],
            size,
        );
        frame_sized(ctx, layout, vec![press(at, false)], size);
        frame_sized(ctx, layout, vec![press(at, true)], size);
        frame_sized(ctx, layout, vec![press(at, false)], size);
        frame_sized(ctx, layout, vec![], size);
        frame_sized(ctx, layout, vec![], size);
    }

    #[test]
    fn double_click_edges_restore_default_panel_widths() {
        // The maintainer: double-clicking the edge sent Develop to half the window.
        // egui's hit test reaches past the seam's own few points, so a double-click
        // just inside the panel — on its scroll bar — still hit the seam; upstream
        // evened out the split, and the reset, which re-derived the seam from the
        // pointer afterwards, missed. Hence the offsets: on the seam, and 6pt either
        // side of it.
        let size = egui::vec2(2200.0, 900.0);
        for offset in [0.0, -6.0, 6.0] {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut layout = Layout::default();
            frame_sized(&ctx, &mut layout, vec![], size);
            frame_sized(&ctx, &mut layout, vec![], size);
            let root = layout.tree.root.unwrap();
            let children = match layout.tree.tiles.get(root).unwrap() {
                Tile::Container(Container::Linear(row)) => row.children.clone(),
                _ => panic!("expected horizontal root"),
            };
            // Both panels well off their defaults first, or there is nothing to reset.
            {
                let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get_mut(root)
                else {
                    panic!()
                };
                row.shares.set_share(children[0], 250.0);
                row.shares.set_share(children[1], 1500.0);
                row.shares.set_share(children[2], 450.0);
            }
            frame_sized(&ctx, &mut layout, vec![], size);
            frame_sized(&ctx, &mut layout, vec![], size);
            for (panel, desired, inward) in
                [(children[0], DEVELOP_W, -1.0), (children[2], INFO_W, 1.0)]
            {
                let r = layout.tree.tiles.rect(panel).unwrap();
                let edge = if inward < 0.0 {
                    r.right() + GAP / 2.0
                } else {
                    r.left() - GAP / 2.0
                };
                // `inward` points into the panel, so a negative offset is toward the
                // image and a positive one onto the panel's own contents.
                let at = egui::pos2(edge - inward * offset, r.center().y);
                double_click(&ctx, &mut layout, at, size);
                let width = layout.tree.tiles.rect(panel).unwrap().width();
                assert!(
                    (width - desired).abs() < 2.0,
                    "{width} instead of {desired}, double-clicked {offset}pt from the seam"
                );
            }
        }
    }

    const WINDOW: egui::Vec2 = egui::vec2(1400.0, 900.0);

    /// One frame of the tree in a fixed window, with the given input events.
    fn frame(ctx: &egui::Context, layout: &mut Layout, events: Vec<egui::Event>) {
        frame_sized(ctx, layout, events, WINDOW);
    }

    fn frame_sized(
        ctx: &egui::Context,
        layout: &mut Layout,
        events: Vec<egui::Event>,
        size: egui::Vec2,
    ) {
        // The same sequence `App::tile_tree` runs. Without it these tests exercise a
        // frame the app never has.
        layout.before_ui();
        let mut bare = Bare::new(&layout.tree);
        bare.tabbed = layout.tabbed();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), size)),
            events,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                layout.restore_pixel_widths(ui.available_width());
                let area = ui.available_rect_before_wrap();
                layout.tree.ui(&mut bare, ui);
                if ui.input(|i| layout.tuck_from_drag(i, bare.resizing)) {
                    ui.ctx().stop_dragging();
                }
                layout.tuck_strips(ui, area);
            });
        });
        layout.restructured |= bare.dropped;
    }

    fn press(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Run `n` frames of the tree in a fixed window, and report the width the develop
    /// pane was given each time.
    fn pane_widths(n: usize) -> Vec<f32> {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        (0..n)
            .map(|_| {
                frame(&ctx, &mut layout, vec![]);
                let develop = tile_of(&layout.tree, Pane::Develop);
                layout.tree.tiles.rect(develop).map_or(0.0, |r| r.width())
            })
            .collect()
    }

    #[test]
    fn the_panel_handle_senses_under_the_tile_id() {
        // **The bug that made docking's headline gesture do nothing.**
        //
        // `Ui::set_dragged_id` — what `egui_tiles` calls when `pane_ui` answers
        // `DragStarted` — writes into `interact_widgets`, and egui recomputes that
        // from its own hit-testing every pass. So a title that is an ordinary
        // `Label::sense(click_and_drag)` has egui reporting the *label's* id as
        // dragged from frame two, `is_being_dragged(tile_id)` goes false, and the
        // drag dies one frame after it started: no preview, no drop, nothing at all
        // to see. Dragging a *tab* worked the whole time, because `egui_tiles` senses
        // its tabs under exactly this id.
        //
        // Asserted through `read_response`, i.e. against the widget egui actually
        // registered, so reverting to `Label::sense` fails here rather than in use.
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        let tree_id = layout.tree.id();
        for pane in Pane::PANELS {
            let id = tile_of(&layout.tree, pane).egui_id(tree_id);
            let response = ctx.read_response(id).unwrap_or_else(|| {
                panic!("{pane:?}'s header registered no widget under its tile id")
            });
            assert!(
                response.sense.senses_drag(),
                "{pane:?}'s handle does not sense drags, so the tree can never move it"
            );
        }
    }

    /// Grab a pane by its title and drop it at `target`, in screen coordinates.
    fn drag_pane(ctx: &egui::Context, layout: &mut Layout, pane: Pane, target: egui::Pos2) {
        // Settle first. `Context::read_response` reports the pass *before* last, so a
        // handle read straight after a layout change is the rect the pane used to
        // have — and a press aimed there quietly lands in whatever now occupies it,
        // which looks exactly like a drag that refused to start.
        frame(ctx, layout, vec![]);
        frame(ctx, layout, vec![]);
        let handle = ctx
            .read_response(tile_of(&layout.tree, pane).egui_id(layout.tree.id()))
            .expect("a handle")
            .rect
            .center();
        frame(
            ctx,
            layout,
            vec![egui::Event::PointerMoved(handle), press(handle, true)],
        );
        // Several moves: egui needs the pointer to travel before a click-and-drag
        // widget counts as dragged, and egui_tiles smooths the preview rect over a
        // few frames before it will accept the drop.
        for _ in 0..6 {
            frame(ctx, layout, vec![egui::Event::PointerMoved(target)]);
        }
        frame(ctx, layout, vec![press(target, false)]);
        frame(ctx, layout, vec![]);
    }

    /// Two settled frames, so every pane has a rect and a registered handle.
    fn settled() -> (egui::Context, Layout) {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        // **With the snapshot list closed**, which is what makes Info the lone pane in
        // the right-hand column. Every test built on this helper measures a *column*
        // widening or narrowing, and a pane sharing a tab strip has no rect of its own
        // while it is the inactive tab — so measuring one would be measuring nothing.
        // They are closed rather than omitted because `heal` requires the whole pane
        // set; their own behaviour is tested where they are the subject.
        layout.close(Pane::Snapshots);
        layout.close(Pane::History);
        // **Toning, for the same reason on the other side.** It shares the left column's
        // tab strip with Develop and Dodge & Burn, so once one of those is dragged away
        // the strip still holds two and the inactive one has no rect — which is the very
        // thing the paragraph above says makes a measurement meaningless. Every test
        // built on this helper measures a column, and this keeps the left one measurable.
        layout.close(Pane::Toning);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        (ctx, layout)
    }

    #[test]
    fn dragging_a_panel_across_the_image_moves_it_there() {
        // The gesture docking exists for, driven end to end: grab DEVELOP by its
        // title and drop it on the right-hand half of the image. It has to survive
        // more than one frame to get anywhere, which is what the bug above prevented.
        let (ctx, mut layout) = settled();
        let develop = tile_of(&layout.tree, Pane::Develop);
        let before = layout.tree.tiles.rect(develop).expect("laid out").width();
        let image_rect = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Image))
            .expect("laid out");
        // The outer right-edge drop zone: this is the gesture that used to create an
        // oversized equal split rather than another normal side panel.
        let target = egui::pos2(WINDOW.x - 4.0, image_rect.center().y);
        drag_pane(&ctx, &mut layout, Pane::Develop, target);

        let root = layout.tree.root.expect("a root");
        let Some(Tile::Container(Container::Linear(row))) = layout.tree.tiles.get(root) else {
            panic!("still a row, got {:?}", layout.tree.tiles.get(root));
        };
        let order: Vec<_> = row
            .children
            .iter()
            .filter_map(|c| pane_of(&layout.tree, *c))
            .collect();
        let image_at = order
            .iter()
            .position(|p| *p == Pane::Image)
            .expect("the image is in the row");
        let develop_at = order
            .iter()
            .position(|p| *p == Pane::Develop)
            .expect("develop is in the row");
        assert!(
            develop_at > image_at,
            "develop did not cross the image — the drag never landed: {order:?}"
        );
        // Moving a panel to the other side changes its position, not its measure.
        // In particular it must not take an equal share of the image-side row and
        // arrive much wider merely because it was dropped on the right.
        let width = layout.tree.tiles.rect(develop).expect("laid out").width();
        assert!(
            (width - before).abs() < 2.0,
            "develop changed from {before:.1}pt to {width:.1}pt when moved right"
        );
    }

    #[test]
    fn dropping_a_panel_below_another_leaves_both_usable() {
        // **The drag that actually lost Info.** Moving a pane within the row it is
        // already in keeps its share, because shares are keyed by `TileId` — which is
        // why the test above passed even while the bug was live. Dropping one pane onto
        // the lower half of another is different: it wraps them in a *new* `Vertical`
        // container, and neither the new container nor its children have a share
        // entry. Those all default to 1.0, and beside point-scale siblings that was a
        // 1.3-point pane which looked exactly like a panel that had been thrown away.
        let (ctx, mut layout) = settled();
        let info_rect = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Info))
            .expect("laid out");
        // The centre of Info's bottom half: nearer that zone's centre than the left or
        // right halves', so the drop wraps Info in a vertical pair.
        let target = egui::pos2(
            info_rect.center().x,
            info_rect.top() + info_rect.height() * 0.75,
        );
        drag_pane(&ctx, &mut layout, Pane::Develop, target);

        let develop = tile_of(&layout.tree, Pane::Develop);
        let info = tile_of(&layout.tree, Pane::Info);
        let parent = layout.tree.tiles.parent_of(develop).expect("a parent");
        assert_eq!(
            layout.tree.tiles.parent_of(info),
            Some(parent),
            "develop did not land beside info"
        );
        for (pane, tile) in [(Pane::Develop, develop), (Pane::Info, info)] {
            let rect = layout.tree.tiles.rect(tile).expect("laid out");
            assert!(
                rect.width() > MIN_PANE && rect.height() > 40.0,
                "{pane:?} came out {:.1} x {:.1} — a sliver, not a panel",
                rect.width(),
                rect.height()
            );
        }
    }

    #[test]
    fn docking_a_panel_onto_another_leaves_that_one_the_width_it_was() {
        // **the maintainer's complaint, measured.** Dropping Info onto Develop took the left
        // column from 316pt to 402pt: Info's share left the row, so the total fell from
        // 3.0 to 2.357 and both survivors grew by that ratio. Nothing was broken — that
        // is what a linear container does — but it means the panel you aimed at moves
        // under you, which is not what a deliberate gesture should feel like.
        //
        // Panels keep their width and the image absorbs the change.
        let (ctx, mut layout) = settled();
        let before = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Develop))
            .expect("laid out")
            .width();

        // Onto the middle of Develop, which is where the tabbing zone's centre is — see
        // the note in `docs/ui-queue.md` about drop zones being chosen by centre.
        let target = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Develop))
            .unwrap()
            .center();
        drag_pane(&ctx, &mut layout, Pane::Info, target);
        // Two frames: the restore lands at the top of the frame after the drop.
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        let develop = tile_of(&layout.tree, Pane::Develop);
        let parent = layout.tree.tiles.parent_of(develop).expect("a parent");
        let after = layout.tree.tiles.rect(parent).expect("laid out").width();
        assert!(
            (after - before).abs() < 2.0,
            "the column went from {before:.1}pt to {after:.1}pt — the panel moved under the drop"
        );
    }

    #[test]
    fn docking_a_floating_panel_brings_it_back_the_width_it_was() {
        // **the maintainer: it docks, but arrives with no width, pushed against the window edge
        // where you cannot see or grab it.** Dragging the window's side revealed it, which
        // is the tell — the panel was there, at a fraction of a point.
        //
        // Two causes, both mine. `keep_panel_sizes` read the tree's visibility flags,
        // which `settle_visibility` had not yet updated this frame, so on the docking
        // frame the panel was still marked invisible and was left out of the solve — and
        // being left out, it kept a share on the *normalised* scale while its siblings
        // were rewritten on the *point* scale. Roughly 0.7 next to 1083.
        //
        // **Info rather than Develop**, since Dodge & Burn: Develop shares a tab
        // strip with the brush, so floating it leaves the strip standing and the
        // column keeps its width — correct, and not what this is measuring. Info is
        // a column of its own and is the same test it always was.
        let (ctx, mut layout) = settled();
        let before = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Info))
            .expect("laid out")
            .width();
        assert!(
            before > MIN_PANE,
            "the panel started too narrow to prove anything"
        );

        layout.float(Pane::Info);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);
        assert!(
            !layout.tree.is_visible(tile_of(&layout.tree, Pane::Info)),
            "a floating panel must not also be drawn in the tree"
        );

        layout.dock(Pane::Info);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        let info = tile_of(&layout.tree, Pane::Info);
        assert!(
            layout.tree.is_visible(info),
            "docking did not put it back in the tree"
        );
        let after = layout.tree.tiles.rect(info).expect("laid out").width();
        assert!(
            (after - before).abs() < 2.0,
            "it came back {after:.1}pt wide, having been {before:.1}pt"
        );
    }

    #[test]
    fn a_hidden_panel_never_leaves_its_container_on_two_scales() {
        // The invariant behind both share bugs this codebase has had, stated directly
        // rather than through a symptom: **every share in a container is on the same
        // scale.** `normalise_shares` exists to keep it, and it is what makes the implicit
        // 1.0 that `Shares` hands an unknown tile mean "an equal share".
        //
        // `keep_panel_sizes` rewrites sizes on the point scale, so it has to rewrite *all*
        // of a row's children — a hidden one left on the normalised scale is 0.7 beside
        // 1083, and normalising preserves the ratio faithfully. The docking path happens
        // to re-solve and recover, which is why the two tests above pass without this; the
        // invariant holding only while nothing is floating is one somebody will trip over.
        let (ctx, mut layout) = settled();
        layout.float(Pane::Info);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        for (id, tile) in layout.tree.tiles.iter() {
            let Tile::Container(Container::Linear(row)) = tile else {
                continue;
            };
            if row.children.len() < 2 {
                continue;
            }
            let shares: Vec<f32> = row.children.iter().map(|c| row.shares[*c]).collect();
            let widest = shares.iter().copied().fold(0.0f32, f32::max);
            let narrowest = shares.iter().copied().fold(f32::INFINITY, f32::min);
            assert!(
                widest <= narrowest * SHARE_SPREAD,
                "{id:?} holds two scales: {narrowest} beside {widest} ({shares:?})"
            );
        }
    }

    #[test]
    fn popping_a_panel_out_gives_its_space_to_the_image() {
        // The other direction, and the reason the same solver serves both: the space a
        // popped-out panel leaves belongs to the picture, not to the panel next to it.
        //
        // Info pops out and Develop stays; see the note in the test above for why
        // the roles swapped when the brush joined Develop's tab strip.
        let (ctx, mut layout) = settled();
        let (image, develop) = (
            tile_of(&layout.tree, Pane::Image),
            tile_of(&layout.tree, Pane::Develop),
        );
        let was = |l: &Layout, t| l.tree.tiles.rect(t).expect("laid out").width();
        let (image_before, develop_before) = (was(&layout, image), was(&layout, develop));

        layout.float(Pane::Info);
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        assert!(
            (was(&layout, develop) - develop_before).abs() < 2.0,
            "Develop moved when Info popped out: {develop_before:.1} to {:.1}",
            was(&layout, develop)
        );
        assert!(
            was(&layout, image) > image_before + 100.0,
            "the image did not take the space Info left"
        );
    }

    #[test]
    fn a_narrow_window_gives_the_picture_the_room_rather_than_the_panels() {
        // The fallback, and the reason it is not "panels always win". Honouring every
        // remembered width in a window too small for them would leave a sliver of image,
        // which is worse than a panel that changed size — this is an image editor.
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        // A window barely wider than two panels at the stop, with the restore asked for
        // on every frame so it gets every chance to starve the picture.
        let tiny = egui::vec2(2.0 * MIN_PANE + 60.0, 600.0);
        for _ in 0..6 {
            frame_sized(&ctx, &mut layout, vec![], tiny);
            layout.restructured = true;
        }
        frame_sized(&ctx, &mut layout, vec![], tiny);
        let image = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Image))
            .expect("laid out");
        assert!(
            image.width() > 20.0,
            "the image was squeezed to {:.1}pt",
            image.width()
        );
    }

    #[test]
    fn a_panel_dragged_away_and_back_does_not_starve_the_one_it_left() {
        // Stack Develop under Info, pull it back out beside the image, and require all
        // three to still be panels rather than slivers.
        //
        // **This does not reproduce the share loss seen in testing**, and the comment
        // said it did until the run proved otherwise: on this path `Tiles::insert_at`
        // reuses the wrapped pane's `TileId` for the new container, so the share goes
        // with it, and `simplify`'s `Replace` keeps the surviving child's id. Some
        // other sequence produced a row where Info had no entry at all. The invariant
        // is pinned by `a_pane_dropped_into_a_row_does_not_land_one_point_wide`
        // instead, which is the better place for it — `normalise_shares` makes a
        // missing share harmless however it came about, so the fix does not depend on
        // knowing which gesture caused it.
        //
        // Kept as an end-to-end guard on the two-drag sequence, which nothing else
        // covers.
        let (ctx, mut layout) = settled();
        let info_rect = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Info))
            .expect("laid out");
        drag_pane(
            &ctx,
            &mut layout,
            Pane::Develop,
            egui::pos2(
                info_rect.center().x,
                info_rect.top() + info_rect.height() * 0.75,
            ),
        );

        let image_rect = layout
            .tree
            .tiles
            .rect(tile_of(&layout.tree, Pane::Image))
            .expect("laid out");
        drag_pane(
            &ctx,
            &mut layout,
            Pane::Develop,
            egui::pos2(
                image_rect.left() + image_rect.width() * 0.2,
                image_rect.center().y,
            ),
        );

        // The open ones. `settled` closes Snapshots so Info has a column to itself, and
        // a closed pane has no rect — filtered explicitly rather than by tolerating a
        // missing one, so a *shown* pane that lost its rect still fails here.
        for pane in Pane::ALL
            .into_iter()
            .filter(|p| *p == Pane::Image || layout.shown.contains(p))
        {
            let rect = layout
                .tree
                .tiles
                .rect(tile_of(&layout.tree, pane))
                .expect("laid out");
            assert!(
                rect.width() > MIN_PANE,
                "{pane:?} ended the round trip {:.1}pt wide",
                rect.width()
            );
        }
    }

    #[test]
    fn the_tree_does_not_run_away() {
        // The 7b runaway, asked of the new container. A module box is content + inner
        // margin + stroke on each side, and leaving the stroke out of that sum made
        // each box two points wider than the space it was given — the panel measured
        // its content, grew by two, and swallowed the window over a few seconds.
        //
        // A tile tree cannot close that loop, and this is what says so: `Tree::ui`
        // divides `available_rect` top-down rather than measuring what a pane drew.
        // The test is kept anyway, because "the container measures its content" is
        // exactly the kind of thing a later version could start doing.
        let w = pane_widths(8);
        assert!(
            w.windows(2).all(|p| (p[0] - p[1]).abs() < 0.01),
            "the develop pane's width is not settling: {w:?}"
        );
        let settled = w.last().copied().expect("eight frames");
        assert!(
            (settled - DEVELOP_W).abs() < 8.0,
            "the develop pane settled at {settled}, not near the {DEVELOP_W} it was given"
        );
    }

    #[test]
    fn the_shipped_side_panels_have_their_authored_widths() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut layout = Layout::default();
        frame(&ctx, &mut layout, vec![]);
        frame(&ctx, &mut layout, vec![]);

        let width = |pane| {
            layout
                .tree
                .tiles
                .rect(tile_of(&layout.tree, pane))
                .expect("active side pane is laid out")
                .width()
        };
        let (left, right) = (width(Pane::Develop), width(Pane::Info));
        // CentralPanel removes its own margin before the tree sees the 1400pt test
        // window, so compare the authored ratio rather than pretending either child
        // receives the unscaled constant verbatim.
        let scale = left / DEVELOP_W;
        assert!(
            (right - INFO_W * scale).abs() < 2.0,
            "Develop/Info came out {left:.1}/{right:.1}pt rather than the authored \
             {DEVELOP_W}/{INFO_W} ratio"
        );
    }

    #[test]
    fn the_default_layout_is_a_reachable_state_and_not_only_a_starting_one() {
        // What Settings' "Reset to the default layout" does, which is
        // `Layout::default()` and nothing more. The button carried a "not built yet"
        // note claiming there was no path that rebuilt the tree in place; there was —
        // `reset_panels_on_start` had been running exactly this since it was added.
        //
        // Worth a test rather than trusting `Default`, because a reset is only a reset
        // if it reaches *every* piece of state a user can move: a floating panel that
        // stayed floating, or a closed one that stayed closed, is the half-reset that
        // makes somebody click the button twice and then stop believing it.
        let mut layout = Layout::default();
        let fresh = (layout.tree.tiles.len(), layout.shown.clone());

        // Move everything a user can move: close a panel, pop another out, and mark
        // the tree dirty the way a drag would.
        layout.close(Pane::Info);
        layout.float(Pane::Snapshots);
        layout.dirty = true;
        assert_ne!(
            layout.shown, fresh.1,
            "the setup did not actually close anything"
        );
        assert!(
            layout.is_out(Pane::Snapshots),
            "the setup did not pop anything out"
        );

        layout = Layout::default();
        assert_eq!(layout.shown, fresh.1, "a closed panel did not come back");
        assert_eq!(
            layout.tree.tiles.len(),
            fresh.0,
            "the tree is not the shipped one"
        );
        for p in Pane::PANELS {
            assert!(!layout.is_out(p), "{p:?} was left floating by a reset");
        }
    }
}
