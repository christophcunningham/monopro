//! OKLab out to a tagged RGB space — the boundary, and the only place primaries exist.
//!
//! # Why this module is small, and why it is late
//!
//! Chemical toning emits OKLab: a lightness and an `(a, b)` pair. **OKLab has no
//! primaries and no gamut**, so while the image is being toned it already lives
//! somewhere fully defined and no working space has to be chosen. The choice bites at
//! a *boundary*, and there are exactly two — the display, and the encode.
//!
//! So this is the encode boundary. Everything upstream is either a scalar luminance or
//! a device-independent perceptual triple, and nothing upstream knows what primaries
//! are. See the colour-transition brief in `docs/`.
//!
//! The corollary is worth stating because it is the trap this app has spent three
//! documents avoiding: **the conversion only ever runs outward**. There is no point
//! anywhere in the chain that computes luminance *from* RGB, which is what
//! `monopro-rs-handoff.md` forbids and what a working RGB space would have invited.
//!
//! # The matrix comes out of the profile the file is tagged with
//!
//! Not hardcoded and not derived from published chromaticities. Every RGB space here
//! ships as an embedded ICC profile whose `rXYZ` / `gXYZ` / `bXYZ` tags **are** the
//! RGB → XYZ matrix, D50-adapted because that is what an ICC profile connection space
//! is. Reading them means the pixel data and the tag describing it cannot disagree:
//! change the profile and the conversion follows, with no second place to remember.
//!
//! # The chain, and why D50 appears in the middle of it
//!
//! ```text
//! OKLab -> LMS -> XYZ (D65)  -> Bradford ->  XYZ (D50) -> profile^-1 -> linear RGB
//! ```
//!
//! OKLab is defined against D65. An ICC profile's primaries are D50. The adaptation
//! between them belongs here rather than being skipped, and skipping it is a plausible
//! mistake because on **neutrals it is invisible** — a grey stays grey either way — so
//! it would only ever show up as a slow drift in the hue of a toned print.
//!
//! The display path does *not* do this and is not inconsistent: a screen is D65, so
//! the display converts OKLab straight to sRGB's D65 primaries. A colour-managed
//! application opening a D50-tagged file adapts it back. The round trip agrees.
//!
//! # Gamut
//!
//! A cyanotype at chroma 0.15 in the shadows is outside sRGB, and outside eciRGB v2 at
//! some hues. [`fit_gamut`] scales the chroma down at **constant hue and lightness**
//! until it fits.
//!
//! Clipping the RGB afterwards is the alternative and it shifts hue: a blue that clips
//! its blue channel comes out purple. Losing saturation is the honest failure; losing
//! hue is not.
//!
//! **Bisection, not the analytic cusp, and that is a finding rather than an
//! expedience.** `okhsl::max_saturation` is a polynomial fit to *sRGB's* gamut
//! boundary — the coefficients are sRGB's — so it does not generalise to eciRGB v2 or
//! ProStar without refitting a polynomial per space. Bisection needs nothing but the
//! matrix, works for any primaries, and is what both boundaries can share.

use crate::okhsl;

/// RGB → XYZ (D50), and its inverse, as read from an ICC profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Primaries {
    to_xyz: [[f32; 3]; 3],
    from_xyz: [[f32; 3]; 3],
}

impl Primaries {
    /// Read `rXYZ` / `gXYZ` / `bXYZ` out of a profile.
    ///
    /// Returns `None` for a greyscale profile, which has no primaries — and that is a
    /// legitimate answer rather than an error: `monostar` is the untoned master and
    /// takes the scalar path.
    pub fn from_icc(icc: &[u8]) -> Option<Self> {
        let column = |sig: &[u8; 4]| -> Option<[f32; 3]> {
            let (off, len) = find_tag(icc, sig)?;
            // `XYZType`: 4 bytes signature, 4 reserved, then three s15Fixed16.
            (len >= 20).then(|| {
                [
                    s15(&icc[off + 8..off + 12]),
                    s15(&icc[off + 12..off + 16]),
                    s15(&icc[off + 16..off + 20]),
                ]
            })
        };
        let r = column(b"rXYZ")?;
        let g = column(b"gXYZ")?;
        let b = column(b"bXYZ")?;
        // Columns, so the matrix maps a column vector of RGB.
        let to_xyz = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
        Some(Self {
            to_xyz,
            from_xyz: invert(to_xyz)?,
        })
    }

    pub fn to_xyz_d50(&self) -> [[f32; 3]; 3] {
        self.to_xyz
    }

    /// XYZ (D50) → linear RGB. Unclamped, so the caller can see that it did not fit.
    pub fn linear_rgb(&self, xyz: [f32; 3]) -> [f32; 3] {
        apply(self.from_xyz, xyz)
    }
}

/// OKLab → XYZ, D65. Ottosson's inverse matrices, transcribed rather than recalled.
pub fn oklab_to_xyz_d65(l: f32, a: f32, b: f32) -> [f32; 3] {
    // M2^-1: Lab -> nonlinear LMS.
    #[allow(clippy::excessive_precision)]
    let lms_ = [
        l + 0.3963377774 * a + 0.2158037573 * b,
        l - 0.1055613458 * a - 0.0638541728 * b,
        l - 0.0894841775 * a - 1.2914855480 * b,
    ];
    let lms = [lms_[0].powi(3), lms_[1].powi(3), lms_[2].powi(3)];
    // M1^-1: LMS -> XYZ.
    // `#[allow]` rather than trimmed digits: these are Ottosson's published constants,
    // transcribed so they can be diffed against the reference. Rounding them to f32's
    // shortest distinct form would make that diff fail for a reader holding the paper.
    #[allow(clippy::excessive_precision)]
    const M1_INV: [[f32; 3]; 3] = [
        [1.2270138511, -0.5577999807, 0.2812561490],
        [-0.0405801784, 1.1122568696, -0.0716766787],
        [-0.0763812845, -0.4214819784, 1.5861632204],
    ];
    apply(M1_INV, lms)
}

/// Bradford chromatic adaptation, D65 → D50.
///
/// **Invisible on neutrals**, which is exactly why it must not be skipped: a grey stays
/// grey with or without it, so leaving it out would show up only as a slow drift in the
/// hue of a toned print and would never fail a test that used a grey ramp.
pub fn adapt_d65_to_d50(xyz: [f32; 3]) -> [f32; 3] {
    // The published Bradford D65 -> D50 matrix, transcribed. See `oklab_to_xyz_d65` on
    // why the digits are not trimmed.
    #[allow(clippy::excessive_precision)]
    const BRADFORD: [[f32; 3]; 3] = [
        [1.0478112, 0.0228866, -0.0501270],
        [0.0295424, 0.9904844, -0.0170491],
        [-0.0092345, 0.0150436, 0.7521316],
    ];
    apply(BRADFORD, xyz)
}

/// The OKLab lightness of a **neutral** at this display luminance.
///
/// Exactly the cube root, and worth a function because `L*/100` is the near-miss that
/// looks right: for a neutral `l = m = s = Y`, so every cube root is `Y^(1/3)` and
/// OKLab's three lightness coefficients sum to one.
pub fn oklab_lightness(y: f32) -> f32 {
    y.max(0.0).cbrt()
}

/// OKLab → linear RGB in `p`'s primaries, with the chroma brought inside the gamut at
/// constant hue and lightness. See the module note on why this is bisection.
pub fn oklab_to_linear(l: f32, a: f32, b: f32, p: &Primaries) -> [f32; 3] {
    // **A neutral bypasses the matrix, and this is not an optimisation.**
    //
    // Sent the long way round, a zero-chroma colour comes back with its three channels
    // differing by about 1e-4 — eight codes at 16 bits. The cause is not fixable by
    // being more careful: an ICC profile stores its primaries as s15Fixed16, so the
    // three columns do not sum to exactly the white point, and no arrangement of f32
    // arithmetic recovers what the quantisation threw away.
    //
    // So an untoned pixel is written as the same number three times, by construction.
    // That makes "an untoned print in an RGB container is the greyscale master with its
    // channels repeated" exactly true rather than true to within matrix rounding, which
    // is the claim `splat` has been making all along and the one the export tests
    // assert. `a_neutral_is_bit_exact_in_every_space` fails if this goes.
    if a == 0.0 && b == 0.0 {
        let y = l.max(0.0).powi(3).clamp(0.0, 1.0);
        return [y, y, y];
    }
    let at = |scale: f32| p.linear_rgb(adapt_d65_to_d50(oklab_to_xyz_d65(l, a * scale, b * scale)));
    let inside = |c: [f32; 3]| c.iter().all(|v| (-1e-6..=1.0 + 1e-6).contains(v));

    let full = at(1.0);
    if inside(full) {
        return full;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..GAMUT_STEPS {
        let mid = 0.5 * (lo + hi);
        if inside(at(mid)) { lo = mid } else { hi = mid }
    }
    let fitted = at(lo);
    [
        fitted[0].clamp(0.0, 1.0),
        fitted[1].clamp(0.0, 1.0),
        fitted[2].clamp(0.0, 1.0),
    ]
}

/// The chroma scale that just fits, for a hue and lightness. Exposed for tests and for
/// anything that wants to *report* how far out of gamut a tone is.
pub fn fit_gamut(l: f32, a: f32, b: f32, p: &Primaries) -> f32 {
    let at = |scale: f32| p.linear_rgb(adapt_d65_to_d50(oklab_to_xyz_d65(l, a * scale, b * scale)));
    let inside = |c: [f32; 3]| c.iter().all(|v| (-1e-6..=1.0 + 1e-6).contains(v));
    if inside(at(1.0)) {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..GAMUT_STEPS {
        let mid = 0.5 * (lo + hi);
        if inside(at(mid)) { lo = mid } else { hi = mid }
    }
    lo
}

/// Iterations of the gamut bisection.
///
/// **The display shader runs the same number**, and it has to: the viewport and the
/// export are the same picture or the module is not doing its job, and two searches
/// that stop at different points along a chroma ramp disagree by a code or two at every
/// saturated tone. See `display.wgsl`.
///
/// Twelve halvings put the residual chroma error at 1/4096 of the range, which is two
/// orders below an 8-bit code.
pub const GAMUT_STEPS: usize = 12;

/// XYZ (D65) → CIELAB. Returns `[L*, a*, b*]` on the familiar scales: L\* 0–100, and
/// a\*/b\* roughly ±128 with `+a` toward magenta and `+b` toward yellow.
///
/// **For the readouts, not for the pipeline.** Nothing here converts through CIELAB —
/// the model works in OKLab, which is perceptually better behaved for the mixing it
/// does. But a\* and b\* are the numbers a photographer already reads, and a footer
/// reporting OKLab's own ±0.3 would be a scale nobody could compare against anything.
pub fn xyz_d65_to_lab(xyz: [f32; 3]) -> [f32; 3] {
    // **The white this path actually produces, not the textbook D65.**
    //
    // Written as the constant `[0.950489, 1.0, 1.088840]` a neutral came back with
    // `b* = 0.019` — small, and not zero, and a footer reporting a cast on a grey is a
    // footer that sends someone hunting for one. The cause is that Ottosson's published
    // matrices are rounded, so `oklab_to_xyz_d65(1, 0, 0)` lands a rounding away from
    // the ideal illuminant, and dividing by the ideal leaves that difference in the
    // answer.
    //
    // Deriving the white from the transform makes a neutral read exactly zero by
    // construction. It is also the more correct statement: the reference white for a
    // conversion is whatever that conversion calls white.
    static WHITE: std::sync::LazyLock<[f32; 3]> =
        std::sync::LazyLock::new(|| oklab_to_xyz_d65(1.0, 0.0, 0.0));
    let f = |t: f32| {
        const E: f32 = 216.0 / 24389.0;
        const K: f32 = 24389.0 / 27.0;
        if t > E {
            t.cbrt()
        } else {
            (K * t + 16.0) / 116.0
        }
    };
    let (fx, fy, fz) = (
        f(xyz[0] / WHITE[0]),
        f(xyz[1] / WHITE[1]),
        f(xyz[2] / WHITE[2]),
    );
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIELAB input on the app's familiar `[L*, a*, b*]` scale to display sRGB bytes.
///
/// This is the inverse of [`lab_of_srgb`] and exists for authored flat colours in the
/// shared picker. Lab is interpreted against the same D65 white the readout derives,
/// then converted through OKLab only as the already-established gamut-fitting route to
/// the display's sRGB primaries. Out-of-gamut inputs keep lightness and hue while their
/// chroma is brought to the nearest displayable boundary.
pub fn srgb_of_lab([l, a, b]: [f32; 3]) -> [u8; 3] {
    static WHITE: std::sync::LazyLock<[f32; 3]> =
        std::sync::LazyLock::new(|| oklab_to_xyz_d65(1.0, 0.0, 0.0));
    const E: f32 = 216.0 / 24389.0;
    const K: f32 = 24389.0 / 27.0;
    let fy = (l.clamp(0.0, 100.0) + 16.0) / 116.0;
    let fx = fy + a.clamp(-128.0, 127.0) / 500.0;
    let fz = fy - b.clamp(-128.0, 127.0) / 200.0;
    let inv = |t: f32| {
        let cube = t.powi(3);
        if cube > E {
            cube
        } else {
            (116.0 * t - 16.0) / K
        }
    };
    let xyz = [WHITE[0] * inv(fx), WHITE[1] * inv(fy), WHITE[2] * inv(fz)];
    let (ol, oa, ob) = xyz_d65_to_oklab(xyz);
    oklab_to_display_srgb(ol, oa, ob).map(|linear| {
        let encoded = if linear <= 0.003_130_8 {
            12.92 * linear
        } else {
            1.055 * linear.powf(1.0 / 2.4) - 0.055
        };
        (encoded.clamp(0.0, 1.0) * 255.0).round() as u8
    })
}

/// XYZ (D65) → OKLab. The forward matrix sits beside its inverse so Lab input and
/// display output cannot acquire a second, subtly different colour transform.
fn xyz_d65_to_oklab(xyz: [f32; 3]) -> (f32, f32, f32) {
    #[allow(clippy::excessive_precision)]
    const M1: [[f32; 3]; 3] = [
        [0.8189330101, 0.3618667424, -0.1288597137],
        [0.0329845436, 0.9293118715, 0.0361456387],
        [0.0482003018, 0.2643662691, 0.6338517070],
    ];
    let lms = apply(M1, xyz);
    let c = [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()];
    #[allow(clippy::excessive_precision)]
    (
        0.2104542553 * c[0] + 0.7936177850 * c[1] - 0.0040720468 * c[2],
        1.9779984951 * c[0] - 2.4285922050 * c[1] + 0.4505937099 * c[2],
        0.0259040371 * c[0] + 0.7827717662 * c[1] - 0.8086757660 * c[2],
    )
}

/// A toned level as `[L*, a*, b*]`, which is what the footer and the value pins report.
pub fn lab_of(y: f32, a: f32, b: f32) -> [f32; 3] {
    xyz_d65_to_lab(oklab_to_xyz_d65(oklab_lightness(y), a, b))
}

/// A **displayed sRGB triple** as `[L*, a*, b*]` — what the colour reference views
/// report under the cursor.
///
/// # Why the readout is Lab and not the RGB it is sampling
///
/// the maintainer's reason, and it is the same argument that put `RAW`/`EDITED` into one unit:
/// the question a camera JPEG is on screen to answer is *what does this colour come
/// out as in grey*, and two numbers only answer that if they subtract. `L*` from this
/// and `L*` from the print are the same axis, so `L* 64` beside `L* 58` is a sentence.
/// `R 210 G 180 B 96` beside `L* 58` is two facts and an exercise.
///
/// **Through the same conversion the toned readout uses**, deliberately: sRGB → linear
/// → OKLab → XYZ → CIELAB, which is [`lab_of`]'s own path with a different entry
/// point. Two routes to `a*` would eventually disagree by a rounding, and the footer
/// is where that would show up as a colour cast on a grey.
///
/// **What this assumes, stated because the raw linear view breaks it.** The bytes are
/// read as sRGB — true of a camera JPEG, and not true of `preview::linear`, which is
/// camera-native RGB with no colour matrix. For that view this reports the colour *as
/// displayed* rather than a colorimetric measurement of the scene, which is the honest
/// reading of a number that sits under a picture you are looking at. Nothing here
/// feeds the pipeline; see the module note.
pub fn lab_of_srgb(rgb: [u8; 3]) -> [f32; 3] {
    let lin = rgb.map(|c| crate::okhsl::transfer_inv(f32::from(c) / 255.0));
    let (l, a, b) = crate::okhsl::linear_srgb_to_oklab(lin);
    xyz_d65_to_lab(oklab_to_xyz_d65(l, a, b))
}

/// Display sRGB bytes to OKLab, for authored flat colours at the output boundary.
///
/// Unlike [`lab_of_srgb`], this returns OKLab's own `L, a, b` rather than a CIELAB
/// readout. FRAME uses it to put a chosen paper colour beside a toned print without
/// sending that colour through the toning or sharpening stages.
pub fn oklab_of_srgb(rgb: [u8; 3]) -> [f32; 3] {
    let lin = rgb.map(|c| crate::okhsl::transfer_inv(f32::from(c) / 255.0));
    let (l, a, b) = crate::okhsl::linear_srgb_to_oklab(lin);
    [l, a, b]
}

/// OKLab → **linear sRGB** at the display boundary, gamut-fitted.
///
/// Straight to D65 primaries with no adaptation, because a screen is D65. The matrix
/// comes from `okhsl`, which already carries Ottosson's sRGB constants and is the one
/// place they should live.
///
/// **Fitted, not clipped**, exactly as the encode boundary is. A first version returned
/// the raw conversion and left the caller to clamp, which shifts hue on anything
/// saturated — and it silently put a third conversion in the app with different
/// behaviour from the other two. It was caught by the viewport and the export
/// disagreeing.
pub fn oklab_to_display_srgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let at = |scale: f32| okhsl::oklab_linear_srgb(l, a * scale, b * scale);
    let inside = |c: [f32; 3]| c.iter().all(|v| (-1e-6..=1.0 + 1e-6).contains(v));

    let full = at(1.0);
    if inside(full) {
        return full;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..GAMUT_STEPS {
        let mid = 0.5 * (lo + hi);
        if inside(at(mid)) { lo = mid } else { hi = mid }
    }
    let fitted = at(lo);
    [
        fitted[0].clamp(0.0, 1.0),
        fitted[1].clamp(0.0, 1.0),
        fitted[2].clamp(0.0, 1.0),
    ]
}

fn apply(m: [[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn invert(m: [[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-9 {
        return None;
    }
    let c = |r: usize, k: usize| {
        let rs: Vec<usize> = (0..3).filter(|&i| i != r).collect();
        let ks: Vec<usize> = (0..3).filter(|&i| i != k).collect();
        let minor = m[rs[0]][ks[0]] * m[rs[1]][ks[1]] - m[rs[0]][ks[1]] * m[rs[1]][ks[0]];
        if (r + k).is_multiple_of(2) {
            minor
        } else {
            -minor
        }
    };
    // Adjugate is the transpose of the cofactor matrix.
    let mut out = [[0.0f32; 3]; 3];
    for (r, row) in out.iter_mut().enumerate() {
        for (k, cell) in row.iter_mut().enumerate() {
            *cell = c(k, r) / det;
        }
    }
    Some(out)
}

fn s15(b: &[u8]) -> f32 {
    i32::from_be_bytes(b.try_into().expect("four bytes")) as f32 / 65536.0
}

/// The human name an ICC profile gives itself — its `desc` tag.
///
/// **So a panel can say `eciRGB v2` rather than `embedded`.** "A profile is present"
/// and "the file is in *this* space" are different facts, and only the second is worth
/// a row.
///
/// Two encodings, because the tag changed between ICC versions and files of both are
/// in circulation: v4's `mluc` — a record table of UTF-16BE strings, of which the first
/// is taken — and v2's `desc`, which is ASCII with a length that includes its own
/// terminator. Anything else returns `None` rather than a guess.
pub fn icc_description(icc: &[u8]) -> Option<String> {
    let (off, len) = find_tag(icc, b"desc")?;
    let tag = icc.get(off..off + len)?;
    match tag.get(0..4)? {
        b"mluc" => {
            // Signature(4) reserved(4) count(4) recordSize(4), then the record table at
            // 16: language(2) country(2) length(4) offset(4), the last two relative to
            // the start of the tag. Getting this table off by one field reads the record
            // size as the count and the string length from the wrong word, which fails
            // closed — `icc_description` returned `None` for every v4 profile the app
            // ships until `the_app_s_own_profiles_can_name_themselves` caught it.
            let n = u32::from_be_bytes(tag.get(8..12)?.try_into().ok()?) as usize;
            if n == 0 {
                return None;
            }
            let l = u32::from_be_bytes(tag.get(20..24)?.try_into().ok()?) as usize;
            let o = u32::from_be_bytes(tag.get(24..28)?.try_into().ok()?) as usize;
            let bytes = tag.get(o..o + l)?;
            let utf16: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&utf16).ok()
        }
        b"desc" => {
            let l = u32::from_be_bytes(tag.get(8..12)?.try_into().ok()?) as usize;
            // The stored length counts the NUL; trim it rather than render it.
            let bytes = tag.get(12..12 + l.saturating_sub(1))?;
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => None,
    }
    .map(|s| s.trim_end_matches('\0').trim().to_owned())
    .filter(|s| !s.is_empty())
}

/// Offset and size of an ICC tag, from the tag table.
fn find_tag(icc: &[u8], sig: &[u8; 4]) -> Option<(usize, usize)> {
    if icc.len() < 132 {
        return None;
    }
    let n = u32::from_be_bytes(icc[128..132].try_into().ok()?) as usize;
    (0..n).find_map(|i| {
        let e = 132 + 12 * i;
        if e + 12 > icc.len() {
            return None;
        }
        (&icc[e..e + 4] == sig).then(|| {
            (
                u32::from_be_bytes(icc[e + 4..e + 8].try_into().unwrap()) as usize,
                u32::from_be_bytes(icc[e + 8..e + 12].try_into().unwrap()) as usize,
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Both ICC encodings, against real profiles rather than a synthetic one.**
    ///
    /// The v4 `mluc` record table sits at a different offset from the v2 `desc` string,
    /// and reading it one field early takes the record *size* for the count and the
    /// string length from the wrong word. That fails closed — every v4 profile came back
    /// `None` — which on a panel is an empty row rather than a wrong one, and so would
    /// have shipped. `monostar` is this app's own v4 profile and `sRGB2014` is the ICC's
    /// own, so a break in either direction is caught.
    #[test]
    fn the_app_s_own_profiles_can_name_themselves() {
        assert_eq!(
            icc_description(&crate::icc::monostar()).as_deref(),
            Some("monostar")
        );

        let read = |p: &str| std::fs::read(p).ok().and_then(|b| icc_description(&b));
        // Run from the crate directory, so the repo root is two up.
        for (file, want) in [
            ("../../profiles/eciRGB_v2_ICCv4.icc", "eciRGB v2 ICCv4"),
            ("../../profiles/sRGB2014.icc", "sRGB2014"),
            ("../../profiles/ProStarRGB.icc", "ProStarRGB"),
        ] {
            // Skipped rather than failed when the file is not beside the source — the
            // profiles are repo data, and a test must not depend on where it was run.
            if std::path::Path::new(file).exists() {
                assert_eq!(read(file).as_deref(), Some(want), "{file}");
            }
        }
    }

    #[test]
    fn a_profile_that_names_nothing_says_so_rather_than_guessing() {
        assert_eq!(
            icc_description(&[]),
            None,
            "an empty buffer is not a profile"
        );
        assert_eq!(
            icc_description(&[0u8; 200]),
            None,
            "zeroed bytes carry no tag table"
        );
    }

    fn srgb_like() -> Primaries {
        // sRGB's primaries, D50-adapted, as an ICC profile stores them. Built here
        // rather than read from a profile so this module's tests do not depend on
        // `raw-app`'s embedded bytes; `raw-app` has the test that reads the real ones.
        #[allow(clippy::excessive_precision)]
        let to_xyz = [
            [0.4360657, 0.3851515, 0.1430784],
            [0.2224884, 0.7168733, 0.0606384],
            [0.0139218, 0.0970769, 0.7141733],
        ];
        Primaries {
            to_xyz,
            from_xyz: invert(to_xyz).unwrap(),
        }
    }

    #[test]
    fn a_matrix_and_its_inverse_are_the_identity() {
        let p = srgb_like();
        let m = p.to_xyz_d50();
        for (i, basis) in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
            .into_iter()
            .enumerate()
        {
            let round = p.linear_rgb(apply(m, basis));
            for (k, v) in round.iter().enumerate() {
                let want = if i == k { 1.0 } else { 0.0 };
                assert!((v - want).abs() < 1e-4, "basis {i} came back {round:?}");
            }
        }
    }

    /// **The invariant that keeps an untoned print untoned**, and it is *exact*.
    ///
    /// A neutral must come out as three identical channels, because a neutral is on the
    /// achromatic axis of every RGB space there is. Through the matrix it does not: the
    /// channels differ by about 1e-4, which is eight codes at 16 bits, because an ICC
    /// profile's primaries are s15Fixed16 and their three columns do not sum to exactly
    /// the white point. That is quantisation in the file format and no amount of care in
    /// the arithmetic recovers it.
    ///
    /// Hence the bypass in `oklab_to_linear`, and hence `assert_eq` here rather than a
    /// tolerance. A tolerance would let the bypass be deleted as redundant.
    #[test]
    fn a_neutral_is_bit_exact_in_every_space() {
        let p = srgb_like();
        for y in [0.0f32, 0.02, 0.18, 0.5, 0.9, 1.0] {
            let rgb = oklab_to_linear(oklab_lightness(y), 0.0, 0.0, &p);
            assert_eq!(rgb[0], rgb[1], "y={y} came out {rgb:?}");
            assert_eq!(rgb[1], rgb[2], "y={y} came out {rgb:?}");
            assert!((rgb[0] - y).abs() < 1e-6, "y={y} came back as {}", rgb[0]);
        }
    }

    /// And the long way round really is only *nearly* neutral — the measurement the
    /// bypass exists because of. If this ever starts passing at 1e-6, the profile
    /// format changed and the bypass can be reconsidered.
    #[test]
    fn the_matrix_path_is_not_bit_exact_on_a_neutral() {
        let p = srgb_like();
        let l = oklab_lightness(0.18);
        let xyz = adapt_d65_to_d50(oklab_to_xyz_d65(l, 0.0, 0.0));
        let rgb = p.linear_rgb(xyz);
        let spread = rgb.iter().fold(0.0f32, |m, v| m.max((v - rgb[0]).abs()));
        assert!(spread > 1e-6, "the matrix path became exact: {rgb:?}");
        assert!(spread < 1e-3, "and it should still be small: {spread}");
    }

    /// Gamut fitting keeps the hue and gives up the saturation. Clipping the channels
    /// instead is the failure this exists to prevent — a cyanotype blue that clips its
    /// blue channel comes out purple.
    #[test]
    fn fitting_the_gamut_preserves_hue() {
        let p = srgb_like();
        // Far outside anything: a very saturated blue at a dark lightness.
        let (l, a, b) = (0.35f32, -0.10f32, -0.30f32);
        let scale = fit_gamut(l, a, b, &p);
        assert!(scale < 1.0, "this colour should not fit, got scale {scale}");
        assert!(scale > 0.0, "and it should not collapse to neutral");

        // The fitted colour, taken back to OKLab, points the same way.
        let want = b.atan2(a);
        let fitted_xyz = oklab_to_xyz_d65(l, a * scale, b * scale);
        let round = xyz_d65_to_oklab(fitted_xyz);
        let got = round.2.atan2(round.1);
        let apart = (want - got).abs();
        assert!(apart < 0.02, "hue moved by {apart} rad");
    }

    /// And a colour that already fits is returned untouched, rather than being
    /// bisected to something very slightly smaller.
    #[test]
    fn a_colour_inside_the_gamut_is_not_scaled() {
        let p = srgb_like();
        assert_eq!(fit_gamut(0.6, 0.02, 0.03, &p), 1.0);
    }

    /// The adaptation is real and points the right way: D65 white lands on D50 white.
    #[test]
    fn white_adapts_from_d65_to_d50() {
        // OKLab (1, 0, 0) is D65 white by construction.
        let d50 = adapt_d65_to_d50(oklab_to_xyz_d65(1.0, 0.0, 0.0));
        // ICC's D50, to the precision s15Fixed16 has: 0.9642, 1.0, 0.8249.
        assert!((d50[0] - 0.9642).abs() < 2e-3, "X {d50:?}");
        assert!((d50[1] - 1.0).abs() < 2e-3, "Y {d50:?}");
        assert!((d50[2] - 0.8249).abs() < 2e-3, "Z {d50:?}");
    }
}

#[cfg(test)]
mod lab_readout {
    use super::*;

    /// **A neutral has no colour, and CIELAB has to say so exactly.** The footer and the
    /// value pins report these numbers beside `L*`, and a neutral reading `a* +0.3`
    /// would have a photographer chasing a cast that is not there.
    #[test]
    fn a_neutral_reads_zero_on_both_chroma_axes() {
        for y in [0.02f32, 0.18, 0.5, 0.9, 1.0] {
            let lab = lab_of(y, 0.0, 0.0);
            assert!(lab[1].abs() < 0.01, "a* was {} at y={y}", lab[1]);
            assert!(lab[2].abs() < 0.01, "b* was {} at y={y}", lab[2]);
        }
    }

    /// `L*` from this path agrees with the one the rest of the app reports, or the
    /// footer would carry two lightnesses that disagree by a digit.
    #[test]
    fn lightness_agrees_with_the_apps_own_l_star() {
        for y in [0.02f32, 0.18, 0.5, 0.9, 1.0] {
            let want = crate::display::lstar_encode(y) * 100.0;
            let got = lab_of(y, 0.0, 0.0)[0];
            assert!((got - want).abs() < 0.2, "L* {got} against {want} at y={y}");
        }
    }

    /// The axes point the way Lab's do, which is what the footer's ink is claiming:
    /// `+a*` toward magenta, `+b*` toward yellow.
    #[test]
    fn the_axes_point_where_lab_says_they_do() {
        // A warm tone — OKLab hue near 60 degrees is yellow-orange.
        let ab = |hue: f32, chroma: f32| {
            let r: f32 = hue.to_radians();
            (chroma * r.cos(), chroma * r.sin())
        };
        let (a, b) = ab(60.0, 0.1);
        let lab = lab_of(0.5, a, b);
        assert!(lab[2] > 5.0, "a warm tone should be +b*, got {}", lab[2]);

        // And a magenta one is +a*.
        let (a, b) = ab(350.0, 0.1);
        let lab = lab_of(0.5, a, b);
        assert!(lab[1] > 5.0, "a magenta tone should be +a*, got {}", lab[1]);
    }

    /// The reference views' readout: a neutral has no colour, the ends are the ends,
    /// and the axes point the same way they do for a toned print.
    #[test]
    fn a_displayed_srgb_reads_as_lab() {
        let white = lab_of_srgb([255, 255, 255]);
        assert!(
            (white[0] - 100.0).abs() < 0.05,
            "white is L* 100, got {}",
            white[0]
        );
        assert_eq!(lab_of_srgb([0, 0, 0])[0], 0.0, "black is L* 0");

        // **A grey must read exactly neutral.** A footer reporting a cast on a grey
        // is a footer that sends somebody hunting for one — the same reason
        // `xyz_d65_to_lab` derives its white from the transform rather than quoting
        // the illuminant.
        for v in [32u8, 119, 187] {
            let lab = lab_of_srgb([v, v, v]);
            assert!(
                lab[1].abs() < 0.05 && lab[2].abs() < 0.05,
                "grey {v} read {lab:?}"
            );
        }

        // Middle grey in sRGB is L* 50 by construction, which is the number that
        // makes this readout comparable with the print's.
        let mid = lab_of_srgb([119, 119, 119]);
        assert!(
            (mid[0] - 50.0).abs() < 0.5,
            "sRGB 119 is about L* 50, got {}",
            mid[0]
        );

        // Same axis directions as `lab_of`, so one ink legend serves both readouts.
        assert!(lab_of_srgb([220, 120, 200])[1] > 5.0, "a magenta is +a*");
        assert!(lab_of_srgb([220, 200, 90])[2] > 5.0, "a yellow is +b*");
    }

    #[test]
    fn lab_input_round_trips_display_colours() {
        for rgb in [
            [0, 0, 0],
            [255, 255, 255],
            [119, 119, 119],
            [220, 120, 200],
            [220, 200, 90],
        ] {
            let round = srgb_of_lab(lab_of_srgb(rgb));
            for (want, got) in rgb.into_iter().zip(round) {
                assert!(want.abs_diff(got) <= 2, "{rgb:?} came back as {round:?}");
            }
        }
    }
}
