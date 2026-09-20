# Rendering memory bounds — implementation item 2

The high-zoom Contrast Mask failure is addressed by changing where the broad
mask is sampled, rather than enlarging the texture pool.

## Implementation

The wide-mask branch now reads exposed log luminance directly from the source.
Its sampling grid is capped at native resolution before reduction. It therefore
does not allocate a full-resolution negative covering the blur's surrounding
area, and zooming no longer multiplies that surrounding area's dimensions.
The visible negative still renders at the requested zoom with its original detail.
Source geometry and sampling are shared between the two shaders, including
rotation, perspective, coverage and edge handling. Small fitted previews reuse
the already sampled negative to avoid doing the same source sampling twice.

Before creating any derived target or intermediate, the renderer checks:

- The requested view and output dimensions are supported.
- Every rounded texture extent fits the actual device limit.
- The conservative sum of all plan textures, including the display target,
  stays within 512 MiB. This counts textures that may reuse the same allocation,
  so it does not depend on current pool contents or executor scheduling.

Rejected requests preserve the previous target and rendering state. The live
view and comparison cells display a message instead of showing stale pixels as
the requested result. Returning to a supported view works normally. Export taps
receive failure rather than attempting the rejected allocation.

The 512 MiB limit is **per render plan**, not a cap on the entire application.
Uploaded source images, retained viewport targets/caches, driver overhead and the
existing 256 MiB idle pool are additional. The pool budget was not increased.

## Validation

The 11648 × 8736 workload with a 2560 × 1600 viewport now plans below 256 MiB
across fit, 100%, 400% and 1600%, with both 1.5% and 5% mask spacers. These plans
pass even an 8192-pixel texture-edge limit. Their largest intermediate edges are
at most 2816 × 1856. A 3840 × 2160 viewport at 1600% also passes preflight.

This replaces the original 400% / 5% request for a 20032 × 19072 intermediate
(approximately 2.9 GiB for that single texture).

Native Metal tests on the supplied RAF exercised the actual high-zoom renderer,
not just graph arithmetic. On the M3 Max, maximum-spacer uncached panning took
approximately 8 ms at 400% and 7 ms at 1600%, averaged over four changed frames
with a GPU completion wait. These are render timings, not complete UI latency
or Windows measurements.

Fit readbacks and three 1024 × 768 native-resolution patches at both spacers
were byte-identical to item 1. The native wide-mask approximation introduced in
item 1 remains; this does not establish bit-identical output to the original
release at every zoom or for every photograph.

Regression coverage includes high-zoom fine detail, reuse during forced mask
rebuilds, texture-limit rounding, integer overflow, memory-budget rejection,
and recovery without allocation after oversized/invalid requests. Existing
border, crop, composition, export-seam and cache tests also pass.

Full workspace validation: **1,071 passed, 0 failed, 4 ignored** with native GPU
access. Clippy with all targets and warnings denied, formatting and whitespace
checks pass.

Cursor/histogram invalidation, synchronous CPU zone preparation and the remaining
review findings are still separate work. The next numbered item is auxiliary
readbacks unnecessarily invalidating the live viewport.
