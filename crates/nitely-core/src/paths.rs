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

/// A path in one comparable form: forward slashes, no trailing slash, lowercase on Windows.
/// Git prints `C:/Development/x`, hooks send `C:\Development\x`; both must match.
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
        assert!(is_within(r"C:\Dev\wt\and-1\aera", "C:/Dev/wt/and-1"));
        assert!(is_within("C:/Dev/wt/and-1/", "C:/Dev/wt/and-1"));
        assert!(!is_within("C:/Dev/wt/and-10", "C:/Dev/wt/and-1"));
    }
}
