//! Why a single-channel DirectMosaic export prints black, in numbers.
//!
//! the maintainer: exporting DirectMosaic with Photosite luminance is fine, and exporting it
//! with one channel comes out black. This is the arithmetic behind that, kept as an
//! example because the claim is about measured values and a reader should be able to
//! re-run it rather than take the table on trust.
//!
//! ```text
//! cargo run -p raw-core --example mosaic-print
//! ```
//!
//! # Two separate failures, and only one of them is recoverable
//!
//! `direct_mosaic` gives a single-channel weighting a gain of `4/density`, so on Bayer
//! a Red weighting is `[4, 0, 0]`: one photosite in four carries four times its value
//! and the other three carry **exactly zero**. That preserves the *linear* mean, which
//! is the documented intent and is why the histogram looks right.
//!
//! **1. The encode runs per pixel, before anything integrates it.** Export order is
//! tone map, resize, grain, sharpen, encode — so at native size nothing averages the
//! mosaic and each pixel meets `lstar_encode` alone. L\* is concave, so encoding three
//! zeros and one spike gives a far darker mean than encoding the average would:
//! Jensen's inequality, and the `Red/native` column is what it costs. Recoverable —
//! the `Red/half` column is the same data resized on export, which happens *before* the
//! encode and therefore integrates in linear light. It matches Photosite exactly.
//!
//! **2. The 4x gain clips everything above a quarter scene-value.** Not recoverable by
//! anything downstream, because the data is gone by then. Above `Y = 0.25` the hot
//! photosite pins to 1.0 and both columns flatten — a normally exposed frame loses its
//! entire upper range to a single value. This is the one that makes it read as *black
//! and flat* rather than merely dark.
//!
//! The conclusion the table supports: **single-channel DirectMosaic is a diagnostic
//! view, not a rendering.** Three of every four pixels never held a measurement.
//! SuperPixel with the same weighting is the printable form — one channel, integrated
//! in linear light, no gain to clip — and it is what the `Red/half` column already is.

use raw_core::display::lstar_encode;

fn main() {
    println!("scene Y is the gain-equalised linear value every photosite carries.");
    println!("Columns are L* 0-100, which is what the file stores.\n");
    println!(
        "{:>8} {:>12} {:>12} {:>12}",
        "scene Y", "Photosite", "Red/native", "Red/half"
    );

    for y in [0.05f32, 0.1, 0.18, 0.25, 0.5] {
        // Photosite on DirectMosaic: gain [1,1,1], so every pixel carries `y`.
        let photosite = lstar_encode(y) * 100.0;

        // Red on DirectMosaic: gain [4,0,0]. The tone map is Clip, the shipped default,
        // so the hot photosite pins at 1.0 rather than rolling off.
        let hot = (4.0 * y).min(1.0);

        // Encoded per pixel at native size, then integrated by the print or the eye.
        let native = (lstar_encode(hot) * 100.0) / 4.0;

        // The same four integrated in LINEAR light first, which is what a resize on
        // export does — it runs before the encode. See `export::write`.
        let half = lstar_encode(hot / 4.0) * 100.0;

        println!("{y:>8.2} {photosite:>12.1} {native:>12.1} {half:>12.1}");
    }

    println!("\nRed/half tracks Photosite exactly: integrating before the encode is the fix.");
    println!("Both Red columns flatten at Y >= 0.25: that is the 4x gain clipping, and");
    println!("no downstream stage can undo it.");
}
