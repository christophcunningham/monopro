//! OKHSL → sRGB.
//!
//! Ported from Björn Ottosson's reference `ok_color.h`, © 2021 Björn Ottosson, MIT.
//! <https://bottosson.github.io/posts/colorpicker/>
//!
//! # Why this space and not a plain RGB picker
//!
//! The surround is a **mount** — a border the print is judged against — so the one
//! property that matters is that changing its hue must not change how *bright* it
//! reads. OKHSL is perceptually uniform in lightness, so it has exactly that: move
//! hue at fixed `l` and the mount stays the same weight against the print. An RGB
//! picker does not, and neither does HSL: full-saturation yellow and full-saturation
//! blue are nowhere near the same lightness.
//!
//! The second property earns its keep on the slider rather than in the image.
//! OKHSL normalises saturation **to the sRGB gamut boundary at that hue and
//! lightness**, so `s = 1` is always the most saturated colour that exists there and
//! the slider always spans the full available range. Oklch would have the lightness
//! property too, but its chroma axis runs off the end of the gamut at a near-white
//! lightness — which is exactly where a mount lives, so most of the slider would do
//! nothing.
//!
//! That normalisation is what all the machinery below is for: finding the cusp of
//! the gamut for a hue, and the chroma at which a given lightness leaves it.
//!
//! # Faithfulness
//!
//! Transcribed rather than recalled, and the constants are the reference's. Where a
//! name here is shorter than the reference's, the reference's is in the doc comment
//! so the two can be diffed.

/// A colour in OKHSL. Hue in **degrees**, saturation and lightness in `[0, 1]`.
///
/// Degrees rather than the reference's turns, because the UI slider is in degrees
/// and one conversion in one place beats the same `/ 360.0` at every call site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Okhsl {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

/// Convert to **gamma-encoded** sRGB in `[0, 1]`.
///
/// Gamma-encoded, not linear, because every consumer here — the egui swatch and the
/// display shader, which writes an already-encoded target — wants it that way.
pub fn to_srgb(c: Okhsl) -> [f32; 3] {
    let (h, s, l) = (c.h / 360.0, c.s.clamp(0.0, 1.0), c.l.clamp(0.0, 1.0));

    // The reference special-cases the ends, where the gamut has no width and the
    // chroma machinery would divide by zero.
    if l >= 1.0 {
        return [1.0, 1.0, 1.0];
    }
    if l <= 0.0 {
        return [0.0, 0.0, 0.0];
    }

    let a_ = (2.0 * std::f32::consts::PI * h).cos();
    let b_ = (2.0 * std::f32::consts::PI * h).sin();
    let big_l = toe_inv(l);

    let (c0, c_mid, c_max) = chromas(big_l, a_, b_);

    // Saturation is piecewise: two rational segments meeting at `MID`, chosen so
    // the curve is smooth and `s` spends most of its range in the part of the gamut
    // a user actually picks from.
    const MID: f32 = 0.8;
    const MID_INV: f32 = 1.25;
    let chroma = if s < MID {
        let t = MID_INV * s;
        let k1 = MID * c0;
        let k2 = 1.0 - k1 / c_mid;
        t * k1 / (1.0 - k2 * t)
    } else {
        let t = (s - MID) / (1.0 - MID);
        let k0 = c_mid;
        let k1 = (1.0 - MID) * c_mid * c_mid * MID_INV * MID_INV / c0;
        let k2 = 1.0 - k1 / (c_max - c_mid);
        k0 + t * k1 / (1.0 - k2 * t)
    };

    let rgb = oklab_to_linear_srgb(big_l, chroma * a_, chroma * b_);
    [transfer(rgb[0]), transfer(rgb[1]), transfer(rgb[2])]
}

/// **sRGB → OKHSL**, the inverse of [`to_srgb`]. Ottosson's `srgb_to_okhsl`.
///
/// # Why this direction has to exist
///
/// OKHSL is what the surround *stores*, because moving hue at fixed lightness is the
/// one property a mount needs — see the module note. But nobody types OKHSL. A hex
/// field and a colour wheel both speak sRGB, so every way of *entering* a colour
/// arrives in the wrong space and has to be brought back into this one.
///
/// That is the split `docs/ui-queue.md` records: **OKHSL authoritative, sRGB as an
/// input and output format**. The consequence worth knowing is that a round trip
/// through here is not always the identity — an sRGB colour outside what OKHSL can
/// name at that lightness comes back clamped to the gamut boundary, which is correct
/// and is what `s = 1` means.
///
/// Input is **gamma-encoded** sRGB in `[0, 1]`, matching what [`to_srgb`] returns.
pub fn from_srgb(rgb: [f32; 3]) -> Okhsl {
    let lin = [
        transfer_inv(rgb[0]),
        transfer_inv(rgb[1]),
        transfer_inv(rgb[2]),
    ];
    let (big_l, a, b) = linear_srgb_to_oklab(lin);

    let chroma = (a * a + b * b).sqrt();
    // A neutral has no hue to recover, and dividing by a zero chroma would produce
    // one out of rounding noise. Hue is arbitrary there, so it is kept at zero and
    // saturation is exactly zero — which is what a grey mount is.
    if chroma < 1.0e-6 {
        return Okhsl {
            h: 0.0,
            s: 0.0,
            l: toe(big_l),
        };
    }
    let (a_, b_) = (a / chroma, b / chroma);
    // `0.5 + 0.5 * atan2(-b, -a) / pi` in the reference, which is `atan2(b, a)`
    // rotated half a turn into `[0, 1]`. Degrees here, as everywhere in this module.
    let h = (0.5 + 0.5 * (-b).atan2(-a) / std::f32::consts::PI) * 360.0;

    let (c0, c_mid, c_max) = chromas(big_l, a_, b_);

    // The inverse of the two rational segments in `to_srgb`, meeting at `MID`.
    const MID: f32 = 0.8;
    const MID_INV: f32 = 1.25;
    let s = if chroma < c_mid {
        let k1 = MID * c0;
        let k2 = 1.0 - k1 / c_mid;
        (chroma / (k1 + k2 * chroma)) * MID
    } else {
        let k0 = c_mid;
        let k1 = (1.0 - MID) * c_mid * c_mid * MID_INV * MID_INV / c0;
        let k2 = 1.0 - k1 / (c_max - c_mid);
        let t = (chroma - k0) / (k1 + k2 * (chroma - k0));
        MID + (1.0 - MID) * t
    };

    Okhsl {
        h: h.rem_euclid(360.0),
        s: s.clamp(0.0, 1.0),
        l: toe(big_l),
    }
}

/// Ottosson's `toe`: the lightness compression that makes OKHSL's `l` match CIE
/// L\*'s spacing rather than Oklab's.
fn toe(x: f32) -> f32 {
    const K1: f32 = 0.206;
    const K2: f32 = 0.03;
    const K3: f32 = (1.0 + K1) / (1.0 + K2);
    0.5 * (K3 * x - K1 + ((K3 * x - K1) * (K3 * x - K1) + 4.0 * K2 * K3 * x).sqrt())
}

/// `toe_inv`.
fn toe_inv(x: f32) -> f32 {
    const K1: f32 = 0.206;
    const K2: f32 = 0.03;
    const K3: f32 = (1.0 + K1) / (1.0 + K2);
    (x * x + K1 * x) / (K3 * (x + K2))
}

fn transfer(a: f32) -> f32 {
    if a <= 0.003_130_8 {
        12.92 * a
    } else {
        1.055 * a.powf(0.416_666_66) - 0.055
    }
}

/// `srgb_transfer_function_inv`.
pub(crate) fn transfer_inv(a: f32) -> f32 {
    if a <= 0.040_45 {
        a / 12.92
    } else {
        ((a + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn linear_srgb_to_oklab(c: [f32; 3]) -> (f32, f32, f32) {
    let l = 0.412_221_46 * c[0] + 0.536_332_55 * c[1] + 0.051_445_995 * c[2];
    let m = 0.211_903_5 * c[0] + 0.680_699_5 * c[1] + 0.107_396_96 * c[2];
    let s = 0.088_302_46 * c[0] + 0.281_718_85 * c[1] + 0.629_978_5 * c[2];
    let (l_, m_, s_) = (l.cbrt(), m.cbrt(), s.cbrt());
    (
        0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.004_072_047 * s_,
        1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_,
        0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_77 * s_,
    )
}

/// OKLab -> linear sRGB, at D65 and with no adaptation.
///
/// `pub` because the display boundary needs it: a screen is D65, so the viewport path
/// converts straight to sRGB's own primaries. The *encode* boundary is a different
/// question and lives in [`crate::colour`], which adapts to the D50 an ICC profile
/// connection space is defined at.
pub fn oklab_linear_srgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    oklab_to_linear_srgb(l, a, b)
}

fn oklab_to_linear_srgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    ]
}

/// `compute_max_saturation`: the saturation `S = C/L` at which one channel of
/// linear sRGB first leaves the gamut, for a hue given as the unit vector `(a, b)`.
///
/// A polynomial fit plus one Halley step. The reference notes the residual error is
/// under 1e-6 except at some blue hues where `dS/dh` is nearly infinite, which is
/// well below anything a mount colour needs.
fn max_saturation(a: f32, b: f32) -> f32 {
    // Which channel leaves first decides the coefficients.
    let (k0, k1, k2, k3, k4, wl, wm, ws) = if -1.881_703_3 * a - 0.809_364_9 * b > 1.0 {
        // Red
        (
            1.190_862_8,
            1.765_767_3,
            0.596_626_4,
            0.755_152,
            0.567_712_4,
            4.076_741_7,
            -3.307_711_6,
            0.230_969_94,
        )
    } else if 1.814_441 * a - 1.194_452_8 * b > 1.0 {
        // Green
        (
            0.739_565_15,
            -0.459_544_04,
            0.082_854_27,
            0.125_410_7,
            0.145_032_04,
            -1.268_438,
            2.609_757_4,
            -0.341_319_4,
        )
    } else {
        // Blue
        (
            1.357_336_5,
            -0.009_157_99,
            -1.151_302_1,
            -0.505_596_06,
            0.006_921_67,
            -0.004_196_086_3,
            -0.703_418_6,
            1.707_614_7,
        )
    };

    let mut s = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let k_l = 0.396_337_78 * a + 0.215_803_76 * b;
    let k_m = -0.105_561_346 * a - 0.063_854_17 * b;
    let k_s = -0.089_484_18 * a - 1.291_485_5 * b;

    let (l_, m_, s_) = (1.0 + s * k_l, 1.0 + s * k_m, 1.0 + s * k_s);
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let l_ds = 3.0 * k_l * l_ * l_;
    let m_ds = 3.0 * k_m * m_ * m_;
    let s_ds = 3.0 * k_s * s_ * s_;
    let l_ds2 = 6.0 * k_l * k_l * l_;
    let m_ds2 = 6.0 * k_m * k_m * m_;
    let s_ds2 = 6.0 * k_s * k_s * s_;

    let f = wl * l3 + wm * m3 + ws * s3;
    let f1 = wl * l_ds + wm * m_ds + ws * s_ds;
    let f2 = wl * l_ds2 + wm * m_ds2 + ws * s_ds2;
    s -= f * f1 / (f1 * f1 - 0.5 * f * f2);
    s
}

/// `find_cusp`: the (lightness, chroma) of the most saturated colour at this hue.
fn find_cusp(a: f32, b: f32) -> (f32, f32) {
    let s_cusp = max_saturation(a, b);
    let rgb = oklab_to_linear_srgb(1.0, s_cusp * a, s_cusp * b);
    let l_cusp = (1.0 / rgb[0].max(rgb[1]).max(rgb[2])).cbrt();
    (l_cusp, l_cusp * s_cusp)
}

/// `find_gamut_intersection`: how far along the line from `(l0, 0)` toward
/// `(l1, c1)` the sRGB gamut boundary lies.
fn gamut_intersection(a: f32, b: f32, l1: f32, c1: f32, l0: f32, cusp: (f32, f32)) -> f32 {
    let (cusp_l, cusp_c) = cusp;
    if (l1 - l0) * cusp_c - (cusp_l - l0) * c1 <= 0.0 {
        // Lower half: the triangle approximation is exact enough.
        return cusp_c * l0 / (c1 * cusp_l + cusp_c * (l0 - l1));
    }

    // Upper half: intersect the triangle, then one Halley step onto the real
    // boundary, which is curved.
    let mut t = cusp_c * (l0 - 1.0) / (c1 * (cusp_l - 1.0) + cusp_c * (l0 - l1));
    let (dl, dc) = (l1 - l0, c1);

    let k_l = 0.396_337_78 * a + 0.215_803_76 * b;
    let k_m = -0.105_561_346 * a - 0.063_854_17 * b;
    let k_s = -0.089_484_18 * a - 1.291_485_5 * b;

    let l_dt = dl + dc * k_l;
    let m_dt = dl + dc * k_m;
    let s_dt = dl + dc * k_s;

    let l = l0 * (1.0 - t) + t * l1;
    let c = t * c1;

    let l_ = l + c * k_l;
    let m_ = l + c * k_m;
    let s_ = l + c * k_s;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    let ldt = 3.0 * l_dt * l_ * l_;
    let mdt = 3.0 * m_dt * m_ * m_;
    let sdt = 3.0 * s_dt * s_ * s_;
    let ldt2 = 6.0 * l_dt * l_dt * l_;
    let mdt2 = 6.0 * m_dt * m_dt * m_;
    let sdt2 = 6.0 * s_dt * s_dt * s_;

    let step = |w: [f32; 3]| -> (f32, f32) {
        let v = w[0] * l3 + w[1] * m3 + w[2] * s3 - 1.0;
        let v1 = w[0] * ldt + w[1] * mdt + w[2] * sdt;
        let v2 = w[0] * ldt2 + w[1] * mdt2 + w[2] * sdt2;
        let u = v1 / (v1 * v1 - 0.5 * v * v2);
        (u, -v * u)
    };
    let (u_r, t_r) = step([4.076_741_7, -3.307_711_6, 0.230_969_94]);
    let (u_g, t_g) = step([-1.268_438, 2.609_757_4, -0.341_319_4]);
    let (u_b, t_b) = step([-0.004_196_086_3, -0.703_418_6, 1.707_614_7]);

    let pick = |u: f32, t: f32| if u >= 0.0 { t } else { f32::MAX };
    t += pick(u_r, t_r).min(pick(u_g, t_g)).min(pick(u_b, t_b));
    t
}

/// `get_ST_mid`: the fitted mid-chroma shape, hue-dependent.
fn st_mid(a: f32, b: f32) -> (f32, f32) {
    let s = 0.115_169_93
        + 1.0
            / (7.447_789_7
                + 4.159_012_4 * b
                + a * (-2.195_573_5
                    + 1.751_984 * b
                    + a * (-2.137_049_5 - 10.023_011 * b
                        + a * (-4.248_945_6 + 5.387_708 * b + 4.698_91 * a))));
    let t = 0.112_396_42
        + 1.0
            / (1.613_203_2 - 0.681_243_8 * b
                + a * (0.403_706_12
                    + 0.901_481_2 * b
                    + a * (-0.270_879_43
                        + 0.612_239_9 * b
                        + a * (0.002_992_15 - 0.453_995_68 * b - 0.146_618_72 * a))));
    (s, t)
}

/// `get_Cs`: the three chroma anchors saturation interpolates between —
/// `(C_0, C_mid, C_max)`.
fn chromas(l: f32, a_: f32, b_: f32) -> (f32, f32, f32) {
    let cusp = find_cusp(a_, b_);
    let c_max = gamut_intersection(a_, b_, l, 1.0, l, cusp);

    // `to_ST`
    let st_max = (cusp.1 / cusp.0, cusp.1 / (1.0 - cusp.0));
    let k = c_max / (l * st_max.0).min((1.0 - l) * st_max.1);

    let c_mid = {
        let (s, t) = st_mid(a_, b_);
        let ca = l * s;
        let cb = (1.0 - l) * t;
        // A soft minimum rather than a hard triangle, so chroma varies smoothly.
        0.9 * k
            * (1.0 / (1.0 / (ca * ca * ca * ca) + 1.0 / (cb * cb * cb * cb)))
                .sqrt()
                .sqrt()
    };

    let c0 = {
        // Hue-independent by construction; the reference picks these as the average
        // S and T.
        let ca = l * 0.4;
        let cb = (1.0 - l) * 0.8;
        (1.0 / (1.0 / (ca * ca) + 1.0 / (cb * cb))).sqrt()
    };

    (c0, c_mid, c_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb(h: f32, s: f32, l: f32) -> [f32; 3] {
        to_srgb(Okhsl { h, s, l })
    }

    /// Relative luminance, for asking "how bright does this read".
    fn luma(c: [f32; 3]) -> f32 {
        // On the encoded values, which is what the eye is judging on screen.
        0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
    }

    #[test]
    fn zero_saturation_is_neutral() {
        for l in [0.1f32, 0.3, 0.5, 0.75, 0.95] {
            let c = srgb(210.0, 0.0, l);
            assert!(
                (c[0] - c[1]).abs() < 1e-4 && (c[1] - c[2]).abs() < 1e-4,
                "l={l} gave a cast: {c:?}"
            );
        }
    }

    #[test]
    fn hue_does_not_change_how_bright_it_reads() {
        // The property the whole space was chosen for, and the reason an RGB or HSL
        // picker would be the wrong control: a mount must not change weight against
        // the print when its hue is moved.
        //
        // Checked at a moderate saturation — at s = 1 the gamut boundary itself is
        // ragged and no space can make that flat.
        for l in [0.4f32, 0.7, 0.9] {
            let lums: Vec<f32> = (0..12)
                .map(|i| luma(srgb(i as f32 * 30.0, 0.5, l)))
                .collect();
            let lo = lums.iter().cloned().fold(f32::MAX, f32::min);
            let hi = lums.iter().cloned().fold(f32::MIN, f32::max);
            assert!(
                hi - lo < 0.10,
                "at l={l} lightness swung {:.3} across the hue circle: {lums:?}",
                hi - lo
            );
        }
    }

    #[test]
    fn hsl_would_have_failed_that() {
        // The counter-example, so the test above is known to be measuring
        // something. Plain HSL at fixed L swings hard: full yellow against full
        // blue is not remotely the same brightness.
        fn hsl(h: f32, s: f32, l: f32) -> [f32; 3] {
            let c = (1.0 - (2.0 * l - 1.0f32).abs()) * s;
            let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
            let m = l - c / 2.0;
            let (r, g, b) = match (h as u32) / 60 {
                0 => (c, x, 0.0),
                1 => (x, c, 0.0),
                2 => (0.0, c, x),
                3 => (0.0, x, c),
                4 => (x, 0.0, c),
                _ => (c, 0.0, x),
            };
            [r + m, g + m, b + m]
        }
        let lums: Vec<f32> = (0..12)
            .map(|i| luma(hsl(i as f32 * 30.0, 1.0, 0.5)))
            .collect();
        let lo = lums.iter().cloned().fold(f32::MAX, f32::min);
        let hi = lums.iter().cloned().fold(f32::MIN, f32::max);
        assert!(hi - lo > 0.5, "HSL was flatter than expected: {lums:?}");
    }

    #[test]
    fn lightness_is_monotonic() {
        let mut last = -1.0;
        for i in 0..=20 {
            let l = i as f32 / 20.0;
            let v = luma(srgb(120.0, 0.3, l));
            assert!(v >= last - 1e-4, "l={l} went backwards: {v} after {last}");
            last = v;
        }
    }

    #[test]
    fn everything_stays_inside_the_gamut() {
        // What OKHSL's normalised saturation buys: s = 1 is the boundary, so no
        // combination should need clipping. A value outside [0,1] means the cusp or
        // the intersection is wrong.
        for hi in 0..36 {
            for si in 0..=10 {
                for li in 1..20 {
                    let c = srgb(hi as f32 * 10.0, si as f32 / 10.0, li as f32 / 20.0);
                    for v in c {
                        assert!(
                            (-0.002..=1.002).contains(&v),
                            "h={} s={} l={} left the gamut: {c:?}",
                            hi * 10,
                            si as f32 / 10.0,
                            li as f32 / 20.0
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_ends_are_black_and_white() {
        assert_eq!(srgb(0.0, 0.5, 1.0), [1.0, 1.0, 1.0]);
        assert_eq!(srgb(0.0, 0.5, 0.0), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn srgb_round_trips_through_okhsl() {
        // The hex field and the colour wheel both hand sRGB back, so a colour typed
        // in and then read out again must be the colour typed in. Checked on the
        // Rising mat values, because those are what the control is *for* and they
        // are all near-white — where the chroma machinery is at its most delicate.
        for hex in [
            [0xf0, 0xf0, 0xee],
            [0xf6, 0xf5, 0xee],
            [0xf5, 0xf1, 0xe5],
            [0xe5, 0xd8, 0xbd],
            [0xa0, 0x99, 0x93],
            [0x34, 0x33, 0x33],
            [0xff, 0xff, 0xff],
            [0x00, 0x00, 0x00],
        ] {
            let rgb = hex.map(|c| c as f32 / 255.0);
            let back = to_srgb(from_srgb(rgb));
            for i in 0..3 {
                assert!(
                    (rgb[i] - back[i]).abs() < 0.004,
                    "{hex:02x?} channel {i}: in {} out {}",
                    rgb[i],
                    back[i]
                );
            }
        }
    }

    #[test]
    fn a_grey_comes_back_with_no_saturation_rather_than_a_hue_from_rounding() {
        // A neutral has no hue to recover, and `atan2` on two near-zero numbers will
        // happily invent one. Every Rising white is close enough to neutral for this
        // to matter, and a mount that acquired a faint hue on a round trip would be
        // the one defect this whole space was chosen to avoid.
        for v in [0.0, 0.18, 0.5, 0.9, 1.0] {
            let c = from_srgb([v, v, v]);
            assert!(c.s.abs() < 1.0e-3, "grey {v} came back with s = {}", c.s);
        }
    }

    #[test]
    fn the_toe_round_trips() {
        for i in 0..=20 {
            let x = i as f32 / 20.0;
            assert!(
                (toe(toe_inv(x)) - x).abs() < 1e-4,
                "toe is not an inverse at {x}"
            );
        }
    }

    #[test]
    fn the_default_mount_is_absolute_white() {
        // What `Settings` ships: hue 0, no saturation, l = 1. Mathematical white.
        let c = srgb(0.0, 0.0, 1.0);
        assert!((c[0] - 1.0).abs() < 1e-4, "not white: {c:?}");
        assert!((c[0] - c[2]).abs() < 1e-4, "not neutral: {c:?}");
    }
}
