//! Native filesystem presentation kept out of the shared application UI.

use std::path::{Path, PathBuf};

pub fn main_viewport() -> egui::ViewportBuilder {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([900.0, 600.0])
        .with_title("monopro")
        .with_icon(application_icon());

    #[cfg(target_os = "macos")]
    return viewport
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);

    #[cfg(not(target_os = "macos"))]
    viewport.with_decorations(true)
}

fn application_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../../../icon.png"))
        .expect("the embedded application icon must be a valid image")
        .into_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

pub fn panel_viewport(title: &str, size: [f32; 2]) -> egui::ViewportBuilder {
    let viewport = egui::ViewportBuilder::default()
        .with_title(title)
        .with_inner_size(size);

    #[cfg(target_os = "macos")]
    return viewport.with_decorations(false);

    #[cfg(not(target_os = "macos"))]
    viewport.with_decorations(true)
}

pub fn settings_viewport() -> egui::ViewportBuilder {
    let viewport = egui::ViewportBuilder::default()
        .with_title("settings")
        .with_inner_size([760.0, 640.0])
        .with_min_inner_size([680.0, 520.0]);

    #[cfg(target_os = "macos")]
    return viewport
        .with_decorations(false)
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);

    #[cfg(not(target_os = "macos"))]
    viewport.with_decorations(true)
}

pub const fn strip_title(lightbox: bool) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        let _ = lightbox;
        "monopro"
    }

    #[cfg(not(target_os = "macos"))]
    if lightbox { "LIGHTBOX" } else { "DEVELOP" }
}

pub const fn draws_custom_window_chrome() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(not(windows))]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

#[cfg(windows)]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let tail = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(tail)).filter(|path| path.is_dir())
        })
}

/// Rebuildable application data belongs in the OS cache location, not beside the
/// preferences eframe keeps in its durable data directory.
#[cfg(target_os = "macos")]
pub fn cache_dir(app_id: &str) -> Option<PathBuf> {
    home_dir().map(|home| home.join("Library/Caches").join(app_component(app_id)))
}

#[cfg(windows)]
pub fn cache_dir(app_id: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join("AppData/Local")))
        .map(|root| root.join(app_component(app_id)).join("cache"))
}

#[cfg(target_os = "linux")]
pub fn cache_dir(app_id: &str) -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".cache")))
        .map(|root| root.join(app_component(app_id)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn cache_dir(_app_id: &str) -> Option<PathBuf> {
    None
}

fn app_component(app_id: &str) -> String {
    let component: String = app_id
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\' | ':') {
                '-'
            } else {
                ch
            }
        })
        .collect();
    let component = component.trim_matches(['.', '-', ' ']);
    if component.is_empty() {
        "monopro".to_owned()
    } else {
        component.to_owned()
    }
}

/// Mounted devices worth offering beside the configured local folder.
#[cfg(windows)]
pub fn external_roots() -> Vec<PathBuf> {
    let system = std::env::var_os("SystemDrive").map(PathBuf::from);
    let mut roots: Vec<PathBuf> = (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|path| {
            path.is_dir()
                && system
                    .as_deref()
                    .is_none_or(|system| !path.starts_with(system))
        })
        .collect();
    roots.sort_by_key(|path| root_name(path).to_lowercase());
    roots
}

#[cfg(not(windows))]
pub fn external_roots() -> Vec<PathBuf> {
    let containers = mount_containers();
    let mut roots: Vec<PathBuf> = containers
        .into_iter()
        .flat_map(|container| std::fs::read_dir(container).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
        })
        .collect();

    roots.sort_by_key(|path| root_name(path).to_lowercase());
    roots.dedup();
    roots
}

#[cfg(target_os = "macos")]
fn mount_containers() -> Vec<PathBuf> {
    vec![PathBuf::from("/Volumes")]
}

#[cfg(target_os = "linux")]
fn mount_containers() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/mnt")];
    if let Some(user) = home_dir().and_then(|path| path.file_name().map(ToOwned::to_owned)) {
        let user_roots = [
            PathBuf::from("/run/media").join(&user),
            PathBuf::from("/media").join(user),
        ];
        roots.extend(user_roots.iter().filter(|path| path.is_dir()).cloned());
        if roots.len() == 1 {
            roots.push(PathBuf::from("/media"));
        }
    } else {
        roots.push(PathBuf::from("/media"));
    }
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn mount_containers() -> Vec<PathBuf> {
    Vec::new()
}

pub fn root_name(path: &Path) -> String {
    if let Some(name) = path.file_name() {
        return name.to_string_lossy().into_owned();
    }
    #[cfg(target_os = "macos")]
    if path == Path::new("/") {
        return "Macintosh HD".to_owned();
    }
    path.display().to_string()
}

#[cfg(target_os = "macos")]
pub const fn file_manager_name() -> &'static str {
    "Finder"
}

#[cfg(windows)]
pub const fn file_manager_name() -> &'static str {
    "File Explorer"
}

#[cfg(not(any(target_os = "macos", windows)))]
pub const fn file_manager_name() -> &'static str {
    "File Manager"
}

#[cfg(target_os = "macos")]
pub fn reveal(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ())
}

#[cfg(windows)]
pub fn reveal(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map(|_| ())
    } else {
        let mut selected = std::ffi::OsString::from("/select,");
        selected.push(path.as_os_str());
        std::process::Command::new("explorer")
            .arg(selected)
            .spawn()
            .map(|_| ())
    }
}

#[cfg(target_os = "linux")]
pub fn reveal(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        })
        .spawn()
        .map(|_| ())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn reveal(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "revealing files is not supported on this platform",
    ))
}

#[cfg(target_os = "macos")]
pub fn font_dirs() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("/System/Library/Fonts/Supplemental"),
        PathBuf::from("/Library/Fonts"),
    ];
    if let Some(home) = home_dir() {
        roots.push(home.join("Library/Fonts"));
    }
    roots
}

#[cfg(windows)]
pub fn font_dirs() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(windows) = std::env::var_os("WINDIR") {
        roots.push(PathBuf::from(windows).join("Fonts"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Microsoft/Windows/Fonts"));
    }
    roots
}

#[cfg(target_os = "linux")]
pub fn font_dirs() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data).join("fonts"));
    } else if let Some(home) = home_dir() {
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join(".fonts"));
    }
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn font_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_native_root_has_a_name() {
        assert!(!root_name(Path::new(std::path::MAIN_SEPARATOR_STR)).is_empty());
    }

    #[test]
    fn the_file_manager_has_platform_copy() {
        assert!(!file_manager_name().is_empty());
    }

    #[test]
    fn font_discovery_keeps_the_platform_roots_separate_from_the_bundled_fallback() {
        assert!(font_dirs().iter().all(|path| path.is_absolute()));
    }

    #[test]
    fn the_cache_uses_one_safe_os_owned_directory_per_profile() {
        let default = cache_dir("monopro").expect("supported desktops have a cache root");
        let profile = cache_dir("monopro-scratch").expect("supported desktops have a cache root");
        assert!(default.is_absolute());
        assert!(
            default
                .components()
                .any(|part| part.as_os_str() == "monopro")
        );
        assert_ne!(default, profile);
        assert!(
            cache_dir("../").is_some_and(|path| {
                path.components().any(|part| part.as_os_str() == "monopro")
            })
        );
    }

    #[test]
    fn the_main_window_uses_the_platforms_shell() {
        let viewport = main_viewport();
        let icon = viewport
            .icon
            .expect("the main window must carry the app icon");
        assert_eq!((icon.width, icon.height), (1024, 1024));
        if cfg!(target_os = "macos") {
            assert_eq!(viewport.fullsize_content_view, Some(true));
            assert_eq!(viewport.titlebar_shown, Some(false));
        } else {
            assert_eq!(viewport.decorations, Some(true));
            assert_ne!(strip_title(false), "monopro");
        }
    }

    #[test]
    fn auxiliary_windows_keep_native_controls_off_mac() {
        let panel = panel_viewport("info", [340.0, 620.0]);
        let settings = settings_viewport();
        let expected = Some(!cfg!(target_os = "macos"));
        assert_eq!(panel.decorations, expected);
        assert_eq!(settings.decorations, expected);
    }
}
