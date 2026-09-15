//! What the emulsion costs, as a number rather than as a hope.
//!
//! **`#[ignore]`d**: it is a measurement, not an assertion. A threshold here would
//! be a flaky test rather than a guard. Run it deliberately:
//!
//! ```text
//! cargo test -p raw-core --release --test grain_cost -- --ignored --nocapture
//! ```
//!
//! `--release` matters more here than anywhere else in the workspace: the layer loop
//! is arithmetic in a tight loop and the dev profile's `opt-level = 1` costs roughly
//! an order of magnitude, so a debug figure would answer a question nobody asked.
//!
//! It exists because two decisions rest on these numbers and both would otherwise be
//! guesses: whether the sparse scatter was worth writing, and whether grain can be
//! shown anywhere other than the loupe.
//!
//! # Measured on Apple silicon, release
//!
//! ```text
//!                        5 layers   30 layers   60 layers
//!   loupe tile   0.16 MP     15 ms       24 ms       44 ms
//!   viewport      4.1 MP     44 ms      250 ms      493 ms
//!   24 MP export   24 MP    250 ms      1.45 s       2.8 s
//!   41 MP export   41 MP    420 ms      2.37 s       4.8 s
//! ```
//!
//! **The loupe grew to 500 px** — `raw_app::loupe::SIZE`, the maintainer's ask — and the tile
//! row was re-measured rather than scaled:
//!
//! ```text
//!   loupe tile   0.25 MP      8 ms       37 ms       68 ms
//! ```
//!
//! 37 ms at the default thirty layers, against 24 at 400. That is the cost of a 56%
//! larger tile arriving where the linear-in-pixels model above says it should, and it
//! is still a window that appears rather than one you wait for.
//!
//! **The two runs are two runs**, which the 5-layer column shows plainly: 8 ms for the
//! bigger tile against 15 for the smaller. At five layers the figure is mostly fixed
//! overhead and thermal state, not work. Compare rows *within* a run; the whole table
//! moved by up to a third between these two.
//!
//! Two things fall out of this, and both were worth measuring rather than assuming.
//!
//! **Crystal size does not affect the cost.** 5 px, 11 px and 21 px are within noise
//! of each other at every size — which looks wrong until you write out what the
//! sparse scatter does. Its work is `nnz x area`, the seeding probability is
//! `sigma_fill / area`, so `nnz` falls exactly as fast as `area` rises and the
//! product is constant. A bigger crystal is fewer, larger stamps. The gather form
//! would have been quadratic in the size instead.
//!
//! **What is left is linear in pixels x layers, at about 2 ns per pixel per layer**,
//! and that is the two full-image passes — the seeding and the deposit — not the
//! convolution. The convolution stopped being the cost the moment it went sparse.
//! Anyone tempted to optimise further should start there and not in `convolve`.
//!
//! For scale: the prototype's serial `Normal` stream alone is 2.4 billion sequential
//! samples on the 41 MP / 60 layer case. This does the whole emulsion in five
//! seconds.

use raw_core::grain::{self, GrainParams};

/// A ramp with tone in it. A flat field would seed uniformly and measure the wrong
/// thing — the seeding is proportional to exposure, so the sparse path's occupancy
/// depends on the picture.
fn ramp(w: usize, h: usize) -> Vec<f32> {
    (0..w * h)
        .map(|i| ((i % w) as f32 / (w - 1) as f32) * 0.9 + 0.05)
        .collect()
}

#[test]
#[ignore = "a measurement, not an assertion"]
fn what_the_emulsion_costs() {
    println!(
        "\n{:>14}  {:>10}  {:>7}  {:>6}  {:>10}  {:>12}",
        "what", "pixels", "layers", "size", "total", "per layer"
    );
    for (label, w, h) in [
        // `raw_app::loupe::SIZE`. Kept as a literal because raw-core cannot depend
        // on the app; if the loupe changes size, change it here too.
        ("loupe tile", 500usize, 500usize),
        ("viewport", 2560, 1600),
        ("24 MP export", 6000, 4000),
        ("41 MP export", 7872, 5208),
    ] {
        let img = ramp(w, h);
        for layers in [5u32, 30, 60] {
            for size in [5u32, 11, 21] {
                let p = GrainParams {
                    enabled: true,
                    layers,
                    size,
                    ..Default::default()
                };
                let t = std::time::Instant::now();
                let out = grain::apply(&img, w, h, &p);
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                std::hint::black_box(&out);
                println!(
                    "{label:>14}  {:>10}  {layers:>7}  {size:>6}  {ms:>8.1} ms  {:>9.2} ms",
                    w * h,
                    ms / f64::from(layers)
                );
            }
        }
        println!();
    }
}
