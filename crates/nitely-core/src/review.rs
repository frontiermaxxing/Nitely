//! Cross-vendor review: another agent reads the branch before it can land (Codex reviews Claude's
//! branches, Claude reviews Codex's). Every reviewer gets the same prompt and must end with the same
//! closing lines, so a review always has an explicit result; one without them is unfinished, not
//! clean. The reviewers, their commands and the yes/no questions come from the playbook.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::playbook::{shell_free_command, tail};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewRules {
    /// Who reviews whose branches: the branch's vendor to the reviewer. `*` covers the rest.
    pub reviewer: BTreeMap<String, String>,
    /// How to run each reviewer. The prompt goes on stdin; an argument `{output}` is replaced with a
    /// file the reviewer writes its reply to, otherwise the reply is read from stdout. Each runs as a
    /// fresh read-only session with no memory of writing the change, so a same-vendor review is
    /// still a second reading. Cursor has no default: `cursor-agent -p` can write and run anything,
    /// and its read-only `--mode ask` has not been shown to run the `git diff` a review needs.
    pub commands: BTreeMap<String, Vec<String>>,
    /// Yes-or-no questions the reviewer answers; a yes sends the change to a human. Use them for
    /// what path rules cannot see, like money logic in a file whose name does not say so.
    pub flags: Vec<Flag>,
    /// Extra instructions for the reviewer, e.g. what matters most in this codebase.
    pub focus: String,
    pub timeout_minutes: u64,
    /// A review by the author's own vendor (because no other reviewer is installed, or the
    /// `reviewer` map says so) sends the change to a human instead of approving it.
    pub require_cross_vendor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flag {
    /// An uppercase name, e.g. MONEY_OR_RELEASE; it becomes the reviewer's closing line.
    pub name: String,
    /// When the answer is yes.
    pub question: String,
    /// Shown to the human when it is yes; defaults to the question.
    #[serde(default)]
    pub reason: String,
}

impl Default for ReviewRules {
    fn default() -> Self {
        let pairs = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        ReviewRules {
            reviewer: pairs(&[("claude", "codex"), ("codex", "claude"), ("*", "claude")]),
            commands: BTreeMap::from([
                (
                    "claude".to_string(),
                    args(&["claude", "-p", "--allowedTools", "Read,Grep,Glob,Bash(git diff:*),Bash(git log:*),Bash(git show:*)"]),
                ),
                ("codex".to_string(), args(&["codex", "exec", "--sandbox", "read-only", "--ephemeral", "-o", "{output}", "-"])),
            ]),
            flags: vec![],
            focus: String::new(),
            timeout_minutes: 20,
            require_cross_vendor: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub priority: u8,
    pub text: String,
    /// Raised in an earlier round and not fixed since; Nitely carries it forward, the reviewer
    /// does not list it again.
    #[serde(default)]
    pub carried: bool,
}

/// What the previous review of the branch found, so the next one checks those findings first and
/// reviews what changed since instead of starting over.
#[derive(Debug, Clone)]
pub struct PreviousRound {
    /// The commit that round reviewed.
    pub commit: String,
    pub findings: Vec<Finding>,
    /// Whether `commit` is still in the branch's history. After a rebase it is not, so there is no
    /// diff since that round and the whole branch is reviewed again.
    pub in_history: bool,
}

/// A previous round's finding and what this round says about it: `fixed`, `open`, or
/// `unchecked` when the reviewer did not say.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recheck {
    pub priority: u8,
    pub text: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    /// `pass` (no P0 or P1 findings) or `changes`.
    pub verdict: String,
    pub findings: Vec<Finding>,
    /// The flags the reviewer answered yes to.
    pub flagged: Vec<String>,
    pub summary: String,
    pub seconds: u64,
    /// The previous round's findings, each marked fixed, open or unchecked. Open and unchecked
    /// ones are also in `findings`, marked carried.
    #[serde(default)]
    pub previous: Vec<Recheck>,
    /// How independent the review was, when it was not the playbook's choice of another vendor:
    /// e.g. `same vendor: codex not installed`. Empty otherwise.
    #[serde(default)]
    pub note: String,
}

/// The reviewer `choose` picked for a branch.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub reviewer: String,
    /// The reviewer is the author's own vendor.
    pub same_vendor: bool,
    /// As `Review::note`.
    pub note: String,
}

impl ReviewRules {
    pub fn reviewer_for(&self, vendor: &str) -> Option<&str> {
        self.reviewer.get(vendor).or_else(|| self.reviewer.get("*")).map(String::as_str)
    }

    /// Whether the reviewer's CLI is on this machine. A relative path counts from `worktree`,
    /// where the reviewer runs.
    pub fn is_installed(&self, reviewer: &str, worktree: &Path) -> bool {
        self.commands.get(reviewer).and_then(|c| c.first()).is_some_and(|p| program_exists(&resolve_program(p), worktree))
    }

    /// Who reviews a branch by `vendor`, among the reviewers `installed` says can run. The
    /// `reviewer` map is the preference; when that one is missing, another vendor that is
    /// installed reviews, and only when none is, the author's own vendor in a fresh session.
    pub fn choose(&self, vendor: &str, installed: &dyn Fn(&str) -> bool) -> Result<Choice> {
        let preferred = self.reviewer_for(vendor).filter(|p| self.commands.contains_key(*p));
        let same = |r: &str| !vendor.is_empty() && r == vendor;
        let others = || self.commands.keys().map(String::as_str).filter(move |r| Some(*r) != preferred);
        let candidates: Vec<&str> =
            preferred.into_iter().chain(others().filter(|r| !same(r))).chain(others().filter(|r| same(r))).collect();
        let Some(reviewer) = candidates.iter().copied().find(|r| installed(r)) else {
            if candidates.is_empty() {
                bail!("the playbook has no reviewer commands (add review.commands)");
            }
            let wanted: Vec<String> = candidates.iter().map(|r| install_hint(r, self)).collect();
            bail!(
                "no reviewer can run on this machine: install {} (or point review.commands in the playbook at a reviewer you have)",
                wanted.join(" or ")
            );
        };
        let missing = self.reviewer_for(vendor).filter(|p| *p != reviewer);
        let same_vendor = same(reviewer);
        let note = match (same_vendor, missing) {
            (true, Some(m)) => format!("same vendor: {m} not installed"),
            (true, None) => "same vendor".to_string(),
            (false, Some(m)) => format!("{m} not installed"),
            (false, None) => String::new(),
        };
        Ok(Choice { reviewer: reviewer.to_string(), same_vendor, note })
    }

    /// Why a review needs a human because of who ran it: a same-vendor review when the playbook
    /// requires another vendor's.
    pub fn independence_reason(&self, choice: &Choice) -> Option<String> {
        (self.require_cross_vendor && choice.same_vendor).then(|| {
            format!("{} review ({}), and this playbook requires a review by another vendor", choice.reviewer, choice.note)
        })
    }

    /// The reviewer's instructions. With a previous round, it first checks that round's findings,
    /// then reviews what changed since, and raises new problems in code that round already saw
    /// only at P0 or P1, so rounds converge instead of finding fresh edge cases each time.
    pub fn prompt(&self, base: &str, previous: Option<&PreviousRound>) -> String {
        let mut p = match previous {
            None => format!(
                "Review the changes on this branch against {base}. Run `git diff {base}...HEAD`, read the changed code and \
its callers, and look for correctness bugs and regressions. Skip style. Do not edit anything.\n"
            ),
            Some(prev) if prev.in_history => format!(
                "This branch was reviewed before, at commit {c}. Run `git diff {base}...HEAD` for the whole change and \
`git diff {c} HEAD` for what changed since that review. Read the changed code and its callers, and look for \
correctness bugs and regressions. Skip style. Do not edit anything.\n",
                c = prev.commit
            ),
            Some(prev) => format!(
                "This branch was reviewed before, at commit {c}, and has been rebased since, so review the whole change \
again. Run `git diff {base}...HEAD`, read the changed code and its callers, and look for correctness bugs and \
regressions. Skip style. Do not edit anything.\n",
                c = prev.commit
            ),
        };
        if let Some(prev) = previous.filter(|p| !p.findings.is_empty()) {
            p.push_str("\nFirst, check each finding of that review against the current code:\n");
            for (i, f) in prev.findings.iter().enumerate() {
                p.push_str(&format!("{}. [P{}] {}\n", i + 1, f.priority, f.text));
            }
            p.push_str(
                "Answer each on its own line as `PREVIOUS <n>: fixed` or `PREVIOUS <n>: open`, with a few words after it \
if useful. Do not list an open one again as a defect: it stays on record by itself.\n",
            );
        }
        if let Some(prev) = previous.filter(|p| p.in_history) {
            p.push_str(&format!(
                "\nThen review what changed since {c} and report any defect in it. The rest of the branch was already \
reviewed: raise something new there only if it is P0 or P1.\n",
                c = prev.commit
            ));
        }
        if !self.focus.trim().is_empty() {
            p.push_str(&format!("\n{}\n", self.focus.trim()));
        }
        p.push_str(
            "\nReport each defect on its own line as:\n- [P0] title: file, symbol: what goes wrong and when\n\
using P0 for data loss or security, P1 for a definite bug in normal use, P2 for an edge case, P3 for a minor issue.\n\n",
        );
        p.push_str(&format!("Then end your reply with exactly these {} lines:\nVERDICT: <findings or clean>\n", 1 + self.flags.len()));
        for f in &self.flags {
            p.push_str(&format!("{}: <yes or no>\n", f.name));
        }
        p.push_str("VERDICT is findings if you listed any new defect, otherwise clean.");
        for f in &self.flags {
            p.push_str(&format!(" {} is yes if {}", f.name, f.question.trim_end_matches('.')));
            p.push('.');
        }
        if !self.flags.is_empty() {
            p.push_str(" Answer yes when unsure.");
        }
        p
    }

    /// Parses a reviewer's reply. Fails when a closing line is missing, or when it claims findings
    /// but lists none (and marks no previous finding open): both mean the review did not finish.
    /// Previous findings it marks open, or does not mention, are carried into `findings` with their
    /// old priority, so the verdict still counts them.
    pub fn parse(&self, reviewer: &str, text: &str, seconds: u64, previous: Option<&PreviousRound>) -> Result<Review> {
        let field = |name: &str, values: &[&str]| -> Option<String> {
            let prefix = format!("{}:", name.to_lowercase());
            text.lines()
                .filter_map(|l| {
                    let l = l.trim().to_lowercase();
                    l.strip_prefix(&prefix).map(|v| v.trim().trim_matches('*').trim().to_string())
                })
                .next_back()
                .filter(|v| values.contains(&v.as_str()))
        };
        let Some(verdict) = field("VERDICT", &["findings", "clean"]) else {
            bail!("the {reviewer} review did not end with a VERDICT line, so it is unfinished:\n{}", tail_lines(text));
        };
        let mut flagged = Vec::new();
        for f in &self.flags {
            match field(&f.name, &["yes", "no"]).as_deref() {
                Some("yes") => flagged.push(f.name.clone()),
                Some(_) => {}
                None => bail!("the {reviewer} review did not answer {}, so it is unfinished:\n{}", f.name, tail_lines(text)),
            }
        }
        let mut findings = parse_findings(text);
        let statuses = parse_previous(text);
        let previous: Vec<Recheck> = previous
            .map(|prev| prev.findings.iter().enumerate().map(|(i, f)| Recheck {
                priority: f.priority,
                text: f.text.clone(),
                status: statuses.get(&(i + 1)).cloned().unwrap_or_else(|| "unchecked".into()),
            }).collect())
            .unwrap_or_default();
        if verdict == "findings" && findings.is_empty() && !previous.iter().any(|r| r.status == "open") {
            bail!("the {reviewer} review says it has findings but lists none as `- [Pn] ...`:\n{}", tail_lines(text));
        }
        for r in previous.iter().filter(|r| r.status != "fixed") {
            findings.push(Finding { priority: r.priority, text: r.text.clone(), carried: true });
        }
        let closing = |l: &str| {
            let l = l.trim().to_uppercase();
            l.starts_with("VERDICT:")
                || l.trim_start_matches(['-', '*', ' ']).starts_with("PREVIOUS ")
                || self.flags.iter().any(|f| l.starts_with(&format!("{}:", f.name)))
        };
        let summary = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("- [P") && !closing(l))
            .unwrap_or("")
            .to_string();
        let verdict = if findings.iter().any(|f| f.priority <= 1) { "changes" } else { "pass" };
        Ok(Review { reviewer: reviewer.into(), verdict: verdict.into(), findings, flagged, summary, seconds, previous, note: String::new() })
    }

    /// The human-facing reasons for the flags a review raised.
    pub fn flag_reasons(&self, review: &Review) -> Vec<String> {
        self.flags
            .iter()
            .filter(|f| review.flagged.contains(&f.name))
            .map(|f| format!("{} review: {}", review.reviewer, if f.reason.is_empty() { &f.question } else { &f.reason }))
            .collect()
    }

    /// Runs `reviewer` in `worktree` and parses its reply.
    pub fn run(&self, reviewer: &str, worktree: &Path, base: &str, previous: Option<&PreviousRound>) -> Result<Review> {
        let command = self
            .commands
            .get(reviewer)
            .filter(|c| !c.is_empty())
            .with_context(|| format!("the playbook has no command for the {reviewer} reviewer (add review.commands.{reviewer})"))?;
        let started = Instant::now();
        let out_file = std::env::temp_dir().join(format!("nitely-review-{}-{}.txt", std::process::id(), crate::db::now()));
        let uses_file = command.iter().any(|a| a.contains("{output}"));
        let program = resolve_program(&command[0]);
        let codex_servers = if cli_name(&command[0]) == "codex" { codex_nitely_servers(&program, worktree) } else { vec![] };
        let command = without_nitely(command, &codex_servers);
        let args: Vec<String> = command[1..].iter().map(|a| a.replace("{output}", &out_file.to_string_lossy())).collect();

        let mut cmd = shell_free_command(&program);
        cmd.args(&args).current_dir(worktree).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        without_session_markers(&mut cmd);
        let mut child = cmd.spawn().with_context(|| format!("starting the {reviewer} reviewer ({})", command[0]))?;
        let mut stdin = child.stdin.take().unwrap();
        let prompt = self.prompt(base, previous);
        let writer = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let out_reader = std::thread::spawn(move || {
            let mut s = String::new();
            stdout.read_to_string(&mut s).map(|_| s)
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            stderr.read_to_string(&mut s).map(|_| s)
        });

        let deadline = Duration::from_secs(self.timeout_minutes.max(1) * 60);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() > deadline {
                child.kill().ok();
                child.wait().ok();
                bail!("the {reviewer} review took longer than {} minutes and was stopped", self.timeout_minutes);
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        writer.join().ok();
        let stdout = out_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let stderr = err_reader.join().ok().and_then(|r| r.ok()).unwrap_or_default();
        let text = if uses_file { std::fs::read_to_string(&out_file).unwrap_or_default() } else { stdout.clone() };
        std::fs::remove_file(&out_file).ok();
        if !status.success() {
            bail!("the {reviewer} review failed ({status}):\n{}", tail(&format!("{stdout}{stderr}")));
        }
        self.parse(reviewer, &text, started.elapsed().as_secs(), previous)
    }
}

/// `PREVIOUS <n>: fixed` and `PREVIOUS <n>: open` lines, by n; the last line for an n wins.
fn parse_previous(text: &str) -> BTreeMap<usize, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.replace("**", "").replace('`', "").to_lowercase();
        let Some(rest) = l.trim().trim_start_matches(['-', '*', ' ']).strip_prefix("previous ") else { continue };
        let Some((n, status)) = rest.split_once(':') else { continue };
        let Ok(n) = n.trim().trim_start_matches('#').parse::<usize>() else { continue };
        let word: String = status.trim().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if matches!(word.as_str(), "fixed" | "open") {
            out.insert(n, word);
        }
    }
    out
}

/// `- [Pn] text` lines, each once.
pub fn parse_findings(text: &str) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix("- [P").or_else(|| l.strip_prefix("* [P")) else { continue };
        let mut chars = rest.chars();
        let (Some(d), Some(']')) = (chars.next(), chars.next()) else { continue };
        let Some(priority) = d.to_digit(10).filter(|p| *p <= 3) else { continue };
        let text = chars.as_str().trim().to_string();
        if !text.is_empty() && !out.iter().any(|f| f.text == text) {
            out.push(Finding { priority: priority as u8, text, carried: false });
        }
    }
    out
}

fn tail_lines(text: &str) -> String {
    let t = text.trim();
    let start = t.char_indices().rev().nth(800).map_or(0, |(i, _)| i);
    t[start..].to_string()
}

/// A reviewer started from inside an agent session must not think it is that session.
fn without_session_markers(cmd: &mut std::process::Command) {
    for (name, _) in std::env::vars() {
        if is_session_marker(&name) {
            cmd.env_remove(name);
        }
    }
}

/// The CLI a command runs, e.g. `codex` for `C:\...\codex.exe`.
fn cli_name(program: &str) -> String {
    Path::new(program).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// The reviewer's command with Nitely's MCP server taken away: a reviewer reads the diff and
/// answers, and one that can report, note or move issues is a second actor on the board. Claude
/// starts with no MCP servers at all. Codex cannot drop its server table from the command line and
/// refuses to turn off a server it does not have, so each server in `codex_servers` (the Nitely
/// ones Codex has, from `codex_nitely_servers`) is turned off by name. Other commands are as given.
pub fn without_nitely(command: &[String], codex_servers: &[String]) -> Vec<String> {
    let mut out = command.to_vec();
    match cli_name(&command[0]).as_str() {
        "claude" if !command.iter().any(|a| a == "--strict-mcp-config") => out.insert(1, "--strict-mcp-config".into()),
        "codex" => {
            let at = command.iter().position(|a| a == "exec").map_or(1, |i| i + 1);
            for (i, name) in codex_servers.iter().enumerate() {
                out.splice(at + 2 * i..at + 2 * i, ["-c".to_string(), format!("mcp_servers.{name}.enabled=false")]);
            }
        }
        _ => {}
    }
    out
}

/// The MCP servers Codex would start in `worktree` (user and project config) that are Nitely's:
/// named `nitely`, or a nitely executable run with `mcp`. Empty when Codex cannot list them.
fn codex_nitely_servers(program: &str, worktree: &Path) -> Vec<String> {
    let mut cmd = shell_free_command(program);
    cmd.args(["mcp", "list", "--json"]).current_dir(worktree).stdin(Stdio::null()).stderr(Stdio::null());
    without_session_markers(&mut cmd);
    cmd.output().ok().filter(|o| o.status.success()).map(|o| nitely_servers(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

/// The Nitely servers in `codex mcp list --json` output. Only plain names are kept, since a name
/// goes into a `-c` key path.
fn nitely_servers(json: &str) -> Vec<String> {
    let Ok(serde_json::Value::Array(servers)) = serde_json::from_str(json) else { return vec![] };
    servers
        .iter()
        .filter(|s| s.get("enabled").and_then(serde_json::Value::as_bool) != Some(false))
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?;
            let transport = s.get("transport");
            let command = transport.and_then(|t| t.get("command")).and_then(serde_json::Value::as_str);
            let args = transport
                .and_then(|t| t.get("args"))
                .and_then(serde_json::Value::as_array)
                .and_then(|a| a.iter().map(serde_json::Value::as_str).collect::<Option<Vec<_>>>());
            let ours = name == "nitely" || crate::connect::is_nitely_mcp(command, args);
            let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            (ours && plain).then(|| name.to_string())
        })
        .collect()
}

/// Variables that make a CLI think it runs inside another agent's session.
pub(crate) fn is_session_marker(name: &str) -> bool {
    name == "CLAUDECODE"
        || name == "CLAUDE_PID"
        || name.starts_with("CLAUDE_CODE_SESSION")
        || name.starts_with("CLAUDE_CODE_HOST_SESSION")
        || name.starts_with("CLAUDE_CODE_CHILD_SESSION")
        || name.starts_with("CLAUDE_CODE_ENTRYPOINT")
        || name.starts_with("CLAUDE_CODE_EXECPATH")
        || name.starts_with("CLAUDE_CODE_SDK")
        || name.starts_with("CLAUDE_AGENT_SDK")
}

/// The program to start for a reviewer command's first word. On Windows a CLI installed with npm
/// is a `.cmd` file, which is started by its full path, and the Codex CLI that ships inside the
/// Codex Store app is not on PATH at all.
pub(crate) fn resolve_program(program: &str) -> String {
    #[cfg(windows)]
    if Path::new(program).extension().is_none() && Path::new(program).components().count() == 1 && which(program).is_none() {
        if let Some(script) = which_with(program, &["cmd", "bat"]) {
            return script.to_string_lossy().into_owned();
        }
        if program == "codex" {
            let out = shell_free_command("powershell")
                .args(["-NoProfile", "-Command", "(Get-AppxPackage -Name 'OpenAI.Codex*' | Select-Object -First 1).InstallLocation"])
                .output();
            if let Ok(out) = out {
                let exe = Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("app").join("resources").join("codex.exe");
                if exe.exists() {
                    return exe.to_string_lossy().into_owned();
                }
            }
        }
    }
    program.to_string()
}

/// Whether `program` can be started from `dir`: a path that exists, or a name found on PATH.
fn program_exists(program: &str, dir: &Path) -> bool {
    let path = &dir.join(program);
    if Path::new(program).components().count() > 1 {
        return path.is_file() || (cfg!(windows) && path.extension().is_none() && path.with_extension("exe").is_file());
    }
    which(program).is_some()
}

/// `program` on PATH, as the OS would start it.
fn which(program: &str) -> Option<std::path::PathBuf> {
    if cfg!(windows) && Path::new(program).extension().is_none() {
        which_with(program, &["exe"])
    } else {
        which_with(program, &[""])
    }
}

fn which_with(program: &str, extensions: &[&str]) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| if ext.is_empty() { dir.join(program) } else { dir.join(format!("{program}.{ext}")) })
            .find(|p| p.is_file())
    })
}

/// "Codex (the `codex` CLI)", for the error that says what to install.
fn install_hint(reviewer: &str, rules: &ReviewRules) -> String {
    let program = rules.commands.get(reviewer).and_then(|c| c.first()).map(String::as_str).unwrap_or(reviewer);
    match cli_name(program).as_str() {
        "claude" => "Claude Code (the `claude` CLI)".to_string(),
        "codex" => "Codex (the `codex` CLI)".to_string(),
        _ => format!("{reviewer} (`{program}`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> ReviewRules {
        ReviewRules {
            flags: vec![Flag { name: "MONEY_OR_RELEASE".into(), question: "the change alters billing".into(), reason: "money or release logic".into() }],
            ..Default::default()
        }
    }

    #[test]
    fn prompt_lists_every_closing_line() {
        let p = rules().prompt("main", None);
        assert!(p.contains("git diff main...HEAD"));
        assert!(p.contains("exactly these 2 lines:\nVERDICT: <findings or clean>\nMONEY_OR_RELEASE: <yes or no>"));
        assert!(!p.contains("PREVIOUS"));
    }

    fn previous(in_history: bool) -> PreviousRound {
        let f = |priority, text: &str| Finding { priority, text: text.into(), carried: false };
        PreviousRound {
            commit: "abc1234".into(),
            findings: vec![f(1, "Drops the last row: src/a.rs, f"), f(2, "Empty name: src/b.rs, g"), f(2, "Slow loop: src/c.rs, h")],
            in_history,
        }
    }

    #[test]
    fn a_re_review_checks_the_previous_round_first() {
        let p = rules().prompt("main", Some(&previous(true)));
        assert!(p.contains("git diff abc1234 HEAD"), "{p}");
        assert!(p.contains("1. [P1] Drops the last row: src/a.rs, f\n2. [P2] Empty name"), "{p}");
        assert!(p.contains("`PREVIOUS <n>: fixed` or `PREVIOUS <n>: open`"), "{p}");
        assert!(p.contains("only if it is P0 or P1"), "{p}");
        assert!(p.contains("exactly these 2 lines:\nVERDICT: <findings or clean>\nMONEY_OR_RELEASE: <yes or no>"));

        // After a rebase there is no diff since that round: check the findings, review it all.
        let rebased = rules().prompt("main", Some(&previous(false)));
        assert!(rebased.contains("rebased since") && rebased.contains("PREVIOUS <n>"), "{rebased}");
        assert!(!rebased.contains("git diff abc1234") && !rebased.contains("only if it is P0 or P1"), "{rebased}");

        // A clean previous round leaves no checklist, but old code still only takes P0 or P1.
        let clean = PreviousRound { findings: vec![], ..previous(true) };
        let p = rules().prompt("main", Some(&clean));
        assert!(!p.contains("PREVIOUS") && p.contains("only if it is P0 or P1"), "{p}");
    }

    #[test]
    fn a_re_review_carries_what_is_not_fixed() {
        let prev = previous(true);
        let text = "Two of three fixed.\n**PREVIOUS 1:** fixed, now keeps the row\n- PREVIOUS 2: open - still accepts \"\"\n\
- [P3] New nit: src/d.rs\nVERDICT: findings\nMONEY_OR_RELEASE: no\n";
        let r = rules().parse("codex", text, 3, Some(&prev)).unwrap();
        let statuses: Vec<&str> = r.previous.iter().map(|p| p.status.as_str()).collect();
        assert_eq!(statuses, ["fixed", "open", "unchecked"]);
        assert_eq!(r.summary, "Two of three fixed.");
        let carried: Vec<(u8, bool)> = r.findings.iter().map(|f| (f.priority, f.carried)).collect();
        assert_eq!(carried, [(3, false), (2, true), (2, true)]);
        assert_eq!(r.verdict, "pass");

        // An open P1 from the previous round still asks for changes, though nothing new was found.
        let text = "PREVIOUS 1: open\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed\nVERDICT: findings\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", text, 3, Some(&prev)).unwrap();
        assert_eq!(r.verdict, "changes");
        assert_eq!(r.findings, vec![Finding { priority: 1, text: "Drops the last row: src/a.rs, f".into(), carried: true }]);

        // Everything fixed and nothing new is clean; an unfinished reply is still unfinished.
        let text = "PREVIOUS 1: fixed\nPREVIOUS 2: fixed\nPREVIOUS 3: fixed\nVERDICT: clean\nMONEY_OR_RELEASE: no";
        let r = rules().parse("codex", text, 3, Some(&prev)).unwrap();
        assert!(r.findings.is_empty() && r.verdict == "pass");
        assert!(rules().parse("codex", "PREVIOUS 1: fixed\nMONEY_OR_RELEASE: no", 1, Some(&prev)).is_err());
    }

    #[test]
    fn parses_a_finished_review() {
        let text = "Looks mostly fine.\n- [P1] Off by one: src/a.rs, f: drops the last row\n- [P3] Typo: README\n- [P1] Off by one: src/a.rs, f: drops the last row\nVERDICT: findings\nMONEY_OR_RELEASE: **yes**\n";
        let r = rules().parse("codex", text, 5, None).unwrap();
        assert_eq!(r.verdict, "changes");
        assert_eq!(r.findings.len(), 2);
        assert_eq!(r.flagged, vec!["MONEY_OR_RELEASE"]);
        assert_eq!(r.summary, "Looks mostly fine.");
        assert_eq!(rules().flag_reasons(&r), vec!["codex review: money or release logic"]);

        let clean = rules().parse("claude", "Nothing found.\n- [P3] nit: x\nVERDICT: findings\nMONEY_OR_RELEASE: no", 1, None).unwrap();
        assert_eq!(clean.verdict, "pass");
    }

    #[test]
    fn unfinished_reviews_fail() {
        assert!(rules().parse("codex", "VERDICT: clean", 1, None).is_err(), "missing flag");
        assert!(rules().parse("codex", "MONEY_OR_RELEASE: no", 1, None).is_err(), "missing verdict");
        assert!(rules().parse("codex", "VERDICT: findings\nMONEY_OR_RELEASE: no", 1, None).is_err(), "findings but none listed");
    }

    #[test]
    fn picks_the_other_vendor() {
        let r = ReviewRules::default();
        assert_eq!(r.reviewer_for("claude"), Some("codex"));
        assert_eq!(r.reviewer_for("codex"), Some("claude"));
        assert_eq!(r.reviewer_for("cursor"), Some("claude"));
    }

    #[test]
    fn reviewers_start_without_nitely() {
        let commands = ReviewRules::default().commands;
        let claude = without_nitely(&commands["claude"], &[]);
        assert_eq!(claude[..3], ["claude", "--strict-mcp-config", "-p"]);
        assert_eq!(without_nitely(&claude, &[]), claude, "added once");

        let codex = without_nitely(&commands["codex"], &["nitely".into(), "nit_dev".into()]);
        assert_eq!(
            codex[..6],
            ["codex", "exec", "-c", "mcp_servers.nitely.enabled=false", "-c", "mcp_servers.nit_dev.enabled=false"]
        );
        assert_eq!(codex[6..], commands["codex"][2..]);
        // Codex refuses to turn off a server it does not have, so none means no override.
        assert_eq!(without_nitely(&commands["codex"], &[]), commands["codex"]);
        let path = vec!["/opt/Codex Tools/codex.exe".to_string(), "exec".into(), "-".into()];
        assert_eq!(without_nitely(&path, &["nitely".into()])[2..4], ["-c", "mcp_servers.nitely.enabled=false"]);
        let other = vec!["gemini".to_string(), "-p".into()];
        assert_eq!(without_nitely(&other, &["nitely".into()]), other);
    }

    #[test]
    fn finds_codexs_nitely_servers() {
        let json = r#"[
            {"name": "nitely", "enabled": true, "transport": {"type": "stdio", "command": "nitely", "args": ["mcp"]}},
            {"name": "board", "enabled": true, "transport": {"type": "stdio", "command": "/opt/Nitely Tools/nitely", "args": ["mcp"]}},
            {"name": "off", "enabled": false, "transport": {"type": "stdio", "command": "nitely", "args": ["mcp"]}},
            {"name": "docs", "enabled": true, "transport": {"type": "stdio", "command": "node", "args": ["docs.js"]}},
            {"name": "web", "enabled": true, "transport": {"type": "streamable_http", "url": "http://localhost:1"}}
        ]"#;
        assert_eq!(nitely_servers(json), ["nitely", "board"]);
        assert!(nitely_servers("not json").is_empty());
    }

    #[test]
    fn chooses_from_the_installed_reviewers() {
        let r = ReviewRules::default();
        let only = |names: &'static [&'static str]| move |n: &str| names.contains(&n);
        let choice = |vendor, names| r.choose(vendor, &only(names)).unwrap();
        let c = |reviewer: &str, same_vendor, note: &str| Choice { reviewer: reviewer.into(), same_vendor, note: note.into() };

        assert_eq!(choice("claude", &["claude", "codex"]), c("codex", false, ""));
        assert_eq!(choice("claude", &["claude"]), c("claude", true, "same vendor: codex not installed"));
        assert_eq!(choice("codex", &["codex"]), c("codex", true, "same vendor: claude not installed"));
        assert_eq!(choice("cursor", &["codex"]), c("codex", false, "claude not installed"));
        // Another vendor beats the author's own, whatever the map's order.
        let mut three = ReviewRules::default();
        three.commands.insert("gemini".into(), vec!["gemini".into()]);
        assert_eq!(three.choose("claude", &only(&["claude", "gemini"])).unwrap(), c("gemini", false, "codex not installed"));
        // A playbook that has an agent review its own vendor says so even when nothing is missing.
        let mut own = ReviewRules::default();
        own.reviewer.insert("claude".into(), "claude".into());
        assert_eq!(own.choose("claude", &only(&["claude", "codex"])).unwrap(), c("claude", true, "same vendor"));

        let none = r.choose("claude", &only(&[])).unwrap_err().to_string();
        assert!(none.contains("no reviewer can run") && none.contains("Codex (the `codex` CLI)") && none.contains("Claude Code"), "{none}");

        assert_eq!(r.independence_reason(&choice("claude", &["claude"])), None);
        let strict = ReviewRules { require_cross_vendor: true, ..ReviewRules::default() };
        assert!(strict.independence_reason(&choice("claude", &["claude", "codex"])).is_none());
        let reason = strict.independence_reason(&choice("claude", &["claude"])).unwrap();
        assert!(reason.contains("same vendor: codex not installed") && reason.contains("another vendor"), "{reason}");
    }

    #[test]
    fn finds_programs_on_path_or_by_path() {
        let dir = std::env::temp_dir().join(format!("nitely-which-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts").join("review"), "").unwrap();
        assert!(program_exists(&crate::playbook::posix_sh(), &dir));
        assert!(!program_exists("nitely-no-such-reviewer", &dir));
        assert!(!program_exists("/nitely/no/such/reviewer", &dir));
        // A relative path counts from the worktree the reviewer runs in, not from Nitely's own.
        assert!(program_exists("./scripts/review", &dir) && program_exists("scripts/review", &dir));
        assert!(!program_exists("./scripts/review", &std::env::temp_dir()));
        let r = ReviewRules::default();
        assert!(!r.is_installed("gemini", &dir), "no command, not installed");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn runs_a_reviewer_command() {
        let mut r = rules();
        r.commands.insert("fake".into(), vec![crate::playbook::posix_sh(), "-c".into(), "cat > /dev/null; printf 'ok\\nVERDICT: clean\\nMONEY_OR_RELEASE: no\\n' > \"$0\"".into(), "{output}".into()]);
        let review = r.run("fake", &std::env::temp_dir(), "main", None).unwrap();
        assert_eq!(review.verdict, "pass");
        assert!(review.flagged.is_empty());
    }
}
