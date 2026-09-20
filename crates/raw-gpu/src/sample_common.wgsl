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


// Shared by the visible negative and the bounded mask source.
fn sample_scene(s: vec2<f32>, scale: f32) -> vec2<f32> {
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
    if (scale >= 1.0) {
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
        let n = min(i32(ceil(1.0 / scale)), 16);
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

    return vec2<f32>(v, covered);
}
