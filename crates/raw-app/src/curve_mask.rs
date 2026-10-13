//! A Curve pass's mask: what shape a pick starts as, what a press on empty canvas does to the
//! one that is there, and what the Tone switch and the point sampler's "limit to tone" do
//! to the mask.
//!
//! # Why this is not in `curve_mask_tool`
//!
//! The same reason `paint` and `crop` are not inside `viewport_panel`: where a shape
//! lands on a quarter-turned, straightened, cropped negative is arithmetic with a way
//! to be wrong that only shows on the pictures you did not test. Kept here it is pure
//! functions with tests that name the cases, where inside the tool it would be forty
//! untested lines in a function that also owns the pointer and the painter.
//!
//! # The shapes are Dodge & Burn's
//!
//! A mask's shape is a [`Shape`], the handles are [`paint::handles`] and
//! [`paint::move_handle`], and the overlay is `App::gradient_overlay`, so a mask is
//! placed and adjusted with the gesture a gradient layer already has. What differs is
//! only how it begins: a layer begins as a zero-length drag under the cursor, because
//! its point is the drag, and a mask begins **placed**, because picking a shape on a
//! curve should change the picture at once and the tool then refines it.
//!
//! A painted mask, Round or Card, is the fourth of them and begins the other way: with
//! nothing painted, since its point is the painting. It is painted with Dodge & Burn's
//! brush (`paint::begin_pass`, `paint::extend_pass`), and what it does on being picked
//! is [`pick`].

use raw_core::curve::{CurveInstance, CurveMask, Rule};
use raw_core::dodgeburn::{Linear, Radial, Shape, ZoneMask};
use raw_core::Frame;

use crate::paint::{self, End, Pick, Tool};

/// Which row button a mask's shape lights.
pub fn pick_of(shape: &Shape) -> Pick {
    match shape {
        Shape::Brush { nib, .. } => Pick::of(Tool::Brush, *nib),
        Shape::Linear(_) => Pick::Linear,
        Shape::Radial(_) => Pick::Radial,
    }
}

/// The shape a pick starts as, laid on the picture **as it is shown**.
///
/// - **Linear** is a sky grad: full at the top edge of the picture and gone by its
///   middle, which is where a darkroom would start burning a sky.
/// - **Radial** is a soft ellipse in the centre, with the proportions of the frame, and
///   reaching a third of the way across it.
///
/// - **Round** and **Card** are painted, and start with nothing painted: the pass is
///   painted in, as a Dodge & Burn layer is, so until the first stroke it applies
///   nowhere.
///
/// "Top", "middle" and "across" are the *frame's*, not the stored image's: a portrait
/// shot is stored on its side, and a sky grad laid along the stored top would run down
/// the side of the picture you are looking at. So the points are placed in frame pixels
/// and carried onto the negative through `frame`, which is also what keeps them right
/// under a straighten. `size` is the stored image's width and height, which the
/// negative's own coordinates are fractions of.
pub fn starting_shape(pick: Pick, frame: &Frame, size: (f32, f32)) -> Shape {
    let (lw, lh) = size;
    let crop = frame.crop;
    let (x, y, w, h) = (crop.x as f32, crop.y as f32, crop.w as f32, crop.h as f32);
    let (cx, cy) = (x + w * 0.5, y + h * 0.5);
    let onto = |fx: f32, fy: f32| {
        let (sx, sy) = frame.to_source(fx, fy);
        (sx / lw.max(1.0), sy / lh.max(1.0))
    };
    match pick {
        Pick::Linear => {
            let (x0, y0) = onto(cx, y);
            let (x1, y1) = onto(cx, cy);
            Shape::Linear(Linear {
                x0,
                y0,
                x1,
                y1,
                feather: 1.0,
                ev: 0.0,
            })
        }
        Pick::Radial => {
            let (rx, ry) = onto(cx, cy);
            let mut shape = Shape::Radial(Radial {
                cx: rx,
                cy: ry,
                inner: 0.0,
                // Set from the handle below; it only has to exist to be moved.
                outer: 1.0e-3,
                aspect: (h / w.max(1.0)).clamp(
                    *Radial::ASPECT_RANGE.start(),
                    *Radial::ASPECT_RANGE.end(),
                ),
                angle: 0.0,
                feather: 1.0,
                invert: false,
                ev: 0.0,
            });
            // The outer handle, a third of the frame's width to the right of the
            // centre: dragging a radial's handle sets its reach **and** its turn, so
            // this one placement gets a quarter-turned or straightened negative right.
            paint::move_handle(
                &mut shape,
                End::To,
                onto(cx + w * 0.35, cy),
                lh / lw.max(1.0),
            );
            shape
        }
        Pick::Round | Pick::Card => Shape::Brush {
            nib: pick.nib().unwrap_or_default(),
            passes: Vec::new(),
        },
    }
}

/// A shape picked on a Curve instance's MASK row: what it does to the mask.
///
/// - **The shape the mask already has** changes nothing; the caller opens the tool on it.
/// - **The other nib, on a mask already painted,** changes the nib and keeps the
///   strokes. A painted mask is coverage, and coverage laid with a disc and then a
///   card is still coverage; throwing away an afternoon's strokes because the next one
///   wanted a straight edge would be the worse reading of a click. A dab carries its
///   own nib, so what is painted does not change shape.
/// - **Any other shape** replaces the mask's, keeps its tone and its rule, and starts
///   un-inverted, since the invert was the old shape's. An instance with no mask gets
///   one.
///
/// Either way the mask is switched on. Returns whether the mask's shape is now painted,
/// which is whether the tool that opens on it is the brush.
pub fn pick(instance: &mut CurveInstance, pick: Pick, frame: &Frame, size: (f32, f32)) -> bool {
    let has = instance.mask.as_ref().and_then(|m| m.shape.as_ref());
    if has.map(pick_of) != Some(pick) {
        match (instance.mask.as_mut(), pick.nib()) {
            (
                Some(CurveMask {
                    shape: Some(Shape::Brush { nib, .. }),
                    ..
                }),
                Some(picked),
            ) => *nib = picked,
            (Some(mask), _) => {
                mask.shape = Some(starting_shape(pick, frame, size));
                mask.invert = false;
            }
            (None, _) => {
                instance.mask = Some(CurveMask::of_shape(starting_shape(pick, frame, size)));
            }
        }
    }
    let mask = instance.mask.as_mut().expect("just ensured");
    mask.enabled = true;
    matches!(mask.shape, Some(Shape::Brush { .. }))
}

/// A press on empty canvas: lay the shape again from `at`.
///
/// Dodge & Burn's gradient gesture, and like it the press and the drag are one gesture,
/// so this leaves a shape with no extent for the drag that follows to give one. **What
/// it keeps is the Transition and the ellipse**, which a layer's press resets: those are
/// the two things set in the panel rather than on the picture, and losing them to a
/// click that meant "here, instead" would undo work the click had nothing to say about.
pub fn relay(shape: &mut Shape, at: (f32, f32)) {
    match shape {
        Shape::Linear(l) => {
            (l.x0, l.y0) = at;
            (l.x1, l.y1) = at;
        }
        Shape::Radial(r) => {
            (r.cx, r.cy) = at;
            // A point, for the drag to widen. The core is part of the reach, so it goes
            // with it; `Aspect` and `Angle` stay, and the drag turns the angle anyway.
            r.outer = 0.0;
            r.inner = 0.0;
        }
        Shape::Brush { .. } => {}
    }
}

/// The Tone switch on a Curve instance's MASK section.
///
/// **A tone is the mask's second half, and it works with or without a shape**, so the
/// switch has three cases and the middle one is the one that makes it worth a function:
///
/// - *No mask yet.* Switching the tone on **makes the mask**, a tone alone and a
///   luminosity mask across the whole frame. That is how a tone-only mask starts.
/// - *A mask with no tone.* It gains an open range, switched on, and keeps its shape.
/// - *A range already there.* The switch flips its own `enabled` and nothing else, so the
///   ruler is still where it was when the tone comes back on.
///
/// Turning a tone **on** also switches the mask on, because asking for a tone is asking to
/// see it work; turning it off leaves the mask's own switch alone, since the shape may be
/// doing the work.
pub fn flip_tone(instance: &mut CurveInstance) {
    let mask = instance.mask.get_or_insert_with(empty);
    match &mut mask.tone {
        Some(tone) => tone.enabled = !tone.enabled,
        None => {
            mask.tone = Some(ZoneMask {
                enabled: true,
                ..ZoneMask::default()
            });
        }
    }
    if mask.live_tone().is_some() {
        mask.enabled = true;
    }
}

/// A mask with neither part yet, switched on, for a tone to be put into.
fn empty() -> CurveMask {
    CurveMask {
        enabled: true,
        shape: None,
        invert: false,
        tone: None,
        rule: Rule::Intersect,
    }
}

/// The settings the point sampler's "limit to tone" reads the picture with: the
/// instance's own tone range when it has one, so a range that was made per-pixel is read
/// per-pixel, and otherwise what a new tone part starts as.
pub fn tone_to_limit(instance: &CurveInstance) -> ZoneMask {
    instance
        .mask
        .as_ref()
        .and_then(|mask| mask.tone)
        .unwrap_or_default()
}

/// The point sampler's "limit to tone": set the instance's tone range around `ev`, the
/// tone the picture's basis reads where the point was sampled.
///
/// **One click gives the pass a point and the tones the point is for.** The range is one
/// zone around the tone clicked, with one-zone shoulders, so dragging the point that was
/// just placed moves those tones and leaves the rest of the picture where it was: the
/// curve tool pointed at a range rather than at everything.
///
/// It makes the mask if there is none, and the tone part if the mask has only a shape,
/// keeping the shape and the rule: limiting a sky grad's curve to the tone you clicked
/// inside it is the Intersect the pair starts with. Both switches come on, because the
/// option was chosen in order to see it work. A range already there is replaced, and
/// keeps how it reads the picture and its `Inv`.
pub fn limit_to_tone(instance: &mut CurveInstance, ev: f32) {
    let mask = instance.mask.get_or_insert_with(empty);
    mask.tone.get_or_insert_with(ZoneMask::default).sample(ev, ev);
    mask.enabled = true;
}

/// Whether a mask has anything in it to look at: a shape, or a tone that is switched on.
/// Its own switch does not come into it, since a mask that is off is still a thing you
/// may want to see before turning it on.
pub fn viewable(mask: &CurveMask) -> bool {
    mask.shape.is_some() || mask.live_tone().is_some()
}

/// How the mask is shown on the picture this frame, if at all.
///
/// A look pinned in the MASK header wins: it was asked for, and stays. Otherwise the
/// mask shows itself **while it is being shaped** and goes when the drag ends, with no
/// key to hold:
///
/// - **A handle dragged on the picture** shows the red film. The shape is being placed
///   against the subject, which has to stay visible to place it against.
/// - **A handle dragged on the tone ruler** shows the mask alone. The pointer is in the
///   panel, and what is being judged is the selection as a shape, which is easier
///   without the photograph it is laid on.
pub fn shown(
    pinned: Option<raw_gpu::MaskLook>,
    dragging_shape: bool,
    dragging_tone: bool,
) -> Option<raw_gpu::MaskLook> {
    use raw_gpu::MaskLook;
    pinned.or(if dragging_shape {
        Some(MaskLook::Overlay)
    } else if dragging_tone {
        Some(MaskLook::Mask)
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::composition::{CompositionParams, Orientation};
    use raw_core::{Dims, curve::CurveMask};

    /// A 3:2 landscape negative, stored the way it is shown.
    const SOURCE: Dims = Dims { w: 300, h: 200 };

    fn frame(orientation: Orientation) -> Frame {
        Frame::resolve(SOURCE, orientation, &CompositionParams::default())
    }

    fn size() -> (f32, f32) {
        (SOURCE.w as f32, SOURCE.h as f32)
    }

    /// The mask's weight at a point of the picture AS SHOWN, given in frame pixels.
    fn weight_at_frame(frame: &Frame, shape: &Shape, fx: f32, fy: f32) -> f32 {
        let (sx, sy) = frame.to_source(fx, fy);
        CurveMask::of_shape(shape.clone()).weight_at(
            sx / SOURCE.w as f32,
            sy / SOURCE.h as f32,
            SOURCE.h as f32 / SOURCE.w as f32,
            0.0,
        )
    }

    #[test]
    fn a_pick_lights_the_button_of_the_shape_it_made() {
        for pick in Pick::ALL {
            let shape = starting_shape(pick, &frame(Orientation::Rotate0), size());
            assert_eq!(pick_of(&shape), pick);
        }
    }

    #[test]
    fn a_painted_mask_starts_with_nothing_painted_and_holds_the_pass_back() {
        let f = frame(Orientation::Rotate0);
        for (picked, nib) in [(Pick::Round, raw_core::dodgeburn::Nib::Round), (Pick::Card, raw_core::dodgeburn::Nib::Card)] {
            let mut instance = plain();
            assert!(pick(&mut instance, picked, &f, size()), "the brush is the tool");
            let mask = instance.mask.as_ref().expect("the pick made a mask");
            assert!(mask.is_live());
            assert_eq!(mask.shape.as_ref().and_then(Shape::nib), Some(nib));
            assert!(mask.shape.as_ref().is_some_and(|s| s.passes().is_empty()));
            // Nothing painted is nothing selected, wherever you look.
            for (x, y) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.9)] {
                assert_eq!(mask.weight_at(x, y, 0.667, 0.0), 0.0);
            }
        }
    }

    #[test]
    fn picking_the_other_nib_keeps_what_was_painted() {
        use raw_core::dodgeburn::{Dab, Gesture, Nib};
        let f = frame(Orientation::Rotate0);
        let mut instance = plain();
        pick(&mut instance, Pick::Round, &f, size());
        if let Some(passes) = instance.mask.as_mut().and_then(|m| m.shape.as_mut()).and_then(Shape::passes_mut) {
            passes.push(Gesture::new(vec![Dab {
                x: 0.5,
                y: 0.5,
                radius: 0.2,
                opacity: 1.0,
                ev: 1.0,
                ..Dab::ROUND
            }]));
        }
        let mask = instance.mask.as_mut().unwrap();
        mask.invert = true;
        mask.enabled = false;
        assert!(pick(&mut instance, Pick::Card, &f, size()));
        let mask = instance.mask.as_ref().unwrap();
        let Some(Shape::Brush { nib, passes }) = &mask.shape else {
            panic!("no longer painted");
        };
        assert_eq!(*nib, Nib::Card, "the next stroke is a card's");
        assert_eq!(passes.len(), 1, "the strokes were thrown away");
        assert_eq!(passes[0].dabs[0].nib, Nib::Round, "and what was painted is as it was");
        assert!(mask.invert, "a nib is not a new shape, so the invert stays");
        assert!(mask.enabled, "picking is asking to see it");

        // The same pick again changes nothing at all.
        let before = instance.clone();
        assert!(pick(&mut instance, Pick::Card, &f, size()));
        assert_eq!(instance, before);

        // A gradient replaces the paint, and keeps the tone and the rule.
        flip_tone(&mut instance);
        instance.mask.as_mut().unwrap().rule = Rule::Add;
        assert!(!pick(&mut instance, Pick::Linear, &f, size()));
        let mask = instance.mask.as_ref().unwrap();
        assert!(matches!(mask.shape, Some(Shape::Linear(_))));
        assert!(!mask.invert, "the invert was the old shape's");
        assert!(mask.tone.is_some() && mask.rule == Rule::Add);
    }

    #[test]
    fn a_sky_grad_is_full_at_the_top_and_gone_by_the_middle_however_the_picture_is_turned() {
        // Landscape as shot, and the same negative turned a quarter either way, which
        // is a portrait on screen and a stored image on its side. In every case the
        // top of what is SHOWN is full and the middle is gone: the grad is placed in
        // the frame, not on the stored image.
        for orientation in [
            Orientation::Rotate0,
            Orientation::Rotate90,
            Orientation::Rotate270,
        ] {
            let f = frame(orientation);
            let shape = starting_shape(Pick::Linear, &f, size());
            let (w, h) = (f.crop.w as f32, f.crop.h as f32);
            for across in [0.1, 0.5, 0.9] {
                let fx = f.crop.x as f32 + w * across;
                let top = weight_at_frame(&f, &shape, fx, f.crop.y as f32 + 0.5);
                assert!(top > 0.99, "{orientation:?}: top at {across}: {top}");
                let middle = weight_at_frame(&f, &shape, fx, f.crop.y as f32 + h * 0.5);
                assert!(middle < 0.01, "{orientation:?}: middle at {across}: {middle}");
                let bottom = weight_at_frame(&f, &shape, fx, f.crop.y as f32 + h - 0.5);
                assert!(bottom < 0.01, "{orientation:?}: bottom at {across}: {bottom}");
                let quarter = weight_at_frame(&f, &shape, fx, f.crop.y as f32 + h * 0.25);
                assert!(
                    (quarter - 0.5).abs() < 0.05,
                    "{orientation:?}: a quarter of the way down is half way through: {quarter}"
                );
            }
        }
    }

    #[test]
    fn a_soft_ellipse_sits_in_the_centre_with_the_frames_proportions() {
        for orientation in [Orientation::Rotate0, Orientation::Rotate90] {
            let f = frame(orientation);
            let shape = starting_shape(Pick::Radial, &f, size());
            let (x, y) = (f.crop.x as f32, f.crop.y as f32);
            let (w, h) = (f.crop.w as f32, f.crop.h as f32);
            let (cx, cy) = (x + w * 0.5, y + h * 0.5);
            assert!(weight_at_frame(&f, &shape, cx, cy) > 0.99, "{orientation:?}");
            // Across the frame's width it reaches 35% of the way and stops.
            assert!(weight_at_frame(&f, &shape, cx + w * 0.30, cy) > 0.0);
            assert!(weight_at_frame(&f, &shape, cx + w * 0.37, cy) < 1.0e-3);
            // Down the frame's height it reaches the same fraction OF THE HEIGHT, which
            // is what "the frame's proportions" means: an ellipse that fills a
            // landscape frame the way it fills a portrait one.
            assert!(weight_at_frame(&f, &shape, cx, cy + h * 0.30) > 0.0, "{orientation:?}");
            assert!(
                weight_at_frame(&f, &shape, cx, cy + h * 0.37) < 1.0e-3,
                "{orientation:?}: it spilled past the frame's proportions"
            );
        }
    }

    #[test]
    fn a_press_lays_the_shape_again_and_keeps_what_the_panel_set() {
        let f = frame(Orientation::Rotate0);
        let mut shape = starting_shape(Pick::Linear, &f, size());
        if let Shape::Linear(l) = &mut shape {
            l.feather = 0.4;
        }
        relay(&mut shape, (0.3, 0.7));
        let Shape::Linear(l) = shape else { panic!() };
        assert_eq!((l.x0, l.y0, l.x1, l.y1), (0.3, 0.7, 0.3, 0.7));
        assert_eq!(l.feather, 0.4, "the Transition was reset by a click");

        let mut shape = starting_shape(Pick::Radial, &f, size());
        if let Shape::Radial(r) = &mut shape {
            r.aspect = 2.5;
            r.feather = 0.3;
            r.inner = 0.1;
        }
        relay(&mut shape, (0.2, 0.4));
        let Shape::Radial(r) = shape else { panic!() };
        assert_eq!((r.cx, r.cy), (0.2, 0.4));
        assert_eq!((r.outer, r.inner), (0.0, 0.0), "a point, for the drag to widen");
        assert_eq!((r.aspect, r.feather), (2.5, 0.3), "the ellipse and Transition stay");
    }

    #[test]
    fn a_press_with_no_drag_leaves_a_shape_the_math_can_hold() {
        // Between the press and the first movement the shape has no extent. It must
        // read as something finite, or the first frame of every drag is a NaN.
        let f = frame(Orientation::Rotate0);
        for pick in [Pick::Linear, Pick::Radial] {
            let mut shape = starting_shape(pick, &f, size());
            relay(&mut shape, (0.5, 0.5));
            for (x, y) in [(0.5, 0.5), (0.1, 0.9), (0.9, 0.1)] {
                let w = CurveMask::of_shape(shape.clone()).weight_at(x, y, 0.667, 0.0);
                assert!(w.is_finite() && (0.0..=1.0).contains(&w), "{pick:?}: {w}");
            }
        }
    }

    fn plain() -> CurveInstance {
        CurveInstance::new("Curve 1".into())
    }

    fn shaped() -> CurveInstance {
        let mut instance = plain();
        let frame = frame(Orientation::Rotate0);
        instance.mask = Some(CurveMask::of_shape(
            starting_shape(Pick::Linear, &frame, size()),
        ));
        instance
    }

    #[test]
    fn switching_a_tone_on_with_no_mask_makes_a_tone_only_mask() {
        let mut instance = plain();
        flip_tone(&mut instance);
        let mask = instance.mask.as_ref().expect("the switch made a mask");
        assert!(mask.enabled && mask.shape.is_none());
        assert!(mask.live_tone().is_some(), "and its tone is on");
        assert!(mask.is_live(), "a tone alone is a whole mask");
        // It starts open, which selects everything until the ruler is moved.
        assert!(mask.live_tone().is_some_and(|t| t.is_identity()));
    }

    #[test]
    fn a_tone_added_to_a_shape_leaves_the_shape_alone() {
        let mut instance = shaped();
        let shape = instance.mask.as_ref().unwrap().shape.clone();
        flip_tone(&mut instance);
        let mask = instance.mask.as_ref().unwrap();
        assert_eq!(mask.shape, shape);
        assert!(mask.live_tone().is_some());
        assert_eq!(mask.rule, Rule::Intersect, "a new pair intersects");
    }

    #[test]
    fn switching_a_tone_off_and_on_keeps_the_range() {
        let mut instance = plain();
        flip_tone(&mut instance);
        if let Some(tone) = instance.mask.as_mut().and_then(|m| m.tone.as_mut()) {
            (tone.lo, tone.hi, tone.invert) = (-1.5, 2.0, true);
        }
        let ruler = instance.mask.as_ref().unwrap().tone;
        flip_tone(&mut instance);
        let mask = instance.mask.as_ref().unwrap();
        assert!(mask.live_tone().is_none(), "off");
        assert!(!mask.is_live(), "a mask with only a tone that is off limits nothing");
        assert_eq!(
            mask.tone.map(|t| (t.lo, t.hi, t.invert)),
            ruler.map(|t| (t.lo, t.hi, t.invert)),
            "the range was kept"
        );
        flip_tone(&mut instance);
        assert_eq!(instance.mask.as_ref().unwrap().tone.map(|t| (t.lo, t.hi, t.invert)), Some((-1.5, 2.0, true)));
    }

    #[test]
    fn a_mask_shows_itself_while_it_is_shaped_and_a_pinned_look_stays() {
        use raw_gpu::MaskLook;
        assert_eq!(shown(None, false, false), None, "at rest, the picture");
        assert_eq!(shown(None, true, false), Some(MaskLook::Overlay), "placing a shape");
        assert_eq!(shown(None, false, true), Some(MaskLook::Mask), "shaping the range");
        // Pinned, it is what was asked for, whatever is being dragged.
        for pinned in [MaskLook::Mask, MaskLook::Overlay] {
            for (shape, tone) in [(false, false), (true, false), (false, true)] {
                assert_eq!(shown(Some(pinned), shape, tone), Some(pinned));
            }
        }
    }

    #[test]
    fn a_mask_can_be_looked_at_when_it_has_a_part_even_switched_off() {
        let mut instance = shaped();
        let mask = instance.mask.as_mut().unwrap();
        assert!(viewable(mask));
        mask.enabled = false;
        assert!(viewable(mask), "off is still worth seeing before it goes on");
        mask.shape = None;
        assert!(!viewable(mask), "nothing in it");
        // A tone alone counts while it is on.
        let mut toned = plain();
        flip_tone(&mut toned);
        assert!(viewable(toned.mask.as_ref().unwrap()));
        flip_tone(&mut toned);
        assert!(!viewable(toned.mask.as_ref().unwrap()));
    }

    #[test]
    fn limiting_to_a_tone_makes_a_tone_only_mask_around_it() {
        let mut instance = plain();
        assert_eq!(tone_to_limit(&instance), ZoneMask::default());
        limit_to_tone(&mut instance, 1.5);
        let mask = instance.mask.as_ref().expect("the click made a mask");
        assert!(mask.is_live() && mask.shape.is_none());
        let tone = mask.live_tone().expect("with a tone that is on");
        assert_eq!((tone.lo, tone.hi), (1.0, 2.0), "one zone around the tone clicked");
        assert_eq!((tone.f_lo, tone.f_hi), (1.0, 1.0));
        assert!(tone.edge_aware, "a new range reads the picture regionally");
    }

    #[test]
    fn limiting_a_shaped_mask_keeps_the_shape_and_the_rule() {
        let mut instance = shaped();
        if let Some(mask) = &mut instance.mask {
            mask.rule = Rule::Subtract;
            mask.enabled = false;
        }
        let shape = instance.mask.as_ref().unwrap().shape.clone();
        limit_to_tone(&mut instance, -1.0);
        let mask = instance.mask.as_ref().unwrap();
        assert_eq!(mask.shape, shape);
        assert_eq!(mask.rule, Rule::Subtract);
        assert!(mask.enabled, "asking for the limit is asking to see it");
        assert_eq!(mask.live_tone().map(|t| (t.lo, t.hi)), Some((-1.5, -0.5)));
    }

    #[test]
    fn limiting_again_replaces_the_range_and_keeps_how_it_reads() {
        let mut instance = plain();
        limit_to_tone(&mut instance, 1.5);
        if let Some(tone) = instance.mask.as_mut().and_then(|m| m.tone.as_mut()) {
            tone.edge_aware = false;
            tone.invert = true;
            tone.enabled = false;
            tone.f_hi = 3.0;
        }
        // The click reads the picture the way the range it is about to set reads it.
        assert!(!tone_to_limit(&instance).edge_aware);
        limit_to_tone(&mut instance, -2.0);
        let tone = instance.mask.as_ref().unwrap().tone.expect("still there");
        assert_eq!((tone.lo, tone.hi, tone.f_hi), (-2.5, -1.5, 1.0));
        assert!(tone.enabled && tone.invert && !tone.edge_aware);
    }

    #[test]
    fn turning_a_tone_on_switches_a_switched_off_mask_on_but_off_does_not_touch_it() {
        let mut instance = shaped();
        if let Some(mask) = &mut instance.mask {
            mask.enabled = false;
        }
        flip_tone(&mut instance);
        assert!(instance.mask.as_ref().unwrap().enabled, "asking for a tone is asking to see it");
        // And turning it off again leaves the mask as it is: the shape may be doing the work.
        flip_tone(&mut instance);
        assert!(instance.mask.as_ref().unwrap().enabled);
    }
}
