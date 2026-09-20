//! Headless render regression tests, through `Viewport::read_back`.
//!
//! **Why property assertions and a content hash rather than golden PNGs.** A stored
//! image tells you *that* something changed, never *what should be true*. When one
//! trips six months from now, a byte-diff against a binary blob is not evidence —
//! you cannot tell an intended tone-curve improvement from a geometry regression.
//! Each test here names the invariant it is defending, so a failure reads as a
//! sentence. `the_whole_chain_is_stable` is the catch-all hash that notices changes
//! nobody predicted; the rest say why the pixels are what they are.
//!
//! Every test runs against a synthetic `LumaImage`, so they need no raw files and
//! no binary assets in git.
//!
//! These run on whatever adapter wgpu picks. If there is none — a headless CI box
//! with no software fallback — they skip rather than fail, since a missing GPU is
//! not a regression in this crate.

use raw_core::composition::{CompositionParams, Frame, Orientation, Rect};
use raw_core::display::encode;
use raw_core::{AgxParams, Curve, Dims, LumaImage, Params, ToneMap};
use raw_gpu::{
    GpuContext, HISTOGRAM_BINS, HISTOGRAM_PROXY_EDGE, HISTOGRAM_WEIGHT, Overlays, Surround,
    ViewGeometry, Viewport, headless_device,
};

/// A flat image of one value, so any output variation is the pipeline's doing.
fn flat(value: f32, w: usize, h: usize) -> LumaImage {
    LumaImage {
        data: vec![value; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    }
}

#[test]
fn a_mask_cache_survives_downstream_edits_and_rebuilds_for_its_inputs() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(577, 385);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = masked(48.0, 0.35, luma.output_dims);
    let mut view = ONE_TO_ONE;
    let draw = |vp: &mut Viewport, ctx: &mut GpuContext, p: &Params, view| {
        vp.render(ctx, &device, &queue, 256, 192, view, p, &frame);
        vp.read_back(&device, &queue).unwrap().2
    };
    let original = draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 1);
    p.display.gamma = 1.8;
    let downstream = draw(&mut vp, &mut ctx, &p, view);
    assert_ne!(original, downstream);
    assert_eq!(vp.contrast_mask_rebuilds(), 1, "gamma rebuilt the mask");

    // Auxiliary renders must not corrupt a cached prefix's pixels or uniforms.
    vp.patch(&mut ctx, &device, &queue, &p, &frame, 220, 150, 5, 5)
        .unwrap();
    assert_eq!(draw(&mut vp, &mut ctx, &p, view), downstream);
    assert_eq!(vp.contrast_mask_rebuilds(), 1);

    p.exposure.ev = 0.25;
    draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 2);
    p.contrast_mask.spacer *= 1.2;
    draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 3);
    p.contrast_mask.offset = (7.0, -3.0);
    draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 4);
    view.off_x = 32.25;
    draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 5);
    let replacement = flat(0.18, 577, 385);
    vp.set_image(&device, &queue, &replacement);
    let replaced = draw(&mut vp, &mut ctx, &p, view);
    assert_eq!(vp.contrast_mask_rebuilds(), 6);
    let mut fresh = Viewport::new(&device, &queue, &replacement);
    assert_eq!(replaced, draw(&mut fresh, &mut ctx, &p, view));
}

#[test]
fn excessive_render_requests_allocate_nothing_and_recover() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.18, 64, 64);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = Params::default();
    for (w, h, scale) in [(u32::MAX, 64, 1.0), (8192, 8192, 256.0), (64, 64, f32::NAN)] {
        let allocations = ctx.pool().allocations();
        assert!(!vp.render(
            &mut ctx,
            &device,
            &queue,
            w,
            h,
            ViewGeometry {
                scale,
                ..ONE_TO_ONE
            },
            &p,
            &frame
        ));
        assert!(vp.render_error().is_some());
        assert!(vp.target_view().is_none());
        assert_eq!(ctx.pool().allocations(), allocations);
    }
    assert!(vp.render(&mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, &p, &frame));
    assert!(vp.render_error().is_none());
    let before = vp.read_back(&device, &queue).unwrap();
    assert!(!vp.render(
        &mut ctx,
        &device,
        &queue,
        u32::MAX,
        64,
        ONE_TO_ONE,
        &p,
        &frame
    ));
    assert_eq!(before, vp.read_back(&device, &queue).unwrap());
    vp.render(&mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, &p, &frame);
    assert!(vp.render_error().is_none());
}

#[test]
fn high_zoom_masks_keep_native_detail_and_reuse_bounded_buffers() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let mut ctx = GpuContext::new(&device);
    let luma = ramp_with_texture(1025, 769, 5, 4.0, 0.2);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = masked(80.0, 0.35, luma.output_dims);
    p.display.dither = false;
    let view = ViewGeometry {
        off_x: 320.0,
        off_y: 220.0,
        ..ONE_TO_ONE
    };
    vp.render(&mut ctx, &device, &queue, 256, 192, view, &p, &frame);
    let native = vp.read_back(&device, &queue).unwrap().2;
    for scale in [4.0, 16.0] {
        let zoomed = ViewGeometry { scale, ..view };
        assert!(vp.render(&mut ctx, &device, &queue, 256, 192, zoomed, &p, &frame));
        assert!(vp.render_error().is_none());
        let pixels = vp.read_back(&device, &queue).unwrap().2;
        let stride = scale as usize;
        for y in 0..192 / stride {
            for x in 0..256 / stride {
                let i = ((y * stride + stride / 2) * 256 + x * stride + stride / 2) * 4;
                let j = (y * 256 + x) * 4;
                assert!((i32::from(pixels[i]) - i32::from(native[j])).abs() <= 1);
            }
        }
        // Changed exposure forces a mask rebuild; fixed geometry should retain
        // its working set instead of throwing oversized textures away each frame.
        let allocations = ctx.pool().allocations();
        p.exposure.ev += 0.01;
        vp.render(&mut ctx, &device, &queue, 256, 192, zoomed, &p, &frame);
        vp.read_back(&device, &queue).unwrap();
        assert_eq!(ctx.pool().allocations(), allocations);
        p.exposure.ev -= 0.01;
    }
}

#[test]
fn auxiliary_targets_do_not_invalidate_the_live_image() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(193, 129);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut live = masked(24.0, 0.35, luma.output_dims);
    live.display.dither = false;
    let view = ViewGeometry {
        off_x: 17.25,
        off_y: 12.5,
        scale: 1.25,
        ..ONE_TO_ONE
    };
    assert!(vp.render(&mut ctx, &device, &queue, 128, 96, view, &live, &frame));
    let pixels = vp.read_back(&device, &queue).unwrap();
    assert!(vp.target_changed);
    let mut other = live.clone();
    other.exposure.ev = 0.7;
    other.curve.add(0.5, 0.7);
    other.toning.enabled = true;
    other.toning.process = raw_core::Process::Albumen;
    other.toning.apply("gold-gp1", 0.85);
    for params in [&live, &other] {
        for kind in 0..5 {
            let changed = vp.target_changed;
            let builds = vp.contrast_mask_rebuilds();
            match kind {
                0 => {
                    vp.patch(&mut ctx, &device, &queue, params, &frame, 20, 15, 3, 3)
                        .unwrap();
                }
                1 => {
                    let mut pending = vp
                        .begin_patch(&mut ctx, &device, &queue, params, &frame, 30, 25, 1, 1)
                        .unwrap();
                    // An in-flight readback must not demand a live redraw either.
                    assert!(!vp.render(&mut ctx, &device, &queue, 128, 96, view, &live, &frame));
                    device
                        .poll(wgpu::PollType::Wait {
                            submission_index: None,
                            timeout: None,
                        })
                        .unwrap();
                    assert!(pending.poll(&device, &mut ctx).unwrap().is_ok());
                }
                2 => {
                    let mut pending = vp
                        .begin_histogram(&mut ctx, &device, &queue, params, &frame)
                        .unwrap();
                    device
                        .poll(wgpu::PollType::Wait {
                            submission_index: None,
                            timeout: None,
                        })
                        .unwrap();
                    assert!(pending.poll(&device).unwrap().is_ok());
                }
                3 => {
                    vp.export(&mut ctx, &device, &queue, params, &frame, |_, _| {})
                        .unwrap();
                }
                _ => {
                    let mut cell = raw_gpu::Cell::default();
                    vp.render_into(
                        &mut cell, &mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, params, &frame,
                    );
                    assert!(cell.view().is_some());
                }
            }
            if kind != 1 {
                assert_eq!(vp.target_changed, changed);
            }
            if kind != 4 {
                assert_eq!(vp.contrast_mask_rebuilds(), builds);
            }
            let allocations = ctx.pool().allocations();
            assert!(
                !vp.render(&mut ctx, &device, &queue, 128, 96, view, &live, &frame),
                "auxiliary kind {kind} forced a redraw"
            );
            assert_eq!(ctx.pool().allocations(), allocations);
            assert_eq!(vp.read_back(&device, &queue).unwrap(), pixels);
        }
    }
    // Restoring shared GPU resources must also work when a real edit DOES draw.
    live.display.gamma = 1.7;
    assert!(vp.render(&mut ctx, &device, &queue, 128, 96, view, &live, &frame));
    let mut fresh = Viewport::new(&device, &queue, &luma);
    fresh.render(&mut ctx, &device, &queue, 128, 96, view, &live, &frame);
    assert_eq!(
        vp.read_back(&device, &queue),
        fresh.read_back(&device, &queue)
    );
}

#[test]
fn sampling_preserves_pending_source_changes_and_live_errors() {
    let Some((device, queue)) = headless_device() else {
        return;
    };
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.18, 64, 64);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = no_dither();
    vp.render(&mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, &p, &frame);
    let before = vp.read_back(&device, &queue).unwrap();
    vp.set_image(&device, &queue, &flat(0.5, 64, 64));
    vp.patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 1, 1)
        .unwrap();
    assert!(vp.render(&mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, &p, &frame));
    assert_ne!(vp.read_back(&device, &queue).unwrap(), before);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        u32::MAX,
        64,
        ONE_TO_ONE,
        &p,
        &frame,
    );
    let error = vp.render_error().unwrap().to_owned();
    vp.patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 1, 1)
        .unwrap();
    assert_eq!(vp.render_error(), Some(error.as_str()));
    assert!(!vp.render(&mut ctx, &device, &queue, 64, 64, ONE_TO_ONE, &p, &frame));
    assert!(vp.render_error().is_none());
    // A changed view still dispatches after sampling.
    assert!(vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ViewGeometry {
            off_x: 1.0,
            ..ONE_TO_ONE
        },
        &p,
        &frame
    ));
}

/// A horizontal ramp across the scene range, including above 1.0.
fn ramp(w: usize, h: usize) -> LumaImage {
    let data = (0..w * h)
        .map(|i| {
            let t = (i % w) as f32 / (w - 1) as f32;
            (t * 14.0 - 11.0).exp2() // 2^-11 .. 2^3, spanning the headroom
        })
        .collect();
    LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    }
}

/// 1:1, top-left aligned, so output pixel (x, y) is source pixel (x, y).
/// The background is the canvas outside the image. These tests render the image
/// itself, so it only matters where coverage is partial — and it is pinned here
/// rather than defaulted so a change to the default cannot silently move a
/// readback assertion.
const ONE_TO_ONE: ViewGeometry = ViewGeometry {
    scale: 1.0,
    off_x: 0.0,
    off_y: 0.0,
    background: 0.09,
    overlays: Overlays::NONE,
    surround: Surround::NONE,
};

/// The composition an untouched image resolves to: as shot, unstraightened,
/// uncropped. What every test written before composition existed means, and the
/// case whose plan must stay byte-identical to the one the graph produced then.
fn upright(luma: &LumaImage) -> Frame {
    Frame::resolve(
        luma.output_dims,
        Orientation::Rotate0,
        &CompositionParams::default(),
    )
}

/// Render under an explicit composition and read back. The frame is resolved from
/// `params` as shot, which is what the app does for a file whose EXIF says upright.
fn render_composed(
    luma: &LumaImage,
    w: u32,
    h: u32,
    view: ViewGeometry,
    params: &Params,
) -> Option<Vec<u8>> {
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &params.composition);
    let (device, queue) = headless_device()?;
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, luma);
    vp.render(&mut ctx, &device, &queue, w, h, view, params, &frame);
    vp.read_back(&device, &queue).map(|(_, _, px)| px)
}

fn no_dither() -> Params {
    let mut p = Params::default();
    p.display.dither = false;
    p
}

/// Render and read back. Returns None when there is no usable adapter.
fn render(
    luma: &LumaImage,
    w: u32,
    h: u32,
    view: ViewGeometry,
    params: &Params,
) -> Option<Vec<u8>> {
    let (device, queue) = headless_device()?;
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, luma);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        w,
        h,
        view,
        params,
        &upright(luma),
    );
    vp.read_back(&device, &queue).map(|(_, _, px)| px)
}

fn histogram(luma: &LumaImage, params: &Params, frame: &Frame) -> Option<[u32; HISTOGRAM_BINS]> {
    let (device, queue) = headless_device()?;
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, luma);
    let mut pending = vp
        .begin_histogram(&mut ctx, &device, &queue, params, frame)
        .expect("histogram dispatch");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(result) = pending.poll(&device) {
            return Some(result.expect("histogram readback"));
        }
        assert!(std::time::Instant::now() < deadline, "histogram timed out");
        std::thread::yield_now();
    }
}

fn final_bin(scene: f32, params: &Params) -> usize {
    let y = raw_core::display::tone_map(scene, params.display.tone_map);
    let encoded = if params.toning.is_active() {
        let toned = params.toning.evaluate(y);
        let (a, b) = toned.ab();
        let rgb = raw_core::colour::oklab_to_display_srgb(
            raw_core::colour::oklab_lightness(toned.y),
            a,
            b,
        );
        let gamma = 1.0 / params.display.gamma.max(0.01);
        let encoded = rgb.map(|v| v.max(0.0).powf(gamma));
        encoded[0] * 0.2126 + encoded[1] * 0.7152 + encoded[2] * 0.0722
    } else {
        y.powf(1.0 / params.display.gamma.max(0.01))
    };
    ((encoded.clamp(0.0, 1.0) * (HISTOGRAM_BINS - 1) as f32) as usize).min(HISTOGRAM_BINS - 1)
}

macro_rules! gpu {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("no GPU adapter; skipping");
                return;
            }
        }
    };
}

fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

#[test]
fn the_shader_agrees_with_the_cpu_display_transform() {
    // raw_core::display is the reference implementation and is what the histogram
    // calls. If the shader drifts from it, the histogram stops describing the image
    // on screen — a silent divergence, so it gets an explicit test.
    let modes = [
        ToneMap::Clip,
        ToneMap::AGX_DEFAULT,
        ToneMap::Agx(AgxParams {
            auto_range: false,
            black_ev: -7.5,
            white_ev: 4.0,
            contrast: 4.0,
            toe_power: 2.0,
            shoulder_power: 4.5,
        }),
        ToneMap::Shoulder {
            threshold: 0.75,
            strength: 0.8,
        },
        // Edge settings: a hard clip via strength 0, and a very low knee.
        ToneMap::Shoulder {
            threshold: 0.75,
            strength: 0.0,
        },
        ToneMap::Shoulder {
            threshold: 0.25,
            strength: 1.0,
        },
    ];
    for tone_map in modes {
        for v in [0.05f32, 0.18, 0.5, 1.0, 2.5] {
            let mut params = no_dither();
            params.display.tone_map = tone_map;
            let out = gpu!(render(&flat(v, 32, 32), 32, 32, ONE_TO_ONE, &params));

            let expected = (encode(v, &params.display) * 255.0 + 0.5).floor() as u8;
            let got = px(&out, 32, 16, 16)[0];
            assert!(
                got.abs_diff(expected) <= 1,
                "{tone_map:?} at scene {v}: shader gave {got}, raw_core::display says {expected}"
            );
        }
    }
}

#[test]
fn the_histogram_reads_toning_before_dither() {
    let luma = ramp(64, 16);
    let mut params = toned(|t| {
        t.apply("selenium", 1.0);
    });
    params.display.tone_map = ToneMap::Shoulder {
        threshold: 0.7,
        strength: 0.6,
    };
    params.display.dither = true;

    let bins = gpu!(histogram(&luma, &params, &upright(&luma)));
    let mut expected = [0u32; HISTOGRAM_BINS];
    for &scene in &luma.data {
        expected[final_bin(scene, &params)] += HISTOGRAM_WEIGHT;
    }
    assert_eq!(bins, expected);

    params.display.dither = false;
    let without_dither = gpu!(histogram(&luma, &params, &upright(&luma)));
    assert_eq!(bins, without_dither, "dither reached the histogram");
}

#[test]
fn a_large_histogram_uses_the_bounded_full_frame_proxy() {
    let (w, h) = (1024, 768);
    let luma = LumaImage {
        data: (0..w * h)
            .map(|i| if i % w < w / 2 { 0.1 } else { 0.8 })
            .collect(),
        output_dims: Dims { w, h },
        source_dims: Dims { w, h },
        clipped: Vec::new(),
    };
    let params = no_dither();
    let bins = gpu!(histogram(&luma, &params, &upright(&luma)));
    let half = HISTOGRAM_PROXY_EDGE * (HISTOGRAM_PROXY_EDGE * 3 / 4) / 2 * HISTOGRAM_WEIGHT;

    assert_eq!(bins[final_bin(0.1, &params)], half);
    assert_eq!(bins[final_bin(0.8, &params)], half);
    assert_eq!(bins.iter().sum::<u32>(), half * 2);
}

#[test]
fn an_identity_curve_is_bit_identical_to_no_curve() {
    // "The module is a no-op until touched" — literally, not to within LUT
    // quantisation. The curve pass is skipped outright when the curve is identity,
    // and this pins that the skip and the default agree.
    let luma = ramp(64, 8);
    let base = gpu!(render(&luma, 64, 8, ONE_TO_ONE, &no_dither()));

    let mut touched = no_dither();
    touched.curve.add(0.5, 0.5); // a point ON the identity line
    touched.curve.remove(1); // ...and remove it again
    assert!(touched.curve.is_identity());
    let after = gpu!(render(&luma, 64, 8, ONE_TO_ONE, &touched));

    assert_eq!(base, after, "identity curve changed the image");
}

#[test]
fn lifting_the_curve_brightens_without_inverting() {
    let luma = ramp(64, 4);
    let base = gpu!(render(&luma, 64, 4, ONE_TO_ONE, &no_dither()));

    let mut lifted = no_dither();
    lifted.curve.add(0.5, 0.68);
    let after = gpu!(render(&luma, 64, 4, ONE_TO_ONE, &lifted));

    assert_ne!(base, after, "a lifted curve did not reach the GPU");
    // Brighter everywhere, and still monotone left to right.
    let mut prev = 0u8;
    for x in 0..64 {
        let (b, a) = (px(&base, 64, x, 2)[0], px(&after, 64, x, 2)[0]);
        assert!(a >= b, "curve darkened x={x}: {b} -> {a}");
        assert!(a >= prev, "curve inverted the ramp at x={x}");
        prev = a;
    }
}

#[test]
fn agx_keeps_headroom_that_clip_throws_away() {
    // The reason AgX is offered at all. Scene values above 1.0 are real — decode
    // measures up to 3.52 on the Leica — and Clip collapses all of them onto white.
    let luma = LumaImage {
        data: vec![1.0, 2.0, 3.5],
        output_dims: Dims { w: 3, h: 1 },
        source_dims: Dims { w: 6, h: 2 },
        clipped: Vec::new(),
    };

    let mut clip = no_dither();
    clip.display.tone_map = ToneMap::Clip;
    let clipped = gpu!(render(&luma, 3, 1, ONE_TO_ONE, &clip));
    assert_eq!(
        px(&clipped, 3, 0, 0)[0],
        px(&clipped, 3, 2, 0)[0],
        "Clip should flatten the headroom"
    );

    let mut agx = no_dither();
    agx.display.tone_map = ToneMap::AGX_DEFAULT;
    let rolled = gpu!(render(&luma, 3, 1, ONE_TO_ONE, &agx));
    let (a, b, c) = (
        px(&rolled, 3, 0, 0)[0],
        px(&rolled, 3, 1, 0)[0],
        px(&rolled, 3, 2, 0)[0],
    );
    assert!(a < b && b < c, "AgX flattened the headroom: {a} {b} {c}");
}

#[test]
fn exposure_moves_the_image_and_does_not_clamp_on_the_way() {
    // Exposure must not clamp: +2 stops sends 1.0 to 4.0 and the display transform
    // decides what happens to it. With AgX that stays distinguishable, which is what
    // proves the value survived the working stage unclamped.
    let luma = flat(0.25, 16, 16);
    let mut a = no_dither();
    a.display.tone_map = ToneMap::AGX_DEFAULT;
    a.exposure.ev = 2.0; // 0.25 -> 1.0
    let mut b = a.clone();
    b.exposure.ev = 4.0; // 0.25 -> 4.0, well above the clamp point

    let ra = gpu!(render(&luma, 16, 16, ONE_TO_ONE, &a));
    let rb = gpu!(render(&luma, 16, 16, ONE_TO_ONE, &b));
    assert!(
        px(&rb, 16, 8, 8)[0] > px(&ra, 16, 8, 8)[0],
        "exposure above 1.0 was clamped in the working stage"
    );
}

#[test]
fn outside_the_image_is_the_neutral_surround() {
    // Perceived tonality of a monochrome rendering depends on its surround, so the
    // area outside the image is a fixed mid-grey — and, critically, NOT the edge
    // pixel smeared outward, which is the bug this replaced.
    let luma = flat(0.9, 8, 8);
    // Viewport larger than the image at 1:1, so the right half is off-image.
    let out = gpu!(render(&luma, 32, 16, ONE_TO_ONE, &no_dither()));

    let inside = px(&out, 32, 4, 4)[0];
    let outside = px(&out, 32, 24, 12);
    let surround = (0.09f32 * 255.0 + 0.5).floor() as u8;
    assert_eq!(outside[0], surround, "surround is not the fixed grey");
    assert_eq!(outside[0], outside[1], "surround must be neutral");
    assert!(inside > surround, "the image did not render");
    assert_eq!(outside[3], 255, "surround must be opaque");
}

#[test]
fn dither_is_zero_mean() {
    // TPDF dither preserves the tone scale statistically — that is the whole reason
    // it is safe to leave on. If it biased the mean it would be shifting exposure.
    let luma = flat(0.5, 128, 128);
    let off = gpu!(render(&luma, 128, 128, ONE_TO_ONE, &no_dither()));

    let mut on = no_dither();
    on.display.dither = true;
    let dithered = gpu!(render(&luma, 128, 128, ONE_TO_ONE, &on));

    assert_ne!(off, dithered, "dither had no effect");
    let mean = |b: &[u8]| b.chunks_exact(4).map(|p| p[0] as f64).sum::<f64>() / (128.0 * 128.0);
    let (m_off, m_on) = (mean(&off), mean(&dithered));
    assert!(
        (m_on - m_off).abs() < 0.5,
        "dither shifted the mean: {m_off} -> {m_on}"
    );
}

#[test]
fn dither_is_deterministic_across_renders() {
    // Deterministic from (x, y), so a frame re-renders identically and there is no
    // shimmer between renders.
    let luma = flat(0.5, 64, 64);
    let a = gpu!(render(&luma, 64, 64, ONE_TO_ONE, &Params::default()));
    let b = gpu!(render(&luma, 64, 64, ONE_TO_ONE, &Params::default()));
    assert_eq!(a, b, "dither is not deterministic");
}

#[test]
fn dither_is_locked_to_the_image_not_the_screen() {
    // Keyed on the SOURCE pixel, so panning slides the noise WITH the image rather
    // than letting it crawl underneath. Panning by exactly one output pixel must
    // shift the pattern by exactly one pixel.
    let luma = flat(0.5, 64, 64);
    let a = gpu!(render(&luma, 32, 8, ONE_TO_ONE, &Params::default()));
    let panned = ViewGeometry {
        scale: 1.0,
        off_x: 1.0,
        off_y: 0.0,
        ..Default::default()
    };
    let b = gpu!(render(&luma, 32, 8, panned, &Params::default()));

    for x in 0..31u32 {
        assert_eq!(
            px(&a, 32, x + 1, 4)[0],
            px(&b, 32, x, 4)[0],
            "dither did not travel with the image at x={x}"
        );
    }
}

#[test]
fn nothing_is_dispatched_when_nothing_changed() {
    // The idle-cost contract. An untouched window must not re-run the chain every
    // frame — which matters more with three passes than it did with one.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.4, 32, 32);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let params = Params::default();

    assert!(
        vp.render(
            &mut ctx,
            &device,
            &queue,
            64,
            64,
            ONE_TO_ONE,
            &params,
            &upright(&luma)
        ),
        "first render must run"
    );
    assert!(
        !vp.render(
            &mut ctx,
            &device,
            &queue,
            64,
            64,
            ONE_TO_ONE,
            &params,
            &upright(&luma)
        ),
        "idle frame re-dispatched"
    );

    let mut moved = params.clone();
    moved.exposure.ev = 0.5;
    assert!(
        vp.render(
            &mut ctx,
            &device,
            &queue,
            64,
            64,
            ONE_TO_ONE,
            &moved,
            &upright(&luma)
        ),
        "param change was ignored"
    );

    let panned = ViewGeometry {
        scale: 1.0,
        off_x: 3.0,
        off_y: 0.0,
        ..Default::default()
    };
    assert!(
        vp.render(
            &mut ctx,
            &device,
            &queue,
            64,
            64,
            panned,
            &moved,
            &upright(&luma)
        ),
        "pan was ignored"
    );

    // A new source image cannot be seen in the params, so it must be forced.
    vp.set_image(&device, &queue, &flat(0.7, 32, 32));
    assert!(
        vp.render(
            &mut ctx,
            &device,
            &queue,
            64,
            64,
            panned,
            &moved,
            &upright(&luma)
        ),
        "new image was not rendered"
    );
}

#[test]
fn readback_crops_to_the_used_region_not_the_allocation() {
    // Targets are over-allocated to a 128px grid so window resizing does not churn
    // egui texture registrations. Readback and the UV rect must both account for
    // that, or export silently gains a band of stale pixels.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.6, 200, 100);
    let mut vp = Viewport::new(&device, &queue, &luma);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        200,
        100,
        ONE_TO_ONE,
        &no_dither(),
        &upright(&luma),
    );

    let (w, h, buf) = vp.read_back(&device, &queue).expect("readback");
    assert_eq!(
        (w, h),
        (200, 100),
        "readback returned the padded allocation"
    );
    assert_eq!(buf.len(), 200 * 100 * 4);

    let [u, v] = vp.uv_rect();
    assert!((u - 200.0 / 256.0).abs() < 1e-6, "u was {u}");
    assert!((v - 100.0 / 128.0).abs() < 1e-6, "v was {v}");
}

#[test]
fn resizing_within_a_band_does_not_reallocate() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = no_dither();

    vp.render(
        &mut ctx,
        &device,
        &queue,
        200,
        100,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    assert!(vp.target_changed, "first target must be created");
    for w in 201..=256 {
        vp.render(
            &mut ctx,
            &device,
            &queue,
            w,
            100,
            ONE_TO_ONE,
            &p,
            &upright(&luma),
        );
        assert!(!vp.target_changed, "reallocated at width {w}");
    }
    vp.render(
        &mut ctx,
        &device,
        &queue,
        257,
        100,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    assert!(vp.target_changed, "crossing the band must reallocate");
}

#[test]
fn superpixel_dims_drive_the_view_not_source_dims() {
    // output_dims != source_dims in SuperPixel mode, and every consumer reads
    // output_dims. A viewport that sized itself from source_dims would render the
    // image at half scale with a quadrant of surround, so this pins the contract at
    // the crate boundary.
    let (device, queue) = gpu!(headless_device());
    let luma = flat(0.5, 100, 60); // source_dims is 200x120
    let vp = Viewport::new(&device, &queue, &luma);
    assert_eq!(
        vp.source_dims(),
        (100, 60),
        "viewport must size from output_dims"
    );
}

#[test]
fn export_is_full_resolution_and_scene_referred() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.42, 300, 200);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (w, h, scene) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    assert_eq!(
        (w, h),
        (300, 200),
        "export must be output_dims, not the viewport"
    );
    assert_eq!(scene.len(), 300 * 200, "one scene sample per pixel");
    // Scene-referred, not display-encoded: an untouched 0.42 comes back as 0.42,
    // NOT as 0.42^(1/2.2). If this ever reads ~0.68 the display tail leaked in.
    assert!(
        (scene[0] - 0.42).abs() < 1e-4,
        "export is not scene-referred: {}",
        scene[0]
    );
}

#[test]
fn export_carries_headroom_that_an_8_bit_path_would_destroy() {
    // The reason export branches before the display transform. Scene values above
    // 1.0 are real — decode measures up to 3.52 on the Leica — and any path through
    // the 8-bit display target would have flattened them onto 255 before the
    // container ever got a say.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = LumaImage {
        data: vec![1.0, 2.0, 3.5],
        output_dims: Dims { w: 3, h: 1 },
        source_dims: Dims { w: 6, h: 2 },
        clipped: Vec::new(),
    };
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (_, _, scene) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    assert!(
        scene[2] > 3.0,
        "headroom was clamped before the container: {}",
        scene[2]
    );
    assert!(scene[0] < scene[1] && scene[1] < scene[2]);
}

#[test]
fn export_has_no_tile_seam() {
    // Export is tiled at 2048 because a 41 MP frame's intermediates do not fit whole.
    // An image wider than one tile is the only way to catch an off-by-one in the tile
    // origin, and the symptom would be a visible vertical line — the kind of thing
    // that reaches a print before it reaches a bug report.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let w = 2100; // > EXPORT_TILE
    let luma = flat(0.42, w, 4);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (rw, _, scene) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");
    assert_eq!(rw, w as u32);

    let first = scene[0];
    for (x, v) in scene.iter().take(w).enumerate() {
        assert!(
            (v - first).abs() < 1e-6,
            "tile seam or offset error at x={x}: {v} vs {first}"
        );
    }
}

#[test]
fn export_preserves_a_gradient_across_the_seam() {
    // Flat catches offset errors; a ramp catches a tile reading the wrong source
    // region while still being internally consistent.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let w = 2100;
    let luma = ramp(w, 2);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (_, _, scene) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    let mut prev = f32::NEG_INFINITY;
    for (x, &v) in scene.iter().take(w).enumerate() {
        assert!(v >= prev, "export inverted the ramp at x={x}");
        prev = v;
    }
    assert!(prev > 0.0, "export produced a black frame");
}

#[test]
fn export_and_viewport_agree_through_the_display_transform() {
    // The corrected export claim. Export and the viewport deliberately do NOT share
    // a raw encoding — export is scene-referred, the viewport is display-encoded —
    // so the thing to check is that they agree once the display transform is
    // applied. That is the same relationship a colour-managed viewer establishes
    // between a tagged TIFF and the screen.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(200, 40);
    let mut params = no_dither();
    params.display.tone_map = ToneMap::AGX_DEFAULT;
    params.exposure.ev = 0.6;
    params.curve.add(0.4, 0.55);

    let mut vp = Viewport::new(&device, &queue, &luma);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        200,
        40,
        ONE_TO_ONE,
        &params,
        &upright(&luma),
    );
    let (_, _, shown) = vp.read_back(&device, &queue).expect("readback");
    let (_, _, exported) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &params,
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    for i in 0..200 * 40 {
        let via_display = (encode(exported[i], &params.display) * 255.0 + 0.5).floor() as u8;
        assert!(
            via_display.abs_diff(shown[i * 4]) <= 1,
            "export and viewport disagree at pixel {i}: {via_display} vs {}",
            shown[i * 4]
        );
    }
}

#[test]
fn export_reports_progress_once_per_tile() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 2100, 2100); // 2x2 tiles
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut seen = Vec::new();
    vp.export(
        &mut ctx,
        &device,
        &queue,
        &no_dither(),
        &upright(&luma),
        |done, total| seen.push((done, total)),
    )
    .expect("export");
    assert_eq!(seen, vec![(1, 4), (2, 4), (3, 4), (4, 4)]);
}

#[test]
fn export_preserves_the_live_viewport_without_a_redraw() {
    // Export owns a separate target; its tiles must not replace the live view
    // or invalidate the live render key.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = no_dither();

    vp.render(
        &mut ctx,
        &device,
        &queue,
        128,
        128,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    vp.export(&mut ctx, &device, &queue, &p, &upright(&luma), |_, _| {})
        .expect("export");
    assert!(
        !vp.render(
            &mut ctx,
            &device,
            &queue,
            128,
            128,
            ONE_TO_ONE,
            &p,
            &upright(&luma)
        ),
        "export unnecessarily invalidated the live view"
    );

    let (w, h, _) = vp.read_back(&device, &queue).expect("readback");
    assert_eq!((w, h), (128, 128), "viewport kept the export's tile size");
}

#[test]
fn the_pool_stops_allocating_once_it_is_warm() {
    // What makes a pool a pool. The first frame creates its intermediates; every
    // frame after reuses them, so a live slider drag allocates no VRAM at all.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 128, 128);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = no_dither();

    vp.render(
        &mut ctx,
        &device,
        &queue,
        256,
        256,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    let warm = ctx.pool().allocations();
    assert!(warm > 0, "the first frame must have allocated something");

    for i in 1..=30 {
        p.exposure.ev = i as f32 * 0.05;
        vp.render(
            &mut ctx,
            &device,
            &queue,
            256,
            256,
            ONE_TO_ONE,
            &p,
            &upright(&luma),
        );
    }
    assert_eq!(
        ctx.pool().allocations(),
        warm,
        "a slider drag must not allocate"
    );
    assert!(
        ctx.pool().reuses() >= 30,
        "buffers were not being handed back"
    );
}

#[test]
fn resizing_within_a_bucket_does_not_allocate() {
    // The pool buckets to 128px for the same reason the target quantises: a
    // window drag changes the extent every frame, and exact-fit allocation would
    // churn a texture per frame.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 400, 400);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = no_dither();

    vp.render(
        &mut ctx,
        &device,
        &queue,
        300,
        300,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    let warm = ctx.pool().allocations();
    for w in 300..=380 {
        vp.render(
            &mut ctx,
            &device,
            &queue,
            w,
            300,
            ONE_TO_ONE,
            &p,
            &upright(&luma),
        );
    }
    assert_eq!(
        ctx.pool().allocations(),
        warm,
        "a resize within one bucket reallocated"
    );
}

#[test]
fn two_viewports_share_one_context() {
    // The app-level requirement, at N=2. Eight Develop tabs must not mean eight
    // copies of every pipeline, and they must not fight over the pool: each
    // viewport hands its buffers back at the end of its own frame.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let a_luma = flat(0.25, 64, 64);
    let b_luma = flat(0.75, 64, 64);
    let mut a = Viewport::new(&device, &queue, &a_luma);
    let mut b = Viewport::new(&device, &queue, &b_luma);
    let p = no_dither();

    a.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &p,
        &upright(&a_luma),
    );
    let (_, _, ra) = a.read_back(&device, &queue).expect("readback");
    b.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &p,
        &upright(&b_luma),
    );
    let (_, _, rb) = b.read_back(&device, &queue).expect("readback");

    assert_ne!(
        px(&ra, 64, 32, 32),
        px(&rb, 64, 32, 32),
        "tabs must not share pixels"
    );

    // Interleave them: whatever the pool handed the second tab must not have
    // corrupted the first.
    a.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &p,
        &upright(&a_luma),
    );
    let (_, _, ra2) = a.read_back(&device, &queue).expect("readback");
    assert_eq!(ra, ra2, "one tab's render disturbed another's");
}

#[test]
fn a_compare_cell_is_a_second_picture_from_one_source() {
    // **The claim the whole compare grid rests on.** Four cells must be four
    // different renders of one negative, and they must cost four small *targets*
    // rather than four copies of the luminance texture — which on a 100 MP frame
    // would be 1.6 GB. `render_into` is what makes that true, so this asserts the
    // two halves of it: the cell really is a different picture, and it comes out of
    // the viewport that already owns the source.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.25, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let live = no_dither();
    let mut brighter = no_dither();
    brighter.exposure.ev = 2.0;

    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &live,
        &upright(&luma),
    );
    let (_, _, before) = vp.read_back(&device, &queue).expect("readback");

    let mut cell = raw_gpu::Cell::default();
    vp.render_into(
        &mut cell,
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &brighter,
        &upright(&luma),
    );
    let (_, _, in_cell) = cell.read_back(&device, &queue).expect("cell readback");

    assert!(
        px(&in_cell, 64, 32, 32)[0] > px(&before, 64, 32, 32)[0],
        "the cell did not render its own params: {:?} against {:?}",
        px(&in_cell, 64, 32, 32),
        px(&before, 64, 32, 32)
    );

    // **And the live target still holds the live picture.** `render_into` swaps the
    // viewport's target out and back; a swap that leaked would leave the window
    // showing whichever cell was drawn last, which is the failure that would look
    // like the app randomly applying somebody else's exposure.
    let (_, _, after) = vp.read_back(&device, &queue).expect("readback");
    assert_eq!(before, after, "rendering a cell overwrote the live view");
}

#[test]
fn the_live_view_still_updates_after_a_cell_is_drawn() {
    // The other way the swap can go wrong, and it is not the same bug: `last` is the
    // viewport's change detection, and a cell left in it would have the next live
    // render compare itself against the *cell* and decide nothing had changed. The
    // window would then freeze on the frame before compare opened — which looks like
    // a hang rather than like a wrong picture, so it needs its own test.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.25, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let live = no_dither();
    let mut other = no_dither();
    other.exposure.ev = 2.0;

    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &live,
        &upright(&luma),
    );
    let mut cell = raw_gpu::Cell::default();
    vp.render_into(
        &mut cell,
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &other,
        &upright(&luma),
    );

    // Now move the *live* params and render again. If `last` still holds the cell's,
    // this render is skipped and the pixels do not move.
    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &other,
        &upright(&luma),
    );
    let (_, _, moved) = vp.read_back(&device, &queue).expect("readback");
    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &live,
        &upright(&luma),
    );
    let (_, _, back) = vp.read_back(&device, &queue).expect("readback");
    assert_ne!(
        px(&moved, 64, 32, 32),
        px(&back, 64, 32, 32),
        "the live view stopped updating after a cell was rendered"
    );
}

#[test]
fn the_export_tap_follows_the_graph_when_the_curve_appears() {
    // Milestone 2 chose the export buffer by re-deriving `curve.is_identity()` at
    // a second site. The graph now names the tap — the sink's input — so adding a
    // node cannot silently export the wrong stage. Both shapes of graph must
    // export their POST-curve signal.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.25, 32, 32);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let plain = no_dither();
    let (_, _, before) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &plain,
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    let mut lifted = plain.clone();
    lifted.curve.add(0.3, 0.6); // well above the identity through the midtones
    let (_, _, after) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &lifted,
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");

    assert!(
        after[0] > before[0] * 1.05,
        "export ignored the curve node: {} vs {}",
        after[0],
        before[0]
    );
}

/// Contrast Mask on, with a spacer small enough to keep the tests quick.
///
/// `sigma_px` is in pixels, not in the percentage the parameter now stores.
/// These tests reason about halo widths, apron sizes and blur reach, all of which
/// are pixel quantities — converting here keeps the assertions saying what they
/// mean instead of restating them as percentages of frames that vary per test.
fn masked(sigma_px: f32, contrast: f32, dims: Dims) -> Params {
    let mut p = no_dither();
    p.contrast_mask.enabled = true;
    p.contrast_mask.spacer = pct_for(sigma_px, dims);
    p.contrast_mask.contrast = contrast;
    p
}

#[test]
fn a_reduced_mask_preserves_an_odd_sized_log_ramp_at_every_border() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let (w, h) = (513, 257);
    let mut luma = flat(0.18, w, h);
    for (i, v) in luma.data.iter_mut().enumerate() {
        *v *= (3.0 * (i % w) as f32 / (w - 1) as f32 + 2.0 * (i / w) as f32 / (h - 1) as f32 - 2.5)
            .exp2();
    }
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = masked(48.0, 0.35, luma.output_dims);
    let (_, _, result) = vp
        .export(&mut ctx, &device, &queue, &p, &upright(&luma), |_, _| {})
        .unwrap();
    for (i, (&before, &after)) in luma.data.iter().zip(&result).enumerate() {
        let expected = (1.0 - p.contrast_mask.contrast) * before.log2()
            + p.contrast_mask.contrast * 0.18f32.log2();
        assert!(
            (after.log2() - expected).abs() < 0.002,
            "mask introduced an edge/partial-cell ramp error at ({}, {})",
            i % w,
            i / w
        );
    }
}

#[test]
fn a_wide_reduced_mask_agrees_across_export_tiles_and_unaligned_patches() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp_with_texture(2305, 257, 5, 4.0, 0.2);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = masked(128.0, 0.4, luma.output_dims);
    p.contrast_mask.offset = (7.0, -3.0);
    let frame = upright(&luma);
    let (w, _, full) = vp
        .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
        .unwrap();
    for x in [13, 2035, 2258] {
        let (pw, ph, tile) = vp
            .patch(&mut ctx, &device, &queue, &p, &frame, x, 19, 47, 71)
            .unwrap();
        for y in 0..ph as usize {
            for col in 0..pw as usize {
                let reference = full[(y + 19) * w as usize + x as usize + col];
                let delta = (tile[y * pw as usize + col].log2() - reference.log2()).abs();
                assert!(
                    delta < 0.0001,
                    "reduced-grid seam at ({}, {}): {delta} EV",
                    x as usize + col,
                    y + 19
                );
            }
        }
    }
}

/// The spacer percentage that yields a `sigma_px` sigma on a `dims` frame — the
/// inverse of `ContrastMaskParams::spacer_px`, which is what the renderer will
/// apply to get back to pixels.
fn pct_for(sigma_px: f32, dims: Dims) -> f32 {
    let (w, h) = (dims.w as f32, dims.h as f32);
    100.0 * sigma_px / (w * w + h * h).sqrt()
}

/// Vertical bars, so a horizontal blur has something to do and a vertical seam
/// has something to disturb.
fn bars(w: usize, h: usize, period: usize) -> LumaImage {
    let data = (0..w * h)
        .map(|i| {
            if ((i % w) / period).is_multiple_of(2) {
                0.12
            } else {
                0.6
            }
        })
        .collect();
    LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    }
}

#[test]
fn the_mask_survives_an_export_tile_boundary() {
    // **The test this whole milestone exists for.** Export tiles at 2048, and
    // before ROI propagation the seams were invisible only because every pass was
    // per-pixel. A blur dropped into that chain reads pixels the neighbouring tile
    // owns; without an apron the tile edge is computed from clamped-at-the-boundary
    // data and a vertical line appears in every export.
    //
    // Flat input, so the mask is the identity everywhere and ANY variation across
    // the frame is the seam.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let w = 2100; // > EXPORT_TILE
    let luma = flat(0.42, w, 8);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (rw, _, scene) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &masked(24.0, 0.4, luma.output_dims),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");
    assert_eq!(rw, w as u32);

    // The whole width, borders included. This used to skip 200px each side to
    // dodge the frame-border artefact; odd reflection removed the artefact, so
    // the test now covers the pixels that were being excused.
    let first = scene[0];
    for (x, v) in scene.iter().enumerate().take(w) {
        assert!(
            (v - first).abs() < 1e-5,
            "tile seam at x={x}: {v} vs {first} (tile boundary is 2048)"
        );
    }
}

#[test]
fn the_mask_is_seamless_across_a_tile_boundary_with_real_structure() {
    // Flat input catches an offset. Structure catches a tile whose apron is
    // present but read at the wrong origin — internally smooth, wrong value.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let w = 2100;
    let luma = bars(w, 8, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = masked(20.0, 0.4, luma.output_dims);
    let (_, _, tiled) = vp
        .export(&mut ctx, &device, &queue, &p, &upright(&luma), |_, _| {})
        .expect("export");

    // The bar pattern has period 128, so pixels 2048 and 2048-128k are the same
    // phase and must carry the same value.
    let at = |x: usize| tiled[x];
    for k in 1..8 {
        let reference = at(2048 - 128 * k);
        assert!(
            (at(2048) - reference).abs() < 1e-4,
            "the pixel on the tile boundary disagrees with the same phase {} bars earlier: {} vs {}",
            k,
            at(2048),
            reference
        );
    }
}

#[test]
fn panning_does_not_change_a_masked_pixel() {
    // The sharpest available test of ROI correctness, and it costs four lines.
    // A spatial node computes each pixel from a neighbourhood; if the apron is
    // missing or misplaced, the answer depends on where the viewport happens to
    // sit. Integer pan deltas only — a fractional pan resamples onto a different
    // grid, which changes the answer for a legitimate reason.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = bars(256, 64, 9);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let p = masked(12.0, 0.45, luma.output_dims);

    let shift = 32.0;
    vp.render(
        &mut ctx,
        &device,
        &queue,
        96,
        48,
        ViewGeometry {
            scale: 1.0,
            off_x: 64.0,
            off_y: 8.0,
            ..Default::default()
        },
        &p,
        &upright(&luma),
    );
    let (wa, _, a) = vp.read_back(&device, &queue).expect("readback");
    let view_b = ViewGeometry {
        scale: 1.0,
        off_x: 64.0 + shift,
        off_y: 8.0,
        ..Default::default()
    };
    vp.render(
        &mut ctx,
        &device,
        &queue,
        96,
        48,
        view_b,
        &p,
        &upright(&luma),
    );
    let (wb, _, b) = vp.read_back(&device, &queue).expect("readback");

    // Source pixel (64 + shift + x) is at x + shift in frame A and x in frame B.
    for x in 0..48u32 {
        for y in [4u32, 24, 40] {
            assert_eq!(
                px(&a, wa, x + shift as u32, y),
                px(&b, wb, x, y),
                "the same source pixel rendered differently after a {shift}px pan (x={x}, y={y})"
            );
        }
    }
}

/// A broad tonal ramp with fine texture riding on it — the two scales Contrast
/// Mask is supposed to treat differently.
fn ramp_with_texture(w: usize, h: usize, period: usize, stops: f32, ripple: f32) -> LumaImage {
    let data = (0..w * h)
        .map(|i| {
            let x = i % w;
            let base = 0.18 * (x as f32 / (w - 1) as f32 * stops - stops * 0.5).exp2();
            base * if (x / period).is_multiple_of(2) {
                1.0
            } else {
                1.0 + ripple
            }
        })
        .collect();
    LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    }
}

#[test]
fn the_mask_compresses_the_broad_range_and_leaves_fine_detail_alone() {
    // **The argument for Contrast Mask over a clarity slider, as an assertion.**
    //
    // A synthetic local contrast tool multiplies texture amplitude. Printing
    // through a blurred positive does something else entirely: it subtracts the
    // LOW-frequency component, so the broad tonal range compresses while the fine
    // detail passes through at its original amplitude. The apparent local contrast
    // increase is that detail sitting on a compressed background — a byproduct,
    // not the operation.
    //
    // Structure finer than the spacer is invisible to the blur and therefore
    // untouched; structure coarser than it is what gets compressed. Both halves
    // are asserted, because either alone would also pass for a plain gain change.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let (w, period, contrast) = (1024usize, 8usize, 0.4f32);
    let luma = ramp_with_texture(w, 4, period, 4.0, 0.3);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let shoot = |vp: &mut Viewport, ctx: &mut GpuContext, p: &Params| {
        let (_, _, s) = vp
            .export(ctx, &device, &queue, p, &upright(&luma), |_, _| {})
            .expect("export");
        s
    };
    // A whole number of bar cycles, so the ripple averages out of the mean.
    let window = |s: &[f32], at: usize| -> f32 {
        let n = period * 2;
        s[at..at + n].iter().map(|v| v.log2()).sum::<f32>() / n as f32
    };
    // Ripple amplitude via a SECOND difference across one bar period. A plain
    // max-minus-min would also pick up the ramp's own slope across the window,
    // and that slope is compressed by the mask — so the metric would report the
    // detail changing when only the background had. The second difference
    // cancels any linear trend exactly, leaving the ripple alone.
    let amplitude = |s: &[f32], near: usize| -> f32 {
        // Land on the centre of an odd bar, with both neighbours on even ones.
        let at = near - near % (period * 2) + period + period / 2;
        let l = |i: usize| s[i].log2();
        (l(at) - 0.5 * (l(at - period) + l(at + period))).abs()
    };

    let plain = shoot(&mut vp, &mut ctx, &no_dither());
    let out = shoot(&mut vp, &mut ctx, &masked(40.0, contrast, luma.output_dims));
    // Sample well inside the frame: within 3 sigma of an edge the blur clamps,
    // which compresses for its own, legitimate reason.
    let (lo, hi) = (200usize, 800usize);

    let broad_before = window(&plain, hi) - window(&plain, lo);
    let broad_after = window(&out, hi) - window(&out, lo);
    let ratio = broad_after / broad_before;
    assert!(
        (ratio - (1.0 - contrast)).abs() < 0.05,
        "the broad range should compress by 1 - contrast = {}, got {ratio}",
        1.0 - contrast
    );

    for at in [lo, w / 2, hi] {
        let before = amplitude(&plain, at);
        let after = amplitude(&out, at);
        assert!(
            (before - 1.3f32.log2()).abs() < 0.01,
            "the test image's own ripple is not what it claims: {before}"
        );
        assert!(
            (after - before).abs() < 0.02 * before,
            "fine detail at x={at} was rescaled: {after} stops vs {before} — \
             the mask is behaving like a clarity slider"
        );
    }
}

#[test]
fn the_mask_holds_middle_grey_and_compresses_everything_toward_it() {
    // The pivot, stated honestly. This test used to be called
    // `a_flat_field_comes_through_the_mask_at_its_own_brightness`, which claimed
    // more than it checked: it only ever tried a flat 0.18 field, and 0.18 IS the
    // pivot, so it passed by construction and would have kept passing however
    // wrong the pivot was.
    //
    // What is actually true: middle grey is fixed, and a flat field anywhere else
    // is pulled TOWARD middle grey by `1 - contrast`, because a flat field is its
    // own local mean and the mask compresses the broad range. That is the
    // operation working, not a brightness bug — but it does mean enabling the
    // mask shifts overall level on any frame whose local means sit away from
    // 0.18. See docs/decisions.md for why that is still open.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.18, 128, 32);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let (_, _, plain) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("e");
    for contrast in [0.1f32, 0.35, 0.6] {
        let (_, _, out) = vp
            .export(
                &mut ctx,
                &device,
                &queue,
                &masked(30.0, contrast, luma.output_dims),
                &upright(&luma),
                |_, _| {},
            )
            .expect("e");
        let mid = out[out.len() / 2];
        assert!(
            (mid - plain[0]).abs() < 1e-3,
            "mask gamma {contrast} moved a flat 18% field: {mid} vs {}",
            plain[0]
        );
    }

    // ...and the other half, which the old name denied: a flat field away from
    // the pivot is compressed toward it by exactly `1 - contrast` in stops.
    let bright = flat(0.72, 128, 32);
    let mut vp = Viewport::new(&device, &queue, &bright);
    let (_, _, plain) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("e");
    let c = 0.4f32;
    let (_, _, out) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &masked(30.0, c, bright.output_dims),
            &upright(&luma),
            |_, _| {},
        )
        .expect("e");
    let pivot = 0.18f32.log2();
    let expected = (1.0 - c) * plain[0].log2() + c * pivot;
    assert!(
        (out[out.len() / 2].log2() - expected).abs() < 2e-3,
        "a flat field off the pivot should compress toward it: {} vs {expected}",
        out[out.len() / 2].log2()
    );
}

#[test]
fn the_registration_offset_shifts_the_mask_directionally() {
    // The novel control, and the one whose apron is asymmetric. Slipping the mask
    // sideways must produce a different image — and a different one in each
    // direction, or the offset is being taken as an absolute value somewhere.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = bars(256, 16, 24);
    let mut vp = Viewport::new(&device, &queue, &luma);

    let shot = |vp: &mut Viewport, ctx: &mut GpuContext, off: (f32, f32)| {
        let mut p = masked(10.0, 0.5, luma.output_dims);
        p.contrast_mask.offset = off;
        let (_, _, s) = vp
            .export(ctx, &device, &queue, &p, &upright(&luma), |_, _| {})
            .expect("export");
        s
    };
    let centred = shot(&mut vp, &mut ctx, (0.0, 0.0));
    let right = shot(&mut vp, &mut ctx, (12.0, 0.0));
    let left = shot(&mut vp, &mut ctx, (-12.0, 0.0));

    let differs = |a: &[f32], b: &[f32]| {
        a.iter()
            .zip(b)
            .skip(64)
            .take(128)
            .any(|(x, y)| (x - y).abs() > 1e-3)
    };
    assert!(
        differs(&centred, &right),
        "a registration slip changed nothing"
    );
    assert!(
        differs(&left, &right),
        "slipping left and right gave the same image"
    );
}

#[test]
fn intermediates_are_sized_to_the_image_not_the_viewport() {
    // ROI propagation earning its keep on the ordinary case. A small image in a
    // large window is mostly surround, and the surround has no scene data behind
    // it — so every node upstream of display works on the image's rectangle only.
    // Before per-node regions this allocated four 512x512 intermediates to
    // compute a 64x64 picture.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);

    vp.render(
        &mut ctx,
        &device,
        &queue,
        512,
        512,
        ONE_TO_ONE,
        &no_dither(),
        &upright(&luma),
    );

    let biggest = ctx
        .pool()
        .idle_descs()
        .map(|d| d.w.max(d.h))
        .max()
        .expect("pooled buffers");
    assert!(
        biggest <= 128,
        "intermediates were sized to the viewport, not the image: {biggest}"
    );
}

#[test]
fn the_surround_survived_losing_its_coverage_channel() {
    // The surround used to come from a coverage value the input node wrote for
    // every off-image pixel. Those pixels are no longer computed at all, so the
    // display node paints the surround where its input's rectangle does not
    // reach. Same picture, two different mechanisms — worth its own assertion
    // because a bounds-check bug here shows as a border, not a crash.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 16, 16);
    let mut vp = Viewport::new(&device, &queue, &luma);
    // Image sits at (8, 8) inside a 48x48 viewport: surround on all four sides.
    let view = ViewGeometry {
        scale: 1.0,
        off_x: -8.0,
        off_y: -8.0,
        ..Default::default()
    };
    vp.render(
        &mut ctx,
        &device,
        &queue,
        48,
        48,
        view,
        &no_dither(),
        &upright(&luma),
    );
    let (w, _, out) = vp.read_back(&device, &queue).expect("readback");

    let surround = px(&out, w, 1, 1);
    for (x, y) in [(1, 24), (46, 24), (24, 1), (24, 46), (46, 46)] {
        assert_eq!(px(&out, w, x, y), surround, "surround broke at ({x}, {y})");
    }
    assert_ne!(
        px(&out, w, 16, 16),
        surround,
        "the image itself went missing"
    );
    // ...and the boundary is where the image says it is, not one pixel off.
    assert_eq!(px(&out, w, 7, 16), surround, "image leaked one pixel left");
    assert_ne!(
        px(&out, w, 8, 16),
        surround,
        "image is inset one pixel too far"
    );
}

#[test]
fn the_device_asks_for_the_adapters_full_texture_limit() {
    // Found by launching the GUI: in DirectMosaic the working image is the full
    // sensor width — 11648 on the Fuji GFX 100S — and both wgpu's and egui-wgpu's
    // defaults cap max_texture_dimension_2d at 8192. Creating the texture is then
    // a validation failure inside the driver call, which surfaces as a panic
    // rather than as an unsupported mode.
    //
    // The corpus needs 11648; Metal on Apple silicon offers 16384.
    let (device, _queue) = gpu!(headless_device());
    let got = raw_gpu::max_image_dim(&device);
    assert!(
        got >= 11648,
        "this device tops out at {got} px, so the Fuji GFX 100S cannot be shown \
         in DirectMosaic"
    );
}

#[test]
fn a_working_image_at_the_device_limit_can_be_uploaded() {
    // The limit is only useful if a texture that size actually allocates. One
    // pixel tall, so this stays a bounds check and not a 1 GB allocation.
    let (device, queue) = gpu!(headless_device());
    let w = raw_gpu::max_image_dim(&device) as usize;
    let luma = flat(0.4, w, 1);
    let vp = Viewport::new(&device, &queue, &luma);
    assert_eq!(vp.source_dims(), (w as u32, 1));
}

#[test]
fn the_whole_chain_is_stable() {
    // The catch-all. Everything above says why a particular pixel is what it is;
    // this notices changes nobody predicted. If it trips alone, some behaviour moved
    // that no invariant above was defending — decide whether that was intended, then
    // update the hash and ideally add the test that should have caught it.
    let luma = ramp(96, 24);
    let mut params = Params::default();
    params.display.tone_map = ToneMap::AGX_DEFAULT;
    params.exposure.ev = 0.75;
    params.exposure.black = -0.002;
    params.curve = {
        let mut c = Curve::default();
        c.add(0.35, 0.28);
        c.add(0.72, 0.83);
        c.into()
    };
    let out = gpu!(render(
        &luma,
        96,
        24,
        ViewGeometry {
            scale: 1.0,
            off_x: -4.0,
            off_y: -2.0,
            ..Default::default()
        },
        &params
    ));

    // FNV-1a over the whole frame.
    let hash = out.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x1000_0000_01b3)
    });
    // Last updated when AgX gained White EV, Black EV, Contrast and Shape, and its
    // pivot stopped being a constant. `AGX_PIVOT_X` was the literal `0.606`; the
    // parameterised curve computes `-black_ev / (white_ev - black_ev)`, which at the
    // defaults is `10 / 16.5 = 0.6060606` — the number the literal was a three-decimal
    // transcription of. So the defaults are **not** bit-identical to the fixed AgX
    // they replaced, and the size of that is the whole point: 22 of these 2304 pixels
    // move, every one of them by a single 8-bit code, because the curve never departs
    // from its predecessor by more than a twentieth of a code.
    // `agx_defaults_match_the_curve_they_replaced` in raw-core is the invariant that
    // now defends it, which is the test this hash should have had all along.
    //
    // **Isolated rather than assumed.** The same render was hashed at four stages —
    // Clip alone, plus exposure, plus the curve, then AgX — against the commit before
    // this work. The first three were identical, so nothing outside the tone map
    // moved: the `Curve` -> `CurveStack` migration is bit-exact for one instance at
    // full opacity, and so is the projective identity path that replaced the affine.
    assert_eq!(
        hash, 0xaeea_42c9_b5a4_839e,
        "the render chain changed; hash was {hash:#018x}"
    );
}

#[test]
fn the_mask_leaves_no_halo_at_the_frame_border() {
    // The bug this test exists for was visible in the app before it was visible in
    // any assertion: a rim 3 sigma wide — about 15% of the frame at a 200px spacer
    // — whose sign followed the edge content. Every mask test until now sampled
    // well inside the borders, so none of them could see it.
    //
    // A log-linear ramp is the sharp probe. Its Gaussian blur is itself, so the
    // correct output is exactly `(1 - contrast) * L + contrast * pivot` at EVERY
    // pixel including the first and last — no special case, no tolerance band.
    // Any boundary rule that biases the local mean shows up immediately here.
    //
    // Clamped taps gave -0.032 stops at x=0; a renormalised one-sided mean is
    // worse still, because it is honest about the data it has but is not centred.
    // Odd reflection extrapolates the gradient and lands exactly.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let (w, contrast, spacer, stops) = (400usize, 0.4f32, 20.0f32, 4.0f32);
    let data: Vec<f32> = (0..w * 8)
        .map(|i| {
            let x = (i % w) as f32 / (w - 1) as f32;
            0.18f32 * (x * stops - stops / 2.0).exp2()
        })
        .collect();
    let luma = LumaImage {
        data,
        output_dims: Dims { w, h: 8 },
        source_dims: Dims { w: w * 2, h: 16 },
        clipped: Vec::new(),
    };
    let mut vp = Viewport::new(&device, &queue, &luma);

    let (_, _, plain) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("e");
    let (_, _, out) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &masked(spacer, contrast, luma.output_dims),
            &upright(&luma),
            |_, _| {},
        )
        .expect("e");
    let pivot = 0.18f32.log2();

    let mut worst = (0usize, 0.0f32);
    for x in 0..w {
        let ideal = (1.0 - contrast) * plain[x].log2() + contrast * pivot;
        let delta = out[x].log2() - ideal;
        if delta.abs() > worst.1.abs() {
            worst = (x, delta);
        }
    }
    assert!(
        worst.1.abs() < 2e-3,
        "border halo: {:+.4} stops at x={} (support is {} px, so a band that wide \
         is the boundary rule, not noise)",
        worst.1,
        worst.0,
        3.0 * spacer
    );
}

#[test]
fn an_uncovered_edge_row_does_not_poison_the_mask() {
    // Reported from the app: a white band along the bottom of the frame with
    // Contrast Mask on, below 100% zoom only, and absent from exports.
    //
    // Cause, and the reason every earlier mask test missed it: those all ran at
    // scale 1.0. Below 100% the input node box-filters, and at some geometries the
    // last row of the grid has NO in-bounds taps at all. It used to write value 0
    // there — meaning "nothing", but read by log2 as a real number tens of stops
    // below the scene, which the blur then averaged into every row within its
    // radius. Coverage said "do not show this pixel"; nothing said "do not
    // COMPUTE with it".
    //
    // A flat field makes the mask an identity, so any spread is the bug. The zoom
    // level is swept because whether the last row is uncovered depends on how the
    // grid rounds, and the reported case needed a particular one.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let (iw, ih) = (700usize, 460usize);
    let luma = flat(0.30, iw, ih);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (ow, oh) = (400u32, 300u32);

    for scale in [0.19f32, 0.23, 0.27, 0.31, 0.37, 0.41, 0.47, 0.5, 0.53, 0.55] {
        let view = ViewGeometry {
            scale,
            off_x: (iw as f32 - ow as f32 / scale) * 0.5,
            off_y: (ih as f32 - oh as f32 / scale) * 0.5,
            ..Default::default()
        };
        let mut shot = |ctx: &mut GpuContext, on: bool| {
            let mut p = no_dither();
            p.contrast_mask.enabled = on;
            p.contrast_mask.spacer = 40.0;
            vp.render(ctx, &device, &queue, ow, oh, view, &p, &upright(&luma));
            vp.read_back(&device, &queue).expect("readback").2
        };
        let off = shot(&mut ctx, false);
        let on = shot(&mut ctx, true);

        // The image is letterboxed, so the frame holds image and surround only.
        // On a flat field both are single values, and the mask maps the image
        // value uniformly — so the brightest pixel anywhere IS the image value.
        let peak = |b: &[u8]| b.chunks_exact(4).map(|p| p[0]).max().unwrap();
        let interior = on[(oh / 2 * ow + ow / 2) as usize * 4];
        assert!(
            peak(&on).abs_diff(interior) <= 2,
            "edge spike at scale {scale}: peak {} vs interior {interior} \
             (mask off peaks at {})",
            peak(&on),
            peak(&off)
        );
    }
}

// ---------------------------------------------------------- diagnostic overlays

/// `ONE_TO_ONE` with overlays on.
fn with_overlays(o: Overlays) -> ViewGeometry {
    ViewGeometry {
        overlays: o,
        ..ONE_TO_ONE
    }
}

/// Read one pixel as RGB.
fn rgb(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 3] {
    let i = (y * w + x) * 4;
    [buf[i], buf[i + 1], buf[i + 2]]
}

#[test]
fn overlays_off_leave_the_image_grey() {
    // The baseline that makes the rest meaningful: a monochrome pipeline's output
    // is neutral, so any channel difference below is the overlay and nothing else.
    let luma = flat(0.5, 32, 32);
    let Some(out) = render(&luma, 32, 32, ONE_TO_ONE, &no_dither()) else {
        return;
    };
    let p = rgb(&out, 32, 16, 16);
    assert_eq!(p[0], p[1], "a monochrome render is not neutral");
    assert_eq!(p[1], p[2]);
}

#[test]
fn sensor_clipping_is_green_with_support_and_yellow_when_the_block_is_gone() {
    let Some(_) = headless_device() else { return };
    let (w, h) = (32usize, 8usize);
    let mut luma = flat(0.5, w, h);
    luma.clipped = (0..w * h)
        .map(|i| match i % w {
            0..=11 => 1,
            20..=31 => 4,
            _ => 0,
        })
        .collect();
    let sensor = Overlays {
        sensor: true,
        ..Overlays::NONE
    };
    let out = render(
        &luma,
        w as u32,
        h as u32,
        with_overlays(sensor),
        &no_dither(),
    )
    .expect("adapter checked above");

    // All three positions sit on a hatch line. The centre band carries no clipping
    // and must remain neutral rather than receiving a general tint.
    let partial = rgb(&out, w, 8, 0);
    let clear = rgb(&out, w, 16, 0);
    let gone = rgb(&out, w, 24, 0);
    assert!(
        partial[1] > partial[0] * 3 && partial[1] > partial[2],
        "not green: {partial:?}"
    );
    assert_eq!(clear[0], clear[1], "unclipped pixel was marked: {clear:?}");
    assert!(
        gone[0] > gone[1] && gone[1] > gone[2] * 3,
        "not yellow: {gone:?}"
    );
}

#[test]
fn the_overexposed_overlay_marks_blown_highlights_black() {
    // The prototype's top tier, and the counter-intuitive one worth pinning: fully
    // blown reads BLACK, not red. Against a white sky that is the only mark that
    // stays visible, which is the point of it.
    let luma = flat(4.0, 32, 32); // far above scene white; Clip takes it to 1.0
    let over = Overlays {
        overexposed: true,
        ..Overlays::NONE
    };
    let Some(out) = render(&luma, 32, 32, with_overlays(over), &no_dither()) else {
        return;
    };
    assert_eq!(
        rgb(&out, 32, 16, 16),
        [0, 0, 0],
        "blown highlights were not flagged"
    );
}

#[test]
fn the_underexposed_overlay_marks_pure_black_white() {
    let luma = flat(0.0, 32, 32);
    let under = Overlays {
        underexposed: true,
        ..Overlays::NONE
    };
    let Some(out) = render(&luma, 32, 32, with_overlays(under), &no_dither()) else {
        return;
    };
    assert_eq!(
        rgb(&out, 32, 16, 16),
        [255, 255, 255],
        "crushed shadows were not flagged"
    );
}

#[test]
fn an_overlay_leaves_the_rest_of_the_picture_alone() {
    // A diagnostic that tinted everything would be useless. Mid-grey is nowhere
    // near either threshold and must come through untouched.
    let luma = flat(0.18, 40, 8);
    let both = Overlays {
        overexposed: true,
        underexposed: true,
        ..Overlays::NONE
    };
    let Some(plain) = render(&luma, 40, 8, ONE_TO_ONE, &no_dither()) else {
        return;
    };
    let Some(marked) = render(&luma, 40, 8, with_overlays(both), &no_dither()) else {
        return;
    };
    assert_eq!(
        plain, marked,
        "the overlays touched pixels that are correctly exposed"
    );
}

#[test]
fn the_near_clip_tier_is_a_checkerboard_and_the_hard_tier_is_not() {
    // The tiers have to be distinguishable at a glance, which is what the
    // checkerboard is for — and why it is keyed to screen position rather than to
    // the source, where it would alias into a flat tint at fit-to-screen.
    //
    // Scene 0.87 encodes to 239 of 255 through gamma 2.2: past the near-clip
    // threshold of 230, short of the hard one at 248. (0.95 lands on 249 and is
    // the hard tier — the bands are narrow at the top, which is rather the point.)
    let luma = flat(0.87, 32, 32);
    let over = Overlays {
        overexposed: true,
        ..Overlays::NONE
    };
    let Some(out) = render(&luma, 32, 32, with_overlays(over), &no_dither()) else {
        return;
    };
    let a = rgb(&out, 32, 10, 10);
    let b = rgb(&out, 32, 11, 10);
    assert_ne!(a, b, "the near-clip tier is solid, not a checkerboard");
    let magenta = [255u8, 68, 170];
    assert!(
        a == magenta || b == magenta,
        "neither pixel is the near-clip magenta: {a:?} {b:?}"
    );
}

#[test]
fn false_colour_puts_the_midtone_target_in_green() {
    // The convention the map exists to serve: 0.40-0.60 encoded is the "correct
    // exposure" band, and it is green so a correctly-exposed face is unmistakable.
    // Scene 0.18 through gamma 2.2 lands at ~0.46 — the middle of that band.
    let luma = flat(0.18, 32, 32);
    let fc = Overlays {
        false_colour: true,
        ..Overlays::NONE
    };
    let Some(out) = render(&luma, 32, 32, with_overlays(fc), &no_dither()) else {
        return;
    };
    let [r, g, b] = rgb(&out, 32, 16, 16);
    assert!(
        g > r && g > b,
        "middle grey did not read as green: {r},{g},{b}"
    );
}

#[test]
fn false_colour_is_monotonic_through_the_ramp() {
    // Ten stops with a linear ramp between them. If a band were transcribed out of
    // order, or a lerp inverted, the map would still look colourful and would lie —
    // so this checks the thing the eye cannot: that brighter always maps forward
    // along the scale, never back.
    let luma = ramp(64, 4);
    let fc = Overlays {
        false_colour: true,
        ..Overlays::NONE
    };
    let Some(out) = render(&luma, 64, 4, with_overlays(fc), &no_dither()) else {
        return;
    };
    let Some(plain) = render(&luma, 64, 4, ONE_TO_ONE, &no_dither()) else {
        return;
    };
    // Every distinct input luminance must map to one colour, and equal luminances
    // to equal colours.
    let mut seen: std::collections::HashMap<u8, [u8; 3]> = std::collections::HashMap::new();
    for x in 0..64 {
        let lum = rgb(&plain, 64, x, 2)[0];
        let col = rgb(&out, 64, x, 2);
        if let Some(prev) = seen.insert(lum, col) {
            assert_eq!(prev, col, "luminance {lum} mapped to two different colours");
        }
    }
    assert!(
        seen.len() > 8,
        "the ramp did not cover enough of the scale to be a test"
    );
}

#[test]
fn the_overlays_ignore_the_dither() {
    // The defect this test exists for, reported from a screenshot: the tier
    // thresholds were being applied to the value AFTER TPDF dither, so ±1 LSB of
    // deliberate display noise decided whether a pixel read as clipped. A uniformly
    // crushed region came out as speckle rather than a shape, and the checkerboard
    // never formed because dither broke every run of same-tier pixels into single
    // pixels.
    //
    // The overlay is a diagnostic of the image. It must not be a diagnostic of the
    // dither — so the marks have to be identical with dither on and off, even
    // though the pixels underneath them are not.
    let luma = ramp(96, 8);
    let both = Overlays {
        overexposed: true,
        underexposed: true,
        ..Overlays::NONE
    };

    let mut dithered = Params::default();
    dithered.display.dither = true;

    let Some(with) = render(&luma, 96, 8, with_overlays(both), &dithered) else {
        return;
    };
    let Some(without) = render(&luma, 96, 8, with_overlays(both), &no_dither()) else {
        return;
    };

    // Compare only the flagged pixels: everywhere else the two SHOULD differ,
    // because that is the dither doing its job.
    let marks = [
        [255u8, 68, 170], // near-clip magenta
        [255, 32, 32],    // hard-clip red
        [0, 0, 0],        // blown black
        [0, 204, 204],    // near-crush cyan
        [0, 68, 255],     // hard-crush blue
        [255, 255, 255],  // pure-black white
    ];
    let mut flagged = 0;
    for i in (0..with.len()).step_by(4) {
        let a = [with[i], with[i + 1], with[i + 2]];
        let b = [without[i], without[i + 1], without[i + 2]];
        if marks.contains(&a) || marks.contains(&b) {
            flagged += 1;
            assert_eq!(a, b, "a mark moved with the dither at byte {i}");
        }
    }
    assert!(
        flagged > 0,
        "the ramp flagged nothing, so this test proved nothing"
    );
}

// ------------------------------------------------------------------- the mount

#[test]
fn the_mount_sits_between_the_image_and_the_canvas() {
    // A viewport larger than the image, so there is letterbox to put a mount in.
    // The band immediately outside the frame is the mount; beyond it, the canvas.
    let luma = flat(0.5, 16, 16);
    let mount = Surround {
        width: 4.0,
        rgb: [1.0, 0.0, 0.0],
    };
    let view = ViewGeometry {
        surround: mount,
        ..ONE_TO_ONE
    };
    let Some(out) = render(&luma, 40, 40, view, &no_dither()) else {
        return;
    };

    // Inside the image: untouched.
    assert_eq!(
        rgb(&out, 40, 8, 8)[0],
        rgb(&out, 40, 8, 8)[1],
        "the mount reached the image"
    );
    // Just outside the frame: mount.
    assert_eq!(
        rgb(&out, 40, 18, 8),
        [255, 0, 0],
        "no mount beside the image"
    );
    // Past the mount's width: canvas.
    let canvas = rgb(&out, 40, 30, 8);
    assert_eq!(canvas[0], canvas[1], "the canvas is not neutral");
    assert_ne!(canvas, [255, 0, 0], "the mount did not end");
}

#[test]
fn a_zero_width_mount_is_no_mount() {
    // Width and "enabled" are one number on purpose, so there is no state where a
    // flag and a width disagree.
    let luma = flat(0.5, 16, 16);
    let Some(plain) = render(&luma, 40, 40, ONE_TO_ONE, &no_dither()) else {
        return;
    };
    let off = ViewGeometry {
        surround: Surround {
            width: 0.0,
            rgb: [1.0, 0.0, 0.0],
        },
        ..ONE_TO_ONE
    };
    let Some(out) = render(&luma, 40, 40, off, &no_dither()) else {
        return;
    };
    assert_eq!(plain, out, "a zero-width mount drew something");
}

#[test]
fn the_mounts_corners_are_square() {
    // Chebyshev distance, not Euclidean: a mount is cut with a knife and a straight
    // edge, and a rounded corner would be a decoration rather than a reference.
    let luma = flat(0.5, 16, 16);
    let mount = Surround {
        width: 6.0,
        rgb: [1.0, 0.0, 0.0],
    };
    let view = ViewGeometry {
        surround: mount,
        ..ONE_TO_ONE
    };
    let Some(out) = render(&luma, 40, 40, view, &no_dither()) else {
        return;
    };
    // The diagonal corner pixel is 5 out on both axes — inside a square mount of
    // width 6, outside a circular one of radius 6 (5*sqrt(2) = 7.07).
    assert_eq!(
        rgb(&out, 40, 20, 20),
        [255, 0, 0],
        "the corner was rounded off"
    );
}

#[test]
fn the_image_edge_blends_into_the_mount_not_the_canvas() {
    // The seam this fixes: the partly-covered row at the image boundary is
    // composited by coverage, and it used to blend toward the canvas even when a
    // mount was sitting against it — drawing a thin dark line all the way around
    // the image, an antialiasing edge fading to a colour that was nowhere near it.
    //
    // A white mount on a near-black canvas makes the difference unmissable: with
    // the bug, the pixels just inside the frame darken; without it, they do not.
    let luma = flat(0.6, 15, 15);
    let mount = Surround {
        width: 6.0,
        rgb: [1.0, 1.0, 1.0],
    };
    // Scale below 1.0 so the last column is only partly covered — the coverage
    // path only exists there. This is the same class of bug as the Contrast Mask
    // one from milestone 3: it cannot happen at 1:1.
    let view = ViewGeometry {
        scale: 0.7,
        surround: mount,
        ..ONE_TO_ONE
    };
    let Some(out) = render(&luma, 40, 40, view, &no_dither()) else {
        return;
    };

    // Find the image's right-hand edge: the last column that is not the mount.
    let row = 5;
    let is_mount = |x: usize| rgb(&out, 40, x, row) == [255, 255, 255];
    let edge = (0..40)
        .rev()
        .find(|x| !is_mount(*x) && rgb(&out, 40, *x, row) != [23, 23, 23])
        .expect("no image pixel on this row — the test found nothing to assert about");
    assert!(edge > 3, "the image is too narrow for an interior sample");
    let interior = rgb(&out, 40, edge.saturating_sub(3), row);
    let at_edge = rgb(&out, 40, edge, row);
    assert!(
        at_edge[0] >= interior[0],
        "the image edge darkened against a white mount: {at_edge:?} beside {interior:?}"
    );
}

#[test]
fn the_warning_tiers_are_wide_enough_to_see() {
    // What the maintainer reported: not enough magenta or red before everything went black.
    // The cause was that the prototype's top two over-exposed tiers are "any
    // channel clipped" and "all three clipped" — different tests in colour, the
    // same test in monochrome, so red was a seven-level sliver.
    //
    // This walks a ramp and counts how much of it each tier claims, which is the
    // thing that was actually wrong and which no threshold assertion would catch.
    let luma = ramp(256, 4);
    let over = Overlays {
        overexposed: true,
        ..Overlays::NONE
    };
    let under = Overlays {
        underexposed: true,
        ..Overlays::NONE
    };

    let count = |o: Overlays, want: [u8; 3]| -> usize {
        let Some(out) = render(&luma, 256, 4, with_overlays(o), &no_dither()) else {
            return usize::MAX; // no adapter; make the assertions vacuous
        };
        (0..256).filter(|x| rgb(&out, 256, *x, 2) == want).count()
    };

    // Each warning tier has to claim a real slice of the ramp, not a sliver.
    assert!(
        count(over, [255, 32, 32]) >= 4,
        "the red tier is still a sliver"
    );
    assert!(
        count(under, [0, 68, 255]) >= 4,
        "the blue tier is still a sliver"
    );
}

// ── Composition ──────────────────────────────────────────────────────────────

/// One bright pixel at the top-left of the source, everything else dark. Which
/// corner it comes back in is the whole of what an orientation does.
fn corner_mark(w: usize, h: usize) -> LumaImage {
    let mut data = vec![0.05f32; w * h];
    data[0] = 0.9;
    LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    }
}

#[test]
fn each_quarter_turn_puts_the_picture_the_way_the_exif_tag_says() {
    // The defect milestone 9a opens with, all the way through the GPU. Two files
    // in the corpus are `Rotate 270 CW` and displayed on their side for eight
    // milestones; this asserts that the tag now reaches the pixels, and that each
    // of the four turns goes the way it claims rather than merely going *a* way.
    //
    // Asymmetric dims on purpose: a bug that transposed the frame without moving
    // the content, or moved the content without transposing the frame, would pass
    // on a square image.
    let luma = corner_mark(8, 4);
    let bright = |o: Orientation, fw: u32, fh: u32| -> (u32, u32) {
        let mut p = no_dither();
        p.composition.orientation = Some(o);
        let out = render_composed(&luma, fw, fh, ONE_TO_ONE, &p).expect("render");
        let mut best = ((0, 0), 0u8);
        for y in 0..fh {
            for x in 0..fw {
                let v = px(&out, fw, x, y)[0];
                if v > best.1 {
                    best = ((x, y), v);
                }
            }
        }
        best.0
    };

    assert_eq!(
        bright(Orientation::Rotate0, 8, 4),
        (0, 0),
        "upright moved the picture"
    );
    // 90 CW: the sensor's top-left arrives at the frame's top-RIGHT, and the frame
    // is now 4 wide by 8 high.
    assert_eq!(bright(Orientation::Rotate90, 4, 8), (3, 0));
    assert_eq!(bright(Orientation::Rotate180, 8, 4), (7, 3));
    assert_eq!(bright(Orientation::Rotate270, 4, 8), (0, 7));
}

#[test]
fn a_quarter_turn_moves_pixels_without_touching_their_values() {
    // Why the four rotations are exempt from the resampling branch. A quarter turn
    // lands whole pixels on whole pixels, so the rendered frame must be a
    // permutation of the upright one — the same multiset of values, not merely a
    // similar-looking picture. Anything that interpolated here would soften every
    // portrait frame in the corpus for no reason.
    let luma = ramp(16, 8);
    let p = no_dither();
    let mut turned = p.clone();
    turned.composition.orientation = Some(Orientation::Rotate90);

    let flat = render_composed(&luma, 16, 8, ONE_TO_ONE, &p).expect("render");
    let side = render_composed(&luma, 8, 16, ONE_TO_ONE, &turned).expect("render");

    let sorted = |b: &[u8]| {
        let mut v: Vec<u8> = b.chunks_exact(4).map(|p| p[0]).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(
        sorted(&flat),
        sorted(&side),
        "a quarter turn resampled the picture"
    );
}

#[test]
fn a_crop_shows_the_same_pixels_it_showed_uncropped() {
    // The crop must not move, rescale or re-tone anything — it decides what is
    // visible and nothing else. Rendered at the same view both ways, so a pixel
    // inside the crop is at the same place in both buffers and can be compared
    // directly.
    let luma = ramp(64, 32);
    let mut p = no_dither();
    p.exposure.ev = 0.4;
    let mut c = p.clone();
    c.composition.crop = Rect {
        x: 0.25,
        y: 0.25,
        w: 0.5,
        h: 0.5,
    };

    let whole = render_composed(&luma, 64, 32, ONE_TO_ONE, &p).expect("render");
    let part = render_composed(&luma, 64, 32, ONE_TO_ONE, &c).expect("render");

    // 25%..75% of 64x32 is x in 16..48, y in 8..24. One pixel in from each edge,
    // because the boundary itself is antialiased by coverage.
    for y in 9..23u32 {
        for x in 17..47u32 {
            assert_eq!(
                px(&part, 64, x, y),
                px(&whole, 64, x, y),
                "the crop changed pixel ({x}, {y})"
            );
        }
    }
    // And outside it is the canvas, not the picture.
    assert_ne!(
        px(&part, 64, 2, 2),
        px(&whole, 64, 2, 2),
        "the crop showed nothing"
    );
}

#[test]
fn a_spatial_module_reads_across_the_crop_boundary() {
    // **The load-bearing render test of milestone 9a.** The handoff resolves
    // "crop cannot execute last" by running the tone chain on the uncropped frame,
    // and `raw_graph` implements that by intersecting the crop into the sink's
    // request before the aprons expand out of it.
    //
    // If that order were reversed, Contrast Mask at the crop's edge would have no
    // data past the boundary and would compute its blur from a clamped edge pixel —
    // a bright halo along the inside of the crop. That is exactly the milestone-3
    // bug arriving again by a different route, and it was invisible below 100% zoom
    // the first time.
    //
    // Measured by rendering the same view cropped and uncropped and demanding the
    // pixels agree right up to the boundary. They can only agree if the cropped
    // render read pixels that the crop excludes.
    let luma = ramp_with_texture(256, 64, 8, 4.0, 0.3);
    let mut p = masked(20.0, 0.45, luma.output_dims);
    p.display.dither = false;
    let mut c = p.clone();
    c.composition.crop = Rect {
        x: 0.25,
        y: 0.0,
        w: 0.5,
        h: 1.0,
    };

    let whole = render_composed(&luma, 256, 64, ONE_TO_ONE, &p).expect("render");
    let part = render_composed(&luma, 256, 64, ONE_TO_ONE, &c).expect("render");

    // The crop starts at x = 64. Walk the first 24 columns inside it — well within
    // the mask's reach, which is where a clamped apron would show.
    let mut worst = 0u8;
    for y in 8..56u32 {
        for x in 65..89u32 {
            let d = px(&part, 256, x, y)[0].abs_diff(px(&whole, 256, x, y)[0]);
            worst = worst.max(d);
        }
    }
    assert!(
        worst <= 1,
        "a halo along the crop edge: worst difference {worst} levels"
    );
}

#[test]
fn export_writes_the_crop_and_not_the_negative() {
    // Export reports and writes `output_dims`, which after a crop is the crop. An
    // export that handed back the whole frame would be a different picture from the
    // one on screen — and the caption above it would be a different number again.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(400, 200);
    let mut p = no_dither();
    p.composition.crop = Rect {
        x: 0.25,
        y: 0.5,
        w: 0.5,
        h: 0.25,
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &p.composition);
    assert_eq!(
        (frame.crop.x, frame.crop.y),
        (100, 100),
        "the fixture moved"
    );

    let mut vp = Viewport::new(&device, &queue, &luma);
    let (w, h, cropped) = vp
        .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
        .expect("export");
    assert_eq!((w, h), (200, 50), "export must be the crop's dims");

    // And the content is the right part of the negative: pixel (0, 0) of the export
    // is frame pixel (100, 100), which for this ramp is source column 100.
    let (_, _, whole) = vp
        .export(
            &mut ctx,
            &device,
            &queue,
            &no_dither(),
            &upright(&luma),
            |_, _| {},
        )
        .expect("export");
    for x in 0..200usize {
        let want = whole[100 * 400 + 100 + x];
        assert!(
            (cropped[x] - want).abs() < 1e-5,
            "cropped export is offset at x={x}: {} vs {want}",
            cropped[x]
        );
    }
}

#[test]
fn a_straighten_interpolates_rather_than_showing_the_rotation_staircase() {
    // At or above 100% the sampler shows real pixels, because a source pixel really
    // does sit under the sample position. A straighten breaks that premise: there is
    // no pixel there, and nearest-neighbour would draw the staircase of the rotation
    // across every edge in the picture.
    //
    // Measured on a smooth ramp, along a column. Straightened, each row reads from a
    // slightly different source column, so the column should walk smoothly. Under
    // nearest-neighbour it walks in stair-steps — runs of identical values broken by
    // jumps — so the test is on the size of the largest jump.
    let luma = ramp(256, 256);
    let mut p = no_dither();
    p.display.tone_map = ToneMap::AGX_DEFAULT; // keeps the whole ramp on screen
    let mut s = p.clone();
    s.composition.straighten = 4.0;

    let out = render_composed(&luma, 256, 256, ONE_TO_ONE, &s).expect("render");
    let w = 256;
    let mut jumps = 0;
    for y in 80..180u32 {
        let a = px(&out, w, 128, y)[0];
        let b = px(&out, w, 128, y + 1)[0];
        if a.abs_diff(b) > 3 {
            jumps += 1;
        }
    }
    assert_eq!(
        jumps, 0,
        "the straightened column stair-steps; sampling did not interpolate"
    );
}

#[test]
fn the_crop_tool_renders_the_whole_frame_while_it_is_open() {
    // The handoff's Capture One behaviour, end to end: with the tool open the
    // viewport shows the full uncropped frame so parts outside the crop can be seen
    // and re-grabbed. `Params::uncropped` is the substitution, and it must produce
    // the same pixels as never having cropped at all — the crop is suppressed, not
    // widened to something almost the same.
    let luma = ramp(64, 32);
    let mut p = no_dither();
    p.exposure.ev = 0.3;
    let mut c = p.clone();
    c.composition.crop = Rect {
        x: 0.3,
        y: 0.3,
        w: 0.3,
        h: 0.3,
    };

    let never = render_composed(&luma, 64, 32, ONE_TO_ONE, &p).expect("render");
    let open = render_composed(&luma, 64, 32, ONE_TO_ONE, &c.uncropped()).expect("render");
    assert_eq!(never, open, "the crop tool did not show the whole frame");

    // And the stored rectangle survived being suppressed, so the handles still have
    // somewhere to be drawn.
    assert_eq!(c.composition.crop.w, 0.3);
}

#[test]
fn a_crop_lands_on_the_same_picture_at_fit_as_at_one_to_one() {
    // **Four times now a bug in this app has existed only below 1:1** — the Contrast
    // Mask halo in milestone 3, the mount's antialiasing seam in 6b, the panel
    // runaway in 7b, and milestone 8's one-pixel panel. A crop rendered at fit is a
    // resample of a resample, and the crop rectangle is converted from frame pixels
    // to grid pixels by a rounding rule that only does anything when the scale is
    // not 1.
    //
    // Checked as a correspondence rather than as a hash: the same crop, rendered at
    // four scales, must put the same feature of the picture in the same *fraction*
    // of the output. A rounding error in the crop conversion shifts the picture
    // against its own frame edge, which is exactly what a fraction catches and a
    // per-pixel comparison across scales cannot express.
    let luma = bars(256, 256, 8);
    let mut p = no_dither();
    p.composition.crop = Rect {
        x: 0.25,
        y: 0.25,
        w: 0.5,
        h: 0.5,
    };

    // Where the crop's own edges land, as a fraction of the output, at each scale.
    // The view is the whole frame, so the crop sits at 25%..75% of it however far
    // out we are zoomed.
    for scale in [1.0f32, 0.5, 0.25, 0.128] {
        let (ow, oh) = (256, 256);
        let view = ViewGeometry {
            scale,
            off_x: 0.0,
            off_y: 0.0,
            ..Default::default()
        };
        let out = render_composed(&luma, ow, oh, view, &p).expect("render");

        // Walk the centre row and find where the picture starts and stops. Outside
        // the crop is the canvas, which at these settings is a flat 0.09 encoded.
        let canvas = (0.09f32 * 255.0).round() as u8;
        let row = (oh as f32 * 0.5 * scale) as u32;
        let lit: Vec<u32> = (0..ow)
            .filter(|x| px(&out, ow, *x, row.min(oh - 1))[0].abs_diff(canvas) > 6)
            .collect();
        assert!(!lit.is_empty(), "nothing was drawn at scale {scale}");

        let (first, last) = (*lit.first().expect("lit"), *lit.last().expect("lit"));
        // 256 frame px * 0.25 = 64, times the scale.
        let want_lo = 64.0 * scale;
        let want_hi = 192.0 * scale;
        assert!(
            (first as f32 - want_lo).abs() <= 2.0,
            "at scale {scale} the crop started at {first}, expected {want_lo}"
        );
        assert!(
            (last as f32 - want_hi).abs() <= 2.0,
            "at scale {scale} the crop ended at {last}, expected {want_hi}"
        );
    }
}

#[test]
fn a_cropped_frame_never_shows_canvas_inside_its_own_edge() {
    // The one-pixel class of bug, aimed at directly and measured as a **span**
    // rather than as a sample inside one. An earlier version of this test checked
    // two pixels in from each edge, which is precisely where the bug is not: the
    // dropped column is the last one, and stepping past it to be safe is how a test
    // for an off-by-one comes to be off by one. Verified by breaking the rounding
    // rule on purpose — that version passed.
    //
    // The crop's far edge is rounded up and its near edge down, so rounding can only
    // ever make the region a hair larger. Floor on the far edge drops the last
    // column at some zoom levels and not others, which reads as a thin line of
    // surround eating into the picture and is invisible unless you go looking.
    let luma = flat(0.6, 100, 100);
    let mut p = no_dither();
    p.composition.crop = Rect {
        x: 0.1,
        y: 0.1,
        w: 0.8,
        h: 0.8,
    };
    let canvas = (0.09f32 * 255.0).round() as u8;

    for scale in [1.0f32, 0.7, 0.55, 0.37, 0.33, 0.3, 0.15] {
        let view = ViewGeometry {
            scale,
            off_x: 0.0,
            off_y: 0.0,
            ..Default::default()
        };
        let out = render_composed(&luma, 200, 200, view, &p).expect("render");
        let row = ((50.0 * scale) as u32).min(199);
        let lit: Vec<u32> = (0..200)
            .filter(|x| px(&out, 200, *x, row)[0].abs_diff(canvas) > 6)
            .collect();
        let span = lit.len();
        // The crop is frame pixels 10..90, so the written region runs from
        // floor(10·s) to ceil(90·s) and every column in it is picture.
        //
        // The ceil carries `roi::ceil_px`'s slack, restated here rather than
        // approximated: 90 × 0.3 is 27.000002 in f32, and a bare ceil buys a whole
        // extra column for the last bits of the mantissa. Writing the rule out is
        // the point — this test exists to pin *which* rounding, and a looser
        // expectation passed with the rule deliberately broken.
        let ceil_px = |v: f32| (v - v * 4.0 * f32::EPSILON).ceil() as usize;
        let want = ceil_px(90.0 * scale) - (10.0 * scale).floor() as usize;
        assert_eq!(
            span, want,
            "at scale {scale} the crop drew {span} columns, not {want}"
        );
        // Contiguous, so a hole in the middle cannot pass as the right count.
        assert_eq!(
            lit.last().expect("lit") - lit.first().expect("lit") + 1,
            span as u32,
            "at scale {scale} the crop has a gap in it"
        );
    }
}

// ---------------------------------------------------------------- dodge & burn
//
// The CPU reference in `raw_core::dodgeburn` is the specification, and these tests
// exist to prove the compute shader implements *that* rather than something that
// looks similar. Comparisons go through `Viewport::export`, which hands back the
// scene-referred f32 the display node consumed — full precision, no 8-bit
// quantisation between the claim and the evidence.

use raw_core::dodgeburn::{
    Dab, DodgeBurnParams, Gesture, Instance, Linear, Nib, Radial, Shape, Sign, ZoneMask,
};

fn dab(x: f32, y: f32, radius: f32, feather: f32, ev: f32) -> Dab {
    Dab {
        x,
        y,
        radius,
        feather,
        opacity: 1.0,
        ev,
        ..Dab::ROUND
    }
}

fn instance(sign: Sign, gestures: Vec<Gesture>) -> Instance {
    Instance::of(sign, format!("{} 1", sign.label()), Shape::brush(gestures))
}

/// Export the scene-referred result. The curve is left at the identity so the
/// exported value is exactly the D&B node's output.
fn exported(luma: &LumaImage, params: &Params) -> (u32, u32, Vec<f32>) {
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &params.composition);
    let (device, queue) = headless_device().expect("an adapter");
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, luma);
    vp.export(&mut ctx, &device, &queue, params, &frame, |_, _| {})
        .expect("export")
}

#[test]
fn the_histogram_includes_contrast_mask_and_dodge_burn() {
    let (device, queue) = gpu!(headless_device());
    let luma = bars(96, 72, 7);
    let mut params = masked(4.0, 0.35, luma.output_dims);
    params.dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Dodge,
            vec![Gesture::new(vec![dab(0.43, 0.57, 0.2, 0.4, 0.8)])],
        )],
    };
    params.curve.add(0.45, 0.55);
    params.composition.orientation = Some(Orientation::Rotate90);
    params.composition.crop = Rect {
        x: 0.15,
        y: 0.2,
        w: 0.7,
        h: 0.65,
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &params.composition);
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (_, _, scene) = vp
        .export(&mut ctx, &device, &queue, &params, &frame, |_, _| {})
        .expect("export");

    let mut pending = vp
        .begin_histogram(&mut ctx, &device, &queue, &params, &frame)
        .expect("histogram dispatch");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let bins = loop {
        if let Some(result) = pending.poll(&device) {
            break result.expect("histogram readback");
        }
        assert!(std::time::Instant::now() < deadline, "histogram timed out");
        std::thread::yield_now();
    };

    let mut expected = [0u32; HISTOGRAM_BINS];
    for value in scene {
        expected[final_bin(value, &params)] += HISTOGRAM_WEIGHT;
    }
    assert_eq!(bins, expected);
}

#[test]
fn asynchronous_samples_match_spatially_adjusted_export_pixels() {
    let (device, queue) = gpu!(headless_device());
    let luma = bars(96, 72, 7);
    let mut params = masked(4.0, 0.35, luma.output_dims);
    params.dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Dodge,
            vec![Gesture::new(vec![dab(0.43, 0.57, 0.2, 0.4, 0.8)])],
        )],
    };
    params.composition.orientation = Some(Orientation::Rotate90);
    params.composition.crop = Rect {
        x: 0.15,
        y: 0.2,
        w: 0.7,
        h: 0.65,
    };
    let frame = Frame::resolve(luma.output_dims, Orientation::Rotate0, &params.composition);
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    vp.render(
        &mut ctx, &device, &queue, 64, 48, ONE_TO_ONE, &params, &frame,
    );
    assert_eq!(
        vp.read_back(&device, &queue).map(|v| (v.0, v.1)),
        Some((64, 48))
    );
    let (_, _, exported) = vp
        .export(&mut ctx, &device, &queue, &params, &frame, |_, _| {})
        .unwrap();
    let (x, y, w, h) = (frame.crop.x + 5, frame.crop.y + 4, 3, 3);
    let mut pending = vp
        .begin_patch(&mut ctx, &device, &queue, &params, &frame, x, y, w, h)
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (_, _, sampled) = loop {
        if let Some(result) = pending.poll(&device, &mut ctx) {
            break result.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sample readback timed out"
        );
        std::thread::yield_now();
    };
    let mut expected = Vec::new();
    for row in 0..h {
        let start = ((y - frame.crop.y + row as i32) as usize * frame.crop.w as usize)
            + (x - frame.crop.x) as usize;
        expected.extend_from_slice(&exported[start..start + w as usize]);
    }
    assert_eq!(sampled.len(), expected.len());
    for (got, expected) in sampled.iter().zip(expected) {
        assert!((got - expected).abs() < 1.0e-5, "{got} != {expected}");
    }
    assert_eq!(
        vp.read_back(&device, &queue).map(|v| (v.0, v.1)),
        Some((64, 48)),
        "sampling resized the live viewport target"
    );
}

#[test]
fn the_shader_agrees_with_the_cpu_reference() {
    // The whole of milestone 10's correctness, in one assertion: every pixel of a
    // real stroke set, against `DodgeBurnParams::ev_at`. The params below are
    // chosen to exercise each level of the composite at once — two instances of
    // opposite sign, one of them with two passes and an eraser pass over them,
    // overlapping dabs inside a pass, a hard dab and feathered ones, and a master
    // opacity that is not 1.
    let Some(_) = headless_device() else { return };
    let (w, h) = (128usize, 96usize);
    let luma = LumaImage {
        data: vec![0.25; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };

    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![
            Instance {
                opacity: 0.75,
                ..instance(
                    Sign::Burn,
                    vec![
                        // A pass whose dabs overlap: the MAX rule is what stops the
                        // overlap depositing twice.
                        Gesture::new(vec![
                            dab(0.30, 0.45, 0.14, 0.5, -0.8),
                            dab(0.36, 0.45, 0.14, 0.5, -0.8),
                            dab(0.42, 0.52, 0.14, 0.5, -0.8),
                        ]),
                        // A second pass over the first: passes SUM.
                        Gesture::new(vec![dab(0.34, 0.48, 0.10, 0.9, -0.5)]),
                        // An eraser pass: opposite sign, clamped at zero.
                        Gesture::new(vec![dab(0.30, 0.45, 0.08, 0.3, 1.2)]),
                    ],
                )
            },
            instance(
                Sign::Dodge,
                vec![Gesture::new(vec![
                    dab(0.70, 0.55, 0.16, 0.0, 1.1), // hard disc
                    dab(0.78, 0.40, 0.12, 0.7, 1.1),
                ])],
            ),
        ],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };

    let (ew, eh, out) = exported(&luma, &p);
    assert_eq!((ew as usize, eh as usize), (w, h));

    let aspect = h as f32 / w as f32;
    let mut worst = 0.0f32;
    for y in 0..h {
        for x in 0..w {
            // The shader reads `source_coord`, which at 1:1 with an untouched
            // composition is the pixel centre. Normalising by the stored image's
            // own dimensions is the whole coordinate contract.
            let nx = (x as f32 + 0.5) / w as f32;
            let ny = (y as f32 + 0.5) / h as f32;
            let want = 0.25 * p.dodgeburn.ev_at(nx, ny, aspect, &[]).exp2();
            let got = out[y * w + x];
            worst = worst.max((got - want).abs() / want.max(1e-6));
        }
    }
    // f32 arithmetic in a different order on a different device. A tenth of a
    // percent is far below anything visible and far above anything a structural
    // disagreement — a sum where a max belongs, a missing clamp — could hide in.
    assert!(
        worst < 1e-3,
        "worst relative disagreement with the CPU reference: {worst}"
    );
}

#[test]
fn layer_contrast_reshapes_detail_only_inside_the_painted_coverage() {
    let Some(_) = headless_device() else { return };
    // **Big enough that the detail scale outruns the bars.** The shared base is a
    // Gaussian at `CONTRAST_SCALE` of the working diagonal, floored at one pixel, so
    // a small fixture does not test what this asserts: at 128 x 96 sigma floors at
    // 1 px, which leaves about 29% of a four-pixel-period pattern standing in the
    // base and makes a full negative Contrast look like a partial one. 400 x 300
    // puts sigma at 2 px, where the bars are genuinely local detail.
    let (w, h) = (400usize, 300usize);
    // Two-pixel bars keep both sides of the local residual well sampled.
    let data: Vec<f32> = (0..w * h)
        .map(|i| if (i % w) / 2 % 2 == 0 { 0.2 } else { 0.4 })
        .collect();
    let luma = LumaImage {
        data: data.clone(),
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };
    let layer = Instance {
        contrast: 1.0,
        ..instance(
            Sign::Dodge,
            vec![Gesture::new(vec![dab(0.5, 0.5, 0.3, 0.0, 0.0)])],
        )
    };
    let mut p = Params {
        dodgeburn: DodgeBurnParams {
            enabled: true,
            instances: vec![layer],
        },
        ..Default::default()
    };
    let (_, _, more) = exported(&luma, &p);

    // Well inside the hard brush, positive contrast pushes the two sides of the
    // local residual farther apart.
    let y = h / 2;
    let x0 = w / 2;
    let x1 = x0 + 2;
    let before = (data[y * w + x0] - data[y * w + x1]).abs();
    let after = (more[y * w + x0] - more[y * w + x1]).abs();
    assert!(
        after > before * 1.5,
        "positive local contrast did not separate detail: {before} -> {after}"
    );

    // Outside the painted disc the pointwise composite is an exact identity even
    // though the shared blur had to read that part of the image.
    let outside = y * w + 4;
    assert!(
        (more[outside] - data[outside]).abs() < 1e-6,
        "contrast leaked outside its layer"
    );

    p.dodgeburn.instances[0].contrast = -1.0;
    let (_, _, less) = exported(&luma, &p);
    let softened = (less[y * w + x0] - less[y * w + x1]).abs();
    assert!(
        softened < before * 0.25,
        "negative local contrast did not soften detail: {before} -> {softened}"
    );

    // A flat field has no detail residual. Contrast alone must not become local
    // brightness or exposure.
    let flat_luma = flat(0.3, w, h);
    p.dodgeburn.instances[0].contrast = 1.0;
    let (_, _, flat_out) = exported(&flat_luma, &p);
    assert!(flat_out.iter().all(|v| (*v - 0.3).abs() < 1e-6));
}

#[test]
fn a_stroke_is_welded_to_the_negative_at_every_zoom() {
    // **The bug shape the brief names**: strokes are in normalised coordinates and
    // the view has a scale, so an error in the mapping is invisible at 100% and
    // obvious at 33%. Six milestones have now shipped a defect that existed only
    // below 1:1, so this renders the same burn at five scales and checks the dark
    // patch is over the same part of the PICTURE each time.
    let Some(_) = headless_device() else { return };
    let luma = flat(0.5, 200, 150);
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        // Deliberately off-centre in both axes: a burn at (0.5, 0.5) lands in the
        // middle whatever the mapping does with it, which is exactly the
        // degenerate test pattern 9b was caught by.
        instances: vec![instance(
            Sign::Burn,
            vec![Gesture::new(vec![dab(0.25, 0.7, 0.08, 0.0, -2.0)])],
        )],
    };
    let p = Params {
        dodgeburn,
        ..no_dither()
    };

    for scale in [1.0f32, 0.75, 0.5, 0.33, 0.25] {
        let view = ViewGeometry {
            scale,
            off_x: 0.0,
            off_y: 0.0,
            ..Default::default()
        };
        let out = render_composed(&luma, 220, 180, view, &p).expect("render");

        // Centroid of the darkened pixels, in output pixels, converted back to
        // normalised frame coordinates.
        let unburnt = px(&out, 220, 5, 5)[0];
        let (mut sx, mut sy, mut n) = (0.0f64, 0.0f64, 0u32);
        for y in 0..(150.0 * scale) as u32 {
            for x in 0..(200.0 * scale) as u32 {
                if unburnt.saturating_sub(px(&out, 220, x, y)[0]) > 20 {
                    sx += x as f64;
                    sy += y as f64;
                    n += 1;
                }
            }
        }
        assert!(n > 4, "at scale {scale} the burn did not render at all");
        let cx = (sx / n as f64) as f32 / (200.0 * scale);
        let cy = (sy / n as f64) as f32 / (150.0 * scale);
        // Half an output pixel at the coarsest scale is 1/50 of the frame, so the
        // tolerance is quantisation and nothing more.
        assert!(
            (cx - 0.25).abs() < 0.02,
            "at scale {scale} the burn sat at x={cx}, not 0.25"
        );
        assert!(
            (cy - 0.70).abs() < 0.02,
            "at scale {scale} the burn sat at y={cy}, not 0.70"
        );
    }
}

#[test]
fn a_dab_is_round_in_pixels_on_a_landscape_frame() {
    // The aspect correction, checked through the shader rather than only in the
    // model. A 2:1 frame: a hard dab of radius 0.1 must be 0.1 of the WIDTH in
    // both directions — which is 0.2 of the height — and not an ellipse.
    let Some(_) = headless_device() else { return };
    let (w, h) = (200usize, 100usize);
    let luma = LumaImage {
        data: vec![0.4; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Burn,
            vec![Gesture::new(vec![dab(0.5, 0.5, 0.1, 0.0, -1.0)])],
        )],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };

    let (_, _, out) = exported(&luma, &p);
    let burnt = |x: usize, y: usize| out[y * w + x] < 0.3;
    // 0.1 of the width is 20px; 20px vertically is 0.2 of the height.
    assert!(burnt(100 + 18, 50), "18px right of centre");
    assert!(!burnt(100 + 22, 50), "22px right of centre");
    assert!(burnt(100, 50 + 18), "18px below centre");
    assert!(!burnt(100, 50 + 22), "22px below centre");
}

#[test]
fn contrast_mask_zone_basis_matches_gpu_and_selects_middle_grey() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    for contrast in [0.05, 0.35, 0.60] {
        for exposure in [-2.0, 0.0, 1.5] {
            let luma = flat(0.19, 64, 48);
            let frame = upright(&luma);
            let mut vp = Viewport::new(&device, &queue, &luma);
            let mut p = masked(3.0, contrast, luma.output_dims);
            p.exposure.ev = exposure;
            p.exposure.black = 0.01;
            let basis = raw_core::zone::Basis::build(&luma, &p.exposure, &p.contrast_mask);
            let (_, _, pixels) = vp
                .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
                .unwrap();
            for (&cpu, &pixel) in basis.ev.iter().zip(&pixels) {
                let gpu = (pixel / 0.18).log2();
                assert!(
                    (cpu - gpu).abs() < 2e-5,
                    "contrast={contrast}, exposure={exposure}: CPU {cpu}, GPU {gpu}"
                );
            }
            // A narrow selection around the actual incoming tone must admit a
            // burn. At zero exposure this is exactly the original grey failure.
            let center = exposure * (1.0 - contrast);
            p.dodgeburn = DodgeBurnParams {
                enabled: true,
                instances: vec![Instance {
                    mask: ZoneMask {
                        enabled: true,
                        lo: center - 0.1,
                        hi: center + 0.1,
                        f_lo: 0.05,
                        f_hi: 0.05,
                        ..Default::default()
                    },
                    ..instance(
                        Sign::Burn,
                        vec![Gesture::new(vec![dab(0.5, 0.5, 0.9, 0.0, -1.0)])],
                    )
                }],
            };
            let (_, _, burned) = vp
                .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
                .unwrap();
            let center_pixel = 24 * 64 + 32;
            assert!(
                (burned[center_pixel] / pixels[center_pixel] - 0.5).abs() < 1e-4,
                "tonal mask missed its intended tone at contrast={contrast}, exposure={exposure}"
            );
        }
    }
}

#[test]
fn contrast_mask_zone_basis_matches_gpu_across_dark_and_bright_regions() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let mut luma = flat(0.18, 96, 64);
    for (i, value) in luma.data.iter_mut().enumerate() {
        let x = i % 96;
        *value = match x {
            0..24 => -0.01,
            24..48 => 0.0,
            48..72 => 0.18,
            _ => 0.72,
        };
    }
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    for contrast in [0.35, 0.60] {
        let p = masked(3.0, contrast, luma.output_dims);
        let basis = raw_core::zone::Basis::build(&luma, &p.exposure, &p.contrast_mask);
        let (_, _, pixels) = vp
            .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
            .unwrap();
        // Exclude the physical image edge: proxy blur clamps there while the
        // renderer samples an apron. Interior region transitions must agree.
        for y in 12..52 {
            for x in 12..84 {
                let i = y * 96 + x;
                let gpu = (pixels[i] / 0.18).log2();
                assert!(
                    (basis.ev[i] - gpu).abs() < 2e-4,
                    "contrast={contrast}, ({x}, {y}): CPU {}, GPU {gpu}",
                    basis.ev[i]
                );
            }
        }
    }
}

#[test]
fn a_zone_mask_holds_a_burn_to_its_own_tones() {
    // The mask, end to end: proxy, guided filter, trapezoid, strip upload,
    // bilinear read in the shader. A frame that is dark on the left and bright on
    // the right, one burn covering the whole width, and a Shadows mask on it. The
    // dark half must darken and the bright half must not.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 120usize);
    let data: Vec<f32> = (0..w * h)
        .map(|i| {
            if i % w < w / 2 {
                0.18 / 16.0
            } else {
                0.18 * 4.0
            }
        })
        .collect();
    let luma = LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };

    let (_, lo, hi, f_lo, f_hi) = ZoneMask::PRESETS[0]; // Shadows
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![Instance {
            mask: ZoneMask {
                enabled: true,
                lo,
                hi,
                f_lo,
                f_hi,
                ..Default::default()
            },
            ..instance(
                Sign::Burn,
                vec![Gesture::new(vec![dab(0.5, 0.5, 0.9, 0.0, -2.0)])],
            )
        }],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };
    let (_, _, masked) = exported(&luma, &p);

    // The same strokes with the mask switched off, as the control — otherwise a
    // burn that never rendered at all would pass the "bright half untouched" half
    // of this test.
    let mut open = p.clone();
    open.dodgeburn.instances[0].mask.enabled = false;
    let (_, _, plain) = exported(&luma, &open);

    let at = |v: &Vec<f32>, x: usize| v[60 * w + x];
    assert!(
        at(&plain, 20) < 0.18 / 16.0 * 0.3,
        "control: the dark half burns"
    );
    assert!(
        at(&plain, 140) < 0.18 * 4.0 * 0.3,
        "control: the bright half burns too"
    );

    assert!(
        at(&masked, 20) < 0.18 / 16.0 * 0.3,
        "masked: the shadows still burn"
    );
    assert!(
        at(&masked, 140) > 0.18 * 4.0 * 0.95,
        "masked: the highlights are spared, got {}",
        at(&masked, 140)
    );
}

#[test]
fn strokes_survive_a_quarter_turn_and_a_crop() {
    // Dabs are normalised to the STORED image, not the composed frame, so no
    // composition can move a stroke off what it was painted on. Rotating the
    // picture must carry the burn with it — the alternative, a burn that stays put
    // while the picture turns under it, is what frame-normalised coordinates would
    // have given.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 120usize);
    let luma = LumaImage {
        data: vec![0.5; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Burn,
            vec![Gesture::new(vec![dab(0.2, 0.25, 0.1, 0.0, -2.0)])],
        )],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };

    let (uw, uh, upright_out) = exported(&luma, &p);
    assert_eq!((uw, uh), (w as u32, h as u32));
    // Source pixel (32, 30) is the dab's centre.
    assert!(
        upright_out[30 * w + 32] < 0.2,
        "upright: the burn is where it was painted"
    );

    let mut turned = p.clone();
    turned.composition.orientation = Some(Orientation::Rotate90);
    let (tw, th, turned_out) = exported(&luma, &turned);
    assert_eq!(
        (tw, th),
        (h as u32, w as u32),
        "a quarter turn transposes the frame"
    );
    // A clockwise quarter turn sends source (x, y) to frame (h-1-y, x).
    let (fx, fy) = (h - 1 - 30, 32);
    assert!(
        turned_out[fy * (th as usize).min(h) + fx] < 0.2,
        "turned: the burn turned with the picture"
    );

    // And a crop moves the region without moving the strokes within it.
    let mut cropped = p.clone();
    cropped.composition.crop = Rect {
        x: 0.1,
        y: 0.1,
        w: 0.5,
        h: 0.5,
    };
    let (cw, _, cropped_out) = exported(&luma, &cropped);
    // Frame pixel (32, 30) is (32 - 16, 30 - 12) inside a crop starting at (16, 12).
    assert!(
        cropped_out[18 * cw as usize + 16] < 0.2,
        "cropped: the burn is still on the same part of the negative"
    );
}

#[test]
fn the_shader_agrees_with_the_cpu_reference_on_gradients() {
    // The same claim `the_shader_agrees_with_the_cpu_reference` makes for the brush,
    // for the two shapes 10b adds — and made separately rather than folded into it,
    // because the gradients are reached by a *different loop* in the shader. The
    // dab scan cannot see them; a second pass over the instance buffer picks them
    // up, and a test that only exercised the first would pass with the second
    // missing entirely.
    //
    // A diagonal linear and a turned, offset, inner-radiused ellipse on a
    // non-square frame, so both aspect corrections are under load: neither shape
    // is symmetric about an axis that would hide an error.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 96usize);
    let luma = LumaImage {
        data: vec![0.3; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };

    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![
            Instance {
                opacity: 0.8,
                ..Instance::of(
                    Sign::Burn,
                    "Linear Burn 1".into(),
                    Shape::Linear(Linear {
                        x0: 0.15,
                        y0: 0.2,
                        x1: 0.8,
                        y1: 0.75,
                        feather: 0.6,
                        ev: -1.4,
                    }),
                )
            },
            Instance::of(
                Sign::Dodge,
                "Radial Dodge 1".into(),
                Shape::Radial(Radial {
                    cx: 0.35,
                    cy: 0.6,
                    inner: 0.08,
                    outer: 0.3,
                    aspect: 1.8,
                    angle: 35.0,
                    feather: 0.7,
                    invert: false,
                    ev: 1.1,
                }),
            ),
            // And a brush beside them, so the two loops have to agree about which
            // instance is which — the dab scan indexes the same buffer the gradient
            // pass walks, and an off-by-one between them would apply the wrong
            // opacity to the wrong shape.
            instance(
                Sign::Burn,
                vec![Gesture::new(vec![dab(0.7, 0.3, 0.12, 0.5, -0.9)])],
            ),
        ],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };

    let (ew, eh, out) = exported(&luma, &p);
    assert_eq!((ew as usize, eh as usize), (w, h));

    let aspect = h as f32 / w as f32;
    let mut worst = 0.0f32;
    for y in 0..h {
        for x in 0..w {
            let nx = (x as f32 + 0.5) / w as f32;
            let ny = (y as f32 + 0.5) / h as f32;
            let want = 0.3 * p.dodgeburn.ev_at(nx, ny, aspect, &[]).exp2();
            worst = worst.max((out[y * w + x] - want).abs() / want.max(1e-6));
        }
    }
    assert!(
        worst < 1e-3,
        "worst relative disagreement on gradients: {worst}"
    );
}

#[test]
fn a_vignette_darkens_the_corners_and_not_the_middle() {
    // A property assertion beside the numeric one, because "it matches the CPU"
    // would still pass if both were wrong in the same direction. This says what a
    // vignette *is*, in a form that reads as a sentence when it fails.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 120usize);
    let luma = LumaImage {
        data: vec![0.4; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![Instance::of(
            Sign::Burn,
            "Radial Burn 1".into(),
            Shape::Radial(Radial {
                cx: 0.5,
                cy: 0.5,
                inner: 0.1,
                outer: 0.5,
                aspect: 1.0,
                angle: 0.0,
                feather: 1.0,
                invert: true,
                ev: -2.0,
            }),
        )],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };
    let (_, _, out) = exported(&luma, &p);

    let at = |x: usize, y: usize| out[y * w + x];
    assert!(
        (at(w / 2, h / 2) - 0.4).abs() < 1e-4,
        "the middle is untouched"
    );
    assert!(at(2, 2) < 0.2, "and the corner is burnt down: {}", at(2, 2));
    // The four corners burn equally, which is symmetry and not much else.
    for (x, y) in [(2, 2), (w - 3, 2), (2, h - 3), (w - 3, h - 3)] {
        assert!(
            (at(x, y) - at(2, 2)).abs() < 1e-4,
            "corner ({x}, {y}) differs"
        );
    }
    // **Round, not the frame's shape**, which is the claim that needs a different
    // pair of points to test. The four corners stay symmetric with the aspect
    // correction removed, so the assertion above passes against the bug — the first
    // version of this test stopped there and was checked, and it did.
    //
    // Forty pixels right of centre and forty pixels below it are the same distance
    // from the middle, so on a round vignette they must be equally burnt. On a
    // 160x120 frame an uncorrected one puts them at 0.25 and 0.33 of their
    // respective axes and reads them as different radii.
    let right = at(w / 2 + 40, h / 2);
    let below = at(w / 2, h / 2 + 40);
    assert!(
        (right - below).abs() < 5e-3,
        "the vignette is the frame's shape, not a circle: {right} across vs {below} down"
    );
}

#[test]
fn the_shader_agrees_with_the_cpu_reference_on_brush_shapes() {
    // 10c's shapes against `Dab::magnitude_at`, on a 2:1 frame so the frame's aspect
    // and the nib's own are both under load and neither can hide an error in the
    // other. Every dab is turned to a non-trivial angle for the same reason.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 80usize);
    let luma = LumaImage {
        data: vec![0.3; w * h],
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };

    let shaped = |x, y, aspect, angle, nib, feather| Dab {
        x,
        y,
        radius: 0.09,
        feather,
        opacity: 1.0,
        ev: -0.9,
        aspect,
        angle,
        nib,
    };
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Burn,
            vec![
                // A long soft ellipse, a turned card, and a hard round one, in one
                // pass — so the max-within-a-gesture rule runs over three shapes.
                Gesture::new(vec![
                    shaped(0.25, 0.4, 3.0, 34.0, Nib::Round, 0.6),
                    shaped(0.5, 0.55, 0.45, -62.0, Nib::Card, 0.35),
                    shaped(0.75, 0.35, 1.0, 0.0, Nib::Round, 0.0),
                ]),
                // And a second pass of cards, which is the gesture a straight edge
                // is actually used for.
                Gesture::new(
                    (0..6)
                        .map(|i| shaped(0.2 + i as f32 * 0.12, 0.75, 1.6, 15.0, Nib::Card, 0.5))
                        .collect(),
                ),
            ],
        )],
    };
    let p = Params {
        dodgeburn,
        ..Default::default()
    };

    let (_, _, out) = exported(&luma, &p);
    let frame = h as f32 / w as f32;
    let mut worst = 0.0f32;
    for y in 0..h {
        for x in 0..w {
            let nx = (x as f32 + 0.5) / w as f32;
            let ny = (y as f32 + 0.5) / h as f32;
            let want = 0.3 * p.dodgeburn.ev_at(nx, ny, frame, &[]).exp2();
            worst = worst.max((out[y * w + x] - want).abs() / want.max(1e-6));
        }
    }
    assert!(
        worst < 1e-3,
        "worst relative disagreement on brush shapes: {worst}"
    );
}

#[test]
fn a_card_lays_a_straight_edge_at_any_zoom() {
    // The gesture the card exists for: burning up to a straight edge. A round brush
    // scallops it, and a card must not — at 1:1 *or* zoomed out, which is where six
    // previous defects have hidden.
    let Some(_) = headless_device() else { return };
    let luma = flat(0.5, 200, 200);
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Burn,
            vec![Gesture::new(
                // Overlapping hard cards along a vertical line: their right edges
                // all sit at x = 0.5, so the composite edge is one straight line.
                (0..12)
                    .map(|i| Dab {
                        x: 0.4,
                        y: 0.05 + i as f32 * 0.09,
                        radius: 0.1,
                        feather: 0.0,
                        opacity: 1.0,
                        ev: -2.0,
                        nib: Nib::Card,
                        ..Dab::ROUND
                    })
                    .collect(),
            )],
        )],
    };
    let p = Params {
        dodgeburn,
        ..no_dither()
    };

    for scale in [1.0f32, 0.5, 0.33] {
        let view = ViewGeometry {
            scale,
            off_x: 0.0,
            off_y: 0.0,
            ..Default::default()
        };
        let out = render_composed(&luma, 220, 220, view, &p).expect("render");
        let side = (200.0 * scale) as u32;
        // Where the burn ends, row by row, down the middle of the run. A straight
        // edge means every row reports the same column; a scalloped one does not.
        let edges: Vec<u32> = (side / 5..side * 4 / 5)
            .map(|y| {
                (0..side)
                    .filter(|x| px(&out, 220, *x, y)[0] < 100)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let (lo, hi) = (
            edges.iter().copied().min().expect("rows"),
            edges.iter().copied().max().expect("rows"),
        );
        assert!(
            hi - lo <= 1,
            "at scale {scale} the card edge wandered {lo}..{hi}"
        );
    }
}

#[test]
fn a_patch_is_the_export_cut_out_of_it() {
    // **The grain loupe's foundation.** The loupe shows a few hundred thousand pixels
    // at 1:1 and claims they are the file's; that claim starts here, one layer below
    // the grain, and it is only true if `patch` and `export` are the same rendering
    // and not merely two renderings that look alike.
    //
    // A ramp rather than a flat field, because a flat one is degenerate in exactly
    // the axis an offset error lives in: every patch of it matches every other, so
    // the test would pass with the offset ignored entirely.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(240, 160);
    let frame = upright(&luma);
    let p = no_dither();
    let mut vp = Viewport::new(&device, &queue, &luma);

    let (fw, _, whole) = vp
        .export(&mut ctx, &device, &queue, &p, &frame, |_, _| {})
        .expect("export");

    // Two patches: one away from every edge, one straddling the far corner, which is
    // where a clamp or a sign error would show and nowhere else.
    for (ox, oy, w, h) in [(37i32, 29i32, 64u32, 48u32), (200, 130, 40, 30)] {
        let (pw, ph, patch) = vp
            .patch(&mut ctx, &device, &queue, &p, &frame, ox, oy, w, h)
            .expect("patch");
        assert_eq!((pw, ph), (w, h), "patch came back a different size");
        for y in 0..ph {
            for x in 0..pw {
                let want = whole[((oy as u32 + y) * fw + ox as u32 + x) as usize];
                let got = patch[(y * pw + x) as usize];
                assert!(
                    (want - got).abs() <= want.abs() * 1e-5 + 1e-6,
                    "patch at ({ox}, {oy}) pixel ({x}, {y}): {got} but the export has {want}"
                );
            }
        }
    }
}

#[test]
fn a_patch_follows_its_offset_and_repeats_exactly() {
    // Two claims the loupe rests on and that `a_patch_is_the_export_cut_out_of_it`
    // does not make: moving the sample point actually moves the sample, and asking
    // twice for the same tile gets the same pixels. The second is what lets the app
    // key a cached loupe render on its inputs and trust the cache — grain is
    // deterministic above this, but a viewport that quietly reused a pooled buffer
    // would make that determinism invisible.
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(200, 120);
    let frame = upright(&luma);
    let p = no_dither();
    let mut vp = Viewport::new(&device, &queue, &luma);

    let a = vp
        .patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 32, 32)
        .expect("first")
        .2;
    let b = vp
        .patch(&mut ctx, &device, &queue, &p, &frame, 96, 0, 32, 32)
        .expect("second")
        .2;
    assert_ne!(a, b, "the second patch came back as the first");

    // And the same request twice really is the same answer — determinism, not luck.
    let c = vp
        .patch(&mut ctx, &device, &queue, &p, &frame, 96, 0, 32, 32)
        .expect("third")
        .2;
    assert_eq!(b, c);
}

#[test]
fn the_mask_view_is_yellow_and_keeps_its_gradient() {
    // the maintainer asked for darktable's yellow, and the two halves of that are separable:
    // it has to be *yellow*, and it has to still be a *mask*. A tint that flooded the
    // frame would satisfy the first and destroy the second — the gradient across a
    // feathered zone is the whole reason to open this view.
    //
    // A frame dark on the left and bright on the right with a Shadows mask, so the
    // mask itself has somewhere to go from and to.
    let Some(_) = headless_device() else { return };
    let (w, h) = (160usize, 120usize);
    let data: Vec<f32> = (0..w * h)
        .map(|i| {
            if i % w < w / 2 {
                0.18 / 16.0
            } else {
                0.18 * 4.0
            }
        })
        .collect();
    let luma = LumaImage {
        data,
        output_dims: Dims { w, h },
        source_dims: Dims { w: w * 2, h: h * 2 },
        clipped: Vec::new(),
    };

    let (_, lo, hi, f_lo, f_hi) = ZoneMask::PRESETS[0]; // Shadows
    let dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![Instance {
            mask: ZoneMask {
                enabled: true,
                lo,
                hi,
                f_lo,
                f_hi,
                ..Default::default()
            },
            ..instance(
                Sign::Burn,
                vec![Gesture::new(vec![dab(0.5, 0.5, 0.9, 0.0, -2.0)])],
            )
        }],
    };
    let p = Params {
        dodgeburn,
        ..no_dither()
    };
    let frame = upright(&luma);

    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let view = ViewGeometry {
        overlays: Overlays {
            zone_mask: 0,
            ..Overlays::NONE
        },
        ..ONE_TO_ONE
    };
    vp.render(
        &mut ctx, &device, &queue, w as u32, h as u32, view, &p, &frame,
    );
    let (rw, _, buf) = vp.read_back(&device, &queue).expect("read back");

    // Inside the mask — the dark half — must be yellow: red full, green a little
    // under it, blue far below both. Grey would have all three equal, which is what
    // this shipped as before.
    let inside = px(&buf, rw, 20, 60);
    assert!(
        inside[0] > 200 && inside[1] > 150 && inside[2] < 80,
        "the mask is not yellow inside the zone: {inside:?}"
    );
    assert!(
        inside[0] > inside[1] && inside[1] > inside[2],
        "not a yellow ramp: {inside:?}"
    );

    // Outside it — the bright half, which the Shadows mask excludes — must be dark.
    // If the tint were a flood rather than a multiply this would be yellow too, and
    // the view would be telling you the mask covers the whole frame.
    let outside = px(&buf, rw, 140, 60);
    assert!(
        outside[0] < 60 && outside[1] < 60,
        "the mask view flooded the frame instead of multiplying: {outside:?}"
    );
    assert!(
        inside[0] as i32 - outside[0] as i32 > 120,
        "the mask's gradient did not survive the tint: {inside:?} vs {outside:?}"
    );
}

// ── Chemical toning, in the display pass ────────────────────────────────────────

fn toned(build: impl FnOnce(&mut raw_core::ToningParams)) -> Params {
    let mut p = no_dither();
    p.toning.enabled = true;
    build(&mut p.toning);
    p
}

/// **The migration guarantee, on the GPU.** An inactive toning module must render the
/// frame the app rendered before the module existed — not close, the same bytes.
///
/// This is the failure the design is most exposed to, because toning now lives *inside*
/// the display pass rather than in a node the graph can omit. A node that is not built
/// cannot change anything; a branch inside a shader that runs every frame can, and the
/// only thing standing between the two is `p.toning`.
#[test]
fn an_inactive_toning_module_renders_the_same_bytes() {
    let luma = ramp(64, 32);

    let before = gpu!(render(&luma, 64, 32, ONE_TO_ONE, &no_dither()));

    // Off by default.
    let after = gpu!(render(&luma, 64, 32, ONE_TO_ONE, &no_dither()));
    assert_eq!(before, after, "the default render moved");

    // Enabled, but with nothing in it — `is_active` is false, and the flag the shader
    // reads comes from `is_active` rather than from `enabled`.
    let empty = gpu!(render(&luma, 64, 32, ONE_TO_ONE, &toned(|_| {})));
    assert_eq!(before, empty, "an empty stack changed the picture");

    // A bath at zero strength is still nothing.
    let inert = gpu!(render(
        &luma,
        64,
        32,
        ONE_TO_ONE,
        &toned(|t| {
            t.apply("selenium", 0.0);
        })
    ));
    assert_eq!(before, inert, "a bath at zero strength changed the picture");
}

/// Toning on screen is toning: a selenium bath must actually leave the neutral axis,
/// and it must do so **in the shadows** rather than uniformly. The chemistry's shape
/// has to survive the trip through the table and the shader, not only the unit tests.
#[test]
fn a_selenium_bath_tints_the_shadows_and_not_the_paper() {
    let luma = ramp(64, 32);

    let plain = gpu!(render(&luma, 64, 32, ONE_TO_ONE, &no_dither()));
    let selenium = gpu!(render(
        &luma,
        64,
        32,
        ONE_TO_ONE,
        &toned(|t| {
            t.apply("selenium", 1.0);
        })
    ));

    assert_ne!(plain, selenium, "a full-strength selenium bath did nothing");

    // The ramp runs dark to light across x. Find the largest departure from neutral in
    // each half and compare: the shadows must carry more.
    let spread = |buf: &[u8], from: u32, to: u32| {
        let mut worst = 0i32;
        for x in from..to {
            for y in 0..32 {
                let [r, g, b, _] = px(buf, 64, x, y);
                let hi = r.max(g).max(b) as i32;
                let lo = r.min(g).min(b) as i32;
                worst = worst.max(hi - lo);
            }
        }
        worst
    };

    let shadows = spread(&selenium, 0, 24);
    let highlights = spread(&selenium, 48, 64);
    assert!(
        shadows > 2,
        "the shadows should be visibly toned, got {shadows}"
    );
    assert!(
        shadows > highlights,
        "toning should favour the shadows: shadows {shadows}, highlights {highlights}"
    );
}

/// **The rubylith, which the colour transition made possible.**
///
/// The dodge and burn maps were greyscale for one reason and it was the pipeline: the
/// chain carried one channel, so there was nothing to tint with. This holds the two
/// halves of what replaced that — the maps are tinted, and the tint *multiplies*, so
/// the map's own gradient is still there to judge.
#[test]
fn the_dodge_and_burn_maps_are_tinted_rather_than_grey() {
    let luma = ramp(64, 8);
    let mut params = no_dither();
    // A burn instance, so there is a map with something in it.
    params.dodgeburn.enabled = true;
    params.dodgeburn.instances.push(instance(
        Sign::Burn,
        vec![Gesture::new(vec![dab(0.5, 0.5, 0.2, 0.5, -2.0)])],
    ));

    let grey = gpu!(render(&luma, 64, 8, ONE_TO_ONE, &params));

    let mut view = ONE_TO_ONE;
    view.overlays = Overlays {
        burn_map: true,
        ..Overlays::NONE
    };
    let toned = gpu!(render(&luma, 64, 8, view, &params));

    assert_ne!(grey, toned, "the burn map came out the same as the picture");

    // Tinted: somewhere the channels differ.
    let coloured = toned.chunks_exact(4).any(|p| p[0] != p[1] || p[1] != p[2]);
    assert!(coloured, "the burn map is still neutral");

    // **Multiplied, not added**, which is the half that matters: the map's own
    // gradient is what the key is held to judge, and an additive tint would flood the
    // unpainted ground and hide the boundary.
    //
    // The test for a multiply is that the channel *ratios* are constant — `g = o·kg`
    // and `r = o·kr` give the same `g/r` at every brightness, while an add does not.
    // Only pixels with something in them are sampled; at the bottom of the range 8-bit
    // quantisation makes a ratio meaningless.
    let ratios: Vec<f32> = toned
        .chunks_exact(4)
        .filter(|p| p[0] > 64)
        .map(|p| p[1] as f32 / p[0] as f32)
        .collect();
    assert!(
        ratios.len() > 16,
        "not enough painted pixels to judge: {}",
        ratios.len()
    );
    let lo = ratios.iter().cloned().fold(f32::MAX, f32::min);
    let hi = ratios.iter().cloned().fold(0.0f32, f32::max);
    assert!(
        hi - lo < 0.03,
        "the tint is not a multiply: g/r ran {lo:.3} to {hi:.3}"
    );

    // And the unpainted ground stays black, which is the same statement from the other
    // end and the one a reader will look for.
    let darkest = toned
        .chunks_exact(4)
        .map(|p| p[0].max(p[1]).max(p[2]))
        .min()
        .unwrap();
    assert!(darkest < 24, "the tint lifted the black floor to {darkest}");
}

/// A map is an editing aid, so a normal brush amount must be more visible than the
/// photograph retained merely to locate it. This is intentionally a modest 0.5 EV
/// stroke rather than the 2 EV stress case above: dividing by the 4 EV ceiling used
/// to make this practical range almost disappear.
#[test]
fn a_half_stop_is_clear_in_the_dodge_map() {
    let luma = flat(0.18, 64, 64);
    let mut params = no_dither();
    params.dodgeburn.enabled = true;
    params.dodgeburn.instances.push(instance(
        Sign::Dodge,
        vec![Gesture::new(vec![dab(0.5, 0.5, 0.18, 0.35, 0.5)])],
    ));

    let mut view = ONE_TO_ONE;
    view.overlays = Overlays {
        dodge_map: true,
        ..Overlays::NONE
    };
    let map = gpu!(render(&luma, 64, 64, view, &params));
    let ground = px(&map, 64, 4, 4)[0];
    let stroke = px(&map, 64, 32, 32)[0];
    assert!(
        stroke > ground.saturating_add(60),
        "a half-stop stroke is still lost in the ground: {stroke} over {ground}"
    );
}

/// **Preview and export must be the same picture, and once they were not.**
///
/// The CPU tail passed `Toned::y` — a display-referred *luminance* — where OKLab wants
/// a *lightness*, which is its cube root. The shader converted correctly. So the
/// viewport looked right and every exported file came out several stops too dark, which
/// is the exact divergence the single baked table was supposed to make impossible: the
/// table was shared, and the thing done with its output was not.
///
/// This compares the two ends against each other rather than each against its own
/// idea of correct, which is the only comparison that can catch it.
#[test]
fn the_export_agrees_with_the_shader() {
    use raw_core::colour;

    let luma = ramp(64, 8);
    let mut params = no_dither();
    params.display.tone_map = ToneMap::Clip;
    params.toning.enabled = true;
    params.toning.process = raw_core::Process::Albumen;
    params.toning.apply("gold-gp1", 0.85);

    let shown = gpu!(render(&luma, 64, 8, ONE_TO_ONE, &params));

    // The same chain the export tail runs: tone map, then the table, then OKLab out to
    // the display's own primaries and the display's gamma.
    for x in [4u32, 20, 40, 60] {
        let scene = luma.data[(x + 4 * 64) as usize];
        let y = raw_core::display::tone_map(scene, params.display.tone_map);
        let toned = params.toning.evaluate(y);
        let (a, b) = toned.ab();
        let lin = colour::oklab_to_display_srgb(colour::oklab_lightness(toned.y), a, b);
        let want: Vec<u8> = lin
            .iter()
            .map(|v| (v.clamp(0.0, 1.0).powf(1.0 / params.display.gamma) * 255.0).round() as u8)
            .collect();

        let got = px(&shown, 64, x, 4);
        for c in 0..3 {
            let apart = (got[c] as i32 - want[c] as i32).abs();
            assert!(
                apart <= 2,
                "at x={x} channel {c}: shader {} against the CPU's {} (whole pixel {got:?} \
                 against {want:?})",
                got[c],
                want[c]
            );
        }
    }
}

#[test]
fn the_pool_learns_recent_sizes_after_its_initial_slots_fill() {
    let (device, _) = gpu!(headless_device());
    let mut pool = raw_gpu::TexturePool::new();
    for i in 1..=12 {
        let lease = pool.acquire(&device, i * 128, 128, wgpu::TextureFormat::Rg32Float);
        pool.release(lease);
    }
    let warm = pool.allocations();
    for _ in 0..30 {
        let lease = pool.acquire(&device, 1664, 128, wgpu::TextureFormat::Rg32Float);
        pool.release(lease);
    }
    assert_eq!(
        pool.allocations() - warm,
        1,
        "a new steady size keeps allocating"
    );
}

#[test]
fn export_only_edits_do_not_dispatch_the_viewport() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 128, 128);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = no_dither();
    assert!(vp.render(
        &mut ctx,
        &device,
        &queue,
        128,
        128,
        ONE_TO_ONE,
        &p,
        &upright(&luma)
    ));
    p.output.ppi += 1.0;
    p.grain.enabled = !p.grain.enabled;
    assert!(!vp.render(
        &mut ctx,
        &device,
        &queue,
        128,
        128,
        ONE_TO_ONE,
        &p,
        &upright(&luma)
    ));
    p.exposure.ev += 1.0;
    assert!(vp.render(
        &mut ctx,
        &device,
        &queue,
        128,
        128,
        ONE_TO_ONE,
        &p,
        &upright(&luma)
    ));
}

#[test]
fn unmasked_edits_and_source_changes_do_not_prepare_zones() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 128, 128);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = no_dither();
    p.dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![instance(
            Sign::Burn,
            vec![Gesture::new(vec![dab(0.5, 0.5, 0.9, 0.0, -1.0)])],
        )],
    };
    vp.render(
        &mut ctx,
        &device,
        &queue,
        128,
        128,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    let warm = vp.basis_rebuilds();
    assert_eq!(warm, 0);
    p.contrast_mask.enabled = true;
    p.contrast_mask.spacer = 5.0;
    p.curve.add(0.5, 0.7);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        256,
        256,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    assert_eq!(vp.basis_rebuilds(), warm);
    p.exposure.ev += 1.0;
    vp.render(
        &mut ctx,
        &device,
        &queue,
        256,
        256,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    assert_eq!(vp.basis_rebuilds(), warm);
    vp.set_image(&device, &queue, &luma);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        256,
        256,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    assert_eq!(vp.basis_rebuilds(), warm);
}

#[test]
fn interactive_zone_jobs_preserve_the_previous_frame_and_match_blocking_results() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(193, 129);
    let frame = upright(&luma);
    let mut p = masked(24.0, 0.35, luma.output_dims);
    p.display.dither = false;
    p.dodgeburn = DodgeBurnParams {
        enabled: true,
        instances: vec![Instance {
            mask: ZoneMask {
                enabled: true,
                hi: 0.0,
                ..Default::default()
            },
            ..instance(
                Sign::Burn,
                vec![Gesture::new(vec![dab(0.5, 0.5, 0.9, 0.0, -2.0)])],
            )
        }],
    };
    let mut vp = Viewport::new(&device, &queue, &luma);
    vp.set_interactive_zones(true);
    assert!(!vp.render(&mut ctx, &device, &queue, 128, 96, ONE_TO_ONE, &p, &frame));
    assert!(vp.zone_render_pending());
    assert!(vp.target_view().is_none());
    let finish = |vp: &mut Viewport, ctx: &mut GpuContext, p: &Params| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let drawn = vp.render(ctx, &device, &queue, 128, 96, ONE_TO_ONE, p, &frame);
            if !vp.zone_render_pending() {
                assert!(drawn);
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    finish(&mut vp, &mut ctx, &p);
    let before = vp.read_back(&device, &queue).unwrap();
    p.exposure.ev = 0.5;
    assert!(!vp.render(&mut ctx, &device, &queue, 128, 96, ONE_TO_ONE, &p, &frame));
    assert!(vp.zone_render_pending());
    assert_eq!(before, vp.read_back(&device, &queue).unwrap());
    // Replacing the source while a job runs must not install its obsolete masks.
    let replacement = flat(0.12, 193, 129);
    vp.set_image(&device, &queue, &replacement);
    finish(&mut vp, &mut ctx, &p);
    let mut fresh = Viewport::new(&device, &queue, &replacement);
    fresh.render(&mut ctx, &device, &queue, 128, 96, ONE_TO_ONE, &p, &frame);
    assert_eq!(
        vp.read_back(&device, &queue),
        fresh.read_back(&device, &queue)
    );
    let builds = vp.basis_rebuilds();
    p.dodgeburn.instances[0].mask.hi = 1.0;
    finish(&mut vp, &mut ctx, &p);
    assert_eq!(
        vp.basis_rebuilds(),
        builds,
        "zone bounds rebuilt the exposure/mask basis"
    );
    // Explicit panel demand obtains a current histogram from the same basis.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(hist) = vp.request_zone_histogram(&p) {
            assert_eq!(hist.len(), Viewport::ZONE_BINS);
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(vp.basis_rebuilds(), builds);
    // Asynchronous sampling retries, while the explicitly blocking patch API
    // waits for the same correct result and restores interactive mode afterward.
    p.exposure.ev += 0.25;
    assert!(
        vp.begin_patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 3, 3)
            .is_none()
    );
    let patch = vp
        .patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 3, 3)
        .unwrap();
    let expected = fresh
        .patch(&mut ctx, &device, &queue, &p, &frame, 0, 0, 3, 3)
        .unwrap();
    assert_eq!(patch, expected);
    p.exposure.ev += 0.25;
    assert!(!vp.render(&mut ctx, &device, &queue, 128, 96, ONE_TO_ONE, &p, &frame));
    assert!(vp.zone_render_pending());
}

#[test]
fn a_comparison_cell_cannot_leave_its_curve_in_the_live_viewport() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 64, 64);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut live = no_dither();
    live.curve.add(0.5, 0.6);
    let mut comparison = no_dither();
    comparison.curve.add(0.5, 0.8);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &live,
        &upright(&luma),
    );
    let before = vp.read_back(&device, &queue).unwrap().2;
    let mut cell = raw_gpu::Cell::default();
    vp.render_into(
        &mut cell,
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &comparison,
        &upright(&luma),
    );
    vp.render(
        &mut ctx,
        &device,
        &queue,
        64,
        64,
        ONE_TO_ONE,
        &live,
        &upright(&luma),
    );
    assert_eq!(vp.read_back(&device, &queue).unwrap().2, before);
}

#[test]
fn idle_texture_bytes_are_bounded_and_oversized_returns_are_not_retained() {
    let (device, _) = gpu!(headless_device());
    let mut pool = raw_gpu::TexturePool::with_byte_budget(1024 * 1024);
    let first = pool.acquire(&device, 256, 256, wgpu::TextureFormat::Rg32Float);
    let recent = pool.acquire(&device, 384, 256, wgpu::TextureFormat::Rg32Float);
    pool.release(first);
    pool.release(recent);
    assert_eq!(pool.idle(), 1);
    assert_eq!(pool.idle_bytes(), 384 * 256 * 8);
    let oversized = pool.acquire(&device, 512, 512, wgpu::TextureFormat::Rg32Float);
    pool.release(oversized);
    assert_eq!(pool.idle(), 1);
    let warm = pool.allocations();
    let recent = pool.acquire(&device, 384, 256, wgpu::TextureFormat::Rg32Float);
    assert_eq!(pool.allocations(), warm);
    assert_eq!(pool.idle_bytes(), 0);
    pool.release(recent);
    pool.clear();
    assert_eq!(pool.idle_bytes(), 0);
    assert_eq!(pool.idle(), 0);
}

#[test]
fn a_large_contrast_mask_view_stays_warm_within_the_idle_budget() {
    let (device, queue) = gpu!(headless_device());
    let mut ctx = GpuContext::new(&device);
    let luma = flat(0.5, 2048, 1024);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let mut p = no_dither();
    p.contrast_mask.enabled = true;
    vp.render(
        &mut ctx,
        &device,
        &queue,
        2048,
        1024,
        ONE_TO_ONE,
        &p,
        &upright(&luma),
    );
    let warm = ctx.pool().allocations();
    for i in 1..=3 {
        p.exposure.ev = i as f32 * 0.1;
        vp.render(
            &mut ctx,
            &device,
            &queue,
            2048,
            1024,
            ONE_TO_ONE,
            &p,
            &upright(&luma),
        );
    }
    assert_eq!(
        ctx.pool().allocations(),
        warm,
        "the default idle budget cannot retain this working set"
    );
    assert!(ctx.pool().idle_bytes() <= 256 * 1024 * 1024);
    eprintln!(
        "large Contrast Mask: {warm} warm allocations, {} idle bytes",
        ctx.pool().idle_bytes()
    );
}

/// End-to-end tiled render, blocking readback and CPU assembly; excludes source
/// upload, context creation, file encoding and the application's finishing stages.
#[test]
#[ignore = "performance measurement; run explicitly with --release --nocapture"]
fn measure_tiled_export_and_readback() {
    use std::{hint::black_box, time::Instant};
    let (device, queue) = headless_device().expect("measurement requires a GPU adapter");
    let mut ctx = GpuContext::new(&device);
    let luma = ramp(6000, 4000);
    let frame = upright(&luma);
    let mut vp = Viewport::new(&device, &queue, &luma);
    for mask in [false, true] {
        let mut params = Params::default();
        params.contrast_mask.enabled = mask;
        let mut samples = Vec::new();
        for trial in 0..4 {
            let start = Instant::now();
            let output = vp
                .export(&mut ctx, &device, &queue, &params, &frame, |_, _| {})
                .unwrap();
            let elapsed = start.elapsed();
            black_box(&output);
            if trial != 0 {
                samples.push(elapsed);
            }
        }
        samples.sort();
        eprintln!(
            "24MP tiled export/readback, Contrast Mask {mask}: median {:?}",
            samples[1]
        );
    }
}
