//! Native dialogs and the path validation shared by dialogs, drops and CLI opens.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Folder(PathBuf),
    Raw(PathBuf),
}

/// Classify before mutating tabs or remembered folders. A cancelled dialog never calls
/// this; an unavailable drop or stale command-line path gets one clear status instead
/// of an empty tab and a background decode failure.
pub fn classify_open(path: PathBuf) -> Result<OpenTarget, String> {
    let metadata = std::fs::metadata(&path)
        .map_err(|error| format!("could not open {} — {error}", path.display()))?;
    if metadata.is_dir() {
        return Ok(OpenTarget::Folder(path));
    }
    if !metadata.is_file() {
        return Err(format!(
            "could not open {} — not a regular file",
            path.display()
        ));
    }
    if !crate::lightbox::is_raw(&path) {
        return Err(format!(
            "could not open {} — monopro develops supported RAW files",
            path.display()
        ));
    }
    std::fs::File::open(&path)
        .map_err(|error| format!("could not open {} — {error}", path.display()))?;
    Ok(OpenTarget::Raw(path))
}

fn start_directory(preferred: Option<&Path>) -> PathBuf {
    preferred
        .filter(|path| path.is_dir())
        .map(Path::to_path_buf)
        .or_else(crate::platform::home_dir)
        .or_else(|| std::env::current_dir().ok().filter(|path| path.is_dir()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn pick_raw(preferred: Option<&Path>) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open RAW Photograph")
        .set_directory(start_directory(preferred))
        .add_filter("RAW photographs", crate::lightbox::RAW_EXTENSIONS)
        .pick_file()
}

pub fn pick_folder(preferred: Option<&Path>) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_directory(start_directory(preferred))
        .pick_folder()
}

pub fn save_file(
    title: &str,
    file_name: &str,
    preferred: Option<&Path>,
    filter: impl Into<String>,
    extension: &str,
) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title(title)
        .set_file_name(file_name)
        .set_directory(start_directory(preferred))
        .add_filter(filter, &[extension])
        .save_file()
        .map(|path| force_extension(path, extension))
}

fn force_extension(mut path: PathBuf, extension: &str) -> PathBuf {
    if !path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
    {
        path.set_extension(extension);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_intake_distinguishes_folders_raws_missing_and_ordinary_images() {
        let root = crate::settings::dir().unwrap().join("dialog-intake");
        std::fs::create_dir_all(&root).unwrap();
        let raw = root.join("frame.DNG");
        let jpeg = root.join("proof.jpg");
        std::fs::write(&raw, b"not decoded by this policy test").unwrap();
        std::fs::write(&jpeg, b"not decoded by this policy test").unwrap();

        assert_eq!(classify_open(root.clone()), Ok(OpenTarget::Folder(root)));
        assert_eq!(classify_open(raw.clone()), Ok(OpenTarget::Raw(raw)));
        assert!(classify_open(jpeg).unwrap_err().contains("supported RAW"));
        assert!(
            classify_open(PathBuf::from("missing.dng"))
                .unwrap_err()
                .contains("could not open")
        );
    }

    #[test]
    fn save_dialog_results_keep_the_selected_formats_extension() {
        assert_eq!(
            force_extension(PathBuf::from("print"), "tif"),
            PathBuf::from("print.tif")
        );
        assert_eq!(
            force_extension(PathBuf::from("print.TIF"), "tif"),
            PathBuf::from("print.TIF")
        );
        assert_eq!(
            force_extension(PathBuf::from("print.jpg"), "tif"),
            PathBuf::from("print.tif")
        );
    }

    #[test]
    fn a_stale_dialog_directory_falls_back_to_an_existing_location() {
        let root = crate::settings::dir().unwrap();
        let missing = root.join("not-mounted");
        let selected = start_directory(Some(&missing));
        assert!(selected.is_dir());
    }
}
