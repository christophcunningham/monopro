// The zone masks, as the passes that read them see them: one texture ARRAY, one layer per
// mask, and a record per layer saying what is in it. Prepended to `dodge_burn.wgsl`,
// `curve.wgsl` and `display.wgsl`, so a Dodge & Burn layer's tone mask, a Curve mask's tone
// part and the mask view are worked out by the same code and cannot drift.
//
// `raw_core::zone` is where a mask comes from: the CPU worker reads each range's tones
// from the edge-aware basis at proxy resolution, grows its island and diffuses it, and
// `raw_gpu` uploads what `raw_core::zone::PreparedMask::upload` hands it. This file does
// the rest at each pixel, as `PreparedMask::at` does on the CPU.
//
// # The range is cut here, at the pixel
//
// A layer holds the TONE a range reads at each proxy pixel, and `mask_at` blends it to the
// pixel being rendered and cuts the range there. The masks used to arrive cut, and were
// blended up as they came: a steep range is a step from nothing to everything, and a step
// blended between proxy pixels drew every boundary as stairs a proxy pixel high, sixteen
// pixels of a 40 MP negative, in the picture and in the print. Blended first and cut
// second, a boundary runs smoothly between proxy pixels, and at a proxy pixel's center it
// is what it was. `raw_core::zone` says why the tones themselves are at proxy resolution.
//
// Two kinds of layer are not cut. A diffused mask arrives finished, since a blur needs the
// whole image, and is only blended. An island's weight has a layer of its own, named by
// the range it limits, and is blended and multiplied into the cut.
//
// # Why an array and not a strip
//
// The masks used to be stacked vertically in one texture, `mask_h` rows apiece. Eight
// Dodge & Burn layers and sixteen Curve passes at up to 480 rows is 11,520 rows, past
// WebGPU's default 8,192 for one dimension. A layer per mask has no such ceiling (the
// array limit is 256, and a mask and its island take two of the fifty there can be), and
// a mask's address is one number rather than a row offset that has to be added to a row.
//
// An array rather than a bindless set because the intermediates are not
// float32-filterable, so nothing here uses a sampler: `zone_blend` blends four texels
// itself.

@group(0) @binding(8) var masks: texture_2d_array<f32>;

// One layer's record: `raw_gpu::GpuTone`, written with the array.
struct ZoneTone {
    // The range, as `raw_core::ZoneMask` holds it: EV against middle gray, with the
    // shoulders' widths. Read only when the layer is cut.
    lo: f32,
    hi: f32,
    f_lo: f32,
    f_hi: f32,
    // ZONE_* bits.
    flags: u32,
    // The layer holding the range's island weight, or -1 for a range with no island.
    island: i32,
    pad0: u32,
    pad1: u32,
};

// The range has no lower bound, or no upper one: `ZoneMask::opens_below` and `opens_above`,
// decided on the CPU so the ruler's ends are known in one place.
const ZONE_OPEN_BELOW: u32 = 1u;
const ZONE_OPEN_ABOVE: u32 = 2u;
const ZONE_INVERT: u32 = 4u;
// The layer is a finished mask, or an island's weight: blended and not cut.
const ZONE_FINISHED: u32 = 8u;

@group(0) @binding(14) var<storage, read> zone_tones: array<ZoneTone>;

// One layer's value, blended bilinearly over the layer `layer`.
//
// Bilinear and not nearest, and the prototype learned this the hard way: nearest turns
// the proxy's pixels into hard blocks, which the guided filter then faithfully preserves
// as "edges" — a mask that reads as visibly pixelated at 100% while the Region slider
// appears to do nothing.
//
// `raw_core::zone`'s `bilinear` is this on the CPU; the two change together.
fn zone_blend(layer: i32, u: f32, v: f32) -> f32 {
    let mw = i32(p.mask_w);
    let mh = i32(p.mask_h);
    // Texel centers, so the corners of the mask are not half a texel adrift.
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

fn zone_ease(t: f32) -> f32 {
    let c = clamp(t, 0.0, 1.0);
    return c * c * (3.0 - 2.0 * c);
}

// How strongly the range selects a tone, before `Inv`: the trapezoid alone.
// `raw_core::ZoneMask::select`, line for line; the two change together.
fn zone_select(t: ZoneTone, ev: f32) -> f32 {
    var m = 1.0;
    if ((t.flags & ZONE_OPEN_BELOW) == 0u) {
        if (t.f_lo > 1e-6) {
            m = zone_ease((ev - (t.lo - t.f_lo)) / t.f_lo);
        } else {
            m = select(0.0, 1.0, ev >= t.lo);
        }
    }
    if ((t.flags & ZONE_OPEN_ABOVE) == 0u) {
        if (t.f_hi > 1e-6) {
            m = m * (1.0 - zone_ease((ev - t.hi) / t.f_hi));
        } else {
            m = m * select(0.0, 1.0, ev <= t.hi);
        }
    }
    return m;
}

// One mask's weight at a source-normalized point.
//
// The island before `Inv`: it limits what the range selects, and `Inv` then turns over
// the selection it leaves, so inverted it is everything but the island.
fn mask_at(layer: i32, u: f32, v: f32) -> f32 {
    let t = zone_tones[layer];
    let value = zone_blend(layer, u, v);
    if ((t.flags & ZONE_FINISHED) != 0u) {
        return value;
    }
    var m = zone_select(t, value);
    if (t.island >= 0) {
        m = m * zone_blend(t.island, u, v);
    }
    if ((t.flags & ZONE_INVERT) != 0u) {
        m = 1.0 - m;
    }
    return m;
}
