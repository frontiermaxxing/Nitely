//! Which running processes use a folder: a dev server started in a worktree, a build, a shell
//! whose command `cd`s there. Windows will not delete a folder such a process holds files in, and
//! git then stops halfway through removing a worktree, so land checks this first.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// A process using a folder, by name and pid only: its command line is matched but never kept,
/// since it can carry a token or password.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Proc {
    process_id: u32,
    #[serde(default)]
    parent_process_id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    executable_path: Option<String>,
    #[serde(default)]
    command_line: Option<String>,
    /// The working directory, where the platform tells it (Linux).
    #[serde(skip)]
    cwd: Option<String>,
}

/// Processes whose executable, command line or working directory is inside `dir`, other than this
/// process and the ones that started it. Empty when processes cannot be listed.
pub fn holders(dir: &Path) -> Vec<Holder> {
    let procs = list();
    let me = std::process::id();
    let parents: HashMap<u32, u32> = procs.iter().map(|p| (p.process_id, p.parent_process_id)).collect();
    let mut mine = vec![me];
    while let Some(&parent) = parents.get(mine.last().unwrap()) {
        if parent == 0 || mine.contains(&parent) {
            break;
        }
        mine.push(parent);
    }
    let dir = dir.to_string_lossy();
    procs
        .into_iter()
        .filter(|p| !mine.contains(&p.process_id))
        .filter(|p| [&p.executable_path, &p.command_line, &p.cwd].into_iter().flatten().any(|text| names_path(text, &dir)))
        .map(|p| Holder { pid: p.process_id, name: p.name.unwrap_or_default() })
        .collect()
}

/// Whether `text` names `dir` or a path inside it, in any spelling a process might use:
/// `C:\x\y`, `C:/x/y`, Git Bash's `/c/x/y`, WSL's `/mnt/c/x/y`; case-insensitive on Windows.
pub fn names_path(text: &str, dir: &str) -> bool {
    let text = normalize(text);
    let dir = normalize(dir);
    let mut spellings = vec![dir.clone()];
    if let Some((drive, rest)) = dir.split_once(":/").filter(|(d, _)| d.len() == 1) {
        spellings.push(format!("/{drive}/{rest}"));
        spellings.push(format!("/mnt/{drive}/{rest}"));
    }
    spellings.iter().any(|s| {
        text.match_indices(s.as_str()).any(|(i, _)| {
            let after = text[i + s.len()..].chars().next();
            // A whole path component: not `nit-2` inside `nit-27-x`.
            after.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
        })
    })
}

/// Forward slashes, one at a time, lowercase on Windows (where paths ignore case).
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = if c == '\\' { '/' } else { c };
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    let out = out.trim_end_matches('/').to_string();
    if cfg!(windows) { out.to_lowercase() } else { out }
}

#[cfg(windows)]
fn list() -> Vec<Proc> {
    let script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,CommandLine | ConvertTo-Json -Compress";
    let out = crate::playbook::shell_free_command("powershell").args(["-NoProfile", "-NonInteractive", "-Command", script]).output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<Vec<Proc>>(text.trim()).unwrap_or_default()
}

#[cfg(not(windows))]
fn list() -> Vec<Proc> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,comm=,args="]).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let ppid: u32 = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            let args = parts.collect::<Vec<_>>().join(" ");
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned());
            Some(Proc { process_id: pid, parent_process_id: ppid, name: Some(name), executable_path: None, command_line: Some(args), cwd })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_folder_in_every_spelling() {
        let dir = if cfg!(windows) { r"C:\Development\Nitely-worktrees\nit-27-lanes" } else { "/c/Development/Nitely-worktrees/nit-27-lanes" };
        assert!(names_path("bash -c 'cd /c/Development/Nitely-worktrees/nit-27-lanes && npm run dev'", dir));
        assert!(!names_path("cd /c/Development/Nitely-worktrees/nit-27-lanes-two && npm run dev", dir), "another worktree");
        assert!(!names_path("cd /c/Development/Nitely && cargo test", dir));
        if cfg!(windows) {
            assert!(names_path(r#""node" "C:\Development\Nitely-worktrees\nit-27-lanes\node_modules\.bin\\..\vite\bin\vite.js" --port 5184"#, dir));
            assert!(names_path("wsl -- ls /mnt/c/development/nitely-worktrees/nit-27-lanes/src", dir));
            assert!(names_path("C:/DEVELOPMENT/Nitely-worktrees/nit-27-lanes", dir));
        }
    }

    #[test]
    fn finds_a_process_that_names_the_folder() {
        let dir = std::env::temp_dir().join(format!("nitely-held-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        // A long-running child whose command line names the folder, like a dev server's would.
        let mut child = if cfg!(windows) {
            crate::playbook::shell_free_command("cmd").args(["/C", &format!("ping -n 30 127.0.0.1 >nul & rem {path}")]).spawn().unwrap()
        } else {
            std::process::Command::new("sh").args(["-c", &format!("sleep 30; : {path}")]).spawn().unwrap()
        };
        let found = holders(&dir);
        child.kill().ok();
        child.wait().ok();
        assert!(found.iter().any(|h| h.pid == child.id()), "{found:?}");
        assert!(!found.iter().any(|h| h.pid == std::process::id()), "never this process");
        std::fs::remove_dir_all(&dir).ok();
    }
}
