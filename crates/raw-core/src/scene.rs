//! Stage 2: Scene (the cacheable boundary) and Stage 3: Working.

use rayon::prelude::*;

use crate::geometry::{CfaColor, CfaGeometry, Dims};
use crate::sensor::{Gains, SensorImage};

/// f32, black-subtracted, gain-equalised, normalised, and STILL MOSAICED.
///
/// Range is `[~-epsilon, gains.headroom()]` — measured up to 3.52 on the Leica
/// M10-R. Two things are deliberately *not* clamped here:
///
/// - **Above 1.0**, because gain equalisation gives the less-sensitive sensor colours
///   real headroom beyond the first colour's saturation point.
/// - **Below 0.0**, because photosites read below the black level as sensor noise,
///   and rectifying that noise to zero biases the shadow floor upward. It matters
///   specifically for SuperPixel: averaging four unrectified samples gives an
///   unbiased mean, averaging four rectified ones does not. The exposure module's
///   black correction is the intended place to trim shadows; decode is not.
pub struct SceneImage {
    pub data: Vec<f32>,
    pub geom: CfaGeometry,
    pub gains: Gains,
    pub camera: String,
    /// Which photosites reached the sensor's clipping threshold, in `data` order.
    ///
    /// The values themselves cannot answer this after gain equalisation: an unclipped
    /// red or blue sample may legitimately sit above scene 1.0. Keeping the decode's
    /// own verdict is what lets the sensor-clipping overlay report the file rather than
    /// guess from the rendered picture.
    pub clipped: Vec<bool>,
}

impl SceneImage {
    /// CFA colour at a position in the *cropped scene buffer*.
    ///
    /// Safe only because `CfaGeometry` snapped the crop origin to even/even, which
    /// makes crop-relative parity equal absolute sensor parity. That snap is what
    /// buys this shortcut; without it every lookup here would need the absolute
    /// coordinate. Do not remove the snap.
    #[inline]
    pub fn color_at(&self, row: usize, col: usize) -> CfaColor {
        debug_assert!(
            self.geom.crop_x.is_multiple_of(2) && self.geom.crop_y.is_multiple_of(2),
            "the crop origin must sit on a 2x2 CFA tile boundary"
        );
        self.geom.pattern[row & 1][col & 1]
    }
}

/// Per-photosite clipping, computed in RAW units before gains are applied.
///
/// This mask exists because clipping is only detectable before gains. Afterwards
/// the three channels clip at three different values and the boundary is lost.
/// The mask carries that pre-gain verdict forward for overlays and diagnostics.
pub struct ClipMask(pub Vec<bool>);

/// Geometry axis. Orthogonal to `Weighting`; gain equalisation is upstream of both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    /// W/2 x H/2, one output pixel per 2x2 Bayer quad, no interpolation.
    SuperPixel,
    DirectMosaic,
    Demosaic(DemosaicAlgo),
}

/// **Demosaic, at RCD.** the maintainer's call, 2026-08-06, reversing an earlier
/// decision that SuperPixel was the working default.
///
/// The old reason was a fact about one photographer rather than about the app: *the
/// mode the maintainer actually edits in, because he prints smaller.* Against that, SuperPixel
/// halves each dimension, and the halving reaches the export
/// too, so the default put a permanent ceiling on print size — 13 x 8.7 inches from a
/// Leica M10-R against 26 x 17. A default nobody knows to change is the one that decides
/// what most files are printed at.
///
/// The app prepares luminance on a bounded worker; downstream edits reuse it.
impl Default for Sampling {
    fn default() -> Self {
        Self::Demosaic(DemosaicAlgo::Rcd)
    }
}

/// Which full-resolution reconstruction to use. See `crate::demosaic`.
///
/// `UI_ORDER` is the menu, best-first. These are ports of published algorithms and
/// each is attributed at its implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DemosaicAlgo {
    /// Ratio Corrected Demosaicing (Luis Sanz Rodríguez). The default.
    #[default]
    Rcd,
    /// Aliasing Minimization and Zipper Elimination (Emil Martinec).
    Amaze,
    /// Adaptive colour plane interpolation (Adams & Hamilton, 1997).
    HamiltonAdams,
    /// The floor: no look at the image before interpolating. Full size without
    /// full-size detail, plus edge stipple.
    Bilinear,
}

impl DemosaicAlgo {
    pub const UI_ORDER: [Self; 4] = [Self::Rcd, Self::Amaze, Self::HamiltonAdams, Self::Bilinear];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rcd => "RCD",
            Self::Amaze => "AMaZE",
            Self::HamiltonAdams => "Hamilton-Adams",
            Self::Bilinear => "Bilinear",
        }
    }

    /// Stable key for persistence, distinct from the display label so a change of
    /// wording cannot orphan a stored setting or a sidecar. The sidecar already
    /// uses these strings.
    pub fn key(self) -> &'static str {
        match self {
            Self::Rcd => "rcd",
            Self::Amaze => "amaze",
            Self::HamiltonAdams => "hamilton-adams",
            Self::Bilinear => "bilinear",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|a| a.key() == s)
    }

    /// What this one is for. Sourced, not recalled — see `crate::demosaic`.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Rcd => {
                "Ratio Corrected Demosaicing. Interpolates along the direction the \
                 detail runs, chosen per pixel. Strong on fine repeating structure, \
                 where undirected methods maze."
            }
            Self::Amaze => {
                "Aliasing Minimization and Zipper Elimination. Area-interpolates \
                 where texture reaches the sensor's sampling limit and directional \
                 methods start to alias. For fabric, foliage, distant brickwork."
            }
            Self::HamiltonAdams => {
                "Adaptive color plane interpolation. Older and simpler than RCD, and \
                 a useful check against it \u{2014} where the two disagree, the detail is \
                 genuinely ambiguous."
            }
            Self::Bilinear => {
                "Plain averaging, with no look at the image first. The reference the \
                 others improve on. Stipples edges near the resolution limit."
            }
        }
    }
}

/// Spectral axis, in UI order.
///
/// `Photosite` is the default because it is the identity case — literally what the
/// Bayer quad hands you, with no model imported from elsewhere. Rec.2020 luminance
/// coefficients are a colorimetric statement about human perception of a
/// DISPLAY-REFERRED signal and do not apply to sensor-referred data.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Weighting {
    /// 1/4 R, 1/2 G, 1/4 B — which for a Bayer quad is the plain mean of the four
    /// photosites, since two of them are green.
    #[default]
    Photosite,
    Equal,
    Red,
    Green,
    Blue,
    Weighted(f32, f32, f32),
    // Emulsion(Stock) — weights must be re-derived against camera spectral
    // sensitivities, not Rec.2020 CMFs. Open question; not milestone 1.
}

impl Weighting {
    pub const UI_ORDER: [Self; 6] = [
        Self::Photosite,
        Self::Equal,
        Self::Red,
        Self::Green,
        Self::Blue,
        Self::Weighted(1.0, 1.0, 1.0),
    ];

    pub fn label(&self) -> String {
        match self {
            Self::Photosite => "Photosite (1/4 R, 1/2 G, 1/4 B)".into(),
            Self::Equal => "Equal (1/3 each)".into(),
            Self::Red => "Red".into(),
            Self::Green => "Green".into(),
            Self::Blue => "Blue".into(),
            // Show the actual mix. "Weighted" alone is indistinguishable from every
            // other "Weighted", including the one that happens to equal Equal.
            Self::Weighted(r, g, b) => {
                let [r, g, b] = Self::Weighted(*r, *g, *b).weights();
                format!("Weighted ({r:.2}, {g:.2}, {b:.2})")
            }
        }
    }

    /// The weighting's bare name, with no mix in parentheses.
    ///
    /// **For a readout; [`label`](Self::label) is for a chooser.** They are two jobs
    /// and the difference is what each has to answer. A combo box has to say what
    /// picking `Photosite` would *mean*, so it spells out `1/4 R, 1/2 G, 1/4 B`; the
    /// pipeline summary is reporting what already ran to somebody who chose it, and
    /// the parenthetical there is a definition nobody is asking for a second time.
    /// the maintainer asked for it out of the Info panel on exactly that ground.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Photosite => "Photosite",
            Self::Equal => "Equal",
            Self::Red => "Red",
            Self::Green => "Green",
            Self::Blue => "Blue",
            Self::Weighted(..) => "Weighted",
        }
    }

    /// The `Weighted` variant seeded with this weighting's own mix.
    ///
    /// Switching to `Weighted` should unlock the sliders **without changing the
    /// image** — you start from where you were and depart from it deliberately.
    /// Snapping to some fixed triple would make selecting the mode an edit, which is
    /// the same reason the curve is an identity until touched.
    pub fn as_weighted(&self) -> Self {
        match self {
            Self::Weighted(..) => *self,
            other => {
                let [r, g, b] = other.weights();
                Self::Weighted(r, g, b)
            }
        }
    }

    /// Mutable access to the user mix, for the sliders. `None` unless `Weighted`.
    pub fn mix_mut(&mut self) -> Option<(&mut f32, &mut f32, &mut f32)> {
        match self {
            Self::Weighted(r, g, b) => Some((r, g, b)),
            _ => None,
        }
    }

    /// Weights over (R, G, B), normalised to sum 1.0.
    ///
    /// Normalising to unit sum IS the exposure match between modes. After gain
    /// equalisation every channel reads the same value for neutral light, so any
    /// unit-sum weighting yields the same mean brightness — mode switching then
    /// changes spectral character and sharpness without changing exposure, which is
    /// the stated invariant. It falls out of equalisation rather than needing a
    /// per-mode fudge factor.
    pub fn weights(&self) -> [f32; 3] {
        let raw = match *self {
            Self::Photosite => [0.25, 0.5, 0.25],
            Self::Equal => [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0],
            Self::Red => [1.0, 0.0, 0.0],
            Self::Green => [0.0, 1.0, 0.0],
            Self::Blue => [0.0, 0.0, 1.0],
            Self::Weighted(r, g, b) => [r, g, b],
        };
        let sum = raw[0] + raw[1] + raw[2];
        if sum > 0.0 {
            [raw[0] / sum, raw[1] / sum, raw[2] / sum]
        } else {
            [1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0]
        }
    }
}

/// The working stage. `output_dims` is NOT `source_dims` in SuperPixel mode, and
/// every downstream consumer — crop rect, zoom, export — reads `output_dims`.
pub struct LumaImage {
    pub data: Vec<f32>,
    pub output_dims: Dims,
    pub source_dims: Dims,
    /// How many photosites in this output pixel's Bayer block were clipped, `0..=4`.
    /// Empty for synthetic/test images that carry no decode mask.
    pub clipped: Vec<u8>,
}

/// Decode-stage parameters. These change the `SceneImage` itself, so a change here
/// re-runs decode; a change to sampling/weighting only re-derives luminance from an
/// unchanged scene.
// `Eq + Hash` because the decode cache keys on (content, options).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DecodeOptions {
    /// Skip gain equalisation (gains = 1,1,1). A diagnostic: it makes the CFA
    /// pattern visible, which is how you confirm equalisation is doing its job.
    /// The name matches the prototype's "Unity WB (unscaled channels)".
    pub unity_wb: bool,
}

/// Full decode from sensor to a ready `SceneImage`: black -> clip mask -> gains ->
/// normalise. `SceneImage` is the cache boundary downstream of it.
///
/// Highlight reconstruction and TCA correction were deliberately retired on
/// 2026-08-28. Reconstruction produced solarized Bayer-block boundaries even after
/// conservative evidence gating; TCA resampled the mosaic without showing a reliable
/// benefit in real-image review. Neither belongs in the active decode path. Keep the
/// clipping mask for diagnosis and use tonal controls for highlight rendering.
pub fn decode(sensor: &SensorImage, opts: DecodeOptions) -> (SceneImage, ClipMask) {
    to_scene(sensor, opts.unity_wb)
}

/// Decode: black -> clip mask -> gains -> normalise.
///
/// Order is load-bearing. The clip mask must be computed before gains; see
/// `ClipMask`. Normalisation must not clamp; see `SceneImage`.
pub fn to_scene(sensor: &SensorImage, unity_wb: bool) -> (SceneImage, ClipMask) {
    let geom = sensor.geom.clone();
    // Unity gains (1,1,1) leave the CFA pattern in place — the diagnostic case.
    let gains = if unity_wb {
        Gains([1.0, 1.0, 1.0])
    } else {
        Gains::equalizing(sensor.wb_coeffs)
    };
    let (w, h) = (geom.crop.w, geom.crop.h);

    let mut data = vec![0.0f32; w * h];
    let mut clipped = vec![false; w * h];

    data.par_chunks_mut(w)
        .zip(clipped.par_chunks_mut(w))
        .enumerate()
        .for_each(|(row, (drow, crow))| {
            // Absolute sensor coordinates throughout. Never crop-relative.
            let y = geom.crop_y + row;
            for col in 0..w {
                let x = geom.crop_x + col;
                let colour = geom.color_at(y, x);
                let raw = sensor.data[geom.index(y, x)] as f32;

                // 1. clip test, in RAW units, BEFORE gains. rawler does not clamp
                //    to the reported saturation point — measured maxima exceed it
                //    on the Leica, Sony and Canon — so this is `>=`, not `==`.
                let white = sensor.white[colour as usize];
                crow[col] = raw >= white;

                // 2. black subtract, 3. normalise, 4. gain-equalise. No clamping.
                let black = sensor.black.at(y, x);
                let norm = (raw - black) / (white - black);
                drow[col] = norm * gains.for_color(colour);
            }
        });

    let scene = SceneImage {
        data,
        geom,
        gains,
        camera: sensor.camera.clone(),
        clipped: clipped.clone(),
    };
    (scene, ClipMask(clipped))
}

/// Mode switching must change sharpness and spectral character, NOT brightness.
/// See `Weighting::weights` for why unit-sum weights deliver that for free.
pub fn derive_luminance(scene: &SceneImage, sampling: Sampling, weighting: Weighting) -> LumaImage {
    let mut luma = match sampling {
        Sampling::SuperPixel => superpixel(scene, weighting),
        Sampling::DirectMosaic => direct_mosaic(scene, weighting),
        Sampling::Demosaic(algo) => crate::demosaic::demosaic(scene, algo, weighting),
    };
    luma.clipped = clip_counts(scene, sampling);
    luma
}

/// Collapse the photosite mask by Bayer block, using the same output geometry as the
/// luminance path. A block count gives the overlay its only useful distinction in mono:
/// some channels still measured the highlight, or the whole block was censored.
fn clip_counts(scene: &SceneImage, sampling: Sampling) -> Vec<u8> {
    if scene.clipped.is_empty() {
        return Vec::new();
    }
    let (sw, sh) = (scene.geom.crop.w, scene.geom.crop.h);
    let (ow, oh) = match sampling {
        Sampling::SuperPixel => {
            let d = scene.geom.superpixel_dims();
            (d.w, d.h)
        }
        Sampling::DirectMosaic | Sampling::Demosaic(_) => (sw, sh),
    };
    let mut out = vec![0u8; ow * oh];
    out.par_chunks_mut(ow.max(1))
        .enumerate()
        .for_each(|(y, row)| {
            for (x, count) in row.iter_mut().enumerate() {
                let (bx, by) = match sampling {
                    Sampling::SuperPixel => (x * 2, y * 2),
                    Sampling::DirectMosaic | Sampling::Demosaic(_) => (x & !1, y & !1),
                };
                let mut n = 0u8;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (sx, sy) = (bx + dx, by + dy);
                        if sx < sw && sy < sh && scene.clipped[sy * sw + sx] {
                            n += 1;
                        }
                    }
                }
                *count = n;
            }
        });
    out
}

/// One output pixel per 2x2 Bayer quad. No interpolation, no invented data — the
/// closest thing to a true monochrome readout, and what the hardware would produce
/// with the CFA removed.
fn superpixel(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    let src = scene.geom.crop;
    let out = scene.geom.superpixel_dims();
    let [wr, wg, wb] = weighting.weights();

    let mut data = vec![0.0f32; out.w * out.h];
    data.par_chunks_mut(out.w)
        .enumerate()
        .for_each(|(oy, orow)| {
            let r0 = oy * 2;
            for (ox, out_px) in orow.iter_mut().enumerate() {
                let c0 = ox * 2;
                // Accumulate by colour rather than by position, so the quad's two
                // greens are averaged (G1/G2 are not distinguished, by decision) and
                // the code is independent of whether the pattern is RGGB or GBRG.
                let mut sum = [0.0f32; 3];
                let mut count = [0u32; 3];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let c = scene.color_at(r0 + dy, c0 + dx) as usize;
                        sum[c] += scene.data[(r0 + dy) * src.w + (c0 + dx)];
                        count[c] += 1;
                    }
                }
                let mean = |i: usize| {
                    if count[i] > 0 {
                        sum[i] / count[i] as f32
                    } else {
                        0.0
                    }
                };
                *out_px = wr * mean(0) + wg * mean(1) + wb * mean(2);
            }
        });

    LumaImage {
        data,
        output_dims: out,
        source_dims: src,
        clipped: Vec::new(),
    }
}

/// Full-resolution, no interpolation: every output pixel is its own gain-equalised
/// photosite, scaled by the weight for its colour normalised by that colour's
/// photosite density. The CFA pattern stays visible as residual on saturated
/// colour — the point of the mode — while a neutral subject under the default
/// Photosite weighting renders flat.
///
/// Normalising by density is what makes Photosite the identity here. A Bayer quad
/// is 1 red, 2 green, 1 blue, so densities are (¼, ½, ¼) and
/// `weight[c] / density[c]` is `[.25,.5,.25] / [.25,.5,.25] = [1,1,1]` for
/// Photosite — every photosite passes through untouched, flat on neutral. Green
/// weighting gives `[0,2,0]`: greens at 2× (compensating for half density so mean
/// brightness holds), red and blue black — the classic single-channel mosaic. And
/// because `Σ (weight[c]/density[c]) · density[c] = Σ weight[c] = 1`, the mean
/// matches every other sampling mode for any weighting.
fn direct_mosaic(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    let src = scene.geom.crop;
    let w = weighting.weights();

    // Photosite density per colour, from the actual 2×2 tile (generalises across
    // RGGB/BGGR/etc; all Bayer variants give ¼, ½, ¼).
    let mut count = [0u32; 3];
    for r in 0..2 {
        for c in 0..2 {
            count[scene.geom.pattern[r][c] as usize] += 1;
        }
    }
    let gain = [
        if count[0] > 0 {
            w[0] * 4.0 / count[0] as f32
        } else {
            0.0
        },
        if count[1] > 0 {
            w[1] * 4.0 / count[1] as f32
        } else {
            0.0
        },
        if count[2] > 0 {
            w[2] * 4.0 / count[2] as f32
        } else {
            0.0
        },
    ];

    let mut data = vec![0.0f32; src.w * src.h];
    data.par_chunks_mut(src.w)
        .enumerate()
        .for_each(|(row, orow)| {
            for (col, out_px) in orow.iter_mut().enumerate() {
                let c = scene.color_at(row, col) as usize;
                *out_px = scene.data[row * src.w + col] * gain[c];
            }
        });

    LumaImage {
        data,
        output_dims: src,
        source_dims: src,
        clipped: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::CfaColor::*;

    fn scene_rggb(data: Vec<f32>, w: usize, h: usize) -> SceneImage {
        let geom = CfaGeometry::new(w, Dims { w, h }, 0, 0, w, h, [[Red, Green], [Green, Blue]]);
        SceneImage {
            data,
            geom,
            gains: Gains([1.0, 1.0, 1.0]),
            camera: "test".into(),
            clipped: Vec::new(),
        }
    }

    #[test]
    fn photosite_superpixel_is_the_quad_mean() {
        // One RGGB quad: R=0.4, G1=0.6, G2=0.8, B=0.2
        let s = scene_rggb(vec![0.4, 0.6, 0.8, 0.2], 2, 2);
        let luma = derive_luminance(&s, Sampling::SuperPixel, Weighting::Photosite);
        assert_eq!(luma.output_dims, Dims { w: 1, h: 1 });
        assert!((luma.data[0] - 0.5).abs() < 1e-6, "got {}", luma.data[0]);
    }

    #[test]
    fn superpixel_halves_dims_and_keeps_source_dims() {
        let s = scene_rggb(vec![0.5; 8 * 6], 8, 6);
        let luma = derive_luminance(&s, Sampling::SuperPixel, Weighting::Photosite);
        assert_eq!(luma.output_dims, Dims { w: 4, h: 3 });
        assert_eq!(luma.source_dims, Dims { w: 8, h: 6 });
    }

    #[test]
    fn mode_switching_changes_character_not_brightness() {
        // A neutral patch: after gain equalisation every channel reads the same,
        // so every unit-sum weighting must return the same value.
        let s = scene_rggb(vec![0.42; 4 * 4], 4, 4);
        let modes = [
            Weighting::Photosite,
            Weighting::Equal,
            Weighting::Red,
            Weighting::Green,
            Weighting::Blue,
            Weighting::Weighted(0.7, 0.2, 0.1),
        ];
        for m in modes {
            let luma = derive_luminance(&s, Sampling::SuperPixel, m);
            for v in &luma.data {
                assert!((v - 0.42).abs() < 1e-6, "{:?} shifted brightness: {v}", m);
            }
        }
    }

    #[test]
    fn green_weighting_averages_g1_and_g2() {
        let s = scene_rggb(vec![0.1, 0.6, 0.8, 0.9], 2, 2);
        let luma = derive_luminance(&s, Sampling::SuperPixel, Weighting::Green);
        assert!((luma.data[0] - 0.7).abs() < 1e-6, "got {}", luma.data[0]);
    }

    #[test]
    fn all_sampling_modes_full_res_except_superpixel() {
        let s = scene_rggb(vec![0.5; 8 * 6], 8, 6);
        for (mode, dims) in [
            (Sampling::SuperPixel, Dims { w: 4, h: 3 }),
            (Sampling::DirectMosaic, Dims { w: 8, h: 6 }),
            (
                Sampling::Demosaic(DemosaicAlgo::Bilinear),
                Dims { w: 8, h: 6 },
            ),
        ] {
            let luma = derive_luminance(&s, mode, Weighting::Photosite);
            assert_eq!(luma.output_dims, dims, "{mode:?}");
            assert_eq!(luma.source_dims, Dims { w: 8, h: 6 }, "{mode:?}");
        }
    }

    #[test]
    fn every_sampling_and_weighting_matches_brightness_on_neutral() {
        // The load-bearing invariant across BOTH axes: on a neutral patch, the mean
        // output must be independent of sampling and weighting. Per-pixel values
        // differ (DirectMosaic lights only some photosites), so this checks the
        // mean, which is what "brightness" means for a mode switch.
        let s = scene_rggb(vec![0.42; 16 * 16], 16, 16);
        let target = 0.42;
        let weightings = [
            Weighting::Photosite,
            Weighting::Equal,
            Weighting::Red,
            Weighting::Green,
            Weighting::Blue,
            Weighting::Weighted(0.7, 0.2, 0.1),
        ];
        for sampling in [
            Sampling::SuperPixel,
            Sampling::DirectMosaic,
            Sampling::Demosaic(DemosaicAlgo::Bilinear),
        ] {
            for w in weightings {
                let luma = derive_luminance(&s, sampling, w);
                let mean = luma.data.iter().sum::<f32>() / luma.data.len() as f32;
                assert!(
                    (mean - target).abs() < 1e-4,
                    "{sampling:?} + {w:?} shifted brightness: mean {mean}"
                );
            }
        }
    }

    #[test]
    fn weighted_can_actually_differ_from_equal() {
        // The regression: `UI_ORDER` offered `Weighted(1,1,1)`, which normalises to
        // exactly Equal, and nothing in the UI could change r/g/b. "Weighted" was a
        // second name for Equal, and no test noticed because every test that used it
        // passed an explicit non-uniform triple.
        let uniform = Weighting::Weighted(1.0, 1.0, 1.0);
        assert_eq!(
            uniform.weights(),
            Weighting::Equal.weights(),
            "sanity: 1,1,1 is Equal"
        );

        // On a subject that is NOT neutral, a real mix has to move the result.
        let mut data = vec![0.0f32; 4 * 4];
        for row in 0..4 {
            for col in 0..4 {
                // Strongly red subject: red high, blue low.
                data[row * 4 + col] = match [[Red, Green], [Green, Blue]][row & 1][col & 1] {
                    Red => 0.9,
                    Green => 0.5,
                    Blue => 0.1,
                };
            }
        }
        let s = scene_rggb(data, 4, 4);
        let equal = derive_luminance(&s, Sampling::SuperPixel, Weighting::Equal).data[0];
        let reddish = derive_luminance(
            &s,
            Sampling::SuperPixel,
            Weighting::Weighted(0.8, 0.15, 0.05),
        )
        .data[0];
        assert!(
            (reddish - equal).abs() > 0.05,
            "a red-heavy mix rendered the same as Equal: {reddish} vs {equal}"
        );
    }

    #[test]
    fn switching_to_weighted_does_not_change_the_image() {
        // Selecting `Weighted` should unlock the sliders where you already are, not
        // snap to some fixed triple. Otherwise picking the mode is itself an edit.
        let s = scene_rggb(vec![0.9, 0.5, 0.5, 0.1], 2, 2);
        for from in [
            Weighting::Photosite,
            Weighting::Equal,
            Weighting::Red,
            Weighting::Blue,
        ] {
            let before = derive_luminance(&s, Sampling::SuperPixel, from).data[0];
            let after = derive_luminance(&s, Sampling::SuperPixel, from.as_weighted()).data[0];
            assert!(
                (before - after).abs() < 1e-6,
                "{from:?} -> Weighted shifted the image: {before} vs {after}"
            );
        }
    }

    #[test]
    fn as_weighted_is_idempotent() {
        // Re-selecting Weighted must not reset a mix the user has dialled in.
        let w = Weighting::Weighted(0.7, 0.2, 0.1);
        assert_eq!(w.as_weighted(), w);
    }

    #[test]
    fn the_weighted_label_shows_the_mix() {
        // Every Weighted reading "Weighted" is how a dead control hides.
        let a = Weighting::Weighted(0.8, 0.15, 0.05).label();
        let b = Weighting::Weighted(0.1, 0.1, 0.8).label();
        assert_ne!(a, b, "two different mixes shared a label: {a}");
        assert!(a.contains("0.8"), "label does not show the mix: {a}");
    }

    #[test]
    fn a_weighted_mix_is_scale_invariant() {
        // Only the ratios matter — the weights are normalised to unit sum, which is
        // what keeps brightness constant across every mode.
        let s = scene_rggb(vec![0.9, 0.5, 0.5, 0.1], 2, 2);
        let a = derive_luminance(&s, Sampling::SuperPixel, Weighting::Weighted(0.6, 0.3, 0.1));
        let b = derive_luminance(&s, Sampling::SuperPixel, Weighting::Weighted(6.0, 3.0, 1.0));
        assert!(
            (a.data[0] - b.data[0]).abs() < 1e-6,
            "scaling the mix changed the result"
        );
    }

    #[test]
    fn an_all_zero_mix_falls_back_rather_than_producing_nan() {
        // The sliders can reach zero on all three.
        let s = scene_rggb(vec![0.4; 4], 2, 2);
        let luma = derive_luminance(&s, Sampling::SuperPixel, Weighting::Weighted(0.0, 0.0, 0.0));
        assert!(
            luma.data[0].is_finite(),
            "zero mix produced {}",
            luma.data[0]
        );
        assert!(
            (luma.data[0] - 0.4).abs() < 1e-6,
            "zero mix should fall back to Equal"
        );
    }

    #[test]
    fn direct_mosaic_photosite_is_flat_on_neutral() {
        // Photosite is the identity weighting: on an equalised neutral subject,
        // DirectMosaic must render every photosite flat, with NO CFA checkerboard.
        // (This is the regression the visual check caught: a per-colour normaliser
        // that modulated even Photosite.)
        let s = scene_rggb(vec![0.42; 8 * 8], 8, 8);
        let luma = derive_luminance(&s, Sampling::DirectMosaic, Weighting::Photosite);
        for v in &luma.data {
            assert!(
                (v - 0.42).abs() < 1e-6,
                "DirectMosaic+Photosite not flat: {v}"
            );
        }
    }

    #[test]
    fn direct_mosaic_green_lights_only_green_photosites() {
        // GreenOnly == DirectMosaic + Green weighting: greens carry signal, red and
        // blue positions go black.
        let s = scene_rggb(vec![0.5; 4 * 4], 4, 4);
        let luma = derive_luminance(&s, Sampling::DirectMosaic, Weighting::Green);
        for row in 0..4 {
            for col in 0..4 {
                let v = luma.data[row * 4 + col];
                match s.color_at(row, col) {
                    Green => assert!(v > 0.0, "green photosite went dark at {row},{col}"),
                    _ => assert_eq!(v, 0.0, "non-green lit at {row},{col}: {v}"),
                }
            }
        }
    }

    #[test]
    fn unity_wb_leaves_the_cfa_pattern_in_place() {
        // With real gains a neutral-after-equalisation quad is flat; with unity_wb
        // the raw per-channel differences survive, which is the diagnostic's point.
        use crate::sensor::{BlackPattern, SensorImage};
        let geom = CfaGeometry::new(
            2,
            Dims { w: 2, h: 2 },
            0,
            0,
            2,
            2,
            [[Red, Green], [Green, Blue]],
        );
        // Raw quad with distinct channels; wb_coeffs chosen to equalise them.
        let sensor = SensorImage {
            data: vec![1000, 2000, 2000, 1500],
            geom,
            black: BlackPattern {
                levels: vec![0.0],
                w: 1,
                h: 1,
            },
            white: [4000.0; 3],
            wb_coeffs: [2.0, 1.0, 4.0 / 3.0], // R*2=0.5, G=0.5, B*4/3=0.5 after /4000
            camera: "test".into(),
            meta: Default::default(),
        };
        let (equalised, _) = to_scene(&sensor, false);
        let (unity, _) = to_scene(&sensor, true);
        // Equalised: all four photosites read ~0.5.
        for v in &equalised.data {
            assert!((v - 0.5).abs() < 1e-4, "equalised not flat: {v}");
        }
        // Unity: the raw spread survives.
        let hi = unity.data.iter().cloned().fold(0.0f32, f32::max);
        let lo = unity.data.iter().cloned().fold(1.0f32, f32::min);
        assert!(
            hi - lo > 0.1,
            "unity_wb flattened the CFA (spread {})",
            hi - lo
        );
    }

    #[test]
    fn clipping_counts_follow_bayer_blocks_in_every_output_geometry() {
        let mut s = scene_rggb(vec![0.5; 8], 4, 2);
        // First block: one of four clipped. Second block: all four.
        s.clipped = vec![true, false, true, true, false, false, true, true];

        assert_eq!(clip_counts(&s, Sampling::SuperPixel), vec![1, 4]);
        for mode in [
            Sampling::DirectMosaic,
            Sampling::Demosaic(DemosaicAlgo::Bilinear),
        ] {
            assert_eq!(clip_counts(&s, mode), vec![1, 1, 4, 4, 1, 1, 4, 4]);
        }
    }
}
