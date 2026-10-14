//! Tonal-distribution histogram.
//!
//! Tonal bins come from the finished-image GPU proxy. CFA RGB and curve-input
//! distributions remain intermediate diagnostics.
//!
//! # Subsampling must not align with the CFA lattice
//!
//! Both passes subsample to a fixed budget so a slider drag stays cheap. That is
//! where the interesting bug lives, and it is worth stating plainly because the
//! naive version looks obviously correct:
//!
//! A flat `step_by(stride)` over the mosaic **cannot reach half the CFA positions**.
//! `CfaGeometry` snaps the crop to an even width — the invariant that keeps every
//! colour lookup correct everywhere else — so `i % width` has the same parity as
//! `i`. With an even stride, `i` is always even, so the sampled column is always
//! even, and only two of the four Bayer positions are ever visited. On the Leica
//! (`G B / R G`) that means **blue is never sampled at all** and its curve is a flat
//! line at zero. Red and green look plausible, which is what makes it survive a
//! glance.
//!
//! Two fixes, one per pass:
//!
//! - The **RGB** pass walks 2x2 quads and reads all four photosites of each visited
//!   quad. Provably complete rather than probabilistically fine, and it preserves
//!   true photosite density (one R, two G, one B per quad).
//! - The **tonal** pass uses an odd stride, which makes the sampled column parity
//!   alternate every step. It matters because in DirectMosaic the working image is
//!   still on the CFA lattice — under Green weighting the non-green photosites are
//!   zero, so an even stride would sample a wildly wrong distribution.

use raw_core::{Params, SceneImage};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const BINS: usize = raw_gpu::HISTOGRAM_BINS;

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const MIN_COOLDOWN: Duration = Duration::from_millis(50);
const MAX_COOLDOWN: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct GpuKey {
    params: Arc<Params>,
    frame: raw_core::Frame,
    luma_gen: u64,
}

impl PartialEq for GpuKey {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (&self.params, &other.params);
        self.frame == other.frame
            && self.luma_gen == other.luma_gen
            && a.exposure == b.exposure
            && a.contrast_mask == b.contrast_mask
            && a.dodgeburn == b.dodgeburn
            && a.curve.same_render(&b.curve)
            && a.display.tone_map == b.display.tone_map
            && a.display.gamma == b.display.gamma
            && a.toning == b.toning
        // Dither and output-only settings never enter these bins. Decode and
        // luminance changes arrive as a new prepared-source generation.
    }
}

struct PendingGpuHistogram {
    key: GpuKey,
    readback: raw_gpu::PendingHistogram,
    started: Instant,
}

#[derive(Default)]
pub struct GpuHistogram {
    wanted: Option<GpuKey>,
    ready: Option<(GpuKey, [u32; BINS])>,
    pending: Option<PendingGpuHistogram>,
    next_submit: Option<Instant>,
}

fn cooldown(elapsed: Duration) -> Duration {
    elapsed.saturating_mul(2).clamp(MIN_COOLDOWN, MAX_COOLDOWN)
}

impl GpuHistogram {
    pub fn prepare(&mut self, params: Arc<Params>, frame: raw_core::Frame, luma_gen: u64) {
        let wanted = GpuKey {
            params,
            frame,
            luma_gen,
        };
        if self.wanted.as_ref() != Some(&wanted) {
            self.wanted = Some(wanted);
            self.ready = None;
        }
    }

    pub fn update(
        &mut self,
        viewport: &mut raw_gpu::Viewport,
        gpu: &mut raw_gpu::GpuContext,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<Duration> {
        let now = Instant::now();
        if let Some(pending) = &mut self.pending
            && let Some(result) = pending.readback.poll(device)
        {
            let pending = self
                .pending
                .take()
                .expect("the completed histogram is pending");
            self.next_submit = Some(now + cooldown(pending.started.elapsed()));
            if let Ok(bins) = result {
                self.store(&pending.key, bins);
            }
        }

        if self.pending.is_some() {
            return Some(POLL_INTERVAL);
        }
        let wanted = self.wanted.clone()?;
        if self.ready.as_ref().is_some_and(|(key, _)| key == &wanted) {
            return None;
        }
        if let Some(next) = self.next_submit
            && let Some(wait) = next.checked_duration_since(now)
        {
            return Some(wait);
        }
        if let Some(readback) =
            viewport.begin_histogram(gpu, device, queue, &wanted.params, &wanted.frame)
        {
            self.pending = Some(PendingGpuHistogram {
                key: wanted,
                readback,
                started: now,
            });
            Some(POLL_INTERVAL)
        } else {
            self.next_submit = Some(now + MAX_COOLDOWN);
            Some(MAX_COOLDOWN)
        }
    }

    pub fn bins(&self) -> Option<&[u32; BINS]> {
        self.ready
            .as_ref()
            .and_then(|(key, bins)| (self.wanted.as_ref() == Some(key)).then_some(bins))
    }

    fn store(&mut self, key: &GpuKey, bins: [u32; BINS]) -> bool {
        if self.wanted.as_ref() != Some(key) {
            return false;
        }
        self.ready = Some((key.clone(), bins));
        true
    }
}

/// Samples per pass. Enough for a smooth 128-bin curve, small enough to recompute
/// every frame of a drag.
const BUDGET: usize = 200_000;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Finished display luminance before dither and output-only processing.
    Tonal,
    /// The three CFA channels from the scene, display-mapped. Reveals how far the
    /// subject is from neutral (equal R=G=B) at each tone.
    Rgb,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tonal => "tonal",
            Self::Rgb => "CFA RGB",
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Tonal => Self::Rgb,
            Self::Rgb => Self::Tonal,
        }
    }
}

#[derive(Clone)]
struct MappingKey {
    ev: f32,
    black: f32,
    curve: raw_core::CurveStack,
    tone_map: raw_core::ToneMap,
    gamma: f32,
}
impl MappingKey {
    fn of(p: &Params) -> Self {
        Self {
            ev: p.exposure.ev,
            black: p.exposure.black,
            curve: p.curve.clone(),
            tone_map: p.display.tone_map,
            gamma: p.display.gamma,
        }
    }
}
impl PartialEq for MappingKey {
    fn eq(&self, other: &Self) -> bool {
        self.ev == other.ev
            && self.black == other.black
            // `same_luts`, not `same_render`: this mapping reads the curve through its
            // table, which ignores where a mask lies, so dragging a mask's handle must
            // not read as a new mapping and re-bin every histogram behind it.
            && self.curve.same_luts(&other.curve)
            && self.tone_map == other.tone_map
            && self.gamma == other.gamma
    }
}

/// What the curve editor draws behind the selected pass, and says beneath it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveGhost {
    /// The tones entering the pass, on the curve's own axis. **Inside the pass's mask**
    /// when it has one that is on: each sample counts by how much of the pass reaches
    /// it, so the ghost is the tones the curve touches and not the whole frame's.
    pub bins: [u32; BINS],
    /// The mean EV change the pass makes, opacity included, over those same tones.
    /// `None` when there is no tone with a finite EV to average, or a mask selects
    /// nothing.
    pub average: Option<f32>,
    /// How much of the frame the mask selects, as a fraction of it, when there is one.
    pub coverage: Option<f32>,
    /// Where on the curve's axis the masked tones lie: the span holding the middle nine
    /// tenths of what the mask selects, as positions in `[0, 1]`. For shading the axis.
    pub span: Option<(f32, f32)>,
}

impl CurveGhost {
    /// The line under the curve editor: the mean change the pass makes, and, for a
    /// masked pass, that it is the change inside the mask and how much of the frame the
    /// mask selects.
    pub fn readout(&self) -> String {
        let change = match self.average {
            Some(delta) if delta.abs() < 0.005 => "unchanged".to_owned(),
            Some(delta) => format!(
                "{:.2} EV {}",
                delta.abs(),
                if delta > 0.0 { "lighter" } else { "darker" },
            ),
            None => "—".to_owned(),
        };
        match self.coverage {
            None => format!("Average: {change}"),
            Some(share) => {
                let percent = share * 100.0;
                // A mask that selects a sliver must not read as selecting nothing.
                let of_frame = if percent > 0.0 && percent < 1.0 {
                    "<1%".to_owned()
                } else {
                    format!("{percent:.0}%")
                };
                format!("Average: {change} inside the mask ({of_frame} of the frame)")
            }
        }
    }
}

/// The tones entering one pass, sample by sample: the half of the ghost that costs a
/// curve evaluation per sample, kept apart from the mask so a mask that moves does not
/// pay for it again.
struct Entering {
    /// The passes up to the selected one, with the selected one's own mask taken off:
    /// what the tone entering it, and the change it makes at full weight, depend on.
    upstream: raw_core::CurveStack,
    stop: usize,
    aspect: f32,
    /// The basis the passes above were weighed by, as its address; zero for none.
    basis: usize,
    bins: [u32; BINS],
    average: Option<f32>,
    /// Per sample: the bin its entering tone falls in, and the EV change the pass makes
    /// to it at full weight (not finite where the tone has no EV).
    bin: Vec<u8>,
    delta: Vec<f32>,
    /// Counts the times this was rebuilt, so what is made from it knows when it is stale.
    serial: u64,
}

/// The ghost inside the selected pass's mask, and what it was made from.
struct Masked {
    serial: u64,
    mask: raw_core::curve::CurveMask,
    aspect: f32,
    basis: usize,
    ghost: CurveGhost,
}

/// How many samples the masked ghost weighs. A mask is weighed again on every frame of
/// a handle drag, and a radial's weight is a sine and a cosine, so this is a quarter of
/// the samples the ghost is binned from: enough for a distribution, in about a
/// millisecond.
const MASK_BUDGET: usize = 50_000;

pub struct Histogram {
    pub mode: Mode,
    pub tonal: [u32; BINS],
    /// The same pixels binned in **curve space** — position in the curve's own
    /// log2-EV window — for the ghost behind the curve editor.
    ///
    /// A separate array rather than a reuse of `tonal`, and the distinction is not
    /// pedantic: `tonal` is display-encoded, the curve's horizontal axis is log2 EV,
    /// and drawing one under the other would put the mass in visibly the wrong
    /// place. A histogram you place a control point against has to be binned on the
    /// axis that control point moves along, or it misinforms every decision made
    /// from it.
    pub curve_in: [u32; BINS],
    // Keep unquantized post-exposure samples: remapping whole histogram bins
    // through a contrast-expanding curve leaves artificial holes.
    curve_samples: Vec<f32>,
    /// Where each of those samples is on the negative, source-normalized, in step with
    /// `curve_samples`: what lets the ghost be taken inside a mask.
    curve_at: Vec<(f32, f32)>,
    curve_cache: Option<Entering>,
    curve_masked: Option<Masked>,
    /// The selected pass's tone range, prepared for the basis at this address.
    curve_tone: Option<(raw_core::ZoneMask, usize, raw_core::zone::PreparedMask)>,
    /// What each pass's painted mask covers, as a small picture, by the pass's place in
    /// the stack; `None` for a pass whose mask is not painted. Kept across frames and
    /// brought up to date with only the dabs that are new, because weighing a sample
    /// against the strokes themselves is every dab for every sample.
    curve_cover: Vec<Option<raw_core::dodgeburn::Coverage>>,
    curve_serial: u64,
    pub rgb: [[u32; BINS]; 3],
    sample_key: Option<(f32, f32, u64, Option<raw_core::Frame>)>,
    rgb_key: Option<(MappingKey, u64)>,
    map_key: Option<MappingKey>,
    #[cfg(test)]
    key: Option<(MappingKey, u64, raw_core::Frame)>,
    /// Shared CPU mapping for the CFA diagnostic, sampling references and visual
    /// centering. Finished tonal bins come only from the GPU. The LUT survives
    /// exposure/display changes and is baked only when the curve itself changes.
    map: Option<Mapping>,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            mode: Mode::Tonal,
            tonal: [0; BINS],
            curve_in: [0; BINS],
            curve_samples: Vec::new(),
            curve_at: Vec::new(),
            curve_cache: None,
            curve_masked: None,
            curve_tone: None,
            curve_cover: Vec::new(),
            curve_serial: 0,
            rgb: [[0; BINS]; 3],
            sample_key: None,
            rgb_key: None,
            map_key: None,
            #[cfg(test)]
            key: None,
            map: None,
        }
    }
}

impl Histogram {
    /// Refresh the inexpensive tone-chain description used by readouts and
    /// visual centering. Bake a LUT only when the rendered curve changes.
    pub fn refresh_mapping(&mut self, params: &Params) {
        let key = MappingKey::of(params);
        if self.map_key.as_ref() == Some(&key) {
            return;
        }
        if let Some(map) = &mut self.map {
            // Mask-blind, for the reason `MappingKey` is: the table does not depend on it.
            if !map.curve.same_luts(&params.curve) {
                map.lut = params.curve.bake();
                map.identity = params.curve.is_identity();
                map.curve = params.curve.clone();
            }
            map.ev = params.exposure.ev;
            map.black = params.exposure.black;
            map.display = params.display;
        } else {
            self.map = Some(Mapping::new(params));
        }
        self.map_key = Some(key);
    }

    /// Called by the visible CFA RGB panel. Crop and luminance reconstruction do
    /// not affect this source-space diagnostic; decode generation does.
    pub fn refresh_rgb(&mut self, scene: &SceneImage, params: &Params, scene_gen: u64) {
        let key = (MappingKey::of(params), scene_gen);
        if self.rgb_key.as_ref() == Some(&key) {
            return;
        }
        self.refresh_mapping(params);
        self.rgb = rgb_bins(scene, self.map.as_ref().expect("mapping prepared"));
        self.rgb_key = Some(key);
    }

    /// Called only when the curve editor needs its input distribution. These
    /// samples precede the curve and display transform, so neither invalidates it.
    pub fn refresh_curve_samples(
        &mut self,
        luma: &raw_core::LumaImage,
        params: &Params,
        luma_gen: u64,
        frame: &raw_core::Frame,
    ) {
        let key = (
            params.exposure.ev,
            params.exposure.black,
            luma_gen,
            (!frame.is_uncropped()).then_some(*frame),
        );
        if self.sample_key == Some(key) {
            return;
        }
        self.curve_samples.clear();
        self.curve_at.clear();
        self.curve_in = [0; BINS];
        let gain = params.exposure.ev.exp2();
        for_each_tonal_sample(luma, frame, |v, x, y| {
            let scene = (v - params.exposure.black) * gain;
            self.curve_samples.push(scene);
            self.curve_at.push((x, y));
            let bin = ((raw_core::Curve::to_normalized(scene) * (BINS - 1) as f32) as usize)
                .min(BINS - 1);
            self.curve_in[bin] += 1;
        });
        self.curve_cache = None;
        self.curve_masked = None;
        self.sample_key = Some(key);
    }

    // Legacy CPU reference used by sampling tests. Production tonal bins are GPU
    // results and must never be replaced with this pre-spatial estimate.
    #[cfg(test)]
    pub fn refresh(
        &mut self,
        luma: &raw_core::LumaImage,
        scene: &SceneImage,
        params: &Params,
        luma_gen: u64,
        frame: &raw_core::Frame,
    ) {
        let key = (MappingKey::of(params), luma_gen, *frame);
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.refresh_mapping(params);
        self.refresh_curve_samples(luma, params, luma_gen, frame);
        self.refresh_rgb(scene, params, luma_gen);
        self.tonal = tonal_bins(luma, frame, self.map.as_ref().unwrap()).0;
        self.key = Some(key);
    }

    pub fn accept_finished(&mut self, bins: [u32; BINS]) {
        self.tonal = bins;
    }

    /// The ghost behind the curve editor for pass `stop`, and its Average: the tones
    /// entering the pass, binned sample by sample before quantization, **inside the
    /// pass's mask** when it has one that is on.
    ///
    /// `aspect` is the stored image's height over width, which the shapes are measured
    /// in. `basis` is the picture's pre-Dodge & Burn basis, which a tone range is read
    /// from; without it a tone range weighs one, so the caller passes it whenever a
    /// pass up to `stop` has one and draws again when it arrives.
    ///
    /// # Two caches, because two things move
    ///
    /// - **The tones entering the pass** take a curve evaluation per sample, for every
    ///   pass above. They depend on those curves and on where *their* masks lie, and
    ///   are rebuilt only when one of those changes: the passes above are weighed at
    ///   each sample's own place on the negative, as the shader weighs them.
    /// - **The selected pass's mask** is weighed over them, which is arithmetic. It is
    ///   redone on every frame of a handle drag and never re-runs a curve.
    ///
    /// # A painted mask is weighed from a picture of it
    ///
    /// A gradient's weight at a sample is a few multiplications. A painted mask's is a
    /// test of every dab, and a mask that has been worked on has thousands, so asking
    /// the strokes themselves for fifty thousand samples on every frame of the next
    /// stroke would stop the brush. Each painted mask up to `stop` is kept as a
    /// [`Coverage`](raw_core::dodgeburn::Coverage), which a stroke in progress updates
    /// with its new dabs alone, and a sample reads it with one lookup.
    pub fn curve_ghost(
        &mut self,
        stack: &raw_core::CurveStack,
        stop: usize,
        aspect: f32,
        basis: Option<&Arc<raw_core::Basis>>,
    ) -> CurveGhost {
        let reads_picture = |instance: &raw_core::CurveInstance| {
            instance
                .live_mask()
                .and_then(|mask| mask.live_tone())
                .is_some_and(|tone| !tone.is_identity())
        };
        let address = |needed: bool| match basis {
            Some(basis) if needed => Arc::as_ptr(basis) as usize,
            _ => 0,
        };

        // ── What has been painted, for the passes that are painted ───────────
        if self.curve_cover.len() < stack.instances.len() {
            self.curve_cover.resize_with(stack.instances.len(), || None);
        }
        for (i, slot) in self.curve_cover.iter_mut().enumerate() {
            let painted = stack
                .instances
                .get(i)
                .and_then(|instance| instance.live_mask())
                .and_then(|mask| mask.shape.as_ref())
                .filter(|shape| shape.nib().is_some());
            match painted {
                // Brought up to date only for the passes this ghost reads. One below the
                // selection keeps what it had, and catches up if it is ever selected.
                Some(shape) if i <= stop => {
                    if slot.as_ref().is_none_or(|cover| cover.aspect() != aspect) {
                        *slot = Some(raw_core::dodgeburn::Coverage::new(aspect));
                    }
                    if let Some(cover) = slot {
                        cover.update(shape.passes());
                    }
                }
                Some(_) => {}
                None => *slot = None,
            }
        }
        // How much of pass `pass` reaches a sample: its shape from the picture when it
        // is painted and from the shape itself otherwise, joined with its tone.
        let covers = &self.curve_cover;
        let weight = |pass: usize,
                      mask: &raw_core::curve::CurveMask,
                      (x, y): (f32, f32),
                      tone: &dyn Fn(&raw_core::ZoneMask) -> f32| {
            let covered = match covers.get(pass).and_then(Option::as_ref) {
                Some(cover) => Some(cover.at(x, y)),
                None => mask.shape_coverage(x, y, aspect),
            };
            mask.weigh(covered, mask.live_tone().map(tone))
        };

        // ── The tones entering the pass ──────────────────────────────────────
        let mut upstream = stack.clone();
        upstream.instances.truncate(stop.saturating_add(1));
        if let Some(selected) = upstream.instances.get_mut(stop) {
            selected.mask = None;
        }
        let masked_above = stack
            .instances
            .iter()
            .take(stop)
            .any(|instance| instance.live_mask().is_some());
        let above_basis = address(stack.instances.iter().take(stop).any(reads_picture));
        let stale = self.curve_cache.as_ref().is_none_or(|entry| {
            entry.stop != stop
                || entry.aspect != aspect
                || entry.basis != above_basis
                || !entry.upstream.same_render(&upstream)
        });
        if stale {
            // Each tone range above, prepared once and read at every sample.
            let prepared: Vec<(raw_core::ZoneMask, raw_core::zone::PreparedMask)> =
                match basis.filter(|_| above_basis != 0) {
                    Some(basis) => {
                        let mut tones: Vec<raw_core::ZoneMask> = Vec::new();
                        for instance in stack.instances.iter().take(stop) {
                            if let Some(tone) = instance.live_mask().and_then(|m| m.live_tone())
                                && !tones.contains(tone)
                            {
                                tones.push(*tone);
                            }
                        }
                        tones.into_iter().map(|t| (t, basis.prepare(&t))).collect()
                    }
                    None => Vec::new(),
                };
            // The selected pass alone and unmasked: the change it makes at full weight.
            let solo = upstream.instances.get(stop).map(|selected| raw_core::CurveStack {
                enabled: true,
                instances: vec![selected.clone()],
            });
            // **Each pass's spline is derived here, once**, and not once a sample: asked
            // of the stack tone by tone, this loop was about 24 ms a frame at two hundred
            // thousand samples, every frame a point or an opacity was dragged.
            let above = stack.passes_before(stop);
            let selected = solo.as_ref().map(|solo| solo.passes_before(1));
            let mut bins = [0; BINS];
            let mut bin = Vec::with_capacity(self.curve_samples.len());
            let mut delta = Vec::with_capacity(self.curve_samples.len());
            let mut total_ev = 0.0f64;
            let mut count = 0usize;
            // What one sample adds, given the tone entering the pass there.
            let mut take = |entering: f32| {
                let at = ((raw_core::Curve::to_normalized(entering) * (BINS - 1) as f32) as usize)
                    .min(BINS - 1);
                bins[at] += 1;
                bin.push(at as u8);
                // A bypassed module changes nothing, and still has tones to average over.
                let change = match &selected {
                    Some(selected) if stack.enabled => selected.change_ev(entering),
                    _ => (entering > 0.0 && entering.is_finite()).then_some(0.0),
                };
                match change {
                    Some(change) => {
                        total_ev += f64::from(change);
                        count += 1;
                        delta.push(change);
                    }
                    None => delta.push(f32::NAN),
                }
            };
            // **Two loops and not one with a branch in it.** The unmasked path is the
            // one it always was, so a stack with no mask above the pass bins exactly as
            // it did before masks existed; and it is the common one, which measured
            // nearly twice as slow with the masked case's calls sharing its loop.
            if masked_above {
                for (i, &scene) in self.curve_samples.iter().enumerate() {
                    let at = self.curve_at.get(i).copied().unwrap_or((0.5, 0.5));
                    take(above.apply_weighted(scene, &|pass, mask| {
                        weight(pass, mask, at, &|tone| {
                            prepared
                                .iter()
                                .find(|(range, _)| range == tone)
                                .map_or(1.0, |(_, mask)| mask.at(at.0, at.1))
                        })
                    }));
                }
            } else {
                for &scene in &self.curve_samples {
                    take(above.apply(scene));
                }
            }
            self.curve_serial += 1;
            self.curve_cache = Some(Entering {
                upstream,
                stop,
                aspect,
                basis: above_basis,
                bins,
                average: (count > 0).then(|| (total_ev / count as f64) as f32),
                bin,
                delta,
                serial: self.curve_serial,
            });
        }
        let entering = self.curve_cache.as_ref().expect("just ensured");

        // ── Inside the selected pass's mask ──────────────────────────────────
        let Some(mask) = stack.instances.get(stop).and_then(|i| i.live_mask()) else {
            return CurveGhost {
                bins: entering.bins,
                average: entering.average,
                coverage: None,
                span: None,
            };
        };
        let tone = mask.live_tone().filter(|tone| !tone.is_identity());
        let mask_basis = address(tone.is_some());
        if let Some(made) = &self.curve_masked
            && made.serial == entering.serial
            && made.aspect == aspect
            && made.basis == mask_basis
            && made.mask == *mask
        {
            return made.ghost;
        }
        // The tone range's mask, prepared when the range or the basis changed and not
        // when the shape beside it moved.
        if let (Some(tone), Some(basis)) = (tone, basis) {
            let current = self
                .curve_tone
                .as_ref()
                .is_some_and(|(range, at, _)| range == tone && *at == mask_basis);
            if !current {
                self.curve_tone = Some((*tone, mask_basis, basis.prepare(tone)));
            }
        }
        let prepared = self
            .curve_tone
            .as_ref()
            .filter(|(range, at, _)| Some(range) == tone && *at == mask_basis && mask_basis != 0)
            .map(|(_, _, mask)| mask);

        let n = entering.bin.len();
        let mut weighed = [0.0f64; BINS];
        let (mut selected, mut counted) = (0.0f64, 0usize);
        let (mut change, mut change_weight) = (0.0f64, 0.0f64);
        for i in (0..n).step_by(odd_stride(n, MASK_BUDGET)) {
            let at = self.curve_at.get(i).copied().unwrap_or((0.5, 0.5));
            let m = f64::from(weight(stop, mask, at, &|_| {
                prepared.map_or(1.0, |tone| tone.at(at.0, at.1))
            }));
            weighed[usize::from(entering.bin[i])] += m;
            selected += m;
            counted += 1;
            if entering.delta[i].is_finite() {
                change += m * f64::from(entering.delta[i]);
                change_weight += m;
            }
        }
        // Counts again, in 256ths of a sample, which is finer than the square root the
        // editor draws them through can show.
        let mut bins = [0u32; BINS];
        for (out, weight) in bins.iter_mut().zip(weighed) {
            *out = (weight * 256.0).round() as u32;
        }
        // The middle nine tenths of what the mask selects, as a span of the axis.
        let span = (selected > 1.0e-6).then(|| {
            let cut = |share: f64| {
                let mut run = 0.0;
                weighed
                    .iter()
                    .position(|weight| {
                        run += weight;
                        run >= selected * share
                    })
                    .unwrap_or(BINS - 1)
            };
            let x = |bin: usize| bin as f32 / (BINS - 1) as f32;
            (x(cut(0.05)), x(cut(0.95)))
        });
        let ghost = CurveGhost {
            bins,
            average: (change_weight > 1.0e-6).then(|| (change / change_weight) as f32),
            coverage: (counted > 0).then(|| (selected / counted as f64) as f32),
            span,
        };
        self.curve_masked = Some(Masked {
            serial: entering.serial,
            mask: mask.clone(),
            aspect,
            basis: mask_basis,
            ghost,
        });
        ghost
    }

    /// The ghost's bins for a stack whose masks read nothing from the picture. What the
    /// tests of the binning itself call.
    #[cfg(test)]
    pub fn curve_input(&mut self, stack: &raw_core::CurveStack, stop: usize) -> [u32; BINS] {
        self.curve_ghost(stack, stop, 1.0, None).bins
    }

    /// Mean per-pixel EV change of the pass most recently asked about, before its mask.
    /// Nonpositive samples have no finite EV and are excluded.
    #[cfg(test)]
    pub fn curve_average_ev(&self) -> Option<f32> {
        self.curve_cache.as_ref().and_then(|entry| entry.average)
    }

    /// CPU tone-chain estimate `(L*, EV)` from working luminance. This remains for
    /// non-readout calculations that deliberately stop before spatial processing.
    pub fn sample(&self, raw: f32) -> Option<(f32, f32)> {
        let m = self.map.as_ref()?;
        let scene = m.scene(raw);
        let lstar =
            raw_core::display::lstar_encode(raw_core::display::tone_map(scene, m.display.tone_map));
        Some((lstar * 100.0, scene.max(1.0e-6).log2()))
    }

    /// The same photosite **before any development**, as `L*` on 0–100.
    ///
    /// This is what the negative would print as with nothing done to it: no
    /// exposure, no black or white point, no curve, no tone map — the working
    /// luminance encoded straight to `L*`.
    ///
    /// # Why this exists rather than the EV it replaced
    ///
    /// The Inspector's `RAW` used to report the scene value in **stops relative to
    /// clipping**, which is a true number and the wrong one to put beside `L*`.
    /// the maintainer: "The EV values are just confusing to read... I'm not sure how that is
    /// helping me determine anything." The fault is that the two stages were then in
    /// **different units**, so the one question the toggle exists to answer — *what
    /// did my developing do to this tone* — could not be answered by reading the two
    /// numbers, because 55.2 and −4.47 EV do not subtract.
    ///
    /// In the same units they do. `RAW 21.6 → EDITED 55.2` is a shadow lifted
    /// thirty-four points of lightness, which is a sentence about a print.
    ///
    /// This is also the prototype's behaviour, read from `ColorSample.values`: it
    /// holds a raw and an edited linear triple and puts **both through the same
    /// conversion**, so its toggle changes the stage and never the space.
    ///
    /// Clamped at 1.0 before encoding: scene-linear values run above white — that is
    /// what headroom is — and `L*` above 100 is not a lightness. The EV readout in
    /// the footer is what answers *how far* above.
    pub fn sample_undeveloped(&self, raw: f32) -> Option<f32> {
        self.map.as_ref()?;
        Some(raw_core::display::lstar_encode(raw.clamp(0.0, 1.0)) * 100.0)
    }

    /// Fraction of mapped histogram samples at each end: `(shadow, highlight)`.
    pub fn clipped(&self) -> (f32, f32) {
        let total: u32 = self.tonal.iter().sum();
        if total == 0 {
            return (0.0, 0.0);
        }
        let t = total as f32;
        (self.tonal[0] as f32 / t, self.tonal[BINS - 1] as f32 / t)
    }
}

/// The display chain the shader runs, minus dither, as a scene value -> bin index.
struct Mapping {
    ev: f32,
    black: f32,
    lut: Vec<f32>,
    identity: bool,
    curve: raw_core::CurveStack,
    display: raw_core::DisplayParams,
}

impl Mapping {
    fn new(p: &Params) -> Self {
        Self {
            ev: p.exposure.ev,
            black: p.exposure.black,
            lut: p.curve.bake(),
            identity: p.curve.is_identity(),
            curve: p.curve.clone(),
            display: p.display,
        }
    }

    /// A photosite value through exposure and the curve: scene-referred in,
    /// scene-referred out, stopping short of the display transform.
    ///
    /// Split out of `bin` so the footer can take the same value and encode it for
    /// print (L\*) where the histogram encodes it for screen (gamma). One chain,
    /// two endings — which is the same split `raw_core::display` documents between
    /// the viewport and export.
    fn scene(&self, v: f32) -> f32 {
        let scene = (v - self.black) * self.ev.exp2();
        if self.identity {
            scene
        } else {
            self.curve.apply_linear(&self.lut, scene)
        }
    }

    fn bin(&self, v: f32) -> usize {
        let enc = raw_core::display::encode(self.scene(v), &self.display);
        ((enc * (BINS as f32 - 1.0)) as usize).min(BINS - 1)
    }

    /// Where a photosite value falls on the **curve editor's** horizontal axis.
    ///
    /// Post-exposure and pre-curve, because that is what the curve is a function
    /// *of*: binning its own output would draw a histogram that moves every time
    /// you drag a point, which tells you nothing about where the tones you are
    /// trying to move actually are.
    #[cfg(test)]
    fn curve_bin(&self, v: f32) -> usize {
        use raw_core::curve::{HI_EV, LO_EV};
        let scene = (v - self.black) * self.ev.exp2();
        if scene <= 0.0 {
            return 0;
        }
        let x = ((scene.log2() - LO_EV) / (HI_EV - LO_EV)).clamp(0.0, 1.0);
        ((x * (BINS as f32 - 1.0)) as usize).min(BINS - 1)
    }
}

/// Subsample stride, forced odd.
///
/// Odd is the point: with an even crop width, an even stride locks the sampled
/// column to one parity and silently halves the CFA lattice. See the module docs.
fn odd_stride(len: usize, budget: usize) -> usize {
    let s = (len / budget).max(1);
    if s.is_multiple_of(2) { s + 1 } else { s }
}

/// Both tonal binnings in one pass: display-encoded for the histogram, curve-space
/// for the ghost behind the curve editor. Same pixels, two axes.
///
/// # The histogram describes the crop
///
/// the maintainer's decision, and the reasoning is that this panel answers "what is on
/// screen": after a crop, what is on screen is the crop, and cropping a blown sky
/// out of a frame should visibly drop the highlight end. The sample region follows
/// the crop, but these bins need not match spatially processed clipping overlays.
///
/// Two walks, and the fast one is the common one. **Uncropped, this is the flat
/// stride over the stored buffer it has always been** — bit for bit, because an
/// orientation is a permutation of the pixels and a permutation cannot change a
/// histogram. Only a crop changes the *set*, and only then does it walk the frame
/// and map each sample back through the composition.
#[cfg(test)]
fn tonal_bins(
    luma: &raw_core::LumaImage,
    frame: &raw_core::Frame,
    map: &Mapping,
) -> ([u32; BINS], [u32; BINS]) {
    let mut bins = [0u32; BINS];
    let mut curve = [0u32; BINS];
    for_each_tonal_sample(luma, frame, |v, _, _| {
        bins[map.bin(v)] += 1;
        curve[map.curve_bin(v)] += 1;
    });
    (bins, curve)
}

/// Suggest an AgX input window from the tones entering the display transform.
///
/// The percentiles deliberately ignore the outer 0.1% before adding half a stop of
/// breathing room. One dead pixel should not spend several stops of the curve, but
/// a small bright subject still should. The sampled set is exactly the histogram's
/// crop-aware set, and `Mapping::scene` means Exposure and Curve are included while
/// AgX itself is not — Auto Range measures the signal the control is about to map.
pub fn agx_auto_range(
    luma: &raw_core::LumaImage,
    frame: &raw_core::Frame,
    params: &Params,
) -> Option<(f32, f32)> {
    let map = Mapping::new(params);
    let grey_ev = 0.18f32.log2();
    let mut ev = Vec::with_capacity(BUDGET.min(luma.data.len()));
    for_each_tonal_sample(luma, frame, |v, _, _| {
        let scene = map.scene(v);
        if scene.is_finite() && scene > 1.0e-10 {
            ev.push(scene.log2() - grey_ev);
        }
    });
    if ev.is_empty() {
        return None;
    }
    ev.sort_unstable_by(f32::total_cmp);
    let last = ev.len() - 1;
    let at = |q: f32| ev[((last as f32 * q).round() as usize).min(last)];
    let black = (at(0.001) - 0.5).clamp(-16.0, -1.0);
    let white = (at(0.999) + 0.5).clamp(1.0, 12.0);
    Some((black, white))
}

/// Walk the frame's samples: each value, and where it is on the negative as a
/// source-normalized pixel center, which is what a mask is laid on.
fn for_each_tonal_sample(
    luma: &raw_core::LumaImage,
    frame: &raw_core::Frame,
    mut add: impl FnMut(f32, f32, f32),
) {
    let (w, h) = (luma.output_dims.w.max(1), luma.output_dims.h.max(1));
    if frame.is_uncropped() {
        let step = odd_stride(luma.data.len(), BUDGET);
        for (i, v) in luma.data.iter().enumerate().step_by(step) {
            add(
                *v,
                ((i % w) as f32 + 0.5) / w as f32,
                ((i / w) as f32 + 0.5) / h as f32,
            );
        }
        return;
    }

    let c = frame.crop;
    let (lw, lh) = (luma.output_dims.w as i32, luma.output_dims.h as i32);
    // A stride per axis rather than one over a flattened length, because the crop is
    // a rectangle on a grid and walking it flat would sample a diagonal. Odd for the
    // reason the flat walk's is: in DirectMosaic the working image is still a mosaic,
    // and an even column stride would sample one Bayer phase forever.
    let area = (c.w as usize).saturating_mul(c.h as usize);
    let step = odd_stride(area, BUDGET).isqrt().max(1) | 1;
    for fy in (0..c.h as i32).step_by(step) {
        for fx in (0..c.w as i32).step_by(step) {
            // Frame -> source, through the same affine the shader applies. Sample
            // centres, so a quarter turn lands on the pixel it names rather than
            // between two of them.
            let (sx, sy) = frame.to_source((c.x + fx) as f32 + 0.5, (c.y + fy) as f32 + 0.5);
            let (x, y) = (sx.floor() as i32, sy.floor() as i32);
            // A straighten leaves corners with nothing behind them. They are drawn
            // as surround, so they are not part of what is on screen and must not be
            // counted — otherwise straightening a frame puts a spike at whatever the
            // clamped edge pixel happens to be.
            if x < 0 || y < 0 || x >= lw || y >= lh {
                continue;
            }
            add(
                luma.data[y as usize * lw as usize + x as usize],
                (x as f32 + 0.5) / w as f32,
                (y as f32 + 0.5) / h as f32,
            );
        }
    }
}

/// Bucket the mosaic by CFA colour, walking 2x2 quads.
///
/// Every visited quad contributes all four of its photosites — one red, two green,
/// one blue — so no colour can be missed however the stride and the width happen to
/// line up, and the relative counts reflect real photosite density.
fn rgb_bins(scene: &SceneImage, map: &Mapping) -> [[u32; BINS]; 3] {
    let mut bins = [[0u32; BINS]; 3];
    let w = scene.geom.crop.w;
    let (qw, qh) = (w / 2, scene.geom.crop.h / 2);
    if qw == 0 || qh == 0 {
        return bins;
    }
    // Four samples per quad, so aim at a quarter of the budget in quads.
    let step = ((qw * qh) / (BUDGET / 4)).max(1);

    for q in (0..qw * qh).step_by(step) {
        let (qy, qx) = (q / qw, q % qw);
        for dy in 0..2 {
            for dx in 0..2 {
                let (row, col) = (qy * 2 + dy, qx * 2 + dx);
                let c = scene.color_at(row, col) as usize;
                bins[c][map.bin(scene.data[row * w + col])] += 1;
            }
        }
    }
    bins
}

#[cfg(test)]
mod tests {

    /// A `LumaImage` wrapping `data`, and the identity composition over it.
    ///
    /// Uncropped and upright, which is what every test written before composition
    /// existed means — and the case the fast flat walk has to keep answering
    /// bit-for-bit.
    fn upright(data: &[f32], w: usize) -> (raw_core::LumaImage, raw_core::Frame) {
        let h = data.len() / w.max(1);
        let luma = raw_core::LumaImage {
            data: data.to_vec(),
            output_dims: raw_core::Dims { w, h },
            source_dims: raw_core::Dims { w: w * 2, h: h * 2 },
            clipped: Vec::new(),
        };
        let frame = raw_core::Frame::resolve(
            luma.output_dims,
            raw_core::Orientation::Rotate0,
            &raw_core::CompositionParams::default(),
        );
        (luma, frame)
    }
    use super::*;
    use raw_core::geometry::CfaColor::{Blue, Green, Red};
    use raw_core::sensor::Gains;
    use raw_core::{CfaGeometry, Dims};

    #[test]
    fn hidden_distributions_do_no_sampling_and_preserve_finished_bins() {
        let mut h = Histogram::default();
        let bins = [3; BINS];
        h.accept_finished(bins);
        let mut p = Params::default();
        h.refresh_mapping(&p);
        let lut = h.map.as_ref().unwrap().lut.as_ptr();
        p.exposure.ev = 1.0;
        p.display.gamma = 1.8;
        h.refresh_mapping(&p);
        assert_eq!(h.map.as_ref().unwrap().lut.as_ptr(), lut);
        assert!(h.curve_samples.is_empty());
        assert_eq!(h.rgb, [[0; BINS]; 3]);
        assert_eq!(h.tonal, bins);
        p.curve.add(0.5, 0.7);
        h.refresh_mapping(&p);
        assert_ne!(h.map.as_ref().unwrap().lut.as_ptr(), lut);
        assert_eq!(h.tonal, bins);
    }

    #[test]
    fn cpu_distributions_follow_only_their_inputs() {
        let scene = leica_like(32, 24);
        let (luma, frame) = upright(&[0.18; 32 * 24], 32);
        let mut p = Params::default();
        let mut h = Histogram::default();
        h.refresh_curve_samples(&luma, &p, 1, &frame);
        h.refresh_rgb(&scene, &p, 1);
        let expected_rgb = h.rgb;
        let expected_samples = h.curve_samples.clone();
        h.rgb = [[77; BINS]; 3];
        h.curve_samples = vec![77.0];
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = 5.0;
        p.grain.enabled = !p.grain.enabled;
        p.toning.enabled = !p.toning.enabled;
        p.display.dither = !p.display.dither;
        h.refresh_curve_samples(&luma, &p, 1, &frame);
        h.refresh_rgb(&scene, &p, 1);
        assert_eq!(h.rgb, [[77; BINS]; 3]);
        assert_eq!(h.curve_samples, vec![77.0]);
        h.refresh_curve_samples(&luma, &p, 2, &frame);
        h.refresh_rgb(&scene, &p, 1);
        assert_eq!(h.curve_samples, expected_samples);
        assert_eq!(h.rgb, [[77; BINS]; 3]);
        h.refresh_rgb(&scene, &p, 2);
        assert_eq!(h.rgb, expected_rgb);
        h.curve_samples = vec![77.0];
        p.curve.add(0.5, 0.7);
        p.display.gamma = 1.8;
        h.refresh_curve_samples(&luma, &p, 2, &frame);
        h.refresh_rgb(&scene, &p, 2);
        assert_eq!(h.curve_samples, vec![77.0]);
        assert_eq!(h.rgb, rgb_bins(&scene, &Mapping::new(&p)));
        p.exposure.ev = 1.0;
        h.refresh_curve_samples(&luma, &p, 2, &frame);
        assert!(h.curve_samples.iter().all(|v| (*v - 0.36).abs() < 1e-6));
        let mut crop = frame;
        crop.crop.w /= 2;
        h.refresh_curve_samples(&luma, &p, 2, &crop);
        assert_eq!(h.curve_in, tonal_bins(&luma, &crop, &Mapping::new(&p)).1);
        assert!(h.curve_samples.len() < expected_samples.len());
    }

    #[test]
    fn later_curve_layers_do_not_invalidate_earlier_distributions() {
        let mut h = Histogram {
            curve_samples: vec![0.18, 0.5],
            ..Default::default()
        };
        let mut stack = raw_core::CurveStack::default();
        stack.add_instance();
        h.curve_input(&stack, 0);
        h.curve_cache.as_mut().unwrap().bins = [77; BINS];
        stack.instances[1].curve.add(0.5, 0.7);
        assert_eq!(h.curve_input(&stack, 0), [77; BINS]);
        stack.instances[0].opacity = 0.5;
        assert_ne!(h.curve_input(&stack, 0), [77; BINS]);
    }

    #[test]
    fn output_only_edits_preserve_ready_and_inflight_gpu_histograms() {
        let (_, frame) = upright(&[0.18; 4], 2);
        let mut h = GpuHistogram::default();
        let mut p = Params::default();
        h.prepare(Arc::new(p.clone()), frame, 1);
        let in_flight = h.wanted.clone().unwrap();
        p.grain.enabled = !p.grain.enabled;
        p.display.dither = !p.display.dither;
        h.prepare(Arc::new(p.clone()), frame, 1);
        assert!(h.store(&in_flight, [3; BINS]));
        h.prepare(Arc::new(p.clone()), frame, 1);
        assert_eq!(h.bins(), Some(&[3; BINS]));
        p.contrast_mask.enabled = true;
        h.prepare(Arc::new(p), frame, 1);
        assert!(h.bins().is_none());
        assert!(!h.store(&in_flight, [3; BINS]));
    }

    #[test]
    fn stale_gpu_histograms_cannot_replace_the_latest_request() {
        let (_, frame) = upright(&[0.2; 4], 2);
        let mut changed = Params::default();
        changed.exposure.ev = 1.0;
        let turned = raw_core::Frame::resolve(
            Dims { w: 2, h: 2 },
            raw_core::Orientation::Rotate90,
            &Default::default(),
        );
        for (params, latest_frame, generation) in [
            (Arc::new(changed), frame, 1),
            (Arc::new(Params::default()), turned, 1),
            (Arc::new(Params::default()), frame, 2),
        ] {
            let mut histogram = GpuHistogram::default();
            histogram.prepare(Arc::new(Params::default()), frame, 1);
            let old = histogram.wanted.clone().unwrap();
            histogram.prepare(params, latest_frame, generation);
            assert!(!histogram.store(&old, [1; BINS]));
            assert!(histogram.ready.is_none());

            let current = histogram.wanted.clone().unwrap();
            assert!(histogram.store(&current, [2; BINS]));
            assert_eq!(
                histogram.ready.as_ref().map(|(_, bins)| bins),
                Some(&[2; BINS])
            );
        }
    }

    #[test]
    fn gpu_histogram_cooldown_tracks_cost_with_fixed_bounds() {
        assert_eq!(cooldown(Duration::from_millis(1)), MIN_COOLDOWN);
        assert_eq!(
            cooldown(Duration::from_millis(40)),
            Duration::from_millis(80)
        );
        assert_eq!(cooldown(Duration::from_secs(1)), MAX_COOLDOWN);
    }

    #[test]
    fn finished_bins_drive_tonal_and_clipping_without_touching_diagnostics() {
        let mut histogram = Histogram::default();
        histogram.curve_in[4] = 7;
        histogram.rgb[2][8] = 9;
        let mut bins = [0; BINS];
        bins[0] = 64;
        bins[32] = 32;
        bins[BINS - 1] = 32;

        histogram.accept_finished(bins);

        assert_eq!(histogram.tonal, bins);
        assert_eq!(histogram.clipped(), (0.5, 0.25));
        assert_eq!(histogram.curve_in[4], 7);
        assert_eq!(histogram.rgb[2][8], 9);
    }

    /// A sky grad: full at the top of the negative and gone by `y1`.
    fn sky(y1: f32) -> raw_core::curve::CurveMask {
        use raw_core::dodgeburn::{Linear, Shape};
        raw_core::curve::CurveMask::of_shape(Shape::Linear(Linear {
            x0: 0.5,
            y0: 0.0,
            x1: 0.5,
            y1,
            feather: 1.0,
            ev: 0.0,
        }))
    }

    /// A hard edge down the middle: the left half of the negative and none of the right.
    fn left_half() -> raw_core::curve::CurveMask {
        use raw_core::dodgeburn::{Linear, Shape};
        raw_core::curve::CurveMask::of_shape(Shape::Linear(Linear {
            x0: 0.0,
            y0: 0.5,
            x1: 1.0,
            y1: 0.5,
            feather: 0.0,
            ev: 0.0,
        }))
    }

    /// A histogram over a picture whose left half is `left` and right half `right`, as
    /// scene values, sampled at every pixel of a 40 by 20 negative.
    fn two_halves(left: f32, right: f32) -> Histogram {
        let (w, h) = (40usize, 20usize);
        let mut histogram = Histogram::default();
        for i in 0..w * h {
            let (x, y) = (i % w, i / w);
            histogram.curve_samples.push(if x < w / 2 { left } else { right });
            histogram
                .curve_at
                .push(((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32));
        }
        histogram
    }

    /// The bin a scene value falls in on the curve's axis.
    fn bin_of(scene: f32) -> usize {
        ((raw_core::Curve::to_normalized(scene) * (BINS - 1) as f32) as usize).min(BINS - 1)
    }

    #[test]
    fn moving_a_curve_mask_is_not_a_new_mapping_and_does_not_re_run_a_curve() {
        // The CPU chain reads the curve through a table that ignores where a mask lies,
        // so a mask handle moving 60 times a second must not rebuild it. The ghost is
        // taken inside the mask and so does change, but by weighing the samples again:
        // the tones entering the pass, which cost a curve evaluation each, are kept.
        let mut a = Params::default();
        a.curve.add(0.5, 0.65);
        a.curve.instances[0].mask = Some(sky(0.5));
        let mut b = a.clone();
        b.curve.instances[0].mask = Some(sky(0.3));
        assert!(MappingKey::of(&a) == MappingKey::of(&b));

        let mut h = two_halves(0.18, 0.5);
        let first = h.curve_ghost(&a.curve, 0, 0.5, None);
        let built = h.curve_cache.as_ref().map(|c| c.serial);
        let moved = h.curve_ghost(&b.curve, 0, 0.5, None);
        assert_eq!(
            h.curve_cache.as_ref().map(|c| c.serial),
            built,
            "the entering tones were rebuilt for a mask that moved"
        );
        assert!(
            moved.coverage < first.coverage,
            "a shorter grad selects less: {:?} against {:?}",
            moved.coverage,
            first.coverage
        );
        // Asked again with nothing changed, it is the same answer from the cache.
        assert_eq!(h.curve_ghost(&b.curve, 0, 0.5, None), moved);

        // But a curve that changed is still a new mapping, and new entering tones.
        let mut c = a.clone();
        c.curve.add(0.3, 0.2);
        assert!(MappingKey::of(&a) != MappingKey::of(&c));
        h.curve_ghost(&c.curve, 0, 0.5, None);
        assert_ne!(h.curve_cache.as_ref().map(|c| c.serial), built);
    }

    #[test]
    fn the_ghost_and_the_average_are_taken_inside_the_mask() {
        // Dark on the left, bright on the right, and a curve that lifts the dark tone by
        // more than the bright one. Masked to the left half, the ghost is the dark tone
        // alone, the average is the dark tone's change, and half the frame is selected.
        let (dark, bright) = ((-6.0f32).exp2(), (-1.0f32).exp2());
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0]
            .curve
            .add(raw_core::Curve::to_normalized(dark), 0.5);
        let change = |scene: f32| stack.apply_before(1, scene).log2() - scene.log2();
        let (lift_dark, lift_bright) = (change(dark), change(bright));
        assert!(lift_dark > lift_bright + 0.5, "{lift_dark} against {lift_bright}");

        let mut h = two_halves(dark, bright);
        let whole = h.curve_ghost(&stack, 0, 0.5, None);
        assert_eq!(whole.coverage, None, "no mask, no share of the frame to report");
        assert_eq!(whole.span, None);
        assert_eq!(whole.bins[bin_of(dark)], 400);
        assert_eq!(whole.bins[bin_of(bright)], 400);
        let mean = whole.average.expect("positive tones");
        assert!((mean - 0.5 * (lift_dark + lift_bright)).abs() < 1e-4);

        stack.instances[0].mask = Some(left_half());
        let masked = h.curve_ghost(&stack, 0, 0.5, None);
        assert!(masked.bins[bin_of(dark)] > 0);
        assert_eq!(masked.bins[bin_of(bright)], 0, "a tone the mask does not reach");
        assert!((masked.coverage.unwrap() - 0.5).abs() < 0.03, "{:?}", masked.coverage);
        assert!((masked.average.unwrap() - lift_dark).abs() < 1e-3);
        // The axis is shaded where those tones are, and nowhere near the bright one.
        let (lo, hi) = masked.span.expect("the mask selects something");
        let at = raw_core::Curve::to_normalized(dark);
        assert!(lo <= at && at <= hi + 1.0 / BINS as f32, "{lo}..{hi} around {at}");
        assert!(hi < raw_core::Curve::to_normalized(bright));

        // Switched off, the mask is not there and the ghost is the whole frame's again.
        stack.instances[0].mask.as_mut().unwrap().enabled = false;
        assert_eq!(h.curve_ghost(&stack, 0, 0.5, None), whole);

        // A mask that selects nothing has no average to give, and says so.
        let mut nothing = left_half();
        nothing.invert = true;
        let mut nowhere = two_halves(dark, dark);
        nowhere.curve_at.iter_mut().for_each(|at| at.0 *= 0.4);
        stack.instances[0].mask = Some(nothing);
        let empty = nowhere.curve_ghost(&stack, 0, 0.5, None);
        assert_eq!(empty.average, None);
        assert_eq!(empty.coverage, Some(0.0));
        assert_eq!(empty.span, None);
    }

    #[test]
    fn a_mask_on_the_pass_above_shapes_what_enters_the_one_below() {
        // The first pass lifts, on the left half only. What enters the second is lifted
        // on the left and not on the right, which a ghost that read the first pass at
        // full strength everywhere used to get wrong.
        let tone = (-5.0f32).exp2();
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0]
            .curve
            .add(raw_core::Curve::to_normalized(tone), 0.6);
        stack.add_instance();
        let lifted = stack.apply_before(1, tone);
        assert!(bin_of(lifted) > bin_of(tone) + 4);

        let mut h = two_halves(tone, tone);
        let everywhere = h.curve_ghost(&stack, 1, 0.5, None);
        assert_eq!(everywhere.bins[bin_of(lifted)], 800);

        stack.instances[0].mask = Some(left_half());
        let split = h.curve_ghost(&stack, 1, 0.5, None);
        assert_eq!(split.bins[bin_of(lifted)], 400, "the left half, lifted");
        assert_eq!(split.bins[bin_of(tone)], 400, "the right half, as it was");
        assert_eq!(split.coverage, None, "the pass below has no mask of its own");

        // Moving that mask is a change to what enters, and is rebuilt.
        let built = h.curve_cache.as_ref().map(|c| c.serial);
        let mut inverted = left_half();
        inverted.invert = true;
        stack.instances[0].mask = Some(inverted);
        assert_eq!(h.curve_ghost(&stack, 1, 0.5, None), split);
        assert_ne!(h.curve_cache.as_ref().map(|c| c.serial), built);
    }

    /// One hard card dab, a rectangle `half` wide either side of `x` and the full height
    /// of a negative half as tall as it is wide: `paint` adds to a mask, or erases.
    fn slab(x: f32, half: f32, paint: bool) -> raw_core::dodgeburn::Gesture {
        raw_core::dodgeburn::Gesture::new(vec![raw_core::dodgeburn::Dab {
            x,
            y: 0.5,
            radius: half,
            feather: 0.0,
            opacity: 1.0,
            ev: if paint { 1.0 } else { -1.0 },
            aspect: 2.0,
            angle: 0.0,
            nib: raw_core::dodgeburn::Nib::Card,
            falloff: raw_core::dodgeburn::Falloff::Core,
        }])
    }

    fn painted(passes: Vec<raw_core::dodgeburn::Gesture>) -> raw_core::curve::CurveMask {
        raw_core::curve::CurveMask::of_shape(raw_core::dodgeburn::Shape::Brush {
            nib: raw_core::dodgeburn::Nib::Card,
            passes,
        })
    }

    #[test]
    fn a_painted_mask_weighs_the_ghost_by_what_has_been_painted() {
        // Dark on the left, bright on the right, and the left half painted in. The ghost
        // is the dark tone alone and half the frame is selected, read from the picture
        // of the strokes and not from the strokes one sample at a time.
        let (dark, bright) = ((-6.0f32).exp2(), (-1.0f32).exp2());
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0].curve.add(0.4, 0.6);
        stack.instances[0].mask = Some(painted(vec![slab(0.25, 0.25, true)]));
        let mut h = two_halves(dark, bright);
        let left = h.curve_ghost(&stack, 0, 0.5, None);
        assert!(h.curve_cover[0].is_some(), "the painted mask has no picture");
        assert!((left.coverage.unwrap() - 0.5).abs() < 0.03, "{:?}", left.coverage);
        assert!(left.bins[bin_of(dark)] > 0);
        assert_eq!(left.bins[bin_of(bright)], 0);

        // A second stroke, over the right half: the ghost follows it, with both tones,
        // and the entering tones are not rebuilt for a stroke.
        let built = h.curve_cache.as_ref().map(|c| c.serial);
        let both = vec![slab(0.25, 0.25, true), slab(0.75, 0.25, true)];
        stack.instances[0].mask = Some(painted(both.clone()));
        let all = h.curve_ghost(&stack, 0, 0.5, None);
        assert!((all.coverage.unwrap() - 1.0).abs() < 0.03, "{:?}", all.coverage);
        assert!(all.bins[bin_of(bright)] > 0);
        assert_eq!(h.curve_cache.as_ref().map(|c| c.serial), built);

        // An eraser pass takes the left back, and an undo of it returns it.
        let mut erased = both.clone();
        erased.push(slab(0.25, 0.25, false));
        stack.instances[0].mask = Some(painted(erased));
        let right = h.curve_ghost(&stack, 0, 0.5, None);
        assert!((right.coverage.unwrap() - 0.5).abs() < 0.03, "{:?}", right.coverage);
        assert_eq!(right.bins[bin_of(dark)], 0);
        stack.instances[0].mask = Some(painted(both));
        assert_eq!(h.curve_ghost(&stack, 0, 0.5, None), all);

        // Inverted, it is everywhere but the paint.
        let mut inverted = painted(vec![slab(0.25, 0.25, true)]);
        inverted.invert = true;
        stack.instances[0].mask = Some(inverted);
        let outside = h.curve_ghost(&stack, 0, 0.5, None);
        assert_eq!(outside.bins[bin_of(dark)], 0);
        assert!(outside.bins[bin_of(bright)] > 0);

        // No longer painted, the picture of the strokes is let go.
        stack.instances[0].mask = Some(left_half());
        h.curve_ghost(&stack, 0, 0.5, None);
        assert!(h.curve_cover[0].is_none());
    }

    #[test]
    fn a_painted_mask_above_shapes_what_enters_the_pass_below() {
        let tone = (-5.0f32).exp2();
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0]
            .curve
            .add(raw_core::Curve::to_normalized(tone), 0.6);
        stack.add_instance();
        let lifted = stack.apply_before(1, tone);
        stack.instances[0].mask = Some(painted(vec![slab(0.25, 0.25, true)]));
        let mut h = two_halves(tone, tone);
        let split = h.curve_ghost(&stack, 1, 0.5, None);
        // The painted half lifted and the other as it was, give or take the one column
        // of samples the picture's edge falls across.
        let near = |got: u32, want: u32| got.abs_diff(want) <= 20;
        assert!(near(split.bins[bin_of(lifted)], 400), "{}", split.bins[bin_of(lifted)]);
        assert!(near(split.bins[bin_of(tone)], 400), "{}", split.bins[bin_of(tone)]);
        assert_eq!(split.coverage, None, "the pass below has no mask of its own");
    }

    #[test]
    fn a_tone_range_weighs_the_ghost_by_the_picture_and_waits_for_it() {
        // A tone-only mask on the highlights. The basis says the right half is bright;
        // with it, the ghost is the right half alone. Until it arrives the range weighs
        // one, which is the whole frame, and the ghost is taken again when it does.
        let (w, h) = (40usize, 20usize);
        let basis = Arc::new(raw_core::Basis {
            w,
            h,
            ev: (0..w * h)
                .map(|i| if i % w < w / 2 { -3.0 } else { 2.5 })
                .collect(),
        });
        let (dark, bright) = ((-6.0f32).exp2(), (-1.0f32).exp2());
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0].curve.add(0.5, 0.6);
        stack.instances[0].mask = Some(raw_core::curve::CurveMask {
            enabled: true,
            shape: None,
            invert: false,
            tone: Some(raw_core::ZoneMask {
                enabled: true,
                lo: 2.0,
                hi: 5.0,
                f_lo: 0.5,
                edge_aware: false,
                ..Default::default()
            }),
            rule: raw_core::curve::Rule::Intersect,
        });

        let mut histogram = two_halves(dark, bright);
        let waiting = histogram.curve_ghost(&stack, 0, 0.5, None);
        assert_eq!(waiting.coverage, Some(1.0), "no basis yet: the range weighs one");
        let ready = histogram.curve_ghost(&stack, 0, 0.5, Some(&basis));
        assert!((ready.coverage.unwrap() - 0.5).abs() < 0.03, "{:?}", ready.coverage);
        assert_eq!(ready.bins[bin_of(dark)], 0);
        assert!(ready.bins[bin_of(bright)] > 0);

        // A new basis, as after an exposure change, is weighed again.
        let darker = Arc::new(raw_core::Basis {
            w,
            h,
            ev: vec![-3.0; w * h],
        });
        let gone = histogram.curve_ghost(&stack, 0, 0.5, Some(&darker));
        assert_eq!(gone.coverage, Some(0.0));
    }

    /// What the masked ghost costs on a frame where the mask moved, and on one where a
    /// curve did. A measurement and not an assertion; run it in release:
    ///
    /// ```text
    /// cargo test --release -p raw-app --bin monopro what_the_masked_ghost_costs -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, not an assertion"]
    fn what_the_masked_ghost_costs() {
        use raw_core::dodgeburn::{Radial, Shape};
        let (w, h) = (500usize, 400usize);
        let mut histogram = Histogram::default();
        for i in 0..w * h {
            let (x, y) = (i % w, i / w);
            histogram
                .curve_samples
                .push((-8.0 + 8.0 * x as f32 / w as f32).exp2());
            histogram
                .curve_at
                .push(((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32));
        }
        let basis = Arc::new(raw_core::Basis {
            w: 480,
            h: 384,
            ev: (0..480 * 384).map(|i| -4.0 + 8.0 * (i % 480) as f32 / 480.0).collect(),
        });
        let spot = |cx: f32| {
            Shape::Radial(Radial {
                cx,
                cy: 0.5,
                inner: 0.05,
                outer: 0.3,
                aspect: 0.7,
                angle: 20.0,
                feather: 0.8,
                invert: false,
                ev: 0.0,
            })
        };
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0].curve.add(0.4, 0.55);
        stack.instances[0].mask = Some(raw_core::curve::CurveMask {
            tone: Some(raw_core::ZoneMask {
                enabled: true,
                lo: -1.0,
                hi: 2.0,
                ..Default::default()
            }),
            ..raw_core::curve::CurveMask::of_shape(spot(0.5))
        });
        let rounds = 20u32;
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds);
        println!("  {} samples                          ms/frame", w * h);
        histogram.curve_ghost(&stack, 0, 0.8, Some(&basis));

        let t = Instant::now();
        for i in 0..rounds {
            stack.instances[0].mask.as_mut().unwrap().shape = Some(spot(0.3 + 0.01 * i as f32));
            histogram.curve_ghost(&stack, 0, 0.8, Some(&basis));
        }
        println!("  a shape handle dragged              {:9.2}", ms(t));
        let t = Instant::now();
        for i in 0..rounds {
            let tone = stack.instances[0].mask.as_mut().unwrap().tone.as_mut().unwrap();
            tone.hi = 1.0 + 0.02 * i as f32;
            histogram.curve_ghost(&stack, 0, 0.8, Some(&basis));
        }
        println!("  a tone ruler handle dragged         {:9.2}", ms(t));
        let t = Instant::now();
        for i in 0..rounds {
            stack.instances[0].opacity = 1.0 - 0.01 * i as f32;
            histogram.curve_ghost(&stack, 0, 0.8, Some(&basis));
        }
        println!("  a curve or an opacity changed       {:9.2}", ms(t));
        let t = Instant::now();
        for _ in 0..rounds {
            histogram.curve_ghost(&stack, 0, 0.8, Some(&basis));
        }
        println!("  nothing changed                     {:9.2}", ms(t));

        let mut plain = stack.clone();
        plain.instances[0].mask = None;
        let t = Instant::now();
        for i in 0..rounds {
            plain.instances[0].opacity = 1.0 - 0.01 * i as f32;
            histogram.curve_ghost(&plain, 0, 0.8, None);
        }
        println!("  the same, on an unmasked curve      {:9.2}", ms(t));

        // A painted mask that has been worked on: two thousand dabs in forty passes.
        let dab = |i: usize| raw_core::dodgeburn::Dab {
            x: 0.05 + 0.9 * ((i * 37) % 1000) as f32 / 1000.0,
            y: 0.05 + 0.9 * ((i * 91) % 1000) as f32 / 1000.0,
            radius: 0.06,
            feather: 0.4,
            opacity: 1.0,
            ev: 1.0,
            ..raw_core::dodgeburn::Dab::ROUND
        };
        let mut passes: Vec<raw_core::dodgeburn::Gesture> = (0..40)
            .map(|p| raw_core::dodgeburn::Gesture::new((0..50).map(|i| dab(p * 50 + i)).collect()))
            .collect();
        let mask = |passes: &[raw_core::dodgeburn::Gesture]| {
            raw_core::curve::CurveMask::of_shape(Shape::Brush {
                nib: raw_core::dodgeburn::Nib::Round,
                passes: passes.to_vec(),
            })
        };
        stack.instances[0].opacity = 1.0;
        stack.instances[0].mask = Some(mask(&passes));
        let t = Instant::now();
        histogram.curve_ghost(&stack, 0, 0.8, None);
        println!(
            "  a painted mask of 2000 dabs, first   {:9.2}",
            t.elapsed().as_secs_f64() * 1000.0
        );
        // A stroke in progress: a new pass, growing two dabs a frame.
        passes.push(raw_core::dodgeburn::Gesture::new(vec![dab(5000)]));
        let t = Instant::now();
        for i in 0..rounds as usize {
            let stroke = passes.last_mut().unwrap().dabs_mut();
            stroke.push(dab(5001 + 2 * i));
            stroke.push(dab(5002 + 2 * i));
            stack.instances[0].mask = Some(mask(&passes));
            histogram.curve_ghost(&stack, 0, 0.8, None);
        }
        println!("  a stroke on it, two dabs a frame    {:9.2}", ms(t));
        // What the same frame would cost asking the strokes themselves.
        let shape = Shape::Brush {
            nib: raw_core::dodgeburn::Nib::Round,
            passes: passes.clone(),
        };
        let t = Instant::now();
        let mut sum = 0.0f32;
        for i in (0..histogram.curve_at.len()).step_by(odd_stride(w * h, MASK_BUDGET)) {
            let (x, y) = histogram.curve_at[i];
            sum += shape.painted_at(x, y, 0.8);
        }
        std::hint::black_box(sum);
        println!(
            "  the same frame, dab by dab           {:9.2}",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn the_readout_says_when_the_average_is_inside_a_mask() {
        let ghost = |average, coverage| CurveGhost {
            bins: [0; BINS],
            average,
            coverage,
            span: None,
        };
        assert_eq!(ghost(Some(-0.42), None).readout(), "Average: 0.42 EV darker");
        assert_eq!(ghost(Some(0.001), None).readout(), "Average: unchanged");
        assert_eq!(ghost(None, None).readout(), "Average: —");
        assert_eq!(
            ghost(Some(-0.42), Some(0.38)).readout(),
            "Average: 0.42 EV darker inside the mask (38% of the frame)"
        );
        assert_eq!(
            ghost(Some(0.3), Some(0.004)).readout(),
            "Average: 0.30 EV lighter inside the mask (<1% of the frame)"
        );
        assert_eq!(
            ghost(None, Some(0.0)).readout(),
            "Average: — inside the mask (0% of the frame)"
        );
    }

    #[test]
    fn every_sample_knows_where_on_the_negative_it_is() {
        // Uncropped, the walk is over the stored buffer and a sample's place is its
        // pixel's center. Cropped and turned, it is the same pixel the value came from.
        let (w, h) = (8usize, 6usize);
        let data: Vec<f32> = (0..w * h).map(|i| i as f32).collect();
        let (luma, frame) = upright(&data, w);
        let mut seen = 0;
        for_each_tonal_sample(&luma, &frame, |v, x, y| {
            let i = v as usize;
            assert!((x - ((i % w) as f32 + 0.5) / w as f32).abs() < 1e-6);
            assert!((y - ((i / w) as f32 + 0.5) / h as f32).abs() < 1e-6);
            seen += 1;
        });
        assert_eq!(seen, w * h);

        let turned = raw_core::Frame::resolve(
            luma.output_dims,
            raw_core::Orientation::Rotate90,
            &raw_core::CompositionParams {
                crop: raw_core::composition::Rect {
                    x: 0.25,
                    y: 0.25,
                    w: 0.5,
                    h: 0.5,
                },
                ..Default::default()
            },
        );
        let mut seen = 0;
        for_each_tonal_sample(&luma, &turned, |v, x, y| {
            let i = v as usize;
            assert!((x - ((i % w) as f32 + 0.5) / w as f32).abs() < 1e-6, "{i}: {x}");
            assert!((y - ((i / w) as f32 + 0.5) / h as f32).abs() < 1e-6, "{i}: {y}");
            seen += 1;
        });
        assert!(seen > 0 && seen < w * h);
    }

    #[test]
    fn curve_average_accounts_for_opacity_bypass_and_missing_tones() {
        let mut histogram = Histogram::default();
        let mut stack = raw_core::CurveStack {
            enabled: true,
            ..Default::default()
        };
        // At the middle of the EV window, this straight curve lifts by 0.5 EV.
        stack.instances[0].curve.move_point(0, 0.0, 1.0 / 12.0);
        histogram.curve_samples = vec![(-4.0f32).exp2(), 0.0, -1.0];
        histogram.curve_input(&stack, 0);
        assert!((histogram.curve_average_ev().unwrap() - 0.5).abs() < 1e-5);
        stack.instances[0].opacity = 0.5;
        histogram.curve_input(&stack, 0);
        assert!((histogram.curve_average_ev().unwrap() - 0.25).abs() < 1e-5);
        stack.enabled = false;
        histogram.curve_input(&stack, 0);
        assert_eq!(histogram.curve_average_ev(), Some(0.0));
        let mut empty = Histogram::default();
        empty.curve_input(&stack, 0);
        assert_eq!(empty.curve_average_ev(), None);
    }

    #[test]
    fn later_curve_histograms_do_not_introduce_empty_bins() {
        let mut histogram = Histogram {
            curve_samples: (0..20_000)
                .map(|i| {
                    let x = 0.15 + 0.60 * i as f32 / 19_999.0;
                    (raw_core::curve::LO_EV + x * (raw_core::curve::HI_EV - raw_core::curve::LO_EV))
                        .exp2()
                })
                .collect(),
            ..Default::default()
        };
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0].curve.add(0.5, 0.75);
        stack.add_instance();
        stack.instances[1].curve.add(0.5, 0.25);
        stack.add_instance();
        let original = histogram.curve_input(&stack, 0);
        for layer in [1, 2] {
            let bins = histogram.curve_input(&stack, layer);
            assert_eq!(bins.iter().sum::<u32>(), 20_000);
            let first = bins.iter().position(|&n| n > 0).unwrap();
            let last = bins.iter().rposition(|&n| n > 0).unwrap();
            assert!(
                bins[first..=last].iter().all(|&n| n > 0),
                "layer {layer} introduced gaps in a continuous tone ramp"
            );
            assert_eq!(histogram.curve_input(&stack, layer), bins);
        }
        stack.instances[0].curve.enabled = false;
        assert_eq!(histogram.curve_input(&stack, 1), original);
        stack.instances[0].curve.enabled = true;
        stack.instances[0].opacity = 0.0;
        assert_eq!(histogram.curve_input(&stack, 1), original);
    }

    #[test]
    fn the_readout_agrees_with_what_export_would_write() {
        // The claim the footer rests on: `L*` under the cursor is not an
        // approximation of the print value, it *is* the print value. Both sides run
        // the same tone map and the same `lstar_encode`, so if the readout and
        // export ever diverge, one of them changed and this says which.
        let mut p = Params::default();
        p.exposure.ev = 1.25;
        p.curve.add(0.4, 0.62);
        p.display.tone_map = raw_core::ToneMap::SHOULDER_DEFAULT;

        let mut h = Histogram::default();
        let (l, f) = upright(&[0.0; 4], 2);
        h.refresh(&l, &leica_like(2, 2), &p, 0, &f);

        for raw in [0.02_f32, 0.18, 0.5, 0.9, 1.4] {
            let (lstar, _) = h.sample(raw).expect("refreshed");
            // What export does, from raw_gpu::export -> display::lstar_u16.
            let map = Mapping::new(&p);
            let exported = raw_core::display::lstar_u16(raw_core::display::tone_map(
                map.scene(raw),
                p.display.tone_map,
            ));
            let as_lstar = exported as f32 / 65535.0 * 100.0;
            assert!(
                (lstar - as_lstar).abs() < 0.01,
                "readout {lstar} but export would write {as_lstar} for scene {raw}"
            );
        }
    }

    #[test]
    fn bypassing_a_module_moves_the_readout_with_the_image() {
        // The readout is fed `render_params`, so a bypassed module must drop out of
        // it exactly as it drops out of the viewport. Reading `params` here was the
        // bug this pins: the number would have kept reporting an exposure the screen
        // was no longer showing.
        let mut p = Params::default();
        p.exposure.ev = 2.0;

        let mut on = Histogram::default();
        let (l, f) = upright(&[0.0; 4], 2);
        on.refresh(&l, &leica_like(2, 2), &p.effective(), 0, &f);
        let lit = on.sample(0.1).expect("refreshed").0;

        p.exposure.enabled = false;
        let mut off = Histogram::default();
        off.refresh(&l, &leica_like(2, 2), &p.effective(), 0, &f);
        let dark = off.sample(0.1).expect("refreshed").0;

        assert!(
            dark < lit - 1.0,
            "bypassing +2 EV left the readout at {dark} vs {lit}"
        );
    }

    #[test]
    fn clipped_counts_both_ends_of_the_frame() {
        let p = Params::default();
        let mut h = Histogram::default();
        // Four pixels: two crushed black, one blown, one midtone.
        let (l, f) = upright(&[-1.0, -1.0, 8.0, 0.18], 2);
        h.refresh(&l, &leica_like(2, 2), &p, 0, &f);
        let (lo, hi) = h.clipped();
        assert!(
            (lo - 0.5).abs() < 1.0e-6,
            "half the frame is at the black end, got {lo}"
        );
        assert!((hi - 0.25).abs() < 1.0e-6, "a quarter is blown, got {hi}");
    }

    #[test]
    fn agx_auto_range_reads_the_post_exposure_scene() {
        let grey = 0.18f32;
        let mut values = vec![grey * 2.0f32.powf(-4.0); 500];
        values.extend(vec![grey * 2.0f32.powf(3.0); 500]);
        let (luma, frame) = upright(&values, 40);
        let mut params = Params::default();

        let (black, white) = agx_auto_range(&luma, &frame, &params).expect("has tones");
        assert!((black + 4.5).abs() < 1.0e-4, "black was {black}");
        assert!((white - 3.5).abs() < 1.0e-4, "white was {white}");

        params.exposure.ev = 1.0;
        let (black, white) = agx_auto_range(&luma, &frame, &params).expect("has tones");
        assert!((black + 3.5).abs() < 1.0e-4, "exposed black was {black}");
        assert!((white - 4.5).abs() < 1.0e-4, "exposed white was {white}");
    }

    /// Leica M10-R geometry: `G B / R G` at the sensor origin, even crop.
    fn leica_like(w: usize, h: usize) -> SceneImage {
        let geom = CfaGeometry::new(w, Dims { w, h }, 0, 0, w, h, [[Green, Blue], [Red, Green]]);
        // Distinct per-colour values so a missing channel is unmistakable.
        let mut data = vec![0.0f32; w * h];
        for row in 0..h {
            for col in 0..w {
                data[row * w + col] = match geom.color_at(row, col) {
                    Red => 0.20,
                    Green => 0.50,
                    Blue => 0.80,
                };
            }
        }
        SceneImage {
            data,
            geom,
            gains: Gains([1.0, 1.0, 1.0]),
            camera: "test".into(),
            clipped: Vec::new(),
        }
    }

    #[test]
    fn every_cfa_colour_is_sampled() {
        // The regression. At Leica dimensions the old flat even stride sampled zero
        // blue photosites and drew a flat line, while red and green looked fine.
        let scene = leica_like(7864, 5200);
        let map = Mapping::new(&Params::default());
        let bins = rgb_bins(&scene, &map);

        for (i, name) in ["red", "green", "blue"].iter().enumerate() {
            let total: u32 = bins[i].iter().sum();
            assert!(total > 0, "{name} channel was never sampled");
        }
    }

    #[test]
    fn quad_sampling_preserves_photosite_density() {
        // One R, two G, one B per quad — so green must come out at exactly twice the
        // others. If this drifts, the sampler has stopped walking whole quads.
        let scene = leica_like(1024, 768);
        let map = Mapping::new(&Params::default());
        let bins = rgb_bins(&scene, &map);
        let total = |c: usize| bins[c].iter().sum::<u32>();

        assert_eq!(total(0), total(2), "red and blue densities must match");
        assert_eq!(total(1), 2 * total(0), "green must be twice red");
    }

    #[test]
    fn each_channel_lands_in_its_own_bin() {
        // Distinct per-colour values must produce distinct peaks, which is what makes
        // the RGB view readable as "how far from neutral is this subject".
        let scene = leica_like(512, 512);
        let map = Mapping::new(&Params::default());
        let bins = rgb_bins(&scene, &map);

        let peak = |c: usize| {
            bins[c]
                .iter()
                .enumerate()
                .max_by_key(|(_, n)| **n)
                .map(|(i, _)| i)
                .unwrap()
        };
        let (r, g, b) = (peak(0), peak(1), peak(2));
        assert!(
            r < g && g < b,
            "channels did not separate: r={r} g={g} b={b}"
        );
    }

    #[test]
    fn a_narrow_image_still_samples_every_colour() {
        // Odd-ish shapes are where quad walking could go wrong.
        for (w, h) in [(2, 2), (4, 6), (6, 4), (1024, 2), (2, 1024)] {
            let scene = leica_like(w, h);
            let bins = rgb_bins(&scene, &Mapping::new(&Params::default()));
            for (i, name) in ["red", "green", "blue"].iter().enumerate() {
                assert!(bins[i].iter().sum::<u32>() > 0, "{name} missing at {w}x{h}");
            }
        }
    }

    #[test]
    fn the_tonal_stride_is_always_odd() {
        // An even stride over a DirectMosaic working image locks to one column
        // parity, which under Green weighting samples mostly zeros.
        for len in [0, 1, 1000, 200_001, 40_892_800, 102_000_000] {
            let s = odd_stride(len, BUDGET);
            assert!(s % 2 == 1, "stride {s} for len {len} is even");
            assert!(s >= 1);
        }
    }

    #[test]
    fn tonal_sampling_sees_both_column_parities() {
        // The DirectMosaic + Green case: only green photosites carry signal, and they
        // sit on alternating columns. A stride that cannot change column parity would
        // report an image that is either all signal or all black.
        let w = 1024;
        let scene = leica_like(w, 64);
        // Emulate DirectMosaic + Green: zero everything that is not green.
        let luma: Vec<f32> = (0..w * 64)
            .map(|i| {
                if scene.color_at(i / w, i % w) == Green {
                    scene.data[i]
                } else {
                    0.0
                }
            })
            .collect();

        let (l, f) = upright(&luma, w);
        let (bins, _) = tonal_bins(&l, &f, &Mapping::new(&Params::default()));
        let zeros = bins[0];
        let signal: u32 = bins[1..].iter().sum();
        assert!(zeros > 0, "sampled no dark photosites");
        assert!(signal > 0, "sampled no green photosites");
    }

    #[test]
    fn the_curve_ghost_is_binned_on_the_curve_axis() {
        // The trap this avoids: reusing `tonal` for the ghost behind the curve
        // editor. `tonal` is display-encoded and the editor's x is position in the
        // curve's log2-EV window, so the same pixel lands in a different column
        // under each. A histogram drawn on the wrong axis is worse than none —
        // every control point placed against it goes somewhere the tones are not.
        //
        // Scene 1.0 is the top of the curve window's usable range: LO=-10, HI=+2,
        // so log2(1.0) = 0 sits at 10/12 across. Display-encoded it would be hard
        // against the right edge instead.
        let mut params = Params::default();
        // This assertion is specifically about the hard-clip encoding, not about the
        // shipped tone-map default. Soft Shoulder deliberately keeps scene white below
        // the edge so values above it still have somewhere to go.
        params.display.tone_map = raw_core::ToneMap::Clip;
        let m = Mapping::new(&params);
        let at_white = m.curve_bin(1.0) as f32 / (BINS - 1) as f32;
        assert!(
            (at_white - 10.0 / 12.0).abs() < 0.02,
            "scene white landed at {at_white} of the curve axis, not 10/12"
        );
        assert!(
            m.bin(1.0) > BINS - 3,
            "and display-encoded it really is at the edge"
        );

        // A stop down is exactly one twelfth to the left, because the axis is log2.
        let a_stop_down = m.curve_bin(0.5) as f32 / (BINS - 1) as f32;
        assert!(
            (at_white - a_stop_down - 1.0 / 12.0).abs() < 0.02,
            "one stop should be one twelfth of the axis, got {}",
            at_white - a_stop_down
        );
    }

    #[test]
    fn refresh_is_a_no_op_when_nothing_changed() {
        let scene = leica_like(256, 256);
        let params = Params::default();
        let luma = vec![0.4f32; 4096];
        let (l, f) = upright(&luma, 64);

        let mut h = Histogram::default();
        h.refresh(&l, &scene, &params, 1, &f);
        let first = h.tonal;

        // Poison the buffer; an unnecessary recompute would overwrite it.
        h.tonal = [7; BINS];
        h.refresh(&l, &scene, &params, 1, &f);
        assert_eq!(
            h.tonal, [7; BINS],
            "histogram recomputed when nothing changed"
        );

        // A real change must get through.
        h.refresh(&l, &scene, &params, 2, &f);
        assert_eq!(
            h.tonal, first,
            "histogram did not recompute on a new generation"
        );
    }
}
