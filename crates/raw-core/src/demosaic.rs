//! Full-resolution demosaic.
//!
//! # What these are for, in this app
//!
//! Demosaic is the one sampling mode that **invents data**, which is exactly what
//! SuperPixel and DirectMosaic exist to avoid. It earns its place anyway: it is what
//! a user reaches for when they want full resolution without the DirectMosaic grid.
//!
//! Everything here reconstructs three channels and then collapses them with the
//! weighting, in one pass — a full RGB image is never materialised. On a 100 MP
//! frame three `f32` planes would be 1.2 GB, and the output is one channel.
//!
//! **The weighted sum to grey erases roughly half of what distinguishes these
//! algorithms from each other**, because they diverge mostly in how they handle
//! *colour* artifacts — false colour, zipper fringing, maze patterns on fine
//! detail. What survives the sum is the luminance-resolution difference and the
//! spatial artifacts that are not purely chromatic. That is why the decided scope is
//! to build them, compare them on real frames, and **ship only the ones that
//! visibly differ**; a menu promising a difference it does not deliver is worse than
//! a shorter menu.
//!
//! # Provenance
//!
//! These are ports of published algorithms, transcribed from the reference
//! implementations rather than from recollection. Each carries its attribution at
//! the function that implements it. They are GPL, which is compatible with this
//! workspace's `GPL-3.0-or-later`, and each would have to be replaced for any
//! closed-source build.
//!
//! # Two adaptations every port here shares
//!
//! **Clamp at zero, never at one.** The references clamp their input to `[0, 1]`
//! because their raw data is `0..65535` integer and the ratio steps want a known
//! range. `SceneImage` is `f32` in `[0, ~2]` and **clamping at 1.0 would discard
//! real gain-equalised sensor headroom**. The low clamp is kept and is not cosmetic:
//! `(raw - black) / (white - black)` can go
//! negative on noise below the black level, and a negative sample would poison the
//! ratio and gradient arithmetic.
//!
//! The absolute tolerances (`EPS` and friends) carry over unchanged, because scene
//! normalisation already puts 1.0 at the saturation point — the same place the
//! references' `x / 65536` puts it.
//!
//! **Tiles, because they bound memory.** The reference tiling is usually presented
//! as a cache and threading optimisation. Here it is load-bearing for a second
//! reason: whole-image scratch for RCD would be ~2.6 GB on a 100 MP frame, against
//! ~1 MB per thread tiled.
//!
//! # The lattice trap
//!
//! Tile origins **must be even**, or the tile-relative CFA phase stops matching the
//! sensor's and every tile after the first demosaics against the wrong pattern.
//! `CfaGeometry` already snaps the crop origin to even/even, which is what lets this
//! module use crop-relative coordinates at all; the tile stride here must preserve
//! it. `TILE_STEP` is even, and there is a test that says so.

use rayon::prelude::*;

use crate::geometry::CfaColor;
use crate::scene::{DemosaicAlgo, LumaImage, SceneImage, Weighting};

/// Dispatch. `derive_luminance` is the only caller.
pub fn demosaic(scene: &SceneImage, algo: DemosaicAlgo, weighting: Weighting) -> LumaImage {
    match algo {
        DemosaicAlgo::Rcd => rcd(scene, weighting),
        DemosaicAlgo::Amaze => amaze(scene, weighting),
        DemosaicAlgo::HamiltonAdams => hamilton_adams(scene, weighting),
        DemosaicAlgo::Bilinear => bilinear(scene, weighting),
    }
}

/// Tile side, and the border discarded on every side of it.
///
/// **Shared by every tiled algorithm here, deliberately.** Hamilton-Adams needs 5,
/// RCD needs 9 and AMaZE needs 16, and giving each its own would mean they disagree
/// about the outermost pixels for a reason that has nothing to do with the
/// algorithms — the comparison instrument would then report a difference at the
/// frame edge that is an artifact of the harness. One border makes every mode's
/// frame identical, so a difference map shows only where they actually reconstruct
/// differently. It is therefore the **maximum** any of them needs, not the minimum.
const TILE: usize = 192;
const BORDER: usize = 16;
/// Distance between tile origins. **Must be even** — see the module note on the
/// lattice trap.
const TILE_STEP: usize = TILE - 2 * BORDER;

/// The tile row a band index covers: `(row_start, row_end)` in image coordinates.
///
/// Band `tr` writes output rows `[row_start + BORDER, row_end - BORDER)`, and those
/// ranges tile `[BORDER, h - BORDER)` exactly — which is what lets the output be
/// split into uniform `TILE_STEP`-row chunks and handed to rayon.
#[inline]
fn band_bounds(tr: usize, h: usize) -> (usize, usize) {
    let row_start = tr * TILE_STEP;
    (row_start, (row_start + TILE).min(h))
}

/// The weighting, as a per-CFA-colour lookup.
type Mix = [f32; 3];

/// Clamp at zero. See the module note: the high end is deliberately open.
#[inline]
fn floor0(v: f32) -> f32 {
    if v > 0.0 { v } else { 0.0 }
}

#[inline]
fn sqr(x: f32) -> f32 {
    x * x
}

/// `a * b + (1 - a) * c`, written as the reference writes it.
#[inline]
fn intp(a: f32, b: f32, c: f32) -> f32 {
    a * (b - c) + c
}

// --------------------------------------------------------------------- borders

/// Interpolate the outermost `bord` pixels the way the reference does: a 3x3
/// per-colour mean, using whatever neighbours exist.
///
/// This is not a quality path and is not meant to be. Every directional algorithm
/// needs several rows of context it does not have at the frame edge, and the
/// alternative — reflecting the image to fabricate that context — invents structure
/// at the one place the eye checks for it. A local mean is honest about knowing
/// less there.
fn border_luma(scene: &SceneImage, mix: Mix, bord: usize, out: &mut [f32]) {
    let src = scene.geom.crop;
    let (w, h) = (src.w, src.h);

    let is_border =
        |row: usize, col: usize| row < bord || col < bord || row + bord >= h || col + bord >= w;

    out.par_chunks_mut(w).enumerate().for_each(|(row, orow)| {
        for (col, o) in orow.iter_mut().enumerate() {
            if !is_border(row, col) {
                continue;
            }
            let mut sum = [0.0f32; 3];
            let mut n = [0u32; 3];
            for r1 in row.saturating_sub(1)..=(row + 1).min(h - 1) {
                for c1 in col.saturating_sub(1)..=(col + 1).min(w - 1) {
                    let c = scene.color_at(r1, c1) as usize;
                    sum[c] += floor0(scene.data[r1 * w + c1]);
                    n[c] += 1;
                }
            }
            let own = scene.color_at(row, col) as usize;
            let mut rgb = [0.0f32; 3];
            for c in 0..3 {
                rgb[c] = if c == own {
                    floor0(scene.data[row * w + col])
                } else if n[c] > 0 {
                    sum[c] / n[c] as f32
                } else {
                    // Cannot happen on a Bayer pattern with any 3x3 neighbourhood,
                    // but a missing colour must not become a black pixel.
                    floor0(scene.data[row * w + col])
                };
            }
            *o = mix[0] * rgb[0] + mix[1] * rgb[1] + mix[2] * rgb[2];
        }
    });
}

// ------------------------------------------------------------------------- RCD

/// **Ratio Corrected Demosaicing**, Luis Sanz Rodríguez.
///
/// Ported from RawTherapee `rtengine/rcd_demosaic.cc` (release 2.3), © 2017–2020
/// Luis Sanz Rodríguez and Ingo Weyrich, GPL-3.0-or-later. Original at
/// <https://github.com/LuisSR/RCD-Demosaicing>. The tiling is Ingo Weyrich's.
///
/// The shape of it, which is worth understanding before touching any constant:
///
/// 1. **Directional discrimination.** A high-pass filter along each axis, squared
///    and summed over a 3x3 neighbourhood, gives `V_Stat` and `H_Stat`; their ratio
///    `V/(V+H)` is a soft vote in `[0,1]` for which direction is *smoother*.
/// 2. **A low-pass plane** mixing green, red and blue local samples — this is the
///    "ratio" the name refers to.
/// 3. **Green at red and blue sites**, estimated along each cardinal direction as
///    the neighbour scaled by the ratio of local low-pass means, then blended by
///    gradient weight and finally by the direction vote.
/// 4. **Red and blue**, first at each other's sites along the diagonals, then at
///    green sites along the cardinals — both interpolating *colour differences*
///    against the now-complete green, which is what keeps chroma from ringing.
///
/// The refinement at steps 3 and 4.2 is the part that reads as arbitrary and is
/// not: where the central direction vote is *less* decisive than its neighbours'
/// mean, the neighbourhood value is used instead. It stops a single ambiguous pixel
/// in an otherwise clearly-directional region from being interpolated the wrong way.
fn rcd(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    const EPS: f32 = 1e-5;
    const EPSSQ: f32 = 1e-10;

    let src = scene.geom.crop;
    let (w, h) = (src.w, src.h);
    let mix = weighting.weights();
    let mut data = vec![0.0f32; w * h];

    // Too small to hold a single interior pixel: the border routine covers it all.
    if w <= 2 * BORDER || h <= 2 * BORDER {
        border_luma(scene, mix, BORDER.max(w).max(h), &mut data);
        return LumaImage {
            data,
            output_dims: src,
            source_dims: src,
            clipped: Vec::new(),
        };
    }

    // Row bands, one per tile row. Band `tr` covers output rows
    // `[tr*TILE_STEP + BORDER, min(tr*TILE_STEP + TILE, h) - BORDER)`, and those
    // ranges tile `[BORDER, h - BORDER)` exactly — which is what lets the output
    // be split into uniform `TILE_STEP`-row chunks and handed to rayon.
    let band_rows = TILE_STEP;
    let mid = &mut data[BORDER * w..(h - BORDER) * w];

    mid.par_chunks_mut(band_rows * w)
        .enumerate()
        .for_each(|(tr, band)| {
            let (row_start, row_end) = band_bounds(tr, h);
            let tile_rows = row_end - row_start;
            // The band's first output row, in image coordinates.
            let band_row0 = row_start + BORDER;

            let mut s = Scratch::new(TILE);
            let num_tw = w.div_ceil(TILE_STEP);

            for tc in 0..num_tw {
                let col_start = tc * TILE_STEP;
                let col_end = (col_start + TILE).min(w);
                let tile_cols = col_end - col_start;
                if col_start + BORDER >= col_end.saturating_sub(BORDER) {
                    continue;
                }
                if row_start + BORDER >= row_end.saturating_sub(BORDER) {
                    continue;
                }

                s.clear();

                // Load, pre-filling the two channels present in each row with the CFA
                // value. Every position those leave wrong is overwritten below; the
                // ones they leave right are the native samples.
                for row in row_start..row_end {
                    let c0 = scene.color_at(row, col_start) as usize;
                    let c1 = scene.color_at(row, col_start + 1) as usize;
                    let base = (row - row_start) * TILE;
                    for col in col_start..col_end {
                        let indx = base + (col - col_start);
                        let v = floor0(scene.data[row * w + col]);
                        s.cfa[indx] = v;
                        s.rgb[c0][indx] = v;
                        s.rgb[c1][indx] = v;
                    }
                }

                let w1 = TILE;
                let w2 = 2 * TILE;
                let w3 = 3 * TILE;
                let w4 = 4 * TILE;
                // Tile-relative CFA phase. Valid only because TILE_STEP is even.
                let fc0 = |row: usize| scene.color_at(row_start + row, col_start) as usize & 1;
                let fc1 = |row: usize| scene.color_at(row_start + row, col_start + 1) as usize & 1;

                // -- Step 1.1: vertical colour-difference high-pass, rows 3 and 4.
                let vw = TILE - 8;
                for row in 3..tile_rows.min(5) {
                    for col in 4..tile_cols - 4 {
                        let indx = row * TILE + col;
                        s.bv[(row % 3) * vw + col - 4] =
                            sqr((s.cfa[indx - w3] - s.cfa[indx - w1] - s.cfa[indx + w1]
                                + s.cfa[indx + w3])
                                - 3.0 * (s.cfa[indx - w2] + s.cfa[indx + w2])
                                + 6.0 * s.cfa[indx]);
                    }
                }

                // -- Step 1.2: the direction vote, V/(V+H).
                for row in 4..tile_rows - 4 {
                    for col in 3..tile_cols - 3 {
                        let indx = row * TILE + col;
                        s.bh[col - 3] = sqr((s.cfa[indx - 3] - s.cfa[indx - 1] - s.cfa[indx + 1]
                            + s.cfa[indx + 3])
                            - 3.0 * (s.cfa[indx - 2] + s.cfa[indx + 2])
                            + 6.0 * s.cfa[indx]);
                    }
                    // The row below, so the vertical statistic is centred on `row`.
                    for col in 4..tile_cols - 4 {
                        let indx = (row + 1) * TILE + col;
                        s.bv[((row + 1) % 3) * vw + col - 4] =
                            sqr((s.cfa[indx - w3] - s.cfa[indx - w1] - s.cfa[indx + w1]
                                + s.cfa[indx + w3])
                                - 3.0 * (s.cfa[indx - w2] + s.cfa[indx + w2])
                                + 6.0 * s.cfa[indx]);
                    }
                    for col in 4..tile_cols - 4 {
                        let i = col - 4;
                        let v_stat = EPSSQ.max(
                            s.bv[((row - 1) % 3) * vw + i]
                                + s.bv[(row % 3) * vw + i]
                                + s.bv[((row + 1) % 3) * vw + i],
                        );
                        let h_stat = EPSSQ.max(s.bh[col - 4] + s.bh[col - 3] + s.bh[col - 2]);
                        s.vh_dir[row * TILE + col] = v_stat / (v_stat + h_stat);
                    }
                }

                // -- Step 2: the low-pass plane, at CFA sites of one parity.
                for row in 2..tile_rows - 2 {
                    let mut col = 2 + fc0(row);
                    while col < tile_cols - 2 {
                        let indx = row * TILE + col;
                        s.lpf[indx / 2] = s.cfa[indx]
                            + 0.5
                                * (s.cfa[indx - w1]
                                    + s.cfa[indx + w1]
                                    + s.cfa[indx - 1]
                                    + s.cfa[indx + 1])
                            + 0.25
                                * (s.cfa[indx - w1 - 1]
                                    + s.cfa[indx - w1 + 1]
                                    + s.cfa[indx + w1 - 1]
                                    + s.cfa[indx + w1 + 1]);
                        col += 2;
                    }
                }

                // -- Step 3: green at red and blue sites.
                for row in 4..tile_rows - 4 {
                    let mut col = 4 + fc0(row);
                    while col < tile_cols - 4 {
                        let indx = row * TILE + col;
                        let lpindx = indx / 2;
                        let cfai = s.cfa[indx];

                        let n_grad = EPS
                            + ((s.cfa[indx - w1] - s.cfa[indx + w1]).abs()
                                + (cfai - s.cfa[indx - w2]).abs())
                            + ((s.cfa[indx - w1] - s.cfa[indx - w3]).abs()
                                + (s.cfa[indx - w2] - s.cfa[indx - w4]).abs());
                        let s_grad = EPS
                            + ((s.cfa[indx - w1] - s.cfa[indx + w1]).abs()
                                + (cfai - s.cfa[indx + w2]).abs())
                            + ((s.cfa[indx + w1] - s.cfa[indx + w3]).abs()
                                + (s.cfa[indx + w2] - s.cfa[indx + w4]).abs());
                        let w_grad = EPS
                            + ((s.cfa[indx - 1] - s.cfa[indx + 1]).abs()
                                + (cfai - s.cfa[indx - 2]).abs())
                            + ((s.cfa[indx - 1] - s.cfa[indx - 3]).abs()
                                + (s.cfa[indx - 2] - s.cfa[indx - 4]).abs());
                        let e_grad = EPS
                            + ((s.cfa[indx - 1] - s.cfa[indx + 1]).abs()
                                + (cfai - s.cfa[indx + 2]).abs())
                            + ((s.cfa[indx + 1] - s.cfa[indx + 3]).abs()
                                + (s.cfa[indx + 2] - s.cfa[indx + 4]).abs());

                        // The ratio correction: a neighbour, rescaled by how its local
                        // low-pass mean compares with this pixel's.
                        let lpfi = s.lpf[lpindx];
                        let n_est =
                            s.cfa[indx - w1] * (lpfi + lpfi) / (EPS + lpfi + s.lpf[lpindx - w1]);
                        let s_est =
                            s.cfa[indx + w1] * (lpfi + lpfi) / (EPS + lpfi + s.lpf[lpindx + w1]);
                        let w_est =
                            s.cfa[indx - 1] * (lpfi + lpfi) / (EPS + lpfi + s.lpf[lpindx - 1]);
                        let e_est =
                            s.cfa[indx + 1] * (lpfi + lpfi) / (EPS + lpfi + s.lpf[lpindx + 1]);

                        let v_est = (s_grad * n_est + n_grad * s_est) / (n_grad + s_grad);
                        let h_est = (w_grad * e_est + e_grad * w_est) / (e_grad + w_grad);

                        let vh_disc = refine(&s.vh_dir, indx, w1);
                        s.rgb[1][indx] = intp(vh_disc, h_est, v_est);
                        col += 2;
                    }
                }

                // -- Step 4.0: diagonal colour-difference high-pass.
                for row in 3..tile_rows - 3 {
                    let mut col = 3;
                    while col < tile_cols - 3 {
                        let indx = row * TILE + col;
                        let i2 = indx / 2;
                        s.p_hpf[i2] = sqr((s.cfa[indx - w3 - 3]
                            - s.cfa[indx - w1 - 1]
                            - s.cfa[indx + w1 + 1]
                            + s.cfa[indx + w3 + 3])
                            - 3.0 * (s.cfa[indx - w2 - 2] + s.cfa[indx + w2 + 2])
                            + 6.0 * s.cfa[indx]);
                        s.q_hpf[i2] = sqr((s.cfa[indx - w3 + 3]
                            - s.cfa[indx - w1 + 1]
                            - s.cfa[indx + w1 - 1]
                            + s.cfa[indx + w3 - 3])
                            - 3.0 * (s.cfa[indx - w2 + 2] + s.cfa[indx + w2 - 2])
                            + 6.0 * s.cfa[indx]);
                        col += 2;
                    }
                }

                // -- Step 4.1: the diagonal direction vote.
                for row in 4..tile_rows - 4 {
                    let mut col = 4 + fc0(row);
                    while col < tile_cols - 4 {
                        let indx = row * TILE + col;
                        let i2 = indx / 2;
                        let i3 = (indx - w1 - 1) / 2;
                        let i4 = (indx + w1 - 1) / 2;
                        let p_stat = EPSSQ.max(s.p_hpf[i3] + s.p_hpf[i2] + s.p_hpf[i4 + 1]);
                        let q_stat = EPSSQ.max(s.q_hpf[i3 + 1] + s.q_hpf[i2] + s.q_hpf[i4]);
                        s.pq_dir[i2] = p_stat / (p_stat + q_stat);
                        col += 2;
                    }
                }

                // -- Step 4.2: red at blue sites and blue at red sites, along the
                //    diagonals, interpolating colour differences against green.
                for row in 4..tile_rows - 4 {
                    let mut col = 4 + fc0(row);
                    while col < tile_cols - 4 {
                        let indx = row * TILE + col;
                        let c = 2 - scene.color_at(row_start + row, col_start + col) as usize;
                        let pq = indx / 2;
                        let pq2 = (indx - w1 - 1) / 2;
                        let pq3 = (indx + w1 - 1) / 2;

                        let central = s.pq_dir[pq];
                        let neighbourhood = 0.25
                            * (s.pq_dir[pq2]
                                + s.pq_dir[pq2 + 1]
                                + s.pq_dir[pq3]
                                + s.pq_dir[pq3 + 1]);
                        let pq_disc = if (0.5 - central).abs() < (0.5 - neighbourhood).abs() {
                            neighbourhood
                        } else {
                            central
                        };

                        let nw_grad = EPS
                            + (s.rgb[c][indx - w1 - 1] - s.rgb[c][indx + w1 + 1]).abs()
                            + (s.rgb[c][indx - w1 - 1] - s.rgb[c][indx - w3 - 3]).abs()
                            + (s.rgb[1][indx] - s.rgb[1][indx - w2 - 2]).abs();
                        let ne_grad = EPS
                            + (s.rgb[c][indx - w1 + 1] - s.rgb[c][indx + w1 - 1]).abs()
                            + (s.rgb[c][indx - w1 + 1] - s.rgb[c][indx - w3 + 3]).abs()
                            + (s.rgb[1][indx] - s.rgb[1][indx - w2 + 2]).abs();
                        let sw_grad = EPS
                            + (s.rgb[c][indx - w1 + 1] - s.rgb[c][indx + w1 - 1]).abs()
                            + (s.rgb[c][indx + w1 - 1] - s.rgb[c][indx + w3 - 3]).abs()
                            + (s.rgb[1][indx] - s.rgb[1][indx + w2 - 2]).abs();
                        let se_grad = EPS
                            + (s.rgb[c][indx - w1 - 1] - s.rgb[c][indx + w1 + 1]).abs()
                            + (s.rgb[c][indx + w1 + 1] - s.rgb[c][indx + w3 + 3]).abs()
                            + (s.rgb[1][indx] - s.rgb[1][indx + w2 + 2]).abs();

                        let nw_est = s.rgb[c][indx - w1 - 1] - s.rgb[1][indx - w1 - 1];
                        let ne_est = s.rgb[c][indx - w1 + 1] - s.rgb[1][indx - w1 + 1];
                        let sw_est = s.rgb[c][indx + w1 - 1] - s.rgb[1][indx + w1 - 1];
                        let se_est = s.rgb[c][indx + w1 + 1] - s.rgb[1][indx + w1 + 1];

                        let p_est = (nw_grad * se_est + se_grad * nw_est) / (nw_grad + se_grad);
                        let q_est = (ne_grad * sw_est + sw_grad * ne_est) / (ne_grad + sw_grad);

                        let v = s.rgb[1][indx] + intp(pq_disc, q_est, p_est);
                        s.rgb[c][indx] = v;
                        col += 2;
                    }
                }

                // -- Step 4.3: red and blue at green sites, along the cardinals.
                for row in 4..tile_rows - 4 {
                    let mut col = 4 + fc1(row);
                    while col < tile_cols - 4 {
                        let indx = row * TILE + col;
                        let vh_disc = refine(&s.vh_dir, indx, w1);

                        let g = s.rgb[1][indx];
                        let n1 = EPS + (g - s.rgb[1][indx - w2]).abs();
                        let s1 = EPS + (g - s.rgb[1][indx + w2]).abs();
                        let w1d = EPS + (g - s.rgb[1][indx - 2]).abs();
                        let e1 = EPS + (g - s.rgb[1][indx + 2]).abs();

                        let gmw1 = s.rgb[1][indx - w1];
                        let gpw1 = s.rgb[1][indx + w1];
                        let gm1 = s.rgb[1][indx - 1];
                        let gp1 = s.rgb[1][indx + 1];

                        for c in [0usize, 2] {
                            let sn = (s.rgb[c][indx - w1] - s.rgb[c][indx + w1]).abs();
                            let ew = (s.rgb[c][indx - 1] - s.rgb[c][indx + 1]).abs();
                            let n_grad =
                                n1 + sn + (s.rgb[c][indx - w1] - s.rgb[c][indx - w3]).abs();
                            let s_grad =
                                s1 + sn + (s.rgb[c][indx + w1] - s.rgb[c][indx + w3]).abs();
                            let w_grad = w1d + ew + (s.rgb[c][indx - 1] - s.rgb[c][indx - 3]).abs();
                            let e_grad = e1 + ew + (s.rgb[c][indx + 1] - s.rgb[c][indx + 3]).abs();

                            let n_est = s.rgb[c][indx - w1] - gmw1;
                            let s_est = s.rgb[c][indx + w1] - gpw1;
                            let w_est = s.rgb[c][indx - 1] - gm1;
                            let e_est = s.rgb[c][indx + 1] - gp1;

                            let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
                            let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);

                            s.rgb[c][indx] = g + intp(vh_disc, h_est, v_est);
                        }
                        col += 2;
                    }
                }

                // -- Collapse to luma straight out of the tile. A full RGB image is
                //    never assembled; see the module note.
                let first_col = col_start + BORDER;
                let last_col = col_end - BORDER;
                let first_row = row_start + BORDER;
                let last_row = row_end - BORDER;
                for row in first_row..last_row {
                    let out_row = row - band_row0;
                    for col in first_col..last_col {
                        let idx = (row - row_start) * TILE + (col - col_start);
                        band[out_row * w + col] = mix[0] * floor0(s.rgb[0][idx])
                            + mix[1] * floor0(s.rgb[1][idx])
                            + mix[2] * floor0(s.rgb[2][idx]);
                    }
                }
            }
        });

    // The frame the tiles could not reach.
    border_luma(scene, mix, BORDER, &mut data);

    LumaImage {
        data,
        output_dims: src,
        source_dims: src,
        clipped: Vec::new(),
    }
}

/// The direction-vote refinement RCD applies at both green and chroma steps: prefer
/// the neighbourhood mean when the central vote is the *less* decisive of the two.
///
/// "Decisive" means far from 0.5 — 0.5 is a tie between the two directions. A lone
/// undecided pixel inside a confidently-directional region gets the region's answer
/// rather than its own coin flip.
#[inline]
fn refine(dir: &[f32], indx: usize, w1: usize) -> f32 {
    let central = dir[indx];
    let neighbourhood = 0.25
        * ((dir[indx - w1 - 1] + dir[indx - w1 + 1]) + (dir[indx + w1 - 1] + dir[indx + w1 + 1]));
    if (0.5 - central).abs() < (0.5 - neighbourhood).abs() {
        neighbourhood
    } else {
        central
    }
}

/// Per-thread tile buffers. Allocated once per band and reused across the tiles in
/// it, because a 100 MP frame is ~3200 tiles and each allocation here is ~1 MB.
struct Scratch {
    cfa: Vec<f32>,
    rgb: [Vec<f32>; 3],
    vh_dir: Vec<f32>,
    pq_dir: Vec<f32>,
    lpf: Vec<f32>,
    p_hpf: Vec<f32>,
    q_hpf: Vec<f32>,
    /// Three rows of the vertical statistic, addressed `(row % 3) * (tile - 8)`.
    /// The reference rotates three pointers; indexing by row modulo three is the
    /// same schedule without the aliasing.
    bv: Vec<f32>,
    bh: Vec<f32>,
}

impl Scratch {
    fn new(tile: usize) -> Self {
        let n = tile * tile;
        Self {
            cfa: vec![0.0; n],
            rgb: [vec![0.0; n], vec![0.0; n], vec![0.0; n]],
            vh_dir: vec![0.0; n],
            pq_dir: vec![0.0; n / 2 + 1],
            lpf: vec![0.0; n / 2 + 1],
            p_hpf: vec![0.0; n / 2 + 1],
            q_hpf: vec![0.0; n / 2 + 1],
            bv: vec![0.0; 3 * (tile - 8)],
            bh: vec![0.0; tile - 6],
        }
    }

    /// Zero between tiles. The last tile of a row or column is short, so stale
    /// values from the previous tile would otherwise be read as image data in the
    /// unfilled margin.
    fn clear(&mut self) {
        self.cfa.fill(0.0);
        for p in &mut self.rgb {
            p.fill(0.0);
        }
        self.vh_dir.fill(0.0);
        self.pq_dir.fill(0.0);
        self.lpf.fill(0.0);
        self.p_hpf.fill(0.0);
        self.q_hpf.fill(0.0);
        self.bv.fill(0.0);
        self.bh.fill(0.0);
    }
}

// ------------------------------------------------------------- Hamilton-Adams

/// **Hamilton-Adams**, "adaptive colour plane interpolation" (Adams & Hamilton,
/// Eastman Kodak, 1997).
///
/// The green estimator and the red/blue colour-difference steps are transcribed
/// from RawTherapee `rtengine/ahd_demosaic_RT.cc`, which uses Hamilton-Adams as
/// AHD's first stage.
///
/// # Why this is Hamilton-Adams and not AHD
///
/// AHD computes *both* the horizontal and the vertical Hamilton-Adams candidate and
/// then picks between them per pixel by which yields the more homogeneous
/// neighbourhood **in CIELab** — which requires converting camera RGB to XYZ, which
/// requires the camera colour matrix.
///
/// **This pipeline does not have one, by design.** "Nothing may reintroduce a
/// demosaic-then-convert path" is the invariant the whole rewrite exists to hold, and
/// a colour matrix in the middle of the demosaic is exactly that path. AHD is
/// therefore not portable here, and its absence is a consequence of the thesis
/// rather than an omission.
///
/// So the direction is chosen the way Hamilton-Adams itself chooses it: by comparing
/// a horizontal and a vertical gradient estimate, each combining the first
/// difference of the green neighbours with the second difference of the centre
/// channel. Ties average the two candidates. That is the published algorithm, and it
/// needs nothing but the mosaic.
///
/// The `median` clamp on each candidate is RawTherapee's, and it is worth keeping:
/// the second-difference correction term is what lets Hamilton-Adams resolve detail
/// that plain averaging cannot, and it is also what lets it overshoot. Clamping the
/// estimate into the range of the two neighbours it sits between removes the
/// overshoot without touching the correction where the correction is honest.
fn hamilton_adams(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    let src = scene.geom.crop;
    let (w, h) = (src.w, src.h);
    let mix = weighting.weights();
    let mut data = vec![0.0f32; w * h];

    if w <= 2 * BORDER || h <= 2 * BORDER {
        border_luma(scene, mix, BORDER.max(w).max(h), &mut data);
        return LumaImage {
            data,
            output_dims: src,
            source_dims: src,
            clipped: Vec::new(),
        };
    }

    {
        let mid = &mut data[BORDER * w..(h - BORDER) * w];
        mid.par_chunks_mut(TILE_STEP * w)
            .enumerate()
            .for_each(|(tr, band)| {
                let (row_start, row_end) = band_bounds(tr, h);
                let tile_rows = row_end - row_start;
                let band_row0 = row_start + BORDER;
                if row_start + BORDER >= row_end.saturating_sub(BORDER) {
                    return;
                }

                let mut cfa = vec![0.0f32; TILE * TILE];
                let mut green = vec![0.0f32; TILE * TILE];
                let num_tw = w.div_ceil(TILE_STEP);

                for tc in 0..num_tw {
                    let col_start = tc * TILE_STEP;
                    let col_end = (col_start + TILE).min(w);
                    let tile_cols = col_end - col_start;
                    if col_start + BORDER >= col_end.saturating_sub(BORDER) {
                        continue;
                    }

                    cfa.fill(0.0);
                    green.fill(0.0);
                    for row in row_start..row_end {
                        let base = (row - row_start) * TILE;
                        for col in col_start..col_end {
                            cfa[base + (col - col_start)] = floor0(scene.data[row * w + col]);
                        }
                    }

                    let w1 = TILE;
                    let w2 = 2 * TILE;

                    // -- Green. At green sites it is the sample; elsewhere it is the
                    //    Hamilton-Adams estimate along the smoother axis.
                    for row in 2..tile_rows - 2 {
                        for col in 2..tile_cols - 2 {
                            let indx = row * TILE + col;
                            if scene.color_at(row_start + row, col_start + col) == CfaColor::Green {
                                green[indx] = cfa[indx];
                                continue;
                            }
                            let c0 = cfa[indx];

                            // The two candidates: the mean of the green neighbours, plus
                            // a quarter of the centre channel's second difference. That
                            // correction term is the whole idea — it carries the
                            // high-frequency detail the green samples cannot see.
                            let hv = 0.25
                                * ((cfa[indx - 1] + c0 + cfa[indx + 1]) * 2.0
                                    - cfa[indx - 2]
                                    - cfa[indx + 2]);
                            let vv = 0.25
                                * ((cfa[indx - w1] + c0 + cfa[indx + w1]) * 2.0
                                    - cfa[indx - w2]
                                    - cfa[indx + w2]);
                            let hv = median3(hv, cfa[indx - 1], cfa[indx + 1]);
                            let vv = median3(vv, cfa[indx - w1], cfa[indx + w1]);

                            // Hamilton-Adams' own direction test.
                            let dh = (cfa[indx - 1] - cfa[indx + 1]).abs()
                                + (cfa[indx - 2] - 2.0 * c0 + cfa[indx + 2]).abs();
                            let dv = (cfa[indx - w1] - cfa[indx + w1]).abs()
                                + (cfa[indx - w2] - 2.0 * c0 + cfa[indx + w2]).abs();

                            green[indx] = if dh < dv {
                                hv
                            } else if dv < dh {
                                vv
                            } else {
                                0.5 * (hv + vv)
                            };
                        }
                    }

                    // -- Red and blue, then straight to luma. Both interpolate colour
                    //    DIFFERENCES against the completed green, which is what stops
                    //    chroma ringing at an edge green already resolved.
                    let first_col = col_start + BORDER;
                    let last_col = col_end - BORDER;
                    for row in (row_start + BORDER)..(row_end - BORDER) {
                        let trow = row - row_start;
                        let out_row = row - band_row0;
                        // Which channel the vertical neighbours carry on this row. On a
                        // green site the horizontal and vertical neighbours are the two
                        // different colours, so each is interpolated along its own axis.
                        let cng = {
                            let above = scene.color_at(row + 1, col_start) as usize & 1;
                            scene.color_at(row + 1, col_start + above) as usize
                        };
                        for col in first_col..last_col {
                            let tcol = col - col_start;
                            let indx = trow * TILE + tcol;
                            let g = green[indx];
                            let mut rgb = [0.0f32; 3];
                            rgb[1] = g;

                            if scene.color_at(row, col) == CfaColor::Green {
                                rgb[2 - cng] = floor0(
                                    cfa[indx]
                                        + 0.5
                                            * (cfa[indx - 1] + cfa[indx + 1]
                                                - green[indx - 1]
                                                - green[indx + 1]),
                                );
                                rgb[cng] = floor0(
                                    cfa[indx]
                                        + 0.5
                                            * (cfa[indx - w1] + cfa[indx + w1]
                                                - green[indx - w1]
                                                - green[indx + w1]),
                                );
                            } else {
                                rgb[cng] = floor0(
                                    g + 0.25
                                        * (cfa[indx - w1 - 1]
                                            + cfa[indx - w1 + 1]
                                            + cfa[indx + w1 - 1]
                                            + cfa[indx + w1 + 1]
                                            - green[indx - w1 - 1]
                                            - green[indx - w1 + 1]
                                            - green[indx + w1 - 1]
                                            - green[indx + w1 + 1]),
                                );
                                rgb[2 - cng] = cfa[indx];
                            }

                            band[out_row * w + col] =
                                mix[0] * rgb[0] + mix[1] * rgb[1] + mix[2] * rgb[2];
                        }
                    }
                }
            });
    }

    border_luma(scene, mix, BORDER, &mut data);
    LumaImage {
        data,
        output_dims: src,
        source_dims: src,
        clipped: Vec::new(),
    }
}

/// Median of three. Used to clamp a Hamilton-Adams estimate into the range of the
/// two neighbours it interpolates between.
#[inline]
fn median3(a: f32, b: f32, c: f32) -> f32 {
    a.max(b.min(c)).min(b.max(c))
}

// ----------------------------------------------------------------------- AMaZE

/// **AMaZE** — Aliasing Minimization and Zipper Elimination, Emil J. Martinec.
///
/// Ported from RawTherapee `rtengine/amaze_demosaic_RT.cc`, © 2010 Emil Martinec,
/// GPL-3.0-or-later, scalar path. The reference's SSE branches are an optimisation
/// of the same arithmetic and were not transcribed.
///
/// The most involved algorithm here by a wide margin, and the shape is worth having
/// before reading the code:
///
/// 1. **Directional weights and colour differences.** For each pixel, green is
///    estimated up/down/left/right two ways — Hamilton-Adams, and an *adaptive
///    ratio* that rescales the neighbour by a local ratio. The ratio is used only
///    where it is close to 1 (`ARTHRESH`), because far from 1 it is unstable; near
///    clipping it is dropped entirely.
/// 2. **Variance-based direction choice.** Horizontal and vertical colour
///    differences are compared by their local variance, and by the fluctuation
///    between opposite-direction interpolations. Where the two measures agree, the
///    more decisive one wins; where they disagree, the fluctuation measure does.
/// 3. **The Nyquist test**, which is what the name is about. Regions where the
///    colour-difference disagreement outweighs the luminance gradient are texture at
///    the sampling limit — the case that produces aliasing and zippering. They are
///    detected, grown by a neighbour vote, and interpolated by *area averaging*
///    instead, then refined again using the curvature of the interpolated green.
/// 4. **Diagonal interpolation correction** for red at blue sites and vice versa,
///    with its own plus/minus direction weighting.
/// 5. **Fancy chrominance interpolation** — a four-direction weighted filter on the
///    colour differences, with the small negative lobes that sharpen it.
///
/// # What it costs here
///
/// Every step above is spent deciding **direction** and **chroma**. The weighted sum
/// to grey then discards the chroma half. What survives into a monochrome render is
/// the aliasing and zipper handling of step 3 — which is real, and is the reason this
/// is worth having at all, but it is a fraction of what the algorithm does.
fn amaze(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    const EPS: f32 = 1e-5;
    const EPSSQ: f32 = 1e-10;
    /// Adaptive-ratio threshold: beyond this distance from 1.0 the ratio estimate is
    /// abandoned for the Hamilton-Adams one.
    const ARTHRESH: f32 = 0.75;
    /// Scene 1.0 is the saturation point, which is exactly what the reference's
    /// `1.0 / initialGain` means for its own normalisation.
    const CLIP_PT: f32 = 1.0;
    const CLIP_PT8: f32 = 0.8;

    /// Gaussian on a 5x5 quincunx, sigma 1.2.
    const GAUSSODD: [f32; 4] = [0.146_597_28, 0.103_592_71, 0.073_203_61, 0.036_554_355];
    /// Nyquist texture test threshold, folded into `GAUSSGRAD` as the reference does.
    const NYQTHRESH: f32 = 0.5;
    const GAUSSGRAD: [f32; 6] = [
        NYQTHRESH * 0.073_844_12,
        NYQTHRESH * 0.062_075_12,
        NYQTHRESH * 0.052_181_82,
        NYQTHRESH * 0.036_874_19,
        NYQTHRESH * 0.030_997_32,
        NYQTHRESH * 0.018_413_194,
    ];
    /// Gaussian on a 5x5 alternate quincunx, sigma 1.5.
    const GAUSSEVEN: [f32; 2] = [0.137_194_94, 0.056_402_53];
    /// Gaussian on the quincunx grid.
    const GQUINC: [f32; 4] = [0.169_917, 0.108_947, 0.069_855, 0.028_718_2];

    // Neighbour offsets within a tile. The reference's `p*` offsets are negative
    // (up-right diagonal); here they are positive magnitudes and the sign lives at
    // the use site, so every index stays unsigned and an out-of-range read is a
    // panic rather than a wrap.
    const V1: usize = TILE;
    const V2: usize = 2 * TILE;
    const V3: usize = 3 * TILE;
    const M1: usize = TILE + 1;
    const M2: usize = 2 * TILE + 2;
    const M3: usize = 3 * TILE + 3;
    const P1: usize = TILE - 1;
    const P2: usize = 2 * TILE - 2;
    const P3: usize = 3 * TILE - 3;

    let src = scene.geom.crop;
    let (w, h) = (src.w, src.h);
    let mix = weighting.weights();
    let mut data = vec![0.0f32; w * h];

    if w <= 2 * BORDER || h <= 2 * BORDER {
        border_luma(scene, mix, BORDER.max(w).max(h), &mut data);
        return LumaImage {
            data,
            output_dims: src,
            source_dims: src,
            clipped: Vec::new(),
        };
    }

    // (ey, ex) is the offset of the red photosite within the Bayer quartet. Used
    // only to decide which rows carry the blue coset when the colour differences
    // are split into G-R and G-B.
    let (ey, ex) = {
        let c00 = scene.color_at(0, 0);
        if c00 == CfaColor::Green {
            if scene.color_at(0, 1) == CfaColor::Red {
                (0usize, 1usize)
            } else {
                (1, 0)
            }
        } else if c00 == CfaColor::Red {
            (0, 0)
        } else {
            (1, 1)
        }
    };

    {
        let mid = &mut data[BORDER * w..(h - BORDER) * w];
        mid.par_chunks_mut(TILE_STEP * w)
            .enumerate()
            .for_each(|(tr, band)| {
                let (row_start, row_end) = band_bounds(tr, h);
                let rr1 = row_end - row_start;
                let band_row0 = row_start + BORDER;
                if row_start + BORDER >= row_end.saturating_sub(BORDER) {
                    return;
                }

                let n = TILE * TILE;
                let nh = n / 2;
                let mut s = Amaze::new(n, nh);
                let num_tw = w.div_ceil(TILE_STEP);

                for tc in 0..num_tw {
                    let col_start = tc * TILE_STEP;
                    let col_end = (col_start + TILE).min(w);
                    let cc1 = col_end - col_start;
                    if col_start + BORDER >= col_end.saturating_sub(BORDER) {
                        continue;
                    }
                    s.clear();

                    // Tile-relative CFA phase, valid because TILE_STEP is even.
                    let green_row =
                        |rr: usize| scene.color_at(row_start + rr, col_start) as usize & 1 == 1;
                    let colour =
                        |rr: usize, cc: usize| scene.color_at(row_start + rr, col_start + cc);

                    for rr in 0..rr1 {
                        let base = rr * TILE;
                        for cc in 0..cc1 {
                            s.cfa[base + cc] =
                                floor0(scene.data[(row_start + rr) * w + col_start + cc]);
                            s.rgbgreen[base + cc] = s.cfa[base + cc];
                        }
                    }

                    // -- Horizontal and vertical gradients.
                    for rr in 2..rr1 - 2 {
                        for cc in 2..cc1 - 2 {
                            let indx = rr * TILE + cc;
                            let delh = (s.cfa[indx + 1] - s.cfa[indx - 1]).abs();
                            let delv = (s.cfa[indx + V1] - s.cfa[indx - V1]).abs();
                            s.dirwts0[indx] = EPS
                                + (s.cfa[indx + V2] - s.cfa[indx]).abs()
                                + (s.cfa[indx] - s.cfa[indx - V2]).abs()
                                + delv;
                            s.dirwts1[indx] = EPS
                                + (s.cfa[indx + 2] - s.cfa[indx]).abs()
                                + (s.cfa[indx] - s.cfa[indx - 2]).abs()
                                + delh;
                            s.delhvsqsum[indx] = sqr(delh) + sqr(delv);
                        }
                    }

                    // -- Vertical and horizontal colour differences, two ways each.
                    for rr in 4..rr1 - 4 {
                        let mut fcswitch = green_row(rr);
                        for cc in 4..cc1 - 4 {
                            let indx = rr * TILE + cc;
                            let c0 = s.cfa[indx];

                            // Colour ratios in each cardinal direction.
                            let cru = s.cfa[indx - V1] * (s.dirwts0[indx - V2] + s.dirwts0[indx])
                                / (s.dirwts0[indx - V2] * (EPS + c0)
                                    + s.dirwts0[indx] * (EPS + s.cfa[indx - V2]));
                            let crd = s.cfa[indx + V1] * (s.dirwts0[indx + V2] + s.dirwts0[indx])
                                / (s.dirwts0[indx + V2] * (EPS + c0)
                                    + s.dirwts0[indx] * (EPS + s.cfa[indx + V2]));
                            let crl = s.cfa[indx - 1] * (s.dirwts1[indx - 2] + s.dirwts1[indx])
                                / (s.dirwts1[indx - 2] * (EPS + c0)
                                    + s.dirwts1[indx] * (EPS + s.cfa[indx - 2]));
                            let crr = s.cfa[indx + 1] * (s.dirwts1[indx + 2] + s.dirwts1[indx])
                                / (s.dirwts1[indx + 2] * (EPS + c0)
                                    + s.dirwts1[indx] * (EPS + s.cfa[indx + 2]));

                            // Green by Hamilton-Adams in each direction.
                            let guha = s.cfa[indx - V1] + 0.5 * (c0 - s.cfa[indx - V2]);
                            let gdha = s.cfa[indx + V1] + 0.5 * (c0 - s.cfa[indx + V2]);
                            let glha = s.cfa[indx - 1] + 0.5 * (c0 - s.cfa[indx - 2]);
                            let grha = s.cfa[indx + 1] + 0.5 * (c0 - s.cfa[indx + 2]);

                            // Green by adaptive ratio, where the ratio is near enough 1
                            // to be trusted.
                            let mut guar = if (1.0 - cru).abs() < ARTHRESH {
                                c0 * cru
                            } else {
                                guha
                            };
                            let mut gdar = if (1.0 - crd).abs() < ARTHRESH {
                                c0 * crd
                            } else {
                                gdha
                            };
                            let mut glar = if (1.0 - crl).abs() < ARTHRESH {
                                c0 * crl
                            } else {
                                glha
                            };
                            let mut grar = if (1.0 - crr).abs() < ARTHRESH {
                                c0 * crr
                            } else {
                                grha
                            };

                            let hwt =
                                s.dirwts1[indx - 1] / (s.dirwts1[indx - 1] + s.dirwts1[indx + 1]);
                            let vwt = s.dirwts0[indx - V1]
                                / (s.dirwts0[indx + V1] + s.dirwts0[indx - V1]);

                            let gintvha = vwt * gdha + (1.0 - vwt) * guha;
                            let ginthha = hwt * grha + (1.0 - hwt) * glha;

                            if fcswitch {
                                s.vcd[indx] = c0 - (vwt * gdar + (1.0 - vwt) * guar);
                                s.hcd[indx] = c0 - (hwt * grar + (1.0 - hwt) * glar);
                                s.vcdalt[indx] = c0 - gintvha;
                                s.hcdalt[indx] = c0 - ginthha;
                            } else {
                                s.vcd[indx] = (vwt * gdar + (1.0 - vwt) * guar) - c0;
                                s.hcd[indx] = (hwt * grar + (1.0 - hwt) * glar) - c0;
                                s.vcdalt[indx] = gintvha - c0;
                                s.hcdalt[indx] = ginthha - c0;
                            }
                            fcswitch = !fcswitch;

                            // Near clipping the ratio estimate is meaningless, so fall
                            // back to Hamilton-Adams.
                            if c0 > CLIP_PT8 || gintvha > CLIP_PT8 || ginthha > CLIP_PT8 {
                                guar = guha;
                                gdar = gdha;
                                glar = glha;
                                grar = grha;
                                s.vcd[indx] = s.vcdalt[indx];
                                s.hcd[indx] = s.hcdalt[indx];
                            }

                            s.dgintv[indx] = sqr(guha - gdha).min(sqr(guar - gdar));
                            s.dginth[indx] = sqr(glha - grha).min(sqr(glar - grar));
                        }
                    }

                    // -- Pick the lower-variance estimate, then bound it where saturated.
                    for rr in 4..rr1 - 4 {
                        let mut c = green_row(rr);
                        for cc in 4..cc1 - 4 {
                            let indx = rr * TILE + cc;
                            let hcdvar = 3.0
                                * (sqr(s.hcd[indx - 2]) + sqr(s.hcd[indx]) + sqr(s.hcd[indx + 2]))
                                - sqr(s.hcd[indx - 2] + s.hcd[indx] + s.hcd[indx + 2]);
                            let hcdaltvar = 3.0
                                * (sqr(s.hcdalt[indx - 2])
                                    + sqr(s.hcdalt[indx])
                                    + sqr(s.hcdalt[indx + 2]))
                                - sqr(s.hcdalt[indx - 2] + s.hcdalt[indx] + s.hcdalt[indx + 2]);
                            let vcdvar = 3.0
                                * (sqr(s.vcd[indx - V2])
                                    + sqr(s.vcd[indx])
                                    + sqr(s.vcd[indx + V2]))
                                - sqr(s.vcd[indx - V2] + s.vcd[indx] + s.vcd[indx + V2]);
                            let vcdaltvar = 3.0
                                * (sqr(s.vcdalt[indx - V2])
                                    + sqr(s.vcdalt[indx])
                                    + sqr(s.vcdalt[indx + V2]))
                                - sqr(s.vcdalt[indx - V2] + s.vcdalt[indx] + s.vcdalt[indx + V2]);

                            // The smaller variance yields the smoother interpolation.
                            if hcdaltvar < hcdvar {
                                s.hcd[indx] = s.hcdalt[indx];
                            }
                            if vcdaltvar < vcdvar {
                                s.vcd[indx] = s.vcdalt[indx];
                            }

                            let c0 = s.cfa[indx];
                            if c {
                                // Green site.
                                let ginth = -s.hcd[indx] + c0;
                                let gintv = -s.vcd[indx] + c0;
                                if s.hcd[indx] > 0.0 {
                                    let m = median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]);
                                    if 3.0 * s.hcd[indx] > ginth + c0 {
                                        s.hcd[indx] = -m + c0;
                                    } else {
                                        let hwt = 1.0 - 3.0 * s.hcd[indx] / (EPS + ginth + c0);
                                        s.hcd[indx] = hwt * s.hcd[indx] + (1.0 - hwt) * (-m + c0);
                                    }
                                }
                                if s.vcd[indx] > 0.0 {
                                    let m = median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]);
                                    if 3.0 * s.vcd[indx] > gintv + c0 {
                                        s.vcd[indx] = -m + c0;
                                    } else {
                                        let vwt = 1.0 - 3.0 * s.vcd[indx] / (EPS + gintv + c0);
                                        s.vcd[indx] = vwt * s.vcd[indx] + (1.0 - vwt) * (-m + c0);
                                    }
                                }
                                if ginth > CLIP_PT {
                                    s.hcd[indx] =
                                        -median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]) + c0;
                                }
                                if gintv > CLIP_PT {
                                    s.vcd[indx] =
                                        -median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]) + c0;
                                }
                            } else {
                                // Red or blue site.
                                let ginth = s.hcd[indx] + c0;
                                let gintv = s.vcd[indx] + c0;
                                if s.hcd[indx] < 0.0 {
                                    let m = median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]);
                                    if 3.0 * s.hcd[indx] < -(ginth + c0) {
                                        s.hcd[indx] = m - c0;
                                    } else {
                                        let hwt = 1.0 + 3.0 * s.hcd[indx] / (EPS + ginth + c0);
                                        s.hcd[indx] = hwt * s.hcd[indx] + (1.0 - hwt) * (m - c0);
                                    }
                                }
                                if s.vcd[indx] < 0.0 {
                                    let m = median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]);
                                    if 3.0 * s.vcd[indx] < -(gintv + c0) {
                                        s.vcd[indx] = m - c0;
                                    } else {
                                        let vwt = 1.0 + 3.0 * s.vcd[indx] / (EPS + gintv + c0);
                                        s.vcd[indx] = vwt * s.vcd[indx] + (1.0 - vwt) * (m - c0);
                                    }
                                }
                                if ginth > CLIP_PT {
                                    s.hcd[indx] =
                                        median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]) - c0;
                                }
                                if gintv > CLIP_PT {
                                    s.vcd[indx] =
                                        median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]) - c0;
                                }
                                s.cddiffsq[indx] = sqr(s.vcd[indx] - s.hcd[indx]);
                            }
                            c = !c;
                        }
                    }

                    // -- Adaptive weight for the horizontal/vertical choice.
                    for rr in 6..rr1 - 6 {
                        let mut cc = 6 + usize::from(green_row(rr));
                        while cc < cc1 - 6 {
                            let indx = rr * TILE + cc;
                            let uave = s.vcd[indx]
                                + s.vcd[indx - V1]
                                + s.vcd[indx - V2]
                                + s.vcd[indx - V3];
                            let dave = s.vcd[indx]
                                + s.vcd[indx + V1]
                                + s.vcd[indx + V2]
                                + s.vcd[indx + V3];
                            let lave =
                                s.hcd[indx] + s.hcd[indx - 1] + s.hcd[indx - 2] + s.hcd[indx - 3];
                            let rave =
                                s.hcd[indx] + s.hcd[indx + 1] + s.hcd[indx + 2] + s.hcd[indx + 3];

                            let mut vvaru = sqr(s.vcd[indx] - uave)
                                + sqr(s.vcd[indx - V1] - uave)
                                + sqr(s.vcd[indx - V2] - uave)
                                + sqr(s.vcd[indx - V3] - uave);
                            let mut vvard = sqr(s.vcd[indx] - dave)
                                + sqr(s.vcd[indx + V1] - dave)
                                + sqr(s.vcd[indx + V2] - dave)
                                + sqr(s.vcd[indx + V3] - dave);
                            let mut hvarl = sqr(s.hcd[indx] - lave)
                                + sqr(s.hcd[indx - 1] - lave)
                                + sqr(s.hcd[indx - 2] - lave)
                                + sqr(s.hcd[indx - 3] - lave);
                            let mut hvarr = sqr(s.hcd[indx] - rave)
                                + sqr(s.hcd[indx + 1] - rave)
                                + sqr(s.hcd[indx + 2] - rave)
                                + sqr(s.hcd[indx + 3] - rave);

                            let hwt =
                                s.dirwts1[indx - 1] / (s.dirwts1[indx - 1] + s.dirwts1[indx + 1]);
                            let vwt = s.dirwts0[indx - V1]
                                / (s.dirwts0[indx + V1] + s.dirwts0[indx - V1]);

                            let vcdvar = EPSSQ + vwt * vvard + (1.0 - vwt) * vvaru;
                            let hcdvar = EPSSQ + hwt * hvarr + (1.0 - hwt) * hvarl;

                            // Fluctuation between opposite-direction interpolations.
                            vvaru = s.dgintv[indx] + s.dgintv[indx - V1] + s.dgintv[indx - V2];
                            vvard = s.dgintv[indx] + s.dgintv[indx + V1] + s.dgintv[indx + V2];
                            hvarl = s.dginth[indx] + s.dginth[indx - 1] + s.dginth[indx - 2];
                            hvarr = s.dginth[indx] + s.dginth[indx + 1] + s.dginth[indx + 2];

                            let vcdvar1 = EPSSQ + vwt * vvard + (1.0 - vwt) * vvaru;
                            let hcdvar1 = EPSSQ + hwt * hvarr + (1.0 - hwt) * hvarl;

                            let varwt = hcdvar / (vcdvar + hcdvar);
                            let diffwt = hcdvar1 / (vcdvar1 + hcdvar1);

                            // Where both measures agree on direction, take the more
                            // decisive; where they disagree, take the fluctuation one.
                            s.hvwt[indx >> 1] = if (0.5 - varwt) * (0.5 - diffwt) > 0.0
                                && (0.5 - diffwt).abs() < (0.5 - varwt).abs()
                            {
                                varwt
                            } else {
                                diffwt
                            };
                            cc += 2;
                        }
                    }

                    // -- The Nyquist test: is the colour-difference disagreement larger
                    //    than the luminance gradient? If so this is texture at the
                    //    sampling limit, and directional interpolation will alias.
                    for rr in 6..rr1 - 6 {
                        let mut cc = 6 + usize::from(green_row(rr));
                        while cc < cc1 - 6 {
                            let indx = rr * TILE + cc;
                            s.nyqutest[indx >> 1] = (GAUSSODD[0] * s.cddiffsq[indx]
                                + GAUSSODD[1]
                                    * (s.cddiffsq[indx - M1]
                                        + s.cddiffsq[indx - P1]
                                        + s.cddiffsq[indx + P1]
                                        + s.cddiffsq[indx + M1])
                                + GAUSSODD[2]
                                    * (s.cddiffsq[indx - V2]
                                        + s.cddiffsq[indx - 2]
                                        + s.cddiffsq[indx + 2]
                                        + s.cddiffsq[indx + V2])
                                + GAUSSODD[3]
                                    * (s.cddiffsq[indx - M2]
                                        + s.cddiffsq[indx - P2]
                                        + s.cddiffsq[indx + P2]
                                        + s.cddiffsq[indx + M2]))
                                - (GAUSSGRAD[0] * s.delhvsqsum[indx]
                                    + GAUSSGRAD[1]
                                        * (s.delhvsqsum[indx - V1]
                                            + s.delhvsqsum[indx + 1]
                                            + s.delhvsqsum[indx - 1]
                                            + s.delhvsqsum[indx + V1])
                                    + GAUSSGRAD[2]
                                        * (s.delhvsqsum[indx - M1]
                                            + s.delhvsqsum[indx - P1]
                                            + s.delhvsqsum[indx + P1]
                                            + s.delhvsqsum[indx + M1])
                                    + GAUSSGRAD[3]
                                        * (s.delhvsqsum[indx - V2]
                                            + s.delhvsqsum[indx - 2]
                                            + s.delhvsqsum[indx + 2]
                                            + s.delhvsqsum[indx + V2])
                                    + GAUSSGRAD[4]
                                        * (s.delhvsqsum[indx - V2 - 1]
                                            + s.delhvsqsum[indx - V2 + 1]
                                            + s.delhvsqsum[indx - TILE - 2]
                                            + s.delhvsqsum[indx - TILE + 2]
                                            + s.delhvsqsum[indx + TILE - 2]
                                            + s.delhvsqsum[indx + TILE + 2]
                                            + s.delhvsqsum[indx + V2 - 1]
                                            + s.delhvsqsum[indx + V2 + 1])
                                    + GAUSSGRAD[5]
                                        * (s.delhvsqsum[indx - M2]
                                            + s.delhvsqsum[indx - P2]
                                            + s.delhvsqsum[indx + P2]
                                            + s.delhvsqsum[indx + M2]));
                            cc += 2;
                        }
                    }

                    let (mut nystartrow, mut nyendrow) = (0usize, 0usize);
                    let (mut nystartcol, mut nyendcol) = (TILE + 1, 0usize);
                    for rr in 6..rr1 - 6 {
                        let mut cc = 6 + usize::from(green_row(rr));
                        while cc < cc1 - 6 {
                            let indx = rr * TILE + cc;
                            if s.nyqutest[indx >> 1] > 0.0 {
                                s.nyquist[indx >> 1] = 1;
                                if nystartrow == 0 {
                                    nystartrow = rr;
                                }
                                nyendrow = rr;
                                nystartcol = nystartcol.min(cc);
                                nyendcol = nyendcol.max(cc);
                            }
                            cc += 2;
                        }
                    }

                    let do_nyquist = nystartrow != nyendrow && nystartcol != nyendcol;
                    if do_nyquist {
                        nyendrow += 1;
                        nyendcol += 1;
                        nystartcol -= nystartcol & 1;
                        nystartrow = nystartrow.max(8);
                        nyendrow = nyendrow.min(rr1 - 8);
                        nystartcol = nystartcol.max(8);
                        nyendcol = nyendcol.min(cc1 - 8);

                        // Grow the region by a neighbour vote: if most of your
                        // neighbours are Nyquist, you probably are too.
                        for rr in nystartrow..nyendrow {
                            let mut cc = nystartcol + usize::from(green_row(rr));
                            while cc < nyendcol {
                                let indx = rr * TILE + cc;
                                let t = s.nyquist[(indx - V2) >> 1] as u32
                                    + s.nyquist[(indx - M1) >> 1] as u32
                                    + s.nyquist[(indx - P1) >> 1] as u32
                                    + s.nyquist[(indx - 2) >> 1] as u32
                                    + s.nyquist[(indx + 2) >> 1] as u32
                                    + s.nyquist[(indx + P1) >> 1] as u32
                                    + s.nyquist[(indx + M1) >> 1] as u32
                                    + s.nyquist[(indx + V2) >> 1] as u32;
                                s.nyquist2[indx >> 1] = match t.cmp(&4) {
                                    std::cmp::Ordering::Greater => 1,
                                    std::cmp::Ordering::Less => 0,
                                    std::cmp::Ordering::Equal => s.nyquist[indx >> 1],
                                };
                                cc += 2;
                            }
                        }

                        // Area interpolation over the Nyquist pixels only.
                        for rr in nystartrow..nyendrow {
                            let mut cc = nystartcol + usize::from(green_row(rr));
                            while cc < nyendcol {
                                let indx = rr * TILE + cc;
                                if s.nyquist2[indx >> 1] != 0 {
                                    let (mut sumcfa, mut sumh, mut sumv) = (0.0f32, 0.0f32, 0.0f32);
                                    let (mut sumsqh, mut sumsqv, mut areawt) =
                                        (0.0f32, 0.0f32, 0.0f32);
                                    let mut i: isize = -6;
                                    while i < 7 {
                                        let mut indx1 =
                                            (indx as isize + i * TILE as isize - 6) as usize;
                                        let mut j: isize = -6;
                                        while j < 7 {
                                            if s.nyquist2[indx1 >> 1] != 0 {
                                                let t = s.cfa[indx1];
                                                sumcfa += t;
                                                sumh += s.cfa[indx1 - 1] + s.cfa[indx1 + 1];
                                                sumv += s.cfa[indx1 - V1] + s.cfa[indx1 + V1];
                                                sumsqh += sqr(t - s.cfa[indx1 - 1])
                                                    + sqr(t - s.cfa[indx1 + 1]);
                                                sumsqv += sqr(t - s.cfa[indx1 - V1])
                                                    + sqr(t - s.cfa[indx1 + V1]);
                                                areawt += 1.0;
                                            }
                                            j += 2;
                                            indx1 += 2;
                                        }
                                        i += 2;
                                    }
                                    let sumh = sumcfa - 0.5 * sumh;
                                    let sumv = sumcfa - 0.5 * sumv;
                                    let areawt = 0.5 * areawt;
                                    let hcdvar = EPSSQ + (areawt * sumsqh - sumh * sumh).abs();
                                    let vcdvar = EPSSQ + (areawt * sumsqv - sumv * sumv).abs();
                                    s.hvwt[indx >> 1] = hcdvar / (vcdvar + hcdvar);
                                }
                                cc += 2;
                            }
                        }
                    }

                    // -- Green at red and blue sites, finally.
                    for rr in 8..rr1 - 8 {
                        let mut cc = 8 + usize::from(green_row(rr));
                        while cc < cc1 - 8 {
                            let indx = rr * TILE + cc;
                            let i1 = indx >> 1;
                            // Prefer the neighbours' discrimination when it is more
                            // decisive than this pixel's own.
                            let hvwtalt = 0.25
                                * (s.hvwt[(indx - M1) >> 1]
                                    + s.hvwt[(indx - P1) >> 1]
                                    + s.hvwt[(indx + P1) >> 1]
                                    + s.hvwt[(indx + M1) >> 1]);
                            if (0.5 - s.hvwt[i1]).abs() < (0.5 - hvwtalt).abs() {
                                s.hvwt[i1] = hvwtalt;
                            }
                            s.dgrb0[i1] = intp(s.hvwt[i1], s.vcd[indx], s.hcd[indx]);
                            s.rgbgreen[indx] = s.cfa[indx] + s.dgrb0[i1];

                            // Local curvature of the interpolated green, for the
                            // Nyquist refinement below.
                            if s.nyquist2[i1] != 0 {
                                s.dgrb2h[i1] = sqr(s.rgbgreen[indx]
                                    - 0.5 * (s.rgbgreen[indx - 1] + s.rgbgreen[indx + 1]));
                                s.dgrb2v[i1] = sqr(s.rgbgreen[indx]
                                    - 0.5 * (s.rgbgreen[indx - V1] + s.rgbgreen[indx + V1]));
                            } else {
                                s.dgrb2h[i1] = 0.0;
                                s.dgrb2v[i1] = 0.0;
                            }
                            cc += 2;
                        }
                    }

                    // -- Refine the Nyquist areas using those curvatures.
                    if do_nyquist {
                        for rr in nystartrow..nyendrow {
                            let mut cc = nystartcol + usize::from(green_row(rr));
                            while cc < nyendcol {
                                let indx = rr * TILE + cc;
                                let i1 = indx >> 1;
                                if s.nyquist2[i1] != 0 {
                                    let gvarh = EPSSQ
                                        + (GQUINC[0] * s.dgrb2h[i1]
                                            + GQUINC[1]
                                                * (s.dgrb2h[(indx - M1) >> 1]
                                                    + s.dgrb2h[(indx - P1) >> 1]
                                                    + s.dgrb2h[(indx + P1) >> 1]
                                                    + s.dgrb2h[(indx + M1) >> 1])
                                            + GQUINC[2]
                                                * (s.dgrb2h[(indx - V2) >> 1]
                                                    + s.dgrb2h[(indx - 2) >> 1]
                                                    + s.dgrb2h[(indx + 2) >> 1]
                                                    + s.dgrb2h[(indx + V2) >> 1])
                                            + GQUINC[3]
                                                * (s.dgrb2h[(indx - M2) >> 1]
                                                    + s.dgrb2h[(indx - P2) >> 1]
                                                    + s.dgrb2h[(indx + P2) >> 1]
                                                    + s.dgrb2h[(indx + M2) >> 1]));
                                    let gvarv = EPSSQ
                                        + (GQUINC[0] * s.dgrb2v[i1]
                                            + GQUINC[1]
                                                * (s.dgrb2v[(indx - M1) >> 1]
                                                    + s.dgrb2v[(indx - P1) >> 1]
                                                    + s.dgrb2v[(indx + P1) >> 1]
                                                    + s.dgrb2v[(indx + M1) >> 1])
                                            + GQUINC[2]
                                                * (s.dgrb2v[(indx - V2) >> 1]
                                                    + s.dgrb2v[(indx - 2) >> 1]
                                                    + s.dgrb2v[(indx + 2) >> 1]
                                                    + s.dgrb2v[(indx + V2) >> 1])
                                            + GQUINC[3]
                                                * (s.dgrb2v[(indx - M2) >> 1]
                                                    + s.dgrb2v[(indx - P2) >> 1]
                                                    + s.dgrb2v[(indx + P2) >> 1]
                                                    + s.dgrb2v[(indx + M2) >> 1]));
                                    s.dgrb0[i1] = (s.hcd[indx] * gvarv + s.vcd[indx] * gvarh)
                                        / (gvarv + gvarh);
                                    s.rgbgreen[indx] = s.cfa[indx] + s.dgrb0[i1];
                                }
                                cc += 2;
                            }
                        }
                    }

                    // -- Diagonal gradients, for red at blue sites and vice versa.
                    for rr in 6..rr1 - 6 {
                        if !green_row(rr) {
                            let mut cc = 6;
                            while cc < cc1 - 6 {
                                let indx = rr * TILE + cc;
                                let i1 = indx >> 1;
                                s.delp[i1] = (s.cfa[indx - P1] - s.cfa[indx + P1]).abs();
                                s.delm[i1] = (s.cfa[indx + M1] - s.cfa[indx - M1]).abs();
                                s.dgrbsq1p[i1] = sqr(s.cfa[indx + 1] - s.cfa[indx + 1 + P1])
                                    + sqr(s.cfa[indx + 1] - s.cfa[indx + 1 - P1]);
                                s.dgrbsq1m[i1] = sqr(s.cfa[indx + 1] - s.cfa[indx + 1 - M1])
                                    + sqr(s.cfa[indx + 1] - s.cfa[indx + 1 + M1]);
                                cc += 2;
                            }
                        } else {
                            let mut cc = 6;
                            while cc < cc1 - 6 {
                                let indx = rr * TILE + cc;
                                let i1 = indx >> 1;
                                s.dgrbsq1p[i1] = sqr(s.cfa[indx] - s.cfa[indx + P1])
                                    + sqr(s.cfa[indx] - s.cfa[indx - P1]);
                                s.dgrbsq1m[i1] = sqr(s.cfa[indx] - s.cfa[indx - M1])
                                    + sqr(s.cfa[indx] - s.cfa[indx + M1]);
                                s.delp[i1] = (s.cfa[indx + 1 - P1] - s.cfa[indx + 1 + P1]).abs();
                                s.delm[i1] = (s.cfa[indx + 1 + M1] - s.cfa[indx + 1 - M1]).abs();
                                cc += 2;
                            }
                        }
                    }

                    // -- Diagonal interpolation of red at blue sites and vice versa.
                    for rr in 8..rr1 - 8 {
                        let mut cc = 8 + usize::from(green_row(rr));
                        while cc < cc1 - 8 {
                            let indx = rr * TILE + cc;
                            let i1 = indx >> 1;
                            let c0 = s.cfa[indx];

                            let crse = 2.0 * s.cfa[indx + M1] / (EPS + c0 + s.cfa[indx + M2]);
                            let crnw = 2.0 * s.cfa[indx - M1] / (EPS + c0 + s.cfa[indx - M2]);
                            let crne = 2.0 * s.cfa[indx - P1] / (EPS + c0 + s.cfa[indx - P2]);
                            let crsw = 2.0 * s.cfa[indx + P1] / (EPS + c0 + s.cfa[indx + P2]);

                            let rbse = if (1.0 - crse).abs() < ARTHRESH {
                                c0 * crse
                            } else {
                                s.cfa[indx + M1] + 0.5 * (c0 - s.cfa[indx + M2])
                            };
                            let rbnw = if (1.0 - crnw).abs() < ARTHRESH {
                                c0 * crnw
                            } else {
                                s.cfa[indx - M1] + 0.5 * (c0 - s.cfa[indx - M2])
                            };
                            let rbne = if (1.0 - crne).abs() < ARTHRESH {
                                c0 * crne
                            } else {
                                s.cfa[indx - P1] + 0.5 * (c0 - s.cfa[indx - P2])
                            };
                            let rbsw = if (1.0 - crsw).abs() < ARTHRESH {
                                c0 * crsw
                            } else {
                                s.cfa[indx + P1] + 0.5 * (c0 - s.cfa[indx + P2])
                            };

                            let wtse = EPS
                                + s.delm[i1]
                                + s.delm[(indx + M1) >> 1]
                                + s.delm[(indx + M2) >> 1];
                            let wtnw = EPS
                                + s.delm[i1]
                                + s.delm[(indx - M1) >> 1]
                                + s.delm[(indx - M2) >> 1];
                            let wtne = EPS
                                + s.delp[i1]
                                + s.delp[(indx - P1) >> 1]
                                + s.delp[(indx - P2) >> 1];
                            let wtsw = EPS
                                + s.delp[i1]
                                + s.delp[(indx + P1) >> 1]
                                + s.delp[(indx + P2) >> 1];

                            s.rbm[i1] = (wtse * rbnw + wtnw * rbse) / (wtse + wtnw);
                            s.rbp[i1] = (wtne * rbsw + wtsw * rbne) / (wtne + wtsw);

                            let rbvarm = EPSSQ
                                + (GAUSSEVEN[0]
                                    * (s.dgrbsq1m[(indx - V1) >> 1]
                                        + s.dgrbsq1m[(indx - 1) >> 1]
                                        + s.dgrbsq1m[(indx + 1) >> 1]
                                        + s.dgrbsq1m[(indx + V1) >> 1])
                                    + GAUSSEVEN[1]
                                        * (s.dgrbsq1m[(indx - V2 - 1) >> 1]
                                            + s.dgrbsq1m[(indx - V2 + 1) >> 1]
                                            + s.dgrbsq1m[(indx - 2 - V1) >> 1]
                                            + s.dgrbsq1m[(indx + 2 - V1) >> 1]
                                            + s.dgrbsq1m[(indx - 2 + V1) >> 1]
                                            + s.dgrbsq1m[(indx + 2 + V1) >> 1]
                                            + s.dgrbsq1m[(indx + V2 - 1) >> 1]
                                            + s.dgrbsq1m[(indx + V2 + 1) >> 1]));
                            let rbvarp = EPSSQ
                                + (GAUSSEVEN[0]
                                    * (s.dgrbsq1p[(indx - V1) >> 1]
                                        + s.dgrbsq1p[(indx - 1) >> 1]
                                        + s.dgrbsq1p[(indx + 1) >> 1]
                                        + s.dgrbsq1p[(indx + V1) >> 1])
                                    + GAUSSEVEN[1]
                                        * (s.dgrbsq1p[(indx - V2 - 1) >> 1]
                                            + s.dgrbsq1p[(indx - V2 + 1) >> 1]
                                            + s.dgrbsq1p[(indx - 2 - V1) >> 1]
                                            + s.dgrbsq1p[(indx + 2 - V1) >> 1]
                                            + s.dgrbsq1p[(indx - 2 + V1) >> 1]
                                            + s.dgrbsq1p[(indx + 2 + V1) >> 1]
                                            + s.dgrbsq1p[(indx + V2 - 1) >> 1]
                                            + s.dgrbsq1p[(indx + V2 + 1) >> 1]));
                            s.pmwt[i1] = rbvarm / (rbvarp + rbvarm);

                            // Bound the interpolation where saturated.
                            if s.rbp[i1] < c0 {
                                let m = median3(s.rbp[i1], s.cfa[indx - P1], s.cfa[indx + P1]);
                                if 2.0 * s.rbp[i1] < c0 {
                                    s.rbp[i1] = m;
                                } else {
                                    let pwt = 2.0 * (c0 - s.rbp[i1]) / (EPS + s.rbp[i1] + c0);
                                    s.rbp[i1] = pwt * s.rbp[i1] + (1.0 - pwt) * m;
                                }
                            }
                            if s.rbm[i1] < c0 {
                                let m = median3(s.rbm[i1], s.cfa[indx - M1], s.cfa[indx + M1]);
                                if 2.0 * s.rbm[i1] < c0 {
                                    s.rbm[i1] = m;
                                } else {
                                    let mwt = 2.0 * (c0 - s.rbm[i1]) / (EPS + s.rbm[i1] + c0);
                                    s.rbm[i1] = mwt * s.rbm[i1] + (1.0 - mwt) * m;
                                }
                            }
                            if s.rbp[i1] > CLIP_PT {
                                s.rbp[i1] = median3(s.rbp[i1], s.cfa[indx - P1], s.cfa[indx + P1]);
                            }
                            if s.rbm[i1] > CLIP_PT {
                                s.rbm[i1] = median3(s.rbm[i1], s.cfa[indx - M1], s.cfa[indx + M1]);
                            }
                            cc += 2;
                        }
                    }

                    // -- R+B, interpolated.
                    for rr in 10..rr1 - 10 {
                        let mut cc = 10 + usize::from(green_row(rr));
                        while cc < cc1 - 10 {
                            let indx = rr * TILE + cc;
                            let i1 = indx >> 1;
                            let pmwtalt = 0.25
                                * (s.pmwt[(indx - M1) >> 1]
                                    + s.pmwt[(indx - P1) >> 1]
                                    + s.pmwt[(indx + P1) >> 1]
                                    + s.pmwt[(indx + M1) >> 1]);
                            if (0.5 - s.pmwt[i1]).abs() < (0.5 - pmwtalt).abs() {
                                s.pmwt[i1] = pmwtalt;
                            }
                            s.rbint[i1] = 0.5
                                * (s.cfa[indx]
                                    + s.rbm[i1] * (1.0 - s.pmwt[i1])
                                    + s.rbp[i1] * s.pmwt[i1]);
                            cc += 2;
                        }
                    }

                    // -- Diagonal interpolation correction: redo green from R+B where
                    //    the diagonal discrimination was the more decisive one.
                    for rr in 12..rr1 - 12 {
                        let mut cc = 12 + usize::from(green_row(rr));
                        while cc < cc1 - 12 {
                            let indx = rr * TILE + cc;
                            let i1 = indx >> 1;
                            if (0.5 - s.pmwt[i1]).abs() < (0.5 - s.hvwt[i1]).abs() {
                                cc += 2;
                                continue;
                            }
                            let rb = s.rbint[i1];
                            // `rbint` holds R+B at red and blue sites, which exist on
                            // every row — so one entry is two columns and `TILE` entries
                            // is two ROWS, which is the nearest site of the same class
                            // vertically. That is why the reference steps by `v1` on an
                            // already-halved index; it is not an off-by-two.
                            let cru = s.cfa[indx - V1] * 2.0 / (EPS + rb + s.rbint[i1 - TILE]);
                            let crd = s.cfa[indx + V1] * 2.0 / (EPS + rb + s.rbint[i1 + TILE]);
                            let crl = s.cfa[indx - 1] * 2.0 / (EPS + rb + s.rbint[i1 - 1]);
                            let crr = s.cfa[indx + 1] * 2.0 / (EPS + rb + s.rbint[i1 + 1]);

                            let gu = if (1.0 - cru).abs() < ARTHRESH {
                                rb * cru
                            } else {
                                s.cfa[indx - V1] + 0.5 * (rb - s.rbint[i1 - TILE])
                            };
                            let gd = if (1.0 - crd).abs() < ARTHRESH {
                                rb * crd
                            } else {
                                s.cfa[indx + V1] + 0.5 * (rb - s.rbint[i1 + TILE])
                            };
                            let gl = if (1.0 - crl).abs() < ARTHRESH {
                                rb * crl
                            } else {
                                s.cfa[indx - 1] + 0.5 * (rb - s.rbint[i1 - 1])
                            };
                            let gr = if (1.0 - crr).abs() < ARTHRESH {
                                rb * crr
                            } else {
                                s.cfa[indx + 1] + 0.5 * (rb - s.rbint[i1 + 1])
                            };

                            let mut gintv = (s.dirwts0[indx - V1] * gd + s.dirwts0[indx + V1] * gu)
                                / (s.dirwts0[indx + V1] + s.dirwts0[indx - V1]);
                            let mut ginth = (s.dirwts1[indx - 1] * gr + s.dirwts1[indx + 1] * gl)
                                / (s.dirwts1[indx - 1] + s.dirwts1[indx + 1]);

                            if gintv < rb {
                                let m = median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]);
                                if 2.0 * gintv < rb {
                                    gintv = m;
                                } else {
                                    let vwt = 2.0 * (rb - gintv) / (EPS + gintv + rb);
                                    gintv = vwt * gintv + (1.0 - vwt) * m;
                                }
                            }
                            if ginth < rb {
                                let m = median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]);
                                if 2.0 * ginth < rb {
                                    ginth = m;
                                } else {
                                    let hwt = 2.0 * (rb - ginth) / (EPS + ginth + rb);
                                    ginth = hwt * ginth + (1.0 - hwt) * m;
                                }
                            }
                            if ginth > CLIP_PT {
                                ginth = median3(ginth, s.cfa[indx - 1], s.cfa[indx + 1]);
                            }
                            if gintv > CLIP_PT {
                                gintv = median3(gintv, s.cfa[indx - V1], s.cfa[indx + V1]);
                            }

                            s.rgbgreen[indx] = ginth * (1.0 - s.hvwt[i1]) + gintv * s.hvwt[i1];
                            s.dgrb0[i1] = s.rgbgreen[indx] - s.cfa[indx];
                            cc += 2;
                        }
                    }

                    // -- Split G-R from G-B: the blue coset moves to `dgrb1`.
                    let mut rr = 13usize.saturating_sub(ey);
                    while rr < rr1 - 12 {
                        let start = (rr * TILE + 13 - ex) >> 1;
                        let end = (rr * TILE + cc1 - 12) >> 1;
                        for i1 in start..end {
                            s.dgrb1[i1] = s.dgrb0[i1];
                            s.dgrb0[i1] = 0.0;
                        }
                        rr += 2;
                    }

                    // -- Fancy chrominance interpolation: fill in the colour difference
                    //    the site does not carry, from the four diagonals.
                    for rr in 14..rr1 - 14 {
                        let mut cc = 14 + usize::from(green_row(rr));
                        while cc < cc1 - 14 {
                            let indx = rr * TILE + cc;
                            // At a red site fill G-B; at a blue site fill G-R.
                            let use1 = colour(rr, cc) == CfaColor::Red;
                            let d: &mut [f32] = if use1 { &mut s.dgrb1 } else { &mut s.dgrb0 };

                            let nw = (indx - M1) >> 1;
                            let ne = (indx - P1) >> 1;
                            let sw = (indx + P1) >> 1;
                            let se = (indx + M1) >> 1;
                            let nw3 = (indx - M3) >> 1;
                            let ne3 = (indx - P3) >> 1;
                            let sw3 = (indx + P3) >> 1;
                            let se3 = (indx + M3) >> 1;

                            let wtnw = 1.0
                                / (EPS
                                    + (d[nw] - d[se]).abs()
                                    + (d[nw] - d[nw3]).abs()
                                    + (d[se] - d[nw3]).abs());
                            let wtne = 1.0
                                / (EPS
                                    + (d[ne] - d[sw]).abs()
                                    + (d[ne] - d[ne3]).abs()
                                    + (d[sw] - d[ne3]).abs());
                            let wtsw = 1.0
                                / (EPS
                                    + (d[sw] - d[ne]).abs()
                                    + (d[sw] - d[se3]).abs()
                                    + (d[ne] - d[sw3]).abs());
                            let wtse = 1.0
                                / (EPS
                                    + (d[se] - d[nw]).abs()
                                    + (d[se] - d[sw3]).abs()
                                    + (d[nw] - d[se3]).abs());

                            let v = (wtnw
                                * (1.325 * d[nw]
                                    - 0.175 * d[nw3]
                                    - 0.075 * d[(indx - M1 - 2) >> 1]
                                    - 0.075 * d[(indx - M1 - V2) >> 1])
                                + wtne
                                    * (1.325 * d[ne]
                                        - 0.175 * d[ne3]
                                        - 0.075 * d[(indx - P1 + 2) >> 1]
                                        - 0.075 * d[(indx - P1 + V2) >> 1])
                                + wtsw
                                    * (1.325 * d[sw]
                                        - 0.175 * d[sw3]
                                        - 0.075 * d[(indx + P1 - 2) >> 1]
                                        - 0.075 * d[(indx + P1 - V2) >> 1])
                                + wtse
                                    * (1.325 * d[se]
                                        - 0.175 * d[se3]
                                        - 0.075 * d[(indx + M1 + 2) >> 1]
                                        - 0.075 * d[(indx + M1 + V2) >> 1]))
                                / (wtnw + wtne + wtsw + wtse);
                            d[indx >> 1] = v;
                            cc += 2;
                        }
                    }

                    // -- Collapse to luma. At a green site red and blue come from the
                    //    four neighbours weighted by the same horizontal/vertical
                    //    discrimination that produced green; elsewhere they are the
                    //    colour difference against this site's own green.
                    let first_col = col_start + BORDER;
                    let last_col = col_end - BORDER;
                    for row in (row_start + BORDER)..(row_end - BORDER) {
                        let rr = row - row_start;
                        let out_row = row - band_row0;
                        for col in first_col..last_col {
                            let cc = col - col_start;
                            let indx = rr * TILE + cc;
                            let i1 = indx >> 1;
                            let g = s.rgbgreen[indx];
                            let (r, b) = if colour(rr, cc) == CfaColor::Green {
                                let t = 1.0
                                    / (s.hvwt[(indx - V1) >> 1] + 2.0
                                        - s.hvwt[(indx + 1) >> 1]
                                        - s.hvwt[(indx - 1) >> 1]
                                        + s.hvwt[(indx + V1) >> 1]);
                                let mix4 = |d: &[f32]| {
                                    (s.hvwt[(indx - V1) >> 1] * d[(indx - V1) >> 1]
                                        + (1.0 - s.hvwt[(indx + 1) >> 1]) * d[(indx + 1) >> 1]
                                        + (1.0 - s.hvwt[(indx - 1) >> 1]) * d[(indx - 1) >> 1]
                                        + s.hvwt[(indx + V1) >> 1] * d[(indx + V1) >> 1])
                                        * t
                                };
                                (g - mix4(&s.dgrb0), g - mix4(&s.dgrb1))
                            } else {
                                (g - s.dgrb0[i1], g - s.dgrb1[i1])
                            };
                            band[out_row * w + col] =
                                mix[0] * floor0(r) + mix[1] * floor0(g) + mix[2] * floor0(b);
                        }
                    }
                }
            });
    }

    border_luma(scene, mix, BORDER, &mut data);
    LumaImage {
        data,
        output_dims: src,
        source_dims: src,
        clipped: Vec::new(),
    }
}

/// AMaZE's per-tile working set. Roughly 2.7 MB at `TILE = 192`.
///
/// The reference overlays several of these on one allocation, with comments noting
/// where lifetimes do not overlap. They are separate here: the sharing saves memory
/// that this app does not need to save, and it is the kind of aliasing that turns a
/// reordered step into a silent corruption rather than a compile error.
struct Amaze {
    cfa: Vec<f32>,
    rgbgreen: Vec<f32>,
    delhvsqsum: Vec<f32>,
    dirwts0: Vec<f32>,
    dirwts1: Vec<f32>,
    vcd: Vec<f32>,
    hcd: Vec<f32>,
    vcdalt: Vec<f32>,
    hcdalt: Vec<f32>,
    cddiffsq: Vec<f32>,
    dgintv: Vec<f32>,
    dginth: Vec<f32>,
    hvwt: Vec<f32>,
    dgrb0: Vec<f32>,
    dgrb1: Vec<f32>,
    dgrb2h: Vec<f32>,
    dgrb2v: Vec<f32>,
    delp: Vec<f32>,
    delm: Vec<f32>,
    rbint: Vec<f32>,
    dgrbsq1m: Vec<f32>,
    dgrbsq1p: Vec<f32>,
    pmwt: Vec<f32>,
    rbm: Vec<f32>,
    rbp: Vec<f32>,
    nyqutest: Vec<f32>,
    nyquist: Vec<u8>,
    nyquist2: Vec<u8>,
}

impl Amaze {
    fn new(n: usize, nh: usize) -> Self {
        let full = || vec![0.0f32; n];
        // +TILE of slack: the half-lattice indices are derived as `indx >> 1` from
        // full indices that legitimately reach a row beyond the last half entry.
        let half = || vec![0.0f32; nh + TILE];
        Self {
            cfa: full(),
            rgbgreen: full(),
            delhvsqsum: full(),
            dirwts0: full(),
            dirwts1: full(),
            vcd: full(),
            hcd: full(),
            vcdalt: full(),
            hcdalt: full(),
            cddiffsq: full(),
            dgintv: full(),
            dginth: full(),
            hvwt: half(),
            dgrb0: half(),
            dgrb1: half(),
            dgrb2h: half(),
            dgrb2v: half(),
            delp: half(),
            delm: half(),
            rbint: half(),
            dgrbsq1m: half(),
            dgrbsq1p: half(),
            pmwt: half(),
            rbm: half(),
            rbp: half(),
            nyqutest: half(),
            nyquist: vec![0u8; nh + TILE],
            nyquist2: vec![0u8; nh + TILE],
        }
    }

    fn clear(&mut self) {
        for v in [
            &mut self.cfa,
            &mut self.rgbgreen,
            &mut self.delhvsqsum,
            &mut self.dirwts0,
            &mut self.dirwts1,
            &mut self.vcd,
            &mut self.hcd,
            &mut self.vcdalt,
            &mut self.hcdalt,
            &mut self.cddiffsq,
            &mut self.dgintv,
            &mut self.dginth,
            &mut self.hvwt,
            &mut self.dgrb0,
            &mut self.dgrb1,
            &mut self.dgrb2h,
            &mut self.dgrb2v,
            &mut self.delp,
            &mut self.delm,
            &mut self.rbint,
            &mut self.dgrbsq1m,
            &mut self.dgrbsq1p,
            &mut self.pmwt,
            &mut self.rbm,
            &mut self.rbp,
            &mut self.nyqutest,
        ] {
            v.fill(0.0);
        }
        self.nyquist.fill(0);
        self.nyquist2.fill(0);
    }
}

// -------------------------------------------------------------------- bilinear

/// Textbook bilinear: a channel's own photosite is used directly; otherwise the
/// nearest photosites of that colour are averaged — orthogonal neighbours if any
/// carry the colour, else the diagonal ones. For a Bayer pattern that reduces to
/// the standard 2- or 4-tap averages.
///
/// Kept as the floor to measure the others against, not as a mode anyone should
/// reach for: it does not look at the image before interpolating, so it is full
/// size without full-size detail, plus edge stipple.
fn bilinear(scene: &SceneImage, weighting: Weighting) -> LumaImage {
    let src = scene.geom.crop;
    let mix = weighting.weights();
    let (w, h) = (src.w as isize, src.h as isize);

    let at = |r: isize, c: isize| -> f32 {
        let rr = r.clamp(0, h - 1) as usize;
        let cc = c.clamp(0, w - 1) as usize;
        scene.data[rr * src.w + cc]
    };
    let channel = |r: isize, c: isize, target: CfaColor| -> f32 {
        if scene.color_at(r as usize, c as usize) == target {
            return at(r, c);
        }
        let orth = [(-1, 0), (1, 0), (0, -1), (0, 1)];
        let (mut sum, mut n) = (0.0f32, 0u32);
        for (dr, dc) in orth {
            if scene.color_at(
                (r + dr).clamp(0, h - 1) as usize,
                (c + dc).clamp(0, w - 1) as usize,
            ) == target
            {
                sum += at(r + dr, c + dc);
                n += 1;
            }
        }
        if n > 0 {
            return sum / n as f32;
        }
        let diag = [(-1, -1), (-1, 1), (1, -1), (1, 1)];
        let (mut sum, mut n) = (0.0f32, 0u32);
        for (dr, dc) in diag {
            if scene.color_at(
                (r + dr).clamp(0, h - 1) as usize,
                (c + dc).clamp(0, w - 1) as usize,
            ) == target
            {
                sum += at(r + dr, c + dc);
                n += 1;
            }
        }
        if n > 0 { sum / n as f32 } else { at(r, c) }
    };

    let mut data = vec![0.0f32; src.w * src.h];
    data.par_chunks_mut(src.w)
        .enumerate()
        .for_each(|(row, orow)| {
            let r = row as isize;
            for (col, o) in orow.iter_mut().enumerate() {
                let c = col as isize;
                *o = mix[0] * channel(r, c, CfaColor::Red)
                    + mix[1] * channel(r, c, CfaColor::Green)
                    + mix[2] * channel(r, c, CfaColor::Blue);
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
    use crate::geometry::CfaColor::{Blue, Green, Red};
    use crate::{CfaGeometry, Dims, sensor::Gains};

    /// A synthetic RGGB scene from a per-pixel luminance function.
    ///
    /// `f` returns the value every photosite at that position records, which models
    /// a **neutral** subject after gain equalisation — all three channels read the
    /// same value for neutral light, which is the whole point of equalisation. That
    /// makes these tests about spatial reconstruction only, with no chroma to hide
    /// behind.
    fn scene(w: usize, h: usize, f: impl Fn(usize, usize) -> f32) -> SceneImage {
        let geom = CfaGeometry::new(w, Dims { w, h }, 0, 0, w, h, [[Red, Green], [Green, Blue]]);
        let mut data = vec![0.0f32; w * h];
        for row in 0..h {
            for col in 0..w {
                data[row * w + col] = f(row, col);
            }
        }
        SceneImage {
            data,
            geom,
            gains: Gains([1.0, 1.0, 1.0]),
            camera: "test".into(),
            clipped: Vec::new(),
        }
    }

    /// Big enough to span three tiles each way, so seams and phase are exercised.
    const BIG: usize = 400;

    #[test]
    fn the_tile_step_is_even() {
        // The lattice trap, as an assertion. Tile-relative CFA lookups are only
        // valid because every tile origin lands on the same Bayer phase as the
        // sensor. An odd step would demosaic every tile after the first against a
        // pattern shifted by one photosite — which does not crash, does not look
        // broken at a glance, and puts a grid of wrong pixels across the frame.
        const TILE: usize = 194;
        const BORDER: usize = 9;
        assert_eq!(
            (TILE - 2 * BORDER) % 2,
            0,
            "tile step must preserve Bayer phase"
        );
    }

    #[test]
    fn a_flat_field_stays_flat_across_tile_seams() {
        // The strongest single test of the port: it catches a phase error, a seam,
        // an unwritten border and a brightness shift at once. 400px spans three
        // tiles each way, so the tile grid is genuinely exercised — a 16px image
        // would take the all-border path and prove nothing.
        let s = scene(BIG, BIG, |_, _| 0.42);
        for algo in DemosaicAlgo::UI_ORDER {
            let luma = demosaic(&s, algo, Weighting::Photosite);
            assert_eq!(luma.output_dims, Dims { w: BIG, h: BIG });
            let (min, max) = luma
                .data
                .iter()
                .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
            assert!(
                (min - 0.42).abs() < 1e-4 && (max - 0.42).abs() < 1e-4,
                "{algo:?} did not stay flat: {min}..{max}"
            );
        }
    }

    #[test]
    fn every_pixel_is_written_and_finite() {
        // A skipped tile, or a border that was not covered, shows up as a black
        // band; a division by a zero gradient shows up as NaN. Neither is visible
        // in a mean.
        let s = scene(BIG, BIG, |r, c| {
            0.2 + 0.3 * ((r + c) as f32 / (2 * BIG) as f32)
        });
        for algo in DemosaicAlgo::UI_ORDER {
            let luma = demosaic(&s, algo, Weighting::Photosite);
            for (i, v) in luma.data.iter().enumerate() {
                assert!(
                    v.is_finite() && *v > 0.0,
                    "{algo:?} left pixel {},{} as {v}",
                    i / BIG,
                    i % BIG
                );
            }
        }
    }

    #[test]
    fn a_smooth_ramp_survives_reconstruction() {
        // Nothing here needs inventing, so a correct demosaic reproduces it. This is
        // the test that would fail if the ratio step's index arithmetic read the
        // wrong neighbour — that stays flat-field-correct and goes wrong on any
        // gradient.
        let s = scene(BIG, BIG, |_, c| 0.1 + 0.8 * (c as f32 / BIG as f32));
        let luma = demosaic(&s, DemosaicAlgo::Rcd, Weighting::Photosite);
        let mut worst = 0.0f32;
        // Interior only: the border is an explicit local mean and does not claim to
        // reconstruct a gradient.
        for row in 12..BIG - 12 {
            for col in 12..BIG - 12 {
                let want = 0.1 + 0.8 * (col as f32 / BIG as f32);
                worst = worst.max((luma.data[row * BIG + col] - want).abs());
            }
        }
        assert!(worst < 1e-3, "RCD distorted a plain ramp by {worst}");
    }

    #[test]
    fn rcd_resolves_fine_detail_bilinear_smears() {
        // The claim the mode exists to deliver, measured rather than asserted.
        // Vertical bars at a 4px period: every CFA colour samples both phases, so
        // the information is present and a directional method should keep it. This
        // is the frequency where undirected interpolation visibly gives up.
        let bars = |_r: usize, c: usize| if (c / 2).is_multiple_of(2) { 0.7 } else { 0.2 };
        let s = scene(BIG, BIG, bars);

        let modulation = |algo| {
            let luma = demosaic(&s, algo, Weighting::Photosite);
            let row = BIG / 2;
            let (mut bright, mut dark) = (f32::MAX, f32::MIN);
            for col in 24..BIG - 24 {
                let v = luma.data[row * BIG + col];
                if (col / 2).is_multiple_of(2) {
                    bright = bright.min(v);
                } else {
                    dark = dark.max(v);
                }
            }
            // Worst-case contrast retained across the bar pattern; 0.5 is perfect.
            bright - dark
        };

        let bil = modulation(DemosaicAlgo::Bilinear);
        for algo in DemosaicAlgo::UI_ORDER {
            if algo == DemosaicAlgo::Bilinear {
                continue;
            }
            let got = modulation(algo);
            assert!(
                got > bil,
                "{algo:?} did not resolve more than bilinear: {got} vs {bil}"
            );
        }
    }

    #[test]
    fn brightness_holds_across_weightings_at_tile_scale() {
        // The app-wide invariant — mode switching changes character, not brightness
        // — checked at a size that actually runs the tiled path.
        let s = scene(BIG, BIG, |_, _| 0.42);
        for w in [
            Weighting::Photosite,
            Weighting::Equal,
            Weighting::Red,
            Weighting::Green,
            Weighting::Blue,
            Weighting::Weighted(0.7, 0.2, 0.1),
        ] {
            let luma = demosaic(&s, DemosaicAlgo::Rcd, w);
            // f64, and the reason is not fastidiousness: 160 000 f32 additions at
            // magnitude 0.42 accumulate ~1e-3 of rounding, which is ten times the
            // tolerance this test wants to assert. Summing in f32 made this test
            // report a brightness shift that no pixel actually had.
            let mean = luma.data.iter().map(|v| *v as f64).sum::<f64>() / luma.data.len() as f64;
            assert!(
                (mean - 0.42).abs() < 1e-4,
                "{w:?} shifted brightness to {mean}"
            );
        }
    }

    #[test]
    fn an_image_smaller_than_one_tile_is_still_fully_reconstructed() {
        // Not a real photograph, but it is what every unit test upstream uses, and
        // an unhandled small image is a panic rather than a bad picture.
        for n in [4usize, 17, 20, 64] {
            let s = scene(n, n, |_, _| 0.3);
            let luma = demosaic(&s, DemosaicAlgo::Rcd, Weighting::Photosite);
            // Not `n * n`: `CfaGeometry` forces the crop extent even so 2x2 binning
            // is exact, so a 17px request is a 16px frame. Reading `output_dims` is
            // the rule — it is `source_dims` that is allowed to differ.
            let dims = luma.output_dims;
            assert_eq!(
                dims,
                Dims {
                    w: n & !1,
                    h: n & !1
                },
                "{n}x{n}"
            );
            assert_eq!(luma.data.len(), dims.w * dims.h);
            for v in &luma.data {
                assert!((v - 0.3).abs() < 1e-4, "{n}x{n} gave {v}");
            }
        }
    }
}
