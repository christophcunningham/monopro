# monostar

**An ICC v4.4 grayscale output profile encoding CIE L\* (CIELAB lightness) as its tone reproduction curve.**

Created by C. Cunningham / [monopro](https://github.com/christophcunningham/monopro)  
Released April 1, 2026 · Open · Freely redistributable

---

## What this is

monostar is a grayscale ICC v4.4 profile whose `kTRC` tag encodes the inverse CIE L\* transfer function as a `para` type 4 parametric curve — 7 exact rational coefficients with zero quantisation error. This means pixel values stored in a monostar-tagged TIFF are L\*-encoded — perceptually uniform — and any color-managed application will correctly decode them back to linear light for display or further processing.

To our knowledge, this is the first openly distributed grayscale ICC profile to use L\* as its TRC rather than a power-law gamma or dot-gain approximation.

---

## The math

### CIE L\* encoding (forward — what monopro writes)

Given a normalised linear luminance value Y in [0, 1]:

```
L* = 116 × Y^(1/3) − 16      if Y > 0.008856
L* = 903.3 × Y                if Y ≤ 0.008856
```

L\* is then normalised to [0, 1] and stored as a 16-bit integer:

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

This is encoded as an ICC v4 `para` type 4 parametric curve — 7 exact rational coefficients, zero quantisation error, 40 bytes. The equivalent 1024-point `curv` table would be 2,060 bytes with up to 0.03% rounding error at each sample point.

### The `para` type 4 coefficients

The ICC v4 `para` type 4 curve takes the form:

```
Y = (a·X + b)^g + e    for X ≥ d
Y = c·X + f            for X < d
```

For monostar, substituting the inverse L\* function (where X is the normalised code value in [0, 1]):

```
g = 3.0               (cube)
a = 100/116           ≈ 0.862069
b = 16/116            ≈ 0.137931
c = 100/903.3         ≈ 0.110706
d = 8/100  = 0.08     (L*=8 threshold in code-value space)
e = 0.0
f = 0.0
```

These are exact rational values derived directly from the CIE L\* definition. Encoded as s15Fixed16Number (signed 16.16 fixed point), the maximum rounding per coefficient is 1/65536 ≈ 0.0015%.

### Why the direction matters

Most common grayscale profiles — Gray Gamma 1.8, Gray Gamma 2.2, Dot Gain 20% — encode simple power-law curves where the forward and inverse functions are structurally identical (just reciprocal exponents). L\* is not symmetric: the shadow region is piecewise linear, and the cube-root region has a different shape in each direction. Embedding the forward L\* curve (linear → perceptual) as a TRC would cause every application to interpret stored values as significantly brighter than they are — roughly 1.5–2× overestimate in the shadows. monostar uses the inverse (decode) direction, which is what the ICC spec requires.

### Round-trip accuracy

The `para` parametric curve is mathematically exact — no sampling, no quantisation. The only rounding occurs in the s15Fixed16Number encoding of the 7 coefficients (fixed-point with 16 fractional bits), which introduces error of at most 1/65536 ≈ 0.0015% per coefficient. Verification of the decoded curve at key tonal values:

| Linear Y | L\* code | para decode Y | Error |
|:--------:|:--------:|:-------------:|:-----:|
| 0.0000 | 0.0000 | 0.0000 | 0.000% |
| 0.0089 | 0.0801 | 0.0089 | <0.002% |
| 0.2140 | 0.5000 | 0.2140 | <0.002% |
| 0.5000 | 0.7607 | 0.5000 | <0.002% |
| 1.0000 | 1.0000 | 1.0000 | 0.000% |

---

## Profile structure

| Field | Value |
|-------|-------|
| ICC version | 4.4.0 |
| Profile class | Display (`mntr`) |
| Colour space | Gray |
| PCS | XYZ |
| PCS illuminant | D50 (0.96420, 1.00000, 0.82491) |
| TRC tag | `kTRC` — `para` type 4 parametric curve (inverse L\*, 7 coefficients, 40 bytes) |
| Description | `mluc` enUS — "monostar" |
| Copyright | `mluc` enUS — "C. Cunningham / monopro project" |
| Creator | `CRC ` |
| Created | 2026-04-01 |
| File size | 536 bytes |

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

These processes respond to UV density rather than visual density, and require careful control of the tonal scale. L\* encoding distributes tonal steps perceptually uniformly, which aligns well with the linearization step used in digital negative workflows (QTR, Piccturrico, Jon Cone's Piezography). A monostar-tagged TIFF entering a linearized print pipeline will behave predictably.

### Archive and interchange

L\* is a device-independent perceptual encoding. A monostar-tagged file can be converted to any other gray working space by a color management engine with full accuracy, because the source encoding is precisely declared.

---

## What monostar is not

monostar is not a **device profile** — it does not characterise a specific monitor, printer, or scanner. It is a **data encoding profile**: it declares the mathematical meaning of the pixel values in a file. This is the same role played by sRGB (for RGB files) or Dot Gain 20% (for grayscale press files).

monostar does not perform dot-gain compensation. It is not intended for files going directly to an offset press. For press workflows, profiles derived from characterisation data (GRACoL, FOGRA, or a custom press profile) remain appropriate.

---

## Files

| File | Description |
|------|-------------|
| `monostar.icc` | The profile — 536 bytes, ICC v4.4, `para` TRC. Install in `~/Library/ColorSync/Profiles/` on macOS |
| `eciRGB_v2_ICCv4.icc` | eciRGB v2 ICCv4 from [eci.org](https://www.eci.org) — used by monopro for toned RGB exports |
| `DisplayP3-v4.icc` | Display P3 ICC v4 — 480 bytes, description string `sP3` — from [saucecontrol/Compact-ICC-Profiles](https://github.com/saucecontrol/Compact-ICC-Profiles) (CC0) — used by monopro for JPEG proof exports |

Display P3 is the native colour space of all Apple displays since 2016. monopro uses it for screen-destined JPEG proof exports so that colour-managed display on Apple Silicon Macs is exact.

---

## License

monostar is open and freely redistributable. No attribution required, though it is appreciated.

The profile binary is generated deterministically from first principles (CIE standard, ICC specification). No third-party ICC data or Adobe IP is incorporated.

---

## monopro

monostar was created as part of **monopro**, a macOS RAW/TIFF processing application for digital printing and alternative printmaking workflows. monopro processes in Linear Rec2020 float32, applies L\* encoding at export, and embeds monostar in every grayscale TIFF it produces.

[github.com/christophcunningham/monopro](https://github.com/christophcunningham/monopro)
