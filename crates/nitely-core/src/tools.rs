//! What the person has of the tools Nitely works with (NIT-687): git, Claude Code, Codex and the
//! GitHub CLI. For each, where its command is (PATH, then the folders its installer usually puts it
//! in), its version, and whether it is signed in, as far as the tool itself says so. Nothing here
//! reads a token or a credential file: signed-in state comes only from the tool's own status
//! command, read from what it prints, and is "unknown" where the tool has none.
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a `--version` or status command may take before it counts as no answer.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The tools, in the order Settings and `nitely tools` show them: (command, name).
pub const TOOLS: [(&str, &str); 4] = [("git", "Git"), ("claude", "Claude Code"), ("codex", "Codex"), ("gh", "GitHub CLI")];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tool {
    /// Its command: `git`, `claude`, `codex` or `gh`.
    pub command: String,
    pub name: String,
    /// Where its command is; None when it is not installed.
    pub path: Option<String>,
    pub version: Option<String>,
    /// Signed in (true), signed out (false), or None: the tool cannot say, or has no sign-in.
    pub signed_in: Option<bool>,
}

/// Every tool, checked now. Runs each one found for its version and status, a few seconds at most.
pub fn check() -> Vec<Tool> {
    TOOLS.iter().map(|(command, name)| check_one(command, name)).collect()
}

fn check_one(command: &str, name: &str) -> Tool {
    let Some(path) = find(command) else {
        return Tool { command: command.into(), name: name.into(), path: None, version: None, signed_in: None };
    };
    let version = run(&path, &["--version"]).and_then(|(_, out)| version_of(&out));
    let signed_in = status_args(command).and_then(|args| run(&path, args)).and_then(|(ok, out)| signed_in(command, ok, &out));
    Tool { command: command.into(), name: name.into(), path: Some(path.to_string_lossy().into_owned()), version, signed_in }
}

/// Whether `command` is installed: it is on PATH or where its installer puts it.
pub fn installed(command: &str) -> bool {
    find(command).is_some()
}

/// Where `command` is: on PATH, else in the folders its installers use, which a shell started
/// before the install may not have on its PATH yet.
pub fn find(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let mut dirs = path;
    dirs.extend(usual_dirs(command, &env));
    let names = names(command);
    dirs.iter().find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| p.is_file())).or_else(|| {
        // As reviews start it: on Windows the Codex CLI that ships inside the Codex Store app is on
        // no PATH (review::resolve_program).
        let p = PathBuf::from(crate::review::resolve_program(command));
        (p.is_absolute() && p.is_file()).then_some(p)
    })
}

/// The file names `command` goes by here.
fn names(command: &str) -> Vec<String> {
    if cfg!(windows) {
        ["exe", "cmd", "bat"].iter().map(|e| format!("{command}.{e}")).collect()
    } else {
        vec![command.to_string()]
    }
}

/// The folders each tool's installer puts its command in, beyond PATH, read from `env`.
pub fn usual_dirs(command: &str, env: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<PathBuf> {
    let home = env(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let mut dirs = Vec::new();
    let mut add = |base: Option<PathBuf>, rel: &str| {
        if let Some(b) = base {
            dirs.push(if rel.is_empty() { b } else { b.join(rel) });
        }
    };
    if cfg!(windows) {
        match command {
            "git" => {
                add(env("ProgramFiles"), r"Git\cmd");
                add(env("LOCALAPPDATA"), r"Programs\Git\cmd");
            }
            "gh" => {
                add(env("ProgramFiles"), "GitHub CLI");
                add(env("LOCALAPPDATA"), r"Programs\GitHub CLI");
            }
            _ => {}
        }
        // Claude Code's native installer and npm's global folder (Claude Code, Codex).
        add(home.clone(), r".local\bin");
        add(env("APPDATA"), "npm");
        add(env("LOCALAPPDATA"), r"Microsoft\WinGet\Links");
    } else {
        add(home.clone(), ".local/bin");
        add(home.clone(), ".claude/local");
        add(Some(PathBuf::from("/opt/homebrew/bin")), "");
        add(Some(PathBuf::from("/usr/local/bin")), "");
        add(Some(PathBuf::from("/usr/bin")), "");
    }
    dirs
}

/// The command that says whether a tool is signed in, without network or secrets, where it has
/// one. Claude Code has none to rely on offline, so its state is unknown.
fn status_args(command: &str) -> Option<&'static [&'static str]> {
    match command {
        "gh" => Some(&["auth", "status"]),
        "codex" => Some(&["login", "status"]),
        _ => None,
    }
}

/// The version in a `--version` reply: the first word that starts with a digit, without a
/// trailing comma ("git version 2.45.1.windows.1", "gh version 2.60.1 (2024-10-08)",
/// "1.0.30 (Claude Code)", "codex-cli 0.46.0").
pub fn version_of(reply: &str) -> Option<String> {
    reply.lines().next()?.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())).map(|w| w.trim_end_matches(',').to_string())
}

/// Whether `command`'s status reply (`ok`: it exited 0) says it is signed in. Only what it plainly
/// says counts; anything else is unknown, so an unexpected reply never reads as signed out.
pub fn signed_in(command: &str, ok: bool, reply: &str) -> Option<bool> {
    let text = reply.to_ascii_lowercase();
    match command {
        "gh" if text.contains("not logged in") || text.contains("not logged into") => Some(false),
        "gh" if text.contains("logged in to") => Some(true),
        "gh" => Some(ok),
        "codex" if text.contains("not logged in") => Some(false),
        "codex" if text.contains("logged in") => Some(true),
        _ => None,
    }
}

/// Runs `path args` with no window and no input, for at most `TIMEOUT`: whether it exited 0, and
/// what it printed (out and err together). None when it would not start or did not finish.
fn run(path: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut child = crate::playbook::shell_free_command(&path.to_string_lossy())
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIT-687: versions and signed-in state, read from what each tool prints.
    #[test]
    fn versions_and_sign_in_come_from_what_each_tool_prints() {
        assert_eq!(version_of("git version 2.45.1.windows.1\n").as_deref(), Some("2.45.1.windows.1"));
        assert_eq!(version_of("gh version 2.60.1 (2024-10-08)\nhttps://github.com/cli/cli/releases/tag/v2.60.1\n").as_deref(), Some("2.60.1"));
        assert_eq!(version_of("1.0.30 (Claude Code)\n").as_deref(), Some("1.0.30"));
        assert_eq!(version_of("codex-cli 0.46.0\n").as_deref(), Some("0.46.0"));
        assert_eq!(version_of("command not found"), None);

        assert_eq!(signed_in("gh", true, "github.com\n  - Logged in to github.com account sam (keyring)\n"), Some(true));
        assert_eq!(signed_in("gh", false, "You are not logged into any GitHub hosts. To log in, run: gh auth login\n"), Some(false));
        assert_eq!(signed_in("gh", false, "error connecting to api.github.com\n"), Some(false), "gh's own exit code when it says neither");
        assert_eq!(signed_in("codex", true, "Logged in using ChatGPT\n"), Some(true));
        assert_eq!(signed_in("codex", false, "Not logged in\n"), Some(false));
        assert_eq!(signed_in("codex", false, "error: unrecognized subcommand 'login'\n"), None, "an older codex says nothing either way");
        assert_eq!(signed_in("claude", true, "anything"), None, "Claude Code has no offline status");
    }

    /// The installers' folders are looked in too, so a tool installed after the shell started is
    /// found; and a folder whose variable is unset is skipped.
    #[test]
    fn a_tools_usual_folders_come_from_the_environment() {
        let env = |name: &str| match name {
            "USERPROFILE" | "HOME" => Some(PathBuf::from("/home/sam")),
            "ProgramFiles" => Some(PathBuf::from(r"C:\Program Files")),
            _ => None,
        };
        let dirs = usual_dirs("git", &env);
        if cfg!(windows) {
            assert!(dirs.contains(&PathBuf::from(r"C:\Program Files").join(r"Git\cmd")), "{dirs:?}");
            assert!(!dirs.iter().any(|d| d.to_string_lossy().contains("npm")), "APPDATA unset: {dirs:?}");
        } else {
            assert!(dirs.contains(&PathBuf::from("/home/sam/.local/bin")) && dirs.contains(&PathBuf::from("/opt/homebrew/bin")), "{dirs:?}");
        }
    }
}
