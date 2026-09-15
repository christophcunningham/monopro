# monostar

**An ICC v4.4 grayscale output profile encoding CIE L\* (CIELAB lightness) as its tone reproduction curve.**

Created by C. Cunningham / [monopro](https://github.com/christophcunningham/monopro)  
Released April 1, 2026 · CC0 1.0 · Freely redistributable

---

## What this is

monostar is a grayscale ICC v4.4 profile whose `kTRC` tag encodes the inverse CIE L\* transfer function as a `para` type 3 parametric curve — 5 coefficients, no sampling, no lookup table. Pixel values in a monostar-tagged TIFF are L\*-encoded — perceptually uniform — and any color-managed application will correctly decode them back to linear light for display or further processing.

The common grayscale profiles — Gray Gamma 1.8, Gray Gamma 2.2, Dot Gain 20% — carry a power-law or dot-gain curve instead. L\* as a TRC is established in RGB working spaces (eciRGB v2, ProStarRGB); monostar is the grayscale case of the same curve.

---

## The math

### CIE L\* encoding (forward — what monopro writes)

Given a normalized linear luminance value Y in [0, 1]:

```
L* = 116 × Y^(1/3) − 16      if Y > 0.008856
L* = 903.3 × Y                if Y ≤ 0.008856
```

L\* is then normalized to [0, 1] and stored as a 16-bit integer:

```
code = round( (L* / 100) × 65535 )
```

The shadow region (Y ≤ 0.008856, i.e. L\* ≤ 8) uses a linear segment to avoid the singularity at zero that a pure cube-root function would produce. This is the same formulation used by eciRGB v2 and every CIE L\*a\*b\* implementation.

### ICC TRC semantics (decode direction — what the profile declares)

The ICC specification defines the `kTRC` tag as a **decode** curve: given a stored code value, it returns the corresponding linear light value. monostar's `kTRC` is therefore the inverse of L\* encoding:

```
Y = ((L* + 16) / 116)³        if L* > 8
Y = L* / 903.3                 if L* ≤ 8

where L* = code × 100
```

This is encoded as an ICC v4 `para` type 3 parametric curve — 32 bytes, exact. The equivalent 1024-point `curv` table would be 2,060 bytes with up to 0.03% rounding error at each sample point.

### The `para` type 3 coefficients

The ICC v4 `para` type 3 curve takes the form:

```
Y = (a·X + b)^g    for X ≥ d
Y = c·X            for X < d
```

For monostar, substituting the inverse L\* function (where X is the normalized code value in [0, 1]):

| | exact rational | `s15Fixed16` integer | as shipped |
|---|---|---|---|
| `g` | 3 | 196608 | 3.0 |
| `a` | 100/116 | 56497 | 0.86207581 |
| `b` | 16/116 | 9039 | 0.13792419 |
| `c` | 100/903.3 | 7255 | 0.11070251 |
| `d` | 8/100 | 5243 | 0.08000183 |

Type 3 is used rather than type 4 (which adds offset parameters `e` and `f`) because both offsets are zero for L\*. This matches the TRC encoding used by eciRGB v2.

These are exact rational values derived directly from the CIE L\* definition, quantized to `s15Fixed16Number` (signed 16.16 fixed point). Maximum rounding is 1/65536 ≈ 0.0015% per coefficient.

### Three properties of the shipped coefficients, verified against the binary

**`a + b = 65536` exactly**, so the curve reaches exactly 1.0 at full code. This is not hand-tuning — `100/116 + 16/116 = 1` exactly, and the two fractional parts (0.55 and 0.45 of a fixed-point step) round in opposite directions, so the sum is preserved. Worth checking rather than assuming: had both rounded the same way, white would land at 0.99998 instead of 1.0.

**Continuity at `d`.** The linear segment gives `c·d = 0.0088564`; the power segment gives `(a·d + b)³ = 0.0088558`. Both agree with the CIE constant 216/24389 = 0.00885645 to within 6×10⁻⁷ — the two branches meet, and they meet at the value CIE defines rather than near it.

**The whole coefficient set matches eciRGB v2.** All five `s15Fixed16` words in monostar's `kTRC` are byte-identical to those in eciRGB v2's `rTRC`, `gTRC` and `bTRC` — same curve, read out of both binaries rather than assumed. See *Relation to eciRGB v2 and ProStarRGB* below.

### Why the direction matters

Most common grayscale profiles — Gray Gamma 1.8, Gray Gamma 2.2, Dot Gain 20% — encode simple power-law curves where the forward and inverse functions are structurally identical (just reciprocal exponents). L\* is not symmetric: the shadow region is piecewise linear, and the cube-root region has a different shape in each direction. Embedding the forward L\* curve (linear → perceptual) as a TRC would cause every application to interpret stored values as significantly brighter than they are — roughly 1.5–2× overestimate in the shadows. monostar uses the inverse (decode) direction, which is what the ICC spec requires.

### Round-trip accuracy

The `para` parametric curve is mathematically exact — no sampling, no interpolation. The only rounding is in the fixed-point encoding of the 5 coefficients. Verification of the decoded curve at key tonal values:

| Linear Y | L\* code | para decode Y | Error |
|:--------:|:--------:|:-------------:|:-----:|
| 0.0000 | 0.0000 | 0.0000 | 0.000% |
| 0.0089 | 0.0800 | 0.0089 | <0.002% |
| 0.1842 | 0.5000 | 0.1842 | <0.002% |
| 0.5000 | 0.7607 | 0.5000 | <0.002% |
| 1.0000 | 1.0000 | 1.0000 | 0.000% |

---

## Profile structure

Read from the shipped binary.

| Field | Value |
|-------|-------|
| ICC version | 4.4.0 |
| Profile class | Display (`mntr`) |
| color space | `GRAY` |
| PCS | `XYZ ` |
| Rendering intent | Perceptual (0) |
| Creator | `CRC ` |
| Created | 2026-04-01 |
| Profile ID | `e4aab4b2e442adb8918125a7de0e50b0` — MD5 per ICC v4 clause 7.2.18, verified |
| File size | 996 bytes |

Five tags:

| Tag | Type | Contents |
|---|---|---|
| `wtpt` | `XYZ ` | `0000F6D6 00010000 0000D32D` — D50 (0.9642, 1.0000, 0.8249) to `s15Fixed16` |
| `kTRC` | `para` type 3 | inverse L\*, 5 coefficients, 32 bytes |
| `desc` | `mluc` enUS | "monostar" |
| `cprt` | `mluc` enUS | "C. Cunningham / monopro. Public domain: Creative Commons CC0 1.0 Universal. No rights reserved." |
| `LPIN` | `text` | construction, provenance and the CC0 dedication in full — see below |

`LPIN` is a private tag, a house convention inherited from `ProStarRGB.icc`. Private tags are ignored by every CMM and carry provenance for anyone who opens the binary.

**`cprt` is short on purpose.** It is an `mluc`, so it is UTF-16 and every character costs two bytes; `LPIN` is a `text` tag at one byte per character. Naming CC0 1.0 Universal *is* the dedication — it is a specific published instrument — so the legal minimum goes in `cprt` and the prose goes where it is half the price.

### A note on the white point

ICC.1 clause 7.2.16 says the PCS illuminant **shall** be 0.9642 / 1.0000 / 0.8249, encoded `0000F6D6 00010000 0000D32D`. That is what this profile carries, and what ICC's own `sRGB2014.icc` and eciRGB v2 carry.

It is worth recording that the field is split: `ProStarRGB.icc` and `ETRGB.icc` both encode Z as `0xD32B` (≈ 0.824875), and so did monostar before it was regenerated from a verified generator. The difference is 3×10⁻⁵ in Z — nothing renders differently — so the only thing at stake is conformance. Anyone comparing monostar against the profiles it descends from will find this and should not conclude that one of them is broken.

---

## Relation to eciRGB v2 and ProStarRGB

The `*star` suffix denotes the **L\* transfer function**. It is inherited rather than coined here: `ProStarRGB.icc` — ProPhoto primaries with the L\* curve — was created by Scott Geffert and comes out of the cultural-heritage community.

Three shipped profiles carry the same transfer function and differ only in primaries:

| | primaries | white | TRC encoding | ceiling |
|---|---|---|---|---|
| **monostar** | none (grayscale) | D50 | `para` type 3 | 1.0 |
| eciRGB v2 | eciRGB v2 | D50 | `para` type 3 | 1.0 |
| ProStarRGB | ProPhoto | D50 | 700-point `curv` | 1.0 |

Read out of the binaries, not assumed:

- monostar's `kTRC` and eciRGB v2's `rTRC`/`gTRC`/`bTRC` are byte-identical 32-byte tags, carrying the same five coefficient words: `00030000 0000DCB1 0000234F 00001C57 0000147B` (`g`, `a`, `b`, `c`, `d`).
- ProStarRGB stores the same curve sampled as a 700-point `curv` table (1,412 bytes); it matches the parametric curve to 0.00000 mean error.

The consequence is that converting a monostar file to eciRGB v2 changes the channel count and nothing else — the tone encoding carries over untouched.

---

## Where monostar sits in monopro's pipeline

monopro is monochrome **by construction**, not a color pipeline with the color switched off. Luminance is collapsed from the CFA photosites *upstream of any demosaic or color matrix*, so what the pipeline carries is a scene-linear float32 density with no colorimetric claim attached — deliberately not Rec.2020 luminance, whose coefficients describe human perception of a display-referred signal and do not apply to sensor-referred data.

monostar is where that scalar acquires a declared meaning. The export path is:

```
scene-linear float32 → tone map → resize → grain → toning → output sharpening → encode → 16-bit
```

Toning is the one stage that can turn the single channel into three. It emits OKLab — a lightness and an `(a, b)` pair, no primaries and no gamut — and an active toner therefore retargets the export to eciRGB v2, whose TRC is the same curve. **monostar tags the untoned path**, where the file is one channel and the encode is the L\* encode above.

Two notes for anyone reading pixel values back:

- **The clamp to [0, 1] happens at the encode and nowhere earlier**, on purpose. The resampler rings past its input's range at an edge, and output sharpening overshoots either side of one by design; both are supposed to, and clamping mid-chain would flatten highlights the later stages still need. So the file is bounded even though the signal that produced it was not.
- **Grain, toning and sharpening all run after the resize**, so their scale is in the pixels of the *file*, not of the picture.

---

## Use cases

### Primary: print workflow with L\*-encoded files

monostar is designed for photographers and printmakers who work with L\*-encoded 16-bit grayscale TIFFs destined for inkjet or alternative process printing. The profile ensures that:

- Pixel values read correctly in Photoshop's Info panel (L\* values match what your develop application reported)
- Soft proofing against a printer ICC works correctly, because the source transform starts from an accurate characterisation of what the file contains
- No implicit re-encoding occurs when converting to a print working space — Photoshop or any color-managed application will apply only the declared transform

### Setting monostar as your Photoshop gray working space

**Edit → Color Settings → Gray → Load Gray...** → select `monostar.icc`

This allows Photoshop to display monostar-tagged TIFFs without proof colors active, and to use monostar as the source profile when converting to CMYK or a printer profile.

### Alternative process printmaking (Pt/Pd, photogravure, cyanotype)

These processes respond to UV density rather than visual density, and require explicit control of the tonal scale. L\* distributes code values perceptually uniformly, which is the same axis the linearisation step in digital negative workflows (QTR, Piezography) works along. Because the file's encoding is declared exactly, the correction curve those tools build applies to a known starting distribution rather than an assumed gamma.

### Archive and interchange

L\* is a device-independent perceptual encoding. A monostar-tagged file can be converted to any other gray working space by a color management engine with full accuracy, because the source encoding is exactly declared: the CMM decodes through a closed-form curve rather than an interpolated table. The ceiling is 1.0, so the conversion involves no clip-or-compress decision at the top end.

---

## What monostar is not

monostar is not a **device profile** — it does not characterise a specific monitor, printer, or scanner. It is a **data encoding profile**: it declares the mathematical meaning of the pixel values in a file. This is the same role played by sRGB (for RGB files) or Dot Gain 20% (for grayscale press files).

monostar does not perform dot-gain compensation. It is not intended for files going directly to an offset press. For press workflows, profiles derived from characterisation data (GRACoL, FOGRA, or a custom press profile) remain appropriate.

---

## Files in this directory

| File | Description |
|------|-------------|
| `monostar.icc` | The profile — 996 bytes, ICC v4.4, `para` TRC. Install in `~/Library/ColorSync/Profiles/` on macOS |
| `eciRGB_v2_ICCv4.icc` | eciRGB v2 ICCv4 from [eci.org](https://www.eci.org). Licence in `eciRGB_v2_license.rtf` |
| `ProStarRGB.icc` | ProPhoto primaries with the L\* curve, by Scott Geffert. The source of the `*star` naming convention |
| `sRGB2014.icc` | sRGB v4 from the [ICC](https://www.color.org). Embedded in proof exports, whose recipient's screen is unmanaged |
| `LICENSE` | CC0 1.0, covering the profiles generated for this project |
| `licensing-iccorg.txt` | The ICC's recommended wording for freely-distributed profiles. See *License* on why monostar does not use it verbatim |

All four profiles are embedded in the application binary and selectable as export spaces. On the untoned path **no color transform is applied to any of them**: the same L\*-encoded scalar is written to every channel, and the profile declares what it means. The primaries start to matter when toning gives the signal chroma, which is the point at which the export retargets from monostar to eciRGB v2.

---

## How this profile is built

The binary is generated by `raw_core::icc` in the monopro source tree, and regenerated with:

```
cargo run -p raw-core --example write-profiles
```

Every claim on this page is checked by the test suite. Six tests run against the generator on every build:

- the coefficients satisfy `a + b = 65536` exactly, and both TRC branches meet at CIE's 216/24389
- the white point is D50 quantized, not a copied constant
- the profile ID recomputes from the profile's own contents, per clause 7.2.18
- the header's declared size is the actual size and the reserved bytes are zero
- **the generated curve is the inverse of the L\* encode the application actually applies**, checked over a 1000-step ramp
- **the bytes in `monostar.icc` are exactly what the generator produces**

The last two are the ones that bind the profile to the application. The fifth prevents the profile drifting from the encoder that writes the files it tags — a divergence that would be silent and retroactive, mislabelling every file already written. The sixth prevents the shipped binary being hand-edited, and prevents a change to the maths landing without the profile following it.

Reproducibility is also what supports the CC0 dedication: the path from the published CIE constants to the shipped bytes is visible and re-runnable, so no third-party ICC data is involved at any step.

---

## License

monostar is dedicated to the public domain under the **Creative Commons CC0 1.0 Universal** dedication. No rights reserved. No attribution required, though it is appreciated.

The profile binary is generated from the CIE L\* definition and the ICC specification, as above. No third-party ICC data is incorporated.

**A note on the ICC's recommended wording.** The ICC suggests that freely-distributed profiles carry a licence permitting all use *on the condition* that altered versions remove the original identification (see `licensing-iccorg.txt`). monostar does not use that wording verbatim, because CC0 is a **waiver** rather than a licence — it reserves nothing, so it cannot impose a condition, and stating both would be incoherent. The request survives in the `LPIN` tag as what it honestly is: a courtesy, not a term. If you ship a modified monostar, please change its name — but nothing here requires you to.

---

## monopro

monostar was created as part of **monopro**, a macOS RAW/TIFF processing application for digital printing and alternative printmaking workflows. monopro derives luminance directly from CFA photosites, processes scene-linear in float32, applies the L\* encode at export, and embeds monostar in the grayscale TIFFs it writes. The profile is self-contained and carries no dependency on the application: it is an ICC v4.4 binary that any color-managed software can read.

[github.com/christophcunningham/monopro](https://github.com/christophcunningham/monopro)
