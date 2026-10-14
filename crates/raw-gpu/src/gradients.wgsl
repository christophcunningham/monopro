// The two gradients a layer or a mask can be limited to. Prepended to `dodge_burn.wgsl`
// and `curve.wgsl`, so a Dodge & Burn layer and a Curve mask that lay the same shape on
// the same negative read the same arithmetic and cannot drift apart.
//
// Both are the GPU side of `raw_core::dodgeburn::{Linear, Radial}::weight_at`, which are
// the references they must agree with pixel for pixel. The Dodge & Burn gradient test in
// `tests/render.rs` holds Dodge & Burn to it, and the masked-curve test holds the curve.
//
// Coordinates are SOURCE-normalised: `x` and `y` are fractions of the stored image's
// width and height, and `aspect` is its height over width. **The aspect correction on dy
// is the trap.** x is normalised to the width, y to the height, and the radius to the
// width alone, which keeps a circle a circle in pixels rather than an ellipse that
// changes shape with the negative. Backwards, it looks right on a square test image
// and wrong on every real one.
//
// Taking plain scalars rather than a struct is what lets two differently-shaped
// records (a Dodge & Burn `Instance` and a curve `Segment`) share them.

// A linear gradient's strength: full at `a`, zero at `b`, unbounded perpendicular to the
// drag. See raw_core::dodgeburn::Linear.
//
// Aspect-corrected, which the prototype's is not — its iso-lines are not perpendicular
// to a diagonal drag on a non-square frame.
fn linear_gradient(a: vec2<f32>, b: vec2<f32>, feather: f32, x: f32, y: f32, aspect: f32) -> f32 {
    let dx = b.x - a.x;
    let dy = (b.y - a.y) * aspect;
    let len2 = dx * dx + dy * dy;
    // Every frame between the press and the first movement.
    if (len2 < 1e-10) {
        return 1.0;
    }
    let t = ((x - a.x) * dx + ((y - a.y) * aspect) * dy) / len2;
    let f = clamp(feather, 0.001, 1.0);
    let lo = 0.5 - f * 0.5;
    let hi = 0.5 + f * 0.5;
    let u = clamp((t - lo) / (hi - lo), 0.0, 1.0);
    return 1.0 - u * u * (3.0 - 2.0 * u);
}

// A radial gradient's strength: a spotlight, or a vignette when `invert` is set.
// `ellipse` is the ellipse's own height over width (1.0 is a circle) and `angle` its
// turn in RADIANS, converted on the CPU once per change rather than once per pixel per
// frame. See raw_core::dodgeburn::Radial.
//
// Into width units BEFORE the rotation, so the ellipse turns without shearing — and so
// an `ellipse` of 1.0 is a real circle rather than the frame's shape.
fn radial_gradient(
    centre: vec2<f32>,
    inner: f32,
    outer: f32,
    ellipse: f32,
    angle: f32,
    feather: f32,
    invert: bool,
    x: f32,
    y: f32,
    aspect: f32,
) -> f32 {
    let c = cos(angle);
    let si = sin(angle);
    let qx = x - centre.x;
    let qy = (y - centre.y) * aspect;
    let lx = qx * c + qy * si;
    let ly = -qx * si + qy * c;

    let r = max(outer, 1e-4);
    let nx = lx / r;
    let ny = ly / (r * max(ellipse, 0.01));
    let dist = sqrt(nx * nx + ny * ny);

    let core = clamp(inner / r, 0.0, 0.999);
    let u = clamp((dist - core) / max(1.0 - core, 1e-6), 0.0, 1.0);
    let f = clamp(feather, 0.0, 1.0);
    // Blended rather than switched, so the feather control is continuous.
    let ramp = (1.0 - f) * u + f * (u * u * (3.0 - 2.0 * u));
    let w = 1.0 - ramp;
    if (invert) {
        return 1.0 - w;
    }
    return w;
}
