// BLUR — one axis of a separable Gaussian.  Role-preserving.
//
// Two nodes make one blur: `blur.x` then `blur.y`. Separate, so each declares a
// **one-axis apron** and the scheduler allocates a cross rather than a square. This is
// why `Apron` has four sides instead of a radius.
//
// Role-preserving: the same node blurs log data for Contrast Mask and linear data for
// output sharpening's bands.
//
// # The radius is in OUTPUT pixels
//
// `blur_sigma` and `blur_support` arrive already multiplied by the view scale, so a
// 60px spacer is 60px of blur at 100% and 9px at fit-to-screen: the cost scales with
// zoom, and the preview filters the same *physical* frequency the export does.
// Blurring a downsample is not identical to blurring then downsampling;
// `the_mask_survives_a_change_of_zoom` bounds the disagreement.
//
// # Edge handling: shrink the kernel, never invent data
//
// At distance `d` from the nearest edge of the input rectangle the radius becomes
// `min(r, d)`, so only real pixels are ever read and the mean stays centred. Clamping,
// renormalising and odd reflection were each measured and each gave up one of those;
// the table and the bug that found it are in `docs/decisions.md`.
//
// **View-independent, which is what keeps preview and export agreeing.** For a pixel at
// image position x the effective radius is `min(x, r)` wherever the viewport or tile
// boundary sits: past the image the rectangle is clamped to it, and short of it the
// apron guarantees a full r of real data both sides. `panning_does_not_change_a_masked_pixel`
// and the tile-seam tests cover the two halves.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rg32float, write>;

// One tap, addressed by its position along the blur axis.
fn at(base: vec2<i32>, along_x: bool, k: i32) -> f32 {
    let c = select(vec2<i32>(base.x, k), vec2<i32>(k, base.y), along_x);
    return textureLoad(src, c, 0).r;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    let g = abs_coord(gid.xy);
    let base = in_coord(g);
    let along_x = p.blur_axis == 0u;
    let pos = select(base.y, base.x, along_x);
    let lim = select(i32(p.in_h) - 1, i32(p.in_w) - 1, along_x);

    let sigma = max(p.blur_sigma, 1e-4);
    let inv = -0.5 / (sigma * sigma);

    // Symmetric, and inside the data. Every tap below is valid by construction,
    // so there is no clamp and no reflection anywhere in the loop.
    let r = min(i32(p.blur_support), min(pos, lim - pos));

    var acc = 0.0;
    var wsum = 0.0;
    for (var i = -r; i <= r; i = i + 1) {
        let w = exp(f32(i * i) * inv);
        acc = acc + at(base, along_x, pos + i) * w;
        wsum = wsum + w;
    }

    // Coverage passes through from this pixel rather than being blurred. It says
    // how much of the output pixel the image fills — geometry, not signal — and
    // smearing it would soften the image edge for no consumer: the contrast mask
    // takes coverage from the negative, never from the mask.
    let centre = textureLoad(src, base, 0);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(acc / wsum, centre.g, 0.0, 0.0));
}
