//! What each pipeline stage is, and the two things the graph needs before any pixels
//! exist: what signal it produces, and how far outside its output it reads.
//!
//! A closed enum rather than a trait object, because the graph is a pure function of
//! `Params` — no node editor, no user-authored topology, nothing loaded at runtime.
//! See `docs/decisions.md`.

use crate::roi::Apron;

/// What kind of signal an edge carries.
///
/// Catches connecting a display-space node to a scene-space input, which produces an
/// image that looks *plausible* rather than obviously broken. `Graph::validate` checks
/// these at connection time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageRole {
    /// Mosaiced f32 from decode. Unclamped, range `[~0, gains.headroom()]`.
    /// Never on the GPU yet — decode is a CPU stage — but named for completeness
    /// so nothing invents a second vocabulary later.
    Scene,
    /// Luminance-derived, scene-referred, **linear**, unbounded. Exposure and the
    /// curve both live here.
    Working,
    /// Scene-referred in **log2 stops**. Same information as `Working`, and *not*
    /// interchangeable with it: blurring linear light then taking the log is a
    /// different operator to blurring the log, and Contrast Mask needs the latter.
    /// Feeding a log signal to the curve produces a plausible, wrong image.
    WorkingLog,
    /// Display-referred, `[0, 1]`, transfer-encoded. Only the display node
    /// produces this, and nothing may consume it back into the chain.
    Display,
}

/// Which axis a separable filter pass runs along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    X,
    Y,
}

/// A pipeline stage.
///
/// Each variant carries only what changes the graph's structure, role or reach.
/// Exposure EV, the curve LUT and the tone map are read from `Params` at execution
/// time — putting them here would be a second source of truth.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeKind {
    /// Reads the stored luminance image under the view transform: maps the output
    /// grid onto the source grid, box-filters the footprint when zoomed out, and
    /// clamps at the frame border.
    ///
    /// Shares source sampling with MaskInput. Downstream nodes work on plain
    /// pixel grids; ROI propagation converts between their scales.
    Input,
    /// `out = (in - black) * 2^stops`. Pointwise.
    Exposure,
    /// Linear -> log2 stops. Pointwise. Also the **fork point** for Contrast
    /// Mask: the log signal is consumed both directly and through the blur.
    Log2,
    /// log2 stops -> linear. Pointwise.
    Exp2,
    /// Read and area-average exposed log luminance directly from the source.
    /// Its base grid is capped at native resolution; the negative stays separate.
    MaskInput { sigma: f32 },
    /// One axis of a separable blur. `sigma` and `support` are in **source** pixels;
    /// the apron converts support to this grid's pixels and the executor converts
    /// sigma. Carrying both here lets Contrast Mask and D&B local contrast share
    /// the same pass without one module's settings leaking into the other.
    ///
    /// Role-**preserving**: a blur does not change what space its signal is in,
    /// so it is legal on linear working data (a future a-trous band) and on log
    /// data (Contrast Mask) alike, and it is the same node either way.
    Blur {
        axis: Axis,
        sigma: f32,
        support: f32,
    },
    /// `out = direct + (-blurred * contrast)`, in log2 stops.
    ///
    /// Two inputs: the negative, and the mask. In the darkroom the mask sits a
    /// spacer's thickness from the negative and misregistration was a real
    /// variable, so `offset` shifts the mask against the negative — in source
    /// pixels, like `Blur::support`.
    ContrastMask { contrast: f32, offset: (f32, f32) },
    /// Local exposure plus optional local-detail contrast, where both maps are
    /// rasterised from the same stroke/layer buffers. Input 0 is the negative;
    /// input 1 is its shared blurred log base (or input 0 again when every layer's
    /// contrast is zero).
    ///
    /// The composite itself remains pointwise. The optional log-detail base is one
    /// shared upstream blur, so eight contrast layers do not mean eight spatial
    /// analyses and a brush drag re-dispatches only the pointwise composite.
    ///
    /// Carries **no parameters at all**, unlike every other configurable node
    /// here. The strokes are a variable-length list and travel in a storage
    /// buffer, and the graph has nothing to schedule differently for one stroke or
    /// a thousand — so putting a count in the enum would recompile the pipeline on
    /// every dab of a drag for a number that changes no decision. Emitted only
    /// when there is something to render, so an unpainted frame carries no node.
    DodgeBurn,
    /// Tone curve via the baked LUT. Pointwise. Emitted only when the curve is
    /// not the identity, so "no-op until touched" stays literal.
    Curve,
    /// Tone map + transfer function + dither + surround composite. The one node
    /// that produces `Display`, and the end of every chain.
    Display,
}

impl NodeKind {
    /// How many inputs this node takes. Input and MaskInput read the source.
    pub fn arity(self) -> usize {
        match self {
            Self::Input | Self::MaskInput { .. } => 0,
            Self::ContrastMask { .. } | Self::DodgeBurn => 2,
            _ => 1,
        }
    }

    /// Whether `role` may be connected to input `index`.
    pub fn accepts(self, index: usize, role: StageRole) -> bool {
        match self {
            Self::Input | Self::MaskInput { .. } => false,
            Self::DodgeBurn => match index {
                // The image being adjusted is always scene-linear.
                0 => role == StageRole::Working,
                // With local Contrast active this is the blurred log2 base. With
                // Contrast at zero the graph deliberately aliases input 0 here so
                // the fixed two-binding shader layout costs no analysis pass.
                1 => matches!(role, StageRole::Working | StageRole::WorkingLog),
                _ => false,
            },
            Self::Exposure | Self::Log2 | Self::Curve | Self::Display => role == StageRole::Working,
            Self::Exp2 => role == StageRole::WorkingLog,
            // Role-preserving: legal in either working space, and the output
            // role follows the input rather than being fixed.
            Self::Blur { .. } => matches!(role, StageRole::Working | StageRole::WorkingLog),
            // Both the negative and the mask are log-domain. Mixing a linear
            // negative with a log mask is exactly the plausible-looking mistake
            // the roles exist to refuse.
            Self::ContrastMask { .. } => {
                let _ = index;
                role == StageRole::WorkingLog
            }
        }
    }

    /// What this node produces, given what its inputs carry.
    pub fn out_role(self, inputs: &[StageRole]) -> StageRole {
        match self {
            Self::Input => StageRole::Working,
            Self::Exposure | Self::DodgeBurn | Self::Curve => StageRole::Working,
            Self::Log2 => StageRole::WorkingLog,
            Self::Exp2 => StageRole::Working,
            Self::MaskInput { .. } => StageRole::WorkingLog,
            Self::Blur { .. } => inputs.first().copied().unwrap_or(StageRole::Working),
            Self::ContrastMask { .. } => StageRole::WorkingLog,
            Self::Display => StageRole::Display,
        }
    }

    /// How far outside its output this node reads on input `index`, in **this node's**
    /// pixels.
    ///
    /// **The scale conversion is the load-bearing part.** A 200-source-pixel spacer is
    /// 200px of reach at 100% and 30px at fit-to-screen, so the apron shrinks with zoom
    /// and the preview filters the same *physical* frequency the export does.
    pub fn apron(self, index: usize, scale: f32) -> Apron {
        let px = |source: f32| crate::roi::ceil_px(source * scale);
        match self {
            Self::Blur { axis, support, .. } => match axis {
                Axis::X => Apron::horizontal(px(support)),
                Axis::Y => Apron::vertical(px(support)),
            },
            // Input 0 is the negative, read in place. Input 1 is the mask, read
            // shifted, so only it pays for the registration offset.
            Self::ContrastMask { offset, .. } if index == 1 => {
                Apron::shift(offset.0 * scale, offset.1 * scale)
            }
            _ => Apron::NONE,
        }
    }

    /// Full extent and scale of this node's output grid.
    ///
    /// Only the mask branch changes grid. Powers of two anchor its cells to the
    /// image rather than to a viewport/tile origin, so panning cannot move the blur.
    pub fn out_grid(
        self,
        input: Option<((u32, u32), f32)>,
        root: ((u32, u32), f32),
    ) -> ((u32, u32), f32) {
        match self {
            Self::Input => root,
            Self::MaskInput { sigma } => {
                let (full, scale) = root;
                let mut factor = 1;
                // Below 1:1 the input already averages sensor footprints before
                // taking the log. Preserve that preview's exact boundary math:
                // reducing it again changes partially covered edge rows. At native
                // resolution and above, aim for sigma <= 16 reduced pixels, with
                // at most 32x32 reads per cell and two centres for interpolation.
                while scale >= 1.0
                    && sigma * scale / factor as f32 > 16.0
                    && factor < 32
                    && full.0.div_ceil(factor * 2) >= 2
                    && full.1.div_ceil(factor * 2) >= 2
                {
                    factor *= 2;
                }
                (
                    (full.0.div_ceil(factor), full.1.div_ceil(factor)),
                    scale / factor as f32,
                )
            }
            _ => input.unwrap_or(root),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Exposure => "exposure",
            Self::Log2 => "log2",
            Self::Exp2 => "exp2",
            Self::MaskInput { .. } => "mask source",
            Self::Blur { axis: Axis::X, .. } => "blur.x",
            Self::Blur { axis: Axis::Y, .. } => "blur.y",
            Self::ContrastMask { .. } => "contrast mask",
            Self::DodgeBurn => "dodge & burn",
            Self::Curve => "curve",
            Self::Display => "display",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blur_preserves_whatever_space_it_is_given() {
        // The reason Blur is one node and not two. Contrast Mask blurs log data;
        // a-trous will blur linear data; neither wants its own blur.
        let b = NodeKind::Blur {
            axis: Axis::X,
            sigma: 4.0 / 3.0,
            support: 4.0,
        };
        assert_eq!(b.out_role(&[StageRole::WorkingLog]), StageRole::WorkingLog);
        assert_eq!(b.out_role(&[StageRole::Working]), StageRole::Working);
        assert!(b.accepts(0, StageRole::Working) && b.accepts(0, StageRole::WorkingLog));
    }

    #[test]
    fn the_curve_refuses_log_data() {
        // The whole point of WorkingLog. The curve's LUT is indexed by log2 of a
        // LINEAR value; feeding it something already logged gives a plausible,
        // wrong image rather than an obvious failure.
        assert!(NodeKind::Curve.accepts(0, StageRole::Working));
        assert!(!NodeKind::Curve.accepts(0, StageRole::WorkingLog));
    }

    #[test]
    fn dodge_and_burn_takes_a_linear_image_and_a_log_detail_base() {
        let d = NodeKind::DodgeBurn;
        assert!(d.accepts(0, StageRole::Working));
        assert!(!d.accepts(0, StageRole::WorkingLog));
        assert!(d.accepts(1, StageRole::WorkingLog));
        assert!(
            d.accepts(1, StageRole::Working),
            "zero Contrast aliases the linear image into the unused second binding"
        );
        assert!(!d.accepts(2, StageRole::Working));
    }

    #[test]
    fn nothing_consumes_display_back_into_the_chain() {
        for kind in [
            NodeKind::Exposure,
            NodeKind::Log2,
            NodeKind::Exp2,
            NodeKind::Curve,
            NodeKind::Display,
            NodeKind::Blur {
                axis: Axis::X,
                sigma: 1.0 / 3.0,
                support: 1.0,
            },
            NodeKind::ContrastMask {
                contrast: 0.3,
                offset: (0.0, 0.0),
            },
        ] {
            assert!(
                !kind.accepts(0, StageRole::Display),
                "{} must not consume display-referred data",
                kind.label()
            );
        }
    }

    #[test]
    fn the_input_node_is_the_only_source() {
        assert_eq!(NodeKind::Input.arity(), 0);
        assert!(!NodeKind::Input.accepts(0, StageRole::Working));
        for kind in [NodeKind::Exposure, NodeKind::Curve, NodeKind::Display] {
            assert_eq!(kind.arity(), 1);
        }
        assert_eq!(
            NodeKind::ContrastMask {
                contrast: 0.3,
                offset: (0.0, 0.0)
            }
            .arity(),
            2
        );
        assert_eq!(
            NodeKind::DodgeBurn.arity(),
            2,
            "negative and shared local-detail base"
        );
    }

    #[test]
    fn apron_shrinks_with_zoom() {
        // A 200px spacer costs 200px of apron at 100% and 30 at fit-to-screen.
        // That is what stops a large-radius mask from being unaffordable while
        // still filtering the same physical frequency.
        let b = NodeKind::Blur {
            axis: Axis::X,
            sigma: 200.0 / 3.0,
            support: 200.0,
        };
        assert_eq!(b.apron(0, 1.0), Apron::horizontal(200));
        assert_eq!(b.apron(0, 0.15), Apron::horizontal(30));
    }

    #[test]
    fn only_the_mask_input_pays_for_registration_offset() {
        // The negative is read in place; misregistration moves the mask against
        // it. Charging both inputs would over-allocate the negative's buffer.
        let cm = NodeKind::ContrastMask {
            contrast: 0.3,
            offset: (6.0, 0.0),
        };
        assert_eq!(
            cm.apron(0, 1.0),
            Apron::NONE,
            "the negative is read in place"
        );
        assert_eq!(cm.apron(1, 1.0), Apron::shift(6.0, 0.0));
    }

    #[test]
    fn pointwise_nodes_have_no_reach() {
        for kind in [
            NodeKind::Exposure,
            NodeKind::Log2,
            NodeKind::Exp2,
            NodeKind::Curve,
        ] {
            assert!(
                kind.apron(0, 1.0).is_none(),
                "{} must be pointwise",
                kind.label()
            );
        }
    }
}
