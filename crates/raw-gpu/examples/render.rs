//! Headless render of the milestone-1 chain, for inspection and regression checks.
//!
//!     cargo run --release --example render -- <raw> <out.ppm> [options]
//!
//!     --scale S        screen px per output px (1.0 = 100%)
//!     --at X Y         top-left of the region, in output-pixel coords
//!     --size W H       output extent in pixels (default 1200x800)
//!     --exposure EV
//!     --gamma G
//!     --agx            AgX tone map instead of a hard clip
//!     --no-dither
//!     --rotate D       override the file's EXIF orientation: 0, 90, 180, 270
//!     --straighten D   degrees clockwise
//!     --crop X Y W H   crop, as fractions of the frame
//!
//! **The EXIF orientation is honoured by default**, as it is in the app, so this
//! renders the picture the right way up rather than the way the sensor read it.

use raw_core::composition::{Orientation, Rect};
use raw_core::{Frame, Params, Sampling, SensorImage, ToneMap, Weighting, scene};
use raw_gpu::{GpuContext, ViewGeometry, Viewport};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: render <raw> <out.ppm> [options]");
        std::process::exit(2);
    }
    let flag = |name: &str| args.iter().any(|a| a == name);
    let val = |name: &str, n: usize| -> Option<Vec<f32>> {
        let i = args.iter().position(|a| a == name)?;
        let v: Vec<f32> = args[i + 1..]
            .iter()
            .take(n)
            .filter_map(|s| s.parse().ok())
            .collect();
        (v.len() == n).then_some(v)
    };

    let scale = val("--scale", 1).map_or(1.0, |v| v[0]);
    let at = val("--at", 2).unwrap_or(vec![0.0, 0.0]);
    let size = val("--size", 2).unwrap_or(vec![1200.0, 800.0]);
    let exposure = val("--exposure", 1).map_or(0.0, |v| v[0]);
    let gamma = val("--gamma", 1).map_or(2.2, |v| v[0]);

    let sensor = SensorImage::load(std::path::Path::new(&args[0])).expect("load");
    let (sc, mask) = scene::decode(
        &sensor,
        scene::DecodeOptions {
            unity_wb: flag("--unity-wb"),
        },
    );
    let clipped = mask.0.iter().filter(|c| **c).count();

    let lo = sc.data.iter().cloned().fold(f32::INFINITY, f32::min);
    let hi = sc.data.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let over1 = sc.data.iter().filter(|v| **v > 1.0).count();
    println!(
        "scene    min {lo:.4}  max {hi:.4}  >1.0: {:.3}%  clipped: {:.3}%",
        100.0 * over1 as f32 / sc.data.len() as f32,
        100.0 * clipped as f32 / mask.0.len() as f32
    );

    let sampling = match args
        .iter()
        .position(|a| a == "--sampling")
        .and_then(|i| args.get(i + 1))
    {
        Some(s) if s == "direct" => Sampling::DirectMosaic,
        Some(s) if s == "demosaic" => Sampling::Demosaic(scene::DemosaicAlgo::Bilinear),
        _ => Sampling::SuperPixel,
    };
    let weighting = match args
        .iter()
        .position(|a| a == "--weight")
        .and_then(|i| args.get(i + 1))
    {
        Some(s) if s == "equal" => Weighting::Equal,
        Some(s) if s == "green" => Weighting::Green,
        Some(s) if s == "red" => Weighting::Red,
        Some(s) if s == "blue" => Weighting::Blue,
        _ => Weighting::Photosite,
    };
    let luma = scene::derive_luminance(&sc, sampling, weighting);
    println!(
        "sampling {sampling:?}  weight {weighting:?}  output {} x {}",
        luma.output_dims.w, luma.output_dims.h
    );

    let (device, queue) = raw_gpu::headless_device().expect("no usable GPU adapter");

    let mut params = Params::default();
    params.exposure.ev = exposure;
    params.display.gamma = gamma;
    params.display.dither = !flag("--no-dither");
    if flag("--agx") {
        params.display.tone_map = ToneMap::AGX_DEFAULT;
    }
    params.composition.orientation = val("--rotate", 1).map(|v| match v[0] as i32 {
        90 => Orientation::Rotate90,
        180 => Orientation::Rotate180,
        270 => Orientation::Rotate270,
        _ => Orientation::Rotate0,
    });
    params.composition.straighten = val("--straighten", 1).map_or(0.0, |v| v[0]);
    if let Some(c) = val("--crop", 4) {
        params.composition.crop = Rect {
            x: c[0],
            y: c[1],
            w: c[2],
            h: c[3],
        };
    }
    // As the app resolves it: the file's tag unless `--rotate` overrode it.
    let frame = Frame::resolve(
        luma.output_dims,
        sensor.meta.orientation,
        &params.composition,
    );
    println!(
        "frame    {:?}  {} x {}  crop {} x {}",
        frame.orientation, frame.frame.w, frame.frame.h, frame.crop.w, frame.crop.h
    );

    let mut ctx = GpuContext::new(&device);
    let mut vp = Viewport::new(&device, &queue, &luma);
    let (w, h) = (size[0] as u32, size[1] as u32);
    vp.render(
        &mut ctx,
        &device,
        &queue,
        w,
        h,
        ViewGeometry {
            scale,
            off_x: at[0],
            off_y: at[1],
            ..Default::default()
        },
        &params,
        &frame,
    );

    let (rw, rh, rgba) = vp.read_back(&device, &queue).expect("readback");
    let mut ppm = format!("P6\n{rw} {rh}\n255\n").into_bytes();
    for px in rgba.chunks_exact(4) {
        ppm.extend_from_slice(&px[..3]);
    }
    std::fs::write(&args[1], ppm).expect("write");
    println!("wrote    {} ({rw}x{rh})", args[1]);
}
