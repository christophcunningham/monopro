// LOG2.  Working -> WorkingLog.
//
// Linear scene value -> log2 stops. Pointwise.
//
// Contrast Mask must blur in **stops**: blurring linear light then taking the log is a
// different operator. A node rather than a step inside the blur, so the log signal is a
// typed edge — `StageRole::WorkingLog` — that cannot reach the curve, which indexes its
// LUT by the log of a *linear* value and would otherwise render plausibly and wrongly.
//
// Also the fork point: the mask reads this directly and the blur branch reads it too,
// so the buffer satisfies the union of both. The graph schedules that, not this.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

// -14 EV. The scene carries genuine zeroes and small negatives (sensor noise below the
// black point, left unrectified so SuperPixel averaging stays unbiased) and log2 of
// those is -inf.
//
// **The value matters because this signal gets blurred.** One sample at -24 EV drags a
// 250-tap mean down 0.09 stops by itself; -14 EV halves that and is already below every
// corpus sensor's noise floor. A whole row of zeroes is fixed at source in sample.wgsl,
// not papered over here.
const FLOOR: f32 = 6.1035156e-5;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    let v = log2(max(texel.r, FLOOR));
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(v, texel.g, 0.0, 0.0));
}
