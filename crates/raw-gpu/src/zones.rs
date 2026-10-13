//! Bounded, demand-driven CPU zone preparation. One job runs per viewport; newer
//! requests are not queued behind obsolete slider positions. Results are keyed
//! by source generation and only the parameters actually consumed here.
use raw_core::{
    Basis, ContrastMaskParams, ExposureParams, Params, dodgeburn::ZoneMask, zone::Proxy,
};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
};

#[derive(Clone, PartialEq)]
struct BasisKey {
    generation: u64,
    exposure: ExposureParams,
    mask: ContrastMaskParams,
}
#[derive(Clone, PartialEq)]
struct Key {
    basis: BasisKey,
    masks: Vec<ZoneMask>,
}

pub(crate) struct Prepared {
    pub masks: Vec<Option<Vec<f32>>>,
    pub histogram: Vec<f32>,
    /// What the masks and the histogram were made from, shared by every entry with the
    /// same exposure and Contrast Mask, and what a tone sampled from the picture reads.
    pub basis: Arc<Basis>,
}
struct Entry {
    key: Key,
    data: Arc<Prepared>,
}

#[derive(Default)]
pub(crate) struct Zones {
    generation: u64,
    ready: VecDeque<Entry>,
    pending: Option<Receiver<Entry>>,
    pub rebuilds: u64,
}
impl Zones {
    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.ready.clear();
        // Keep the running job until it finishes, bounding concurrency even when
        // sources change repeatedly. Its obsolete generation will be discarded.
    }
    pub fn request(
        &mut self,
        proxy: &Arc<Proxy>,
        params: &Params,
        histogram_only: bool,
        view: Option<ZoneMask>,
        blocking: bool,
    ) -> Option<Arc<Prepared>> {
        // Dodge & Burn's active layers first, then the Curve's tone ranges, then the one
        // the mask view is showing, which is the order `MaskLayout` reads them back in. A
        // stack with none of them asks for no mask and so never builds the basis a mask
        // would be computed from.
        //
        // **The viewed tone is prepared in its own right**, last, even when a pass that
        // runs has the same range and it is prepared twice: a mask is looked at before
        // its pass has a curve to run with, so it cannot be found among the passes'.
        let masks = if histogram_only {
            Vec::new()
        } else {
            params
                .dodgeburn
                .active()
                .map(|i| i.mask)
                .chain(params.curve.tone_masks())
                .chain(view)
                .collect()
        };
        // Match Proxy::basis's actual inputs: registration is intentionally
        // ignored there, and inactive mask settings do not affect its output.
        let mut mask = params.contrast_mask;
        mask.offset = (0.0, 0.0);
        if !mask.is_active() {
            mask = ContrastMaskParams {
                enabled: false,
                ..Default::default()
            };
        }
        let exposure = ExposureParams {
            enabled: true,
            ..params.exposure
        };
        let key = Key {
            basis: BasisKey {
                generation: self.generation,
                exposure,
                mask,
            },
            masks,
        };
        loop {
            if let Some(rx) = &self.pending {
                let result = if blocking {
                    Some(rx.recv().expect("zone worker stopped"))
                } else {
                    rx.try_recv().ok()
                };
                if let Some(entry) = result {
                    self.pending = None;
                    if entry.key.basis.generation == self.generation {
                        self.ready.push_back(entry);
                        if self.ready.len() > 8 {
                            self.ready.pop_front();
                        }
                    }
                }
            }
            if let Some(entry) = self.ready.iter().find(|e| e.key == key) {
                return Some(Arc::clone(&entry.data));
            }
            if self.pending.is_some() {
                return None;
            }
            let basis = self
                .ready
                .iter()
                .find(|e| e.key.basis == key.basis)
                .map(|e| Arc::clone(&e.data.basis));
            if basis.is_none() {
                self.rebuilds += 1;
            }
            let proxy = Arc::clone(proxy);
            let task = key.clone();
            let (tx, rx) = mpsc::channel();
            self.pending = Some(rx);
            std::thread::spawn(move || {
                let basis = basis.unwrap_or_else(|| {
                    Arc::new(proxy.basis(&task.basis.exposure, &task.basis.mask))
                });
                let data = Arc::new(Prepared {
                    masks: task.masks.iter().map(|m| basis.evaluate(m)).collect(),
                    histogram: basis.histogram(crate::Viewport::ZONE_BINS),
                    basis,
                });
                let _ = tx.send(Entry { key: task, data });
            });
            if !blocking {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::{Dims, LumaImage};
    fn proxy() -> Arc<Proxy> {
        Arc::new(Proxy::of(&LumaImage {
            data: vec![0.18; 64 * 48],
            output_dims: Dims { w: 64, h: 48 },
            source_dims: Dims { w: 64, h: 48 },
            clipped: vec![],
        }))
    }

    #[test]
    fn only_basis_inputs_rebuild_and_mask_jobs_share_the_basis() {
        let proxy = proxy();
        let mut zones = Zones::default();
        let mut p = Params::default();
        let a = zones.request(&proxy, &p, true, None, true).unwrap();
        assert_eq!(zones.rebuilds, 1);
        p.display.gamma = 1.8;
        p.curve.add(0.5, 0.7);
        assert!(Arc::ptr_eq(
            &a,
            &zones.request(&proxy, &p, true, None, true).unwrap()
        ));
        // Histogram and mask requests can share the expensive basis even though
        // the prepared mask list is a different cache entry.
        p.dodgeburn.enabled = true;
        p.dodgeburn
            .instances
            .push(raw_core::dodgeburn::Instance::of(
                raw_core::dodgeburn::Sign::Burn,
                "test".into(),
                raw_core::dodgeburn::Shape::Linear(raw_core::dodgeburn::Linear {
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0,
                    feather: 1.0,
                    ev: -1.0,
                }),
            ));
        p.dodgeburn.instances[0].mask = ZoneMask {
            enabled: true,
            hi: 0.0,
            ..Default::default()
        };
        let b = zones.request(&proxy, &p, false, None, true).unwrap();
        assert!(b.masks[0].is_some());
        assert!(Arc::ptr_eq(&a.basis, &b.basis));
        assert_eq!(zones.rebuilds, 1);
        p.exposure.ev = 1.0;
        zones.request(&proxy, &p, true, None, true).unwrap();
        assert_eq!(zones.rebuilds, 2);
        zones.invalidate();
        zones.request(&proxy, &p, true, None, true).unwrap();
        assert_eq!(zones.rebuilds, 3);
    }

    #[test]
    fn an_islands_seeds_are_part_of_what_a_mask_is_prepared_from() {
        // Two bright blocks on a dark ground: the range takes both, an island one.
        let (w, h) = (64usize, 48usize);
        let mut data = vec![0.02f32; w * h];
        for y in 12..36 {
            for x in (6..26).chain(38..58) {
                data[y * w + x] = 0.72;
            }
        }
        let proxy = Arc::new(Proxy::of(&LumaImage {
            data,
            output_dims: Dims { w, h },
            source_dims: Dims { w, h },
            clipped: vec![],
        }));
        let mut zones = Zones::default();
        let mut p = Params::default();
        p.curve.instances[0].curve.add(0.5, 0.7);
        let mut tone = ZoneMask {
            enabled: true,
            lo: 1.0,
            f_lo: 0.5,
            edge_aware: false,
            ..Default::default()
        };
        let mut prepared = |tone: ZoneMask, zones: &mut Zones| {
            p.curve.instances[0].mask = Some(raw_core::curve::CurveMask {
                enabled: true,
                shape: None,
                invert: false,
                tone: Some(tone),
                rule: raw_core::curve::Rule::default(),
            });
            let ready = zones.request(&proxy, &p, false, None, true).unwrap();
            ready.masks[0].clone().expect("a bounded range has a mask")
        };
        let at = |mask: &[f32], x: usize, y: usize| mask[y * w + x];
        let plain = prepared(tone, &mut zones);
        assert_eq!((at(&plain, 16, 24), at(&plain, 48, 24)), (1.0, 1.0));

        tone.island.enabled = true;
        assert!(tone.island.set((16.5 / w as f32, 24.5 / h as f32)));
        let left = prepared(tone, &mut zones);
        assert_eq!((at(&left, 16, 24), at(&left, 48, 24)), (1.0, 0.0));
        // A seed moved is another mask, and the basis it is cut from is not rebuilt.
        assert!(tone.island.set((48.5 / w as f32, 24.5 / h as f32)));
        let right = prepared(tone, &mut zones);
        assert_eq!((at(&right, 16, 24), at(&right, 48, 24)), (0.0, 1.0));
        assert_eq!(zones.rebuilds, 1, "a seed rebuilt the basis");
    }

    #[test]
    fn requests_do_not_queue_behind_a_running_job_and_old_sources_are_discarded() {
        let proxy = proxy();
        let mut zones = Zones::default();
        let mut p = Params::default();
        let data = zones.request(&proxy, &p, true, None, true).unwrap();
        let old_key = zones.ready[0].key.clone();
        zones.ready.clear();
        let (tx, rx) = mpsc::channel();
        zones.pending = Some(rx); // deterministic running job, held by this test
        for ev in [0.1, 0.2, 0.3] {
            p.exposure.ev = ev;
            assert!(zones.request(&proxy, &p, true, None, false).is_none());
        }
        assert_eq!(zones.rebuilds, 1, "running work spawned more workers");
        zones.invalidate();
        tx.send(Entry { key: old_key, data }).ok().unwrap();
        assert!(zones.request(&proxy, &p, true, None, false).is_none());
        let ready = zones.request(&proxy, &p, true, None, true).unwrap();
        assert_eq!(zones.rebuilds, 2, "obsolete slider positions were queued");
        assert_eq!(zones.ready.len(), 1);
        assert_eq!(zones.ready[0].key.basis.generation, 1);
        assert_eq!(*ready.basis, proxy.basis(&p.exposure, &p.contrast_mask));
    }
}
