//! The native menu bar, built from [`crate::hotkeys::TABLE`].
//!
//! # Generated, not written
//!
//! Every item names a `hotkeys::Action` and takes its label, its chord and its
//! *enabled* state from the same table the keyboard reads. That is the whole design,
//! and it buys three things a hand-written menu would have to keep buying:
//!
//! - **The menu and the keys cannot drift.** A chord changed in one place changes in
//!   both, because there is only one place.
//! - **An unbuilt command appears greyed rather than absent**, which is the rule the
//!   table already enforces for the "not built yet" note. A menu that silently omitted
//!   the features that do not exist yet would be a menu that lies about the app.
//! - **The accelerator is the binding**, so there is no second transcription of `⌘⇧Z`
//!   to get wrong.
//!
//! # The accelerator problem, which is the whole reason this file is careful
//!
//! **A macOS menu item with an accelerator owns that chord.** AppKit routes the key to
//! the menu and the window never sees it — so the moment `⌘Z` appears in a menu,
//! `hotkeys::pressed` stops receiving it. Left alone, that is a menu bar that silently
//! breaks undo.
//!
//! So the ownership is made explicit: [`Menus::claims`] reports which actions the menu
//! took, the frame filters those out of the keyboard path, and the menu's own events
//! dispatch the identical `Action`. **One command, one route.** If the menu fails to
//! install — another platform, or muda refusing — nothing is claimed and the keyboard
//! path is exactly what it was, which is why the claim is recorded from what was
//! actually built rather than assumed from the table.
//!
//! # Why muda and not egui
//!
//! egui has a menu bar and it draws *inside the window*. This app already has its own
//! title strip, and an in-window menu under a custom title strip is neither a macOS
//! menu bar nor a good imitation of one. muda talks to `NSApp` directly, so it needs no
//! eframe API and no access to the winit event loop — it is installed on the first
//! frame, by which time `NSApp` exists.

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::hotkeys::{self, Action, Mods};
use crate::layout::Pane;

/// What a menu click asks for.
///
/// Two variants because two different things belong in a menu bar: the commands the
/// keyboard already has, and the panes, which have no chord of their own and today can
/// only be brought back with `tab` — all of them at once.
#[derive(Clone, Copy, PartialEq)]
pub enum Command {
    Key(Action),
    /// Show this pane, bringing it forward if it is behind a tab.
    Show(Pane),
    /// Show a Lightbox pane, and switch to Lightbox if it is not up.
    ShowLightbox(crate::lightbox::Pane),
    /// The manual update check (macOS). Menu-only: it has no chord and so no
    /// row in `hotkeys::TABLE`, whose bindings all carry a key.
    CheckForUpdates,
}

/// The installed menu bar, and what it took.
pub struct Menus {
    /// Kept alive: dropping the `Menu` takes the bar off the screen with it.
    _bar: Option<Menu>,
    /// Actions whose chord the OS now owns. Read by the frame to suppress the
    /// keyboard path for exactly those and no others.
    claimed: Vec<Action>,
}

impl Default for Menus {
    /// No menu bar. The app runs unchanged and every chord stays with the keyboard,
    /// which is what makes installing the menu bar optional rather than load-bearing.
    fn default() -> Self {
        Self {
            _bar: None,
            claimed: Vec::new(),
        }
    }
}

/// `hotkeys::Mods` as muda sees them.
fn modifiers(m: Mods) -> Option<Modifiers> {
    match m {
        Mods::None => None,
        // `SUPER` is Command on macOS. Named for the platform-neutral key so the same
        // table would produce Ctrl-accelerated menus on Windows without a second list.
        Mods::Cmd => Some(Modifiers::SUPER),
        Mods::Shift => Some(Modifiers::SHIFT),
        Mods::CmdShift => Some(Modifiers::SUPER | Modifiers::SHIFT),
        Mods::Alt => Some(Modifiers::ALT),
        // `CONTROL` is genuinely Control on macOS, where `SUPER` is Command. The two
        // are separate keys here, unlike on Windows — see `hotkeys::Mods::Ctrl`.
        Mods::Ctrl => Some(Modifiers::CONTROL),
        Mods::CtrlShift => Some(Modifiers::CONTROL | Modifiers::SHIFT),
    }
}

/// `egui::Key` as a physical `Code`.
///
/// **Complete for every key `TABLE` uses**, which it was not at first — and the gap was
/// visible rather than theoretical. `Equals`, `Minus` and `Backtick` were missing, so
/// `⌘+`, `⌘−` and `⌘\`` fell back to having their chord written into the label while
/// `⌘0` beside them got the proper right-aligned accelerator. the maintainer saw two shortcuts
/// formatted two ways in one menu and reasonably asked why.
///
/// `a_menu_can_spell_every_chord_the_table_binds` is what stops it happening again.
/// Anything genuinely unmappable still degrades safely — the item appears, works when
/// clicked, and keeps its key — but it should be a decision rather than an oversight.
fn code(key: egui::Key) -> Option<Code> {
    use egui::Key as K;
    Some(match key {
        K::B => Code::KeyB,
        K::C => Code::KeyC,
        K::D => Code::KeyD,
        K::E => Code::KeyE,
        K::I => Code::KeyI,
        K::J => Code::KeyJ,
        K::K => Code::KeyK,
        K::L => Code::KeyL,
        K::O => Code::KeyO,
        K::P => Code::KeyP,
        K::Q => Code::KeyQ,
        K::R => Code::KeyR,
        K::S => Code::KeyS,
        K::U => Code::KeyU,
        K::V => Code::KeyV,
        K::W => Code::KeyW,
        K::X => Code::KeyX,
        K::Y => Code::KeyY,
        K::Z => Code::KeyZ,
        K::Num0 => Code::Digit0,
        K::Num1 => Code::Digit1,
        K::Num2 => Code::Digit2,
        K::Num3 => Code::Digit3,
        K::Num4 => Code::Digit4,
        K::Num5 => Code::Digit5,
        K::Num6 => Code::Digit6,
        K::Comma => Code::Comma,
        K::Period => Code::Period,
        K::Minus => Code::Minus,
        // Both spell the same physical key. `Plus` is the shifted form and the table
        // binds it separately so `⌘⇧+` and `⌘+` can both reach zoom-in.
        K::Equals | K::Plus => Code::Equal,
        K::Backtick => Code::Backquote,
        K::OpenBracket => Code::BracketLeft,
        K::CloseBracket => Code::BracketRight,
        K::Tab => Code::Tab,
        K::Escape => Code::Escape,
        K::Enter => Code::Enter,
        _ => return None,
    })
}

/// A menu id that survives the round trip through the OS.
///
/// The **index into `TABLE`** rather than the action's name: `Action` carries payloads
/// (`Rating(u8)`, `CompareUp(u8)`) and an id built from a name alone could not tell
/// `2` from `3`. The index is exact, and it is stable for the life of a build — which
/// is the whole life of a menu.
fn id_for(i: usize) -> MenuId {
    MenuId::new(format!("k{i}"))
}

fn pane_id(p: Pane) -> MenuId {
    MenuId::new(format!("p{}", p.label()))
}

/// The Lightbox's canvas is historically named `GRID` internally and in its panel
/// tab. In the macOS Window menu, however, the destination is the app mode rather
/// than the implementation of its canvas, so call it what the rest of the UI calls
/// it. Keep the menu id based on `Pane::label()` so changing this display copy cannot
/// break event dispatch.
fn lightbox_window_label(p: crate::lightbox::Pane) -> &'static str {
    match p {
        crate::lightbox::Pane::Grid => "LIGHTBOX",
        _ => p.label(),
    }
}

/// Build one item from its binding, and report whether it took the chord.
fn item(i: usize, claimed: &mut Vec<Action>) -> MenuItem {
    let b = &hotkeys::TABLE[i];
    // **A menu accelerator must carry a modifier**, and this line is the whole of a bug
    // worth not repeating. `k`, `b`, `z` and `tab` are bare keys; built as accelerators
    // they became modifier-less menu shortcuts, `claims` then took them off the
    // keyboard, and the compare viewer simply stopped opening. macOS does not want
    // them either — a menu shortcut with no modifier would fire while you are doing
    // anything at all.
    //
    // So a bare or Shift-only binding gets **no accelerator and no claim**: the item
    // still appears in the menu, still works when clicked, and its key stays exactly
    // where it was.
    //
    // **`CONTROL` is deliberately not in this list, and the omission is the safe
    // side.** `⌃Tab` could be a menu accelerator — AppKit permits Control key
    // equivalents — but claiming a chord takes it *off* the keyboard, and the whole
    // reason that binding exists is that `⌘\`` was claimed by something that then did
    // not deliver it. Leaving Control unclaimed means the keyboard path is the one that
    // runs, which is the path egui has already been shown to receive: `raw_input_hook`
    // steals only the bare key and egui's focus navigation ignores Tab once Control is
    // down. The menu item still lists `⌃tab` in its label and still works when clicked.
    // Promote it only after watching a Control accelerator actually fire.
    let accel = modifiers(b.mods)
        .filter(|m| m.contains(Modifiers::SUPER) || m.contains(Modifiers::ALT))
        .and_then(|m| code(b.key).map(|c| Accelerator::new(Some(m), c)));
    if accel.is_some() && b.built {
        // Only a *built* command's chord is claimed. An unbuilt item is greyed, so
        // AppKit will not route to it — and taking the chord away from the keyboard on
        // its behalf would disable a key that today at least says it is pending.
        claimed.push(b.action);
    }
    // A bare binding cannot occupy AppKit's right-aligned shortcut column without
    // becoming a native key equivalent and stealing that key from egui. Do not fake
    // the column by appending the chord to the title: proportional menu text makes the
    // result ragged and look broken. Bare keys remain functional and are listed in the
    // Hotkey HUD, while the native menu stays typographically native.
    MenuItem::with_id(id_for(i), b.what, b.built, accel)
}

/// Every binding for `action`, as an index. Actions with payloads have several.
fn index_of(action: Action) -> Option<usize> {
    hotkeys::TABLE.iter().position(|b| b.action == action)
}

impl Menus {
    /// Build the bar and hand it to `NSApp`.
    ///
    /// Called once, on the first frame, because that is the earliest point at which
    /// `NSApp` exists — eframe creates it during startup and there is no callback that
    /// says so.
    pub fn install(app_name: &str) -> Self {
        let mut claimed = Vec::new();
        let bar = Menu::new();

        // ── The application menu ────────────────────────────────────────────────
        //
        // macOS puts the app's name here whatever it contains, so it exists whether or
        // not we want it. Settings belongs in it by platform convention rather than in
        // File, which is where a Windows app would put it.
        let app = Submenu::new(app_name, true);
        let _ = app.append(&PredefinedMenuItem::about(None, None));
        let _ = app.append(&PredefinedMenuItem::separator());
        if let Some(i) = index_of(Action::Settings) {
            let _ = app.append(&item(i, &mut claimed));
        }
        // The manual update check, under Settings by the same platform convention.
        // A fixed id, not a `k` index — this command has no binding row to point at.
        #[cfg(target_os = "macos")]
        let _ = app.append(&MenuItem::with_id(
            MenuId::new("updates-check"),
            "Check for Updates…",
            true,
            None,
        ));
        let _ = app.append(&PredefinedMenuItem::separator());
        let _ = app.append(&PredefinedMenuItem::services(None));
        let _ = app.append(&PredefinedMenuItem::hide(None));
        let _ = app.append(&PredefinedMenuItem::separator());
        let _ = app.append(&PredefinedMenuItem::quit(None));
        let _ = bar.append(&app);

        // ── The four menus, from the table ──────────────────────────────────────
        //
        // Listed rather than grouped by `hotkeys::Group`, because that grouping is for
        // the reference sheet: it has a VALUE PINS heading and a DODGE & BURN one, and
        // neither is a menu anybody would pull down. The mapping from one to the other
        // is a judgement, so it is written out.
        let menus: [(&str, &[Action]); 3] = [
            (
                "File",
                &[
                    Action::OpenFile,
                    Action::SaveDuplicate,
                    Action::Export,
                    Action::ExportProof,
                    Action::CloseTab,
                ],
            ),
            (
                "Edit",
                &[Action::Undo, Action::Redo, Action::CaptureSnapshot],
            ),
            (
                "View",
                &[
                    Action::ZoomFit,
                    Action::ZoomToggle,
                    Action::ZoomIn,
                    Action::ZoomOut,
                    Action::CyclePreviewSource,
                    Action::Surround,
                    Action::CompareViewer,
                    Action::TogglePanels,
                ],
            ),
        ];
        for (name, actions) in menus {
            let sub = Submenu::new(name, true);
            for a in actions {
                if let Some(i) = index_of(*a) {
                    let _ = sub.append(&item(i, &mut claimed));
                }
            }
            let _ = bar.append(&sub);
        }

        // ── Window ──────────────────────────────────────────────────────────────
        //
        // **The menu that earns its place immediately.** Five panes can be closed and
        // popped out, and until now the only way back was `tab`, which brings them all
        // at once. Each entry shows *its* panel and brings it forward if it is behind
        // another tab — the same call a mode makes when it opens onto a hidden panel.
        //
        // **Both modes' panes are here**, Lightbox's first because it is the mode the
        // app opens in. the maintainer found the Lightbox panels missing from this menu, which
        // mattered more once a pane could be dragged somewhere it stopped being
        // visible — the menu is the way back.
        let window = Submenu::new("Window", true);
        for pane in crate::lightbox::Pane::ALL {
            let _ = window.append(&MenuItem::with_id(
                format!("l{}", pane.label()),
                lightbox_window_label(pane),
                true,
                None,
            ));
        }
        let _ = window.append(&PredefinedMenuItem::separator());
        for pane in Pane::PANELS {
            let _ = window.append(&MenuItem::with_id(pane_id(pane), pane.label(), true, None));
        }
        let _ = window.append(&PredefinedMenuItem::separator());
        for a in [Action::CycleTabs, Action::FlickTab] {
            if let Some(i) = index_of(a) {
                let _ = window.append(&item(i, &mut claimed));
            }
        }
        let _ = window.append(&PredefinedMenuItem::separator());
        let _ = window.append(&PredefinedMenuItem::minimize(None));
        let _ = bar.append(&window);

        #[cfg(target_os = "macos")]
        bar.init_for_nsapp();

        Self {
            _bar: Some(bar),
            claimed,
        }
    }

    /// Whether the OS menu owns this action's chord, so the keyboard must not also
    /// fire it.
    ///
    /// Belt and braces: on macOS AppKit consumes a claimed chord before egui sees it,
    /// so in practice the keyboard path never produces one. It is filtered anyway
    /// because "in practice" is doing a lot of work in that sentence — a chord that
    /// slipped through would fire its command **twice**, and a doubled undo is two
    /// steps of work lost with nothing on screen to say why.
    pub fn claims(&self, action: Action) -> bool {
        self.claimed.contains(&action)
    }

    /// Drain whatever was clicked since the last frame.
    ///
    /// A channel rather than a callback, so menu clicks arrive on the same frame
    /// boundary as keypresses and take the identical path through the dispatch. muda's
    /// receiver is global, which is why this is a free-standing drain and not something
    /// the `Menus` value owns.
    pub fn pressed() -> Vec<Command> {
        let mut out = Vec::new();
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            let id = ev.id.0.as_str();
            if let Some(rest) = id.strip_prefix('k')
                && let Ok(i) = rest.parse::<usize>()
                && let Some(b) = hotkeys::TABLE.get(i)
            {
                out.push(Command::Key(b.action));
            } else if let Some(name) = id.strip_prefix('p')
                && let Some(p) = Pane::PANELS.into_iter().find(|p| p.label() == name)
            {
                out.push(Command::Show(p));
            } else if let Some(name) = id.strip_prefix('l')
                && let Some(p) = crate::lightbox::Pane::ALL
                    .into_iter()
                    .find(|p| p.label() == name)
            {
                out.push(Command::ShowLightbox(p));
            } else if id == "updates-check" {
                out.push(Command::CheckForUpdates);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_menu_calls_the_grid_lightbox() {
        assert_eq!(
            lightbox_window_label(crate::lightbox::Pane::Grid),
            "LIGHTBOX"
        );
        assert_eq!(
            lightbox_window_label(crate::lightbox::Pane::Folders),
            "FOLDERS"
        );
    }

    #[test]
    fn every_menu_action_is_a_binding_the_table_really_has() {
        // The menu is generated from `TABLE` by *looking actions up*, and a lookup that
        // misses produces no item and no error — so a renamed or retired action would
        // quietly leave a hole in a menu rather than fail anything. This is what says
        // so. It is the whole risk of building a menu from a table by name.
        let listed = [
            Action::Settings,
            Action::OpenFile,
            Action::SaveDuplicate,
            Action::Export,
            Action::ExportProof,
            Action::CloseTab,
            Action::Undo,
            Action::Redo,
            Action::CaptureSnapshot,
            Action::ZoomFit,
            Action::ZoomToggle,
            Action::ZoomIn,
            Action::ZoomOut,
            Action::CyclePreviewSource,
            Action::Surround,
            Action::CompareViewer,
            Action::TogglePanels,
            Action::CycleTabs,
            Action::FlickTab,
        ];
        for a in listed {
            assert!(index_of(a).is_some(), "{a:?} is in a menu and not in TABLE");
        }
    }

    #[test]
    fn a_menu_id_survives_the_round_trip() {
        // Ids go through the OS as strings and come back as strings. The index is used
        // rather than a name because `Action` has payloads — this asserts that the two
        // ends agree, including for the payload-carrying ones.
        for (i, b) in hotkeys::TABLE.iter().enumerate() {
            let id = id_for(i);
            let back =
                id.0.strip_prefix('k')
                    .and_then(|r| r.parse::<usize>().ok())
                    .and_then(|i| hotkeys::TABLE.get(i))
                    .map(|b| b.action);
            assert_eq!(back, Some(b.action), "binding {i} did not survive its id");
        }
    }

    #[test]
    fn a_pane_id_survives_the_round_trip() {
        // Panes are keyed by their label, which is also what the menu item says. A
        // label changed for display reasons would break the click silently.
        for p in Pane::PANELS {
            let id = pane_id(p);
            let back =
                id.0.strip_prefix('p')
                    .and_then(|n| Pane::PANELS.into_iter().find(|q| q.label() == n));
            assert_eq!(back, Some(p), "{p:?} did not survive its id");
        }
    }

    #[test]
    fn a_menu_can_spell_every_chord_the_table_binds() {
        // The gap the maintainer spotted: `Equals`, `Minus` and `Backtick` had no mapping, so
        // `⌘+`, `⌘−` and `⌘`` wrote their chord into the label while `⌘0` beside them
        // got a proper right-aligned accelerator — two formats in one menu, for no
        // reason anybody could see.
        //
        // Checked over the whole table rather than over the menus, because the next
        // action added to a menu should be right by construction. A key that genuinely
        // cannot be spelled is allowed; it just has to be a decision made here.
        for b in hotkeys::TABLE {
            assert!(
                code(b.key).is_some(),
                "{:?} has no Code, so {} could only ever be written into a label",
                b.key,
                b.chord()
            );
        }
    }

    #[test]
    fn a_bare_key_keeps_its_chord() {
        // The defect this is written against: `k` opens the compare viewer, it is a
        // bare key, and putting it in a menu turned it into a modifier-less accelerator
        // that `claims` then removed from the keyboard — so the feature stopped
        // responding entirely. Every bare and Shift-only binding is checked, not just
        // the ones in a menu today, because the next one added to a menu must be safe
        // by construction rather than by somebody remembering.
        let mut claimed = Vec::new();
        for (i, b) in hotkeys::TABLE.iter().enumerate() {
            if matches!(b.mods, Mods::None | Mods::Shift) {
                let _ = item(i, &mut claimed);
            }
        }
        assert!(
            claimed.is_empty(),
            "a menu took an unmodified key off the keyboard: {claimed:?}"
        );
    }

    #[test]
    fn a_command_key_does_claim_its_chord() {
        // The mirror, so the test above cannot pass by the menu claiming nothing at
        // all. `⌘Z` must be claimed — that is the case the whole ownership scheme
        // exists for.
        let mut claimed = Vec::new();
        let i = index_of(Action::Undo).expect("undo is bound");
        let _ = item(i, &mut claimed);
        assert_eq!(claimed, vec![Action::Undo], "the menu did not take ⌘Z");
    }

    #[test]
    fn nothing_unbuilt_takes_a_chord_away_from_the_keyboard() {
        // The rule that keeps a greyed menu item from disabling a working key. An
        // unbuilt command is greyed, so AppKit will not route to it — claiming its
        // chord anyway would make the key do *nothing at all* rather than say it is
        // pending, which is strictly worse than before the menu existed.
        let mut claimed = Vec::new();
        for (i, b) in hotkeys::TABLE.iter().enumerate() {
            if !b.built {
                let _ = item(i, &mut claimed);
            }
        }
        assert!(
            claimed.is_empty(),
            "an unbuilt command claimed a chord: {claimed:?}"
        );
    }
}
