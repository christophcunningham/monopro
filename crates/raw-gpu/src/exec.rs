//! Compiled compute passes and the executor that walks a `Plan`.
//!
//! One `Pass` per node **kind**, compiled once and shared by every tab. Eight
//! Develop tabs must not mean eight copies of the same pipeline; that is the
//! other half of what "app-level" means, alongside the texture pool.
//!
//! The executor is deliberately dumb: the graph has already decided what runs,
//! in what order, and which buffers die when. All that is left here is binding
//! resources and recording dispatches.

use crate::pool::{Lease, TexturePool};
use raw_graph::{NodeId, NodeKind, Plan};
use std::collections::HashMap;

/// A node kind's pipeline identity. Payload-free: `Blur { support: 4.0 }` and
/// `Blur { support: 90.0 }` are the same shader with different uniforms, and
/// compiling one per radius would recompile on every slider frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PassKey {
    Input,
    Exposure,
    Log2,
    Exp2,
    Blur,
    ContrastMask,
    DodgeBurn,
    Curve,
    Display,
}

impl PassKey {
    fn of(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Input => Self::Input,
            NodeKind::Exposure => Self::Exposure,
            NodeKind::Log2 => Self::Log2,
            NodeKind::Exp2 => Self::Exp2,
            // One pipeline for both axes: the axis is a uniform, so a slider
            // drag does not recompile anything.
            NodeKind::Blur { .. } => Self::Blur,
            NodeKind::ContrastMask { .. } => Self::ContrastMask,
            NodeKind::DodgeBurn => Self::DodgeBurn,
            NodeKind::Curve => Self::Curve,
            NodeKind::Display => Self::Display,
        }
    }
}

/// What a pass writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutKind {
    /// `Rg32Float` intermediate: value and coverage.
    Work,
    /// `Rgba8Unorm` viewport target.
    Display,
}

/// The resources a pass binds, which is all that distinguishes one bind group
/// layout from another. Derived once and used to build both the layout and every
/// bind group, so the two cannot drift apart.
#[derive(Debug, Clone, Copy)]
struct Spec {
    inputs: usize,
    out: OutKind,
    lut: bool,
    tone_lut: bool,
    histogram: bool,
    /// Binds the dab buffer, the instance buffer and the zone-mask strip.
    ///
    /// Grouped as one flag rather than three because they are one payload: a pass
    /// that reads strokes reads all three, and a pass that does not reads none.
    strokes: bool,
}

impl Spec {
    /// An ordinary pass: N texture inputs, a working-space output, nothing else.
    const fn plain(inputs: usize) -> Self {
        Self {
            inputs,
            out: OutKind::Work,
            lut: false,
            tone_lut: false,
            histogram: false,
            strokes: false,
        }
    }

    fn of(key: PassKey) -> Self {
        match key {
            PassKey::Input => Spec::plain(1),
            PassKey::Exposure => Spec::plain(1),
            PassKey::Log2 => Spec::plain(1),
            PassKey::Exp2 => Spec::plain(1),
            PassKey::Blur => Spec::plain(1),
            // The join: negative and mask.
            PassKey::ContrastMask => Spec::plain(2),
            // Negative plus the shared blurred-log detail base. When every layer's
            // Contrast is zero the graph binds the negative in both slots and the
            // shader skips the second read.
            PassKey::DodgeBurn => Spec {
                strokes: true,
                ..Spec::plain(2)
            },
            PassKey::Curve => Spec {
                lut: true,
                ..Spec::plain(1)
            },
            // Toning rides in the display pass because that is where the tone map
            // is, and a toner acts on a print. See `raw_core::toning`.
            PassKey::Display => Spec {
                out: OutKind::Display,
                tone_lut: true,
                histogram: true,
                ..Spec::plain(2)
            },
        }
    }
}

/// Binding slots, fixed across every shader so the shared `PRELUDE` and the
/// per-shader declarations agree.
///
/// - 0: first input texture
/// - 1: `Rg32Float` storage output
/// - 2: `Rgba8Unorm` storage output
/// - 3: curve LUT storage buffer
/// - 4: uniforms
/// - 5: second input texture (Contrast Mask's mask)
/// - 6: dab storage buffer
/// - 7: instance storage buffer
/// - 8: zone-mask strip
const B_IN0: u32 = 0;
const B_WORK_OUT: u32 = 1;
const B_DISPLAY_OUT: u32 = 2;
const B_LUT: u32 = 3;
const B_UNIFORMS: u32 = 4;
const B_IN1: u32 = 5;
const B_DABS: u32 = 6;
const B_INSTANCES: u32 = 7;
const B_MASKS: u32 = 8;
/// The toning table. Its own slot rather than sharing `B_LUT`, because the curve and
/// the display pass can both want a table in one plan — and **after** the uniforms at
/// 4, which is why it is not the apparent gap in the sequence above.
const B_TONE_LUT: u32 = 9;
const B_HISTOGRAM: u32 = 10;

pub struct Pass {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    spec: Spec,
}

/// Everything the executor needs that the graph does not own.
pub struct Resources<'a> {
    /// The stored luminance image. Read by the input node only.
    pub source: &'a wgpu::TextureView,
    /// Sensor-clipping counts in the stored image's geometry. Read by Display only.
    pub clipping: &'a wgpu::TextureView,
    /// The viewport target. Written by the sink only.
    pub target: &'a wgpu::TextureView,
    /// One `NodeParams` block per step, at `UNIFORM_STRIDE` intervals in step
    /// order. Written by the caller, which is what knows the module parameters;
    /// the regions come from the plan.
    pub uniforms: &'a wgpu::Buffer,
    pub lut: &'a wgpu::Buffer,
    /// The baked toning table, as flat `[y, a, b]` triples. Read by the display pass.
    pub tone_lut: &'a wgpu::Buffer,
    pub histogram: &'a wgpu::Buffer,
    /// Dabs, sorted by (instance, pass) — the order the shader's linear scan
    /// depends on. Never empty: a zero-length storage buffer is not bindable, so
    /// an unpainted image carries one inert dab. See `Viewport::write_strokes`.
    pub dabs: &'a wgpu::Buffer,
    pub instances: &'a wgpu::Buffer,
    /// Zone masks, stacked vertically. A 1x1 texture when nothing is masked.
    pub masks: &'a wgpu::TextureView,
}

/// Device-level state shared by every tab: compiled pipelines and the
/// intermediate texture pool.
///
/// This crate does not create the device. It borrows the one eframe already has,
/// which is what "app-level" means in practice.
pub struct GpuContext {
    passes: HashMap<PassKey, Pass>,
    pool: TexturePool,
}

impl GpuContext {
    pub fn new(device: &wgpu::Device) -> Self {
        let mut passes = HashMap::new();
        for (key, body) in [
            (PassKey::Input, include_str!("sample.wgsl")),
            (PassKey::Exposure, include_str!("exposure.wgsl")),
            (PassKey::Log2, include_str!("log2.wgsl")),
            (PassKey::Exp2, include_str!("exp2.wgsl")),
            (PassKey::Blur, include_str!("blur.wgsl")),
            (PassKey::ContrastMask, include_str!("contrast_mask.wgsl")),
            (PassKey::DodgeBurn, include_str!("dodge_burn.wgsl")),
            (PassKey::Curve, include_str!("curve.wgsl")),
            (PassKey::Display, include_str!("display.wgsl")),
        ] {
            passes.insert(key, Pass::new(device, key, body));
        }
        Self {
            passes,
            pool: TexturePool::new(),
        }
    }

    pub fn pool(&self) -> &TexturePool {
        &self.pool
    }

    pub fn pool_mut(&mut self) -> &mut TexturePool {
        &mut self.pool
    }

    /// Record and submit every step of `plan`.
    ///
    /// `retain` names a node whose output should be handed back rather than
    /// returned to the pool — the export tap. Export needs the scene-referred
    /// signal the display node consumed, and asking the graph for it beats the
    /// milestone-2 approach of re-deriving which of two buffers held it from
    /// `params.curve.is_identity()`, which was the same decision made in a
    /// second place.
    pub fn execute(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        plan: &Plan,
        res: &Resources<'_>,
        retain: Option<NodeId>,
    ) -> Option<Lease> {
        let n = plan.steps.len();

        // Buffers currently live, indexed by node. `None` means either not yet
        // produced or already released.
        let mut live: Vec<Option<Lease>> = (0..n).map(|_| None).collect();
        // Bind groups must outlive the passes that reference them, so they are
        // collected first and the dispatches recorded afterwards.
        let mut recorded: Vec<(PassKey, wgpu::BindGroup, (u32, u32))> = Vec::with_capacity(n);
        let mut retained: Option<Lease> = None;

        for (i, step) in plan.steps.iter().enumerate() {
            let key = PassKey::of(step.kind);
            let is_sink = i + 1 == n;

            // Each node's buffer is sized to its OWN region, which with an apron
            // is larger than the viewport and with a fitted view is smaller.
            let lease = (!is_sink).then(|| {
                self.pool.acquire(
                    device,
                    step.out.w,
                    step.out.h,
                    wgpu::TextureFormat::Rg32Float,
                )
            });

            {
                let pass = &self.passes[&key];
                let out_view = match &lease {
                    Some(l) => &l.view,
                    None => res.target,
                };
                // The input node has no graph inputs: it reads the stored image.
                // Everything else reads a producer's buffer.
                let mut inputs: Vec<&wgpu::TextureView> = if step.inputs.is_empty() {
                    vec![res.source]
                } else {
                    step.inputs
                        .iter()
                        .map(|(p, _)| {
                            live[p.0]
                                .as_ref()
                                .map(|l| &l.view)
                                .expect("a producer's buffer was released too early")
                        })
                        .collect()
                };
                // A diagnostic of the negative, not a graph edge: Display reads the
                // decode mask beside its ordinary producer input.
                if key == PassKey::Display {
                    inputs.push(res.clipping);
                }
                let offset = i as u64 * crate::UNIFORM_STRIDE;
                let groups = (step.out.w.div_ceil(8), step.out.h.div_ceil(8));
                recorded.push((
                    key,
                    pass.bind(device, &inputs, out_view, res, offset),
                    groups,
                ));
            }

            if let Some(l) = lease {
                live[i] = Some(l);
            }

            // Return buffers whose last reader has now run. Safe within a frame:
            // the reuse is a later node WRITING the texture, and the compute pass
            // boundary orders that after this node's read. It is the same
            // ping-pong the hardcoded chain did with two fixed buffers.
            for id in &step.release {
                if retain == Some(*id) {
                    retained = live[id.0].take();
                    continue;
                }
                if let Some(l) = live[id.0].take() {
                    self.pool.release(l);
                }
            }
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("graph"),
        });
        for (key, bind_group, groups) in &recorded {
            // A separate compute pass per node, not one pass with N dispatches:
            // each write must be visible to the next read, and a pass boundary is
            // the barrier that guarantees it.
            let mut cpass = encoder.begin_compute_pass(&Default::default());
            cpass.set_pipeline(&self.passes[key].pipeline);
            cpass.set_bind_group(0, bind_group, &[]);
            cpass.dispatch_workgroups(groups.0, groups.1, 1);
        }
        queue.submit([encoder.finish()]);

        // Anything still live had no consumer downstream of it — only the sink,
        // whose target is not pooled, and the retained tap.
        for slot in live.into_iter().flatten() {
            self.pool.release(slot);
        }
        retained
    }
}

impl Pass {
    fn new(device: &wgpu::Device, key: PassKey, body: &str) -> Self {
        let spec = Spec::of(key);
        let label = format!("{key:?}").to_lowercase();

        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage = |binding, format| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        };

        let mut entries = vec![
            texture(B_IN0),
            wgpu::BindGroupLayoutEntry {
                binding: B_UNIFORMS,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    // The offset is baked into each bind group rather than set
                    // dynamically: a bind group is built per node per frame
                    // anyway, since the input and output views differ, so a
                    // dynamic offset would add machinery and save nothing.
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(
                        std::mem::size_of::<crate::NodeParams>() as u64,
                    ),
                },
                count: None,
            },
        ];
        match spec.out {
            OutKind::Work => entries.push(storage(B_WORK_OUT, wgpu::TextureFormat::Rg32Float)),
            OutKind::Display => {
                entries.push(storage(B_DISPLAY_OUT, wgpu::TextureFormat::Rgba8Unorm))
            }
        }
        for (want, binding) in [(spec.lut, B_LUT), (spec.tone_lut, B_TONE_LUT)] {
            if !want {
                continue;
            }
            entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }
        if spec.histogram {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: B_HISTOGRAM,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: false },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }
        if spec.inputs > 1 {
            entries.push(texture(B_IN1));
        }
        if spec.strokes {
            for binding in [B_DABS, B_INSTANCES] {
                entries.push(wgpu::BindGroupLayoutEntry {
                    binding,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                });
            }
            entries.push(texture(B_MASKS));
        }

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(&label),
            entries: &entries,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(&label),
            source: wgpu::ShaderSource::Wgsl(format!("{}\n{body}", crate::PRELUDE).into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&label),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(&label),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Pass {
            pipeline,
            layout,
            spec,
        }
    }

    fn bind(
        &self,
        device: &wgpu::Device,
        inputs: &[&wgpu::TextureView],
        out: &wgpu::TextureView,
        res: &Resources<'_>,
        uniform_offset: u64,
    ) -> wgpu::BindGroup {
        debug_assert_eq!(
            inputs.len(),
            self.spec.inputs,
            "input count must match the layout"
        );
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: B_IN0,
                resource: wgpu::BindingResource::TextureView(inputs[0]),
            },
            wgpu::BindGroupEntry {
                binding: B_UNIFORMS,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: res.uniforms,
                    offset: uniform_offset,
                    size: wgpu::BufferSize::new(std::mem::size_of::<crate::NodeParams>() as u64),
                }),
            },
        ];
        let out_binding = match self.spec.out {
            OutKind::Work => B_WORK_OUT,
            OutKind::Display => B_DISPLAY_OUT,
        };
        entries.push(wgpu::BindGroupEntry {
            binding: out_binding,
            resource: wgpu::BindingResource::TextureView(out),
        });
        for (want, binding, buffer) in [
            (self.spec.lut, B_LUT, res.lut),
            (self.spec.tone_lut, B_TONE_LUT, res.tone_lut),
        ] {
            if want {
                entries.push(wgpu::BindGroupEntry {
                    binding,
                    resource: buffer.as_entire_binding(),
                });
            }
        }
        if self.spec.histogram {
            entries.push(wgpu::BindGroupEntry {
                binding: B_HISTOGRAM,
                resource: res.histogram.as_entire_binding(),
            });
        }
        if self.spec.inputs > 1 {
            entries.push(wgpu::BindGroupEntry {
                binding: B_IN1,
                resource: wgpu::BindingResource::TextureView(inputs[1]),
            });
        }
        if self.spec.strokes {
            entries.push(wgpu::BindGroupEntry {
                binding: B_DABS,
                resource: res.dabs.as_entire_binding(),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: B_INSTANCES,
                resource: res.instances.as_entire_binding(),
            });
            entries.push(wgpu::BindGroupEntry {
                binding: B_MASKS,
                resource: wgpu::BindingResource::TextureView(res.masks),
            });
        }
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &entries,
        })
    }
}
