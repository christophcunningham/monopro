//! The Toning Process pane.
//!
//! See the Toning brief in `docs/` for the model. What matters for the *panel* is the
//! rule that governs it, which is the maintainer's:
//!
//! > No look is unreachable because chemistry would not do it. The chemistry decides
//! > the **defaults**; every number it produces is a control the user can move.
//!
//! # What the first version got wrong, kept because it explains the shape of this one
//!
//! It offered a **bench of buttons and a stack**: any bath could be added to any
//! process, in any order, as many times as you liked. That put gold toning on a carbon
//! print in the menu and let you sulphide a cyanotype twice, and the maintainer reported it as
//! stacking chemistry that does not make sense. He is right, and the fix makes the
//! model better rather than only tidier:
//!
//! - **The process owns its chemistry.** `Process::treatments` is what is offered, so
//!   what you can reach is what is possible.
//! - **The ordering question disappears.** The stack existed so gold could be placed
//!   after sulphide and come out red. The chemistry knows its own sequence, so the user
//!   sets amounts and `Treatment::after` decides what they mean.
//! - **Each process has its own image tone.** Every silver process used to derive its
//!   colour from one hue scaled by particle fineness, so they all came out the same
//!   orange with nothing to adjust unless a treatment was added. They have their own
//!   tones now, and a **Tone** and **Hue** pair to move them.
//!
//! # The placement curve
//!
//! One curve for the process, strength against L\*, replacing the four-control range
//! trapezoid the first design gave every bath. Flat is even; pull it down at the right
//! and the highlights keep the paper while the shadows tone. That is the whole of
//! highlight/shadow control and it is one gesture.

use crate::theme::{self, size};
use crate::widgets;
use eframe::egui;
use egui::{Color32, Rect, Sense, pos2, vec2};
use raw_core::toning::{Process, ToningParams, Treatment};

/// What one frame of the panel asked for. Applied by the caller, which owns undo.
#[derive(Default)]
pub struct Actions {
    pub changed: bool,
    /// A treatment to add, by key.
    pub add: Option<&'static str>,
    /// A treatment to drop, by key.
    pub remove: Option<&'static str>,
}

/// **Any real edit arms the module.**
///
/// The rule itself is [`widgets::arm`], which Grain and Sharpening use too, and this
/// changed with them: it used to fire only on the transition out of default, which
/// meant a module armed once in its life and then never again. See that function for
/// why the narrow rule had to go.
///
/// Toning is compared field by field rather than through `is_default`, for the reason
/// the other two are — "did this frame change anything" is not the same question as "is
/// this different from the factory". `enabled` is normalised out of both sides so the
/// bypass cannot read as an edit and re-arm itself.
pub fn arm(was: &ToningParams, now: &mut ToningParams) {
    let unchanged = ToningParams {
        enabled: now.enabled,
        ..was.clone()
    } == *now;
    widgets::arm(!unchanged, &mut now.enabled);
}

/// Draw the pane body.
pub fn body(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    t: &ToningParams,
    drag: &mut Option<usize>,
    hist: &[f32],
) -> (ToningParams, Actions) {
    let mut next = t.clone();
    let mut act = Actions::default();

    let clicks = widgets::Module::new("TONING PROCESS")
        .modified(!next.is_default())
        .switch(next.enabled)
        .show(ui, |ui| process_section(ui, &mut next, &mut act));
    if clicks.bypass {
        next.enabled = !next.enabled;
        act.changed = true;
    }
    if clicks.reset {
        next = ToningParams {
            enabled: next.enabled,
            ..ToningParams::default()
        };
        act.changed = true;
    }

    // Chemistry and Placement are independently previewable parts of Toning Process.
    // Their switches preserve the authored controls, just like a curve instance or a
    // Dodge & Burn layer, so a comparison never requires destructive reset.
    let clicks = widgets::Module::new("CHEMISTRY")
        .modified(!next.applied.is_empty())
        .switch(next.chemistry_enabled)
        .show(ui, |ui| {
            if next.process.treatments().is_empty() {
                // A carbon print's colour was decided when the tissue was mixed, and
                // there is no bath to add. The module remains visible and switchable so
                // the Toning stack keeps the same controls from process to process.
                ui.label(
                    egui::RichText::new(
                        "A carbon print's color is in the tissue.\nThere is nothing to tone.",
                    )
                    .size(size::BODY)
                    .color(theme::DIM),
                );
            } else {
                chemistry_section(ui, icons, &mut next, &mut act);
            }
        });
    if clicks.bypass {
        next.chemistry_enabled = !next.chemistry_enabled;
        act.changed = true;
    }
    if clicks.reset {
        next.applied.clear();
        act.changed = true;
    }

    let clicks = widgets::Module::new("PLACEMENT")
        .modified(next.placement != raw_core::toning::flat_placement())
        .switch(next.placement_enabled)
        .show(ui, |ui| {
            if widgets::placement_editor(ui, &mut next.placement, drag, hist) {
                act.changed = true;
            }
        });
    if clicks.bypass {
        next.placement_enabled = !next.placement_enabled;
        act.changed = true;
    }
    if clicks.reset {
        next.placement = raw_core::toning::flat_placement();
        act.changed = true;
    }

    (next, act)
}

fn process_section(ui: &mut egui::Ui, t: &mut ToningParams, act: &mut Actions) {
    egui::ComboBox::from_id_salt("toning-process")
        .selected_text(t.process.label())
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for p in Process::ALL {
                if ui.selectable_label(t.process == p, p.label()).clicked() && t.process != p {
                    t.process = p;
                    // A treatment the new process does not offer is a value with no
                    // control, which is a setting nobody can find.
                    t.reconcile();
                    act.changed = true;
                }
            }
        });

    ui.add_space(6.0);
    ramp(ui, t);
    ui.add_space(6.0);

    // **The controls the first version did not have.** Every process came out the same
    // warm hue with nothing to move unless a treatment was added, which is not how a
    // paper works: the same process on two papers is two colours.
    act.changed |= widgets::Row::new(&mut t.tone, 1.0, 0.0..=2.0, "Tone")
        .tip(
            "How strongly the process's own image tone shows. 0 is neutral; 1 is the \
             process as it usually comes out.",
        )
        .show(ui);
    act.changed |= widgets::Row::new(&mut t.contrast, 1.0, 0.4..=1.8, "Contrast")
        .tip(
            "The print's grade, against the process's own. Albumen is contrasty and \
             platinum long and soft before you touch it; 1.00 is each process as it \
             usually comes out.",
        )
        .show(ui);
    act.changed |= widgets::Row::new(&mut t.hue, 0.0, -60.0..=60.0, "Hue")
        .decimals(0)
        .suffix("°")
        .tip(
            "A trim off the process's own hue. The process decides what it is; the trim \
             says how yours came out.",
        )
        .show(ui);

    if let Some(label) = t.process.mix_label() {
        act.changed |= widgets::Row::new(&mut t.mix, 0.5, 0.0..=1.0, label)
            .tip(
                "Decided in the sensitizer or the tissue, before there is an image to \
                 treat — which is why it is here and not in Chemistry.",
            )
            .show(ui);
    }
    if t.process == Process::Carbon {
        act.changed |= widgets::Row::new(&mut t.pigment, 0.5, 0.0..=1.0, "Pigment")
            .tip(
                "How much pigment went into the tissue. A carbon print's color and its \
                 density are the same decision.",
            )
            .show(ui);
    }
}

fn chemistry_section(
    ui: &mut egui::Ui,
    icons: &crate::icons::Icons,
    t: &mut ToningParams,
    act: &mut Actions,
) {
    let offered: &'static [Treatment] = t.process.treatments();

    if t.applied.is_empty() {
        ui.label(
            egui::RichText::new("Untoned. The print is as the process made it.")
                .size(size::BODY)
                .color(theme::DIM),
        );
        ui.add_space(4.0);
    }

    // In the process's own order, which is the order a darkroom reaches for them — not
    // the order they were added. The sequence is the chemistry's, not the user's, and
    // that is what took the stack away.
    for spec in offered {
        let Some(i) = t.applied.iter().position(|a| a.key == spec.key) else {
            continue;
        };
        ui.horizontal(|ui| {
            // **The mark goes after the row, not before it.** A D&B layer puts its × at
            // the end and so does a tab; leading with it put the delete under the
            // reading eye and pushed every label one mark to the right of the grid the
            // rest of the panel keeps.
            act.changed |= widgets::Row::new(&mut t.applied[i].amount, 0.6, 0.0..=1.0, spec.label)
                .tip(spec.note)
                .show(ui);
            let (close_rect, close) =
                ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::click());
            let hot = close.hovered();
            if hot {
                ui.painter()
                    .rect_filled(close_rect, 2.0, Color32::from_gray(72));
            }
            crate::icons::paint(
                ui,
                icons,
                "close",
                "×",
                close_rect,
                if hot {
                    Color32::from_gray(245)
                } else {
                    theme::DIM
                },
            );
            if close
                .on_hover_text(theme::tip("remove treatment"))
                .clicked()
            {
                act.remove = Some(spec.key);
            }
        });
    }

    let spare: Vec<&Treatment> = offered
        .iter()
        .filter(|s| !t.applied.iter().any(|a| a.key == s.key))
        .collect();

    if spare.is_empty() {
        return;
    }
    ui.add_space(4.0);
    // **A menu, not a bench of buttons.** Seven chips for a process most prints tone
    // once was the loudest thing in the panel, and none of it was the picture.
    //
    // Removal is the row's own × rather than a second menu beside this one — the same
    // mark, through the same helper, as a Dodge & Burn layer. A verb that already has a
    // gesture in this app does not get a second one here.
    egui::ComboBox::from_id_salt("toning-add")
        .selected_text("+  add treatment")
        .show_ui(ui, |ui| {
            for spec in &spare {
                if ui
                    .selectable_label(false, spec.label)
                    .on_hover_text(theme::tip(spec.note))
                    .clicked()
                {
                    act.add = Some(spec.key);
                }
            }
        });
}

/// The toned ramp: **the tones this process actually produces**, and nothing else.
///
/// # What was here before, and why it went
///
/// It carried the print's distribution as well — a smoothed line over the ramp — and
/// before that a chroma curve underneath it too. the maintainer: *"the histogram line and
/// gradient background looks cheap, doesn't really resemble the tonal distribution
/// shape. Maybe it's not needed and the curve in placement does the same thing but
/// better."*
///
/// He is right on both counts. A distribution drawn over a colour ramp cannot be read
/// as a distribution — the ramp is doing the eye's work at every point along it — and
/// Placement's editor is a graph with an axis, which is where a distribution belongs.
/// So it moved there, and this is left doing the one job it is good at: showing what
/// the process looks like across the scale.
fn ramp(ui: &mut egui::Ui, t: &ToningParams) {
    const H: f32 = 34.0;

    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, H), Sense::hover());
    let painter = ui.painter_at(rect);

    let n = (w.max(2.0) as usize).clamp(2, 512);
    // The process's own colours: never re-greyed for the module ground.
    theme::true_colour(ui, || for (i, toned) in t.bake(n).iter().enumerate() {
        let x0 = rect.left() + i as f32 / n as f32 * rect.width();
        let x1 = rect.left() + (i + 1) as f32 / n as f32 * rect.width();
        let l = raw_core::colour::oklab_lightness(toned.y);
        let (a, b) = toned.ab();
        let lin = raw_core::colour::oklab_to_display_srgb(l, a, b);
        let enc = |v: f32| (v.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
        painter.rect_filled(
            Rect::from_min_max(pos2(x0, rect.top()), pos2(x1, rect.bottom())),
            0.0,
            Color32::from_rgb(enc(lin[0]), enc(lin[1]), enc(lin[2])),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_core::toning::Applied;

    /// Every process's treatments are usable, and none is a row that could do nothing.
    #[test]
    fn every_process_offers_only_what_it_can_run() {
        for p in Process::ALL {
            for t in p.treatments() {
                assert!(
                    !t.label.is_empty(),
                    "{p:?} offers a treatment with no label"
                );
                assert!(!t.note.is_empty(), "{:?}/{} has no note", p, t.key);
                assert!(t.rate > 0.0, "{:?}/{} would never do anything", p, t.key);
            }
        }
    }

    /// **The stacking complaint, as a test.** A treatment cannot reach a process that
    /// does not offer it, and switching process drops what no longer applies rather than
    /// leaving a value with no control.
    #[test]
    fn switching_process_drops_chemistry_it_cannot_run() {
        let mut p = ToningParams {
            enabled: true,
            process: Process::GelatinSilver,
            applied: vec![Applied {
                key: "selenium",
                amount: 0.8,
            }],
            ..Default::default()
        };
        assert_eq!(p.amount("selenium"), 0.8);

        // A cyanotype has no silver for a selenium bath to reach.
        p.process = Process::Cyanotype;
        p.reconcile();
        assert!(
            p.applied.is_empty(),
            "selenium survived onto a cyanotype: {:?}",
            p.applied
        );
    }

    /// Carbon is the process with nothing to tone, and the panel branches on the model's
    /// own list rather than a second one of its own.
    #[test]
    fn only_carbon_offers_no_chemistry() {
        let bare: Vec<Process> = Process::ALL
            .into_iter()
            .filter(|p| p.treatments().is_empty())
            .collect();
        assert_eq!(bare, vec![Process::Carbon]);
    }

    /// **The complaint that started the redesign.** Every silver process used to derive
    /// its colour from one hue scaled by fineness, so they all came out the same orange.
    #[test]
    fn the_silver_processes_are_not_all_the_same_hue() {
        let family = [
            Process::GelatinSilver,
            Process::SaltPrint,
            Process::Albumen,
            Process::CollodionPop,
            Process::Kallitype,
            Process::Vandyke,
        ];
        let mut seen: Vec<(Process, f32)> = Vec::new();
        for p in family {
            let h = p.dense_tone().hue;
            for (other, oh) in &seen {
                let apart = (h - oh).abs().min(360.0 - (h - oh).abs());
                assert!(
                    apart > 5.0,
                    "{p:?} and {other:?} share a hue: {h} against {oh}"
                );
            }
            seen.push((p, h));
        }
    }

    /// And each is adjustable without adding chemistry, which is the other half of the
    /// same complaint.
    #[test]
    fn a_process_can_be_moved_without_a_treatment() {
        let mid = raw_core::toning::y_from_lstar(0.45);
        let plain = ToningParams {
            enabled: true,
            process: Process::Albumen,
            ..Default::default()
        };
        let warmer = ToningParams {
            hue: 45.0,
            ..plain.clone()
        };
        let stronger = ToningParams {
            tone: 1.8,
            ..plain.clone()
        };

        assert!(plain.applied.is_empty(), "no chemistry involved");
        let moved = (warmer.evaluate(mid).hue - plain.evaluate(mid).hue).abs();
        assert!(moved > 20.0, "hue barely moved: {moved}");
        assert!(stronger.evaluate(mid).chroma > plain.evaluate(mid).chroma * 1.4);
    }

    /// The placement curve replaces the per-bath range, and it holds the highlights back
    /// when it is pulled down at the right.
    #[test]
    fn placement_shapes_where_the_toning_lands() {
        let base = ToningParams {
            enabled: true,
            applied: vec![Applied {
                key: "sulphide",
                amount: 0.9,
            }],
            ..Default::default()
        };
        let shadows = ToningParams {
            placement: raw_core::Curve::from_points(&[[0.0, 1.0], [0.5, 0.0], [1.0, 0.0]]).unwrap(),
            ..base.clone()
        };
        let hi = raw_core::toning::y_from_lstar(0.8);
        assert!(
            shadows.evaluate(hi).chroma < base.evaluate(hi).chroma,
            "placement did not hold the highlights back"
        );
    }

    /// **Gold over a sulphided print is red, and gold over plain silver is blue-black.**
    /// The fact that justified an ordered stack, surviving the stack being removed —
    /// the chemistry knows its own sequence, so amounts are all the user sets.
    #[test]
    fn gold_still_knows_what_came_before_it() {
        let shadow = raw_core::toning::y_from_lstar(0.2);
        let gold_only = ToningParams {
            enabled: true,
            applied: vec![Applied {
                key: "gold-gp1",
                amount: 1.0,
            }],
            ..Default::default()
        };
        let after_sepia = ToningParams {
            applied: vec![
                Applied {
                    key: "sepia",
                    amount: 1.0,
                },
                Applied {
                    key: "gold-gp1",
                    amount: 1.0,
                },
            ],
            ..gold_only.clone()
        };

        let a = gold_only.evaluate(shadow).hue;
        let b = after_sepia.evaluate(shadow).hue;
        let apart = (a - b).abs().min(360.0 - (a - b).abs());
        assert!(apart > 45.0, "order stopped mattering: {a} against {b}");
    }
}
