//! Reusable controls: the develop module frame, reset-on-double-click sliders, and
//! the curve editor.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};
use raw_core::{Curve, ZoneMask};

use crate::theme::{self, DIM, RUBY};

/// Numeric fields must clamp keyboard edits too: egui 0.35 only clamps the old
/// value before applying an arrow step, allowing a one-frame out-of-range value.
pub fn bounded_number<Num: egui::emath::Numeric>(
    value: &mut Num,
    range: std::ops::RangeInclusive<Num>,
    speed: f64,
) -> egui::DragValue<'_> {
    let lo = range.start().to_f64();
    let hi = range.end().to_f64();
    let field = egui::DragValue::from_get_set(move |new| {
        if let Some(new) = new {
            *value = Num::from_f64(new.clamp(lo, hi));
        }
        value.to_f64()
    })
    .range(lo..=hi)
    .speed(speed);
    if Num::INTEGRAL {
        field.fixed_decimals(0)
    } else {
        field
    }
}

fn slider_number(
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    decimals: usize,
) -> egui::DragValue<'_> {
    let unit = 10.0_f64.powf(-(decimals as f64));
    // Keep the established drag sensitivity, but use whole displayed units so
    // every arrow press survives rounding, including zero-decimal controls.
    let nominal = f64::from(*range.end() - *range.start()) / 300.0;
    let speed = (nominal / unit).round().max(1.0) * unit;
    bounded_number(value, range, speed).fixed_decimals(decimals)
}

/// One develop module: a hairline container, a header of dot + name + reset, and a
/// body that collapses away.
///
/// # Every container is the same width
///
/// A `Frame` sizes itself to its content, so left to itself LUMINANCE came out
/// narrower than EXPOSURE and both came out narrower than CONTRAST MASK — a ragged
/// right edge down a panel whose whole job is to look like a set of fields. The
/// content width is therefore forced to the panel's, so the boxes stack as one
/// column. It also fixes a second bug with the same cause: `ui.available_width()`
/// inside a `Frame` does not know about the frame's own right margin, so the
/// histogram was drawing itself wider than the box containing it.
///
/// # The dot encodes two independent facts on two axes
///
/// ```text
///            running          bypassed
/// default    · small grey     ○ grey ring
/// modified   ● large ruby     ○ ruby ring
/// ```
///
/// **Colour says modified, fill says running.** They are genuinely independent —
/// a module can be switched off without having been touched, and a module you
/// have spent five minutes on can be switched off to compare — so encoding them
/// on one axis would have to lose one of them. Reading either fact takes no
/// legend once you know which axis is which, and both are legible at 8px.
///
/// **"Modified" means the module would render differently than it does at defaults**,
/// which is not quite the same as "a value has been moved" — see
/// `raw_core::params::is_modified`. For a module that is off by default, switching it on
/// *is* the edit: Contrast Mask at its default values changes every pixel, and until
/// this was widened its dot stayed grey and the panel had no way of saying so. For
/// everything else the two readings coincide.
///
/// A module without a bypass (`Module::new` alone) still gets a dot, and it is an
/// indicator only. Decode, Luminance and Tonal Transform are the chain rather than
/// effects; there is no honest "off" for a display transform, so those dots do not
/// take a click.
///
/// # Why the dot is on the left and the reset is on the right
///
/// The dots form a single vertical rail down the panel, which is what makes "has
/// anything been touched" answerable at a glance rather than by scanning. It is
/// also the flush-left instinct of the letterheads this UI is drawn from.
///
/// The reset is justified right — but to the **container's** inner edge, not the
/// panel's, which is the distinction that matters. Controls justified to the panel
/// went under the scrollbar; the container is inset from it, and the scrollbar now
/// allocates its own width rather than floating over the content.
///
/// **The reset appears only when the module is modified**, because on an untouched
/// module it would do nothing, and a control that looks live and does nothing is
/// worse than no control.
pub struct Module<'a> {
    name: &'a str,
    modified: bool,
    resettable: Option<bool>,
    open_on_start: bool,
}

/// A section that is not a pipeline module: no dot, no reset, and nothing to
/// report back. The histogram, which is a readout, and Export, which is an action.
///
/// A separate type so that `show` can return `()` here and something `#[must_use]`
/// on the two types that do report a click. One `show` returning a must-use value
/// was tried and warns on every section that cannot possibly produce one, which
/// trains you to ignore the warning that matters.
pub struct Plain<'a> {
    name: &'a str,
    open_on_start: bool,
    open_this_frame: bool,
}

/// A module whose dot is a switch, reached only through [`Module::switch`].
#[must_use = "the clicks are reported by `show`; drop them and the header is dead"]
pub struct Switchable<'a> {
    module: Module<'a>,
    /// **By value, not `&mut`.** A `&mut` to one field of a params group would be
    /// held until `show` returns, so the body closure — which needs the whole group
    /// — could not borrow it. Reporting the click instead costs one line per call
    /// site and keeps the borrow window to nothing.
    enabled: bool,
}

/// **Any real edit arms a switchable module.** Pass whether this frame changed the
/// module's parameters at all; the switch goes on if it did.
///
/// the maintainer: moving a Grain or Sharpen slider left the module off, so the first thing you
/// did to it did nothing and the dot had to be found and clicked. A control that
/// silently does nothing is the single thing most likely to make somebody decide a
/// module is broken — and for these two the module is invisible until the loupe opens,
/// so there was not even a picture to contradict it.
///
/// # This used to fire only on the transition out of default, and that was too narrow
///
/// The first rule armed on `was_default && !now_default`, so that a module you had
/// edited and then deliberately bypassed stayed off while you kept nudging it. The
/// intent was to keep the dot meaningful. What it actually produced is a module that
/// arms **exactly once in its life**: after the first edit the params are no longer
/// default, so `was_default` is false from then on and no slider ever arms it again.
/// Switch it off once and the dot becomes the only way back, permanently — and because
/// the sidecar stores the edited values, that state survives reopening the file. the maintainer
/// hit it constantly, which is the answer to whether the narrow rule was worth it.
///
/// **What this costs, stated plainly:** you can no longer bypass a module and keep
/// working its sliders with it off. That gesture is gone, and it is the price of the
/// first edit always doing something. The ways to compare survive — `p` previews the
/// original, and the dot is still an explicit off switch that holds until you touch a
/// control.
///
/// All three switchable modules use this, which is the point: Grain, Sharpening and
/// Toning arm the same way rather than each having its own idea. See
/// [`crate::toning::arm`].
pub fn arm(edited: bool, enabled: &mut bool) {
    if edited {
        *enabled = true;
    }
}

/// What a module header reported this frame.
#[must_use = "the clicks are reported here; drop them and the header is dead"]
pub struct Clicked {
    /// The bypass dot was clicked. Always false for a module without a switch.
    pub bypass: bool,
    /// The reset was clicked.
    pub reset: bool,
}

impl<'a> Module<'a> {
    /// A module whose dot is an indicator. `name` is used as-is — pass it upper
    /// case; see `theme::header_text`.
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            modified: false,
            resettable: None,
            open_on_start: true,
        }
    }

    /// Whether the module would render differently than it does at defaults. The
    /// `is_modified` methods in `raw_core::params` answer exactly this; see
    /// `params::is_modified` for why that is not the same as "a value has moved".
    /// Also decides whether a reset is offered.
    pub fn modified(mut self, yes: bool) -> Self {
        self.modified = yes;
        self
    }

    /// Override whether Reset is offered. Most modules reset as a whole and use
    /// `modified`; Curve resets only its selected instance, so the two states differ.
    pub fn resettable(mut self, yes: bool) -> Self {
        self.resettable = Some(yes);
        self
    }

    /// The module's state at application start. Collapse remains live for the rest
    /// of the session, but is deliberately not restored across launches.
    pub fn open_on_start(mut self, yes: bool) -> Self {
        self.open_on_start = yes;
        self
    }

    /// Make the dot a switch, showing `enabled` as its current state.
    pub fn switch(self, enabled: bool) -> Switchable<'a> {
        Switchable {
            module: self,
            enabled,
        }
    }

    pub fn show(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) -> Clicked {
        draw(
            self.name,
            Some(self.modified),
            None,
            self.resettable.unwrap_or(self.modified),
            self.open_on_start,
            false,
            ui,
            body,
        )
    }
}

impl<'a> Plain<'a> {
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            open_on_start: true,
            open_this_frame: false,
        }
    }

    pub fn open_on_start(mut self, yes: bool) -> Self {
        self.open_on_start = yes;
        self
    }

    /// Reveal this section now, without pinning it open on later frames.
    pub fn open_when(mut self, yes: bool) -> Self {
        self.open_this_frame = yes;
        self
    }

    pub fn show(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
        let _ = draw(
            self.name,
            None,
            None,
            false,
            self.open_on_start,
            self.open_this_frame,
            ui,
            body,
        );
    }
}

impl<'a> Switchable<'a> {
    pub fn show(self, ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) -> Clicked {
        draw(
            self.module.name,
            Some(self.module.modified),
            Some(self.enabled),
            self.module.resettable.unwrap_or(self.module.modified),
            self.module.open_on_start,
            false,
            ui,
            body,
        )
    }
}

/// Horizontal inner margin. Named because the content width is computed from it.
const PAD_X: i8 = 9;

/// The container's stroke, in points.
///
/// **It has to be in the width arithmetic.** `Frame` advances the cursor by its
/// widget rect, which is the content plus the inner margin plus this on each side —
/// so sizing the content to `available - 2*PAD_X` made the box come out two points
/// wider than the space it was given. In a resizable panel that is not a cosmetic
/// error but a runaway: the panel measures its content, grows by two, hands back a
/// wider `available_width` next frame, and the develop panel eats the window over a
/// few seconds. `the_module_width_does_not_run_away` is the guard.
const STROKE: f32 = 1.0;

/// All three `show`s. `modified` is `None` for a plain section — no dot and no
/// reset — and `enabled` is `None` for a module without a switch.
#[expect(
    clippy::too_many_arguments,
    reason = "the one drawing routine behind all three module `show`s; each flag is one of their differences"
)]
fn draw(
    name: &str,
    modified: Option<bool>,
    enabled: Option<bool>,
    resettable: bool,
    open_on_start: bool,
    open_this_frame: bool,
    ui: &mut egui::Ui,
    body: impl FnOnce(&mut egui::Ui),
) -> Clicked {
    let has_dot = modified.is_some();
    let modified = modified.unwrap_or(false);

    // Collapse is **session view state, not `Params`**: it is not undoable, not in
    // the sidecar, and not copied by a duplicate. Temp memory keeps it live while
    // the app is running without allowing yesterday's working arrangement to
    // override the requested startup stack.
    let id = ui.make_persistent_id(("module", name));
    let mut open = ui
        .data_mut(|d| d.get_temp::<bool>(id))
        .unwrap_or(open_on_start);
    if open_this_frame {
        open = true;
    }

    // Every box the same width; see the note above. Taken before the frame, where
    // `available_width` still knows about the scrollbar — and netting off both the
    // margin and the stroke, because the box is content + margin + stroke on each
    // side and anything left out of this sum is added to the panel every frame.
    let content_w = (ui.available_width() - 2.0 * (PAD_X as f32 + STROKE)).max(0.0);

    let mut out = Clicked {
        bypass: false,
        reset: false,
    };
    // Filled, not transparent: the panel ground behind it can follow the viewer
    // background, and the modules keep their own grey whatever it is set to. Drawn
    // at `CHROME` and then re-greyed to the chosen module ground — see
    // `theme::regrey` — so every colour inside is written once, for one ground.
    crate::theme::module_ground_ui(ui, |ui| {
        egui::Frame::new()
            .fill(crate::theme::CHROME)
            .stroke(Stroke::new(STROKE, Color32::from_gray(52)))
            .corner_radius(2.0)
            .inner_margin(egui::Margin::symmetric(PAD_X, 7))
            .show(ui, |ui| {
                // A touch more air between control rows. Kept local to Develop module
                // frames so Lightbox grids, Settings rows and footer chrome do not grow
                // with it.
                ui.spacing_mut().item_spacing.y += 1.0;
                ui.set_width(content_w);
                ui.horizontal(|ui| {
                    match enabled {
                        // Indented by exactly the dot's own box, so the names line up
                        // down the panel whether or not a section has one.
                        _ if !has_dot => ui.add_space(13.0),
                        Some(on) => {
                            out.bypass = dot(ui, modified, on, true)
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(theme::tip(if on {
                                    "click to bypass"
                                } else {
                                    "bypassed — click to switch back on"
                                }))
                                .clicked();
                        }
                        // Not switchable, so it does not offer to be clicked and does
                        // not light up under the pointer. A control that looks live and
                        // does nothing is worse than no control.
                        None => {
                            dot(ui, modified, true, false);
                        }
                    }
                    ui.add_space(2.0);
                    let title = theme::module_header_label(ui, name)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(theme::tip(if open {
                            "click to collapse"
                        } else {
                            "click to expand"
                        }));
                    if title.clicked() {
                        open = !open;
                    }
                    if resettable {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            out.reset =
                                theme::reset_button(ui, "reset", "back to default").clicked();
                        });
                    }
                });
                if open {
                    ui.add_space(5.0);
                    body(ui);
                }
            });
    });

    ui.data_mut(|d| d.insert_temp(id, open));
    ui.add_space(7.0);
    out
}

/// The state dot.
///
/// **Public since the UI pass**, so the Dodge & Burn layer rows use the module's own
/// dot rather than a lookalike. the maintainer asked for exactly that — "the same red circle
/// preview/off/on behaviour as the module buttons" — and the only way to be sure of
/// it is to call the same function.
pub fn dot(ui: &mut egui::Ui, modified: bool, running: bool, switchable: bool) -> egui::Response {
    // A box big enough to click. The mark inside it is small; a 6px circle is a
    // miserable hit target and this is a control, not a decoration.
    let sense = if switchable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(13.0, 13.0), sense);
    let hot = switchable && resp.hovered();
    let colour = match (modified, hot) {
        (true, false) => RUBY,
        (true, true) => RUBY.gamma_multiply(1.4),
        (false, false) => Color32::from_gray(78),
        (false, true) => Color32::from_gray(130),
    };
    let painter = ui.painter_at(rect);
    if running {
        // Small when nothing has been touched, large when something has: the size
        // difference is what makes a modified module findable without reading.
        painter.circle_filled(rect.center(), if modified { 4.0 } else { 2.5 }, colour);
    } else {
        // Hollow: there is something here and it is not running.
        painter.circle_stroke(rect.center(), 4.0, Stroke::new(1.4, colour));
    }
    resp
}

/// One control row: **label, value, slider**, in that order left to right.
///
/// the maintainer's layout, from the mockup, and it replaces the arrangement egui's own
/// `Slider` imposes — which puts the track and the value on the left and the label
/// on the right, so a column of them is read right-to-left and the labels do not
/// align with anything.
///
/// # Why this is painted rather than configured
///
/// `egui::Slider` cannot be reordered: `.text()` is documented as trailing and the
/// value box is welded to the track. So the row is laid out here — three fixed
/// columns, which is what makes a stack of them align — and the track is drawn as
/// the mockup draws it: a hairline rule with a small open handle, rather than egui's
/// filled trough.
///
/// # `Slider` is this
///
/// [`Slider`] is a four-line forward to `Row`, kept only because its argument order is
/// what eighteen call sites already read. There is one row type in the app.
pub struct Row<'a> {
    value: &'a mut f32,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
    label: &'a str,
    label_size: f32,
    decimals: usize,
    suffix: &'a str,
    tip: &'a str,
}

impl<'a> Row<'a> {
    /// Column widths, in points. **Both are set by the widest thing that has to fit in
    /// them, measured rather than chosen** — see `the_columns_fit_what_goes_in_them`,
    /// which is where the promise is checked against the font.
    ///
    /// Fixed rather than proportional, and *global* rather than per-module: a column
    /// sized to its own module's longest label would put a different grid on every box
    /// in the panel, and the panel is read as one column top to bottom.
    ///
    /// # Why they are wider than the mockup's
    ///
    /// They were 74 and 52, taken from the mockup's tab stops. 74pt is ten characters
    /// of JetBrains Mono at `size::BODY`, and eleven of the app's labels were longer
    /// than that. A column its content overflows is not a column: `add_sized` lays the
    /// overflow out *centred* and then advances the cursor by what it actually drew, so
    /// a long label pushed its own number and its own track to the right and that row
    /// alone left the grid. `Crystal size` and `Black correction` are what the maintainer was
    /// looking at.
    ///
    /// The value column is the widest readout the ranges can produce — `-40.0 px` and
    /// `-6.00 EV`, eight characters — plus `button_padding` at each end. That one is
    /// not negotiable; the label column is, because label *text* is.
    ///
    /// The fixed columns must leave room for the full track at the default panel
    /// width. Wider panels may leave spare space; the track keeps its chosen cap.
    pub const LABEL_W: f32 = 110.0;
    const VALUE_W: f32 = 66.0;
    const GAP: f32 = 4.0;
    /// How long the track is allowed to get. **the maintainer's number.**
    ///
    /// **Capped, not proportional.** It used to take every remaining point, which put
    /// the right-hand end hard against the panel edge — so the handle at the top of a
    /// range was cut in half by the clip rect, and on a wide panel a slider became a
    /// rule several inches long whose handle moved imperceptibly per pixel of drag.
    /// the maintainer reported both as one symptom.
    ///
    /// It was 140, which the fixed columns could not afford. 100 is what the row is
    /// budgeted around, and a track this length is a coarse control on purpose — the
    /// `DragValue` beside it takes a typed value and a fine drag, so the slider does not
    /// have to be the precise one.
    const TRACK_W: f32 = 100.0;

    pub fn new(
        value: &'a mut f32,
        default: f32,
        range: std::ops::RangeInclusive<f32>,
        label: &'a str,
    ) -> Self {
        Self {
            value,
            default,
            range,
            label,
            label_size: theme::size::SLIDER,
            decimals: 2,
            suffix: "",
            tip: "",
        }
    }

    /// Hover text for the whole row.
    ///
    /// **A tooltip rather than a caption under the control**, which is what two of
    /// these were. A paragraph of prose below a slider is read once and then occupies
    /// the panel for ever; it also breaks the column rhythm the row grid exists to
    /// keep, which is what the maintainer saw first. The words are worth having and they are
    /// not worth the height.
    pub fn tip(mut self, s: &'a str) -> Self {
        self.tip = s;
        self
    }

    /// Locally reduce or enlarge this row's label without changing the shared
    /// slider grid. Used by dense option stacks such as Dodge & Burn.
    pub fn label_size(mut self, points: f32) -> Self {
        self.label_size = points;
        self
    }

    pub fn decimals(mut self, n: usize) -> Self {
        self.decimals = n;
        self
    }

    pub fn suffix(mut self, s: &'a str) -> Self {
        self.suffix = s;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> bool {
        let before = *self.value;
        let (lo, hi) = (*self.range.start(), *self.range.end());
        let h = ui.spacing().interact_size.y.min(18.0);
        let tip = self.tip;

        let inner = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = Self::GAP;

            // **Label, then value, then track — left to right, in that order, and all
            // three left-justified in a column of their own.** the maintainer's rule and it is
            // the app's now. The label column is a fixed width rather than the text's
            // own so that a stack of rows puts every number in the same place; that
            // alignment is what tells a measured value from a chosen name, and is why
            // the app no longer needs a second typeface to say it.
            label_cell(ui, self.label, h, self.label_size);

            // egui's own `DragValue`, not a hand-drawn readout with a click-to-edit
            // path bolted on. It drags, it types, it clamps, and it is the same widget
            // the Output module already uses — one behaviour for one gesture.
            //
            // `add_sized` is right here and wrong for the label: a `DragValue` is a
            // button, and a justified button fills its cell rather than sitting in the
            // middle of it. What it must not do is *overflow* the cell, which is what
            // `VALUE_W` is measured to prevent — an eight-character readout in a column
            // sized for six moved that row's track and nothing else's.
            // The number and its label are one typographic level. Scope the style
            // to this DragValue so non-slider number fields elsewhere keep their
            // own scale.
            ui.scope(|ui| {
                ui.style_mut().text_styles.insert(
                    egui::TextStyle::Button,
                    egui::FontId::new(theme::size::SLIDER, egui::FontFamily::Proportional),
                );
                ui.style_mut().drag_value_text_style = egui::TextStyle::Button;
                ui.add_sized(
                    [Self::VALUE_W, h],
                    slider_number(self.value, lo..=hi, self.decimals).suffix(self.suffix),
                );
            });

            // Whatever is left, capped. Both columns before it are exact now, so every
            // row in a panel is handed the same remainder — the tracks are the same
            // length and start at the same x, which is the whole of what the fixed
            // columns are for. A row in a container that indents would be handed a
            // smaller one, so the app does not indent them; see `body_unindented` in
            // the zone mask.
            let w = ui.available_width().clamp(24.0, Self::TRACK_W);
            let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
            let t = ((*self.value - lo) / (hi - lo).max(1e-9)).clamp(0.0, 1.0);
            // Half the handle at each end, so its edge never leaves the track.
            let (x0, x1) = (rect.left() + HANDLE_W * 0.5, rect.right() - HANDLE_W * 0.5);
            let y = rect.center().y;

            if (resp.dragged() || resp.is_pointer_button_down_on())
                && let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos())
            {
                let u = ((p.x - x0) / (x1 - x0).max(1e-9)).clamp(0.0, 1.0);
                *self.value = lo + u * (hi - lo);
            }
            if resp.double_clicked() {
                *self.value = self.default;
            }

            let painter = ui.painter_at(rect.expand(2.0));
            painter.line_segment(
                [pos2(x0, y), pos2(x1, y)],
                Stroke::new(1.0, Color32::from_gray(72)),
            );

            // **A rectangle at 0.48, ruby only while it is being pulled.** the maintainer's
            // shape and the maintainer's rule: the handle is a mark of position, and a control
            // that turned red merely because the pointer crossed it would make a panel
            // flicker as the cursor travelled over it. Ruby is reserved for "you are
            // doing this now".
            //
            // The stroke does not thicken on drag either — a handle that grew would
            // move its own edges under the finger holding it.
            let handle = egui::Rect::from_center_size(
                pos2(x0 + t * (x1 - x0), y),
                vec2(HANDLE_W, HANDLE_W / HANDLE_ASPECT),
            );
            let ink = if resp.dragged() {
                RUBY
            } else {
                Color32::from_gray(150)
            };
            painter.rect_filled(handle, 1.0, theme::CHROME);
            painter.rect_stroke(handle, 1.0, Stroke::new(1.0, ink), egui::StrokeKind::Inside);
            resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
                .on_hover_text(theme::tip(
                    "drag, or type in the value — double-click to reset",
                ));
        });

        // The whole row is the hit target, so the tip is available from the label as
        // well as from the control — the label is what you are reading when you want it.
        if !tip.is_empty() {
            inner.response.on_hover_text(crate::theme::tip(tip));
        }

        *self.value != before
    }
}

/// The row's label column: the text left-justified in exactly [`Row::LABEL_W`] points,
/// whatever the text is.
///
/// # `with_main_align(Align::Min)` is the whole of it, and it is not obvious
///
/// **`Layout::left_to_right` hardcodes `main_align: Align::Center`** — its own comment
/// says "looks best to e.g. center text within a button", which is true and is not what
/// a column of labels wants. `main_justify` stretches the *cell* to the full column, and
/// then `main_align` decides where the widget sits inside it, so a justified left-to-
/// right layout gives you an exactly-sized cell with the text **centred in it**.
///
/// That is worth spelling out because two plausible fixes are wrong. It is *not*
/// `Label::halign`, and it is not `Layout::horizontal_placement` — that already returns
/// `Align::LEFT` here, and the galley's own halign already reads `Min`. The text is
/// centred one level up, in where the galley's rect was allocated, and neither of those
/// two reaches it. Measured: with `main_align` left at its default the galley sits at
/// exactly `(LABEL_W − text) / 2` from the column's left edge, which is why
/// `Gamma` stood a full 37pt clear of it.
///
/// It is also why `add_sized` cannot draw this cell — `centered_and_justified` has the
/// same `main_align: Center`, so every label in the app was centred in its column. That
/// is what the maintainer reported in three panels at once, and it survived the first fix because
/// the cell was the right width and only the text inside it was wrong.
///
/// # And `truncate`, for the other half
///
/// What `add_sized` does when the text does *not* fit is overflow and then advance the
/// cursor by what it actually drew, so a long label pushes its own number and its own
/// track to the right and that row alone leaves the grid. `truncate` makes that
/// impossible — and it truncates rather than wraps because a row that gained a second
/// line would grow taller than its neighbours, breaking the grid the other way.
///
/// # It lays the galley out itself, so that the test can see where the glyphs went
///
/// `ui.add(label).rect` is **not** the text — in a justified layout it is the allocated
/// frame, the full column, whatever the text does inside it. So it reads 0..110 for a
/// centred label and 0..110 for a left-justified one, and a test built on it passes
/// either way. That is not hypothetical: it is what the first attempt at this asserted,
/// and it is why a build with every label centred went out with a green suite.
///
/// `Label::layout_in_ui` returns the position the galley is actually painted at, which
/// is the only number that answers the question. The cost of taking it is painting the
/// galley here rather than letting `Label::ui` do it — worth it, and small, because
/// these labels are already inert (`selectable(false)`, no sense, no interaction).
///
/// Returns the cell and the x the glyphs start at.
fn label_cell(ui: &mut egui::Ui, text: &str, h: f32, font_size: f32) -> (egui::Rect, f32) {
    let laid_out = ui.allocate_ui_with_layout(
        vec2(Row::LABEL_W, h),
        egui::Layout::left_to_right(egui::Align::Center)
            .with_main_align(egui::Align::Min)
            .with_main_justify(true),
        |ui| {
            let (pos, galley, _) = egui::Label::new(theme::label(text).size(font_size))
                .selectable(false)
                .truncate()
                .layout_in_ui(ui);
            let x = pos.x;
            // The colour is on the `RichText`, so this fallback is never reached; it is
            // what a galley with no colour of its own would be painted in.
            ui.painter().galley(pos, galley, DIM);
            x
        },
    );
    (laid_out.response.rect, laid_out.inner)
}

/// The slider handle's width, and its width-to-height ratio.
///
/// **0.48 is the maintainer's figure**: a rectangle noticeably taller than it is wide, which
/// reads as a position on a rule where a square reads as a button sitting on one.
/// Stated as an aspect rather than as two sizes so the shape survives a change to
/// either — the thing being specified is the proportion.
const HANDLE_W: f32 = 7.0;
const HANDLE_ASPECT: f32 = 0.48;

/// A labelled switch: **the box on the left, the name after it.**
///
/// It was the other way round — label left, box hard against the panel's right edge —
/// on the argument that neither moves when the other changes length. That argument is
/// fine and it was answering the wrong question. `docs/decisions.md` already records
/// "checkbox: box left, label right" as the app's rule, and the stock `egui::checkbox`
/// this sits beside has drawn it that way since the design pass set it in `Visuals`, so
/// the one hand-painted switch in the app was the only thing breaking its own written
/// rule. A box a panel's width away from the words it governs is also simply hard to
/// read. the maintainer asked for it on the left.
///
/// Returns the response of the whole row, so the entire strip is the hit target
/// rather than a 12pt square — the same reason the layer rows widened their own.
pub fn check(ui: &mut egui::Ui, label: &str, on: &mut bool) -> egui::Response {
    let h = ui.spacing().interact_size.y.min(18.0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let painter = ui.painter_at(rect);

    let side = 12.0;
    let box_rect = egui::Rect::from_center_size(
        pos2(rect.left() + side * 0.5, rect.center().y),
        vec2(side, side),
    );
    // **`DIM` whether it is on or off**, which is a change: it used to brighten to
    // `BRIGHT` when ticked. the maintainer asked for eight checkbox and control labels across
    // three modules to be body grey rather than white, and this is the ninth — it would
    // otherwise be the only switch in the app that turns its own name white, and it does
    // so in the one state his screenshots did not happen to catch it in.
    //
    // Nothing is lost by it. The ruby tick is what says "on", and a label that changes
    // colour as well is the state told twice — at the cost of the name of the control
    // being the loudest thing in the panel exactly when you have finished deciding
    // about it.
    painter.text(
        pos2(box_rect.right() + Row::GAP, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::new(theme::size::BODY, egui::FontFamily::Proportional),
        DIM,
    );
    let edge = if resp.hovered() {
        Color32::from_gray(150)
    } else {
        Color32::from_gray(90)
    };
    painter.rect_stroke(
        box_rect,
        2.0,
        Stroke::new(1.0, edge),
        egui::StrokeKind::Inside,
    );
    if *on {
        // A tick rather than a fill: a filled box at this size is a dot, and a dot is
        // what the layer rows already use to mean something else entirely.
        let c = box_rect.center();
        let s = side * 0.28;
        painter.add(egui::Shape::line(
            vec![
                pos2(c.x - s, c.y),
                pos2(c.x - s * 0.2, c.y + s * 0.75),
                pos2(c.x + s, c.y - s * 0.7),
            ],
            Stroke::new(1.6, RUBY),
        ));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A slider that resets to its default on double-click.
///
/// **Kept as one line of delegation.** It was `egui::Slider` — track, then value, then
/// label, reading right to left — and the app had two control rows for one job.
/// the maintainer settled the design: one row everywhere, `Label · DragValue · Slider`. Rather
/// than convert eighteen call sites by hand and leave the two shapes coexisting for a
/// commit, this forwards to [`Row`] and the call sites did not change at all.
///
/// It stays because the argument order differs — `Slider::new(v, default, range,
/// text)` is what the Develop panel already reads — and collapsing that too would have
/// made one change into two.
/// The app's slider **track and handle alone**, with no label column and no value.
///
/// The footer has room for a control and not for a row: a 78 pt label column and a
/// `DragValue` beside a size slider would be most of the bar spent saying what the
/// grid in front of you already shows. This is the same 1 pt track and the same
/// rectangular handle [`Row`] draws, at the same greys and with the same rule about
/// ruby — so it *is* the app's slider rather than something that resembles it.
///
/// `steps` snaps to whole positions when it is `Some`; a size ladder has rungs and
/// nothing between them.
pub fn settings_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
) -> egui::Response {
    // **egui's slider for the gesture and the rail, Develop's handle on top of it.**
    // The rail is what the Settings window has always shown and stays exactly as it
    // was — the `inactive` fill and corner, painted here because the handle and the
    // rail read the same visuals slot and the only way to hide one is to hide both.
    // The handle is the develop slider's: a narrow outlined rectangle, ruby only while
    // it is being pulled.
    let rail_fill = ui.visuals().widgets.inactive.bg_fill;
    let rail_corner = ui.visuals().widgets.inactive.corner_radius;
    // egui sizes its handle from the slider's height (`handle_radius` is height / 2.5)
    // and insets the travel by the handle's half-width. Asking for an aspect that
    // makes that half-width ours keeps the drawn handle on egui's own position.
    let thickness = ui
        .text_style_height(&egui::TextStyle::Body)
        .max(ui.spacing().interact_size.y);
    let aspect_ratio = (HANDLE_W * 0.5) / (thickness / 2.5);
    let mut response = ui
        .scope(|ui| {
            let w = &mut ui.visuals_mut().widgets;
            for v in [&mut w.inactive, &mut w.hovered, &mut w.active] {
                v.bg_fill = Color32::TRANSPARENT;
                v.fg_stroke = Stroke::NONE;
                v.expansion = 0.0;
            }
            ui.add(
                egui::Slider::new(value, range.clone())
                    .integer()
                    .show_value(false)
                    .handle_shape(egui::style::HandleShape::Rect { aspect_ratio }),
            )
        })
        .inner;
    {
        let rect = response.rect;
        let rail_h = ui.spacing().slider_rail_height;
        let painter = ui.painter();
        painter.rect_filled(
            Rect::from_center_size(rect.center(), vec2(rect.width(), rail_h)),
            rail_corner,
            rail_fill,
        );
        let (lo, hi) = (*range.start(), *range.end());
        let t = ((*value - lo) / (hi - lo).max(1e-9)).clamp(0.0, 1.0);
        let (x0, x1) = (rect.left() + HANDLE_W * 0.5, rect.right() - HANDLE_W * 0.5);
        let handle = egui::Rect::from_center_size(
            pos2(x0 + t * (x1 - x0), rect.center().y),
            vec2(HANDLE_W, HANDLE_W / HANDLE_ASPECT),
        );
        let ink = if response.dragged() {
            RUBY
        } else {
            Color32::from_gray(150)
        };
        painter.rect_filled(handle, 1.0, theme::CHROME);
        painter.rect_stroke(handle, 1.0, Stroke::new(1.0, ink), egui::StrokeKind::Inside);
    }
    // The number box egui would have drawn after the rail, in the same place.
    let value_box = ui.add(
        egui::DragValue::new(value)
            .range(range.clone())
            .fixed_decimals(0)
            .speed(((*range.end() - *range.start()) / response.rect.width().max(1.0)).max(0.01)),
    );
    response = response.union(value_box);
    // Native slider tracks sense drags, not clicks. Inspect the pointer gesture
    // over their response so double-click works on the track as well as the value.
    if response.enabled()
        && response.contains_pointer()
        && ui.input(|i| {
            i.pointer
                .button_double_clicked(egui::PointerButton::Primary)
        })
    {
        *value = default;
        response.mark_changed();
    }
    response.on_hover_text("Double-click to reset to default")
}

pub fn bare_slider(
    ui: &mut egui::Ui,
    value: &mut f32,
    default: f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
    steps: Option<usize>,
) -> egui::Response {
    let before = *value;
    let (lo, hi) = (*range.start(), *range.end());
    let h = ui.spacing().interact_size.y.min(18.0);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(width, h), Sense::click_and_drag());
    let (x0, x1) = (rect.left() + HANDLE_W * 0.5, rect.right() - HANDLE_W * 0.5);
    let y = rect.center().y;

    if (resp.dragged() || resp.is_pointer_button_down_on())
        && let Some(p) = ui.ctx().input(|i| i.pointer.interact_pos())
    {
        let u = ((p.x - x0) / (x1 - x0).max(1e-9)).clamp(0.0, 1.0);
        let raw = lo + u * (hi - lo);
        *value = match steps {
            Some(n) if n > 1 => {
                let step = (hi - lo) / (n - 1) as f32;
                lo + ((raw - lo) / step).round() * step
            }
            _ => raw,
        };
    }

    if resp.double_clicked() {
        *value = default.clamp(lo, hi);
    }
    if *value != before {
        resp.mark_changed();
    }
    let t = ((*value - lo) / (hi - lo).max(1e-9)).clamp(0.0, 1.0);
    let painter = ui.painter_at(rect.expand(2.0));
    painter.line_segment(
        [pos2(x0, y), pos2(x1, y)],
        Stroke::new(1.0, Color32::from_gray(72)),
    );
    let handle = egui::Rect::from_center_size(
        pos2(x0 + t * (x1 - x0), y),
        vec2(HANDLE_W, HANDLE_W / HANDLE_ASPECT),
    );
    // Ruby only while it is being pulled — the rule `Row` states: ruby means "you are
    // doing this now", not "the pointer is here".
    let ink = if resp.dragged() {
        RUBY
    } else {
        Color32::from_gray(150)
    };
    painter.rect_filled(handle, 1.0, theme::CHROME);
    painter.rect_stroke(handle, 1.0, Stroke::new(1.0, ink), egui::StrokeKind::Inside);
    resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
}

pub struct Slider<'a>(Row<'a>);

impl<'a> Slider<'a> {
    pub fn new(
        value: &'a mut f32,
        default: f32,
        range: std::ops::RangeInclusive<f32>,
        text: &'a str,
    ) -> Self {
        Self(Row::new(value, default, range, text))
    }

    pub fn decimals(mut self, n: usize) -> Self {
        self.0 = self.0.decimals(n);
        self
    }

    pub fn suffix(mut self, s: &'a str) -> Self {
        self.0 = self.0.suffix(s);
        self
    }

    /// Returns true if the value changed for any reason, including the reset.
    pub fn show(self, ui: &mut egui::Ui) -> bool {
        self.0.show(ui)
    }
}

/// Keep the EV grid legible at different panel widths, using whole-stop steps.
fn curve_grid_step(extent: f32) -> usize {
    [1, 2, 4, 6]
        .into_iter()
        .min_by(|a, b| {
            (extent * *a as f32 / 12.0 - 48.0)
                .abs()
                .total_cmp(&(extent * *b as f32 / 12.0 - 48.0).abs())
        })
        .unwrap_or(2)
}

/// The curve editor.
///
/// Axes are the curve's own log2-EV window, not scene-linear: a curve editor on
/// linear data crushes the interesting range into the bottom few percent of the
/// horizontal axis and has nowhere to put the above-1.0 headroom. Horizontal is
/// stops, left (shadows) to right (highlights); vertical is output, bottom to top.
///
/// Interaction:
/// - click empty space to add a point and drag it
/// - drag a point to move it
/// - click a point, then use the arrow keys to move it (Shift moves ten times faster)
/// - right-click a point to delete it (endpoints are permanent)
///
/// `drag` is the caller-owned index of the point currently being dragged. It lives
/// outside this function because egui rebuilds the widget every frame and a drag
/// spans frames.
pub fn curve_editor(
    ui: &mut egui::Ui,
    curve: &mut Curve,
    drag: &mut Option<usize>,
    selected: &mut Option<usize>,
    ghost: Option<&[u32]>,
) -> bool {
    const HIT: f32 = 9.0;
    let width = ui.available_width();
    // A named interaction id keeps keyboard focus attached to the graph across
    // curve edits. Relying on the next automatic id let a rebuild orphan focus
    // after the first arrow press, which made a selected point move exactly once.
    let (rect, _) = ui.allocate_exact_size(vec2(width, 220.0), Sense::hover());
    let resp = ui.interact(rect, ui.id().with("curve editor"), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let mut changed = false;
    if selected.is_some_and(|i| i >= curve.points().len()) {
        *selected = None;
    }

    // Curve space (x right, y UP) -> screen space (y down).
    let to_screen = |p: [f32; 2]| {
        pos2(
            rect.left() + p[0] * rect.width(),
            rect.bottom() - p[1] * rect.height(),
        )
    };
    let to_curve = |p: Pos2| {
        [
            ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
            ((rect.bottom() - p.y) / rect.height()).clamp(0.0, 1.0),
        ]
    };

    painter.rect_filled(rect, 2.0, Color32::from_gray(20));

    // Match Tonal Distribution's filled outline, but keep the bins on the
    // curve's input EV axis so control points still target the correct tones.
    if let Some(bins) = ghost.filter(|b| b.len() > 1) {
        let peak = bins.iter().copied().max().unwrap_or(1).max(1) as f32;
        let color = Color32::from_gray(150);
        let points: Vec<Pos2> = bins
            .iter()
            .enumerate()
            .map(|(i, count)| {
                let h = (*count as f32 / peak).sqrt();
                pos2(
                    rect.left() + rect.width() * i as f32 / (bins.len() - 1) as f32,
                    rect.bottom() - (rect.height() - 2.0) * h,
                )
            })
            .collect();
        // One triangle strip follows the outline exactly, even at wide sizes.
        let mut fill = egui::Mesh::default();
        for point in &points {
            fill.colored_vertex(*point, color.linear_multiply(0.22));
            fill.colored_vertex(pos2(point.x, rect.bottom()), color.linear_multiply(0.22));
        }
        for i in 0..points.len() - 1 {
            let base = (i * 2) as u32;
            fill.add_triangle(base, base + 1, base + 2);
            fill.add_triangle(base + 2, base + 1, base + 3);
        }
        painter.add(egui::Shape::mesh(fill));
        painter.add(egui::Shape::line(points, Stroke::new(1.0, color)));
    }
    resp.clone().on_hover_text(crate::theme::tip(
        "Background: sampled input tones on the curve's −10 to +2 EV axis. Height uses square-root scaling. Tonal Distribution shows output tones after the display transform. Contrast Mask is not included.",
    ));

    // Independent spacing avoids stretched cells in a wide editor. Positions
    // remain whole EV stops; intermediate one-stop lines are quieter.
    for (vertical, extent) in [(true, rect.width()), (false, rect.height())] {
        for stop in (curve_grid_step(extent)..12).step_by(curve_grid_step(extent)) {
            let t = stop as f32 / 12.0;
            let stroke = Stroke::new(1.0, Color32::from_gray(if stop % 2 == 0 { 38 } else { 30 }));
            let line = if vertical {
                let x = rect.left() + t * rect.width();
                [pos2(x, rect.top()), pos2(x, rect.bottom())]
            } else {
                let y = rect.bottom() - t * rect.height();
                [pos2(rect.left(), y), pos2(rect.right(), y)]
            };
            painter.line_segment(line, stroke);
        }
    }
    // Identity reference, so a departure from linear is visible as a departure.
    painter.line_segment(
        [to_screen([0.0, 0.0]), to_screen([1.0, 1.0])],
        Stroke::new(1.0, Color32::from_gray(52)),
    );

    // Interaction before drawing, so the trace shows this frame's state.
    let nearest = |p: Pos2| -> Option<usize> {
        curve
            .points()
            .iter()
            .enumerate()
            .map(|(i, q)| (i, to_screen(*q).distance(p)))
            .filter(|(_, d)| *d <= HIT)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };

    if let Some(p) = resp.interact_pointer_pos() {
        if resp.secondary_clicked() {
            if let Some(i) = nearest(p) {
                curve.remove(i);
                changed = true;
                *drag = None;
                *selected = None;
            }
        } else if resp.drag_started() {
            // Hit-test where the press began, not where the pointer happens to be on
            // the frame egui declares a drag. A quick movement can travel beyond the
            // point's hit radius before crossing the drag threshold; testing `p`
            // there mistook that existing-point drag for an empty-space gesture and
            // inserted a second point. A slow/held drag appeared to work only because
            // it crossed the threshold while still near the original handle.
            let origin = ui
                .input(|input| input.pointer.press_origin())
                .filter(|origin| rect.contains(*origin))
                .unwrap_or(p);
            let i = match nearest(origin) {
                Some(i) => i,
                None => {
                    changed = true;
                    let c = to_curve(origin);
                    curve.add(c[0], c[1])
                }
            };
            *drag = Some(i);
            *selected = Some(i);
            resp.request_focus();
        } else if resp.clicked() {
            let i = match nearest(p) {
                Some(i) => i,
                None => {
                    changed = true;
                    let c = to_curve(p);
                    curve.add(c[0], c[1])
                }
            };
            *drag = Some(i);
            *selected = Some(i);
            resp.request_focus();
        }
        if let (true, Some(i)) = (resp.dragged(), *drag) {
            let c = to_curve(p);
            curve.move_point(i, c[0], c[1]);
            changed = true;
        }
    }
    if resp.drag_stopped() {
        *drag = None;
    }

    // Keyboard movement belongs to the graph only after a point has been clicked.
    // `key_down`, rather than `key_pressed`, handles both separate presses and a
    // held key's continuous movement. One frame is one hundredth of a stop; Shift
    // follows Adobe's convention and makes the step ten times larger. The endpoints'
    // x positions stay structural because `move_point` already pins them.
    if resp.has_focus()
        && let Some(i) = *selected
        && let Some(point) = curve.points().get(i).copied()
    {
        // Tell egui's spatial keyboard navigation that all four arrows belong to
        // this widget while it is focused. The filter protects subsequent frames;
        // cancelling the queued direction below also protects the very first arrow
        // after the point is clicked (the direction is queued before widgets draw).
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                resp.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    ..Default::default()
                },
            );
        });
        let (left, right, up, down, shift) = ui.input_mut(|input| {
            let unmodified =
                !input.modifiers.command && !input.modifiers.ctrl && !input.modifiers.alt;
            let down = |key| unmodified && input.key_down(key);
            let state = (
                down(egui::Key::ArrowLeft),
                down(egui::Key::ArrowRight),
                down(egui::Key::ArrowUp),
                down(egui::Key::ArrowDown),
                input.modifiers.shift,
            );
            for key in [
                egui::Key::ArrowLeft,
                egui::Key::ArrowRight,
                egui::Key::ArrowUp,
                egui::Key::ArrowDown,
            ] {
                let _ = input.count_and_consume_key(egui::Modifiers::NONE, key);
            }
            state
        });
        let ev = if shift { 0.1 } else { 0.01 };
        let step = ev / (raw_core::curve::HI_EV - raw_core::curve::LO_EV);
        let dx = (right as i8 - left as i8) as f32 * step;
        let dy = (up as i8 - down as i8) as f32 * step;
        if dx != 0.0 || dy != 0.0 {
            ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
            resp.request_focus();
            curve.move_point(i, point[0] + dx, point[1] + dy);
            changed = true;
            ui.ctx().request_repaint();
        }
    }

    // The trace. Sampled densely enough that the monotone cubic reads as a smooth
    // curve rather than the polyline it is drawn as.
    let n = 128;
    let line: Vec<Pos2> = (0..=n)
        .map(|i| {
            let x = i as f32 / n as f32;
            to_screen([x, curve.eval(x)])
        })
        .collect();
    painter.add(egui::Shape::line(
        line,
        Stroke::new(1.5, Color32::from_gray(215)),
    ));

    for (i, p) in curve.points().iter().enumerate() {
        let c = to_screen(*p);
        let hovered = resp.hover_pos().is_some_and(|h| h.distance(c) <= HIT);
        let active = *drag == Some(i) || *selected == Some(i) || hovered;
        painter.circle(
            c,
            if active { 5.0 } else { 3.5 },
            if active {
                RUBY
            } else {
                Color32::from_gray(230)
            },
            Stroke::new(1.0, Color32::from_gray(20)),
        );
    }

    changed
}

/// Draw a histogram as a filled area (tonal) or overlaid channel curves (RGB).
///
/// Counts are sqrt-compressed so a tall spike does not flatten everything else —
/// the standard trick for a readable photographic histogram.
/// The tonal-range mask's face: a strip of the actual print tones, ticked in L\*,
/// with the pre-D&B EV distribution behind it and the mask's trapezoid over it.
///
/// Returns whether the mask changed.
///
/// # What this is a redesign of
///
/// The prototype has a 58px control that stacks a ghost histogram in its own band
/// on top of a tone ramp with the L\* numbers printed *into* the ramp, and washes a
/// translucent gold trapezoid across the whole thing. the maintainer said he was not sure
/// about the look of it, and the three things that make it busy are separable from
/// the thing that makes it good:
///
/// - the histogram is **inside** the strip rather than in a band above it, so the
///   control is one object;
/// - the ticks are an **axis row beneath** rather than numbers over the tones they
///   are labelling, which is what forced them to switch between dark and light ink
///   halfway along;
/// - the trapezoid is **stroked with handles** rather than filled with a colour
///   wash, so the mask reads as a selection over the tones instead of tinting them.
///
/// What is kept is the part that earns the graphic at all: **the width you grab is
/// the range you get**. Dragging on the ramp selects a band of print tones
/// directly, and the histogram behind it says whether there are any there. No
/// arrangement of numeric sliders does that.
///
/// # Interactions
///
/// ```text
/// drag on the empty ramp   rubber-band a new range
/// drag a bound             move that edge
/// drag a feather handle    widen or narrow that shoulder
/// drag inside the band     slide the whole range
/// double-click             back to open
/// ```
pub fn zone_ruler(
    ui: &mut egui::Ui,
    mask: &mut ZoneMask,
    hist: &[f32],
    drag: &mut Option<ZoneGrab>,
) -> bool {
    const RAMP_H: f32 = 30.0;
    const AXIS_H: f32 = 11.0;
    /// Hit slop for a bound or a feather handle, in points.
    ///
    /// **Nine, not six.** the maintainer reported the feather handles as hard to grab, and
    /// six points either side of a line is a twelve-point target for something you
    /// aim at with a mouse — under Fitts's law that is a control you have to
    /// concentrate on. The diamonds are drawn at radius three, so the target is now
    /// three times the mark, which is the usual ratio for a small handle.
    const HIT: f32 = 9.0;

    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, RAMP_H + AXIS_H), Sense::click_and_drag());
    let ramp = Rect::from_min_size(rect.min, vec2(w, RAMP_H));
    let painter = ui.painter_at(rect);
    let mut changed = false;

    let (lo_ev, hi_ev) = (ZoneMask::MIN_EV, ZoneMask::MAX_EV);
    let to_x = |ev: f32| ramp.left() + (ev - lo_ev) / (hi_ev - lo_ev) * ramp.width();
    let to_ev = |x: f32| lo_ev + (x - ramp.left()) / ramp.width().max(1.0) * (hi_ev - lo_ev);

    // The ramp, as the actual print values: lum = 0.18 * 2^ev, display-encoded. It
    // is the reason the control is legible without a legend — the tones under the
    // trapezoid are the tones it selects.
    let step = 2.0f32.max(1.0);
    let mut x = ramp.left();
    // Print values, so exempt from the module re-grey.
    crate::theme::true_colour(ui, || {
        while x < ramp.right() {
            let lin = (0.18 * (to_ev(x + step * 0.5)).exp2()).min(1.0);
            let g = (lin.powf(1.0 / 2.2) * 255.0).round() as u8;
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(x, ramp.top()),
                    pos2((x + step).min(ramp.right()), ramp.bottom()),
                ),
                0.0,
                Color32::from_gray(g),
            );
            x += step;
        }
    });

    // The distribution, drawn INSIDE the ramp from its baseline rather than in a
    // band of its own. Ruby at low alpha: it is the one thing here that is neither
    // a tone nor a selection, and it has to be distinguishable from both while
    // sitting on top of a full black-to-white sweep — which no grey can be.
    //
    // **A filled curve, not bars.** the maintainer reported the top as jagged. Half of that
    // is in the data and is fixed where the data is made — `Basis::histogram` now
    // smooths — and half is here: a hundred bins across four hundred points is a 4px
    // bar, so every bin boundary was a visible vertical step. Sampling the bins at
    // one point per *pixel* and joining them turns the same numbers into a line.
    if hist.len() > 1 {
        let n = hist.len();
        let cols = ramp.width().round().max(2.0) as usize;
        let mut top: Vec<Pos2> = Vec::with_capacity(cols + 1);
        for c in 0..=cols {
            let u = c as f32 / cols as f32;
            // Linear between bin centres, so the curve passes through the data
            // rather than stepping between it.
            let f = (u * n as f32 - 0.5).clamp(0.0, (n - 1) as f32);
            let i = f.floor() as usize;
            let v = hist[i] + (hist[(i + 1).min(n - 1)] - hist[i]) * (f - i as f32);
            // sqrt, as the develop histogram does, so a spike does not flatten
            // everything else into the floor.
            let h = v.sqrt().clamp(0.0, 1.0) * (RAMP_H - 2.0);
            top.push(pos2(ramp.left() + u * ramp.width(), ramp.bottom() - h));
        }
        // **A mesh of quads, not `Shape::convex_polygon`.** That is what drew this
        // before and it is the whole of the stray diagonals the maintainer screenshotted: a
        // histogram outline is *concave* almost everywhere — every dip between two
        // peaks is a notch — and `convex_polygon` triangulates as a fan from the first
        // vertex, which for a concave outline throws triangles straight across the
        // hollows. The lines looked like they belonged to the feather because they
        // started near it; they belonged to the fill.
        //
        // One quad per column has no triangulation to get wrong. It is also exactly
        // the shape the data has, so nothing is being approximated back.
        let mut mesh = egui::Mesh::default();
        for c in 0..cols {
            let (a, b) = (top[c], top[c + 1]);
            let i = mesh.vertices.len() as u32;
            for p in [a, pos2(a.x, ramp.bottom()), b, pos2(b.x, ramp.bottom())] {
                mesh.colored_vertex(p, RUBY.gamma_multiply(0.40));
            }
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i + 1, i + 3, i + 2);
        }
        painter.add(egui::Shape::mesh(mesh));
        // A brighter line along the top. Without it the fill's edge is the only
        // thing describing the curve, and at 40% alpha over a black-to-white ramp
        // that edge vanishes in the highlights.
        painter.add(egui::Shape::line(
            top,
            Stroke::new(1.0, RUBY.gamma_multiply(0.85)),
        ));
    }

    // The axis, beneath. L\*, because that is the unit the value readout reports —
    // so pointing at a tone and reading the footer speak one language.
    for l in [10, 30, 50, 70, 90] {
        let f = (l as f32 + 16.0) / 116.0;
        let lin = if l > 8 { f * f * f } else { l as f32 / 903.3 };
        let ev = (lin / 0.18).log2();
        if ev < lo_ev || ev > hi_ev {
            continue;
        }
        let x = to_x(ev);
        painter.line_segment(
            [pos2(x, ramp.bottom()), pos2(x, ramp.bottom() + 2.0)],
            Stroke::new(1.0, DIM),
        );
        painter.text(
            pos2(x, ramp.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            l,
            egui::FontId::new(8.0, egui::FontFamily::Monospace),
            DIM,
        );
    }

    // ---- interaction, before the trapezoid is drawn, so it shows this frame
    let open_lo = mask.lo <= ZoneMask::MIN_EV + 1e-6;
    let open_hi = mask.hi >= ZoneMask::MAX_EV - 1e-6;
    let pointer = ui.ctx().input(|i| i.pointer.latest_pos());

    // What a press would take, before it is pressed. Drives the cursor and the
    // hover accent — the two things that were missing when the maintainer found the feather
    // handles hard to grab. A handle you cannot tell you are over is a handle you
    // aim at twice.
    let hovering = pointer
        .filter(|_| resp.hovered() || resp.contains_pointer())
        .and_then(|p| {
            let near = |ev: f32| (p.x - to_x(ev)).abs() <= HIT;
            if !open_lo && near(mask.lo) {
                Some(ZoneGrab::Lo)
            } else if !open_hi && near(mask.hi) {
                Some(ZoneGrab::Hi)
            } else if !open_lo && near(mask.lo - mask.f_lo) {
                Some(ZoneGrab::FeatherLo)
            } else if !open_hi && near(mask.hi + mask.f_hi) {
                Some(ZoneGrab::FeatherHi)
            } else {
                None
            }
        });
    // What is lit: whatever is being dragged wins over whatever is merely hovered,
    // so a handle does not go dim because the pointer ran ahead of it.
    let hot = drag.or(hovering);

    if resp.double_clicked() {
        mask.lo = ZoneMask::MIN_EV;
        mask.hi = ZoneMask::MAX_EV;
        changed = true;
    } else if resp.drag_started()
        && let Some(p) = pointer
    {
        *drag = if let Some(g) = hovering {
            Some(g)
        } else if p.x > to_x(mask.lo) && p.x < to_x(mask.hi) && !(open_lo && open_hi) {
            Some(ZoneGrab::Band {
                grab: to_ev(p.x) - mask.lo,
                span: mask.hi - mask.lo,
            })
        } else {
            // Rubber-band a new range from here. The anchor is where the press
            // landed, and the band grows in whichever direction the drag goes.
            Some(ZoneGrab::New { anchor: to_ev(p.x) })
        };
        mask.enabled = true;
        changed = true;
    }
    if !resp.dragged() && !resp.drag_stopped() {
        *drag = None;
    }
    if resp.dragged()
        && let Some(p) = pointer
    {
        let ev = to_ev(p.x).clamp(lo_ev, hi_ev);
        match *drag {
            Some(ZoneGrab::Lo) => mask.lo = ev.min(mask.hi),
            Some(ZoneGrab::Hi) => mask.hi = ev.max(mask.lo),
            // A feather handle sits OUTSIDE its bound, so the width is the distance
            // back to it — and cannot go negative, which would put the shoulder on
            // the wrong side of the edge it belongs to.
            Some(ZoneGrab::FeatherLo) => mask.f_lo = (mask.lo - ev).max(0.0),
            Some(ZoneGrab::FeatherHi) => mask.f_hi = (ev - mask.hi).max(0.0),
            Some(ZoneGrab::Band { grab, span }) => {
                // Slide, never resize, and clamp by SHIFTING rather than trimming —
                // the same rule `crop::drag` applies to `Handle::Body`, and for the
                // same reason: dragging a band into the end of the ruler must stop
                // it, not eat it.
                let start = (ev - grab).clamp(lo_ev, hi_ev - span);
                mask.lo = start;
                mask.hi = start + span;
            }
            Some(ZoneGrab::New { anchor }) => {
                mask.lo = anchor.min(ev);
                mask.hi = anchor.max(ev);
            }
            None => {}
        }
        changed = true;
    }

    // ---- the trapezoid, stroked
    let (x_lo, x_hi) = (to_x(mask.lo), to_x(mask.hi));
    let ink = if mask.enabled { RUBY } else { DIM };
    let edge = Stroke::new(1.0, ink);
    // **The feather slopes are white, the bounds stay ruby.** the maintainer's call, and it
    // separates two things that were being drawn as one: the bounds say *where* the
    // zone is and the slopes say *how fast it lets go*. In one ink the ramp read as a
    // single ruby zig-zag whose corners were hard to tell apart; in two, the shoulder
    // is legible as a shoulder. White rather than `theme::BRIGHT` because this is a
    // line on a chart rather than chrome text, and it has to stay visible where it
    // crosses the ramp's own white end.
    let feather_edge = Stroke::new(1.0, Color32::WHITE);

    if mask.enabled {
        // The band, as a floor line and two verticals rather than a fill. A wash
        // over a tone ramp changes the tones it is meant to be selecting, which is
        // the one thing this control must not do.
        let band = Rect::from_min_max(pos2(x_lo, ramp.top()), pos2(x_hi, ramp.bottom()));
        painter.rect_filled(band, 0.0, ink.gamma_multiply(0.18));
        painter.line_segment([pos2(x_lo, ramp.top()), pos2(x_hi, ramp.top())], edge);
        if !open_lo {
            painter.line_segment([pos2(x_lo, ramp.top()), pos2(x_lo, ramp.bottom())], edge);
            // The shoulder, as the slope it actually is.
            let xf = to_x(mask.lo - mask.f_lo);
            painter.line_segment(
                [pos2(xf, ramp.bottom()), pos2(x_lo, ramp.top())],
                feather_edge,
            );
            handle(
                &painter,
                pos2(xf, ramp.bottom()),
                Color32::WHITE,
                hot == Some(ZoneGrab::FeatherLo),
            );
        }
        if !open_hi {
            painter.line_segment([pos2(x_hi, ramp.top()), pos2(x_hi, ramp.bottom())], edge);
            let xf = to_x(mask.hi + mask.f_hi);
            painter.line_segment(
                [pos2(x_hi, ramp.top()), pos2(xf, ramp.bottom())],
                feather_edge,
            );
            handle(
                &painter,
                pos2(xf, ramp.bottom()),
                Color32::WHITE,
                hot == Some(ZoneGrab::FeatherHi),
            );
        }
        // The bounds get a mark too, and for the same reason: a bare vertical rule
        // gives nothing to aim at.
        if !open_lo {
            handle(
                &painter,
                pos2(x_lo, ramp.top()),
                ink,
                hot == Some(ZoneGrab::Lo),
            );
        }
        if !open_hi {
            handle(
                &painter,
                pos2(x_hi, ramp.top()),
                ink,
                hot == Some(ZoneGrab::Hi),
            );
        }
        if mask.invert {
            // Inverted: the band is the hole. Hatch it rather than drawing a second
            // filled shape, so which side is selected stays readable at a glance.
            let mut x = x_lo;
            while x < x_hi {
                painter.line_segment(
                    [
                        pos2(x, ramp.top()),
                        pos2((x + 6.0).min(x_hi), ramp.bottom()),
                    ],
                    Stroke::new(1.0, ink.gamma_multiply(0.5)),
                );
                x += 6.0;
            }
        }
    }
    painter.rect_stroke(
        ramp,
        0.0,
        Stroke::new(1.0, Color32::from_gray(60)),
        egui::StrokeKind::Inside,
    );

    if resp.hovered() || resp.dragged() {
        // A distinct cursor over a handle, so the two things you can do to this
        // control — move an edge, or rubber-band a new range — say which is which
        // before you commit to a press.
        ui.ctx().set_cursor_icon(match hot {
            Some(ZoneGrab::Lo) | Some(ZoneGrab::Hi) => egui::CursorIcon::ResizeHorizontal,
            Some(_) => egui::CursorIcon::ResizeColumn,
            None => egui::CursorIcon::Crosshair,
        });
    }
    changed
}

/// A feather handle: a small diamond, which reads as a handle where a dot reads as
/// a data point — the curve editor's control points are dots and these are not the
/// same kind of thing.
fn handle(painter: &egui::Painter, at: Pos2, ink: Color32, hot: bool) {
    // Grows as well as brightens when it is the one a press would take. Brightness
    // alone is not enough on a control drawn over a black-to-white ramp — at the
    // white end there is nothing to brighten *against*.
    let r = if hot { 4.5 } else { 3.0 };
    let ink = if hot { Color32::WHITE } else { ink };
    painter.add(egui::Shape::convex_polygon(
        vec![
            pos2(at.x, at.y - r),
            pos2(at.x + r, at.y),
            pos2(at.x, at.y + r),
            pos2(at.x - r, at.y),
        ],
        ink,
        Stroke::new(1.0, Color32::from_gray(20)),
    ));
}

/// What a zone-ruler drag has hold of. Spans frames, like `crop::Grab`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneGrab {
    Lo,
    Hi,
    FeatherLo,
    FeatherHi,
    /// Sliding the whole band. Both halves frozen at the press, so the band does
    /// not jump to centre itself on the cursor.
    Band {
        grab: f32,
        span: f32,
    },
    /// Rubber-banding a new one from where the press landed.
    New {
        anchor: f32,
    },
}

/// Fill the area under `line` down to `floor`, one rectangle per physical pixel
/// column, each as tall as the line at that column's centre.
///
/// **Why columns, and why a pixel wide.** Two earlier fills each had a visible flaw:
/// - one flat bar per *bin* stepped out from under the sloped outline, a jagged edge
///   along it;
/// - a triangle mesh following the outline exactly fixed that, but meshes are neither
///   anti-aliased nor snapped to pixels, so as the data moved by fractions of a pixel
///   during a drag the hard edge crawled — the histogram looked like it was jumping.
///
/// Rectangles snap to the pixel grid, so a column either stays or moves one clean
/// pixel; at one pixel wide its top is within half a pixel of the line, which the
/// line itself covers.
///
/// `line` must run left to right.
fn fill_under(painter: &egui::Painter, line: &[Pos2], floor: f32, fill: Color32) {
    let (Some(first), Some(last)) = (line.first(), line.last()) else {
        return;
    };
    let px = 1.0 / painter.pixels_per_point();
    let mut seg = 0;
    let mut x = first.x;
    while x < last.x {
        let x1 = (x + px).min(last.x);
        let c = 0.5 * (x + x1);
        while seg + 2 < line.len() && line[seg + 1].x < c {
            seg += 1;
        }
        let (a, b) = (line[seg], line[(seg + 1).min(line.len() - 1)]);
        let t = if b.x > a.x {
            ((c - a.x) / (b.x - a.x)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let y = a.y + (b.y - a.y) * t;
        if y < floor {
            painter.rect_filled(Rect::from_min_max(pos2(x, y), pos2(x1, floor)), 0.0, fill);
        }
        x = x1;
    }
}

pub fn histogram(
    ui: &mut egui::Ui,
    bins: usize,
    tonal: Option<&[u32]>,
    rgb: Option<[&[u32]; 3]>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 74.0), Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(20));

    let curve = |b: &[u32]| -> Vec<f32> {
        let raw: Vec<f32> = b.iter().map(|&c| (c as f32).sqrt()).collect();
        let max = raw.iter().cloned().fold(1.0f32, f32::max);
        raw.into_iter().map(|v| v / max).collect()
    };
    let x_at = |i: usize| rect.left() + rect.width() * i as f32 / (bins - 1) as f32;
    let y_at = |h: f32| rect.bottom() - (rect.height() - 2.0) * h;

    let draw = |b: &[u32], color: Color32, fill: bool| {
        let c = curve(b);
        let line: Vec<Pos2> = (0..bins).map(|i| pos2(x_at(i), y_at(c[i]))).collect();
        if fill {
            fill_under(&painter, &line, rect.bottom(), color.linear_multiply(0.28));
        }
        painter.add(egui::Shape::line(line, Stroke::new(1.0, color)));
    };

    if let Some(t) = tonal {
        // Neutral, not the accent: this describes image data, and a tint next to a
        // monochrome rendering shifts how its tonality reads.
        draw(t, Color32::from_gray(205), true);
    }
    if let Some([r, g, b]) = rgb {
        // Conventional channel tints, even in a mono app — they read as R/G/B.
        draw(r, Color32::from_rgb(210, 90, 90), false);
        draw(g, Color32::from_rgb(90, 200, 110), false);
        draw(b, Color32::from_rgb(100, 130, 220), false);
    }
    response
}

/// The update badge, as the title strip draws it at its right end.
///
/// A pill with the offered version in it — Mole's manner: the update announces
/// itself in the chrome and waits, it does not open a window over your work.
pub struct UpdateBadge {
    /// The pill's text, e.g. `0.2.1 ready`.
    pub text: String,
    /// The one-line status behind it, shown as the hover tooltip.
    pub detail: String,
    /// The last attempt failed. The pill borrows ruby, which elsewhere marks
    /// states that want attention, rather than the amber of a waiting offer.
    pub failed: bool,
}

/// What the title strip's update badge was asked to do this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeClick {
    None,
    /// The body: open the update sheet.
    Open,
    /// The `×`: hide the badge until the update state changes.
    Dismiss,
}

/// Paint the window's app strip on macOS and its mode/status strip elsewhere.
///
/// The window is created with `fullsize_content_view` and a hidden titlebar so the
/// frame is the app's dark grey rather than the OS default. That puts the traffic
/// lights on top of our content, so this claims the space back. Windows and Linux
/// retain their native titlebars; there the left label names the active mode instead
/// of repeating the application title.
/// `centre` is the **top-centre readout**: the app's one line for saying what the
/// frame in front of you *is*, rather than what the app is doing.
///
/// Its resting state is the exposure — ISO, aperture, shutter, focal length — set
/// in ruby because it is the only text in the chrome that describes the photograph
/// rather than the program. Star ratings (`⌘1`–`⌘5`) and colour labels (`⇧1`–`⇧5`)
/// are specified to report through here too, transiently, when they exist; the
/// hotkey table has them bound and pending.
///
/// The undo/redo depth used to sit at the right of this strip. It came out because
/// it is *state*, not identity — a number that changes as you work and tells you
/// nothing about the image — and a title bar is the wrong place to watch it.
///
/// `update` is the badge that occupies the strip's right end when the updater has
/// something to say. The centre readout stays centred on the *window* whether or
/// not the badge is up; a badge appearing never displaces it.
pub fn title_strip(
    ui: &mut egui::Ui,
    title: &str,
    centre: &str,
    update: Option<&UpdateBadge>,
) -> BadgeClick {
    let h = crate::theme::TITLE_STRIP;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, crate::theme::CHROME_DEEP);
    painter.text(
        rect.left_center() + Vec2::new(crate::theme::TITLE_INSET, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(crate::theme::size::TITLE),
        Color32::from_gray(190),
    );
    if !centre.is_empty() {
        // Centred on the *window*, not on the space left over after the title —
        // the traffic-light inset is asymmetric, so centring in the remainder
        // would put it visibly off-centre against the image below it.
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            centre,
            egui::FontId::monospace(crate::theme::size::BODY),
            RUBY,
        );
    }

    let Some(badge) = update else {
        return BadgeClick::None;
    };
    // Pinned to the right end, clear of the edge by the strip's own inset. Square,
    // like every other control, with the body opening the sheet and the `×` at its
    // right end dismissing it.
    let font = egui::FontId::proportional(crate::theme::size::CAPTION);
    let galley = painter.layout_no_wrap(badge.text.clone(), font.clone(), Color32::WHITE);
    let pad_x = 8.0;
    let close_w = h - 8.0;
    let height = h - 8.0;
    let right = rect.right() - crate::theme::TITLE_INSET.min(16.0);
    let close = Rect::from_min_size(
        pos2(right - close_w, rect.center().y - height / 2.0),
        vec2(close_w, height),
    );
    let body = Rect::from_min_max(
        pos2(close.left() - galley.size().x - pad_x * 2.0, close.top()),
        pos2(close.left(), close.bottom()),
    );
    let body_response = ui
        .interact(body, ui.id().with("update-badge"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if badge.detail.is_empty() {
            "An update is available — click for details".to_owned()
        } else {
            badge.detail.clone()
        });
    let close_response = ui
        .interact(close, ui.id().with("update-badge-close"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Hide until the update changes");
    // Amber is the strip's "there is something waiting" ink — the loupe's
    // argument: ruby would put this in the class of marks that change the
    // picture. A failed check is the exception, and borrows ruby's pair.
    let (ground, ink) = if badge.failed {
        (crate::theme::RUBY_FILL_DIM, RUBY)
    } else {
        (
            crate::theme::AMBER.linear_multiply(0.22),
            crate::theme::AMBER,
        )
    };
    let whole = body.union(close);
    painter.rect_filled(whole, 0.0, ground);
    if body_response.hovered() {
        painter.rect_filled(body, 0.0, ink.linear_multiply(0.12));
    }
    if close_response.hovered() {
        painter.rect_filled(close, 0.0, ink.linear_multiply(0.12));
    }
    painter.vline(
        close.left(),
        close.y_range().shrink(3.0),
        egui::Stroke::new(1.0, ink.linear_multiply(0.35)),
    );
    painter.text(
        body.center(),
        egui::Align2::CENTER_CENTER,
        &badge.text,
        font.clone(),
        ink,
    );
    painter.text(close.center(), egui::Align2::CENTER_CENTER, "×", font, ink);
    if close_response.clicked() {
        BadgeClick::Dismiss
    } else if body_response.clicked() {
        BadgeClick::Open
    } else {
        BadgeClick::None
    }
}

/// The toning placement curve: **strength against print tone**.
///
/// A compact editor over `Curve`'s own normalised 0–1 space, rather than
/// [`curve_editor`], which draws its x axis in log2 EV because that is what a *tone*
/// curve needs. Placement is not a tone curve: x is L\* as the panel shows it and y is
/// how much toning that tone receives, so it wants the plain unit square and would have
/// to fight the EV mapping to get it.
///
/// Interaction is the tone curve's, so the gesture is the app's one gesture:
/// - click empty space to add a point and drag it
/// - drag a point to move it
/// - right-click a point to delete it (the ends are permanent)
/// - double-click empty space to reset to flat
///
/// **Flat is the identity here, not the diagonal.** A diagonal would mean "tone the
/// highlights and leave the blacks", which is a strange thing to open a module on.
pub fn placement_editor(
    ui: &mut egui::Ui,
    curve: &mut Curve,
    drag: &mut Option<usize>,
    hist: &[f32],
) -> bool {
    const H: f32 = 84.0;
    /// Hit slop, matching the zone ruler's — three times the mark it targets.
    const HIT: f32 = 9.0;

    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, H), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let mut changed = false;

    let to_screen = |p: [f32; 2]| {
        pos2(
            rect.left() + p[0] * rect.width(),
            rect.bottom() - p[1] * rect.height(),
        )
    };
    let to_curve = |p: egui::Pos2| {
        [
            ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0),
            ((rect.bottom() - p.y) / rect.height().max(1.0)).clamp(0.0, 1.0),
        ]
    };

    painter.rect_filled(rect, 2.0, crate::theme::CHROME_DEEP);

    // The tone ramp along the bottom edge, so the axis says what it is without a legend
    // — the same trick the zone ruler uses, and the reason neither needs labelling.
    let strip = 6.0;
    // Print values, so exempt from the module re-grey.
    crate::theme::true_colour(ui, || {
        let steps = 48;
        for i in 0..steps {
            let t = i as f32 / steps as f32;
            let g = (t * 255.0).round() as u8;
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(rect.left() + t * rect.width(), rect.bottom() - strip),
                    pos2(
                        rect.left() + (i + 1) as f32 / steps as f32 * rect.width(),
                        rect.bottom(),
                    ),
                ),
                0.0,
                egui::Color32::from_gray(g),
            );
        }
    });

    // **The print's own distribution, behind the curve**, which is where it belongs and
    // where it did not used to be: it was drawn over the toned ramp in the module above,
    // and a distribution over a colour ramp cannot be read as a distribution. Here it
    // has an axis to sit on and a curve to be compared against, which is the comparison
    // anyone shaping placement is actually making — *is there anything at the tone I am
    // holding back?*
    //
    // Smoothed, because a 256-bin count across a 300pt strip is a bin per pixel and
    // drawing a count that jumps between neighbours faithfully is drawing the noise.
    if hist.len() >= 8 {
        let smooth: Vec<f32> = (0..hist.len())
            .map(|i| {
                let lo = i.saturating_sub(3);
                let hi = (i + 4).min(hist.len());
                hist[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
            })
            .collect();
        let peak = smooth.iter().cloned().fold(0.0f32, f32::max).max(1e-6);
        let floor = rect.bottom() - strip;
        let top = rect.top() + 1.0;

        // **Filled per pixel column, not as a polygon.** It was `Shape::convex_polygon`,
        // and a distribution is the least convex shape there is — egui's tessellator
        // fans it from a single vertex, so every trough sent a triangle back across the
        // graph. See [`fill_under`] for why columns rather than bars or a mesh.
        let step = rect.width() / (smooth.len() - 1) as f32;
        let line: Vec<Pos2> = smooth
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let h = (v / peak) * (floor - top) * 0.9;
                pos2(rect.left() + i as f32 * step, floor - h)
            })
            .collect();
        fill_under(&painter, &line, floor, egui::Color32::from_white_alpha(20));
    }

    // **The datum, drawn through the middle.** Flat is where the module opens, so the
    // line says what "no shaping" looks like — without it the curve is a shape with
    // nothing to be a shape against.
    //
    // It sits at 0.5 of the curve's own range, which is strength 1.0: see
    // `toning::flat_placement` on why the datum is a half. Drawn at the top, which is
    // what a 1.0 datum gave, the control could only ever take toning away and the line
    // read as a border rather than as a reference.
    let unity = to_screen([0.0, 0.5]).y;
    painter.line_segment(
        [pos2(rect.left(), unity), pos2(rect.right(), unity)],
        egui::Stroke::new(1.0, crate::theme::DIM.gamma_multiply(0.55)),
    );

    let pts: Vec<[f32; 2]> = curve.points().to_vec();
    let hit = |p: egui::Pos2| pts.iter().position(|c| to_screen(*c).distance(p) <= HIT);

    if let Some(p) = resp.interact_pointer_pos() {
        if resp.double_clicked() {
            *curve = raw_core::toning::flat_placement();
            *drag = None;
            changed = true;
        } else if resp.drag_started() {
            *drag = hit(p).or_else(|| {
                let c = to_curve(p);
                changed = true;
                Some(curve.add(c[0], c[1]))
            });
        } else if resp.secondary_clicked()
            && let Some(i) = hit(p)
            && i != 0
            && i + 1 != pts.len()
        {
            curve.remove(i);
            changed = true;
        }
        if resp.dragged()
            && let Some(i) = *drag
        {
            let c = to_curve(p);
            curve.move_point(i, c[0], c[1]);
            changed = true;
        }
    }
    if resp.drag_stopped() {
        *drag = None;
    }

    // The curve itself, sampled rather than drawn through its control points, so what is
    // on screen is what the model will evaluate.
    let line: Vec<egui::Pos2> = (0..=64)
        .map(|i| {
            let x = i as f32 / 64.0;
            to_screen([x, curve.eval(x).clamp(0.0, 1.2) / 1.2 * 1.2])
        })
        .map(|p| pos2(p.x, p.y.clamp(rect.top(), rect.bottom() - strip)))
        .collect();
    painter.add(egui::Shape::line(
        line,
        egui::Stroke::new(1.5, crate::theme::AMBER),
    ));

    for (i, c) in pts.iter().enumerate() {
        let p = to_screen(*c);
        let p = pos2(p.x, p.y.clamp(rect.top(), rect.bottom() - strip));
        painter.circle_filled(
            p,
            if *drag == Some(i) { 4.0 } else { 3.0 },
            crate::theme::BRIGHT,
        );
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut field: impl FnMut(&mut egui::Ui) -> egui::Response,
    ) -> bool {
        let mut changed = false;
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 100.0))),
                events,
                ..Default::default()
            },
            |ui| {
                let response = field(ui);
                response.request_focus();
                changed = response.changed();
            },
        );
        changed
    }

    fn arrow(key: egui::Key, repeat: bool) -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    #[test]
    fn numeric_slider_arrows_survive_display_rounding() {
        // Actual egui key events, including held-key repeats, for the controls
        // reproduced in the bug review. Focus itself enters text-edit mode.
        for (initial, lo, hi, decimals, step) in [
            (0.0, -6.0, 6.0, 2, 0.04),
            (0.25, 0.05, 0.60, 2, 0.01),
            (0.5, 0.0, 1.0, 2, 0.01),
            (5.0, 1.0, 20.0, 0, 1.0),
            (20.0, 5.0, 60.0, 0, 1.0),
            (0.3, 0.05, 0.75, 2, 0.01),
            (80.0, 0.0, 100.0, 0, 1.0),
            (5.0, 1.0, 12.0, 1, 0.1),
            (2.2, 1.0, 3.0, 2, 0.01),
        ] {
            let ctx = egui::Context::default();
            let mut value = initial;
            number_frame(&ctx, vec![], |ui| {
                ui.add(slider_number(&mut value, lo..=hi, decimals))
            });
            for n in 1..=3 {
                assert!(number_frame(&ctx, arrow(egui::Key::ArrowUp, n > 1), |ui| {
                    ui.add(slider_number(&mut value, lo..=hi, decimals))
                }));
                assert!((value - (initial + step * n as f32)).abs() < 1e-5);
            }
            for n in (0..3).rev() {
                assert!(number_frame(
                    &ctx,
                    arrow(egui::Key::ArrowDown, n < 2),
                    |ui| { ui.add(slider_number(&mut value, lo..=hi, decimals)) }
                ));
                assert!((value - (initial + step * n as f32)).abs() < 1e-5);
            }
            for (limit, key) in [(lo, egui::Key::ArrowDown), (hi, egui::Key::ArrowUp)] {
                value = limit;
                number_frame(&ctx, arrow(key, false), |ui| {
                    ui.add(slider_number(&mut value, lo..=hi, decimals))
                });
                assert_eq!(value, limit);
            }
        }
    }

    #[test]
    fn numeric_integer_arrows_typing_and_limits() {
        let ctx = egui::Context::default();
        let mut value = 4_u32;
        number_frame(&ctx, vec![], |ui| {
            ui.add(bounded_number(&mut value, 1..=12, 1.0))
        });
        for expected in 5..=12 {
            assert!(number_frame(&ctx, arrow(egui::Key::ArrowUp, true), |ui| {
                ui.add(bounded_number(&mut value, 1..=12, 1.0))
            }));
            assert_eq!(value, expected);
        }
        assert!(!number_frame(&ctx, arrow(egui::Key::ArrowUp, true), |ui| {
            ui.add(bounded_number(&mut value, 1..=12, 1.0))
        }));
        assert_eq!(value, 12);
        // Replace the focused text, then nudge the typed value.
        let select_all = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                command: true,
                ctrl: true,
                ..Default::default()
            },
        };
        assert!(number_frame(
            &ctx,
            vec![select_all, egui::Event::Text("1".into())],
            |ui| { ui.add(bounded_number(&mut value, 1..=12, 1.0)) }
        ));
        assert_eq!(value, 1);
        assert!(!number_frame(
            &ctx,
            arrow(egui::Key::ArrowDown, false),
            |ui| { ui.add(bounded_number(&mut value, 1..=12, 1.0)) }
        ));
        assert_eq!(value, 1);
        assert!(number_frame(&ctx, arrow(egui::Key::ArrowUp, false), |ui| {
            ui.add(bounded_number(&mut value, 1..=12, 1.0))
        }));
        assert_eq!(value, 2);
    }

    #[test]
    fn settings_and_lightbox_slider_tracks_double_click_to_default() {
        for native in [true, false] {
            let ctx = egui::Context::default();
            crate::theme::apply(&ctx);
            let mut value = 190.0;
            let mut rect = egui::Rect::NOTHING;
            let mut changed = false;
            for pass in 0..5 {
                let at = rect.left_center() + egui::vec2(20.0, 0.0);
                let events = if pass == 0 {
                    vec![]
                } else {
                    vec![
                        egui::Event::PointerMoved(at),
                        egui::Event::PointerButton {
                            pos: at,
                            button: egui::PointerButton::Primary,
                            pressed: pass % 2 == 1,
                            modifiers: egui::Modifiers::default(),
                        },
                    ]
                };
                let input = egui::RawInput {
                    time: Some(pass as f64 * 0.05),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 100.0),
                    )),
                    events,
                    ..Default::default()
                };
                let _ = ctx.run_ui(input, |ui| {
                    let response = if native {
                        settings_slider(ui, &mut value, 85.0, 0.0..=300.0)
                    } else {
                        bare_slider(ui, &mut value, 85.0, 0.0..=300.0, 100.0, None)
                    };
                    rect = response.rect;
                    changed = response.changed();
                });
            }
            assert_eq!(value, 85.0, "native: {native}");
            assert!(changed, "reset must report a change");
        }
    }

    #[test]
    fn a_selected_curve_point_accepts_repeated_and_held_arrow_input() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut curve = Curve::default();
        let point = curve.add(0.5, 0.5);
        let mut selected = Some(point);
        let mut drag = None;
        let mut graph_id = None;
        let key = |pressed| egui::Event::Key {
            key: egui::Key::ArrowUp,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };

        // Press, hold for another frame, release, then press again. Focus is only
        // requested before the first edit: retaining it is the behavior under test.
        for pass in 0..6 {
            let events = match pass {
                1 | 4 => vec![key(true)],
                3 | 5 => vec![key(false)],
                _ => Vec::new(),
            };
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(400.0, 400.0),
                )),
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                let _ = ui.button("focusable control above");
                let id = ui.id().with("curve editor");
                graph_id = Some(id);
                if pass == 0 {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                curve_editor(ui, &mut curve, &mut drag, &mut selected, None);
                let _ = ui.button("focusable control below");
            });
            assert_eq!(
                ctx.memory(|m| m.focused()),
                graph_id,
                "an arrow moved keyboard focus from the curve to a neighboring control"
            );
        }

        let one_step = 0.01 / (raw_core::curve::HI_EV - raw_core::curve::LO_EV);
        assert!(
            curve.points()[point][1] >= 0.5 + 3.0 * one_step - 1.0e-6,
            "the graph lost focus or did not continue while the key was held"
        );
    }

    #[test]
    fn a_fast_drag_grabs_the_point_at_the_press_origin() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut curve = Curve::default();
        let point = curve.add(0.5, 0.5);
        let mut selected = None;
        let mut drag = None;
        let graph = std::cell::Cell::new(egui::Rect::NOTHING);
        let input = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(400.0, 400.0),
            )),
            events,
            ..Default::default()
        };
        let mut frame = |events| {
            let _ = ctx.run_ui(input(events), |ui| {
                let width = ui.available_width();
                graph.set(egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(width, 220.0),
                ));
                curve_editor(ui, &mut curve, &mut drag, &mut selected, None);
            });
        };

        frame(Vec::new());
        let start = egui::pos2(graph.get().center().x, graph.get().center().y);
        let end = start + egui::vec2(45.0, -45.0);
        let button = |at, pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(vec![egui::Event::PointerMoved(start), button(start, true)]);
        // One large movement crosses the drag threshold well outside the 9pt point
        // hit radius. The press origin must still decide what was grabbed.
        frame(vec![egui::Event::PointerMoved(end)]);
        frame(vec![button(end, false)]);

        assert_eq!(
            curve.points().len(),
            3,
            "a fast drag inserted a new point instead of grabbing the pressed one"
        );
        assert_eq!(selected, Some(point));
        assert!(curve.points()[point][0] > 0.6);
        assert!(curve.points()[point][1] > 0.6);
    }

    #[test]
    fn a_module_honours_its_startup_fold() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        // Font changes become active at the start of the next frame.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        let mut opened = false;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = Module::new("START CLOSED")
                .open_on_start(false)
                .show(ui, |_| {
                    opened = true;
                });
        });
        assert!(!opened, "a startup-closed module drew its body");

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let _ = Module::new("START OPEN").open_on_start(true).show(ui, |_| {
                opened = true;
            });
        });
        assert!(opened, "a startup-open module hid its body");
    }

    #[test]
    fn a_plain_section_can_be_revealed_when_its_tool_appears() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        // Font changes become active at the start of the next frame.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});

        let mut opened = false;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            Plain::new("REVEAL ON DEMAND")
                .open_on_start(false)
                .show(ui, |_| opened = true);
        });
        assert!(!opened, "the section did not begin collapsed");

        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            Plain::new("REVEAL ON DEMAND")
                .open_on_start(false)
                .open_when(true)
                .show(ui, |_| opened = true);
        });
        assert!(opened, "the appearing tool did not reveal its section");
    }

    /// **Any edit arms, and a frame with no edit in it changes nothing.**
    ///
    /// The second case is the one that changed. The rule used to fire only on the
    /// transition out of default, which meant a module armed once and then never again
    /// — switch it off and the dot was the only way back, permanently, because the
    /// sidecar keeps the edited values across a reopen. the maintainer asked for the simple
    /// rule; what it costs is bypassing a module and continuing to work its sliders.
    #[test]
    fn any_edit_arms_the_module_and_nothing_else_does() {
        // The first slider move on an untouched module switches it on, so the control
        // does something the moment it is moved.
        let mut fresh = false;
        arm(true, &mut fresh);
        assert!(fresh, "an edit to an untouched module must arm it");

        // **Already edited, switched off, edited again.** This is the case the old rule
        // got wrong: `was_default` was false, so nothing armed and the slider was dead
        // for the rest of the file's life.
        let mut edited_then_bypassed = false;
        arm(true, &mut edited_then_bypassed);
        assert!(
            edited_then_bypassed,
            "a module that was edited before must still arm — this is the reported bug"
        );

        // A frame that changed nothing leaves the switch exactly as it was, in both
        // positions. This is what keeps the dot an explicit control: it holds until you
        // touch something, rather than being re-asserted every frame.
        let mut off = false;
        arm(false, &mut off);
        assert!(!off, "a frame with no edit turned a module on");

        let mut on = true;
        arm(false, &mut on);
        assert!(on, "a frame with no edit turned a module off");
    }

    /// Run `n` frames of a resizable left panel full of modules, and report the
    /// width the panel offered its content each time.
    fn panel_widths(n: usize) -> Vec<f32> {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        (0..n)
            .map(|_| {
                let mut got = 0.0;
                let _ = ctx.run_ui(input(), |ui| {
                    egui::Panel::left("develop")
                        .default_size(320.0)
                        .resizable(true)
                        .show(ui, |ui| {
                            got = ui.available_width();
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let _ = Module::new("DECODE").modified(true).show(ui, |ui| {
                                    ui.label("a control");
                                });
                                Plain::new("HISTOGRAM").show(ui, |ui| {
                                    ui.label("a readout");
                                });
                                let _ = Module::new("CURVE").modified(true).switch(true).show(
                                    ui,
                                    |ui| {
                                        ui.label("a much much much wider control");
                                    },
                                );
                            });
                        });
                    egui::CentralPanel::default().show(ui, |_| {});
                });
                got
            })
            .collect()
    }

    /// The width of a string at `size::BODY` in the app's own face, one throwaway
    /// frame in so the fonts are actually loaded.
    fn text_w(text: &str) -> f32 {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut w = 0.0;
        for _ in 0..2 {
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                let rich = egui::RichText::new(text).size(crate::theme::size::BODY);
                w = ui.label(rich).rect.width();
            });
        }
        w
    }

    /// **The row's two fixed columns are wide enough for everything the app puts in
    /// them.** This is the guard on what the maintainer reported, and it is worth saying why it
    /// is a test rather than a comment: a widget wider than its column does not clip,
    /// it pushes the rest of the row along — so one long label silently takes its own
    /// row out of the grid and nothing else on screen changes. The failure is invisible
    /// in a diff and obvious in use, which is the wrong way round.
    ///
    /// The two lists are the longest label and the widest readout **in the app today**,
    /// found by sorting every `Row::new` / `Slider::new` call site and by taking each
    /// one's range at its stated decimals and suffix. Adding a longer one is allowed;
    /// adding it without moving the constant is what fails here.
    ///
    /// `Spacer distance` is the binding one at fifteen characters, and the column has
    /// two points on it. That is deliberately tight: the whole budget was solved
    /// backwards from the 100pt track the maintainer asked for, so slack here is track nobody
    /// gets.
    #[test]
    fn the_columns_fit_what_goes_in_them() {
        let labels = [
            "Spacer distance",
            "Registration X",
            "Mask contrast",
            "Crystal size",
        ];
        for label in labels {
            let w = text_w(label);
            assert!(
                w <= Row::LABEL_W,
                "{label:?} is {w:.1}pt and the label column is {:.1}pt — it would push \
                 its own value and track right and take that row out of the grid",
                Row::LABEL_W,
            );
        }

        // A `DragValue` is a button: its box is the text plus `button_padding` at each
        // end. The minus sign counts — it is the *low* end of these ranges that is
        // widest, which is exactly the case an eyeballed constant misses.
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut pad = 0.0;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            pad = ui.spacing().button_padding.x;
        });
        for readout in ["-40.0 px", "-6.00 EV", "-0.1500", "-180.0°", "1.50 %"] {
            let w = text_w(readout) + 2.0 * pad;
            assert!(
                w <= Row::VALUE_W,
                "the readout {readout:?} needs {w:.1}pt and the value column is {:.1}pt \
                 — that row's track would start further right than every other row's",
                Row::VALUE_W,
            );
        }
    }

    /// The chosen track must fit without clipping at the default panel width.
    #[test]
    fn the_row_budget_fits_the_default_panel_without_shortening_the_track() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1400.0, 900.0),
            )),
            ..Default::default()
        };
        // What a module body actually offers a row at the default panel width — which
        // is the panel less the scrollbar gutter, the frame's inner margin and its
        // stroke, none of which are worth re-deriving here when the frame can be asked.
        let mut body = 0.0;
        for _ in 0..2 {
            let _ = ctx.run_ui(input(), |ui| {
                egui::Panel::left("develop")
                    .default_size(crate::layout::DEVELOP_W)
                    .resizable(true)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let _ = Module::new("EXPOSURE").show(ui, |ui| {
                                body = ui.available_width();
                            });
                        });
                    });
                egui::CentralPanel::default().show(ui, |_| {});
            });
        }

        let spent = Row::LABEL_W + 2.0 * Row::GAP + Row::VALUE_W + Row::TRACK_W;
        assert!(
            spent <= body + 0.5,
            "the row wants {spent:.1}pt and a module body offers {body:.1}pt — the track \
             would clamp to {:.1}pt instead of the {:.1}pt it promises",
            body - Row::LABEL_W - 2.0 * Row::GAP - Row::VALUE_W,
            Row::TRACK_W,
        );
    }

    /// **The cell is its column wide, and the text starts at the column's left edge** —
    /// for a label far shorter than the column and for one far longer.
    ///
    /// Three failures, and they are not the same failure. A short label that only
    /// advances the cursor by its own galley puts the next column somewhere different on
    /// every row; a long one that overflows does it in the other direction; and a label
    /// **centred inside a correctly-sized cell** leaves the columns perfectly aligned and
    /// the words ragged, which is the one that ships.
    ///
    /// That third assert is here because its absence is what let the bug through. The
    /// first version of this test measured the cell and nothing else — and the cell was
    /// already right, so it passed on a build where every label in the app was centred.
    /// the maintainer found it in a screenshot by drawing a guide down the panel and seeing that
    /// nothing touched it. **A test that measures the container does not test the
    /// contents**, and a fixed-width column is exactly where that distinction hides.
    #[test]
    fn a_label_cell_puts_its_text_at_the_column_edge() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut seen: Vec<(String, f32, egui::Rect, f32)> = Vec::new();
        // Twice: the first frame lays out before the fonts have finished loading.
        for _ in 0..2 {
            seen.clear();
            let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
                for text in ["R", "Radius", "Spacer distance", &"M".repeat(60)] {
                    ui.horizontal(|ui| {
                        let left = ui.cursor().left();
                        let (cell, glyphs) = label_cell(ui, text, 18.0, theme::size::SLIDER);
                        seen.push((text.to_owned(), left, cell, glyphs));
                    });
                }
            });
        }
        for (text, left, cell, glyphs) in &seen {
            assert!(
                (cell.width() - Row::LABEL_W).abs() < 0.5,
                "the cell for {text:?} is {:.1}pt wide, not the column's {:.1}pt — every \
                 row under it would start somewhere different",
                cell.width(),
                Row::LABEL_W,
            );
            assert!(
                (cell.left() - left).abs() < 0.5,
                "the cell for {text:?} starts at {:.1} and its row starts at {left:.1}",
                cell.left(),
            );
            assert!(
                (glyphs - cell.left()).abs() < 0.5,
                "the text {text:?} is painted {:.1}pt into its own column — the columns \
                 line up and the words do not, which is exactly what a centred galley in \
                 an exactly-sized cell looks like",
                glyphs - cell.left(),
            );
        }
    }

    /// The tab strip's width budget, as `tab_strip` computes it. A tab is now as wide
    /// as its filename, so the budget is the **cap** on that rather than the width
    /// every tab gets.
    fn strip_width(tabs: usize, available: f32, name: f32) -> f32 {
        let n = tabs.max(1) as f32;
        let per_tab = crate::TAB_PAD * 2.0 + crate::icons::BOX + crate::CLOSE_GAP + crate::GAP_TAB;
        let fixed = 6.0 + n * per_tab + crate::icons::BOX + crate::GAP_ICONS + crate::icons::BOX;
        let cap = ((available - fixed) / n).clamp(crate::NAME_MIN, crate::NAME_MAX);
        fixed + n * name.min(cap)
    }

    #[test]
    fn eight_tabs_fit_the_strip() {
        // MAX_TABS is 8 and the strip is one row, so at the cap the names have to
        // give way rather than the row running off the window — a `+` you cannot
        // reach is worse than a filename you have to hover to read in full.
        //
        // 900pt is a small laptop window; the cap has to work there, not only on
        // the display it was written on. The name is deliberately absurd, because the
        // cap is what has to hold, not the filenames anybody happens to have.
        let long = 10_000.0;
        let n = crate::tabs::MAX_TABS;
        assert!(
            strip_width(n, 900.0, long) <= 900.0,
            "eight tabs overflow a 900pt window"
        );
        assert!(strip_width(n, 1400.0, long) <= 1400.0);
    }

    #[test]
    fn a_short_name_does_not_stretch_its_tab() {
        // The complaint this answers: every tab used to be the same width, so one open
        // file sat in a 260pt tab with its name in the left third.
        let short = 70.0;
        let wide = strip_width(1, 1400.0, 10_000.0);
        let narrow = strip_width(1, 1400.0, short);
        assert!(narrow < wide, "a short name claimed the full cap");
        assert!(narrow < 200.0, "one tab took {narrow}pt for a 70pt name");
    }

    #[test]
    fn the_module_width_does_not_run_away() {
        // The bug this exists for, and it is worth stating because it looked like a
        // taste complaint: a module box is content + inner margin + **stroke** on
        // each side. Sizing the content to `available - 2 * PAD_X` left the two
        // stroke points out, so the box came back two points wider than the space it
        // was handed. In a resizable panel that closes a loop — the panel measures
        // its content, grows by two, offers two more next frame — and the develop
        // panel swallowed the whole window in a few seconds of mouse movement.
        //
        // Anything added to the frame that occupies width has to appear in that sum,
        // and this is what says so.
        let w = panel_widths(8);
        assert!(
            w.windows(2).all(|p| (p[0] - p[1]).abs() < 0.01),
            "the panel width is not settling — a module is claiming more width than \
             it was given: {w:?}"
        );
    }

    #[test]
    fn a_module_never_claims_more_width_than_the_panel_offers() {
        // The same invariant stated forwards rather than as a fixed point, so a
        // regression that happens to be stable at some *wrong* width still fails.
        let offered = panel_widths(4);
        let w = offered.last().copied().expect("four frames");
        assert!(
            w > 100.0 && w < 340.0,
            "the panel settled at an implausible {w}"
        );
    }
}
