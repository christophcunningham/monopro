# Auxiliary rendering — implementation item 3

Cursor sampling and histogram rendering no longer invalidate an unchanged live
viewport. Blocking patches, export tiles and comparison cells follow the same
rule: their separate targets do not replace the live image.

## Changes

- Only live rendering updates the live render key and clears its pending source
  or target invalidation. Auxiliary requests always execute, without needing to
  mark the live image dirty before or after their work.
- Auxiliary requests preserve the live target's allocation notification and
  error state, including rejected requests. Comparison cells restore the prior
  live dirty state instead of forcing a redraw.
- Shared curve, toning and stroke resources still track the settings actually
  uploaded. Restoring resources after a different auxiliary look does not imply
  that existing live pixels are invalid. That restoration is recorded even when
  the subsequent live render is skipped, preventing repeated idle uploads.
- The bounded live Contrast Mask cache from items 1–2 survives cursor, histogram
  and export work. Those requests do not force another live mask calculation.

## Verification

Native GPU regressions exercise blocking and in-flight asynchronous samples,
histograms, exports and comparison cells, both with matching settings and with
different exposure, curve and toning settings. They assert unchanged live pixels,
no subsequent live dispatch, and no allocations from the skipped render. Separate
checks verify that pending source replacements, view changes and real parameter
edits still draw correctly, including comparison against a fresh renderer.

Full workspace tests: **1,073 passed, 0 failed, 4 ignored**. Clippy with all targets
and warnings denied, formatting and whitespace checks pass.

The release renderer was also exercised on DSCF0256.RAF (11648 × 8736, RCD) on
the M3 Max. With maximum Contrast Mask, both a 1 × 1 cursor sample and a full-frame
histogram were followed by an unchanged live render returning `false` (no work
submitted). Both returned `true` before this fix. The sample itself took about
5.6 ms; that processing cost remains.

This eliminates the redundant viewport render. Sampling itself still performs
its requested processing and readback; this change does not make every cursor
query free. Synchronous CPU zone preparation and CPU histogram cache granularity
remain review items 4 and 5.
