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
    basis: Arc<Basis>,
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
        blocking: bool,
    ) -> Option<Arc<Prepared>> {
        let masks = if histogram_only {
            Vec::new()
        } else {
            params.dodgeburn.active().map(|i| i.mask).collect()
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
        let a = zones.request(&proxy, &p, true, true).unwrap();
        assert_eq!(zones.rebuilds, 1);
        p.display.gamma = 1.8;
        p.curve.add(0.5, 0.7);
        assert!(Arc::ptr_eq(
            &a,
            &zones.request(&proxy, &p, true, true).unwrap()
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
        let b = zones.request(&proxy, &p, false, true).unwrap();
        assert!(b.masks[0].is_some());
        assert!(Arc::ptr_eq(&a.basis, &b.basis));
        assert_eq!(zones.rebuilds, 1);
        p.exposure.ev = 1.0;
        zones.request(&proxy, &p, true, true).unwrap();
        assert_eq!(zones.rebuilds, 2);
        zones.invalidate();
        zones.request(&proxy, &p, true, true).unwrap();
        assert_eq!(zones.rebuilds, 3);
    }

    #[test]
    fn requests_do_not_queue_behind_a_running_job_and_old_sources_are_discarded() {
        let proxy = proxy();
        let mut zones = Zones::default();
        let mut p = Params::default();
        let data = zones.request(&proxy, &p, true, true).unwrap();
        let old_key = zones.ready[0].key.clone();
        zones.ready.clear();
        let (tx, rx) = mpsc::channel();
        zones.pending = Some(rx); // deterministic running job, held by this test
        for ev in [0.1, 0.2, 0.3] {
            p.exposure.ev = ev;
            assert!(zones.request(&proxy, &p, true, false).is_none());
        }
        assert_eq!(zones.rebuilds, 1, "running work spawned more workers");
        zones.invalidate();
        tx.send(Entry { key: old_key, data }).ok().unwrap();
        assert!(zones.request(&proxy, &p, true, false).is_none());
        let ready = zones.request(&proxy, &p, true, true).unwrap();
        assert_eq!(zones.rebuilds, 2, "obsolete slider positions were queued");
        assert_eq!(zones.ready.len(), 1);
        assert_eq!(zones.ready[0].key.basis.generation, 1);
        assert_eq!(*ready.basis, proxy.basis(&p.exposure, &p.contrast_mask));
    }
}
