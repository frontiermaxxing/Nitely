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
    #[serde(default)]
    pub land: LandRules,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandRules {
    /// Run in the main checkout after a change lands, in order, when what landed matches (e.g.
    /// rebuild and install a tool the agents use). A failure is a warning: the land stands.
    #[serde(default)]
    pub after: Vec<GateStep>,
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
    /// Run in a new worktree, in order, before the agent starts (e.g. `npm ci`). Run again
    /// before a gate when a dependency file changed since (see `setup_when_changed`).
    #[serde(default)]
    pub setup: Vec<String>,
    /// More files (relative to the worktree) whose change means setup must run again, besides the
    /// usual lockfiles and manifests at the worktree's root (`SETUP_FILES`).
    #[serde(default)]
    pub setup_when_changed: Vec<String>,
}

/// Dependency files whose change, by a rebase or by the agent, makes a worktree's setup stale.
const SETUP_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "Cargo.lock",
    "requirements.txt",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "go.sum",
    "Gemfile.lock",
];

/// Where a worktree keeps the fingerprint of the dependency files its setup last ran against:
/// in its own git folder, so it is never committed and goes away with the worktree.
fn setup_record(worktree: &Path) -> Option<PathBuf> {
    let path = git(worktree, &["rev-parse", "--git-path", "nitely-setup"]).ok()?;
    let path = PathBuf::from(path.trim());
    Some(if path.is_absolute() { path } else { worktree.join(path) })
}

/// FNV-1a over the names and contents of the dependency files present, stable across builds.
fn setup_fingerprint(rules: &WorktreeRules, worktree: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for name in SETUP_FILES.iter().copied().chain(rules.setup_when_changed.iter().map(String::as_str)) {
        if let Ok(bytes) = std::fs::read(worktree.join(name)) {
            eat(name.as_bytes());
            eat(&[0]);
            eat(&bytes);
        }
    }
    format!("{hash:016x}")
}

/// Runs setup again when the dependency files changed since it last ran (a rebase brought a new
/// dependency, or the agent added one), so the gate does not fail on a stale node_modules or
/// target. A worktree from before this was recorded is taken as set up. Returns the setup results,
/// and false when one failed.
fn refresh_setup(rules: &WorktreeRules, worktree: &Path, main: &Path, base: &str) -> Result<(Vec<CommandResult>, bool)> {
    if rules.setup.is_empty() {
        return Ok((vec![], true));
    }
    let Some(record) = setup_record(worktree) else { return Ok((vec![], true)) };
    let now = setup_fingerprint(rules, worktree);
    match std::fs::read_to_string(&record) {
        Ok(last) if last.trim() == now => return Ok((vec![], true)),
        Ok(_) => {}
        Err(_) => {
            std::fs::write(&record, &now).ok();
            return Ok((vec![], true));
        }
    }
    let mut results = Vec::new();
    for command in &rules.setup {
        let command = expand(command, main, worktree, base);
        let (ok, output, seconds) = run_shell(&command, worktree)?;
        results.push(CommandResult { command: format!("{command} (setup again: a dependency file changed)"), ok, seconds, output, skipped: false });
        if !ok {
            return Ok((results, false));
        }
    }
    std::fs::write(&record, &now).ok();
    Ok((results, true))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRules {
    /// Must all pass before a change can land. Run in the task's worktree, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Commands that run only for some changes, after `commands`.
    #[serde(default)]
    pub steps: Vec<GateStep>,
}

/// A gate command that depends on what the branch changes (against the integration branch).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GateStep {
    pub run: String,
    /// Runs only when a changed path matches one of these regular expressions.
    pub when_changed: Vec<String>,
    /// Skipped when every changed path matches one of these (e.g. docs-only changes).
    pub unless_only: Vec<String>,
}

impl GateStep {
    /// Why the step is skipped for `files`, or None when it runs.
    pub fn skip_reason(&self, files: &[String]) -> Result<Option<String>> {
        let any = |patterns: &[String], f: &str| -> Result<bool> {
            for p in patterns {
                if regex::Regex::new(p).with_context(|| format!("gate pattern {p}"))?.is_match(f) {
                    return Ok(true);
                }
            }
            Ok(false)
        };
        if !self.when_changed.is_empty() {
            let mut hit = false;
            for f in files {
                hit |= any(&self.when_changed, f)?;
            }
            if !hit {
                return Ok(Some(format!("nothing matching {} changed", self.when_changed.join(" or "))));
            }
        }
        if !self.unless_only.is_empty() && !files.is_empty() {
            let mut all = true;
            for f in files {
                all &= any(&self.unless_only, f)?;
            }
            if all {
                return Ok(Some(format!("only paths matching {} changed", self.unless_only.join(" or "))));
            }
        }
        Ok(None)
    }
}

/// Which changes wait for a human. Everything else is approved by a passing review.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalRules {
    /// Every change waits for a human.
    pub always_human: bool,
    /// Changed paths that need a human, as regular expressions over `/`-separated repo paths.
    pub human_paths: Vec<PathRule>,
    /// Changed paths no task branch may touch (submit and land refuse), e.g. a submodule pointer
    /// that changes through another process.
    pub blocked_paths: Vec<PathRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub pattern: String,
    /// Shown to the human, e.g. "billing and money".
    pub reason: String,
}

impl ApprovalRules {
    /// Fails when the branch touches a blocked path.
    pub fn check_blocked(&self, files: &[String]) -> Result<()> {
        for rule in &self.blocked_paths {
            let re = regex::Regex::new(&rule.pattern).with_context(|| format!("blocked pattern {}", rule.pattern))?;
            if let Some(f) = files.iter().find(|f| re.is_match(&f.replace('\\', "/"))) {
                bail!("the branch changes {f}, which task branches may not change: {}", rule.reason);
            }
        }
        Ok(())
    }

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
        WorktreeRules { dir: "../{repo}-worktrees/{key}-{slug}".into(), setup: vec![], setup_when_changed: vec![] }
    }
}

impl Playbook {
    /// The repo's playbook: its own `.nitely/playbook.toml` when it has one, else the one Nitely
    /// keeps for it (written when the repo was connected), else the defaults.
    pub fn load(repo_root: &Path) -> Result<Playbook> {
        let Some(path) = Playbook::path_for(repo_root) else { return Ok(Playbook::default()) };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Playbook::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Where the repo's playbook is read from, if it has one.
    pub fn path_for(repo_root: &Path) -> Option<PathBuf> {
        let in_repo = repo_root.join(PLAYBOOK_PATH);
        if in_repo.exists() {
            return Some(in_repo);
        }
        let config = crate::Config::load().ok()?;
        let root = crate::paths::normalize(&repo_root.to_string_lossy());
        let repo = config.repos.iter().find(|r| crate::paths::normalize(&r.root) == root)?;
        Some(Playbook::stored_path(&repo.name)).filter(|p| p.exists())
    }

    /// The playbook Nitely keeps for a connected repo that has none of its own.
    pub fn stored_path(repo_name: &str) -> PathBuf {
        Playbook::stored_path_in(&crate::paths::data_dir(), repo_name)
    }

    pub(crate) fn stored_path_in(data_dir: &Path, repo_name: &str) -> PathBuf {
        let safe: String = repo_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
        data_dir.join("playbooks").join(format!("{safe}.toml"))
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

/// `command` run through the platform shell: cmd on Windows, sh elsewhere.
pub(crate) fn shell_command(command: &str) -> Command {
    let mut cmd;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Passed raw: Rust's argument quoting turns `"` into `\"`, which cmd keeps literally.
        // /S strips only the outer quotes, so quotes inside the command reach the program as written.
        cmd = shell_free_command("cmd");
        cmd.args(["/D", "/S", "/C"]).raw_arg(format!("\"{command}\""));
    }
    #[cfg(not(windows))]
    {
        cmd = shell_free_command("sh");
        cmd.args(["-c", command]);
    }
    cmd
}

/// Runs one playbook command through the platform shell.
pub(crate) fn run_shell(command: &str, dir: &Path) -> Result<(bool, String, u64)> {
    let mut cmd = shell_command(command);
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
    /// The end of stdout and stderr, or why the step was skipped.
    pub output: String,
    #[serde(default)]
    pub skipped: bool,
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
        let command = &expand(command, repo_root, &worktree, base);
        let (ok, output, seconds) = run_shell(command, &worktree)?;
        setup.push(CommandResult { command: command.clone(), ok, seconds, output, skipped: false });
        if !ok {
            db.log_event(actor, Some(&issue.key), Some(&worktree_str), "setup_failed", &format!("{command} failed"))?;
            break;
        }
    }
    if setup.iter().all(|r| r.ok) {
        if let Some(record) = setup_record(&worktree) {
            std::fs::write(record, setup_fingerprint(&playbook.worktree, &worktree)).ok();
        }
    }
    Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: true, setup })
}

/// The ref a branch's changes are measured against: `origin/<base>` when the local `<base>` is
/// behind it (or missing), so a stale local branch doesn't count already-merged work as this change.
pub fn diff_base(worktree: &Path, base: &str) -> String {
    let remote = format!("origin/{base}");
    if git(worktree, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{remote}")]).is_err() {
        return base.to_string();
    }
    let local_exists = git(worktree, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{base}")]).is_ok();
    if !local_exists || git(worktree, &["merge-base", "--is-ancestor", base, &remote]).is_ok() {
        remote
    } else {
        base.to_string()
    }
}

/// Runs the gate commands in `worktree` and records the outcome on the issue's timeline.
/// Stops at the first failure, so the agent sees the error that matters.
pub fn run_gate(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor) -> Result<GateResult> {
    let playbook = Playbook::load(repo_root)?;
    if playbook.gate.commands.is_empty() && playbook.gate.steps.is_empty() {
        bail!("this repo's playbook has no gate commands; add them under [gate] in {PLAYBOOK_PATH}");
    }
    let base = playbook.integration_branch();
    let since = diff_base(worktree, base);
    let commit = git(worktree, &["rev-parse", "--short", "HEAD"]).unwrap_or_default().trim().to_string();
    // --no-renames: a moved file counts as a change at both paths.
    let files: Vec<String> = git(worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let mut steps: Vec<(String, Option<String>)> = playbook.gate.commands.iter().map(|c| (c.clone(), None)).collect();
    for step in &playbook.gate.steps {
        steps.push((step.run.clone(), step.skip_reason(&files)?));
    }

    // A stale setup fails the gate for reasons that are not the change's; set up again first.
    let (mut results, mut passed) = refresh_setup(&playbook.worktree, worktree, repo_root, base)?;
    if !passed {
        steps.clear();
    }
    for (command, skip) in steps {
        let command = expand(&command, repo_root, worktree, base);
        if let Some(why) = skip {
            results.push(CommandResult { command, ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true });
            continue;
        }
        let (ok, output, seconds) = run_shell(&command, worktree)?;
        results.push(CommandResult { command, ok, seconds, output, skipped: false });
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

/// Fills `{main}` (the main checkout), `{worktree}` and `{base}` in a playbook command.
pub(crate) fn expand(command: &str, main: &Path, worktree: &Path, base: &str) -> String {
    let slash = |p: &Path| p.to_string_lossy().replace('\\', "/");
    command.replace("{main}", &slash(main)).replace("{worktree}", &slash(worktree)).replace("{base}", base)
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
    fn gate_steps_run_by_what_changed() {
        let step = |when: &[&str], unless: &[&str]| GateStep {
            run: "x".into(),
            when_changed: when.iter().map(|s| s.to_string()).collect(),
            unless_only: unless.iter().map(|s| s.to_string()).collect(),
        };
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let api = step(&["^aera-api/"], &[]);
        assert!(api.skip_reason(&files(&["aera/x.ts"])).unwrap().is_some());
        assert!(api.skip_reason(&files(&["aera/x.ts", "aera-api/y.ts"])).unwrap().is_none());
        let aera = step(&[], &["^docs/", "^aera-website/"]);
        assert!(aera.skip_reason(&files(&["docs/a.md", "aera-website/b.tsx"])).unwrap().is_some());
        assert!(aera.skip_reason(&files(&["docs/a.md", "aera/c.ts"])).unwrap().is_none());
        assert!(aera.skip_reason(&[]).unwrap().is_none(), "an empty diff runs everything");

        let rules = ApprovalRules {
            blocked_paths: vec![PathRule { pattern: "^src$".into(), reason: "src lands through the C++ lane".into() }],
            ..Default::default()
        };
        assert!(rules.check_blocked(&files(&["src/x.cc", "aera/y.ts"])).is_ok());
        assert!(rules.check_blocked(&files(&["src"])).unwrap_err().to_string().contains("C++ lane"));
    }

    /// Aera's playbook must keep parsing, and must route paths the way scripts/work/approval.mjs does.
    #[test]
    fn aera_playbook_matches_its_scripts() {
        let pb = Playbook::parse(include_str!("../../../docs/playbooks/aera.toml")).unwrap();
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let human = |f: &str| !pb.approval.human_reasons(&files(&[f])).unwrap().is_empty();
        // The cases in scripts/work/approval.test.mjs.
        for f in [
            "aera/core/entitlement/paidFeatures.ts", "aera-api/src/lib/credits/reverse.ts", "aera-api/src/lib/billing/legacyEvents.ts",
            "aera/newtab/components/settings/SubscriptionCard.tsx", "aera-website/src/app/pricing/page.tsx",
            "deployment/win64/build-release.ps1", "deployment/win64/upload-release.ps1", "scripts/deploy_release.ps1",
            "aera-api/deploy.sh", "aera-api/ecosystem.config.js", "installer/aera.nsi", "src/chrome/browser/aera/aera_update_service.cc",
        ] {
            assert!(human(f), "{f} should wait for Andrew");
        }
        for f in [
            "aera/core/agent/prompts/character.md", "aera/ui/i18n/locales/en.json", "aera-website/messages/en.json",
            "aera/core/auth/authFailure.ts", "CLAUDE.md", "AGENTS.md", "aera/test/holds.test.ts", "docs/knowledge/agent-workflow.md",
            "scripts/work/land.mjs", "aera/core/board/holds.ts", "aera-api/src/app/api/onboarding/route.ts", "tools/something-new.py",
        ] {
            assert!(!human(f), "{f} should not wait for Andrew");
        }
        assert!(!pb.approval.human_reasons(&[]).unwrap().is_empty(), "an empty branch is not approved by anyone");
        assert!(pb.approval.check_blocked(&files(&["src"])).is_err());
        assert_eq!(pb.review.reviewer_for("claude"), Some("codex"));
        assert_eq!(pb.review.flags.len(), 2);
        let docs_only = files(&["docs/a.md", "aera-website/app/page.tsx"]);
        let skipped: Vec<bool> = pb.gate.steps.iter().map(|s| s.skip_reason(&docs_only).unwrap().is_some()).collect();
        assert_eq!(skipped, vec![true, true, true, true, false]);
        if !cfg!(target_os = "macos") {
            assert_eq!(pb.integration_branch(), "win-dev");
        }
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
        // Commands run through cmd on Windows and sh elsewhere; the quotes must reach the program.
        let find = if cfg!(windows) { r#"findstr /c:"main" setup.txt"# } else { r#"grep -q "main" setup.txt"# };
        std::fs::write(
            repo.join(PLAYBOOK_PATH),
            format!(
                "[worktree]\ndir = \"../wt/{{key}}\"\nsetup = [\"echo {{base}} > setup.txt\"]\n[gate]\ncommands = [{find:?}]\n\
                 [[gate.steps]]\nrun = \"echo never\"\nwhen_changed = [\"^api/\"]\n[[gate.steps]]\nrun = \"exit 1\"\n"
            ),
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
        assert_eq!(gate.results.len(), 3);
        assert!(gate.results[0].ok && gate.results[1].skipped && !gate.results[2].ok, "{gate:?}");
        let kinds: Vec<String> = db.timeline("NIT-3").unwrap().into_iter().map(|t| t.kind).collect();
        assert_eq!(kinds, vec!["worktree", "gate_failed"]);

        // A dependency file changes (a rebase brought one): setup runs again before the gate, once.
        let wt = Path::new(&started.worktree);
        std::fs::remove_file(wt.join("setup.txt")).unwrap();
        std::fs::write(wt.join("package-lock.json"), "{}").unwrap();
        let gate = run_gate(&db, &repo, wt, Some("NIT-3"), &actor).unwrap();
        assert!(gate.results[0].command.contains("setup again") && gate.results[0].ok, "{gate:?}");
        assert!(wt.join("setup.txt").exists() && gate.results[1].ok, "the gate saw the fresh setup: {gate:?}");
        let gate = run_gate(&db, &repo, wt, Some("NIT-3"), &actor).unwrap();
        assert_eq!(gate.results.len(), 3, "nothing changed since: {gate:?}");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn measures_against_the_newer_of_local_and_remote_base() {
        let base = std::env::temp_dir().join(format!("nitely-db-{}-{}", std::process::id(), now()));
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let sh = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success(), "{args:?}");
        sh(&["init", "-q", "-b", "main"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "init"]);
        assert_eq!(diff_base(&repo, "main"), "main", "no remote");
        sh(&["commit", "-q", "--allow-empty", "-m", "merged elsewhere"]);
        sh(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        sh(&["reset", "-q", "--hard", "HEAD~1"]);
        assert_eq!(diff_base(&repo, "main"), "origin/main", "local main is behind");
        sh(&["commit", "-q", "--allow-empty", "-m", "local only"]);
        assert_eq!(diff_base(&repo, "main"), "main", "local main has diverged");
        std::fs::remove_dir_all(base).ok();
    }
}
