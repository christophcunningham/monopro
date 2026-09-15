//! The app-level intermediate texture pool.
//!
//! vkdt's pool assumes one graph owns the device. This app will have up to eight
//! Develop tabs, so the pool belongs to the **app**, not to a viewport — the
//! handoff is firm about designing that in rather than retrofitting it. The
//! memory story it exists to deliver: N decoded scene images in RAM, roughly one
//! image's worth of GPU intermediates live.
//!
//! A fork is what makes a pool necessary rather than merely tidy. A linear chain
//! needs two buffers and can ping-pong between them; Contrast Mask needs the log
//! signal alive across the whole blur branch, so the number of live buffers stops
//! being a property of the code and becomes a property of the graph.

/// What a lease is for. Sizes are already bucketed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TexDesc {
    pub w: u32,
    pub h: u32,
    pub format: wgpu::TextureFormat,
}

/// A texture taken out of the pool. Owns its texture until handed back, so the
/// borrow checker enforces what a handle-and-index scheme would leave to
/// discipline: a buffer cannot be in the pool and in use at the same time.
#[derive(Debug)]
pub struct Lease {
    pub desc: TexDesc,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
}

/// Allocation granularity, in pixels.
///
/// Same reasoning as the viewport target's quantum: panning shifts a region's
/// origin every frame and aprons make its size wobble, so exact-fit allocation
/// would churn. Rounding up means a drag reallocates once per band instead of
/// once per frame, at the cost of a little unused margin the shaders never read.
const BUCKET: u32 = 128;

fn bucket(n: u32) -> u32 {
    n.div_ceil(BUCKET).max(1) * BUCKET
}

/// How many idle textures to keep. Enough for a forked graph's working set at
/// two or three recent sizes; beyond that, holding VRAM against a size that may
/// never recur costs more than re-creating the texture.
const CAP: usize = 12;
const DEFAULT_IDLE_BYTES: u64 = 256 * 1024 * 1024;

impl TexDesc {
    fn bytes(self) -> Option<u64> {
        let (bw, bh) = self.format.block_dimensions();
        u64::from(self.w.div_ceil(bw))
            .checked_mul(u64::from(self.h.div_ceil(bh)))?
            .checked_mul(u64::from(self.format.block_copy_size(None)?))
    }
}

#[derive(Debug)]
pub struct TexturePool {
    free: Vec<Lease>,
    idle_bytes: u64,
    byte_budget: u64,
    allocations: u64,
    reuses: u64,
}

impl Default for TexturePool {
    fn default() -> Self {
        Self::new()
    }
}

impl TexturePool {
    pub fn new() -> Self {
        Self::with_byte_budget(DEFAULT_IDLE_BYTES)
    }

    /// Budget for idle texel storage only; leased textures and driver overhead
    /// are not included. Oversized leases can be used but will not be retained.
    pub fn with_byte_budget(byte_budget: u64) -> Self {
        Self {
            free: Vec::new(),
            idle_bytes: 0,
            byte_budget,
            allocations: 0,
            reuses: 0,
        }
    }

    pub fn idle_bytes(&self) -> u64 {
        self.idle_bytes
    }

    /// Take a texture at least `w` x `h`. The result is bucketed and therefore
    /// usually larger; callers write only the region they own and bound-check on
    /// their own extent, exactly as the viewport target already does.
    pub fn acquire(
        &mut self,
        device: &wgpu::Device,
        w: u32,
        h: u32,
        format: wgpu::TextureFormat,
    ) -> Lease {
        let desc = TexDesc {
            w: bucket(w),
            h: bucket(h),
            format,
        };
        if let Some(i) = self.free.iter().rposition(|l| l.desc == desc) {
            self.reuses += 1;
            let lease = self.free.remove(i);
            self.idle_bytes -= lease.desc.bytes().expect("retained format has a byte size");
            return lease;
        }
        self.allocations += 1;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pooled intermediate"),
            size: wgpu::Extent3d {
                width: desc.w,
                height: desc.h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            // COPY_SRC so export can read the scene-referred signal straight out
            // of an intermediate rather than from the 8-bit display target.
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Lease {
            desc,
            texture,
            view,
        }
    }

    /// Keep recent returns; evict the oldest idle leases to meet both limits.
    pub fn release(&mut self, lease: Lease) {
        let Some(bytes) = lease
            .desc
            .bytes()
            .filter(|bytes| *bytes <= self.byte_budget)
        else {
            return;
        };
        while self.free.len() >= CAP || self.idle_bytes > self.byte_budget - bytes {
            let oldest = self.free.remove(0);
            self.idle_bytes -= oldest
                .desc
                .bytes()
                .expect("retained format has a byte size");
        }
        self.idle_bytes += bytes;
        self.free.push(lease);
    }

    /// Textures created since construction. A test asserting this stays flat
    /// across frames is what proves the pool is a pool.
    pub fn allocations(&self) -> u64 {
        self.allocations
    }

    pub fn reuses(&self) -> u64 {
        self.reuses
    }

    pub fn idle(&self) -> usize {
        self.free.len()
    }

    /// Sizes currently held. Lets a test see what the ROI actually asked for,
    /// which is otherwise invisible from outside.
    pub fn idle_descs(&self) -> impl Iterator<Item = TexDesc> + '_ {
        self.free.iter().map(|l| l.desc)
    }

    /// Drop every idle texture. For a tab losing focus: background tabs hold
    /// their params and their cached `SceneImage` but release GPU intermediates.
    pub fn clear(&mut self) {
        self.free.clear();
        self.idle_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_bucket_so_a_drag_does_not_churn() {
        assert_eq!(bucket(1), BUCKET);
        assert_eq!(bucket(128), 128);
        assert_eq!(bucket(129), 256);
        for n in 130..=256 {
            assert_eq!(bucket(n), 256, "size {n} left the band");
        }
    }

    #[test]
    fn a_bucket_is_never_zero() {
        // Zoomed far enough out a region is one pixel, and wgpu rejects a
        // zero-sized texture.
        assert_eq!(bucket(0), BUCKET);
    }
}
