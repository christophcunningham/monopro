//! Measure whether the demosaic algorithms actually differ **in mono**.
//!
//! This exists because of a decided rule: ship only the algorithms that visibly
//! differ after the weighted sum to grey, and drop the rest rather than offer a menu
//! that promises a difference it does not deliver. That is a measurement, and this
//! is the instrument.
//!
//! ```text
//! cargo run --release -p raw-core --example demosaic-compare -- <raw>...
//!   [--crop X,Y,W,H] [--out DIR]
//! ```
//!
//! With `--crop`, each algorithm's rendering of that region is written as a PNG,
//! along with an **amplified difference map** for each pair. The statistics say
//! whether two algorithms differ; only the pictures say whether the difference is
//! an improvement, and which one is the improvement.
//!
//! # The metric, and why it is this one
//!
//! Differences are reported in **8-bit display levels**, not in scene-linear units.
//! A scene-linear difference is not a perceptual quantity: 0.001 is invisible in the
//! highlights and obvious in the shadows, because the display transform stretches
//! the bottom of the range. Encoding both images through the same gamma first and
//! then differencing puts the numbers in the space the eye actually judges, and one
//! level is the smallest difference the output can even represent.
//!
//! So: **a pair whose 99.9th percentile is below one level is indistinguishable** in
//! any print or on any screen, however different the two algorithms are on paper.
//! The percentile rather than the max, because a handful of pixels at a specular
//! edge is not what "visibly differs" means; the max is reported alongside so a
//! localised-but-severe difference is not hidden.

use std::path::Path;

use raw_core::scene::{self, DemosaicAlgo, Sampling, Weighting};
use raw_core::sensor::SensorImage;

/// The transfer function the viewport uses. Matches `display.wgsl`'s default.
const GAMMA: f32 = 2.2;

fn encode(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(1.0 / GAMMA) * 255.0
}

struct Stats {
    rms: f32,
    p999: f32,
    max: f32,
    over_one: f32,
    over_two: f32,
}

fn compare(a: &[f32], b: &[f32]) -> Stats {
    let mut diffs: Vec<f32> = a
        .iter()
        .zip(b)
        .map(|(x, y)| (encode(*x) - encode(*y)).abs())
        .collect();
    let sumsq: f64 = diffs.iter().map(|d| (*d as f64) * (*d as f64)).sum();
    let rms = (sumsq / diffs.len() as f64).sqrt() as f32;
    let over_one = diffs.iter().filter(|d| **d > 1.0).count() as f32 / diffs.len() as f32;
    let over_two = diffs.iter().filter(|d| **d > 2.0).count() as f32 / diffs.len() as f32;
    diffs.sort_by(f32::total_cmp);
    let p999 = diffs[(diffs.len() as f64 * 0.999) as usize];
    let max = *diffs.last().expect("non-empty");
    Stats {
        rms,
        p999,
        max,
        over_one,
        over_two,
    }
}

/// A region of the frame, in source pixels.
#[derive(Clone, Copy)]
struct Crop {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Write an 8-bit grey PNG.
fn write_png(path: &Path, w: usize, h: usize, grey: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("header")
        .write_image_data(grey)
        .expect("write");
}

/// Pull a crop out of a full-frame luma image and display-encode it.
fn crop_of(data: &[f32], stride: usize, c: Crop) -> Vec<u8> {
    let mut out = Vec::with_capacity(c.w * c.h);
    for row in c.y..c.y + c.h {
        for col in c.x..c.x + c.w {
            out.push(encode(data[row * stride + col]).round().clamp(0.0, 255.0) as u8);
        }
    }
    out
}

/// The difference between two crops, amplified so a one-level difference is
/// visible. Without the gain the map is uniformly black and proves nothing.
const DIFF_GAIN: f32 = 16.0;

fn diff_of(a: &[f32], b: &[f32], stride: usize, c: Crop) -> Vec<u8> {
    let mut out = Vec::with_capacity(c.w * c.h);
    for row in c.y..c.y + c.h {
        for col in c.x..c.x + c.w {
            let i = row * stride + col;
            let d = (encode(a[i]) - encode(b[i])).abs() * DIFF_GAIN;
            out.push(d.clamp(0.0, 255.0) as u8);
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let crop = flag("--crop").map(|s| {
        let n: Vec<usize> = s
            .split(',')
            .map(|v| v.parse().expect("--crop X,Y,W,H"))
            .collect();
        assert_eq!(n.len(), 4, "--crop needs X,Y,W,H");
        Crop {
            x: n[0],
            y: n[1],
            w: n[2],
            h: n[3],
        }
    });
    let out_dir = flag("--out").unwrap_or_else(|| ".".to_owned());
    // Everything that is neither a flag nor a flag's value.
    let mut files: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            i += 2; // every flag here takes a value
        } else {
            files.push(args[i].clone());
            i += 1;
        }
    }
    if files.is_empty() {
        eprintln!("usage: demosaic-compare <raw>... [--crop X,Y,W,H] [--out DIR]");
        std::process::exit(2);
    }

    let algos = DemosaicAlgo::UI_ORDER;

    for file in &files {
        let sensor = match SensorImage::load(Path::new(file)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{file}: load failed: {e}");
                continue;
            }
        };
        let (sc, _) = scene::decode(&sensor, Default::default());
        println!(
            "\n=== {} · {} · {} x {}",
            Path::new(file)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            sc.camera,
            sc.geom.crop.w,
            sc.geom.crop.h
        );

        let mut rendered = Vec::new();
        for algo in algos {
            let t = std::time::Instant::now();
            let luma = scene::derive_luminance(&sc, Sampling::Demosaic(algo), Weighting::Photosite);
            let ms = t.elapsed().as_millis();
            let mp = (luma.output_dims.w * luma.output_dims.h) as f64 / 1.0e6;
            println!(
                "  {:<10} {:>6} ms   ({:.1} MP, {:.0} MP/s)",
                algo.label(),
                ms,
                mp,
                mp / (ms.max(1) as f64 / 1000.0)
            );
            rendered.push((algo, luma.data));
        }

        let stride = sc.geom.crop.w;
        let stem = Path::new(file)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Some(c) = crop {
            assert!(
                c.x + c.w <= stride && c.y + c.h <= sc.geom.crop.h,
                "crop {},{} {}x{} is outside a {}x{} frame",
                c.x,
                c.y,
                c.w,
                c.h,
                stride,
                sc.geom.crop.h
            );
            for (algo, data) in &rendered {
                let p = Path::new(&out_dir).join(format!("{stem}-{}.png", algo.label()));
                write_png(&p, c.w, c.h, &crop_of(data, stride, c));
                println!("  wrote {}", p.display());
            }
        }

        println!("\n  pair                     rms    p99.9     max   >1lvl   >2lvl   verdict");
        for i in 0..rendered.len() {
            for j in i + 1..rendered.len() {
                let (a, ad) = &rendered[i];
                let (b, bd) = &rendered[j];
                let s = compare(ad, bd);
                if let Some(c) = crop {
                    let p = Path::new(&out_dir).join(format!(
                        "{stem}-diff-{}-{}.png",
                        a.label(),
                        b.label()
                    ));
                    write_png(&p, c.w, c.h, &diff_of(ad, bd, stride, c));
                }
                // The decision rule, applied rather than left to the reader.
                let verdict = if s.p999 < 1.0 {
                    "indistinguishable"
                } else if s.over_one < 0.001 {
                    "marginal"
                } else {
                    "DIFFERS"
                };
                println!(
                    "  {:<10} vs {:<10} {:5.2}  {:6.2}  {:6.1}  {:5.2}%  {:5.2}%   {}",
                    a.label(),
                    b.label(),
                    s.rms,
                    s.p999,
                    s.max,
                    s.over_one * 100.0,
                    s.over_two * 100.0,
                    verdict
                );
            }
        }
    }
}
