//! What output sharpening costs and buys, measured on a real frame.
//!
//!     cargo run --release --example sharpen-sweep -- raws/*.ARW
//!
//! Two of this module's three numbers were placeholders when it was written: the fixed
//! envelope `SPREAD`, and whether `edges` should default on and at what. Neither can be
//! settled from the prototype — its spread was a slider nobody moved independently, and
//! its `edges` default of zero exists only for compatibility with builds that predate
//! the shield. So they get measured here rather than guessed, and then judged by eye in
//! the loupe, which is the part no example can do.
//!
//! # What is measured, and why these two things
//!
//! Sharpening is a trade, and both sides of it have to be on the same table:
//!
//! - **Texture gain** — mean absolute neighbour difference over the whole frame,
//!   relative to unsharpened. This is what you are buying. Above 1.0 is sharpening.
//! - **Halo overshoot** — the 99.9th percentile of how far a pixel is pushed *past*
//!   the local range it started in, in display units. This is what you are paying.
//!   A band gain cannot leave the range of the original image at that scale, so this
//!   is bounded, but bounded is not the same as invisible: it is the rim on a
//!   silhouette that makes an over-sharpened print look electronic.
//!
//! The useful reading is not either column alone but the ratio: the setting that buys
//! the most texture per unit of overshoot is the one to default to.

use raw_core::display::tone_map;
use raw_core::sharpen::{self, SharpenParams};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: sharpen-sweep [--crop <out.png>] <raw files...>");
        std::process::exit(2);
    }

    // `--crop out.png` writes a 1:1 strip instead of a table: the same patch at four
    // amounts, side by side, so the defaults can be *looked at*. The numbers below
    // bound the answer and cannot settle it — a knee in a ratio is not the same as a
    // print you would hang, and this app's whole habit is to put the picture in front
    // of the person deciding.
    let crop_to = args.first().is_some_and(|a| a == "--crop").then(|| {
        args.remove(0);
        args.remove(0)
    });

    for path in &args {
        let name = path.rsplit('/').next().unwrap_or(path);
        let Ok(sensor) = raw_core::SensorImage::load(std::path::Path::new(path)) else {
            println!("{name}\n  FAILED to load");
            continue;
        };
        let (scene, _) = raw_core::scene::to_scene(&sensor, false);
        let luma = raw_core::scene::derive_luminance(
            &scene,
            raw_core::scene::Sampling::default(),
            raw_core::scene::Weighting::default(),
        );
        let (w, h) = (luma.output_dims.w, luma.output_dims.h);
        // The export's own first step, so the numbers below are in the units the module
        // actually runs in: display-referred, nominally [0, 1].
        let base: Vec<f32> = luma
            .data
            .iter()
            .map(|&v| tone_map(v, raw_core::ToneMap::default()))
            .collect();

        if let Some(out) = &crop_to {
            write_strip(&base, w, h, out);
            println!("{name}  {w} x {h}  ->  {out}");
            continue;
        }

        println!("{name}  {w} x {h}");
        println!(
            "  {:>7} {:>7} {:>7}   {:>8} {:>9} {:>7}",
            "amount", "radius", "edges", "texture", "overshoot", "ratio"
        );

        let reference = texture(&base, w, h);
        let row = |p: SharpenParams| {
            let out = sharpen::apply(&base, w, h, &p);
            let t = texture(&out, w, h) / reference;
            let o = overshoot(&base, &out, w, h);
            println!(
                "  {:>7.2} {:>7.2} {:>7.2}   {:>8.3} {:>9.5} {:>7.1}",
                p.amount,
                p.radius,
                p.edges,
                t,
                o,
                if o > 1e-9 {
                    (t - 1.0) / o
                } else {
                    f32::INFINITY
                }
            );
        };

        // The shield's whole range at the default strength. `0` is the prototype's
        // default and the unshielded behaviour; the question is what it costs.
        println!("  -- edges, at the default amount and radius --");
        for edges in [0.0, 0.25, 0.5, 0.75, 1.0] {
            row(SharpenParams {
                enabled: true,
                edges,
                ..Default::default()
            });
        }

        // And the strength, so the trade can be read at more than one point on it —
        // a shield that only helps at one amount is not a shield.
        println!("  -- amount, at the default radius --");
        for amount in [0.5, 0.75, 1.0, 1.5, 2.0] {
            row(SharpenParams {
                enabled: true,
                amount,
                ..Default::default()
            });
        }

        println!("  -- radius, at the default amount --");
        for radius in [0.5, 1.0, 2.0, 4.0] {
            row(SharpenParams {
                enabled: true,
                radius,
                ..Default::default()
            });
        }
    }
}

/// Mean absolute neighbour difference — how much local detail the picture carries.
fn texture(v: &[f32], w: usize, h: usize) -> f32 {
    let mut acc = 0.0f64;
    let mut n = 0u64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let c = v[y * w + x];
            acc += ((c - v[y * w + x + 1]).abs() + (c - v[(y + 1) * w + x]).abs()) as f64;
            n += 2;
        }
    }
    (acc / n.max(1) as f64) as f32
}

/// How far pixels are pushed past the local range they started in.
///
/// The 99.9th percentile rather than the maximum: one pixel at a specular highlight is
/// not a halo, and a maximum over forty megapixels is a measure of the noisiest pixel
/// in the frame rather than of the module.
fn overshoot(before: &[f32], after: &[f32], w: usize, h: usize) -> f32 {
    let mut over = Vec::with_capacity((w - 2) * (h - 2));
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            // The 3x3 range the pixel started inside. Anything past it is new contrast
            // the sharpener invented rather than detail it recovered.
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for dy in 0..3 {
                for dx in 0..3 {
                    let v = before[(y + dy - 1) * w + (x + dx - 1)];
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            let a = after[y * w + x];
            over.push((a - hi).max(lo - a).max(0.0));
        }
    }
    over.sort_by(|a, b| a.partial_cmp(b).unwrap());
    over[(over.len() as f64 * 0.999) as usize % over.len()]
}

/// Four amounts over one patch, side by side at 1:1, as an 8-bit PNG.
///
/// The patch is taken from the busiest 512x384 region the frame has — measured, not
/// centred — because a sharpening comparison over a smooth sky shows nothing and the
/// middle of a frame is as likely to be sky as anything else.
///
/// **sRGB-ish gamma, not L\***, so this looks right in an image viewer. It is a picture
/// to judge by eye and not a file to measure, and that difference is the same one the
/// loupe makes when it sends its tile to a monitor.
fn write_strip(base: &[f32], w: usize, h: usize, out: &str) {
    const CW: usize = 512;
    const CH: usize = 384;
    let (cw, ch) = (CW.min(w), CH.min(h));

    // The busiest patch, on a coarse grid so this stays cheap on a 40 MP frame.
    let (mut best, mut at) = (-1.0f32, (0usize, 0usize));
    let step = 64;
    let mut y = 0;
    while y + ch <= h {
        let mut x = 0;
        while x + cw <= w {
            let mut acc = 0.0f32;
            for yy in (y..y + ch).step_by(4) {
                for xx in (x..x + cw - 1).step_by(4) {
                    acc += (base[yy * w + xx] - base[yy * w + xx + 1]).abs();
                }
            }
            if acc > best {
                best = acc;
                at = (x, y);
            }
            x += step;
        }
        y += step;
    }

    let amounts = [0.0f32, 0.5, 0.75, 1.5];
    const GUTTER: usize = 8;
    let sw = cw * amounts.len() + GUTTER * (amounts.len() - 1);
    let mut strip = vec![0u8; sw * ch];

    for (i, &amount) in amounts.iter().enumerate() {
        let p = SharpenParams {
            enabled: amount > 0.0,
            amount,
            ..Default::default()
        };
        // Sharpened on the WHOLE frame and then cut, not cut and then sharpened — the
        // patch has no apron, and cutting first would put this example's own edge
        // artefacts into a picture about edge artefacts.
        let full = sharpen::apply(base, w, h, &p);
        let x0 = i * (cw + GUTTER);
        for yy in 0..ch {
            for xx in 0..cw {
                let v = full[(at.1 + yy) * w + at.0 + xx].clamp(0.0, 1.0);
                strip[yy * sw + x0 + xx] = (v.powf(1.0 / 2.2) * 255.0 + 0.5) as u8;
            }
        }
    }

    let file = std::fs::File::create(out).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), sw as u32, ch as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("header")
        .write_image_data(&strip)
        .expect("write");
    println!(
        "  patch at ({}, {}), amounts {amounts:?} left to right",
        at.0, at.1
    );
}
