# monopro bug review — 19 September 2026

Reviewed commit `5b9ada4`. This is a review report only; application source and the supplied photograph were not changed.

The reported slowdown and inconsistent arrow keys are reproducible. Contrast Mask has several compounding performance defects, including a high-zoom allocation failure. The wider review also found export, print-preview, keyboard-routing, and snapshot-comparison defects.

The supplied `DSCF0256.RAF` identifies as **Fujifilm GFX 100S**, with a working image of **11,648 × 8,736 pixels** (approximately 102 MP). Measurements below use the local **Apple M3 Max, 36 GiB RAM**, optimized release builds, actual Metal GPU access, default **RCD** reconstruction, and a **2,560 × 1,600-pixel viewport**. They are not measurements of your friend's computers.

| Contrast Mask setting | Fit view, one changed frame | 100% view, one changed frame |
|---|---:|---:|
| Off | 12 ms | 3 ms |
| Default spacer: 1.5% | 23 ms | 113 ms |
| Maximum spacer: 5% | 40 ms | 571 ms |

These are warmed render-path measurements averaged over four frames, changing display gamma each time and waiting for GPU completion. They exclude most application UI work and therefore are not end-to-end interaction latency. Values varied approximately 10–20% between runs. Fit and 100% sample different amounts of source data, so the off-state timings are not directly interchangeable.

At fit view, changing exposure takes approximately **35 ms** with the default mask spacer and **87 ms** with the maximum spacer in the render path alone. Additional application histogram work measured approximately **14–15 ms** per changed frame. These independent measurements should not be treated as an exact summed UI benchmark.

Priorities: **P1** means address in the first repair pass because of major responsiveness problems, rendering failure, or uncontrolled memory allocation. **P2** means a reproducible functional or correctness defect. “Reproduced” means a targeted test or measurement exercised the behavior; “source-confirmed” means the implementation and a reachable input establish the issue without a full desktop reproduction.

1. **[P1] Contrast Mask's Gaussian blur becomes prohibitively expensive at normal inspection zoom.**

   **Evidence:** Reproduced on the supplied RAF. At 100%, enabling the default mask raises a changed-frame render from approximately 3 ms to 113 ms; maximum spacer raises it to 571 ms. Every output pixel loops through the full blur kernel and recomputes an exponential weight for every tap, in both blur directions. The default full-resolution radius is approximately 656 pixels; maximum is approximately 2,184 pixels. Downstream changes such as gamma rerun this upstream blur.

   **Reproduction:** Open the RAF at default RCD sampling, enable Contrast Mask, zoom to 100%, and adjust a control or pan. Increase Spacer to 5% to amplify the stall.

   **Step 2:** Bound blur work with an appropriate reduced-resolution or more efficient filter, and retain valid upstream results when only downstream settings change. Preserve the mask's appearance, gray pivot, edges and export consistency.

   **Source:** [blur.wgsl](crates/raw-gpu/src/blur.wgsl:60); [graph construction](crates/raw-graph/src/lib.rs:414).

2. **[P1] Contrast Mask at supported zoom levels exceeds GPU texture limits; allocation churn starts much earlier.**

   **Evidence:** Reproduced graph planning without making dangerous allocations. On this RAF, a centered 2,560 × 1,600 viewport at **400% and 5% spacer** asks for an intermediate of **20,032 × 19,072 pixels**, approximately **2.9 GiB per large texture**. At **1600% and default spacer**, it asks for **23,528 × 22,568 pixels**. Both exceed the measured GPU limit of **16,384 pixels per edge**. The allocator passes these sizes directly to wgpu. A validation failure follows from this unchecked path; I did not deliberately crash the app to demonstrate it.

   At 100% and maximum spacer, large intermediates are already approximately **315 MiB each**, exceeding the entire **256 MiB idle texture-pool budget**. Four warmed frames allocated four new textures at default spacer and twelve at maximum spacer. The idle budget does not bound live allocations.

   **Reproduction:** Enable Contrast Mask on the RAF, set Spacer to 5%, and choose 400% zoom. This should be reproduced with allocation guards in place during implementation.

   **Step 2:** Bound processing resolution and working memory, validate derived texture sizes, and handle unsupported requests cleanly. Merely enlarging the texture pool will not solve this growth.

   **Source:** [zoom-scaled blur extent](crates/raw-graph/src/node.rs:165); [unchecked texture allocation](crates/raw-gpu/src/pool.rs:117); [pool retention limit](crates/raw-gpu/src/pool.rs:145).

3. **[P1] Moving the cursor forces unnecessary full-image redraws.**

   **Evidence:** Reproduced with the live render API. An unchanged warmed viewport correctly skips rendering. Requesting a **1 × 1-pixel cursor sample** makes the next identical viewport render run again. Histogram rendering does the same. These paths have separate target textures, but overwrite the live render's cached state and set its dirty flag. With maximum masking, the tiny sample itself took approximately **19 ms**, followed by the unnecessary viewport render.

   **Reproduction:** Leave image settings and zoom unchanged, then move the cursor over the photograph with Contrast Mask enabled. The automatic cursor readout requests these samples.

   **Step 2:** Separate live-view validity from auxiliary sampling/histogram state and avoid repeating expensive shared processing. Removing only the dirty assignment is insufficient because the cached render key is also overwritten.

   **Source:** [shared render state](crates/raw-gpu/src/lib.rs:1699); [histogram invalidation](crates/raw-gpu/src/lib.rs:2054); [sample invalidation](crates/raw-gpu/src/lib.rs:2113); [automatic cursor sampling](crates/raw-app/src/main.rs:9309).

4. **[P1] Exposure and mask edits run an unnecessary synchronous CPU blur before GPU rendering.**

   **Evidence:** Reproduced in release builds: the Dodge & Burn zone basis costs approximately **12.6 ms** at default spacer and **44.4 ms** at maximum spacer. It rebuilds synchronously whenever exposure or Contrast Mask changes, even with no Dodge & Burn adjustments present. This work occurs in the UI's render call.

   **Reproduction:** With no Dodge & Burn layers, enable Contrast Mask and drag Exposure or Spacer.

   **Step 2:** Make zone preparation demand-driven, keep its expensive work off the UI thread, and cache inputs appropriately. The zone histogram may still require this data when its panel is actually using it.

   **Source:** [unconditional zone rebuild](crates/raw-gpu/src/lib.rs:1662); [CPU Gaussian](crates/raw-core/src/zone.rs:344).

5. **[P2] Unrelated controls unnecessarily rebuild all CPU histogram data.**

   **Evidence:** Reproduced using the actual histogram module and supplied RAF with optimized libraries. Repeated unchanged refreshes cost effectively zero; changing only a Contrast Mask field or toggling Grain costs approximately **14–15 ms per refresh**. The cache key contains all parameters, but these CPU calculations only use a subset. Each invalidation rebakes the curve lookup table and recomputes tonal, curve-input and CFA RGB data. This happens before panel drawing, including when that data is not visible.

   **Reproduction:** Drag an export-only Grain control while an image is open, or change Contrast Mask. These changes invalidate CPU distributions whose calculation does not include those operations.

   **Step 2:** Give each distribution a cache key containing only its actual inputs and compute it only when needed.

   **Source:** [histogram cache and rebuild](crates/raw-app/src/histogram.rs:231); [per-frame histogram refresh](crates/raw-app/src/main.rs:11562).

6. **[P2] Many numeric fields cannot be nudged with Up/Down because the step rounds away.**

   **Evidence:** Reproduced with the installed egui 0.35 widgets. Shared slider rows set drag speed to `(maximum − minimum) / 300` and also force fixed decimals. Arrow-key edits use that speed and round immediately. When the increment is smaller than half a displayed unit, every keypress returns to the original value.

   Ten Up presses left these values unchanged: Contrast Mask contrast, custom luminance weighting, Grain crystal size/layers/density, Perspective correction and AgX White EV. Exposure and Gamma did change. Separate integer fields also fail: Contact Sheet columns remained **4 after ten Up presses**, because the inherited fractional speed rounds back to the same integer. Rename's integer controls use the same problematic pattern.

   **Step 2:** Define meaningful numeric increments consistent with precision and units, including explicit integer steps. Check both focused display mode and text-edit mode across the affected controls.

   **Source:** [shared numeric row](crates/raw-app/src/widgets.rs:534); [Contact Sheet integer fields](crates/raw-app/src/contact_sheet.rs:593); [Rename integer fields](crates/raw-app/src/rename.rs:222).

7. **[P2] Dodge & Burn tonal selection is shifted when Contrast Mask is enabled.**

   **Evidence:** Reproduced with uniform 18% gray. The GPU mask preserves gray, but the CPU zone basis places it **+0.865875 EV** above gray at the default contrast of 0.35; at 0.60 the discrepancy is approximately **+1.484 EV**. The CPU calculation omits the gray-pivot term present in the shader.

   **Reproduction:** Enable Contrast Mask and use a narrow Dodge & Burn tonal selection around middle gray. Selection and the zone distribution refer to brighter values than the actual image entering Dodge & Burn.

   **Step 2:** Align the two formulas and add CPU/GPU parity checks with active masking.

   **Source:** [CPU formula](crates/raw-core/src/zone.rs:126); [GPU formula](crates/raw-gpu/src/contrast_mask.wgsl:49).

8. **[P2] Undo while typing can undo photograph edits.**

   **Evidence:** Reproduced shortcut dispatch with a focused text editor: `text_edit_focused=true` still dispatches `Undo`. The typing guard suppresses bare and Shift keys only. The application then unconditionally applies Undo to the active photograph's history before drawing the text control. macOS menu commands also join that dispatch without a text-focus guard.

   **Reproduction:** Make a photograph edit, focus a caption, search or rename text field, type, then press Ctrl+Z / Command+Z. The image history can change even though the user is editing text.

   **Step 2:** Route text-editing commands to the focused editor; apply photograph history commands only when that editor does not own them.

   **Source:** [typing guard](crates/raw-app/src/hotkeys.rs:1126); [image Undo dispatch](crates/raw-app/src/main.rs:3063).

9. **[P2] Unicode captions can make PNG export fail completely.**

   **Evidence:** Reproduced with the real export function. A caption containing an em dash produced: `The text metadata cannot be encoded into valid ISO 8859-1`. The same image and metadata exported successfully to TIFF; an ASCII-only caption exported successfully to PNG. XMP is correctly stored as UTF-8, but additional Author/Copyright/Description copies are inserted into Latin-1-only PNG text chunks.

   **Reproduction:** Set Description to “An em dash — in a caption”, leave metadata export enabled, and export PNG. Curly quotes and non-Latin names also reach this failure path.

   **Step 2:** Use UTF-8 text chunks for these fields or omit incompatible redundant mirrors without failing image export.

   **Source:** [PNG metadata mirrors](crates/raw-app/src/export.rs:1490).

10. **[P2] JPEG export silently drops requested metadata.**

    **Evidence:** Reproduced with a unique copyright marker and caption. JPEG export succeeds but contains neither the supplied marker nor an XMP packet; TIFF contains both. The JPEG branch never receives the computed XMP packet, and its encoder writes the ICC profile and density without writing the requested descriptive metadata.

    **Reproduction:** Add creator/copyright/caption, enable metadata export, and write a JPEG proof.

    **Step 2:** Embed supported metadata in JPEG and verify a real readback, including Unicode values.

    **Source:** [JPEG export branch](crates/raw-app/src/export.rs:966); [JPEG encoder](crates/raw-app/src/export.rs:1262).

11. **[P1] Framed proofs bypass output-size limits and can attempt enormous allocations.**

    **Evidence:** Source-confirmed; dimensions reproduced without allocating the image. The size guard runs only for masters, on the assumption proofs cannot exceed their source. FRAME breaks that assumption. At the allowed 400-inch equal margins and 300 ppi, a full-size proof of this RAF resolves to **251,648 × 248,736 pixels**—approximately **233 GiB for a single grayscale f32 canvas**, before encoder buffers. The allocation path is unconditional. Much smaller margins can also exceed the limits when a proof retains native pixels while Output defines a small physical print.

    **Reproduction:** Use FRAME margins on a full-size proof large enough to exceed the documented 30,000-pixel edge / 400 MP limits. Do not use the extreme example as a live app test before fixing the guard.

    **Step 2:** Validate complete final dimensions and allocation sizes for every export type, before rendering or allocating.

    **Source:** [master-only limit check](crates/raw-app/src/export.rs:798); [canvas allocation](crates/raw-app/src/export.rs:1043); [allowed margin range](crates/raw-app/src/main.rs:8057).

12. **[P2] The print loupe omits Toning.**

    **Evidence:** Source-confirmed. The loupe requests scene-referred pixels, applies tone mapping, resizing, grain and sharpening, then always constructs grayscale display pixels. It neither passes Toning into its composition function nor applies the chemistry. Export applies Toning before sharpening, so the loupe can disagree in color, density and resulting sharpened detail.

    **Reproduction:** Enable a visibly colored toning treatment and open the print loupe. Compare it with the main viewport and export.

    **Step 2:** Share the appropriate export-tail processing with the loupe, including toned color conversion.

    **Source:** [loupe composition arguments](crates/raw-app/src/loupe.rs:583); [forced grayscale pixels](crates/raw-app/src/loupe.rs:606).

13. **[P2] The print loupe shows the wrong region/scale when output is reduced substantially.**

    **Evidence:** Source-confirmed by the request/response contract. Loupe source dimensions grow as output scale shrinks, but `Viewport::patch` silently clamps each dimension to 2,048. The loupe then stretches the truncated patch to the intended output tile. For this RAF reduced to 3,000 pixels wide, the 750-pixel-wide loupe requests approximately **2,912 source columns** even with grain/sharpening off; it receives only **2,048**.

    **Reproduction:** Resize this image to 3,000 pixels wide, open the loupe at 100%, and compare its covered area with the actual export.

    **Step 2:** Assemble the whole requested source region, or perform a correctly scaled render. Do not silently truncate a region and resample it as though it were complete.

    **Source:** [loupe source request](crates/raw-app/src/loupe.rs:561); [silent patch clamp](crates/raw-gpu/src/lib.rs:1992).

14. **[P2] Snapshot comparison can reapply bypassed modules.**

    **Evidence:** Reproduced through the same GPU cell-rendering API. A snapshot with Exposure set to +2 EV but disabled renders gray at byte value **220** when passed as Compare does, versus **117** when correctly resolved through `effective()`. Snapshot capture renders the thumbnail with effective parameters but stores authored parameters; Compare passes those authored parameters directly to the renderer. Disabled tonal-transform settings have the same missing resolution step.

    **Reproduction:** Set Exposure to +2 EV, bypass Exposure, capture a snapshot, capture/pin another, then open Compare. The compare cell can be much brighter than its saved thumbnail and restored live view.

    **Step 2:** Resolve module bypass consistently before rendering each snapshot.

    **Source:** [raw snapshot parameters passed to renderer](crates/raw-app/src/main.rs:8858); [bypass resolution](crates/raw-core/src/params.rs:571).

15. **[P2] Snapshot comparison ignores each snapshot's saved Decode/Luminance state.**

    **Evidence:** Source-confirmed. Compare takes the current tab's single luminance image and renders every saved look through that same viewport/source texture. It never reconstructs the snapshot's saved decode options, sampling algorithm or channel weighting. Those operations happen before the GPU renderer, so merely supplying the saved Params cannot apply them. The cell cache also omits a source-generation identifier.

    **Reproduction:** Capture one snapshot using Red channel weighting and another using Blue, then compare them. Both cells use the current weighting's source; restoring a snapshot can therefore differ from its compare cell. Changing the source after opening Compare can also leave cached cells stale until another key changes.

    **Step 2:** Associate each snapshot comparison with the correct prepared source and include its identity in cache validity.

    **Source:** [shared current luminance](crates/raw-app/src/main.rs:8745); [cell cache key](crates/raw-app/src/main.rs:8843).

The existing workspace suite passed with native GPU access: **1,062 passed, 0 failed, 4 ignored**. This includes **90 passing GPU render integration tests**. An initial sandbox run hid the GPU and produced eight adapter-related helper failures; those disappeared with native access. Some camera integration tests return early when the optional private `raws/` corpus is absent, so the pass count does not establish coverage for every camera. The supplied RAF was exercised separately for decoding, default reconstruction, GPU rendering and performance.

Additional targeted checks exercised egui arrow keys, focused-text shortcut dispatch, PNG/JPEG/TIFF metadata handling, snapshot bypass rendering, graph allocation dimensions, and CPU histogram invalidation. Large hazardous allocations were evaluated arithmetically, not attempted.

**Suggested step-2 order:** first address rendering bounds and Contrast Mask work (1–4), then restore reliable numeric controls and remove avoidable histogram work (5–6). Correct the tonal-selection math alongside those changes (7). Guard proof allocations (11) before further large-image export stress tests, then resolve text Undo, export metadata, loupe and snapshot correctness (8–10, 12–15). Regression checks should use synthetic data plus this approximately 102 MP workload at fit, 100%, 400% and the highest supported zoom, with active masking, cursor motion and realistic panel interactions.

**Remaining uncertainty:** no Windows machine, driver details, friend's exact release checksum/settings, or native PC timings were available. The defects above are portable, but their relative impact on that PC is not measured. The application already inherits a high-performance GPU preference; there is no evidence here that it deliberately selected a low-power GPU. This review does not certify every workflow or camera model as bug-free.

