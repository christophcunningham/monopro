# Histogram cache performance — implementation item 5

CPU distributions now have separate cache keys and explicit consumers. The
unconditional frame refresh updates only the lightweight CPU mapping used by
sampling references and visual centering; it no longer samples the image or
replaces finished-image GPU bins with a CPU approximation.

- CFA RGB bins are prepared inside the open distribution section, only in RGB
  mode. Their inputs are decoded-scene generation, exposure, curve, tone map and
  gamma. Luminance reconstruction, crop, Contrast Mask, Toning, Grain and Dither
  do not affect this intermediate diagnostic.
- Curve samples are prepared inside the open Curve section. Their inputs are
  luminance generation, exposure and the sampled crop/transform. Curve and display
  edits reuse these pre-curve samples. The selected layer's distribution/average
  depends only on the curve stack through that layer; later layers and labels
  cannot invalidate it.
- The shared mapping rebakes its LUT only when the rendered curve changes.
  Exposure and gamma edits update scalar settings without rebaking the curve.
- Finished-image GPU histogram validity now ignores output-only settings and
  Dither. Grain edits preserve both completed and in-flight results. Exposure,
  Contrast Mask, Dodge & Burn, curves, display mapping, Toning, frame and source
  changes still invalidate it. This histogram remains needed by the footer's
  clipping readout even when the distribution panel is closed.

Source generation is tracked separately from luminance generation so switching
channel weighting or reconstruction does not rebuild unchanged CFA RGB data.
New image loads and re-decodes invalidate that diagnostic correctly.

## Measurement

Optimized CPU benchmark using DSCF0256.RAF (Fujifilm GFX 100S, 11648 × 8736), RCD,
on the local M3 Max. Fifteen changed iterations after warming the distributions:

| Edit | Original CPU refresh | Updated cache checks |
| --- | ---: | ---: |
| Contrast Mask spacer | 14.75 ms | <0.01 ms |
| Grain enabled | 14.46 ms | <0.01 ms |

The updated benchmark requests both CPU distributions and the selected curve
layer on every iteration, exercising their warmed caches even as if both panels
were visible. Hidden panels skip those requests entirely. These are CPU histogram
costs, not complete UI latency or GPU Contrast Mask timings; Windows was not tested.

## Validation

Full workspace suite with native GPU access: **1,080 passed, 0 failed, 4 ignored**.
Clippy with all targets and warnings denied, formatting and whitespace checks pass.

New regressions check no sampling for hidden distributions, LUT reuse, preservation
of GPU tonal bins, independent source/luminance invalidation, crop and exposure
refreshes, reuse across irrelevant edits and later curve layers, and acceptance
of in-flight GPU results across Grain/Dither changes. Existing CFA sampling,
curve-layer, readout, GPU histogram and rendering tests remain passing.

The next numbered review item is numeric arrow-key nudging (item 6).
