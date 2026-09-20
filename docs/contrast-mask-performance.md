# Contrast Mask performance — implementation item 1

Historical measurements for item 1. The subsequent [item 2 implementation](rendering-memory-bounds.md)
removes the full-resolution mask apron and adds allocation bounds; its results
supersede the high-zoom memory limitation described below.

Implemented following the September 19, 2026 bug review. This addresses the
expensive GPU mask blur and repeated upstream processing during downstream edits.

## Changes

- Wide mask blurs at native resolution and above use a reduced log-luminance
  grid. The negative retains its original resolution. Powers of two, anchored to
  the image, keep the mask stable across viewport and export tile boundaries.
- The reduction targets a Gaussian sigma of 16 reduced pixels, with a maximum
  reduction factor of 32. The physical blur radius is unchanged. Reconstruction
  interpolates the mask and handles partial cells at image boundaries.
- Views below 100% keep the original blur grid, preserving their existing edge
  behavior. Small blur kernels also retain their original path.
- Each viewport can retain its scene-linear Contrast Mask result, bounded to
  64 MiB including allocation rounding. Gamma, curves and other downstream edits
  can reuse it. Changes to the source, exposure, mask, geometry or required region
  invalidate it. Export and auxiliary readbacks do not overwrite this cache.

## Measurements

Release build on an Apple M3 Max with 36 GiB RAM, using DSCF0256.RAF (Fujifilm
GFX 100S, 11648 × 8736), RCD luminance and a 2560 × 1600 viewport. Each number is
the mean of four changed frames after warmup, with a GPU completion wait after
each frame. These are render timings, not end-to-end UI latency or Windows results.
The original measurements preceded implementation; normal run-to-run variation
applies.

| Gamma edit | Original | Updated |
| --- | ---: | ---: |
| Fit, 1.5% spacer | 23.15 ms | 1.34 ms |
| Fit, 5% spacer | 40.37 ms | 1.35 ms |
| 100%, 1.5% spacer | 113.49 ms | 1.35 ms |
| 100%, 5% spacer | 570.96 ms | 2.61 ms |

Updated uncached panning, moving one output pixel per frame, took 3.92 ms at
100% with the 1.5% spacer and 55.30 ms with the 5% spacer. At fit, it took
13.68 ms and 25.93 ms respectively. Thus the widest uncached blur still exceeds
a 60 Hz frame budget. The cached Gamma measurements created no new pool allocations.

## Image agreement and checks

- Fit-view readbacks with dithering disabled were byte-identical to the original
  at both tested spacers.
- Three 1024 × 768 native-resolution patches (interior, top-left and bottom-right)
  were compared in scene-linear output at both spacers. Maximum deviation was
  0.01223 stops, at an image edge; maximum interior deviation was 0.00050 stops.
  Native-resolution broad masks are an approximation, not bit-identical output.
- Regression tests cover an odd-sized logarithmic ramp through every border,
  export seams and unaligned patches, cache reuse/invalidation/source replacement,
  and graph regions at fractional scales, crops and small tiles.
- Full workspace tests with native GPU access: **1,067 passed, 0 failed, 4 ignored**.
- Workspace Clippy with all targets and warnings denied passed; formatting and
  whitespace checks passed.

## Remaining review items

This does not fix oversized high-zoom intermediate textures, synchronous CPU zone
basis/histogram work, auxiliary readbacks invalidating the live view, or numeric
arrow-key behavior. Fit-view exposure edits still measured 31.51 ms / 78.28 ms
at the two spacers. The full-resolution source apron still requires substantial
memory; the cache bound is not a bound on total application GPU memory.
