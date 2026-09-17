//! Reusable individual curves, stored with the application rather than an image.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "curve-presets.toml";

/// One selected Curve instance. Loading creates a new instance rather than replacing
/// anything already on the image. Preview/bypass state is deliberately not stored:
/// choosing Load is an affirmative request to see the curve, so the new instance is
/// always enabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default = "full_opacity")]
    pub opacity: f32,
    pub points: Vec<[f32; 2]>,
}

fn full_opacity() -> f32 {
    1.0
}

impl Preset {
    pub fn from_instance(name: String, instance: &raw_core::CurveInstance) -> Self {
        Self {
            name,
            opacity: instance.opacity,
            points: instance.curve.points().to_vec(),
        }
    }

    /// Rebuild through `Curve::from_points`, so a hand-edited preset file cannot
    /// inject an invalid curve into interpolation or the GPU LUT.
    pub fn instance(&self) -> Option<raw_core::CurveInstance> {
        if !self.opacity.is_finite() || self.points.iter().flatten().any(|value| !value.is_finite())
        {
            return None;
        }
        let mut curve = raw_core::Curve::from_points(&self.points)?;
        curve.enabled = true;
        Some(raw_core::CurveInstance {
            name: self.name.trim().to_owned(),
            curve,
            opacity: self.opacity.clamp(0.0, 1.0),
        })
    }

    /// Add this preset after every curve already on the image. The preset name becomes
    /// the instance name; a suffix keeps the list unambiguous when it is loaded twice.
    pub fn append_to(&self, stack: &mut raw_core::CurveStack) -> Result<usize, AppendError> {
        if stack.instances.len() >= raw_core::CurveStack::MAX_INSTANCES {
            return Err(AppendError::Full);
        }
        let mut instance = self.instance().ok_or(AppendError::Invalid)?;
        let base = if instance.name.trim().is_empty() {
            "Curve Preset".to_owned()
        } else {
            instance.name.trim().to_owned()
        };
        let available = |candidate: &str| {
            stack
                .instances
                .iter()
                .all(|existing| !existing.name.eq_ignore_ascii_case(candidate))
        };
        instance.name = if available(&base) {
            base.clone()
        } else {
            (2..)
                .map(|suffix| format!("{base} {suffix}"))
                .find(|candidate| available(candidate))
                .expect("an unbounded suffix always has a free name")
        };
        stack.instances.push(instance);
        stack.enabled = true;
        Ok(stack.instances.len() - 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendError {
    Invalid,
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub presets: Vec<Preset>,
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
                Some(format!("Curve presets could not be read — {error}")),
            ),
        }
    }

    fn load_from(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        match toml::from_str::<Self>(&text) {
            Ok(store) => Ok(store),
            Err(current_error) => toml::from_str::<LegacyStore>(&text)
                .map(Self::from_legacy)
                .map_err(|_| std::io::Error::other(current_error)),
        }
    }

    /// The first Curve-preset build stored an entire stack. Preserve those files by
    /// splitting every old stack into individual presets on read. A multi-instance
    /// preset carries both names so no curve becomes ambiguous after the split.
    fn from_legacy(legacy: LegacyStore) -> Self {
        let mut store = Self::default();
        for preset in legacy.presets {
            let many = preset.instances.len() > 1;
            for instance in preset.instances {
                let requested = if many && !instance.name.trim().is_empty() {
                    format!("{} — {}", preset.name, instance.name.trim())
                } else {
                    preset.name.clone()
                };
                let name = store.unique_name(&requested);
                store.presets.push(Preset {
                    name,
                    opacity: instance.opacity,
                    points: instance.points,
                });
            }
        }
        store
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

    pub fn unique_name(&self, requested: &str) -> String {
        let base = requested.trim();
        let base = if base.is_empty() {
            "Curve Preset"
        } else {
            base
        };
        let available = |candidate: &str| {
            !self
                .presets
                .iter()
                .any(|preset| preset.name.eq_ignore_ascii_case(candidate))
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

/// Read-only compatibility with the stack format shipped immediately before the
/// individual-preset design. Unknown `enabled` fields are harmlessly ignored.
#[derive(Deserialize)]
struct LegacyStore {
    #[serde(default)]
    presets: Vec<LegacyPreset>,
}

#[derive(Deserialize)]
struct LegacyPreset {
    name: String,
    #[serde(default)]
    instances: Vec<LegacyInstance>,
}

#[derive(Deserialize)]
struct LegacyInstance {
    #[serde(default)]
    name: String,
    #[serde(default = "full_opacity")]
    opacity: f32,
    points: Vec<[f32; 2]>,
}

fn path() -> Option<PathBuf> {
    crate::settings::dir().map(|dir| dir.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "monopro-curve-presets-{}-{}.toml",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn an_individual_curve_round_trips_and_loads_enabled() {
        let path = scratch();
        let mut instance = raw_core::CurveInstance::new("Highlights".into());
        instance.curve.add(0.8, 0.72);
        instance.curve.enabled = false;
        instance.opacity = 0.4;
        let preset = Preset::from_instance("Portrait paper".into(), &instance);
        let store = Store {
            presets: vec![preset],
        };

        store.save_to(&path).unwrap();
        let read = Store::load_from(&path).unwrap();
        assert_eq!(read, store);
        let restored = read.presets[0].instance().unwrap();
        assert!(
            restored.curve.enabled,
            "loading a preset must arm its instance"
        );
        assert_eq!(restored.opacity, instance.opacity);
        assert_eq!(restored.curve.points(), instance.curve.points());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_invalid_curve_is_refused() {
        let invalid = Preset {
            name: "Invalid".into(),
            opacity: f32::NAN,
            points: vec![[0.0, 0.0], [1.0, 1.0]],
        };
        assert!(invalid.instance().is_none());
    }

    #[test]
    fn loading_appends_after_the_existing_curve_and_never_replaces_it() {
        let mut stack = raw_core::CurveStack::default();
        stack.instances[0].curve.add(0.25, 0.3);
        let original = stack.instances[0].clone();
        let mut saved = raw_core::CurveInstance::new("Curve 1".into());
        saved.curve.add(0.8, 0.72);
        let preset = Preset::from_instance("Curve 1".into(), &saved);

        let added = preset.append_to(&mut stack).unwrap();
        assert_eq!(added, 1);
        assert_eq!(stack.instances[0], original);
        assert_eq!(stack.instances[1].name, "Curve 1 2");
        assert_eq!(stack.instances[1].curve.points(), saved.curve.points());
    }

    #[test]
    fn the_previous_stack_format_is_split_without_losing_a_curve() {
        let path = scratch();
        let old = r#"
[[presets]]
name = "Portrait paper"

[[presets.instances]]
name = "Shadows"
enabled = true
opacity = 0.8
points = [[0.0, 0.0], [0.25, 0.3], [1.0, 1.0]]

[[presets.instances]]
name = "Highlights"
enabled = false
opacity = 0.4
points = [[0.0, 0.0], [0.8, 0.72], [1.0, 1.0]]
"#;
        std::fs::write(&path, old).unwrap();
        let migrated = Store::load_from(&path).unwrap();
        assert_eq!(migrated.presets.len(), 2);
        assert_eq!(migrated.presets[0].name, "Portrait paper — Shadows");
        assert_eq!(migrated.presets[1].name, "Portrait paper — Highlights");
        assert!(
            migrated
                .presets
                .iter()
                .all(|preset| preset.instance().is_some())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn duplicate_names_receive_a_stable_suffix() {
        let instance = raw_core::CurveInstance::new("Curve 1".into());
        let store = Store {
            presets: vec![
                Preset::from_instance("Portrait".into(), &instance),
                Preset::from_instance("Portrait 2".into(), &instance),
            ],
        };
        assert_eq!(store.unique_name("portrait"), "portrait 3");
    }
}
