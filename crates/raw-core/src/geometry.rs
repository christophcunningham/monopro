//! CFA geometry. This is the module that stops the off-by-2 bugs.
//!
//! Two facts, both measured rather than assumed (see `docs/corpus.md`):
//!
//! 1. The CFA pattern is defined against the full sensor array origin, but
//!    `crop_area` may start on an odd row or column. Fujifilm GFX 100S crops at
//!    top=7; Canon EOS 400D at top=23. In both, the colour at the crop origin is
//!    GREEN while the colour at sensor (0,0) is RED. Any code that indexes the CFA
//!    with crop-relative coordinates gets the wrong channel for the entire image.
//!
//! 2. SuperPixel bins a 2x2 quad, so the quad grid must be aligned to the sensor's
//!    Bayer phase, not to the crop rectangle. `CfaGeometry` therefore snaps the
//!    crop origin down to even/even and shrinks the extent to match.

/// Which filter sits over a photosite. G1/G2 are not distinguished here -- they are
/// averaged in the SuperPixel bin, per the design decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfaColor {
    Red = 0,
    Green = 1,
    Blue = 2,
}

impl CfaColor {
    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Red),
            1 => Some(Self::Green),
            2 => Some(Self::Blue),
            _ => None, // index 3 is the E (emerald) plane; not Bayer
        }
    }

    /// Whether a 2x2 quad is a Bayer permutation: one red, two green, one blue.
    ///
    /// **Being 2x2 and being Bayer are different claims, and the pipeline makes the
    /// second one everywhere.** SuperPixel averages "the quad's two greens";
    /// `Weighting::Photosite` is 1/4, 1/2, 1/4 *because* a Bayer quad has that
    /// census; `preview::linear` divides each channel by its own count. A quad that
    /// passed the size check but was, say, two reds over two blues would satisfy none
    /// of that and would come out as a plausible-looking wrong picture rather than as
    /// an error — which is the worst of the three outcomes. `Error::UnsupportedCfa`
    /// has always *said* "only 2x2 Bayer is handled"; this is what makes that true.
    pub fn is_bayer_quad(quad: &[[Self; 2]; 2]) -> bool {
        let mut census = [0u8; 3];
        for row in quad {
            for cell in row {
                census[*cell as usize] += 1;
            }
        }
        census == [1, 2, 1]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dims {
    pub w: usize,
    pub h: usize,
}

/// The mapping between the decoded buffer, the usable crop, and the Bayer phase.
#[derive(Debug, Clone)]
pub struct CfaGeometry {
    /// Stride of the decoded buffer, in photosites. Row `y` starts at `y * stride`.
    pub stride: usize,
    /// Full decoded array dimensions.
    pub full: Dims,
    /// Crop origin in ABSOLUTE sensor coordinates, snapped to even/even.
    pub crop_x: usize,
    pub crop_y: usize,
    /// Crop extent from the snapped origin, forced even so 2x2 binning is exact.
    pub crop: Dims,
    /// 2x2 Bayer pattern indexed as `pattern[row % 2][col % 2]`, in ABSOLUTE
    /// sensor coordinates.
    pub pattern: [[CfaColor; 2]; 2],
}

impl CfaGeometry {
    /// Snap a reported crop to an even origin and an even extent.
    ///
    /// Snapping the origin DOWN (rather than up) keeps the pattern phase identical
    /// to the sensor's, at the cost of including at most one extra row and column
    /// of the masked border. That border is dark, not garbage, and one photosite of
    /// it is cheaper than a phase error across the whole frame.
    pub fn new(
        stride: usize,
        full: Dims,
        crop_x: usize,
        crop_y: usize,
        crop_w: usize,
        crop_h: usize,
        pattern: [[CfaColor; 2]; 2],
    ) -> Self {
        let sx = crop_x & !1;
        let sy = crop_y & !1;
        // grow the extent by however much the origin moved, then round down to even
        let w = ((crop_w + (crop_x - sx)).min(full.w.saturating_sub(sx))) & !1;
        let h = ((crop_h + (crop_y - sy)).min(full.h.saturating_sub(sy))) & !1;
        Self {
            stride,
            full,
            crop_x: sx,
            crop_y: sy,
            crop: Dims { w, h },
            pattern,
        }
    }

    /// Colour of a photosite, in ABSOLUTE sensor coordinates. Never pass
    /// crop-relative coordinates to this.
    #[inline]
    pub fn color_at(&self, y: usize, x: usize) -> CfaColor {
        self.pattern[y & 1][x & 1]
    }

    /// Index into the decoded buffer for an absolute sensor coordinate.
    #[inline]
    pub fn index(&self, y: usize, x: usize) -> usize {
        y * self.stride + x
    }

    /// SuperPixel output dimensions -- half the cropped extent, both axes.
    pub fn superpixel_dims(&self) -> Dims {
        Dims {
            w: self.crop.w / 2,
            h: self.crop.h / 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CfaColor::*;

    const RGGB: [[CfaColor; 2]; 2] = [[Red, Green], [Green, Blue]];

    #[test]
    fn a_quad_can_be_two_by_two_without_being_bayer() {
        // The size check and the census are different questions, and the decoder used
        // to ask only the first. Each of these has four legal colour indices in a 2x2
        // and none of them has the 1/2/1 census the SuperPixel bin, the Photosite
        // weighting and the raw-linear preview all assume.
        assert!(CfaColor::is_bayer_quad(&RGGB));
        assert!(CfaColor::is_bayer_quad(&[[Blue, Green], [Green, Red]]));
        assert!(CfaColor::is_bayer_quad(&[[Green, Red], [Blue, Green]]));

        assert!(!CfaColor::is_bayer_quad(&[[Red, Red], [Blue, Blue]]));
        assert!(!CfaColor::is_bayer_quad(&[[Green, Green], [Green, Green]]));
        assert!(!CfaColor::is_bayer_quad(&[[Red, Green], [Green, Green]]));
    }

    #[test]
    fn odd_crop_origin_snaps_without_changing_phase() {
        // Fujifilm GFX 100S: crop origin (8, 7), odd top.
        let g = CfaGeometry::new(11808, Dims { w: 11808, h: 8754 }, 8, 7, 11648, 8736, RGGB);
        assert_eq!((g.crop_x, g.crop_y), (8, 6));
        // The snapped origin reports the same colour the absolute lookup does.
        assert_eq!(g.color_at(g.crop_y, g.crop_x), RGGB[0][0]);
        assert_eq!(g.crop.w % 2, 0);
        assert_eq!(g.crop.h % 2, 0);
    }

    #[test]
    fn crop_relative_lookup_would_have_been_wrong() {
        // The bug this module exists to prevent: at Fuji's reported crop top of 7,
        // a crop-relative lookup reports the colour of absolute row 0, not row 7.
        let absolute = RGGB[7 & 1][8 & 1];
        let crop_relative = RGGB[0][0];
        assert_ne!(absolute, crop_relative);
    }

    #[test]
    fn superpixel_halves_the_cropped_extent() {
        let g = CfaGeometry::new(5504, Dims { w: 5504, h: 3672 }, 12, 12, 5472, 3648, RGGB);
        assert_eq!(g.superpixel_dims(), Dims { w: 2736, h: 1824 });
    }
}
