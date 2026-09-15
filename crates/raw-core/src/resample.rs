//! Resampling an image to output size. The stage between the export tap and the
//! encoder, and the one output sharpening sharpens.
//!
//! # Where this runs, and why it is not scene-referred
//!
//! Export's chain is
//!
//! ```text
//!   scene f32 --> tone_map --> [resample] --> lstar_encode --> dither --> file
//! ```
//!
//! and the brackets are the only place it may go. **After `lstar_encode`** averages
//! perceptual codes rather than light, and edges darken — the ordinary resize-in-gamma
//! mistake. **Before `tone_map`** is worse: scene values are unclamped by design, and a
//! windowed filter's undershoot is proportional to local range, so a kernel straddling
//! a blown highlight swings negative, clamps at zero, and leaves a black outline around
//! the light source.
//!
//! Post-tone-map is bounded to roughly [0, 1] *and* still linear in light, so averages
//! mean what they should and the ringing cannot explode.
//!
//! Ringing still puts a few samples slightly outside the input's range, and this
//! module **does not clamp**. The caller owns the range contract; export clamps at
//! the encoder, where it also has to clamp for every other reason.
//!
//! # The kernel is scaled on downsample, and that is the whole correctness story
//!
//! For each output sample `i`, the source coordinate is
//!
//! ```text
//!   centre = (i + 0.5) / scale - 0.5
//! ```
//!
//! **The half-pixel is not decoration.** Both grids are sampled at pixel *centres*;
//! dropping it shifts the whole picture half a pixel, which reads as softness rather
//! than as a bug and survives review. `a_symmetric_signal_stays_symmetric` catches it.
//!
//! When **upsampling**, the kernel keeps its native radius: there is a source sample
//! near every output sample and the filter interpolates between them.
//!
//! When **downsampling**, the kernel stretches by `1/scale` — twelve source samples
//! either side at 0.25x instead of three — so it integrates everything contributing to
//! the output sample. Without the stretch you point-sample a Lanczos and everything
//! above the new Nyquist folds back: moire on fabric and foliage.
//! `fine_detail_averages_out_instead_of_aliasing` is the test.
//!
//! Weights are normalised to sum to one per output sample, which is what makes a flat
//! field stay flat, and edges clamp to the border sample rather than wrapping.
//!
//! # Separable, in two passes
//!
//! Horizontally into a `dst_w x src_h` intermediate, then vertically. The weight table
//! for an axis depends only on that axis's two lengths, so it is built once and reused
//! for every row or column: O(w*h*k) instead of O(w*h*k^2). At 3x3 taps that is a
//! detail; at a 0.25x downsample, where the stretched kernel is 25 taps wide, it is the
//! difference between a second and half a minute.

use rayon::prelude::*;

/// The reconstruction filter.
///
/// Two, not five. They differ in one function and one radius, and the pair spans the
/// only choice a print actually poses: keep the acutance and accept the ringing, or
/// give up a little sharpness to be sure of a clean edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// `sinc(x) * sinc(x/3)`, the sharpest of the practical windowed sincs and what
    /// the prototype used (Pillow's `LANCZOS` *is* Lanczos-3), so an export's
    /// character does not change under anyone's feet.
    ///
    /// The default, and deliberately so: **output sharpening sits
    /// directly downstream**. A soft resample would leave 13 undoing this stage's work
    /// rather than doing its own.
    #[default]
    Lanczos3,
    /// Mitchell-Netravali with B = C = 1/3, the parameters the paper recommends. A
    /// cubic with a shallow negative lobe — measured on a step edge it overshoots
    /// about a third as far as Lanczos-3 — so a hard edge comes through without a
    /// visible halo. Slightly softer, and the right answer for a large upsample where
    /// the ringing would otherwise be the first thing you see.
    Mitchell,
}

impl Filter {
    pub const UI_ORDER: [Self; 2] = [Self::Lanczos3, Self::Mitchell];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lanczos3 => "Lanczos",
            Self::Mitchell => "Mitchell",
        }
    }

    /// The persisted spelling. Separate from `label` so renaming the UI string cannot
    /// silently orphan every sidecar that stored it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Lanczos3 => "lanczos3",
            Self::Mitchell => "mitchell",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|f| f.key() == s)
    }

    /// Half-width of the kernel's support, in source samples at 1:1.
    fn radius(self) -> f32 {
        match self {
            Self::Lanczos3 => 3.0,
            Self::Mitchell => 2.0,
        }
    }

    /// The kernel, evaluated at a distance measured in *unscaled* samples. The caller
    /// divides by the kernel scale before calling, so this stays the textbook
    /// definition and can be read against one.
    fn weight(self, x: f32) -> f32 {
        let x = x.abs();
        match self {
            Self::Lanczos3 => {
                if x >= 3.0 {
                    0.0
                } else {
                    sinc(x) * sinc(x / 3.0)
                }
            }
            // Written with B and C symbolic rather than folded to decimals, because
            // the folded coefficients are unreadable and this way it can be checked
            // line by line against Mitchell & Netravali 1988.
            Self::Mitchell => {
                const B: f32 = 1.0 / 3.0;
                const C: f32 = 1.0 / 3.0;
                if x < 1.0 {
                    ((12.0 - 9.0 * B - 6.0 * C) * x * x * x
                        + (-18.0 + 12.0 * B + 6.0 * C) * x * x
                        + (6.0 - 2.0 * B))
                        / 6.0
                } else if x < 2.0 {
                    ((-B - 6.0 * C) * x * x * x
                        + (6.0 * B + 30.0 * C) * x * x
                        + (-12.0 * B - 48.0 * C) * x
                        + (8.0 * B + 24.0 * C))
                        / 6.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// Normalised sinc, `sin(pi x) / (pi x)`, with the removable singularity filled in.
fn sinc(x: f32) -> f32 {
    if x == 0.0 {
        1.0
    } else {
        let px = std::f32::consts::PI * x;
        px.sin() / px
    }
}

/// One axis's weight table.
///
/// Flat and stride-major rather than a `Vec<Vec<f32>>`: the tap count is uniform (the
/// support is the same for every output sample, and the one-sample wobble from where
/// the centre falls is absorbed by trailing zero weights), so this is one allocation
/// read in order instead of a pointer chase per output sample.
struct Taps {
    stride: usize,
    /// First source index each output sample reads. Negative near the left edge; the
    /// apply step clamps.
    first: Vec<i32>,
    /// `stride` weights per output sample, each group summing to one.
    weights: Vec<f32>,
}

impl Taps {
    fn plan(src: u32, dst: u32, filter: Filter) -> Self {
        let scale = dst as f32 / src as f32;
        // Below 1:1 the kernel is stretched to cover everything that contributes; at
        // or above 1:1 it keeps its native width. See the module note — this one line
        // is the difference between a resample and an aliaser.
        let kernel_scale = (1.0 / scale).max(1.0);
        let support = filter.radius() * kernel_scale;
        // An upper bound on `floor(centre + support) - ceil(centre - support) + 1`,
        // rounded up so float fuzz can only ever cost a trailing zero weight.
        let stride = (2.0 * support).ceil() as usize + 1;

        let mut first = Vec::with_capacity(dst as usize);
        let mut weights = vec![0.0f32; dst as usize * stride];

        for i in 0..dst as usize {
            // Pixel centres on both grids. Dropping the halves shifts the picture.
            let centre = (i as f32 + 0.5) / scale - 0.5;
            let left = (centre - support).ceil() as i32;
            first.push(left);

            let row = &mut weights[i * stride..(i + 1) * stride];
            let mut sum = 0.0;
            for (t, w) in row.iter_mut().enumerate() {
                // Distance in unscaled kernel units, which is where `weight` is
                // defined. Samples past the support land on the kernel's zero tail,
                // so the stride's slack costs nothing and needs no special case.
                let d = (left + t as i32) as f32 - centre;
                *w = filter.weight(d / kernel_scale);
                sum += *w;
            }
            // Normalising per output sample is what keeps a flat field flat. It also
            // silently absorbs the 1/kernel_scale the stretched kernel would otherwise
            // need, and the clamped edge taps, which would each need their own line if
            // the weights were used raw.
            if sum != 0.0 {
                let inv = 1.0 / sum;
                for w in row.iter_mut() {
                    *w *= inv;
                }
            }
        }

        Self {
            stride,
            first,
            weights,
        }
    }

    /// `src` is one line of `len` samples with the given stride between them; returns
    /// the filtered line. Clamp-to-edge at both ends.
    #[inline]
    fn apply_line(&self, src: &[f32], len: u32, step: usize, out: &mut [f32]) {
        let last = len as i32 - 1;
        for (i, o) in out.iter_mut().enumerate() {
            let first = self.first[i];
            let w = &self.weights[i * self.stride..(i + 1) * self.stride];
            let mut acc = 0.0;
            for (t, &wt) in w.iter().enumerate() {
                let s = (first + t as i32).clamp(0, last) as usize;
                acc += wt * src[s * step];
            }
            *o = acc;
        }
    }
}

/// Resample `src` (`sw` x `sh`, row-major) to `dw` x `dh`.
///
/// Values are taken to be linear in light and bounded — see the module note. The
/// result may ring a little outside the input's range and is **not** clamped.
///
/// Equal dimensions return the input unchanged rather than running the filter. At 1:1
/// the kernel evaluates to a unit impulse and the two agree to within float error, but
/// "the file is byte-identical when you have not asked for a resize" is a property
/// worth having exactly rather than nearly, since it is what makes the print-size
/// controls safe to fiddle with.
pub fn resample(src: &[f32], sw: u32, sh: u32, dw: u32, dh: u32, filter: Filter) -> Vec<f32> {
    assert_eq!(
        src.len(),
        sw as usize * sh as usize,
        "resample: source length does not match dims"
    );
    assert!(
        sw > 0 && sh > 0 && dw > 0 && dh > 0,
        "resample: zero dimension"
    );

    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }

    // Horizontal, into a dw x sh intermediate. Rows are independent.
    let mut mid = vec![0.0f32; dw as usize * sh as usize];
    if dw == sw {
        mid.copy_from_slice(src);
    } else {
        let taps = Taps::plan(sw, dw, filter);
        mid.par_chunks_mut(dw as usize)
            .zip(src.par_chunks(sw as usize))
            .for_each(|(out, row)| taps.apply_line(row, sw, 1, out));
    }

    if dh == sh {
        return mid;
    }

    // Vertical. Each output row reads a window of intermediate rows, so the walk is
    // strided by `dw` and the parallel split is over output rows.
    let taps = Taps::plan(sh, dh, filter);
    let mut out = vec![0.0f32; dw as usize * dh as usize];
    out.par_chunks_mut(dw as usize)
        .enumerate()
        .for_each(|(y, row)| {
            let first = taps.first[y];
            let w = &taps.weights[y * taps.stride..(y + 1) * taps.stride];
            let last = sh as i32 - 1;
            for (x, o) in row.iter_mut().enumerate() {
                let mut acc = 0.0;
                for (t, &wt) in w.iter().enumerate() {
                    let s = (first + t as i32).clamp(0, last) as usize;
                    acc += wt * mid[s * dw as usize + x];
                }
                *o = acc;
            }
        });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both filters, so a property that holds for one is not quietly assumed of the
    /// other.
    const BOTH: [Filter; 2] = [Filter::Lanczos3, Filter::Mitchell];

    #[test]
    fn a_key_round_trips_and_is_not_the_label() {
        for f in BOTH {
            assert_eq!(Filter::from_key(f.key()), Some(f));
        }
        assert_eq!(Filter::from_key("bicubic"), None);
    }

    #[test]
    fn identity_is_exact() {
        let src: Vec<f32> = (0..48).map(|i| i as f32 * 0.01).collect();
        for f in BOTH {
            assert_eq!(resample(&src, 8, 6, 8, 6, f), src);
        }
    }

    #[test]
    fn the_kernels_partition_unity() {
        // Sampled at every phase, a normalised kernel's taps must sum to one — the
        // property `a_flat_field_stays_flat` depends on, checked directly so a failure
        // says which of the two it is.
        for f in BOTH {
            for &(src, dst) in &[(10u32, 30u32), (30, 10), (7, 11), (100, 13)] {
                let t = Taps::plan(src, dst, f);
                for i in 0..dst as usize {
                    let sum: f32 = t.weights[i * t.stride..(i + 1) * t.stride].iter().sum();
                    assert!(
                        (sum - 1.0).abs() < 1e-5,
                        "{f:?} {src}->{dst} tap {i} sums to {sum}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_flat_field_stays_flat() {
        // Catches an unnormalised kernel and a mishandled edge in one: without
        // clamp-to-edge the border samples read short and darken.
        let src = vec![0.5f32; 40 * 30];
        for f in BOTH {
            for &(dw, dh) in &[(97u32, 71u32), (13, 9), (40, 7), (11, 30)] {
                let out = resample(&src, 40, 30, dw, dh, f);
                assert_eq!(out.len(), dw as usize * dh as usize);
                for (i, v) in out.iter().enumerate() {
                    assert!((v - 0.5).abs() < 1e-4, "{f:?} {dw}x{dh}: sample {i} is {v}");
                }
            }
        }
    }

    #[test]
    fn a_symmetric_signal_stays_symmetric() {
        // The half-pixel test. A signal symmetric about the centre of its row must
        // resample to one symmetric about the centre of the output row; a missing
        // `- 0.5` biases every centre by half a source sample and the result tilts.
        //
        // Seen to fail before it was trusted: with `- 0.5` removed from `Taps::plan`,
        // the 2x upsample of this row is asymmetric by ~0.09 — far outside the
        // tolerance below.
        let src: Vec<f32> = vec![0.0, 0.1, 0.4, 0.9, 0.9, 0.4, 0.1, 0.0];
        for f in BOTH {
            for dw in [16u32, 24, 5, 3] {
                let out = resample(&src, 8, 1, dw, 1, f);
                for i in 0..out.len() / 2 {
                    let (a, b) = (out[i], out[out.len() - 1 - i]);
                    assert!((a - b).abs() < 1e-5, "{f:?} ->{dw}: {i} {a} vs mirror {b}");
                }
            }
        }
    }

    #[test]
    fn the_two_axes_agree() {
        // Resampling the transpose must transpose the result. A pass that indexed its
        // stride wrong would still look plausible on a square test image.
        let (sw, sh) = (9u32, 5u32);
        let src: Vec<f32> = (0..sw * sh)
            .map(|i| ((i * 37) % 13) as f32 / 13.0)
            .collect();
        let mut t = vec![0.0f32; src.len()];
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                t[x * sh as usize + y] = src[y * sw as usize + x];
            }
        }
        let (dw, dh) = (14u32, 8u32);
        for f in BOTH {
            let a = resample(&src, sw, sh, dw, dh, f);
            let b = resample(&t, sh, sw, dh, dw, f);
            for y in 0..dh as usize {
                for x in 0..dw as usize {
                    let (p, q) = (a[y * dw as usize + x], b[x * dh as usize + y]);
                    assert!((p - q).abs() < 1e-5, "{f:?} at {x},{y}: {p} vs {q}");
                }
            }
        }
    }

    #[test]
    fn fine_detail_averages_out_instead_of_aliasing() {
        // The kernel-scaling test, and it took two attempts to get an honest one.
        //
        // The obvious pattern — a one-pixel checkerboard downsampled 8x — does NOT
        // discriminate: it passes with `kernel_scale` forced to 1.0. At that exact
        // phase the unscaled kernel straddles equal numbers of black and white
        // samples, so it lands on the mean for the wrong reason. A test that passes
        // against the bug it was written for is worse than no test.
        //
        // What aliasing actually is, is a pattern *beating* against the sample grid.
        // Period-5 stripes downsampled 8x have a true mean of 0.4 everywhere, and the
        // 8-sample stride sees a different phase of the period every time. A kernel
        // stretched to 1/scale integrates whole periods and returns 0.4; an unscaled
        // one reads six of every eight samples and swings with the phase.
        //
        // The border column is excluded, and legitimately: a kernel stretched to 24
        // samples reaches well past the edge there and clamp-to-edge replicates
        // whichever phase of the stripe happens to sit on the border, which lands the
        // edge samples at 0.48 and 0.33. That is the edge rule behaving correctly, not
        // aliasing, and including it would have the test measuring the wrong thing.
        //
        // Seen to fail before it was trusted: with `kernel_scale` forced to 1.0 the
        // interior spread is 0.29 for Lanczos-3, against 0.02 with the stretch.
        let (sw, sh) = (80u32, 80u32);
        let src: Vec<f32> = (0..sw * sh)
            .map(|i| if i % sw % 5 < 2 { 1.0 } else { 0.0 })
            .collect();
        for f in BOTH {
            let out = resample(&src, sw, sh, 10, 10, f);
            let interior = (1..9)
                .flat_map(|y| (1..9).map(move |x| y * 10 + x))
                .map(|i| out[i]);
            let (lo, hi) = interior.fold((f32::MAX, f32::MIN), |(l, h), v| (l.min(v), h.max(v)));
            assert!(hi - lo < 0.05, "{f:?}: aliased, spread {lo}..{hi}");
            assert!((lo - 0.4).abs() < 0.05, "{f:?}: mean drifted to {lo}");
        }
    }

    #[test]
    fn mitchell_rings_far_less_than_lanczos() {
        // Both overshoot a step edge — Mitchell's negative lobe is small, not absent —
        // so the claim under test is the ratio, which is what the choice between them
        // is actually about.
        let src: Vec<f32> = (0..32).map(|i| if i < 16 { 0.0 } else { 1.0 }).collect();
        let overshoot = |f: Filter| {
            resample(&src, 32, 1, 128, 1, f)
                .iter()
                .fold(0.0f32, |m, &v| m.max((-v).max(v - 1.0)))
        };
        let (l, m) = (overshoot(Filter::Lanczos3), overshoot(Filter::Mitchell));
        assert!(l > 0.05, "Lanczos-3 should ring on a step, got {l}");
        // Measured: 0.118 against 0.035, so a third. The bound is 0.4 to leave room
        // for float ordering, not because a third is approximate.
        assert!(m < l * 0.4, "Mitchell {m} should be far below Lanczos {l}");
    }

    #[test]
    fn an_upsample_interpolates_rather_than_replicating() {
        // A gradient upsampled must produce intermediate values, not stair-steps: the
        // check that this is a filter and not a nearest-neighbour zoom.
        let src: Vec<f32> = (0..8).map(|i| i as f32 / 7.0).collect();
        for f in BOTH {
            let out = resample(&src, 8, 1, 64, 1, f);
            assert!(
                out.windows(2).all(|w| w[1] >= w[0] - 1e-6),
                "{f:?}: not monotonic"
            );
            let distinct = out
                .iter()
                .map(|v| (v * 1000.0) as i32)
                .collect::<std::collections::HashSet<_>>();
            assert!(
                distinct.len() > 40,
                "{f:?}: only {} distinct values",
                distinct.len()
            );
        }
    }

    #[test]
    fn one_axis_alone_leaves_the_other_untouched() {
        // The `dw == sw` and `dh == sh` short circuits: a width-only resize must not
        // perturb a column, which is also what makes them safe to take.
        let (sw, sh) = (10u32, 6u32);
        let src: Vec<f32> = (0..sw * sh).map(|i| (i % 7) as f32 / 7.0).collect();
        for f in BOTH {
            let wide = resample(&src, sw, sh, 25, sh, f);
            for y in 0..sh as usize {
                let row: Vec<f32> = src[y * sw as usize..(y + 1) * sw as usize].to_vec();
                let expect = resample(&row, sw, 1, 25, 1, f);
                for x in 0..25 {
                    assert!(
                        (wide[y * 25 + x] - expect[x]).abs() < 1e-6,
                        "{f:?} at {x},{y}"
                    );
                }
            }
        }
    }
}
