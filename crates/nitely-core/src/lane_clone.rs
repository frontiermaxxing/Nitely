//! Optional copy-on-write cache seeding. Never hard-link files or fall back to a full byte copy.
use anyhow::{bail, Context, Result};
use std::path::{Component, Path};
use std::time::{Duration, Instant};

/// Populate a new cache directory. Partial/unsupported clones are removed; its caller stays cold.
pub fn seed(source: &Path, lane: &Path, relative: &str) -> Result<bool> {
    seed_with(source, lane, relative, &clone_file)
}

fn seed_with(source: &Path, lane: &Path, relative: &str, clone: &dyn Fn(&Path, &Path) -> Result<()>) -> Result<bool> {
    if relative.is_empty() || Path::new(relative).components().any(|c| !matches!(c, Component::Normal(_)))
        || Path::new(relative).components().any(|c| c.as_os_str() == ".git") {
        bail!("unsafe lane cache path");
    }
    // No source or destination ancestor may redirect the clone outside these owned lanes.
    for root in [source, lane] {
        let mut at = root.to_path_buf();
        for part in Path::new(relative).components() {
            at.push(part);
            if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) { return Ok(false); }
        }
    }
    let from = source.join(relative);
    let to = lane.join(relative);
    if !from.is_dir() || to.exists() { return Ok(false); }
    let started = Instant::now();
    if copy_tree(&from, &to, started, 0, clone).is_ok() { return Ok(true); }
    // `relative` is validated and `to` did not exist before this attempt. Never remove the donor.
    if to.exists() { std::fs::remove_dir_all(&to).context("removing incomplete lane cache clone")?; }
    Ok(false)
}

fn copy_tree(from: &Path, to: &Path, started: Instant, depth: u32, clone: &dyn Fn(&Path, &Path) -> Result<()>) -> Result<()> {
    if depth > 100 || started.elapsed() > Duration::from_secs(30) { bail!("lane clone budget exhausted"); }
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() { bail!("linked cache needs its normal setup"); }
    if metadata.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()), started, depth + 1, clone)?;
        }
    } else if metadata.is_file() {
        clone(from, to)?;
    } else { bail!("special cache file cannot be cloned"); }
    std::fs::set_permissions(to, metadata.permissions())?;
    Ok(())
}

fn clone_file(from: &Path, to: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        unsafe extern "C" { fn clonefile(from: *const std::ffi::c_char, to: *const std::ffi::c_char, flags: u32) -> i32; }
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: valid terminated paths, no flags; clonefile creates a distinct destination inode.
        if unsafe { clonefile(from.as_ptr(), to.as_ptr(), 0) } != 0 { return Err(std::io::Error::last_os_error().into()); }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        unsafe extern "C" { fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32; }
        let source = std::fs::File::open(from)?;
        let target = std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
        // SAFETY: FICLONE takes a source fd, not a pointer; both files stay open for the call.
        if unsafe { ioctl(target.as_raw_fd(), 0x4004_9409, source.as_raw_fd()) } != 0 { return Err(std::io::Error::last_os_error().into()); }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    { let _ = (from, to); bail!("this platform has no lane clone adapter"); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsupported_clone_removes_partial_cache_and_never_changes_donor() {
        let root = crate::paths::unique_temp("lane-clone", "");
        let source = root.join("source");let dest = root.join("dest");
        std::fs::create_dir_all(source.join("target")).unwrap();std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(source.join("target/artifact"), "original").unwrap();
        let fail = |_: &Path, to: &Path| { std::fs::write(to, "partial")?;bail!("unsupported"); };
        assert!(!seed_with(&source, &dest, "target", &fail).unwrap());
        assert!(!dest.join("target").exists());
        assert_eq!(std::fs::read_to_string(source.join("target/artifact")).unwrap(), "original");
        assert!(seed_with(&source, &dest, "../outside", &fail).is_err());
        assert!(seed_with(&source, &dest, ".git", &fail).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn existing_destination_is_never_replaced() {
        let root = crate::paths::unique_temp("lane-clone-existing", "");
        std::fs::create_dir_all(root.join("source/target")).unwrap();
        std::fs::create_dir_all(root.join("dest/target")).unwrap();
        std::fs::write(root.join("dest/target/owned"), "keep").unwrap();
        assert!(!seed(&root.join("source"), &root.join("dest"), "target").unwrap());
        assert!(root.join("dest/target/owned").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn native_clone_is_independent_when_supported() {
        let root = crate::paths::unique_temp("lane-clone-native", "");std::fs::create_dir_all(&root).unwrap();
        let from = root.join("from");let to = root.join("to");std::fs::write(&from, vec![42u8; 8192]).unwrap();
        match clone_file(&from, &to) {
            Ok(()) => { std::fs::write(&to, "changed").unwrap();assert_eq!(std::fs::read(&from).unwrap(), vec![42u8;8192]); }
            Err(error) => eprintln!("filesystem does not support native clones: {error}"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
