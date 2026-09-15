//! Colour reference views. **Not the pipeline.**
//!
//! `j` cycles the viewport between the monochrome render, the camera's own embedded
//! JPEG, and a linear rendering of the raw — the FastRawViewer idiom, for answering
//! "what did the camera think" and "what is actually in the file" before committing
//! to a monochrome interpretation.
//!
//! # These are in colour, and that has to be contained
//!
//! The invariant this whole project exists to hold is that **nothing may reintroduce
//! a demosaic-then-convert path**: the Python prototype died of having LibRaw
//! demosaic and apply a colour matrix before the pipeline ever started, which made
//! honest sensor luminance unreachable by construction.
//!
//! So these views are contained by architecture rather than by discipline:
//!
//! - They produce **8-bit RGB for the screen** and nothing else. There is no float,
//!   no scene-referred value, nothing another stage could consume.
//! - They **never enter the graph**. `raw-graph` and the display shader do not know
//!   they exist; the viewport draws them as a plain texture, the same way it would
//!   draw an icon.
//! - Nothing downstream reads them. Export, the histogram, and every node take the
//!   `LumaImage`, which these do not touch.
//!
//! A colour view you can look at is not a colour pipeline. The difference is that
//! this one cannot be plumbed into anything without deleting the type.
//!
//! # No camera colour matrix, deliberately
//!
//! [`linear`] shows **camera-native RGB**: the gain-equalised photosites, binned,
//! gamma-encoded. It is not colour-managed and is not meant to be — applying the
//! camera matrix here would be the exact conversion the app refuses to depend on,
//! and having it available would make it tempting.
//!
//! What it is good for is what FastRawViewer's linear mode is good for: true
//! clipping, real shadow detail, and no camera tone curve dressing either up. Greens
//! will read strong, because that is what a Bayer sensor records before anyone
//! interprets it.

use crate::scene::SceneImage;

/// An 8-bit RGB image, for the screen and for nothing else.
///
/// Deliberately the narrowest type that can be displayed: no float, no scene
/// reference, no geometry beyond its own size. See the module note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgb8 {
    /// Tightly packed RGB, three bytes per pixel.
    pub data: Vec<u8>,
    pub w: usize,
    pub h: usize,
}

impl Rgb8 {
    pub fn pixels(&self) -> usize {
        self.w * self.h
    }
}

/// The display transfer function these views use.
///
/// Plain gamma, matching the viewport's default, and applied for the same reason:
/// without it a linear image is nearly black and tells you nothing. It is *not* a
/// tone curve — nothing is compressed, so clipping still reads as clipping, which
/// is the whole point of a linear reference.
fn encode(v: f32) -> u8 {
    (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8
}

/// Longest edge an embedded preview is kept at. Comfortably above any display this
/// runs on, and far below what cameras write.
const MAX_EDGE: u32 = 3840;

/// Longest edge a grid tile is kept at.
///
/// The prototype's number (`monopro.py:21890`), and it holds up: 512 covers the
/// largest tile the size slider offers with room to spare, so one cached tile serves
/// every size rather than one cache per size.
pub const TILE_EDGE: u32 = 512;

/// The embedded JPEG, decoded, at whatever size the camera wrote it.
///
/// **`full_image` is the one that answers**, despite the name. rawler 0.7.2 declares
/// `preview_image`, `full_image` and `thumbnail_image` on the decoder trait, but the
/// first two are defaulted to `Ok(None)` and *no decoder in the crate overrides
/// `preview_image`* — only `full_image`, which is where every format puts the big
/// embedded JPEG. `thumbnail_image` is implemented by the DNG decoder alone and
/// returned `None` for every file in this project's corpus.
///
/// So the chain is ordered by what actually resolves, not by what the names suggest.
/// This was measured rather than read: an earlier version of this function tried
/// `preview_image` first and every call in the app's history has silently fallen
/// through it.
fn decoded(
    path: &std::path::Path,
) -> Option<(image::DynamicImage, crate::composition::Orientation)> {
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;

    let src = RawSource::new(path).ok()?;
    let decoder = rawler::get_decoder(&src).ok()?;
    let params = RawDecodeParams::default();

    // **The embedded JPEG is stored the way the sensor read it**, not the way the
    // picture goes on the wall — the orientation tag applies to it exactly as it
    // applies to the raw. Measured rather than assumed: the corpus ARW carries a
    // 1616x1080 landscape preview and an orientation of `Rotate270`, so a portrait
    // frame arrives lying on its side unless the tag is honoured.
    //
    // Read here so that every consumer of this module gets an upright picture, which
    // is the same reason `sensor` reads the tag in one place rather than at the
    // render: a picture that is sideways in one view and upright in another is a bug
    // that takes a screenshot to notice.
    let orientation = decoder
        .raw_metadata(&src, &params)
        .ok()
        .and_then(|m| m.exif.orientation)
        .map(crate::composition::Orientation::from_exif)
        .unwrap_or_default();

    let img = decoder
        .full_image(&src, &params)
        .ok()
        .flatten()
        .or_else(|| decoder.preview_image(&src, &params).ok().flatten())
        .or_else(|| decoder.thumbnail_image(&src, &params).ok().flatten())?;

    Some((img, orientation))
}

/// Average `src` down to `tw` x `th` — a box filter, written out.
///
/// **This is a replacement for `image::imageops::thumbnail`, which is the single most
/// expensive thing in building a Lightbox tile.** Measured on the corpus: reducing the
/// 7840x5184 embedded JPEG of a DNG cost 204 ms there against 108 ms to decode the JPEG
/// in the first place, so two thirds of a cold tile was going on the resize. `thumbnail`
/// is a box filter too, so this is not a quality trade — it is the same average
/// computed without going through a generic per-pixel accessor and floating point.
///
/// Integer accumulators over `u8`, one pass, each source byte read exactly once. That
/// makes it memory-bandwidth-bound rather than arithmetic-bound, which is the floor for
/// an operation that has to look at every pixel — and at these ratios (15x on a 100 MP
/// body) it looks at a great many.
///
/// The output pixel's source rect is `[x*w/tw, (x+1)*w/tw)`, computed in `u64` so the
/// products cannot overflow on a large frame. Rounding leaves some rects a pixel wider
/// than others, which is what a box filter over a non-integer ratio has to do; the
/// alternative is a phase error that shows as a shimmer down one edge.
pub fn box_down(src: &image::RgbImage, tw: u32, th: u32) -> image::RgbImage {
    let (sw, sh) = (src.width(), src.height());
    if tw == 0 || th == 0 || sw == 0 || sh == 0 {
        return image::RgbImage::new(tw.max(1), th.max(1));
    }
    // The x spans, precomputed once rather than per row: they are the same for every
    // row and the division is the expensive part of the inner loop.
    let xs: Vec<(u32, u32)> = (0..tw)
        .map(|x| {
            // `a` is clamped below `sw` before `b` is derived from it, so an output
            // wider than the input — which `tile` never asks for, but a future caller
            // might — yields a one-pixel span rather than an empty one. An empty span
            // divides by a floored 1 and writes black, which is a stripe down the edge
            // of the picture and not an obvious consequence of "upscaled".
            let a = ((x as u64 * sw as u64 / tw as u64) as u32).min(sw - 1);
            let b = (((x + 1) as u64 * sw as u64 / tw as u64) as u32).clamp(a + 1, sw);
            (a, b)
        })
        .collect();

    let raw = src.as_raw();
    let mut out = Vec::with_capacity(tw as usize * th as usize * 3);
    for y in 0..th {
        let y0 = ((y as u64 * sh as u64 / th as u64) as u32).min(sh - 1);
        let y1 = ((((y + 1) as u64 * sh as u64) / th as u64) as u32).clamp(y0 + 1, sh);
        for &(x0, x1) in &xs {
            let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
            for row in y0..y1 {
                let base = (row as usize * sw as usize + x0 as usize) * 3;
                let span = &raw[base..base + (x1 - x0) as usize * 3];
                for px in span.chunks_exact(3) {
                    r += px[0] as u32;
                    g += px[1] as u32;
                    b += px[2] as u32;
                }
            }
            // **Rounded, not truncated.** `r / n` loses half a level on average, and
            // it does so on every pixel in the same direction — so the tile comes back
            // measurably darker than the picture. Caught by `box_down_does_not_darken`,
            // which is the test worth having here: the per-pixel difference from a
            // fractional-coverage filter is a wash and invisible, and a bias of half a
            // level across the whole grid is neither.
            let n = ((x1 - x0) * (y1 - y0)).max(1);
            let half = n / 2;
            out.push(((r + half) / n) as u8);
            out.push(((g + half) / n) as u8);
            out.push(((b + half) / n) as u8);
        }
    }
    image::RgbImage::from_raw(tw, th, out).unwrap_or_else(|| image::RgbImage::new(tw, th))
}

/// Turn stored pixels upright.
///
/// [`crate::composition::Orientation`] is named for what has to be *done* to the
/// stored pixels, clockwise — and `image::imageops`' rotations are clockwise too, so
/// the mapping is direct rather than inverted. Getting that backwards would leave
/// every portrait frame upside down instead of sideways, which is not obviously
/// better, so the correspondence is stated rather than left to be re-derived.
fn upright(rgb: image::RgbImage, orientation: crate::composition::Orientation) -> image::RgbImage {
    use crate::composition::Orientation as O;
    match orientation {
        O::Rotate0 => rgb,
        O::Rotate90 => image::imageops::rotate90(&rgb),
        O::Rotate180 => image::imageops::rotate180(&rgb),
        O::Rotate270 => image::imageops::rotate270(&rgb),
    }
}

/// The camera's own JPEG, as it would appear on the back of the camera.
///
/// `None` when the file carries no preview — some DNGs do not. The caller shows the
/// monochrome render instead rather than treating it as an error, because a missing
/// preview is a property of the file, not a failure.
pub fn embedded(
    path: &std::path::Path,
    upright_as: Option<crate::composition::Orientation>,
) -> Option<Rgb8> {
    let (img, exif) = decoded(path)?;
    // The sidecar's orientation replaces the file's own, exactly as `Frame::resolve`
    // treats it, so a frame turned in the grid and the same frame in Develop agree.
    let orientation = upright_as.unwrap_or(exif);

    // Cap the long edge. A Nikon's embedded preview is 8256x5504, which is 136 MB
    // of RGB8 and a texture upload to match — for a view whose job is to answer
    // "what did the camera think", on a screen a fraction of that size. Triangle
    // rather than Lanczos because this is a reference, not an output, and the
    // difference is invisible at the sizes involved while the cost is not.
    //
    // Triangle and not [`tile`]'s box filter because the ratio here is small — 8256
    // to 3840 is barely more than 2:1, where a box filter aliases and Triangle does
    // not. The tile path downscales 16:1, where the tradeoff reverses.
    let img = if img.width().max(img.height()) > MAX_EDGE {
        img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
    } else {
        img
    };

    let rgb = upright(img.to_rgb8(), orientation);
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Some(Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    })
}

/// The embedded JPEG at a size a screen can actually show.
///
/// **This is the full-frame preview, and it is not [`embedded`].** The difference is
/// measured, on this project's corpus and in release:
///
/// | | M10-R DNG | Nikon NEF |
/// |---|---|---|
/// | `embedded` — Triangle to 3840 | 222 ms | 237 ms |
/// | this — box filter to 2048 | **39 ms** | **43 ms** |
///
/// Two changes, and both are the same argument as [`tile`]'s. The filter: at a 4:1
/// reduction `imageops::thumbnail`'s two-stage box beats `Triangle` several times over
/// and the difference is invisible. The size: 3840 was headroom for a display nobody
/// has this view open on — the browser draws the frame to fit a window, so anything
/// past the window's own pixels is decoded, resized, uploaded and then thrown away by
/// the sampler. 2048 covers a full-screen view on a Retina panel and makes the texture
/// 11 MB instead of 39.
///
/// `embedded` keeps its 3840 and its Triangle because Develop's reference view can be
/// zoomed to 1:1, where this one cannot: there the extra pixels are ones you can
/// actually go and look at.
pub fn screen(
    path: &std::path::Path,
    max_edge: u32,
    upright_as: Option<crate::composition::Orientation>,
) -> Option<Rgb8> {
    let (img, exif) = decoded(path)?;
    let orientation = upright_as.unwrap_or(exif);
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let rgb = if w.max(h) > max_edge {
        let scale = max_edge as f32 / w.max(h) as f32;
        let tw = ((w as f32 * scale).round() as u32).max(1);
        let th = ((h as f32 * scale).round() as u32).max(1);
        image::imageops::thumbnail(&img.to_rgb8(), tw, th)
    } else {
        img.into_rgb8()
    };
    let rgb = upright(rgb, orientation);
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Some(Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    })
}

/// Longest edge the full-frame preview is kept at. See [`screen`].
pub const SCREEN_EDGE: u32 = 2048;

/// The same JPEG, reduced to a grid tile.
///
/// Separate from [`embedded`] because the filter is the whole cost. A tile is a
/// ~16:1 reduction of an 8000 px preview, and at that ratio `imageops::thumbnail`'s
/// two-stage box reduction beats `Triangle` by **3.3x** — measured across this
/// project's corpus, 45.6 ms to 13.9 ms mean — while differing from it by under one
/// part in 255 on the large files. Triangle at a 16:1 downscale is paying for a
/// reconstruction filter whose support covers two source pixels out of sixteen.
///
/// That difference is what makes a folder of 500 raws browsable rather than a
/// progress bar, so it is a load-bearing choice and not a micro-optimisation.
pub fn tile(
    path: &std::path::Path,
    upright_as: Option<crate::composition::Orientation>,
) -> Option<Rgb8> {
    let (img, exif) = decoded(path)?;
    let orientation = upright_as.unwrap_or(exif);
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }

    // Fit inside a TILE_EDGE box, preserving aspect. A preview already smaller than
    // the box is kept as it is: upscaling a small thumbnail would cost memory to add
    // nothing, and the grid can scale it up on the GPU for free if it must.
    //
    // Downscale *then* rotate: the turn is a pixel shuffle whose cost is the size of
    // what it is turning, and at that point this is a 512 px tile rather than an
    // 8000 px preview.
    let rgb = if w.max(h) > TILE_EDGE {
        let scale = TILE_EDGE as f32 / w.max(h) as f32;
        let tw = ((w as f32 * scale).round() as u32).max(1);
        let th = ((h as f32 * scale).round() as u32).max(1);
        box_down(&img.into_rgb8(), tw, th)
    } else {
        img.into_rgb8()
    };
    let rgb = upright(rgb, orientation);

    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Some(Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    })
}

/// Linear camera-native RGB, binned from the CFA.
///
/// **SuperPixel binning, not a demosaic**: one output pixel per 2x2 quad, the two
/// greens averaged. No interpolation and no invented data, which is the same
/// reasoning that makes SuperPixel the app's default sampling mode — and for a
/// reference view it is doubly right, since a reference that invented detail would
/// be answering a different question than the one being asked.
///
/// Half resolution, which also keeps this affordable: a 100 MP frame is 25 MP of
/// RGB8 here rather than 100.
///
/// **`orientation` is a parameter because `SceneImage` does not carry the tag.** The
/// mosaic is sensor-native by definition — the whole point of it — and the EXIF turn
/// belongs to the file rather than to the photosites. The monochrome render gets its
/// turn from `Frame::resolve` and the camera JPEG from [`tile`], so a reference view
/// that skipped it was the one picture in the app that stood on its side. The turn is
/// applied *after* the bin, on a quarter-size image, for the reason [`tile`] gives:
/// a rotation costs the size of what it is turning.
pub fn linear(scene: &SceneImage, orientation: crate::composition::Orientation) -> Rgb8 {
    let src = scene.geom.crop;
    let (w, h) = (src.w / 2, src.h / 2);
    let mut data = vec![0u8; w * h * 3];

    for oy in 0..h {
        for ox in 0..w {
            let (r0, c0) = (oy * 2, ox * 2);
            // Accumulate by CFA colour rather than by position, so the quad's two
            // greens average and the code does not care whether the pattern is
            // RGGB or GBRG.
            let mut sum = [0.0f32; 3];
            let mut n = [0u32; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let c = scene.color_at(r0 + dy, c0 + dx) as usize;
                    sum[c] += scene.data[(r0 + dy) * src.w + (c0 + dx)];
                    n[c] += 1;
                }
            }
            let i = (oy * w + ox) * 3;
            for c in 0..3 {
                let v = if n[c] > 0 { sum[c] / n[c] as f32 } else { 0.0 };
                data[i + c] = encode(v);
            }
        }
    }

    let rgb = match image::RgbImage::from_raw(w as u32, h as u32, data) {
        Some(rgb) => upright(rgb, orientation),
        // Unreachable: `data` was sized `w * h * 3` above. Returning the unturned
        // buffer would be a portrait frame on its side, so refuse to guess.
        None => {
            return Rgb8 {
                data: Vec::new(),
                w: 0,
                h: 0,
            };
        }
    };
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    Rgb8 {
        data: rgb.into_raw(),
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composition::Orientation;
    use crate::geometry::CfaColor::{Blue, Green, Red};
    use crate::sensor::Gains;
    use crate::{CfaGeometry, Dims};

    fn scene(w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> SceneImage {
        let geom = CfaGeometry::new(w, Dims { w, h }, 0, 0, w, h, [[Red, Green], [Green, Blue]]);
        let mut data = vec![0.0f32; w * h];
        for row in 0..h {
            for col in 0..w {
                data[row * w + col] = f(row, col);
            }
        }
        SceneImage {
            data,
            geom,
            gains: Gains([1.0, 1.0, 1.0]),
            camera: "test".into(),
            clipped: Vec::new(),
        }
    }

    #[test]
    fn a_neutral_subject_renders_neutral() {
        // Gain equalisation is what buys this: after it, every channel reads the
        // same value for neutral light, so camera-native RGB is already grey on
        // grey WITHOUT a colour matrix. It is the reason this view is usable at all
        // without the conversion the app refuses to make.
        let s = scene(64, 64, |_, _| 0.4);
        let p = linear(&s, Orientation::Rotate0);
        for px in p.data.chunks_exact(3) {
            assert_eq!(px[0], px[1], "a neutral subject picked up a cast: {px:?}");
            assert_eq!(px[1], px[2]);
        }
    }

    #[test]
    fn a_portrait_frame_stands_up_in_the_raw_linear_view() {
        // The reference views and the render must agree about which way up the
        // picture is, or switching between them turns the photograph on its side and
        // reads as a decode fault rather than a missing rotation. `tile` and
        // `embedded` have always applied the tag; this one did not.
        let s = scene(64, 32, |_, _| 0.4);
        let flat = linear(&s, Orientation::Rotate0);
        let turned = linear(&s, Orientation::Rotate90);
        assert_eq!((flat.w, flat.h), (32, 16));
        assert_eq!(
            (turned.w, turned.h),
            (16, 32),
            "a quarter turn did not swap the axes"
        );
        assert_eq!(turned.data.len(), flat.data.len());
    }

    #[test]
    fn it_bins_rather_than_interpolating() {
        assert_eq!(
            linear(&scene(64, 48, |_, _| 0.5), Orientation::Rotate0).w,
            32
        );
        assert_eq!(
            linear(&scene(64, 48, |_, _| 0.5), Orientation::Rotate0).h,
            24
        );
    }

    #[test]
    fn each_channel_reports_its_own_photosites() {
        // The point of the view: a red cast in the file must show as a red cast
        // here. Distinct values per colour, so a channel wired to the wrong index
        // is unmistakable.
        let s = scene(8, 8, |row, col| match (row % 2, col % 2) {
            (0, 0) => 0.8, // R
            (1, 1) => 0.2, // B
            _ => 0.5,      // G
        });
        let p = linear(&s, Orientation::Rotate0);
        let px = &p.data[..3];
        assert!(
            px[0] > px[1] && px[1] > px[2],
            "channels are not in order: {px:?}"
        );
        assert_eq!(px[0], encode(0.8));
        assert_eq!(px[1], encode(0.5));
        assert_eq!(px[2], encode(0.2));
    }

    #[test]
    fn clipping_still_reads_as_clipping() {
        // A linear reference exists to show what is actually clipped, so nothing
        // here may roll off the top the way a tone curve would.
        let s = scene(8, 8, |_, _| 1.4);
        assert!(
            linear(&s, Orientation::Rotate0)
                .data
                .iter()
                .all(|v| *v == 255)
        );
        let s = scene(8, 8, |_, _| 1.0);
        assert!(
            linear(&s, Orientation::Rotate0)
                .data
                .iter()
                .all(|v| *v == 255),
            "scene white is not display white"
        );
    }

    #[test]
    fn the_output_is_bytes_and_only_bytes() {
        // The containment this module rests on, as a compile-time fact: `Rgb8`
        // carries no float and no scene reference, so there is nothing here another
        // pipeline stage could consume even if someone wanted it to.
        let p = linear(&scene(8, 8, |_, _| 0.5), Orientation::Rotate0);
        assert_eq!(p.data.len(), p.pixels() * 3);
        assert_eq!(std::mem::size_of_val(&p.data[0]), 1);
    }
    #[test]
    fn the_real_raws_carry_previews() {
        // Whether a file actually has an embedded JPEG is a property of the file,
        // and rawler's support varies by format — so this checks the corpus rather
        // than assuming. Skips when the raws are not there.
        // Cargo runs tests with the PACKAGE root as the working directory, not the
        // workspace root, so a bare "raws" silently finds nothing and the test
        // passes having checked zero files.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let dir = dir.as_path();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !["dng", "raf", "nef", "cr3", "arw", "rw2", "3fr"].contains(&ext.as_str()) {
                continue;
            }
            checked += 1;
            match embedded(&p, None) {
                Some(img) => {
                    assert!(img.w > 0 && img.h > 0, "{}: empty preview", p.display());
                    assert_eq!(img.data.len(), img.pixels() * 3, "{}", p.display());
                    assert!(
                        img.w.max(img.h) <= MAX_EDGE as usize,
                        "{}: {}x{} exceeds the cap",
                        p.display(),
                        img.w,
                        img.h
                    );
                    eprintln!(
                        "{}: {}x{}",
                        p.file_name().unwrap().to_string_lossy(),
                        img.w,
                        img.h
                    );
                }
                None => {
                    eprintln!(
                        "{}: no embedded preview",
                        p.file_name().unwrap().to_string_lossy()
                    )
                }
            }
        }
        eprintln!("checked {checked} raws");
    }

    #[test]
    fn a_portrait_frame_comes_back_portrait() {
        // The bug this exists to catch, found on screen: the embedded JPEG is stored
        // the way the sensor read it, so a portrait frame arrives lying on its side
        // unless the orientation tag is honoured. `sensor`'s own test records these
        // two as `Rotate270` — a quarter turn means the tile's aspect must be the
        // *reciprocal* of the stored preview's.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let mut checked = 0;
        for name in ["sony-reference.ARW", "canon_eos_r_54.cr3"] {
            let p = dir.join(name);
            let Some(t) = tile(&p, None) else { continue };
            checked += 1;
            assert!(
                t.h > t.w,
                "{name}: a portrait frame came back {}x{} — still sideways",
                t.w,
                t.h
            );
        }
        if checked == 0 {
            return; // the raws are not here
        }

        // And a landscape frame is left alone, so the fix is not just "turn
        // everything". This one is `Rotate0` in the same record.
        if let Some(t) = tile(&dir.join("leica-reference.dng"), None) {
            assert!(
                t.w > t.h,
                "an upright landscape frame was turned: {}x{}",
                t.w,
                t.h
            );
        }
    }

    #[test]
    fn the_screen_preview_fits_its_box_and_is_the_same_picture() {
        // It is a different function from `embedded` with a different filter and a
        // different cap, so what has to hold is that it is still the same photograph:
        // same shape, same way up, inside the box it was asked for.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(dir.as_path()) else {
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !["dng", "raf", "nef", "cr3", "arw", "rw2", "cr2"].contains(&ext.as_str()) {
                continue;
            }
            let (Some(full), Some(s)) = (embedded(&p, None), screen(&p, SCREEN_EDGE, None)) else {
                continue;
            };
            checked += 1;

            assert!(
                s.w.max(s.h) <= SCREEN_EDGE as usize,
                "{}: {}x{} exceeds the box",
                p.display(),
                s.w,
                s.h
            );
            assert_eq!(s.data.len(), s.pixels() * 3, "{}", p.display());
            // Same orientation and same aspect as the reference view — a preview that
            // disagreed with `j` about which way up a frame goes would be worse than
            // a slow one.
            assert_eq!(
                s.w > s.h,
                full.w > full.h,
                "{}: the preview is turned the other way",
                p.display()
            );
            let (a, b) = (full.w as f64 / full.h as f64, s.w as f64 / s.h as f64);
            assert!(
                (a - b).abs() < 0.01,
                "{}: aspect {a:.4} became {b:.4}",
                p.display()
            );
        }
        eprintln!("checked {checked} screen previews");
    }

    #[test]
    fn a_tile_fits_the_box_and_keeps_its_shape() {
        // The grid's contract: never wider or taller than TILE_EDGE, aspect held to
        // within a pixel of the source, and the buffer the length its dimensions
        // claim. Skips when the raws are not there, like its neighbour.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../raws");
        let Ok(entries) = std::fs::read_dir(dir.as_path()) else {
            return;
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let p = e.path();
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !["dng", "raf", "nef", "cr3", "arw", "rw2", "3fr"].contains(&ext.as_str()) {
                continue;
            }
            let (Some(full), Some(t)) = (embedded(&p, None), tile(&p, None)) else {
                continue;
            };
            checked += 1;

            assert!(t.w > 0 && t.h > 0, "{}: empty tile", p.display());
            assert_eq!(t.data.len(), t.pixels() * 3, "{}", p.display());
            assert!(
                t.w.max(t.h) <= TILE_EDGE as usize,
                "{}: {}x{} exceeds the tile box",
                p.display(),
                t.w,
                t.h
            );

            // Same picture, not a differently-cropped one. A rounding step at each
            // end of a 16:1 reduction is worth about half a percent.
            let a = full.w as f64 / full.h as f64;
            let b = t.w as f64 / t.h as f64;
            assert!(
                (a - b).abs() < 0.01,
                "{}: aspect {a:.4} became {b:.4}",
                p.display()
            );
        }
        eprintln!("checked {checked} tiles");
    }
}

#[cfg(test)]
mod box_down_tests {
    use super::*;

    /// A deterministic stand-in for a photograph: a diagonal gradient with gentle
    /// texture over it, and a hard edge down one third so there is something for a
    /// filter to get wrong.
    ///
    /// **Deliberately not per-pixel noise.** The first version of this was
    /// `(x * 7 + y * 13) % 251`, which is a full-amplitude swing between adjacent
    /// pixels — and at that frequency *no* two resamplers agree, because what they
    /// return is a function of which phase each box happens to land on. It made the
    /// comparison below fail at 17 levels and said nothing about either filter. A
    /// photograph at the scale of a four-pixel box is smooth; the test image has to be
    /// too, or it is measuring aliasing rather than averaging.
    fn photo(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let base = 40.0 + 150.0 * (fx * 0.6 + fy * 0.4);
            let texture = 12.0 * (fx * 19.0).sin() * (fy * 23.0).cos();
            let edge = if fx > 1.0 / 3.0 { 25.0 } else { 0.0 };
            let v = |k: f32| (base + texture + edge + k).clamp(0.0, 255.0) as u8;
            image::Rgb([v(0.0), v(-6.0), v(-14.0)])
        })
    }

    fn mean(img: &image::RgbImage) -> f64 {
        img.as_raw().iter().map(|v| *v as f64).sum::<f64>() / img.as_raw().len() as f64
    }

    #[test]
    fn box_down_does_not_darken() {
        // **The bug this test exists for, and it was in the first version.** `sum / n`
        // truncates, so every output pixel loses half a level *in the same direction*
        // — the tile comes back measurably darker than the picture and a whole contact
        // sheet is dimmer than the frames in it. Measured at -0.48 of a level on the
        // corpus, which is small enough to pass an eyeball and is exactly the kind of
        // systematic error a grid of two hundred tiles makes visible.
        //
        // A mean is the right test because it is the one thing a box filter must get
        // right by definition: the average of the averages is the average.
        for (w, h) in [(1920, 1280), (4000, 3000), (777, 513)] {
            let src = photo(w, h);
            let scale = TILE_EDGE as f64 / w.max(h) as f64;
            let out = box_down(
                &src,
                ((w as f64 * scale) as u32).max(1),
                ((h as f64 * scale) as u32).max(1),
            );
            let (a, b) = (mean(&src), mean(&out));
            assert!(
                (a - b).abs() < 0.15,
                "{w}x{h}: source mean {a:.3}, tile mean {b:.3}"
            );
        }
    }

    #[test]
    fn box_down_agrees_with_the_filter_it_replaces() {
        // It is a straight swap for `image::imageops::thumbnail`, which is also a box
        // filter — so this is not a quality trade and the test says so in numbers. The
        // two differ only in how a source pixel straddling an output boundary is
        // shared, which at the ratios a 512 px tile uses is under a level.
        let src = photo(4000, 3000);
        let (tw, th) = (512, 384);
        let mine = box_down(&src, tw, th);
        let theirs = image::imageops::thumbnail(&src, tw, th);
        let err: f64 = mine
            .as_raw()
            .iter()
            .zip(theirs.as_raw())
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as f64)
            .sum::<f64>()
            / mine.as_raw().len() as f64;
        assert!(err < 1.5, "mean absolute difference of {err:.2} levels");
    }

    #[test]
    fn box_down_survives_the_shapes_that_are_not_pictures() {
        // Reachable from a corrupt or exotic embedded preview, and every one of them
        // divides by something. A panic here is a Lightbox worker thread dying, which
        // takes the tile with it and leaves the grid with a permanent gap.
        assert_eq!(box_down(&photo(1, 1), 1, 1).dimensions(), (1, 1));
        assert_eq!(box_down(&photo(10, 10), 1, 1).dimensions(), (1, 1));
        // A one-pixel-tall panorama, where one axis reduces and the other cannot.
        assert_eq!(box_down(&photo(4000, 1), 512, 1).dimensions(), (512, 1));
        // Asked to grow, which `tile` never does but which must not write black.
        let up = box_down(&photo(4, 4), 16, 16);
        assert_eq!(up.dimensions(), (16, 16));
        assert!(mean(&up) > 1.0, "an upscale came back black");
        // Degenerate output, guarded before any division.
        assert_eq!(box_down(&photo(8, 8), 0, 5).dimensions(), (1, 5));
    }
}
