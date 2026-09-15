//! Can an export resize rescue a single-channel DirectMosaic? Measured, and mostly yes.
//!
//! ```text
//! cargo run --release -p raw-core --example mosaic-resize
//! ```
//!
//! Companion to `mosaic-print`, which establishes that a Red weighting on DirectMosaic
//! puts one photosite in four at `4y` and the other three at exactly zero, and that
//! encoding that per pixel at native size reads far too dark. This asks the follow-on
//! question: what resize fixes it.
//!
//! # Three findings
//!
//! **Any ratio of 2:1 or more integrates the mosaic away, and it need not be exact.**
//! The worry going in was ringing — the export offers only Lanczos3 and Mitchell, both
//! windowed kernels with negative lobes, and a single-channel mosaic is maximum local
//! range at every pixel. On a regular grid that turns out not to matter: both filters
//! return the correct mean, and at 2:1 the patch comes back perfectly flat. A print size
//! typed as 13.1 inches lands on 3930 px against an exact half of 3932, which is 2.001:1
//! and fine.
//!
//! **Below 2:1 the CFA pattern survives as texture.** The mean stays right at every
//! ratio tested; what degrades is flatness. A patch that should be uniform comes back
//! spanning 0.173..0.187 at 1.6:1 and 0.151..0.212 at 1.333:1 — the residual mosaic,
//! which on a photograph is a grid you can see.
//!
//! **None of it helps above a quarter scene-value.** The `4x` gain clips at the tone
//! map, and `resample`'s own module note explains why the resize cannot move above it:
//! before the tone map a windowed filter undershoots on unclamped scene values and
//! outlines highlights in black. So the clip is upstream of the only place a resize may
//! legally go, and the data is gone before any filter sees it. Every column flattens at
//! `Y = 0.5`.
//!
//! # What that adds up to
//!
//! A 2:1 downsample of full-resolution DirectMosaic produces SuperPixel's output size
//! and, below the clip, SuperPixel's values. So the resize that makes single-channel
//! DirectMosaic printable is the one that reconstructs SuperPixel — one stage later,
//! having spent two stops of highlight on the way. There is no print size at which the
//! mode is the better choice.
use raw_core::display::lstar_encode;
use raw_core::resample::Filter;

/// A patch of gain-equalised scene at `y`, sampled as DirectMosaic with a Red
/// weighting: gain [4,0,0] on RGGB, so one photosite in four at 4y and three at zero.
fn mosaic(y: f32, w: usize, h: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; w * h];
    for r in 0..h {
        for c in 0..w {
            // RGGB: red is the even/even site.
            if r % 2 == 0 && c % 2 == 0 {
                v[r * w + c] = 4.0 * y;
            }
        }
    }
    v
}

fn main() {
    let (w, h) = (64usize, 64usize);
    println!("A flat neutral patch. SuperPixel+Red is the right answer in every row.\n");
    println!(
        "{:>8} {:>12} {:>12} {:>12} {:>12}",
        "scene Y", "correct L*", "box 2:1", "Lanczos3", "Mitchell"
    );

    for y in [0.05f32, 0.1, 0.18, 0.25, 0.5] {
        let correct = lstar_encode(y) * 100.0;

        // Tone map is Clip, the shipped default: everything past 1.0 is gone here,
        // BEFORE any resize can see it. See `resample`'s module note on the order.
        let mapped: Vec<f32> = mosaic(y, w, h).iter().map(|v| v.min(1.0)).collect();

        // An ideal 2:1 box — one Bayer quad to one pixel, which is what SuperPixel
        // does one stage earlier. Not a filter this app offers; here as the datum.
        let mut box_sum = 0.0;
        for r in (0..h).step_by(2) {
            for c in (0..w).step_by(2) {
                let q = mapped[r * w + c]
                    + mapped[r * w + c + 1]
                    + mapped[(r + 1) * w + c]
                    + mapped[(r + 1) * w + c + 1];
                box_sum += lstar_encode(q / 4.0);
            }
        }
        let boxed = box_sum / ((w / 2) * (h / 2)) as f32 * 100.0;

        // The two filters the export actually offers, at exactly 2:1.
        let mut got = [0.0f32; 2];
        for (i, f) in [Filter::Lanczos3, Filter::Mitchell].into_iter().enumerate() {
            let small = raw_core::resample(
                &mapped,
                w as u32,
                h as u32,
                (w / 2) as u32,
                (h / 2) as u32,
                f,
            );
            // Interior only: the frame edge is its own story and not the point here.
            let (sw, sh) = (w / 2, h / 2);
            let mut sum = 0.0;
            let mut n = 0;
            for r in 4..sh - 4 {
                for c in 4..sw - 4 {
                    sum += lstar_encode(small[r * sw + c].clamp(0.0, 1.0));
                    n += 1;
                }
            }
            got[i] = sum / n as f32 * 100.0;
        }

        println!(
            "{y:>8.2} {correct:>12.1} {boxed:>12.1} {:>12.1} {:>12.1}",
            got[0], got[1]
        );
    }

    // **Does the ratio have to be exactly 2:1?** A print size is typed in inches to one
    // decimal, so the pixel count it lands on is not free to choose.
    println!("\nOff-ratio, at Y = 0.18 (correct is 49.5):");
    let y = 0.18f32;
    let mapped: Vec<f32> = mosaic(y, w, h).iter().map(|v| v.min(1.0)).collect();
    for dw in [32usize, 31, 30, 33, 40, 48] {
        let dh = dw;
        let small = raw_core::resample(
            &mapped,
            w as u32,
            h as u32,
            dw as u32,
            dh as u32,
            Filter::Lanczos3,
        );
        let (mut sum, mut n, mut lo, mut hi) = (0.0f32, 0u32, 1.0f32, 0.0f32);
        for r in 4..dh - 4 {
            for c in 4..dw - 4 {
                let v = small[r * dw + c].clamp(0.0, 1.0);
                sum += lstar_encode(v);
                lo = lo.min(v);
                hi = hi.max(v);
                n += 1;
            }
        }
        let ratio = w as f32 / dw as f32;
        println!(
            "  {dw:>3} px ({ratio:.3}:1)  mean L* {:>5.1}   flatness: linear {:.3}..{:.3}",
            sum / n as f32 * 100.0,
            lo,
            hi
        );
    }
}
