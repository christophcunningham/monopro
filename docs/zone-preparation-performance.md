# Zone preparation — implementation item 4

Exposure and Contrast Mask edits no longer compute a CPU zone basis when no
active Dodge & Burn adjustment uses a bounded tonal mask. This includes ordinary
editing with no layers, bypassed layers, and active strokes with unrestricted
tonal masks. Viewport creation no longer computes an unused default basis.

When needed, zone preparation runs on a CPU worker. Both the upstream basis and
the guided/blurred tonal-mask calculations run there. Interactive renders defer
until matching results are available, keeping the previous image visible and
requesting another frame. They never apply masks from an older source or setting.
Visible Dodge & Burn and Toning panels explicitly request their distribution;
closed panels create no histogram demand.

One job runs per viewport. Slider positions are not queued: after a running job
finishes, the next request determines what to prepare. Up to eight results are
retained at the existing bounded proxy resolution. The basis cache depends on
source generation, exposure and the Contrast Mask inputs actually used by the
CPU calculation. Changing tonal bounds reuses that basis; curves, gamma, brush
geometry and viewport geometry do not rebuild it.

Snapshot capture and comparison cells retry pending work. Explicitly blocking
exports, patch reads and one-shot thumbnail saves still wait for correct results;
they retain their existing blocking contract. GPU uploads and stroke packing
remain on the calling thread. This is not a claim that every application operation
is asynchronous.

## Measurements

Release renderer, supplied DSCF0256.RAF (11648 × 8736), RCD, M3 Max, 2560 × 1600
fit view, no tonal-masked adjustments or zone-panel demand. Four changed exposure
frames averaged with a GPU completion wait after each frame:

| Contrast Mask spacer | After item 3 | After item 4 |
| --- | ---: | ---: |
| 1.5% | 31.09 ms | 18.30 ms |
| 5% | 78.34 ms | 33.38 ms |

The viewport performed **zero zone-basis builds** throughout this editing run.
Cursor and histogram readbacks still left the live view valid, preserving item 3.
These measurements exclude other UI work and do not establish Windows timings.

## Validation and scope

**1,076 tests passed, 0 failed, 4 ignored** with native GPU access. Clippy with all
targets and warnings denied, formatting and whitespace checks pass. Regression
coverage includes unused preparation, active unrestricted strokes, cache input
selection, basis reuse across tonal bounds and histogram requests, bounded worker
concurrency, obsolete source results, and interactive output agreement with the
blocking renderer.

The existing CPU zone formula is unchanged. Its gray-pivot discrepancy remains
review item 7; this change preserves its output rather than silently changing
tonal selection. CPU histogram invalidation remains item 5.
