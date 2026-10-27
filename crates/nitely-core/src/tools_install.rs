//! Installing a missing tool (NIT-688): git, Claude Code, Codex or the GitHub CLI, each with its
//! own documented installer or the OS's package manager, never a download from anywhere else.
//! Every source is in `SOURCES`, one table, with the vendor page that documents it, so a reviewer
//! checks them in one place. The person sees the exact command before it runs (`plan`) and starts
//! it themselves (`install`); a step that needs administrator rights says so, and the OS asks for
//! them, never Nitely. On Linux the commands are shown and never run. The harness never installs.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::process::Stdio;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Macos,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

/// How a source installs: through a package manager the person has, or the vendor's own script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Winget,
    Brew,
    Npm,
    /// The vendor's own installer script, fetched from the vendor.
    Vendor,
    /// The OS's own installer (Apple's command line tools).
    System,
}

/// One documented way to install a tool: on `os`, through `via`, running `argv`. `docs` is the
/// vendor's page that gives this command. `elevates`: the installer asks for administrator rights.
pub struct Source {
    pub tool: &'static str,
    pub os: Os,
    pub via: Via,
    pub argv: &'static [&'static str],
    pub docs: &'static str,
    pub elevates: bool,
}

/// Every way Nitely installs a tool, in the order it prefers them for each tool and platform. A
/// source runs only when what it goes `via` is there (`Available`). Nothing else is ever run.
pub const SOURCES: &[Source] = &[
    // Git
    Source { tool: "git", os: Os::Windows, via: Via::Winget, argv: &["winget", "install", "--id", "Git.Git", "--exact", "--source", "winget", "--accept-package-agreements", "--accept-source-agreements"], docs: "https://git-scm.com/downloads/win", elevates: true },
    Source { tool: "git", os: Os::Macos, via: Via::Brew, argv: &["brew", "install", "git"], docs: "https://git-scm.com/downloads/mac", elevates: false },
    Source { tool: "git", os: Os::Macos, via: Via::System, argv: &["xcode-select", "--install"], docs: "https://git-scm.com/downloads/mac", elevates: false },
    Source { tool: "git", os: Os::Linux, via: Via::System, argv: &["sudo", "apt-get", "install", "git"], docs: "https://git-scm.com/downloads/linux", elevates: true },
    // Claude Code
    Source { tool: "claude", os: Os::Windows, via: Via::Vendor, argv: &["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "irm https://claude.ai/install.ps1 | iex"], docs: "https://docs.claude.com/en/docs/claude-code/setup", elevates: false },
    Source { tool: "claude", os: Os::Macos, via: Via::Brew, argv: &["brew", "install", "--cask", "claude-code"], docs: "https://docs.claude.com/en/docs/claude-code/setup", elevates: false },
    Source { tool: "claude", os: Os::Macos, via: Via::Vendor, argv: &["bash", "-c", "curl -fsSL https://claude.ai/install.sh | bash"], docs: "https://docs.claude.com/en/docs/claude-code/setup", elevates: false },
    Source { tool: "claude", os: Os::Linux, via: Via::Vendor, argv: &["bash", "-c", "curl -fsSL https://claude.ai/install.sh | bash"], docs: "https://docs.claude.com/en/docs/claude-code/setup", elevates: false },
    // Codex
    Source { tool: "codex", os: Os::Windows, via: Via::Npm, argv: &["npm", "install", "--global", "@openai/codex"], docs: "https://github.com/openai/codex#quickstart", elevates: false },
    Source { tool: "codex", os: Os::Macos, via: Via::Brew, argv: &["brew", "install", "--cask", "codex"], docs: "https://github.com/openai/codex#quickstart", elevates: false },
    Source { tool: "codex", os: Os::Macos, via: Via::Npm, argv: &["npm", "install", "--global", "@openai/codex"], docs: "https://github.com/openai/codex#quickstart", elevates: false },
    Source { tool: "codex", os: Os::Linux, via: Via::Npm, argv: &["npm", "install", "--global", "@openai/codex"], docs: "https://github.com/openai/codex#quickstart", elevates: false },
    // The GitHub CLI
    Source { tool: "gh", os: Os::Windows, via: Via::Winget, argv: &["winget", "install", "--id", "GitHub.cli", "--exact", "--source", "winget", "--accept-package-agreements", "--accept-source-agreements"], docs: "https://github.com/cli/cli#windows", elevates: true },
    Source { tool: "gh", os: Os::Macos, via: Via::Brew, argv: &["brew", "install", "gh"], docs: "https://github.com/cli/cli#macos", elevates: false },
    Source { tool: "gh", os: Os::Linux, via: Via::System, argv: &["sudo", "apt-get", "install", "gh"], docs: "https://github.com/cli/cli/blob/trunk/docs/install_linux.md", elevates: true },
];

/// What the person's machine has to install with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Available {
    pub winget: bool,
    pub brew: bool,
    pub npm: bool,
}

impl Available {
    pub fn now() -> Available {
        Available { winget: crate::tools::installed("winget"), brew: crate::tools::installed("brew"), npm: crate::tools::installed("npm") }
    }
    fn has(&self, via: Via) -> bool {
        match via {
            Via::Winget => self.winget,
            Via::Brew => self.brew,
            Via::Npm => self.npm,
            Via::Vendor | Via::System => true,
        }
    }
}

/// How a missing tool would be installed: the exact command, shown before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub tool: String,
    pub command: Vec<String>,
    /// The command as the person reads it.
    pub shown: String,
    pub via: Via,
    pub docs: String,
    /// The installer asks for administrator rights; the OS shows that prompt.
    pub elevates: bool,
    /// Nitely runs it when the person says so; false where it only shows it (Linux).
    pub runs: bool,
    /// Why there is no command to run, or what to do first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The plan for `tool` on `os` with `available`: the first source in `SOURCES` it can use. None
/// for a tool Nitely does not install.
pub fn plan(tool: &str, os: Os, available: Available) -> Option<Plan> {
    let mut sources = SOURCES.iter().filter(|s| s.tool == tool && s.os == os).peekable();
    let first = *sources.peek()?;
    let chosen = sources.find(|s| available.has(s.via));
    let source = chosen.unwrap_or(first);
    let note = match (chosen, source.via) {
        (None, Via::Npm) => Some("Codex installs with npm, which comes with Node.js: install Node.js from nodejs.org first.".to_string()),
        (None, Via::Winget) => Some("winget, Windows' package manager, is missing: install App Installer from the Microsoft Store first.".to_string()),
        (None, _) => Some("Its installer is missing on this machine.".to_string()),
        (Some(_), _) if os == Os::Linux => Some("Run it in a terminal: Nitely does not install on Linux.".to_string()),
        _ => None,
    };
    let command: Vec<String> = source.argv.iter().map(|a| a.to_string()).collect();
    Some(Plan {
        tool: tool.into(),
        shown: shown(&command),
        command,
        via: source.via,
        docs: source.docs.into(),
        elevates: source.elevates,
        runs: chosen.is_some() && os != Os::Linux,
        note,
    })
}

/// A command as a person types it: each argument with a space quoted.
fn shown(argv: &[String]) -> String {
    argv.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" ")
}

/// Every missing tool's plan, on this machine.
pub fn plans() -> Vec<Plan> {
    let available = Available::now();
    crate::tools::TOOLS.iter().filter(|(c, _)| !crate::tools::installed(c)).filter_map(|(c, _)| plan(c, Os::current(), available)).collect()
}

/// Installs `tool` with the plan `plan` shows now, which the person confirmed, passing each line
/// it prints to `line`. Then checks the tool again: what it returns is how it is now. Refused under
/// the test harness, on Linux, and for a plan with nothing to run.
pub fn install(tool: &str, line: &mut dyn FnMut(&str)) -> Result<crate::tools::Tool> {
    if cfg!(test) || std::env::var_os("NITELY_HARNESS").is_some() {
        bail!("the test harness never installs anything");
    }
    let plan = plan(tool, Os::current(), Available::now()).with_context(|| format!("Nitely does not install {tool}"))?;
    if !plan.runs {
        bail!("{}", plan.note.unwrap_or_else(|| format!("run it yourself: {}", plan.shown)));
    }
    line(&format!("> {}", plan.shown));
    let (program, args) = plan.command.split_first().context("an empty install command")?;
    // The program as the OS finds it (npm is a .cmd on Windows).
    let program = crate::tools::find(program).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| program.clone());
    let mut child = crate::playbook::shell_free_command(&program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", plan.shown))?;
    // Errors to the same lines, in the order they come, from a thread of their own.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let err = child.stderr.take().map(|e| {
        let tx = tx.clone();
        std::thread::spawn(move || BufReader::new(e).lines().map_while(Result::ok).for_each(|l| { tx.send(l).ok(); }))
    });
    let out = child.stdout.take().map(|o| std::thread::spawn(move || BufReader::new(o).lines().map_while(Result::ok).for_each(|l| { tx.send(l).ok(); })));
    for l in rx {
        line(&l);
    }
    if let Some(t) = out {
        t.join().ok();
    }
    if let Some(t) = err {
        t.join().ok();
    }
    let status = child.wait()?;
    let now = crate::tools::check().into_iter().find(|t| t.command == tool).context("unknown tool")?;
    if !status.success() && now.path.is_none() {
        bail!("{} exited with {status}, and {} is still not installed", plan.shown, now.name);
    }
    // The installer changed PATH for new processes only: put its folder on this one's, so what
    // Nitely starts next (git, an agent) finds the tool without a restart (NIT-693 review).
    if let Some(dir) = now.path.as_deref().and_then(|p| std::path::Path::new(p).parent()) {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
        if !dirs.iter().any(|d| d == dir) {
            if let Ok(joined) = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(dirs)) {
                std::env::set_var("PATH", joined);
            }
        }
    }
    Ok(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIT-688: which installer each missing tool gets, per platform and what the machine has.
    #[test]
    fn each_tool_gets_its_official_installer_for_the_machine_it_is_on() {
        let none = Available::default();
        let all = Available { winget: true, brew: true, npm: true };
        let p = |tool, os, a| plan(tool, os, a).unwrap();

        let git = p("git", Os::Windows, all);
        assert_eq!((git.via, git.runs, git.elevates), (Via::Winget, true, true), "{git:?}");
        assert!(git.shown.starts_with("winget install --id Git.Git --exact --source winget"));
        let no_winget = p("gh", Os::Windows, none);
        assert!(!no_winget.runs && no_winget.note.unwrap().contains("App Installer"), "winget missing: shown, not run");

        assert_eq!(p("claude", Os::Windows, none).via, Via::Vendor, "Claude Code's own installer needs nothing else");
        assert!(p("claude", Os::Windows, none).shown.contains("https://claude.ai/install.ps1"));
        assert_eq!(p("claude", Os::Macos, all).via, Via::Brew, "Homebrew when the Mac has it");
        assert_eq!(p("claude", Os::Macos, none).via, Via::Vendor, "else the vendor's script");
        assert_eq!(p("git", Os::Macos, none).via, Via::System, "Apple's command line tools");

        let codex = p("codex", Os::Windows, none);
        assert!(!codex.runs && codex.note.unwrap().contains("Node.js"), "npm missing");
        assert!(p("codex", Os::Windows, Available { npm: true, ..none }).runs);

        let linux = p("gh", Os::Linux, all);
        assert!(!linux.runs && linux.shown.contains("apt-get"), "Linux: shown, never run");
        assert!(plan("node", Os::Windows, all).is_none(), "only the four tools");
        assert!(install("git", &mut |_| {}).unwrap_err().to_string().contains("never installs"));
    }

    /// Every source comes from its vendor or the OS: a reviewer reads them here.
    #[test]
    fn every_install_source_is_a_vendor_or_the_os() {
        for s in SOURCES {
            assert!(s.docs.starts_with("https://") && ["git-scm.com", "docs.claude.com", "github.com"].iter().any(|h| s.docs.contains(h)), "{}", s.docs);
            let text = s.argv.join(" ");
            for url in text.split_whitespace().filter(|w| w.starts_with("http")) {
                assert!(url.starts_with("https://claude.ai/"), "{url}");
            }
        }
    }
}
