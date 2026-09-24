//! The node graph: topology, typed connectors, and ROI propagation.
//!
//! **No wgpu here, deliberately.** ROI arithmetic is the most bug-prone part of the
//! pipeline, and `raw-gpu`'s tests skip when there is no adapter. Everything here is
//! arithmetic over a DAG, so its tests run everywhere, always.
//!
//! # The graph is a pure function of `Params`
//!
//! There is no node editor. `build` turns a `Params` into a `Graph` and is called
//! afresh whenever anything changes; nothing mutates a graph in place. That is
//! what keeps the sidecar a flat list of settings, undo a stack of parameter
//! snapshots, and tab duplication a `Params::clone`.
//!
//! # The two propagation passes
//!
//! ```text
//!   forward  (roi_out)   source -> sink   what grid does each node produce on?
//!   backward (roi_in)    sink -> source   what region does each node need?
//! ```
//!
//! Forward resolves each grid, including the reduced Contrast Mask branch.
//! Backward converts each node's output
//! request into a request on each input, expanding by its reach, clamping to the
//! image, and **unioning** with whatever other consumers already asked for.
//!
//! The union is the part a linear chain never needed. Contrast Mask consumes the
//! log signal through direct and blurred branches for small kernels. Wide masks
//! instead read the source on a separate bounded grid, so their aprons do not
//! inflate the full-resolution negative.

pub mod node;
pub mod roi;

pub use node::{Axis, NodeKind, StageRole};
pub use roi::{Apron, Roi};

use raw_core::Params;

/// Index into `Graph::nodes`. Always less than the index of any node consuming
/// it, which is what makes the graph acyclic by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone, PartialEq)]
struct Node {
    kind: NodeKind,
    inputs: Vec<NodeId>,
}

/// A pipeline, as topology only. Cheap to build, cheap to compare.
#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    nodes: Vec<Node>,
    sink: NodeId,
}

/// Where the viewport is looking, in the terms the app already speaks.
///
/// `off` is in **source** pixels and fractional, exactly as `ViewGeometry` has
/// always been. `resolve` splits it into an integer region origin and a
/// sub-pixel phase, because a region has to be integral for a spatial operation
/// to be reproducible while panning must stay smooth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Output pixels per source pixel. 1.0 is 100%.
    pub scale: f32,
    pub off_x: f32,
    pub off_y: f32,
    /// Region to produce, in output pixels. This is the viewport, which may be
    /// larger than the image — the excess is the surround.
    pub out_w: u32,
    pub out_h: u32,
    /// The crop, in **frame** pixels at scale 1. `None` is the whole frame — the
    /// absence of a constraint, not a rectangle the size of one.
    ///
    /// Restricts what the chain produces without shrinking the grid it runs on. What
    /// it does *not* do: clamp aprons, change `full`, or reach the sink's own region —
    /// the sink still paints a whole viewport of surround. See [`Roi::intersect`].
    pub crop: Option<(i32, i32, u32, u32)>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GraphError {
    #[error("node {node} ({kind}) takes {expected} inputs but was given {got}")]
    Arity {
        node: usize,
        kind: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("node {node} reads node {input}, which is not upstream of it")]
    NotTopological { node: usize, input: usize },
    #[error("node {node} ({kind}) input {index} cannot accept {got:?}")]
    RoleMismatch {
        node: usize,
        kind: &'static str,
        index: usize,
        got: StageRole,
    },
    #[error("the chain must end display-referred, but the sink produces {got:?}")]
    SinkNotDisplay { got: StageRole },
    #[error("node {node} ({kind}) is not reachable from the sink")]
    Unreachable { node: usize, kind: &'static str },
}

/// One node's place in the resolved schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: NodeId,
    pub kind: NodeKind,
    pub role: StageRole,
    /// The region this node must write.
    pub out: Roi,
    /// For each input, the producer and **the region that producer actually
    /// writes** — which may be larger than this node asked for, because the
    /// producer satisfies the union of every consumer.
    ///
    /// This is why a node needs its input's origin and not just its own: the
    /// buffer it reads is not necessarily aligned to the region it produces.
    pub inputs: Vec<(NodeId, Roi)>,
    /// Producers whose buffer is dead once this step has run, and may go back to
    /// the pool. Pure topology, so it is computed here rather than guessed at by
    /// the executor.
    pub release: Vec<NodeId>,
}

/// A resolved, ready-to-execute schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// Sub-pixel pan phase, in output-grid pixels, `[0, 1)`.
    ///
    /// Consumed by the input node alone. Everything downstream works on an
    /// integral grid, which is what makes a spatial operation give the same
    /// answer for the same pixel regardless of where the pan happens to be.
    pub subpixel: (f32, f32),
}

impl Plan {
    pub fn sink(&self) -> &Step {
        self.steps.last().expect("a plan always has a sink")
    }

    /// Total pixels written across every step. The honest cost of a graph,
    /// aprons included — a fork with a wide blur writes noticeably more than the
    /// viewport.
    pub fn pixels_written(&self) -> u64 {
        self.steps.iter().map(|s| s.out.area()).sum()
    }
}

impl Graph {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            sink: NodeId(0),
        }
    }

    /// Append a node reading `inputs`. Inputs must already exist, which is what
    /// keeps index order topological.
    fn add(&mut self, kind: NodeKind, inputs: &[NodeId]) -> NodeId {
        self.nodes.push(Node {
            kind,
            inputs: inputs.to_vec(),
        });
        NodeId(self.nodes.len() - 1)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn kind(&self, id: NodeId) -> NodeKind {
        self.nodes[id.0].kind
    }

    /// The role each node produces, in index order. Also the validation pass:
    /// arity, topological order, and connector types are all checked here.
    fn roles(&self) -> Result<Vec<StageRole>, GraphError> {
        let mut roles: Vec<StageRole> = Vec::with_capacity(self.nodes.len());
        for (i, node) in self.nodes.iter().enumerate() {
            if node.inputs.len() != node.kind.arity() {
                return Err(GraphError::Arity {
                    node: i,
                    kind: node.kind.label(),
                    expected: node.kind.arity(),
                    got: node.inputs.len(),
                });
            }
            let mut in_roles = Vec::with_capacity(node.inputs.len());
            for (k, &p) in node.inputs.iter().enumerate() {
                if p.0 >= i {
                    return Err(GraphError::NotTopological {
                        node: i,
                        input: p.0,
                    });
                }
                let role = roles[p.0];
                if !node.kind.accepts(k, role) {
                    return Err(GraphError::RoleMismatch {
                        node: i,
                        kind: node.kind.label(),
                        index: k,
                        got: role,
                    });
                }
                in_roles.push(role);
            }
            roles.push(node.kind.out_role(&in_roles));
        }
        Ok(roles)
    }

    /// Check topology and connector types without resolving any regions.
    pub fn validate(&self) -> Result<(), GraphError> {
        let roles = self.roles()?;
        let sink = roles[self.sink.0];
        if sink != StageRole::Display {
            return Err(GraphError::SinkNotDisplay { got: sink });
        }
        Ok(())
    }

    /// Resolve every node's region for one view of one source image.
    ///
    /// Forward for grids, backward for regions, then a schedule with buffer lifetimes.
    pub fn resolve(&self, source: (u32, u32), view: View) -> Result<Plan, GraphError> {
        let roles = self.roles()?;
        let sink_role = roles[self.sink.0];
        if sink_role != StageRole::Display {
            return Err(GraphError::SinkNotDisplay { got: sink_role });
        }

        let root = (Roi::grid_for(source, view.scale), view.scale);

        // Forward: what grid does each node produce on?
        let mut grids: Vec<((u32, u32), f32)> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let upstream = node.inputs.first().map(|p| grids[p.0]);
            let node_root = if matches!(node.kind, NodeKind::MaskInput { .. }) {
                let scale = view.scale.min(1.0);
                (Roi::grid_for(source, scale), scale)
            } else {
                root
            };
            grids.push(node.kind.out_grid(upstream, node_root));
        }

        // The sink's request. Split the fractional pan into an integral region
        // origin and a sub-pixel phase the input node applies.
        let gx = view.off_x * view.scale;
        let gy = view.off_y * view.scale;
        let (ix, iy) = (gx.floor(), gy.floor());
        let subpixel = (gx - ix, gy - iy);
        let (sink_full, sink_scale) = grids[self.sink.0];

        let mut req: Vec<Option<Roi>> = vec![None; self.nodes.len()];
        // Deliberately NOT clamped to the image. The viewport can be larger than
        // the image — at fit-to-window it always is — and the excess is the
        // surround the display node paints. Every *derived* request below is
        // clamped; only the sink's own output may exceed its grid.
        req[self.sink.0] = Some(Roi::window(
            sink_full, sink_scale, ix as i32, iy as i32, view.out_w, view.out_h,
        ));

        // The crop, on the sink's grid. Scaled the same way the image is, so it
        // tracks the zoom exactly and needs no separate rounding rule.
        let crop = view.crop.map(|(cx, cy, cw, ch)| {
            let x = (cx as f32 * sink_scale).floor() as i32;
            let y = (cy as f32 * sink_scale).floor() as i32;
            // `ceil` on the far edge against `floor` on the near one, so rounding
            // can only ever make the crop a hair larger. The other way round drops
            // the last column at some zoom levels, which reads as a one-pixel line
            // of surround eating into the picture — and this app has now shipped
            // that bug at four separate boundaries.
            let x1 = roi::ceil_px((cx + cw as i32) as f32 * sink_scale) as i32;
            let y1 = roi::ceil_px((cy + ch as i32) as f32 * sink_scale) as i32;
            Roi::window(
                sink_full,
                sink_scale,
                x,
                y,
                (x1 - x).max(1) as u32,
                (y1 - y).max(1) as u32,
            )
        });

        // Backward. Reverse index order is reverse topological order, so every
        // consumer of a node has already contributed by the time we reach it.
        for i in (0..self.nodes.len()).rev() {
            let node = &self.nodes[i];
            let Some(out) = req[i] else {
                return Err(GraphError::Unreachable {
                    node: i,
                    kind: node.kind.label(),
                });
            };
            // **The crop applies here and nowhere else.** The sink keeps the whole
            // viewport as its own region, because outside the crop it paints the
            // surround; what it *asks its input for* is the part that will survive.
            // Intersecting before the expansion below is what lets an apron reach
            // past the crop boundary into real pixels — see `Roi::intersect`.
            let out = match crop {
                Some(c) if i == self.sink.0 => out.intersect(c),
                _ => out,
            };
            for (k, &p) in node.inputs.iter().enumerate() {
                let (pfull, pscale) = grids[p.0];
                let want = input_region(node.kind, k, out, pfull, pscale);
                req[p.0] = Some(match req[p.0] {
                    Some(existing) => existing.union(want),
                    None => want,
                });
            }
        }

        // Last consumer of each node, for buffer reuse.
        let mut last_use: Vec<Option<usize>> = vec![None; self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            for &p in &node.inputs {
                last_use[p.0] = Some(i);
            }
        }

        let steps = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| Step {
                id: NodeId(i),
                kind: node.kind,
                role: roles[i],
                out: req[i].expect("every node was reached"),
                inputs: node
                    .inputs
                    .iter()
                    .map(|&p| (p, req[p.0].expect("upstream")))
                    .collect(),
                release: last_use
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| **u == Some(i))
                    .map(|(p, _)| NodeId(p))
                    .collect(),
            })
            .collect::<Vec<_>>();

        // The apron contract, asserted rather than assumed: whatever a node will
        // read must be inside the buffer its producer wrote.
        //
        // The sink is checked against its *cropped* region, matching what it asked
        // for above. Checking it against its whole region instead would demand a
        // buffer covering the surround, which has no scene data behind it by
        // construction — the assertion would fail on every cropped frame while
        // nothing was wrong.
        debug_assert!(
            steps.iter().all(|s| {
                let out = match crop {
                    Some(c) if s.id == self.sink => s.out.intersect(c),
                    _ => s.out,
                };
                s.inputs.iter().enumerate().all(|(k, (_, got))| {
                    let needed = input_region(s.kind, k, out, got.full, got.scale);
                    got.contains(needed)
                })
            }),
            "a node would read outside its input buffer"
        );

        Ok(Plan { steps, subpixel })
    }
}

fn input_region(kind: NodeKind, index: usize, out: Roi, full: (u32, u32), scale: f32) -> Roi {
    let mut want = out
        .expand(kind.apron(index, out.scale))
        .on_grid(full, scale);
    if matches!(kind, NodeKind::ContrastMask { .. }) && index == 1 && scale != out.scale {
        // Bilinear reconstruction reads the neighbouring reduced-grid texels.
        want = want.expand(Apron::uniform(1));
    }
    want.clamp_to_full()
}

/// Turn parameters into a pipeline. A module that is off is not emitted at all,
/// rather than emitted and skipped.
///
/// `source_dims` is the **luminance image's** size, despite the name, and it is here
/// because a percentage-of-frame spacer cannot become an allocation size without it.
/// Two modules read it now. **Do not widen this argument again** — a third would be
/// the moment to reconsider the shape instead.
pub fn build(params: &Params, source_dims: (u32, u32)) -> Graph {
    build_with_mask_source(params, source_dims, true)
}

/// Small fitted previews can share the negative's already sampled log signal.
/// Larger views must use the independent mask source to avoid expanded buffers.
/// Allocation preflight remains required for either choice.
pub fn build_with_mask_source(
    params: &Params,
    source_dims: (u32, u32),
    independent: bool,
) -> Graph {
    let mut g = Graph::new();
    let mut n = g.add(NodeKind::Input, &[]);
    n = g.add(NodeKind::Exposure, &[n]);

    // The fork, and the reason the graph exists: the log signal is consumed twice, once
    // directly and once blurred, which a linear chain cannot express without
    // special-casing.
    //
    //   exposure ─► log2 ─┬───────────────────────► mask ─► exp2 ─► ...
    //                     └─ blur.x ─► blur.y ────►
    let cm = params.contrast_mask;
    if cm.is_active() {
        // Keep the Gaussian's physical radius. Wide masks sample exposed log
        // luminance directly on a bounded grid, without allocating the expanded
        // negative first. Small masks retain the ordinary shared-log branch.
        let sigma = cm.spacer_px(source_dims);
        let support = 3.0 * sigma;
        let log = g.add(NodeKind::Log2, &[n]);
        let mask_input = if independent && sigma > 16.0 {
            g.add(NodeKind::MaskInput { sigma }, &[])
        } else {
            log
        };
        let bx = g.add(
            NodeKind::Blur {
                axis: Axis::X,
                sigma,
                support,
            },
            &[mask_input],
        );
        let by = g.add(
            NodeKind::Blur {
                axis: Axis::Y,
                sigma,
                support,
            },
            &[bx],
        );
        let masked = g.add(
            NodeKind::ContrastMask {
                contrast: cm.contrast,
                offset: cm.offset,
            },
            &[log, by],
        );
        n = g.add(NodeKind::Exp2, &[masked]);
    }

    // Dodge & Burn goes **after Contrast Mask, before the curve** — the darkroom order,
    // and not the tempting one beside the global exposure it locally modifies. Putting
    // it first would make burning a sky change the mask that sky prints through.
    // `raw_core::zone::Basis` builds its proxy in this same order so the zone masks
    // read the tonality the user is looking at.
    //
    //   exposure ─► [contrast mask] ─► dodge & burn ─► curve ─► display
    if params.dodgeburn.is_active() {
        let detail = if params.dodgeburn.has_contrast() {
            let sigma = raw_core::DodgeBurnParams::contrast_sigma_px(source_dims);
            let support = 3.0 * sigma;
            let log = g.add(NodeKind::Log2, &[n]);
            let bx = g.add(
                NodeKind::Blur {
                    axis: Axis::X,
                    sigma,
                    support,
                },
                &[log],
            );
            g.add(
                NodeKind::Blur {
                    axis: Axis::Y,
                    sigma,
                    support,
                },
                &[bx],
            )
        } else {
            // The pass has one fixed two-input layout. Binding the negative twice
            // avoids paying for a detail decomposition when Contrast is zero; the
            // shader does not read the second input in that case.
            n
        };
        n = g.add(NodeKind::DodgeBurn, &[n, detail]);
    }

    // `is_active`, not `!is_identity`: a bypassed curve must emit no node either.
    // `Params::effective` already substitutes the default and would make this true
    // anyway, but the graph should not depend on having been handed effective
    // params to build the right chain.
    if params.curve.is_active() {
        n = g.add(NodeKind::Curve, &[n]);
    }
    g.sink = g.add(NodeKind::Display, &[n]);
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_mask_blurs_a_reduced_grid_without_reducing_the_negative() {
        let mut p = Params::default();
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = 5.0;
        let source = (11648, 8736);
        let plan = build(&p, source)
            .resolve(source, view(1.0, 4000.0, 3000.0, 2560, 1600))
            .unwrap();
        let join = plan
            .steps
            .iter()
            .find(|s| matches!(s.kind, NodeKind::ContrastMask { .. }))
            .unwrap();
        assert_eq!(join.out.scale, 1.0);
        assert_eq!(join.inputs[0].1.scale, 1.0);
        assert_eq!(join.inputs[1].1.scale, 1.0 / 32.0);
        for blur in plan
            .steps
            .iter()
            .filter(|s| matches!(s.kind, NodeKind::Blur { .. }))
        {
            assert!(
                blur.out.area() < 100_000,
                "wide blur still runs at viewport resolution"
            );
        }
        let preview = build(&p, source)
            .resolve(source, view(0.183, 0.0, 0.0, 2560, 1600))
            .unwrap();
        assert!(
            preview.steps.iter().all(|s| s.out.scale == 0.183),
            "fit preview must retain its already-filtered boundary samples"
        );
    }

    #[test]
    fn a_reduced_mask_resolves_small_tiles_crops_and_fractional_pans() {
        let mut p = Params::default();
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = 5.0;
        let source = (11649, 8737);
        for scale in [0.07, 0.183, 1.0, 2.0] {
            for (x, y) in [(0.0, 0.0), (5001.25, 3701.75), (11648.0, 8736.0)] {
                let mut v = view(scale, x, y, 1, 1);
                v.crop = Some((200, 200, 10000, 7000));
                let plan = build(&p, source).resolve(source, v).unwrap();
                for step in plan.steps.iter().filter(|s| s.kind != NodeKind::Display) {
                    // A view wholly outside the crop may request an empty region.
                    assert!(step.out.x >= 0 && step.out.y >= 0);
                    assert!(step.out.right() <= step.out.full.0 as i32);
                    assert!(step.out.bottom() <= step.out.full.1 as i32);
                }
            }
        }
    }

    /// Frame size for `build`. Only Contrast Mask reads it, and only to turn its
    /// percentage spacer into pixels — the diagonal is exactly 1000, so 1% is
    /// 10px and the arithmetic in these tests stays checkable by eye.
    const FRAME: (u32, u32) = (600, 800);

    /// Uncropped, which is what every test written before composition existed
    /// means — and they must keep meaning it, so the crop defaults here rather
    /// than being spelled out thirty times.
    fn view(scale: f32, off_x: f32, off_y: f32, out_w: u32, out_h: u32) -> View {
        View {
            scale,
            off_x,
            off_y,
            out_w,
            out_h,
            crop: None,
        }
    }

    fn cropped(v: View, crop: (i32, i32, u32, u32)) -> View {
        View {
            crop: Some(crop),
            ..v
        }
    }

    /// The current live pipeline, plus a Contrast Mask fork. Used to test the
    /// algebra before the GPU side can execute it.
    fn with_contrast_mask(support: f32, contrast: f32, offset: (f32, f32)) -> Graph {
        let mut g = Graph::new();
        let input = g.add(NodeKind::Input, &[]);
        let exposure = g.add(NodeKind::Exposure, &[input]);
        let log = g.add(NodeKind::Log2, &[exposure]);
        let sigma = support / 3.0;
        let bx = g.add(
            NodeKind::Blur {
                axis: Axis::X,
                sigma,
                support,
            },
            &[log],
        );
        let by = g.add(
            NodeKind::Blur {
                axis: Axis::Y,
                sigma,
                support,
            },
            &[bx],
        );
        let cm = g.add(NodeKind::ContrastMask { contrast, offset }, &[log, by]);
        let lin = g.add(NodeKind::Exp2, &[cm]);
        let curve = g.add(NodeKind::Curve, &[lin]);
        g.sink = g.add(NodeKind::Display, &[curve]);
        g
    }

    #[test]
    fn an_untouched_curve_emits_no_curve_node() {
        // "No-op until touched" is literal here: the node is absent, not present
        // and skipped.
        let g = build(&Params::default(), FRAME);
        assert_eq!(g.len(), 3, "input, exposure, display");
        assert!(!(0..g.len()).any(|i| g.kind(NodeId(i)) == NodeKind::Curve));

        let mut p = Params::default();
        p.curve.add(0.5, 0.6);
        let g = build(&p, FRAME);
        assert_eq!(g.len(), 4);
        assert!((0..g.len()).any(|i| g.kind(NodeId(i)) == NodeKind::Curve));
    }

    #[test]
    fn contrast_mask_builds_the_fork_and_only_when_it_is_active() {
        // The module that earned the graph. Off, it costs nothing — not a
        // disabled node, no node. On, it is a real fork: log2 feeds two branches
        // and the mask joins them.
        let mut p = Params::default();
        assert!(!p.contrast_mask.is_active(), "must be off by default");
        assert_eq!(build(&p, FRAME).len(), 3);

        p.contrast_mask.enabled = true;
        let g = build(&p, FRAME);
        assert_eq!(g.validate(), Ok(()));
        let kinds: Vec<_> = (0..g.len()).map(|i| g.kind(NodeId(i)).label()).collect();
        assert_eq!(
            kinds,
            [
                "input",
                "exposure",
                "log2",
                "blur.x",
                "blur.y",
                "contrast mask",
                "exp2",
                "display"
            ]
        );

        // A zero mask gamma is the identity, so the branch is not built at all
        // rather than blurring an image to multiply it by nothing.
        p.contrast_mask.contrast = 0.0;
        assert_eq!(
            build(&p, FRAME).len(),
            3,
            "an inert mask must not cost a blur"
        );
    }

    /// One instance carrying one dab, which is the smallest thing that renders.
    fn painted() -> Params {
        use raw_core::dodgeburn::{Dab, Gesture, Instance, Shape, Sign};
        let mut p = Params::default();
        p.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Burn 1".into(),
            Shape::brush(vec![Gesture::new(vec![Dab {
                x: 0.5,
                y: 0.5,
                radius: 0.1,
                feather: 0.4,
                opacity: 1.0,
                ev: -0.5,
                ..Dab::ROUND
            }])]),
        )];
        p
    }

    #[test]
    fn dodge_and_burn_lands_after_contrast_mask_and_before_the_curve() {
        // **the maintainer's decision, and the one thing about this module that could not
        // be read off the prototype** — it has no contrast mask, so the ordering
        // question is this codebase's own. The darkroom argument: the mask is
        // contact-printed from the negative and sandwiched with it, and only then
        // do you dodge under the enlarger. Reversing these two would let a burn
        // change the mask it prints through.
        let mut p = painted();
        p.contrast_mask.enabled = true;
        p.curve.add(0.5, 0.6);
        let g = build(&p, FRAME);
        assert_eq!(g.validate(), Ok(()));
        let kinds: Vec<_> = (0..g.len()).map(|i| g.kind(NodeId(i)).label()).collect();
        assert_eq!(
            kinds,
            [
                "input",
                "exposure",
                "log2",
                "blur.x",
                "blur.y",
                "contrast mask",
                "exp2",
                "dodge & burn",
                "curve",
                "display"
            ]
        );
    }

    #[test]
    fn an_unpainted_frame_emits_no_dodge_and_burn_node() {
        // Same rule as the curve's: absent, not present-and-skipped. An instance
        // that exists but has nothing in it is still nothing to render, which is
        // the state the panel is in the moment you press ⌘D.
        use raw_core::dodgeburn::{Instance, Sign};
        let mut p = Params::default();
        assert_eq!(build(&p, FRAME).len(), 3, "input, exposure, display");

        p.dodgeburn.instances = vec![Instance::new(Sign::Burn, "Burn 1".into())];
        assert_eq!(build(&p, FRAME).len(), 3, "an empty instance is not a node");

        assert_eq!(build(&painted(), FRAME).len(), 4, "one dab is");

        // And so is a placed gradient, which has no dabs at all — the node is
        // emitted on `is_active`, not on a stroke count, and 10b is exactly the
        // change that would have caught a version keyed on the latter.
        use raw_core::dodgeburn::{Linear, Shape};
        let mut grad = Params::default();
        grad.dodgeburn.instances = vec![Instance::of(
            Sign::Burn,
            "Linear Burn 1".into(),
            Shape::Linear(Linear {
                x0: 0.1,
                y0: 0.5,
                x1: 0.9,
                y1: 0.5,
                feather: 1.0,
                ev: -1.0,
            }),
        )];
        assert_eq!(build(&grad, FRAME).len(), 4, "a placed gradient renders");

        // And the bypass, which `Params::effective` implements by emptying the
        // list — so a switched-off module costs no node either.
        let mut off = painted();
        off.dodgeburn.enabled = false;
        assert_eq!(build(&off.effective(), FRAME).len(), 3);
    }

    #[test]
    fn a_stroke_widens_nothing_and_moves_nothing() {
        // The property the whole interaction rests on: D&B is pointwise, so it
        // asks its producer for exactly its own region and the plan is identical
        // to the unpainted one but for the extra step. If this ever fails, a brush
        // drag has started paying for an apron — and on a 100 MP file that is the
        // difference between a tool and a slideshow.
        let v = view(0.5, 100.0, 60.0, 320, 240);
        let plain = build(&Params::default(), FRAME)
            .resolve((600, 800), v)
            .unwrap();
        let with = build(&painted(), FRAME).resolve((600, 800), v).unwrap();

        assert_eq!(with.steps.len(), plain.steps.len() + 1);
        let db = with
            .steps
            .iter()
            .find(|s| s.kind == NodeKind::DodgeBurn)
            .expect("the node");
        assert_eq!(
            db.out, db.inputs[0].1,
            "it must read exactly what it writes"
        );

        // Every step the unpainted plan also has writes the same rectangle. Paired
        // by kind rather than by position, because the new node is spliced into
        // the middle and a positional zip would compare display against it — which
        // is how the first version of this test failed for a reason that was in
        // the test.
        for a in &plain.steps {
            let b = with
                .steps
                .iter()
                .find(|s| s.kind == a.kind)
                .expect("kind still present");
            assert_eq!(a.out, b.out, "{:?} moved", a.kind);
        }
        assert_eq!(
            with.pixels_written(),
            plain.pixels_written() + db.out.area()
        );
    }

    #[test]
    fn layer_contrast_adds_one_shared_detail_decomposition_and_zero_adds_none() {
        let plain = painted();
        let plain_graph = build(&plain, FRAME);
        let plain_kinds: Vec<_> = (0..plain_graph.len())
            .map(|i| plain_graph.kind(NodeId(i)).label())
            .collect();
        assert_eq!(
            plain_kinds,
            ["input", "exposure", "dodge & burn", "display"]
        );
        plain_graph
            .resolve((600, 800), view(1.0, 0.0, 0.0, 600, 800))
            .expect("zero Contrast must validate with its aliased second input");

        let mut contrast = plain;
        contrast.dodgeburn.instances[0].contrast = 0.5;
        let g = build(&contrast, FRAME);
        let kinds: Vec<_> = (0..g.len()).map(|i| g.kind(NodeId(i)).label()).collect();
        assert_eq!(
            kinds,
            [
                "input",
                "exposure",
                "log2",
                "blur.x",
                "blur.y",
                "dodge & burn",
                "display"
            ]
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == "blur.x").count(),
            1,
            "the layer stack shares one analysis"
        );
        g.resolve((600, 800), view(1.0, 0.0, 0.0, 600, 800))
            .expect("moving the Contrast slider must produce a valid graph");
    }

    #[test]
    fn the_masks_blur_support_is_three_sigma() {
        // Beyond 3 sigma a Gaussian contributes under 0.3%, and every pixel of
        // apron is real work on every frame. Pinned because it is the one place
        // the spacer parameter turns into an allocation size.
        //
        // Now pins the percent -> pixel conversion as well: 5% of FRAME's
        // 1000px diagonal is a 50px sigma, so the support is 150. Both halves of
        // that arithmetic have to hold, because `node_params` repeats it
        // independently for the shader and the two must not drift.
        let mut p = Params::default();
        p.contrast_mask.enabled = true;
        p.contrast_mask.spacer = 5.0;
        let g = build(&p, FRAME);
        let blur = (0..g.len())
            .map(|i| g.kind(NodeId(i)))
            .find_map(|k| match k {
                NodeKind::Blur { support, .. } => Some(support),
                _ => None,
            });
        assert_eq!(blur, Some(150.0));
    }

    #[test]
    fn the_built_graph_validates() {
        assert_eq!(build(&Params::default(), FRAME).validate(), Ok(()));
        let mut p = Params::default();
        p.curve.add(0.5, 0.6);
        assert_eq!(build(&p, FRAME).validate(), Ok(()));
        assert_eq!(with_contrast_mask(20.0, 0.3, (0.0, 0.0)).validate(), Ok(()));
    }

    #[test]
    fn a_log_signal_cannot_reach_the_curve() {
        // The typed connectors doing the job they exist for. Without the check
        // this graph renders a plausible, wrong image.
        let mut g = Graph::new();
        let input = g.add(NodeKind::Input, &[]);
        let log = g.add(NodeKind::Log2, &[input]);
        let curve = g.add(NodeKind::Curve, &[log]);
        g.sink = g.add(NodeKind::Display, &[curve]);
        assert_eq!(
            g.validate(),
            Err(GraphError::RoleMismatch {
                node: 2,
                kind: "curve",
                index: 0,
                got: StageRole::WorkingLog,
            })
        );
    }

    #[test]
    fn a_chain_that_does_not_end_display_referred_is_rejected() {
        let mut g = Graph::new();
        let input = g.add(NodeKind::Input, &[]);
        g.sink = g.add(NodeKind::Exposure, &[input]);
        assert_eq!(
            g.validate(),
            Err(GraphError::SinkNotDisplay {
                got: StageRole::Working
            })
        );
    }

    #[test]
    fn a_node_with_the_wrong_number_of_inputs_is_rejected() {
        let mut g = Graph::new();
        let input = g.add(NodeKind::Input, &[]);
        // Contrast Mask takes the negative AND the mask; one is a wiring bug.
        let cm = g.add(
            NodeKind::ContrastMask {
                contrast: 0.3,
                offset: (0.0, 0.0),
            },
            &[input],
        );
        g.sink = g.add(NodeKind::Display, &[cm]);
        assert!(matches!(
            g.validate(),
            Err(GraphError::Arity {
                expected: 2,
                got: 1,
                ..
            })
        ));
    }

    #[test]
    fn a_node_nothing_reads_is_rejected_rather_than_silently_executed() {
        let mut g = Graph::new();
        let input = g.add(NodeKind::Input, &[]);
        let _orphan = g.add(NodeKind::Exposure, &[input]);
        g.sink = g.add(NodeKind::Display, &[input]);
        let e = g.resolve((100, 100), view(1.0, 0.0, 0.0, 100, 100));
        assert_eq!(
            e,
            Err(GraphError::Unreachable {
                node: 1,
                kind: "exposure"
            })
        );
    }

    #[test]
    fn a_pointwise_chain_asks_for_exactly_the_visible_region() {
        // With no spatial node there is no apron, so every intermediate is the
        // viewport and nothing extra is computed.
        let g = build(&Params::default(), FRAME);
        let plan = g
            .resolve((1000, 800), view(1.0, 100.0, 50.0, 200, 150))
            .unwrap();
        for step in &plan.steps {
            assert_eq!(
                (step.out.x, step.out.y, step.out.w, step.out.h),
                (100, 50, 200, 150),
                "{} grew without a spatial node",
                step.kind.label()
            );
        }
    }

    #[test]
    fn a_blur_makes_its_producer_write_wider() {
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let by_kind = |k: &str| plan.steps.iter().find(|s| s.kind.label() == k).unwrap();

        // blur.y writes the region asked of it and reads 10px above and below,
        // so its producer has to write 20 rows more.
        assert_eq!(
            (by_kind("blur.y").out.w, by_kind("blur.y").out.h),
            (200, 200)
        );
        assert_eq!(
            (by_kind("blur.x").out.w, by_kind("blur.x").out.h),
            (200, 220)
        );
        // blur.x in turn reads 10px either side of ITS output, so log2 writes
        // 20 columns more than that.
        assert_eq!((by_kind("log2").out.w, by_kind("log2").out.h), (220, 220));
    }

    #[test]
    fn the_fork_point_writes_the_union_of_both_branches() {
        // Contrast Mask reads log2 directly (200x200) and through the blur,
        // which needs 220x220. The shared producer must write the union, or the
        // mask is computed from garbage along its edges.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let log = plan
            .steps
            .iter()
            .find(|s| s.kind == NodeKind::Log2)
            .unwrap();
        let cm = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "contrast mask")
            .unwrap();

        assert_eq!(
            (log.out.w, log.out.h),
            (220, 220),
            "the union, not either branch"
        );
        assert!(log.out.contains(cm.out), "the direct branch must fit");
        let blurred = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "blur.x")
            .unwrap();
        assert!(log.out.contains(blurred.out), "the blurred branch must fit");
    }

    #[test]
    fn each_blur_pass_grows_only_along_the_axis_asked_of_it() {
        // Separability, seen by the scheduler. blur.x writes 200x220: it grew
        // vertically to satisfy blur.y's reach, and did NOT pre-emptively grow
        // along its own axis. Modelling both passes as square aprons would make
        // it 220x220 — 4400 pixels computed and never read, on every frame.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let bx = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "blur.x")
            .unwrap();
        assert_eq!((bx.out.w, bx.out.h), (200, 220));
    }

    #[test]
    fn the_registration_offset_reaches_only_the_way_it_points() {
        let g = with_contrast_mask(0.0, 0.3, (8.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let blur = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "blur.y")
            .unwrap();
        // Mask shifted right by 8 => the mask branch is read 8px further right.
        assert_eq!(blur.out.x, 300, "no reach to the left");
        assert_eq!(blur.out.w, 208, "8px of reach to the right");
    }

    #[test]
    fn the_apron_shrinks_as_you_zoom_out() {
        // A 200px spacer is affordable at fit-to-screen precisely because the
        // apron is expressed in output pixels.
        // Source is large enough that neither case clips at the frame border,
        // so the numbers measure the apron and nothing else.
        let g = with_contrast_mask(200.0, 0.3, (0.0, 0.0));
        let wide = g
            .resolve((8000, 8000), view(1.0, 2000.0, 2000.0, 400, 400))
            .unwrap();
        let zoomed_out = g
            .resolve((8000, 8000), view(0.1, 2000.0, 2000.0, 400, 400))
            .unwrap();

        let log_of = |p: &Plan| {
            p.steps
                .iter()
                .find(|s| s.kind == NodeKind::Log2)
                .unwrap()
                .out
                .w
        };
        assert_eq!(log_of(&wide), 800, "400 + 200 either side");
        assert_eq!(log_of(&zoomed_out), 440, "400 + 20 either side");
    }

    #[test]
    fn the_apron_is_trimmed_at_the_frame_border() {
        // There is no data outside the image, so the request is clipped and the
        // node falls back to edge clamping — the same thing sample.wgsl already
        // does at the source. Asking for it anyway would allocate a buffer whose
        // margin can never be filled.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 0.0, 0.0, 200, 200))
            .unwrap();
        let log = plan
            .steps
            .iter()
            .find(|s| s.kind == NodeKind::Log2)
            .unwrap();
        assert_eq!(log.out.x, 0, "cannot reach left of the image");
        assert_eq!(
            log.out.w, 210,
            "10px trimmed off the left, 10 kept on the right"
        );
    }

    #[test]
    fn the_surround_is_the_only_region_allowed_outside_the_image() {
        // At fit-to-window the viewport is bigger than the image. The sink's
        // region legitimately extends past the grid; every upstream region is
        // clamped to it.
        let g = build(&Params::default(), FRAME);
        let plan = g
            .resolve((100, 100), view(1.0, -50.0, -50.0, 200, 200))
            .unwrap();
        let sink = plan.sink();
        assert_eq!(
            (sink.out.x, sink.out.y),
            (-50, -50),
            "the surround is outside the image"
        );

        for step in plan.steps.iter().filter(|s| s.kind != NodeKind::Display) {
            assert!(
                step.out.x >= 0 && step.out.y >= 0,
                "{} escaped the image",
                step.kind.label()
            );
            assert!(step.out.right() <= 100 && step.out.bottom() <= 100);
        }
    }

    #[test]
    fn a_crop_shrinks_the_work_without_shrinking_the_sink() {
        // The two halves of the crop mechanism, in one plan. The sink still covers
        // the whole viewport — it has a surround to paint outside the crop, and a
        // dispatch that stopped at the crop edge would leave whatever the last frame
        // wrote in the margin. Everything upstream of it does only the work that
        // will survive.
        let g = build(&Params::default(), FRAME);
        let v = cropped(view(1.0, 0.0, 0.0, 400, 400), (100, 100, 150, 150));
        let plan = g.resolve((1000, 800), v).unwrap();

        let sink = plan.sink();
        assert_eq!(
            (sink.out.w, sink.out.h),
            (400, 400),
            "the sink stopped painting the surround"
        );
        let input = &plan.steps[0];
        assert_eq!(
            (input.out.x, input.out.y, input.out.w, input.out.h),
            (100, 100, 150, 150),
            "the chain computed pixels the crop covers over"
        );
    }

    #[test]
    fn a_spatial_node_reads_outside_the_crop() {
        // **The load-bearing one.** Crop-then-blur has no data past the boundary;
        // blur-then-crop is correct. So the crop is intersected in, the apron is
        // expanded out, and the clamp is to the whole frame — the blur feeding the
        // crop's corner reads real pixels from outside it. The other order puts a halo
        // along the crop edge.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let v = cropped(view(1.0, 0.0, 0.0, 600, 600), (200, 200, 100, 100));
        let plan = g.resolve((1000, 800), v).unwrap();
        let by_kind = |k: &str| plan.steps.iter().find(|s| s.kind.label() == k).unwrap().out;

        let cm = by_kind("contrast mask");
        assert_eq!(
            (cm.x, cm.y, cm.w, cm.h),
            (200, 200, 100, 100),
            "the mask is computed on the crop"
        );

        // blur.y reaches 10px above and below, blur.x 10px either side, so log2
        // writes 20px more on each axis — and its origin is OUTSIDE the crop, which
        // is the whole point. Each axis pays the apron once, not once per pass; see
        // `each_blur_pass_grows_only_along_the_axis_asked_of_it`.
        let log = by_kind("log2");
        assert!(
            log.x < cm.x && log.y < cm.y,
            "the apron was clamped at the crop edge: {log:?}"
        );
        assert_eq!((log.x, log.y), (190, 190));
        assert_eq!((log.w, log.h), (120, 120));
        assert!(log.contains(cm), "the direct branch must still fit");
    }

    #[test]
    fn an_apron_is_still_trimmed_at_the_frame_border_not_at_the_crop() {
        // The other side of the same rule. Outside the crop there is data; outside
        // the FRAME there is none. A crop flush against the image edge must clamp
        // where the pixels stop, exactly as an uncropped frame does.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let v = cropped(view(1.0, 0.0, 0.0, 400, 400), (0, 0, 100, 100));
        let plan = g.resolve((1000, 800), v).unwrap();
        let log = plan
            .steps
            .iter()
            .find(|s| s.kind == NodeKind::Log2)
            .unwrap();
        assert_eq!(log.out.x, 0, "reached left of the image");
        assert_eq!(log.out.w, 110, "nothing on the left, 10px on the right");
    }

    #[test]
    fn the_crop_scales_with_the_zoom() {
        // The crop is in frame pixels and the grid is in output pixels, so the one
        // conversion has to track the zoom exactly — a crop that drifted by a pixel
        // per zoom level would make the frame edge crawl while you zoomed.
        let g = build(&Params::default(), FRAME);
        for (scale, want) in [(1.0, (200, 100)), (0.5, (100, 50)), (2.0, (400, 200))] {
            let v = cropped(view(scale, 0.0, 0.0, 2000, 2000), (100, 50, 200, 100));
            let plan = g.resolve((1000, 800), v).unwrap();
            let input = plan.steps[0].out;
            assert_eq!((input.w, input.h), want, "at scale {scale}");
            assert_eq!(
                (input.x, input.y),
                ((100.0 * scale) as i32, (50.0 * scale) as i32)
            );
        }
    }

    #[test]
    fn a_crop_the_size_of_the_frame_costs_nothing() {
        // An untouched composition must produce byte-for-byte the plan it produced
        // before this feature existed, or every existing test is measuring something
        // other than what it says.
        let g = with_contrast_mask(10.0, 0.3, (2.0, -1.0));
        let v = view(0.37, 123.4, 56.7, 300, 250);
        let bare = g.resolve((1000, 800), v).unwrap();
        let full = g
            .resolve((1000, 800), cropped(v, (0, 0, 1000, 800)))
            .unwrap();
        assert_eq!(
            bare, full,
            "declaring the whole frame as a crop changed the plan"
        );
    }

    #[test]
    fn a_crop_entirely_off_screen_asks_for_no_work() {
        // Pan far enough and the crop leaves the viewport. Nothing upstream should
        // be asked for anything, and no intermediate may be sized zero — wgpu
        // rejects a zero-extent texture, so this is a crash rather than a blank.
        let g = build(&Params::default(), FRAME);
        let v = cropped(view(1.0, 5000.0, 5000.0, 200, 200), (0, 0, 100, 100));
        let plan = g.resolve((1000, 800), v).unwrap();
        assert!(
            plan.steps[0].out.is_empty(),
            "computed a region that is not on screen"
        );
        assert_eq!(
            plan.sink().out.w,
            200,
            "the sink still fills the viewport with surround"
        );
    }

    #[test]
    fn a_fractional_pan_becomes_an_integer_region_plus_a_phase() {
        // Regions must be integral for a spatial operation to be reproducible;
        // panning must stay smooth. The phase is what reconciles them, and it is
        // the input node's business alone.
        let g = build(&Params::default(), FRAME);
        let plan = g
            .resolve((1000, 800), view(2.0, 10.25, 10.75, 100, 100))
            .unwrap();
        // 10.25 source px at scale 2 is 20.5 grid px: region starts at 20, phase 0.5.
        assert_eq!(plan.steps[0].out.x, 20);
        assert_eq!(plan.subpixel.0, 0.5);
        assert_eq!(plan.steps[0].out.y, 21);
        assert_eq!(plan.subpixel.1, 0.5);
    }

    #[test]
    fn a_negative_pan_phase_stays_in_the_unit_interval() {
        // floor, not truncate. At off = -0.25 the region starts one pixel back
        // with phase 0.75; truncation would give phase -0.25 and shift the image
        // by a pixel on one side of the origin only.
        let g = build(&Params::default(), FRAME);
        let plan = g
            .resolve((1000, 800), view(1.0, -0.25, -0.25, 100, 100))
            .unwrap();
        // The sink carries the negative origin; the input node's own region is
        // clamped to the image, because there is nothing to read out there.
        assert_eq!(plan.sink().out.x, -1);
        assert_eq!(plan.steps[0].out.x, 0);
        assert!(
            (plan.subpixel.0 - 0.75).abs() < 1e-6,
            "phase was {}",
            plan.subpixel.0
        );
    }

    #[test]
    fn a_buffer_is_released_after_its_last_reader_not_its_first() {
        // The fork's whole hazard: log2 feeds the blur immediately and the
        // contrast mask much later. Freeing it after the blur would hand the
        // pool a buffer that is still needed.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let log_id = plan
            .steps
            .iter()
            .find(|s| s.kind == NodeKind::Log2)
            .unwrap()
            .id;
        let cm_id = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "contrast mask")
            .unwrap()
            .id;

        let releaser = plan
            .steps
            .iter()
            .find(|s| s.release.contains(&log_id))
            .unwrap();
        assert_eq!(
            releaser.id, cm_id,
            "log2 was freed while the direct branch still needed it"
        );
    }

    #[test]
    fn every_intermediate_is_released_exactly_once() {
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let sink = plan.sink().id;
        for step in &plan.steps {
            if step.id == sink {
                continue;
            }
            let n = plan
                .steps
                .iter()
                .filter(|s| s.release.contains(&step.id))
                .count();
            assert_eq!(n, 1, "{} released {n} times", step.kind.label());
        }
        assert!(
            !plan.steps.iter().any(|s| s.release.contains(&sink)),
            "the sink is the target, not a pooled buffer"
        );
    }

    #[test]
    fn a_node_is_told_what_its_producer_actually_wrote() {
        // Not what it asked for. The contrast mask asks log2 for 200x200 and gets
        // 240x220 because the blur asked for more. If it indexed its input as if
        // the buffer were its own region, the mask would be offset by the apron —
        // a shift, not a crash.
        let g = with_contrast_mask(10.0, 0.3, (0.0, 0.0));
        let plan = g
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let cm = plan
            .steps
            .iter()
            .find(|s| s.kind.label() == "contrast mask")
            .unwrap();
        let (_, direct) = cm.inputs[0];
        assert_eq!(
            (direct.w, direct.h),
            (220, 220),
            "the blur's request, not its own"
        );
        assert_eq!((cm.out.w, cm.out.h), (200, 200));
        assert_ne!(
            (direct.x, direct.y),
            (cm.out.x, cm.out.y),
            "origins differ; indexing must too"
        );
    }

    #[test]
    fn a_fork_costs_real_pixels_and_the_plan_says_how_many() {
        let plain = build(&Params::default(), FRAME)
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        let masked = with_contrast_mask(10.0, 0.3, (0.0, 0.0))
            .resolve((1000, 800), view(1.0, 300.0, 300.0, 200, 200))
            .unwrap();
        assert!(
            masked.pixels_written() > plain.pixels_written(),
            "the apron has to show up in the cost"
        );
    }

    #[test]
    fn resolving_is_deterministic() {
        // The graph is rebuilt from params every frame; if resolve were not a
        // pure function of (graph, source, view), idle-frame detection would
        // dispatch work forever.
        let g = with_contrast_mask(10.0, 0.3, (2.0, -1.0));
        let v = view(0.37, 123.4, 56.7, 300, 250);
        assert_eq!(
            g.resolve((1000, 800), v).unwrap(),
            g.resolve((1000, 800), v).unwrap()
        );
    }

    #[test]
    fn a_grid_is_never_empty_however_far_you_zoom_out() {
        let g = build(&Params::default(), FRAME);
        let plan = g
            .resolve((4000, 3000), view(0.0001, 0.0, 0.0, 64, 64))
            .unwrap();
        let input = &plan.steps[0];
        assert!(
            !input.out.is_empty(),
            "a zero-sized intermediate cannot be allocated"
        );
    }
}
