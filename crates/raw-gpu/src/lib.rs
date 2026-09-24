//! wgpu device work: upload the working image, execute the graph, composite.
//!
//! Decided, and not to be relitigated casually:
//!
//! - **Compute shaders writing storage textures**, not fullscreen-triangle
//!   raster. The vkdt-derived choice, correct for a modular per-pixel pipeline.
//! - **Pipelines and the intermediate pool are APP-level**, shared across tab
//!   graphs, because there will eventually be up to 8 tabs. vkdt's pool assumes
//!   one graph owns the device. `GpuContext` is that shared state; a `Viewport`
//!   owns only what belongs to one image.
//!
//! This crate does not create the device. It borrows the one eframe already has,
//! which is what "app-level" means in practice.
//!
//! # The chain
//!
//! The topology now comes from `raw_graph::build`, not from this file. What runs
//! for an untouched image is:
//!
//! ```text
//!   LumaImage (R32Float)
//!        |  sample.wgsl     view transform, box filter, coverage
//!        v
//!     Working (Rg32Float: value, coverage)       scene-referred
//!        |  exposure.wgsl   (in - black) * 2^stops
//!        v
//!     Working
//!        |  curve.wgsl      tone curve via LUT   [node absent when identity]
//!        v
//!     Working
//!        |  display.wgsl    tone map + transfer + dither + surround
//!        v
//!     target (Rgba8Unorm)                        display-referred, [0,1]
//! ```
//!
//! The curve is not *skipped* any more — when it is the identity the graph does
//! not contain it, so "no-op until touched" is a property of the pipeline rather
//! than a branch in the executor.

pub mod exec;
mod limits;
pub mod pool;
mod zones;

pub use exec::{GpuContext, Resources};
pub use pool::{Lease, TexDesc, TexturePool};
// The role vocabulary belongs to the graph now, where it is validated at
// connection time rather than debug_asserted against hardcoded wiring.
pub use raw_graph::StageRole;

use raw_core::curve::{HI_EV, LO_EV};
use raw_core::dodgeburn::{DodgeBurnParams, EV_MAX, Shape};
use raw_core::toning::ToningParams;
use raw_core::zone::Proxy;
use raw_core::{Curve, Frame, LumaImage, Params, ToneMap};
use raw_graph::{Axis, NodeKind, Plan, Roi, View};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use wgpu::util::DeviceExt as _;

pub const HISTOGRAM_BINS: usize = 128;
pub const HISTOGRAM_WEIGHT: u32 = 256;
/// Long-edge cap for full-frame histogram renders.
pub const HISTOGRAM_PROXY_EDGE: u32 = 512;
const HISTOGRAM_BYTES: u64 = (HISTOGRAM_BINS * std::mem::size_of::<u32>()) as u64;

/// One node's uniform block. Field order matches the `PRELUDE` below.
///
/// **Per node, not per frame.** Once nodes have their own regions, a shared block
/// cannot describe them: a blur's producer writes a wider rectangle at a
/// different origin, and every node has to know both its own extent and the
/// extent of the buffer it is reading. The module parameters are duplicated into
/// every block rather than split into a second bind group — at 128 bytes a node
/// that is a rounding error, and one binding is one fewer thing to keep in step.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct NodeParams {
    /// This node's own region, in grid pixels. The dispatch covers `out_w` x
    /// `out_h`; `out_x`/`out_y` place it on the grid.
    out_x: i32,
    out_y: i32,
    out_w: u32,
    out_h: u32,
    /// The region input 0's buffer actually holds — **not** what this node asked
    /// for. A producer satisfies the union of its consumers, so the buffer can be
    /// larger and differently placed, and indexing it as if it matched this
    /// node's region would offset the image by the apron.
    in_x: i32,
    in_y: i32,
    in_w: u32,
    in_h: u32,
    /// Input 1's region. Contrast Mask's mask; zero elsewhere.
    in2_x: i32,
    in2_y: i32,
    in2_w: u32,
    in2_h: u32,
    /// The stored image, in its own pixels. Read by the input node only.
    src_w: u32,
    src_h: u32,
    /// Grid pixels per **frame** pixel.
    ///
    /// Frame, not source: after a quarter turn those two grids are transposed and
    /// after a straighten they are not even the same size. The scale relates the
    /// output grid to the composed frame, and `comp` relates the frame to the
    /// stored image.
    scale: f32,
    /// Sub-pixel pan phase, in grid pixels. Regions are integral so that a
    /// spatial operation gives the same answer for the same pixel wherever the
    /// pan happens to be; this is the fractional remainder, and the input node is
    /// the only thing that applies it.
    sub_x: f32,
    sub_y: f32,
    exposure_ev: f32,
    black: f32,
    gamma: f32,
    curve_lo_ev: f32,
    curve_hi_ev: f32,
    tone_map: u32,
    dither: u32,
    shoulder_t: f32,
    shoulder_s: f32,
    agx_black_ev: f32,
    agx_white_ev: f32,
    agx_contrast: f32,
    agx_toe_power: f32,
    agx_shoulder_power: f32,
    /// Blur axis: 0 for x, 1 for y.
    blur_axis: u32,
    /// Gaussian sigma and tap radius, both in **grid** pixels — already scaled by
    /// the zoom, so the blur filters the same physical frequency at every zoom
    /// level. `blur_support` is taken from the graph's own apron rather than
    /// recomputed, so the shader cannot read further than the buffer it was given.
    blur_sigma: f32,
    blur_support: i32,
    /// Mask gamma, and the registration offset in grid pixels.
    cm_contrast: f32,
    cm_off_x: f32,
    cm_off_y: f32,
    /// Display-encoded canvas value; see `ViewGeometry::background`.
    background: f32,
    /// Diagnostic overlay bits; see `Overlays`.
    overlays: u32,
    /// Border width in screen pixels. Zero means no border.
    surround_w: f32,
    /// Border colour, gamma-encoded. Three scalars rather than a `vec3`: every
    /// member of this block is a 4-byte scalar, and a `vec3` in WGSL aligns to 16,
    /// which would silently shift everything after it.
    surround_r: f32,
    surround_g: f32,
    surround_b: f32,
    /// Composition, as a row-major 3×3 projective map from a **frame** coordinate
    /// to a **source** coordinate. The crop is not in
    /// here, because a crop is a region rather than a transform and the graph
    /// already expresses regions.
    ///
    /// Nine scalars rather than a WGSL matrix: every member of this block is a 4-byte
    /// scalar, and a matrix in WGSL aligns its columns to 8 or 16 bytes, which would
    /// silently shift everything after it — the same trap the `vec3` note above
    /// records for the surround colour.
    comp: [f32; 9],
    /// Whether the input node must interpolate. Non-zero exactly when the
    /// composition is not a quarter turn; see `Frame::resamples`.
    resample: u32,
    /// Where the accumulated Dodge & Burn total is clipped, in stops.
    ///
    /// A uniform rather than a WGSL constant so `raw_core::dodgeburn::EV_MAX` stays
    /// the single source of truth. A shader `const` would be a second copy of a
    /// number the CPU reference also uses, and the two would agree right up until
    /// one of them was changed.
    db_ev_max: f32,
    /// How many dabs of the buffer are live.
    ///
    /// **Not `arrayLength`**, deliberately. The buffer is over-allocated and grown
    /// in doublings so that a drag depositing a dab per frame does not allocate a
    /// new one per frame — and an over-allocated buffer's length is not its
    /// contents. Reading `arrayLength` would rasterise whatever the last longer
    /// stroke list left in the tail.
    db_dabs: u32,
    /// How many instances of the buffer are live. Same argument as `db_dabs`.
    db_instances: u32,
    /// Which instance's zone mask to show instead of the picture; `-1` is off.
    /// Indexes the *rasterised* list — see `Overlays::zone_mask`.
    db_show_mask: i32,
    /// The zone-mask strip: one mask's dimensions, not the whole texture's. The
    /// strip is `mask_h` per masked instance tall and rows are addressed by offset.
    mask_w: u32,
    mask_h: u32,
    /// Non-zero when chemical toning is live, so the display pass reads its table
    /// and expands to three channels.
    ///
    /// A uniform struct whose size is not 16-byte aligned is read back wrong by the
    /// shader **silently**: no validation error, just members at the wrong offsets.
    toning: u32,
    /// Accumulate final display luminance into the histogram buffer.
    histogram: u32,
    input_scale: f32,
    mask_scale: f32,
    _pad4: f32,
}

/// One dab, as the shader reads it.
///
/// The six floats `raw_core::dodgeburn::Dab` stores, plus the two indices that let
/// a single linear scan recover the three-level composite. The indices are not on
/// the CPU record because there they are the *shape of the data* — a dab lives
/// inside a gesture inside an instance — and flattening happens here, once, at the
/// buffer boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuDab {
    x: f32,
    y: f32,
    radius: f32,
    feather: f32,
    opacity: f32,
    ev: f32,
    /// The nib's own height/width ratio, its rotation in **radians** — converted on
    /// this side, once per change rather than once per pixel per frame — and 0 for a
    /// round nib, 1 for a card.
    aspect: f32,
    angle: f32,
    nib: u32,
    /// The radius of the circle that encloses this dab, in width units.
    ///
    /// Precomputed rather than derived in the shader: the cull evaluates it for every
    /// dab in every workgroup, and it is a square root and a branch that never change
    /// between frames.
    bound: f32,
    inst: u32,
    gesture: u32,
}

/// One instance's composite settings, and its geometry when it is a gradient.
///
/// **The gradient parameters ride here rather than in a buffer of their own.** An
/// instance is one of three shapes and at most `MAX_INSTANCES` of them exist, so a
/// third storage binding would carry at most eight records — and the shader needs
/// the sign, the opacity and the mask row in the same breath as the geometry
/// anyway. The brush case simply leaves them zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuInstance {
    sign: f32,
    opacity: f32,
    /// First row of this instance's mask in the strip, or -1 for no mask.
    mask_row: i32,
    /// 0 brush, 1 linear, 2 radial. The shader's dab scan skips anything but a
    /// brush; a second, short loop over instances handles the other two.
    kind: u32,
    /// Linear: `from` and `to`. Radial: the centre in `g0`/`g1`, unused after.
    g0: f32,
    g1: f32,
    g2: f32,
    g3: f32,
    /// Radial: inner, outer, ellipse aspect, angle in radians. Zero for a linear.
    g4: f32,
    g5: f32,
    g6: f32,
    g7: f32,
    /// Both: transition width, signed strength, and the radial's invert flag.
    feather: f32,
    ev: f32,
    invert: u32,
    contrast: f32,
}

impl GpuInstance {
    const BRUSH: u32 = 0;
    const LINEAR: u32 = 1;
    const RADIAL: u32 = 2;

    /// An instance that renders nothing, for the buffer that must exist even when
    /// no image has been painted on. See `Viewport::stroke_buffer`.
    const EMPTY: Self = Self {
        sign: 1.0,
        opacity: 0.0,
        mask_row: -1,
        kind: Self::BRUSH,
        g0: 0.0,
        g1: 0.0,
        g2: 0.0,
        g3: 0.0,
        g4: 0.0,
        g5: 0.0,
        g6: 0.0,
        g7: 0.0,
        feather: 0.0,
        ev: 0.0,
        invert: 0,
        contrast: 0.0,
    };
}

/// Stride between node blocks in the uniform buffer.
///
/// `min_uniform_buffer_offset_alignment` is 256 in WebGPU's default limits. A
/// device may permit less, but nothing is gained by finding out.
pub(crate) const UNIFORM_STRIDE: u64 = 256;

/// Where the viewport is looking. Separate from `Params` because it is view
/// state, not image state: it is not undoable, not saved to the sidecar, and not
/// copied when a tab is duplicated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewGeometry {
    /// Screen pixels per OUTPUT pixel. 1.0 is 100% — one screen pixel per output
    /// pixel, NOT per photosite. In SuperPixel mode `output_dims` is half
    /// `source_dims`, so 100% shows the pipeline's real working resolution.
    pub scale: f32,
    /// Top-left of the visible region, in output-pixel coordinates.
    pub off_x: f32,
    pub off_y: f32,
    /// Diagnostic overlays. View state, like the background.
    pub overlays: Overlays,
    /// The mount drawn around the image.
    pub surround: Surround,
    /// The canvas behind and around the image, display-encoded in `[0, 1]`.
    ///
    /// **View state, not image state** — which is why it rides here and not on
    /// `Params`: it is an application preference, the same for every open image, and
    /// it is not written to the sidecar. It has to reach the shader because the
    /// letterbox egui draws and the fill this shader writes meet at the image edge,
    /// and two values there is a visible seam.
    pub background: f32,
}

/// The mount: a border drawn around the image, the way a print sits on one.
///
/// **Screen pixels, not image pixels.** A mount surrounds the print you are looking
/// at, so it stays the same width as you zoom — and zooming in far enough that the
/// image fills the viewport hides it entirely, which is right: with your nose to a
/// print you do not see the mount either.
///
/// Not the viewer background. That is the canvas the whole thing sits on; this is
/// the board immediately around the frame. They were one concept in this codebase
/// once and separating them is deliberate.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Surround {
    /// Screen pixels. Zero is off — one representation of "no mount" rather than a
    /// width and a flag that can disagree.
    pub width: f32,
    /// Gamma-encoded sRGB, from `raw_core::okhsl`.
    pub rgb: [f32; 3],
}

impl Surround {
    pub const NONE: Self = Self {
        width: 0.0,
        rgb: [0.0; 3],
    };
}

/// Which diagnostic overlays are on.
///
/// **View state, not image state**: not undoable, not written to the sidecar, and
/// not copied when a tab is duplicated. They ride on `ViewGeometry` for the same
/// reason the canvas value does, and because that struct is compared whole for
/// change detection — so toggling one re-renders without anything having to
/// remember to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlays {
    /// Three tiers, from the prototype: near-clip magenta checkerboard, hard-clip
    /// red, blown black.
    pub overexposed: bool,
    /// Near-crush cyan checkerboard, hard-crush blue, pure black white.
    pub underexposed: bool,
    /// Cinema-standard exposure map. Replaces the image rather than marking it.
    pub false_colour: bool,
    /// Show one instance's **zone mask** instead of the picture, by its index in the
    /// rasterised list — `raw_core::DodgeBurnParams::active_index`. `-1` is off.
    ///
    /// Judging a tonal range without seeing it is guesswork: the ruler says which
    /// tones are selected and the picture says where they are, and only one of those
    /// is the question you are actually asking. The prototype flashes the mask while
    /// the ruler is dragged; this is a toggle, which the maintainer chose so it can be left on
    /// while the Region and Edge controls are worked.
    pub zone_mask: i32,
    /// Show the accumulated dodge or burn EV map instead of the picture, held on
    /// `⇧D` / `⇧X`. A display-only monotonic expansion keeps the working 0.25–0.5 EV
    /// range legible; it changes neither the mask nor the adjustment.
    ///
    /// The map reads through the curve and display transform like anything else, and
    /// the rubylith tint multiplies rather than adds so the gradient survives — see
    /// `display.wgsl`. A held diagnostic, like the other overlays: not a mode, not
    /// undoable, never written.
    pub dodge_map: bool,
    pub burn_map: bool,
    /// Hatch Bayer blocks where the sensor clipped. Green means at least one
    /// photosite in the block still measured the highlight; yellow means all four
    /// were censored and the sensor recorded no interior highlight detail.
    pub sensor: bool,
}

impl Overlays {
    pub const NONE: Self = Self {
        overexposed: false,
        underexposed: false,
        false_colour: false,
        zone_mask: -1,
        dodge_map: false,
        burn_map: false,
        sensor: false,
    };

    pub fn any(self) -> bool {
        self.overexposed
            || self.underexposed
            || self.false_colour
            || self.zone_mask >= 0
            || self.dodge_map
            || self.burn_map
            || self.sensor
    }

    /// The bitmask the shader reads. Kept beside the shader's own comment naming
    /// the same numbers.
    fn bits(self) -> u32 {
        u32::from(self.overexposed)
            | (u32::from(self.underexposed) << 1)
            | (u32::from(self.false_colour) << 2)
            | (u32::from(self.dodge_map) << 3)
            | (u32::from(self.burn_map) << 4)
            | (u32::from(self.sensor) << 5)
    }
}

impl Default for Overlays {
    fn default() -> Self {
        Self::NONE
    }
}

impl Default for ViewGeometry {
    fn default() -> Self {
        Self {
            scale: 1.0,
            off_x: 0.0,
            off_y: 0.0,
            background: 0.09,
            overlays: Overlays::NONE,
            surround: Surround::NONE,
        }
    }
}

/// Shared WGSL prologue. WGSL has no `#include`, and the uniform block must match
/// `NodeParams` field for field in every shader — so it is defined once here and
/// prepended, rather than copy-pasted into each and left to drift.
pub(crate) const PRELUDE: &str = r#"
struct Params {
    // This node's region on the grid.
    out_x: i32,
    out_y: i32,
    out_w: u32,
    out_h: u32,
    // The region input 0's buffer holds — not necessarily this node's own.
    in_x: i32,
    in_y: i32,
    in_w: u32,
    in_h: u32,
    // Input 1's region. Contrast Mask's mask; zero elsewhere.
    in2_x: i32,
    in2_y: i32,
    in2_w: u32,
    in2_h: u32,
    // The stored image, in its own pixels.
    src_w: u32,
    src_h: u32,
    // Grid pixels per source pixel, and the sub-pixel pan phase.
    scale: f32,
    sub_x: f32,
    sub_y: f32,
    // Exposure. Black first, so raising exposure does not amplify the offset.
    exposure_ev: f32,
    black: f32,
    // Display transfer function. Plain gamma — deliberately NOT called sRGB.
    gamma: f32,
    // The curve's log2 window, from raw_core::curve.
    curve_lo_ev: f32,
    curve_hi_ev: f32,
    tone_map: u32,
    dither: u32,
    // Soft-shoulder threshold and strength.
    shoulder_t: f32,
    shoulder_s: f32,
    // AgX range relative to 18% grey, pivot contrast, and the two sigmoid powers.
    agx_black_ev: f32,
    agx_white_ev: f32,
    agx_contrast: f32,
    agx_toe_power: f32,
    agx_shoulder_power: f32,
    // Blur axis (0 = x), and its sigma and tap radius in GRID pixels.
    blur_axis: u32,
    blur_sigma: f32,
    blur_support: i32,
    // Contrast Mask: gamma, and the registration offset in grid pixels.
    cm_contrast: f32,
    cm_off_x: f32,
    cm_off_y: f32,
    // The canvas behind and around the image, display-encoded.
    background: f32,
    // Diagnostic overlay bits: 1 over, 2 under, 4 false colour, 8 dodge map,
    // 16 burn map.
    overlays: u32,
    // The mount around the image: width in screen pixels, and its colour.
    surround_w: f32,
    surround_r: f32,
    surround_g: f32,
    surround_b: f32,
    // Composition: frame -> source, as a row-major 3x3 projective map;
    // see NodeParams.
    comp0: f32, comp1: f32, comp2: f32,
    comp3: f32, comp4: f32, comp5: f32,
    comp6: f32, comp7: f32, comp8: f32,
    // Non-zero when the composition is not a quarter turn and the input node
    // therefore has to interpolate.
    resample: u32,
    // Dodge & Burn: where the accumulated total clips, how many dabs are live,
    // and one zone mask's size within the strip.
    db_ev_max: f32,
    db_dabs: u32,
    db_instances: u32,
    db_show_mask: i32,
    mask_w: u32,
    mask_h: u32,
    // Non-zero when chemical toning is live. Took this block's padding word; see
    // NodeParams.
    toning: u32,
    histogram: u32,
    input_scale: f32,
    mask_scale: f32,
    _pad4: f32,
};

@group(0) @binding(4) var<uniform> p: Params;

// A dispatch covers this node's region, so `gid` is region-relative. Absolute
// grid coordinates are what every node agrees on, and the only currency in which
// two differently-placed buffers can be compared.
fn abs_coord(gid: vec2<u32>) -> vec2<i32> {
    return vec2<i32>(i32(gid.x) + p.out_x, i32(gid.y) + p.out_y);
}

// Absolute grid coordinate -> index into input 0's buffer.
fn in_coord(g: vec2<i32>) -> vec2<i32> {
    return g - vec2<i32>(p.in_x, p.in_y);
}

fn in_holds(g: vec2<i32>) -> bool {
    let l = in_coord(g);
    return l.x >= 0 && l.y >= 0 && l.x < i32(p.in_w) && l.y < i32(p.in_h);
}

// Input 1, for two-input nodes.
fn in2_coord(g: vec2<i32>) -> vec2<i32> {
    return g - vec2<i32>(p.in2_x, p.in2_y);
}

fn in2_holds(g: vec2<i32>) -> bool {
    let l = in2_coord(g);
    return l.x >= 0 && l.y >= 0 && l.x < i32(p.in2_w) && l.y < i32(p.in2_h);
}

// Absolute grid coordinate -> FRAME pixel. The view transform: zoom and pan, and
// nothing about composition.
fn frame_coord(gid: vec2<u32>) -> vec2<f32> {
    let g = abs_coord(gid);
    return vec2<f32>(
        (f32(g.x) + p.sub_x + 0.5) / p.scale,
        (f32(g.y) + p.sub_y + 0.5) / p.scale
    );
}

// Absolute grid coordinate -> source pixel. The one definition of the view transform.
//
// Two steps, kept separate so the composition cannot leak into the ROI arithmetic:
// `frame_coord` undoes zoom and pan onto the frame the graph runs on, `comp` maps that
// frame back onto the stored image.
//
// **The display node calls this to key the dither on the source pixel**, not the
// screen and not the frame — so the dither cannot crawl under a pan or a straighten.
// It is welded to the negative, which is where grain lives.
fn source_coord(gid: vec2<u32>) -> vec2<f32> {
    let g = abs_coord(gid);
    return source_at(f32(g.x), f32(g.y));
}

// The same map from an arbitrary, possibly fractional, ABSOLUTE grid coordinate.
//
// `source_coord` is this at a thread's own pixel. The general form exists because
// a bounding box is not a pixel: Dodge & Burn maps its workgroup's corners through
// here to cull strokes, and doing that with a second copy of the arithmetic is how
// a cull comes to disagree with the thing it is culling for.
fn source_at(gx: f32, gy: f32) -> vec2<f32> {
    let f = vec2<f32>((gx + p.sub_x + 0.5) / p.scale, (gy + p.sub_y + 0.5) / p.scale);
    return source_from_frame(f);
}

fn source_from_frame(f: vec2<f32>) -> vec2<f32> {
    let raw_w = p.comp6 * f.x + p.comp7 * f.y + p.comp8;
    let signed_floor = select(-1.0e-6, 1.0e-6, raw_w >= 0.0);
    let w = select(signed_floor, raw_w, abs(raw_w) >= 1.0e-6);
    return vec2<f32>(
        (p.comp0 * f.x + p.comp1 * f.y + p.comp2) / w,
        (p.comp3 * f.x + p.comp4 * f.y + p.comp5) / w
    );
}
"#;

/// Viewport targets are allocated in multiples of this, so an interactive window
/// resize does not reallocate — and, more to the point, does not force egui to
/// free and re-register the texture — on every frame of the drag. The shader
/// writes only the used sub-rect; `uv_rect` tells the host which part to draw.
const TARGET_QUANTUM: u32 = 128;

fn quantize(n: u32) -> u32 {
    n.div_ceil(TARGET_QUANTUM).max(1) * TARGET_QUANTUM
}

struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Allocated size, quantised.
    w: u32,
    h: u32,
    /// Region actually written this frame.
    used_w: u32,
    used_h: u32,
}

/// A render target owned by a caller rather than by a [`Viewport`].
///
/// One compare cell. It holds a texture and nothing else — the source, the LUT, the
/// zone proxy and the basis all stay on the viewport, which is what makes N-up cost
/// N small targets rather than N copies of the negative. See [`Viewport::render_into`].
#[derive(Default)]
pub struct Cell {
    target: Option<Target>,
    render_error: Option<String>,
    /// The texture was reallocated and the host must re-register it. Per cell, not per
    /// viewport: a cell that reported its reallocation somewhere else would be drawn
    /// from a `TextureId` that no longer refers to it.
    pub changed: bool,
}

impl Cell {
    pub fn render_error(&self) -> Option<&str> {
        self.render_error.as_deref()
    }

    pub fn view(&self) -> Option<&wgpu::TextureView> {
        self.target.as_ref().map(|t| &t.view)
    }

    /// Fraction of the allocated texture this cell's picture occupies. Targets are
    /// over-allocated to avoid resize churn, so drawing the whole thing would show
    /// whatever a larger earlier frame left in the padding.
    pub fn uv_rect(&self) -> [f32; 2] {
        match &self.target {
            Some(t) => [t.used_w as f32 / t.w as f32, t.used_h as f32 / t.h as f32],
            None => [1.0, 1.0],
        }
    }

    /// This cell's pixels, display-encoded and cropped to the used region.
    pub fn read_back(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<(u32, u32, Vec<u8>)> {
        read_target(self.target.as_ref()?, device, queue)
    }
}

#[derive(PartialEq)]
struct MaskCacheKey {
    steps: Vec<raw_graph::Step>,
    subpixel: (f32, f32),
    exposure: raw_core::ExposureParams,
    frame: Frame,
}

// One viewport-sized scene-linear result, not the much larger blur apron. Keep
// the live cache bounded independently of the shared idle pool.
const MASK_CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// One image's source, targets and bounded Contrast Mask result cache.
/// Compiled pipelines and reusable transient textures live in `GpuContext`.
pub struct Viewport {
    mask_cache: Option<(MaskCacheKey, Lease)>,
    mask_rebuilds: u64,
    render_error: Option<String>,
    uniforms: wgpu::Buffer,
    lut: wgpu::Buffer,
    tone_lut: wgpu::Buffer,
    /// The stroke buffers, the second and third things of the curve LUT's shape:
    /// storage buffers re-written only when their source changes, rather than
    /// uniforms rewritten every frame.
    dabs: wgpu::Buffer,
    instances: wgpu::Buffer,
    /// Zone masks stacked vertically, and the size of one of them.
    masks: wgpu::Texture,
    masks_view: wgpu::TextureView,
    /// One mask's size, and how many rows of them the strip is allocated for.
    /// Grown, never shrunk: the count changes a handful of times in a session and
    /// only ever by the user adding a masked instance.
    mask_dims: (u32, u32),
    mask_rows: u32,
    /// Live dabs, which is not the dab buffer's capacity. See `NodeParams::db_dabs`.
    live_dabs: u32,
    live_instances: u32,
    /// The luminance downsample the zone masks are computed from. Rebuilt with the
    /// source image and with nothing else; see `raw_core::zone::Proxy`.
    proxy: Arc<Proxy>,
    zones: zones::Zones,
    interactive_zones: bool,
    zone_render_pending: bool,
    // These describe actual resident resources, independently of the target's
    // last frame: comparison cells and exports share these resources.
    resource_params: Option<Arc<Params>>,
    source: wgpu::Texture,
    source_view: wgpu::TextureView,
    /// Per-working-pixel sensor clipping count, `0..=4`, stored in an R8 texture.
    /// Separate from the R32 luminance so this diagnostic costs one byte per pixel
    /// rather than doubling the source image.
    clipping: wgpu::Texture,
    clipping_view: wgpu::TextureView,
    source_dims: (u32, u32),
    target: Option<Target>,
    /// Display sink used by export taps so sampling never resizes the live target.
    tap_target: Option<Target>,
    histogram_target: Option<Target>,
    histogram: wgpu::Buffer,
    /// Set when the target was reallocated, so the host re-registers it with egui.
    pub target_changed: bool,
    /// Last state actually rendered, so this layer does its own change detection rather
    /// than trusting the caller. The graph is a pure function of `Params`, so comparing
    /// params covers topology too.
    ///
    /// **`ViewGeometry` is held whole, not just the parts the plan captures.** The plan
    /// encodes scale and offset but not the canvas value, and leaving it out meant the
    /// background slider changed a uniform nothing re-submitted. Holding the struct
    /// means the next field added cannot repeat that.
    ///
    /// **`Frame` is here for exactly that reason.** It is a function of the params
    /// *and* of the file's EXIF orientation, so params alone cannot describe it, and
    /// two compositions can produce the same plan with a different matrix — a
    /// quarter turn one way and a quarter turn the other give identical regions and
    /// a picture the other way up.
    last: Option<(Plan, Arc<Params>, ViewGeometry, Frame)>,
    /// Forces the next render regardless of `last` — set when a resource the
    /// params cannot describe (the source image, the target) was replaced.
    dirty: bool,
}

/// A small export-tap readback submitted to the GPU and polled without blocking
/// the UI thread.
pub struct PendingPatch {
    buffer: wgpu::Buffer,
    ready: Receiver<Result<(), wgpu::BufferAsyncError>>,
    lease: Option<Lease>,
    padded: u32,
    region: Roi,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

pub type PatchData = (u32, u32, Vec<f32>);

pub struct PendingHistogram {
    buffer: wgpu::Buffer,
    ready: Receiver<Result<(), wgpu::BufferAsyncError>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HistogramProxy {
    w: u32,
    h: u32,
    view: ViewGeometry,
}

fn histogram_proxy(frame: &Frame) -> HistogramProxy {
    let crop = frame.crop;
    let long = crop.w.max(crop.h);
    let scale = (HISTOGRAM_PROXY_EDGE as f32 / long as f32).min(1.0);
    let scaled = |side: u32| {
        if scale == 1.0 {
            side
        } else {
            (u64::from(side) * u64::from(HISTOGRAM_PROXY_EDGE)).div_ceil(u64::from(long)) as u32
        }
    };
    HistogramProxy {
        w: scaled(crop.w).max(1),
        h: scaled(crop.h).max(1),
        view: ViewGeometry {
            scale,
            off_x: crop.x as f32,
            off_y: crop.y as f32,
            ..Default::default()
        },
    }
}

impl PendingHistogram {
    pub fn poll(&mut self, device: &wgpu::Device) -> Option<Result<[u32; HISTOGRAM_BINS], String>> {
        let _ = device.poll(wgpu::PollType::Poll);
        match self.ready.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                return Some(Err("GPU histogram callback disconnected".into()));
            }
            Ok(Err(error)) => {
                return Some(Err(format!("GPU histogram mapping failed: {error}")));
            }
            Ok(Ok(())) => {}
        }
        let view = self.buffer.slice(..).get_mapped_range();
        let mut bins = [0; HISTOGRAM_BINS];
        bins.copy_from_slice(bytemuck::cast_slice(&view));
        drop(view);
        self.buffer.unmap();
        Some(Ok(bins))
    }
}

impl PendingPatch {
    /// Return `None` while the GPU is still working.
    pub fn poll(
        &mut self,
        device: &wgpu::Device,
        ctx: &mut GpuContext,
    ) -> Option<Result<PatchData, String>> {
        let _ = device.poll(wgpu::PollType::Poll);
        match self.ready.try_recv() {
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                if let Some(lease) = self.lease.take() {
                    ctx.pool_mut().release(lease);
                }
                return Some(Err("GPU sample callback disconnected".into()));
            }
            Ok(Err(error)) => {
                if let Some(lease) = self.lease.take() {
                    ctx.pool_mut().release(lease);
                }
                return Some(Err(format!("GPU sample mapping failed: {error}")));
            }
            Ok(Ok(())) => {}
        }
        let lease = self.lease.take().expect("a pending patch completes once");
        let view = self.buffer.slice(..).get_mapped_range();
        let result = working_values(
            &view,
            self.padded,
            &lease,
            self.region,
            self.x,
            self.y,
            self.w,
            self.h,
            true,
        )
        .ok_or_else(|| "GPU sample region was empty".to_owned());
        drop(view);
        self.buffer.unmap();
        ctx.pool_mut().release(lease);
        Some(result)
    }
}

impl Viewport {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, luma: &LumaImage) -> Self {
        let (w, h) = (luma.output_dims.w as u32, luma.output_dims.h as u32);
        let (source, source_view) = Self::make_source(device, queue, luma);
        let (clipping, clipping_view) = Self::make_clipping(device, queue, luma);

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("node params"),
            size: UNIFORM_STRIDE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Baked once here so the buffer always has valid contents even before the
        // first curve edit; `render` re-bakes only when the curve actually changes.
        let lut = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("curve lut"),
            contents: bytemuck::cast_slice(&Curve::default().bake()),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });

        // Same, for the toning table. Baked from the default parameters, which is an
        // inert module — but the buffer must be bindable whether or not anything is
        // toned, so it is never empty.
        let tone_lut = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("toning lut"),
            contents: bytemuck::cast_slice(
                &ToningParams::default().bake_flat(raw_core::toning::LUT_ENTRIES),
            ),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        });
        let histogram = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("histogram bins"),
            size: HISTOGRAM_BYTES,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        // An unpainted image still has to bind something: a zero-length storage
        // buffer is not bindable, and a bind group layout cannot be conditional on
        // a parameter. So the empty state is one inert dab of zero radius on an
        // instance of zero opacity, which the shader's own bounding test rejects
        // before it reads anything else — cheaper than a branch, and it means the
        // pass has no empty case to get wrong. It is also never dispatched: the
        // graph omits the node entirely when there is nothing to draw.
        let proxy = Proxy::of(luma);
        let dabs = Self::stroke_buffer(device, "db dabs", 0);
        let instances = Self::stroke_buffer(device, "db instances", 0);
        let (masks, masks_view) = Self::make_masks(device, 1, 1, 1);

        Self {
            mask_cache: None,
            mask_rebuilds: 0,
            render_error: None,
            uniforms,
            lut,
            tone_lut,
            dabs,
            instances,
            masks,
            masks_view,
            mask_dims: (1, 1),
            mask_rows: 1,
            live_dabs: 0,
            live_instances: 0,
            zones: zones::Zones::default(),
            interactive_zones: false,
            zone_render_pending: false,
            resource_params: None,
            proxy: Arc::new(proxy),
            source,
            source_view,
            clipping,
            clipping_view,
            source_dims: (w, h),
            target: None,
            tap_target: None,
            histogram_target: None,
            histogram,
            target_changed: false,
            last: None,
            dirty: true,
        }
    }

    /// A storage buffer of at least `bytes`, which is never zero: a zero-length
    /// storage binding is invalid, and an unpainted image still has to bind
    /// something.
    fn stroke_buffer(device: &wgpu::Device, label: &'static str, bytes: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: bytes.max(64),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// The zone-mask strip: room for `rows` masks of `w` x `h`, stacked.
    fn make_masks(
        device: &wgpu::Device,
        w: u32,
        h: u32,
        rows: u32,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("zone masks"),
            size: wgpu::Extent3d {
                width: w,
                height: h * rows.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        (texture, view)
    }

    /// How many bins the zone ruler's ghost is drawn from.
    ///
    /// Over the ruler's whole ±5 EV window, so one bin is a tenth of a stop — finer
    /// than the control can be dragged and coarse enough that the ghost reads as a
    /// distribution rather than as noise.
    pub const ZONE_BINS: usize = 100;

    /// The pre-D&B EV distribution over the zone ruler's window, normalised to its
    /// own peak. See [`Viewport::ZONE_BINS`].
    pub fn basis_rebuilds(&self) -> u64 {
        self.zones.rebuilds
    }

    /// Enable nonblocking zone preparation for an interactive host. Blocking
    /// clients (export workers and headless tools) retain synchronous results.
    pub fn set_interactive_zones(&mut self, enabled: bool) {
        self.interactive_zones = enabled;
    }

    pub fn zone_render_pending(&self) -> bool {
        self.zone_render_pending
    }

    pub fn request_zone_histogram(&mut self, params: &Params) -> Option<Vec<f32>> {
        self.zones
            .request(&self.proxy, params, true, false)
            .map(|p| p.histogram.clone())
    }

    pub fn zones_ready(&mut self, params: &Params) -> bool {
        !Self::needs_zones(params)
            || self
                .zones
                .request(&self.proxy, params, false, false)
                .is_some()
    }

    fn needs_zones(params: &Params) -> bool {
        params.dodgeburn.is_active() && params.dodgeburn.active().any(|i| !i.mask.is_identity())
    }

    /// Repack and upload the stroke buffers and the zone-mask strip.
    ///
    /// Called only when `dodgeburn` changed, or when the two modules the masks are
    /// computed *from* changed — exposure and Contrast Mask. Not every frame: this
    /// packs dabs and uploads masks already prepared by the CPU worker. Filtering
    /// is never performed here, and a pan must not repack unchanged strokes.
    ///
    /// Masks are resolved before instances because packing an instance needs to
    /// know which strip row it was given, and an instance whose mask is the
    /// identity is given none at all.
    fn write_strokes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params: &Params,
        prepared: Option<&zones::Prepared>,
    ) {
        let db: &DodgeBurnParams = &params.dodgeburn;

        let mut mask_data: Vec<f32> = Vec::new();
        let mut instances: Vec<GpuInstance> = Vec::new();
        for (index, inst) in db.active().enumerate() {
            let mask_row = match prepared.and_then(|p| p.masks[index].as_ref()) {
                Some(m) => {
                    let row = (mask_data.len() / self.proxy.w) as i32;
                    mask_data.extend_from_slice(m);
                    row
                }
                None => -1,
            };
            let mut gi = GpuInstance {
                sign: inst.sign.ev(),
                opacity: inst.opacity,
                mask_row,
                contrast: inst.contrast,
                ..GpuInstance::EMPTY
            };
            match &inst.shape {
                Shape::Brush { .. } => {}
                Shape::Linear(l) => {
                    gi.kind = GpuInstance::LINEAR;
                    (gi.g0, gi.g1, gi.g2, gi.g3) = (l.x0, l.y0, l.x1, l.y1);
                    gi.feather = l.feather;
                    gi.ev = l.ev;
                }
                Shape::Radial(r) => {
                    gi.kind = GpuInstance::RADIAL;
                    (gi.g0, gi.g1) = (r.cx, r.cy);
                    // Radians here rather than in the shader: it is one conversion
                    // per instance per change instead of one per pixel per frame,
                    // and the CPU reference is the place the unit is named.
                    (gi.g4, gi.g5, gi.g6, gi.g7) =
                        (r.inner, r.outer, r.aspect, r.angle.to_radians());
                    gi.feather = r.feather;
                    gi.ev = r.ev;
                    gi.invert = u32::from(r.invert);
                }
            }
            instances.push(gi);
        }

        // `inst` and `gesture` index only the ACTIVE instances and this instance's
        // own gestures, so they line up with the instance buffer above and stay
        // adjacent-comparable in the shader's scan. Empty gestures are skipped and
        // simply leave a gap in the numbering, which the scan cannot see; a
        // gradient contributes no dabs at all and is picked up by the shader's
        // second loop instead.
        let mut dabs: Vec<GpuDab> = Vec::new();
        for (ii, inst) in db.active().enumerate() {
            for (gi, g) in inst.gestures().iter().enumerate() {
                for d in g.dabs.iter() {
                    dabs.push(GpuDab {
                        x: d.x,
                        y: d.y,
                        radius: d.radius,
                        feather: d.feather,
                        opacity: d.opacity,
                        ev: d.ev,
                        aspect: d.aspect,
                        angle: d.angle.to_radians(),
                        nib: u32::from(d.nib == raw_core::dodgeburn::Nib::Card),
                        bound: d.bound(),
                        inst: ii as u32,
                        gesture: gi as u32,
                    });
                }
            }
        }

        // Grow in doublings. A drag appends a dab per frame, and reallocating a
        // buffer sixty times a second to add twenty-four bytes is the kind of
        // churn that shows up as a stutter rather than as a number.
        let need = std::mem::size_of_val(&dabs[..]) as u64;
        if self.dabs.size() < need {
            self.dabs = Self::stroke_buffer(device, "db dabs", need.next_power_of_two());
        }
        if !dabs.is_empty() {
            queue.write_buffer(&self.dabs, 0, bytemuck::cast_slice(&dabs));
        }
        self.live_dabs = dabs.len() as u32;
        self.live_instances = instances.len() as u32;

        let need = std::mem::size_of_val(&instances[..]) as u64;
        if self.instances.size() < need {
            self.instances = Self::stroke_buffer(device, "db instances", need.next_power_of_two());
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }

        let (mw, mh) = (self.proxy.w as u32, self.proxy.h as u32);
        let rows = (mask_data.len() / self.proxy.w.max(1) / self.proxy.h.max(1)) as u32;
        if rows > 0 {
            if self.mask_dims != (mw, mh) || self.mask_rows < rows {
                let (t, v) = Self::make_masks(device, mw, mh, rows);
                self.masks = t;
                self.masks_view = v;
                self.mask_dims = (mw, mh);
                self.mask_rows = rows;
            }
            queue.write_texture(
                self.masks.as_image_copy(),
                bytemuck::cast_slice(&mask_data),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(mw * 4),
                    rows_per_image: Some(mh * rows),
                },
                wgpu::Extent3d {
                    width: mw,
                    height: mh * rows,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    fn make_source(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        luma: &LumaImage,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let (w, h) = (luma.output_dims.w as u32, luma.output_dims.h as u32);
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("working image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // R32Float, per the design. Read with textureLoad rather than a sampler,
            // so the float32-filterable feature is not required.
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            source.as_image_copy(),
            bytemuck::cast_slice(&luma.data),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = source.create_view(&Default::default());
        (source, view)
    }

    fn make_clipping(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        luma: &LumaImage,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let (w, h) = (luma.output_dims.w as u32, luma.output_dims.h as u32);
        let clipping = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sensor clipping"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Unorm rather than Uint so it can use the executor's ordinary float
            // texture binding. The shader multiplies by 255 to recover the exact count.
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let empty;
        let counts = if luma.clipped.len() == w as usize * h as usize {
            luma.clipped.as_slice()
        } else {
            empty = vec![0u8; w as usize * h as usize];
            &empty
        };
        queue.write_texture(
            clipping.as_image_copy(),
            counts,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = clipping.create_view(&Default::default());
        (clipping, view)
    }

    /// Replace the working image without rebuilding pipelines. Used when the
    /// sampling or weighting mode changes and luminance is re-derived on the CPU.
    pub fn set_image(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, luma: &LumaImage) {
        self.mask_cache = None;
        let (w, h) = (luma.output_dims.w as u32, luma.output_dims.h as u32);
        if (w, h) == self.source_dims {
            queue.write_texture(
                self.source.as_image_copy(),
                bytemuck::cast_slice(&luma.data),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
            let empty;
            let counts = if luma.clipped.len() == w as usize * h as usize {
                luma.clipped.as_slice()
            } else {
                empty = vec![0u8; w as usize * h as usize];
                &empty
            };
            queue.write_texture(
                self.clipping.as_image_copy(),
                counts,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        } else {
            let (source, view) = Self::make_source(device, queue, luma);
            let (clipping, clipping_view) = Self::make_clipping(device, queue, luma);
            self.source = source;
            self.source_view = view;
            self.clipping = clipping;
            self.clipping_view = clipping_view;
            self.source_dims = (w, h);
        }
        // The zone masks are computed from this. Rebuilding here is what makes
        // `Proxy` depends on the luminance image and nothing else. Prepared zones
        // are invalidated; running jobs from this source cannot satisfy the next
        // generation's requests.
        self.proxy = Arc::new(Proxy::of(luma));
        self.zones.invalidate();
        self.resource_params = None;
        // The params can be identical across a source change, so change detection
        // cannot see this. Say so explicitly.
        self.dirty = true;
    }

    pub fn source_dims(&self) -> (u32, u32) {
        self.source_dims
    }

    /// Number of live-view Contrast Mask prefixes actually computed. Useful for
    /// checking that downstream edits reuse the expensive spatial result.
    pub fn contrast_mask_rebuilds(&self) -> u64 {
        self.mask_rebuilds
    }

    /// A rejected render leaves the previous target intact. The host should show
    /// this message instead of presenting that stale image as the requested view.
    pub fn render_error(&self) -> Option<&str> {
        self.render_error.as_deref()
    }

    pub fn target_view(&self) -> Option<&wgpu::TextureView> {
        self.target.as_ref().map(|t| &t.view)
    }

    /// Render `params` into a **caller-owned** target instead of this viewport's own.
    ///
    /// A `Viewport` per compare cell is not an option: [`Viewport::new`] uploads its own
    /// luminance texture, 400 MB on a 100 MP frame, so four cells would be 1.6 GB of the
    /// same pixels. Only the *target* differs per cell; the source, proxy and basis are
    /// functions of the image, which every cell shares.
    ///
    /// **Two pieces of state must not leak between cells.** `last` is the *live*
    /// render's change detection — left in place, each cell would compare itself against
    /// the previous cell and a pan would silently stop updating, so it is taken and put
    /// back. `target_changed` is per target and is captured onto the cell, or the host
    /// re-registers a reallocation on the viewport and a cell draws from a freed
    /// `TextureId`.
    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    pub fn render_into(
        &mut self,
        cell: &mut Cell,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        out_w: u32,
        out_h: u32,
        view: ViewGeometry,
        params: &Params,
        frame: &Frame,
    ) -> bool {
        std::mem::swap(&mut self.target, &mut cell.target);
        let live = self.last.take();
        let live_dirty = self.dirty;
        let live_error = self.render_error.take();
        let was_changed = self.target_changed;
        self.target_changed = false;
        // Forced: `last` is gone, but `dirty` is what `run` reads to bypass the
        // comparison entirely, and a cell arriving at the same size as the last one
        // must still be drawn — it is a different picture.
        self.dirty = true;
        let ok = self.render(ctx, device, queue, out_w, out_h, view, params, frame);
        cell.render_error = self.render_error.take();
        self.render_error = live_error;
        cell.changed = self.target_changed;
        self.target_changed = was_changed;
        self.last = live;
        // The live target's pixels survived this cell render unchanged.
        self.dirty = live_dirty;
        std::mem::swap(&mut self.target, &mut cell.target);
        ok
    }

    /// Fraction of the allocated target that holds this frame's image, for the
    /// host's UV rect. Targets are over-allocated to avoid resize churn, so
    /// drawing the whole texture would show stale content in the padding.
    pub fn uv_rect(&self) -> [f32; 2] {
        match &self.target {
            Some(t) => [t.used_w as f32 / t.w as f32, t.used_h as f32 / t.h as f32],
            None => [1.0, 1.0],
        }
    }

    /// One uniform block per step: the module parameters, plus this node's region
    /// and the regions of the buffers it reads.
    fn node_params(
        &self,
        plan: &Plan,
        params: &Params,
        view: ViewGeometry,
        frame: &Frame,
        histogram: bool,
    ) -> Vec<NodeParams> {
        let agx = match params.display.tone_map {
            ToneMap::Agx(agx) => agx.normalized(),
            _ => raw_core::AgxParams::DEFAULT,
        };
        let common = NodeParams {
            out_x: 0,
            out_y: 0,
            out_w: 0,
            out_h: 0,
            in_x: 0,
            in_y: 0,
            in_w: 0,
            in_h: 0,
            in2_x: 0,
            in2_y: 0,
            in2_w: 0,
            in2_h: 0,
            src_w: self.source_dims.0,
            src_h: self.source_dims.1,
            scale: plan.sink().out.scale,
            sub_x: plan.subpixel.0,
            sub_y: plan.subpixel.1,
            exposure_ev: params.exposure.ev,
            black: params.exposure.black,
            gamma: params.display.gamma,
            curve_lo_ev: LO_EV,
            curve_hi_ev: HI_EV,
            tone_map: match params.display.tone_map {
                ToneMap::Clip => 0,
                ToneMap::Agx(..) => 1,
                ToneMap::Shoulder { .. } => 2,
            },
            dither: u32::from(params.display.dither),
            shoulder_t: match params.display.tone_map {
                ToneMap::Shoulder { threshold, .. } => threshold,
                _ => 1.0,
            },
            shoulder_s: match params.display.tone_map {
                ToneMap::Shoulder { strength, .. } => strength,
                _ => 0.0,
            },
            agx_black_ev: agx.black_ev,
            agx_white_ev: agx.white_ev,
            agx_contrast: agx.contrast,
            agx_toe_power: agx.toe_power,
            agx_shoulder_power: agx.shoulder_power,
            blur_axis: 0,
            blur_sigma: 0.0,
            blur_support: 0,
            // On `common` like the surround and the composition: only one node
            // reads them, and a per-node exception is a thing to forget.
            db_ev_max: EV_MAX,
            db_dabs: self.live_dabs,
            db_instances: self.live_instances,
            db_show_mask: view.overlays.zone_mask,
            mask_w: self.mask_dims.0,
            mask_h: self.mask_dims.1,
            cm_contrast: 0.0,
            cm_off_x: 0.0,
            cm_off_y: 0.0,
            // On `common`, so every node carries it. Only the display node reads it,
            // but a per-node exception is a thing to forget.
            background: view.background,
            overlays: view.overlays.bits(),
            surround_w: view.surround.width,
            surround_r: view.surround.rgb[0],
            surround_g: view.surround.rgb[1],
            surround_b: view.surround.rgb[2],
            // On `common` for the same reason `background` is: only the input and
            // display nodes read it, and a per-node exception is a thing to forget.
            comp: frame.inverse(),
            resample: u32::from(frame.resamples()),
            // On `common` for the same reason `background` is: only the display node
            // reads it, and a per-node exception is a thing to forget.
            toning: u32::from(params.toning.is_active()),
            histogram: u32::from(histogram),
            input_scale: plan.sink().out.scale,
            mask_scale: plan.sink().out.scale,
            _pad4: 0.0,
        };

        plan.steps
            .iter()
            .map(|step| {
                let mut np = NodeParams {
                    out_x: step.out.x,
                    out_y: step.out.y,
                    out_w: step.out.w,
                    out_h: step.out.h,
                    scale: step.out.scale,
                    ..common
                };
                // The source node reads the stored image, whose "region" is the
                // whole texture in source pixels rather than a grid rectangle.
                if step.inputs.is_empty() {
                    np.in_w = self.source_dims.0;
                    np.in_h = self.source_dims.1;
                }
                if let Some((_, r)) = step.inputs.first() {
                    np.in_x = r.x;
                    np.in_y = r.y;
                    np.in_w = r.w;
                    np.in_h = r.h;
                    np.input_scale = r.scale;
                }
                if let Some((_, r)) = step.inputs.get(1) {
                    np.in2_x = r.x;
                    np.in2_y = r.y;
                    np.in2_w = r.w;
                    np.in2_h = r.h;
                    np.mask_scale = r.scale;
                }
                if matches!(step.kind, NodeKind::MaskInput { .. }) {
                    np.input_scale = view.scale.min(1.0);
                    let full =
                        Roi::grid_for((frame.frame.w as u32, frame.frame.h as u32), np.input_scale);
                    np.in_w = full.0;
                    np.in_h = full.1;
                    np.sub_x *= np.input_scale / view.scale;
                    np.sub_y *= np.input_scale / view.scale;
                }
                let scale = step.out.scale;
                match step.kind {
                    NodeKind::Blur { axis, sigma, .. } => {
                        np.blur_axis = u32::from(axis == Axis::Y);
                        // The graph owns the source-pixel sigma because more than
                        // one module now uses this pass. Convert it by the same scale
                        // the apron uses; the two must agree about physical reach.
                        np.blur_sigma = sigma * scale;
                        // Taken from the graph's own apron, not recomputed: the
                        // buffer was sized against that number, and a shader that
                        // reached one tap further would read whatever the pool
                        // last left there.
                        let a = step.kind.apron(0, scale);
                        np.blur_support = a.left.max(a.top) as i32;
                    }
                    NodeKind::ContrastMask { contrast, offset } => {
                        np.cm_contrast = contrast;
                        np.cm_off_x = offset.0 * scale;
                        np.cm_off_y = offset.1 * scale;
                    }
                    _ => {}
                }
                np
            })
            .collect()
    }

    /// Write the per-node blocks, growing the buffer if this graph is larger than
    /// any before it.
    fn write_uniforms(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blocks: &[NodeParams],
    ) {
        let needed = blocks.len() as u64 * UNIFORM_STRIDE;
        if self.uniforms.size() < needed {
            self.uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("node params"),
                size: needed,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        for (i, block) in blocks.iter().enumerate() {
            queue.write_buffer(
                &self.uniforms,
                i as u64 * UNIFORM_STRIDE,
                bytemuck::bytes_of(block),
            );
        }
    }

    /// Build and resolve the graph for one view of this image.
    ///
    /// Two different sets of dimensions cross this function and confusing them is
    /// the off-by-2 bug `CfaGeometry`'s doc comment warns about, one stage later:
    ///
    /// - **`self.source_dims`** — the stored luminance image, and what `build` is
    ///   given. Contrast Mask's spacer is a percentage of *that* diagonal, and it
    ///   stays that way through every composition. The reasoning in `params.rs` is
    ///   that the diagonal was chosen because it "survives a change of aspect or
    ///   orientation", which points straight at the uncropped, unrotated frame —
    ///   and the mask is computed on it anyway, because crop applies at the end. A
    ///   mask whose look changed when you cropped would be the same surprise as the
    ///   one that changed with sampling mode.
    /// - **`frame.frame`** — the composed frame, and the grid the graph resolves
    ///   against. Transposed by a quarter turn, grown by a straighten.
    fn plan(
        &self,
        out_w: u32,
        out_h: u32,
        view: ViewGeometry,
        params: &Params,
        frame: &Frame,
    ) -> Plan {
        let f = frame.frame;
        let c = frame.crop;
        // A small fitted frame already bounds the entire shared apron. Reuse
        // its sampled log image instead of sampling the same source twice.
        let grid = Roi::grid_for((f.w as u32, f.h as u32), view.scale);
        let share_mask = view.scale < 1.0
            && grid.0 <= out_w
            && grid.1 <= out_h
            && u64::from(grid.0) * u64::from(grid.1) <= 4 * 1024 * 1024;
        raw_graph::build_with_mask_source(params, self.source_dims, !share_mask)
            .resolve(
                (f.w as u32, f.h as u32),
                View {
                    scale: view.scale,
                    off_x: view.off_x,
                    off_y: view.off_y,
                    out_w,
                    out_h,
                    // An uncropped frame passes `None` rather than its own extent,
                    // so the plan is byte-identical to the one the graph produced
                    // before composition existed.
                    crop: (!frame.is_uncropped()).then_some((c.x, c.y, c.w, c.h)),
                },
            )
            // `build` is the only producer of graphs and its output is validated
            // by `the_built_graph_validates`. A failure here is a wiring bug in
            // this crate, not bad input.
            .expect("build() must produce a valid graph")
    }

    /// Render the visible region at `out_w` x `out_h` physical pixels.
    ///
    /// Returns whether work was actually submitted. Nothing is dispatched when
    /// neither the parameters, the view, the output size, nor the source image
    /// have changed — an idle window costs no GPU time.
    ///
    /// The output texture is viewport-sized, not image-sized, so zoom and pan
    /// cost the same regardless of image size, and only the visible region is
    /// ever computed.
    // Device, queue, and context are all borrowed app state rather than
    // parameters in any meaningful sense; bundling them into a struct would move
    // the argument count rather than reduce the coupling.
    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    pub fn render(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        out_w: u32,
        out_h: u32,
        view: ViewGeometry,
        params: &Params,
        frame: &Frame,
    ) -> bool {
        self.run(
            ctx, device, queue, out_w, out_h, view, params, frame, false, false,
        )
        .is_some()
    }

    /// The body of `render`, with the export tap exposed.
    ///
    /// `tap` retains the sink's input — the scene-referred, post-curve signal the
    /// display node consumed — and hands it back instead of returning it to the
    /// pool. Asking the graph which buffer that is replaces the milestone-2
    /// approach of re-deriving it from `params.curve.is_identity()`, which was
    /// the same decision made in a second place and would have gone wrong the
    /// moment a fourth node landed.
    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    fn run(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        out_w: u32,
        out_h: u32,
        view: ViewGeometry,
        params: &Params,
        frame: &Frame,
        tap: bool,
        histogram: bool,
    ) -> Option<Option<(Lease, Roi)>> {
        if !tap && !histogram {
            return self.run_inner(
                ctx, device, queue, out_w, out_h, view, params, frame, tap, histogram,
            );
        }
        // Auxiliary targets share GPU resources, never live-target validity or
        // presentation metadata. Restore these even when preflight rejects work.
        let changed = self.target_changed;
        let error = self.render_error.take();
        let result = self.run_inner(
            ctx, device, queue, out_w, out_h, view, params, frame, tap, histogram,
        );
        self.target_changed = changed;
        self.render_error = error;
        result
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    fn run_inner(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        out_w: u32,
        out_h: u32,
        view: ViewGeometry,
        params: &Params,
        frame: &Frame,
        tap: bool,
        histogram: bool,
    ) -> Option<Option<(Lease, Roi)>> {
        let (out_w, out_h) = (out_w.max(1), out_h.max(1));
        self.target_changed = false;
        self.zone_render_pending = false;

        // Validate before allocating targets, preparing CPU resources or touching
        // live-view validity. The plan's budget includes rounded texture storage.
        self.render_error = None;
        let max_dim = device.limits().max_texture_dimension_2d;
        let valid_view = view.scale.is_finite()
            && view.scale > 0.0
            && [view.off_x, view.off_y]
                .iter()
                .all(|x| x.is_finite() && (x * view.scale).abs() < 1.0e8)
            && (frame.frame.w.max(frame.frame.h) as f32 * view.scale) < 1.0e8;
        if !valid_view || out_w > max_dim || out_h > max_dim {
            self.render_error = Some("This view exceeds the renderer's supported dimensions. Reduce the window size or zoom.".into());
            return None;
        }
        let plan = self.plan(out_w, out_h, view, params, frame);
        if let Err(message) = limits::validate_plan(&plan, max_dim) {
            self.render_error = Some(message);
            return None;
        }

        let prepared = if Self::needs_zones(params) {
            match self
                .zones
                .request(&self.proxy, params, false, !self.interactive_zones)
            {
                Some(ready) => Some(ready),
                None => {
                    self.zone_render_pending = true;
                    return None;
                }
            }
        } else {
            None
        };

        let (alloc_w, alloc_h) = (quantize(out_w), quantize(out_h));
        if histogram {
            Self::ensure_target_slot(&mut self.histogram_target, device, alloc_w, alloc_h);
        } else if tap {
            Self::ensure_target_slot(&mut self.tap_target, device, alloc_w, alloc_h);
        } else {
            self.ensure_target(device, alloc_w, alloc_h);
        }
        let target = if histogram {
            &mut self.histogram_target
        } else if tap {
            &mut self.tap_target
        } else {
            &mut self.target
        };
        if let Some(t) = target {
            t.used_w = out_w;
            t.used_h = out_h;
        }

        // Re-bake the LUT only when the curve itself changed. A 256 KB upload is
        // cheap but not free, and it must not happen on every exposure frame.
        let curve_changed = self
            .resource_params
            .as_ref()
            .is_none_or(|p| !p.curve.same_render(&params.curve));
        if curve_changed {
            queue.write_buffer(&self.lut, 0, bytemuck::cast_slice(&params.curve.bake()));
        }

        // The toning table, on the same rule and for the same reason. The chemistry
        // is the expensive part and it runs here, once per edit, never in the shader.
        let toning_changed = self
            .resource_params
            .as_ref()
            .is_none_or(|p| p.toning != params.toning);
        if toning_changed {
            queue.write_buffer(
                &self.tone_lut,
                0,
                bytemuck::cast_slice(&params.toning.bake_flat(raw_core::toning::LUT_ENTRIES)),
            );
        }

        let strokes_changed = self.resource_params.as_ref().is_none_or(|p| {
            p.dodgeburn != params.dodgeburn
                || (Self::needs_zones(params)
                    && (p.exposure != params.exposure || p.contrast_mask != params.contrast_mask))
        });
        if strokes_changed {
            self.write_strokes(device, queue, params, prepared.as_deref());
        }

        // Record restored resources even if the live target is already valid.
        // Otherwise every idle frame after a different auxiliary look would
        // upload the same tables and strokes again.
        let snapshot =
            (curve_changed || toning_changed || strokes_changed).then(|| Arc::new(params.clone()));
        if let Some(snapshot) = &snapshot {
            self.resource_params = Some(Arc::clone(snapshot));
        }

        // Resource uploads above do not invalidate pixels already rendered into
        // the live target. They may restore resources changed by an auxiliary
        // request; only source/target changes and this live key require a redraw.
        // Params::diff owns the render tier, excluding export-only settings.
        // Compare before cloning; idle frames should not copy brush histories.
        if !self.dirty
            && !tap
            && !histogram
            && self
                .last
                .as_ref()
                .is_some_and(|(old_plan, p, old_view, old_frame)| {
                    old_plan == &plan
                        && !p.diff(params).render
                        && *old_view == view
                        && old_frame == frame
                })
        {
            return None;
        }
        let snapshot = snapshot.unwrap_or_else(|| Arc::new(params.clone()));
        self.resource_params = Some(Arc::clone(&snapshot));
        if !tap && !histogram {
            self.last = Some((plan.clone(), snapshot, view, *frame));
            self.dirty = false;
        }

        let blocks = self.node_params(&plan, params, view, frame, histogram);
        self.write_uniforms(device, queue, &blocks);

        // The export tap is the sink's input, and the graph is asked for BOTH the
        // buffer and the region it holds. That region happens to equal the tile
        // today, because nothing between the tap and the display node has any
        // reach — but relying on that silently is how a future node with an apron
        // would shift every export by a few pixels instead of failing.
        let tap_edge = tap.then(|| plan.sink().inputs[0]);
        let target = if histogram {
            self.histogram_target.as_ref()
        } else if tap {
            self.tap_target.as_ref()
        } else {
            self.target.as_ref()
        }
        .expect("just ensured");

        let res = Resources {
            source: &self.source_view,
            clipping: &self.clipping_view,
            target: &target.view,
            uniforms: &self.uniforms,
            lut: &self.lut,
            tone_lut: &self.tone_lut,
            histogram: &self.histogram,
            dabs: &self.dabs,
            instances: &self.instances,
            masks: &self.masks_view,
        };
        let cache_key = (!tap && !histogram)
            .then(|| {
                plan.steps
                    .iter()
                    .position(|step| step.kind == NodeKind::Exp2)
                    .filter(|&i| {
                        let roi = plan.steps[i].out;
                        u64::from(quantize(roi.w)) * u64::from(quantize(roi.h)) * 8
                            <= MASK_CACHE_BYTES
                    })
                    .map(|i| {
                        (
                            plan.steps[i].id,
                            MaskCacheKey {
                                steps: plan.steps[..=i].to_vec(),
                                subpixel: plan.subpixel,
                                exposure: params.exposure,
                                frame: *frame,
                            },
                        )
                    })
            })
            .flatten();
        let lease = if let Some((id, key)) = cache_key {
            let old = self.mask_cache.take().and_then(|(previous, lease)| {
                if previous == key {
                    Some(lease)
                } else {
                    ctx.pool_mut().release(lease);
                    None
                }
            });
            if old.is_none() {
                self.mask_rebuilds += 1;
            }
            let (tap_lease, cached) =
                ctx.execute_cached(device, queue, &plan, &res, None, Some((id, old)));
            self.mask_cache = cached.map(|lease| (key, lease));
            tap_lease
        } else {
            if !tap
                && !histogram
                && let Some((_, lease)) = self.mask_cache.take()
            {
                ctx.pool_mut().release(lease);
            }
            ctx.execute(device, queue, &plan, &res, tap_edge.map(|(id, _)| id))
        };
        Some(lease.map(|l| (l, tap_edge.expect("a lease is only retained when tapped").1)))
    }

    fn ensure_target(&mut self, device: &wgpu::Device, w: u32, h: u32) {
        let changed = Self::ensure_target_slot(&mut self.target, device, w, h);
        if changed {
            self.target_changed = true;
            self.dirty = true;
        }
    }

    fn ensure_target_slot(
        target: &mut Option<Target>,
        device: &wgpu::Device,
        w: u32,
        h: u32,
    ) -> bool {
        if target.as_ref().is_some_and(|t| t.w == w && t.h == h) {
            return false;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("viewport target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        *target = Some(Target {
            texture,
            view,
            w,
            h,
            used_w: w,
            used_h: h,
        });
        true
    }

    /// Read the rendered viewport back as RGBA8, cropped to the region actually
    /// written. Blocking; for headless regression tests.
    pub fn read_back(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<(u32, u32, Vec<u8>)> {
        read_target(self.target.as_ref()?, device, queue)
    }
}

/// Pull one target into host memory, cropped to the region actually written.
///
/// Shared by the viewport's own target and by a [`Cell`]'s, because they are the same
/// texture with the same quantised allocation and the same padding — and a second copy
/// of this arithmetic is a second place for the crop to be got wrong.
fn read_target(
    t: &Target,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
) -> Option<(u32, u32, Vec<u8>)> {
    // copy_texture_to_buffer requires 256-byte row alignment, unlike
    // write_texture, so the readback is padded and unpadded here.
    let padded = (t.w * 4).div_ceil(256) * 256;
    let raw = read_texture(device, queue, &t.texture, t.w, t.h, padded)?;
    // Crop to the used region: the allocation is quantised, and the padding
    // holds whatever the last larger frame left there.
    let (uw, uh) = (t.used_w.min(t.w), t.used_h.min(t.h));
    let mut out = Vec::with_capacity((uw * uh * 4) as usize);
    for row in 0..uh {
        let start = (row * padded) as usize;
        out.extend_from_slice(&raw[start..start + (uw * 4) as usize]);
    }
    Some((uw, uh, out))
}

/// Copy a texture into host memory. Blocking. Returns the padded rows as-is;
/// callers unpad according to what they want out of it.
fn read_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    w: u32,
    h: u32,
    padded: u32,
) -> Option<Vec<u8>> {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .ok()?;
    rx.recv().ok()?.ok()?;

    let view = slice.get_mapped_range();
    let out = view.to_vec();
    drop(view);
    buffer.unmap();
    Some(out)
}

/// Largest export tile, in pixels. See `Viewport::export`.
const EXPORT_TILE: u32 = 2048;

impl Viewport {
    /// Render the whole image at 1:1 and return **scene-referred f32**,
    /// post-curve.
    ///
    /// The branch point between screen and file, deliberately **upstream of the display
    /// transform**: the viewport dithers this to 8 bits, export takes the same signal
    /// through its container's L\* TRC at 16. Sharing the display tail would export a
    /// dithered approximation of a monitor.
    ///
    /// **Tiled at 2048**, because a 41 MP frame in DirectMosaic is 7872x5208 and one
    /// `Rg32Float` intermediate is 333 MB. To keep tile boundaries invisible,
    /// tile requests propagate spatial aprons through the graph before readback.
    ///
    /// Progress is reported through `on_tile(done, total)` so a caller can drive
    /// a progress bar; it is called on this thread.
    /// **Exports the crop, at 1:1 in frame pixels.** Tiles walk the crop rectangle
    /// rather than the stored image, and the offset carries the crop origin, so a
    /// cropped export is the picture the viewport shows and not the whole negative
    /// with the crop drawn on it.
    pub fn export(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params: &Params,
        frame: &Frame,
        mut on_tile: impl FnMut(u32, u32),
    ) -> Option<(u32, u32, Vec<f32>)> {
        let crop = frame.crop;
        let (w, h) = (crop.w, crop.h);
        let mut out = vec![0.0f32; (w as usize) * (h as usize)];

        let cols = w.div_ceil(EXPORT_TILE);
        let rows = h.div_ceil(EXPORT_TILE);
        let total = cols * rows;
        let mut done = 0;

        for ty in 0..rows {
            for tx in 0..cols {
                // Where this tile sits in the output, and where it sits in the
                // frame. The two differ by the crop origin, and keeping them apart
                // is the whole of what makes a cropped export land in the right
                // place — `ox` indexes the buffer, `fx` addresses the picture.
                let ox = tx * EXPORT_TILE;
                let oy = ty * EXPORT_TILE;
                let fx = crop.x + ox as i32;
                let fy = crop.y + oy as i32;
                let tw = EXPORT_TILE.min(w - ox);
                let th = EXPORT_TILE.min(h - oy);

                // scale 1.0 with the tile origin as the offset: output pixel 0 of
                // this tile is frame pixel `fx`, exactly.
                // Export never sees the canvas — every pixel is inside the crop —
                // so the value is irrelevant here and taking the default keeps the
                // export independent of a view preference.
                let view = ViewGeometry {
                    scale: 1.0,
                    off_x: fx as f32,
                    off_y: fy as f32,
                    ..Default::default()
                };
                // Tapped requests always dispatch, including one-tile exports.
                let interactive = self.interactive_zones;
                self.interactive_zones = false;
                let result = self.run(ctx, device, queue, tw, th, view, params, frame, true, false);
                self.interactive_zones = interactive;
                let (lease, region) = result??;
                let (rw, rh, values) =
                    self.read_working(device, queue, &lease, region, fx, fy, tw, th)?;
                ctx.pool_mut().release(lease);

                for y in 0..rh.min(th) {
                    let src = (y * rw) as usize;
                    let dst = ((oy + y) as usize) * (w as usize) + ox as usize;
                    let n = tw.min(rw) as usize;
                    out[dst..dst + n].copy_from_slice(&values[src..src + n]);
                }
                done += 1;
                on_tile(done, total);
            }
        }
        Some((w, h, out))
    }

    /// Render **one tile** at 1:1 from anywhere in the frame, as scene-referred f32.
    ///
    /// The same signal `export` produces and the same code path down to the tile
    /// loop — this is that loop's body with the walk taken out. It exists for the
    /// **grain loupe**, which needs a few hundred thousand pixels of the finished
    /// picture at scale 1.0 and needs them to be the export's pixels and not an
    /// approximation of them. Anything that wants "what will the file look like
    /// *here*" should come through this rather than growing a second path.
    ///
    /// `x` and `y` are in **frame** coordinates — the composed, straightened picture,
    /// the same space `Frame::crop` is in — so a caller that wants a region of the
    /// visible picture adds the crop origin, exactly as `export` does. They are
    /// allowed to sit outside the crop; the graph renders the frame, and the crop is
    /// a restriction on what the sink is asked for rather than a wall.
    ///
    /// Blocking GPU readback. The loupe re-renders when its inputs change; do not
    /// call this every frame or assume its latency is independent of the graph.
    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    pub fn patch(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params: &Params,
        frame: &Frame,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Option<(u32, u32, Vec<f32>)> {
        let (w, h) = (w.clamp(1, EXPORT_TILE), h.clamp(1, EXPORT_TILE));
        let view = ViewGeometry {
            scale: 1.0,
            off_x: x as f32,
            off_y: y as f32,
            ..Default::default()
        };
        // Taps always dispatch and write their own target; live validity is untouched.
        let interactive = self.interactive_zones;
        self.interactive_zones = false;
        let result = self.run(ctx, device, queue, w, h, view, params, frame, true, false);
        self.interactive_zones = interactive;
        let (lease, region) = result??;
        let read = self.read_working(device, queue, &lease, region, x, y, w, h);
        ctx.pool_mut().release(lease);
        read
    }

    /// Accumulate final display luminance through a bounded full-frame proxy.
    pub fn begin_histogram(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params: &Params,
        frame: &Frame,
    ) -> Option<PendingHistogram> {
        let proxy = histogram_proxy(frame);
        queue.write_buffer(
            &self.histogram,
            0,
            bytemuck::cast_slice(&[0u32; HISTOGRAM_BINS]),
        );
        self.run(
            ctx, device, queue, proxy.w, proxy.h, proxy.view, params, frame, false, true,
        )?;

        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("histogram readback"),
            size: HISTOGRAM_BYTES,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(&self.histogram, 0, &buffer, 0, HISTOGRAM_BYTES);
        queue.submit([encoder.finish()]);
        let (tx, ready) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        Some(PendingHistogram { buffer, ready })
    }

    /// Submit one export-tap region for asynchronous readback. Rendering and the
    /// copy are queued immediately; [`PendingPatch::poll`] never waits for them.
    #[expect(
        clippy::too_many_arguments,
        reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
    )]
    pub fn begin_patch(
        &mut self,
        ctx: &mut GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        params: &Params,
        frame: &Frame,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Option<PendingPatch> {
        let (w, h) = (w.clamp(1, EXPORT_TILE), h.clamp(1, EXPORT_TILE));
        let view = ViewGeometry {
            scale: 1.0,
            off_x: x as f32,
            off_y: y as f32,
            ..Default::default()
        };
        let (lease, region) =
            self.run(ctx, device, queue, w, h, view, params, frame, true, false)??;
        let padded = (lease.desc.w * 8).div_ceil(256) * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sample readback"),
            size: (padded * lease.desc.h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            lease.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(lease.desc.h),
                },
            },
            wgpu::Extent3d {
                width: lease.desc.w,
                height: lease.desc.h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let (tx, ready) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        Some(PendingPatch {
            buffer,
            ready,
            lease: Some(lease),
            padded,
            region,
            x,
            y,
            w,
            h,
        })
    }

    /// Read the scene-referred value out of a retained intermediate, dropping
    /// coverage. Blocking; used by export.
    #[expect(
        clippy::too_many_arguments,
        reason = "a readback region is its lease, origin and extent; each argument is one of those"
    )]
    fn read_working(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lease: &Lease,
        region: Roi,
        ox: i32,
        oy: i32,
        w: u32,
        h: u32,
    ) -> Option<(u32, u32, Vec<f32>)> {
        // 8 bytes per texel (Rg32Float), and copy_texture_to_buffer wants
        // 256-byte row alignment.
        let padded = (lease.desc.w * 8).div_ceil(256) * 256;
        let raw = read_texture(
            device,
            queue,
            &lease.texture,
            lease.desc.w,
            lease.desc.h,
            padded,
        )?;

        working_values(&raw, padded, lease, region, ox, oy, w, h, false)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "a readback region is its lease, origin and extent; each argument is one of those"
)]
fn working_values(
    raw: &[u8],
    padded: u32,
    lease: &Lease,
    region: Roi,
    ox: i32,
    oy: i32,
    w: u32,
    h: u32,
    covered_only: bool,
) -> Option<(u32, u32, Vec<f32>)> {
    // The node writes its widened region at the texture origin. Skip the apron and
    // coverage channel so callers receive only the requested finished luminance.
    let (sx, sy) = ((ox - region.x).max(0) as u32, (oy - region.y).max(0) as u32);
    let uw = w.min(lease.desc.w.saturating_sub(sx));
    let uh = h.min(lease.desc.h.saturating_sub(sy));
    if uw == 0 || uh == 0 {
        return None;
    }
    let mut out = Vec::with_capacity((uw * uh) as usize);
    for row in 0..uh {
        let start = ((row + sy) * padded + sx * 8) as usize;
        let texels: &[f32] = bytemuck::cast_slice(&raw[start..start + (uw * 8) as usize]);
        out.extend(
            texels
                .chunks_exact(2)
                .filter(|p| !covered_only || p[1] > 0.0)
                .map(|p| p[0]),
        );
    }
    (!out.is_empty()).then_some((uw, uh, out))
}

/// Limits to request from an adapter.
///
/// **The default `max_texture_dimension_2d` is 8192, and that is not enough.** In
/// DirectMosaic or Demosaic the working image is the full sensor width: 11648 on
/// the Fuji GFX 100S, which is a plain validation failure at the default limit —
/// `Device::create_texture` rejects it and wgpu panics inside the driver call, so
/// it reads as a crash rather than as an unsupported mode.
///
/// egui-wgpu hardcodes 8192 too (it sizes for a 4K depth buffer), so the app has
/// to override its device descriptor as well; see `raw-app`. Both paths must ask
/// for the same thing or export and viewport disagree about which files open.
///
/// Metal on Apple silicon reports 16384, which covers every sensor in the corpus.
pub fn limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
    wgpu::Limits {
        max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d,
        ..wgpu::Limits::default()
    }
}

/// Largest working image this device can hold, per axis.
///
/// Callers check `LumaImage::output_dims` against this **before** building a
/// `Viewport`, so an image too large for the GPU is a message rather than a panic.
pub fn max_image_dim(device: &wgpu::Device) -> u32 {
    device.limits().max_texture_dimension_2d
}

/// Build a headless device. Shared by the examples and the regression tests.
pub fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance.request_adapter(&Default::default()).await.ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("monopro headless"),
                required_limits: limits(&adapter),
                ..Default::default()
            })
            .await
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::Dims;
    use raw_core::composition::{CompositionParams, Orientation, Rect};

    fn frame(w: usize, h: usize, crop: Rect) -> Frame {
        let composition = CompositionParams {
            crop,
            ..Default::default()
        };
        Frame::resolve(Dims { w, h }, Orientation::Rotate0, &composition)
    }

    #[test]
    fn histogram_proxies_are_bounded_and_keep_the_whole_crop() {
        let landscape = histogram_proxy(&frame(6000, 4000, Rect::default()));
        assert_eq!((landscape.w, landscape.h), (512, 342));
        assert_eq!(landscape.view.scale, 512.0 / 6000.0);

        let portrait = histogram_proxy(&frame(4000, 6000, Rect::default()));
        assert_eq!((portrait.w, portrait.h), (342, 512));

        let cropped = histogram_proxy(&frame(
            6000,
            4000,
            Rect {
                x: 0.25,
                y: 0.25,
                w: 0.5,
                h: 0.25,
            },
        ));
        assert_eq!((cropped.w, cropped.h), (512, 171));
        assert_eq!((cropped.view.off_x, cropped.view.off_y), (1500.0, 1000.0));
        assert!(cropped.w <= HISTOGRAM_PROXY_EDGE);
        assert!(cropped.h <= HISTOGRAM_PROXY_EDGE);
    }

    #[test]
    fn small_histogram_proxies_stay_at_one_to_one() {
        let proxy = histogram_proxy(&frame(320, 200, Rect::default()));
        assert_eq!((proxy.w, proxy.h), (320, 200));
        assert_eq!(proxy.view.scale, 1.0);
    }

    #[test]
    fn targets_quantise_so_resizing_does_not_realloc() {
        // The whole point: every size inside a 128px band maps to one allocation, so
        // dragging a window edge does not churn egui texture registrations.
        assert_eq!(quantize(1), TARGET_QUANTUM);
        assert_eq!(quantize(128), 128);
        assert_eq!(quantize(129), 256);
        for n in 130..=256 {
            assert_eq!(quantize(n), 256, "size {n} left the band");
        }
    }

    #[test]
    fn view_params_is_a_multiple_of_sixteen() {
        // Uniform buffers must be 16-byte aligned, and a mismatch here shows up as a
        // silently misread struct in the shader rather than an error.
        assert_eq!(std::mem::size_of::<NodeParams>() % 16, 0);
        // 160 before composition added a transform and a resample flag; 192
        // before Dodge & Burn added its clip, its live dab count and the zone-mask
        // strip's dimensions; 224 before editable AgX added five curve values.
        assert_eq!(std::mem::size_of::<NodeParams>(), 256);
        // Blocks are addressed at UNIFORM_STRIDE intervals, so one must fit.
        assert!(std::mem::size_of::<NodeParams>() as u64 <= UNIFORM_STRIDE);
        assert_eq!(
            UNIFORM_STRIDE % 256,
            0,
            "min_uniform_buffer_offset_alignment is 256"
        );
    }

    #[test]
    fn every_shader_accepts_the_projective_uniform_layout() {
        let Some((device, _queue)) = headless_device() else {
            eprintln!("no headless GPU adapter; shader smoke test skipped");
            return;
        };
        let _ = exec::GpuContext::new(&device);
    }

    #[test]
    fn lut_size_fits_a_storage_buffer_not_a_1d_texture() {
        // The reason the LUT is a buffer: maxTextureDimension1D is 8192 by default.
        let n = raw_core::curve::LUT_SIZE;
        assert!(n > 8192, "a {n}-entry LUT would fit a 1D texture after all");
        assert!(n * 4 < 128 * 1024 * 1024);
    }
}
