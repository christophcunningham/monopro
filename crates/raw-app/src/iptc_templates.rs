//! Reusable IPTC field overlays, stored with the application rather than beside images.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use raw_core::sidecar::IptcField;
use serde::{Deserialize, Serialize};

const FILE: &str = "iptc-templates.toml";

/// A field omitted from a template is deliberately left unchanged. The two values
/// here are therefore the other two states: replace it, or explicitly clear it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Set { value: String },
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub fields: BTreeMap<String, Action>,
}

impl Template {
    pub fn from_values(
        name: String,
        values: &[String; IptcField::ALL.len()],
        mixed: &[bool; IptcField::ALL.len()],
    ) -> Self {
        let fields = IptcField::ALL
            .into_iter()
            .enumerate()
            // A capture date is this frame's alone; see `IptcField::per_image`.
            .filter(|(i, field)| !field.per_image() && !mixed[*i] && !values[*i].trim().is_empty())
            .map(|(i, field)| {
                (
                    field.key().to_owned(),
                    Action::Set {
                        value: values[i].trim().to_owned(),
                    },
                )
            })
            .collect();
        Self { name, fields }
    }

    pub fn action(&self, field: IptcField) -> Option<&Action> {
        self.fields.get(field.key())
    }

    pub fn set_action(&mut self, field: IptcField, action: Option<Action>) {
        match action {
            Some(action) => {
                self.fields.insert(field.key().to_owned(), action);
            }
            None => {
                self.fields.remove(field.key());
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub templates: Vec<Template>,
}

impl Store {
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = path() else {
            return (Self::default(), None);
        };
        match Self::load_from(&path) {
            Ok(store) => (store, None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(error) => (
                Self::default(),
                Some(format!("Metadata templates could not be read — {error}")),
            ),
        }
    }

    fn load_from(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path().ok_or_else(|| std::io::Error::other("no storage directory"))?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        raw_core::atomic_file::write(path, |file| file.write_all(text.as_bytes()))
    }

    pub fn unique_name(&self, requested: &str, except: Option<usize>) -> String {
        let base = requested.trim();
        let base = if base.is_empty() {
            "Metadata Template"
        } else {
            base
        };
        let available = |candidate: &str| {
            !self.templates.iter().enumerate().any(|(i, template)| {
                Some(i) != except && template.name.eq_ignore_ascii_case(candidate)
            })
        };
        if available(base) {
            return base.to_owned();
        }
        for suffix in 2.. {
            let candidate = format!("{base} {suffix}");
            if available(&candidate) {
                return candidate;
            }
        }
        unreachable!()
    }
}

fn path() -> Option<PathBuf> {
    crate::settings::dir().map(|dir| dir.join(FILE))
}

/// A Copyright Notice to start from, in the phrasing each is conventionally written
/// in — the Metadata pane's menu beside the field.
///
/// **They fill the field; they are not a separate setting.** The notice is free text
/// in IPTC and stays that way here: pick one, then edit it like anything else.
///
/// The Creative Commons ones carry the license's deed URL, which is how CC asks to be
/// cited when the notice cannot be a link — and embedded metadata cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    AllRightsReserved,
    /// A CC 4.0 license: its short code (`BY-NC`) and its name.
    CreativeCommons(&'static str, &'static str),
    /// CC0, which is a waiver and not a license, so it is worded as one.
    PublicDomain,
}

impl Notice {
    pub const ALL: [Self; 8] = [
        Self::AllRightsReserved,
        Self::CreativeCommons("BY", "Attribution"),
        Self::CreativeCommons("BY-SA", "Attribution-ShareAlike"),
        Self::CreativeCommons("BY-ND", "Attribution-NoDerivatives"),
        Self::CreativeCommons("BY-NC", "Attribution-NonCommercial"),
        Self::CreativeCommons("BY-NC-SA", "Attribution-NonCommercial-ShareAlike"),
        Self::CreativeCommons("BY-NC-ND", "Attribution-NonCommercial-NoDerivatives"),
        Self::PublicDomain,
    ];

    /// The menu's name for it.
    pub fn label(self) -> String {
        match self {
            Self::AllRightsReserved => "All rights reserved".to_owned(),
            Self::CreativeCommons(code, name) => format!("CC {code} 4.0 — {name}"),
            Self::PublicDomain => "CC0 1.0 — Public domain dedication".to_owned(),
        }
    }

    /// The notice for `owner`, first published in `year`. An empty owner leaves the
    /// sentence grammatical rather than leaving a gap to fill.
    pub fn text(self, year: &str, owner: &str) -> String {
        let owner = owner.trim();
        let by = if owner.is_empty() {
            String::new()
        } else {
            format!(" {owner}")
        };
        match self {
            Self::AllRightsReserved => format!("© {year}{by}. All rights reserved."),
            Self::CreativeCommons(code, _) => format!(
                "© {year}{by}. Licensed under CC {code} 4.0: https://creativecommons.org/licenses/{}/4.0/",
                code.to_ascii_lowercase()
            ),
            Self::PublicDomain if owner.is_empty() => "No rights reserved. Dedicated to the \
                 public domain under CC0 1.0: https://creativecommons.org/publicdomain/zero/1.0/"
                .to_owned(),
            Self::PublicDomain => format!(
                "No rights reserved. {owner} has dedicated this work to the public domain \
                 under CC0 1.0: https://creativecommons.org/publicdomain/zero/1.0/"
            ),
        }
    }
}

/// The year a notice claims: Date Created's, when it starts with one, since a
/// copyright runs from when the work was made — otherwise this year.
pub fn notice_year(date_created: &str) -> String {
    let year = date_created.trim().get(0..4).unwrap_or("");
    if year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()) {
        return year.to_owned();
    }
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        .div_euclid(86_400);
    crate::rename::civil_from_days(days).0.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "monopro-iptc-templates-{}-{}.toml",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn templates_round_trip_all_three_field_states() {
        let path = scratch();
        let mut template = Template {
            name: "News desk".into(),
            fields: BTreeMap::new(),
        };
        template.set_action(
            IptcField::Creator,
            Some(Action::Set {
                value: "A. Photographer".into(),
            }),
        );
        template.set_action(IptcField::Copyright, Some(Action::Clear));
        let store = Store {
            templates: vec![template],
        };

        store.save_to(&path).unwrap();
        let read = Store::load_from(&path).unwrap();
        assert_eq!(read, store);
        assert_eq!(
            read.templates[0].action(IptcField::Title),
            None,
            "an omitted field must remain unchanged"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saving_current_ignores_blank_and_mixed_fields() {
        let mut values: [String; IptcField::ALL.len()] = Default::default();
        let mut mixed = [false; IptcField::ALL.len()];
        values[IptcField::Creator as usize] = "Example Photographer".into();
        values[IptcField::Title as usize] = "   ".into();
        values[IptcField::City as usize] = "New York".into();
        mixed[IptcField::City as usize] = true;

        let template = Template::from_values("Basic".into(), &values, &mixed);
        assert!(matches!(
            template.action(IptcField::Creator),
            Some(Action::Set { value }) if value == "Example Photographer"
        ));
        assert_eq!(template.action(IptcField::Title), None);
        assert_eq!(template.action(IptcField::City), None);
    }

    #[test]
    fn a_capture_date_is_never_saved_into_a_template() {
        // Applied to a folder, it would stamp one frame's capture time on all of them.
        let mut values: [String; IptcField::ALL.len()] = Default::default();
        values[IptcField::DateCreated as usize] = "2026-03-08T14:22:05".into();
        values[IptcField::City as usize] = "New York".into();
        let template =
            Template::from_values("Shoot".into(), &values, &[false; IptcField::ALL.len()]);
        assert_eq!(template.action(IptcField::DateCreated), None);
        assert!(template.action(IptcField::City).is_some());
    }

    #[test]
    fn copyright_notices_are_phrased_as_they_are_conventionally_written() {
        assert_eq!(
            Notice::AllRightsReserved.text("2026", "Jane Doe"),
            "© 2026 Jane Doe. All rights reserved."
        );
        assert_eq!(
            Notice::AllRightsReserved.text("2026", "  "),
            "© 2026. All rights reserved.",
            "no owner is no gap"
        );
        assert_eq!(
            Notice::CreativeCommons("BY-NC-SA", "Attribution-NonCommercial-ShareAlike")
                .text("2025", "Jane Doe"),
            "© 2025 Jane Doe. Licensed under CC BY-NC-SA 4.0: https://creativecommons.org/licenses/by-nc-sa/4.0/"
        );
        assert!(Notice::PublicDomain.text("2026", "Jane Doe").starts_with(
            "No rights reserved. Jane Doe has dedicated this work to the public domain"
        ));
        assert_eq!(notice_year("2019-05-01T10:00:00"), "2019");
        let this_year = notice_year("");
        assert_eq!(this_year.len(), 4, "falls back to the current year");
        assert!(this_year.as_str() >= "2026");
    }

    #[test]
    fn duplicate_names_receive_a_stable_suffix() {
        let store = Store {
            templates: vec![
                Template {
                    name: "Copyright".into(),
                    fields: BTreeMap::new(),
                },
                Template {
                    name: "Copyright 2".into(),
                    fields: BTreeMap::new(),
                },
            ],
        };
        assert_eq!(store.unique_name("copyright", None), "copyright 3");
        assert_eq!(store.unique_name("Copyright", Some(0)), "Copyright");
    }
}
