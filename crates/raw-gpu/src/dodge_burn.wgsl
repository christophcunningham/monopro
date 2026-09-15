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
// # The mask strip
//
// Zone masks arrive as one texture of up to MAX_INSTANCES masks stacked vertically,
// each mask_w x mask_h, computed on the CPU at proxy resolution — `raw_core::zone` says
// why that is the resolution the signal has. A strip rather than a 2D array because it
// is one plain texture_2d either way and the intermediates are not float32-filterable,
// so nothing here uses a sampler.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;
// Shared blurred log2 base for local contrast. The graph binds `src` here too
// when every layer's Contrast is zero, avoiding the spatial analysis entirely.
@group(0) @binding(5) var detail_base: texture_2d<f32>;

struct Dab {
    x: f32,
    y: f32,
    radius: f32,
    feather: f32,
    opacity: f32,
    ev: f32,
    // The nib: its own height/width ratio, its rotation in RADIANS (converted on the
    // CPU), 0 for round and 1 for a card, and the radius of the circle enclosing it.
    aspect: f32,
    angle: f32,
    nib: u32,
    bound: f32,
    // Which instance and which pass. Only ever compared for equality with the
    // neighbouring dab's, which is what makes the sorted scan work.
    //
    // `gesture` and not `pass`, which is the word the rest of the app uses for it:
    // `pass` is a WGSL reserved keyword and naga rejects it outright.
    inst: u32,
    gesture: u32,
};

struct Instance {
    // +1 dodge, -1 burn. The half of the number line an eraser cannot cross.
    sign: f32,
    opacity: f32,
    // Row offset of this instance's mask in the strip, or -1 for no mask.
    mask_row: i32,
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

@group(0) @binding(6) var<storage, read> dabs: array<Dab>;
@group(0) @binding(7) var<storage, read> instances: array<Instance>;
@group(0) @binding(8) var masks: texture_2d<f32>;

// # Culling, and why the loop has this shape
//
// The naive form — one thread per pixel over every dab — is linear in dab count:
// measured at 2.3 ms for one dab, 9.6 ms at 200 and **44 ms at a thousand** on a
// 2560x1600 viewport. A thousand dabs is an afternoon on one print, so that form does
// not get slow, it stops working the more you use it. With the cull it is roughly flat:
// 3.0 ms at a thousand. `tests/dodgeburn_cost.rs` re-takes both columns.
//
// Each workgroup culls the stroke list to the dabs that can reach its 8x8 tile, into a
// workgroup **bitmask** — not a compacted list, because order is load-bearing. The
// three-level composite is recovered by comparing each dab's (inst, gesture) with its
// neighbour's, which only works walking the survivors in their original order; an
// atomic append would give the right set in the wrong order.
//
// So the composite is a state machine over a possibly-gappy sequence rather than three
// nested loops. A gesture whose dabs are all culled is never seen and contributes zero,
// which is what it would have contributed anyway. A pass's sign comes from the first
// *surviving* dab — every dab in a pass shares a sign, so that is the same value.
//
// # There is no early return in this shader
//
// `workgroupBarrier` must be reached by every invocation in the workgroup. The
// usual `if (gid.x >= p.out_w) { return; }` guard would leave the threads past the
// edge of a partial tile sitting outside the barrier, which is undefined behaviour
// and, on some drivers, a hang. Out-of-range threads do the work and skip only the
// store.

// 32 bits per word. 4096 dabs per chunk is 512 bytes of workgroup memory, and
// stroke lists longer than that are walked in several chunks — the state machine
// carries across the boundary, so a chunk is invisible to the composite.
const CULL_WORDS: u32 = 128u;
const CULL_CHUNK: u32 = 4096u;
const THREADS: u32 = 64u;

var<workgroup> keep: array<atomic<u32>, 128>;

// Below this feather the profile is a hard disc. Matches raw_core Dab::HARD; the
// sigma below diverges as feather goes to zero, and the disc describes that shape
// exactly anyway.
const HARD: f32 = 0.01;

// One dab's unsigned contribution, in stops.
//
// **Two aspects, and they are not the same thing.** `frame` is the negative's h/w and
// undoes the mixed normalisation so that a nib at ratio 1 is round IN PIXELS;
// `d.aspect` is the nib's own ratio and makes it deliberately not round. Into width
// units first, THEN rotate, then scale by the nib's ratio — rotating in normalised
// space shears the shape. Same order as raw_core::dodgeburn::Dab::distance, which is
// the reference this must agree with.
//
// Returns zero at and beyond the boundary whatever the shape, which is what the cull
// below depends on.
fn dab_coverage(d: Dab, x: f32, y: f32, frame: f32) -> f32 {
    let dx = x - d.x;
    let dy = (y - d.y) * frame;
    let c = cos(d.angle);
    let s = sin(d.angle);
    let lx = dx * c + dy * s;
    let ly = -dx * s + dy * c;
    let r = max(d.radius, 1e-6);
    let nx = lx / r;
    let ny = ly / (r * max(d.aspect, 0.01));
    // Euclidean is an ellipse; Chebyshev is a rectangle — the boundary is where
    // EITHER axis reaches one rather than where they reach it together, which is
    // what gives a card its straight edge and its corners.
    var dist = sqrt(nx * nx + ny * ny);
    if (d.nib != 0u) {
        dist = max(abs(nx), abs(ny));
    }
    if (dist >= 1.0) {
        return 0.0;
    }
    var profile = 1.0;
    if (d.feather >= HARD) {
        let f = clamp(d.feather, HARD, 0.999);
        // In the nib's own normalised space, so the feather stays the same fraction
        // of the brush whatever shape or aspect it has.
        let sigma = 1.0 / sqrt(-2.0 * log(1.0 - f));
        let t = clamp(1.0 - dist, 0.0, 1.0);
        profile = exp(-0.5 * (dist / sigma) * (dist / sigma)) * t * t * (3.0 - 2.0 * t);
    }
    return profile * d.opacity;
}

// A linear gradient's strength: full at the press, zero at the release, unbounded
// perpendicular to the drag. See raw_core::dodgeburn::Linear, which is the
// reference this must agree with.
//
// Aspect-corrected, which the prototype's is not — its iso-lines are not
// perpendicular to a diagonal drag on a non-square frame.
fn linear_weight(s: Instance, x: f32, y: f32, aspect: f32) -> f32 {
    let dx = s.g2 - s.g0;
    let dy = (s.g3 - s.g1) * aspect;
    let len2 = dx * dx + dy * dy;
    // Every frame between the press and the first movement.
    if (len2 < 1e-10) {
        return 1.0;
    }
    let t = ((x - s.g0) * dx + ((y - s.g1) * aspect) * dy) / len2;
    let f = clamp(s.feather, 0.001, 1.0);
    let lo = 0.5 - f * 0.5;
    let hi = 0.5 + f * 0.5;
    let u = clamp((t - lo) / (hi - lo), 0.0, 1.0);
    return 1.0 - u * u * (3.0 - 2.0 * u);
}

// A radial gradient's strength: a spotlight, or a vignette when inverted.
//
// Into width units BEFORE the rotation, so the ellipse turns without shearing —
// and so `aspect = 1.0` is a real circle rather than the frame's shape.
fn radial_weight(s: Instance, x: f32, y: f32, aspect: f32) -> f32 {
    let a = s.g7;
    let c = cos(a);
    let si = sin(a);
    let qx = x - s.g0;
    let qy = (y - s.g1) * aspect;
    let lx = qx * c + qy * si;
    let ly = -qx * si + qy * c;

    let r = max(s.g5, 1e-4);
    let nx = lx / r;
    let ny = ly / (r * max(s.g6, 0.01));
    let dist = sqrt(nx * nx + ny * ny);

    let inner = clamp(s.g4 / r, 0.0, 0.999);
    let u = clamp((dist - inner) / max(1.0 - inner, 1e-6), 0.0, 1.0);
    let f = clamp(s.feather, 0.0, 1.0);
    // Blended rather than switched, so the feather control is continuous.
    let ramp = (1.0 - f) * u + f * (u * u * (3.0 - 2.0 * u));
    let w = 1.0 - ramp;
    if (s.invert != 0u) {
        return 1.0 - w;
    }
    return w;
}

// One mask value, bilinear over the strip row for `row`.
//
// Bilinear and not nearest, and the prototype learned this the hard way: nearest
// turns the proxy's pixels into hard blocks, which the guided filter then
// faithfully preserves as "edges" — a mask that reads as visibly pixelated at 100%
// while the Region slider appears to do nothing.
fn mask_at(row: i32, u: f32, v: f32) -> f32 {
    let mw = i32(p.mask_w);
    let mh = i32(p.mask_h);
    // Texel centres, so the corners of the mask are not half a texel adrift.
    let fx = clamp(u * f32(mw) - 0.5, 0.0, f32(mw - 1));
    let fy = clamp(v * f32(mh) - 0.5, 0.0, f32(mh - 1));
    let x0 = i32(floor(fx));
    let y0 = i32(floor(fy));
    let x1 = min(x0 + 1, mw - 1);
    let y1 = min(y0 + 1, mh - 1);
    let tx = fx - f32(x0);
    let ty = fy - f32(y0);
    let a = textureLoad(masks, vec2<i32>(x0, row + y0), 0).r;
    let b = textureLoad(masks, vec2<i32>(x1, row + y0), 0).r;
    let c = textureLoad(masks, vec2<i32>(x0, row + y1), 0).r;
    let e = textureLoad(masks, vec2<i32>(x1, row + y1), 0).r;
    return mix(mix(a, b, tx), mix(c, e, tx), ty);
}

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
    if (s.mask_row >= 0) {
        e = e * mask_at(s.mask_row, x, y);
    }
    return e * s.opacity;
}

// Local contrast follows the layer's painted coverage, Tone Mask and master
// opacity, but not its EV Intensity. Dodge/Burn direction is irrelevant: either
// kind of layer can increase or soften detail.
fn apply_contrast(idx: u32, coverage_in: f32, x: f32, y: f32) -> f32 {
    let s = instances[idx];
    var w = clamp(coverage_in, 0.0, 1.0);
    if (s.mask_row >= 0) {
        w = w * mask_at(s.mask_row, x, y);
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

    // This tile's footprint on the negative. An affine maps a rectangle to a
    // parallelogram, whose bounding box is exactly the box of its four corners —
    // so this is tight rather than conservative, even under a straighten.
    let ax0 = f32(i32(wid.x * 8u) + p.out_x);
    let ay0 = f32(i32(wid.y * 8u) + p.out_y);
    let c0 = source_at(ax0, ay0);
    let c1 = source_at(ax0 + 7.0, ay0);
    let c2 = source_at(ax0, ay0 + 7.0);
    let c3 = source_at(ax0 + 7.0, ay0 + 7.0);
    // One source pixel of slack in each direction. The box is exact in exact
    // arithmetic; the slack is for the last bits of the mantissa, because the
    // failure mode of a box that is a hair too small is a hard-clipped dab edge
    // along a tile boundary — an 8-pixel step in the middle of a soft falloff.
    let lo = min(min(c0, c1), min(c2, c3)) - vec2<f32>(1.0, 1.0);
    let hi = max(max(c0, c1), max(c2, c3)) + vec2<f32>(1.0, 1.0);
    let bx0 = lo.x / sw;
    let bx1 = hi.x / sw;
    let by0 = lo.y / sh;
    let by1 = hi.y / sh;

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

        var w = lid;
        loop {
            if (w >= CULL_WORDS) { break; }
            atomicStore(&keep[w], 0u);
            w = w + THREADS;
        }
        workgroupBarrier();

        // Mark every dab whose disc meets this tile. Closest-point test against the
        // box, in width-normalised units — the same units `dab_magnitude` measures
        // distance in, which is what makes the two agree exactly.
        var k = lid;
        loop {
            if (k >= count) { break; }
            let d = dabs[base + k];
            let ddx = d.x - clamp(d.x, bx0, bx1);
            let ddy = (d.y - clamp(d.y, by0, by1)) * aspect;
            // `d.bound`, not `d.radius`: the enclosing circle of the nib, which for
            // an elongated or rotated one is larger. Conservative — nothing that
            // could contribute is dropped — and less effective the longer the nib,
            // which is the honest trade.
            if (ddx * ddx + ddy * ddy < d.bound * d.bound) {
                atomicOr(&keep[k >> 5u], 1u << (k & 31u));
            }
            k = k + THREADS;
        }
        workgroupBarrier();

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
            w = linear_weight(s, x, y, aspect);
        } else {
            w = radial_weight(s, x, y, aspect);
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
    // The zone mask, on its own, for the instance the panel has selected. Shown
    // rather than the picture: a mask is a shape, and judging a shape against a
    // photograph it is drawn over is harder than judging it alone.
    if (p.db_show_mask >= 0 && u32(p.db_show_mask) < p.db_instances) {
        let s = instances[u32(p.db_show_mask)];
        if (s.mask_row >= 0) {
            out = mask_at(s.mask_row, x, y);
        } else {
            // No mask on that instance is not an error and must not read as a black
            // frame — an open mask selects everything, and white is what everything
            // looks like.
            out = 1.0;
        }
    } else if ((p.overlays & 8u) != 0u) {
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
