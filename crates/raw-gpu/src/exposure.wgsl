// EXPOSURE.  Working -> Working (scene-referred).
//
//   out = (in - black) * 2^stops
//
// Black FIRST, so raising exposure does not amplify the black offset. The black
// term is a fine trim on top of the sensor black level already subtracted at
// decode, and it is allowed to go negative so shadows can lift.
//
// NO clamping. +2 stops sends 1.0 to 4.0 and the display transform decides what
// happens to it — that is the whole reason the tone map is a separate stage.
//
// A node of its own rather than folded into the resampler, so it can be reordered or
// switched off. Coverage passes through untouched — a display concern, and this pass is
// scene-referred; see sample.wgsl.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    // The producer's buffer may be wider than this region and start elsewhere, so read
    // through the absolute grid coordinate rather than assuming they line up.
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    let scene = (texel.r - p.black) * exp2(p.exposure_ev);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(scene, texel.g, 0.0, 0.0));
}
