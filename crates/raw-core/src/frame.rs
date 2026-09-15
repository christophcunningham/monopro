//! Export frame: a physical canvas placed around the finished photograph.
//!
//! The important boundary is not cosmetic: this module is applied only after the
//! photograph has been resized, grained, toned and sharpened.  A frame pixel is
//! therefore never an input to a toner or a convolution kernel, and a hard image
//! edge cannot acquire a halo from the frame colour beside it.

use crate::{Dims, Unit};

/// Which quantity is authoritative when the frame is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    /// The four margins are authored and the outer size follows.
    #[default]
    Sides,
    /// The outer frame is authored and the margins are derived from placement.
    Outer,
}

impl Priority {
    pub const UI_ORDER: [Self; 2] = [Self::Sides, Self::Outer];

    pub fn label(self) -> &'static str {
        match self {
            Self::Sides => "SIDES",
            Self::Outer => "FRAME",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Sides => "sides",
            Self::Outer => "frame",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|v| v.key() == key)
    }
}

/// How the photograph is placed when the outer size is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    #[default]
    Centred,
    BottomWeighted,
    Custom,
}

impl Placement {
    pub const UI_ORDER: [Self; 3] = [Self::Centred, Self::BottomWeighted, Self::Custom];

    pub fn label(self) -> &'static str {
        match self {
            Self::Centred => "Centered",
            Self::BottomWeighted => "Bottom weight",
            Self::Custom => "Custom",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Centred => "centered",
            Self::BottomWeighted => "bottom-weight",
            Self::Custom => "custom",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|v| v.key() == key)
    }
}

/// Physical margins, canonically in inches. The UI may display centimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Margins {
    pub const ZERO: Self = Self {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    pub fn all(v: f32) -> Self {
        let v = sane_length(v);
        Self {
            left: v,
            top: v,
            right: v,
            bottom: v,
        }
    }

    pub fn sane(self) -> Self {
        Self {
            left: sane_length(self.left),
            top: sane_length(self.top),
            right: sane_length(self.right),
            bottom: sane_length(self.bottom),
        }
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::all(1.0)
    }
}

/// The resolved physical frame around one particular output size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub image_inches: [f32; 2],
    pub outer_inches: [f32; 2],
    pub margins: Margins,
}

/// The same layout on the actual file grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelLayout {
    pub image: Dims,
    pub outer: Dims,
    pub left: usize,
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    /// Output pixels outside the authored physical frame. Currently either 0 or 1.
    pub trim: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("the image has no physical size")]
    EmptyImage,
    #[error("the image is wider than the selected frame")]
    TooWide,
    #[error("the image is taller than the selected frame")]
    TooTall,
}

/// Per-image FRAME state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameParams {
    pub enabled: bool,
    /// Independent of OUTPUT's display unit, by design.
    pub unit: Unit,
    pub priority: Priority,
    /// In Sides priority, use one authored value for all four sides.
    pub equal: bool,
    /// Authored values for Sides priority, always inches.
    pub margins: Margins,
    /// Fixed outer size for Frame priority, always inches.
    pub outer_inches: [f32; 2],
    /// The current outer dimensions were authored rather than chosen from the list.
    pub custom_size: bool,
    pub placement: Placement,
    /// Difference between bottom and top in Bottom Weight placement, in inches.
    pub bottom_weight_inches: f32,
    /// Position inside the available horizontal/vertical slack for Custom placement.
    /// 0 touches left/top, 1 touches right/bottom.
    pub custom_position: [f32; 2],
    /// Display-referred sRGB. It is converted at the final output boundary.
    pub color: [u8; 3],
    /// A one-output-pixel black perimeter outside the physical frame dimensions.
    pub trim_line: bool,
}

impl Default for FrameParams {
    fn default() -> Self {
        Self {
            enabled: false,
            unit: Unit::Inches,
            priority: Priority::Sides,
            equal: true,
            margins: Margins::default(),
            // A useful first fixed frame, landscape. The UI can swap it in one click.
            outer_inches: [24.0, 20.0],
            custom_size: false,
            placement: Placement::Centred,
            bottom_weight_inches: 1.0,
            custom_position: [0.5, 0.5],
            // Explicit white. Warmer mount colours are authored choices in the UI,
            // not a hidden opinion baked into a control called "Paper".
            color: [0xff, 0xff, 0xff],
            trim_line: false,
        }
    }
}

impl FrameParams {
    pub const MAX_INCHES: f32 = 400.0;
    /// Conventional outer-frame sizes, stored portrait and presented in this order.
    pub const PRESETS: [[f32; 2]; 8] = [
        [8.0, 10.0],
        [11.0, 14.0],
        [14.0, 17.0],
        [16.0, 20.0],
        [20.0, 24.0],
        [22.0, 28.0],
        [24.0, 32.0],
        [30.0, 40.0],
    ];

    pub fn is_active(self) -> bool {
        self.enabled
    }

    pub fn is_modified(self) -> bool {
        let mut authored = self;
        authored.unit = Self::default().unit;
        authored != Self::default()
    }

    pub fn needs_colour(self) -> bool {
        self.enabled && !(self.color[0] == self.color[1] && self.color[1] == self.color[2])
    }

    /// The bottom-minus-top margin needed to put the photograph's measured visual
    /// centre on the frame's geometric centre.
    ///
    /// `visual_y` is a fraction from the top of the photograph. A centre below 50%
    /// asks for a larger bottom margin and moves the photograph upward. Bottom Weight
    /// is deliberately one-directional, so a centre above 50% resolves to zero rather
    /// than silently becoming a top weight. The result is also limited by the actual
    /// vertical room in the selected outer frame.
    pub fn bottom_weight_for_visual_center(
        self,
        image_inches: [f32; 2],
        visual_y: f32,
    ) -> Result<f32, LayoutError> {
        let [iw, ih] = image_inches;
        if !iw.is_finite() || !ih.is_finite() || iw <= 0.0 || ih <= 0.0 {
            return Err(LayoutError::EmptyImage);
        }
        let [ow, oh] = self.outer_inches.map(sane_length);
        if iw > ow + 1e-4 {
            return Err(LayoutError::TooWide);
        }
        if ih > oh + 1e-4 {
            return Err(LayoutError::TooTall);
        }
        let room = (oh - ih).max(0.0);
        let visual_y = finite_clamp(visual_y, 0.0, 1.0, 0.5);
        Ok((ih * (visual_y * 2.0 - 1.0)).clamp(0.0, room))
    }

    /// Resolve authored measurements around a photograph of `image_inches`.
    pub fn layout(self, image_inches: [f32; 2]) -> Result<Layout, LayoutError> {
        let [iw, ih] = image_inches;
        if !iw.is_finite() || !ih.is_finite() || iw <= 0.0 || ih <= 0.0 {
            return Err(LayoutError::EmptyImage);
        }
        if !self.enabled {
            return Ok(Layout {
                image_inches,
                outer_inches: image_inches,
                margins: Margins::ZERO,
            });
        }

        let (outer_inches, margins) = match self.priority {
            Priority::Sides => {
                let m = if self.equal {
                    Margins::all(self.margins.left)
                } else {
                    self.margins.sane()
                };
                ([iw + m.left + m.right, ih + m.top + m.bottom], m)
            }
            Priority::Outer => {
                let ow = sane_length(self.outer_inches[0]);
                let oh = sane_length(self.outer_inches[1]);
                if iw > ow + 1e-4 {
                    return Err(LayoutError::TooWide);
                }
                if ih > oh + 1e-4 {
                    return Err(LayoutError::TooTall);
                }
                let (sx, sy) = ((ow - iw).max(0.0), (oh - ih).max(0.0));
                let m = match self.placement {
                    Placement::Centred => Margins {
                        left: sx * 0.5,
                        right: sx * 0.5,
                        top: sy * 0.5,
                        bottom: sy * 0.5,
                    },
                    Placement::BottomWeighted => {
                        let weight = sane_length(self.bottom_weight_inches).min(sy);
                        Margins {
                            left: sx * 0.5,
                            right: sx * 0.5,
                            top: (sy - weight) * 0.5,
                            bottom: (sy + weight) * 0.5,
                        }
                    }
                    Placement::Custom => {
                        let x = finite_clamp(self.custom_position[0], 0.0, 1.0, 0.5);
                        let y = finite_clamp(self.custom_position[1], 0.0, 1.0, 0.5);
                        let left = sx * x;
                        let top = sy * y;
                        Margins {
                            left,
                            right: sx - left,
                            top,
                            bottom: sy - top,
                        }
                    }
                };
                ([ow, oh], m)
            }
        };
        Ok(Layout {
            image_inches,
            outer_inches,
            margins,
        })
    }

    /// Resolve to pixels without ever resizing the photograph. Rounding residue is
    /// assigned to the far side, so the requested fixed outer size remains exact.
    pub fn pixel_layout(
        self,
        image: Dims,
        image_inches: [f32; 2],
    ) -> Result<PixelLayout, LayoutError> {
        let layout = self.layout(image_inches)?;
        if !self.enabled {
            return Ok(PixelLayout {
                image,
                outer: image,
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
                trim: 0,
            });
        }
        let sx = image.w as f32 / image_inches[0];
        let sy = image.h as f32 / image_inches[1];
        let outer_w = (layout.outer_inches[0] * sx).round().max(image.w as f32) as usize;
        let outer_h = (layout.outer_inches[1] * sy).round().max(image.h as f32) as usize;
        let left = (layout.margins.left * sx)
            .round()
            .clamp(0.0, (outer_w - image.w) as f32) as usize;
        let top = (layout.margins.top * sy)
            .round()
            .clamp(0.0, (outer_h - image.h) as f32) as usize;
        let trim = usize::from(self.trim_line);
        Ok(PixelLayout {
            image,
            outer: Dims {
                w: outer_w + trim * 2,
                h: outer_h + trim * 2,
            },
            left: left + trim,
            top: top + trim,
            right: outer_w - image.w - left + trim,
            bottom: outer_h - image.h - top + trim,
            trim,
        })
    }
}

/// Estimate the perceptual centre of a small, display-referred luminance image.
///
/// This is the monochrome form of Javier B\u{00ed}te's Visual Center method: pixels are
/// weighted by their colour-distance from the background, compressed by a cube-root,
/// and candidate centres are scored by distance-weighted support. The app supplies a
/// compact proxy of the *developed and cropped* photograph, so this is cheap enough to
/// run when the Optical button is pressed and never becomes part of interactive render.
///
/// Returns `[x, y]` as fractions from the top-left. A featureless image has no visual
/// preference and therefore returns the geometric centre.
pub fn visual_center(luma: &[f32], width: usize, height: usize) -> Option<[f32; 2]> {
    const ROUNDS: usize = 250;
    const COLOR_DIFF_WEIGHT_EXPO: f32 = 0.333;
    const DISTANCE_WEIGHT_EXPO: f32 = 0.5;

    if width == 0 || height == 0 || luma.len() != width.checked_mul(height)? {
        return None;
    }
    let background = finite_clamp(luma[0], 0.0, 1.0, 0.0);
    let max_difference = background.max(1.0 - background).max(f32::EPSILON);
    let weights: Vec<f32> = luma
        .iter()
        .map(|&v| {
            let v = if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                background
            };
            ((v - background).abs() / max_difference).powf(COLOR_DIFF_WEIGHT_EXPO)
        })
        .collect();
    if weights.iter().copied().sum::<f32>() <= f32::EPSILON {
        return Some([0.5, 0.5]);
    }

    let max_distance = (width as f32).hypot(height as f32).max(1.0);
    let score = |cx: f32, cy: f32| {
        weights
            .iter()
            .enumerate()
            .map(|(i, &weight)| {
                let x = (i % width) as f32;
                let y = (i / width) as f32;
                let distance = (x - cx).hypot(y - cy);
                let proximity = (1.0 - distance / max_distance).max(0.0);
                weight * proximity.powf(DISTANCE_WEIGHT_EXPO)
            })
            .sum::<f32>()
    };
    let best_axis = |horizontal: bool, fixed: f32| {
        let mut best = (0.5f32, f32::NEG_INFINITY);
        for step in 0..=ROUNDS {
            let position = step as f32 / ROUNDS as f32;
            let (cx, cy) = if horizontal {
                (position * width as f32, fixed * height as f32)
            } else {
                (fixed * width as f32, position * height as f32)
            };
            let value = score(cx, cy);
            if value > best.1 {
                best = (position, value);
            }
        }
        best.0
    };

    let x = best_axis(true, 0.5);
    let y = best_axis(false, x);
    Some([x, y])
}

fn sane_length(v: f32) -> f32 {
    finite_clamp(v, 0.0, FrameParams::MAX_INCHES, 0.0)
}

fn finite_clamp(v: f32, lo: f32, hi: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_sides_derive_the_outer_canvas() {
        let p = FrameParams {
            enabled: true,
            margins: Margins::all(2.0),
            ..Default::default()
        };
        let l = p.layout([10.0, 8.0]).unwrap();
        assert_eq!(l.outer_inches, [14.0, 12.0]);
        assert_eq!(l.margins, Margins::all(2.0));
    }

    #[test]
    fn a_fixed_frame_centres_opposing_sides_not_all_four() {
        let p = FrameParams {
            enabled: true,
            priority: Priority::Outer,
            outer_inches: [20.0, 24.0],
            ..Default::default()
        };
        let l = p.layout([14.0, 18.0]).unwrap();
        assert_eq!(l.margins.left, 3.0);
        assert_eq!(l.margins.right, 3.0);
        assert_eq!(l.margins.top, 3.0);
        assert_eq!(l.margins.bottom, 3.0);
    }

    #[test]
    fn bottom_weight_moves_the_picture_up_without_changing_outer_size() {
        let p = FrameParams {
            enabled: true,
            priority: Priority::Outer,
            outer_inches: [20.0, 24.0],
            placement: Placement::BottomWeighted,
            bottom_weight_inches: 2.0,
            ..Default::default()
        };
        let l = p.layout([14.0, 18.0]).unwrap();
        assert_eq!(l.outer_inches, [20.0, 24.0]);
        assert_eq!((l.margins.top, l.margins.bottom), (2.0, 4.0));
    }

    #[test]
    fn optical_weight_aligns_a_lower_visual_centre_and_respects_the_frame() {
        let p = FrameParams {
            enabled: true,
            priority: Priority::Outer,
            outer_inches: [20.0, 24.0],
            ..Default::default()
        };
        assert!(
            (p.bottom_weight_for_visual_center([14.0, 18.0], 0.55)
                .unwrap()
                - 1.8)
                .abs()
                < 1e-5
        );
        assert_eq!(
            p.bottom_weight_for_visual_center([14.0, 23.0], 0.75)
                .unwrap(),
            1.0
        );
        assert_eq!(
            p.bottom_weight_for_visual_center([14.0, 18.0], 0.4)
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn visual_center_follows_asymmetric_picture_weight() {
        let mut proxy = vec![0.0; 21 * 21];
        proxy[16 * 21 + 10] = 1.0;
        let centre = visual_center(&proxy, 21, 21).unwrap();
        assert!((centre[0] - 0.5).abs() < 0.08);
        assert!(centre[1] > 0.55);
        assert_eq!(visual_center(&[0.4; 9], 3, 3), Some([0.5, 0.5]));
    }

    #[test]
    fn fixed_outer_rounding_never_resizes_the_picture() {
        let p = FrameParams {
            enabled: true,
            priority: Priority::Outer,
            outer_inches: [20.0, 24.0],
            ..Default::default()
        };
        let l = p
            .pixel_layout(Dims { w: 4201, h: 5401 }, [14.0, 18.0])
            .unwrap();
        assert_eq!(l.image, Dims { w: 4201, h: 5401 });
        assert_eq!(l.left + l.image.w + l.right, l.outer.w);
        assert_eq!(l.top + l.image.h + l.bottom, l.outer.h);
    }

    #[test]
    fn trim_line_sits_outside_the_physical_frame_and_adds_two_pixels() {
        let p = FrameParams {
            enabled: true,
            margins: Margins::all(1.0),
            trim_line: true,
            ..Default::default()
        };
        let physical = p.layout([10.0, 8.0]).unwrap();
        let pixels = p.pixel_layout(Dims { w: 100, h: 80 }, [10.0, 8.0]).unwrap();

        assert_eq!(physical.outer_inches, [12.0, 10.0]);
        assert_eq!(pixels.outer, Dims { w: 122, h: 102 });
        assert_eq!(pixels.trim, 1);
        assert_eq!((pixels.left, pixels.top), (11, 11));
        assert_eq!((pixels.right, pixels.bottom), (11, 11));
    }

    #[test]
    fn an_image_that_does_not_fit_is_reported_not_resized() {
        let p = FrameParams {
            enabled: true,
            priority: Priority::Outer,
            outer_inches: [8.0, 10.0],
            ..Default::default()
        };
        assert_eq!(p.layout([9.0, 7.0]), Err(LayoutError::TooWide));
    }

    #[test]
    fn the_preset_list_is_the_authored_frame_family() {
        assert_eq!(
            FrameParams::PRESETS,
            [
                [8.0, 10.0],
                [11.0, 14.0],
                [14.0, 17.0],
                [16.0, 20.0],
                [20.0, 24.0],
                [22.0, 28.0],
                [24.0, 32.0],
                [30.0, 40.0],
            ]
        );
    }

    #[test]
    fn changing_only_the_display_unit_cannot_change_the_physical_frame() {
        let inches = FrameParams {
            enabled: true,
            unit: Unit::Inches,
            margins: Margins::all(1.25),
            ..Default::default()
        };
        let centimetres = FrameParams {
            unit: Unit::Centimetres,
            ..inches
        };
        assert_eq!(
            inches.layout([14.0, 18.0]),
            centimetres.layout([14.0, 18.0])
        );
    }
}
