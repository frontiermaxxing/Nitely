//! The app driven by a test (scripts/app-harness.mjs): started with `NITELY_HARNESS=1` and a
//! throwaway `NITELY_HOME`. Then it touches nothing of the person's: it refuses their real data
//! folder, registers no `nitely://` links, signs approvals with a key that lives only in this
//! process (never the OS keychain), and writes its notifications to a file in its data folder
//! instead of showing them, unless `NITELY_HARNESS_TOASTS=1` asks for real ones.

use std::path::{Path, PathBuf};

/// Whether the app runs under the harness. Errors when the harness was asked for without a data
/// folder of its own: the app must not start then.
pub fn mode() -> Result<bool, String> {
    if std::env::var_os("NITELY_HARNESS").is_none_or(|v| v != "1") {
        return Ok(false);
    }
    check(std::env::var_os("NITELY_HOME").map(PathBuf::from).as_deref(), &nitely_core::paths::real_data_dir())?;
    Ok(true)
}

/// The harness's data folder must be given, exist, and be neither the person's real one nor in it.
fn check(home: Option<&Path>, real: &Path) -> Result<(), String> {
    let home = home.ok_or("NITELY_HARNESS is set without NITELY_HOME: the harness needs a data folder of its own")?;
    if same(home, real) || nitely_core::paths::normalize(&home.to_string_lossy()).starts_with(&format!("{}/", nitely_core::paths::normalize(&real.to_string_lossy()))) {
        return Err(format!("NITELY_HOME is the person's real data folder ({}); the harness only runs on a throwaway one", real.display()));
    }
    if !home.is_dir() {
        return Err(format!("NITELY_HOME {} does not exist; the harness makes it and seeds it first", home.display()));
    }
    Ok(())
}

/// True once `mode()` said so at start.
pub fn on() -> bool {
    ON.load(std::sync::atomic::Ordering::Relaxed)
}

static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set(on: bool) {
    ON.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether harness notifications are shown as real toasts too (a live check of them).
pub fn real_toasts() -> bool {
    std::env::var_os("NITELY_HARNESS_TOASTS").is_some_and(|v| v == "1")
}

/// Appends a notification the app asked for to `<NITELY_HOME>/harness-notifications.jsonl`, where
/// the test reads it.
pub fn record_notification(title: &str, body: &str, decision: Option<&str>) {
    let Some(home) = std::env::var_os("NITELY_HOME") else { return };
    let line = serde_json::json!({ "title": title, "body": body, "decision": decision, "at": nitely_core::db::now() });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(Path::new(&home).join("harness-notifications.jsonl")) {
        let _ = writeln!(f, "{line}");
    }
}

/// Writes the taskbar count the app last set (and whether it drew an overlay for it) to
/// `<NITELY_HOME>/harness-badge.json`.
pub fn record_badge(count: u32, drawn: bool) {
    let Some(home) = std::env::var_os("NITELY_HOME") else { return };
    let _ = std::fs::write(Path::new(&home).join("harness-badge.json"), serde_json::json!({ "count": count, "drawn": drawn }).to_string());
}

fn same(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    nitely_core::paths::normalize(&canon(a).to_string_lossy()) == nitely_core::paths::normalize(&canon(b).to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_the_real_data_folder_and_one_it_was_not_given() {
        let real = nitely_core::paths::unique_temp("nitely-real", "");
        let mine = nitely_core::paths::unique_temp("nitely-harness", "");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        assert!(check(None, &real).is_err());
        assert!(check(Some(&real), &real).unwrap_err().contains("real data folder"));
        assert!(check(Some(&real.join("sub")), &real).unwrap_err().contains("real data folder"));
        assert!(check(Some(&mine.join("missing")), &real).unwrap_err().contains("does not exist"));
        assert!(check(Some(&mine), &real).is_ok());
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(&mine).ok();
    }
}
