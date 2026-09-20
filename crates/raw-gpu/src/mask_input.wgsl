// Read the broad mask directly from source: no full-resolution apron textures.
// Exposure and the logarithm precede averaging, exactly as in the ordinary chain.
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

fn log_sample(c: vec2<i32>) -> f32 {
    let f = (vec2<f32>(c) + vec2<f32>(p.sub_x, p.sub_y) + vec2<f32>(0.5)) / p.input_scale;
    let v = sample_scene(source_from_frame(f), p.input_scale).x;
    return log2(max((v - p.black) * exp2(p.exposure_ev), 6.1035156e-5));
}

fn extended(c: vec2<i32>) -> f32 {
    let hi = vec2<i32>(i32(p.in_w) - 1, i32(p.in_h) - 1);
    let at = clamp(c, vec2<i32>(0), hi);
    let v = log_sample(at);
    return v + f32(max(c.x - hi.x, 0)) * (v - log_sample(max(at - vec2<i32>(1, 0), vec2<i32>(0))))
             + f32(max(c.y - hi.y, 0)) * (v - log_sample(max(at - vec2<i32>(0, 1), vec2<i32>(0))));
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) { return; }
    let factor = i32(round(p.input_scale / p.scale));
    let start = abs_coord(gid.xy) * factor;
    var sum = 0.0;
    for (var y = 0; y < factor; y = y + 1) {
        for (var x = 0; x < factor; x = x + 1) {
            let c = start + vec2<i32>(x, y);
            if (c.x < i32(p.in_w) && c.y < i32(p.in_h)) {
                sum = sum + log_sample(c);
            } else {
                sum = sum + extended(c);
            }
        }
    }
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(sum / f32(factor * factor), 1.0, 0.0, 0.0));
}
