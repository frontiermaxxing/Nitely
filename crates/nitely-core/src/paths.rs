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

/// A path in one comparable form: forward slashes, no trailing slash, lowercase on Windows.
/// Git prints `C:/Code/x`, hooks send `C:\Code\x`; both must match.
pub fn normalize(path: &str) -> String {
    let mut p = path.replace('\\', "/");
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if cfg!(windows) {
        p = p.to_lowercase();
    }
    p
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
