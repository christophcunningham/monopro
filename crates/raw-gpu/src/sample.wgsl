// INPUT — the source node.  -> Working (scene-referred).
//
// Reads the stored luminance image under the view transform. Output is
// Rg32Float: R is the scene-referred value, G is coverage in [0, 1].
//
// **The only node that touches source geometry.** Everything downstream works on
// a plain pixel grid at a fixed scale, which is what makes ROI arithmetic
// tractable at all: apron, tiling, and pan all become integer rectangles on one
// grid rather than transforms that have to be composed.
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

// Bounds in SOURCE pixels. This node reads the stored image, not another node's
// buffer, so `in_holds` (which speaks grid coordinates) is not the check here.
fn in_bounds(x: i32, y: i32) -> bool {
    return x >= 0 && y >= 0 && x < i32(p.src_w) && y < i32(p.src_h);
}

fn load(x: i32, y: i32) -> f32 {
    let cx = clamp(x, 0, i32(p.src_w) - 1);
    let cy = clamp(y, 0, i32(p.src_h) - 1);
    return textureLoad(src, vec2<i32>(cx, cy), 0).r;
}

// Bilinear, for a composition that is not a quarter turn.
//
// The rule above this pass — "at or above 100%, show real pixels, no interpolation
// invents detail" — is a statement about the VIEW transform, where a source pixel
// really does sit under the sample position and rounding to it is the honest
// answer. A straighten breaks that premise: there is no source pixel under a
// rotated sample position, and nearest-neighbour would draw the staircase of the
// rotation rather than anything in the photograph. Interpolating here is not a
// softening, it is the only way to render a rotation at all.
//
// Only where it is needed. `p.resample` is zero for every quarter turn, so the
// existing path is bit-identical to what it was — an orientation fix must not
// quietly resample every frame in the corpus.
fn bilinear(s: vec2<f32>) -> f32 {
    // Sample positions are pixel CENTRES, so the texel grid is offset by a half.
    let t = s - vec2<f32>(0.5, 0.5);
    let i = floor(t);
    let f = t - i;
    let x = i32(i.x);
    let y = i32(i.y);
    let a = mix(load(x, y), load(x + 1, y), f.x);
    let b = mix(load(x, y + 1), load(x + 1, y + 1), f.x);
    return mix(a, b, f.y);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }

    let s = source_coord(gid.xy);
    let sxi = i32(floor(s.x));
    let syi = i32(floor(s.y));

    // The fallback value for a pixel the image does not cover.
    //
    // **Not zero.** Coverage says whether to SHOW this pixel; the value still has
    // to be a number every downstream stage can survive. Zero is a poison value:
    // log2 sends it to the floor, tens of stops below anything real, and a blur
    // then averages that into every pixel within its radius. That shipped as a
    // white halo along the bottom of the frame with Contrast Mask enabled —
    // visible only below 100% zoom, because only there does the box filter
    // produce a row it cannot fill, and invisible in export for the same reason.
    //
    // `load` clamps, so this is the nearest real pixel: plausible for a
    // neighbourhood operation, and never displayed, because coverage stays 0.
    var v = load(sxi, syi);
    var covered = 0.0;
    if (p.scale >= 1.0) {
        // At or above 100%, show real pixels. No interpolation invents detail —
        // unless the composition has rotated the frame, in which case there is no
        // real pixel here to show. See `bilinear`.
        if (in_bounds(sxi, syi)) {
            covered = 1.0;
            if (p.resample != 0u) {
                v = bilinear(s);
            }
        }
    } else {
        // Below 100% one output pixel covers many source pixels; averaging the
        // footprint avoids the aliasing shimmer that point-sampling gives on fine
        // grain. Out-of-bounds taps do not contribute, so the average stays honest
        // right up to the image edge instead of being dragged toward a smeared
        // edge pixel.
        let n = min(i32(ceil(1.0 / p.scale)), 16);
        let half = f32(n) * 0.5;
        var acc = 0.0;
        var taps = 0.0;
        for (var j = 0; j < n; j = j + 1) {
            for (var i = 0; i < n; i = i + 1) {
                let tx = i32(floor(s.x - half + f32(i) + 0.5));
                let ty = i32(floor(s.y - half + f32(j) + 0.5));
                if (in_bounds(tx, ty)) {
                    acc = acc + load(tx, ty);
                    taps = taps + 1.0;
                }
            }
        }
        if (taps > 0.0) {
            v = acc / taps;
            covered = taps / f32(n * n);
        }
    }

    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(v, covered, 0.0, 0.0));
}
