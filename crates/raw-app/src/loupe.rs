//! The print loupe — a window onto the file, at the file's own scale.
//!
//! **Two modules run past the point the viewport can see**, and this is the only place
//! either of them is visible: grain, and output sharpening. Both are export-only, so
//! without this window they are sliders with no feedback — you would set them, export,
//! look, and set them again. The prototype answers grain with a 400x400 crop rendered
//! at 1:1 and floated over the viewport, and that is what this grew from.
//!
//! **It is one window and not two**, which was a decision rather than an omission. The
//! tile goes through the export's whole tail, so a sharpening change is already visible
//! in it — [`Key`] holds the entire `Params` for exactly that reason. A second loupe
//! would render the same tile through the same chain, could never be open at the same
//! time as this one (`Mode::Loupe` is exclusive, which is what makes the loupe *one*
//! thing), and would still be showing both effects whichever module it claimed to be
//! for.
//!
//! # What makes it a prediction rather than an impression
//!
//! The loupe's whole value is that what it shows is what the file will have, so
//! every step is the export's step and not a cheaper one that looks similar:
//!
//! - The pixels come from [`Viewport::patch`](raw_gpu::Viewport::patch), which is
//!   `export`'s tile loop with the walk taken out — the same graph, the same tap,
//!   the same scene-referred f32. `a_patch_is_the_export_cut_out_of_it` pins it.
//! - The tone map is the export's, then the resize, then grain, then sharpening, then
//!   **the screen's** transfer function. Only the last step differs from a file, and it
//!   has to: this is going on a monitor, and L\* on a monitor is the exact mistake
//!   `raw_core::display` exists to record.
//! - **Sharpening runs over the grain**, which is what makes this window the only place
//!   either module can honestly be judged: Amount changes how loud the grain reads, so
//!   the two are dialled together or not at all.
//! - The grain is keyed on the pixel's place in the print, so the crystals land
//!   where the export lands them. See [`raw_core::grain::apply_at`].
//! - The tile is rendered [`apron`] pixels wider than it is shown, because a crop's
//!   convolution runs against its own edge where the print had real pixels. Without it
//!   a border ring is wrong — measured, and the measurements are
//!   `a_tile_grains_like_the_print_it_was_cut_from_outside_its_apron` and
//!   `a_tile_sharpens_like_the_print_it_was_cut_from_outside_its_apron`.
//!
//! # The one place it approximates, stated rather than buried
//!
//! With **Resample on**, the file is finished at the output size, so the loupe samples
//! `1/scale` as much picture and resamples it before the tail — otherwise it would show
//! a crystal that is the right size in picture pixels and the wrong size in the pixels
//! that get written. Resampling a *tile* is only phase-exact if the tile's source
//! origin is `output_origin / scale` exactly, and that is not generally an integer, so
//! the source region is taken at the nearest whole pixel: the picture under the tail
//! can sit up to half a source pixel from where the file has it.
//!
//! **The grain itself is not approximated by this** — it is keyed on the output
//! coordinate, which is exact. What moves is the picture behind it, by less than a
//! pixel, which is not a thing this window is for looking at.
//!
//! **Sharpening is the same class of approximation with one more step to state.** A
//! band gain is not keyed on position, so it does not drift the way a crystal could —
//! but it acts on the picture *content*, so what this window shows is a faithful
//! prediction of the sharpening over a picture that may itself be a sub-pixel off. The
//! sharpening you are judging is the file's; the half pixel under it is not.

use std::sync::mpsc::{Receiver, channel};

use raw_core::display::{monitor_encode, tone_map};
use raw_core::geometry::Dims;
use raw_core::{Frame, Params};

/// The loupe's width, in image pixels. See [`SIZE_H`].
pub const SIZE_W: u32 = 750;

/// The loupe's height, in image pixels.
///
/// **3:2 and rectangular, where this used to be a 500 px circle** — the maintainer's call, and
/// the arithmetic is the argument. The tile was always rendered square and then drawn
/// through its inscribed circle, which threw away the 21% of it outside the disc: 500²
/// cost 250,000 px to show about 196,000. A 3:2 window shows everything it renders, and
/// it is the shape of the negatives this app is for.
///
/// **750 × 500 after the maintainer saw it in use**, against the 612 × 408 it shipped at, which
/// was sized to cost exactly what the circle cost. That turned out to be the wrong thing
/// to hold constant: the window is where two export-only modules are judged, and it was
/// too small to read grain and sharpening together.
///
/// The cost is the one thing to know before moving these again: the emulsion is
/// `SIZE_W · SIZE_H` pixels times the layer count, so it grows with *area* and not with
/// either edge. 375,000 px is 1.5× the circle's, so about 56 ms at thirty layers against
/// its 37 — still a window that appears rather than one you wait for, and still well
/// inside a drag's settling time. `grain_cost` is where to measure rather than guess.
pub const SIZE_H: u32 = 500;

/// The visible space between the sampled-area box and the loupe window, in points.
/// Close enough to read as a pair, while leaving the two amber strokes distinct.
pub const WINDOW_GAP: f32 = 12.0;

/// How much margin the tile needs beyond what it shows, in file pixels.
///
/// **The larger of the two apron-taking modules**, because the tile goes through both
/// and the wider one governs. Grain's is the largest crystal it can draw; sharpening's
/// is the reach of the à-trous ladder plus its edge shield's reference blur.
///
/// One helper rather than two call sites, because the trap here is a third module with
/// an apron arriving later and being filed into `place` but not `refresh` — at which
/// point the loupe would show a ring of wrong pixels only at some settings.
pub fn apron(params: &Params) -> u32 {
    params.grain.apron().max(params.sharpen.apron())
}

/// Everything that decides what the loupe should be showing.
///
/// Compared whole to answer "is the picture on screen still the right one". Holding
/// the entire `Params` is deliberate: the loupe sits at the bottom of the chain, so
/// *anything* upstream changes it, and a hand-listed set of fields is a list with the
/// next module missing from it.
#[derive(Clone, PartialEq)]
struct Key {
    /// Top-left of the shown region, in the **file's** pixels.
    origin: (u32, u32),
    /// Size of the shown region, in the file's pixels.
    size: (u32, u32),
    params: Params,
    frame: Frame,
}

/// A finished pair of renders, on their way back from the worker.
struct Ready {
    key: Key,
    grained: egui::ColorImage,
    clean: egui::ColorImage,
}

/// The loupe's state, per tab.
#[derive(Default)]
pub struct Loupe {
    /// Whether the window is up. Driven by the panel's checkbox and by
    /// `tabs::Mode::Loupe`, which is what claims the drag that moves it.
    pub open: bool,
    /// Show the **clean** crop instead of the grained one.
    ///
    /// Both are rendered from one patch and cached together, so this is a texture
    /// swap and not a re-render — which is why it can be a checkbox that stays put
    /// rather than a key you hold. The prototype re-renders; there is no need to.
    pub before: bool,
    /// Zero-based magnification rung: 0 = 100%, through 3 = 400%.
    ///
    /// Stored as an index so derived `Default` is the useful 100% view rather than an
    /// invalid zero-times magnification.
    magnification_index: u8,
    /// Opening the loupe asks the Develop module to reveal itself once. Kept until
    /// the panel is actually drawn, so a hidden or floating Develop pane cannot lose
    /// the request.
    reveal_module: bool,
    /// Where the loupe is sampling, in **frame** pixels — the composed, straightened
    /// picture, the space `Frame::crop` lives in. `None` until first shown, which
    /// puts it in the middle of the crop.
    pub at: Option<(f32, f32)>,
    /// What the held textures were rendered for.
    shown: Option<Key>,
    /// What the worker is computing, so the same request is not queued twice.
    pending: Option<Key>,
    rx: Option<Receiver<Ready>>,
    grained: Option<egui::TextureHandle>,
    clean: Option<egui::TextureHandle>,
}

impl Loupe {
    pub const MAGNIFICATIONS: [u8; 4] = [1, 2, 3, 4];

    pub fn magnification(&self) -> u8 {
        self.magnification_index.min(3) + 1
    }

    /// Change magnification and retire the differently sized texture immediately.
    pub fn set_magnification(&mut self, magnification: u8) {
        let index = magnification.clamp(1, 4) - 1;
        if self.magnification_index != index {
            self.magnification_index = index;
            self.forget();
        }
    }

    pub fn request_module_reveal(&mut self) {
        self.reveal_module = true;
    }

    pub fn take_module_reveal(&mut self) -> bool {
        std::mem::take(&mut self.reveal_module)
    }

    /// True while a render is in flight, which the overlay dims itself for.
    pub fn rendering(&self) -> bool {
        self.pending.is_some()
    }

    /// The texture to draw, if there is one yet.
    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        if self.before {
            self.clean.as_ref()
        } else {
            self.grained.as_ref()
        }
    }

    /// Drop the rendered tile. Called when the loupe closes and when the picture is
    /// re-derived — a 400x400 RGBA texture is not much, but a stale one shown for a
    /// frame is a picture of something that is no longer true.
    ///
    /// **Keeps `at`**, which is where the user put the loupe. A re-decode is the same
    /// negative and the sample point still means what it meant; moving it back to the
    /// middle every time the sampling mode changed would be its own small betrayal.
    pub fn forget(&mut self) {
        self.shown = None;
        self.pending = None;
        self.rx = None;
        self.grained = None;
        self.clean = None;
    }

    /// Forget the tile **and** where it was looking. For a genuinely different
    /// picture, where a coordinate from the last one means nothing.
    pub fn reset(&mut self) {
        self.at = None;
        self.forget();
    }

    /// Take delivery of anything the worker has finished.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let Ok(ready) = rx.try_recv() else { return };
        self.rx = None;
        self.pending = None;
        // Nearest, not linear: the loupe's entire claim is that these are the file's
        // pixels at 1:1, and a filtered upscale would be showing an interpolation of
        // the grain rather than the grain.
        let opts = egui::TextureOptions::NEAREST;
        self.grained = Some(ctx.load_texture("grain-loupe", ready.grained, opts));
        self.clean = Some(ctx.load_texture("grain-loupe-clean", ready.clean, opts));
        self.shown = Some(ready.key);
    }
}

/// Where the loupe samples and how big the sample is, worked out once so the
/// reticle, the render and the cache key cannot disagree about it.
///
/// All in **file** pixels: after the crop, and after the output resize if there is
/// one. That is the space grain runs in, so it is the space the loupe has to think
/// in — see the module note.
pub struct Placement {
    /// The shown region's top-left, in file pixels.
    pub origin: (u32, u32),
    /// The shown region's size, in file pixels. `SIZE_W × SIZE_H` at 100%, divided by
    /// the display magnification, unless the picture itself is smaller.
    pub size: (u32, u32),
    /// File pixels per frame pixel. 1.0 unless Output is resampling.
    pub scale: f32,
    /// The sample's centre and half-extent in **frame** pixels, for drawing the
    /// reticle on the picture.
    pub reticle: (f32, f32, f32, f32),
}

/// The tile rendered around a placed sample.
///
/// **The apron is taken per side, not required on every side.** It used to be required,
/// which put the reachable origin `apron` in from each edge — so the last band of the
/// print could not be inspected, and near an edge the sample stopped moving under a
/// pointer that kept going. Where the picture stops the tile stops with it, and grain
/// and sharpening meet their own frame border, which is the same edge the export gets.
///
/// Asking for apron that is not there and letting `patch` clamp the request would be
/// worse than either: a short patch resampled up to a full tile is a stretch, which is
/// the shift this exists to remove.
struct Tile {
    a_left: u32,
    a_top: u32,
    w: u32,
    h: u32,
    origin: (u32, u32),
}

fn tile_for(place: &Placement, params: &Params, frame: &Frame) -> Tile {
    let want = apron(params);
    let (sw, sh) = place.size;
    let file = params.output.target_dims(Dims {
        w: frame.crop.w as usize,
        h: frame.crop.h as usize,
    });
    let a_left = want.min(place.origin.0);
    let a_top = want.min(place.origin.1);
    let a_right = want.min((file.w as u32).saturating_sub(place.origin.0 + sw));
    let a_bottom = want.min((file.h as u32).saturating_sub(place.origin.1 + sh));
    Tile {
        a_left,
        a_top,
        w: sw + a_left + a_right,
        h: sh + a_top + a_bottom,
        origin: (place.origin.0 - a_left, place.origin.1 - a_top),
    }
}

/// Work out what the loupe is looking at.
///
/// `at` is where the user has put it, in frame pixels; `None` means the middle of the
/// crop. The result is clamped so the sample — **and its grain apron** — lies inside
/// the picture, because a tile that reached past the edge would grain against
/// reflected pixels the file does not have there.
#[cfg(test)]
fn place(at: Option<(f32, f32)>, frame: &Frame, params: &Params) -> Placement {
    place_magnified(at, frame, params, 1)
}

/// Place the sample at one of the loupe's 100–400% display magnifications.
///
/// The window stays the same size. Higher magnification samples fewer file pixels
/// and egui enlarges those exact pixels with nearest-neighbor texture sampling.
pub fn place_magnified(
    at: Option<(f32, f32)>,
    frame: &Frame,
    params: &Params,
    magnification: u8,
) -> Placement {
    let crop = frame.crop;
    let picture = Dims {
        w: crop.w as usize,
        h: crop.h as usize,
    };
    let file = params.output.target_dims(picture);
    // One scale for both axes: `target_dims` derives the second edge from the first
    // so the aspect cannot drift, which is what makes a single number honest here.
    let scale = if picture.w > 0 {
        file.w as f32 / picture.w as f32
    } else {
        1.0
    };

    // The shown region shrinks only if the file is smaller than the loupe, which is a
    // real case — a contact-sheet-sized export — and not one to divide by zero over.
    let magnification = magnification.clamp(1, 4) as f32;
    let sample_w = (SIZE_W as f32 / magnification).round() as u32;
    let sample_h = (SIZE_H as f32 / magnification).round() as u32;
    let sw = sample_w.min(file.w as u32).max(1);
    let sh = sample_h.min(file.h as u32).max(1);
    // **The whole picture is reachable, corner included.** The apron used to be
    // required rather than taken, so the origin was inset by it on every side: the
    // last band of the print could not be inspected, and the sample stopped moving
    // under a pointer that kept going, which reads as the image shifting. `refresh`
    // takes whatever apron each side actually has instead, exactly as the export's own
    // grain and sharpening handle their frame border.
    let max_x = (file.w as u32).saturating_sub(sw);
    let max_y = (file.h as u32).saturating_sub(sh);

    // Where the user is pointing, in the file's pixels.
    let (cx, cy) = at.unwrap_or((
        crop.x as f32 + crop.w as f32 * 0.5,
        crop.y as f32 + crop.h as f32 * 0.5,
    ));
    let fx = (cx - crop.x as f32) * scale;
    let fy = (cy - crop.y as f32) * scale;

    let ox = (fx - sw as f32 * 0.5).round().max(0.0) as u32;
    let oy = (fy - sh as f32 * 0.5).round().max(0.0) as u32;
    let origin = (ox.min(max_x), oy.min(max_y));

    // Back to frame pixels for the reticle, from the clamped origin rather than from
    // the pointer — so the ring is drawn around what is actually being shown and
    // stops moving when the sample stops moving.
    let hx = sw as f32 * 0.5 / scale;
    let hy = sh as f32 * 0.5 / scale;
    let reticle = (
        crop.x as f32 + (origin.0 as f32 + sw as f32 * 0.5) / scale,
        crop.y as f32 + (origin.1 as f32 + sh as f32 * 0.5) / scale,
        hx,
        hy,
    );

    Placement {
        origin,
        size: (sw, sh),
        scale,
        reticle,
    }
}

/// Place the loupe beside the sampled-area box, never diagonally from it.
///
/// The four candidates are a horizontal pair (left/right) or a vertical stack
/// (above/below). Whichever side has the most room after paying for the window is
/// used, then only the cross-axis is clamped to the viewer. On a viewer too small
/// to fit either rectangle cleanly, the final clamp keeps the loupe visible.
pub fn window_rect(viewer: egui::Rect, sample: egui::Rect, window_size: egui::Vec2) -> egui::Rect {
    #[derive(Clone, Copy)]
    enum Side {
        Right,
        Left,
        Below,
        Above,
    }

    let candidates = [
        (
            Side::Right,
            viewer.max.x - sample.max.x - WINDOW_GAP - window_size.x,
        ),
        (
            Side::Left,
            sample.min.x - viewer.min.x - WINDOW_GAP - window_size.x,
        ),
        (
            Side::Below,
            viewer.max.y - sample.max.y - WINDOW_GAP - window_size.y,
        ),
        (
            Side::Above,
            sample.min.y - viewer.min.y - WINDOW_GAP - window_size.y,
        ),
    ];
    let side = candidates
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(side, _)| side)
        .unwrap_or(Side::Right);

    let mut min = match side {
        Side::Right => egui::pos2(
            sample.max.x + WINDOW_GAP,
            sample.center().y - window_size.y * 0.5,
        ),
        Side::Left => egui::pos2(
            sample.min.x - WINDOW_GAP - window_size.x,
            sample.center().y - window_size.y * 0.5,
        ),
        Side::Below => egui::pos2(
            sample.center().x - window_size.x * 0.5,
            sample.max.y + WINDOW_GAP,
        ),
        Side::Above => egui::pos2(
            sample.center().x - window_size.x * 0.5,
            sample.min.y - WINDOW_GAP - window_size.y,
        ),
    };

    let latest = egui::pos2(
        (viewer.max.x - window_size.x).max(viewer.min.x),
        (viewer.max.y - window_size.y).max(viewer.min.y),
    );
    min.x = min.x.clamp(viewer.min.x, latest.x);
    min.y = min.y.clamp(viewer.min.y, latest.y);
    egui::Rect::from_min_size(min, window_size)
}

/// The emulsion half of a loupe render: scene-referred patch in, the clean tile and
/// the grained tile out, both still linear and both still carrying their apron.
///
/// Split out of [`refresh`] so it can be driven without a thread, a context or a
/// texture — which is what lets `the_loupe_shows_the_export_pixel_for_pixel` compare
/// it against a real export instead of against a description of one.
///
/// The order is `export::write`'s exactly: **tone map, resize, grain, sharpen**. The
/// screen's transfer function is applied by the caller and is the only step a file
/// does not take.
///
/// **"Clean" means before the whole export tail, not before grain.** Both modules that
/// run down here are export-only and invisible to the viewport, so the useful
/// comparison is the print with them and the print without — one checkbox, not two.
#[expect(
    clippy::too_many_arguments,
    reason = "the export tail's inputs for one tile, passed as the export passes them"
)]
fn compose(
    scene: &[f32],
    pw: u32,
    ph: u32,
    tile: (u32, u32),
    // Where the *tile* sits in the file, apron included — not the shown region's
    // origin. The emulsion is keyed on where a crystal lands in the print.
    tile_origin: (u32, u32),
    tone: raw_core::ToneMap,
    filter: raw_core::Filter,
    grain: &raw_core::GrainParams,
    sharpen: &raw_core::sharpen::SharpenParams,
) -> (Vec<f32>, Vec<f32>) {
    let (tw, th) = tile;
    let mapped: Vec<f32> = scene.iter().map(|&v| tone_map(v, tone)).collect();
    let clean = if (pw, ph) == (tw, th) {
        mapped
    } else {
        raw_core::resample(&mapped, pw, ph, tw, th, filter)
    };
    if !grain.is_active() && !sharpen.is_active() {
        // Both switched off: the two views are the same picture, which is the honest
        // answer and also what stops "Before" looking broken with the tail bypassed.
        return (clean.clone(), clean);
    }

    // Grain first, as the export does. The apron's first purpose: grain the wide tile,
    // show the middle. The origin handed to the emulsion is the tile's, not the shown
    // region's, so the crystals are keyed on where they land in the file.
    let grained = if grain.is_active() {
        raw_core::grain::apply_at(&clean, tw as usize, th as usize, tile_origin, grain).image
    } else {
        clean.clone()
    };

    // Then sharpening, over the grain — see `export::write` for why that way round.
    // **This is what the apron is really for now**, and it is the wider of the two
    // reaches: the wavelet ladder and the shield's reference blur both look outside the
    // shown region, and everything they look at has to be really there. Sharpening a
    // tile whose grain stopped at the shown edge would put a seam in the one window
    // whose job is to have none.
    //
    // `raw_core::sharpen` takes no origin because a band gain is not keyed on position —
    // unlike the emulsion above, it gives the same answer anywhere in the file.
    let print = raw_core::sharpen::apply(&grained, tw as usize, th as usize, sharpen);
    (clean, print)
}

/// Ask for a render if what is on screen is not what the parameters describe.
///
/// The GPU half runs here on the calling thread — one small tile and a readback,
/// well under a millisecond — and the emulsion goes to a worker, because it is tens
/// of milliseconds at the default settings and hundreds at sixty layers. That split
/// is the same one `App::export` makes and for the same reason: the slow half must
/// not be on the frame.
#[expect(
    clippy::too_many_arguments,
    reason = "device, queue and context are borrowed app state beside the real inputs; a struct would move the count, not the coupling"
)]
pub fn refresh(
    loupe: &mut Loupe,
    ctx: &egui::Context,
    gpu: &mut raw_gpu::GpuContext,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    viewport: &mut raw_gpu::Viewport,
    params: &Params,
    frame: &Frame,
    place: &Placement,
) {
    let key = Key {
        origin: place.origin,
        size: place.size,
        params: params.clone(),
        frame: *frame,
    };
    // Already showing it, or already making it. The second test is what stops a
    // slider drag queueing sixty emulsions.
    if loupe.shown.as_ref() == Some(&key) || loupe.pending.as_ref() == Some(&key) {
        return;
    }
    if loupe.rx.is_some() {
        // A render is in flight for something else. Let it land — dropping it would
        // cost the work already done, and it settles within a frame or two of a drag.
        return;
    }

    let (sw, sh) = place.size;
    let Tile {
        a_left,
        a_top,
        w: tw,
        h: th,
        origin: tile_origin,
    } = tile_for(place, params, frame);

    // The source region, in **frame** pixels. With no resize this is the tile itself;
    // with one it is the larger patch that resamples down to it. Rounded to whole
    // source pixels, which is the sub-pixel approximation the module note describes.
    let crop = frame.crop;
    let src_x = crop.x + (tile_origin.0 as f32 / place.scale).round() as i32;
    let src_y = crop.y + (tile_origin.1 as f32 / place.scale).round() as i32;
    let src_w = ((tw as f32 / place.scale).round() as u32).max(1);
    let src_h = ((th as f32 / place.scale).round() as u32).max(1);

    let Some((pw, ph, scene)) = viewport.patch(
        gpu, device, queue, params, frame, src_x, src_y, src_w, src_h,
    ) else {
        return;
    };

    let tone = params.display.tone_map;
    let display = params.display;
    let grain = params.grain;
    let sharpen = params.sharpen;
    // The export's filter, not a fixed one. Lanczos and Mitchell ring differently at
    // an edge, and a loupe that always used the default would be showing a resample
    // the file is not going to get.
    let filter = params.output.filter;
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    loupe.pending = Some(key.clone());
    loupe.rx = Some(rx);
    std::thread::spawn(move || {
        let (tile, grained) = compose(
            &scene,
            pw,
            ph,
            (tw, th),
            tile_origin,
            tone,
            filter,
            &grain,
            &sharpen,
        );

        let inner = |src: &[f32]| -> egui::ColorImage {
            let mut px = Vec::with_capacity((sw * sh) as usize);
            for y in 0..sh {
                for x in 0..sw {
                    let v = src[((y + a_top) * tw + x + a_left) as usize];
                    // `compose` already began with the selected tonal transform, just
                    // as export does. Applying `display::encode` here would run AgX or
                    // the soft shoulder a second time and make this window darker than
                    // the viewport. At this boundary only the monitor transfer remains.
                    let q =
                        (monitor_encode(v, display.gamma) * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                    px.push(egui::Color32::from_gray(q));
                }
            }
            egui::ColorImage {
                size: [sw as usize, sh as usize],
                pixels: px,
                source_size: egui::vec2(sw as f32, sh as f32),
            }
        };

        let ready = Ready {
            key,
            grained: inner(&grained),
            clean: inner(&tile),
        };
        // A closed loupe drops the receiver; that is not a failure, it is the
        // user having moved on, and the thread simply ends.
        let _ = tx.send(ready);
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::composition::{IRect, Orientation};

    fn frame_of(w: u32, h: u32) -> Frame {
        Frame::resolve(
            Dims {
                w: w as usize,
                h: h as usize,
            },
            Orientation::Rotate0,
            &Default::default(),
        )
    }

    fn cropped(w: u32, h: u32, crop: IRect) -> Frame {
        let mut f = frame_of(w, h);
        f.crop = crop;
        f
    }

    #[test]
    fn the_sample_reaches_every_edge_of_the_picture() {
        // **the maintainer, at the screen:** the loupe could not be taken to the edge, and near
        // one the sample stopped moving while the pointer kept going — which reads as
        // the picture shifting inside the window. The apron was *required* rather than
        // taken, so the reachable origin was inset by it on all four sides.
        //
        // It is taken per side now, so the corner is reachable and the tile simply
        // stops where the picture does. `refresh` is what shortens the tile; this is
        // the half that decides where the sample may sit.
        let f = frame_of(2000, 1500);
        let mut p = Params::default();
        p.grain.enabled = true;
        p.grain.set_size(19); // apron 30
        assert!(
            apron(&p) > 0,
            "the test needs a real apron to be measuring anything"
        );

        let corners = [
            ((-9999.0, -9999.0), (0, 0)),
            ((9999.0, -9999.0), (2000, 0)),
            ((-9999.0, 9999.0), (0, 1500)),
            ((9999.0, 9999.0), (2000, 1500)),
        ];
        for (at, (want_x, want_y)) in corners {
            let pl = place(Some(at), &f, &p);
            let (ox, oy) = pl.origin;
            let (sw, sh) = pl.size;
            // Hard into a corner puts the sample flush against both edges it names.
            let flush_x = if want_x == 0 { ox } else { 2000 - (ox + sw) };
            let flush_y = if want_y == 0 { oy } else { 1500 - (oy + sh) };
            assert_eq!(
                (flush_x, flush_y),
                (0, 0),
                "{at:?} left a gap: origin {:?} size {:?}",
                pl.origin,
                pl.size
            );
            // And never past it, which would be sampling pixels the file has not got.
            assert!(ox + sw <= 2000 && oy + sh <= 1500);
        }
    }

    #[test]
    fn magnification_shows_fewer_exact_file_pixels_in_the_same_window() {
        let f = frame_of(4000, 3000);
        let p = Params::default();
        for (magnification, expected) in [
            (1, (750, 500)),
            (2, (375, 250)),
            (3, (250, 167)),
            (4, (188, 125)),
        ] {
            let placement = place_magnified(None, &f, &p, magnification);
            assert_eq!(placement.size, expected, "{magnification}00%");
        }
    }

    #[test]
    fn loupe_magnification_defaults_to_100_and_stays_on_its_four_rungs() {
        let mut loupe = Loupe::default();
        assert_eq!(loupe.magnification(), 1);
        loupe.set_magnification(3);
        assert_eq!(loupe.magnification(), 3);
        loupe.set_magnification(99);
        assert_eq!(loupe.magnification(), 4);
        loupe.set_magnification(0);
        assert_eq!(loupe.magnification(), 1);
    }

    #[test]
    fn the_window_forms_a_horizontal_pair_when_the_side_has_room() {
        let viewer = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0));
        let sample = egui::Rect::from_min_max(egui::pos2(100.0, 340.0), egui::pos2(200.0, 460.0));
        let window = window_rect(viewer, sample, egui::vec2(375.0, 250.0));

        assert!((window.min.x - sample.max.x - WINDOW_GAP).abs() < 1e-3);
        assert!((window.center().y - sample.center().y).abs() < 1e-3);
        assert!(viewer.contains_rect(window));
    }

    #[test]
    fn the_window_forms_a_vertical_stack_when_that_side_has_room() {
        let viewer = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 1000.0));
        let sample = egui::Rect::from_min_max(egui::pos2(375.0, 800.0), egui::pos2(525.0, 860.0));
        let window = window_rect(viewer, sample, egui::vec2(375.0, 250.0));

        assert!((sample.min.y - window.max.y - WINDOW_GAP).abs() < 1e-3);
        assert!((window.center().x - sample.center().x).abs() < 1e-3);
        assert!(viewer.contains_rect(window));
    }

    #[test]
    fn the_sample_is_measured_in_the_files_pixels_not_the_pictures() {
        // With Output resampling, grain runs at the output size — so the loupe has to
        // as well, or it shows a crystal that is the right size against the picture
        // and the wrong size against the file. The reticle is the visible half of
        // this: at a 2x downsample one loupe-worth of file is *two* loupe-worths of
        // picture, and the ring on the image has to grow to say so.
        let f = frame_of(4000, 3000);
        let mut p = Params::default();
        p.grain.enabled = true;

        let native = place(None, &f, &p);
        assert_eq!(native.size, (SIZE_W, SIZE_H));
        assert!((native.scale - 1.0).abs() < 1e-6);
        assert!(
            (native.reticle.2 - SIZE_W as f32 * 0.5).abs() < 1.0,
            "{:?}",
            native.reticle
        );

        // Half size: 2000 px wide file from a 4000 px picture.
        p.output.ppi = 300.0;
        p.output.resize = Some(raw_core::Resize {
            inches: 2000.0 / 300.0,
            axis: raw_core::Axis::Width,
        });
        let resized = place(None, &f, &p);
        assert!(
            (resized.scale - 0.5).abs() < 1e-3,
            "scale is {}",
            resized.scale
        );
        assert_eq!(
            resized.size,
            (SIZE_W, SIZE_H),
            "the loupe still shows one tile of file pixels"
        );
        assert!(
            (resized.reticle.2 - SIZE_W as f32).abs() < 2.0,
            "the reticle must cover twice the picture: {:?}",
            resized.reticle
        );
    }

    #[test]
    fn the_sample_is_relative_to_the_crop_and_not_the_negative() {
        // The crop is what gets exported, so it is what the loupe's coordinates are
        // in. A loupe that indexed the whole frame would sample the right-looking
        // place on an uncropped file and drift by the crop origin on every other one
        // — which is the coordinate-frame bug that has cost this project time before.
        let whole = frame_of(3000, 2000);
        let cut = cropped(
            3000,
            2000,
            IRect {
                x: 800,
                y: 600,
                w: 1200,
                h: 900,
            },
        );
        let p = Params::default();

        // Dead centre of each, asked for the same way.
        let a = place(None, &whole, &p);
        let b = place(None, &cut, &p);
        assert_eq!(a.origin, (1500 - SIZE_W / 2, 1000 - SIZE_H / 2));
        assert_eq!(
            b.origin,
            (600 - SIZE_W / 2, 450 - SIZE_H / 2),
            "the crop origin leaked in"
        );
        // And the reticle goes back to frame coordinates, so it is drawn over the
        // part of the picture the loupe is actually showing.
        assert!((b.reticle.0 - 1400.0).abs() < 1.0, "{:?}", b.reticle);
        assert!((b.reticle.1 - 1050.0).abs() < 1.0, "{:?}", b.reticle);
    }

    #[test]
    fn the_loupe_shows_the_export_pixel_for_pixel() {
        // **The claim the whole module rests on, end to end and on a real GPU**: what
        // the loupe puts on screen is what the file will have. Everything else here
        // tests a piece of that; this one exports a picture, takes the loupe's own
        // path over the same picture, and compares the two arrays.
        //
        // Only the encode is left off both sides — the file goes to L\* and the screen
        // to gamma, and that difference is the one thing the loupe is *supposed* to
        // do differently. Comparing before it is what makes this a test of the
        // pipeline rather than of two transfer functions.
        //
        // A ramp, not a flat field: the seeding is proportional to exposure, so a flat
        // picture is degenerate in the axis every part of this lives in.
        let Some((device, queue)) = raw_gpu::headless_device() else {
            // No adapter in this environment. Skipping is right — the assertion is
            // about the pipeline, not about whether CI has a GPU — and every other
            // test in this file still runs.
            return;
        };
        // Big enough that the loupe's whole tile — the shown region *plus* an apron on
        // every side — fits inside it with room to be placed off centre. Sharpening's
        // apron is eight times grain's, so this is a good deal larger than the window.
        let (w, h) = (900usize, 700usize);
        let luma = raw_core::LumaImage {
            data: (0..w * h)
                .map(|i| (i % w) as f32 / (w - 1) as f32 * 1.2)
                .collect(),
            output_dims: Dims { w, h },
            source_dims: Dims { w, h },
            clipped: Vec::new(),
        };
        let mut p = Params::default();
        p.grain.enabled = true;
        p.grain.layers = 12;
        // **Both tail modules on, not one.** With sharpening off this test would still
        // pass with the sharpen call missing from `compose` altogether, and the apron
        // it needs is eight times grain's — so the interesting failure, a tile whose
        // edge pixels were sharpened against neighbours that are not there, only
        // appears when this is switched on.
        p.sharpen.enabled = true;
        p.sharpen.amount = 1.5;
        p.display.dither = false;
        let frame = frame_of(w as u32, h as u32);

        let mut ctx = raw_gpu::GpuContext::new(&device);
        let mut vp = raw_gpu::Viewport::new(&device, &queue, &luma);

        // The file, by the export's own route.
        let (fw, fh, scene) = vp
            .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
            .expect("export");
        let mapped: Vec<f32> = scene
            .iter()
            .map(|&v| tone_map(v, p.display.tone_map))
            .collect();
        let grained = raw_core::grain::apply(&mapped, fw as usize, fh as usize, &p.grain).image;
        let file = raw_core::sharpen::apply(&grained, fw as usize, fh as usize, &p.sharpen);

        // The loupe, by the loupe's route — off centre, so an origin that was being
        // ignored could not pass by landing where the picture is symmetric anyway.
        let pl = place(Some((260.0, 190.0)), &frame, &p);
        // The same arithmetic `refresh` uses, from the same function — this test used
        // to keep its own copy, and that copy is what broke when the apron became
        // per-side.
        let t = tile_for(&pl, &p, &frame);
        let (tw, th) = (t.w, t.h);
        let (px, py) = t.origin;
        let (pw, ph, patch) = vp
            .patch(
                &mut ctx, &device, &queue, &p, &frame, px as i32, py as i32, tw, th,
            )
            .expect("patch");
        let (_, grained) = compose(
            &patch,
            pw,
            ph,
            (tw, th),
            (px, py),
            p.display.tone_map,
            p.output.filter,
            &p.grain,
            &p.sharpen,
        );

        let mut worst = 0.0f32;
        for y in 0..pl.size.1 {
            for x in 0..pl.size.0 {
                let want = file[((pl.origin.1 + y) * fw + pl.origin.0 + x) as usize];
                let got = grained[((y + t.a_top) * tw + x + t.a_left) as usize];
                worst = worst.max((want - got).abs());
            }
        }
        // The residue is the exposure coefficient, which a crop computes over itself —
        // the one global quantity a tile cannot reproduce, and `apply_at` says so.
        assert!(
            worst < 0.02,
            "the loupe is not showing the file: worst {worst}"
        );

        // And the comparison is not vacuous: the same region *without* grain is a
        // visibly different picture, so a `compose` that quietly skipped the emulsion
        // would fail here rather than sail through the assertion above.
        let clean: Vec<f32> = (0..pl.size.1)
            .flat_map(|y| {
                (0..pl.size.0).map(move |x| ((pl.origin.1 + y) * fw + pl.origin.0 + x) as usize)
            })
            .map(|i| mapped[i])
            .collect();
        let grain_moved = (0..clean.len())
            .map(|i| {
                let (x, y) = (i as u32 % pl.size.0, i as u32 / pl.size.0);
                (grained[((y + t.a_top) * tw + x + t.a_left) as usize] - clean[i]).abs()
            })
            .fold(0.0f32, f32::max);
        assert!(
            grain_moved > 0.05,
            "grain changed almost nothing: {grain_moved}"
        );
    }

    #[test]
    fn a_picture_smaller_than_the_loupe_still_places() {
        // A contact-sheet-sized export is a real thing to ask for, and every clamp
        // here has a subtraction in it. The sample shrinks to what there is rather
        // than wrapping, underflowing, or asking the GPU for a negative tile.
        let f = frame_of(120, 90);
        let mut p = Params::default();
        p.grain.enabled = true;
        p.grain.set_size(19);
        let pl = place(Some((0.0, 0.0)), &f, &p);
        assert!(pl.size.0 <= 120 && pl.size.1 <= 90, "{:?}", pl.size);
        assert!(pl.size.0 > 0 && pl.size.1 > 0);
    }
}
