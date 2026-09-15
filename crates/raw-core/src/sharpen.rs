//! Output sharpening — à-trous wavelet band gain, on the print.
//!
//! **Where it runs is the design.** Last thing before the encode:
//! `tone map → resample → grain → sharpen → encode`. Last means last — it runs over the
//! grain too, because output sharpening compensates the *medium*, and dot gain does not
//! discriminate between grain and detail. The radius is in **output pixels**, which is
//! what makes it *output* sharpening.
//!
//! `OutputParams` is in no tier of [`Dirty`](crate::params::Dirty), so anything defined
//! on the output grid is downstream of that seal: this module is **CPU, export-only and
//! invisible to the viewport**, filed as [`grain`](crate::grain) is and judged in the
//! same loupe.
//!
//! # The algorithm
//!
//! Decompose into bands with the 5-tap cardinal B-spline at à-trous stride `2^s`,
//! weight each by a Gaussian envelope in scale space centred on the radius, and
//! reconstruct:
//!
//! ```text
//! out = Σ_s H_s · (1 + amount · w_s · shield_s) + G_{n-1}
//! ```
//!
//! **The residual `G` is never modified**, which is why this cannot shift overall tone.
//! Full derivation, and why a wavelet rather than the bi-Laplacian PDE, are in
//! `docs/milestone-14-brief.md`.
//!
//! [`MAX_SCALES`] is **4, not 6**: six covers σ ≈ 68 px, which is local contrast and a
//! different module, and with [`RADIUS_RANGE`] topping out at 2 px later scales can
//! never carry weight. Band storage is real memory on a 100 MP export.
//!
//! # The edge shield is normalised against a LOCAL energy
//!
//! Per band, the local coefficient energy `v = à-trous-blur(H_s²)` against a reference:
//!
//! ```text
//! shield = 1 / (1 + edges · v / (4 · v_ref))
//! ```
//!
//! Texture sits at `v ≈ v_ref` and keeps nearly full gain; a hard edge concentrates
//! energy orders of magnitude above its surroundings and is left alone.
//!
//! **`v_ref` is `v` blurred again at [`REF_STRIDE`], not the band's mean over the whole
//! image — and the loupe is why.** A tile cut from a smooth sky has far lower mean band
//! energy than the frame, so under a global mean the same pixel would be shielded
//! harder in the loupe than in the export, and the module's only feedback would be
//! lying about the module. A local reference is computable from a bounded neighbourhood,
//! so a tile and the print agree outside [`SharpenParams::apron`].

use rayon::prelude::*;

/// 5-tap cardinal B-spline: `[1, 4, 6, 4, 1] / 16`.
const B: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// Effective Gaussian σ of [`B`] at stride 1, in pixels.
const SIGMA_B: f32 = 1.055_365;

/// Maximum wavelet scales. Scale 4 reaches stride 8 and σ ≈ 9.8 px, comfortably past
/// the top of [`RADIUS_RANGE`] with its spread — a ceiling rather than a working value,
/// since [`SharpenParams::scales`] stops at 3 anywhere in the range the panel offers.
/// See the module note on why this is 4 and the prototype's is 6.
pub const MAX_SCALES: usize = 4;

/// Stride of the shield's reference blur. **Coarser than every band the module can
/// use** — the coarsest is stride 8 — so the reference is a neighbourhood energy
/// rather than a restatement of the band. See the module note.
const REF_STRIDE: usize = 16;

/// Width of the scale-space envelope, in the same units as σ.
///
/// **Fixed rather than exposed.** The prototype has it as a slider and then never moves
/// it independently: its three presets are `(passes, radius, spread)` of
/// `(0, 1.0, 0.5)`, `(2, 1.0, 0.5)` and `(3, 1.0, 1.0)`, so spread moved once, with the
/// strength, and radius never moved at all. A third slider that only ever tracks the
/// second is a control to learn rather than a control to use.
///
/// 0.75 sits between the prototype's two values, and it is the value **measured**
/// rather than the value assumed — `cargo run --release --example sharpen-sweep` is
/// what settled it. Texture gained per unit of halo overshoot, on a 2736 × 1824 frame
/// at the default amount:
///
/// ```text
///                  radius 0.5      radius 1.0
///   spread 0.5     1.118 / 21.7    1.411 / 20.6
///   spread 0.75    1.237 / 21.1    1.417 / 19.6
///   spread 1.5     1.377 / 15.1    1.448 / 12.2
/// ```
///
/// **1.5 is plainly wrong**: it costs 40% of the efficiency for 2% more texture,
/// because a wide envelope gains bands the radius was not pointing at. **0.5 is subtly
/// wrong**, and this is the reading worth keeping: it buys about 5% more efficiency and
/// pays for it at the *bottom* of the radius slider, where an envelope that narrow falls
/// between the finest band and nothing at all — 1.118 against 0.75's 1.237. A control
/// whose first third does very little is worse than one that is a few percent less
/// efficient everywhere.
const SPREAD: f32 = 0.75;

/// What [`amount`](SharpenParams::amount) may be, and what a panel should offer.
pub const AMOUNT_RANGE: std::ops::RangeInclusive<f32> = 0.0..=2.0;
/// What [`radius`](SharpenParams::radius) may be, in **output** pixels.
///
/// **Tops out at 2, not at the prototype's 8**, and this is measured rather than
/// tidied. `sharpen-sweep` on three frames — a 24 MP ARW, a 11 MP NEF and a 3 MP RW2 —
/// puts the texture gain at its peak at 1 px and gone by 4:
///
/// ```text
///   radius        0.5     1.0     2.0     4.0
///   texture     1.237   1.417   1.146   1.019
///   ratio        21.1    19.6     4.1     0.9
/// ```
///
/// By 4 px the module is adding essentially no detail and only overshoot, because those
/// bands carry local contrast rather than the fine structure a downsample cost. That is
/// the same argument that kept the six-band equaliser out — see the module note — and
/// it applies to a slider's upper half just as well as to four extra controls. A range
/// whose top half does nothing useful is a range that invites the wrong setting.
pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 0.5..=2.0;
/// What [`edges`](SharpenParams::edges) may be.
pub const EDGES_RANGE: std::ops::RangeInclusive<f32> = 0.0..=1.0;

/// Output sharpening's settings.
///
/// **In [`Params`](crate::params::Params) but in no tier of
/// [`Dirty`](crate::params::Dirty)**, for the same reason as
/// [`GrainParams`](crate::grain::GrainParams) and
/// [`OutputParams`](crate::output::OutputParams): it is per-image state that belongs in
/// the sidecar and in undo, and nothing in it can change a viewport pixel. What it
/// *can* change is the print loupe, which is not the viewport and asks for itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SharpenParams {
    /// Module bypass. Off by default: every frame judged before this module existed
    /// must still export the same way.
    pub enabled: bool,
    /// How hard the selected band is amplified. The primary strength control.
    pub amount: f32,
    /// The detail scale to target, in **output pixels** — the unit is the point of the
    /// module. See the module note.
    pub radius: f32,
    /// The variance edge shield. `0` amplifies every coefficient alike and haloes
    /// silhouettes; see the module note for what the number does.
    pub edges: f32,
}

impl Default for SharpenParams {
    /// **A light capture-sharpen for a print.** `amount` is a taste call — the sweep
    /// finds the texture-per-overshoot ratio flat across its whole range, so there is no
    /// cliff to avoid. `radius` is the prototype's settled 1.0 px.
    ///
    /// **`edges` defaults ON**, against the prototype's `0.0`, which exists only for
    /// compatibility with builds that predate the shield. Halos on silhouettes are the
    /// characteristic failure of output sharpening, so defaulting it off would ship the
    /// known-bad configuration.
    ///
    /// **0.5 is the knee**, measured by `sharpen-sweep`; the table is in
    /// `docs/decisions.md`. The metric keeps improving past it, and that is why the
    /// metric does not decide it — a shield driven hard sharpens flat texture and leaves
    /// real edges alone, and a print wants its edges crisp too.
    fn default() -> Self {
        Self {
            enabled: false,
            amount: 0.75,
            radius: 1.0,
            edges: 0.5,
        }
    }
}

impl SharpenParams {
    /// Whether this changes any exported pixel.
    ///
    /// **One predicate, and every pipeline and UI site must share it.** The prototype
    /// is emphatic about this for a reason its two-model design made sharper, and it
    /// still holds with one model: an amount of zero is the module doing nothing, and
    /// a site that tested `enabled` alone would pay for a decomposition that
    /// reconstructs its own input.
    pub fn is_active(&self) -> bool {
        self.enabled && self.amount > 1e-6
    }

    /// Whether the user has touched it, ignoring the bypass. See
    /// [`GrainParams::is_default`](crate::grain::GrainParams::is_default).
    pub fn is_default(&self) -> bool {
        let d = Self::default();
        Self {
            enabled: d.enabled,
            ..*self
        } == d
    }

    /// Whether the module's dot should read as modified. See
    /// [`is_modified`](crate::params::is_modified).
    pub fn is_modified(&self) -> bool {
        crate::params::is_modified(
            self.is_default(),
            self.is_active(),
            Self::default().is_active(),
        )
    }

    /// How many bands the envelope actually reaches, which is what runs.
    ///
    /// Stops as soon as σ is well outside the envelope, so a 1 px radius costs two
    /// bands rather than four. The bands beyond it would be multiplied by a weight of
    /// order `1e-8` and cost a full-resolution blur each to do it.
    pub fn scales(&self) -> usize {
        (0..MAX_SCALES)
            .take_while(|&s| sigma_at_scale(s) < self.radius + 2.0 * SPREAD + 2.0)
            .count()
            .clamp(1, MAX_SCALES)
    }

    /// How much margin a **crop** needs around it to sharpen like the print it was cut
    /// from, in output pixels.
    ///
    /// A crop's convolution clamps against the crop's own edge instead of seeing the
    /// neighbours that are really there, so a band around the outside comes out wrong.
    /// Render `apron()` pixels wider on each side, show the middle, and the crop *is*
    /// the file — which is what the print loupe does, and what
    /// `a_tile_sharpens_like_the_print_it_was_cut_from_outside_its_apron` measures.
    ///
    /// The reach is three things stacked, and the shield's reference is the largest:
    ///
    /// ```text
    ///   G_s      blurs at strides 1..2^s          2·(2^(s+1) − 1)
    ///   v        blurs H_s² at the band's stride  2·2^s
    ///   v_ref    blurs v at REF_STRIDE            2·REF_STRIDE
    /// ```
    pub fn apron(&self) -> u32 {
        if !self.is_active() {
            return 0;
        }
        let s = self.scales() - 1;
        let band = 2 * ((1usize << (s + 1)) - 1);
        let energy = if self.edges > 1e-6 {
            2 * (1usize << s) + 2 * REF_STRIDE
        } else {
            0
        };
        (band + energy) as u32
    }
}

/// Cumulative σ of the low-pass `G_s`, from the à-trous ladder:
/// `σ²(G_s) = σ_B² · (4^(s+1) − 1) / 3`.
fn sigma_at_scale(s: usize) -> f32 {
    let exp = 4_f32.powi(s as i32 + 1);
    SIGMA_B * ((exp - 1.0) / 3.0).sqrt()
}

/// Scale-space envelope weight for band `s`: `w_s = exp(-(σ_s - radius)² / spread²)`.
fn band_weight(s: usize, radius: f32) -> f32 {
    let delta = sigma_at_scale(s) - radius;
    (-delta * delta / (SPREAD * SPREAD)).exp()
}

/// Separable 5-tap B-spline blur at à-trous stride `step`.
///
/// **Clamped (replicate) boundary, not mirrored.** The prototype's comment says mirror
/// and its code clamps; the code is what produced the pictures the maintainer judged, so the
/// code is what is ported. It matters only within [`SharpenParams::apron`] of an edge,
/// which is exactly the region the apron exists to make irrelevant.
fn atrous_blur(src: &[f32], w: usize, h: usize, step: usize) -> Vec<f32> {
    let mut tmp = vec![0.0_f32; src.len()];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let base = y * w;
        for (x, out) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (k, &bk) in B.iter().enumerate() {
                let off = (k as isize - 2) * step as isize;
                let xi = (x as isize + off).clamp(0, w as isize - 1) as usize;
                acc += bk * src[base + xi];
            }
            *out = acc;
        }
    });

    // The vertical pass walks **rows** of the destination rather than columns, so both
    // passes stay row-major. Striding down a column of a flat buffer is a cache miss
    // per tap on anything wider than a cache line.
    let mut dst = vec![0.0_f32; src.len()];
    dst.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (k, &bk) in B.iter().enumerate() {
            let off = (k as isize - 2) * step as isize;
            let yi = (y as isize + off).clamp(0, h as isize - 1) as usize;
            let src_row = &tmp[yi * w..yi * w + w];
            for (out, &v) in row.iter_mut().zip(src_row) {
                *out += bk * v;
            }
        }
    });
    dst
}

/// Sharpen a toned print: interleaved **luminance and chroma**, and the luminance only.
///
/// # What the three numbers are, stated because the name used to get it wrong
///
/// This was called `apply_lab` and documented as taking OKLab. It does not. The export
/// tail carries `[y, a, b]` where **`y` is display-referred luminance** — what the tone
/// map produced and what `Toned::y` is — beside OKLab's two chroma axes. The lightness
/// OKLab wants is its cube root, and it is taken at the *encode*, not here.
///
/// That the buffer is a mixture is deliberate rather than sloppy: plane 0 is exactly
/// what the **untoned** path sharpens, so a toned export and an untoned one sharpen the
/// same quantity and a print does not change its detail because it acquired a hue. A
/// version that converted to lightness first would sharpen a different signal from the
/// one beside it in the same `match`.
///
/// **`a` and `b` pass through untouched.** Sharpening three channels independently is
/// the obvious thing and it produces **hue fringes**: an à-trous band gain overshoots
/// either side of an edge by design, and three independent overshoots are three
/// different colours. Sharpening the lightness alone cannot fringe, because nothing
/// moves off the neutral axis that was not already off it.
///
/// It is also the physically honest reading of what this module is for. Output
/// sharpening compensates the **medium's spatial response** — dot gain, paper spread —
/// and that response blurs detail rather than hue. The eye's chroma acuity is far below
/// its luma acuity at the scale this works on, so sharpening the chroma would buy
/// nothing visible and cost the fringes.
///
/// Cheaper as a side effect: one plane, not three.
pub fn apply_toned(image: &[f32], w: usize, h: usize, p: &SharpenParams) -> Vec<f32> {
    if !p.is_active() || w == 0 || h == 0 {
        return image.to_vec();
    }
    debug_assert_eq!(image.len(), w * h * 3, "sharpen: buffer is not w * h * 3");

    let luma: Vec<f32> = image.iter().step_by(3).copied().collect();
    let sharpened = apply(&luma, w, h, p);

    let mut out = image.to_vec();
    for (px, l) in out.chunks_exact_mut(3).zip(sharpened) {
        px[0] = l;
    }
    out
}

/// Sharpen a display-referred single-channel image in place of its input.
///
/// `image` is post-tone-map and post-resize, so it is nominally in `[0, 1]` — and like
/// [`resample`](crate::resample), this may ring a little past that range. The clamp
/// belongs at the encode, where it already is, and not here: clamping mid-chain would
/// flatten a highlight that grain is about to modulate.
///
/// Returns the input unchanged when [`SharpenParams::is_active`] is false, so callers
/// do not each need the predicate.
pub fn apply(image: &[f32], w: usize, h: usize, p: &SharpenParams) -> Vec<f32> {
    if !p.is_active() || w == 0 || h == 0 {
        return image.to_vec();
    }
    debug_assert_eq!(image.len(), w * h, "sharpen: buffer is not w * h");

    let n = p.scales();
    let mut low = image.to_vec();
    let mut out = vec![0.0_f32; image.len()];

    for s in 0..n {
        let step = 1usize << s;
        let g_s = atrous_blur(&low, w, h, step);
        // H_s = G_{s-1} - G_s, the detail this band carries.
        let h_s: Vec<f32> = low.iter().zip(&g_s).map(|(a, b)| a - b).collect();

        let gain = p.amount * band_weight(s, p.radius);
        if gain.abs() < 1e-6 {
            // Outside the envelope. The band still has to be *added back* — dropping it
            // would low-pass the picture rather than leave it alone.
            for (o, d) in out.iter_mut().zip(&h_s) {
                *o += d;
            }
        } else if p.edges <= 1e-6 {
            for (o, d) in out.iter_mut().zip(&h_s) {
                *o += d * (1.0 + gain);
            }
        } else {
            let energy: Vec<f32> = h_s.iter().map(|v| v * v).collect();
            let v = atrous_blur(&energy, w, h, step);
            let v_ref = atrous_blur(&v, w, h, REF_STRIDE);
            let k = p.edges * 0.25;
            for ((o, d), (&vv, &rr)) in out.iter_mut().zip(&h_s).zip(v.iter().zip(&v_ref)) {
                let shield = 1.0 / (1.0 + k * vv / rr.max(1e-12));
                *o += d * (1.0 + gain * shield);
            }
        }
        low = g_s;
    }

    // The residual low-pass, unmodified. This is what makes the module unable to shift
    // overall tone however hard it is driven.
    for (o, l) in out.iter_mut().zip(&low) {
        *o += l;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> SharpenParams {
        SharpenParams {
            enabled: true,
            ..Default::default()
        }
    }

    /// A deterministic field with structure at several scales: fine texture, a hard
    /// vertical edge, and a slow ramp. Enough for the shield to have something to tell
    /// apart.
    fn field(w: usize, h: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let ramp = x as f32 / w as f32 * 0.3;
                let texture = (((x * 7 + y * 13) % 11) as f32 / 11.0 - 0.5) * 0.06;
                let edge = if x > w / 2 { 0.45 } else { 0.0 };
                v[y * w + x] = (0.25 + ramp + texture + edge).clamp(0.0, 1.0);
            }
        }
        v
    }

    #[test]
    fn an_inactive_module_returns_its_input() {
        let img = field(40, 30);
        let off = SharpenParams::default();
        assert!(!off.is_active());
        assert_eq!(apply(&img, 40, 30, &off), img);

        // Enabled but at zero amount is the same nothing, and `is_active` is the one
        // predicate that says so.
        let zero = SharpenParams {
            enabled: true,
            amount: 0.0,
            ..Default::default()
        };
        assert!(!zero.is_active());
        assert_eq!(apply(&img, 40, 30, &zero), img);
    }

    #[test]
    fn a_flat_field_is_unchanged() {
        // Every band is zero, so there is nothing to amplify and the residual is the
        // picture. Any drift here is a reconstruction that does not sum to unity.
        let img = vec![0.4_f32; 32 * 32];
        let out = apply(&img, 32, 32, &active());
        for (a, b) in img.iter().zip(&out) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn the_residual_is_never_modified_so_the_mean_holds() {
        // The claim in the module note, measured. However hard it is driven, the module
        // moves detail about and cannot shift overall tone.
        let (w, h) = (64, 48);
        let img = field(w, h);
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        for amount in [0.5, 1.0, 2.0] {
            let p = SharpenParams { amount, ..active() };
            let out = apply(&img, w, h, &p);
            assert!(
                (mean(&img) - mean(&out)).abs() < 2e-4,
                "amount {amount}: {} -> {}",
                mean(&img),
                mean(&out)
            );
        }
    }

    #[test]
    fn sharpening_amplifies_fine_detail() {
        let (w, h) = (64, 48);
        let img = field(w, h);
        // Local variance over an interior window well away from the hard edge, which is
        // where the texture lives.
        let energy = |v: &[f32]| {
            let mut acc = 0.0_f32;
            for y in 8..h - 8 {
                for x in 8..w / 2 - 8 {
                    let c = v[y * w + x];
                    acc += (c - v[y * w + x + 1]).abs() + (c - v[(y + 1) * w + x]).abs();
                }
            }
            acc
        };
        let before = energy(&img);
        let after = energy(&apply(&img, w, h, &active()));
        assert!(after > before * 1.05, "{before} -> {after}");
    }

    #[test]
    fn the_shield_spares_a_hard_edge_and_leaves_texture_alone() {
        // The whole reason the shield exists: an unshielded band gain haloes a
        // silhouette. Measured as the overshoot either side of the step at x = w/2.
        let (w, h) = (64, 48);
        let img = field(w, h);
        let overshoot = |v: &[f32]| {
            let y = h / 2;
            let lo = v[y * w + w / 2 - 2];
            let hi = v[y * w + w / 2 + 1];
            (img[y * w + w / 2 - 2] - lo).max(hi - img[y * w + w / 2 + 1])
        };
        let unshielded = apply(
            &img,
            w,
            h,
            &SharpenParams {
                edges: 0.0,
                ..active()
            },
        );
        let shielded = apply(
            &img,
            w,
            h,
            &SharpenParams {
                edges: 1.0,
                ..active()
            },
        );
        assert!(
            overshoot(&shielded) < overshoot(&unshielded),
            "shield did not reduce the halo: {} vs {}",
            overshoot(&shielded),
            overshoot(&unshielded)
        );
    }

    #[test]
    fn the_radius_selects_a_scale() {
        // A 1 px radius must not reach a structure ten pixels wide. If it does, the
        // envelope is not doing its job and the module is an unsharp mask with extra
        // steps.
        let (w, h) = (64, 64);
        let mut img = vec![0.2_f32; w * h];
        for y in 20..44 {
            for x in 20..30 {
                img[y * w + x] = 0.8;
            }
        }
        let out = apply(
            &img,
            w,
            h,
            &SharpenParams {
                edges: 0.0,
                ..active()
            },
        );
        // The middle of the bar is far from either of its own edges at this radius.
        let mid = 32 * w + 25;
        assert!(
            (out[mid] - img[mid]).abs() < 1e-3,
            "{} vs {}",
            img[mid],
            out[mid]
        );
    }

    #[test]
    fn scales_and_apron_grow_with_the_radius() {
        let fine = SharpenParams {
            radius: 0.5,
            ..active()
        };
        let coarse = SharpenParams {
            radius: 4.0,
            ..active()
        };
        assert!(
            fine.scales() < coarse.scales(),
            "{} {}",
            fine.scales(),
            coarse.scales()
        );
        assert!(
            fine.apron() < coarse.apron(),
            "{} {}",
            fine.apron(),
            coarse.apron()
        );
        assert_eq!(
            SharpenParams::default().apron(),
            0,
            "an inactive module needs no margin"
        );
        assert!(coarse.scales() <= MAX_SCALES);
    }

    #[test]
    fn a_tile_sharpens_like_the_print_it_was_cut_from_outside_its_apron() {
        // The loupe's whole contract, and the reason the shield normalises locally: a
        // tile rendered `apron()` wider and shown from the middle has to be the file's
        // own pixels. Under the prototype's global mean this test cannot pass at all.
        let (w, h) = (260, 220);
        let img = field(w, h);
        for p in [
            active(),
            SharpenParams {
                radius: 4.0,
                ..active()
            },
            SharpenParams {
                edges: 1.0,
                amount: 2.0,
                ..active()
            },
        ] {
            let print = apply(&img, w, h, &p);
            let apron = p.apron() as usize;

            // A tile from the interior, taken with its apron and shown without it.
            let (tw, th) = (48, 40);
            let (ox, oy) = (100, 90);
            assert!(
                ox >= apron && oy >= apron && ox + tw + apron <= w && oy + th + apron <= h,
                "apron {apron} does not fit this fixture"
            );
            let (sw, sh) = (tw + 2 * apron, th + 2 * apron);
            let mut tile = vec![0.0_f32; sw * sh];
            for y in 0..sh {
                let src = (oy - apron + y) * w + (ox - apron);
                tile[y * sw..y * sw + sw].copy_from_slice(&img[src..src + sw]);
            }
            let cut = apply(&tile, sw, sh, &p);

            let mut worst = 0.0_f32;
            for y in 0..th {
                for x in 0..tw {
                    let a = cut[(y + apron) * sw + (x + apron)];
                    let b = print[(oy + y) * w + (ox + x)];
                    worst = worst.max((a - b).abs());
                }
            }
            assert!(
                worst < 1e-5,
                "apron {apron} leaves {worst} of error at radius {}",
                p.radius
            );
        }
    }
}

#[cfg(test)]
mod toned_tests {
    use super::*;

    fn edge(w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .map(|i| if (i % w) < w / 2 { 0.2 } else { 0.8 })
            .collect()
    }

    fn params() -> SharpenParams {
        SharpenParams {
            enabled: true,
            ..Default::default()
        }
    }

    /// **The hue-fringe guard.** Sharpening three channels independently overshoots
    /// each of them differently at an edge, and three different overshoots are three
    /// different colours. Carrying `a` and `b` through untouched makes that
    /// unreachable rather than merely unlikely.
    #[test]
    fn chroma_is_carried_through_a_sharpen_untouched() {
        let (w, h) = (32, 16);
        let l = edge(w, h);
        let mut lab = Vec::with_capacity(w * h * 3);
        for (i, &v) in l.iter().enumerate() {
            // Chroma that varies across the same edge, so a leak would be visible.
            lab.extend_from_slice(&[v, 0.05 + 0.01 * (i % 7) as f32, -0.03]);
        }

        let out = apply_toned(&lab, w, h, &params());
        assert_eq!(out.len(), lab.len());
        for (i, (o, s)) in out.chunks_exact(3).zip(lab.chunks_exact(3)).enumerate() {
            assert_eq!(o[1], s[1], "a moved at {i}");
            assert_eq!(o[2], s[2], "b moved at {i}");
        }
    }

    /// And the lightness really is sharpened — by exactly what the single-channel path
    /// would have done, so the two cannot drift.
    #[test]
    fn the_luminance_plane_matches_the_single_channel_path() {
        let (w, h) = (32, 16);
        let l = edge(w, h);
        let lab: Vec<f32> = l.iter().flat_map(|&v| [v, 0.04, -0.02]).collect();

        let want = apply(&l, w, h, &params());
        let got = apply_toned(&lab, w, h, &params());

        for (i, (o, e)) in got.chunks_exact(3).zip(&want).enumerate() {
            assert_eq!(o[0], *e, "luminance diverged at {i}");
        }
        assert_ne!(want, l, "the fixture should actually be sharpened");
    }

    /// An inactive module returns its input, so callers do not each need the predicate.
    #[test]
    fn an_inactive_sharpen_returns_the_toned_buffer_unchanged() {
        let (w, h) = (8, 8);
        let lab: Vec<f32> = (0..w * h)
            .flat_map(|i| [i as f32 / 64.0, 0.03, 0.01])
            .collect();
        assert_eq!(apply_toned(&lab, w, h, &SharpenParams::default()), lab);
    }
}
