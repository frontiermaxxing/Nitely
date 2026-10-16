//! A project's playbook: how a task becomes a branch and a worktree, how the worktree is set up,
//! and which commands gate a change. It lives in the repo at `.nitely/playbook.toml`, versioned
//! with the code it governs, so every agent and every machine follows the same rules. This is the
//! Aera `work.mjs` workflow made configurable; `review.rs` and `submit.rs` build on it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};

pub const PLAYBOOK_PATH: &str = ".nitely/playbook.toml";

/// Output kept per gate command: the end of it, where failures are.
const OUTPUT_TAIL: usize = 4000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Playbook {
    #[serde(default)]
    pub branch: BranchRules,
    #[serde(default)]
    pub worktree: WorktreeRules,
    #[serde(default)]
    pub gate: GateRules,
    #[serde(default)]
    pub review: crate::review::ReviewRules,
    #[serde(default)]
    pub approval: ApprovalRules,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BranchRules {
    /// The branch tasks are cut from and land on.
    pub integration: String,
    /// Per-OS overrides (Aera keeps `win-dev` and `mac-dev`).
    pub integration_windows: Option<String>,
    pub integration_macos: Option<String>,
    /// `{vendor}`, `{key}` (lowercase) and `{slug}` are filled in.
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeRules {
    /// Relative to the repo root. `{repo}`, `{key}`, `{slug}` and `{vendor}` are filled in.
    /// Keep it outside the repo so searches in the main checkout never walk task worktrees.
    pub dir: String,
    /// Run in a new worktree, in order, before the agent starts (e.g. `npm ci`).
    #[serde(default)]
    pub setup: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRules {
    /// Must all pass before a change can land. Run in the task's worktree.
    #[serde(default)]
    pub commands: Vec<String>,
}

/// Which changes wait for a human. Everything else is approved by a passing review.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalRules {
    /// Every change waits for a human.
    pub always_human: bool,
    /// Changed paths that need a human, as regular expressions over `/`-separated repo paths.
    pub human_paths: Vec<PathRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub pattern: String,
    /// Shown to the human, e.g. "billing and money".
    pub reason: String,
}

impl ApprovalRules {
    /// The reasons a human must approve a change to `files`; empty when the review is enough.
    pub fn human_reasons(&self, files: &[String]) -> Result<Vec<String>> {
        let mut reasons = Vec::new();
        if self.always_human {
            reasons.push("this playbook sends every change to a human".to_string());
        }
        if files.is_empty() {
            reasons.push("the branch changes no files".to_string());
        }
        for rule in &self.human_paths {
            let re = regex::Regex::new(&rule.pattern).with_context(|| format!("approval pattern {}", rule.pattern))?;
            for f in files {
                let f = f.replace('\\', "/");
                if re.is_match(&f) {
                    reasons.push(format!("{}: {f}", rule.reason));
                }
            }
        }
        Ok(reasons)
    }
}

impl Default for BranchRules {
    fn default() -> Self {
        BranchRules {
            integration: "main".into(),
            integration_windows: None,
            integration_macos: None,
            pattern: "{vendor}/{key}-{slug}".into(),
        }
    }
}

impl Default for WorktreeRules {
    fn default() -> Self {
        WorktreeRules { dir: "../{repo}-worktrees/{key}-{slug}".into(), setup: vec![] }
    }
}

impl Playbook {
    /// The repo's playbook, or the defaults when it has none.
    pub fn load(repo_root: &Path) -> Result<Playbook> {
        let path = repo_root.join(PLAYBOOK_PATH);
        if !path.exists() {
            return Ok(Playbook::default());
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Playbook::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Playbook> {
        Ok(toml::from_str(text)?)
    }

    pub fn integration_branch(&self) -> &str {
        let os = if cfg!(windows) {
            self.branch.integration_windows.as_deref()
        } else if cfg!(target_os = "macos") {
            self.branch.integration_macos.as_deref()
        } else {
            None
        };
        os.unwrap_or(&self.branch.integration)
    }

    pub fn branch_for(&self, vendor: &str, issue: &IssueRecord) -> String {
        fill(&self.branch.pattern, vendor, issue, "")
    }

    pub fn worktree_for(&self, repo_root: &Path, vendor: &str, issue: &IssueRecord) -> PathBuf {
        let repo = repo_root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let rel = fill(&self.worktree.dir, vendor, issue, &repo);
        normalize_lexically(&repo_root.join(rel))
    }
}

fn slug(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
}

fn fill(pattern: &str, vendor: &str, issue: &IssueRecord, repo: &str) -> String {
    pattern
        .replace("{vendor}", vendor)
        .replace("{key}", &issue.key.to_lowercase())
        .replace("{slug}", &slug(&issue.title))
        .replace("{repo}", repo)
}

/// Resolves `..` without touching the disk (the worktree does not exist yet).
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = shell_free_command("git").arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn shell_free_command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Runs one playbook command through the platform shell.
fn run_shell(command: &str, dir: &Path) -> Result<(bool, String, u64)> {
    let mut cmd = if cfg!(windows) {
        let mut c = shell_free_command("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = shell_free_command("sh");
        c.args(["-c", command]);
        c
    };
    let started = Instant::now();
    let out = cmd.current_dir(dir).output().with_context(|| format!("running {command}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), tail(&text), started.elapsed().as_secs()))
}

pub(crate) fn tail(s: &str) -> String {
    if s.len() <= OUTPUT_TAIL {
        return s.to_string();
    }
    let mut start = s.len() - OUTPUT_TAIL;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// The main checkout of the repo `dir` is in: the watched repo's root when Nitely knows it,
/// otherwise git's answer.
pub fn repo_root(config: &crate::Config, dir: &str) -> Option<PathBuf> {
    config
        .repo_for_dir(dir)
        .map(|r| PathBuf::from(&r.root))
        .or_else(|| crate::git::main_root(dir).map(PathBuf::from))
}

/// The worktree `dir` is in, and the issue its branch names (`claude/nit-3-...` is NIT-3).
pub fn current_worktree(dir: &str) -> Result<(PathBuf, Option<String>)> {
    let dir = Path::new(dir);
    let top = git(dir, &["rev-parse", "--show-toplevel"]).context("not inside a git worktree")?;
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let (_, key) = crate::git::parse_branch(branch.trim());
    Ok((PathBuf::from(top.trim()), key))
}

#[derive(Debug, Clone, Serialize)]
pub struct Started {
    pub issue: String,
    pub branch: String,
    pub worktree: String,
    /// False when the worktree already existed (starting is idempotent).
    pub created: bool,
    pub setup: Vec<CommandResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    pub command: String,
    pub ok: bool,
    pub seconds: u64,
    /// The end of stdout and stderr.
    pub output: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub passed: bool,
    pub commit: String,
    pub results: Vec<CommandResult>,
}

/// Creates the issue's branch and worktree from the integration branch and runs the setup
/// commands. Calling it again for the same issue returns the existing worktree.
pub fn start_task(db: &Db, repo_root: &Path, issue: &IssueRecord, vendor: &str, actor: &Actor) -> Result<Started> {
    let playbook = Playbook::load(repo_root)?;
    let branch = playbook.branch_for(vendor, issue);
    let worktree = playbook.worktree_for(repo_root, vendor, issue);
    let worktree_str = worktree.to_string_lossy().into_owned();

    if worktree.join(".git").exists() {
        return Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: false, setup: vec![] });
    }

    let base = playbook.integration_branch();
    let branch_exists = git(repo_root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if branch_exists {
        git(repo_root, &["worktree", "add", &worktree_str, &branch])?;
    } else {
        git(repo_root, &["worktree", "add", "-b", &branch, &worktree_str, base])?;
    }
    db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started {branch} from {base}"))?;

    let mut setup = Vec::new();
    for command in &playbook.worktree.setup {
        let (ok, output, seconds) = run_shell(command, &worktree)?;
        setup.push(CommandResult { command: command.clone(), ok, seconds, output });
        if !ok {
            db.log_event(actor, Some(&issue.key), Some(&worktree_str), "setup_failed", &format!("{command} failed"))?;
            break;
        }
    }
    Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: true, setup })
}

/// Runs the gate commands in `worktree` and records the outcome on the issue's timeline.
/// Stops at the first failure, so the agent sees the error that matters.
pub fn run_gate(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor) -> Result<GateResult> {
    let playbook = Playbook::load(repo_root)?;
    if playbook.gate.commands.is_empty() {
        bail!("this repo's playbook has no gate commands; add them under [gate] in {PLAYBOOK_PATH}");
    }
    let commit = git(worktree, &["rev-parse", "--short", "HEAD"]).unwrap_or_default().trim().to_string();
    let mut results = Vec::new();
    let mut passed = true;
    for command in &playbook.gate.commands {
        let (ok, output, seconds) = run_shell(command, worktree)?;
        results.push(CommandResult { command: command.clone(), ok, seconds, output });
        if !ok {
            passed = false;
            break;
        }
    }
    let summary = if passed {
        format!("Gate passed at {commit}")
    } else {
        format!("Gate failed at {commit}: {}", results.last().map(|r| r.command.as_str()).unwrap_or(""))
    };
    db.log_event(actor, issue_key, Some(&worktree.to_string_lossy()), if passed { "gate_passed" } else { "gate_failed" }, &summary)?;
    Ok(GateResult { passed, commit, results })
}

impl Db {
    /// A timeline entry not tied to a session heartbeat.
    pub fn log_event(&self, actor: &Actor, issue_key: Option<&str>, cwd: Option<&str>, kind: &str, summary: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event (ts, actor, cwd, issue_key, type, summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now(), actor.id(), cwd, issue_key, kind, summary],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_sections_keep_defaults() {
        let p = Playbook::parse("[worktree]\nsetup = [\"npm ci\"]\n[branch]\nintegration = \"win-dev\"\n").unwrap();
        assert_eq!(p.worktree.dir, WorktreeRules::default().dir);
        assert_eq!(p.branch.pattern, BranchRules::default().pattern);
        assert_eq!(p.branch.integration, "win-dev");
        assert!(Playbook::parse("[gate]\ncomand = []\n").is_err());
    }

    fn issue() -> IssueRecord {
        IssueRecord { key: "NIT-3".into(), title: "Playbook engine: start a task!".into(), ..Default::default() }
    }

    #[test]
    fn parses_and_fills_patterns() {
        let pb = Playbook::parse(
            r#"
            [branch]
            integration = "win-dev"
            integration_macos = "mac-dev"
            pattern = "{vendor}/{key}-{slug}"
            [worktree]
            dir = "../{repo}-worktrees/{key}-{slug}"
            setup = ["npm ci"]
            [gate]
            commands = ["npm run test:gate"]
            "#,
        )
        .unwrap();
        assert_eq!(pb.branch_for("claude", &issue()), "claude/nit-3-playbook-engine-start-a-task");
        let wt = pb.worktree_for(Path::new("/dev/AeraChromium"), "claude", &issue());
        assert_eq!(wt, PathBuf::from("/dev/AeraChromium-worktrees/nit-3-playbook-engine-start-a-task"));
        if !cfg!(windows) && !cfg!(target_os = "macos") {
            assert_eq!(pb.integration_branch(), "win-dev");
        }
    }

    #[test]
    fn classifies_paths_for_human_approval() {
        let pb = Playbook::parse(
            r#"
            [[approval.human_paths]]
            pattern = "(^|/)[^/]*(billing|payment)[^/]*(/|$)"
            reason = "billing and money"
            "#,
        )
        .unwrap();
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(pb.approval.human_reasons(&files(&["src/ui/sidebar.tsx"])).unwrap().is_empty());
        assert_eq!(
            pb.approval.human_reasons(&files(&["src\\billing\\plan.ts", "README.md"])).unwrap(),
            vec!["billing and money: src/billing/plan.ts".to_string()]
        );
        assert_eq!(pb.approval.human_reasons(&[]).unwrap().len(), 1);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(Playbook::parse("[gate]\ncomand = [\"x\"]").is_err());
        assert_eq!(Playbook::parse("").unwrap().branch.integration, "main");
    }

    #[test]
    fn starts_a_worktree_and_runs_the_gate() {
        let base = std::env::temp_dir().join(format!("nitely-pb-{}-{}", std::process::id(), now()));
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap().status.success());
        sh(&["init", "-q", "-b", "main"]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(
            repo.join(PLAYBOOK_PATH),
            "[worktree]\ndir = \"../wt/{key}\"\nsetup = [\"echo ready > setup.txt\"]\n[gate]\ncommands = [\"test -f setup.txt\", \"false\"]\n",
        )
        .unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());

        let started = start_task(&db, &repo, &issue(), "claude", &actor).unwrap();
        assert!(started.created && started.setup.iter().all(|r| r.ok), "{started:?}");
        assert!(Path::new(&started.worktree).join("setup.txt").exists());
        let again = start_task(&db, &repo, &issue(), "claude", &actor).unwrap();
        assert!(!again.created);

        let gate = run_gate(&db, &repo, Path::new(&started.worktree), Some("NIT-3"), &actor).unwrap();
        assert!(!gate.passed);
        assert_eq!(gate.results.len(), 2);
        assert!(gate.results[0].ok && !gate.results[1].ok);
        let kinds: Vec<String> = db.timeline("NIT-3").unwrap().into_iter().map(|t| t.kind).collect();
        assert_eq!(kinds, vec!["worktree", "gate_failed"]);
        std::fs::remove_dir_all(base).ok();
    }
}
