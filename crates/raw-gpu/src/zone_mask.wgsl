// The zone masks, as the passes that read them see them: one texture ARRAY, one layer per
// mask. Prepended to `dodge_burn.wgsl` and `curve.wgsl`, so a Dodge & Burn layer's tone
// mask and a Curve mask's tone part are looked up by the same code and cannot drift.
//
// `raw_core::zone` is where a mask comes from: the CPU worker evaluates each trapezoid on
// the edge-aware basis at proxy resolution, and `raw_gpu` uploads the results here. This
// file only reads them back.
//
// # Why an array and not a strip
//
// The masks used to be stacked vertically in one texture, `mask_h` rows apiece. Eight
// Dodge & Burn layers and sixteen Curve passes at up to 480 rows is 11,520 rows, past
// WebGPU's default 8,192 for one dimension. A layer per mask has no such ceiling (the
// array limit is 256, and 24 masks are the most there can be), and a mask's address is
// one number rather than a row offset that has to be added to a row.
//
// An array rather than a bindless set because the intermediates are not
// float32-filterable, so nothing here uses a sampler: `mask_at` blends four texels
// itself.

@group(0) @binding(8) var masks: texture_2d_array<f32>;

// One mask value, bilinear over the layer `layer`.
//
// Bilinear and not nearest, and the prototype learned this the hard way: nearest turns
// the proxy's pixels into hard blocks, which the guided filter then faithfully preserves
// as "edges" — a mask that reads as visibly pixelated at 100% while the Region slider
// appears to do nothing.
fn mask_at(layer: i32, u: f32, v: f32) -> f32 {
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
    let a = textureLoad(masks, vec2<i32>(x0, y0), layer, 0).r;
    let b = textureLoad(masks, vec2<i32>(x1, y0), layer, 0).r;
    let c = textureLoad(masks, vec2<i32>(x0, y1), layer, 0).r;
    let e = textureLoad(masks, vec2<i32>(x1, y1), layer, 0).r;
    return mix(mix(a, b, tx), mix(c, e, tx), ty);
}
