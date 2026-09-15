// Pass 3 of 3 — DISPLAY TRANSFORM.  Working -> Display.
//
// This is the ONLY pass allowed to clamp, and the only one that produces
// display-referred values. Everything upstream is scene-referred and unbounded.
//
// Mirrors raw_core::display, which is the reference implementation and is what the
// histogram calls. If the two disagree, this file is wrong.
//
// Three concerns, deliberately not conflated: tone mapping (fit scene range into
// display range), transfer function (what the monitor expects), and display ICC (not
// implemented).
//
// The transfer is **not called sRGB**: only the EOTF is wanted, and sRGB's piecewise
// curve exists to dodge infinite slope at zero in 8-bit encoding. Modern displays are
// closer to pure 2.2.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<rgba8unorm, write>;
// Per-working-pixel Bayer-block clipping count, 0 to 4. R8Unorm stores the byte
// directly, so textureLoad returns count / 255.
@group(0) @binding(5) var clip_src: texture_2d<f32>;
// Chemical toning, baked to flat [y, a, b] triples over L*. See `raw_core::toning`.
@group(0) @binding(9) var<storage, read> tone_lut: array<f32>;
@group(0) @binding(10) var<storage, read_write> histogram: array<atomic<u32>>;

// The maximum over a capped footprint. Point sampling makes a fine clipped region
// sparkle below 100%; averaging makes a warning fade out. Maximum is conservative,
// and four taps per axis keeps a fit-to-screen diagnostic from becoming the frame's
// dominant cost.
fn sensor_censored(gid: vec2<u32>) -> f32 {
    let c = source_coord(gid);
    let span = max(1.0 / max(p.scale, 1e-6), 1.0);
    let taps = i32(clamp(ceil(span), 1.0, 4.0));
    let dims = vec2<i32>(textureDimensions(clip_src));
    var worst = 0.0;
    for (var j = 0; j < taps; j = j + 1) {
        for (var i = 0; i < taps; i = i + 1) {
            let t = vec2<f32>(f32(i), f32(j)) / f32(max(taps - 1, 1));
            let o = (t - 0.5) * span;
            let x = clamp(i32(floor(c.x + o.x)), 0, dims.x - 1);
            let y = clamp(i32(floor(c.y + o.y)), 0, dims.y - 1);
            worst = max(worst, round(textureLoad(clip_src, vec2<i32>(x, y), 0).r * 255.0));
        }
    }
    return worst;
}

// Display-referred luminance weights. See the note where `enc` is derived.
const LUMA_709 = vec3<f32>(0.2126, 0.7152, 0.0722);

// L*/100 from display-linear luminance. Matches `raw_core::display::lstar_encode`,
// which is the axis `ToningParams::bake` indexes on — the two must agree or the table
// is read at the wrong place.
fn lstar(y: f32) -> f32 {
    let v = clamp(y, 0.0, 1.0);
    var l: f32;
    if (v > 0.008856) {
        l = 116.0 * pow(v, 1.0 / 3.0) - 16.0;
    } else {
        l = 903.3 * v;
    }
    return clamp(l / 100.0, 0.0, 1.0);
}

// OKLab -> linear sRGB. Ottosson's reference matrices, the same constants as
// `raw_core::okhsl::oklab_to_linear_srgb`.
fn oklab_linear(l: f32, a: f32, b: f32) -> vec3<f32> {
    let l_ = l + 0.39633778 * a + 0.21580376 * b;
    let m_ = l - 0.105561346 * a - 0.06385417 * b;
    let s_ = l - 0.08948418 * a - 1.2914855 * b;
    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;
    return vec3<f32>(
        4.0767417 * l3 - 3.3077116 * m3 + 0.23096994 * s3,
        -1.268438 * l3 + 2.6097574 * m3 - 0.3413194 * s3,
        -0.0041960863 * l3 - 0.7034186 * m3 + 1.7076147 * s3,
    );
}

// The OKLab lightness of a NEUTRAL at this luminance.
//
// Exactly the cube root, and worth stating because L*/100 is the near-miss: for a
// neutral, l = m = s = Y, so every cube root is Y^(1/3) and the three L coefficients
// sum to 1. Feeding L*/100 in here instead would lighten every toned pixel.
fn oklab_l(y: f32) -> f32 {
    return pow(max(y, 0.0), 1.0 / 3.0);
}

// OKLab -> gamma-encoded-ready linear sRGB, with the chroma brought inside the gamut
// at **constant hue and lightness**.
//
// Clipping RGB afterwards shifts hue — a cyanotype blue that clips its blue channel
// comes out purple. Losing saturation is the honest failure; losing hue is not.
//
// Bisection rather than the analytic cusp, because `okhsl::max_saturation` is fitted to
// *sRGB's* boundary and would need refitting per export space.
//
// **The step count must match `raw_core::colour::GAMUT_STEPS`.** Two searches stopping
// at different points disagree by a code or two at every saturated tone, and the
// viewport and the export have to be the same picture.
fn oklab_to_srgb(l: f32, a: f32, b: f32) -> vec3<f32> {
    var lin = oklab_linear(l, a, b);
    if (all(lin >= vec3<f32>(0.0)) && all(lin <= vec3<f32>(1.0))) {
        return lin;
    }
    var lo = 0.0;
    var hi = 1.0;
    for (var i = 0; i < 12; i = i + 1) {
        let mid = 0.5 * (lo + hi);
        let probe = oklab_linear(l, a * mid, b * mid);
        if (all(probe >= vec3<f32>(0.0)) && all(probe <= vec3<f32>(1.0))) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    return clamp(oklab_linear(l, a * lo, b * lo), vec3<f32>(0.0), vec3<f32>(1.0));
}

// ── AgX ──────────────────────────────────────────────────────────────────────
// Troy Sobotka / Blender 4.0, piecewise sigmoid.
//
// A monochrome pipeline needs none of AgX's matrix work. The inset and outset
// matrices exist to rotate chroma inward before the sigmoid and back out after, so
// per-channel compression does not skew hue. With one channel there is no chroma to
// rotate: the log encode and the sigmoid ARE the transform. A real simplification,
// not a shortcut.

// The piecewise sigmoid, matching darktable's `agx` at its defaults, which matches
// Blender's AgX. NOT the 6th-order polynomial approximation three.js ships — that
// is a regression fitted to this curve and it places middle grey a quarter stop
// bright (0.2145 instead of 0.18) while lowering contrast throughout.
//
// Below the pivot the scale is NEGATIVE, which is what mirrors the toe: both
// (x - pivot_x) and the scale are negative, so the sigmoid argument stays positive
// and pow() never sees a negative base.
//
// Constants derived in raw_core::display; agx_constants_match_the_derivation
// recomputes them from darktable's _scale so they cannot go stale.

const AGX_PIVOT_Y: f32 = 0.45865645;   // 0.18^(1/2.2)
const AGX_GREY_EV: f32 = -2.4739312;   // log2(0.18)

// The gamma the curve is SHAPED in — darktable's `curve gamma`. NOT p.gamma.
const AGX_CURVE_GAMMA: f32 = 2.2;

fn agx_sigmoid(u: f32, power: f32) -> f32 {
    return u / pow(1.0 + pow(u, power), 1.0 / power);
}

fn agx_scale(lx: f32, ly: f32, tx: f32, ty: f32, slope: f32, power: f32) -> f32 {
    let epsilon = 1e-6;
    let pr = slope * max(lx - tx, epsilon);
    let ar = max(ly - ty, epsilon);
    let base = max(pow(ar, -power) - pow(pr, -power), epsilon);
    return min(pow(base, -1.0 / power), 1e9);
}

fn agx_segment(x: f32, pivot_x: f32, scale: f32, slope: f32, power: f32) -> f32 {
    return scale * agx_sigmoid(slope * (x - pivot_x) / scale, power) + AGX_PIVOT_Y;
}

fn agx_curve(x: f32, pivot_x: f32) -> f32 {
    let toe_scale = -agx_scale(
        1.0, 1.0, 1.0 - pivot_x, 1.0 - AGX_PIVOT_Y,
        p.agx_contrast, p.agx_toe_power
    );
    let shoulder_scale = agx_scale(
        1.0, 1.0, pivot_x, AGX_PIVOT_Y,
        p.agx_contrast, p.agx_shoulder_power
    );
    var r: f32;
    if (x < pivot_x) {
        r = agx_segment(x, pivot_x, toe_scale, p.agx_contrast, p.agx_toe_power);
    } else if (x > pivot_x) {
        r = agx_segment(
            x, pivot_x, shoulder_scale, p.agx_contrast, p.agx_shoulder_power
        );
    } else {
        r = AGX_PIVOT_Y;
    }
    return clamp(r, 0.0, 1.0);
}

// Returns DISPLAY-LINEAR. The trailing pow is load-bearing: the curve is shaped in
// a gamma-encoded space and the formulation linearises it afterwards. Without it
// the transfer function below encodes a second time and middle grey lands at 0.73
// instead of 0.46. See raw_core::display::agx.
fn agx(v: f32) -> f32 {
    // Its own log2 window, by design: AgX bypasses any black/white point clip.
    let min_ev = AGX_GREY_EV + p.agx_black_ev;
    let max_ev = AGX_GREY_EV + p.agx_white_ev;
    let ev = clamp(log2(max(v, 1e-10)), min_ev, max_ev);
    let x = (ev - min_ev) / (max_ev - min_ev);
    let pivot_x = -p.agx_black_ev / (p.agx_white_ev - p.agx_black_ev);
    return pow(agx_curve(x, pivot_x), AGX_CURVE_GAMMA);
}

// ── Soft shoulder ────────────────────────────────────────────────────────────
// Linear below the threshold, exponential roll-off above, asymptotic to 1.0.
// C1 continuous at the knee by construction — its derivative there is exactly 1.0,
// matching the linear segment below — so a smooth gradient gets no visible seam.
// Mirrors raw_core::display::shoulder.

fn shoulder(v: f32) -> f32 {
    let t = clamp(p.shoulder_t, 0.0, 0.999);
    let hard = clamp(v, 0.0, 1.0);
    if (v <= t) {
        return hard;
    }
    let w = 1.0 - t;
    let soft = t + w * (1.0 - exp(-(v - t) / w));
    return hard + (soft - hard) * clamp(p.shoulder_s, 0.0, 1.0);
}

// ── TPDF dither ──────────────────────────────────────────────────────────────
// Ported from monopro_core. Two uniforms from an integer hash, summed and offset,
// giving a triangular distribution on (-1, +1) LSB with zero mean — so the tone
// scale is statistically preserved. 8-bit truncation bands on the smooth extended
// gradients that Pt/Pd and photogravure live on, and the banding gets blamed on the
// paper. Invisible until missing.
//
// Keyed on the SOURCE pixel, not the viewport pixel, so the dither is locked to the
// image and does not crawl underneath it while panning. That is why this pass needs
// the view transform: it recomputes the source coordinate rather than having it
// piped through the intermediates. The view transform is ROI state that every node
// legitimately sees, so this is not a layering violation.

fn pixel_hash(x: u32, y: u32, salt: u32) -> u32 {
    var h = (x * 0x9E3779B1u) ^ (y * 0x85EBCA77u) ^ salt;
    h = h ^ (h >> 16u);
    h = h * 0x7FEB352Du;
    h = h ^ (h >> 15u);
    h = h * 0x846CA68Bu;
    h = h ^ (h >> 16u);
    return h;
}

fn tpdf(x: u32, y: u32) -> f32 {
    let inv = 1.0 / 4294967296.0; // 2^-32
    let u1 = f32(pixel_hash(x, y, 0x68E31DA4u)) * inv;
    let u2 = f32(pixel_hash(x, y, 0xB5297A4Du)) * inv;
    return u1 + u2 - 1.0;
}

/// Cinema-standard false-colour exposure map, transcribed from the Python
/// prototype's `to_false_color` — same stops, same convention (ARRI / Pomfort /
/// Divergent), so a zone reads the same in both apps.
///
/// Ten stops with a linear ramp between them. The ramp matters: stepping straight
/// between stops would band a smooth sky into stripes and invite reading a gradient
/// as a contour.
fn false_colour(v: f32) -> vec3<f32> {
    let x = clamp(v, 0.0, 1.0);
    var lo = 0.0;
    var hi = 0.04;
    var c0 = vec3<f32>(40.0, 10.0, 80.0);    // crushed black, no detail
    var c1 = vec3<f32>(30.0, 60.0, 200.0);   // deep shadow
    if (x >= 0.97)      { lo = 0.97; hi = 1.01; c0 = vec3<f32>(255.0, 200.0, 220.0); c1 = vec3<f32>(255.0, 255.0, 255.0); }
    else if (x >= 0.90) { lo = 0.90; hi = 0.97; c0 = vec3<f32>(220.0, 20.0, 20.0);   c1 = vec3<f32>(255.0, 200.0, 220.0); }
    else if (x >= 0.80) { lo = 0.80; hi = 0.90; c0 = vec3<f32>(240.0, 120.0, 0.0);   c1 = vec3<f32>(220.0, 20.0, 20.0); }
    else if (x >= 0.70) { lo = 0.70; hi = 0.80; c0 = vec3<f32>(230.0, 200.0, 0.0);   c1 = vec3<f32>(240.0, 120.0, 0.0); }
    else if (x >= 0.60) { lo = 0.60; hi = 0.70; c0 = vec3<f32>(160.0, 220.0, 20.0);  c1 = vec3<f32>(230.0, 200.0, 0.0); }
    else if (x >= 0.40) { lo = 0.40; hi = 0.60; c0 = vec3<f32>(0.0, 200.0, 60.0);    c1 = vec3<f32>(160.0, 220.0, 20.0); }
    else if (x >= 0.18) { lo = 0.18; hi = 0.40; c0 = vec3<f32>(30.0, 180.0, 180.0);  c1 = vec3<f32>(0.0, 200.0, 60.0); }
    else if (x >= 0.04) { lo = 0.04; hi = 0.18; c0 = vec3<f32>(30.0, 60.0, 200.0);   c1 = vec3<f32>(30.0, 180.0, 180.0); }
    let t = (x - lo) / max(hi - lo, 1e-6);
    return mix(c0, c1, clamp(t, 0.0, 1.0)) / 255.0;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.out_w || gid.y >= p.out_h) {
        return;
    }
    // The canvas outside the image, already display-encoded, so it is applied after
    // the transform rather than through it. A uniform rather than a constant: it is
    // a user setting, and the egui-drawn letterbox reads the same number so the two
    // cannot seam.
    let SURROUND: f32 = p.background;

    // This is the ONLY node whose region may extend past the image: the viewport
    // is larger than the image whenever the view is fitted, and the excess is the
    // surround. Every upstream region is clamped to the image, so a pixel outside
    // the input's rectangle has no scene data behind it by construction.
    let g = abs_coord(gid.xy);
    if (!in_holds(g)) {
        // Outside the image: the mount if we are within its width of the frame,
        // otherwise the canvas.
        //
        // Chebyshev distance, so the corners are square. A mount is cut with a
        // knife and a straight edge; a rounded or mitred corner would be a
        // decoration rather than a reference.
        var canvas = vec3<f32>(SURROUND, SURROUND, SURROUND);
        if (p.surround_w > 0.0) {
            let l = in_coord(g);
            let dx = max(max(-l.x, l.x - i32(p.in_w) + 1), 0);
            let dy = max(max(-l.y, l.y - i32(p.in_h) + 1), 0);
            if (f32(max(dx, dy)) <= p.surround_w) {
                canvas = vec3<f32>(p.surround_r, p.surround_g, p.surround_b);
            }
        }
        textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(canvas, 1.0));
        return;
    }

    let texel = textureLoad(src, in_coord(g), 0);
    let v = texel.r;
    // Coverage still matters inside the image: at the last, partly-filled column
    // the box filter has fewer taps, and compositing by coverage is what keeps
    // that edge anti-aliased rather than hard.
    let covered = clamp(texel.g, 0.0, 1.0);

    if (covered <= 0.0) {
        textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(SURROUND, SURROUND, SURROUND, 1.0));
        return;
    }

    // Tone map. The clamp belongs here and nowhere upstream.
    var tone: f32;
    if (p.tone_map == 1u) {
        tone = agx(v);
    } else if (p.tone_map == 2u) {
        tone = shoulder(v);
    } else {
        tone = clamp(v, 0.0, 1.0);
    }

    // -- Toning ---------------------------------------------------------------
    //
    // Here, and not as a node of its own, because a toner acts on a **print** and the
    // print does not exist until the tone map above has run. Everything upstream is
    // scene-referred, so there is no point in the graph where a toning node could read
    // the value it needs. The signal point is identical to the CPU export tail's, which
    // is what preview-equals-export actually requires; the node boundary is not.
    //
    // The table is indexed by L* rather than by luminance, because L* is perceptually
    // uniform: a table on luminance spends most of its entries on the highlights and
    // almost none on the shadows, which is where toning does its work.
    var lit = tone;
    var lab_a = 0.0;
    var lab_b = 0.0;
    if (p.toning == 1u) {
        let n = arrayLength(&tone_lut) / 3u;
        let t = clamp(lstar(tone), 0.0, 1.0) * f32(n - 1u);
        let i = min(u32(floor(t)), n - 2u);
        let f = t - f32(i);
        lit    = mix(tone_lut[i * 3u],      tone_lut[(i + 1u) * 3u],      f);
        lab_a  = mix(tone_lut[i * 3u + 1u], tone_lut[(i + 1u) * 3u + 1u], f);
        lab_b  = mix(tone_lut[i * 3u + 2u], tone_lut[(i + 1u) * 3u + 2u], f);
    }

    // Transfer function.
    var enc3: vec3<f32>;
    if (p.toning == 1u) {
        // OKLab -> linear sRGB, then the display's plain gamma per channel. The
        // expansion to primaries happens **here**, at the boundary, because OKLab has
        // no primaries and needs none until something has to be encoded.
        //
        // Chroma is clamped to the sRGB gamut at this hue and lightness rather than the
        // RGB being clipped afterwards: clipping a channel shifts hue, so a cyanotype
        // blue that clips would come out purple. Losing saturation is the honest
        // failure; losing hue is not.
        let lin = oklab_to_srgb(oklab_l(lit), lab_a, lab_b);
        enc3 = pow(max(lin, vec3<f32>(0.0)), vec3<f32>(1.0 / max(p.gamma, 0.01)));
    } else {
        enc3 = vec3<f32>(pow(tone, 1.0 / max(p.gamma, 0.01)));
    }

    // The scalar the diagnostic overlays are written against.
    //
    // **Luminance of the toned pixel, not its red channel.** The overlay thresholds are
    // statements about exposure, so a clipping warning that fired on one primary would
    // flag a saturated tone as a blown highlight. Untoned, `enc3` is three equal
    // channels and this is exactly the value it always was.
    //
    // Rec.709 coefficients are correct *here* and nowhere upstream: this is a
    // display-referred, already-encoded sRGB triple, which is the signal those
    // coefficients are a colorimetric statement about. Applying them to sensor-referred
    // data is the thing `monopro-rs-handoff.md` forbids, and this is not that.
    let enc = select(enc3.r, dot(enc3, LUMA_709), p.toning == 1u);

    if (p.histogram == 1u) {
        let last = arrayLength(&histogram) - 1u;
        let bin = min(u32(clamp(enc, 0.0, 1.0) * f32(last)), last);
        atomicAdd(&histogram[bin], u32(round(covered * 256.0)));
    }

    var q3 = enc3 * 255.0 + 0.5;
    if (p.dither == 1u) {
        let s = source_coord(gid.xy);
        q3 = q3 + tpdf(u32(max(i32(floor(s.x)), 0)), u32(max(i32(floor(s.y)), 0)));
    }
    let o3 = clamp(floor(q3), vec3<f32>(0.0), vec3<f32>(255.0)) / 255.0;
    // The scalar the overlays are written against. It is the **luminance** of the toned
    // pixel rather than its red channel: the overlay thresholds are statements about
    // exposure, and a clipping warning that fired on one primary would flag a saturated
    // tone as a blown highlight. Untoned, `o3` is three equal channels and this is
    // exactly the value it always was.
    let o = select(o3.r, dot(o3, LUMA_709), p.toning == 1u);

    // -- Diagnostic overlays -------------------------------------------------
    //
    // Everything below reads the FINAL display-encoded value, which is what the
    // Python prototype's overlays read too — the thresholds are its 8-bit numbers
    // and the colours are its colours, so the two apps flag the same pixels.
    var rgb = o3;

    // -- The mask view ------------------------------------------------------
    //
    // Yellow, darktable's convention: a greyscale coverage map is indistinguishable at
    // a glance from a very flat photograph, and a hue says "this is not your picture"
    // before you have read anything. Slightly warm rather than #FFFF00, which vibrates
    // against this panel's near-black.
    //
    // **Multiplies rather than tints**, so black stays black and the mask's own gradient
    // — the thing you opened the view to judge — survives.
    if (p.db_show_mask >= 0) {
        rgb = vec3<f32>(o, o, o) * vec3<f32>(1.0, 0.886, 0.157);
    }

    // -- Rubylith: the dodge and burn maps ----------------------------------
    //
    // Multiplies, like the mask view above and for the same reason: an *additive* tint
    // would flood the unpainted ground and hide the boundary you are looking for.
    //
    // Ruby for burn, cool blue for dodge — `theme::BURN` and `theme::DODGE`. The theme's
    // rule that the two are also told apart by *value* is deliberately not carried
    // over: these are shown one at a time, so there is nothing to read a value
    // difference against.
    if ((p.overlays & 8u) != 0u) {
        rgb = vec3<f32>(o, o, o) * vec3<f32>(0.569, 0.890, 0.988);
    }
    if ((p.overlays & 16u) != 0u) {
        rgb = vec3<f32>(o, o, o) * vec3<f32>(0.980, 0.533, 0.510);
    }

    // False colour replaces the image; the clipping overlays sit on top of
    // whichever is underneath. Cinema convention (ARRI/Pomfort), ten stops with a
    // linear ramp between them, so a gradient reads as a gradient rather than as
    // bands.
    if ((p.overlays & 4u) != 0u) {
        rgb = false_colour(o);
    }

    // The checkerboard is keyed to SCREEN position, not to the source.
    //
    // That is the opposite of the dither above, and deliberately: the dither must
    // not crawl under a pan because it sits in the picture, whereas this has to
    // stay legible as a checkerboard at every zoom. Keyed to the source it would
    // alias into a flat 50% tint at fit-to-screen, and the near-clip tier would
    // stop being distinguishable from the solid one — which is the whole
    // information it carries.
    let checker = ((gid.x + gid.y) & 1u) == 1u;

    // **Quantised WITHOUT the dither**, unlike the pixel that gets drawn.
    //
    // TPDF dither is ±1 LSB of deliberate noise added to break banding. Testing the
    // tier thresholds against it lets that noise decide whether a pixel reads as
    // clipped — so a shadow at exactly 0 dithers to 1 and goes unflagged while its
    // neighbour at 1 dithers to 0 and does, and a region that is uniformly crushed
    // comes out as speckle instead of a shape. The overlay is a diagnostic of the
    // image; it must not be a diagnostic of the dither.
    //
    // This is also what makes the checkerboard read as a checkerboard: it only
    // looks like one across a contiguous run of same-tier pixels, and dither was
    // breaking every run into single pixels.
    let q8 = clamp(floor(enc * 255.0 + 0.5), 0.0, 255.0);

    // Overexposed, lowest tier first so each overwrites the last.
    //
    // **Widened from the prototype's numbers, because its top two tiers collapse
    // here.** It distinguished "any channel clipped" (red) from "all three clipped"
    // (black) — two different tests in a colour image. There is one channel here,
    // so those became the same test, red was a seven-level sliver, and anything
    // clipped went straight to black with no warning band worth seeing.
    //
    // In stops below display white: magenta from about -0.7, red from about -0.2.
    if ((p.overlays & 1u) != 0u) {
        if (q8 >= 204.0 && checker) { rgb = vec3<f32>(1.0, 0.267, 0.667); }
        if (q8 >= 240.0)            { rgb = vec3<f32>(1.0, 0.125, 0.125); }
        if (q8 >= 255.0)            { rgb = vec3<f32>(0.0, 0.0, 0.0); }
    }
    // Underexposed. Applied after, so at equal severity an under tier wins — the
    // prototype's priority order, and the useful one: a pixel that is both is
    // black, and black is the thing you are checking for.
    //
    // Widened for the same reason, and by the same reasoning in linear terms: cyan
    // from about 1% of display white, blue from about 0.2%.
    if ((p.overlays & 2u) != 0u) {
        if (q8 <= 32.0 && checker) { rgb = vec3<f32>(0.0, 0.8, 0.8); }
        if (q8 <= 16.0)            { rgb = vec3<f32>(0.0, 0.267, 1.0); }
        if (q8 <= 0.0)             { rgb = vec3<f32>(1.0, 1.0, 1.0); }
    }

    // Sensor clipping is a fact about the negative, so it is deliberately distinct
    // from the exposure tiers above, which move with the rendering. The picture stays
    // visible between diagonal hatch lines. Green marks a partly clipped Bayer block:
    // another photosite still measured the highlight, so the block has sensor support.
    // Yellow marks four of four clipped: the sensor left no measurement inside it.
    if ((p.overlays & 32u) != 0u) {
        let n = sensor_censored(gid.xy);
        let diag = i32(gid.x + gid.y) % 8;
        let gone = n >= 3.5;
        let hatched = select(diag == 0, diag % 3 == 0, gone);
        if (n >= 0.5 && hatched) {
            rgb = select(
                vec3<f32>(0.10, 0.90, 0.30),
                vec3<f32>(1.00, 0.85, 0.10),
                gone,
            );
        }
    }

    // Composite over whatever is actually adjacent, by coverage, so the partly
    // filled row at the image boundary is anti-aliased rather than a hard or
    // smeared line.
    //
    // **Toward the mount when there is one**, not toward the canvas. Blending the
    // edge to the canvas while the mount sits against it drew a thin dark line
    // around the image — an antialiasing seam fading to a colour that was nowhere
    // near it.
    var behind = vec3<f32>(SURROUND, SURROUND, SURROUND);
    if (p.surround_w > 0.0) {
        behind = vec3<f32>(p.surround_r, p.surround_g, p.surround_b);
    }
    rgb = mix(behind, rgb, covered);

    // egui expects textures that are NOT sRGB-aware — it treats the sampled value as
    // already gamma-encoded and round-trips it to the framebuffer unchanged. So the
    // display-encoded value goes in a plain Rgba8Unorm target and lands on screen
    // exactly as computed. Writing linear here would double-encode.
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(rgb, 1.0));
}
