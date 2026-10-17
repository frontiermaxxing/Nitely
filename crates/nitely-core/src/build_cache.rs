//! One Rust build directory for all of a repo's task worktrees (NIT-190).
//!
//! Each worktree used to build the whole workspace into its own `target/`: on Windows a cold
//! `cargo test` wrote some 5,500 files (5.6 GB) per new worktree, and Defender's real-time scan
//! read every one of them, more CPU than the builds themselves with a few lanes running. Cargo
//! can keep its intermediate artifacts in a `build-dir` apart from `target/`; pointed at one
//! folder beside the worktrees, dependencies compile once and every later worktree writes only
//! its own crates (about 1,500 files, 1.1 GB), with its `target/` down to the final binaries.
//!
//! The setting goes in `.cargo/config.toml` in the folder that holds the worktrees, which cargo
//! reads for every build under it, so an agent's own `cargo test` shares it as the gate does. It
//! is written only where that folder holds nothing but worktrees: never above the main checkout,
//! and never over a config file someone else wrote. Cargo serializes builds that share the
//! directory; measured, two gates at once still finished sooner (74 s against 122 s).

use std::path::{Path, PathBuf};

use crate::playbook::Playbook;

/// The first line of the config file Nitely writes, so it knows the file is its own.
const MARKER: &str = "# Written by Nitely";
/// The shared build directory, in the folder that holds the worktrees.
const BUILD_DIR: &str = ".cargo-build";

/// The folder a repo's task worktrees are made in, when it holds nothing else of the repo's: not
/// the main checkout, inside it, or above it.
pub fn worktrees_folder(pb: &Playbook, repo_root: &Path) -> Option<PathBuf> {
    let issue = crate::IssueRecord { key: "x".into(), title: "x".into(), ..Default::default() };
    let folder = pb.worktree_for(repo_root, "x", &issue).parent()?.to_path_buf();
    let same = |a: &Path, b: &Path| crate::paths::normalize(&a.to_string_lossy()) == crate::paths::normalize(&b.to_string_lossy());
    let ancestor = |a: &Path, of: &Path| of.ancestors().any(|p| same(p, a));
    (!ancestor(&folder, repo_root) && !ancestor(repo_root, &folder)).then_some(folder)
}

/// Makes a new worktree build into the shared directory, as the playbook says: writes (or, when
/// `shared_build` is off, removes) Nitely's `.cargo/config.toml` in the worktrees' folder. Only
/// for a Rust repo. Returns a warning when it cannot.
pub fn prepare(pb: &Playbook, repo_root: &Path, worktree: &Path) -> Option<String> {
    let folder = worktrees_folder(pb, repo_root)?;
    let config = folder.join(".cargo").join("config.toml");
    let existing = std::fs::read_to_string(&config).ok();
    let ours = existing.as_deref().is_some_and(|c| c.starts_with(MARKER));
    if !pb.worktree.shared_build {
        if ours {
            std::fs::remove_file(&config).ok();
        }
        return None;
    }
    if !worktree.join("Cargo.toml").is_file() {
        return None;
    }
    if existing.is_some() && !ours {
        return Some(format!("{} is not Nitely's, so this worktree builds into its own target/ (shared_build)", config.display()));
    }
    let wanted = config_text(&folder);
    if existing.as_deref() != Some(wanted.as_str()) {
        let written = std::fs::create_dir_all(config.parent()?).and_then(|_| std::fs::write(&config, &wanted));
        if let Err(e) = written {
            return Some(format!("could not write {} ({e}), so this worktree builds into its own target/", config.display()));
        }
    }
    None
}

fn config_text(folder: &Path) -> String {
    let dir = folder.join(BUILD_DIR).to_string_lossy().replace('\\', "/").replace('"', "\\\"");
    format!(
        "{MARKER} for the task worktrees in this folder (`shared_build` in the repo's playbook).\n\
# They build into one shared directory, so dependencies compile once instead of once per\n\
# worktree. Nitely writes this file again unless the playbook turns shared_build off.\n\
[build]\n\
build-dir = \"{dir}\"\n"
    )
}

/// Total size of the files under `dir`, in bytes.
fn size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(_) => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    total
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// After land removes a worktree: when the shared build directory has grown past the cap (every
/// worktree adds its own crates, about a gigabyte for Nitely), clears it; the next build fills it
/// again. Only when no build is using it: it is renamed first, which Windows refuses while a
/// file in it is open, and deleted under the new name.
pub fn trim(pb: &Playbook, repo_root: &Path) -> Option<String> {
    let folder = worktrees_folder(pb, repo_root)?;
    trim_dir(&folder, pb.worktree.shared_build_max_gb.saturating_mul(1_000_000_000))
}

fn trim_dir(folder: &Path, max_bytes: u64) -> Option<String> {
    // Leftovers of an earlier clear that could not finish.
    for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(&format!("{BUILD_DIR}-old-")) {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
    let dir = folder.join(BUILD_DIR);
    let bytes = size(&dir);
    if bytes <= max_bytes {
        return None;
    }
    let aside = folder.join(format!("{BUILD_DIR}-old-{}", crate::db::now()));
    if std::fs::rename(&dir, &aside).is_err() {
        return Some(format!("the shared build directory is {} (over {}) but a build is using it; it is cleared after a later land", gb(bytes), gb(max_bytes)));
    }
    std::fs::remove_dir_all(&aside).ok();
    Some(format!("cleared the shared build directory ({}; the next build fills it again)", gb(bytes)))
}

/// For a worktree land had to keep because a process uses it: deletes its `target/`, the bulk of
/// it, when no process uses that. Returns what it did.
pub fn free_target(worktree: &Path) -> Option<String> {
    let target = worktree.join("target");
    if !target.is_dir() || !crate::procs::holders(&target).is_empty() {
        return None;
    }
    let bytes = size(&target);
    std::fs::remove_dir_all(&target).ok()?;
    Some(format!("deleted its target/ ({})", gb(bytes)))
}

/// On Windows, one line on Defender's real-time scan of a repo's worktrees, when that scan slows
/// their builds: the worktrees' folder is on a normal volume and real-time protection is on. A
/// Dev Drive (a ReFS volume, which Defender scans asynchronously in its performance mode) is the
/// remedy Windows offers for exactly this; the line names one that exists, or where to make one.
/// Nitely never changes Defender's settings; the person decides.
pub fn defender_note(pb: &Playbook, repo_root: &Path) -> Option<String> {
    let folder = worktrees_folder(pb, repo_root).unwrap_or_else(|| pb.worktree_for(repo_root, "x", &crate::IssueRecord::default()).parent().map(Path::to_path_buf).unwrap_or_default());
    let fs = win::file_system(&folder)?;
    if fs.eq_ignore_ascii_case("ReFS") || win::realtime_protection() != Some(true) {
        return None;
    }
    let repo = repo_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    let advice = match win::dev_drives().first() {
        Some(drive) => format!(
            "{drive} is a Dev Drive, where Defender scans without holding builds up: to build there, set `dir = \"{}/{repo}-worktrees/{{key}}-{{slug}}\"` under [worktree] in the playbook",
            drive.trim_end_matches(['\\', '/'])
        ),
        None => "a Dev Drive scans without holding builds up: make one in Settings > System > Storage > Advanced storage settings > Disks & volumes, then point the playbook's [worktree] dir at it (learn.microsoft.com/windows/dev-drive)".into(),
    };
    Some(format!("Windows Defender scans every file your agents' builds write in {} as it is written; {advice}", folder.display()))
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetVolumePathNameW(file: *const u16, volume: *mut u16, len: u32) -> i32;
        fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, fs: *mut u16, fs_len: u32) -> i32;
        fn GetLogicalDriveStringsW(len: u32, buf: *mut u16) -> u32;
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain([0]).collect()
    }

    fn text(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        std::ffi::OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
    }

    fn volume_fs(root: &[u16]) -> Option<String> {
        let mut fs = [0u16; 64];
        let (mut serial, mut max, mut flags) = (0u32, 0u32, 0u32);
        // SAFETY: `root` is NUL-terminated; the buffers and their lengths match, and a null name
        // buffer of length 0 is allowed.
        let ok = unsafe { GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, &mut serial, &mut max, &mut flags, fs.as_mut_ptr(), fs.len() as u32) };
        (ok != 0).then(|| text(&fs))
    }

    /// The file system (`NTFS`, `ReFS`) of the volume `path` is on, or would be on: the nearest
    /// part of it that exists decides.
    pub fn file_system(path: &Path) -> Option<String> {
        let existing = path.ancestors().find(|a| a.exists())?;
        let mut volume = [0u16; 512];
        // SAFETY: the path is NUL-terminated and `volume` holds `volume.len()` u16s.
        let ok = unsafe { GetVolumePathNameW(wide(existing.as_os_str()).as_ptr(), volume.as_mut_ptr(), volume.len() as u32) };
        if ok == 0 {
            return None;
        }
        volume_fs(&volume)
    }

    /// Drive roots (`D:\`) on a ReFS volume: Dev Drives, on Windows 11.
    pub fn dev_drives() -> Vec<String> {
        let mut buf = [0u16; 512];
        // SAFETY: `buf` holds `buf.len()` u16s; the call writes NUL-separated roots into it.
        let n = unsafe { GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr()) } as usize;
        buf[..n.min(buf.len())]
            .split(|&c| c == 0)
            .filter(|r| !r.is_empty())
            .filter(|r| {
                let root: Vec<u16> = r.iter().copied().chain([0]).collect();
                volume_fs(&root).is_some_and(|fs| fs.eq_ignore_ascii_case("ReFS"))
            })
            .map(text)
            .collect()
    }

    /// Whether Defender's real-time protection is on, read once per process (it asks PowerShell,
    /// which takes a second). None when it cannot be told, as with another antivirus.
    pub fn realtime_protection() -> Option<bool> {
        static ON: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            let out = crate::playbook::shell_free_command("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", "(Get-MpComputerStatus).RealTimeProtectionEnabled"])
                .output()
                .ok()?;
            match String::from_utf8_lossy(&out.stdout).trim() {
                "True" => Some(true),
                "False" => Some(false),
                _ => None,
            }
        })
    }
}

#[cfg(not(windows))]
mod win {
    use std::path::Path;

    pub fn file_system(_: &Path) -> Option<String> {
        None
    }

    pub fn dev_drives() -> Vec<String> {
        vec![]
    }

    pub fn realtime_protection() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playbook(text: &str) -> Playbook {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn writes_the_shared_build_dir_beside_the_worktrees_and_only_there() {
        let base = crate::paths::unique_temp("nitely-build-cache", "");
        let repo = base.join("app");
        let wt = base.join("app-worktrees").join("app-1-x");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("Cargo.toml"), "[workspace]\n").unwrap();
        let pb = playbook("[worktree]\ndir = \"../{repo}-worktrees/{key}-{slug}\"\n");
        let config = base.join("app-worktrees").join(".cargo").join("config.toml");

        assert_eq!(prepare(&pb, &repo, &wt), None);
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.starts_with(MARKER) && text.contains("build-dir = ") && text.contains("app-worktrees/.cargo-build"), "{text}");

        // Turned off, Nitely's file goes; someone else's file is never replaced.
        assert_eq!(prepare(&playbook("[worktree]\ndir = \"../{repo}-worktrees/{key}-{slug}\"\nshared_build = false\n"), &repo, &wt), None);
        assert!(!config.exists());
        std::fs::write(&config, "[build]\njobs = 2\n").unwrap();
        assert!(prepare(&pb, &repo, &wt).unwrap().contains("is not Nitely's"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "[build]\njobs = 2\n");
        std::fs::remove_file(&config).unwrap();

        // Not for a repo without Cargo.toml, and never where the folder also holds the main
        // checkout: the setting would reach it too.
        std::fs::remove_file(wt.join("Cargo.toml")).unwrap();
        assert_eq!(prepare(&pb, &repo, &wt), None);
        assert!(!config.exists());
        std::fs::write(wt.join("Cargo.toml"), "[workspace]\n").unwrap();
        let beside = playbook("[worktree]\ndir = \"../{key}-{slug}\"\n");
        assert_eq!(worktrees_folder(&beside, &repo), None);
        let inside = playbook("[worktree]\ndir = \"wt/{key}\"\n");
        assert_eq!(worktrees_folder(&inside, &repo), None);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn clears_the_shared_build_dir_past_its_cap_and_frees_a_kept_worktrees_target() {
        let folder = crate::paths::unique_temp("nitely-build-trim", "");
        let dir = folder.join(BUILD_DIR).join("debug").join("deps");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("libdep-1234.rlib"), vec![0u8; 4096]).unwrap();
        assert_eq!(trim_dir(&folder, 1 << 20), None, "under the cap");
        assert!(dir.exists());
        assert!(trim_dir(&folder, 1024).unwrap().starts_with("cleared"));
        assert!(!folder.join(BUILD_DIR).exists());
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0, "nothing left aside");

        let wt = folder.join("app-1-x");
        std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
        std::fs::write(wt.join("target").join("debug").join("app.exe"), b"x").unwrap();
        assert!(free_target(&wt).unwrap().starts_with("deleted its target/"));
        assert!(!wt.join("target").exists() && wt.exists());
        std::fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn reads_the_volume_a_folder_is_on() {
        if !cfg!(windows) {
            return;
        }
        // A folder not made yet is on the volume of the part of it that exists.
        let fs = win::file_system(&std::env::temp_dir().join("nitely-not-made-yet").join("x")).unwrap();
        assert!(["NTFS", "ReFS"].iter().any(|f| fs.eq_ignore_ascii_case(f)), "{fs}");
        assert!(win::dev_drives().iter().all(|d| d.ends_with('\\')));
    }
}
