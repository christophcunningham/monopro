//! The display transform, on the CPU.
//!
//! This is the reference implementation. `display.wgsl` mirrors it, and the
//! histogram calls it directly -- which is what makes "the histogram tracks what
//! is on screen" a property rather than a hope. If the two ever disagree, the
//! shader is wrong.
//!
//! Three separate concerns, deliberately not conflated (the handoff is emphatic
//! about this):
//!
//! 1. **Tone mapping** answers "how do I fit scene range into display range".
//! 2. **Transfer function** answers "what signal does this monitor expect".
//! 3. **Display ICC** is neutrality, and is not implemented yet.
//!
//! Dither is deliberately absent here. It is a quantisation-stage operation on the
//! encoded value and has zero mean, so including it would add noise to the
//! histogram without moving it.

use crate::params::{AgxParams, DisplayParams, ToneMap};

// ── The AgX curve ────────────────────────────────────────────────────────────
//
// A piecewise sigmoid, matching darktable's `agx` module at its defaults, which in
// turn matches Blender's AgX. **Not the 6th-order polynomial approximation** that
// three.js and older Blender builds ship.
//
// The approximation is a regression *fitted to* this curve, and it is measurably
// off: it places middle grey at 0.2145 instead of 0.18 — a quarter stop bright —
// while lifting shadows and crushing highlights, i.e. lower contrast throughout.
// Some published coefficient sets are also non-monotone near zero and return
// 1.0093 for an input of 1.0; Godot had to fix exactly that. This formulation has
// none of those failure modes: it is monotone by construction and hits 0 and 1
// exactly at the ends.
//
//     sigmoid(u, p)  = u / (1 + u^p)^(1/p)
//     segment(x, s, p) = s · sigmoid(slope·(x - pivot_x)/s, p) + pivot_y
//
// Below the pivot the scale is **negative**, which is what mirrors the toe: both
// `(x - pivot_x)` and the scale are negative, so the sigmoid's argument stays
// positive and `powf` never sees a negative base. That is darktable's trick and it
// is why no separate reflected code path is needed.
//
// The constants below are derived, not chosen — `agx_constants_match_the_derivation`
// recomputes them from darktable's `_scale` so they cannot silently go stale.

/// Normalised log position of middle grey: `10 / 16.5`.
#[cfg(test)]
const AGX_PIVOT_X: f32 = 0.606;
/// Curve-space value at the pivot, `0.18^(1/2.2)`. Raising this to `AGX_CURVE_GAMMA`
/// returns exactly 0.18 — which is why AgX maps middle grey to itself.
const AGX_PIVOT_Y: f32 = 0.458_656_45;
/// Slope at the pivot. darktable's `contrast`, default 3.0. The gamma compensation
/// factor is exactly 1.0 at the default gamma, so it drops out here.
#[cfg(test)]
const AGX_SLOPE: f32 = 3.0;
#[cfg(test)]
const AGX_TOE_POWER: f32 = 1.5;
#[cfg(test)]
const AGX_SHOULDER_POWER: f32 = 3.3;
/// Negative: this is the mirror that makes the toe work. See above.
#[cfg(test)]
const AGX_TOE_SCALE: f32 = -0.502_016_55;
#[cfg(test)]
const AGX_SHOULDER_SCALE: f32 = 0.554_466_9;

fn agx_sigmoid(u: f32, power: f32) -> f32 {
    u / (1.0 + u.powf(power)).powf(1.0 / power)
}

fn agx_scale(lx: f32, ly: f32, tx: f32, ty: f32, slope: f32, power: f32) -> f32 {
    let eps = 1.0e-6;
    let pr = slope * (lx - tx).max(eps);
    let ar = (ly - ty).max(eps);
    let base = (ar.powf(-power) - pr.powf(-power)).max(eps);
    base.powf(-1.0 / power).min(1.0e9)
}

fn agx_segment(x: f32, pivot_x: f32, scale: f32, slope: f32, power: f32) -> f32 {
    scale * agx_sigmoid(slope * (x - pivot_x) / scale, power) + AGX_PIVOT_Y
}

/// The curve, in normalised log position -> curve space. Both in `[0, 1]`.
fn agx_curve_with(x: f32, params: AgxParams) -> f32 {
    let p = params.normalized();
    let pivot_x = -p.black_ev / (p.white_ev - p.black_ev);
    let toe_scale = -agx_scale(
        1.0,
        1.0,
        1.0 - pivot_x,
        1.0 - AGX_PIVOT_Y,
        p.contrast,
        p.toe_power,
    );
    let shoulder_scale = agx_scale(1.0, 1.0, pivot_x, AGX_PIVOT_Y, p.contrast, p.shoulder_power);
    let r = if x < pivot_x {
        agx_segment(x, pivot_x, toe_scale, p.contrast, p.toe_power)
    } else if x > pivot_x {
        agx_segment(x, pivot_x, shoulder_scale, p.contrast, p.shoulder_power)
    } else {
        AGX_PIVOT_Y
    };
    r.clamp(0.0, 1.0)
}

#[cfg(test)]
fn agx_curve(x: f32) -> f32 {
    agx_curve_with(x, AgxParams::DEFAULT)
}

/// The gamma the AgX curve is shaped in. darktable's `curve_gamma`, default 2.2.
/// **Not** `DisplayParams::gamma`, and not a user setting — see `agx`.
const AGX_CURVE_GAMMA: f32 = 2.2;

/// AgX applied to a single scene-referred channel. Returns **display-linear**.
///
/// **A monochrome pipeline needs none of AgX's matrix work.** The inset and outset
/// matrices exist to rotate chroma inward before the sigmoid and back out after, so
/// that per-channel compression does not produce the hue skews that naive tone
/// curves do. With one channel there is no chroma to rotate: the log encode and the
/// sigmoid are the entire transform. This is a real simplification, not a shortcut.
///
/// **The trailing `powf(AGX_CURVE_GAMMA)` is load-bearing and easy to mistake for a
/// stray line.** The curve is *shaped* in a gamma-encoded space — that is what makes
/// its S look right — and the formulation linearises it afterwards. darktable does
/// the same and exposes the exponent as `curve gamma`. Omitting it leaves the output
/// display-encoded, the transfer function then encodes it a second time, and middle
/// grey lands at 0.73 instead of 0.46: an image that reads washed out and milky
/// rather than obviously broken. Keeping the linearisation here is also what keeps
/// the handoff's three concerns genuinely separate — tone mapping produces *light*,
/// and the transfer function alone decides how that light is coded for a monitor.
///
/// The pivot is what ties it together: `AGX_PIVOT_Y^AGX_CURVE_GAMMA` is exactly
/// 0.18, so **AgX maps middle grey to itself**. That is a structural property of the
/// formulation, not a fitted coincidence, and `agx_maps_middle_grey_to_itself` pins
/// it.
pub fn agx_with(v: f32, params: AgxParams) -> f32 {
    let p = params.normalized();
    let grey_ev = 0.18f32.log2();
    let min_ev = grey_ev + p.black_ev;
    let max_ev = grey_ev + p.white_ev;
    // Its own log2 window, by design: AgX bypasses any black/white point clip and
    // defines the range it maps from.
    let ev = v.max(1.0e-10).log2().clamp(min_ev, max_ev);
    let x = (ev - min_ev) / (max_ev - min_ev);
    agx_curve_with(x, p).powf(AGX_CURVE_GAMMA)
}

/// AgX at its photographic defaults. Kept as the small public reference helper
/// used by tests and callers that do not need an editable display parameter.
pub fn agx(v: f32) -> f32 {
    agx_with(v, AgxParams::DEFAULT)
}

/// Scene-referred -> **display-linear** `[0, 1]`. Tone mapping only.
///
/// This is the branch point between the screen and a file. Everything upstream is
/// shared; everything downstream is container-specific — the viewport applies a
/// transfer function and dithers to 8 bits, while export applies the L\* TRC of its
/// ICC container at 16 bits. They agree because a colour-managed viewer decodes both
/// to the same linear light, **not** because they share a raw encoding.
///
/// The clamp belongs here and nowhere upstream. Exposure does not clamp; the curve
/// does not clamp; this does.
pub fn tone_map(v: f32, tm: ToneMap) -> f32 {
    match tm {
        ToneMap::Clip => v.clamp(0.0, 1.0),
        ToneMap::Shoulder {
            threshold,
            strength,
        } => shoulder(v, threshold, strength),
        ToneMap::Agx(params) => agx_with(v, params),
    }
}

/// Linear below `threshold`, exponential roll-off above, asymptotic to 1.0.
///
/// ```text
///   soft(x) = t + (1-t) * (1 - exp(-(x-t)/(1-t)))
/// ```
///
/// Chosen over the more obvious Reinhard-on-the-excess because it is **C1
/// continuous at the threshold by construction**: its derivative there is exactly
/// 1.0, matching the linear segment below. A slope discontinuity at the knee shows
/// up as a visible edge in a smooth gradient — a sky with a seam in it — and is
/// precisely the sort of artefact that gets blamed on the sensor.
///
/// `strength` blends between a hard clip and the full roll-off. Both endpoints have
/// slope 1.0 at the threshold, so every blend of them does too, and the knee stays
/// seamless at any setting.
pub fn shoulder(v: f32, threshold: f32, strength: f32) -> f32 {
    let t = threshold.clamp(0.0, 0.999);
    let hard = v.clamp(0.0, 1.0);
    if v <= t {
        // Passed through exactly. This is the whole point of the mode: nothing
        // below the threshold moves, so midtones and shadows are untouched.
        return hard;
    }
    let w = 1.0 - t;
    let soft = t + w * (1.0 - (-(v - t) / w).exp());
    hard + (soft - hard) * strength.clamp(0.0, 1.0)
}

/// Display-linear value -> display-encoded `[0, 1]`, for the monitor.
///
/// Kept separate from [`encode`] for callers that have already performed the tonal
/// transform. The print loupe is one such caller: its emulation tail begins with the
/// export's tone map, then adds grain and sharpening, and only needs the monitor
/// transfer when that finished tile is drawn on screen.
pub fn monitor_encode(v: f32, gamma: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(1.0 / gamma.max(0.01))
}

/// Scene-referred value -> display-encoded `[0, 1]`, for the screen.
pub fn encode(v: f32, p: &DisplayParams) -> f32 {
    monitor_encode(tone_map(v, p.applied_tone_map()), p.gamma)
}

/// Display-linear -> L\*, normalised to `[0, 1]`.
///
/// The CIE 1976 lightness function, and the TRC of both export containers:
/// `monostar.icc` (GRAY) and eciRGB v2 (RGB). Their `kTRC` is an ICC parametric
/// curve of type 3 with exactly these constants, so encoding here and tagging there
/// is a matched pair rather than an approximation.
///
/// L\* is **export only**. The display encode is a plain gamma. Conflating the two
/// is a mistake this project has already made once: the prototype L\*-encoded for
/// display, the macOS compositor decoded it as sRGB, and midtones lifted about
/// 3 L\* versus a correctly tagged TIFF.
pub fn lstar_encode(v: f32) -> f32 {
    const E: f32 = 0.008_856;
    const K: f32 = 903.3;
    let y = v.clamp(0.0, 1.0);
    let l = if y > E {
        116.0 * y.cbrt() - 16.0
    } else {
        K * y
    };
    (l / 100.0).clamp(0.0, 1.0)
}

/// Display-linear -> 16-bit L\*-encoded sample, ready for a TIFF tagged with an
/// L\* TRC profile.
pub fn lstar_u16(v: f32) -> u16 {
    (lstar_encode(v) * 65535.0 + 0.5).clamp(0.0, 65535.0) as u16
}

/// Display-linear -> sRGB-encoded `[0, 1]`. IEC 61966-2-1, exactly.
///
/// # This exists for **proofs**, and only for proofs
///
/// Every master this app writes is L\*-encoded, because a master goes into a
/// colour-managed print workflow where its profile is honoured. A proof does not: it
/// goes into Mail, Preview, a browser, a phone. Unmanaged and semi-managed viewers
/// routinely ignore a *greyscale* ICC profile, and an L\*-encoded file rendered as if
/// it were sRGB comes out visibly light — which is the same class of defect this
/// module's own note records, where the prototype L\*-encoded for display and the
/// compositor lifted midtones about 3 L\*.
///
/// So the rule is **purpose decides encoding**: master to a managed workflow in L\*,
/// proof to an uncontrolled screen in sRGB. That is deliberately *not* the split
/// milestone 9b rejected, which was depth deciding encoding — "8-bit means gamma" —
/// and had no principle behind it. This one is a sentence long and survives the
/// colour transition unchanged.
///
/// The linear segment below 0.0031308 is not decoration: a pure power function has an
/// infinite slope at zero, which quantises the first few codes of a gradient into a
/// step. This app writes extended smooth gradients, so that end of the curve is
/// exactly where it would show.
pub fn srgb_encode(v: f32) -> f32 {
    let y = v.clamp(0.0, 1.0);
    if y <= 0.003_130_8 {
        12.92 * y
    } else {
        1.055 * y.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(tone_map: ToneMap) -> DisplayParams {
        DisplayParams {
            enabled: true,
            tone_map,
            gamma: 2.2,
            dither: true,
        }
    }

    #[test]
    fn srgb_matches_the_published_curve_at_its_landmarks() {
        // IEC 61966-2-1. The knee at 0.0031308 is where the linear segment hands over
        // to the power function, and the two halves must agree there or the first few
        // codes of every gradient step.
        // 18% grey lands at 0.4612, which is the number every sRGB reference quotes.
        for (linear, encoded) in [(0.0, 0.0), (1.0, 1.0), (0.5, 0.735_356_9), (0.18, 0.461_1)] {
            let got = srgb_encode(linear);
            assert!(
                (got - encoded).abs() < 1.0e-3,
                "srgb_encode({linear}) = {got}, expected {encoded}"
            );
        }
        let knee = 0.003_130_8;
        let below = srgb_encode(knee - 1.0e-7);
        let above = srgb_encode(knee + 1.0e-7);
        assert!(
            (below - above).abs() < 1.0e-4,
            "the curve has a step at the knee"
        );
    }

    #[test]
    fn l_star_sits_above_srgb_through_the_midtones() {
        // The whole reason a proof is not simply the master retagged: an L*-encoded
        // file rendered as sRGB comes out lighter, and the gap is widest exactly where
        // a print is judged. This is the ~3 L* lift the module note records.
        for v in [0.05, 0.18, 0.4, 0.6] {
            let (l, s) = (lstar_encode(v), srgb_encode(v));
            assert!(l > s, "at {v}: L* {l} should sit above sRGB {s}");
        }
        // And they agree at the ends, so black and white are the same in both.
        for v in [0.0, 1.0] {
            assert!((lstar_encode(v) - srgb_encode(v)).abs() < 1.0e-6);
        }
    }

    #[test]
    fn clip_mode_is_a_plain_gamma_encode() {
        let p = params(ToneMap::Clip);
        assert!((encode(1.0, &p) - 1.0).abs() < 1.0e-6);
        assert!((encode(0.0, &p)).abs() < 1.0e-6);
        // Middle grey through 2.2 gamma.
        assert!((encode(0.18, &p) - 0.18f32.powf(1.0 / 2.2)).abs() < 1.0e-6);
    }

    #[test]
    fn an_already_mapped_value_only_needs_the_monitor_transfer() {
        let p = params(ToneMap::AGX_DEFAULT);
        for scene in [0.02, 0.18, 0.6, 1.5, 4.0] {
            let mapped = tone_map(scene, p.applied_tone_map());
            assert!(
                (monitor_encode(mapped, p.gamma) - encode(scene, &p)).abs() < 1.0e-6,
                "the screen path disagrees after the tonal transform at {scene}"
            );
        }
    }

    #[test]
    fn display_bypass_removes_tone_mapping_but_keeps_monitor_gamma() {
        let mut p = params(ToneMap::AGX_DEFAULT);
        p.enabled = false;
        p.gamma = 1.8;

        let scene = 0.18_f32;
        let expected = scene.powf(1.0 / p.gamma);
        assert!(
            (encode(scene, &p) - expected).abs() < 1.0e-6,
            "bypass should be Clip followed by the selected monitor gamma"
        );
        assert_eq!(
            p.tone_map,
            ToneMap::AGX_DEFAULT,
            "comparing before/after must not destroy the selected transform"
        );
    }

    #[test]
    fn clip_mode_discards_headroom() {
        // The honest cost of Clip: everything above 1.0 is gone. This is why AgX
        // exists as an option.
        let p = params(ToneMap::Clip);
        assert_eq!(encode(1.0, &p), encode(3.5, &p));
    }

    #[test]
    fn agx_preserves_ordering_above_one() {
        // The reason to reach for AgX at all: scene values above 1.0 stay distinct
        // instead of collapsing onto white.
        let p = params(ToneMap::AGX_DEFAULT);
        let a = encode(1.0, &p);
        let b = encode(2.0, &p);
        let c = encode(3.5, &p);
        assert!(a < b && b < c, "AgX flattened the headroom: {a} {b} {c}");
    }

    #[test]
    fn agx_is_monotone_across_the_scene_range() {
        let p = params(ToneMap::AGX_DEFAULT);
        let mut prev = f32::NEG_INFINITY;
        for i in 0..2000 {
            let v = (i as f32 / 2000.0 * 20.0 - 16.0).exp2();
            let out = encode(v, &p);
            assert!(out >= prev - 1.0e-6, "AgX inverted at {v}");
            prev = out;
        }
    }

    #[test]
    fn editable_agx_extremes_remain_monotone() {
        for raw in [
            AgxParams {
                auto_range: false,
                black_ev: -16.0,
                white_ev: 1.0,
                contrast: 0.5,
                toe_power: 0.5,
                shoulder_power: 8.0,
            },
            AgxParams {
                auto_range: false,
                black_ev: -1.0,
                white_ev: 12.0,
                contrast: 0.5,
                toe_power: 8.0,
                shoulder_power: 0.5,
            },
        ] {
            let p = params(ToneMap::Agx(raw));
            let mut previous = f32::NEG_INFINITY;
            for i in 0..20_000 {
                let scene = (i as f32 / 20_000.0 * 32.0 - 18.0).exp2();
                let out = encode(scene, &p);
                assert!(out >= previous - 1.0e-6, "curve inverted for {raw:?}");
                previous = out;
            }
        }
    }

    #[test]
    fn moving_agx_white_changes_highlight_placement() {
        let broad = agx_with(1.0, AgxParams::DEFAULT);
        let narrow = agx_with(
            1.0,
            AgxParams {
                white_ev: 3.0,
                ..AgxParams::DEFAULT
            },
        );
        assert!(
            narrow > broad,
            "a nearer white point did not lift the highlight"
        );
    }

    #[test]
    fn agx_stays_in_range() {
        let p = params(ToneMap::AGX_DEFAULT);
        for v in [-1.0f32, 0.0, 1.0e-9, 0.18, 1.0, 100.0, 1.0e6] {
            let out = encode(v, &p);
            assert!((0.0..=1.0).contains(&out), "AgX out of range at {v}: {out}");
        }
    }

    #[test]
    fn agx_rolls_off_rather_than_clipping() {
        // A hard clip would put 1.0 and everything above it at the same place, and
        // would put 1.0 at full white. AgX should place scene 1.0 well below white.
        let p = params(ToneMap::AGX_DEFAULT);
        assert!(
            encode(1.0, &p) < 0.95,
            "AgX put scene white at display white"
        );
    }

    #[test]
    fn agx_maps_middle_grey_to_itself() {
        // THE property of the real formulation, and the single sharpest check that
        // the curve is AgX rather than something AgX-shaped. The pivot is placed at
        // middle grey in log space with a curve-space value of 0.18^(1/2.2), so
        // linearising returns exactly 0.18.
        //
        // The 6th-order polynomial approximation this replaced returned 0.2145 —
        // a quarter stop bright — while looking entirely plausible.
        let out = agx(0.18);
        assert!(
            (out - 0.18).abs() < 0.001,
            "AgX no longer maps middle grey to itself: 0.18 -> {out}"
        );
    }

    #[test]
    fn agx_constants_match_the_derivation() {
        // The scale constants are derived from darktable's `_scale`, not chosen. If
        // the pivot, slope or powers are ever changed, this recomputes them and says
        // so rather than leaving stale magic numbers in place.
        fn scale(lx: f64, ly: f64, tx: f64, ty: f64, slope: f64, power: f64) -> f64 {
            let eps = 1e-6f64;
            let pr = slope * (lx - tx).max(eps);
            let ar = (ly - ty).max(eps);
            let base = (ar.powf(-power) - pr.powf(-power)).max(eps);
            base.powf(-1.0 / power).min(1e9)
        }
        let (px, py) = (AGX_PIVOT_X as f64, AGX_PIVOT_Y as f64);
        let slope = AGX_SLOPE as f64;

        // pivot_y is 0.18 raised to 1/curve_gamma.
        let expected_py = 0.18f64.powf(1.0 / AGX_CURVE_GAMMA as f64);
        assert!(
            (py - expected_py).abs() < 1e-6,
            "pivot_y drifted: {py} vs {expected_py}"
        );

        // Toe is derived in a mirrored coordinate space, then negated.
        let toe = -scale(1.0, 1.0, 1.0 - px, 1.0 - py, slope, AGX_TOE_POWER as f64);
        let shoulder = scale(1.0, 1.0, px, py, slope, AGX_SHOULDER_POWER as f64);
        assert!(
            (AGX_TOE_SCALE as f64 - toe).abs() < 1e-6,
            "toe scale stale: {AGX_TOE_SCALE} vs derived {toe}"
        );
        assert!(
            (AGX_SHOULDER_SCALE as f64 - shoulder).abs() < 1e-6,
            "shoulder scale stale: {AGX_SHOULDER_SCALE} vs derived {shoulder}"
        );

        // And neither fallback curve is needed at these defaults, which is why only
        // the two sigmoid segments are implemented.
        assert!(py / px < slope, "toe would need the convex fallback");
        assert!(
            (1.0 - py) / (1.0 - px) < slope,
            "shoulder would need the concave fallback"
        );
    }

    #[test]
    fn agx_defaults_match_the_curve_they_replaced() {
        // **The claim the controls shipped on, stated so it can fail.** Exposing
        // White EV, Black EV, Contrast and Shape turned three fixed constants into
        // arithmetic, and the intent was that an old sidecar saying only
        // `ToneMap="agx"` opens looking the same. It very nearly does, and the
        // residue is worth pinning rather than rounding away: `AGX_PIVOT_X` was the
        // literal `0.606`, while the parameterised curve computes `10 / 16.5`
        // exactly, so the new default curve is the one the old constant was a
        // three-decimal transcription of.
        //
        // A twentieth of an 8-bit code, measured. That is small enough to be
        // invisible and large enough that `the_whole_chain_is_stable` in raw-gpu
        // saw it — 22 of its 2304 pixels move by one code — so this is where the
        // budget lives, not there.
        fn old_curve(x: f32) -> f32 {
            fn seg(x: f32, scale: f32, power: f32) -> f32 {
                scale * agx_sigmoid(AGX_SLOPE * (x - AGX_PIVOT_X) / scale, power) + AGX_PIVOT_Y
            }
            let r = if x < AGX_PIVOT_X {
                seg(x, AGX_TOE_SCALE, AGX_TOE_POWER)
            } else if x > AGX_PIVOT_X {
                seg(x, AGX_SHOULDER_SCALE, AGX_SHOULDER_POWER)
            } else {
                AGX_PIVOT_Y
            };
            r.clamp(0.0, 1.0)
        }

        let mut worst = 0.0f32;
        for i in 0..=4096 {
            let x = i as f32 / 4096.0;
            worst = worst.max((agx_curve(x) - old_curve(x)).abs());
        }
        assert!(
            worst * 255.0 < 0.1,
            "the AgX defaults drifted from the fixed curve: {} codes",
            worst * 255.0
        );
    }

    #[test]
    fn agx_hits_both_endpoints_exactly() {
        // The polynomial approximation missed at both ends — some published
        // coefficient sets return 1.0093 at 1.0, which banded in Godot. A piecewise
        // sigmoid built from the endpoints cannot.
        assert!(
            agx_curve(0.0).abs() < 1e-6,
            "curve floor is {}",
            agx_curve(0.0)
        );
        assert!(
            (agx_curve(1.0) - 1.0).abs() < 1e-6,
            "curve ceiling is {}",
            agx_curve(1.0)
        );
    }

    #[test]
    fn agx_curve_is_monotone_over_its_whole_domain() {
        // Checked on the curve directly rather than through the log map, so a dip
        // anywhere in [0,1] is caught rather than stepped over.
        let mut prev = f32::NEG_INFINITY;
        for i in 0..=200_000 {
            let x = i as f32 / 200_000.0;
            let y = agx_curve(x);
            assert!(
                y >= prev - 1e-7,
                "AgX curve dips at x={x}: {y} after {prev}"
            );
            assert!(
                (0.0..=1.0).contains(&y),
                "AgX curve out of range at x={x}: {y}"
            );
            prev = y;
        }
    }

    #[test]
    fn lstar_matches_the_icc_parametric_curve() {
        // monostar.icc's kTRC is an ICC type-3 parametric curve with g=3,
        // a=1/1.16, b=0.16/1.16, c=1/9.033, d=0.08 — i.e. the inverse of this
        // function. Encoding here and tagging there has to be a matched pair, so
        // this checks against the ICC form directly rather than against itself.
        let icc_decode = |l: f32| -> f32 {
            let (g, a, b, c, d) = (3.0f32, 1.0 / 1.16, 0.16 / 1.16, 1.0 / 9.033, 0.08);
            if l >= d { (a * l + b).powf(g) } else { c * l }
        };
        for i in 0..=100 {
            let linear = i as f32 / 100.0;
            let round_tripped = icc_decode(lstar_encode(linear));
            assert!(
                (round_tripped - linear).abs() < 2e-3,
                "L* does not invert the ICC curve at {linear}: got {round_tripped}"
            );
        }
    }

    #[test]
    fn lstar_is_not_the_display_encode() {
        // These have been confused before, with a measurable ~3 L* midtone lift.
        // They are different functions and must stay different.
        let mid = 0.18;
        let display = encode(mid, &params(ToneMap::Clip));
        assert!(
            (lstar_encode(mid) - display).abs() > 0.02,
            "L* and the display gamma have converged; one of them is wrong"
        );
    }

    #[test]
    fn lstar_u16_spans_the_full_range() {
        assert_eq!(lstar_u16(0.0), 0);
        assert_eq!(lstar_u16(1.0), 65535);
        // Monotone, and no quantisation cliff.
        let mut prev = 0;
        for i in 0..=1000 {
            let v = lstar_u16(i as f32 / 1000.0);
            assert!(v >= prev, "L* u16 inverted at {i}");
            prev = v;
        }
    }

    #[test]
    fn the_shoulder_leaves_everything_below_the_threshold_alone() {
        // The defining property, and the whole reason to prefer it over AgX: it is
        // a highlight tool, not a look. If midtones move, it has become AgX with
        // extra steps.
        for v in [0.0f32, 0.05, 0.18, 0.5, 0.74] {
            let out = shoulder(v, 0.75, 1.0);
            assert!((out - v).abs() < 1e-6, "shoulder moved {v} to {out}");
        }
    }

    #[test]
    fn the_shoulder_has_no_visible_knee() {
        // C1 continuity at the threshold. A slope break here reads as a seam in a
        // smooth gradient — a hard edge across a sky — which is the artefact this
        // formulation was chosen to avoid.
        let t = 0.75f32;
        for strength in [0.0f32, 0.3, 0.8, 1.0] {
            let e = 1e-4;
            let below = (shoulder(t - e, t, strength) - shoulder(t - 2.0 * e, t, strength)) / e;
            let above = (shoulder(t + 2.0 * e, t, strength) - shoulder(t + e, t, strength)) / e;
            assert!(
                (below - above).abs() < 0.02,
                "slope breaks at the knee (strength {strength}): {below} vs {above}"
            );
        }
    }

    #[test]
    fn the_shoulder_keeps_headroom_distinguishable() {
        // The reason to use it at all: values above scene white must stay ordered
        // rather than collapsing onto one code.
        let (a, b, c) = (
            shoulder(1.0, 0.75, 1.0),
            shoulder(2.0, 0.75, 1.0),
            shoulder(3.5, 0.75, 1.0),
        );
        assert!(
            a < b && b < c,
            "shoulder flattened the headroom: {a} {b} {c}"
        );
        assert!(c <= 1.0, "shoulder exceeded display white: {c}");
    }

    #[test]
    fn shoulder_at_zero_strength_is_a_hard_clip() {
        for v in [0.5f32, 0.9, 1.0, 2.0, 3.5] {
            assert!(
                (shoulder(v, 0.75, 0.0) - v.clamp(0.0, 1.0)).abs() < 1e-6,
                "strength 0 was not a clip at {v}"
            );
        }
    }

    #[test]
    fn the_shoulder_is_monotone_and_bounded() {
        for (t, s) in [(0.5f32, 1.0f32), (0.75, 0.8), (0.9, 0.5), (0.0, 1.0)] {
            let mut prev = f32::NEG_INFINITY;
            for i in 0..4000 {
                let v = i as f32 / 500.0; // 0 .. 8
                let out = shoulder(v, t, s);
                assert!(
                    out >= prev - 1e-6,
                    "shoulder inverted at {v} (t={t}, s={s})"
                );
                assert!(
                    (0.0..=1.0).contains(&out),
                    "shoulder out of range at {v}: {out}"
                );
                prev = out;
            }
        }
    }

    #[test]
    fn the_shoulder_is_local_where_agx_is_global() {
        // The distinction that motivated the mode — but NOT measured at middle grey.
        //
        // An earlier version of this test checked that AgX moves 18% grey. It passed
        // only because the polynomial approximation was wrong: the real formulation
        // pivots *at* middle grey and maps it to itself, so AgX and Clip agree there
        // exactly. The test was pinning a bug.
        //
        // The genuine difference is in the shadows: AgX's toe bends everything below
        // the pivot, while the shoulder passes it through untouched.
        let p_shoulder = DisplayParams {
            enabled: true,
            tone_map: ToneMap::Shoulder {
                threshold: 0.75,
                strength: 0.8,
            },
            gamma: 2.2,
            dither: false,
        };
        let p_agx = params(ToneMap::AGX_DEFAULT);
        let p_clip = params(ToneMap::Clip);

        // Middle grey: AgX and Clip agree, by construction.
        assert!(
            (encode(0.18, &p_agx) - encode(0.18, &p_clip)).abs() < 1e-3,
            "AgX stopped preserving middle grey"
        );

        // A shadow tone: the shoulder is inert, AgX is not.
        let shadow = 0.05f32;
        assert!(
            (encode(shadow, &p_shoulder) - encode(shadow, &p_clip)).abs() < 1e-5,
            "shoulder touched the shadows"
        );
        assert!(
            (encode(shadow, &p_agx) - encode(shadow, &p_clip)).abs() > 0.02,
            "AgX's toe stopped bending the shadows"
        );

        // And in the highlights both act, which is the point of offering either.
        for p in [&p_shoulder, &p_agx] {
            assert!(
                encode(2.0, p) < encode(2.0, &p_clip),
                "no highlight roll-off"
            );
        }
    }

    #[test]
    fn negative_scene_values_encode_to_black_not_nan() {
        // Decode leaves sub-black noise negative on purpose; the display transform
        // is where that stops being visible, and it must not produce NaN doing it.
        for tm in [ToneMap::Clip, ToneMap::AGX_DEFAULT] {
            let out = encode(-0.01, &params(tm));
            assert!(out.is_finite(), "{tm:?} produced {out}");
            assert!(out < 0.02, "{tm:?} lifted negative input to {out}");
        }
    }
}
