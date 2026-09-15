//! The zone mask's spatial machinery: the proxy the mask is computed on, the
//! edge-aware filter that makes a zone a *region*, and the trapezoid's evaluation.
//!
//! [`dodgeburn`](crate::dodgeburn) holds the model — what a mask *is*. This holds
//! what it takes to turn one into pixels.
//!
//! # Why this is on the CPU, and small
//!
//! Everything else in the render path is a GPU node, and a guided filter could be
//! one: it is five local means and some arithmetic. It is not one, for a reason
//! that is a property of the mask rather than an expedience.
//!
//! **A zone mask has no detail to lose.** Its whole purpose is to select a
//! spatially coherent region, and its detail ceiling is the guidance window —
//! `region`, four percent of the frame width by default. Computed at
//! [`PROXY_WIDTH`] that window is nineteen pixels across; computed at 6000px it is
//! two hundred and forty, describing the same shape with a hundred times the
//! arithmetic and no more information in it. So the proxy is not a shortcut taken
//! for speed, it is the resolution the signal actually has, and the prototype
//! reached the same conclusion from the other direction — it raised its stash from
//! 240px to 480 precisely because Region did not have enough guidance detail at
//! the lower figure, and stopped there.
//!
//! What that buys: the whole mask chain is a few hundred microseconds on 150k
//! pixels, the graph gains one pointwise node instead of a dozen wide-apron blur
//! passes, and a brush stroke — which cannot change a mask, because a mask is
//! feed-forward — never touches any of it.
//!
//! # The proxy is in SOURCE space
//!
//! Not the composed frame. The mask is computed on the stored luminance image with
//! no orientation, straighten or crop applied, which makes it invariant to every
//! one of them — the same argument `raw_gpu::Viewport::plan` records for Contrast
//! Mask's spacer, and for the same reason: *a mask whose look changed when you
//! cropped would be a surprise nobody asked for*. It is also the space
//! [`Dab`](crate::dodgeburn::Dab) coordinates live in, so the shader needs one
//! mapping rather than two.

use crate::dodgeburn::{MID_GREY, ZoneMask};
use crate::params::{ContrastMaskParams, ExposureParams};
use crate::scene::LumaImage;

/// Width of the proxy the masks are computed on.
///
/// The prototype's figure, and its history is the argument for it: 240px was not
/// enough guidance detail for the Region control to be usable, 480px was, and
/// nothing above it changed the result. See the module note.
pub const PROXY_WIDTH: usize = 480;

/// The pre-D&B luminance, as EV against middle grey, at proxy resolution.
///
/// **Feed-forward.** This is the signal *entering* the D&B stage — exposure and
/// Contrast Mask applied, no strokes — so a burn can never move the mask that is
/// placing it. Rebuilt when luminance, exposure or Contrast Mask changes; a stroke
/// does not change any of those, which is what keeps a brush drag off this path
/// entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Basis {
    pub w: usize,
    pub h: usize,
    /// `log2(luminance / 0.18)`, one per proxy pixel.
    pub ev: Vec<f32>,
}

/// The downsampled luminance the masks are computed from, before any module has
/// been applied to it.
///
/// **The caching boundary, made explicit.** Building this is the only part of the
/// mask chain that touches the full-resolution image, and it depends on nothing
/// but the luminance itself — so it survives every exposure nudge, every Contrast
/// Mask slider, every zone bound and every brush stroke, and is rebuilt only when
/// luminance is re-derived. Splitting it out of [`Basis`] is what stops a
/// three-hundred-megapixel downsample from riding on a slider drag.
#[derive(Debug, Clone, PartialEq)]
pub struct Proxy {
    pub w: usize,
    pub h: usize,
    /// Scene-linear luminance, area-averaged down from the stored image.
    lum: Vec<f32>,
    /// The dimensions this was reduced from. Contrast Mask's spacer is a
    /// percentage of *that* diagonal, so the number has to travel with the data.
    src: (usize, usize),
}

impl Proxy {
    pub fn of(luma: &LumaImage) -> Self {
        let (sw, sh) = (luma.output_dims.w, luma.output_dims.h);
        let (w, h) = proxy_dims(sw, sh);
        Self {
            w,
            h,
            lum: downsample(&luma.data, sw, sh, w, h),
            src: (sw, sh),
        }
    }

    /// Apply the two modules upstream of D&B and take the log. Cheap: two passes
    /// over 150k floats, plus one small Gaussian when the mask is on.
    ///
    /// The Contrast Mask arithmetic here is the shader's, at proxy scale: log,
    /// blur, subtract a fraction of the blur, exponentiate. Its **registration
    /// offset is deliberately not applied** — at proxy scale the whole ±40px range
    /// is under two pixels, and a mask that selects regions cannot resolve a
    /// displacement smaller than its own guidance window. Stated rather than
    /// silently dropped, because the omission is a judgement and not an oversight.
    pub fn basis(&self, exposure: &ExposureParams, cm: &ContrastMaskParams) -> Basis {
        let (w, h) = (self.w, self.h);
        let mut v = self.lum.clone();

        // Exposure. Black first, so raising exposure does not amplify the offset —
        // the same order `ExposureParams` documents and `exposure.wgsl` runs.
        let gain = exposure.ev.exp2();
        for p in &mut v {
            *p = (*p - exposure.black) * gain;
        }

        if cm.is_active() {
            // Sigma against the SOURCE dims, then scaled to the proxy — exactly
            // how `node_params` converts it for the shader, so the two cannot mean
            // different physical spacers.
            let sigma = cm.spacer_px((self.src.0 as u32, self.src.1 as u32))
                * (w as f32 / self.src.0 as f32);
            let log: Vec<f32> = v.iter().map(|p| p.max(1e-9).log2()).collect();
            let blurred = gaussian(&log, w, h, sigma);
            for (p, (l, b)) in v.iter_mut().zip(log.iter().zip(&blurred)) {
                *p = (l - b * cm.contrast).exp2();
            }
        }

        for p in &mut v {
            *p = (p.max(1e-6) / MID_GREY).log2();
        }
        Basis { w, h, ev: v }
    }
}

impl Basis {
    /// Proxy and basis in one call. The convenience form; the render path holds a
    /// [`Proxy`] across frames and calls [`Proxy::basis`] instead.
    pub fn build(luma: &LumaImage, exposure: &ExposureParams, cm: &ContrastMaskParams) -> Self {
        Proxy::of(luma).basis(exposure, cm)
    }

    /// The EV image a mask of these settings reads: regional when it is edge-aware,
    /// raw per-pixel when it is not.
    ///
    /// This one call is the entire difference between "Zone III" meaning *the
    /// shadowed wall* and meaning *every dark pixel in the frame*.
    fn read(&self, m: &ZoneMask) -> std::borrow::Cow<'_, [f32]> {
        if !m.edge_aware {
            return std::borrow::Cow::Borrowed(&self.ev);
        }
        // Radius as a fraction of width, so one setting means one thing whatever
        // the negative's resolution — and, because the proxy is a fixed width, one
        // thing across every file.
        let r = ((m.region * self.w as f32).round() as usize).max(1);
        std::borrow::Cow::Owned(guided(&self.ev, self.w, self.h, r, m.eps()))
    }

    /// The mask itself, at proxy resolution, in `[0, 1]`.
    ///
    /// `None` when the mask is the identity — there is no such thing as a mask of
    /// all ones worth allocating, and the caller has to distinguish the two cases
    /// anyway to skip binding a texture.
    pub fn evaluate(&self, m: &ZoneMask) -> Option<Vec<f32>> {
        if m.is_identity() {
            return None;
        }
        let ev = self.read(m);
        let mut out: Vec<f32> = ev.iter().map(|&e| m.evaluate(e)).collect();
        // The diffusion spacer. Masks were printed unsharp on purpose; without
        // this the mask carries whatever granularity the trapezoid's edges cut
        // into the proxy.
        let sigma = m.blur * self.w as f32;
        if sigma >= 0.5 {
            out = gaussian(&out, self.w, self.h, sigma);
        }
        Some(out)
    }

    /// The EV distribution, normalised so the tallest bin is 1.0, for the ghost
    /// behind the zone ruler.
    ///
    /// Binned over the ruler's own window rather than the data's range: the
    /// histogram is drawn *behind the ruler* and has to line up with it, so a bin
    /// is a position on the ruler and values outside it are simply not shown.
    ///
    /// **Smoothed before it is normalised**, which is a display decision made here
    /// rather than in the widget because the smoothing has to happen before the peak
    /// is taken — smoothing afterwards lowers the tallest bin and the curve stops
    /// touching the top of its box.
    ///
    /// the maintainer's report was that the curve read "very jagged on top". The cause is not
    /// noise: a 480px proxy puts about fifteen hundred samples in each of a hundred
    /// bins, so the sampling error is under three percent. It is that a photograph's
    /// tonal distribution genuinely *is* spiky at this bin width, and that a spike
    /// one bin wide is a hard-edged 4px tooth once it is drawn. A three-bin
    /// triangular kernel takes the teeth off without moving anything a person would
    /// want to read off it — the mode of the distribution shifts by less than a
    /// twentieth of a stop.
    pub fn histogram(&self, bins: usize) -> Vec<f32> {
        let bins = bins.max(1);
        let mut counts = vec![0f32; bins];
        let span = ZoneMask::MAX_EV - ZoneMask::MIN_EV;
        for &e in &self.ev {
            let t = (e - ZoneMask::MIN_EV) / span;
            if (0.0..1.0).contains(&t) {
                counts[((t * bins as f32) as usize).min(bins - 1)] += 1.0;
            }
        }
        // Clamped at the ends rather than wrapped or zero-padded: the ruler's window
        // is a view onto a wider distribution, and treating what is outside it as
        // empty would pull the first and last bins down for a reason that is about
        // the window rather than about the picture.
        let at = |i: isize| counts[i.clamp(0, bins as isize - 1) as usize];
        let smooth: Vec<f32> = (0..bins)
            .map(|i| (at(i as isize - 1) + 2.0 * counts[i] + at(i as isize + 1)) / 4.0)
            .collect();
        let peak = smooth.iter().copied().fold(0.0f32, f32::max).max(1.0);
        smooth.iter().map(|c| c / peak).collect()
    }
}

/// Proxy dimensions for a source of `(w, h)`: [`PROXY_WIDTH`] on the long edge,
/// never upscaling.
///
/// The **long** edge, so a portrait frame gets the same number of proxy pixels as
/// the landscape one it was cropped from — a mask whose resolution depended on
/// which way up the negative was would be the same defect the Contrast Mask spacer
/// avoids by measuring the diagonal.
fn proxy_dims(w: usize, h: usize) -> (usize, usize) {
    let long = w.max(h);
    if long <= PROXY_WIDTH {
        return (w.max(1), h.max(1));
    }
    let s = PROXY_WIDTH as f32 / long as f32;
    (
        ((w as f32 * s).round() as usize).max(1),
        ((h as f32 * s).round() as usize).max(1),
    )
}

/// Area-average `src` down to `dw` x `dh`.
///
/// An area average and not a point sample, because the proxy is a *statistic* of
/// the frame — the histogram behind the ruler is drawn from it — and point
/// sampling a 6000px frame at 480 would report the tones of one pixel in a
/// hundred and fifty.
fn downsample(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; dw * dh];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = (((y + 1) * sh).div_ceil(dh)).min(sh).max(y0 + 1);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = (((x + 1) * sw).div_ceil(dw)).min(sw).max(x0 + 1);
            let mut sum = 0.0f64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    sum += src[sy * sw + sx] as f64;
                }
            }
            out[y * dw + x] = (sum / ((y1 - y0) * (x1 - x0)) as f64) as f32;
        }
    }
    out
}

/// Mean over a `(2r+1)²` window, via an integral image.
///
/// **Exact at the borders**: the window is clipped to the frame and divided by the
/// number of pixels actually in it, rather than by the nominal window area. The
/// alternative — treating the outside as zero — darkens every edge of the mask by
/// an amount that depends on the radius, which on a `region` sweep looks exactly
/// like the filter breaking down at high settings.
///
/// `O(N)` whatever the radius, which is why the prototype's fast-guided-filter
/// subsampling is **not** carried over: it exists to make a large radius cheap,
/// and with an integral image a large radius already is. One less resampling stage
/// to get wrong.
fn box_mean(a: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    // f64 for the running sums. At 480x320 an f32 accumulator loses low bits by
    // the far corner, and the difference of two large sums is exactly where that
    // shows up.
    let mut ii = vec![0.0f64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0.0f64;
        for x in 0..w {
            row += a[y * w + x] as f64;
            ii[(y + 1) * (w + 1) + x + 1] = ii[y * (w + 1) + x + 1] + row;
        }
    }
    let at = |x: usize, y: usize| ii[y * (w + 1) + x];

    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r + 1).min(h);
        for x in 0..w {
            let x0 = x.saturating_sub(r);
            let x1 = (x + r + 1).min(w);
            let s = at(x1, y1) - at(x0, y1) - at(x1, y0) + at(x0, y0);
            out[y * w + x] = (s / ((y1 - y0) * (x1 - x0)) as f64) as f32;
        }
    }
    out
}

/// Self-guided filter (He et al. 2010), greyscale — edge-aware smoothing.
///
/// ```text
///   q = mean(a)·I + mean(b),   a = var / (var + eps),   b = (1 − a)·mean(I)
/// ```
///
/// Inside a smooth region `var ≪ eps`, so `a ≈ 0` and `q` is the local mean:
/// strong smoothing. Across an edge `var ≫ eps`, so `a ≈ 1` and `q ≈ I`: the edge
/// survives untouched. `eps` is in the **squared units of the input**, which for an
/// EV image makes `sqrt(eps)` the soft edge threshold in stops — see
/// [`ZoneMask::edge`], which stores that root rather than the square.
fn guided(img: &[f32], w: usize, h: usize, r: usize, eps: f32) -> Vec<f32> {
    let mean_i = box_mean(img, w, h, r);
    let sq: Vec<f32> = img.iter().map(|v| v * v).collect();
    let mean_ii = box_mean(&sq, w, h, r);

    let eps = eps.max(1e-6);
    let mut a = vec![0.0f32; w * h];
    let mut b = vec![0.0f32; w * h];
    for i in 0..w * h {
        let var = (mean_ii[i] - mean_i[i] * mean_i[i]).max(0.0);
        a[i] = var / (var + eps);
        b[i] = (1.0 - a[i]) * mean_i[i];
    }
    let ma = box_mean(&a, w, h, r);
    let mb = box_mean(&b, w, h, r);
    (0..w * h).map(|i| ma[i] * img[i] + mb[i]).collect()
}

/// Separable Gaussian with clamped edges, truncated at three sigma.
///
/// Three sigma because beyond it a Gaussian contributes under 0.3% — the same
/// figure `raw_graph::build` uses to size the Contrast Mask apron, kept the same
/// on purpose so the proxy's mask and the shader's agree about how far a spacer
/// reaches.
fn gaussian(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let radius = (3.0 * sigma).ceil().max(1.0) as usize;
    let kernel: Vec<f32> = {
        let mut k: Vec<f32> = (0..=2 * radius)
            .map(|i| {
                let d = i as f32 - radius as f32;
                (-0.5 * d * d / (sigma * sigma)).exp()
            })
            .collect();
        let sum: f32 = k.iter().sum();
        for v in &mut k {
            *v /= sum;
        }
        k
    };

    let mut tmp = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in kernel.iter().enumerate() {
                let sx = (x as isize + i as isize - radius as isize).clamp(0, w as isize - 1);
                acc += src[y * w + sx as usize] * kv;
            }
            tmp[y * w + x] = acc;
        }
    }
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in kernel.iter().enumerate() {
                let sy = (y as isize + i as isize - radius as isize).clamp(0, h as isize - 1);
                acc += tmp[sy as usize * w + x] * kv;
            }
            out[y * w + x] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Dims;

    fn luma(w: usize, h: usize, data: Vec<f32>) -> LumaImage {
        LumaImage {
            data,
            output_dims: Dims { w, h },
            source_dims: Dims { w, h },
            clipped: Vec::new(),
        }
    }

    #[test]
    fn a_box_mean_of_a_constant_is_that_constant_at_the_border_too() {
        // The border rule, as an assertion. Treating the outside as zero would make
        // the corners read 0.25 of the true mean at a large radius — which looks
        // like the guided filter failing at high Region settings rather than like
        // an averaging bug, and is why this is pinned separately.
        let a = vec![0.7f32; 40 * 30];
        for r in [1, 5, 25] {
            let m = box_mean(&a, 40, 30, r);
            for (i, v) in m.iter().enumerate() {
                assert!((v - 0.7).abs() < 1e-5, "r={r} at {i}: {v}");
            }
        }
    }

    #[test]
    fn a_box_mean_averages_what_is_actually_in_the_window() {
        // 4x1 ramp, radius 1: each output is the mean of at most three neighbours,
        // clipped at the ends. Small enough to check by hand, which is the point.
        let a = [0.0f32, 1.0, 2.0, 3.0];
        let m = box_mean(&a, 4, 1, 1);
        assert!(
            (m[0] - 0.5).abs() < 1e-6,
            "two pixels at the left edge: {}",
            m[0]
        );
        assert!((m[1] - 1.0).abs() < 1e-6);
        assert!((m[2] - 2.0).abs() < 1e-6);
        assert!(
            (m[3] - 2.5).abs() < 1e-6,
            "two pixels at the right edge: {}",
            m[3]
        );
    }

    #[test]
    fn the_guided_filter_keeps_an_edge_and_flattens_a_ripple() {
        // The two behaviours the mask depends on, in one image: a 4 EV step down
        // the middle, with a ±0.1 EV ripple everywhere. eps is (0.5 EV)², so the
        // step is well above the threshold and the ripple well below.
        let (w, h) = (64, 32);
        let mut img = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let step = if x < w / 2 { -2.0 } else { 2.0 };
                let ripple = if (x + y) % 2 == 0 { 0.1 } else { -0.1 };
                img[y * w + x] = step + ripple;
            }
        }
        let q = guided(&img, w, h, 6, 0.25);

        // The ripple is gone well away from the edge.
        let flat: Vec<f32> = (4..12).map(|x| q[16 * w + x]).collect();
        let spread = flat.iter().cloned().fold(f32::MIN, f32::max)
            - flat.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.05, "ripple should be smoothed, spread {spread}");
        assert!(
            (flat[0] + 2.0).abs() < 0.05,
            "and settle on the region's own level"
        );

        // The step survives: still nearly its full height across the boundary.
        let jump = q[16 * w + w / 2 + 2] - q[16 * w + w / 2 - 3];
        assert!(jump > 3.0, "a 4 EV edge must survive, got {jump}");
    }

    #[test]
    fn a_guided_filter_of_a_constant_is_that_constant() {
        let a = vec![-1.25f32; 32 * 32];
        for v in guided(&a, 32, 32, 4, 0.25) {
            assert!((v + 1.25).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn the_basis_reads_middle_grey_as_zero_ev() {
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        for e in &b.ev {
            assert!(e.abs() < 1e-5, "0.18 must be Zone V: {e}");
        }
    }

    #[test]
    fn exposure_moves_the_basis_by_exactly_that_many_stops() {
        // The property the zone chips depend on: push exposure up a stop and what
        // was Zone V is Zone VI. If this drifts, every preset lands somewhere else.
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let up = ExposureParams {
            ev: 1.5,
            ..Default::default()
        };
        let b = Basis::build(&l, &up, &ContrastMaskParams::default());
        for e in &b.ev {
            assert!((e - 1.5).abs() < 1e-5, "{e}");
        }
    }

    #[test]
    fn the_proxy_survives_every_slider_that_is_not_luminance() {
        // The caching boundary, as an assertion. The expensive half of the mask
        // chain must depend on the luminance image alone — if a `Proxy` ever grew
        // a field that a module could move, a slider drag would start
        // downsampling a hundred megapixels sixty times a second.
        let l = luma(
            64,
            48,
            (0..64 * 48).map(|i| (i % 17) as f32 / 17.0).collect(),
        );
        let a = Proxy::of(&l);
        let b = Proxy::of(&l);
        assert_eq!(a, b);
        // Two very different bases off one proxy, which is the point of the split.
        let flat = a.basis(&ExposureParams::default(), &ContrastMaskParams::default());
        let lifted = a.basis(
            &ExposureParams {
                ev: 2.0,
                ..Default::default()
            },
            &ContrastMaskParams::default(),
        );
        assert_ne!(flat.ev, lifted.ev);
        assert_eq!(
            a, b,
            "and building a basis must not consume or mutate the proxy"
        );
    }

    #[test]
    fn the_proxy_is_the_long_edge_and_never_upscales() {
        assert_eq!(proxy_dims(6000, 4000), (480, 320));
        assert_eq!(
            proxy_dims(4000, 6000),
            (320, 480),
            "portrait gets the same pixel budget"
        );
        assert_eq!(
            proxy_dims(120, 80),
            (120, 80),
            "a small frame is left alone"
        );
    }

    #[test]
    fn downsampling_averages_rather_than_samples() {
        // A one-pixel checkerboard reduced 4x must read as the mean, not as
        // whichever phase the sample landed on. The histogram behind the ruler is
        // drawn from this, so point sampling would misreport the tonal
        // distribution of every finely textured frame.
        let (w, h) = (16, 16);
        let src: Vec<f32> = (0..w * h)
            .map(|i| if (i / w + i % w) % 2 == 0 { 0.0 } else { 1.0 })
            .collect();
        for v in downsample(&src, w, h, 4, 4) {
            assert!((v - 0.5).abs() < 1e-6, "{v}");
        }
    }

    #[test]
    fn an_identity_mask_evaluates_to_nothing_at_all() {
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        assert!(b.evaluate(&ZoneMask::default()).is_none());
        let open = ZoneMask {
            enabled: true,
            ..Default::default()
        };
        assert!(
            b.evaluate(&open).is_none(),
            "on but open at both ends is still nothing"
        );
    }

    #[test]
    fn a_shadow_mask_selects_the_shadows_and_not_the_highlights() {
        // Half the frame four stops down, half two stops up. The Shadows preset
        // must come out near 1 on the dark half and near 0 on the bright one.
        let (w, h) = (64, 32);
        let data: Vec<f32> = (0..w * h)
            .map(|i| {
                if i % w < w / 2 {
                    MID_GREY / 16.0
                } else {
                    MID_GREY * 4.0
                }
            })
            .collect();
        let l = luma(w, h, data);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );

        let (_, lo, hi, f_lo, f_hi) = ZoneMask::PRESETS[0];
        let m = ZoneMask {
            enabled: true,
            lo,
            hi,
            f_lo,
            f_hi,
            ..Default::default()
        };
        let mask = b.evaluate(&m).expect("a bounded mask is not the identity");
        assert!(
            mask[16 * b.w + 4] > 0.95,
            "dark half: {}",
            mask[16 * b.w + 4]
        );
        assert!(
            mask[16 * b.w + b.w - 4] < 0.05,
            "bright half: {}",
            mask[16 * b.w + b.w - 4]
        );
    }

    #[test]
    fn the_histogram_is_binned_over_the_ruler_not_the_data() {
        // The ghost is drawn behind the ruler and has to line up with it. Binning
        // over the data's own range would slide the distribution around under the
        // trapezoid every time the exposure moved, which is precisely the reading
        // it exists to give.
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        let hist = b.histogram(10);
        assert_eq!(hist.len(), 10);
        assert_eq!(
            hist[5], 1.0,
            "0 EV falls in the middle bin of a -5..+5 ruler"
        );
        // The smoothing kernel spreads a spike into its two neighbours and no
        // further, so a flat 0.18 image lights bins 4, 5 and 6 and nothing else.
        // Asserted as a *shape* rather than as "everything else is zero", which is
        // what this test claimed before there was any smoothing.
        assert!(
            hist[4] > 0.0 && hist[4] < hist[5],
            "shoulder below: {:?}",
            &hist[3..8]
        );
        assert_eq!(hist[4], hist[6], "and symmetric");
        assert!(
            hist.iter()
                .enumerate()
                .all(|(i, &v)| (4..=6).contains(&i) || v == 0.0)
        );
    }

    #[test]
    fn the_contrast_mask_reaches_the_basis() {
        // Not a numerical claim — just that the module upstream is actually in the
        // signal the zones are read from. It is switched off by default, so a wire
        // that was never connected would look identical in every other test here.
        let (w, h) = (64, 64);
        let data: Vec<f32> = (0..w * h)
            .map(|i| {
                if i % w < w / 2 {
                    MID_GREY / 8.0
                } else {
                    MID_GREY * 8.0
                }
            })
            .collect();
        let l = luma(w, h, data);
        let off = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        let on = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams {
                enabled: true,
                ..Default::default()
            },
        );
        assert_ne!(off.ev, on.ev);
        // And it compresses: the range between the two halves narrows.
        let range = |b: &Basis| b.ev[32 * b.w + b.w - 4] - b.ev[32 * b.w + 4];
        assert!(
            range(&on) < range(&off),
            "{} vs {}",
            range(&on),
            range(&off)
        );
    }
}
