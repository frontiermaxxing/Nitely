//! The app driven by a test (scripts/harness/app.mjs): started with `NITELY_HARNESS=1`, a throwaway
//! `NITELY_HOME` and a throwaway `NITELY_AGENT_HOME`. Then it touches nothing of the person's: it
//! refuses their real data folder and their real agents' home (~/.claude, ~/.codex, ~/.cursor and
//! the sessions there), registers no `nitely://` links, keeps the OS keychain shut (a key and
//! entries that live only in this process), never updates itself, and writes its notifications to
//! a file in its data folder instead of showing them, unless `NITELY_HARNESS_TOASTS=1` asks for real
//! ones. `everything_the_app_reaches_outside_its_folders_is_listed` below keeps that list whole.

use std::path::{Path, PathBuf};

/// Whether the app runs under the harness. Errors when the harness was asked for without a data
/// folder of its own: the app must not start then.
pub fn mode() -> Result<bool, String> {
    if std::env::var_os("NITELY_HARNESS").is_none_or(|v| v != "1") {
        return Ok(false);
    }
    check(std::env::var_os("NITELY_HOME").map(PathBuf::from).as_deref(), &nitely_core::paths::real_data_dir())?;
    let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
    check_agents(var("NITELY_AGENT_HOME").as_deref(), nitely_core::paths::real_home().as_deref(), &[("CLAUDE_CONFIG_DIR", var("CLAUDE_CONFIG_DIR")), ("CODEX_HOME", var("CODEX_HOME"))])?;
    Ok(true)
}

/// The harness's agents' home must be given, exist and not be the person's home; an agent's own
/// variable (CLAUDE_CONFIG_DIR, CODEX_HOME) may only point inside it, or the app would read the
/// person's sessions from there.
fn check_agents(agents: Option<&Path>, real: Option<&Path>, vars: &[(&str, Option<PathBuf>)]) -> Result<(), String> {
    let agents = agents.ok_or("NITELY_HARNESS is set without NITELY_AGENT_HOME: the harness needs an agents' home of its own, or Agent setup would read and rewrite the person's ~/.claude, ~/.codex and ~/.cursor")?;
    if real.is_some_and(|r| same(agents, r)) {
        return Err("NITELY_AGENT_HOME is the person's home; the harness only runs on a throwaway one".into());
    }
    if !agents.is_dir() {
        return Err(format!("NITELY_AGENT_HOME {} does not exist; the harness makes it first", agents.display()));
    }
    // Resolved first (links, junctions), and never through `..`, so a path that only looks nested
    // cannot reach the person's sessions.
    let canon = |p: &Path| nitely_core::paths::normalize(&resolved(p).to_string_lossy());
    let inside = |p: &Path| !p.components().any(|c| c == std::path::Component::ParentDir) && canon(p).starts_with(&format!("{}/", canon(agents)));
    for (name, value) in vars {
        if let Some(v) = value.as_deref().filter(|v| !inside(v)) {
            return Err(format!("{name} is {} under the harness, outside NITELY_AGENT_HOME; the harness clears it", v.display()));
        }
    }
    Ok(())
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

/// `p` with its links and junctions resolved, as far as it exists: the nearest folder that does,
/// resolved, and the rest of the path after it.
fn resolved(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = p;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |acc: PathBuf, part| acc.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                at = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
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

    #[test]
    fn refuses_the_real_agents_home_and_their_own_variables_outside_its_own() {
        let real = nitely_core::paths::unique_temp("nitely-real-home", "");
        let mine = nitely_core::paths::unique_temp("nitely-agents", "");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&mine).unwrap();
        assert!(check_agents(None, Some(&real), &[]).unwrap_err().contains("NITELY_AGENT_HOME"));
        assert!(check_agents(Some(&real), Some(&real), &[]).unwrap_err().contains("person's home"));
        assert!(check_agents(Some(&mine.join("missing")), Some(&real), &[]).unwrap_err().contains("does not exist"));
        assert!(check_agents(Some(&mine), Some(&real), &[("CLAUDE_CONFIG_DIR", Some(real.join(".claude")))]).unwrap_err().contains("CLAUDE_CONFIG_DIR"));
        assert!(check_agents(Some(&mine), Some(&real), &[("CODEX_HOME", Some(mine.join(".codex"))), ("CLAUDE_CONFIG_DIR", None)]).is_ok());
        let escape = mine.join(".codex").join("..").join("..").join(real.file_name().unwrap());
        assert!(check_agents(Some(&mine), Some(&real), &[("CODEX_HOME", Some(escape))]).unwrap_err().contains("CODEX_HOME"), "no way out through ..");
        std::fs::remove_dir_all(&real).ok();
        std::fs::remove_dir_all(&mine).ok();
    }

    /// Everything in the app and the core that reaches outside the app's own folders: the OS
    /// keychain, the person's home and agents, registered links, the OS's notifications, scheduled
    /// tasks and login items, the registry, the updater. Each is listed with how the harness keeps
    /// off the person's (this file, scripts/harness/app.mjs). Code that adds one fails here until it
    /// is listed, and so until someone has said how the harness stays clear of it.
    #[test]
    fn everything_the_app_reaches_outside_its_folders_is_listed() {
        const REACHES: &[&str] = &[
            "keyring::",
            "dirs::",
            "register_all",
            "Toast::new",
            "notify_rust::",
            "UNUserNotificationCenter",
            "updater_builder",
            "schtasks",
            "ScheduledTask",
            "LaunchAgents",
            "autostart",
            "winreg",
            "HKEY_",
            "CurrentVersion\\Run",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
        ];
        // (file, what it reaches, how many lines, or 0 for any): why the harness is clear of it.
        const LISTED: &[(&str, &str, usize, &str)] = &[
            ("crates/nitely-core/src/approval.rs", "keyring::", 0, "under the harness the key and every entry live in the process (use_ephemeral_key)"),
            ("crates/nitely-core/src/paths.rs", "dirs::", 0, "the real data folder and home, which harness::check and check_agents refuse"),
            ("crates/nitely-core/src/connect.rs", "dirs::", 1, "agent_home: NITELY_AGENT_HOME, which the harness must set (check_agents)"),
            ("crates/nitely-core/src/usage.rs", "CLAUDE_CONFIG_DIR", 1, "only inside NITELY_AGENT_HOME under the harness (check_agents); app.mjs clears it"),
            ("crates/nitely-core/src/usage.rs", "CODEX_HOME", 1, "only inside NITELY_AGENT_HOME under the harness (check_agents); app.mjs clears it"),
            ("crates/nitely-core/src/connect.rs", "CLAUDE_CONFIG_DIR", 1, "removed from an agent's own command (run_in), which gets the agents' home as its home"),
            ("crates/nitely-core/src/connect.rs", "CODEX_HOME", 1, "as CLAUDE_CONFIG_DIR"),
            ("crates/nitely-core/src/install.rs", "dirs::", 0, "`nitely install`, the CLI's; the app never runs it"),
            ("crates/nitely-core/src/mirror.rs", "dirs::", 1, "the home's path as text, kept out of mirrored issues; nothing is read"),
            ("src-tauri/src/lib.rs", "register_all", 0, "skipped under the harness"),
            ("crates/nitely-core/src/link_service.rs", "CurrentVersion\\Run", 0, "the links listener at login: allowed() refuses it under NITELY_HARNESS and with NITELY_AGENT_HOME set, which the harness sets"),
            ("crates/nitely-core/src/link_service.rs", "LaunchAgents", 0, "as CurrentVersion\\Run"),
            ("crates/nitely-core/src/link_service.rs", "autostart", 0, "as CurrentVersion\\Run"),
            ("src-tauri/src/notify.rs", "Toast::new", 0, "notify() records instead under the harness, unless NITELY_HARNESS_TOASTS=1"),
            ("src-tauri/src/notify.rs", "notify_rust::", 0, "as Toast::new"),
            ("src-tauri/src/notify.rs", "UNUserNotificationCenter", 0, "as Toast::new; init() skips the Mac's center under the harness"),
            ("src-tauri/src/update.rs", "updater_builder", 0, "base_status disables updates under the harness"),
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let mut files = Vec::new();
        let mut dirs = vec![root.join("src-tauri").join("src")];
        for c in std::fs::read_dir(root.join("crates")).unwrap().flatten() {
            dirs.push(c.path().join("src"));
        }
        while let Some(dir) = dirs.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p);
                }
            }
        }
        let mut found = Vec::new();
        for file in &files {
            let rel = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            if rel == "src-tauri/src/harness.rs" {
                continue;
            }
            let text = std::fs::read_to_string(file).unwrap();
            // Tests may reach anything in a scratch folder; only the code that ships counts.
            let code = text.split("#[cfg(test)]").next().unwrap_or("");
            for reach in REACHES {
                let n = code.lines().filter(|l| !l.trim_start().starts_with("//") && l.contains(reach)).count();
                if n > 0 {
                    found.push((rel.clone(), reach.to_string(), n));
                }
            }
        }
        assert!(found.iter().any(|(f, r, _)| f.ends_with("approval.rs") && r == "keyring::"), "the scan found nothing, so it reads no source: {found:?}");
        let unlisted: Vec<String> = found
            .iter()
            .filter(|(f, r, n)| !LISTED.iter().any(|(lf, lr, ln, _)| lf == f && lr == r && (*ln == 0 || ln == n)))
            .map(|(f, r, n)| format!("{f}: {r} ({n} lines)"))
            .collect();
        assert!(
            unlisted.is_empty(),
            "these reach outside the app's own folders and are not listed with how the test harness stays clear of them; add each to LISTED with its harness guard: {unlisted:#?}"
        );
    }
}
