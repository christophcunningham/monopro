# Tonal-selection math — review item 7

Implemented locally on 2026-09-20.

The CPU zone basis now uses the renderer's Contrast Mask gray pivot:

`output_log = input_log - contrast × (blurred_log - log2(0.18))`

Previously the pivot was omitted. A uniform 18% gray image was classified as +0.865875 EV at contrast 0.35, or +1.484359 EV at 0.60, even though the renderer preserved its brightness. This misplaced Dodge & Burn tonal selections and the distribution behind the zone ruler. Gray now remains at 0 EV.

The CPU Contrast Mask logarithm also uses the same 2^-14 input floor as the shaders. Previously its 1e-9 floor let zero and negative samples pull the blurred shadow basis much farther down than the rendered mask.

## Verification

Full workspace suite: **1,085 passed, 0 failed, 4 ignored**, including native Metal tests. Clippy with warnings denied, formatting and whitespace checks passed.

- CPU tests cover gray and exposed flat tones, black correction, and contrast strengths 0.05, 0.35 and 0.60.
- Native Metal tests compare the CPU basis with GPU output at three exposure levels and the same three strengths, within 0.00002 EV.
- A narrow tonal selection around each expected brightness admits a one-stop burn, verified through the full export path.
- A mixed image with negative, zero, gray and bright regions agrees within 0.0002 EV across interior transitions, testing the log floor and blur together.

These checks isolate arithmetic agreement. The existing 480-pixel CPU proxy, physical-edge filtering, and intentionally omitted Contrast Mask registration offset remain approximations; this does not claim pixel-exact masks at every resolution or border. Windows hardware was not available.

Existing edits with active Contrast Mask and bounded tonal selections may render differently because the masks now select the intended tones. Saved parameter values and file formats are unchanged.
