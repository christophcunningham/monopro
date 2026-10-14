// The brush, as every pass that reads strokes sees it: one dab's record, how much of a
// point it covers, and the cull that keeps a pass from testing every dab at every pixel.
//
// Prepended to `dodge_burn.wgsl`, whose layers are painted, and to `curve.wgsl` and
// `display.wgsl`, because a Curve mask can be painted with the same brush: the curve
// pass applies it and the display pass shows it. One definition, so a stroke covers the
// same pixels whichever of them reads it. `raw_core::dodgeburn::Dab` is the reference.
//
// The buffer at binding 6 is whichever strokes the pass is for: a layer stack's, the
// curve's painted masks', or the one mask the mask view is showing. What a pass makes
// of the dabs that survive the cull, a layer's EV or a mask's coverage, is its own.

struct Dab {
    x: f32,
    y: f32,
    radius: f32,
    feather: f32,
    opacity: f32,
    ev: f32,
    // The nib: its own height/width ratio, its rotation in RADIANS (converted on the
    // CPU), its kind and its edge in `nib`'s two bits (NIB_CARD, FALLOFF_CORE), and the
    // radius of the circle enclosing it.
    aspect: f32,
    angle: f32,
    nib: u32,
    bound: f32,
    // Which instance and which pass. Only ever compared for equality with the
    // neighbouring dab's, which is what makes the sorted scan work.
    //
    // `gesture` and not `pass`, which is the word the rest of the app uses for it:
    // `pass` is a WGSL reserved keyword and naga rejects it outright.
    inst: u32,
    gesture: u32,
};

// The strokes, sorted by (inst, gesture): the order every walk over them depends on.
@group(0) @binding(6) var<storage, read> dabs: array<Dab>;

// # Culling, and why the loop has this shape
//
// The naive form — one thread per pixel over every dab — is linear in dab count:
// measured at 2.3 ms for one dab, 9.6 ms at 200 and **44 ms at a thousand** on a
// 2560x1600 viewport. A thousand dabs is an afternoon on one print, so that form does
// not get slow, it stops working the more you use it. With the cull it is roughly flat:
// 3.0 ms at a thousand. `tests/dodgeburn_cost.rs` re-takes both columns.
//
// Each workgroup culls the stroke list to the dabs that can reach its 8x8 tile, into a
// workgroup **bitmask** — not a compacted list, because order is load-bearing. The
// three-level composite is recovered by comparing each dab's (inst, gesture) with its
// neighbour's, which only works walking the survivors in their original order; an
// atomic append would give the right set in the wrong order.
//
// So the composite is a state machine over a possibly-gappy sequence rather than three
// nested loops. A gesture whose dabs are all culled is never seen and contributes zero,
// which is what it would have contributed anyway. A pass's sign comes from the first
// *surviving* dab — every dab in a pass shares a sign, so that is the same value.
//
// # No pass that culls returns before it has
//
// `workgroupBarrier` must be reached by every invocation in the workgroup. The
// usual `if (gid.x >= p.out_w) { return; }` guard would leave the threads past the
// edge of a partial tile sitting outside the barrier, which is undefined behaviour
// and, on some drivers, a hang. Out-of-range threads do the work and skip only the
// store.

// 32 bits per word. 4096 dabs per chunk is 512 bytes of workgroup memory, and
// stroke lists longer than that are walked in several chunks — the state machine
// carries across the boundary, so a chunk is invisible to the composite.
const CULL_WORDS: u32 = 128u;
const CULL_CHUNK: u32 = 4096u;
const THREADS: u32 = 64u;

var<workgroup> keep: array<atomic<u32>, 128>;

// Below this feather the profile is a hard disc. Matches raw_core Dab::HARD; the
// bell's sigma diverges as feather goes to zero, and the disc describes that shape
// exactly anyway.
const HARD: f32 = 0.01;

// `Dab.nib`'s bits. Set, NIB_CARD is a card and not a round nib; FALLOFF_CORE is the
// core falloff, full strength to `1 - feather` of the radius and a smoothstep to the
// rim, and not the bell of a stroke saved before it. raw_core's `Falloff` says why a
// dab carries one or the other.
const NIB_CARD: u32 = 1u;
const FALLOFF_CORE: u32 = 2u;

// One dab's unsigned contribution, in stops.
//
// **Two aspects, and they are not the same thing.** `frame` is the negative's h/w and
// undoes the mixed normalisation so that a nib at ratio 1 is round IN PIXELS;
// `d.aspect` is the nib's own ratio and makes it deliberately not round. Into width
// units first, THEN rotate, then scale by the nib's ratio — rotating in normalised
// space shears the shape. Same order as raw_core::dodgeburn::Dab::distance, which is
// the reference this must agree with.
//
// Returns zero at and beyond the boundary whatever the shape, which is what the cull
// below depends on.
fn dab_coverage(d: Dab, x: f32, y: f32, frame: f32) -> f32 {
    let dx = x - d.x;
    let dy = (y - d.y) * frame;
    let c = cos(d.angle);
    let s = sin(d.angle);
    let lx = dx * c + dy * s;
    let ly = -dx * s + dy * c;
    let r = max(d.radius, 1e-6);
    let nx = lx / r;
    let ny = ly / (r * max(d.aspect, 0.01));
    // Euclidean is an ellipse; Chebyshev is a rectangle — the boundary is where
    // EITHER axis reaches one rather than where they reach it together, which is
    // what gives a card its straight edge and its corners.
    var dist = sqrt(nx * nx + ny * ny);
    if ((d.nib & NIB_CARD) != 0u) {
        dist = max(abs(nx), abs(ny));
    }
    if (dist >= 1.0) {
        return 0.0;
    }
    // In the nib's own normalised space, so the feather stays the same fraction of the
    // brush whatever shape or aspect it has.
    var profile = 1.0;
    if (d.feather >= HARD) {
        if ((d.nib & FALLOFF_CORE) != 0u) {
            let f = min(d.feather, 1.0);
            let s = clamp((dist - (1.0 - f)) / f, 0.0, 1.0);
            profile = 1.0 - s * s * (3.0 - 2.0 * s);
        } else {
            let f = clamp(d.feather, HARD, 0.999);
            let sigma = 1.0 / sqrt(-2.0 * log(1.0 - f));
            let t = clamp(1.0 - dist, 0.0, 1.0);
            profile = exp(-0.5 * (dist / sigma) * (dist / sigma)) * t * t * (3.0 - 2.0 * t);
        }
    }
    return profile * d.opacity;
}

// This workgroup's 8x8 tile as a box on the negative, source-normalized:
// (x0, y0, x1, y1).
//
// An affine maps a rectangle to a parallelogram, whose bounding box is exactly the box
// of its four corners, so this is tight rather than conservative, even under a
// straighten.
fn tile_bounds(wid: vec3<u32>) -> vec4<f32> {
    let sw = f32(p.src_w);
    let sh = f32(p.src_h);
    let ax0 = f32(i32(wid.x * 8u) + p.out_x);
    let ay0 = f32(i32(wid.y * 8u) + p.out_y);
    let c0 = source_at(ax0, ay0);
    let c1 = source_at(ax0 + 7.0, ay0);
    let c2 = source_at(ax0, ay0 + 7.0);
    let c3 = source_at(ax0 + 7.0, ay0 + 7.0);
    // One source pixel of slack in each direction. The box is exact in exact
    // arithmetic; the slack is for the last bits of the mantissa, because the
    // failure mode of a box that is a hair too small is a hard-clipped dab edge
    // along a tile boundary — an 8-pixel step in the middle of a soft falloff.
    let lo = min(min(c0, c1), min(c2, c3)) - vec2<f32>(1.0, 1.0);
    let hi = max(max(c0, c1), max(c2, c3)) + vec2<f32>(1.0, 1.0);
    return vec4<f32>(lo.x / sw, lo.y / sh, hi.x / sw, hi.y / sh);
}

// Mark, in `keep`, every dab of the chunk `base .. base + count` that can reach `tile`.
//
// **Every invocation of the workgroup must call this**, at the same point: it clears
// the mask, marks its share of the dabs, and meets the others at a barrier either side.
// After it, bit `k` of `keep` says dab `base + k` survived, and a pass walks the set
// bits in order.
fn cull_dabs(base: u32, count: u32, lid: u32, tile: vec4<f32>, aspect: f32) {
    var w = lid;
    loop {
        if (w >= CULL_WORDS) { break; }
        atomicStore(&keep[w], 0u);
        w = w + THREADS;
    }
    workgroupBarrier();

    // Closest-point test against the box, in width-normalised units — the same units
    // `dab_coverage` measures distance in, which is what makes the two agree exactly.
    var k = lid;
    loop {
        if (k >= count) { break; }
        let d = dabs[base + k];
        let ddx = d.x - clamp(d.x, tile.x, tile.z);
        let ddy = (d.y - clamp(d.y, tile.y, tile.w)) * aspect;
        // `d.bound`, not `d.radius`: the enclosing circle of the nib, which for
        // an elongated or rotated one is larger. Conservative — nothing that
        // could contribute is dropped — and less effective the longer the nib,
        // which is the honest trade.
        if (ddx * ddx + ddy * ddy < d.bound * d.bound) {
            atomicOr(&keep[k >> 5u], 1u << (k & 31u));
        }
        k = k + THREADS;
    }
    workgroupBarrier();
}
