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
/// Cargo's own build folders, else a folder on PATH to put it in (`fallback_dir`). `cargo run` puts its target folders
/// first on PATH on Windows, and `target/<profile>/deps` holds a copy of the build; installing
/// over either would quietly change nothing. A PATH copy identical to the build is still the
/// target, so installing the same build twice changes nothing rather than overwriting a later
/// copy (NIT-52).
pub fn default_target(from: &Path) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    let folders: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    let cargo = dirs::home_dir().map(|h| h.join(".cargo").join("bin"));
    let windows_apps = if cfg!(windows) { dirs::data_local_dir().map(|d| d.join("Microsoft").join("WindowsApps")) } else { None };
    first_installed(&folders, from, exe).or_else(|| fallback_dir(&folders, cargo, windows_apps).map(|d| d.join(exe)))
}

/// The `nitely` agents and hooks run: the first on PATH outside Cargo's build folders. Under
/// `cargo run` (the app's `npm run tauri dev`) Cargo puts its target folders first on PATH, and the
/// copy there is whatever the last debug build left, often older than the installed one (NIT-193).
pub fn installed_on_path() -> Option<PathBuf> {
    let folders: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    installed_in(&folders)
}

/// `installed_on_path` for a PATH given as its folders.
pub fn installed_in(folders: &[PathBuf]) -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
    first_installed(folders, Path::new(""), exe)
}

/// Where a first install goes: Cargo's install folder when it is on PATH (a machine that builds
/// Nitely), else the user's WindowsApps folder when it is (Windows puts it on every account's
/// PATH, so the release installer's copy is found without changing PATH), else Cargo's folder.
fn fallback_dir(path: &[PathBuf], cargo: Option<PathBuf>, windows_apps: Option<PathBuf>) -> Option<PathBuf> {
    let on_path = |d: &PathBuf| path.iter().any(|p| crate::paths::normalize(&p.to_string_lossy()) == crate::paths::normalize(&d.to_string_lossy()));
    match (&cargo, &windows_apps) {
        (Some(c), _) if on_path(c) => cargo,
        (_, Some(w)) if on_path(w) => windows_apps,
        _ => cargo,
    }
}

/// The first `exe` in `dirs` that is neither `from` itself nor in a folder Cargo builds into: a
/// profile folder (Cargo keeps its `.fingerprint` there), or a `deps` folder beside the build or
/// in a profile folder. Told by Cargo's marker rather than by name, so an install that happens to
/// live under a folder called `target` still counts.
fn first_installed(dirs: &[PathBuf], from: &Path, exe: &str) -> Option<PathBuf> {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (from, build_dir) = (real(from), from.parent().map(real));
    let profile = |p: &Path| p.join(".fingerprint").is_dir();
    let cargo_folder = |dir: &Path| {
        let dir = real(dir);
        let deps = dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("deps"));
        profile(&dir) || (deps && dir.parent().is_some_and(|p| build_dir.as_deref() == Some(p) || profile(p)))
    };
    dirs.iter().filter(|d| !cargo_folder(d)).map(|d| d.join(exe)).find(|p| p.is_file() && real(p) != from)
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

/// Why a build must not be installed: it says it was built from `built_at`, but the checkout it was
/// built in (`cargo run`) is at `checkout_head`, so its stamp is stale and submit and land would
/// refuse it as older than the code it holds (NIT-320). None when they agree or either is unknown.
pub fn stamp_mismatch(built_at: &str, checkout_head: &str) -> Option<String> {
    let (built, head) = (built_at.trim(), checkout_head.trim());
    if built.is_empty() || head.is_empty() || built == head {
        return None;
    }
    Some(format!(
        "this build says it was built from {}, but the checkout it was built in is at {}: its build stamp is stale, and submit and land would refuse it as older than main. Touch crates/nitely-cli/build.rs and run the same command again",
        &built[..built.len().min(7)],
        &head[..head.len().min(7)]
    ))
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

    /// A build whose stamp is not its checkout's commit is refused, with how to fix it (NIT-320).
    #[test]
    fn a_stale_stamp_is_caught_before_it_is_installed() {
        let why = stamp_mismatch("c6b2fe4a64d4", "36c259dd2eae\n").unwrap();
        assert!(why.contains("built from c6b2fe4") && why.contains("is at 36c259d") && why.contains("build.rs"), "{why}");
        assert_eq!(stamp_mismatch("36c259d", "36c259d\n"), None);
        assert_eq!(stamp_mismatch("", "36c259d"), None, "no stamp: nothing to compare");
        assert_eq!(stamp_mismatch("36c259d", ""), None, "no checkout: nothing to compare");
    }

    #[test]
    fn a_first_install_goes_to_a_folder_on_path() {
        let cargo = PathBuf::from("C:/Users/sam/.cargo/bin");
        let apps = PathBuf::from("C:/Users/sam/AppData/Local/Microsoft/WindowsApps");
        let path = |dirs: &[&str]| dirs.iter().map(PathBuf::from).collect::<Vec<_>>();
        let pick = |p: &[PathBuf]| fallback_dir(p, Some(cargo.clone()), Some(apps.clone()));
        assert_eq!(pick(&path(&[r"C:\Users\sam\.cargo\bin", r"C:\Users\sam\AppData\Local\Microsoft\WindowsApps"])), Some(cargo.clone()), "a Rust machine keeps Cargo's folder");
        assert_eq!(pick(&path(&[r"C:\Windows", r"C:\Users\sam\AppData\Local\Microsoft\WindowsApps\"])), Some(apps.clone()), "no Rust: the folder Windows puts on PATH");
        assert_eq!(pick(&path(&[r"C:\Windows"])), Some(cargo.clone()), "neither on PATH: Cargo's folder, as before");
        assert_eq!(fallback_dir(&path(&[r"C:\Windows"]), None, None), None);
    }

    #[test]
    fn installs_over_the_first_path_copy_outside_cargos_folders() {
        let dir = crate::paths::unique_temp("nitely-target", "");
        let exe = "nitely-test.exe";
        let folders = ["repo/target/release", "repo/target/release/deps", "bin", "later"];
        for f in folders {
            std::fs::create_dir_all(dir.join(f)).unwrap();
            std::fs::write(dir.join(f).join(exe), "same build").unwrap();
        }
        std::fs::create_dir_all(dir.join("repo/target/release/.fingerprint")).unwrap();
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
        // A build handed in with --from beside the installed copy: that copy is the target.
        let beside = dir.join("bin").join("nitely-new.exe");
        std::fs::write(&beside, "new").unwrap();
        assert_eq!(first_installed(&[dir.join("bin")], &beside, exe), Some(dir.join("bin").join(exe)));
        // An install under a folder that is only named `target` (cargo install --root ...	arget).
        let tools = dir.join("tools/target/bin");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join(exe), "old").unwrap();
        assert_eq!(first_installed(&[tools.clone(), dir.join("later")], &from, exe), Some(tools.join(exe)));
        // With no build of its own (the app's keeper looking for the installed copy): Cargo's
        // profile folder and its deps are still passed over.
        let keeper = [dir.join("repo/target/release"), dir.join("repo/target/release/deps"), dir.join("bin")];
        assert_eq!(first_installed(&keeper, Path::new(""), exe), Some(dir.join("bin").join(exe)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_build_older_than_landed_nitely_code_is_stale() {
        let dir = crate::paths::unique_temp("nitely-stale", "");
        let repo = dir.join("repo");
        std::fs::create_dir_all(repo.join("crates").join("nitely-cli")).unwrap();
        let git = |args: &[&str]| {
            let o = crate::playbook::run_git(std::process::Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
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
        let dir = crate::paths::unique_temp("nitely-install", "");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = if cfg!(windows) { "nitely.exe" } else { "nitely" };
        let target = dir.join(exe);
        // A real executable that stays running from the target path, as an agent's MCP server would.
        let system = if cfg!(windows) { PathBuf::from(r"C:\Windows\System32\PING.EXE") } else { PathBuf::from("/bin/sleep") };
        std::fs::copy(&system, &target).unwrap();
        let args: &[&str] = if cfg!(windows) { &["-n", "30", "127.0.0.1"] } else { &["30"] };
        // Linux refuses to run a file another thread of this test process still holds open for
        // writing (a test forking while the copy was open hands its child the handle): "text file
        // busy" passes once that child has gone on to run its own program.
        let spawn = || std::process::Command::new(&target).args(args).stdout(std::process::Stdio::null()).spawn();
        let mut running = (0..50)
            .find_map(|_| match spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
                other => Some(other.unwrap()),
            })
            .expect("the copy stayed busy");
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
