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
    /// What the setup's sign-in button says (NIT-689): "Sign in" while the tool says it is signed
    /// out, "Run it once to sign in" where it cannot say; None once signed in, when it is not
    /// installed, or when it has no sign-in (git).
    pub sign_in: Option<String>,
}

/// Every tool, checked now. Runs each one found for its version and status, a few seconds at most.
pub fn check() -> Vec<Tool> {
    TOOLS.iter().map(|(command, name)| check_one(command, name)).collect()
}

fn check_one(command: &str, name: &str) -> Tool {
    let Some(path) = find(command) else {
        return Tool { command: command.into(), name: name.into(), path: None, version: None, signed_in: None, sign_in: None };
    };
    let version = run(&path, &["--version"]).and_then(|(_, out)| version_of(&out));
    let signed_in = status_args(command).and_then(|args| run(&path, args)).and_then(|(ok, out)| signed_in(command, ok, &out));
    let sign_in = sign_in_offer(command, true, signed_in).map(str::to_string);
    Tool { command: command.into(), name: name.into(), path: Some(path.to_string_lossy().into_owned()), version, signed_in, sign_in }
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

/// A tool's own sign-in (NIT-689): `gh auth login`, `codex login`, and Claude Code itself, whose
/// first run asks the person to sign in. Git has none. Nitely only starts it in a terminal window
/// of the person's: what they type there, and any token it keeps, never pass through Nitely.
pub fn login_args(command: &str) -> Option<&'static [&'static str]> {
    match command {
        "gh" => Some(&["auth", "login"]),
        "codex" => Some(&["login"]),
        "claude" => Some(&[]),
        _ => None,
    }
}

/// What the setup offers for a tool's sign-in: "Sign in" while it says it is signed out, "Run it
/// once to sign in" where it cannot say (Claude Code, an older Codex), nothing once it is signed
/// in, when it is not installed, or when it has no sign-in.
pub fn sign_in_offer(command: &str, installed: bool, signed_in: Option<bool>) -> Option<&'static str> {
    if !installed || login_args(command).is_none() {
        return None;
    }
    match signed_in {
        Some(true) => None,
        Some(false) => Some("Sign in"),
        None => Some("Run it once to sign in"),
    }
}

/// The systems a terminal window is opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Mac,
    Linux,
}

impl Platform {
    pub fn here() -> Platform {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::Mac
        } else {
            Platform::Linux
        }
    }
}

/// One way to open a terminal window: a program and its arguments. On Windows the arguments are
/// one line handed to `cmd.exe` as written (`raw`), since cmd's quoting is not a program's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub raw: bool,
}

/// The ways to open a terminal window titled `title` that runs `program args`, best first; the
/// first that starts is used. Windows: `start`, which opens the person's default terminal (Windows
/// Terminal or the console host), kept open after the login so its last words can be read. macOS:
/// Terminal.app. Linux: the default terminal (`x-terminal-emulator`), then the common ones.
pub fn terminal_for(platform: Platform, title: &str, program: &str, args: &[&str]) -> Vec<Launch> {
    match platform {
        Platform::Windows => {
            let line = std::iter::once(format!("\"{program}\"")).chain(args.iter().map(|a| a.to_string())).collect::<Vec<_>>().join(" ");
            // cmd /k strips the outer pair of quotes and runs the rest: "path" args.
            let title = title.replace('"', "'");
            vec![Launch { program: "cmd.exe".into(), args: vec![format!("/c start \"{title}\" cmd.exe /k \"{line}\"")], raw: true }]
        }
        Platform::Mac => {
            let sh = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            let line = std::iter::once(sh(program)).chain(args.iter().map(|a| sh(a))).collect::<Vec<_>>().join(" ");
            let script = line.replace('\\', "\\\\").replace('"', "\\\"");
            vec![Launch {
                program: "osascript".into(),
                args: vec!["-e".into(), format!("tell application \"Terminal\" to do script \"{script}\""), "-e".into(), "tell application \"Terminal\" to activate".into()],
                raw: false,
            }]
        }
        Platform::Linux => {
            let run: Vec<String> = std::iter::once(program.to_string()).chain(args.iter().map(|a| a.to_string())).collect();
            let with = |terminal: &str, flag: &str| Launch { program: terminal.into(), args: std::iter::once(flag.to_string()).chain(run.iter().cloned()).collect(), raw: false };
            vec![with("x-terminal-emulator", "-e"), with("gnome-terminal", "--"), with("konsole", "-e"), with("xfce4-terminal", "-x"), with("xterm", "-e")]
        }
    }
}

/// Opens a terminal window running `command`'s own sign-in (`login_args`), for a tool in `TOOLS`
/// that is installed. Returns at once; the setup checks again (`check`) when the person is done.
/// Never under the harness or in tests: nothing opens a window there.
pub fn open_sign_in(command: &str) -> anyhow::Result<()> {
    let Some((_, name)) = TOOLS.iter().find(|(c, _)| *c == command) else { anyhow::bail!("{command} is not a tool Nitely signs in") };
    let Some(args) = login_args(command) else { anyhow::bail!("{name} has no sign-in") };
    let Some(path) = find(command) else { anyhow::bail!("{name} is not installed") };
    if cfg!(test) || std::env::var_os("NITELY_HARNESS").is_some() {
        anyhow::bail!("not opening a terminal under the harness");
    }
    let mut last = None;
    for launch in terminal_for(Platform::here(), &format!("Sign in to {name}"), &path.to_string_lossy(), args) {
        let mut cmd = std::process::Command::new(&launch.program);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // The cmd that runs `start` needs no window of its own; the one `start` opens is the person's.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
            if launch.raw {
                for a in &launch.args {
                    cmd.raw_arg(a);
                }
            }
        }
        if !launch.raw {
            cmd.args(&launch.args);
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                // Reaped when the window closes, so it does not linger as a zombie.
                std::thread::spawn(move || child.wait().ok());
                return Ok(());
            }
            Err(e) => last = Some(format!("{}: {e}", launch.program)),
        }
    }
    anyhow::bail!("could not open a terminal for {name}'s sign-in ({}); run `{}` in one yourself", last.unwrap_or_default(), std::iter::once(command).chain(args.iter().copied()).collect::<Vec<_>>().join(" "))
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

    /// NIT-689: each tool's own sign-in, offered while it is signed out or cannot say, in the
    /// platform's terminal; the path and title quoted so a space or quote in them stays one word.
    #[test]
    fn a_sign_in_opens_the_tools_own_login_in_the_platforms_terminal() {
        assert_eq!(login_args("gh"), Some(&["auth", "login"][..]));
        assert_eq!(login_args("codex"), Some(&["login"][..]));
        assert_eq!(login_args("claude"), Some(&[][..]));
        assert_eq!(login_args("git"), None);
        assert_eq!(sign_in_offer("gh", true, Some(false)), Some("Sign in"));
        assert_eq!(sign_in_offer("claude", true, None), Some("Run it once to sign in"));
        assert_eq!(sign_in_offer("codex", true, Some(true)), None);
        assert_eq!(sign_in_offer("gh", false, None), None, "not installed: the install step comes first");
        assert_eq!(sign_in_offer("git", true, None), None);

        let win = terminal_for(Platform::Windows, "Sign in to GitHub CLI", r"C:\Program Files\GitHub CLI\gh.exe", &["auth", "login"]);
        assert_eq!(win, vec![Launch { program: "cmd.exe".into(), args: vec![r#"/c start "Sign in to GitHub CLI" cmd.exe /k ""C:\Program Files\GitHub CLI\gh.exe" auth login""#.into()], raw: true }]);

        let mac = terminal_for(Platform::Mac, "Sign in to Codex", "/Users/sam/it's here/codex", &["login"]);
        assert_eq!(mac[0].program, "osascript");
        assert_eq!(mac[0].args[1], r#"tell application "Terminal" to do script "'/Users/sam/it'\\''s here/codex' 'login'""#, "AppleScript doubles the shell's backslash");

        let linux = terminal_for(Platform::Linux, "Sign in to Claude Code", "/home/sam/.local/bin/claude", &[]);
        assert_eq!(linux.iter().map(|l| l.program.as_str()).collect::<Vec<_>>(), ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "xterm"]);
        assert_eq!(linux[1].args, ["--", "/home/sam/.local/bin/claude"]);
        assert!(open_sign_in("rm").is_err(), "only the tools it knows");
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
