//! Output: what the exported *file* is, as distinct from what the picture is.
//!
//! # Pixels, print size and PPI are three views of one decision
//!
//! `px = inches * ppi` has two degrees of freedom, and a UI offering all three as
//! independent fields is lying about one of them. The brief names the three shapes;
//! this module takes both of the ones a photographer actually wants, and the choice
//! between them is a single `Option`:
//!
//! ```text
//!   resize: None    the crop's own pixels are fixed. Print size and PPI are one
//!                   reciprocal pair over that constant -- ask for 16 inches and the
//!                   PPI falls out, ask for 360 ppi and the size falls out. NO pixel
//!                   is touched and the file is byte-identical whatever is typed.
//!
//!   resize: Some    print size and PPI are both given, so the pixel count is
//!                   derived and export RESAMPLES to it.
//! ```
//!
//! `None` is the default because it is the honest description of a file that is
//! simply written out, and because in that mode this whole module is a readout: a
//! user can play with the numbers to see what their negative will print at without
//! any risk of quietly resampling a master.
//!
//! Aspect is never a field. The unanchored dimension always derives from the crop's
//! aspect, so a print cannot be distorted by this panel -- which is also how the
//! Python prototype does it. Editing width anchors width; editing height anchors
//! height.
//!
//! # These parameters must not reach the render
//!
//! `Params::diff` deliberately omits `output` from its `render` term, so changing PPI
//! or print size produces `Dirty::NONE`. That is the brief's warning made structural:
//!
//! > If Output grows an "export at 50%" control, that is a property of the file being
//! > written and must not change the footer, the histogram or anything the viewport
//! > measures.
//!
//! The footer still quotes the print size, because that is the number a print is
//! ordered from -- but it quotes it from *here*, over the crop's unchanged pixels.
//! `an_output_change_is_not_a_render_change` holds the line.
//!
//! # What is NOT here
//!
//! Whether metadata travels with an export was briefly a field of this struct and is
//! now `Settings::export_metadata`. It is a standing decision about how someone works,
//! not a property of one picture — asking it per image would mean answering it again
//! for every image. The unit sizes are shown in sits in Settings for the same reason.

use crate::geometry::Dims;
use crate::resample::Filter;

/// The unit print sizes are shown in. A display preference, not a stored quantity:
/// everything below is canonically **inches**, because round-tripping a value through
/// centimetres at two decimals loses a thousandth of an inch every time the toggle is
/// pressed, and a size that drifts when you look at it is worse than either unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unit {
    #[default]
    Inches,
    Centimetres,
}

impl Unit {
    pub const UI_ORDER: [Self; 2] = [Self::Inches, Self::Centimetres];
    pub const CM_PER_IN: f32 = 2.54;

    /// The suffix, and the toggle's own label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Inches => "in",
            Self::Centimetres => "cm",
        }
    }

    pub fn key(self) -> &'static str {
        self.label()
    }

    pub fn from_key(s: &str) -> Option<Self> {
        Self::UI_ORDER.into_iter().find(|u| u.key() == s)
    }

    /// Display units per inch.
    pub fn per_inch(self) -> f32 {
        match self {
            Self::Inches => 1.0,
            Self::Centimetres => Self::CM_PER_IN,
        }
    }

    pub fn from_inches(self, inches: f32) -> f32 {
        inches * self.per_inch()
    }

    pub fn to_inches(self, shown: f32) -> f32 {
        shown / self.per_inch()
    }
}

/// Which dimension a custom print size is anchored to. The other one follows the
/// crop's aspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Axis {
    #[default]
    Width,
    Height,
}

/// A requested print size: one length, and which edge it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resize {
    /// Always inches. See `Unit`.
    pub inches: f32,
    pub axis: Axis,
}

impl Resize {
    /// The size a picture already is, which is what the control seeds itself with when
    /// it is switched on — so turning resizing on changes nothing until a number is
    /// moved, and the first thing you see is the truth rather than a default.
    pub fn native(picture: Dims, ppi: f32) -> Self {
        Self {
            inches: picture.w as f32 / ppi.max(1.0),
            axis: Axis::Width,
        }
    }
}

/// Everything about the file that is not about the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputParams {
    /// Pixels per inch. Written into the file's resolution tag, and the divisor every
    /// print-size readout in the app uses.
    ///
    /// This replaced a `const DPI: f32 = 300.0` that was quoted in four places, one of
    /// which was a separate literal `300` in the encoder call — so the app could have
    /// reported one number and tagged another.
    pub ppi: f32,
    /// `None` writes the crop's own pixels. `Some` resamples to `inches * ppi`.
    pub resize: Option<Resize>,
    /// Kept outside `Resize` so that turning resizing off and on again does not
    /// silently reset a deliberate choice of filter.
    pub filter: Filter,
}

impl Default for OutputParams {
    fn default() -> Self {
        Self {
            ppi: 300.0,
            resize: None,
            filter: Filter::default(),
        }
    }
}

impl OutputParams {
    /// What a print resolution may be set to. The low end is screen resolution, the
    /// high end is beyond any inkjet's addressable dot; the prototype used the same
    /// pair.
    pub const PPI_RANGE: std::ops::RangeInclusive<f32> = 72.0..=1440.0;

    /// Longest edge an export may be asked for, and the most pixels in one.
    ///
    /// Both, because either alone lets the other through: 30000 x 200 is a silly shape
    /// but harmless, while 25000 x 20000 is 500 MP and its f32 intermediate alone is
    /// 2 GB before the encoder allocates anything. 400 MP still covers a 40 x 60 inch
    /// print at 300 ppi and a 24 x 36 at 600, which is past where any of this stops
    /// being a print and starts being a billboard.
    ///
    /// A request past either limit is *refused*, not clamped: the panel says what it
    /// will write, and quietly writing a different size than the one on screen is the
    /// worse failure.
    pub const MAX_EDGE: u32 = 30_000;
    pub const MAX_PIXELS: u64 = 400_000_000;

    /// The pixel dimensions the file will have, given the picture's own.
    ///
    /// Not clamped to the limits — see `fits`. A caller that is about to allocate must
    /// ask `fits` first.
    pub fn target_dims(&self, picture: Dims) -> Dims {
        let Some(r) = self.resize else { return picture };
        if picture.w == 0 || picture.h == 0 {
            return picture;
        }
        let inches = r.inches.max(0.0);
        let anchored = (inches * self.ppi).round().max(1.0);
        // Derive the other edge from the *scale*, not from a second multiplication,
        // so the two axes cannot disagree by a pixel about what the aspect was.
        let (native, other) = match r.axis {
            Axis::Width => (picture.w, picture.h),
            Axis::Height => (picture.h, picture.w),
        };
        let scale = anchored / native as f32;
        let derived = ((other as f32) * scale).round().max(1.0);
        let (w, h) = match r.axis {
            Axis::Width => (anchored, derived),
            Axis::Height => (derived, anchored),
        };
        Dims {
            w: w as usize,
            h: h as usize,
        }
    }

    /// Whether the target is inside both limits.
    pub fn fits(&self, picture: Dims) -> bool {
        let d = self.target_dims(picture);
        let edge = Self::MAX_EDGE as usize;
        d.w <= edge && d.h <= edge && (d.w as u64) * (d.h as u64) <= Self::MAX_PIXELS
    }

    /// The printed size in inches, width then height.
    ///
    /// In both modes this is `target_dims / ppi`, which is the identity that makes the
    /// two modes one module rather than two: with `resize: None` it reports what the
    /// picture's own pixels come to, and with `Some` it returns the size that was
    /// asked for.
    pub fn print_inches(&self, picture: Dims) -> (f32, f32) {
        let d = self.target_dims(picture);
        let ppi = self.ppi.max(1.0);
        (d.w as f32 / ppi, d.h as f32 / ppi)
    }

    /// The linear scale export will resample by. 1.0 when it will not.
    pub fn scale(&self, picture: Dims) -> f32 {
        if picture.w == 0 {
            return 1.0;
        }
        self.target_dims(picture).w as f32 / picture.w as f32
    }

    /// Whether export will actually run the filter. False when the mode is off *and*
    /// when a requested size happens to land on the picture's own pixels, which is
    /// what keeps "type the native size back in" from costing a resample.
    pub fn resamples(&self, picture: Dims) -> bool {
        self.target_dims(picture) != picture
    }

    /// The PPI a given print size implies at the picture's own pixel count — the other
    /// half of the reciprocal pair, and what the panel writes back into `ppi` when a
    /// size is typed with resizing **off**.
    pub fn implied_ppi(picture: Dims, axis: Axis, inches: f32) -> f32 {
        let px = match axis {
            Axis::Width => picture.w,
            Axis::Height => picture.h,
        };
        if inches <= 0.0 {
            return *Self::PPI_RANGE.end();
        }
        (px as f32 / inches).clamp(*Self::PPI_RANGE.start(), *Self::PPI_RANGE.end())
    }

    /// Apply a typed print size along one axis.
    ///
    /// **This is the whole model in one function**, and the reason it lives here
    /// rather than inline in the panel: which of the three numbers moves depends on
    /// the mode, and that decision should be testable without a UI.
    ///
    /// - Resizing **on**: the size is the request. The pixel count follows it.
    /// - Resizing **off**: the pixels are fixed, so a size is a statement about
    ///   resolution and nothing else — the reciprocal half of the maintainer's ask, and the
    ///   half the prototype has no mode for.
    ///
    /// Either way, reading the size back gives the size that was typed;
    /// `typing_a_size_gives_that_size_back_in_both_modes` holds that.
    pub fn set_print_size(&mut self, picture: Dims, axis: Axis, inches: f32) {
        let inches = inches.max(0.01);
        match &mut self.resize {
            Some(r) => *r = Resize { inches, axis },
            None => self.ppi = Self::implied_ppi(picture, axis, inches),
        }
    }

    /// Turn resizing on or off.
    ///
    /// Switching it **on seeds the size from what the picture already is**, so the
    /// act of enabling it cannot change a file. A control that resampled the moment
    /// it was ticked would make the tick itself dangerous.
    pub fn set_resizing(&mut self, picture: Dims, on: bool) {
        self.resize = on.then(|| Resize::native(picture, self.ppi));
    }

    /// `1.19x upsample`, `0.63x downsample`, or `native` — the line under the size
    /// fields, and the one number that says whether what is being asked for is silly.
    pub fn scale_note(&self, picture: Dims) -> String {
        if !self.resamples(picture) {
            return "native".to_owned();
        }
        let s = self.scale(picture);
        let kind = if s > 1.0 { "upsample" } else { "downsample" };
        format!("{s:.2}x {kind}")
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIC: Dims = Dims { w: 6000, h: 4000 };

    #[test]
    fn a_unit_round_trips_through_its_key_and_its_conversion() {
        for u in Unit::UI_ORDER {
            assert_eq!(Unit::from_key(u.key()), Some(u));
            let there_and_back = u.to_inches(u.from_inches(12.5));
            assert!((there_and_back - 12.5).abs() < 1e-4, "{u:?}");
        }
        assert!((Unit::Centimetres.from_inches(10.0) - 25.4).abs() < 1e-4);
        assert_eq!(Unit::from_key("mm"), None);
    }

    #[test]
    fn with_resizing_off_the_pixels_never_move() {
        // The whole point of the default mode: PPI is a report, and no value of it may
        // change a single pixel of the file.
        for ppi in [72.0, 240.0, 300.0, 720.0, 1440.0] {
            let o = OutputParams {
                ppi,
                ..Default::default()
            };
            assert_eq!(o.target_dims(PIC), PIC, "{ppi} ppi moved the pixels");
            assert!(!o.resamples(PIC));
            assert_eq!(o.scale_note(PIC), "native");
        }
    }

    #[test]
    fn size_and_ppi_are_reciprocal_with_resizing_off() {
        // What the maintainer asked for, stated as the arithmetic: at fixed pixels, asking for
        // a size and asking for a resolution are the same request read two ways.
        let o = OutputParams {
            ppi: 300.0,
            ..Default::default()
        };
        let (w, h) = o.print_inches(PIC);
        assert!(
            (w - 20.0).abs() < 1e-3 && (h - 13.333).abs() < 1e-2,
            "{w} x {h}"
        );

        // Ask for 16 inches wide instead: the PPI follows, and the pixels do not.
        let ppi = OutputParams::implied_ppi(PIC, Axis::Width, 16.0);
        assert!((ppi - 375.0).abs() < 1e-3, "{ppi}");
        let o = OutputParams {
            ppi,
            ..Default::default()
        };
        assert_eq!(o.target_dims(PIC), PIC);
        assert!((o.print_inches(PIC).0 - 16.0).abs() < 1e-3);

        // And the height that implies, from the same aspect.
        assert!((o.print_inches(PIC).1 - 10.6667).abs() < 1e-3);
    }

    #[test]
    fn an_implied_ppi_stays_inside_the_range() {
        // A print two hundred inches wide would imply 30 ppi, and a postage stamp
        // 12000. Both clamp rather than producing a resolution no printer means.
        assert_eq!(OutputParams::implied_ppi(PIC, Axis::Width, 200.0), 72.0);
        assert_eq!(OutputParams::implied_ppi(PIC, Axis::Width, 0.5), 1440.0);
        assert_eq!(OutputParams::implied_ppi(PIC, Axis::Width, 0.0), 1440.0);
    }

    #[test]
    fn a_resize_derives_the_other_edge_from_the_aspect() {
        // 10 inches wide at 300 ppi is 3000 px; the height must follow the 3:2 crop,
        // not a second independent field.
        let o = OutputParams {
            ppi: 300.0,
            resize: Some(Resize {
                inches: 10.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        assert_eq!(o.target_dims(PIC), Dims { w: 3000, h: 2000 });
        assert!((o.scale(PIC) - 0.5).abs() < 1e-6);
        assert_eq!(o.scale_note(PIC), "0.50x downsample");

        // Anchored the other way, the same print size means a different file.
        let o = OutputParams {
            resize: Some(Resize {
                inches: 10.0,
                axis: Axis::Height,
            }),
            ..o
        };
        assert_eq!(o.target_dims(PIC), Dims { w: 4500, h: 3000 });
        assert_eq!(o.scale_note(PIC), "0.75x downsample");
    }

    #[test]
    fn a_resize_preserves_aspect_at_every_size() {
        // Proportions come from the arithmetic rather than from a check, so this is a
        // property test over sizes that do not divide evenly.
        let pic = Dims { w: 5472, h: 3648 };
        let native = pic.w as f32 / pic.h as f32;
        for inches in [1.0, 3.7, 8.25, 11.0, 17.3, 44.0] {
            for axis in [Axis::Width, Axis::Height] {
                let o = OutputParams {
                    ppi: 300.0,
                    resize: Some(Resize { inches, axis }),
                    ..Default::default()
                };
                let d = o.target_dims(pic);
                let got = d.w as f32 / d.h as f32;
                // One pixel of rounding on the short edge of a 300 px image is 0.3%.
                assert!(
                    (got - native).abs() / native < 0.005,
                    "{inches} {axis:?}: {d:?}"
                );
            }
        }
    }

    #[test]
    fn asking_for_the_native_size_back_does_not_resample() {
        // 20 inches at 300 ppi is exactly the 6000 px this picture already is. The
        // filter must not run — it would cost a full pass to produce the same image,
        // and `resample`'s own identity short circuit should never have to be reached.
        let o = OutputParams {
            ppi: 300.0,
            resize: Some(Resize {
                inches: 20.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        assert_eq!(o.target_dims(PIC), PIC);
        assert!(!o.resamples(PIC));
        assert_eq!(o.scale_note(PIC), "native");
    }

    #[test]
    fn the_limits_refuse_rather_than_clamp() {
        // 120 inches at 600 ppi is 72000 px on the long edge: past the edge limit and,
        // at this aspect, far past the pixel limit too. `target_dims` reports it
        // honestly and `fits` says no, so the panel can show the number it is refusing.
        let o = OutputParams {
            ppi: 600.0,
            resize: Some(Resize {
                inches: 120.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        assert_eq!(o.target_dims(PIC), Dims { w: 72000, h: 48000 });
        assert!(!o.fits(PIC));

        // The pixel limit bites before the edge limit on a squarer shape: 25000 x
        // 16667 is 417 MP with neither edge over 30000.
        let square = Dims { w: 3000, h: 2000 };
        let o = OutputParams {
            ppi: 1000.0,
            resize: Some(Resize {
                inches: 25.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        let d = o.target_dims(square);
        assert!(d.w < OutputParams::MAX_EDGE as usize && d.h < OutputParams::MAX_EDGE as usize);
        assert!(!o.fits(square), "{d:?} is {} MP", (d.w * d.h) / 1_000_000);

        // And an ordinary big print is fine.
        let o = OutputParams {
            ppi: 300.0,
            resize: Some(Resize {
                inches: 40.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        assert!(o.fits(PIC));
    }

    #[test]
    fn resizing_seeds_itself_from_what_the_picture_already_is() {
        let o = OutputParams {
            ppi: 300.0,
            ..Default::default()
        };
        let r = Resize::native(PIC, o.ppi);
        assert!((r.inches - 20.0).abs() < 1e-4);
        let o = OutputParams {
            resize: Some(r),
            ..o
        };
        assert!(
            !o.resamples(PIC),
            "switching resizing on must not change the file"
        );
    }

    #[test]
    fn typing_a_size_gives_that_size_back_in_both_modes() {
        // What the panel promises: the number you typed is the number that comes
        // back. The two modes get there by moving *different* things — one moves the
        // resolution, the other moves the pixel count — and a field that showed you
        // something other than what you typed would make either mode unusable.
        //
        // The sizes are ones whose implied resolution is inside `PPI_RANGE`. Outside
        // it the promise genuinely does not hold, and cannot:
        // `an_unprintable_size_snaps_to_the_resolution_limit` is that case, found by
        // this test asserting too much.
        for on in [false, true] {
            for axis in [Axis::Width, Axis::Height] {
                for inches in [7.5f32, 16.0, 30.0] {
                    let mut o = OutputParams::default();
                    o.set_resizing(PIC, on);
                    o.set_print_size(PIC, axis, inches);
                    let (w, h) = o.print_inches(PIC);
                    let got = match axis {
                        Axis::Width => w,
                        Axis::Height => h,
                    };
                    // A pixel of rounding at 300 ppi is 1/300 inch; three of those.
                    assert!(
                        (got - inches).abs() < 0.01,
                        "resizing={on} {axis:?} {inches}: got {got}"
                    );
                    // And the mode's own invariant held while it did: only the
                    // resizing mode may have moved a pixel.
                    assert_eq!(o.resamples(PIC), on, "resizing={on} {axis:?} {inches}");
                }
            }
        }
    }

    #[test]
    fn an_unprintable_size_snaps_to_the_resolution_limit() {
        // With resizing off the pixel count is fixed, so a small enough print implies
        // a resolution past anything a printer addresses. 6000 px at 4 inches is 1500
        // ppi; the range stops at 1440, so the size settles at 4.17 rather than the 4
        // that was typed.
        //
        // That is the honest answer and not a rounding failure — the file really
        // cannot be that small at that pixel count — and the field showing 4.17 is
        // what says so. The way OUT of it is the Resample switch, which is exactly
        // what it is for, and the second half here checks that it works.
        let mut o = OutputParams::default();
        o.set_print_size(PIC, Axis::Width, 4.0);
        assert_eq!(o.ppi, *OutputParams::PPI_RANGE.end());
        let (w, _) = o.print_inches(PIC);
        assert!((w - 4.1666).abs() < 0.01, "{w}");
        assert_eq!(
            o.target_dims(PIC),
            PIC,
            "a clamp must still not move a pixel"
        );

        // With resizing on, 4 inches is simply 4 inches — the pixels give way instead.
        o.set_resizing(PIC, true);
        o.ppi = 300.0;
        o.set_print_size(PIC, Axis::Width, 4.0);
        assert!((o.print_inches(PIC).0 - 4.0).abs() < 0.01);
        assert_eq!(o.target_dims(PIC), Dims { w: 1200, h: 800 });
    }

    #[test]
    fn switching_resizing_on_does_not_change_the_file() {
        // Ticking the box must be safe. It seeds from the picture's own size, so
        // nothing resamples until a number is actually moved.
        for ppi in [72.0, 300.0, 720.0] {
            let mut o = OutputParams {
                ppi,
                ..Default::default()
            };
            let before = o.target_dims(PIC);
            o.set_resizing(PIC, true);
            assert_eq!(
                o.target_dims(PIC),
                before,
                "{ppi} ppi: enabling resized the file"
            );
            assert!(!o.resamples(PIC));
            // ...and off again returns to the same place.
            o.set_resizing(PIC, false);
            assert_eq!(o.target_dims(PIC), PIC);
        }
    }

    #[test]
    fn with_resizing_off_a_typed_size_moves_only_the_resolution() {
        // The reciprocal pair, end to end: type 16 inches on a 6000 px picture and
        // the resolution becomes 375 while every pixel stays where it was.
        let mut o = OutputParams::default();
        o.set_print_size(PIC, Axis::Width, 16.0);
        assert!((o.ppi - 375.0).abs() < 1e-3, "{}", o.ppi);
        assert_eq!(o.target_dims(PIC), PIC);

        // And the other direction: set the resolution, read the size.
        o.ppi = 250.0;
        let (w, _) = o.print_inches(PIC);
        assert!((w - 24.0).abs() < 1e-3, "{w}");
        assert_eq!(o.target_dims(PIC), PIC);
    }

    #[test]
    fn a_degenerate_picture_does_not_divide_by_zero() {
        let o = OutputParams {
            resize: Some(Resize {
                inches: 10.0,
                axis: Axis::Width,
            }),
            ..Default::default()
        };
        let empty = Dims { w: 0, h: 0 };
        assert_eq!(o.target_dims(empty), empty);
        assert_eq!(o.scale(empty), 1.0);
        let (w, h) = o.print_inches(empty);
        assert_eq!((w, h), (0.0, 0.0));
    }
}
