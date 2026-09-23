//! Develop tabs: up to eight images open at once, compared by switching between
//! them.
//!
//! # Why this is cheap
//!
//! `Params` is a **value** and `raw_graph::build` is a pure function of it, so a
//! tab owns no pipeline state — no node objects, no per-tab shaders, no cached
//! topology to keep in step with the controls. Duplicating a tab is therefore a
//! `Params::clone` and four `Arc::clone`s, and nothing decodes, derives, or
//! recompiles. That was typed `Params` paying out; resist any design
//! where a tab owns mutable pipeline state and it stops being true.
//!
//! # Comparison is switching, not splitting
//!
//! Deliberately no split viewer and no synced viewports — flicker comparison is
//! more sensitive to tonal differences than side-by-side, and it is the reason the
//! backtick key exists and the reason [`WARM`] is 2.
//!
//! # Identity
//!
//! A tab is identified by [`TabId`], never by index. Tabs are closed while decodes
//! are in flight, and a result that comes back for a closed tab must be discarded
//! rather than applied to whoever now sits at that index. Ids are not reused.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use raw_core::composition::Orientation;
use raw_core::{Frame, History, LumaImage, Params, SensorImage};
use raw_gpu::{Cell, Viewport};

use crate::decode::Decoded;
use crate::histogram::Histogram;

/// What the viewport is showing.
///
/// The two colour entries are **reference views, not renders** — see
/// `raw_core::preview` for why that distinction is architectural rather than a
/// matter of intent. They are drawn as a plain texture and never enter the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewSource {
    /// The pipeline.
    #[default]
    Mono,
    /// The camera's own JPEG, in colour.
    Jpeg,
    /// Camera-native RGB, linear, binned from the CFA. No colour matrix.
    RawLinear,
}

impl PreviewSource {
    /// `j` walks these in order and back to the render.
    pub fn next(self) -> Self {
        match self {
            Self::Mono => Self::Jpeg,
            Self::Jpeg => Self::RawLinear,
            Self::RawLinear => Self::Mono,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "",
            Self::Jpeg => "camera JPEG",
            Self::RawLinear => "raw linear · camera-native RGB, no color matrix",
        }
    }

    /// The footer badge, in the same clipped upper case as `OVEREXPOSED` and
    /// `PREVIEW ORIGINAL`, or `None` for the render itself.
    ///
    /// **Not [`label`](Self::label), and the two are not interchangeable.** That one
    /// is a sentence for the status line, written once when you press `j` and gone by
    /// the time you have looked away; this is a standing badge that has to sit in a
    /// row of other badges and be read at a glance, so it is a token rather than a
    /// description. `raw linear · camera-native RGB, no colour matrix` in that row
    /// would be longer than the rest of the footer put together.
    ///
    /// **These two badges were missing and they are the ones that most needed to be
    /// there.** Every other reference view in this app announces itself, on the rule
    /// that a mode is invisible until it surprises you — and these are the *only* two
    /// that replace the picture with a different picture. `PREVIEW ORIGINAL` was added
    /// to the list on exactly that argument ("it looks like your picture, just the
    /// wrong one"); a camera JPEG in colour looks even more like a finished
    /// photograph, and a raw linear view looks like a flat one you might start trying
    /// to fix. Both are unrecoverable-looking states with a one-key cause.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::Mono => None,
            Self::Jpeg => Some("CAMERA JPEG"),
            Self::RawLinear => Some("RAW LINEAR"),
        }
    }
}

/// What a drag on the image means.
///
/// # Why this is an enum, and why it is here
///
/// **A mode exists when the same input has to mean different things.** Not when a
/// feature has its own panel — a slider only ever means one thing. Three inputs are
/// contested: the drag on the image (there is exactly one and it pans), the bare
/// keys (`↵` and `Esc` mean commit and cancel only inside a gesture), and what is
/// drawn over the picture. Crop is the first module that wants all three, and Dodge
/// & Burn, value pins and compare are queued behind it.
///
/// **An enum, not a bag of bools.** The contrast with `raw_gpu::Overlays` is the
/// point: overlays are a struct of bools because they are orthogonal and additive,
/// and you can have overexposed and false colour at once. Modes are mutually
/// exclusive *by definition*, and the exclusivity is the whole feature. It carries
/// payload — `Crop { grabbed }`, later `Paint { instance, kind }` — the same shape
/// `Sampling::Demosaic(algo)` and `ToneMap::Paper(grade)` already use.
///
/// **Per tab, not per app.** Crop state belongs to an image, and the backtick flick
/// between two frames would be incoherent if the mode were global and the other
/// frame were not in a state to receive it. `View` is already per tab for the same
/// reason.
///
/// **On the tab, not on the tile tree.** Tabbing a "COMPOSITION" pane beside
/// "DEVELOP" reads well and would put the layout — which can be dragged apart,
/// closed and popped onto another monitor — in charge of what a drag on the picture
/// means. Pop the panel out and the active tab is not even next to the image. The
/// mode is state; the panel reflects it.
///
/// **View state, not `Params`.** The crop *rectangle* is `Params` — it is in the
/// sidecar and it is undoable. Whether the tool is *open* is like zoom: not
/// undoable, not written to disk, not copied by a duplicate.
///
/// **`Clone` but not `Copy`.** Paint mode's cancel snapshot is a
/// stroke list, which owns a `Vec`. Every accessor below therefore takes `&self`,
/// and the call sites that used to copy the mode out now borrow it — which is the
/// honest shape anyway: a mode carrying kilobytes is not a value to duplicate
/// casually, and the compiler now says so.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Mode {
    /// Pan. What a drag has always meant, and the state every other
    /// mode returns to.
    #[default]
    View,
    /// The crop tool.
    ///
    /// `grabbed` spans frames, like `Tab::curve_drag`. `entered` is the composition
    /// as it stood when the tool opened, and it is here rather than on the tab for a
    /// reason worth stating: **the snapshot cannot outlive the mode that needs it**,
    /// and there is no second place to forget to take it or to clear it.
    Crop {
        grabbed: Option<Grab>,
        entered: raw_core::CompositionParams,
    },
    /// Perspective guide editing. `grabbed` is either a handle index or one of the
    /// two private line-drawing sentinels used by the viewport; the guide points
    /// themselves are authored Composition parameters and are restored by Esc from
    /// the snapshot carried here.
    Keystone {
        grabbed: Option<usize>,
        entered: raw_core::CompositionParams,
    },
    /// The Dodge & Burn brush.
    ///
    /// `entered` is the stroke set as it stood when the tool opened, inside the
    /// variant for the reason 9a settled: **the snapshot cannot outlive the mode
    /// that needs it**. It is a `Box` because a `DodgeBurnParams` carries a `Vec`
    /// and `Mode` is `Copy` everywhere else in this file — boxing keeps the enum
    /// small and, more usefully, keeps the clone explicit at the two places that
    /// take and restore it.
    ///
    /// `stroke` is the pass in flight, and spans frames the way `Grab` does.
    Paint {
        sign: raw_core::Sign,
        /// Which of the three shapes the next press makes.
        tool: crate::paint::Tool,
        grab: Option<PaintGrab>,
        entered: Box<raw_core::DodgeBurnParams>,
    },
    /// The Inspector is placing value pins.
    ///
    /// **No snapshot, for the same reason `Loupe` has none**: it edits nothing in
    /// `Params`. Pins are view state, so there is nothing for `Esc` to put back and
    /// leaving simply closes the mode. What it does have that `Loupe` does not is a
    /// drag that means something — moving a pin — which is why it claims one.
    Pin,
    /// The Curve module is waiting for one image click to place a point in the
    /// selected instance. One-shot like Triopro's neutral sampler: a successful
    /// click spends it, while `Esc` leaves without changing anything.
    CurvePoint,
    /// The grain loupe is up, and a drag on the picture moves what it samples.
    ///
    /// **The only mode with no snapshot**, and the reason is worth stating rather
    /// than looking like an omission: the other two edit `Params` while they are open
    /// and need somewhere to put back, and this one edits nothing at all. Dragging
    /// the loupe changes which pixels are being looked at and no pixel of the
    /// picture, so there is nothing for `Esc` to restore — it simply closes.
    Loupe,
}

/// What a paint-mode drag has hold of.
///
/// Two gestures on one button, told apart by the tool and by where the press
/// landed — the same shape [`Grab`] has for crop, and for the same reason: the
/// brush and the two gradients are one mode, not three. Named apart from `Grab`
/// rather than sharing it, because the two modes' gestures have nothing in common
/// beyond the word.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaintGrab {
    /// A brush pass in flight.
    Stroke {
        /// The last dab's centre, in source-normalised coordinates.
        ///
        /// Dabs are laid along the drag at a fixed spacing rather than one per
        /// frame: a frame is a unit of *time*, and a brush that deposited per frame
        /// would put a dense line down when you moved slowly and a dotted one when
        /// you moved fast. Interpolating from here to the pointer makes the deposit
        /// a function of distance, which is what a wand under an enlarger is.
        last: (f32, f32),
        /// This is an eraser pass — `⌥` was down when it started.
        ///
        /// Frozen at the press, not read per frame: a modifier released mid-drag
        /// must not turn the second half of a pass into a burn. The prototype
        /// freezes the mode for the duration of a drag for the same reason.
        erasing: bool,
        /// The direction the last dabs were laid along, in degrees.
        ///
        /// Carried so the **cursor** can show a following nib at the angle it is
        /// actually pressing rather than axis-aligned. `None` until the pointer has
        /// moved far enough to have a direction at all, which is every frame of a
        /// click-and-hold — and kept from the previous frame once it has one, so a
        /// pause inside one spacing does not snap the nib back.
        bearing: Option<f32>,
    },
    /// One end of a gradient, being placed or moved. Placement and adjustment are
    /// the *same* gesture: a press on empty canvas resets the shape to a
    /// zero-length one under the cursor and grabs its far end, so laying a new
    /// gradient and re-dragging an old one run through one path.
    Handle(crate::paint::End),
}

/// What a crop-mode drag has hold of.
///
/// Three gestures on one button, told apart by where the press landed — which is
/// what lets the crop tool also be the rotate tool and the straighten tool without
/// three modes to switch between.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Grab {
    /// An edge, a corner, or the interior.
    Grip(Handle),
    /// Rotating the picture under the box, from just outside a corner.
    ///
    /// Both halves are frozen at the press: the pointer's bearing from the crop
    /// centre, and the angle the picture was already at. The drag is then the
    /// *difference* of bearings added to the base, which is what stops the picture
    /// jumping to meet the cursor on the first frame.
    Rotate { bearing: f32, base: f32 },
    /// Drawing a horizon. Screen points, both ends, because the line is chrome and
    /// never becomes image geometry until it is released.
    Line { from: egui::Pos2, to: egui::Pos2 },
}

impl Mode {
    /// Open the crop tool over `entered`, remembering it so `Esc` can put it back.
    pub fn crop(entered: raw_core::CompositionParams) -> Self {
        Self::Crop {
            grabbed: None,
            entered,
        }
    }

    pub fn keystone(entered: raw_core::CompositionParams) -> Self {
        Self::Keystone {
            grabbed: None,
            entered,
        }
    }

    /// Open the brush over `entered`, remembering it so `Esc` can put it back.
    pub fn paint(
        sign: raw_core::Sign,
        tool: crate::paint::Tool,
        entered: raw_core::DodgeBurnParams,
    ) -> Self {
        Self::Paint {
            sign,
            tool,
            grab: None,
            entered: Box::new(entered),
        }
    }

    pub fn is_crop(&self) -> bool {
        matches!(self, Self::Crop { .. })
    }

    pub fn is_keystone(&self) -> bool {
        matches!(self, Self::Keystone { .. })
    }

    pub fn is_paint(&self) -> bool {
        matches!(self, Self::Paint { .. })
    }

    pub fn is_loupe(&self) -> bool {
        matches!(self, Self::Loupe)
    }

    pub fn is_curve_point(&self) -> bool {
        matches!(self, Self::CurvePoint)
    }

    /// Whether this mode claims a drag on the picture, and pan must stand down.
    ///
    /// **One predicate, read at the one place a drag on the image is handled.** The
    /// alternative — a condition per mode at that call site — is how the second
    /// mode's drag comes to pan the image as well as paint.
    pub fn claims_drag(&self) -> bool {
        !matches!(self, Self::View)
    }

    /// Whether the viewport should pan for this drag.
    ///
    /// Space is a temporary hand for Dodge/Burn and perspective guide placement. It
    /// deliberately does not make crop, pin, curve sampling, or the loupe surrender
    /// their gestures, and it does not replace the active tool with `View` —
    /// releasing it leaves that tool armed.
    pub fn pans_with_drag(&self, temporary_hand: bool) -> bool {
        !self.claims_drag()
            || (temporary_hand && matches!(self, Self::Paint { .. } | Self::Keystone { .. }))
    }

    /// Which way the brush is working, while it is open.
    pub fn painting(&self) -> Option<raw_core::Sign> {
        match self {
            Self::Paint { sign, .. } => Some(*sign),
            _ => None,
        }
    }

    /// Which shape the next press makes.
    pub fn tool(&self) -> Option<crate::paint::Tool> {
        match self {
            Self::Paint { tool, .. } => Some(*tool),
            _ => None,
        }
    }

    /// The gesture in flight.
    pub fn grab(&self) -> Option<PaintGrab> {
        match self {
            Self::Paint { grab, .. } => *grab,
            _ => None,
        }
    }

    /// What to put back if paint mode is **cancelled**. See [`Mode::cancelled`].
    pub fn cancelled_strokes(&self) -> Option<&raw_core::DodgeBurnParams> {
        match self {
            Self::Paint { entered, .. } => Some(entered),
            _ => None,
        }
    }

    /// The grab in flight, if any.
    pub fn grabbed(&self) -> Option<Grab> {
        match self {
            Self::Crop { grabbed, .. } => *grabbed,
            _ => None,
        }
    }

    /// What to put back if this mode is **cancelled**.
    ///
    /// `Esc` cancels; `c` and `↵` commit. the maintainer reported the first version, where
    /// `Esc` left the crop applied: the one key every application uses to mean "I
    /// did not want that" was the key that kept it. A tool you cannot back out of is
    /// a tool you hesitate to open.
    pub fn cancelled(&self) -> Option<raw_core::CompositionParams> {
        match self {
            Self::Crop { entered, .. } | Self::Keystone { entered, .. } => Some(*entered),
            _ => None,
        }
    }

    pub fn keystone_grabbed(&self) -> Option<usize> {
        match self {
            Self::Keystone { grabbed, .. } => *grabbed,
            _ => None,
        }
    }

    /// What the footer says while this mode is open, in the ruby the overlays
    /// already use for "something is on that would not be on by default".
    ///
    /// The prototype's `PIN MODE · click to place · … · I to exit` is the
    /// established shape, and it is the affordance that makes a mode survivable:
    /// **a mode is invisible until it surprises you**, so it says what it is and
    /// how to leave.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::View => None,
            // Two sentences, because the tool has two halves and the second is
            // not discoverable: nothing about a crop box suggests that the space
            // *outside* a corner rotates the picture.
            Self::Crop {
                grabbed: Some(Grab::Line { .. }),
                ..
            } => Some("STRAIGHTEN · drag a line along a horizon or a vertical"),
            // Both ways out are named, because they do different things and only one
            // of them is guessable.
            Self::Crop { .. } => Some(
                "CROP · drag an edge, a corner, or outside a corner to level · \
                 Return or c to apply · Esc to cancel",
            ),
            Self::Keystone { .. } => Some(
                "KEYSTONE · draw two parallel lines or drag a handle · Space to pan · \
                 Return to apply · Esc to cancel",
            ),
            // Names the eraser, which is the one gesture nothing on screen suggests,
            // and both bracket pairs, which are now modal and therefore not in the
            // hotkey reference. The mode's own line is where a modal key is
            // documented; see `docs/hotkeys.md`.
            // The gradients get their own line: nothing about a dashed ellipse says
            // that the drag which made it can be done again to move it, and the
            // eraser and the bracket keys do not apply to them at all.
            // **Both ways out are named, and they do different things**: `↵` keeps
            // the work and `Esc` puts back the layers as they stood when the tool
            // opened. Only `Esc` was listed, which made the destructive one the
            // discoverable one.
            // Says what it is *for* as well as how to work it. Grain is export-only,
            // so the thing a user most needs told is that this window is the only
            // place the module is visible at all — and that the picture behind it has
            // not changed and is not going to.
            Self::Loupe => Some(
                "PRINT LOUPE · drag to move the sample · ⇧V toggles Before / After · \
                 grain and sharpening are export-only and never reach the viewport · \
                 Esc to close",
            ),
            // The prototype's line, and the shape every other mode hint here copied.
            Self::Pin => Some(
                "PIN MODE · click to place · drag pin to move · ⇧+click pin to delete · \
                 i to exit",
            ),
            Self::CurvePoint => {
                Some("CURVE SAMPLER · click the picture to add a point · Esc to cancel")
            }
            Self::Paint {
                tool: crate::paint::Tool::Linear,
                ..
            } => Some(
                "LINEAR GRADIENT · drag from full strength to none · \
                 drag a handle to move it · Return to keep · Esc to discard",
            ),
            Self::Paint {
                tool: crate::paint::Tool::Radial,
                ..
            } => Some(
                "RADIAL GRADIENT · drag from the center outwards · \
                 drag a handle to move or turn it · Return to keep · Esc to discard",
            ),
            Self::Paint {
                sign: raw_core::Sign::Dodge,
                ..
            } => Some(
                "DODGE · drag to paint · ⌥ drag to erase · ⇧ click for a straight pass · \
                 [ ] radius · x to burn · Return to keep · Esc to discard",
            ),
            Self::Paint { .. } => Some(
                "BURN · drag to paint · ⌥ drag to erase · ⇧ click for a straight pass · \
                 [ ] radius · d to dodge · Return to keep · Esc to discard",
            ),
        }
    }
}

/// One value pin: a place on the negative you are watching.
///
/// **Stored in source pixels**, which is the same frame Dodge & Burn welds its dabs
/// and gradients to. A pin therefore stays on the thing it was put on when the
/// picture is turned, straightened or cropped — `Frame::from_source` puts it back on
/// screen. Storing screen or frame coordinates instead would make a pin slide off its
/// subject the first time the image was rotated, which is the failure mode the
/// readout itself had before 9a.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pin {
    pub x: f32,
    pub y: f32,
}

/// The Inspector's pins, and how they are being read.
///
/// **View state, not `Params`.** Nothing here reaches the sidecar, undo, or a
/// duplicate — the same rule that keeps `db_active`, `guide` and the per-layer brush
/// controls off `Params`. Where you were looking is not a property of the picture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pins {
    pub items: Vec<Pin>,
    /// `⇧I` — the pins stay placed and stop being drawn.
    ///
    /// **Hidden, not cleared**, and the two are different actions with different
    /// buttons: `HIDE` is for getting the marks off a picture you are judging, and
    /// `clear` is for being done with them. A hide that discarded would make the
    /// safer-looking control the destructive one.
    pub hidden: bool,
    /// Read the value **entering** the tone chain rather than the one leaving it.
    ///
    /// Not a duplicate of the A/B flick (bare backtick): that compares your edit
    /// against the default, and this compares the value after the chain against the
    /// value before it. See `docs/ux-inventory.md`.
    pub show_raw: bool,
    /// Which pin a drag has hold of. Spans frames, like `Tab::curve_drag`.
    pub dragging: Option<usize>,
}

/// The nearest of `points` to `p`, within `within`, if any.
///
/// **Nearest rather than first-hit**: pins overlap at low zoom, and the one whose
/// centre is closest to the press is the one that was aimed at. Taking the first
/// match would hand the drag to whichever happened to be placed earlier.
///
/// Screen space, and that is a correction. It was source space, which meant the
/// grab radius had to be divided by the zoom at every call site — and, worse, that a
/// pin's *label box* could not take part, because a box is laid out in points and
/// has no size on the negative at all. Hit-testing where the marks are drawn is what
/// lets the thing you can see and the thing you can grab be the same thing.
pub fn nearest_to(
    points: impl Iterator<Item = egui::Pos2>,
    p: egui::Pos2,
    within: f32,
) -> Option<usize> {
    points
        .enumerate()
        .map(|(i, q)| (i, q.distance_sq(p)))
        .filter(|(_, d2)| *d2 <= within * within)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// The pin colours, in placement order.
///
/// **Nine hues on a monochrome print is a departure**, and it is made knowingly:
/// `theme` records that ruby is the app's only saturated colour because it is spent
/// on interaction state, which is the argument that kept a blue dodge and a red burn
/// out of the brush panel. That argument does not reach here, and the difference is
/// what the colour is *for* — dodge and burn are told apart by **value**, because
/// there are two of them and they are opposites; pins have to be told apart from each
/// other, up to nine at once, against an image of arbitrary tone. Value cannot do
/// that job and hue can.
///
/// Amber first, so the common case of one or two pins stays on the app's own accent.
/// These are the prototype's, which are already a muted set rather than a spectrum.
pub const PIN_COLOURS: [egui::Color32; 9] = [
    egui::Color32::from_rgb(0xC8, 0xA9, 0x6E),
    egui::Color32::from_rgb(0x6E, 0xC8, 0xA9),
    egui::Color32::from_rgb(0xA9, 0x6E, 0xC8),
    egui::Color32::from_rgb(0xC8, 0x6E, 0x6E),
    egui::Color32::from_rgb(0x6E, 0x9A, 0xC8),
    egui::Color32::from_rgb(0xC8, 0xC8, 0x6E),
    egui::Color32::from_rgb(0x6E, 0xC8, 0x6E),
    egui::Color32::from_rgb(0xC8, 0x6E, 0xA9),
    egui::Color32::from_rgb(0x6E, 0xA9, 0xC8),
];

/// The colour for the pin at `i`, wrapping past nine.
pub fn pin_colour(i: usize) -> egui::Color32 {
    PIN_COLOURS[i % PIN_COLOURS.len()]
}

/// Which part of the crop rectangle a drag has hold of.
///
/// Eight edges and corners plus the interior, which is the whole vocabulary of a
/// crop drag. Named by compass point rather than by index because every piece of
/// arithmetic below asks "does this move the left edge", and `Handle::W` answers
/// that where `handles[3]` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
    /// The interior: slides the whole rectangle without resizing it.
    Body,
}

/// Which sides of the crop rectangle a drag moves.
///
/// Named fields rather than `(bool, bool, bool, bool)`: the tuple was destructured
/// positionally at the one call site, so transposing two of them compiled and resized
/// the wrong edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Edges {
    pub const NONE: Self = Self {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };

    /// Whether this handle drives the axis at all — a corner drives both.
    pub fn horizontal(self) -> bool {
        self.left || self.right
    }

    pub fn vertical(self) -> bool {
        self.top || self.bottom
    }
}

impl Handle {
    /// The four corners and the four edges, in that order.
    ///
    /// **Corners first**, which is the load-bearing part: their hit areas overlap
    /// the edges', and a corner grab that landed on an edge would resize one axis
    /// when the user aimed at two. `crop::hit` walks corners before edges for
    /// exactly this reason.
    pub const CORNERS: [Self; 4] = [Self::NW, Self::NE, Self::SE, Self::SW];
    pub const EDGES: [Self; 4] = [Self::N, Self::S, Self::W, Self::E];

    /// Which edges this handle moves.
    pub fn edges(self) -> Edges {
        match self {
            Self::N => Edges {
                top: true,
                ..Edges::NONE
            },
            Self::S => Edges {
                bottom: true,
                ..Edges::NONE
            },
            Self::W => Edges {
                left: true,
                ..Edges::NONE
            },
            Self::E => Edges {
                right: true,
                ..Edges::NONE
            },
            Self::NW => Edges {
                left: true,
                top: true,
                ..Edges::NONE
            },
            Self::NE => Edges {
                right: true,
                top: true,
                ..Edges::NONE
            },
            Self::SW => Edges {
                left: true,
                bottom: true,
                ..Edges::NONE
            },
            Self::SE => Edges {
                right: true,
                bottom: true,
                ..Edges::NONE
            },
            Self::Body => Edges::NONE,
        }
    }

    /// Where this handle sits on a unit rectangle, as `(u, v)` in `[0, 1]`.
    pub fn at(self) -> (f32, f32) {
        match self {
            Self::N => (0.5, 0.0),
            Self::S => (0.5, 1.0),
            Self::W => (0.0, 0.5),
            Self::E => (1.0, 0.5),
            Self::NW => (0.0, 0.0),
            Self::NE => (1.0, 0.0),
            Self::SW => (0.0, 1.0),
            Self::SE => (1.0, 1.0),
            Self::Body => (0.5, 0.5),
        }
    }

    /// The cursor to show over it.
    pub fn cursor(self) -> egui::CursorIcon {
        match self {
            Self::N | Self::S => egui::CursorIcon::ResizeVertical,
            Self::W | Self::E => egui::CursorIcon::ResizeHorizontal,
            Self::NW | Self::SE => egui::CursorIcon::ResizeNwSe,
            Self::NE | Self::SW => egui::CursorIcon::ResizeNeSw,
            Self::Body => egui::CursorIcon::Move,
        }
    }
}

/// Tab cap, from the handoff. Eight is a working limit rather than a technical
/// one: past it the strip stops being readable and the memory story stops holding.
pub const MAX_TABS: usize = 8;

/// How many tabs keep their GPU state while not in front.
///
/// **Two, because flicker comparison is between two frames.** The brief's leaning
/// was to drop everything on every tab that is not visible, and the reasoning for
/// that is sound — a background tab is not drawing and the graph rebuilds from
/// `Params` every frame anyway. But the app's *only* comparison mechanism is
/// switching tabs, and a cold tab has to re-upload its working image: 164 MB on a
/// 41 MP frame, tens of milliseconds. Paying that on every A/B flick would make
/// the headline workflow feel broken.
///
/// Two is the smallest number that makes flicker free, and it bounds GPU residency
/// at two images' worth however many tabs are open — which is the memory story the
/// handoff asked for ("roughly one image's worth of GPU intermediates live") at the
/// one place it has to bend to be usable.
pub const WARM: usize = 2;

/// Stable identity for a tab.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct TabId(u64);

/// Where the viewport is looking. View state, not image state: not undoable, not
/// written to the sidecar, and **not copied when a tab is duplicated** — a
/// duplicate is a second look at the same frame, and inheriting a 400% zoom into a
/// corner is not what "duplicate" means.
pub struct View {
    /// Screen pixels per output pixel. 1.0 is 100%.
    pub scale: f32,
    /// Top-left of the visible region, in output-pixel coordinates.
    pub off: egui::Vec2,
    pub fit: bool,
}

/// The preferred zoom percentages `⌘+` and `⌘-` step through.
///
/// **Reciprocals below 100%, integers above.** Multiplying by a constant — which is
/// what the keys used to do — walks through numbers nobody chose: from a fit of 12.8%
/// a 1.25 factor gives 16.0, 20.0, 25.0, 31.2, 39.1, and the footer reads `39%`. The
/// rungs here are the ones a photographer already thinks in, so the readout is a round
/// number at every stop and 50% really is half.
///
/// Below 100% they are `100/n` for n = 12, 8, 6, 4, 3, 2, 1.5 — which is why 16.67 and
/// 66.67 look untidy written down and are exactly right on screen: 16.67% is one screen
/// pixel per six output pixels, a whole-number relationship the resampler can land on.
///
/// **Scroll zoom stays continuous and does not use this.** The rungs are for the keys;
/// a wheel that snapped between them would feel broken. Fit is not on the ladder either
/// — it is whatever the window happens to give.
pub const ZOOM_STEPS: [f32; 15] = [
    0.0625, 0.0833, 0.125, 0.1667, 0.25, 0.3333, 0.5, 0.6667, 1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 16.0,
];

/// The first rung above `scale`, or the top of the ladder.
///
/// A small tolerance, so a scale already sitting on a rung steps past it rather than
/// returning itself — otherwise `⌘+` at exactly 50% would do nothing.
pub fn zoom_in_from(scale: f32) -> f32 {
    ZOOM_STEPS
        .iter()
        .copied()
        .find(|s| *s > scale * 1.001)
        .unwrap_or_else(|| ZOOM_STEPS[ZOOM_STEPS.len() - 1])
}

/// The first rung below `scale`, or the bottom of the ladder.
pub fn zoom_out_from(scale: f32) -> f32 {
    ZOOM_STEPS
        .iter()
        .copied()
        .rev()
        .find(|s| *s < scale * 0.999)
        .unwrap_or(ZOOM_STEPS[0])
}

/// The zoom as the footer prints it.
///
/// One decimal, trimmed when it says nothing. The footer used to round to whole
/// percent, which reported a 12.5% rung as `13%` and a 16.67% rung as `17%` — a readout
/// that disagrees with the ladder it came from, on an app whose whole footer exists to
/// give honest numbers. `50%` stays `50%` rather than `50.0%`, because a decimal that is
/// always zero is noise.
pub fn zoom_label(scale: f32) -> String {
    let text = format!("{:.1}", scale * 100.0);
    format!("{}%", text.trim_end_matches('0').trim_end_matches('.'))
}

impl View {
    /// Change scale while keeping the source point under `anchor` fixed on screen.
    /// `anchor` is in physical viewport pixels from the top-left of the image rect.
    /// This is what makes zoom feel like it happens *at the cursor* rather than at
    /// a corner. Leaves `fit` untouched — the caller decides.
    pub fn zoom_about(&mut self, anchor: egui::Vec2, new_scale: f32) {
        let new_scale = new_scale.clamp(0.02, 32.0);
        let source_under_anchor = self.off + anchor / self.scale;
        self.scale = new_scale;
        self.off = source_under_anchor - anchor / new_scale;
    }
}

impl Default for View {
    fn default() -> Self {
        Self {
            scale: 1.0,
            off: egui::Vec2::ZERO,
            fit: true,
        }
    }
}

/// The decoded file behind a tab. Every field is shared: two tabs on one raw hold
/// the same allocations, so a duplicate costs pointers rather than megabytes.
#[derive(Clone)]
pub struct Image {
    /// Kept so decode-option changes re-decode without re-reading the file.
    pub sensor: Arc<SensorImage>,
    pub decoded: Arc<Decoded>,
    pub path: PathBuf,
    pub key: crate::decode::ContentKey,
}

/// A tab's GPU state. Present only while the tab is warm; see [`WARM`].
/// One compare cell's GPU state: the target the viewport renders into, and egui's
/// registration of it.
///
/// **Held on `Render` rather than on the tab**, because it is GPU state with the same
/// lifetime as the viewport — `free_render` is the one place a tab's textures are
/// handed back, and a cell registered somewhere else would leak its `TextureId` for
/// the session exactly as the main target used to.
#[derive(Default)]
pub struct CompareCell {
    pub gpu: raw_gpu::Cell,
    pub texture: Option<egui::TextureId>,
    /// What this cell was last rendered for: its params, its size in physical pixels,
    /// and the shared view. **A cell's content is a pure function of these**, because a
    /// snapshot's params never change — that is what makes it a snapshot — so anything
    /// else is a re-render of a picture that is already on the GPU.
    ///
    /// The viewport's own change detection cannot do this job: `render_into` has to
    /// clear it, or each cell would compare itself against whichever cell was drawn
    /// before it. So the cache lives per cell, where the identity actually is.
    pub key: Option<CellKey>,
}

/// Everything a compare cell's picture depends on.
#[derive(Clone, PartialEq)]
pub struct CellKey {
    pub params: raw_core::Params,
    pub out: (u32, u32),
    pub zoom: f32,
    pub pan: (f32, f32),
}

/// Whether the compare grid is up, and how many cells it is showing.
///
/// **View state, per tab.** It changes no pixel of the image and has nothing to put
/// back — the same shape as `Mode::Loupe`, which is why `Esc` and `k` both simply close
/// it. Not a `Mode` variant all the same: every mode there claims a *drag on the
/// picture*, and compare replaces the picture rather than reinterpreting a gesture on
/// it.
pub struct Compare {
    pub open: bool,
    /// 2, 3 or 4. The requested layout is deliberately independent of the number of
    /// pinned snapshots: two looks may occupy a three-strip or four-cell layout, with
    /// the unused slots left empty, so the number keys always describe the same grid.
    pub n_up: usize,
    /// Zoom as a **multiple of each cell's own fit**, shared by every cell. 1.0 is
    /// "the whole picture in the tile".
    ///
    /// # Why a multiplier and not a scale
    ///
    /// This is the one real design question in syncing the cells, and it comes from a
    /// decision made earlier: a snapshot carries `composition`, so two cells can be
    /// **different shapes and different sizes**. A shared *pixel* scale would then show
    /// a tightly cropped cell at a completely different magnification from a full-frame
    /// one — and worse, at 1.0 they would not both fit.
    ///
    /// Sharing the multiplier instead means every cell shows the same *fraction* of its
    /// own picture, which is what makes two different crops of one negative comparable
    /// at all. It is also what the prototype does: it syncs a normalised viewport rect
    /// in image space and lets each cell slice its own composed image with it.
    pub zoom: f32,
    /// Where the shared view is centred, as a fraction of each cell's own picture from
    /// its centre. `(0, 0)` is the middle. Normalised for the same reason `zoom` is a
    /// multiplier.
    pub pan: (f32, f32),
}

impl Compare {
    /// How far in the grid goes. Past this the cells are showing a handful of pixels
    /// each and the comparison is of noise; the loupe is the tool for that.
    pub const MAX_ZOOM: f32 = 16.0;

    /// Select one of the comparison number-key layouts.
    ///
    /// `1` returns to the ordinary single-image viewer. `2`–`4` open (or reshape)
    /// comparison whenever there are at least two pinned snapshots. Letting the
    /// larger keys reopen it is important: otherwise pressing `1` makes the rest of
    /// the supposedly continuous `1`–`4` control inert until `k` is pressed again.
    pub fn select_layout(&mut self, n: usize, pinned: usize) -> bool {
        if n <= 1 {
            self.open = false;
            return true;
        }
        if pinned < 2 {
            return false;
        }

        let opening = !self.open;
        self.open = true;
        self.n_up = n.clamp(2, crate::snapshot::Snapshots::MAX_CELLS);
        if opening {
            self.reset_view();
        }
        true
    }

    /// How many snapshots are populated, and how many slots the requested grid keeps.
    pub fn counts(n_up: usize, pinned: usize) -> (usize, usize) {
        let slots = n_up.clamp(2, crate::snapshot::Snapshots::MAX_CELLS);
        (pinned.min(slots), slots)
    }

    /// How far from centre the shared view may be panned at `zoom`, as a fraction of a
    /// cell's own extent.
    ///
    /// At zoom *z* a cell shows `1/z` of its picture, so its centre can travel
    /// `0.5 - 0.5/z` before an edge comes inside the tile. **At fit that is exactly
    /// zero**, which is the case worth stating: a grid you can drag around while it is
    /// already showing the whole picture is one where the pictures no longer line up
    /// with each other, and lining up is what the grid is for.
    pub fn pan_room(zoom: f32) -> f32 {
        (0.5 - 0.5 / zoom.max(1.0)).max(0.0)
    }

    /// How the grid is divided for `n` cells, as (rows, columns).
    ///
    /// The prototype's `_GRID`, and the maintainer's reading of it: **2-up is side by side,
    /// 3-up is three vertical strips, 4-up is a square.** One row of *n* columns gives
    /// full-height strips, which is the right division for two and three because the
    /// pictures being compared are usually landscape — three of them stacked would be
    /// three letterbox slots with the frame edges nowhere near each other. At four a
    /// row would make each strip a quarter of the width and the square wins.
    ///
    /// Stated as a function rather than inline so it can be tested. The table is three
    /// entries and looks too small to get wrong, which is exactly the kind of thing
    /// that ends up transposed — and `(1, 3)` against `(3, 1)` is the difference
    /// between what was asked for and its opposite.
    pub fn grid(n: usize) -> (usize, usize) {
        match n {
            0 | 1 => (1, 1),
            2 => (1, 2),
            3 => (1, 3),
            _ => (2, 2),
        }
    }

    /// Back to the whole picture in every tile.
    pub fn reset_view(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
    }
}

impl Default for Compare {
    fn default() -> Self {
        Self {
            open: false,
            n_up: crate::snapshot::Snapshots::MAX_CELLS,
            zoom: 1.0,
            pan: (0.0, 0.0),
        }
    }
}

pub struct Render {
    pub viewport: Viewport,
    pub samples: crate::FinishedSampler,
    pub histogram: crate::histogram::GpuHistogram,
    /// A small, unregistered target used only to write Lightbox's developed preview.
    /// Separate from the live target so zoom, pan, canvas and Surround cannot leak
    /// into a tile that represents the photograph.
    pub edited_tile: Cell,
    /// A fitted, thumbnail-sized target for snapshot capture. It shares the live
    /// source but renders the complete stored crop without viewport zoom, pan,
    /// Surround or diagnostic overlays.
    pub snapshot: Cell,
    /// Compare's targets, one per cell. Empty until the grid is first drawn.
    pub cells: Vec<CompareCell>,
    /// egui's registration of the viewport target. Freed with the renderer when the
    /// tab goes cold, or the registration leaks for the life of the session.
    pub texture: Option<egui::TextureId>,
}

pub struct Tab {
    pub id: TabId,
    pub name: String,
    /// A duplicate. **Scratch: it never writes a sidecar.**
    ///
    /// Two tabs on one raw would otherwise write the same `.mono.xmp` in turn, and
    /// whichever settled last would win — so the original's edit would vanish
    /// because someone opened a duplicate to try something. Only the original owns
    /// the file. Saving a duplicate (`⌘S`) copies the raw and gives the copy its
    /// own sidecar, at which point the tab stops being scratch.
    pub scratch: bool,
    /// Source path while the initial decode is still queued. Once `image` arrives it
    /// carries the same path itself. Kept so a Lightbox rename can retarget a tab
    /// even in the small window before its decode has opened the file.
    pub opening_path: Option<PathBuf>,
    pub image: Option<Image>,
    /// Shared with duplicates: derived as a whole rather than mutated, so an
    /// `Arc` is exactly right and a duplicate skips a full CPU luminance pass.
    pub luma: Option<Arc<LumaImage>>,
    /// Bumped every time luminance is re-derived, so the histogram knows to refresh.
    pub luma_gen: u64,
    /// Changes only when decoded CFA data is replaced, not for luminance edits.
    pub scene_gen: u64,
    /// Inputs of the resident luminance image, not the currently edited controls.
    pub luma_params: Option<raw_core::LuminanceParams>,
    pub histogram: Histogram,
    pub render: Option<Render>,
    pub error: Option<String>,
    pub params: Params,
    pub history: History,
    /// Index of the curve control point being dragged; spans frames.
    pub curve_drag: Option<usize>,
    /// The selected Curve instance. View state: it changes no pixel by itself and
    /// therefore does not enter history or the sidecar.
    pub curve_active: usize,
    /// The selected control point in that instance, for arrow-key movement and EV
    /// readouts. Also view state.
    pub curve_point: Option<usize>,
    /// Rename editor for a Curve instance; committed text alone enters `Params`.
    pub curve_rename: Option<(usize, String)>,
    pub view: View,
    /// What a drag on the image means. View state, per tab; see [`Mode`].
    pub mode: Mode,
    /// Which Dodge & Burn instance the panel has selected, and the one a pass is
    /// painted onto.
    ///
    /// **View state, per tab.** Which row of a list is highlighted is not a property
    /// of the picture: it is not undoable, does not reach the sidecar, and a
    /// duplicate opens with nothing selected. An index rather than an id because the
    /// list is short, ordered, and rebuilt from `params` on every frame anyway —
    /// `paint::instance_for` keeps it valid, and every read below tolerates it
    /// pointing past the end, which is what an undo across a deletion produces.
    pub db_active: Option<usize>,
    /// Which point of the toning placement curve a drag has hold of. View state, and it
    /// spans frames for the same reason `curve_drag` does.
    pub placement_drag: Option<usize>,
    /// What a zone-ruler drag has hold of; spans frames, like `curve_drag`.
    pub zone_drag: Option<crate::widgets::ZoneGrab>,
    /// The layer being renamed, and the text so far.
    ///
    /// The buffer lives here rather than on the layer because a rename in progress
    /// is not an edit: abandoning it must leave no undo entry and touch no sidecar,
    /// and the only way to be sure of that is for `Params` never to see it until it
    /// is committed.
    pub db_rename: Option<(usize, String)>,
    /// Showing the selected layer's zone mask instead of the picture.
    ///
    /// View state, per tab, and **not** an `Overlays` field on the tab: it names a
    /// layer, and which layer is selected is view state too. It is resolved into
    /// `Overlays::zone_mask` at render time.
    pub db_view_mask: bool,
    /// Which guide is drawn inside the crop.
    ///
    /// View state, and per tab rather than per app: it draws nothing into the
    /// picture, changes no pixel and reaches no file. A frame you are composing on
    /// thirds and one you are composing on a diagonal are different jobs.
    pub guide: crate::crop::Guide,
    /// The straighten tool is armed: the next press on the picture draws a horizon
    /// rather than moving the crop.
    ///
    /// **One-shot.** It disarms the moment a line is released, which is the
    /// prototype's behaviour and the right one: drawing a horizon is a single
    /// corrective act, and a tool that stayed armed would make the next attempt to
    /// nudge an edge redraw the angle instead.
    pub straighten_armed: bool,
    /// Captured looks, newest first, and the pins that feed the compare grid.
    ///
    /// **Per tab, session-only, and not inherited by a duplicate.** Per tab because a
    /// snapshot is a version *of an image* — which is also what makes the compare grid
    /// affordable, since every cell is then the same decode under different parameters
    /// and there is one luminance texture rather than four. Not in the sidecar by
    /// the maintainer's decision; see `crate::snapshot`.
    ///
    /// Not inherited by a duplicate for the same reason `overlays` is not: a duplicate
    /// exists to try something else, and arriving with somebody else's four pinned
    /// comparisons already in the grid would be inheriting a question rather than a
    /// state.
    pub snapshots: crate::snapshot::Snapshots,
    /// The snapshot being renamed, and the text so far.
    ///
    /// Off `Snapshot` for the same reason `db_rename` is off the layer: a rename in
    /// progress is not an edit, so abandoning it must leave nothing behind, and the
    /// only way to be sure is for the thing being renamed never to see the buffer
    /// until it is committed.
    pub snap_rename: Option<(usize, String)>,
    /// The compare grid: up, and how many cells.
    pub compare: Compare,
    /// The Inspector's value pins. See [`Pins`].
    pub pins: Pins,
    /// Diagnostic overlays, per tab. View state: not undoable, not in the sidecar,
    /// and not inherited by a duplicate — checking one frame's clipping says
    /// nothing about another's.
    pub overlays: raw_gpu::Overlays,
    /// The reference mount around this image. Per-tab view state for the same reason
    /// as `overlays`: it is a temporary way of judging this photograph, not a global
    /// app mode and not an image edit. Width and colour remain app preferences.
    pub surround: bool,
    /// Which of the three views the viewport is showing.
    pub preview: PreviewSource,
    /// The colour reference view, decoded on demand and kept until the source
    /// changes. `None` means it has not been asked for, or the file has no
    /// embedded preview.
    pub preview_image: Option<(PreviewSource, std::sync::Arc<raw_core::preview::Rgb8>)>,
    /// egui's handle for `preview_image`.
    ///
    /// The **handle**, not the id: egui frees a managed texture when its handle
    /// drops, so holding the handle is what makes dropping it the whole cleanup.
    /// Keeping only the id and forgetting the handle would leak a full-size RGB
    /// texture on every press of `j`.
    pub preview_texture: Option<egui::TextureHandle>,
    /// Showing the image as it would render with no develop edits.
    ///
    /// A **view**, not an edit: it substitutes params at render time and never
    /// touches `Tab::params`, so it records no undo entry and writes no sidecar.
    /// Doing it by actually resetting the params would be indistinguishable from
    /// the user resetting them, which is the one thing it must not be.
    pub preview_original: bool,
    pub status: String,
    /// Read from the sidecar on load, written back with it. Owned by other
    /// applications as much as by this one, so it is carried through rather than
    /// interpreted.
    pub metadata: raw_core::sidecar::Metadata,
    /// The grain loupe: whether it is up, where it is looking, and the tile it has
    /// rendered.
    ///
    /// **View state, per tab**, beside `view` and `mode` rather than in `Params`.
    /// Whether you happen to be looking through a loupe is not a property of the
    /// print: it is not undoable, does not reach the sidecar, and a duplicate opens
    /// without one. The grain *settings* are in `Params`; this is the window onto
    /// them.
    pub loupe: crate::loupe::Loupe,
    /// Params as they were last written to disk. `None` until a sidecar has been
    /// read or written.
    ///
    /// This is what makes saving idempotent: the write happens when the params
    /// differ from what is on disk, not on every settled gesture, so undoing back
    /// to where you started does not rewrite the file.
    pub saved: Option<Params>,
}

impl Tab {
    /// An image-less tab for the Develop panel to draw against when nothing is open.
    ///
    /// **Not a tab in the strip** — it is never in `Tabs`, never gets an id anyone
    /// could reach, and nothing is ever written to it. It exists so the panel can
    /// render its own module stack at startup instead of an empty box, which is what
    /// the maintainer asked for: the app should look like itself before a file is chosen.
    ///
    /// `TabId(0)` because the real ones start at 1, so a placeholder that ever
    /// escaped into a lookup would miss rather than collide.
    pub fn placeholder() -> Self {
        Self::new(TabId(0), String::new())
    }

    fn new(id: TabId, name: String) -> Self {
        Self {
            id,
            name,
            scratch: false,
            opening_path: None,
            image: None,
            luma: None,
            luma_gen: 0,
            scene_gen: 0,
            luma_params: None,
            histogram: Histogram::default(),
            render: None,
            error: None,
            params: Params::default(),
            history: History::default(),
            curve_drag: None,
            curve_active: 0,
            curve_point: None,
            curve_rename: None,
            db_active: None,
            placement_drag: None,
            zone_drag: None,
            db_rename: None,
            db_view_mask: false,
            loupe: crate::loupe::Loupe::default(),
            view: View::default(),
            mode: Mode::default(),
            guide: crate::crop::Guide::default(),
            straighten_armed: false,
            snapshots: crate::snapshot::Snapshots::default(),
            snap_rename: None,
            compare: Compare::default(),
            pins: Pins::default(),
            overlays: raw_gpu::Overlays::NONE,
            surround: false,
            preview: PreviewSource::default(),
            preview_image: None,
            preview_texture: None,
            preview_original: false,
            status: String::new(),
            metadata: Default::default(),
            saved: None,
        }
    }

    /// A cached edited tile must represent saved edits and the resident source.
    pub fn edited_tile_params(&self) -> Option<Params> {
        let image = self.image.as_ref()?;
        if self.scratch
            || self.unsaved_edits()
            || self.luma.is_none()
            || self.luma_params != Some(self.params.luminance)
            || image.decoded.opts != self.params.decode
        {
            return None;
        }
        Some(self.params.effective())
    }

    pub fn unsaved_edits(&self) -> bool {
        self.has_image() && (self.scratch || self.saved.as_ref() != Some(&self.params))
    }

    pub fn needs_quit_warning(&self, warn_duplicates: bool) -> bool {
        self.unsaved_edits() && (!self.scratch || warn_duplicates)
    }

    /// A failed write leaves the saved snapshot unchanged so close/quit can retry.
    pub fn save_sidecar(&mut self) -> Result<(), String> {
        if self.scratch || !self.unsaved_edits() {
            return Ok(());
        }
        let img = self.image.as_ref().unwrap();
        let metadata = match raw_core::sidecar::read(&img.path) {
            raw_core::sidecar::Loaded::Ok(sidecar) => sidecar.metadata,
            raw_core::sidecar::Loaded::Absent => self.metadata.clone(),
            raw_core::sidecar::Loaded::Corrupt(why) => return Err(why),
        };
        raw_core::sidecar::write(&img.path, &self.params, &metadata).map_err(|e| e.to_string())?;
        self.metadata = metadata;
        self.saved = Some(self.params.clone());
        Ok(())
    }

    pub fn has_image(&self) -> bool {
        self.image.is_some()
    }

    /// Put the print loupe up, centred, and take the interaction mode for it.
    ///
    /// **One function because opening it is two facts that must not drift apart.**
    /// `Mode::Loupe` is the source of truth — `print_loupe` closes the window on the
    /// next frame if the mode is anything else — so setting `loupe.open` alone gives a
    /// window that flickers and vanishes. There are three callers now (the eye, `v`,
    /// and the auto-open when a tail slider moves) and that is two more than a
    /// hand-repeated pair survives.
    ///
    /// **`at = None` is the centring**, and it is deliberate that this re-centres every
    /// time rather than resuming where the loupe last sat. A loupe that opens itself
    /// because you moved a slider has to open somewhere predictable; the middle of the
    /// crop is the only place that is true of on a picture it has not been used on yet.
    /// Once it is up, dragging moves it and nothing re-centres it.
    ///
    /// **Refuses to take the mode from another tool.** Crop and the brush edit `Params`
    /// and carry a snapshot to put back; closing one from under a grain slider would
    /// silently end an edit the user is in the middle of. Nudging Density while
    /// painting is a strange thing to do, and the answer to it is to do nothing rather
    /// than to throw away the stroke set.
    ///
    /// **"Is there an image" is the caller's precondition, not this function's**, which
    /// is how `Action::Crop` already reads — `.filter(|t| t.has_image())`. Keeping it
    /// out here is what makes the mode rules above testable at all: a `Tab` with a real
    /// `Image` needs a decoded `SensorImage` behind it, and a guard nothing can reach in
    /// a test is a guard nobody can check.
    pub fn show_loupe(&mut self) {
        if matches!(
            self.mode,
            Mode::Crop { .. } | Mode::Keystone { .. } | Mode::Paint { .. }
        ) {
            return;
        }
        self.loupe.at = None;
        self.loupe.open = true;
        self.loupe.request_module_reveal();
        self.mode = Mode::Loupe;
    }

    /// The working image is absent or belongs to older luminance controls.
    ///
    /// **`luma == None` beside an image means invalid, not absent.** Every path that
    /// swaps a new decode in leaves it that way; naming the state rather than
    /// leaving it implicit is what stops one of those paths forgetting to follow up,
    /// which is exactly how toggling Unity WB blanked the viewport when a duplicate
    /// was open.
    ///
    /// False while an error is set: a tab that failed to derive luminance — an image
    /// too large for the GPU — must not be asked to try again every frame.
    pub fn needs_luma(&self) -> bool {
        self.image.as_ref().is_some_and(|image| {
            image.decoded.opts == self.params.decode
                && self.error.is_none()
                && !self.has_current_luma()
        })
    }

    pub(crate) fn has_current_luma(&self) -> bool {
        self.luma.is_some()
            && self.luma_params == Some(self.params.luminance)
            && self
                .image
                .as_ref()
                .is_some_and(|image| image.decoded.opts == self.params.decode)
    }

    /// Reject work prepared for older controls or a replaced decode.
    pub(crate) fn accepts_luma(
        &self,
        source: &Arc<Decoded>,
        params: raw_core::LuminanceParams,
    ) -> bool {
        self.params.decode == source.opts
            && self.params.luminance == params
            && self
                .image
                .as_ref()
                .is_some_and(|image| Arc::ptr_eq(&image.decoded, source))
    }

    /// The name as it should be *shown*: with the file's extension.
    ///
    /// `name` itself stays the bare stem, because it is identity as well as label —
    /// it is what `dup_name` numbers from, what `unique_name` compares, and the stem
    /// `save_duplicate` builds a path out of. Putting the extension into it would
    /// make `with_extension` on that path replace the real one.
    ///
    /// A tab whose file has not arrived yet has nothing to append, and shows the
    /// stem alone rather than an invented suffix.
    pub fn display_name(&self) -> String {
        match self
            .image
            .as_ref()
            .and_then(|i| i.path.extension())
            .and_then(|e| e.to_str())
        {
            Some(ext) => format!("{}.{ext}", self.name),
            None => self.name.clone(),
        }
    }

    /// What to render with.
    ///
    /// Preview-original keeps decode and luminance — those describe how the file is
    /// being *read*, not how it is being rendered, and changing them would force a
    /// re-decode for a momentary look. This is narrower than the destructive Reset All,
    /// which returns every Develop parameter to its default.
    ///
    /// Then `effective`, which is where module bypass happens. The two are the same
    /// idea at different scales — preview-original bypasses everything at once —
    /// and composing them here means every consumer of the render params gets both
    /// without knowing about either. **Anything that describes what is on screen
    /// should read this rather than `params`**, the histogram included; that is the
    /// difference between a readout and a guess.
    pub fn render_params(&self) -> Params {
        let mut p = if self.preview_original {
            Params {
                decode: self.params.decode,
                luminance: self.params.luminance,
                // **Composition survives preview-original**, and it is the one
                // module here that does. `p` answers "what have my tone decisions
                // done to this frame", and a frame that changes shape and size the
                // moment you press it makes that comparison harder rather than
                // cleaner — you would be judging two different pictures. It is the
                // same reason decode and luminance stay: those describe how the file
                // is being read, and this describes what the picture *is*. The tone
                // chain is what `p` is about, and the tone chain is all it drops.
                composition: self.params.composition.applied(),
                ..Default::default()
            }
        } else {
            self.params.effective()
        };
        // The crop tool renders the full uncropped frame, per the handoff, so parts
        // outside the current crop can be seen and re-grabbed. The stored rectangle
        // is untouched — only its application is suppressed, which is the same shape
        // as module bypass and the reason `uncropped` exists beside `effective`.
        if self.mode.is_crop() || self.mode.is_keystone() {
            p = p.uncropped();
        }
        // Perspective guides are measurements on the unwarped photograph. If the
        // active homography were applied while a handle moved, full correction
        // would map that handle straight back to its fixed target on every frame —
        // the image would move under a cursor the handle could no longer follow.
        // Like Crop suppressing its own cut while it is edited, Keystone shows the
        // uncropped, straightened photograph while the guides are placed; leaving
        // the mode reveals the authored correction in one step.
        if self.mode.is_keystone() {
            p.composition.keystone = raw_core::KeystoneParams::default();
        }
        p
    }

    /// The file's own orientation tag. Upright when no image has arrived yet, which
    /// is what an unread tag has always meant.
    pub fn exif_orientation(&self) -> Orientation {
        self.image
            .as_ref()
            .map_or(Orientation::default(), |i| i.sensor.meta.orientation)
    }

    /// The resolved geometry of what is on screen: frame dims, crop, and the map
    /// back to the stored image.
    ///
    /// **Derived every frame, never stored.** It is a pure function of the working
    /// image's dims, the file's EXIF tag and the render params — all three of which
    /// this tab already has — and a cached copy would be a fourth thing to
    /// invalidate, on the same rota as `luma`, `saved` and the viewport's own change
    /// detection. It is a handful of trig; the frame it is drawn into costs more.
    ///
    /// `None` while there is no working image, which is the same condition that
    /// stops the viewport drawing at all.
    pub fn frame(&self) -> Option<Frame> {
        let luma = self.luma.as_ref()?;
        Some(Frame::resolve(
            luma.output_dims,
            self.exif_orientation(),
            &self.render_params().composition,
        ))
    }

    /// The geometry as **stored**, with the crop applied whether or not the tool is
    /// open.
    ///
    /// The distinction matters for exactly one reason and it is worth the second
    /// method: `frame` goes through `render_params`, which suppresses the crop so
    /// the tool can show the whole picture. Every *readout* went through it too, so
    /// with the tool open the footer, the COMPOSITION line and the export caption
    /// all reported the uncropped frame — 2144 × 2938 for a crop visibly a third
    /// that size, on a panel whose whole job is to tell you what you are about to
    /// print. the maintainer caught it in a screenshot.
    ///
    /// So: `frame` is what is being *drawn*, `stored_frame` is what is being *set*.
    /// Readouts want the second.
    pub fn stored_frame(&self) -> Option<Frame> {
        let luma = self.luma.as_ref()?;
        Some(Frame::resolve(
            luma.output_dims,
            self.exif_orientation(),
            &self.params.effective().composition,
        ))
    }

    /// The dims everything that reports a size reads: the footer, Info's PIPELINE
    /// rows, and the export caption. **After the crop**, because that is the
    /// picture — and after the *stored* crop, because that is the one being set.
    pub fn output_dims(&self) -> Option<raw_core::Dims> {
        Some(self.stored_frame()?.output_dims())
    }

    /// Rotate a quarter turn, resolving "as shot" into a value first.
    ///
    /// The resolution is the whole subtlety: `None` means *whatever the file says*,
    /// so turning from it has to start from the file's tag rather than from zero, or
    /// the first press of `⌘]` on a portrait frame would straighten it to landscape
    /// instead of turning it.
    pub fn rotate(&mut self, clockwise: bool) {
        let exif = self.exif_orientation();
        self.params.composition.turn(clockwise, exif);
    }

    /// Pull the crop back onto real pixels after the angle moved under it.
    ///
    /// **Every route that sets `straighten` has to call this**, and that is the whole
    /// reason it is a method rather than four lines in the COMPOSITION module. It was
    /// four lines in the COMPOSITION module, comparing the angle against a snapshot
    /// taken at the top of the panel body — which works for the slider, sitting inside
    /// that body, and cannot possibly work for the other two. The rotate ring and the
    /// drawn horizon both write the angle from the *viewport*, so by the time the panel
    /// next runs, its "before" reading is already the new value and the change it is
    /// watching for has no frame in which to be visible. the maintainer's report — a crop box
    /// full of empty corners after straightening — was that, and the slider being the
    /// one control that appeared to work is what made it look like a geometry bug.
    ///
    /// `stored_frame`, not `frame`: with the crop tool open `frame` suppresses the
    /// crop, so the fallback below would read the whole picture's aspect instead of
    /// the crop's and straightening would quietly reshape the print.
    ///
    /// **Only when the corners have actually been lost.** Under the hard clamp that is
    /// a stronger statement than it used to be — a crop is *never* allowed to hold an
    /// empty corner, so this is maintaining an invariant rather than offering a
    /// convenience — but the guard is still what stops an unconditional re-fit from
    /// recentring a crop on every frame of a slider drag.
    pub fn confine_crop(&mut self) {
        let Some(f) = self.stored_frame() else { return };
        if f.covers(self.params.composition.crop) {
            return;
        }
        // The locked ratio if there is one, otherwise whatever shape the crop already
        // had: straightening must not also reshape the picture.
        let ratio = f
            .target_ratio(
                self.params.composition.ratio,
                self.params.composition.portrait,
            )
            .or_else(|| Some(f.crop_ratio()));
        self.params.composition.crop =
            crate::crop::confine(self.params.composition.crop, ratio, &f);
    }

    /// Swap in a fresh decode of the same file, keeping the view. Used when a
    /// decode option changed — the frame did not, so the zoom and pan should not
    /// jump back to fit the way they do on a load.
    pub fn decoded(&mut self, decoded: Arc<Decoded>) {
        let Some(img) = &mut self.image else { return };
        self.scene_gen += 1;
        img.decoded = decoded;
        self.luma = None;
        self.luma_params = None;
        self.update_status();
    }

    pub fn update_status(&mut self) {
        let Some(img) = &self.image else { return };
        self.status = format!(
            "{} · {} · decoded in {} ms · {:.2}% clipped photosites",
            img.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            img.decoded.scene.camera,
            img.decoded.decode_ms,
            img.decoded.clipped_fraction * 100.0
        );
    }
}

/// The open tabs, the focus, and the focus history.
pub struct Tabs {
    tabs: Vec<Tab>,
    active: usize,
    /// Focus order, most recent first. Always exactly the live ids. Drives three
    /// things that would otherwise each invent their own rule: which tab the
    /// backtick flicks to, where focus lands when a tab closes, and which tabs stay
    /// warm.
    mru: Vec<TabId>,
    next_id: u64,
}

impl Default for Tabs {
    fn default() -> Self {
        Self::new()
    }
}

impl Tabs {
    pub fn new() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            mru: Vec::new(),
            next_id: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// Whether the tab strip has no documents.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.tabs.len() >= MAX_TABS
    }

    pub fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.tabs.iter_mut()
    }

    /// Apply each tab's original pathname once, including swaps and rename cycles.
    /// Pending loads are returned for the app to cancel and restart.
    pub fn rename_paths(&mut self, events: &[crate::rename::Event]) -> Vec<(TabId, PathBuf)> {
        if events.is_empty() {
            return Vec::new();
        }
        let changes: std::collections::HashMap<_, _> = events
            .iter()
            .map(|event| (event.old.as_path(), event.new.as_path()))
            .collect();
        let mut restart = Vec::new();
        for tab in self.iter_mut() {
            let old = tab
                .image
                .as_ref()
                .map(|image| image.path.as_path())
                .or(tab.opening_path.as_deref());
            let Some(new) = old.and_then(|old| changes.get(old)).copied() else {
                continue;
            };
            if !tab.scratch {
                tab.name = stem_of(new).to_owned();
            }
            if let Some(image) = &mut tab.image {
                image.path = new.to_path_buf();
                tab.status = format!("renamed to {}", new.display());
            } else {
                tab.opening_path = Some(new.to_path_buf());
                tab.status = format!("decoding {}…", new.display());
                restart.push((tab.id, new.to_path_buf()));
            }
        }
        restart
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    pub fn active_id(&self) -> Option<TabId> {
        self.active().map(|t| t.id)
    }

    pub fn by_id_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn index_of(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Open a tab for `path`. `None` at the cap — the caller says so rather than
    /// silently closing someone's work to make room.
    pub fn open(&mut self, path: &Path) -> Option<TabId> {
        if self.is_full() {
            return None;
        }
        let name = self.unique_name(stem_of(path));
        let id = self.push(name);
        self.by_id_mut(id).expect("just pushed").opening_path = Some(path.to_path_buf());
        Some(id)
    }

    /// Clone the active tab: **params copied, not defaults**, and the decoded image
    /// shared rather than decoded again.
    ///
    /// The view is *not* copied. It is view state by the same definition that keeps
    /// it off `Params`, and a duplicate is a second reading of the frame rather
    /// than a second look at one corner of it.
    pub fn duplicate(&mut self) -> Option<TabId> {
        if self.is_full() {
            return None;
        }
        let src = self.active()?;
        let (image, luma, params, luma_gen, luma_params) = (
            src.image.clone(),
            src.luma.clone(),
            src.params.clone(),
            src.luma_gen,
            src.luma_params,
        );
        let name = self.dup_name(&self.active()?.name);

        let id = self.push(name);
        let tab = self.by_id_mut(id).expect("just pushed");
        tab.scratch = true;
        tab.opening_path = None;
        // Deliberately not carrying `saved` or `metadata`: a duplicate owns no
        // sidecar, so it has nothing on disk to be in sync with.
        tab.image = image;
        tab.luma = luma;
        tab.luma_gen = luma_gen;
        tab.luma_params = luma_params;
        tab.params = params;
        tab.status = "duplicate · scratch until saved".into();
        Some(id)
    }

    fn push(&mut self, name: String) -> TabId {
        let id = TabId(self.next_id);
        self.next_id += 1;
        self.tabs.push(Tab::new(id, name));
        self.active = self.tabs.len() - 1;
        self.touch(id);
        id
    }

    /// Close a tab and report which id went, so the caller can cancel its decode
    /// and free its texture registration.
    pub fn close(&mut self, index: usize) -> Option<(TabId, Option<Render>)> {
        if index >= self.tabs.len() {
            return None;
        }
        let mut tab = self.tabs.remove(index);
        self.mru.retain(|i| *i != tab.id);
        // Focus follows the MRU, not the neighbouring index: after closing a tab
        // the user wants the one they were looking at before, which is rarely the
        // one that happens to be next in the strip.
        self.active = self
            .mru
            .first()
            .and_then(|id| self.index_of(*id))
            .unwrap_or_else(|| index.min(self.tabs.len().saturating_sub(1)));
        Some((tab.id, tab.render.take()))
    }

    pub fn focus(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            let id = self.tabs[index].id;
            self.touch(id);
        }
    }

    pub fn focus_id(&mut self, id: TabId) {
        if let Some(i) = self.index_of(id) {
            self.focus(i);
        }
    }

    /// Step through the strip, wrapping. Strip order, not MRU order: this is
    /// "next tab", and a user pressing it repeatedly expects to walk the row.
    pub fn step(&mut self, delta: isize) {
        if self.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        let next = (self.active as isize + delta).rem_euclid(n) as usize;
        self.focus(next);
    }

    /// Flick to the previously focused tab. **This is the comparison mechanism** —
    /// pressing it repeatedly toggles A/B/A/B, which is what makes a tonal
    /// difference visible in a way side-by-side does not.
    pub fn flicker(&mut self) {
        if let Some(id) = self.mru.get(1).copied() {
            self.focus_id(id);
        }
    }

    fn touch(&mut self, id: TabId) {
        self.mru.retain(|i| *i != id);
        self.mru.insert(0, id);
    }

    /// The tabs entitled to hold GPU state: the focused one and the [`WARM`] - 1
    /// most recently focused behind it.
    pub fn warm_ids(&self) -> impl Iterator<Item = TabId> + '_ {
        self.mru.iter().take(WARM).copied()
    }

    pub fn is_warm(&self, id: TabId) -> bool {
        self.warm_ids().any(|i| i == id)
    }

    /// Ids that hold GPU state they are no longer entitled to.
    pub fn cold_with_render(&self) -> Vec<TabId> {
        self.tabs
            .iter()
            .filter(|t| t.render.is_some() && !self.is_warm(t.id))
            .map(|t| t.id)
            .collect()
    }

    // ------------------------------------------------------------------- naming

    fn taken(&self) -> Vec<&str> {
        self.tabs.iter().map(|t| t.name.as_str()).collect()
    }

    /// `image_1234`, then `image_1234 (2)` if that file is already open.
    ///
    /// Distinct from the `_dup(N)` suffix on purpose: opening the same file twice
    /// and duplicating a tab are different acts, and reading `_dup` on a tab that
    /// was never duplicated would misreport which one owns the sidecar.
    fn unique_name(&self, stem: &str) -> String {
        let taken = self.taken();
        if !taken.contains(&stem) {
            return stem.to_owned();
        }
        (2..)
            .map(|n| format!("{stem} ({n})"))
            .find(|c| !taken.contains(&c.as_str()))
            .expect("the cap bounds this")
    }

    /// `image_1234_dup1`, `_dup2`, …
    ///
    /// Numbered from the **base**, so duplicating a duplicate gives `_dup2` rather
    /// than `_dup1_dup1`.
    ///
    /// **The first duplicate is `_dup1`, not `_dup2`.** The old scheme counted the
    /// original as number one and so began at two, which is defensible in the
    /// abstract and wrong in the hand: nobody looking at `image_dup2` next to
    /// `image` reads "the second of two", they read "where is dup1". The number
    /// counts duplicates, and the original is not one of them. The parentheses went
    /// with it — these become filenames when a duplicate is saved, and brackets in
    /// a filename are an escaping problem in every shell that will ever touch it.
    fn dup_name(&self, name: &str) -> String {
        let base = base_of(name);
        let taken = self.taken();
        (1..)
            .map(|n| format!("{base}_dup{n}"))
            .find(|c| !taken.contains(&c.as_str()))
            .expect("the cap bounds this")
    }
}

/// Strip a trailing `_dup<N>`.
pub fn base_of(name: &str) -> &str {
    let Some(open) = name.rfind("_dup") else {
        return name;
    };
    let digits = &name[open + 4..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        &name[..open]
    } else {
        name
    }
}

pub fn stem_of(path: &Path) -> &str {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn showing_the_loupe_takes_the_mode_and_centres_it() {
        // The two facts that must not drift apart. `Mode::Loupe` is the source of
        // truth — `print_loupe` closes the window on the next frame if the mode is
        // anything else — so a caller that set only the flag would put up a window that
        // vanishes on the frame after. Three callers share this: the eye, `v`, and the
        // auto-open when a tail slider moves.
        let mut t = Tab::placeholder();
        t.loupe.at = Some((123.0, 456.0));
        t.show_loupe();
        assert!(t.loupe.open, "the window did not open");
        assert!(t.mode.is_loupe(), "the mode did not follow the window");
        assert_eq!(t.loupe.at, None, "opening did not re-centre the sample");
        assert!(
            t.loupe.take_module_reveal(),
            "opening did not ask the Develop module to expand"
        );
        assert!(
            !t.loupe.take_module_reveal(),
            "the one-shot module reveal request was not consumed"
        );
    }

    #[test]
    fn the_loupe_will_not_take_the_mode_from_a_tool() {
        // Crop and the brush edit `Params` and carry a snapshot to put back, so a grain
        // slider nudged mid-stroke must not end the stroke set. Doing nothing is the
        // right answer to a strange combination; throwing away an edit is not.
        for mode in [
            Mode::crop(raw_core::CompositionParams::default()),
            Mode::keystone(raw_core::CompositionParams::default()),
            Mode::paint(
                raw_core::Sign::Dodge,
                crate::paint::Tool::default(),
                raw_core::DodgeBurnParams::default(),
            ),
        ] {
            let mut t = Tab::placeholder();
            t.mode = mode;
            t.show_loupe();
            assert!(!t.loupe.open, "the loupe opened over a tool");
            assert!(!t.mode.is_loupe(), "the loupe took a tool's mode");
        }
    }

    #[test]
    fn re_showing_an_open_loupe_re_centres_it() {
        // Which is why the auto-open is guarded on `!loupe.open` at its call site: this
        // function always centres, and centring under a drag would yank the sample away
        // from whatever the user had gone to look at.
        let mut t = Tab::placeholder();
        t.show_loupe();
        t.loupe.at = Some((10.0, 20.0));
        t.show_loupe();
        assert_eq!(t.loupe.at, None, "the contract this test names has changed");
    }

    #[test]
    fn the_grid_is_strips_at_two_and_three_and_a_square_at_four() {
        // the maintainer's shapes, as an assertion rather than as a comment: two side by side,
        // three as vertical strips, four as a square. Written as (rows, columns), so a
        // transposed table — three stacked letterbox slots instead of three strips —
        // fails here rather than on screen.
        assert_eq!(Compare::grid(2), (1, 2), "2-up is not side by side");
        assert_eq!(
            Compare::grid(3),
            (1, 3),
            "3-up is not three vertical strips"
        );
        assert_eq!(Compare::grid(4), (2, 2), "4-up is not a square");
        // And every cell has somewhere to go, at every count the grid can hold.
        for n in 1..=crate::snapshot::Snapshots::MAX_CELLS {
            let (r, c) = Compare::grid(n);
            assert!(r * c >= n, "{n} cells do not fit a {r}x{c} grid");
        }
    }

    #[test]
    fn sparse_comparisons_keep_the_requested_layout() {
        assert_eq!(Compare::counts(2, 2), (2, 2));
        assert_eq!(Compare::counts(3, 2), (2, 3));
        assert_eq!(Compare::counts(4, 2), (2, 4));
    }

    #[test]
    fn the_number_run_can_leave_and_reopen_comparison() {
        let mut compare = Compare {
            open: true,
            n_up: 2,
            zoom: 3.0,
            pan: (0.2, -0.2),
        };
        assert!(compare.select_layout(1, 2));
        assert!(!compare.open);

        assert!(compare.select_layout(4, 2));
        assert!(compare.open);
        assert_eq!(compare.n_up, 4);
        assert_eq!(compare.zoom, 1.0);
        assert_eq!(compare.pan, (0.0, 0.0));
    }

    #[test]
    fn a_fitted_grid_cannot_be_panned_at_all() {
        // The boundary case, and the one that matters: at fit every cell shows its
        // whole picture, so any pan slides them out of agreement with each other —
        // which is the single thing a synced grid exists to provide.
        assert_eq!(
            Compare::pan_room(1.0),
            0.0,
            "a fitted grid could be dragged around"
        );
        assert_eq!(Compare::pan_room(0.5), 0.0, "below fit is still fit");
        // And past it the room grows toward half a picture, never reaching it: at zoom
        // z a cell shows 1/z, so the centre travels 0.5 - 0.5/z.
        assert!((Compare::pan_room(2.0) - 0.25).abs() < 1e-6);
        assert!((Compare::pan_room(4.0) - 0.375).abs() < 1e-6);
        assert!(
            Compare::pan_room(Compare::MAX_ZOOM) < 0.5,
            "an edge came inside the tile"
        );
    }

    fn with_tabs(n: usize) -> Tabs {
        let mut t = Tabs::new();
        for i in 0..n {
            t.open(Path::new(&format!("/raws/image_{i:04}.dng")))
                .expect("under the cap");
        }
        t
    }

    #[test]
    fn a_duplicate_copies_params_not_defaults() {
        // The handoff is explicit, and it is the whole reason duplicate exists: a
        // duplicate that reset the develop settings would be a second *open*, not a
        // second version.
        let mut tabs = with_tabs(1);
        tabs.active_mut().expect("one tab").params.exposure.ev = 1.75;
        tabs.duplicate().expect("under the cap");
        assert_eq!(
            tabs.active().expect("the duplicate").params.exposure.ev,
            1.75
        );
    }

    #[test]
    fn surround_is_per_tab_and_a_new_view_starts_without_it() {
        let mut tabs = with_tabs(1);
        let first = tabs.active_id().expect("first");
        tabs.active_mut().expect("first").surround = true;

        tabs.open(Path::new("/raws/second.dng")).expect("second");
        assert!(!tabs.active().expect("second").surround);

        tabs.focus_id(first);
        assert!(tabs.active().expect("first again").surround);

        tabs.duplicate().expect("duplicate");
        assert!(!tabs.active().expect("duplicate").surround);
    }

    #[test]
    fn a_duplicate_shares_the_decode_rather_than_repeating_it() {
        // Params is a value and the image is behind Arcs, so duplication is
        // pointers. If this ever needs a decode, something started owning mutable
        // per-tab pipeline state.
        let mut tabs = with_tabs(1);
        let luma = Arc::new(LumaImage {
            data: vec![0.5; 16],
            output_dims: raw_core::Dims { w: 4, h: 4 },
            source_dims: raw_core::Dims { w: 8, h: 8 },
            clipped: Vec::new(),
        });
        tabs.active_mut().expect("one tab").luma = Some(Arc::clone(&luma));
        tabs.duplicate().expect("under the cap");

        assert_eq!(
            Arc::strong_count(&luma),
            3,
            "the duplicate copied the working image"
        );
        let a = tabs
            .iter()
            .next()
            .expect("original")
            .luma
            .as_ref()
            .expect("luma");
        let b = tabs
            .active()
            .expect("duplicate")
            .luma
            .as_ref()
            .expect("luma");
        assert!(Arc::ptr_eq(a, b));
    }

    #[test]
    fn a_duplicate_does_not_inherit_the_view() {
        let mut tabs = with_tabs(1);
        let v = &mut tabs.active_mut().expect("one tab").view;
        v.fit = false;
        v.scale = 4.0;
        tabs.duplicate().expect("under the cap");
        let view = &tabs.active().expect("the duplicate").view;
        assert!(view.fit, "the duplicate opened zoomed into a corner");
    }

    #[test]
    fn duplicates_number_from_the_base() {
        let mut tabs = Tabs::new();
        tabs.open(Path::new("/raws/image_1234.dng")).expect("first");
        tabs.duplicate().expect("second");
        // The FIRST duplicate is 1. The number counts duplicates, and the original
        // is not one of them.
        assert_eq!(tabs.active().expect("dup").name, "image_1234_dup1");
        // Duplicating the duplicate must not nest.
        tabs.duplicate().expect("third");
        assert_eq!(tabs.active().expect("dup").name, "image_1234_dup2");
        // And duplicating the original again fills the next free number.
        tabs.focus(0);
        tabs.duplicate().expect("fourth");
        assert_eq!(tabs.active().expect("dup").name, "image_1234_dup3");
    }

    #[test]
    fn the_display_name_carries_the_extension_but_the_identity_does_not() {
        // Two facts that have to stay apart. The strip and the footer show
        // `frame.dng`, because the extension is part of a filename and without it
        // `frame.dng` and `frame.tif` are two identical-looking tabs. But `name`
        // stays the bare stem, because it is also identity: `dup_name` numbers from
        // it and `save_duplicate` builds a path from it with `with_extension`, which
        // would *replace* a real extension that had been folded in.
        let mut tabs = Tabs::new();
        tabs.open(Path::new("/raws/frame_0001.dng")).expect("first");
        let t = tabs.active().expect("one tab");
        assert_eq!(t.name, "frame_0001", "identity must stay the stem");
        // No image has been decoded yet, so there is no extension to show.
        assert_eq!(t.display_name(), "frame_0001");

        tabs.duplicate().expect("second");
        assert_eq!(tabs.active().expect("dup").name, "frame_0001_dup1");
    }

    #[test]
    fn base_of_only_strips_a_real_suffix() {
        assert_eq!(base_of("image_1234"), "image_1234");
        assert_eq!(base_of("image_1234_dup1"), "image_1234");
        assert_eq!(base_of("image_1234_dup12"), "image_1234");
        // Not a suffix this app made; leave it alone rather than eat a real name.
        assert_eq!(base_of("image_dupx"), "image_dupx");
        assert_eq!(base_of("image_dup"), "image_dup");
        assert_eq!(base_of("_dup2 at the front"), "_dup2 at the front");
        // A name that merely ends in digits is not a duplicate suffix.
        assert_eq!(base_of("image_1234"), "image_1234");
        assert_eq!(base_of("dup3"), "dup3", "no underscore, so not our suffix");
    }

    #[test]
    fn opening_one_file_twice_is_not_a_duplicate() {
        // `_dup` means "cloned from a tab" and decides who owns the sidecar. A
        // second open of the same file is its own tab, and must not claim to be a
        // duplicate of anything.
        let mut tabs = Tabs::new();
        tabs.open(Path::new("/raws/image_1234.dng")).expect("first");
        tabs.open(Path::new("/elsewhere/image_1234.dng"))
            .expect("second");
        assert_eq!(tabs.active().expect("second").name, "image_1234 (2)");
        assert!(!tabs.active().expect("second").scratch);
    }

    #[test]
    fn a_duplicate_is_scratch_and_the_original_is_not() {
        let mut tabs = with_tabs(1);
        tabs.duplicate().expect("under the cap");
        assert!(
            tabs.active().expect("dup").scratch,
            "the duplicate must not own the sidecar"
        );
        assert!(!tabs.iter().next().expect("original").scratch);
    }

    #[test]
    fn the_cap_is_eight_and_refuses_rather_than_evicting() {
        let mut tabs = with_tabs(MAX_TABS);
        assert!(tabs.is_full());
        assert!(tabs.open(Path::new("/raws/one_more.dng")).is_none());
        assert!(tabs.duplicate().is_none());
        assert_eq!(
            tabs.len(),
            MAX_TABS,
            "the cap closed someone's work to make room"
        );
    }

    #[test]
    fn closing_returns_to_the_previously_viewed_tab() {
        // Not the neighbouring index. After closing, the tab a user wants is the
        // one they were on before, which is rarely the next one along the strip.
        let mut tabs = with_tabs(4);
        tabs.focus(0);
        tabs.focus(3);
        tabs.focus(1);
        let id3 = tabs.iter().nth(3).expect("fourth").id;
        tabs.close(1).expect("closed");
        assert_eq!(tabs.active().expect("focus survived").id, id3);
    }

    #[test]
    fn closing_the_last_tab_leaves_no_focus_dangling() {
        let mut tabs = with_tabs(1);
        tabs.close(0).expect("closed");
        assert!(tabs.is_empty());
        assert!(tabs.active().is_none());
        // And the next open must still land somewhere valid.
        tabs.open(Path::new("/raws/after.dng")).expect("reopen");
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn the_backtick_flicks_between_two_tabs_not_around_the_strip() {
        // Flicker comparison: press it repeatedly and you get A/B/A/B. Walking the
        // strip instead would make a three-tab session unable to compare a pair.
        let mut tabs = with_tabs(3);
        tabs.focus(0);
        tabs.focus(2);
        let (a, b) = (
            tabs.iter().nth(2).expect("c").id,
            tabs.iter().next().expect("a").id,
        );
        tabs.flicker();
        assert_eq!(tabs.active_id(), Some(b));
        tabs.flicker();
        assert_eq!(tabs.active_id(), Some(a));
        tabs.flicker();
        assert_eq!(tabs.active_id(), Some(b), "the third press left the pair");
    }

    #[test]
    fn the_zoom_ladder_is_ordered_and_holds_the_percentages_that_matter() {
        // The rungs a photographer already thinks in. 100% has to be exactly 1.0 or
        // `z`'s 100%-toggle and the ladder disagree about where 1:1 is.
        assert!(
            ZOOM_STEPS.windows(2).all(|w| w[0] < w[1]),
            "the ladder is not ordered"
        );
        for want in [0.125, 0.25, 0.5, 1.0, 2.0, 4.0] {
            assert!(
                ZOOM_STEPS.contains(&want),
                "{want} is missing from the ladder"
            );
        }
    }

    #[test]
    fn zooming_steps_to_the_next_rung_rather_than_multiplying() {
        // the maintainer: the keys "jump to even numbers". They multiplied by 1.25, so from a fit
        // of 12.8% they walked 16.0, 20.0, 25.0, 31.2, 39.1 — never landing on a third
        // or a half.
        assert_eq!(
            zoom_in_from(0.128),
            0.1667,
            "the first press off fit must reach 1/6"
        );
        assert_eq!(zoom_in_from(0.1667), 0.25);
        assert_eq!(zoom_out_from(1.0), 0.6667);
        assert_eq!(zoom_out_from(0.6667), 0.5);
    }

    #[test]
    fn a_scale_already_on_a_rung_still_moves() {
        // The reason for the tolerance: at exactly 50%, "the first rung above 0.5" is
        // 0.5 itself under a naive comparison, and the key would do nothing.
        assert!(zoom_in_from(0.5) > 0.5, "cmd-+ at 50% did nothing");
        assert!(zoom_out_from(0.5) < 0.5, "cmd-- at 50% did nothing");
        // And the same for a value arrived at by floating-point arithmetic rather than
        // typed in, which is how every scale in the app is reached.
        let fifty = 100.0f32 / 200.0;
        assert!(zoom_in_from(fifty) > fifty);
    }

    #[test]
    fn the_ladder_ends_rather_than_wrapping() {
        // Past the top, stay at the top. Wrapping to 6% because you pressed cmd-+ once
        // too often would be the worst possible response to that gesture.
        let top = ZOOM_STEPS[ZOOM_STEPS.len() - 1];
        assert_eq!(zoom_in_from(top), top);
        assert_eq!(zoom_in_from(top * 4.0), top);
        assert_eq!(zoom_out_from(ZOOM_STEPS[0]), ZOOM_STEPS[0]);
        assert_eq!(zoom_out_from(0.0001), ZOOM_STEPS[0]);
    }

    #[test]
    fn the_zoom_readout_agrees_with_the_ladder() {
        // The footer rounded to whole percent, so a 12.5% rung read `13%` and a 16.67%
        // one read `17%` — numbers that are not on the ladder and are not what the view
        // is doing.
        assert_eq!(zoom_label(0.125), "12.5%");
        assert_eq!(zoom_label(0.1667), "16.7%");
        assert_eq!(zoom_label(0.5), "50%");
        assert_eq!(zoom_label(1.0), "100%");
        assert_eq!(zoom_label(4.0), "400%");
    }

    #[test]
    fn stepping_walks_the_strip_and_wraps() {
        let mut tabs = with_tabs(3);
        tabs.focus(0);
        tabs.step(-1);
        assert_eq!(
            tabs.active_index(),
            2,
            "stepping back from the first must wrap"
        );
        tabs.step(1);
        assert_eq!(tabs.active_index(), 0);
    }

    #[test]
    fn only_the_warm_set_keeps_gpu_state() {
        // The memory bound. Eight open tabs must not mean eight working images
        // resident on the GPU — in DirectMosaic on a 100 MP frame that is 400 MB
        // each.
        let mut tabs = with_tabs(5);
        for i in 0..5 {
            tabs.focus(i);
        }
        assert_eq!(tabs.warm_ids().count(), WARM);
        // Focused, plus the one before it.
        assert!(tabs.is_warm(tabs.active_id().expect("active")));
        assert!(tabs.is_warm(tabs.iter().nth(3).expect("previous").id));
        assert!(!tabs.is_warm(tabs.iter().next().expect("oldest").id));
    }

    #[test]
    fn the_flicker_pair_is_exactly_the_warm_set() {
        // These are one decision, not two: the tab the backtick reaches is the tab
        // whose GPU state was kept, so the comparison the app is built around never
        // pays a re-upload.
        let mut tabs = with_tabs(4);
        for i in 0..4 {
            tabs.focus(i);
        }
        let warm: Vec<TabId> = tabs.warm_ids().collect();
        tabs.flicker();
        assert!(warm.contains(&tabs.active_id().expect("active")));
        tabs.flicker();
        assert!(warm.contains(&tabs.active_id().expect("active")));
    }

    #[test]
    fn ids_are_not_reused_after_a_close() {
        // A decode in flight for a closed tab must not be delivered to a new tab
        // that happens to have taken its place.
        let mut tabs = with_tabs(2);
        let first = tabs.iter().next().expect("first").id;
        tabs.close(0).expect("closed");
        tabs.open(Path::new("/raws/new.dng")).expect("opened");
        assert_ne!(tabs.active_id(), Some(first), "a TabId was reused");
    }

    /// A tab carrying a decoded image, with the smallest sensor that will build.
    fn tab_with_image(tabs: &mut Tabs) -> TabId {
        tab_with_orientation(tabs, Orientation::Rotate0)
    }

    /// The same, for a file whose EXIF says it was shot the other way up.
    fn tab_with_orientation(tabs: &mut Tabs, orientation: Orientation) -> TabId {
        use raw_core::geometry::CfaColor::{Blue, Green, Red};
        use raw_core::sensor::{BlackPattern, Gains, Metadata, SensorImage};
        use raw_core::{CfaGeometry, Dims};

        let geom = CfaGeometry::new(
            2,
            Dims { w: 2, h: 2 },
            0,
            0,
            2,
            2,
            [[Red, Green], [Green, Blue]],
        );
        let sensor = SensorImage {
            data: vec![0u16; 4],
            geom: geom.clone(),
            black: BlackPattern {
                levels: vec![0.0],
                w: 1,
                h: 1,
            },
            white: [1024.0; 3],
            wb_coeffs: [1.0, 1.0, 1.0],
            camera: "test".into(),
            meta: Metadata {
                orientation,
                ..Metadata::default()
            },
        };
        let decoded = std::sync::Arc::new(crate::decode::Decoded {
            opts: Default::default(),
            scene: raw_core::scene::SceneImage {
                data: vec![0.5; 4],
                geom,
                gains: Gains([1.0, 1.0, 1.0]),
                camera: "test".into(),
                clipped: Vec::new(),
            },
            clipped_fraction: 0.0,
            decode_ms: 0,
        });
        let id = tabs.open(Path::new("/raws/t.dng")).expect("open");
        // Use a real, read-only fixture available on every platform.
        let key = crate::decode::ContentKey::of(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("key");
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.image = Some(Image {
            sensor: std::sync::Arc::new(sensor),
            decoded: decoded.clone(),
            path: "/raws/t.dng".into(),
            key,
        });
        tab.luma = Some(std::sync::Arc::new(raw_core::LumaImage {
            data: vec![0.5],
            output_dims: Dims { w: 1, h: 1 },
            source_dims: Dims { w: 2, h: 2 },
            clipped: Vec::new(),
        }));
        tab.luma_params = Some(tab.params.luminance);
        id
    }

    #[test]
    fn failed_sidecar_saves_keep_edits_unsaved_until_a_successful_retry() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).unwrap();
        let dir = crate::settings::dir().unwrap().join("unavailable-folder");
        tab.image.as_mut().unwrap().path = dir.join("photo.dng");
        tab.saved = Some(tab.params.clone());
        tab.params.exposure.ev = 1.25;
        assert!(tab.save_sidecar().is_err());
        assert!(tab.unsaved_edits());
        assert!(
            tab.needs_quit_warning(false),
            "duplicate preferences must not hide failed ordinary saves"
        );
        assert_eq!(tab.saved.as_ref().unwrap().exposure.ev, 0.0);
        std::fs::create_dir(&dir).unwrap();
        tab.save_sidecar().unwrap();
        assert!(!tab.unsaved_edits());
        assert_eq!(
            raw_core::sidecar::read(&dir.join("photo.dng"))
                .ok()
                .unwrap()
                .params
                .exposure
                .ev,
            1.25
        );
    }

    #[test]
    fn duplicate_warnings_only_control_scratch_tabs() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).unwrap();
        tab.scratch = true;
        assert!(tab.needs_quit_warning(true));
        assert!(!tab.needs_quit_warning(false));
        tab.scratch = false;
        assert!(tab.needs_quit_warning(false));
        tab.saved = Some(tab.params.clone());
        assert!(!tab.needs_quit_warning(true));
    }

    #[test]
    fn an_unreadable_sidecar_keeps_the_tab_dirty_and_preserves_the_file() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).unwrap();
        let image = crate::settings::dir().unwrap().join("photo.dng");
        tab.image.as_mut().unwrap().path = image.clone();
        tab.params.exposure.ev = 2.0;
        let path = raw_core::sidecar::path_for(&image);
        std::fs::write(&path, b"unreadable edits").unwrap();
        assert!(tab.save_sidecar().is_err());
        assert!(tab.unsaved_edits());
        assert_eq!(std::fs::read(&path).unwrap(), b"unreadable edits");
    }

    #[test]
    fn edited_tiles_require_saved_params_and_matching_luminance_inputs() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).unwrap();
        tab.saved = Some(tab.params.clone());
        tab.luma_params = Some(tab.params.luminance);
        tab.preview_original = true;
        assert_eq!(tab.edited_tile_params(), Some(tab.params.effective()));
        tab.params.exposure.ev = 1.0;
        assert!(tab.edited_tile_params().is_none());
        tab.saved = Some(tab.params.clone());
        tab.luma_params = None;
        assert!(tab.edited_tile_params().is_none());
        tab.luma_params = Some(tab.params.luminance);
        assert!(tab.edited_tile_params().is_some());
        tab.params.decode.unity_wb = !tab.params.decode.unity_wb;
        tab.saved = Some(tab.params.clone());
        assert!(tab.edited_tile_params().is_none());
    }

    #[test]
    fn a_rename_cycle_updates_loaded_scratch_and_pending_tabs_once() {
        let mut tabs = Tabs::new();
        let a = tab_with_image(&mut tabs);
        tabs.by_id_mut(a).unwrap().image.as_mut().unwrap().path = "/raws/A.dng".into();
        let b = tab_with_image(&mut tabs);
        tabs.by_id_mut(b).unwrap().image.as_mut().unwrap().path = "/raws/B.dng".into();
        let scratch = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(scratch).unwrap();
        tab.image.as_mut().unwrap().path = "/raws/A.dng".into();
        tab.scratch = true;
        tab.name = "my duplicate".into();
        let pending = tabs.open(Path::new("/raws/C.dng")).unwrap();
        let events: Vec<_> = [("A", "B"), ("B", "C"), ("C", "A")]
            .into_iter()
            .map(|(old, new)| crate::rename::Event {
                old: format!("/raws/{old}.dng").into(),
                new: format!("/raws/{new}.dng").into(),
            })
            .collect();
        assert_eq!(
            tabs.rename_paths(&events),
            vec![(pending, "/raws/A.dng".into())]
        );
        for (id, expected) in [(a, "B"), (b, "C"), (scratch, "B")] {
            assert_eq!(
                tabs.by_id_mut(id).unwrap().image.as_ref().unwrap().path,
                PathBuf::from(format!("/raws/{expected}.dng"))
            );
        }
        assert_eq!(tabs.by_id_mut(scratch).unwrap().name, "my duplicate");
        assert_eq!(
            tabs.by_id_mut(pending).unwrap().opening_path.as_deref(),
            Some(Path::new("/raws/A.dng"))
        );
    }

    #[test]
    fn swapping_a_decode_in_marks_the_working_image_stale() {
        // The regression. `decoded` invalidates the working image, and every caller
        // must re-derive it — the cache-hit redecode did not, so toggling a decode
        // option with a duplicate open left the viewport with nothing to draw and
        // no error to explain it.
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        assert!(
            !tab.needs_luma(),
            "a freshly built tab should be renderable"
        );

        let next = tab.image.as_ref().expect("image").decoded.clone();
        tab.decoded(next);
        assert!(
            tab.needs_luma(),
            "a swapped-in decode left the tab claiming to be current"
        );
    }

    #[test]
    fn rotating_starts_from_the_file_rather_than_from_zero() {
        // "As shot" is not an angle, it is the absence of a decision — so the first
        // press of a rotate key has to resolve the EXIF tag before turning from it.
        // Starting from `Rotate0` would straighten a portrait frame to landscape on
        // the first press, which reads as the key rotating the wrong way.
        // The corpus case: a file the camera tagged `Rotate 270 CW`.
        let mut tabs = Tabs::new();
        let id = tab_with_orientation(&mut tabs, Orientation::Rotate270);
        let tab = tabs.by_id_mut(id).expect("tab");

        assert_eq!(
            tab.params.composition.orientation, None,
            "untouched is as-shot"
        );
        tab.rotate(true);
        assert_eq!(
            tab.params.composition.orientation,
            Some(Orientation::Rotate0),
            "a turn from 270 CW should land upright, not at 90"
        );
        tab.rotate(false);
        assert_eq!(
            tab.params.composition.orientation,
            Some(Orientation::Rotate270)
        );
    }

    #[test]
    fn the_crop_turns_with_the_picture() {
        // The frame transposes under a quarter turn and the crop is normalised to
        // it, so leaving the fractions alone would rotate the image and leave the
        // crop pointing somewhere else — you would turn a portrait and find you had
        // cropped the sky. Transposing x and y is the mirror of the right answer and
        // looks correct on a centred crop, which is how it would survive a glance.
        use raw_core::composition::Rect;
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");

        // A crop in the TOP-LEFT of the frame.
        tab.params.composition.crop = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.25,
            h: 0.5,
        };
        tab.rotate(true);
        // Turned clockwise, the top-left of the picture is now the top-right.
        let c = tab.params.composition.crop;
        assert!(
            (c.x - 0.5).abs() < 1e-5 && (c.y - 0.0).abs() < 1e-5,
            "{c:?}"
        );
        assert!(
            (c.w - 0.5).abs() < 1e-5 && (c.h - 0.25).abs() < 1e-5,
            "the crop did not transpose"
        );

        // Four turns is the identity, which no mirrored formula satisfies.
        for _ in 0..3 {
            tab.rotate(true);
        }
        let c = tab.params.composition.crop;
        assert!(
            c.x.abs() < 1e-4 && c.y.abs() < 1e-4,
            "four turns moved the crop: {c:?}"
        );
        assert!(
            (c.w - 0.25).abs() < 1e-4 && (c.h - 0.5).abs() < 1e-4,
            "{c:?}"
        );
    }

    #[test]
    fn a_ratio_stands_on_its_end_when_the_picture_turns() {
        // The `↕` flag, not a different entry in the list: a locked 3:2 turned on
        // its side is still 3:2, stood up. `Freehand` has no shape to turn and
        // `Original` is the picture's own aspect, which transposed with it.
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.params.composition.ratio = raw_core::Ratio::Fixed(1.5);
        assert!(!tab.params.composition.portrait);
        tab.rotate(true);
        assert!(
            tab.params.composition.portrait,
            "a quarter turn did not stand the ratio up"
        );
        assert_eq!(
            tab.params.composition.ratio,
            raw_core::Ratio::Fixed(1.5),
            "the entry moved"
        );
        tab.rotate(false);
        assert!(!tab.params.composition.portrait);

        // Freehand has nothing to turn.
        tab.params.composition.ratio = raw_core::Ratio::Free;
        tab.rotate(true);
        assert!(!tab.params.composition.portrait);
    }

    #[test]
    fn the_crop_tool_suppresses_the_crop_without_touching_the_stored_one() {
        // The handoff's Capture One behaviour, at the seam where the app decides
        // what to render: the values stay on params and only their application is
        // suppressed, so the handles still have somewhere to be drawn.
        use raw_core::composition::Rect;
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.params.composition.crop = Rect {
            x: 0.2,
            y: 0.2,
            w: 0.4,
            h: 0.4,
        };

        assert!(
            !tab.render_params().composition.crop.is_full(),
            "the crop should apply normally"
        );
        tab.mode = Mode::crop(tab.params.composition);
        assert!(
            tab.render_params().composition.crop.is_full(),
            "the tool did not show the frame"
        );
        assert_eq!(
            tab.params.composition.crop.w, 0.4,
            "the stored rectangle was destroyed"
        );
    }

    #[test]
    fn the_keystone_tool_shows_the_unwarped_frame_without_losing_the_authored_fit() {
        use raw_core::{KeystoneMode, KeystoneParams, Point, Rect};
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.params.composition.crop = Rect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        };
        tab.params.composition.keystone = KeystoneParams {
            mode: KeystoneMode::Rectangle,
            guides: [
                Point::new(0.24, 0.18),
                Point::new(0.76, 0.22),
                Point::new(0.82, 0.81),
                Point::new(0.18, 0.78),
            ],
            correction: 1.0,
            ..Default::default()
        };
        let entered = tab.params.composition;

        assert!(tab.render_params().composition.keystone.is_active());
        tab.mode = Mode::keystone(entered);
        let shown = tab.render_params().composition;
        assert_eq!(shown.keystone.mode, KeystoneMode::Off);
        assert!(
            shown.crop.is_full(),
            "the existing crop hid guide territory"
        );
        assert_eq!(
            tab.params.composition, entered,
            "opening the guide tool destroyed authored geometry"
        );
        assert_eq!(
            tab.mode.cancelled(),
            Some(entered),
            "Esc could not restore the geometry from before the gesture"
        );
    }

    #[test]
    fn composition_survives_preview_original() {
        // The one module `p` keeps, and deliberately. `p` answers "what have my tone
        // decisions done to this frame", and a frame that changes shape and size the
        // moment you press it makes that comparison harder rather than cleaner.
        use raw_core::composition::Rect;
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.params.composition.crop = Rect {
            x: 0.2,
            y: 0.2,
            w: 0.4,
            h: 0.4,
        };
        tab.params.composition.straighten = 3.0;
        tab.params.exposure.ev = 2.0;

        tab.preview_original = true;
        let p = tab.render_params();
        assert_eq!(
            p.exposure.ev, 0.0,
            "preview-original must drop the tone chain"
        );
        assert_eq!(
            p.composition.straighten, 3.0,
            "the frame changed shape under p"
        );
        assert_eq!(p.composition.crop.w, 0.4);
    }

    #[test]
    fn escape_puts_back_the_composition_the_crop_tool_opened_over() {
        // the maintainer, on the first version: "esc key commits crop rather than exiting
        // without cropping." The one key every application uses to mean "I did not
        // want that" was the key that kept it — and a tool you cannot back out of is
        // a tool you hesitate to open.
        //
        // The snapshot lives on the mode rather than on the tab, so it cannot outlive
        // what needs it and there is no second place to forget to take it.
        use raw_core::composition::Rect;
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.params.composition.crop = Rect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        };
        tab.params.composition.straighten = 1.0;
        let before = tab.params.composition;

        tab.mode = Mode::crop(tab.params.composition);
        // Everything the tool can change, changed.
        tab.params.composition.crop = Rect {
            x: 0.3,
            y: 0.3,
            w: 0.2,
            h: 0.2,
        };
        tab.params.composition.straighten = 9.5;
        tab.params.composition.ratio = raw_core::Ratio::Fixed(1.0);

        let restored = tab.mode.cancelled().expect("crop mode remembers");
        assert_eq!(
            restored, before,
            "cancelling did not put the composition back"
        );

        // ...and leaving by any other route keeps the edit, which is the whole
        // reason the two exits are different keys.
        assert_eq!(Mode::View.cancelled(), None);
    }

    #[test]
    fn the_mode_is_not_inherited_by_a_duplicate() {
        // View state by the same definition that keeps it off `Params`. A duplicate
        // opening mid-crop-gesture would be a second tab in a state nobody put it in.
        let mut tabs = Tabs::new();
        let _ = tab_with_image(&mut tabs);
        let c = tabs.active().expect("tab").params.composition;
        tabs.active_mut().expect("tab").mode = Mode::crop(c);
        tabs.duplicate().expect("under the cap");
        assert_eq!(tabs.active().expect("dup").mode, Mode::View);
    }

    #[test]
    fn every_mode_but_the_default_says_what_it_is() {
        // The affordance that makes a mode survivable: it is invisible until it
        // surprises you, so it reports itself in the footer and names its exit.
        assert_eq!(Mode::View.hint(), None, "panning is not a mode to announce");
        let hint = Mode::crop(raw_core::CompositionParams::default())
            .hint()
            .expect("crop must announce itself");
        // Both ways out, because they do different things and only one is guessable.
        assert!(hint.contains("Esc"), "a mode must name its way out: {hint}");
        assert!(
            hint.contains("Return"),
            "a mode must name how to apply: {hint}"
        );
    }

    #[test]
    fn a_tab_that_failed_to_derive_luminance_is_not_asked_again() {
        // Without this the retry runs a full-frame CPU luminance pass every frame,
        // forever, on exactly the images that are too big to afford it.
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).expect("tab");
        tab.luma = None;
        assert!(tab.needs_luma());
        tab.error = Some("exceeds this GPU's texture limit".into());
        assert!(!tab.needs_luma(), "an errored tab would retry every frame");
    }

    #[test]
    fn changed_luminance_controls_request_a_new_image() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let tab = tabs.by_id_mut(id).unwrap();
        tab.luma_params = Some(tab.params.luminance);
        assert!(!tab.needs_luma());
        tab.params.luminance.weighting = raw_core::Weighting::Blue;
        assert!(tab.needs_luma());
    }

    #[test]
    fn luminance_work_is_current_only_for_the_same_source_and_controls() {
        let mut tabs = Tabs::new();
        let id = tab_with_image(&mut tabs);
        let source = tabs
            .by_id_mut(id)
            .unwrap()
            .image
            .as_ref()
            .unwrap()
            .decoded
            .clone();
        let params = tabs.by_id_mut(id).unwrap().params.luminance;
        assert!(tabs.by_id_mut(id).unwrap().accepts_luma(&source, params));

        tabs.by_id_mut(id).unwrap().params.luminance.weighting = raw_core::Weighting::Red;
        assert!(!tabs.by_id_mut(id).unwrap().accepts_luma(&source, params));
        tabs.by_id_mut(id).unwrap().params.luminance = params;

        tabs.by_id_mut(id).unwrap().params.decode.unity_wb = true;
        assert!(!tabs.by_id_mut(id).unwrap().accepts_luma(&source, params));
        assert!(!tabs.by_id_mut(id).unwrap().needs_luma());
        tabs.by_id_mut(id).unwrap().params.decode = source.opts;

        let other = tab_with_image(&mut tabs);
        let replacement = tabs
            .by_id_mut(other)
            .unwrap()
            .image
            .as_ref()
            .unwrap()
            .decoded
            .clone();
        tabs.by_id_mut(id).unwrap().image.as_mut().unwrap().decoded = replacement;
        assert!(!tabs.by_id_mut(id).unwrap().accepts_luma(&source, params));
    }
}
