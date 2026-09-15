//! CPU-stage measurements, excluding RAW load and GPU upload.
//! Run with `cargo run --release -p raw-core --example measure_cpu -- <raw paths>`.
use raw_core::dodgeburn::{Dab, Gesture, Instance, Shape, Sign};
use raw_core::{Params, Sampling, SensorImage, scene};
use std::{
    hint::black_box,
    path::Path,
    time::{Duration, Instant},
};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const COPIES: u32 = 20_000;
    for path in std::env::args().skip(1) {
        let sensor = SensorImage::load(Path::new(&path))?;
        let (scene, _) = scene::decode(&sensor, Default::default());
        for sampling in [
            Sampling::SuperPixel,
            Sampling::DirectMosaic,
            Sampling::default(),
        ] {
            let mut samples = Vec::new();
            for _ in 0..5 {
                let start = Instant::now();
                let luma = scene::derive_luminance(
                    &scene,
                    sampling,
                    Params::default().luminance.weighting,
                );
                samples.push(start.elapsed());
                black_box(&luma);
            }
            println!(
                "{} {sampling:?}: median {:?}",
                sensor.camera,
                median(samples)
            );
        }
    }
    for count in [0, 10_000, 100_000] {
        let mut params = Params::default();
        if count != 0 {
            params.dodgeburn.instances.push(Instance::of(
                Sign::Dodge,
                "measurement".into(),
                Shape::brush(vec![Gesture::new(vec![
                    Dab {
                        radius: 0.02,
                        ev: 1.0,
                        ..Dab::ROUND
                    };
                    count
                ])]),
            ));
        }
        let mut samples = Vec::new();
        for _ in 0..9 {
            let start = Instant::now();
            let copies: Vec<_> = (0..COPIES).map(|_| black_box(&params).clone()).collect();
            samples.push(start.elapsed() / COPIES);
            black_box(&copies);
        }
        println!(
            "Params clone {count} dabs ({} dab bytes/copy): median {:?}",
            count * std::mem::size_of::<Dab>(),
            median(samples)
        );

        let twin = params.clone();
        let mut samples = Vec::new();
        for _ in 0..9 {
            let start = Instant::now();
            for _ in 0..COPIES {
                black_box(black_box(&params).diff(black_box(&twin)));
            }
            samples.push(start.elapsed() / COPIES);
        }
        println!(
            "Params unchanged diff {count} dabs: median {:?}",
            median(samples)
        );
    }
    Ok(())
}
