// Pinned as a measurement rather than as four literals, so a change reaching
// the same look by another route passes and one restoring the prototype's loud
// defaults does not.
//
// The two bounds catch different failures. **Modulation is what separates this
// from the prototype**, two to one — 0.101 against 0.216. Correlation does not:
// a 5 px crystal decorrelates in the same three pixels a 3 px one does. What
// the correlation bound catches is a genuinely coarse crystal, from about 9 px
// up. The numbers below sit half again above the default and well clear of the
// prototype.
//! Crystallographic AgX grain synthesis — Aurélien Pierre (2023), ported from the
//! prototype's `grainify_rust`.
//!
//! <https://eng.aurelienpierre.com/2023/07/stochastic-photographic-grain-synthesis-from-crystallographic-structure-simulation/>
//!
//! The emulsion is a stack of N crystal layers. Each seeds AgX crystal positions
//! stochastically against the local exposure and grows them by convolving those seeds
//! with a randomly shaped binary polyhedron. What comes out is *coverage*, and the
//! picture is printed through it.
//!
//! # This is not a node, and cannot become one
//!
//! ```text
//! available = image − accumulated        (read, per layer, per pixel)
//! ```
//!
//! Each layer's seeding is clamped by what the layers before it deposited, so the
//! layers are a **serial dependency chain** — sixty sequential convolutions with a
//! data-dependent read between them. The parallelism is within a layer, which is what
//! rayon is for here.
//!
//! # `silver` is unscaled coverage, and must stay a separate buffer
//!
//! [`GrainResult`] returns the accumulator beside the composited image; chemical toning
//! is what spends it. Three plausible optimisations each destroy the number while
//! leaving something that looks fine: transforming `silver` in place, tiling the layer
//! loop so there is no full-image accumulator, and — the dangerous one — folding the
//! exposure coefficient into the accumulation, since `coef = mean_in / mean_out` needs
//! the whole image and looks like a wasteful second pass. That last one leaves *scaled*
//! coverage, and toning's conversion curve is driven by those numbers, so the failure is
//! a wrong hue rather than a crash. `silver_is_unscaled_coverage` fails if any of it
//! changes.
//!
//! # Determinism
//!
//! The seed is per-image state in the sidecar, so the generator is part of the file
//! format in everything but name — hence a dozen lines of hash here rather than a crate
//! free to change its sampler between releases.
//!
//! Per-pixel draws are **keyed on the pixel's place in the print**, `hash(seed, layer,
//! x, y)`, not taken from a running stream. So the seeding pass runs under rayon, the
//! result does not depend on how rayon split the rows, and **a crop grains identically
//! to the same region of the whole print** — which is what makes the loupe a
//! prediction. See [`apply_at`].
//!
//! This does not reproduce the prototype's pixels and could not: `rand_distr`'s normal
//! sampler is a ziggurat whose stream needs the crate. The *distributions* match, which
//! is what the algorithm is specified in terms of, and `docs/decisions.md` holds the
//! tonal-profile comparison that checked it.
//!
//! At the defaults the grain is heavy — a fifth of local density in the shadows — and
//! that is Pierre's model rather than a port that slipped a factor. What quietens it is
//! **more layers and higher density**, both of which raise the crystals a pixel
//! accumulates and lower relative noise as `1/sqrt(n)`.

use std::ops::RangeInclusive;

use rayon::prelude::*;

/// Crystal size in pixels. Odd-enforced — see [`GrainParams::set_size`].
pub const SIZE_RANGE: RangeInclusive<u32> = 1..=20;
/// Surface filling ratio: the fraction of the frame one layer's crystals cover.
pub const DENSITY_RANGE: RangeInclusive<f32> = 0.05..=0.75;
/// How many crystal layers the emulsion is built from.
pub const LAYERS_RANGE: RangeInclusive<u32> = 5..=60;
/// Log-normal sigma of the crystal size distribution.
pub const VARIABILITY_RANGE: RangeInclusive<f32> = 0.0..=2.0;
/// Tonal bias, in EV. Positive puts grain in the highlights, negative in the shadows.
pub const SENSITIVITY_RANGE: RangeInclusive<f32> = -2.0..=2.0;

/// The largest seed the panel offers, and the range a rolled one is drawn from.
///
/// **Six digits, because a seed is something a person handles.** the maintainer asked to be
/// able to type his own, which is the right instinct — a seed is the identity of this
/// negative's emulsion, and an identity is a thing you read off a print, write down
/// and give to another frame. `13472849382948293847` is not that; `481203` is.
///
/// Entropy is beside the point. There is no adversary here and nothing to guess; a
/// million distinct emulsions is more than anyone will ever compare.
///
/// **Nothing clamps to this on the way in.** [`GrainParams::seed`] is a `u64` and a
/// sidecar may legitimately carry a larger one — hand-written, or from a version that
/// rolled across the whole range — and silently rewriting it would change a print
/// that had already been approved. This is the range a person is *asked* to work in,
/// not a constraint on the format.
pub const SEED_MAX: u64 = 999_999;

/// The floor the log-normal sigma is clamped to inside the kernel.
///
/// A sigma of exactly zero is a degenerate distribution and the sampler would
/// return the mean every time, which is a legitimate thing to want — but the
/// prototype clamps here rather than special-casing, and the visual difference
/// between 0.0 and 0.01 is nothing. Kept identical so the parameter means the same
/// number in both.
const SIGMA_FLOOR: f32 = 0.01;

/// Grain, as parameters.
///
/// **In `Params` but in no tier of `Dirty`**, the same as
/// [`OutputParams`](crate::output::OutputParams) and for a related reason: nothing
/// here can change a viewport pixel, because grain is export-only. It is per-image
/// state, it belongs in the sidecar and it belongs in undo, and `Params::diff`
/// leaves it out of `render` deliberately. What it *can* change is the grain loupe,
/// which is not the viewport and asks for itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrainParams {
    /// Module bypass. Off by default: every frame judged before this module existed
    /// must still export the same way.
    pub enabled: bool,
    /// Crystal kernel size in pixels. **Always odd** — see [`set_size`](Self::set_size).
    pub size: u32,
    /// Surface filling ratio.
    pub density: f32,
    pub layers: u32,
    /// Log-normal sigma of the size distribution. This is what makes grain look like
    /// grain rather than like a stipple of one crystal repeated.
    pub variability: f32,
    /// EV bias on the tonal seeding centre.
    pub sensitivity: f32,
    /// The RNG seed. **Not a slider.** It is per-image state that has to survive a
    /// reload, so that two exports of one negative are the same picture, and the
    /// only sensible control over it is "give me a different one".
    pub seed: u64,
}

impl Default for GrainParams {
    /// **A fine-grained 100 ISO film** — small crystals, tightly packed, and a tight
    /// size distribution, because a fine-grained emulsion is one whose crystals are
    /// *uniform*. the maintainer's numbers, not the prototype's, which are loud enough that the
    /// module reads as a fault the first time it is switched on.
    ///
    /// 2.5% RMS: deliberately not the quietest the module can do, because a default you
    /// cannot see is a default that looks broken the other way. **Layers stay at 30** —
    /// sixty is indistinguishable at 1:1 and doubles every export. Measurements in
    /// `docs/decisions.md`.
    ///
    /// The seed is fixed rather than drawn from the clock, so a file with no grain block
    /// in its sidecar and a file written today grain the same way.
    fn default() -> Self {
        Self {
            enabled: false,
            size: 3,
            density: 0.56,
            layers: 30,
            variability: 0.15,
            sensitivity: 0.0,
            seed: 42,
        }
    }
}

impl GrainParams {
    /// Set the crystal size, snapping to the next odd value inside the range.
    ///
    /// **The snap is here and not in the kernel.** A kernel that silently rounded an
    /// even size would be a control whose readout disagreed with its effect — the
    /// panel would say 6 px, the picture would be 7 px, and nothing would say so.
    /// Snapping in the setter means the stored value is the value that runs.
    pub fn set_size(&mut self, px: u32) {
        self.size = odd(px);
    }

    /// The size the kernel will actually use, for a value that arrived from
    /// somewhere other than the setter — a hand-edited sidecar, say.
    pub fn effective_size(&self) -> u32 {
        odd(self.size)
    }

    /// How much margin a **crop** needs around it to grain like the print it was cut
    /// from, in pixels.
    ///
    /// A crop's convolution reflects against the crop's own edge instead of seeing
    /// the neighbours that are really there, so a band around the outside comes out
    /// wrong — and it is a band, not a smear: measured, the error is exactly zero
    /// beyond it at every layer count, because a perturbed accumulator only reaches
    /// as far as one crystal can carry it.
    ///
    /// The width is the largest crystal the log-normal can produce, which is not a
    /// statistical bound but a hard clamp: [`crystal_size`] caps the draw at three
    /// times the nominal size. So render `apron()` pixels wider on each side, show
    /// the middle, and the crop *is* the file. That is what the grain loupe does.
    pub fn apron(&self) -> u32 {
        // `crystal_size` clamps the draw to `3 * nominal` and the nominal size is
        // always odd, so the product is odd and needs no further rounding. The
        // convolution reaches `ksize / 2` either way, which is what has to be covered.
        //
        // Deliberately **not** `odd()`: that helper clamps into `SIZE_RANGE`, which
        // would turn the widest crystal's 63 px into 21 and hand back an apron three
        // times too narrow — silently, and only at the top of the size slider.
        (3 * self.effective_size()).div_ceil(2)
    }

    /// Whether this changes any exported pixel.
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Whether the user has touched it, ignoring the bypass. See
    /// `ExposureParams::is_default`.
    ///
    /// The seed is **excluded**: rolling a new one is not an edit to the module's
    /// settings in the sense the dot reports, and a file that carried a rolled seed
    /// would otherwise read as modified for ever after.
    pub fn is_default(&self) -> bool {
        let d = Self::default();
        Self {
            enabled: d.enabled,
            seed: d.seed,
            ..*self
        } == d
    }

    /// Whether the module's dot should read as modified. See
    /// [`is_modified`](crate::params::is_modified).
    pub fn is_modified(&self) -> bool {
        crate::params::is_modified(
            self.is_default(),
            self.is_active(),
            Self::default().is_active(),
        )
    }
}

/// Round into [`SIZE_RANGE`] and up to the next odd number.
///
/// Odd because the kernel is a square centred on its middle pixel; an even width has
/// no middle, and the convolution would be displaced by half a pixel.
fn odd(px: u32) -> u32 {
    let clamped = px.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end());
    if clamped.is_multiple_of(2) {
        clamped + 1
    } else {
        clamped
    }
}

/// What one run of the emulsion produced.
#[derive(Debug, Clone, PartialEq)]
pub struct GrainResult {
    /// The picture, composited through the printing model. Same length as the input.
    pub image: Vec<f32>,
    /// Per-pixel crystal accumulation, **before** the exposure compensation and the
    /// printing model — raw coverage, in the same units as the input image.
    ///
    /// Chemical toning is what reads this. Nothing does today; see
    /// the module note for why it is emitted anyway and what would silently break it.
    pub silver_density: Vec<f32>,
}

/// Run the emulsion over a display-referred image in `[0, 1]`.
///
/// `image` is post-tone-map and pre-encode: bounded, linear in light, and at the
/// size the file will be written at. Grain is a property of the print, so it runs on
/// the print's pixels — a 9 px crystal is 9 px in the file whatever the output size,
/// and that is what makes the loupe's 1:1 view a prediction of the file rather than
/// a suggestion. See `raw_app::export::write`.
///
/// Cost is dominated by the layer loop, which is serial by construction. Measure
/// with `crates/raw-core/tests/grain_cost.rs` rather than guessing.
pub fn apply(image: &[f32], w: usize, h: usize, p: &GrainParams) -> GrainResult {
    apply_at(image, w, h, (0, 0), p)
}

/// The emulsion over a **crop** of a larger print, told where the crop sits.
///
/// This is what makes the grain loupe a prediction rather than an impression. The
/// per-pixel seeding is keyed on the pixel's coordinate in the finished print, so a
/// 400x400 tile taken at `origin` deposits its crystals in exactly the places the
/// full-size export deposits them — same seeds, same shapes, same layer chain, since
/// `available = image − accumulated` is per-pixel and carries no neighbours. Key it
/// on a flat index instead and the tile would be *a* correct-looking grain rather
/// than *the* grain the file is going to have.
///
/// **One quantity does not survive the crop, and it is not fixable here**: the
/// exposure coefficient is `mean_in / mean_out` over whatever it was handed, so a
/// tile taken over an unusually dark or bright patch compensates against its own
/// mean rather than the print's. In practice it barely moves — both means scale
/// together, so the ratio is set by the layer count and the density far more than by
/// the picture — and `a_tile_grains_like_the_print_it_was_cut_from_outside_its_apron`
/// measures how far.
/// Passing the print's coefficient in would fix it exactly and would mean rendering
/// the print to find out, which is the thing the loupe exists not to do.
pub fn apply_at(
    image: &[f32],
    w: usize,
    h: usize,
    origin: (u32, u32),
    p: &GrainParams,
) -> GrainResult {
    let npix = w * h;
    assert_eq!(image.len(), npix, "grain: image is not {w} x {h}");

    let filling = p
        .density
        .clamp(*DENSITY_RANGE.start(), *DENSITY_RANGE.end());
    let layers = p.layers.clamp(*LAYERS_RANGE.start(), *LAYERS_RANGE.end()) as usize;
    let sigma = p
        .variability
        .clamp(*VARIABILITY_RANGE.start(), *VARIABILITY_RANGE.end())
        .max(SIGMA_FLOOR);
    let sens = p
        .sensitivity
        .clamp(*SENSITIVITY_RANGE.start(), *SENSITIVITY_RANGE.end());
    let grain_size = p.effective_size() as usize;

    let layers_f = layers as f32;
    let ev_scale = sens.exp2();
    // Pierre's empirical fit from the filling ratio to the seeding variable. One
    // number for the whole run — it depends on the density only.
    let sigma_fill = filling_to_rand_variable(filling);

    // The accumulator. **This is the silver density map**, and it stays coverage
    // from here to the return: nothing below writes a composited value into it.
    let mut silver = vec![0.0f32; npix];

    // A serial stream for the per-layer draws — shape, orientation and size. Cheap
    // (a handful of samples per layer) and order-dependent, so it is a stream rather
    // than a per-pixel hash.
    let mut stream = Stream::new(p.seed);

    for layer in 0..layers {
        // Sample a crystal. Retried because a small enough size with an extreme
        // enough vertex count can produce an empty kernel, and an empty kernel is a
        // layer that deposits nothing.
        let (kernel, ksize, area) = (0..10)
            .find_map(|_| {
                let verts = stream.normal(6.0, 1.5).clamp(3.0, 10.0);
                let rotation = stream.unit() * std::f32::consts::TAU;
                let size = crystal_size(&mut stream, grain_size, sigma);
                let k = create_crystal(size, verts, rotation);
                let area: f32 = k.iter().sum();
                (area > 0.0).then_some((k, size, area))
            })
            // A single lit pixel: deposits, covers nothing, and cannot divide by zero.
            .unwrap_or_else(|| (vec![1.0], 1, 1.0));

        // The per-pixel seeding probability, and the one place the erfinv
        // approximation is load-bearing.
        //
        // The prototype draws `noise ~ Normal(n_a, sd)` with `sd = sqrt(max(n_a, 1))`
        // and seeds where `noise < n_a + erfinv(2v−1)·√2·sd`. Both `n_a` and `sd`
        // appear on each side, so they cancel exactly and the test is on the standard
        // normal:
        //
        // ```text
        // z  <  erfinv(2v − 1)·√2
        // ```
        //
        // which fires with probability Φ(erfinv(2v−1)·√2) = v. So `v` *is* the
        // seeding probability, and `the_seeded_fraction_is_the_seeding_variable`
        // measures exactly that — which makes it a test of the erfinv approximation
        // as much as of the seeding. The algebra is stated rather than performed
        // silently because dropping `n_a` from the code looks like dropping a term.
        let v = if (area - 1.0).abs() < 0.5 {
            sigma_fill
        } else {
            sigma_fill / area
        };
        let z_threshold = distribution_to_variable(v);

        // Seeds: where a crystal nucleates, and how much silver is available to it.
        // Bounded by what the layers before this one have already deposited, which is
        // the serial dependency the whole module is shaped by.
        let seeds: Vec<f32> = image
            .par_iter()
            .zip(silver.par_iter())
            .enumerate()
            .map(|(i, (&px, &acc))| {
                let (x, y) = (origin.0 + (i % w) as u32, origin.1 + (i / w) as u32);
                if standard_normal(p.seed, layer, x, y) < z_threshold {
                    let shifted = (px * ev_scale).clamp(0.0, 1.0);
                    let available = (px - acc).max(0.0);
                    (shifted / layers_f).min(available).max(0.0)
                } else {
                    0.0
                }
            })
            .collect();

        let grains = convolve(&seeds, w, h, &kernel, ksize);

        // Deposit, capped twice: no layer may lay down more than its share, and none
        // may take a pixel past the exposure it is entitled to.
        silver
            .par_iter_mut()
            .zip(image.par_iter())
            .zip(grains.par_iter())
            .for_each(|((acc, &px), &g)| {
                let cap = (px / layers_f).min((px - *acc).max(0.0)).max(0.0);
                *acc += g.clamp(0.0, cap);
            });
    }

    // ── Exposure compensation ────────────────────────────────────────────────
    //
    // The layer stack deposits less silver than the image asked for, so the coverage
    // map is uniformly dark. Rescaling by the ratio of means restores the exposure
    // without touching the *structure*, which is the whole thing being synthesised.
    //
    // Summed in f64. At 40 MP an f32 accumulation of values around 0.2 loses the
    // tail entirely once the running sum passes about 2^24, which would make the
    // coefficient depend on the image size. The prototype sums in f32 and NumPy
    // sums pairwise, so neither of them is a reference to match here.
    let mean_in = mean(image);
    let mean_out = mean(&silver);
    if mean_out < 1e-8 {
        // Nothing was deposited — a black frame, or a layer count of zero silver.
        // Returning the image untouched is the only honest answer, and the map is
        // the zeros it genuinely is.
        return GrainResult {
            image: image.to_vec(),
            silver_density: silver,
        };
    }
    let coef = (mean_in / mean_out) as f32;

    // ── The printing model ───────────────────────────────────────────────────
    //
    // ```text
    // mask  = 1 − I
    // final = mask·grainy + (1 − mask)·image
    // ```
    //
    // **This is what makes it grain and not noise.** The mask is the complement of
    // the picture, so the grained signal dominates where the print is dark and the
    // clean signal dominates where it is light — grain acts most in the shadows,
    // which is what a print does. An added noise field would be uniform, and would
    // look it.
    //
    // Note what it does *not* do: write into `silver`. See the module note.
    let composited = image
        .par_iter()
        .zip(silver.par_iter())
        .map(|(&px, &acc)| {
            let grainy = (acc * coef).clamp(0.0, 1.0);
            let mask = 1.0 - px;
            (mask * grainy + (1.0 - mask) * px).clamp(0.0, 1.0)
        })
        .collect();

    GrainResult {
        image: composited,
        silver_density: silver,
    }
}

fn mean(v: &[f32]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let sum: f64 = v.par_iter().map(|&x| x as f64).sum();
    sum / v.len() as f64
}

// ── The crystal ──────────────────────────────────────────────────────────────

/// Sample one crystal's width from the log-normal size distribution.
///
/// Odd-enforced for the same reason [`odd`] is, and capped at three times the
/// nominal size: the log-normal has no upper bound, and at sigma 2.0 an unclamped
/// draw will occasionally produce a kernel wider than the image.
fn crystal_size(stream: &mut Stream, nominal: usize, sigma: f32) -> usize {
    let ln_mean = (nominal as f32).ln();
    let drawn = (ln_mean + sigma * stream.normal(0.0, 1.0)).exp();
    let mut size = (drawn.round() as i64).clamp(1, (3 * nominal) as i64) as usize;
    if size.is_multiple_of(2) {
        size += 1;
    }
    size
}

/// A binary polyhedric kernel — the crystal's silhouette.
///
/// `n_verts` is the number of faces and `rotation` its orientation, so a layer's
/// crystals are all the same shape but no two layers' are. The polygon's radial
/// extent at an angle is the standard "regular polygon in polar form"; a pixel is in
/// the crystal when it falls inside that radius.
///
/// Ported unchanged from the prototype, `eps` included: it is a half-pixel-ish
/// tolerance that keeps a width-1 kernel from coming out empty.
fn create_crystal(width: usize, n_verts: f32, rotation: f32) -> Vec<f32> {
    let eps = 1.0 / width as f32;
    let radius = ((width as f32 - 1.0) / 2.0).max(1.0);
    let n = n_verts.max(3.0);
    let mut kernel = vec![0.0f32; width * width];
    for i in 0..width {
        let x = i as f32 / radius - 1.0;
        for j in 0..width {
            let y = j as f32 / radius - 1.0;
            let r = x.hypot(y);
            let angle = y.atan2(x);
            let m = (std::f32::consts::PI / n).cos()
                / ((2.0 * (n * (angle + rotation)).cos().asin() + std::f32::consts::PI)
                    / (2.0 * n))
                    .cos();
            kernel[i * width + j] = f32::from(m >= r - eps);
        }
    }
    kernel
}

// ── The distributions ────────────────────────────────────────────────────────

/// Pierre's empirical fit from a filling ratio to the seeding variable.
///
/// The coefficients carry more digits than an `f32` holds, and they are kept that
/// way on purpose: they are a citation, and a reader checking this against the paper
/// should find the same numbers rather than their rounded shadows.
#[expect(clippy::excessive_precision, reason = "Pierre's published fit, kept digit for digit as a citation")]
fn filling_to_rand_variable(p: f32) -> f32 {
    let p = p.clamp(1e-6, 0.9999);
    1.107_247_14 * p.powf(1.048_773_89) / (p - 1.0).abs().powf(0.372_074_05)
}

/// The standard-normal quantile of a population fraction: `erfinv(2p − 1)·√2`.
///
/// Kept as its own function rather than inlined so the round trip
/// `Φ(distribution_to_variable(p)) == p` is a thing a test can state.
fn distribution_to_variable(population: f32) -> f32 {
    let p = population.clamp(1e-6, 0.9999);
    erfinv(2.0 * p - 1.0) * std::f32::consts::SQRT_2
}

/// Winitzki's rational approximation to the inverse error function.
///
/// `std` has no `erfinv` and the prototype approximates rather than depending on one,
/// so this is ported rather than reached for. Accurate to about 2e-3 absolute over
/// (−1, 1), which is the accuracy the log-normal size distribution is specified at —
/// and `erfinv_matches_known_values` pins the tail, because an approximation that is
/// wrong out there produces a texture that is plausible and wrong.
fn erfinv(x: f32) -> f32 {
    const A: f32 = 0.147;
    let ln_term = (1.0 - x * x).max(1e-15).ln();
    let c = 2.0 / (std::f32::consts::PI * A) + ln_term / 2.0;
    let inner = (c * c - ln_term / A).max(0.0).sqrt();
    (inner - c).max(0.0).sqrt().copysign(x)
}

/// The standard normal CDF — **the tests' reference, and only theirs**.
///
/// It exists to measure [`erfinv`] from the other side: `Φ(erfinv(2p−1)·√2)` must
/// come back as `p`, which is the identity the seeding pass rests on. Nothing in the
/// render path calls it, so it is compiled out of the library rather than shipped as
/// a second approximation somebody might reach for by mistake.
#[cfg(test)]
fn phi(z: f32) -> f32 {
    0.5 * (1.0 + erf(z / std::f32::consts::SQRT_2))
}

#[cfg(test)]
#[expect(clippy::excessive_precision, reason = "Pierre's published coefficients; see `filling_to_rand_variable`")]
fn erf(x: f32) -> f32 {
    // A&S 7.1.26; |error| < 1.5e-7, which is well below the erfinv approximation
    // this exists to measure, so it can serve as the reference for it.
    const P: f32 = 0.327_591_1;
    const C: [f32; 5] = [
        0.254_829_592,
        -0.284_496_736,
        1.421_413_741,
        -1.453_152_027,
        1.061_405_429,
    ];
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + P * x);
    let poly = C.iter().rev().fold(0.0, |acc, &c| (acc + c) * t);
    sign * (1.0 - poly * (-x * x).exp())
}

// ── The generator ────────────────────────────────────────────────────────────

/// SplitMix64's finaliser. Good avalanche in three rounds, and short enough that the
/// file format's dependence on it is legible rather than delegated.
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

/// A uniform in `(0, 1)` from the top 24 bits — never exactly 0, which `ln` in
/// Box-Muller would not survive, and never exactly 1.
fn unit_from(bits: u64) -> f32 {
    ((bits >> 40) as f32 + 0.5) / 16_777_216.0
}

/// One standard-normal draw keyed on the pixel rather than taken from a stream.
///
/// Box-Muller from two uniforms out of one 64-bit hash. Keying on `(seed, layer, x,
/// y)` buys two things: the seeding pass runs under rayon and still produces the same
/// picture on a machine with a different core count, and a **crop grains like the
/// print it was cut from**, because the key is the pixel's place in the print rather
/// than its place in whatever buffer it arrived in. See [`apply_at`].
fn standard_normal(seed: u64, layer: usize, x: u32, y: u32) -> f32 {
    let coord = (u64::from(y) << 32) | u64::from(x);
    let key = mix(seed ^ mix((layer as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ coord));
    let u1 = unit_from(key);
    let u2 = unit_from(mix(key));
    (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
}

/// The serial stream, for the handful of per-layer draws.
struct Stream {
    key: u64,
    counter: u64,
}

impl Stream {
    fn new(seed: u64) -> Self {
        Self {
            key: mix(seed),
            counter: 0,
        }
    }

    fn next(&mut self) -> u64 {
        self.counter += 1;
        mix(self.key ^ mix(self.counter))
    }

    fn unit(&mut self) -> f32 {
        unit_from(self.next())
    }

    fn normal(&mut self, mean: f32, sd: f32) -> f32 {
        let bits = self.next();
        let u1 = unit_from(bits);
        let u2 = unit_from(mix(bits));
        mean + sd * (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

// ── The convolution ──────────────────────────────────────────────────────────

/// `same` convolution with a symmetric boundary, over a **sparse** seed field.
///
/// The gather form — every output pixel summing over every kernel cell — is what the
/// prototype does and what `convolve_dense` still is, kept as the reference this
/// is tested against. It is also the entire cost of the module: at 40 MP with a
/// 9 px crystal it is 40M × 60 layers × ~60 taps, which is a quarter of a trillion
/// multiply-adds and takes minutes.
///
/// The seeds are **sparse** — the seeding probability is `v = sigma_fill / area`,
/// which for a default-ish crystal is well under one percent — so scattering from
/// the seeds that exist costs `nnz × area` instead of `pixels × area`, two orders of
/// magnitude less. This is not a micro-optimisation; it is the difference between an
/// export that finishes and one that does not.
///
/// The symmetric boundary is handled by walking the *padded* coordinate space rather
/// than by special-casing edges: a padded position maps back to a source pixel by
/// reflection, so a seed near an edge simply appears at more than one padded
/// position and scatters from each. That keeps one code path for the interior and
/// the border, which is where an off-by-one would otherwise live.
fn convolve(seeds: &[f32], w: usize, h: usize, kernel: &[f32], ksize: usize) -> Vec<f32> {
    let half = ksize / 2;
    // The kernel's lit cells. Binary by construction — `create_crystal` writes 0 or
    // 1 — so the weight is dropped and the scatter is an add.
    let cells: Vec<(usize, usize)> = (0..ksize)
        .flat_map(|ki| (0..ksize).map(move |kj| (ki, kj)))
        .filter(|&(ki, kj)| kernel[ki * ksize + kj] != 0.0)
        .collect();

    let (pw, ph) = (w + 2 * half, h + 2 * half);
    // Reflection tables, computed once. `reflect` is the prototype's `symm`: index
    // −1 maps to 0 and −2 to 1, so the edge pixel is not repeated.
    let rx: Vec<usize> = (0..pw)
        .map(|px| reflect(px as isize - half as isize, w))
        .collect();
    let ry: Vec<usize> = (0..ph)
        .map(|py| reflect(py as isize - half as isize, h))
        .collect();

    // The seed list, in padded coordinates and sorted by row because it is built in
    // row order. `row_start` indexes into it so a band can find its own seeds
    // without scanning the rest.
    let per_row: Vec<Vec<(usize, f32)>> = (0..ph)
        .into_par_iter()
        .map(|py| {
            let base = ry[py] * w;
            (0..pw)
                .filter_map(|px| {
                    let v = seeds[base + rx[px]];
                    (v != 0.0).then_some((px, v))
                })
                .collect()
        })
        .collect();
    let mut row_start = Vec::with_capacity(ph + 1);
    let mut running = 0usize;
    for row in &per_row {
        row_start.push(running);
        running += row.len();
    }
    row_start.push(running);
    let flat: Vec<(usize, f32)> = per_row.into_iter().flatten().collect();

    // Scatter, in bands of output rows. A band owns its rows exclusively, so the
    // writes do not collide; the seeds it reads overlap its neighbours' by the
    // kernel's height, which is why the band is sized against `ksize`.
    let band = ksize.max(64);
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(band * w)
        .enumerate()
        .for_each(|(b, rows)| {
            let r0 = b * band;
            let r1 = r0 + rows.len() / w;
            // Output row `oy` reads padded rows `oy..oy + ksize`.
            for py in r0..(r1 + ksize - 1).min(ph - 1) + 1 {
                for &(px, v) in &flat[row_start[py]..row_start[py + 1]] {
                    for &(ki, kj) in &cells {
                        // `py − ki` and `px − kj` are the output this padded position
                        // contributes to; both can fall outside, which is what the
                        // padding means.
                        let Some(oy) = py.checked_sub(ki) else {
                            continue;
                        };
                        let Some(ox) = px.checked_sub(kj) else {
                            continue;
                        };
                        if oy < r0 || oy >= r1 || ox >= w {
                            continue;
                        }
                        rows[(oy - r0) * w + ox] += v;
                    }
                }
            }
        });
    out
}

/// Symmetric reflection, the `boundary='symm'` of `scipy.signal.convolve2d`.
fn reflect(i: isize, n: usize) -> usize {
    let n = n as isize;
    let r = if i < 0 {
        (-i - 1).min(n - 1)
    } else if i >= n {
        (2 * n - i - 1).max(0)
    } else {
        i
    };
    r.clamp(0, n - 1) as usize
}

/// The straightforward gather convolution. **The reference**, not the path taken:
/// [`convolve`] must agree with it exactly, which
/// `the_sparse_convolution_is_the_dense_one` asserts.
#[cfg(test)]
fn convolve_dense(seeds: &[f32], w: usize, h: usize, kernel: &[f32], ksize: usize) -> Vec<f32> {
    let half = ksize as isize / 2;
    let mut out = vec![0.0f32; w * h];
    for oy in 0..h {
        for ox in 0..w {
            let mut acc = 0.0f32;
            for ki in 0..ksize {
                let iy = reflect(oy as isize + ki as isize - half, h);
                for kj in 0..ksize {
                    let ix = reflect(ox as isize + kj as isize - half, w);
                    acc += seeds[iy * w + ix] * kernel[ki * ksize + kj];
                }
            }
            out[oy * w + ox] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test field with tone in it.
    ///
    /// **Not flat grey, deliberately.** Grain is a *distribution* over tone: the
    /// seeding is proportional to exposure and the printing model is keyed on it, so
    /// a flat field is degenerate in exactly the axis every one of these tests is
    /// about — the brief's "the test pattern is degenerate in the axis the bug is
    /// in", written down before it happened rather than after.
    fn ramp(w: usize, h: usize) -> Vec<f32> {
        (0..w * h)
            .map(|i| (i % w) as f32 / (w - 1) as f32)
            .collect()
    }

    /// The settings the **model** tests run at, written out rather than taken from
    /// `Default`.
    ///
    /// These are the prototype's numbers, and they are here because they put the
    /// emulsion in its *unsaturated* regime: coverage per layer is about 0.29, so a
    /// pixel accumulates a countable number of crystals and every quantity a model
    /// test measures — the seeding fraction, the exposure coefficient, the printing
    /// model's tonal weighting — has room to move.
    ///
    /// The shipped default is deliberately much quieter (see `Default`), and at
    /// density 0.75 the per-layer cap binds on most pixels. That is the right look
    /// and the wrong test bench: `grain_acts_most_in_the_shadows` was written against
    /// the old default and started failing the moment the new one landed, at a
    /// shadow/highlight ratio of 17 against a threshold of 20 — not because the
    /// printing model had changed but because the signal it was measuring had shrunk.
    ///
    /// **A model test that moves when a default moves is measuring the default.** So
    /// the parameters are pinned here, and `the_default_is_a_fine_grained_film` is
    /// what guards the default instead.
    fn params() -> GrainParams {
        GrainParams {
            enabled: true,
            size: 5,
            density: 0.25,
            layers: 30,
            variability: 0.25,
            sensitivity: 0.0,
            seed: 42,
        }
    }

    #[test]
    fn erfinv_matches_known_values() {
        // The tail is what matters and what a fitted approximation is worst at: the
        // log-normal crystal size is driven through `distribution_to_variable`, so an
        // erfinv that is wrong at 0.99 produces a plausible and wrong texture rather
        // than an obvious failure. Reference values from the exact function.
        //
        // The bound is **relative**, because Winitzki's is a relative-error fit and
        // its absolute error grows with the argument — 0.999 comes back 0.0044 low,
        // which is 1.9e-3 of the value. An absolute bound tight enough to be worth
        // stating at x = 0.5 would fail out here for no reason but the shape of the
        // fit, and one loose enough to pass out here would measure nothing near zero.
        for (x, want) in [
            (0.0f32, 0.0f32),
            (0.5, 0.476_936_3),
            (-0.5, -0.476_936_3),
            (0.9, 1.163_087),
            (0.99, 1.821_386),
            (0.999, 2.326_754),
            (-0.99, -1.821_386),
        ] {
            let got = erfinv(x);
            assert!(
                (got - want).abs() <= 3e-3 * want.abs().max(0.5),
                "erfinv({x}) = {got}, want {want} — the approximation has drifted"
            );
        }
    }

    #[test]
    fn the_seeding_variable_round_trips_through_the_normal() {
        // `v` is the seeding probability, and it is only that because
        // Φ(erfinv(2v−1)·√2) == v. This is the identity the seeding pass relies on
        // after `n_a` and the standard deviation cancel; if the erfinv approximation
        // ever degrades, this is where it shows up as a wrong density.
        for v in [0.001, 0.01, 0.05, 0.2, 0.5, 0.8, 0.99] {
            let back = phi(distribution_to_variable(v));
            assert!((back - v).abs() < 3e-3, "v = {v} came back as {back}");
        }
    }

    #[test]
    fn the_seeded_fraction_is_the_seeding_variable() {
        // The measurement the identity above predicts, taken on the actual generator:
        // draw the per-pixel normals the seeding pass draws and count how many fall
        // under the threshold. Catches a broken hash, a broken Box-Muller and a
        // broken erfinv, all of which would leave the picture looking like grain.
        for v in [0.005f32, 0.05, 0.3] {
            let z = distribution_to_variable(v);
            let n = 200_000;
            let hits = (0..n)
                .filter(|&i| standard_normal(7, 3, i as u32, 11) < z)
                .count();
            let got = hits as f32 / n as f32;
            // Three sigma of a binomial at this n is under 0.004 even at v = 0.3.
            assert!(
                (got - v).abs() < 0.006,
                "seeding at v = {v} fired {got} of the time"
            );
        }
    }

    #[test]
    fn the_generator_is_flat_and_uncorrelated_across_layers() {
        // Keying on the pixel is what makes the seeding parallel, and the trap is a
        // hash whose layer term barely moves the output — that would deposit every
        // layer's crystals in the same places and produce a stipple rather than
        // grain. Two layers over the same pixels must agree about as often as chance.
        let z = distribution_to_variable(0.3);
        let n = 100_000;
        let both = (0..n)
            .filter(|&i| {
                let (x, y) = (i as u32 % 512, i as u32 / 512);
                (standard_normal(1, 0, x, y) < z) && (standard_normal(1, 1, x, y) < z)
            })
            .count();
        let rate = both as f32 / n as f32;
        assert!(
            (rate - 0.09).abs() < 0.01,
            "layers 0 and 1 co-fired at {rate}, chance is 0.09"
        );
    }

    #[test]
    fn the_sparse_convolution_is_the_dense_one() {
        // The optimisation that makes export finish, measured against the form it
        // replaced — including the symmetric boundary, which is where an off-by-one
        // would live. Seeds are placed in the corners and along the edges on purpose:
        // an interior-only test would pass with the reflection deleted entirely.
        let (w, h) = (23usize, 17usize);
        let mut seeds = vec![0.0f32; w * h];
        for (i, s) in seeds.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            if x == 0 || y == 0 || x == w - 1 || y == h - 1 || (x * 7 + y * 3) % 11 == 0 {
                *s = 0.1 + (i % 5) as f32 * 0.13;
            }
        }
        for ksize in [1usize, 3, 5, 9] {
            let kernel = create_crystal(ksize, 6.0, 0.4);
            let sparse = convolve(&seeds, w, h, &kernel, ksize);
            let dense = convolve_dense(&seeds, w, h, &kernel, ksize);
            for (i, (a, b)) in sparse.iter().zip(&dense).enumerate() {
                assert!(
                    (a - b).abs() < 1e-4,
                    "ksize {ksize} pixel {i} ({}, {}): sparse {a}, dense {b}",
                    i % w,
                    i / w
                );
            }
        }
    }

    #[test]
    fn the_boundary_is_a_mirror_and_not_a_clamp() {
        // **`the_sparse_convolution_is_the_dense_one` cannot catch this, and was
        // measured not to.** Replacing the reflection with a clamp was tried on
        // purpose: both convolutions call the same `reflect`, so the break moved them
        // together and they went on agreeing exactly. That test pins the optimisation
        // against its reference; the boundary *rule* is a second claim and needs a
        // second test, or two different breakages share one test and one of them
        // goes unguarded.
        //
        // The rule is `scipy`'s `boundary='symm'`: the edge pixel is mirrored about
        // the gap outside it and is **not repeated**, so index −1 is pixel 0 and −2
        // is pixel 1. A clamp agrees at −1 and diverges from −2 on, which is why the
        // half-width has to exceed one for any of this to be observable.
        assert_eq!(
            [-3, -2, -1, 0, 2, 4, 5, 6, 7].map(|i| reflect(i, 5)),
            [2, 1, 0, 0, 2, 4, 4, 3, 2]
        );

        // And the same rule as the convolution shows it, because a correct helper
        // wired in backwards would pass the assertion above. One seed in the corner,
        // a 5x5 kernel of ones: the top-left output sums the seed once for every
        // (ki, kj) whose reflected coordinate lands on it. Rows −2, −1, 0, 1, 2
        // reflect to 1, 0, 0, 1, 2 — the seed's row twice — so the answer is 2 x 2.
        // Under a clamp the first three would all be row 0 and it would be 3 x 3.
        let (w, h) = (8usize, 8usize);
        let mut seeds = vec![0.0f32; w * h];
        seeds[0] = 1.0;
        let out = convolve(&seeds, w, h, &[1.0f32; 25], 5);
        assert_eq!(out[0], 4.0, "the corner is not mirrored");
        assert_eq!(
            out[2 * w + 2],
            1.0,
            "the interior picked up a reflection it should not have"
        );
    }

    #[test]
    fn a_kernel_wider_than_the_image_still_convolves() {
        // The log-normal has no upper bound, so at high variability a crystal can be
        // wider than a small crop — which the loupe's 400px tile makes reachable in
        // ordinary use. The reflection tables index past the image in both directions
        // when that happens.
        let (w, h) = (5usize, 4usize);
        let seeds: Vec<f32> = (0..w * h).map(|i| if i == 7 { 1.0 } else { 0.0 }).collect();
        let ksize = 11;
        let kernel = create_crystal(ksize, 6.0, 0.0);
        let sparse = convolve(&seeds, w, h, &kernel, ksize);
        let dense = convolve_dense(&seeds, w, h, &kernel, ksize);
        assert_eq!(sparse.len(), w * h);
        for (a, b) in sparse.iter().zip(&dense) {
            assert!((a - b).abs() < 1e-4, "sparse {a}, dense {b}");
        }
    }

    #[test]
    fn grain_acts_most_in_the_shadows() {
        // The printing model, and the property that makes this grain rather than an
        // added noise field: `mask = 1 − I` weights the grained signal by darkness.
        //
        // **Measured against the counterfactual, not in absolute terms.** The obvious
        // test — "the pixels move more in the shadows than in the highlights" — is
        // simply false, and measuring it was how that got found: absolute deviation
        // *peaks in the midtones*, because the grain's amplitude scales with the
        // exposure it was seeded from. What falls with tone is the deviation as a
        // fraction of the local density, i.e. how much the grain *modulates* the
        // print, and it is only meaningful beside the picture the model was not
        // applied to. Reconstructing that picture — the exposure-compensated
        // coverage, uncomposited — is what makes this a comparison of two renderings
        // rather than a restatement of the formula.
        let (w, h) = (256usize, 128usize);
        let img = ramp(w, h);
        let out = apply(&img, w, h, &params());
        let coef = (mean(&img) / mean(&out.silver_density)) as f32;

        const BINS: usize = 8;
        let mut composited = [0.0f64; BINS];
        let mut ungrained = [0.0f64; BINS];
        for b in 0..BINS {
            let (mut c, mut u, mut tone, mut n) = (0.0f64, 0.0f64, 0.0f64, 0usize);
            for (i, &clean) in img.iter().enumerate() {
                if (i % w) * BINS / w != b {
                    continue;
                }
                let grainy = (out.silver_density[i] * coef).clamp(0.0, 1.0);
                c += (out.image[i] - clean).abs() as f64;
                u += (grainy - clean).abs() as f64;
                tone += clean as f64;
                n += 1;
            }
            let tone = (tone / n as f64).max(1e-6);
            composited[b] = c / n as f64 / tone;
            ungrained[b] = u / n as f64 / tone;
        }

        // Uncomposited, the modulation is flat: that is what an added noise field
        // looks like, and it is the thing the printing model is not.
        let (lo, hi) = ungrained
            .iter()
            .fold((f64::MAX, 0.0f64), |(l, h), &v| (l.min(v), h.max(v)));
        assert!(
            hi < lo * 2.0,
            "the uncomposited modulation is not flat: {ungrained:?}"
        );

        // Composited, it falls all the way down the scale, and by more than twenty
        // times end to end.
        for b in 1..BINS {
            assert!(
                composited[b] < composited[b - 1],
                "modulation rose from bin {} to {b}: {composited:?}",
                b - 1
            );
        }
        assert!(
            composited[0] > composited[BINS - 1] * 20.0,
            "shadows modulate {:.4}, highlights {:.4} — the mask is barely acting",
            composited[0],
            composited[BINS - 1]
        );
    }

    #[test]
    fn silver_is_unscaled_coverage() {
        // The map toning consumes, and the three optimisations that would leave a
        // plausible wrong number in its place — see the module note. Two properties
        // pin it: it is bounded by the exposure it accumulated from (so it has not
        // been through the printing model, which lifts shadows toward the clean
        // image), and it is NOT the composited image.
        let (w, h) = (96usize, 96usize);
        let img = ramp(w, h);
        let out = apply(&img, w, h, &params());

        assert_eq!(out.silver_density.len(), w * h);
        for (i, (&acc, &px)) in out.silver_density.iter().zip(&img).enumerate() {
            assert!(
                acc >= 0.0 && acc <= px + 1e-6,
                "pixel {i}: coverage {acc} is not inside [0, {px}] — this is not raw coverage"
            );
        }
        // And it holds less silver than the compensated picture, which is exactly
        // what `coef` exists to correct and what fusing the two would destroy.
        let map_mean = mean(&out.silver_density);
        let img_mean = mean(&img);
        assert!(
            map_mean < img_mean * 0.9,
            "the map's mean ({map_mean:.4}) is already the image's ({img_mean:.4}) — \
             the exposure coefficient has been folded into the accumulator"
        );
    }

    #[test]
    fn the_same_seed_is_the_same_picture_and_a_different_one_is_not() {
        // The sidecar's whole reason for carrying a seed: two exports of one negative
        // must be the same file. And the other half, because a seed that did nothing
        // would pass the first assertion perfectly.
        let (w, h) = (64usize, 64usize);
        let img = ramp(w, h);
        let a = apply(&img, w, h, &params());
        let b = apply(&img, w, h, &params());
        assert_eq!(
            a.image, b.image,
            "the same seed produced a different picture"
        );
        assert_eq!(a.silver_density, b.silver_density);

        let c = apply(
            &img,
            w,
            h,
            &GrainParams {
                seed: 43,
                ..params()
            },
        );
        assert_ne!(a.image, c.image, "the seed does nothing");
    }

    #[test]
    fn the_size_control_reads_out_what_it_runs() {
        // Snap in the setter, not in the kernel — a panel that says 6 px while the
        // kernel uses 7 is a control that lies about itself.
        let mut p = GrainParams::default();
        p.set_size(6);
        assert_eq!(p.size, 7);
        p.set_size(1);
        assert_eq!(p.size, 1);
        p.set_size(20);
        assert_eq!(
            p.size,
            21.min(*SIZE_RANGE.end() + 1),
            "the top of the range must still be odd"
        );
        p.set_size(999);
        assert_eq!(
            p.effective_size(),
            p.size,
            "the stored size is the size that runs"
        );
    }

    #[test]
    fn sensitivity_moves_the_grain_up_and_down_the_scale() {
        // The EV bias, measured where it should act rather than asserted. Positive
        // sensitivity scales the seeding exposure up, so more silver is available to
        // the shadows and midtones and the deposited total rises.
        let (w, h) = (96usize, 96usize);
        let img = ramp(w, h);
        let up = apply(
            &img,
            w,
            h,
            &GrainParams {
                sensitivity: 2.0,
                ..params()
            },
        );
        let flat = apply(&img, w, h, &params());
        let down = apply(
            &img,
            w,
            h,
            &GrainParams {
                sensitivity: -2.0,
                ..params()
            },
        );

        let m = |r: &GrainResult| mean(&r.silver_density);
        assert!(
            m(&up) > m(&flat) && m(&flat) > m(&down),
            "sensitivity did not order the deposits: {:.5} / {:.5} / {:.5}",
            m(&up),
            m(&flat),
            m(&down)
        );
    }

    #[test]
    fn a_tile_grains_like_the_print_it_was_cut_from_outside_its_apron() {
        // **The loupe's whole claim, as a measurement**, and it was written as the
        // stronger claim first — "a tile matches the print" — and measured failing at
        // 0.13. Chasing that is what found the apron, so the number here is the one
        // observed rather than the one hoped for.
        //
        // Keying the seeding on the pixel's place in the print means the same crystals
        // nucleate in the same places whether the buffer is the tile or the print. What
        // does not survive being cut out is the *convolution's* neighbourhood: a tile
        // reflects against its own edge where the print had real pixels there. That
        // error is a **band and not a smear** — exactly zero beyond one crystal radius,
        // at every layer count, because a perturbed accumulator can only be carried as
        // far as one crystal reaches.
        //
        // So the loupe renders `apron()` wider and shows the middle, and the middle is
        // the file. Both halves are asserted below, because a test that only checked
        // the interior would pass just as well with the apron set to the whole tile.
        let p = params();
        let apron = p.apron() as usize;
        let (pw, ph) = (320usize, 320usize);
        let print = ramp(pw, ph);
        let full = apply(&print, pw, ph, &p);

        let (ox, oy, tw, th) = (96usize, 96usize, 128usize, 128usize);
        let tile: Vec<f32> = (0..th)
            .flat_map(|y| print[(oy + y) * pw + ox..(oy + y) * pw + ox + tw].to_vec())
            .collect();
        let cut = apply_at(&tile, tw, th, (ox as u32, oy as u32), &p);

        let mut inside = 0.0f32; // beyond the apron: must be exact
        let mut border = 0.0f32; // within it: must not be, or the apron is a fiction
        for y in 0..th {
            for x in 0..tw {
                let e = (full.silver_density[(oy + y) * pw + ox + x]
                    - cut.silver_density[y * tw + x])
                    .abs();
                let edge = x.min(y).min(tw - 1 - x).min(th - 1 - y);
                if edge >= apron {
                    inside = inside.max(e);
                } else {
                    border = border.max(e);
                }
            }
        }
        assert_eq!(
            inside, 0.0,
            "the tile deposited different silver {apron} px in: {inside}"
        );
        assert!(
            border > 1e-3,
            "nothing went wrong at the edge — the apron measures nothing"
        );

        // And the finished picture, not just the coverage. The residue here is the
        // exposure coefficient, which is the one global quantity a crop cannot
        // reproduce; if this ever drifts far, `apply_at`'s note is where to start.
        let mut peak = 0.0f32;
        for y in apron..th - apron {
            for x in apron..tw - apron {
                let a = full.image[(oy + y) * pw + ox + x];
                peak = peak.max((a - cut.image[y * tw + x]).abs());
            }
        }
        assert!(
            peak < 0.02,
            "the tile printed differently from the print: peak {peak}"
        );
    }

    #[test]
    fn the_apron_covers_the_widest_crystal_the_distribution_can_draw() {
        // The apron is only a bound if it is the *hard* one. `crystal_size` clamps at
        // three times the nominal size, so this is arithmetic rather than statistics —
        // and the trap is reaching for `odd()` to round it, which clamps into
        // `SIZE_RANGE` and would quietly return 21 for a crystal that can be 63 wide.
        for size in [1u32, 5, 9, 20, 999] {
            let mut p = GrainParams::default();
            p.set_size(size);
            let mut s = Stream::new(3);
            let widest = (0..5_000)
                .map(|_| crystal_size(&mut s, p.effective_size() as usize, 2.0))
                .max()
                .expect("5000 draws");
            assert!(
                widest / 2 <= p.apron() as usize,
                "size {size}: drew a {widest} px crystal, apron is only {}",
                p.apron()
            );
        }
    }

    #[test]
    fn a_black_frame_comes_back_untouched() {
        // `mean_out` is a divisor. A frame with no exposure deposits no silver, and
        // the only honest answer is the frame it was given — not a division by
        // something near zero.
        let (w, h) = (32usize, 32usize);
        let img = vec![0.0f32; w * h];
        let out = apply(&img, w, h, &params());
        assert_eq!(out.image, img);
        assert!(out.silver_density.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn the_output_stays_in_range() {
        // Grain runs between the tone map and the L* encode, so its input is bounded
        // and its output has to be: the encoder's `[0, 1]` assumption is what the
        // 16-bit sample conversion rests on.
        let (w, h) = (64usize, 64usize);
        let img = ramp(w, h);
        let out = apply(
            &img,
            w,
            h,
            &GrainParams {
                density: 0.75,
                layers: 60,
                ..params()
            },
        );
        assert!(
            out.image
                .iter()
                .all(|v| (0.0..=1.0).contains(v) && v.is_finite())
        );
    }

    #[test]
    fn variability_widens_the_size_distribution() {
        // The log-normal sigma is what makes grain look like grain rather than one
        // crystal stamped repeatedly. Measured on the sampler, where the property is,
        // rather than inferred from the picture.
        let spread = |sigma: f32| {
            let mut s = Stream::new(11);
            let sizes: Vec<f32> = (0..2000)
                .map(|_| crystal_size(&mut s, 9, sigma) as f32)
                .collect();
            let mean = sizes.iter().sum::<f32>() / sizes.len() as f32;
            sizes.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / sizes.len() as f32
        };
        assert!(
            spread(0.01) < 1.0,
            "a near-zero sigma must give near-uniform crystals"
        );
        assert!(
            spread(0.6) > spread(0.01) * 10.0,
            "sigma did not widen the distribution"
        );
    }

    #[test]
    fn the_default_is_a_fine_grained_film() {
        // **the maintainer's decision, pinned as a measurement rather than as four literals.**
        // Asserting `size == 3` would guard the numbers; this guards the *look*, so a
        // later change that reaches the same place by another route still passes and
        // one that quietly restores the prototype's loud defaults does not.
        //
        // The two bounds catch **different** failures, and the measurements say which:
        //
        // ```text
        //                           modulation   decorrelates
        //   prototype  5/0.25/30/0.25    0.216       3 px
        //   size 3 alone 3/0.25/30/0.15  0.206       3 px
        //   DEFAULT    3/0.56/30/0.15    0.101       3 px
        //   coarser    9/0.56/30/0.15    0.114       6 px
        // ```
        //
        // So **modulation is what separates this from the prototype** — two to one —
        // and the correlation length does not: a 5 px crystal decorrelates in the same
        // three pixels a 3 px one does, at this measurement. What the correlation
        // bound catches is a genuinely coarse crystal, from about 9 px up.
        //
        // That is worth stating because the first version of this test had it
        // backwards, and asserted a margin on correlation it did not have. The bounds
        // below are set from the table: 0.15 sits half again above the default and
        // well clear of the prototype's 0.216.
        let (w, h) = (256usize, 128usize);
        let img = ramp(w, h);
        let on = GrainParams {
            enabled: true,
            ..Default::default()
        };
        let out = apply(&img, w, h, &on);

        let (mut dev, mut tone) = (0.0f64, 0.0f64);
        for (i, &clean) in img.iter().enumerate() {
            if clean < 0.25 {
                dev += (out.image[i] - clean).abs() as f64;
                tone += clean as f64;
            }
        }
        let modulation = dev / tone.max(1e-9);
        assert!(
            modulation < 0.15,
            "the default grain modulates the shadows by {modulation:.3} — that is the \
             loud end of the range, not a 100 ISO film"
        );
        // And not so quiet it may as well be off, which is the other way to fail this.
        assert!(
            modulation > 0.01,
            "the default grain is invisible: {modulation:.4}"
        );

        // Correlation length along a mid-tone row: how far apart two pixels have to be
        // before the grain stops agreeing with itself. That is the crystal, measured.
        let row = h / 2;
        let dev: Vec<f64> = (0..w)
            .map(|x| (out.image[row * w + x] - img[row * w + x]) as f64)
            .collect();
        let mean = dev.iter().sum::<f64>() / w as f64;
        let centred: Vec<f64> = dev.iter().map(|v| v - mean).collect();
        let v0 = centred.iter().map(|v| v * v).sum::<f64>() / w as f64;
        let decorrelates = (1..8)
            .find(|&lag| {
                let c = centred[..w - lag]
                    .iter()
                    .zip(&centred[lag..])
                    .map(|(a, b)| a * b)
                    .sum::<f64>()
                    / (w - lag) as f64;
                c / v0.max(1e-12) < 0.2
            })
            .unwrap_or(99);
        assert!(
            decorrelates <= 3,
            "the default crystal decorrelates over {decorrelates} px — too coarse. This \
             bound catches a coarse crystal, not a loud one; see the table above."
        );
    }

    #[test]
    fn the_module_is_off_by_default_and_says_when_it_is_touched() {
        let d = GrainParams::default();
        assert!(
            !d.is_active() && !d.is_modified(),
            "grain must default to off and untouched"
        );
        let on = GrainParams { enabled: true, ..d };
        assert!(
            on.is_modified(),
            "switched on at defaults must light the dot"
        );
        assert!(on.is_default(), "and it is still at its default values");
        // The seed is per-image state, not a setting; rolling one is not an edit.
        assert!(GrainParams { seed: 9_999, ..d }.is_default());
    }
}
