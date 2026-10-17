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
    /// The working directory, where the platform tells it (Linux, and macOS through lsof).
    #[serde(skip)]
    cwd: Option<String>,
}

/// Processes whose executable, command line or working directory is inside `dir`, other than this
/// process and the ones that started it. Empty when processes cannot be listed.
pub fn holders(dir: &Path) -> Vec<Holder> {
    // A list without this process failed (a busy machine can time out the query), and taking it
    // for "nothing holds the folder" would let land remove a worktree in use; so ask again.
    let me = std::process::id();
    let mut procs = list();
    for _ in 0..3 {
        if procs.iter().any(|p| p.process_id == me) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        procs = list();
    }
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
    let texts = [normalize(text), normalize(&spell_out(text))];
    let mut spellings = Vec::new();
    // Long or 8.3 short (`RUNNER~1`): a process may have been given either (NIT-104).
    for dir in [dir.to_string(), crate::paths::long_path(dir), crate::paths::short_path(dir)] {
        let dir = normalize(&dir);
        if spellings.contains(&dir) {
            continue;
        }
        if let Some((drive, rest)) = dir.split_once(":/").filter(|(d, _)| d.len() == 1) {
            spellings.push(format!("/{drive}/{rest}"));
            spellings.push(format!("/mnt/{drive}/{rest}"));
        }
        spellings.push(dir);
    }
    // On a Mac, `/var/…` as a process was given it, for the `/private/var/…` git names.
    if cfg!(target_os = "macos") {
        let linked: Vec<String> = spellings.iter().filter_map(|s| through_link(s)).collect();
        spellings.extend(linked);
    }
    // As written, or with its short names spelled out.
    texts.iter().any(|text| {
        spellings.iter().any(|s| {
            text.match_indices(s.as_str()).any(|(i, _)| {
                let after = text[i + s.len()..].chars().next();
                // A whole path component: not `nit-2` inside `nit-27-x`.
                after.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
            })
        })
    })
}

/// `/private/var/x` as `/var/x`: the macOS link most programs are handed (also `/tmp`, `/etc`).
fn through_link(dir: &str) -> Option<String> {
    let rest = dir.strip_prefix("/private")?;
    ["/var/", "/tmp/", "/etc/"].iter().any(|d| rest.starts_with(d)).then(|| rest.to_string())
}

/// `text` with the 8.3 short names in its paths spelled out, so a path written in any mix of
/// short and long names (`C:\Users\RUNNER~1\AppData\...\nit-3`) compares in its long form (NIT-135).
/// A path runs from a drive letter to the next quote, `;`, `&` or `|`, and within that to the
/// longest part ending at a space that exists; the rest is kept as given.
fn spell_out(text: &str) -> String {
    if !cfg!(windows) || !text.contains('~') {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let drive = i + 2 < bytes.len() && bytes[i].is_ascii_alphabetic() && bytes[i + 1] == b':' && matches!(bytes[i + 2], b'\\' | b'/');
        if !drive || (i > 0 && bytes[i - 1].is_ascii_alphanumeric()) {
            let c = text[i..].chars().next().unwrap_or(' ');
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let end = text[i..].find(['"', '\'', ';', '&', '|', '\r', '\n']).map_or(text.len(), |n| i + n);
        let path = &text[i..end];
        if path.contains('~') {
            // The longest part up to a space that exists is the path (`...\COMMON~1 --watch`);
            // without one, the part that exists of the whole is spelled out.
            let cut = path.char_indices().filter(|(_, c)| c.is_whitespace()).map(|(k, _)| k).chain([path.len()]).rev().find(|&k| Path::new(&path[..k]).exists());
            let cut = cut.unwrap_or(path.len());
            out.push_str(&crate::paths::long_path(&path[..cut]));
            out.push_str(&path[cut..]);
        } else {
            out.push_str(path);
        }
        i = end;
    }
    out
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
    // By its full path when it is where Windows keeps it, so a narrow PATH does not hide it.
    let system = std::env::var_os("SystemRoot").map(|r| std::path::Path::new(&r).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"));
    let program = system.filter(|p| p.is_file()).map_or_else(|| "powershell".to_string(), |p| p.to_string_lossy().into_owned());
    let out = crate::playbook::shell_free_command(&program).args(["-NoProfile", "-NonInteractive", "-Command", script]).output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str::<Vec<Proc>>(text.trim()).unwrap_or_default()
}

#[cfg(not(windows))]
fn list() -> Vec<Proc> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "pid=,ppid=,comm=,args="]).output() else { return Vec::new() };
    let dirs = working_dirs();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid: u32 = parts.next()?.parse().ok()?;
            let ppid: u32 = parts.next()?.parse().ok()?;
            let name = parts.next()?.to_string();
            let args = parts.collect::<Vec<_>>().join(" ");
            let cwd = dirs.get(&pid).cloned().or_else(|| std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned()));
            Some(Proc { process_id: pid, parent_process_id: ppid, name: Some(name), executable_path: None, command_line: Some(args), cwd })
        })
        .collect()
}

/// Each process's working directory where there is no /proc to read it from (macOS): one `lsof`
/// call for them all. Processes of other users are left out, as the system does.
#[cfg(target_os = "macos")]
fn working_dirs() -> HashMap<u32, String> {
    let Ok(out) = std::process::Command::new("lsof").args(["-n", "-P", "-w", "-d", "cwd", "-F", "pn"]).output() else { return HashMap::new() };
    parse_lsof(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn working_dirs() -> HashMap<u32, String> {
    HashMap::new()
}

/// `lsof -F pn` output: a `p<pid>` line, then the `n<path>` of its files.
#[cfg(any(unix, test))]
fn parse_lsof(text: &str) -> HashMap<u32, String> {
    let mut dirs = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let (Some(path), Some(p)) = (line.strip_prefix('n'), pid) {
            dirs.insert(p, path.to_string());
        }
    }
    dirs
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn reads_working_dirs_from_lsof() {
        let dirs = parse_lsof("p12\nfcwd\nn/Users/sam/app\np40\nfcwd\nn/private/var/folders/x y\n");
        assert_eq!(dirs.get(&12).map(String::as_str), Some("/Users/sam/app"));
        assert_eq!(dirs.get(&40).map(String::as_str), Some("/private/var/folders/x y"));
        assert_eq!(dirs.len(), 2);
        assert_eq!(through_link("/private/var/folders/x/wt").as_deref(), Some("/var/folders/x/wt"));
        assert_eq!(through_link("/private/stuff/x"), None);
        assert_eq!(through_link("/Users/sam/app"), None);
    }

    #[test]
    fn names_a_folder_in_every_spelling() {
        let dir = if cfg!(windows) { r"C:\Code\Nitely-worktrees\nit-27-lanes" } else { "/c/Code/Nitely-worktrees/nit-27-lanes" };
        assert!(names_path("bash -c 'cd /c/Code/Nitely-worktrees/nit-27-lanes && npm run dev'", dir));
        assert!(!names_path("cd /c/Code/Nitely-worktrees/nit-27-lanes-two && npm run dev", dir), "another worktree");
        assert!(!names_path("cd /c/Code/Nitely && cargo test", dir));
        if cfg!(windows) {
            assert!(names_path(r#""node" "C:\Code\Nitely-worktrees\nit-27-lanes\node_modules\.bin\\..\vite\bin\vite.js" --port 5184"#, dir));
            assert!(names_path("wsl -- ls /mnt/c/code/nitely-worktrees/nit-27-lanes/src", dir));
            assert!(names_path("C:/CODE/Nitely-worktrees/nit-27-lanes", dir));
        }
    }

    #[test]
    fn names_a_folder_written_in_a_mix_of_short_and_long_names() {
        // As on the CI runner: the user folder short (RUNNER~1), the folders below it long, and land
        // asking about git's fully long path (NIT-135). Program Files has a short name where the
        // volume makes them; new folders here get none.
        let Some(program_files) = std::env::var_os("ProgramFiles").map(|p| p.to_string_lossy().into_owned()) else { return };
        let short = crate::paths::short_path(&program_files);
        if !cfg!(windows) || short == program_files {
            return;
        }
        let long = format!(r"{program_files}\Common Files\nit-3-x");
        assert!(names_path(&format!(r#"sh -c "sleep 30; : {short}\Common Files\nit-3-x""#), &long));
        assert!(names_path(&format!(r"node {short}\Common Files\nit-3-x\vite.js --port 5184"), &long));
        assert!(names_path(&format!("cd {}/Common Files/nit-3-x && npm run dev", short.replace('\\', "/")), &long));
        assert!(!names_path(&format!(r"cd {short}\Common Files\nit-3-xy"), &long), "another folder");
        // A short last folder followed by arguments, matched as written or spelled out.
        let common = format!(r"{program_files}\Common Files");
        let common_short = crate::paths::short_path(&common);
        assert!(names_path(&format!("node {common_short} --watch"), &common), "{common_short}");
        assert!(names_path(&format!(r"node {short}\Common Files --watch"), &common_short));
    }

    /// `holders(dir)` once it names `pid`, or as it last was after a few seconds.
    pub(crate) fn held_by(dir: &Path, pid: u32) -> Vec<Holder> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let found = holders(dir);
            if found.iter().any(|h| h.pid == pid) || std::time::Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    #[test]
    fn finds_a_process_that_names_the_folder() {
        let dir = crate::paths::unique_temp("nitely-held", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        // A long-running child whose command line names the folder, like a dev server's would.
        let mut child = if cfg!(windows) {
            crate::playbook::shell_free_command("cmd").args(["/C", &format!("ping -n 30 127.0.0.1 >nul & rem {path}")]).spawn().unwrap()
        } else {
            std::process::Command::new("sh").args(["-c", &format!("sleep 30; : {path}")]).spawn().unwrap()
        };
        let found = held_by(&dir, child.id());
        child.kill().ok();
        child.wait().ok();
        assert!(found.iter().any(|h| h.pid == child.id()), "{found:?}");
        assert!(!found.iter().any(|h| h.pid == std::process::id()), "never this process");
        std::fs::remove_dir_all(&dir).ok();
    }
}
