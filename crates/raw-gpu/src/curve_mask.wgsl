// A Curve pass's mask, as the two passes that read one see it: the record that describes
// it, and how much of the pass it lets through at a point of the negative.
//
// Prepended to `curve.wgsl`, which blends each masked segment by this weight, and to
// `display.wgsl`, which shows the selected pass's mask over the picture or in its place.
// One definition, so what the mask view draws is what the curve applied. It needs
// `gradients.wgsl`, `zone_mask.wgsl` and `dabs.wgsl` ahead of it, for the shapes, the
// tone lookup and the brush.
//
// The record is `raw_gpu::GpuSegment`. In the curve pass it also names a table; the mask
// view's copy has no table, and says in `len` how many dabs its painted shape has.
//
// # A painted mask
//
// A mask's shape can be strokes of Dodge & Burn's brush instead of a gradient. Its
// weight is then what has been painted at the pixel: within a pass the MAX of its dabs'
// coverage, passes SUMMED, an eraser pass (dabs with a negative `ev`) taking its
// coverage away, and the total held to [0, 1]. That is `Shape::painted_at` in
// `raw_core::dodgeburn`, and a layer's own coverage with the sign set aside.
//
// It cannot be read at one pixel the way a gradient can: it is a walk over the dabs
// that reach the pixel's tile, which `scan_painted` does once, ahead of everything
// else, for every painted mask at a time. `segment_weight` is then handed the answer.

struct Segment {
    // First entry of this segment's table in `tables`, and how many entries it has.
    table: u32,
    len: u32,
    // The shape: 0 none, 1 linear, 2 radial, 3 painted.
    kind: u32,
    // The shape's invert: what it selects it spares, and the reverse.
    invert: u32,
    // How a shape and a tone join: 0 Intersect, 1 Subtract, 2 Add.
    rule: u32,
    // Whether there is a tone part at all, and which layer of the zone-mask array holds
    // its mask. A tone with nothing to prepare (fully open) is there with layer -1, and
    // weighs one.
    has_tone: u32,
    tone_layer: i32,
    // Linear: from (g0, g1) to (g2, g3). Radial: center in (g0, g1), then inner, outer,
    // the ellipse's height over width, and its turn in RADIANS.
    g0: f32, g1: f32, g2: f32, g3: f32,
    g4: f32, g5: f32, g6: f32, g7: f32,
    feather: f32,
};

const KIND_NONE: u32 = 0u;
const KIND_LINEAR: u32 = 1u;
const KIND_PAINTED: u32 = 3u;

// How many masks can be painted at once: one per Curve instance.
const MAX_PAINTED: u32 = 16u;

// Each painted mask's coverage at this invocation's pixel, by the `inst` its dabs
// carry: the segment's index in the curve pass, zero in the mask view. Unclamped, as
// the walk leaves it; `segment_weight` holds it to [0, 1]. All zero until
// `scan_painted` runs, which is right for a stack with nothing painted.
var<private> painted: array<f32, 16>;

// Fill `painted` from the first `n` dabs of the buffer, for the pixel at (x, y).
//
// **Every invocation of the workgroup must call this, before any of them returns**:
// the cull inside it meets at barriers. So a pass calls it first, and tests whether its
// pixel is in range afterwards.
//
// The walk is Dodge & Burn's, for coverage alone. The dabs are sorted by (inst,
// gesture), so a change of either closes a pass, and one linear scan over the
// survivors recovers the two levels; a pass whose dabs were all culled is never seen
// and adds the nothing it would have added.
fn scan_painted(n: u32, lid: u32, wid: vec3<u32>, x: f32, y: f32, aspect: f32) {
    let tile = tile_bounds(wid);
    var started = false;
    var cur_inst = 0u;
    var cur_gesture = 0u;
    var gsign = 1.0;
    var m = 0.0;

    var base = 0u;
    loop {
        if (base >= n) { break; }
        let count = min(CULL_CHUNK, n - base);
        cull_dabs(base, count, lid, tile, aspect);

        var word = 0u;
        loop {
            if (word * 32u >= count) { break; }
            var bits = atomicLoad(&keep[word]);
            loop {
                if (bits == 0u) { break; }
                let b = firstTrailingBit(bits);
                bits = bits & (bits - 1u);
                let d = dabs[base + word * 32u + b];

                let new_gesture = !started || d.inst != cur_inst || d.gesture != cur_gesture;
                if (started && new_gesture) {
                    let at = min(cur_inst, MAX_PAINTED - 1u);
                    painted[at] = painted[at] + gsign * m;
                    m = 0.0;
                }
                if (new_gesture) {
                    // A pass paints or erases, and its first surviving dab says which:
                    // every dab of a pass has the same sign.
                    gsign = 1.0;
                    if (d.ev < 0.0) { gsign = -1.0; }
                }
                cur_inst = d.inst;
                cur_gesture = d.gesture;
                started = true;
                m = max(m, dab_coverage(d, x, y, aspect));
            }
            word = word + 1u;
        }
        base = base + count;
    }
    if (started) {
        let at = min(cur_inst, MAX_PAINTED - 1u);
        painted[at] = painted[at] + gsign * m;
    }
}

// The two parts of a mask joined. See raw_core::curve::Rule.
fn join_parts(rule: u32, shape: f32, tone: f32) -> f32 {
    var w = shape * tone;
    if (rule == 1u) {
        w = shape * (1.0 - tone);
    } else if (rule == 2u) {
        w = shape + tone - shape * tone;
    }
    return clamp(w, 0.0, 1.0);
}

// How much of this segment lands at (x, y): one for a pass that applies everywhere, the
// shape's weight, the tone's, or the two joined. `cover` is what `scan_painted` found
// painted there for this segment, read only when its shape is painted.
fn segment_weight(s: Segment, x: f32, y: f32, aspect: f32, cover: f32) -> f32 {
    let has_shape = s.kind != KIND_NONE;
    let has_tone = s.has_tone != 0u;
    if (!has_shape && !has_tone) {
        return 1.0;
    }
    var shape = 1.0;
    if (has_shape) {
        if (s.kind == KIND_PAINTED) {
            shape = clamp(cover, 0.0, 1.0);
        } else if (s.kind == KIND_LINEAR) {
            shape = linear_gradient(
                vec2<f32>(s.g0, s.g1), vec2<f32>(s.g2, s.g3), s.feather, x, y, aspect,
            );
        } else {
            // The mask's invert is applied below, once; the radial's own is not used.
            shape = radial_gradient(
                vec2<f32>(s.g0, s.g1), s.g4, s.g5, s.g6, s.g7, s.feather, false, x, y, aspect,
            );
        }
        if (s.invert != 0u) {
            shape = 1.0 - shape;
        }
    }
    var tone = 1.0;
    if (has_tone && s.tone_layer >= 0) {
        tone = mask_at(s.tone_layer, x, y);
    }
    if (has_shape && has_tone) {
        return join_parts(s.rule, shape, tone);
    }
    if (has_shape) {
        return shape;
    }
    return tone;
}
