//! Deleting a folder without reaching through a link inside it (NIT-460). On Windows, `git worktree
//! remove` follows a directory junction in the worktree and deletes the files of the folder it
//! points at: an agent that linked `node_modules` into the main checkout emptied the main
//! checkout's `node_modules` when its task landed. So before any worktree, or a folder like one, is
//! removed, every link in it (a symlink, or on Windows any reparse point: a junction) is taken away
//! as a link. The link goes; what it points at is never opened. This does not rely on git, nor on
//! `remove_dir_all` declining to follow junctions.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Removes every link under `dir`, as a link, without following any; `dir` itself must not be one.
/// Returns the links removed. Fails, naming it, at the first link that cannot be removed or folder
/// that cannot be read: a folder that was not looked through may hold a link, so nothing may then
/// delete it recursively.
pub fn unlink_all(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if !dir.exists() && std::fs::symlink_metadata(dir).is_err() {
        return Ok(removed);
    }
    if is_link(dir) {
        bail!("{} is itself a link; remove the link, not what it points at", dir.display());
    }
    walk(dir, &mut removed)?;
    Ok(removed)
}

fn walk(dir: &Path, removed: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {} to find links in it", dir.display()))?;
    for entry in entries {
        let path = entry.with_context(|| format!("reading {} to find links in it", dir.display()))?.path();
        let meta = std::fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if is_link_meta(&meta) {
            unlink(&path, &meta).with_context(|| format!("could not remove the link {} (it points at {}); remove that link, not what it points at", path.display(), target(&path)))?;
            removed.push(path);
        } else if meta.is_dir() {
            walk(&path, removed)?;
        }
    }
    Ok(())
}

/// Deletes `dir` and everything in it, its links first as links (`unlink_all`).
pub fn remove_tree(dir: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    if is_link(dir) {
        let meta = std::fs::symlink_metadata(dir)?;
        return unlink(dir, &meta).with_context(|| format!("removing the link {}", dir.display()));
    }
    unlink_all(dir)?;
    std::fs::remove_dir_all(dir).with_context(|| format!("deleting {}", dir.display()))
}

/// Whether `path` is a link to somewhere else: a symlink, or on Windows any reparse point.
pub fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| is_link_meta(&m))
}

fn is_link_meta(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT: a junction, a mount point
    }
    #[cfg(not(windows))]
    false
}

/// Removes the link itself. On Windows a link to a folder (a junction or a directory symlink) is a
/// directory entry, removed with RemoveDirectory, which deletes the link and never its target.
fn unlink(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x10 != 0 {
            // FILE_ATTRIBUTE_DIRECTORY
            return std::fs::remove_dir(path);
        }
        std::fs::remove_file(path)
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        std::fs::remove_file(path)
    }
}

fn target(path: &Path) -> String {
    std::fs::read_link(path).map(|t| t.display().to_string()).unwrap_or_else(|_| "somewhere outside".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A link at `link` to the folder `to`: a junction on Windows (as `mklink /J` and PowerShell's
    /// `New-Item -ItemType Junction` make, no rights needed), a symlink elsewhere.
    pub(crate) fn link_folder(link: &Path, to: &Path) {
        #[cfg(windows)]
        {
            let made = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(to).output().unwrap();
            assert!(made.status.success(), "mklink /J: {}", String::from_utf8_lossy(&made.stdout));
        }
        #[cfg(not(windows))]
        std::os::unix::fs::symlink(to, link).unwrap();
    }

    /// A folder outside anything removed, holding files: what a link points at.
    pub(crate) fn outside(base: &Path) -> PathBuf {
        let outside = base.join("outside").join("node_modules");
        std::fs::create_dir_all(outside.join("pkg")).unwrap();
        std::fs::write(outside.join("a.txt"), "a").unwrap();
        std::fs::write(outside.join("pkg").join("index.js"), "b").unwrap();
        outside
    }

    pub(crate) fn intact(outside: &Path) -> bool {
        outside.join("a.txt").is_file() && outside.join("pkg").join("index.js").is_file()
    }

    #[test]
    fn a_folder_goes_and_the_folder_a_link_in_it_points_at_keeps_its_files() {
        let base = crate::paths::unique_temp("nitely-unlink", "");
        let outside = outside(&base);
        let doomed = base.join("worktree");
        std::fs::create_dir_all(doomed.join("deep").join("er")).unwrap();
        std::fs::write(doomed.join("deep").join("own.txt"), "mine").unwrap();
        link_folder(&doomed.join("node_modules"), &outside);
        link_folder(&doomed.join("deep").join("er").join("again"), &outside);

        remove_tree(&doomed).unwrap();
        assert!(!doomed.exists(), "the folder is gone");
        assert!(intact(&outside), "what the links pointed at keeps its files");

        // A link given as the folder goes as a link.
        let lone = base.join("lone");
        link_folder(&lone, &outside);
        assert!(unlink_all(&lone).is_err(), "unlink_all is for folders, not links");
        remove_tree(&lone).unwrap();
        assert!(std::fs::symlink_metadata(&lone).is_err() && intact(&outside));
        std::fs::remove_dir_all(&base).ok();
    }
}
