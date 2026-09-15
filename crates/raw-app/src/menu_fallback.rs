//! Menu boundary for platforms where monopro has no attached native menu yet.

use crate::hotkeys::Action;
use crate::layout::Pane;

#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq)]
pub enum Command {
    Key(Action),
    Show(Pane),
    ShowLightbox(crate::lightbox::Pane),
}

#[derive(Default)]
pub struct Menus {
    _unavailable: (),
}

impl Menus {
    pub fn install(_app_name: &str) -> Self {
        Self::default()
    }

    pub fn claims(&self, _action: Action) -> bool {
        false
    }

    pub fn pressed() -> Vec<Command> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_without_an_attached_menu_keeps_every_chord_in_egui() {
        let menus = Menus::install("monopro");
        assert!(
            crate::hotkeys::TABLE
                .iter()
                .all(|binding| !menus.claims(binding.action))
        );
    }
}
