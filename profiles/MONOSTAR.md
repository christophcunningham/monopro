# monostar

An ICC v4.4 greyscale profile for CIE L*-encoded image data.
Created by C. Cunningham for [monopro](../README.md).

## Encoding

For normalised linear luminance `Y` in `[0, 1]`, monopro applies:

```text
L* = 116 × Y^(1/3) − 16    if Y > 0.008856
L* = 903.3 × Y            otherwise

X = L* / 100
code16 = round(X × 65535)
```

These are the rounded constants used by the application's
[L* encoder](../crates/raw-core/src/display.rs). Values are clamped to `[0, 1]`
for encoding.

The profile's `kTRC` describes the inverse direction: stored, normalised code `X`
to linear luminance `Y`. It uses an ICC parametric curve, type 3:

```text
Y = (a × X + b)^g    if X ≥ d
Y = c × X           otherwise
```

| Coefficient | Generator input | Stored signed 16.16 integer |
|---|---|---|
| g | 3 | 196608 |
| a | 100/116 | 56497 |
| b | 16/116 | 9039 |
| c | 100/903.3 | 7255 |
| d | 8/100 | 5243 |

Divide the stored integers by 65536 to obtain the decoded coefficients.
The curve is parametric, with fixed-point rounding of its coefficients.
The stored values satisfy `a + b = 1`; the difference between the two branches
at the breakpoint is less than `10⁻⁶` in normalised luminance.

## Profile

Values from [monostar.icc](monostar.icc):

| Field | Value |
|---|---|
| ICC version | 4.4.0 |
| Profile class | Display (`mntr`) |
| Colour space | `GRAY` |
| Profile connection space | `XYZ` |
| White point | D50; stored XYZ integers `63190, 65536, 54061` |
| Rendering intent | Perceptual (0) |
| Creation date | 2026-04-01 |
| Size | 996 bytes |
| Profile ID | `e4aab4b2e442adb8918125a7de0e50b0` |

The five tags are `wtpt` (white point), `kTRC` (decode curve), `desc` (name),
`cprt` (attribution and CC0 notice), and `LPIN` (construction and provenance).
The `kTRC` occupies 32 bytes.

The profile describes the encoding of greyscale values. Device-specific printer
or display characterisation belongs to the destination profile in a colour-managed
workflow.

## Use in monopro

monostar is the default greyscale export space. The print path is:

```text
scene-linear image → tone map → resize → grain → toning
→ output sharpening → frame → encode → quantise
```

Untoned monostar output stores one L*-encoded channel. Active colour toning uses
an RGB output space. The export controls offer monostar, sRGB, and eciRGB v2;
ProStarRGB remains supported for existing settings but is absent from the menu.
sRGB output uses its own transfer function.

The profile can also be used independently with image data encoded by the function
above. Assigning it to data with a different encoding changes the interpretation
of those values.

## Generation

The [generator](../crates/raw-core/src/icc.rs) constructs the profile from constants
and text metadata. From the repository root:

```sh
cargo run --locked -p raw-core --example write-profiles
cargo test --locked -p raw-core icc::tests
```

The tests check the coefficients, white point, profile ID, header, encoder/decoder
agreement, and byte-for-byte reproduction of the bundled profile.

## Licence

monostar is released under [CC0 1.0 Universal](LICENSE). The dedication is also
embedded in the profile. Other profiles in this directory retain their own notices.
