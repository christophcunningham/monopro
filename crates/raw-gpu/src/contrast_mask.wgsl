// CONTRAST MASK — the join.  WorkingLog x WorkingLog -> WorkingLog.
//
//   mask   = -blur(negative, spacer) * contrast
//   result = negative + mask
//
// Two inputs: the negative (binding 0, in place) and the mask (binding 5, shifted by
// the registration offset) — the two ends of the fork that started at log2.
//
// **Both are indexed through absolute grid coordinates, not `gid`.** The negative's
// buffer is wider than this region because it also fed the blur; indexing by `gid`
// offsets the mask against the negative by the apron, which reads as a registration
// error and gets blamed on the registration control.
//
// This compresses range rather than adding clarity: highlights sit above the local
// mean and are pulled down, shadows are lifted. See `docs/decisions.md`.
//
// # The pivot
//
// Subtracting `contrast * blur` outright also subtracts `contrast * mean`, which
// darkens the whole frame and would make the mask gamma slider double as a
// brightness slider. Pivoting at middle grey means a flat mid-grey field comes
// through unchanged, so the control changes local relationships and not overall
// exposure. In the darkroom this is the print exposure you add back to
// compensate for the mask's base density; folding it in is what makes the slider
// mean one thing.

@group(0) @binding(0) var neg: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;
@group(0) @binding(5) var mask: texture_2d<f32>;

// log2(0.18). Middle grey is the pivot everything else in this pipeline uses.
const PIVOT: f32 = -2.4739312;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let g = abs_coord(gid.xy);
    let n = textureLoad(neg, in_coord(g), 0);

    // Registration offset: the mask is read from a shifted position, which is a
    // deliberate misregistration of mask against negative. The graph charged the
    // apron for exactly this shift, in this direction.
    let shifted = g + vec2<i32>(i32(round(p.cm_off_x)), i32(round(p.cm_off_y)));
    let hi = vec2<i32>(i32(p.in2_w) - 1, i32(p.in2_h) - 1);
    let m = textureLoad(mask, clamp(in2_coord(shifted), vec2<i32>(0, 0), hi), 0);

    let out = n.r - p.cm_contrast * (m.r - PIVOT);

    // Coverage comes from the negative, not the mask: it describes how much of
    // this pixel the image fills, which the blur has smeared and the shift has
    // moved.
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(out, n.g, 0.0, 0.0));
}
