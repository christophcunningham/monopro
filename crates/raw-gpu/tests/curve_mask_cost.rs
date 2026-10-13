//! What a masked Curve costs, as a number rather than as a hope.
//!
//! **`#[ignore]`d**, for the reason `dodgeburn_cost` is: it is a measurement, not an
//! assertion, and a threshold would be a flaky test rather than a guard. Run it
//! deliberately, in release, since a debug shader driver is not what a person sees:
//!
//! ```text
//! cargo test --release -p raw-gpu --test curve_mask_cost -- --ignored --nocapture
//! ```
//!
//! It answers three questions the design makes claims about:
//!
//! - **What the shader pays per masked pass.** A gradient segment is a few dozen ALU
//!   operations and two table reads per pixel, and an unmasked stack must cost what it
//!   always did, because it never leaves the original single-table code.
//! - **What the CPU pays when something moves.** Dragging a mask handle re-sends a few
//!   bytes of segments; dragging a curve point or an opacity re-bakes the tables. The two
//!   columns are why `CurveStack::same_luts` is split from `same_render`.
//! - **What a tone range adds.** A tone part is one more bilinear fetch per masked pixel
//!   and a mask for the CPU worker to prepare once per change to the range, the exposure
//!   or Contrast Mask. Dragging a shape's handle under a tone must cost what it does
//!   without one, because it asks the worker for nothing.
//!
//! - **What a painted mask costs.** Its strokes are scanned by the curve pass with the
//!   cull Dodge & Burn's layers use, so the cost should stay roughly flat as a mask
//!   gathers dabs, and a stack with nothing painted must not pay for the scan at all.
//! - **What a click of the eyedropper costs.** It is the one thing here that waits on
//!   the calling thread: the viewport's basis, fetched blocking, then the image the range
//!   reads (the guided filter when it is edge-aware) and the sample itself. It is paid
//!   once per click and never per frame, and this is the number that says a click can
//!   afford it. The point sampler's version adds one prepared mask for each tone range
//!   above the selected pass.
//!
//! The numbers are the machine's and the adapter's, so none are quoted here: take them
//! on the hardware the question is about, with a large raw, as `./run --release` does.

use raw_core::composition::{Frame, Orientation};
use raw_core::curve::CurveMask;
use raw_core::dodgeburn::{Dab, Gesture, Linear, Nib, Radial, Shape, ZoneMask};
use raw_core::{CurveStack, Dims, LumaImage, Params};
use raw_gpu::{GpuContext, ViewGeometry, Viewport, headless_device};

fn sky_grad(i: usize) -> Shape {
    Shape::Linear(Linear {
        x0: 0.5,
        y0: 0.05 + 0.01 * i as f32,
        x1: 0.5,
        y1: 0.5,
        feather: 1.0,
        ev: 0.0,
    })
}

fn spot(i: usize) -> Shape {
    Shape::Radial(Radial {
        cx: 0.3 + 0.05 * i as f32,
        cy: 0.6,
        inner: 0.05,
        outer: 0.3,
        aspect: 0.7,
        angle: 20.0,
        feather: 0.8,
        invert: false,
        ev: 0.0,
    })
}

/// `n` passes, each lifting a different tone, the first `masked` of them limited to a
/// shape, and with a tone range inside it too when `tone` is set. Alternating Linear and
/// Radial, so both branches are in the measurement, and a different band for each so each
/// has a layer of its own.
fn stack(n: usize, masked: usize, tone: bool) -> CurveStack {
    let mut stack = CurveStack::default();
    for i in 0..n {
        if i > 0 {
            stack.add_instance().expect("room");
        }
        stack.instances[i]
            .curve
            .add(0.2 + 0.6 * i as f32 / n.max(1) as f32, 0.5);
        if i < masked {
            let shape = if i % 2 == 0 { sky_grad(i) } else { spot(i) };
            let mut mask = CurveMask::of_shape(shape);
            if tone {
                mask.tone = Some(ZoneMask {
                    enabled: true,
                    lo: -3.0 + 0.2 * i as f32,
                    hi: 1.0,
                    edge_aware: true,
                    ..ZoneMask::default()
                });
            }
            stack.instances[i].mask = Some(mask);
        }
    }
    stack
}

#[test]
#[ignore = "a measurement, not an assertion"]
fn what_a_masked_curve_costs() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let (w, h) = (6000usize, 4000usize);
    let luma = LumaImage {
        data: (0..w * h)
            .map(|i| 0.02 + 0.9 * ((i % w) as f32 / w as f32))
            .collect(),
        output_dims: Dims { w, h },
        source_dims: Dims { w, h },
        clipped: Vec::new(),
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &Default::default());
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let view = ViewGeometry {
        scale: 0.4,
        ..Default::default()
    };
    let frames = 20;
    let wait = || {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
    };

    println!("  passes  masked  tone   what moves          ms/frame");
    for (n, masked, tone) in [
        (1usize, 0usize, false),
        (3, 0, false),
        (1, 1, false),
        (3, 1, false),
        (3, 3, false),
        (8, 8, false),
        (16, 16, false),
        (3, 3, true),
        (8, 8, true),
        (16, 16, true),
    ] {
        let mut p = Params {
            curve: stack(n, masked, tone),
            ..Default::default()
        };
        // Warm: allocate the targets and bake the tables once, so none of it is in the
        // first timed frame.
        vp.render(&mut ctx, &device, &queue, 2560, 1600, view, &p, &frame);
        wait();

        for moving in ["a mask handle", "a curve opacity"] {
            // A mask has to exist to be moved.
            if moving == "a mask handle" && masked == 0 {
                continue;
            }
            let t = std::time::Instant::now();
            for i in 0..frames {
                // Nudge something every frame, or change detection short-circuits the
                // dispatch and this measures nothing at all.
                let nudge = i as f32 * 1e-3;
                if moving == "a mask handle" {
                    if let Some(Shape::Linear(l)) =
                        p.curve.instances[0].mask.as_mut().and_then(|m| m.shape.as_mut())
                    {
                        l.x0 = 0.5 + nudge;
                    } else if let Some(Shape::Radial(r)) =
                        p.curve.instances[0].mask.as_mut().and_then(|m| m.shape.as_mut())
                    {
                        r.cx = 0.3 + nudge;
                    }
                } else {
                    p.curve.instances[0].opacity = 1.0 - nudge;
                }
                vp.render(&mut ctx, &device, &queue, 2560, 1600, view, &p, &frame);
            }
            wait();
            let ms = t.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
            println!(
                "  {n:6}  {masked:6}  {:<5}  {moving:<17} {ms:9.2}",
                if tone { "yes" } else { "no" }
            );
        }
    }
}

#[test]
#[ignore = "a measurement, not an assertion"]
fn what_a_sample_of_the_picture_costs() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    // Tones with structure, so the guided filter has edges to keep: a ramp with bars.
    let (w, h) = (6000usize, 4000usize);
    let luma = LumaImage {
        data: (0..w * h)
            .map(|i| {
                let bar = if (i % w) / 300 % 2 == 0 { 1.0 } else { 0.35 };
                (0.02 + 0.9 * ((i / w) as f32 / h as f32)) * bar
            })
            .collect(),
        output_dims: Dims { w, h },
        source_dims: Dims { w, h },
        clipped: Vec::new(),
    };
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = Params::default();
    let ms = |t: std::time::Instant, n: u32| t.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
    let rounds = 20u32;

    println!("  what a click waits for                         ms");
    // The first fetch builds the basis; every later one at this exposure finds it.
    let t = std::time::Instant::now();
    let basis = vp.zone_basis(&p);
    println!("  the basis, built                          {:9.2}", ms(t, 1));
    let t = std::time::Instant::now();
    for _ in 0..rounds {
        vp.zone_basis(&p);
    }
    println!("  the basis, already there                  {:9.2}", ms(t, rounds));
    // A changed exposure is a basis the worker has not made: the wait a click can meet.
    let t = std::time::Instant::now();
    for i in 0..rounds {
        p.exposure.ev = 0.01 * (i + 1) as f32;
        vp.zone_basis(&p);
    }
    println!("  the basis, after exposure moved           {:9.2}", ms(t, rounds));

    let box_points: Vec<(f32, f32)> = (0..48 * 48)
        .map(|i| (0.3 + 0.3 * (i % 48) as f32 / 48.0, 0.3 + 0.3 * (i / 48) as f32 / 48.0))
        .collect();
    for (name, edge_aware) in [("per pixel", false), ("edge-aware", true)] {
        let mut tone = ZoneMask {
            edge_aware,
            ..ZoneMask::default()
        };
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            basis.sample_into(&mut tone, [(0.5, 0.5)], false);
        }
        println!("  a click, {name:<10}                      {:9.2}", ms(t, rounds));
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            basis.sample_into(&mut tone, box_points.iter().copied(), false);
        }
        println!("  a box of 48 x 48, {name:<10}             {:9.2}", ms(t, rounds));
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(basis.weight_at(&tone, 0.5, 0.5));
        }
        println!("  one range's weight at a point, {name:<10}{:9.2}", ms(t, rounds));
    }
}

#[test]
#[ignore = "a measurement, not an assertion"]
fn what_a_painted_mask_costs() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let (w, h) = (6000usize, 4000usize);
    let luma = LumaImage {
        data: (0..w * h)
            .map(|i| 0.02 + 0.9 * ((i % w) as f32 / w as f32))
            .collect(),
        output_dims: Dims { w, h },
        source_dims: Dims { w, h },
        clipped: Vec::new(),
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &Default::default());
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let view = ViewGeometry {
        scale: 0.4,
        ..Default::default()
    };
    let frames = 20;
    let wait = || {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
    };
    // Strokes spread over the frame, fifty dabs to a pass, as a worked mask has them.
    let dab = |i: usize| Dab {
        x: 0.05 + 0.9 * ((i * 37) % 1000) as f32 / 1000.0,
        y: 0.05 + 0.9 * ((i * 91) % 1000) as f32 / 1000.0,
        radius: 0.06,
        feather: 0.4,
        opacity: 1.0,
        ev: 1.0,
        ..Dab::ROUND
    };
    let painted = |dabs: usize| -> Vec<Gesture> {
        (0..dabs.div_ceil(50))
            .map(|p| Gesture::new((0..50.min(dabs - p * 50)).map(|i| dab(p * 50 + i)).collect()))
            .collect()
    };

    println!("    dabs   what moves              ms/frame");
    for dabs in [0usize, 50, 200, 1000, 2000, 5000] {
        let mut curve = stack(1, 0, false);
        let mut passes = painted(dabs);
        if dabs > 0 {
            curve.instances[0].mask = Some(CurveMask::of_shape(Shape::Brush {
                nib: Nib::Round,
                passes: passes.clone(),
            }));
        }
        let mut p = Params {
            curve,
            ..Default::default()
        };
        vp.render(&mut ctx, &device, &queue, 2560, 1600, view, &p, &frame);
        wait();
        for moving in ["a stroke in progress", "a curve opacity"] {
            if moving == "a stroke in progress" && dabs == 0 {
                continue;
            }
            let t = std::time::Instant::now();
            for i in 0..frames {
                if moving == "a stroke in progress" {
                    // One more dab on the newest pass every frame, as a drag lays them.
                    passes.last_mut().unwrap().dabs_mut().push(dab(9000 + i));
                    if let Some(Shape::Brush { passes: held, .. }) =
                        p.curve.instances[0].mask.as_mut().and_then(|m| m.shape.as_mut())
                    {
                        *held = passes.clone();
                    }
                } else {
                    p.curve.instances[0].opacity = 1.0 - i as f32 * 1e-3;
                }
                vp.render(&mut ctx, &device, &queue, 2560, 1600, view, &p, &frame);
            }
            wait();
            let ms = t.elapsed().as_secs_f64() * 1000.0 / frames as f64;
            println!("  {dabs:6}   {moving:<22} {ms:9.2}");
        }
    }
}
