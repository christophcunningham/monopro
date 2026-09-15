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

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;
@group(0) @binding(3) var<storage, read> lut: array<f32>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    let v = texel.r;
    let covered = texel.g;

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
