//! `nitely install`: put a new build where agents run `nitely` from, while they keep running the
//! old one. Hooks start a fresh process for every call and pick it up at once; a running
//! `nitely mcp` notices the new file and forwards its tool calls to it (crates/nitely-cli/src/mcp.rs).
//!
//! Windows will not overwrite or delete an executable that a process is running, but it does let
//! one be renamed. So the running one is moved aside as `nitely-old-<time>.exe` in the same folder,
//! the new one is copied in, and old copies nothing runs any more are deleted on the way.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

const OLD_PREFIX: &str = "nitely-old-";

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    /// Where the new build now is.
    pub path: String,
    /// False when the target already was this exact file.
    pub replaced: bool,
    /// The previous build, moved aside because something still ran it (Windows only).
    pub moved_aside: Option<String>,
    /// Older copies deleted because nothing ran them any more.
    pub cleaned: usize,
    /// Older copies still running, left for a later install to delete.
    pub still_running: usize,
}

/// The executable agents run as `nitely` to install `from` over: the first on PATH that is not
/// `from` itself, else Cargo's install folder. `cargo run` puts its own target folder first on
/// PATH (on Windows), and installing a build over itself would quietly change nothing.
pub fn default_target(from: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let source = from.canonicalize().ok();
    let is_source = |p: &Path| source.is_some() && p.canonicalize().ok() == source;
    let on_path = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file() && !is_source(p)));
    on_path.or_else(|| dirs::home_dir().map(|h| h.join(".cargo").join("bin").join(exe)))
}

/// Copies `from` to `to`, moving a running `to` aside first where the platform needs that.
pub fn install(from: &Path, to: &Path) -> Result<Installed> {
    if !from.is_file() {
        bail!("{} is not a file", from.display());
    }
    let dir = to.parent().context("the target has no folder")?;
    std::fs::create_dir_all(dir)?;
    let same = |a: &Path, b: &Path| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
    let mut out = Installed { path: to.to_string_lossy().into_owned(), replaced: false, moved_aside: None, cleaned: 0, still_running: 0 };
    if to.exists() && (same(from, to) || std::fs::read(from)? == std::fs::read(to)?) {
        (out.cleaned, out.still_running) = clean_old(dir);
        return Ok(out);
    }

    // Copied beside the target first, so the swap below is two renames in one folder.
    let staged = dir.join(format!(".nitely-new-{}", std::process::id()));
    std::fs::copy(from, &staged).with_context(|| format!("copying {} to {}", from.display(), staged.display()))?;
    // Whatever `from` was, agents must be able to run what is installed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&staged)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(&staged, perms)?;
    }
    if to.exists() && cfg!(windows) {
        let aside = dir.join(format!("{OLD_PREFIX}{}.exe", crate::db::now()));
        if let Err(e) = std::fs::rename(to, &aside) {
            std::fs::remove_file(&staged).ok();
            return Err(e).with_context(|| format!("moving the running {} aside", to.display()));
        }
        out.moved_aside = Some(aside.to_string_lossy().into_owned());
    }
    // Unix replaces a running executable atomically; on Windows the name is free by now.
    if let Err(e) = std::fs::rename(&staged, to) {
        if let Some(aside) = &out.moved_aside {
            std::fs::rename(aside, to).ok();
        }
        std::fs::remove_file(&staged).ok();
        return Err(e).with_context(|| format!("putting the new build at {}", to.display()));
    }
    out.replaced = true;
    (out.cleaned, out.still_running) = clean_old(dir);
    Ok(out)
}

/// Deletes the moved-aside copies in `dir` that nothing runs any more (Windows refuses to delete
/// one that is running). Returns (deleted, still running).
fn clean_old(dir: &Path) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut counts = (0, 0);
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(OLD_PREFIX) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => counts.0 += 1,
            Err(_) => counts.1 += 1,
        }
    }
    counts
}

/// Identifies the file at `path` as it is now, to tell later whether it was replaced.
pub fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_over_a_build_that_is_running() {
        let dir = std::env::temp_dir().join(format!("nitely-install-{}-{}", std::process::id(), crate::db::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
        let target = dir.join(exe);
        // A real executable that stays running from the target path, as an agent's MCP server would.
        let system = if cfg!(windows) { PathBuf::from(r"C:\Windows\System32\PING.EXE") } else { PathBuf::from("/bin/sleep") };
        std::fs::copy(&system, &target).unwrap();
        let args: &[&str] = if cfg!(windows) { &["-n", "30", "127.0.0.1"] } else { &["30"] };
        let mut running = std::process::Command::new(&target).args(args).stdout(std::process::Stdio::null()).spawn().unwrap();
        let before = stamp(&target).unwrap();

        let build = dir.join("new-build");
        std::fs::write(&build, b"the new nitely").unwrap();
        let first = install(&build, &target).unwrap();
        assert!(first.replaced, "{first:?}");
        assert_eq!(std::fs::read(&target).unwrap(), b"the new nitely");
        assert_ne!(stamp(&target), Some(before), "a running mcp server can tell it was replaced");
        if cfg!(windows) {
            assert!(first.moved_aside.is_some() && first.still_running == 1, "{first:?}");
        }

        // Once the old build stops, the next install deletes its copy; the same build is a no-op.
        running.kill().ok();
        running.wait().ok();
        let again = install(&build, &target).unwrap();
        assert!(!again.replaced && again.still_running == 0, "{again:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
