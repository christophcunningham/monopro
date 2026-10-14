// Pass 2 of 3 — TONE CURVE.  Working -> Working (scene-referred in, scene-referred
// out).
//
// The curve reshapes the negative. It does NOT fit the scene to the display — that
// is AgX's job at the tail, which is why AgX is not a curve preset. Both stages
// exist and they answer different questions.
//
// The spline itself is evaluated on the CPU (raw_core::curve, Fritsch-Carlson
// monotone cubic) and arrives here already baked into a LUT, so editing the curve
// costs a 256 KB buffer upload per drag rather than a shader recompile.
//
// The LUT is a STORAGE BUFFER, not a 1D texture: WebGPU's maxTextureDimension1D is
// 8192 by default, so the prototype's 65536-entry texture is not portable. A manual
// lerp over a buffer has neither that limit nor the float32-filterable requirement.
//
// This pass is skipped entirely when the curve is the untouched identity, so
// "the module is a no-op until touched" is literal rather than true-to-within-
// -quantisation.
//
// # A masked stack
//
// When any pass is limited to part of the picture, `p.curve_segments` is non-zero and
// the pass walks `segments` instead of reading `lut`. `CurveStack::apply_at` in
// `raw_core::curve` is the reference this must agree with, and the masked-curve test in
// `tests/render.rs` holds it to that.
//
// A segment is a table plus, if its pass is masked, what limits it: a shape, a tone, or both
// joined by a rule. Each
// table maps NORMALIZED curve space to normalized curve space with the pass's opacity
// already in it, so the walk is one rule applied in order:
//
//   t = t + (table(t) - t) * mask(x, y)
//
// and a pass that applies everywhere is a segment with neither, whose weight is one. A run
// of such passes is one segment, composed on the CPU.
//
// The mask at a pixel is `S` (the shape, if any: a gradient, or what has been painted
// there with Dodge & Burn's brush) and `T` (the tone, if any), joined as
// `raw_core::curve::Rule` says: Intersect `S*T`, Subtract `S*(1-T)`, Add `S+T-S*T`. Either
// alone is the whole mask. **A tone that is fully open still counts as there**: its `T` is
// one everywhere, which Subtracts away everything, and having no tone at all does not.
// `T` is read from the zone-mask array, one layer per tone, prepared on the CPU from the
// edge-aware basis; `zone_mask.wgsl` is where it is looked up and cut at the pixel.
// The record a segment is and the weight it has at a point are in `curve_mask.wgsl`,
// shared with the display pass, which shows the selected pass's mask.
//
// **The original single-table code below runs untouched when `curve_segments` is zero**,
// so a stack with no mask renders bit for bit as it did before masks existed. The
// segments and their tables are in bindings of their own rather than in `lut`: that code
// sizes its table with `arrayLength(&lut)`, and a buffer grown to hold sixteen tables
// would send it reading the wrong entries.
//
// A pixel a mask leaves alone comes back within the f32 round trip through log2 and exp2,
// a few parts in ten million, rather than bit for bit. Above the window the curve is flat,
// as it always was: a masked stack clamps a very bright tone to the window's top exactly
// as an unmasked one does.
//
// **Below the window each pixel scales linearly to meet its OWN floor**: the segments
// composed from t = 0 with that pixel's masks. A mask can lift the floor in one place
// and not another, so there is no one slope for the frame.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;
@group(0) @binding(3) var<storage, read> lut: array<f32>;

@group(0) @binding(11) var<storage, read> segments: array<Segment>;
@group(0) @binding(12) var<storage, read> tables: array<f32>;

// One segment's table at a normalized position, lerped. Same lookup the LUT gets, in
// normalized space rather than scene-linear.
fn table_at(s: Segment, t: f32) -> f32 {
    let at = clamp(t, 0.0, 1.0) * f32(s.len - 1u);
    let i = min(u32(floor(at)), s.len - 2u);
    let f = at - f32(i);
    return mix(tables[s.table + i], tables[s.table + i + 1u], f);
}

// A normalized tone through every segment, in order, at one pixel.
fn through(start: f32, x: f32, y: f32, aspect: f32) -> f32 {
    var t = start;
    for (var k = 0u; k < p.curve_segments; k = k + 1u) {
        let s = segments[k];
        let to = table_at(s, t);
        let m = segment_weight(s, x, y, aspect, painted[min(k, MAX_PAINTED - 1u)]);
        // Exactly the segment at full weight: `t + (to - t)` is not always `to`.
        if (m >= 1.0) {
            t = to;
        } else {
            t = t + (to - t) * m;
        }
    }
    return t;
}

@compute @workgroup_size(8, 8)
fn main(
    @builtin(global_invocation_id) gid: vec3<u32>,
    @builtin(local_invocation_index) lid: u32,
    @builtin(workgroup_id) wid: vec3<u32>,
) {
    // Where this pixel is on the negative, which is what the masks are laid on.
    // Source-normalized, so crop, rotation and export cannot move a mask.
    let sw = f32(p.src_w);
    let sh = f32(p.src_h);
    let sc = source_coord(gid.xy);
    let x = sc.x / sw;
    let y = sc.y / sh;
    let aspect = sh / sw;

    // **The painted masks first, before the range test below.** The scan culls with
    // barriers that every invocation of the workgroup has to reach, so a thread past
    // the edge of a partial tile does the scan and skips only the store. A stack with
    // nothing painted has no dabs and skips the scan altogether.
    if (p.curve_dabs > 0u) {
        scan_painted(p.curve_dabs, lid, wid, x, y, aspect);
    }

    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    let v = texel.r;
    let covered = texel.g;

    if (p.curve_segments > 0u) {
        let span = p.curve_hi_ev - p.curve_lo_ev;

        var masked: f32;
        // Sub-zero values are sensor noise below the black point, carried unrectified
        // from decode. Below the window a pixel scales to meet its own floor.
        var under = v <= 0.0;
        if (!under) {
            under = log2(v) <= p.curve_lo_ev;
        }
        if (under) {
            let floor_ev = p.curve_lo_ev + through(0.0, x, y, aspect) * span;
            masked = v * exp2(floor_ev) / exp2(p.curve_lo_ev);
        } else {
            let t = clamp((log2(v) - p.curve_lo_ev) / span, 0.0, 1.0);
            masked = exp2(p.curve_lo_ev + through(t, x, y, aspect) * span);
        }
        textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(masked, covered, 0.0, 0.0));
        return;
    }

    let n = arrayLength(&lut);
    // Slope needed to meet the curve at the bottom of the window. For the identity
    // curve this is exactly 1.0, which is what keeps black at black instead of
    // lifting it to 2^LO_EV.
    let below = lut[0] / exp2(p.curve_lo_ev);

    var out: f32;
    if (v <= 0.0) {
        // Sub-zero values are sensor noise below the black point, carried
        // unrectified from decode so SuperPixel averaging stays unbiased. The curve
        // is not the place to rectify them either.
        out = v * below;
    } else {
        let ev = log2(v);
        if (ev <= p.curve_lo_ev) {
            out = v * below;
        } else {
            let t = clamp((ev - p.curve_lo_ev) / (p.curve_hi_ev - p.curve_lo_ev), 0.0, 1.0)
                  * f32(n - 1u);
            let i = min(u32(floor(t)), n - 2u);
            let f = t - f32(i);
            out = mix(lut[i], lut[i + 1u], f);
        }
    }

    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(out, covered, 0.0, 0.0));
}
