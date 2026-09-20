// INPUT — the source node.  -> Working (scene-referred).
//
// Reads the stored luminance image under the view transform. Output is
// Rg32Float: R is the scene-referred value, G is coverage in [0, 1].
//
// Shares source geometry and sampling with mask_input.wgsl through
// sample_common.wgsl. Downstream stages operate on plain pixel grids.
//
// Exposure used to live here and is now its own node. See exposure.wgsl.
//
// Coverage is carried rather than resolved here because the surround is a DISPLAY
// decision — it is an already-encoded grey — and this pass is scene-referred. If
// this pass composited the surround, every downstream node would be operating on a
// signal that is part scene data and part display constant, and the curve would
// pull the surround around with the image. Carrying coverage keeps the stage
// boundary honest and costs one channel.
//
// Note the dispatch: a genuine 2D grid over the OUTPUT extent, indexed with gid.xy
// directly. See docs/prototype-notes.md for why the prototype's flattened
// `gid.x + gid.y * 65535u` is silently wrong above 16.7 MP.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) { return; }
    let value = sample_scene(source_coord(gid.xy), p.scale);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(value, 0.0, 0.0));
}
