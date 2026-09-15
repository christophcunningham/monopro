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
            .filter(|(i, _)| !mixed[*i] && !values[*i].trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "monopro-iptc-templates-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
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
