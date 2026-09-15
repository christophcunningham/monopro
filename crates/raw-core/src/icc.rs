//! Generating `monostar.icc` — a grayscale ICC v4.4 profile whose TRC is CIE L\*.
//!
//! # Why this is in `raw-core` rather than a tool beside the profile
//!
//! **Because the profile's curve and [`display::lstar_encode`](crate::display::lstar_encode)
//! are the same function, and they must not drift.** The app encodes every master with
//! that function and then tags the file with a profile *declaring* what the numbers
//! mean. If the two ever disagree, every file monopro has written is mislabelled, and
//! nothing on screen says so — the failure is silent and it is retrospective.
//!
//! Deriving the profile from the same constants the pipeline uses makes that
//! disagreement impossible to introduce, and
//! `the_generated_curve_inverts_the_encode_this_app_applies` checks it over a ramp
//! rather than trusting the arithmetic twice.
//!
//! # Why it is generated rather than kept as a checked-in binary nobody can verify
//!
//! `profiles/MONOSTAR.md` claims the profile is "generated deterministically from first
//! principles". Before this module that claim was unverifiable by anyone reading it,
//! including us. `the_generator_reproduces_the_shipped_profile` makes it a test: the
//! bytes in `profiles/monostar.icc` are exactly what this code produces, or the suite
//! fails.
//!
//! That is also what makes the CC0 dedication meaningful. A profile is only credibly
//! free of third-party IP if the path from published constants to bytes is visible, and
//! a reproducible build *is* that evidence.
//!
//! # The numbers, and where they come from
//!
//! CIE L\* is defined on `Y ∈ [0, 1]`:
//!
//! ```text
//! L* = 116 · Y^(1/3) − 16    for Y > 216/24389
//! L* = 903.3 · Y             otherwise
//! ```
//!
//! An ICC `kTRC` is a **decode** curve — code in, linear light out — so the profile
//! carries the *inverse*, as a `para` type 3:
//!
//! ```text
//! Y = (a·X + b)^g    for X ≥ d
//! Y = c·X            for X < d
//! ```
//!
//! with `g = 3`, `a = 100/116`, `b = 16/116`, `c = 100/903.3`, `d = 8/100`. Type 3
//! rather than type 4 because L\*'s two offset terms are both zero.

/// D50, the ICC PCS illuminant, as `s15Fixed16` — 0.9642, 1.0000, 0.8249.
///
/// # The Z entry is a decision, and the field is split on it
///
/// ICC.1 clause 7.2.16 says the PCS illuminant **shall** be `0.9642, 1.0000, 0.8249`,
/// encoded `0000F6D6 00010000 0000D32D` — `0xD32D` being 54061. That is what ICC's own
/// `sRGB2014.icc` carries, and what eciRGB v2 carries.
///
/// But `ProStarRGB.icc` and `ETRGB.icc` both carry `0xD32B` (54059, ≈ 0.824875), and so
/// did monostar until this generator was written — the two cultural-heritage profiles
/// this family descends from are on the other side of it.
///
/// **This uses the spec value**, on the reasoning that the difference is 3×10⁻⁵ in Z —
/// nothing renders differently — so the only thing at stake is conformance, and
/// conformance here is free. What monostar inherits from ProStarRGB is the `*star`
/// name and the `LPIN` convention; neither of those is numeric.
///
/// Recorded at this length because the old value is not a mistake and a future reader
/// comparing monostar against ETRGB will find the difference and wonder.
const D50: [i32; 3] = [63190, 65536, 54061];

/// The `para` type 3 coefficients as `s15Fixed16`, in the order the tag stores them.
///
/// Derived from the exact rationals rather than typed in, so the source of each number
/// is the CIE definition and not a previous copy of this file. See
/// `the_coefficients_have_the_three_properties_the_curve_depends_on` for what has to
/// remain true of them.
fn para_lstar() -> [i32; 5] {
    [
        fixed(3.0),           // g — cube
        fixed(100.0 / 116.0), // a
        fixed(16.0 / 116.0),  // b — the L* offset, shared with every `*star` profile
        fixed(100.0 / 903.3), // c — the linear shadow segment
        fixed(8.0 / 100.0),   // d — the L* = 8 breakpoint, in code-value space
    ]
}

/// A real number as ICC `s15Fixed16Number`, rounded to nearest.
fn fixed(v: f64) -> i32 {
    (v * 65536.0).round() as i32
}

/// What distinguishes one profile in this family from another.
///
/// **The name is a parameter and not a constant**, which matters more than it looks:
/// everything else here is numerically independent of what the file is called, and a
/// generator that hard-coded the name would quietly invite a second copy of the maths
/// the first time a sibling profile is wanted.
pub struct GrayLstar<'a> {
    /// The `desc` tag — the name a profile picker shows. Keep it short.
    pub desc: &'a str,
    /// The `cprt` tag. **This is the only licensing text that travels**: the profile is
    /// embedded in every TIFF, and a `LICENSE` file beside it in a repository is not.
    /// A bare attribution with no grant of permission reads as all rights reserved.
    pub copyright: &'a str,
    /// The `LPIN` private tag — provenance for a human who opens the binary. Ignored by
    /// every CMM. The convention is inherited from `ProStarRGB.icc`.
    pub lpin: &'a str,
    /// Creation date, `(year, month, day, hour, minute, second)` UTC.
    pub date: (u16, u16, u16, u16, u16, u16),
}

/// Build the profile. Deterministic: same input, same bytes, including the profile ID.
pub fn gray_lstar(spec: &GrayLstar) -> Vec<u8> {
    let tags: Vec<(&[u8; 4], Vec<u8>)> = vec![
        (b"wtpt", xyz_type(D50)),
        (b"kTRC", para_type3(para_lstar())),
        (b"desc", mluc(spec.desc)),
        (b"cprt", mluc(spec.copyright)),
        (b"LPIN", text_type(spec.lpin)),
    ];

    // Header, then the count, then a 12-byte entry per tag, then the data. Every tag's
    // data is padded to a 4-byte boundary and **the size field records the padded
    // length**, which is what the shipped profile does; both conventions exist in the
    // wild and the one that reproduces the file is the one to keep.
    let table_end = 128 + 4 + tags.len() * 12;
    let mut table = Vec::with_capacity(tags.len() * 12);
    let mut data = Vec::new();
    for (sig, body) in &tags {
        let mut padded = body.clone();
        while padded.len() % 4 != 0 {
            padded.push(0);
        }
        table.extend_from_slice(*sig);
        table.extend_from_slice(&((table_end + data.len()) as u32).to_be_bytes());
        table.extend_from_slice(&(padded.len() as u32).to_be_bytes());
        data.extend_from_slice(&padded);
    }

    let size = table_end + data.len();
    let mut out = vec![0u8; 128];
    out[0..4].copy_from_slice(&(size as u32).to_be_bytes());
    // 4..8 CMM, left zero — no preferred CMM.
    out[8..12].copy_from_slice(&[0x04, 0x40, 0x00, 0x00]); // v4.4.0
    out[12..16].copy_from_slice(b"mntr");
    out[16..20].copy_from_slice(b"GRAY");
    out[20..24].copy_from_slice(b"XYZ ");
    let (y, mo, d, h, mi, s) = spec.date;
    for (i, v) in [y, mo, d, h, mi, s].iter().enumerate() {
        out[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_be_bytes());
    }
    out[36..40].copy_from_slice(b"acsp");
    out[40..44].copy_from_slice(b"APPL");
    // 44..48 flags, 48..56 device manufacturer and model, 56..64 device attributes:
    // all zero, and all correct for a data-encoding profile that characterises no
    // device. See `MONOSTAR.md` on what monostar is not.
    // 64..68 rendering intent: perceptual (0).
    for (i, v) in D50.iter().enumerate() {
        out[68 + i * 4..72 + i * 4].copy_from_slice(&v.to_be_bytes());
    }
    out[80..84].copy_from_slice(b"CRC ");
    // 84..100 profile ID, filled in below. 100..128 reserved, zero.
    out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&data);

    // ICC v4 clause 7.2.18: the ID is an MD5 over the whole profile with the flags,
    // the rendering intent and the ID field itself zeroed — so that a profile whose
    // intent is changed keeps its identity.
    let mut hashed = out.clone();
    hashed[44..48].fill(0);
    hashed[64..68].fill(0);
    hashed[84..100].fill(0);
    let id = md5(&hashed);
    out[84..100].copy_from_slice(&id);
    out
}

fn xyz_type(v: [i32; 3]) -> Vec<u8> {
    let mut t = b"XYZ \0\0\0\0".to_vec();
    for c in v {
        t.extend_from_slice(&c.to_be_bytes());
    }
    t
}

fn para_type3(p: [i32; 5]) -> Vec<u8> {
    let mut t = b"para\0\0\0\0".to_vec();
    t.extend_from_slice(&3u16.to_be_bytes()); // function type 3
    t.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for c in p {
        t.extend_from_slice(&c.to_be_bytes());
    }
    t
}

/// An `mluc` with a single en-US record, UTF-16BE.
fn mluc(s: &str) -> Vec<u8> {
    let utf16: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_be_bytes()).collect();
    let mut t = b"mluc\0\0\0\0".to_vec();
    t.extend_from_slice(&1u32.to_be_bytes()); // one record
    t.extend_from_slice(&12u32.to_be_bytes()); // record size
    t.extend_from_slice(b"enUS");
    t.extend_from_slice(&(utf16.len() as u32).to_be_bytes());
    t.extend_from_slice(&28u32.to_be_bytes()); // offset from the tag's start
    t.extend_from_slice(&utf16);
    t
}

/// A `text` tag — ASCII, NUL-terminated, as the type requires.
fn text_type(s: &str) -> Vec<u8> {
    let mut t = b"text\0\0\0\0".to_vec();
    t.extend_from_slice(s.as_bytes());
    t.push(0);
    t
}

/// MD5 (RFC 1321).
///
/// **Hand-written rather than a dependency**, which is a judgement call and not an
/// obvious one. It is used here purely as the content fingerprint ICC clause 7.2.18
/// specifies — no security property is claimed or needed — and it is ~40 lines against
/// a new entry in a dependency list every other line of which is load-bearing.
/// `md5_matches_the_rfc_1321_test_vectors` is what makes that trade safe.
fn md5(msg: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    // K[i] = floor(2^32 · |sin(i + 1)|), generated rather than tabulated.
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();

    let mut m = msg.to_vec();
    let bitlen = (msg.len() as u64).wrapping_mul(8);
    m.push(0x80);
    while m.len() % 64 != 56 {
        m.push(0);
    }
    m.extend_from_slice(&bitlen.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    for chunk in m.chunks_exact(64) {
        let w: Vec<u32> = chunk
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(k[i]).wrapping_add(w[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, v) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// What ships as `profiles/monostar.icc`.
///
/// Held here rather than at the call site so that the file, the test that pins it and
/// anything that regenerates it cannot disagree about what monostar *is*.
pub fn monostar() -> Vec<u8> {
    gray_lstar(&GrayLstar {
        desc: "monostar",
        // **The grant, not just the name.** The previous profile carried
        // "C. Cunningham / monopro project" and nothing else — an attribution with no
        // permission attached, which under copyright's default reads as all rights
        // reserved. `profiles/LICENSE` said CC0 and never left the repository, while
        // this string is embedded in every TIFF monopro writes. It was the only
        // licensing text that travelled and it said the opposite of the intent.
        //
        // **Short, and deliberately so.** `cprt` is an `mluc`, so it is UTF-16 and
        // every character costs two bytes; `LPIN` below is a `text` tag at one. Naming
        // CC0 1.0 Universal *is* the dedication — it is a specific published
        // instrument — so the legal minimum goes here and the prose goes where it is
        // half the price.
        copyright: "C. Cunningham / monopro. Public domain: Creative Commons CC0 1.0 \
                    Universal. No rights reserved.",
        // **The ICC's model wording is not used verbatim, and the reason is a real
        // conflict rather than a preference.** `profiles/licensing-iccorg.txt`
        // recommends a permissive licence carrying a condition — that altered versions
        // remove the original identification. CC0 is a *waiver*: it reserves nothing,
        // so it cannot impose that condition. Stating both would be incoherent. The
        // request survives here as what it honestly is.
        lpin: "monostar -- L* Gray with D50 white point, for monopro. By C. Cunningham. \
               The kTRC is the inverse CIE L* function as an ICC para type 3 curve. \
               Generated from the CIE definition by raw_core::icc; no third-party \
               profile data is incorporated. Public domain under CC0 1.0 Universal \
               (creativecommons.org/publicdomain/zero/1.0/). CC0 imposes no conditions, \
               so altered versions are asked as a courtesy, not required, to change the \
               identification so they are not taken for the original.",
        date: (2026, 4, 1, 0, 0, 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_rfc_1321_test_vectors() {
        // The whole justification for hand-writing it. If these pass, the profile ID is
        // right; if they do not, nothing else in this module can be trusted.
        let cases: [(&str, &str); 4] = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        ];
        for (input, want) in cases {
            let got: String = md5(input.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(got, want, "MD5({input:?})");
        }
    }

    #[test]
    fn the_coefficients_have_the_three_properties_the_curve_depends_on() {
        let [g, a, b, c, d] = para_lstar();
        assert_eq!(g, 65536 * 3, "g must be exactly 3");

        // **The ceiling lands on exactly 1.0.** `100/116 + 16/116 = 1`, and the two
        // fractional parts (0.55 and 0.45 of a fixed-point step) round in opposite
        // directions, so the sum survives quantisation. Had they rounded the same way,
        // white would decode to 0.99998 instead of 1.0 — small, silent, and wrong.
        assert_eq!(a + b, 65536, "a + b must be exactly 1.0 in s15Fixed16");

        // **The two branches meet, and they meet at the CIE constant** rather than
        // near it. 216/24389 is the L* breakpoint in luminance.
        let (af, bf, cf, df) = (f(a), f(b), f(c), f(d));
        let linear = cf * df;
        let power = (af * df + bf).powi(3);
        let cie = 216.0 / 24389.0;
        assert!(
            (linear - power).abs() < 1e-6,
            "branches disagree: {linear} vs {power}"
        );
        assert!(
            (linear - cie).abs() < 1e-6,
            "breakpoint is not CIE's: {linear} vs {cie}"
        );
    }

    #[test]
    fn the_white_point_is_d50_to_the_precision_the_format_has() {
        // Not a copied constant: ICC's PCS illuminant is 0.9642 / 1.0000 / 0.8249, and
        // what the file must carry is that rounded to s15Fixed16 — which is where the
        // published 0.824875 comes from, and why a document quoting 0.82488 is rounding
        // a rounding.
        assert_eq!(D50, [fixed(0.9642), fixed(1.0), fixed(0.8249)]);
    }

    #[test]
    fn the_generated_curve_inverts_the_encode_this_app_applies() {
        // **The reason this module lives in `raw-core`.** Every master is written with
        // `display::lstar_encode` and tagged with this profile, so the profile's decode
        // curve has to be that function's inverse. Checked against the coefficients the
        // generator actually emits — quantised, as shipped — rather than against the
        // exact rationals, because the quantised ones are what a CMM will use.
        let [g, a, b, c, d] = para_lstar().map(f);
        let icc_decode = |x: f32| if x >= d { (a * x + b).powf(g) } else { c * x };
        for i in 0..=1000 {
            let linear = i as f32 / 1000.0;
            let back = icc_decode(crate::display::lstar_encode(linear));
            assert!(
                (back - linear).abs() < 2e-3,
                "profile does not invert the encode at {linear}: got {back}"
            );
        }
    }

    #[test]
    fn the_profile_id_validates_against_its_own_contents() {
        // ICC v4 clause 7.2.18, recomputed the way a validator would. Plenty of shipped
        // profiles carry a stale or zeroed ID; this asserts ours is neither.
        let p = monostar();
        let stored: [u8; 16] = p[84..100].try_into().unwrap();
        let mut m = p.clone();
        m[44..48].fill(0);
        m[64..68].fill(0);
        m[84..100].fill(0);
        assert_eq!(stored, md5(&m));
    }

    #[test]
    fn the_header_declares_the_size_it_actually_is() {
        let p = monostar();
        assert_eq!(
            u32::from_be_bytes(p[0..4].try_into().unwrap()) as usize,
            p.len()
        );
        assert!(
            p.len().is_multiple_of(4),
            "an ICC profile is a whole number of 32-bit words"
        );
        assert!(
            p[100..128].iter().all(|&b| b == 0),
            "reserved header bytes must be zero"
        );
        assert_eq!(&p[36..40], b"acsp");
    }

    #[test]
    fn the_generator_reproduces_the_shipped_profile() {
        // **What makes "generated deterministically from first principles" checkable**
        // rather than merely stated. The bytes in `profiles/monostar.icc` are this
        // function's output or this test fails — so the file cannot be hand-edited, and
        // a change to the maths cannot land without the shipped profile following it.
        let shipped = include_bytes!("../../../profiles/monostar.icc");
        assert_eq!(
            monostar(),
            shipped,
            "profiles/monostar.icc is not what the generator produces — \
             run `cargo run -p raw-core --example write-profiles`"
        );
    }

    fn f(v: i32) -> f32 {
        v as f32 / 65536.0
    }
}
