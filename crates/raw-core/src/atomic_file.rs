//! Publish a completed file without exposing a partial replacement.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The writer must finalize its encoder and flush any buffers before returning.
/// Failure or unwinding preserves the destination and removes the temporary file.
pub fn write(
    destination: &Path,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (temporary, mut file) = loop {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".monopro-write-{}-{serial}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (Temporary(path), file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    writer(&mut file)?;
    file.sync_all()?;
    drop(file);
    publish(&temporary.0, destination)
}

#[cfg(not(windows))]
fn publish(temporary: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(temporary, destination)
}

#[cfg(windows)]
fn publish(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    // Rename is enough for a first save. If another process creates the destination
    // between this check and the call, fall through to ReplaceFileW.
    if !destination.exists() {
        match std::fs::rename(temporary, destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }

    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are owned, NUL-terminated UTF-16 buffers for the duration of
    // the call. The replacement is in the destination's directory, hence same-volume.
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            temporary.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn failed_and_panicking_writes_preserve_the_previous_file() {
        let dir = std::env::temp_dir().join(format!("monopro-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("output");
        std::fs::write(&path, b"original").unwrap();
        assert!(
            write(&path, |file| {
                file.write_all(b"partial")?;
                Err(io::Error::other("injected finalization failure"))
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert!(
            std::panic::catch_unwind(|| write(&path, |file| {
                file.write_all(b"partial")?;
                panic!("injected encoder panic");
            }))
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        write(&path, |file| file.write_all(b"complete")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
