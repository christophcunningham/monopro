// DODGE & BURN.  Working -> Working (scene-referred in, scene-referred out).
//
//   out = in * 2^ev(x, y)
//
// Local exposure in the same unit the global slider speaks: +1 EV here doubles the
// scene luminance there, exactly. That literalness is why the total is clamped at the
// end rather than run through a tanh. Placement is `raw_graph::build`'s.
//
// # This is `raw_core::dodgeburn`, executed
//
// `DodgeBurnParams::ev_at` is the reference and this must agree with it pixel for
// pixel; `the_shader_agrees_with_the_cpu_reference` asserts it. Three nested levels,
// and the order is the model:
//
//   dabs within a gesture    MAX of magnitude   — one pass of the wand
//   gestures within instance SUM                — passes build
//   instances                SUM                — the stack
//
// The CPU packs the dab buffer sorted by (instance, gesture), so all three walk in one
// linear scan. Nothing here sorts or searches.
//
// # Coordinates are SOURCE-normalised
//
// A dab is stored against the luminance image, not the composed frame, so no
// orientation, straighten or crop can move a stroke off what it was painted on.
//
// **The aspect correction on dy is the trap.** x is normalised to the width, y to the
// height, and radius to the width alone — which keeps a dab a circle *in pixels*
// rather than an ellipse that changes shape with the negative's aspect. Backwards, it
// looks right on a square test image and wrong on every real one.
//
// The two gradients, `linear_gradient` and `radial_gradient`, are not here: they are in
// `gradients.wgsl`, which the executor prepends, because a curve's mask lays the same
// shapes on the same negative and the two must not drift. Neither is the dab: its
// record, its coverage and the cull are in `dabs.wgsl`, because a curve's mask can be
// painted with the same brush. What is here is what a *layer* makes of its dabs.
//
// # The zone masks
//
// Zone masks arrive as a texture array, one layer per mask, each mask_w x mask_h, computed
// on the CPU at proxy resolution — `raw_core::zone` says why that is the resolution the
// signal has. `zone_mask.wgsl` is where they are read, shared with the Curve pass, whose
// masks can have a tone part too.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;
// Shared blurred log2 base for local contrast. The graph binds `src` here too
// when every layer's Contrast is zero, avoiding the spatial analysis entirely.
@group(0) @binding(5) var detail_base: texture_2d<f32>;

struct Instance {
    // +1 dodge, -1 burn. The half of the number line an eraser cannot cross.
    sign: f32,
    opacity: f32,
    // Layer of this instance's mask in the zone-mask array, or -1 for no mask.
    mask_layer: i32,
    // 0 brush, 1 linear, 2 radial.
    kind: u32,
    // Linear: from (g0, g1) to (g2, g3). Radial: centre in (g0, g1).
    g0: f32, g1: f32, g2: f32, g3: f32,
    // Radial: inner, outer, ellipse aspect, angle in RADIANS — converted on the
    // CPU, once per change rather than once per pixel per frame.
    g4: f32, g5: f32, g6: f32, g7: f32,
    feather: f32,
    ev: f32,
    invert: u32,
    contrast: f32,
};

const KIND_BRUSH: u32 = 0u;
const KIND_LINEAR: u32 = 1u;
// Must match log2.wgsl: the detail base and the direct log are two sides of one
// subtraction, so using different floors manufactures detail in sensor-floor noise.
const LOG_FLOOR: f32 = 6.1035156e-5;

// A diagnostic map has a different job from the adjustment itself. Dividing a
// normal 0.25–0.5 EV brush by the full 4 EV safety ceiling made almost every useful
// stroke read as black. This display-only curve gives the working range enough
// separation to see while remaining monotonic: 0 stays 0, 0.5 EV reaches 50%, 1 EV
// reaches 75%, and the ceiling approaches white. It never enters the image result.
fn visible_map_value(ev: f32, ev_max: f32) -> f32 {
    let normalized = clamp(ev / max(ev_max, 1e-6), 0.0, 1.0);
    return 1.0 - exp2(-8.0 * normalized);
}

@group(0) @binding(7) var<storage, read> instances: array<Instance>;

// One instance's finished contribution: the eraser clamp, its zone mask, and its
// master opacity, in that order — which is `Instance::ev_at` followed by the two
// lines after it in `DodgeBurnParams::ev_at`.
fn apply_instance(idx: u32, ev_in: f32, x: f32, y: f32) -> f32 {
    let s = instances[idx];
    var e = ev_in;
    // The eraser clamp: erasing a burn reaches zero and stops, never crossing into
    // a dodge.
    if (s.sign > 0.0) {
        e = max(e, 0.0);
    } else {
        e = min(e, 0.0);
    }
    if (s.mask_layer >= 0) {
        e = e * mask_at(s.mask_layer, x, y);
    }
    return e * s.opacity;
}

// Local contrast follows the layer's painted coverage, Tone Mask and master
// opacity, but not its EV Intensity. Dodge/Burn direction is irrelevant: either
// kind of layer can increase or soften detail.
fn apply_contrast(idx: u32, coverage_in: f32, x: f32, y: f32) -> f32 {
    let s = instances[idx];
    var w = clamp(coverage_in, 0.0, 1.0);
    if (s.mask_layer >= 0) {
        w = w * mask_at(s.mask_layer, x, y);
    }
    return s.contrast * w * s.opacity;
}

@compute @workgroup_size(8, 8)
fn main(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    let sw = f32(p.src_w);
    let sh = f32(p.src_h);
    let aspect = sh / sw;

    // This tile's footprint on the negative, which the cull tests every dab against.
    let tile = tile_bounds(wid);

    let sc = source_coord(gid.xy);
    let x = sc.x / sw;
    let y = sc.y / sh;

    // p.db_dabs, NOT arrayLength: the buffer is over-allocated in doublings so a
    // drag does not reallocate per frame, and its tail holds whatever a longer
    // stroke list left there.
    let n = p.db_dabs;

    var total = 0.0;
    var total_contrast = 0.0;
    // The composite's state, carried across chunks.
    var started = false;
    var cur_inst = 0u;
    var cur_gesture = 0u;
    var gsign = 1.0;
    var cover_sign = 1.0;
    var m = 0.0;
    var cover_m = 0.0;
    var inst_ev = 0.0;
    var inst_cover = 0.0;

    var base = 0u;
    loop {
        if (base >= n) { break; }
        let count = min(CULL_CHUNK, n - base);

        cull_dabs(base, count, lid, tile, aspect);

        // Walk the survivors in order.
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
                    inst_ev = inst_ev + gsign * m;
                    inst_cover = inst_cover + cover_sign * cover_m;
                    m = 0.0;
                    cover_m = 0.0;
                }
                if (started && d.inst != cur_inst) {
                    total = total + apply_instance(cur_inst, inst_ev, x, y);
                    total_contrast = total_contrast + apply_contrast(cur_inst, inst_cover, x, y);
                    inst_ev = 0.0;
                    inst_cover = 0.0;
                }
                if (new_gesture) {
                    gsign = 1.0;
                    if (d.ev < 0.0) { gsign = -1.0; }
                    // Ordinary passes have the instance's sign; erasers oppose it.
                    cover_sign = gsign * instances[d.inst].sign;
                }
                cur_inst = d.inst;
                cur_gesture = d.gesture;
                started = true;
                let coverage = dab_coverage(d, x, y, aspect);
                m = max(m, coverage * abs(d.ev));
                cover_m = max(cover_m, coverage);
            }
            word = word + 1u;
        }

        base = base + count;
    }

    if (started) {
        inst_ev = inst_ev + gsign * m;
        inst_cover = inst_cover + cover_sign * cover_m;
        total = total + apply_instance(cur_inst, inst_ev, x, y);
        total_contrast = total_contrast + apply_contrast(cur_inst, inst_cover, x, y);
    }

    // The gradients, which the dab scan cannot reach because they have no dabs.
    // A second loop rather than a branch inside the first: it runs at most
    // MAX_INSTANCES times, it needs no culling — a gradient covers the whole frame
    // by construction — and folding it into the scan would put a test for a shape
    // that has no dabs inside the loop that walks dabs.
    for (var k = 0u; k < p.db_instances; k = k + 1u) {
        let s = instances[k];
        if (s.kind == KIND_BRUSH) {
            continue;
        }
        var w = 0.0;
        if (s.kind == KIND_LINEAR) {
            w = linear_gradient(vec2<f32>(s.g0, s.g1), vec2<f32>(s.g2, s.g3), s.feather, x, y, aspect);
        } else {
            w = radial_gradient(
                vec2<f32>(s.g0, s.g1), s.g4, s.g5, s.g6, s.g7, s.feather, s.invert != 0u, x, y, aspect,
            );
        }
        total = total + apply_instance(k, w * s.ev, x, y);
        total_contrast = total_contrast + apply_contrast(k, w, x, y);
    }

    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    total = clamp(total, -p.db_ev_max, p.db_ev_max);
    total_contrast = clamp(total_contrast, -1.0, 2.0);

    // `⇧D` / `⇧X`: show the map instead of the picture. Bits 8 and 16 of
    // p.overlays; see raw_gpu::Overlays. Only the half of the map matching the key
    // is shown, so holding one answers "where are my dodges" without the burns
    // filling in the same frame.
    var out = texel.r * exp2(total);
    if (abs(total_contrast) > 1e-6) {
        let log_value = log2(max(texel.r, LOG_FLOOR));
        let local_base = textureLoad(detail_base, in2_coord(abs_coord(gid.xy)), 0).r;
        out = exp2(log_value + total + total_contrast * (log_value - local_base));
    }
    // A layer's own mask is not shown from here: the display pass draws it, from the
    // record the mask view is handed, over the picture or in its place.
    if ((p.overlays & 8u) != 0u) {
        let map = visible_map_value(max(total, 0.0), p.db_ev_max);
        // Keep 15% of the photograph as registration context, then place the map over
        // it. Addition rather than interpolation is deliberate: an unpainted pixel is
        // only the dim photograph, while even a modest stroke rises unmistakably out
        // of that ground instead of replacing it with another dark value.
        out = clamp(texel.r * 0.15 + map * 0.85, 0.0, 1.0);
    } else if ((p.overlays & 16u) != 0u) {
        let map = visible_map_value(max(-total, 0.0), p.db_ev_max);
        out = clamp(texel.r * 0.15 + map * 0.85, 0.0, 1.0);
    }
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(out, texel.g, 0.0, 0.0));
}
