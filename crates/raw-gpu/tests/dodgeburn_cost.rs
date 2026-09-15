//! What the Dodge & Burn pass costs, as a number rather than as a hope.
//!
//! **`#[ignore]`d**: it is a measurement, not an assertion. Timings vary with the
//! adapter and with whatever else the machine is doing, so a threshold here would
//! be a flaky test rather than a guard. Run it deliberately:
//!
//! ```text
//! cargo test -p raw-gpu --test dodgeburn_cost -- --ignored --nocapture
//! ```
//!
//! It exists because the shader's whole structure — a workgroup-cooperative cull
//! into a bitmask, rather than one thread looping every dab — was chosen on the
//! strength of these numbers, and a later reader should be able to re-take them
//! rather than trust a comment. Measured on Apple silicon, 6000x4000 negative, a
//! 2560x1600 viewport at fit:
//!
//! ```text
//!            naive     culled
//!    1 dab    2.3 ms    2.5 ms
//!  200 dabs   9.6 ms    1.5 ms
//! 1000 dabs  44.1 ms    3.0 ms
//! 2000 dabs  87.6 ms    4.8 ms
//! ```
//!
//! The naive column is linear in dab count and reaches 22 fps at a thousand dabs,
//! which is an afternoon's work on one print. The culled column is what makes the
//! tool hold up the more it is used.

use raw_core::composition::{Frame, Orientation};
use raw_core::dodgeburn::{Dab, DodgeBurnParams, Gesture, Instance, Shape, Sign};
use raw_core::{Dims, LumaImage, Params};
use raw_gpu::{GpuContext, ViewGeometry, Viewport, headless_device};

#[test]
#[ignore = "a measurement, not an assertion"]
fn what_a_stroke_list_costs_per_frame() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let (w, h) = (6000usize, 4000usize);
    let luma = LumaImage {
        data: vec![0.25; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w, h },
        clipped: Vec::new(),
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &Default::default());
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);

    // Two spreads: strokes scattered over the frame, and a dense scribble in one
    // corner. The second is the case the cull cannot help with, and is the one to
    // watch if this ever needs revisiting.
    for (n, spread) in [
        (1usize, 0.8f32),
        (50, 0.8),
        (200, 0.8),
        (500, 0.8),
        (1000, 0.8),
        (2000, 0.8),
        (1000, 0.02),
    ] {
        let dodgeburn = DodgeBurnParams {
            enabled: true,
            instances: vec![Instance::of(
                Sign::Burn,
                "Burn 1".into(),
                Shape::brush(vec![Gesture::new(
                    (0..n)
                        .map(|i| Dab {
                            x: 0.1 + (i as f32 * 0.013).fract() * spread,
                            y: 0.1 + (i as f32 * 0.037).fract() * spread,
                            radius: 0.05,
                            feather: 0.4,
                            opacity: 1.0,
                            ev: -0.3,
                            ..Dab::ROUND
                        })
                        .collect(),
                )]),
            )],
        };
        let mut p = Params {
            dodgeburn,
            ..Default::default()
        };
        let view = ViewGeometry {
            scale: 0.4,
            ..Default::default()
        };
        let frames = 20;
        let t = std::time::Instant::now();
        for i in 0..frames {
            // Nudge something every frame, or change detection short-circuits the
            // dispatch and this measures nothing at all.
            p.dodgeburn.instances[0].opacity = 1.0 - i as f32 * 1e-4;
            vp.render(&mut ctx, &device, &queue, 2560, 1600, view, &p, &frame);
        }
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        let ms = t.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        println!("{n:5} dabs  spread {spread:4}   {ms:7.2} ms/frame");
    }
}
