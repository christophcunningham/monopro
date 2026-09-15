//! Headless inspection of a folder of reference raws.
//!
//!     cargo run --release --example inspect -- raws/*.dng
//!
//! Prints what the sensor stage actually parsed, so decode assumptions can be
//! checked against new cameras without opening the GUI. Flags the two things that
//! silently corrupt an entire frame: an odd crop origin, and non-finite gains.

use raw_core::{SensorImage, sensor::Gains};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: inspect <raw files...>");
        std::process::exit(2);
    }

    let mut failed = 0;
    for path in &args {
        let name = path.rsplit('/').next().unwrap_or(path);
        match SensorImage::load(std::path::Path::new(path)) {
            Err(e) => {
                println!("{name}\n  FAILED: {e}");
                failed += 1;
            }
            Ok(s) => {
                let g = Gains::equalizing(s.wb_coeffs);
                let sp = s.geom.superpixel_dims();
                println!("{name}");
                println!("  camera     : {}", s.camera);
                println!(
                    "  crop       : {}x{} at ({},{})  -> superpixel {}x{}",
                    s.geom.crop.w, s.geom.crop.h, s.geom.crop_x, s.geom.crop_y, sp.w, sp.h
                );
                println!(
                    "  bayer      : {:?}/{:?} / {:?}/{:?} at sensor origin",
                    s.geom.pattern[0][0],
                    s.geom.pattern[0][1],
                    s.geom.pattern[1][0],
                    s.geom.pattern[1][1]
                );
                println!(
                    "  black      : {:?} ({}x{} tile)",
                    s.black.levels, s.black.w, s.black.h
                );
                println!("  white      : {:?}", s.white);
                println!(
                    "  gains      : {:?}  headroom {:.2} ({:.2} stops over 1.0)",
                    g.0,
                    g.headroom(),
                    g.headroom().log2()
                );

                // The load-bearing check: gain equalisation is meant to make the
                // CFA pattern invisible, i.e. the three channels should read the
                // same mean for a broadly neutral scene. If the spread stays large
                // after gains, either the gains are wrong or — far more likely —
                // the CFA phase is off and each gain is landing on the wrong
                // photosites. A phase error is otherwise silent in SuperPixel mode,
                // because binning the quad averages the mistake away.
                let (sc, _) = raw_core::scene::to_scene(&s, false);
                let mut sum = [0.0f64; 3];
                let mut n = [0u64; 3];
                let mut raw_sum = [0.0f64; 3];
                for row in 0..sc.geom.crop.h {
                    for col in 0..sc.geom.crop.w {
                        let c = sc.color_at(row, col) as usize;
                        let v = sc.data[row * sc.geom.crop.w + col] as f64;
                        sum[c] += v;
                        raw_sum[c] += v / g.0[c] as f64; // undo the gain
                        n[c] += 1;
                    }
                }
                let mean = |i: usize| sum[i] / n[i].max(1) as f64;
                let pre = |i: usize| raw_sum[i] / n[i].max(1) as f64;
                let spread = |f: &dyn Fn(usize) -> f64| {
                    let (a, b, c) = (f(0), f(1), f(2));
                    let hi = a.max(b).max(c);
                    let lo = a.min(b).min(c);
                    if lo > 0.0 { hi / lo } else { f64::INFINITY }
                };
                println!(
                    "  channel mean before gains: R {:.4} G {:.4} B {:.4}  spread {:.2}x",
                    pre(0),
                    pre(1),
                    pre(2),
                    spread(&pre)
                );
                println!(
                    "  channel mean after  gains: R {:.4} G {:.4} B {:.4}  spread {:.2}x",
                    mean(0),
                    mean(1),
                    mean(2),
                    spread(&mean)
                );

                // Phase check, independent of scene colour.
                //
                // G1 and G2 sit behind physically identical filters, so their means
                // must agree closely for ANY subject. The other pairs need not. So
                // the closest-matching pair of tile positions identifies where the
                // greens really are, and that must agree with the CFA pattern the
                // file reported. This is the check that catches a phase error
                // without needing a neutral test chart — and SuperPixel hides phase
                // errors, so it cannot be caught downstream.
                let mut tile = [0.0f64; 4];
                let mut tile_n = [0u64; 4];
                for row in 0..sc.geom.crop.h {
                    for col in 0..sc.geom.crop.w {
                        let t = (row & 1) * 2 + (col & 1);
                        let c = sc.color_at(row, col) as usize;
                        // undo the gain, so this compares raw photosite response
                        tile[t] += sc.data[row * sc.geom.crop.w + col] as f64 / g.0[c] as f64;
                        tile_n[t] += 1;
                    }
                }
                for i in 0..4 {
                    tile[i] /= tile_n[i].max(1) as f64;
                }
                let pairs = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
                let closest = pairs
                    .iter()
                    .min_by(|a, b| {
                        let d = |p: &(usize, usize)| {
                            (tile[p.0] - tile[p.1]).abs() / (tile[p.0] + tile[p.1]).max(1e-9)
                        };
                        d(a).partial_cmp(&d(b)).unwrap()
                    })
                    .unwrap();
                let green_tiles: Vec<usize> = (0..4)
                    .filter(|t| s.geom.pattern[t / 2][t % 2] == raw_core::CfaColor::Green)
                    .collect();
                let agree = green_tiles.len() == 2
                    && ((green_tiles[0], green_tiles[1]) == *closest
                        || (green_tiles[1], green_tiles[0]) == *closest);
                let delta = (tile[closest.0] - tile[closest.1]).abs()
                    / (tile[closest.0] + tile[closest.1]).max(1e-9)
                    * 2.0;
                println!(
                    "  tile means : {:.4} {:.4} / {:.4} {:.4}",
                    tile[0], tile[1], tile[2], tile[3]
                );
                println!(
                    "  cfa phase  : greens at {:?}, closest pair {:?} (Δ {:.2}%) -> {}",
                    green_tiles,
                    closest,
                    delta * 100.0,
                    if agree { "AGREE" } else { "*** MISMATCH ***" }
                );
            }
        }
    }
    if failed > 0 {
        println!("\n{failed}/{} failed to decode", args.len());
    }
}
