// EXP2.  WorkingLog -> Working.
//
// Back to linear scene-referred. Pointwise, the exact inverse of log2.wgsl apart from
// its floor, and unclamped — what happens above 1.0 is the tone map's job.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let texel = textureLoad(src, in_coord(abs_coord(gid.xy)), 0);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(exp2(texel.r), texel.g, 0.0, 0.0));
}
