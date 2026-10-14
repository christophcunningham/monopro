//! The zone mask's spatial machinery: the proxy the mask is computed on, the
//! edge-aware filter that makes a zone a *region*, and the trapezoid's evaluation.
//!
//! [`dodgeburn`](crate::dodgeburn) holds the model — what a mask *is*. This holds
//! what it takes to turn one into pixels.
//!
//! # Why this is on the CPU, and small
//!
//! Everything else in the render path is a GPU node, and a guided filter could be
//! one: it is five local means and some arithmetic. It is not one, for a reason
//! that is a property of the mask rather than an expedience.
//!
//! **A zone mask has no detail to lose.** Its whole purpose is to select a
//! spatially coherent region, and its detail ceiling is the guidance window —
//! `region`, four percent of the frame width by default. Computed at
//! [`PROXY_WIDTH`] that window is nineteen pixels across; computed at 6000px it is
//! two hundred and forty, describing the same shape with a hundred times the
//! arithmetic and no more information in it. So the proxy is not a shortcut taken
//! for speed, it is the resolution the signal actually has, and the prototype
//! reached the same conclusion from the other direction — it raised its stash from
//! 240px to 480 precisely because Region did not have enough guidance detail at
//! the lower figure, and stopped there.
//!
//! What that buys: the whole mask chain is a few hundred microseconds on 150k
//! pixels, the graph gains one pointwise node instead of a dozen wide-apron blur
//! passes, and a brush stroke — which cannot change a mask, because a mask is
//! feed-forward — never touches any of it.
//!
//! # The range is cut at every pixel, not here
//!
//! What has the proxy's resolution is **the tone a region reads, and not the mask**.
//! The trapezoid is a cut, and a steep one turns a change of tone into a step from
//! nothing to everything, which has detail at every scale. Cut here and then blended
//! up, each proxy pixel became a block of the picture, sixteen pixels across on a
//! 40 MP file, and an edge-aware range, which keeps a hard edge as a jump inside one
//! proxy pixel, drew its boundary in stairs on screen and in the print.
//!
//! So a range is [prepared](Basis::prepare) as the tones it reads, the shader blends
//! them to the pixel it is rendering and cuts there, and the boundary runs smoothly
//! between proxy pixels. At a proxy pixel's center the two orders agree exactly, so
//! what a range selects is unchanged. Where a boundary falls is still the proxy's to
//! say, to within one of its pixels; `docs/known-limitations.md` records that.
//!
//! # The proxy is in SOURCE space
//!
//! Not the composed frame. The mask is computed on the stored luminance image with
//! no orientation, straighten or crop applied, which makes it invariant to every
//! one of them — the same argument `raw_gpu::Viewport::plan` records for Contrast
//! Mask's spacer, and for the same reason: *a mask whose look changed when you
//! cropped would be a surprise nobody asked for*. It is also the space
//! [`Dab`](crate::dodgeburn::Dab) coordinates live in, so the shader needs one
//! mapping rather than two.

use crate::dodgeburn::{MID_GREY, ZoneMask};
use crate::params::{ContrastMaskParams, ExposureParams};
use crate::scene::LumaImage;

/// Width of the proxy the masks are computed on.
///
/// The prototype's figure, and its history is the argument for it: 240px was not
/// enough guidance detail for the Region control to be usable, 480px was, and
/// nothing above it changed the result. See the module note.
pub const PROXY_WIDTH: usize = 480;

/// The pre-D&B luminance, as EV against middle grey, at proxy resolution.
///
/// **Feed-forward.** This is the signal *entering* the D&B stage — exposure and
/// Contrast Mask applied, no strokes — so a burn can never move the mask that is
/// placing it. Rebuilt when luminance, exposure or Contrast Mask changes; a stroke
/// does not change any of those, which is what keeps a brush drag off this path
/// entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Basis {
    pub w: usize,
    pub h: usize,
    /// `log2(luminance / 0.18)`, one per proxy pixel.
    pub ev: Vec<f32>,
}

/// The downsampled luminance the masks are computed from, before any module has
/// been applied to it.
///
/// **The caching boundary, made explicit.** Building this is the only part of the
/// mask chain that touches the full-resolution image, and it depends on nothing
/// but the luminance itself — so it survives every exposure nudge, every Contrast
/// Mask slider, every zone bound and every brush stroke, and is rebuilt only when
/// luminance is re-derived. Splitting it out of [`Basis`] is what stops a
/// three-hundred-megapixel downsample from riding on a slider drag.
#[derive(Debug, Clone, PartialEq)]
pub struct Proxy {
    pub w: usize,
    pub h: usize,
    /// Scene-linear luminance, area-averaged down from the stored image.
    lum: Vec<f32>,
    /// The dimensions this was reduced from. Contrast Mask's spacer is a
    /// percentage of *that* diagonal, so the number has to travel with the data.
    src: (usize, usize),
}

impl Proxy {
    pub fn of(luma: &LumaImage) -> Self {
        let (sw, sh) = (luma.output_dims.w, luma.output_dims.h);
        let (w, h) = proxy_dims(sw, sh);
        Self {
            w,
            h,
            lum: downsample(&luma.data, sw, sh, w, h),
            src: (sw, sh),
        }
    }

    /// Apply the two modules upstream of D&B and take the log. Cheap: two passes
    /// over 150k floats, plus one small Gaussian when the mask is on.
    ///
    /// The Contrast Mask arithmetic here is the shader's, at proxy scale: log,
    /// blur, subtract a fraction of the blur, exponentiate. Its **registration
    /// offset is deliberately not applied** — at proxy scale the whole ±40px range
    /// is under two pixels, and a mask that selects regions cannot resolve a
    /// displacement smaller than its own guidance window. Stated rather than
    /// silently dropped, because the omission is a judgement and not an oversight.
    pub fn basis(&self, exposure: &ExposureParams, cm: &ContrastMaskParams) -> Basis {
        let (w, h) = (self.w, self.h);
        let mut v = self.lum.clone();

        // Exposure. Black first, so raising exposure does not amplify the offset —
        // the same order `ExposureParams` documents and `exposure.wgsl` runs.
        let gain = exposure.ev.exp2();
        for p in &mut v {
            *p = (*p - exposure.black) * gain;
        }

        if cm.is_active() {
            // Sigma against the SOURCE dims, then scaled to the proxy — exactly
            // how `node_params` converts it for the shader, so the two cannot mean
            // different physical spacers.
            let sigma = cm.spacer_px((self.src.0 as u32, self.src.1 as u32))
                * (w as f32 / self.src.0 as f32);
            // Match log2.wgsl and mask_input.wgsl. The floor participates in
            // the blur, so a different floor shifts nearby shadow selections.
            const LOG_FLOOR: f32 = 6.103_515_6e-5;
            let log: Vec<f32> = v.iter().map(|p| p.max(LOG_FLOOR).log2()).collect();
            let blurred = gaussian(&log, w, h, sigma);
            for (p, (l, b)) in v.iter_mut().zip(log.iter().zip(&blurred)) {
                // Match contrast_mask.wgsl: compress around 18% grey, not 1.0.
                *p = (l - (b - MID_GREY.log2()) * cm.contrast).exp2();
            }
        }

        for p in &mut v {
            *p = (p.max(1e-6) / MID_GREY).log2();
        }
        Basis { w, h, ev: v }
    }
}

impl Basis {
    /// Proxy and basis in one call. The convenience form; the render path holds a
    /// [`Proxy`] across frames and calls [`Proxy::basis`] instead.
    pub fn build(luma: &LumaImage, exposure: &ExposureParams, cm: &ContrastMaskParams) -> Self {
        Proxy::of(luma).basis(exposure, cm)
    }

    /// The EV image a mask of these settings reads: regional when it is edge-aware,
    /// raw per-pixel when it is not.
    ///
    /// This one call is the entire difference between "Zone III" meaning *the
    /// shadowed wall* and meaning *every dark pixel in the frame*.
    fn read(&self, m: &ZoneMask) -> std::borrow::Cow<'_, [f32]> {
        if !m.edge_aware {
            return std::borrow::Cow::Borrowed(&self.ev);
        }
        // Radius as a fraction of width, so one setting means one thing whatever
        // the negative's resolution — and, because the proxy is a fixed width, one
        // thing across every file.
        let r = ((m.region * self.w as f32).round() as usize).max(1);
        std::borrow::Cow::Owned(guided(&self.ev, self.w, self.h, r, m.eps()))
    }

    /// The mask at each proxy pixel's center, in `[0, 1]`: what [`PreparedMask::at`]
    /// reads there, whole, for anything that wants the picture of a mask rather than its
    /// value at points.
    ///
    /// `None` when the mask is the identity — there is no such thing as a mask of
    /// all ones worth allocating, and the caller has to distinguish the two cases
    /// anyway to skip binding a texture.
    ///
    /// **Not the mask blended up.** Between centers the range is cut on the blended
    /// tone, which is what keeps its edge off the proxy's grid; see the module note.
    pub fn evaluate(&self, m: &ZoneMask) -> Option<Vec<f32>> {
        match self.prepare(m).form {
            Form::Open => None,
            Form::Finished(mask) => Some(mask),
            Form::Cut { ev, island, tone } => Some(cut_centers(&ev, island.as_deref(), &tone)),
        }
    }

    /// The image a range with `m`'s settings reads, held so the eyedropper can ask it
    /// about points of the negative.
    ///
    /// **The same image the mask is cut from**, regional when the range is edge-aware,
    /// which is the point: a tone read from anything else (the exposed pixel, say) is
    /// not the tone the trapezoid will be compared against, and a click on a cloud would
    /// set a range the cloud then falls outside of.
    ///
    /// Edge-aware, this runs the guided filter once over the proxy. That is the cost of
    /// one mask, paid on a click and not on a frame.
    pub fn reading(&self, m: &ZoneMask) -> Reading<'_> {
        Reading {
            w: self.w,
            h: self.h,
            ev: self.read(m),
        }
    }

    /// The eyedropper, whole: read the tones under `points` and set `tone`'s range from
    /// them, or widen it to take them in when `extend` is set. `false`, and nothing
    /// changed, when no point was on the negative.
    ///
    /// One point is a click and many are a box; both are the span of what was sampled,
    /// which for one point is that point's tone twice. The picture is read **with
    /// `tone`'s own settings**, regionally when it is edge-aware, so the range is set
    /// against the image it will be compared with.
    pub fn sample_into(
        &self,
        tone: &mut ZoneMask,
        points: impl IntoIterator<Item = (f32, f32)>,
        extend: bool,
    ) -> bool {
        let Some((lo, hi)) = self.reading(tone).span(points) else {
            return false;
        };
        if extend {
            tone.extend(lo, hi);
        } else {
            tone.sample(lo, hi);
        }
        true
    }

    /// The prepared mask's value at a point of the negative: what the shader's lookup
    /// returns there, diffusion included. One for a range that masks nothing.
    ///
    /// `x` and `y` are source-normalized. This evaluates the whole mask to read one
    /// point of it, so it is for a click, such as the curve's point sampler asking how
    /// much of a masked pass reached the pixel, and not for a loop over pixels: that is
    /// what [`prepare`](Self::prepare) is for.
    pub fn weight_at(&self, m: &ZoneMask, x: f32, y: f32) -> f32 {
        self.prepare(m).at(x, y)
    }

    /// The mask of a range, prepared once and read at any point of the negative: what the
    /// zone worker uploads for the shader, and kept on the CPU for a caller that weighs
    /// samples of the picture by it, such as the curve editor's histogram.
    ///
    /// Everything that needs the whole picture is done here: the regional read, the
    /// island's growing and the diffusion. What is left for a point is to blend and cut,
    /// which [`PreparedMask::at`] does as the shader does.
    pub fn prepare(&self, m: &ZoneMask) -> PreparedMask {
        let (w, h) = (self.w, self.h);
        if m.is_identity() {
            return PreparedMask {
                w,
                h,
                form: Form::Open,
            };
        }
        let ev = self.read(m).into_owned();
        // The island is grown from what the range selects at the proxy's pixels, and
        // leaves a weight for each that the cut is multiplied by.
        let island = m.island.is_live().then(|| {
            let selected: Vec<f32> = ev.iter().map(|&e| m.select(e)).collect();
            island(&selected, w, h, m.island.seeds())
        });
        // The diffusion spacer. Masks were printed unsharp on purpose; without
        // this the mask carries whatever granularity the trapezoid's edges cut
        // into the proxy. A blur needs the whole image, so a diffused mask is finished
        // here and blended up as it is: from half a proxy pixel the blur leaves no step
        // for the blending to show.
        let sigma = m.blur * w as f32;
        let form = if sigma >= 0.5 {
            Form::Finished(gaussian(
                &cut_centers(&ev, island.as_deref(), m),
                w,
                h,
                sigma,
            ))
        } else {
            Form::Cut {
                ev,
                island,
                tone: *m,
            }
        };
        PreparedMask { w, h, form }
    }

    /// The EV distribution, normalised so the tallest bin is 1.0, for the ghost
    /// behind the zone ruler.
    ///
    /// Binned over the ruler's own window rather than the data's range: the
    /// histogram is drawn *behind the ruler* and has to line up with it, so a bin
    /// is a position on the ruler and values outside it are simply not shown.
    ///
    /// **Smoothed before it is normalised**, which is a display decision made here
    /// rather than in the widget because the smoothing has to happen before the peak
    /// is taken — smoothing afterwards lowers the tallest bin and the curve stops
    /// touching the top of its box.
    ///
    /// the maintainer's report was that the curve read "very jagged on top". The cause is not
    /// noise: a 480px proxy puts about fifteen hundred samples in each of a hundred
    /// bins, so the sampling error is under three percent. It is that a photograph's
    /// tonal distribution genuinely *is* spiky at this bin width, and that a spike
    /// one bin wide is a hard-edged 4px tooth once it is drawn. A three-bin
    /// triangular kernel takes the teeth off without moving anything a person would
    /// want to read off it — the mode of the distribution shifts by less than a
    /// twentieth of a stop.
    pub fn histogram(&self, bins: usize) -> Vec<f32> {
        let bins = bins.max(1);
        let mut counts = vec![0f32; bins];
        let span = ZoneMask::MAX_EV - ZoneMask::MIN_EV;
        for &e in &self.ev {
            let t = (e - ZoneMask::MIN_EV) / span;
            if (0.0..1.0).contains(&t) {
                counts[((t * bins as f32) as usize).min(bins - 1)] += 1.0;
            }
        }
        // Clamped at the ends rather than wrapped or zero-padded: the ruler's window
        // is a view onto a wider distribution, and treating what is outside it as
        // empty would pull the first and last bins down for a reason that is about
        // the window rather than about the picture.
        let at = |i: isize| counts[i.clamp(0, bins as isize - 1) as usize];
        let smooth: Vec<f32> = (0..bins)
            .map(|i| (at(i as isize - 1) + 2.0 * counts[i] + at(i as isize + 1)) / 4.0)
            .collect();
        let peak = smooth.iter().copied().fold(0.0f32, f32::max).max(1.0);
        smooth.iter().map(|c| c / peak).collect()
    }
}

/// One tonal range's mask, prepared to be read at any point of the negative: see
/// [`Basis::prepare`].
///
/// **The shader's `mask_at`, on the CPU** (`raw-gpu/src/zone_mask.wgsl`).
/// [`upload`](Self::upload) is what the shader is given and [`at`](Self::at) is what it
/// does with it, so the two change together, or the histogram and the point sampler
/// stop reading what the picture shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedMask {
    w: usize,
    h: usize,
    form: Form,
}

#[derive(Debug, Clone, PartialEq)]
enum Form {
    /// A range that masks nothing, which weighs one everywhere.
    Open,
    /// Cut at each point: the tone the range reads there, blended from the proxy's
    /// pixels, through the trapezoid, times the island's weight, then `Inv`.
    Cut {
        ev: Vec<f32>,
        island: Option<Vec<f32>>,
        tone: ZoneMask,
    },
    /// Diffused, and so finished at the proxy's pixels and blended as it is.
    Finished(Vec<f32>),
}

/// What the shader is given for one mask: see [`PreparedMask::upload`].
#[derive(Debug, Clone, Copy)]
pub struct Upload<'a> {
    /// One value per proxy pixel, blended to the point: the tone a range reads, which is
    /// then cut, or the finished mask when there is no cut.
    pub values: &'a [f32],
    /// The island's weight per proxy pixel, blended to the point and multiplied into
    /// the cut.
    pub island: Option<&'a [f32]>,
    /// The range `values` is cut with, or `None` when they are a finished mask.
    pub cut: Option<&'a ZoneMask>,
}

impl PreparedMask {
    /// The mask at a source-normalized point, worked out as the shader works it out.
    pub fn at(&self, x: f32, y: f32) -> f32 {
        let blend = |a: &[f32]| bilinear(a, self.w, self.h, x, y);
        match &self.form {
            Form::Open => 1.0,
            Form::Finished(mask) => blend(mask),
            Form::Cut { ev, island, tone } => {
                cut(tone, blend(ev), island.as_deref().map_or(1.0, blend))
            }
        }
    }

    /// What the shader is given for this mask, or `None` for a range that masks nothing,
    /// which is given no layer.
    pub fn upload(&self) -> Option<Upload<'_>> {
        match &self.form {
            Form::Open => None,
            Form::Finished(mask) => Some(Upload {
                values: mask,
                island: None,
                cut: None,
            }),
            Form::Cut { ev, island, tone } => Some(Upload {
                values: ev,
                island: island.as_deref(),
                cut: Some(tone),
            }),
        }
    }
}

/// A range's weight where it reads `ev` and the island weighs `island`.
///
/// The island before `Inv`: it limits what the range selects, and `Inv` then turns
/// over the selection it leaves, so inverted it is everything but the island.
fn cut(tone: &ZoneMask, ev: f32, island: f32) -> f32 {
    let m = tone.select(ev) * island;
    if tone.invert { 1.0 - m } else { m }
}

/// A cut range at each proxy pixel's center: [`PreparedMask::at`] there, with nothing
/// to blend.
fn cut_centers(ev: &[f32], island: Option<&[f32]>, tone: &ZoneMask) -> Vec<f32> {
    ev.iter()
        .enumerate()
        .map(|(i, &e)| cut(tone, e, island.map_or(1.0, |weight| weight[i])))
        .collect()
}

/// The EV image one tonal range reads, for sampling: see [`Basis::reading`].
#[derive(Debug, Clone)]
pub struct Reading<'a> {
    w: usize,
    h: usize,
    ev: std::borrow::Cow<'a, [f32]>,
}

impl Reading<'_> {
    /// Where in a sampled area's tones its range begins and ends: the 10th and the 90th
    /// percentile, so a few stray pixels (a specular glint in a wall, a bird in a sky)
    /// do not stretch the range to reach them.
    pub const SPAN: (f32, f32) = (0.10, 0.90);

    /// The tone at a point of the negative, in EV against middle gray.
    ///
    /// `x` and `y` are source-normalized. Blended from the four proxy pixels around the
    /// point, as the shader blends the mask it looks up, so a click between two of them
    /// reads what the mask is compared against there.
    pub fn at(&self, x: f32, y: f32) -> f32 {
        bilinear(&self.ev, self.w, self.h, x, y)
    }

    /// The span of tones under `points`, as `(low, high)` in EV: the percentiles
    /// [`SPAN`](Self::SPAN) names. Points off the negative are skipped, and `None` means
    /// none were on it.
    ///
    /// **Points and not a rectangle**, because the box is dragged on the picture as it
    /// is shown and the negative under it may be turned, straightened or corrected for
    /// perspective: the caller lays a lattice across the box on screen and carries each
    /// point onto the negative, which is right whatever lies between the two.
    pub fn span(&self, points: impl IntoIterator<Item = (f32, f32)>) -> Option<(f32, f32)> {
        let mut tones: Vec<f32> = points
            .into_iter()
            .filter(|(x, y)| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
            .map(|(x, y)| self.at(x, y))
            .collect();
        if tones.is_empty() {
            return None;
        }
        tones.sort_by(f32::total_cmp);
        // Between ranks rather than the nearest one, so a small sample's ends do not
        // jump by a whole pixel's tone as the box grows by one.
        let at = |q: f32| {
            let f = q * (tones.len() - 1) as f32;
            let i = f.floor() as usize;
            let next = tones[(i + 1).min(tones.len() - 1)];
            tones[i] + (next - tones[i]) * (f - i as f32)
        };
        Some((at(Self::SPAN.0), at(Self::SPAN.1)))
    }
}

/// One value of a proxy-sized image at a source-normalized point, blended from the four
/// pixels around it.
///
/// **The shader's `zone_blend`, on the CPU** (`raw-gpu/src/zone_mask.wgsl`): pixel centers,
/// clamped at the border. The two change together, or a sample stops reading what the
/// picture shows.
fn bilinear(a: &[f32], w: usize, h: usize, u: f32, v: f32) -> f32 {
    let fx = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let at = |x: usize, y: usize| a[y * w + x];
    let top = at(x0, y0) + (at(x1, y0) - at(x0, y0)) * tx;
    let bottom = at(x0, y1) + (at(x1, y1) - at(x0, y1)) * tx;
    top + (bottom - top) * ty
}

/// How strongly a range must select a pixel for an island to grow through it: half.
///
/// Through the range's full-strength band and the inner half of its shoulders, and no
/// further. Growing through anything the range selects at all would let an island leak
/// along the faintest shoulder tones, across a sky that is only a little like the cloud
/// in it, and out to every other bright thing the sky touches.
const ISLAND_GROWS_THROUGH: f32 = 0.5;

/// How far past its edge an island lets the range fade out, as a fraction of the
/// proxy's long edge: a little over one percent of the frame.
///
/// The band of shoulder tones around a subject is the subject's own soft edge, and
/// cutting the island off where it stopped growing would cut that edge hard. This is
/// wide enough to keep it and narrow enough that nothing across a gap is reached.
///
/// Counted in pixels from the island, and not blurred out of it: a blur gives a thin
/// part of a subject less of an edge than a broad one, and was about a third of what
/// an island cost to work out.
const ISLAND_EDGE: f32 = 0.0125;

/// The narrowest connection an island grows across, as a fraction of the proxy's long
/// edge on each side of the pixel: a neck has to be wider than about one percent of
/// the frame to carry it.
///
/// Two subjects of the same tone nearly always touch somewhere, by a strip of rock that
/// reaches the water or a highlight along an edge, and an island that grew through one
/// pixel of contact would be the whole range again: on a cliff over the sea it was, for
/// a click on either. So the growing ignores necks this thin, and a subject is what is
/// joined by something with width.
///
/// **It errs toward selecting too little**, on purpose: a subject whose parts are
/// joined only thinly, a rock face divided by its cracks, comes out one part at a time,
/// and another seed takes the next part. A leak has no such remedy, since no seed takes
/// a thing away.
const ISLAND_NECK: f32 = 0.004;

/// Limit a range's selection to what is connected to its seeds.
///
/// `selected` is the range's selection at each proxy pixel, before `Inv`, and what
/// comes back is **a weight for each pixel that the selection is multiplied by**: one
/// across the island, nothing on another subject, and a fade between. A weight and not
/// the limited selection, so that the shader can cut the range at every pixel it renders
/// and still end where the island does. Each seed
/// grows through its neighbors, diagonals included, wherever the range selects by at
/// least [`ISLAND_GROWS_THROUGH`]; what none of them reaches is taken out. A seed on a
/// pixel the range does not select that strongly grows nothing, which is what a seed
/// left behind by a moved range should do.
///
/// **It grows through the body of a subject and not through its necks.** The growing
/// runs over the selection worn back by [`ISLAND_NECK`] on every side, where a thin
/// connection has no pixels left, and that finds the bodies the seeds are in. Every
/// other strongly selected pixel, the rim that was worn off and whatever is too thin to
/// have a body, then goes to the body it is nearest to along the selection: a twig
/// stays with its branch, and a neck between two subjects is divided between them. A
/// seed's own pixel counts as a body however thin the thing it is on, so a click on a
/// wire selects the wire: a click always selects what is under it.
///
/// **The edge is the range's own.** Inside what was reached the selection is untouched,
/// and just outside it the range's shoulder is kept in a band [`ISLAND_EDGE`] wide and
/// fades to nothing across it, so the island ends as softly as the range does there
/// and there is no step where the growing stopped. Only the shoulder: a pixel the range
/// selects strongly and no seed reached is another subject, however near, and is out.
///
/// **Except across a neck**, the one place the island ends where the range is still at
/// full strength on both sides. There the selection fades out over the same
/// [`ISLAND_EDGE`], on the island's side of the cut, or the cut would be a step with
/// the proxy's pixels for its outline.
fn island(selected: &[f32], w: usize, h: usize, seeds: &[(f32, f32)]) -> Vec<f32> {
    let strong: Vec<bool> = selected
        .iter()
        .map(|&weight| weight >= ISLAND_GROWS_THROUGH)
        .collect();
    let neck = (ISLAND_NECK * w.max(h) as f32).round() as usize;
    let body = worn(&strong, w, h, neck);
    // Where the seeds are, and the bodies within a neck's width of them.
    let on: Vec<usize> = seeds
        .iter()
        .map(|&(x, y)| {
            let px = ((x * w as f32) as usize).min(w - 1);
            let py = ((y * h as f32) as usize).min(h - 1);
            py * w + px
        })
        .filter(|&at| strong[at])
        .collect();
    let near = grown(&strong, w, h, &on, neck);
    let starts: Vec<usize> = (0..w * h).filter(|&p| near[p] && body[p]).collect();
    let seeded = grown(&body, w, h, &starts, usize::MAX);

    // Whose each strongly selected pixel is: the island's, or another body's. Both
    // kinds of body grow outward through the selection a step at a time, and a pixel
    // is taken by whichever gets to it first, the island's on a tie.
    const UNCLAIMED: u8 = 0;
    const ISLANDS: u8 = 1;
    const ANOTHERS: u8 = 2;
    let mut owner = vec![UNCLAIMED; w * h];
    let mut frontier: Vec<usize> = Vec::new();
    for at in (0..w * h).filter(|&p| seeded[p]).chain(on.iter().copied()) {
        if owner[at] == UNCLAIMED {
            owner[at] = ISLANDS;
            frontier.push(at);
        }
    }
    for at in (0..w * h).filter(|&p| body[p]) {
        if owner[at] == UNCLAIMED {
            owner[at] = ANOTHERS;
            frontier.push(at);
        }
    }
    let mut next = Vec::new();
    while !frontier.is_empty() {
        for &at in &frontier {
            let (px, py) = (at % w, at / w);
            for ny in py.saturating_sub(1)..=(py + 1).min(h - 1) {
                for nx in px.saturating_sub(1)..=(px + 1).min(w - 1) {
                    let to = ny * w + nx;
                    if strong[to] && owner[to] == UNCLAIMED {
                        owner[to] = owner[at];
                        next.push(to);
                    }
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next);
        next.clear();
    }
    let reached: Vec<bool> = owner.iter().map(|&o| o == ISLANDS).collect();
    let band = ((ISLAND_EDGE * w.max(h) as f32).round() as usize).max(1);
    let ease = |t: f32| t * t * (3.0 - 2.0 * t);
    let all = || 0..w * h;
    let mut weights = vec![1.0f32; w * h];

    // The cuts: what was reached and touches something as strongly selected that was
    // not, which only a neck leaves. The island fades toward them from `band` pixels
    // back, counted through the island and so never around a corner into its body.
    let cut: Vec<usize> = all()
        .filter(|&at| reached[at])
        .filter(|&at| {
            let (px, py) = (at % w, at / w);
            (py.saturating_sub(1)..=(py + 1).min(h - 1)).any(|ny| {
                (px.saturating_sub(1)..=(px + 1).min(w - 1))
                    .any(|nx| strong[ny * w + nx] && !reached[ny * w + nx])
            })
        })
        .collect();
    if !cut.is_empty() {
        let back = steps(&cut, |p| reached[p], w, h, band);
        for at in all().filter(|&at| reached[at] && back[at] < band) {
            weights[at] = ease((back[at] + 1) as f32 / (band + 1) as f32);
        }
    }

    // Outside it: another subject is out, and the range's shoulder fades to nothing
    // over the band, by how many pixels it is from the island. The fade is given to
    // every pixel the island did not reach and not only to those the range selects
    // there, since between two proxy pixels the range can select what it did not at
    // either.
    let edge: Vec<usize> = all().filter(|&at| reached[at]).collect();
    let out = steps(&edge, |p| !reached[p], w, h, band);
    for at in all().filter(|&at| !reached[at]) {
        weights[at] = if strong[at] {
            0.0
        } else {
            ease(1.0 - out[at] as f32 / (band + 1) as f32)
        };
    }
    weights
}

/// How many steps each pixel is from the nearest of `from`, neighbor to neighbor with
/// the diagonals and only through pixels `through` allows, counted as far as `most`:
/// zero on `from` itself, and `most + 1` for whatever is further or cannot be reached.
fn steps(
    from: &[usize],
    through: impl Fn(usize) -> bool,
    w: usize,
    h: usize,
    most: usize,
) -> Vec<usize> {
    let mut far = vec![most + 1; w * h];
    let mut frontier: Vec<usize> = from.to_vec();
    for &at in from {
        far[at] = 0;
    }
    let mut next = Vec::new();
    for step in 1..=most {
        if frontier.is_empty() {
            break;
        }
        for &at in &frontier {
            let (px, py) = (at % w, at / w);
            for ny in py.saturating_sub(1)..=(py + 1).min(h - 1) {
                for nx in px.saturating_sub(1)..=(px + 1).min(w - 1) {
                    let to = ny * w + nx;
                    if far[to] > most && through(to) {
                        far[to] = step;
                        next.push(to);
                    }
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next);
        next.clear();
    }
    far
}

/// What `from` reaches through the pixels of `through`, neighbor to neighbor with the
/// diagonals, in at most `steps` steps. A start that is not itself in `through` reaches
/// nothing.
fn grown(through: &[bool], w: usize, h: usize, from: &[usize], steps: usize) -> Vec<bool> {
    let mut reached = vec![false; w * h];
    let mut frontier = Vec::new();
    for &at in from {
        if through[at] && !reached[at] {
            reached[at] = true;
            frontier.push(at);
        }
    }
    let mut next = Vec::new();
    let mut step = 0;
    while !frontier.is_empty() && step < steps {
        for &at in &frontier {
            let (px, py) = (at % w, at / w);
            for ny in py.saturating_sub(1)..=(py + 1).min(h - 1) {
                for nx in px.saturating_sub(1)..=(px + 1).min(w - 1) {
                    let to = ny * w + nx;
                    if through[to] && !reached[to] {
                        reached[to] = true;
                        next.push(to);
                    }
                }
            }
        }
        std::mem::swap(&mut frontier, &mut next);
        next.clear();
        step += 1;
    }
    reached
}

/// `set` worn back by `by` pixels on every side: the pixels whose whole neighborhood,
/// `by` out in each direction, is in it. The frame's edge wears nothing away, so a
/// subject against the border keeps its body there.
fn worn(set: &[bool], w: usize, h: usize, by: usize) -> Vec<bool> {
    if by == 0 {
        return set.to_vec();
    }
    // Along rows and then along columns, which is the square neighborhood.
    let mut rows = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            rows[y * w + x] =
                (x.saturating_sub(by)..=(x + by).min(w - 1)).all(|nx| set[y * w + nx]);
        }
    }
    let mut out = vec![false; w * h];
    for y in 0..h {
        for x in 0..w {
            out[y * w + x] =
                (y.saturating_sub(by)..=(y + by).min(h - 1)).all(|ny| rows[ny * w + x]);
        }
    }
    out
}

/// Proxy dimensions for a source of `(w, h)`: [`PROXY_WIDTH`] on the long edge,
/// never upscaling.
///
/// The **long** edge, so a portrait frame gets the same number of proxy pixels as
/// the landscape one it was cropped from — a mask whose resolution depended on
/// which way up the negative was would be the same defect the Contrast Mask spacer
/// avoids by measuring the diagonal.
fn proxy_dims(w: usize, h: usize) -> (usize, usize) {
    let long = w.max(h);
    if long <= PROXY_WIDTH {
        return (w.max(1), h.max(1));
    }
    let s = PROXY_WIDTH as f32 / long as f32;
    (
        ((w as f32 * s).round() as usize).max(1),
        ((h as f32 * s).round() as usize).max(1),
    )
}

/// Area-average `src` down to `dw` x `dh`.
///
/// An area average and not a point sample, because the proxy is a *statistic* of
/// the frame — the histogram behind the ruler is drawn from it — and point
/// sampling a 6000px frame at 480 would report the tones of one pixel in a
/// hundred and fifty.
fn downsample(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; dw * dh];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = (((y + 1) * sh).div_ceil(dh)).min(sh).max(y0 + 1);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = (((x + 1) * sw).div_ceil(dw)).min(sw).max(x0 + 1);
            let mut sum = 0.0f64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    sum += src[sy * sw + sx] as f64;
                }
            }
            out[y * dw + x] = (sum / ((y1 - y0) * (x1 - x0)) as f64) as f32;
        }
    }
    out
}

/// Mean over a `(2r+1)²` window, via an integral image.
///
/// **Exact at the borders**: the window is clipped to the frame and divided by the
/// number of pixels actually in it, rather than by the nominal window area. The
/// alternative — treating the outside as zero — darkens every edge of the mask by
/// an amount that depends on the radius, which on a `region` sweep looks exactly
/// like the filter breaking down at high settings.
///
/// `O(N)` whatever the radius, which is why the prototype's fast-guided-filter
/// subsampling is **not** carried over: it exists to make a large radius cheap,
/// and with an integral image a large radius already is. One less resampling stage
/// to get wrong.
fn box_mean(a: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    // f64 for the running sums. At 480x320 an f32 accumulator loses low bits by
    // the far corner, and the difference of two large sums is exactly where that
    // shows up.
    let mut ii = vec![0.0f64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0.0f64;
        for x in 0..w {
            row += a[y * w + x] as f64;
            ii[(y + 1) * (w + 1) + x + 1] = ii[y * (w + 1) + x + 1] + row;
        }
    }
    let at = |x: usize, y: usize| ii[y * (w + 1) + x];

    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        let y0 = y.saturating_sub(r);
        let y1 = (y + r + 1).min(h);
        for x in 0..w {
            let x0 = x.saturating_sub(r);
            let x1 = (x + r + 1).min(w);
            let s = at(x1, y1) - at(x0, y1) - at(x1, y0) + at(x0, y0);
            out[y * w + x] = (s / ((y1 - y0) * (x1 - x0)) as f64) as f32;
        }
    }
    out
}

/// Self-guided filter (He et al. 2010), greyscale — edge-aware smoothing.
///
/// ```text
///   q = mean(a)·I + mean(b),   a = var / (var + eps),   b = (1 − a)·mean(I)
/// ```
///
/// Inside a smooth region `var ≪ eps`, so `a ≈ 0` and `q` is the local mean:
/// strong smoothing. Across an edge `var ≫ eps`, so `a ≈ 1` and `q ≈ I`: the edge
/// survives untouched. `eps` is in the **squared units of the input**, which for an
/// EV image makes `sqrt(eps)` the soft edge threshold in stops — see
/// [`ZoneMask::edge`], which stores that root rather than the square.
fn guided(img: &[f32], w: usize, h: usize, r: usize, eps: f32) -> Vec<f32> {
    let mean_i = box_mean(img, w, h, r);
    let sq: Vec<f32> = img.iter().map(|v| v * v).collect();
    let mean_ii = box_mean(&sq, w, h, r);

    let eps = eps.max(1e-6);
    let mut a = vec![0.0f32; w * h];
    let mut b = vec![0.0f32; w * h];
    for i in 0..w * h {
        let var = (mean_ii[i] - mean_i[i] * mean_i[i]).max(0.0);
        a[i] = var / (var + eps);
        b[i] = (1.0 - a[i]) * mean_i[i];
    }
    let ma = box_mean(&a, w, h, r);
    let mb = box_mean(&b, w, h, r);
    (0..w * h).map(|i| ma[i] * img[i] + mb[i]).collect()
}

/// Separable Gaussian with clamped edges, truncated at three sigma.
///
/// Three sigma because beyond it a Gaussian contributes under 0.3% — the same
/// figure `raw_graph::build` uses to size the Contrast Mask apron, kept the same
/// on purpose so the proxy's mask and the shader's agree about how far a spacer
/// reaches.
fn gaussian(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let radius = (3.0 * sigma).ceil().max(1.0) as usize;
    let kernel: Vec<f32> = {
        let mut k: Vec<f32> = (0..=2 * radius)
            .map(|i| {
                let d = i as f32 - radius as f32;
                (-0.5 * d * d / (sigma * sigma)).exp()
            })
            .collect();
        let sum: f32 = k.iter().sum();
        for v in &mut k {
            *v /= sum;
        }
        k
    };

    let mut tmp = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in kernel.iter().enumerate() {
                let sx = (x as isize + i as isize - radius as isize).clamp(0, w as isize - 1);
                acc += src[y * w + sx as usize] * kv;
            }
            tmp[y * w + x] = acc;
        }
    }
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0;
            for (i, kv) in kernel.iter().enumerate() {
                let sy = (y as isize + i as isize - radius as isize).clamp(0, h as isize - 1);
                acc += tmp[sy as usize * w + x] * kv;
            }
            out[y * w + x] = acc;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Dims;

    fn luma(w: usize, h: usize, data: Vec<f32>) -> LumaImage {
        LumaImage {
            data,
            output_dims: Dims { w, h },
            source_dims: Dims { w, h },
            clipped: Vec::new(),
        }
    }

    #[test]
    fn a_box_mean_of_a_constant_is_that_constant_at_the_border_too() {
        // The border rule, as an assertion. Treating the outside as zero would make
        // the corners read 0.25 of the true mean at a large radius — which looks
        // like the guided filter failing at high Region settings rather than like
        // an averaging bug, and is why this is pinned separately.
        let a = vec![0.7f32; 40 * 30];
        for r in [1, 5, 25] {
            let m = box_mean(&a, 40, 30, r);
            for (i, v) in m.iter().enumerate() {
                assert!((v - 0.7).abs() < 1e-5, "r={r} at {i}: {v}");
            }
        }
    }

    #[test]
    fn a_box_mean_averages_what_is_actually_in_the_window() {
        // 4x1 ramp, radius 1: each output is the mean of at most three neighbours,
        // clipped at the ends. Small enough to check by hand, which is the point.
        let a = [0.0f32, 1.0, 2.0, 3.0];
        let m = box_mean(&a, 4, 1, 1);
        assert!(
            (m[0] - 0.5).abs() < 1e-6,
            "two pixels at the left edge: {}",
            m[0]
        );
        assert!((m[1] - 1.0).abs() < 1e-6);
        assert!((m[2] - 2.0).abs() < 1e-6);
        assert!(
            (m[3] - 2.5).abs() < 1e-6,
            "two pixels at the right edge: {}",
            m[3]
        );
    }

    #[test]
    fn the_guided_filter_keeps_an_edge_and_flattens_a_ripple() {
        // The two behaviours the mask depends on, in one image: a 4 EV step down
        // the middle, with a ±0.1 EV ripple everywhere. eps is (0.5 EV)², so the
        // step is well above the threshold and the ripple well below.
        let (w, h) = (64, 32);
        let mut img = vec![0.0f32; w * h];
        for y in 0..h {
            for x in 0..w {
                let step = if x < w / 2 { -2.0 } else { 2.0 };
                let ripple = if (x + y) % 2 == 0 { 0.1 } else { -0.1 };
                img[y * w + x] = step + ripple;
            }
        }
        let q = guided(&img, w, h, 6, 0.25);

        // The ripple is gone well away from the edge.
        let flat: Vec<f32> = (4..12).map(|x| q[16 * w + x]).collect();
        let spread = flat.iter().cloned().fold(f32::MIN, f32::max)
            - flat.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.05, "ripple should be smoothed, spread {spread}");
        assert!(
            (flat[0] + 2.0).abs() < 0.05,
            "and settle on the region's own level"
        );

        // The step survives: still nearly its full height across the boundary.
        let jump = q[16 * w + w / 2 + 2] - q[16 * w + w / 2 - 3];
        assert!(jump > 3.0, "a 4 EV edge must survive, got {jump}");
    }

    #[test]
    fn a_guided_filter_of_a_constant_is_that_constant() {
        let a = vec![-1.25f32; 32 * 32];
        for v in guided(&a, 32, 32, 4, 0.25) {
            assert!((v + 1.25).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn the_basis_reads_middle_grey_as_zero_ev() {
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        for e in &b.ev {
            assert!(e.abs() < 1e-5, "0.18 must be Zone V: {e}");
        }
    }

    #[test]
    fn contrast_mask_keeps_grey_at_zero_and_compresses_exposed_flat_tones() {
        for contrast in [0.05, 0.35, 0.60] {
            for ev in [-3.0_f32, 0.0, 2.0] {
                let l = luma(32, 24, vec![MID_GREY + 0.01; 32 * 24]);
                let b = Basis::build(
                    &l,
                    &ExposureParams {
                        ev,
                        black: 0.01,
                        ..Default::default()
                    },
                    &ContrastMaskParams {
                        enabled: true,
                        contrast,
                        ..Default::default()
                    },
                );
                let expected = ev * (1.0 - contrast);
                for &got in &b.ev {
                    assert!(
                        (got - expected).abs() < 1e-5,
                        "contrast={contrast}, exposure={ev}: {got} vs {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn exposure_moves_the_basis_by_exactly_that_many_stops() {
        // The property the zone chips depend on: push exposure up a stop and what
        // was Zone V is Zone VI. If this drifts, every preset lands somewhere else.
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let up = ExposureParams {
            ev: 1.5,
            ..Default::default()
        };
        let b = Basis::build(&l, &up, &ContrastMaskParams::default());
        for e in &b.ev {
            assert!((e - 1.5).abs() < 1e-5, "{e}");
        }
    }

    #[test]
    fn the_proxy_survives_every_slider_that_is_not_luminance() {
        // The caching boundary, as an assertion. The expensive half of the mask
        // chain must depend on the luminance image alone — if a `Proxy` ever grew
        // a field that a module could move, a slider drag would start
        // downsampling a hundred megapixels sixty times a second.
        let l = luma(
            64,
            48,
            (0..64 * 48).map(|i| (i % 17) as f32 / 17.0).collect(),
        );
        let a = Proxy::of(&l);
        let b = Proxy::of(&l);
        assert_eq!(a, b);
        // Two very different bases off one proxy, which is the point of the split.
        let flat = a.basis(&ExposureParams::default(), &ContrastMaskParams::default());
        let lifted = a.basis(
            &ExposureParams {
                ev: 2.0,
                ..Default::default()
            },
            &ContrastMaskParams::default(),
        );
        assert_ne!(flat.ev, lifted.ev);
        assert_eq!(
            a, b,
            "and building a basis must not consume or mutate the proxy"
        );
    }

    #[test]
    fn the_proxy_is_the_long_edge_and_never_upscales() {
        assert_eq!(proxy_dims(6000, 4000), (480, 320));
        assert_eq!(
            proxy_dims(4000, 6000),
            (320, 480),
            "portrait gets the same pixel budget"
        );
        assert_eq!(
            proxy_dims(120, 80),
            (120, 80),
            "a small frame is left alone"
        );
    }

    #[test]
    fn downsampling_averages_rather_than_samples() {
        // A one-pixel checkerboard reduced 4x must read as the mean, not as
        // whichever phase the sample landed on. The histogram behind the ruler is
        // drawn from this, so point sampling would misreport the tonal
        // distribution of every finely textured frame.
        let (w, h) = (16, 16);
        let src: Vec<f32> = (0..w * h)
            .map(|i| if (i / w + i % w) % 2 == 0 { 0.0 } else { 1.0 })
            .collect();
        for v in downsample(&src, w, h, 4, 4) {
            assert!((v - 0.5).abs() < 1e-6, "{v}");
        }
    }

    #[test]
    fn an_identity_mask_evaluates_to_nothing_at_all() {
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        assert!(b.evaluate(&ZoneMask::default()).is_none());
        let open = ZoneMask {
            enabled: true,
            ..Default::default()
        };
        assert!(
            b.evaluate(&open).is_none(),
            "on but open at both ends is still nothing"
        );
    }

    #[test]
    fn a_shadow_mask_selects_the_shadows_and_not_the_highlights() {
        // Half the frame four stops down, half two stops up. The Shadows preset
        // must come out near 1 on the dark half and near 0 on the bright one.
        let (w, h) = (64, 32);
        let data: Vec<f32> = (0..w * h)
            .map(|i| {
                if i % w < w / 2 {
                    MID_GREY / 16.0
                } else {
                    MID_GREY * 4.0
                }
            })
            .collect();
        let l = luma(w, h, data);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );

        let (_, lo, hi, f_lo, f_hi) = ZoneMask::PRESETS[0];
        let m = ZoneMask {
            enabled: true,
            lo,
            hi,
            f_lo,
            f_hi,
            ..Default::default()
        };
        let mask = b.evaluate(&m).expect("a bounded mask is not the identity");
        assert!(
            mask[16 * b.w + 4] > 0.95,
            "dark half: {}",
            mask[16 * b.w + 4]
        );
        assert!(
            mask[16 * b.w + b.w - 4] < 0.05,
            "bright half: {}",
            mask[16 * b.w + b.w - 4]
        );
    }

    // ── Sampling the picture ─────────────────────────────────────────────────

    /// A synthetic basis: a wall at -2 EV on the left half and a sky at +2 EV on the
    /// right, each with a fine ±0.1 EV ripple, so the regional read and the per-pixel
    /// read are different images.
    fn wall_and_sky() -> Basis {
        let (w, h) = (64, 32);
        let ev = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let level = if x < w / 2 { -2.0 } else { 2.0 };
                level + if (x + y) % 2 == 0 { 0.1 } else { -0.1 }
            })
            .collect();
        Basis { w, h, ev }
    }

    /// The center of proxy pixel `(x, y)`, source-normalized.
    fn center(b: &Basis, x: usize, y: usize) -> (f32, f32) {
        ((x as f32 + 0.5) / b.w as f32, (y as f32 + 0.5) / b.h as f32)
    }

    fn per_pixel() -> ZoneMask {
        ZoneMask {
            edge_aware: false,
            ..ZoneMask::default()
        }
    }

    #[test]
    fn a_sample_reads_pixel_centers_and_blends_between_them() {
        let b = Basis {
            w: 2,
            h: 2,
            ev: vec![0.0, 1.0, 2.0, 3.0],
        };
        let r = b.reading(&per_pixel());
        // The four centers are the four values.
        for (i, want) in [0.0, 1.0, 2.0, 3.0].into_iter().enumerate() {
            let (x, y) = center(&b, i % 2, i / 2);
            assert!((r.at(x, y) - want).abs() < 1e-6, "pixel {i}");
        }
        // Half way between all four is their mean, and past a center the edge holds.
        assert!((r.at(0.5, 0.5) - 1.5).abs() < 1e-6);
        assert!((r.at(0.0, 0.0) - 0.0).abs() < 1e-6, "clamped, not extrapolated");
        assert!((r.at(1.0, 1.0) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn a_click_sets_a_range_around_the_tone_that_was_clicked() {
        // Per pixel, the click reads the pixel: the wall's ripple and all.
        let b = wall_and_sky();
        let (x, y) = center(&b, 10, 16);
        let tone = b.reading(&per_pixel()).at(x, y);
        assert!((tone - b.ev[16 * b.w + 10]).abs() < 1e-6);
        let mut m = per_pixel();
        m.sample(tone, tone);
        assert!((m.lo - (tone - 0.5)).abs() < 1e-6 && (m.hi - (tone + 0.5)).abs() < 1e-6);
        // The wall is in and the sky is out.
        let mask = b.evaluate(&m).expect("a bounded range");
        assert!(mask[16 * b.w + 10] > 0.99);
        assert!(mask[16 * b.w + 50] < 0.01);
    }

    #[test]
    fn an_edge_aware_sample_reads_the_region_and_what_is_clicked_is_selected() {
        // Edge-aware, the range reads the wall as one tone, -2 EV, with the ripple
        // smoothed away, and the sample has to read that same image: the property is
        // that the point clicked ends up inside the range it set, on either side of the
        // edge and close up against it.
        let b = wall_and_sky();
        let regional = ZoneMask::default();
        assert!(regional.edge_aware, "the default a new range starts with");
        let r = b.reading(&regional);
        let (x, y) = center(&b, 10, 16);
        assert!((r.at(x, y) + 2.0).abs() < 0.05, "the wall's level: {}", r.at(x, y));
        for px in [3, 10, 29, 34, 50, 60] {
            let (x, y) = center(&b, px, 16);
            let tone = r.at(x, y);
            let mut m = regional;
            m.sample(tone, tone);
            let weight = b.weight_at(&m, x, y);
            assert!(weight > 0.99, "column {px}: clicked {tone}, selected {weight}");
        }
    }

    #[test]
    fn a_box_spans_its_tones_and_stray_pixels_do_not_stretch_it() {
        // A ramp from -3 to +3 EV across the frame, with one blown pixel in it.
        let (w, h) = (100, 10);
        let mut ev: Vec<f32> = (0..w * h)
            .map(|i| -3.0 + 6.0 * ((i % w) as f32 + 0.5) / w as f32)
            .collect();
        ev[5 * w + 30] = 9.0;
        let b = Basis { w, h, ev };
        let r = b.reading(&per_pixel());
        // Every pixel center in the columns 20 to 59: tones -1.8 to +0.6, and the glint.
        let points = (20..60).flat_map(|x| (0..h).map(move |y| (x, y)));
        let (lo, hi) = r
            .span(points.map(|(x, y)| center(&b, x, y)))
            .expect("the box is on the negative");
        // The 10th and 90th percentiles of a ramp are a tenth of the way in from its ends,
        // to within the 0.06 EV step between two columns.
        assert!((lo - (-1.77 + 0.1 * 2.34)).abs() < 0.07, "low end {lo}");
        assert!((hi - (-1.77 + 0.9 * 2.34)).abs() < 0.07, "high end {hi}");
        assert!(hi < 1.0, "the glint stretched the range to {hi}");

        // And the range it sets covers the middle of the box, not the frame.
        let mut m = per_pixel();
        m.sample(lo, hi);
        assert_eq!(m.evaluate(-0.6), 1.0);
        assert_eq!(m.evaluate(2.5), 0.0);
    }

    #[test]
    fn a_box_off_the_negative_samples_nothing() {
        let b = wall_and_sky();
        let r = b.reading(&per_pixel());
        assert!(r.span([(-0.2, 0.5), (1.3, 0.5), (0.5, f32::NAN)]).is_none());
        assert!(r.span(std::iter::empty()).is_none());
        // One point on it is enough, and both ends are that point's tone.
        let (x, y) = center(&b, 50, 4);
        let (lo, hi) = r.span([(-0.2, 0.5), (x, y)]).expect("one point landed");
        assert_eq!(lo, hi);
        assert!((lo - b.ev[4 * b.w + 50]).abs() < 1e-6);
    }

    #[test]
    fn the_eyedropper_sets_a_range_from_a_click_a_box_and_a_shift_click() {
        let b = wall_and_sky();
        let wall = center(&b, 10, 16);
        let sky = center(&b, 50, 16);
        let mut tone = ZoneMask::default();

        // A click on the wall: one zone around the wall's level, switched on.
        assert!(b.sample_into(&mut tone, [wall], false));
        assert!(tone.enabled);
        assert!((tone.lo + 2.5).abs() < 0.05 && (tone.hi + 1.5).abs() < 0.05, "{tone:?}");
        assert!(b.weight_at(&tone, wall.0, wall.1) > 0.99);
        assert!(b.weight_at(&tone, sky.0, sky.1) < 0.01);

        // Shift-click the sky: the range grows to reach it and still holds the wall.
        let clicked = tone;
        assert!(b.sample_into(&mut tone, [sky], true));
        assert!((tone.lo - clicked.lo).abs() < 1e-6, "the low end moved");
        assert!((tone.hi - 2.0).abs() < 0.05, "{tone:?}");
        assert!(b.weight_at(&tone, wall.0, wall.1) > 0.99);
        assert!(b.weight_at(&tone, sky.0, sky.1) > 0.99);

        // A box inside the sky replaces the range with the sky's.
        let points = (40..60).flat_map(|x| (8..24).map(move |y| (x, y)));
        assert!(b.sample_into(&mut tone, points.map(|(x, y)| center(&b, x, y)), false));
        assert!((tone.lo - 1.5).abs() < 0.05 && (tone.hi - 2.5).abs() < 0.05, "{tone:?}");

        // A click that missed the negative changes nothing and says so.
        let before = tone;
        assert!(!b.sample_into(&mut tone, [(1.5, 0.5)], false));
        assert_eq!(tone, before);
    }

    // ── The island ───────────────────────────────────────────────────────────

    /// A cloud and a shirt: two bright patches at +2 EV on a ground at -3 EV, far
    /// apart, each with an edge that ramps down to the ground over five pixels. The
    /// cloud is centered at (30, 20), the shirt at (90, 40), each 12 pixels in radius
    /// to where its ramp begins.
    fn cloud_and_shirt() -> Basis {
        let (w, h) = (120usize, 60usize);
        let ev = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                let to = |cx: f32, cy: f32| ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                let nearest = to(30.0, 20.0).min(to(90.0, 40.0));
                // Full brightness to radius 12, the ground from radius 17.
                2.0 - 5.0 * ((nearest - 12.0) / 5.0).clamp(0.0, 1.0)
            })
            .collect();
        Basis { w, h, ev }
    }

    /// The highlights, read per pixel, with a one-stop shoulder below them.
    fn brights() -> ZoneMask {
        ZoneMask {
            enabled: true,
            lo: 1.0,
            f_lo: 1.0,
            edge_aware: false,
            ..ZoneMask::default()
        }
    }

    fn seeded(seeds: &[(usize, usize)], b: &Basis) -> ZoneMask {
        let mut m = brights();
        m.island.enabled = true;
        for &(x, y) in seeds {
            assert!(m.island.add(center(b, x, y)));
        }
        m
    }

    #[test]
    fn an_island_takes_what_is_connected_to_its_seed_and_leaves_the_rest() {
        let b = cloud_and_shirt();
        let at = |mask: &[f32], x: usize, y: usize| mask[y * b.w + x];
        // The plain range takes both: every bright thing in the frame.
        let plain = b.evaluate(&brights()).expect("a bounded range");
        assert_eq!(at(&plain, 30, 20), 1.0);
        assert_eq!(at(&plain, 90, 40), 1.0);

        // Seeded on the cloud, it is the cloud, whole, and none of the shirt.
        let cloud = b.evaluate(&seeded(&[(30, 20)], &b)).expect("a bounded range");
        for (x, y) in [(30, 20), (22, 14), (40, 20), (30, 31)] {
            assert_eq!(at(&cloud, x, y), 1.0, "the cloud at ({x}, {y})");
        }
        for (x, y) in [(90, 40), (84, 36), (100, 40)] {
            assert_eq!(at(&cloud, x, y), 0.0, "the shirt at ({x}, {y})");
        }
        assert_eq!(at(&cloud, 60, 30), 0.0, "the ground between them");
        // Inside what it reached, the island is the range, untouched.
        for i in 0..b.w * b.h {
            assert!(cloud[i] <= plain[i] + 1e-6, "an island selected more than its range");
        }

        // And seeded on the shirt, the other way round.
        let shirt = b.evaluate(&seeded(&[(90, 40)], &b)).expect("a bounded range");
        assert_eq!(at(&shirt, 90, 40), 1.0);
        assert_eq!(at(&shirt, 30, 20), 0.0);
    }

    #[test]
    fn several_seeds_are_several_islands_in_one_mask() {
        let b = cloud_and_shirt();
        let both = b
            .evaluate(&seeded(&[(30, 20), (90, 40)], &b))
            .expect("a bounded range");
        let plain = b.evaluate(&brights()).expect("a bounded range");
        // Every bright thing in this frame has a seed in it, so wherever the range
        // selects strongly the two are the range, and nowhere do they select more.
        for i in 0..b.w * b.h {
            assert!(both[i] <= plain[i] + 1e-6, "pixel {i}");
            if plain[i] >= 0.5 {
                assert!((both[i] - plain[i]).abs() < 1e-6, "pixel {i}");
            }
        }
        assert_eq!(both[20 * b.w + 30], 1.0);
        assert_eq!(both[40 * b.w + 90], 1.0);
        // A second seed in the same patch changes nothing.
        let twice = b
            .evaluate(&seeded(&[(30, 20), (34, 24)], &b))
            .expect("a bounded range");
        assert_eq!(twice, b.evaluate(&seeded(&[(30, 20)], &b)).unwrap());
    }

    #[test]
    fn an_island_ends_as_softly_as_its_range_does() {
        // A bright field that ramps down to the ground over sixty pixels of a strip as
        // wide as the proxy, a stop every twelve, so the range's shoulder is twelve
        // pixels of the ramp. The island grows to the half-strength line and must not
        // step off there: it follows the range down into its shoulder and fades.
        let (w, h) = (480usize, 8usize);
        let ev = (0..w * h)
            .map(|i| 2.0 - 5.0 * (((i % w) as f32 - 200.0) / 60.0).clamp(0.0, 1.0))
            .collect();
        let b = Basis { w, h, ev };
        let plain = b.evaluate(&brights()).expect("a bounded range");
        let mut seeded = brights();
        seeded.island.enabled = true;
        seeded.island.add(center(&b, 100, 4));
        let island = b.evaluate(&seeded).expect("a bounded range");

        let row = |mask: &[f32]| -> Vec<f32> { (190..250).map(|x| mask[4 * w + x]).collect() };
        let (range, island) = (row(&plain), row(&island));
        let half = range.iter().position(|&r| r < 0.5).expect("the range lets go");
        assert!(range[half] > 0.3, "the test's shoulder is not gentle: {}", range[half]);
        for (i, (r, m)) in range.iter().zip(&island).enumerate() {
            assert!(m <= r, "column {}: {m} above the range's {r}", 190 + i);
            if i < half {
                assert_eq!(m, r, "column {}: inside the island", 190 + i);
            }
        }
        // Just past where it grew to, it is still nearly the range.
        assert!(
            island[half] > range[half] * 0.9,
            "the shoulder was cut where the growing stopped: {} of {}",
            island[half],
            range[half]
        );
        // And there is no cliff. Cut off where it stopped growing, it would drop by all
        // of what the range still selects there in one pixel; it lets go over several,
        // somewhat faster than the range does and nowhere by half that drop.
        let steepest = |row: &[f32]| row.windows(2).map(|w| (w[0] - w[1]).abs()).fold(0.0f32, f32::max);
        assert!(
            steepest(&island) < range[half] * 0.5,
            "a step of {} where a cut would be {} and the range's steepest is {}",
            steepest(&island),
            range[half],
            steepest(&range)
        );
        // Far down the ramp it is nothing, as the range is.
        assert_eq!(island.last().copied(), Some(0.0));
    }

    #[test]
    fn a_subject_close_by_and_not_touching_is_left_out_whole() {
        // Two bright fields a few pixels apart, closer than the band the island's edge
        // fades over. The unseeded one is strongly selected by the range and no seed
        // reached it, so it is another subject and none of it comes with the island.
        let (w, h) = (480usize, 8usize);
        let ev = (0..w * h)
            .map(|i| {
                let x = i % w;
                if x < 200 || (203..300).contains(&x) { 2.0 } else { -3.0 }
            })
            .collect();
        let b = Basis { w, h, ev };
        let mut seeded = brights();
        seeded.island.enabled = true;
        seeded.island.add(center(&b, 100, 4));
        let island = b.evaluate(&seeded).expect("a bounded range");
        assert_eq!(island[4 * w + 199], 1.0, "the seeded field, to its edge");
        for x in [203, 204, 210, 250] {
            assert_eq!(island[4 * w + x], 0.0, "column {x} of the field next door");
        }
    }

    #[test]
    fn an_island_with_nowhere_to_grow_from_selects_as_it_should() {
        let b = cloud_and_shirt();
        let plain = b.evaluate(&brights()).expect("a bounded range");
        // Switched on with no seed placed: nothing to be connected to, so the range.
        let mut unseeded = brights();
        unseeded.island.enabled = true;
        assert_eq!(b.evaluate(&unseeded).unwrap(), plain);
        // Seeds, and switched off: the range, with the seeds kept for when it is on.
        let mut off = seeded(&[(30, 20)], &b);
        off.island.enabled = false;
        assert_eq!(b.evaluate(&off).unwrap(), plain);
        assert_eq!(off.island.seeds().len(), 1);
        // A seed on a tone the range does not select grows nothing at all.
        let stranded = b.evaluate(&seeded(&[(60, 30)], &b)).expect("a bounded range");
        assert!(stranded.iter().all(|&v| v == 0.0), "an island grew from the ground");
        // Inverted, the island is the hole: everything but the cloud.
        let mut inverted = seeded(&[(30, 20)], &b);
        inverted.invert = true;
        let hole = b.evaluate(&inverted).expect("inverted is never the identity");
        assert_eq!(hole[20 * b.w + 30], 0.0, "the cloud");
        assert_eq!(hole[40 * b.w + 90], 1.0, "the shirt, which the island left out");
        assert_eq!(hole[30 * b.w + 60], 1.0);
        // And a range open at both ends is everything, island or not.
        let mut open = ZoneMask {
            enabled: true,
            ..ZoneMask::default()
        };
        open.island.enabled = true;
        open.island.add((0.25, 0.3));
        assert!(b.evaluate(&open).is_none());
    }

    #[test]
    fn an_island_grows_across_a_diagonal_and_not_across_a_gap() {
        // A staircase of bright pixels touching only at their corners is one subject;
        // the same pixels one further apart are not.
        let (w, h) = (24usize, 24usize);
        let stairs = |gap: usize| {
            let mut ev = vec![-3.0f32; w * h];
            for i in 0..8 {
                ev[(2 + i * gap) * w + 2 + i * gap] = 2.0;
            }
            Basis { w, h, ev }
        };
        let mut m = brights();
        m.island.enabled = true;
        m.island.add((2.5 / w as f32, 2.5 / h as f32));
        let joined = stairs(1).evaluate(&m).expect("a bounded range");
        assert_eq!(joined[9 * w + 9], 1.0, "the far end of the staircase");
        let apart = stairs(2).evaluate(&m).expect("a bounded range");
        assert_eq!(apart[2 * w + 2], 1.0, "the pixel the seed is on");
        assert!(apart[16 * w + 16] < 0.01, "a pixel across a gap: {}", apart[16 * w + 16]);
    }

    /// Two bright blocks on a full-size proxy, joined by a strip `neck` pixels tall.
    fn two_blocks_joined_by(neck: usize) -> Basis {
        let (w, h) = (480usize, 120usize);
        let mut ev = vec![-3.0f32; w * h];
        for y in 20..100 {
            for x in (40..140).chain(300..400) {
                ev[y * w + x] = 2.0;
            }
        }
        for y in 60 - neck / 2..60 - neck / 2 + neck {
            for x in 140..300 {
                ev[y * w + x] = 2.0;
            }
        }
        Basis { w, h, ev }
    }

    #[test]
    fn an_island_grows_through_a_subject_and_not_through_a_thin_neck() {
        // Joined by three pixels of contact, the second block is another subject.
        let b = two_blocks_joined_by(3);
        let m = seeded(&[(90, 60)], &b);
        let mask = b.evaluate(&m).expect("a bounded range");
        let at = |mask: &[f32], x: usize, y: usize| mask[y * b.w + x];
        for (x, y) in [(90, 60), (41, 21), (138, 98), (60, 40)] {
            assert_eq!(at(&mask, x, y), 1.0, "the block the seed is in, at ({x}, {y})");
        }
        // The neck is divided between the two, so its far half is the other block's.
        for (x, y) in [(350, 60), (301, 21), (260, 60)] {
            assert_eq!(at(&mask, x, y), 0.0, "past the neck, at ({x}, {y})");
        }
        assert_eq!(at(&mask, 160, 60), 1.0, "the near end of the neck");
        // The plain range takes all of it, which is what the island is for.
        let plain = b.evaluate(&brights()).expect("a bounded range");
        assert_eq!(at(&plain, 350, 60), 1.0);

        // Joined by something with width, the two are one subject.
        let b = two_blocks_joined_by(9);
        let mask = b.evaluate(&seeded(&[(90, 60)], &b)).expect("a bounded range");
        assert_eq!(at(&mask, 350, 60), 1.0, "the far block, across a wide join");
        assert_eq!(at(&mask, 220, 60), 1.0, "the join itself");
    }

    #[test]
    fn an_island_fades_out_where_a_neck_cuts_it() {
        // The one place an island ends with the range at full strength on both sides.
        // Along the neck, from the island's half into the other's, there is no cliff.
        let b = two_blocks_joined_by(3);
        let mask = b.evaluate(&seeded(&[(90, 60)], &b)).expect("a bounded range");
        let row: Vec<f32> = (180..260).map(|x| mask[60 * b.w + x]).collect();
        assert_eq!(row[0], 1.0, "the island's half of the neck");
        assert_eq!(*row.last().unwrap(), 0.0, "the other block's half");
        let steepest = row.windows(2).map(|p| (p[0] - p[1]).abs()).fold(0.0, f32::max);
        assert!(steepest < 0.35, "a step of {steepest} at the cut");
        // And only there: the block's far side is the range, untouched.
        assert_eq!(mask[60 * b.w + 41], 1.0);
        assert_eq!(mask[21 * b.w + 90], 1.0);
    }

    #[test]
    fn a_seed_on_something_thin_still_selects_what_is_under_it() {
        // A wire across the frame has no body to grow through, and a click on it must
        // not select nothing.
        let (w, h) = (480usize, 120usize);
        let mut ev = vec![-3.0f32; w * h];
        for x in 20..460 {
            ev[60 * w + x] = 2.0;
        }
        let b = Basis { w, h, ev };
        let mask = b.evaluate(&seeded(&[(240, 60)], &b)).expect("a bounded range");
        for x in [20, 100, 240, 459] {
            assert_eq!(mask[60 * w + x], 1.0, "the wire at {x}");
        }
        assert_eq!(mask[30 * w + 240], 0.0, "the ground");
    }

    #[test]
    fn the_weight_at_a_point_is_the_prepared_mask_there() {
        let b = wall_and_sky();
        let m = ZoneMask {
            enabled: true,
            lo: 1.0,
            hi: 5.0,
            blur: 0.05,
            ..ZoneMask::default()
        };
        // Diffused, which is finished at the proxy's pixels, and then not, which is cut
        // at the point: both are the whole mask at a pixel's center.
        for blur in [0.05, 0.0] {
            let m = ZoneMask { blur, ..m };
            let mask = b.evaluate(&m).expect("a bounded range");
            for (px, py) in [(4, 4), (31, 16), (32, 16), (60, 28)] {
                let (x, y) = center(&b, px, py);
                assert!((b.weight_at(&m, x, y) - mask[py * b.w + px]).abs() < 1e-6);
            }
        }
        // A range that masks nothing weighs one, everywhere.
        assert_eq!(b.weight_at(&ZoneMask::default(), 0.3, 0.3), 1.0);
    }

    /// Where a straight edge crosses each row of a mask read at every pixel of a negative
    /// eight times the proxy, and how far the crossings stray from the best straight line
    /// through them, in the negative's pixels.
    fn stray(w: usize, h: usize, at: impl Fn(f32, f32) -> f32) -> f32 {
        let rows: Vec<(f32, f32)> = (h / 10..h - h / 10)
            .step_by(3)
            .map(|y| {
                let v = (y as f32 + 0.5) / h as f32;
                let weight = |x: usize| at((x as f32 + 0.5) / w as f32, v);
                // Selected on the left, so the crossing is the first pixel under half.
                let x = (1..w).find(|&x| weight(x) < 0.5).expect("the edge crosses");
                let (a, b) = (weight(x - 1), weight(x));
                (y as f32, x as f32 - 1.0 + (a - 0.5) / (a - b))
            })
            .collect();
        let n = rows.len() as f32;
        let (my, mx) = rows
            .iter()
            .fold((0.0, 0.0), |(sy, sx), (y, x)| (sy + y / n, sx + x / n));
        let slope = rows.iter().map(|(y, x)| (y - my) * (x - mx)).sum::<f32>()
            / rows.iter().map(|(y, _)| (y - my) * (y - my)).sum::<f32>();
        rows.iter()
            .map(|(y, x)| (x - mx - slope * (y - my)).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn a_range_cut_between_proxy_pixels_follows_an_edge_without_stairs() {
        // A dark ground and a bright one meeting along a slanted straight line, eight
        // times the proxy's width, and a steep range that takes the dark side. Blending a
        // mask finished at the proxy's pixels drew the line as stairs a proxy pixel high;
        // cutting the blended tone at each pixel follows it.
        let (w, h) = (PROXY_WIDTH * 8, PROXY_WIDTH * 16 / 3);
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                if x < 0.4 * w as f32 + 0.37 * y {
                    0.03
                } else {
                    0.5
                }
            })
            .collect();
        let b = Basis::build(&luma(w, h, data), &Default::default(), &Default::default());
        assert_eq!(
            b.w * 8,
            w,
            "eight pixels of the negative to one of the proxy"
        );
        for edge_aware in [true, false] {
            let m = ZoneMask {
                enabled: true,
                hi: 0.5 * ((0.03f32 / MID_GREY).log2() + (0.5f32 / MID_GREY).log2()),
                f_hi: 0.25,
                edge_aware,
                ..ZoneMask::default()
            };
            let finished = b.evaluate(&m).expect("a bounded range");
            let stairs = stray(w, h, |x, y| bilinear(&finished, b.w, b.h, x, y));
            let prepared = b.prepare(&m);
            let straight = stray(w, h, |x, y| prepared.at(x, y));
            // Eight pixels to one of the proxy: the old blend strayed by nearly half a
            // proxy pixel, and the cut stays within a quarter of one.
            assert!(
                stairs > 3.0,
                "the old blend drew stairs (edge-aware {edge_aware}): {stairs}"
            );
            assert!(
                straight < 2.0,
                "the line strays {straight} px from straight (edge-aware {edge_aware})"
            );
        }
    }

    #[test]
    fn the_histogram_is_binned_over_the_ruler_not_the_data() {
        // The ghost is drawn behind the ruler and has to line up with it. Binning
        // over the data's own range would slide the distribution around under the
        // trapezoid every time the exposure moved, which is precisely the reading
        // it exists to give.
        let l = luma(16, 16, vec![MID_GREY; 256]);
        let b = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        let hist = b.histogram(10);
        assert_eq!(hist.len(), 10);
        assert_eq!(
            hist[5], 1.0,
            "0 EV falls in the middle bin of a -5..+5 ruler"
        );
        // The smoothing kernel spreads a spike into its two neighbours and no
        // further, so a flat 0.18 image lights bins 4, 5 and 6 and nothing else.
        // Asserted as a *shape* rather than as "everything else is zero", which is
        // what this test claimed before there was any smoothing.
        assert!(
            hist[4] > 0.0 && hist[4] < hist[5],
            "shoulder below: {:?}",
            &hist[3..8]
        );
        assert_eq!(hist[4], hist[6], "and symmetric");
        assert!(
            hist.iter()
                .enumerate()
                .all(|(i, &v)| (4..=6).contains(&i) || v == 0.0)
        );
    }

    #[test]
    fn the_contrast_mask_reaches_the_basis() {
        // Not a numerical claim — just that the module upstream is actually in the
        // signal the zones are read from. It is switched off by default, so a wire
        // that was never connected would look identical in every other test here.
        let (w, h) = (64, 64);
        let data: Vec<f32> = (0..w * h)
            .map(|i| {
                if i % w < w / 2 {
                    MID_GREY / 8.0
                } else {
                    MID_GREY * 8.0
                }
            })
            .collect();
        let l = luma(w, h, data);
        let off = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams::default(),
        );
        let on = Basis::build(
            &l,
            &ExposureParams::default(),
            &ContrastMaskParams {
                enabled: true,
                ..Default::default()
            },
        );
        assert_ne!(off.ev, on.ev);
        // And it compresses: the range between the two halves narrows.
        let range = |b: &Basis| b.ev[32 * b.w + b.w - 4] - b.ev[32 * b.w + 4];
        assert!(
            range(&on) < range(&off),
            "{} vs {}",
            range(&on),
            range(&off)
        );
    }
}
