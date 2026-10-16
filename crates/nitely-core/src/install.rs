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

/// The executable agents run as `nitely` to install `from` over: the first on PATH outside
/// Cargo's own build folders, else Cargo's install folder. `cargo run` puts its target folders
/// first on PATH on Windows, and `target/<profile>/deps` holds a copy of the build; installing
/// over either would quietly change nothing. A PATH copy identical to the build is still the
/// target, so installing the same build twice changes nothing rather than overwriting a later
/// copy (NIT-52).
pub fn default_target(from: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let folders: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    first_installed(&folders, from, exe).or_else(|| dirs::home_dir().map(|h| h.join(".cargo").join("bin").join(exe)))
}

/// The first `exe` in `dirs` that is not in a folder Cargo builds into: the build's own folder
/// and its `deps`, or any `target/<profile>` and `target/<profile>/deps`.
fn first_installed(dirs: &[PathBuf], from: &Path, exe: &str) -> Option<PathBuf> {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let build_dir = from.parent().map(real);
    let cargo_folder = |dir: &Path| {
        let dir = real(dir);
        let named = |p: Option<&Path>, name: &str| p.and_then(Path::file_name).is_some_and(|n| n.eq_ignore_ascii_case(name));
        let profile = if named(Some(&dir), "deps") { dir.parent() } else { Some(dir.as_path()) };
        build_dir.as_deref().is_some_and(|b| profile == Some(b)) || named(profile.and_then(Path::parent), "target")
    };
    dirs.iter().filter(|d| !cargo_folder(d)).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Code whose change makes a nitely build out of date for Nitely's own repo.
const NITELY_CODE: &[&str] = &["crates", "Cargo.toml", "Cargo.lock"];

/// Why this nitely must not submit or land in `root`, if it is Nitely's own repo and `base` has
/// commits to Nitely's code that this build (made from `built_at`) does not have: it would act on
/// the repo with older rules than the ones already landed (NIT-46: an old build moved a submit
/// whose push failed to In Review). None for other repos, an up-to-date build, or a build whose
/// commit this repo does not know.
pub fn stale_build(root: &Path, base: &str, built_at: &str) -> Option<String> {
    if built_at.is_empty() || !root.join("crates").join("nitely-cli").join("Cargo.toml").is_file() {
        return None;
    }
    let range = format!("{built_at}..{base}");
    let mut args = vec!["rev-list", "--count", range.as_str(), "--"];
    args.extend(NITELY_CODE);
    let newer: usize = crate::playbook::git(root, &args).ok()?.trim().parse().ok()?;
    (newer > 0).then(|| {
        format!(
            "this nitely was built from {} and {base} has {newer} newer commit{} to Nitely's code, so it could submit or land with \
older rules than the ones already landed. Install the landed build first: run `cargo run --release -p nitely-cli -- install` in {}, \
then try again (a running nitely MCP server forwards to the new build)",
            &built_at[..built_at.len().min(7)],
            if newer == 1 { "" } else { "s" },
            root.display()
        )
    })
}

/// `stale_build` for the repo `dir` is in, as an error to refuse with.
pub fn check_current(config: &crate::Config, dir: &str, built_at: &str) -> Result<()> {
    let Some(root) = crate::playbook::repo_root(config, dir) else { return Ok(()) };
    let pb = crate::playbook::Playbook::load(&root)?;
    match stale_build(&root, pb.integration_branch(), built_at) {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
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
        if let Err(e) = rename_patiently(to, &aside) {
            std::fs::remove_file(&staged).ok();
            return Err(e).with_context(|| format!("moving the running {} aside", to.display()));
        }
        out.moved_aside = Some(aside.to_string_lossy().into_owned());
    }
    // Unix replaces a running executable atomically; on Windows the name is free by now.
    if let Err(e) = rename_patiently(&staged, to) {
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

/// A rename, tried a few times: on Windows a file just copied or just started is often held for
/// a moment (by antivirus scanning it, or the loader), and the rename fails with "access denied".
fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(_) if tries < 8 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            result => return result,
        }
    }
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
    fn installs_over_the_first_path_copy_outside_cargos_folders() {
        let dir = std::env::temp_dir().join(format!("nitely-target-{}-{}", std::process::id(), crate::db::now()));
        let exe = "nitely-test.exe";
        let folders = ["repo/target/release", "repo/target/release/deps", "bin", "later"];
        for f in folders {
            std::fs::create_dir_all(dir.join(f)).unwrap();
            std::fs::write(dir.join(f).join(exe), "same build").unwrap();
        }
        let from = dir.join("repo/target/release").join(exe);
        let path: Vec<PathBuf> = folders.iter().map(|f| dir.join(f)).collect();
        // Identical to the build, and still the target: a second install is a no-op there.
        assert_eq!(first_installed(&path, &from, exe), Some(dir.join("bin").join(exe)));
        // A build under a custom target folder: its own folder and deps are still skipped.
        let custom = dir.join("out");
        std::fs::create_dir_all(custom.join("deps")).unwrap();
        std::fs::write(custom.join(exe), "x").unwrap();
        std::fs::write(custom.join("deps").join(exe), "x").unwrap();
        let path = vec![custom.clone(), custom.join("deps"), dir.join("later")];
        assert_eq!(first_installed(&path, &custom.join(exe), exe), Some(dir.join("later").join(exe)));
        assert_eq!(first_installed(&[dir.join("nowhere")], &from, exe), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_build_older_than_landed_nitely_code_is_stale() {
        let dir = std::env::temp_dir().join(format!("nitely-stale-{}-{}", std::process::id(), crate::db::now()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("crates").join("nitely-cli")).unwrap();
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
            assert!(o.status.success(), "{args:?}");
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("crates").join("nitely-cli").join("Cargo.toml"), "[package]").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let built = git(&["rev-parse", "HEAD"]);
        assert_eq!(stale_build(&repo, "main", &built), None, "built from main itself");

        std::fs::write(repo.join("README.md"), "docs").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "docs"]);
        assert_eq!(stale_build(&repo, "main", &built), None, "only docs landed since");

        std::fs::write(repo.join("crates").join("nitely-cli").join("Cargo.toml"), "[package]\nname = \"x\"").unwrap();
        git(&["commit", "-q", "-am", "code"]);
        let why = stale_build(&repo, "main", &built).expect("code landed since the build");
        assert!(why.contains("1 newer commit ") && why.contains("install"), "{why}");
        assert_eq!(stale_build(&repo, "main", "0123456789abcdef0123456789abcdef01234567"), None, "unknown build commit");
        assert_eq!(stale_build(&dir, "main", &built), None, "not Nitely's repo");
        std::fs::remove_dir_all(&dir).ok();
    }

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
