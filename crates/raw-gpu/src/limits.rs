//! Preflight every derived allocation before calling wgpu. The conservative sum
//! counts even mutually reusable textures, so it bounds a plan without depending
//! on the pool's current contents or the executor's lifetime optimizations.
use raw_graph::Plan;

const WORKING_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) fn validate_plan(plan: &Plan, max_edge: u32) -> Result<u64, String> {
    let mut bytes = 0u64;
    for (i, step) in plan.steps.iter().enumerate() {
        let w = u64::from(step.out.w).max(1).div_ceil(128) * 128;
        let h = u64::from(step.out.h).max(1).div_ceil(128) * 128;
        if w > u64::from(max_edge) || h > u64::from(max_edge) {
            return Err("This view exceeds the graphics device's image-size limit. Reduce the window size or zoom.".into());
        }
        let pixel_bytes = if i + 1 == plan.steps.len() { 4 } else { 8 };
        let storage = w
            .checked_mul(h)
            .and_then(|n| n.checked_mul(pixel_bytes))
            .ok_or_else(|| "This view exceeds the renderer's memory limit.".to_string())?;
        bytes = bytes
            .checked_add(storage)
            .ok_or_else(|| "This view exceeds the renderer's memory limit.".to_string())?;
        if bytes > WORKING_BYTES {
            return Err(
                "This view needs too much graphics memory. Reduce the window size or zoom.".into(),
            );
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::Params;
    use raw_graph::{View, build};

    fn plan(scale: f32, spacer: f32, size: (u32, u32)) -> Plan {
        let mut p = Params::default();
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = spacer;
        build(&p, (11648, 8736))
            .resolve(
                (11648, 8736),
                View {
                    scale,
                    off_x: 5824.0 - size.0 as f32 / (2.0 * scale),
                    off_y: 4368.0 - size.1 as f32 / (2.0 * scale),
                    out_w: size.0,
                    out_h: size.1,
                    crop: None,
                },
            )
            .unwrap()
    }

    #[test]
    fn medium_format_masks_stay_bounded_through_maximum_zoom() {
        for spacer in [1.5, 5.0] {
            for scale in [0.183, 1.0, 4.0, 16.0] {
                let p = plan(scale, spacer, (2560, 1600));
                let bytes = validate_plan(&p, 8192).unwrap();
                assert!(bytes < 256 * 1024 * 1024, "{scale}x: {bytes}");
                for step in &p.steps {
                    assert!(
                        step.out.w <= 2816 && step.out.h <= 1856,
                        "{scale}x: {step:?}"
                    );
                }
            }
        }
        // A 4K display is supported too, without increasing the idle pool budget.
        validate_plan(&plan(16.0, 5.0, (3840, 2160)), 8192).unwrap();
    }

    #[test]
    fn reject_dimensions_and_memory_before_allocating() {
        let mut p = plan(4.0, 5.0, (2560, 1600));
        p.steps[0].out.w = 8193;
        assert!(validate_plan(&p, 8192).unwrap_err().contains("image-size"));
        p.steps[0].out.w = u32::MAX;
        assert!(validate_plan(&p, 16384).is_err());
        // Bucket rounding counts against the actual device limit.
        p.steps[0].out.w = 8100;
        assert!(validate_plan(&p, 8128).is_err());
        let p = plan(16.0, 5.0, (8192, 8192));
        assert!(validate_plan(&p, 16384).unwrap_err().contains("memory"));
    }
}
