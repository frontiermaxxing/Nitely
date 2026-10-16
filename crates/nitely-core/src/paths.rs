use std::path::PathBuf;

/// Per-user data directory: `%APPDATA%\Nitely` on Windows, `~/Library/Application Support/Nitely`
/// on Mac. `NITELY_HOME` overrides it (tests, or a second profile).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("NITELY_HOME") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Nitely")
}

pub fn db_path() -> PathBuf {
    data_dir().join("nitely.db")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// A path in the temp folder no other call makes, in this process or another: the process id, the
/// clock and a counter, so two made in the same clock tick (Windows' clock moves in 100ns steps,
/// a test name or `now()` in whole seconds) still differ. `suffix` ends the name, e.g. `.txt`.
/// Nothing is created. Every scratch file or folder, in tests too, should come from here.
pub fn unique_temp(prefix: &str, suffix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{n}{suffix}", std::process::id()))
}

/// A path in one comparable form: forward slashes, no trailing slash, lowercase on Windows, and
/// 8.3 short names (`RUNNER~1`) spelled out. Git prints `C:/Code/x`, hooks send `C:\Code\x`, an
/// old tool or a short TEMP gives `C:\Users\RUNNER~1\...`; all must match (NIT-104).
pub fn normalize(path: &str) -> String {
    let long = if cfg!(windows) && path.contains('~') { long_path(path) } else { path.to_string() };
    let mut p = long.replace('\\', "/");
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if cfg!(windows) {
        p = p.to_lowercase();
    }
    p
}

/// `path` with its 8.3 short names spelled out, as far as it exists (the rest is kept as given).
/// Unchanged where there is nothing to spell out, and off Windows.
pub fn long_path(path: &str) -> String {
    win::each_existing(path, win::long)
}

/// `path` with 8.3 short names where the volume has them, as far as it exists: how a cmd shell
/// or an old tool may spell it. Unchanged where there are none, and off Windows.
pub fn short_path(path: &str) -> String {
    win::each_existing(path, win::short)
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLongPathNameW(short: *const u16, long: *mut u16, len: u32) -> u32;
        fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
    }

    /// One of the two calls: each fills `out` and returns its length, or the size it needs.
    type Convert = unsafe extern "system" fn(*const u16, *mut u16, u32) -> u32;

    fn convert(f: Convert, path: &Path) -> Option<String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut out = vec![0u16; 260];
        loop {
            // SAFETY: `wide` is NUL-terminated and `out` holds `out.len()` u16s, as the call needs.
            let n = unsafe { f(wide.as_ptr(), out.as_mut_ptr(), out.len() as u32) } as usize;
            if n == 0 {
                return None;
            }
            if n < out.len() {
                return Some(std::ffi::OsString::from_wide(&out[..n]).to_string_lossy().into_owned());
            }
            out.resize(n, 0);
        }
    }

    pub fn long(path: &Path) -> Option<String> {
        convert(GetLongPathNameW, path)
    }

    pub fn short(path: &Path) -> Option<String> {
        convert(GetShortPathNameW, path)
    }

    /// Converts the longest part of `path` that exists and keeps the rest as given.
    pub fn each_existing(path: &str, f: fn(&Path) -> Option<String>) -> String {
        let p = Path::new(path);
        for base in p.ancestors().filter(|a| !a.as_os_str().is_empty()) {
            if !base.exists() {
                continue;
            }
            let Some(converted) = f(base) else { break };
            let rest = p.strip_prefix(base).unwrap_or(Path::new(""));
            return if rest.as_os_str().is_empty() { converted } else { Path::new(&converted).join(rest).to_string_lossy().into_owned() };
        }
        path.to_string()
    }
}

#[cfg(not(windows))]
mod win {
    use std::path::Path;

    pub fn long(_: &Path) -> Option<String> {
        None
    }

    pub fn short(_: &Path) -> Option<String> {
        None
    }

    pub fn each_existing(path: &str, _: fn(&Path) -> Option<String>) -> String {
        path.to_string()
    }
}

/// True when `child` is `parent` or inside it.
pub fn is_within(child: &str, parent: &str) -> bool {
    let (c, p) = (normalize(child), normalize(parent));
    c == p || c.starts_with(&format!("{p}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_match_long_ones() {
        let top = std::env::temp_dir().join(format!("nitely-long-folder-name-{}", std::process::id()));
        let long = top.join("inner-folder");
        std::fs::create_dir_all(&long).unwrap();
        let long_text = long.to_string_lossy().into_owned();
        let short = short_path(&long_text);
        // Only volumes with 8.3 names enabled have a short spelling to check.
        if cfg!(windows) && short != long_text {
            assert!(short.contains('~'), "{short}");
            assert_eq!(normalize(&short), normalize(&long_text));
            // A worktree not made yet, under a short-named folder, still matches its long spelling.
            assert_eq!(normalize(&format!(r"{short}\not-yet\x")), normalize(&format!("{long_text}/not-yet/x")));
            assert!(is_within(&format!(r"{short}\sub"), &long_text));
        }
        // New folders get no 8.3 name where the volume makes none, but Program Files was made with
        // Windows and usually has one (PROGRA~1), as the CI runner's user folder does (RUNNER~1).
        if let Some(program_files) = std::env::var_os("ProgramFiles").map(|p| p.to_string_lossy().into_owned()) {
            let short = short_path(&program_files);
            if short != program_files {
                assert!(short.contains('~'), "{short}");
                assert_eq!(normalize(&format!(r"{short}\Nitely\not-there")), normalize(&format!("{program_files}/Nitely/not-there")));
                assert!(crate::procs::names_path(&format!(r#"node "{short}\Nitely\x\vite.js""#), &format!(r"{program_files}\Nitely\x")));
                assert!(crate::procs::names_path(&format!("node {program_files}/Nitely/x/vite.js"), &format!(r"{short}\Nitely\x")));
            }
        }
        assert_eq!(long_path(&long_text), long_text, "a long path stays as it is");
        assert_eq!(long_path("C:/no/such/place~1/x"), "C:/no/such/place~1/x", "nothing to spell out");
        std::fs::remove_dir_all(top).ok();
    }

    #[test]
    fn within_matches_mixed_separators() {
        assert!(is_within(r"C:\Dev\wt\app-1\web", "C:/Dev/wt/app-1"));
        assert!(is_within("C:/Dev/wt/and-1/", "C:/Dev/wt/and-1"));
        assert!(!is_within("C:/Dev/wt/and-10", "C:/Dev/wt/and-1"));
    }

    #[test]
    fn temp_paths_never_repeat() {
        let made: std::collections::HashSet<PathBuf> = (0..1000).map(|_| unique_temp("nitely-unique", ".txt")).collect();
        assert_eq!(made.len(), 1000);
        assert!(made.iter().all(|p| p.to_string_lossy().ends_with(".txt") && p.starts_with(std::env::temp_dir())));
    }
}
