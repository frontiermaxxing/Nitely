//! A project's playbook: how a task becomes a branch and a worktree, how the worktree is set up,
//! and which commands gate a change. It lives in the repo at `.nitely/playbook.toml`, versioned
//! with the code it governs, so every agent and every machine follows the same rules. `review.rs`
//! and `submit.rs` build on it.

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
    /// Agent workers Nitely starts itself (docs/crew.md); off unless `workers` is set.
    #[serde(default)]
    pub crew: crate::crew::CrewRules,
    /// Keys this nitely does not know, which it ignored: a typo, or a key a newer nitely reads.
    /// Refusing them would break every older nitely still running the day a playbook starts
    /// using a new key (NIT-50), so they are reported instead (see `unknown_warnings`).
    #[serde(skip)]
    pub unknown: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LandRules {
    /// Who lands a reviewed change. What needs a human stays with `[approval]` and `review.flags`.
    #[serde(default)]
    pub by: LandBy,
    /// Run in the main checkout after a change lands, in order, when what landed matches (e.g.
    /// rebuild and install a tool the agents use). A failure is a warning: the land stands.
    #[serde(default)]
    pub after: Vec<GateStep>,
}

/// Who lands a change once its review passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LandBy {
    /// The agent that wrote it calls land: a passing review moves it to In Review, an approval
    /// to Approved.
    #[default]
    Author,
    /// Nitely lands it in the same submit when the review passes and no human is needed, and as
    /// soon as a human approves it otherwise. Nitely runs the review itself, so a reviewer that
    /// merges is this mode too.
    #[serde(alias = "reviewer")]
    Auto,
    /// Every change waits for a human, and Nitely lands it as soon as they approve it.
    Human,
}

impl LandBy {
    /// Whether Nitely lands the change itself rather than the author.
    pub fn by_nitely(self) -> bool {
        self != LandBy::Author
    }

    /// One line for the pull request and the issue's journey.
    pub fn describe(self) -> &'static str {
        match self {
            LandBy::Author => "the agent that wrote it lands it once the review passes (and a human approves, when one must)",
            LandBy::Auto => "lands automatically once the review passes, or once a human approves when one must",
            LandBy::Human => "every change waits for a human, and lands automatically once they approve it",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchRules {
    /// The branch tasks are cut from and land on.
    pub integration: String,
    /// Per-OS overrides (a repo can keep `win-dev` and `mac-dev`).
    pub integration_windows: Option<String>,
    pub integration_macos: Option<String>,
    /// `{vendor}`, `{key}` (lowercase) and `{slug}` are filled in.
    pub pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// Run in the worktree, in order, before land removes it: undo what setup made that
    /// `git worktree remove` cannot take down (a sparse checkout of a submodule, a nested worktree
    /// git refuses to remove with). A failure is reported and the removal still tried.
    #[serde(default)]
    pub teardown: Vec<String>,
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

/// The dependency files among `files` (paths relative to the repo root): what makes a checkout's
/// setup stale when it changes.
pub fn setup_files_in(rules: &WorktreeRules, files: &[String]) -> Vec<String> {
    files
        .iter()
        .filter(|f| SETUP_FILES.contains(&f.as_str()) || rules.setup_when_changed.iter().any(|w| w.trim_start_matches("./") == f.as_str()))
        .cloned()
        .collect()
}

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
#[serde(default)]
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
#[serde(default)]
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
pub struct PathRule {
    pub pattern: String,
    /// Shown to the human, e.g. "billing and money".
    pub reason: String,
}

impl Playbook {
    /// Why a change touching `files` needs a human: the approval rules, and a landing mode that
    /// lands only what a human approved.
    pub fn human_reasons(&self, files: &[String]) -> Result<Vec<String>> {
        let mut reasons = self.approval.human_reasons(files)?;
        if self.land.by == LandBy::Human {
            reasons.push("this playbook lands only what a human approved".to_string());
        }
        Ok(reasons)
    }
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
        WorktreeRules { dir: "../{repo}-worktrees/{key}-{slug}".into(), setup: vec![], setup_when_changed: vec![], teardown: vec![] }
    }
}

impl Playbook {
    /// The repo's playbook: its own `.nitely/playbook.toml` when it has one, else the one Nitely
    /// keeps for it (written when the repo was connected), else the defaults.
    /// What it leaves unset of the review and landing rules comes from the app-wide defaults
    /// (`rules::RuleDefaults`).
    pub fn load(repo_root: &Path) -> Result<Playbook> {
        // Tests build their own playbooks; the defaults of whoever runs them must not leak in.
        let defaults = if cfg!(test) { Default::default() } else { crate::Config::load().map(|c| c.rule_defaults).unwrap_or_default() };
        let Some(path) = Playbook::path_for(repo_root) else {
            let mut pb = Playbook::default();
            crate::rules::apply_defaults(&mut pb, "", &defaults);
            return Ok(pb);
        };
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut pb = Playbook::parse(&text).with_context(|| format!("in {}", path.display()))?;
        crate::rules::apply_defaults(&mut pb, &text, &defaults);
        Ok(pb)
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
        let mut unknown = Vec::new();
        let de = toml::Deserializer::parse(text)?;
        let mut playbook: Playbook = serde_ignored::deserialize(de, |path| unknown.push(path.to_string()))?;
        playbook.unknown = unknown;
        Ok(playbook)
    }

    /// One line per ignored key, for the results agents read.
    pub fn unknown_warnings(&self) -> Vec<String> {
        self.unknown
            .iter()
            .map(|k| format!("the playbook's `{k}` is not something this nitely knows, so it was ignored (a typo, or a key a newer nitely reads)"))
            .collect()
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
    finish(shell_free_command("git").arg("-C").arg(dir).args(args), args)
}

fn finish(cmd: &mut Command, args: &[&str]) -> Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A git call that talks to `origin` (push, fetch). It never waits for a prompt or a sign-in
/// window: nobody is there to answer one, and Git Credential Manager has hung a land that way.
/// For an HTTPS bitbucket.org remote with a token in the credentials file, the configured helpers
/// are replaced by an Authorization header for bitbucket.org only. Both go in through git's environment config
/// (GIT_CONFIG_COUNT), so the token is on no command line and nothing prints it. Other hosts keep
/// their helpers and fail fast if those cannot answer.
pub(crate) fn git_remote(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = shell_free_command("git");
    cmd.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never");
    cmd.args(["-c", "credential.interactive=never"]);
    // A push authenticates against the push URL, which can differ from the fetch URL.
    let url_args: &[&str] = if args.first() == Some(&"push") { &["remote", "get-url", "--push", "origin"] } else { &["remote", "get-url", "origin"] };
    let url = git(dir, url_args).unwrap_or_default();
    let header = || crate::credentials::load().ok().flatten().and_then(|c| crate::credentials::git_auth_header(&c));
    if is_bitbucket_https(url.trim()) {
        if let Some(header) = header() {
            // Appended after any environment config already set (safe.directory in a sandbox, say).
            let n: usize = std::env::var("GIT_CONFIG_COUNT").ok().and_then(|c| c.trim().parse().ok()).unwrap_or(0);
            cmd.env("GIT_CONFIG_COUNT", (n + 2).to_string())
                .env(format!("GIT_CONFIG_KEY_{n}"), "credential.helper")
                .env(format!("GIT_CONFIG_VALUE_{n}"), "")
                .env(format!("GIT_CONFIG_KEY_{}", n + 1), "http.https://bitbucket.org/.extraHeader")
                .env(format!("GIT_CONFIG_VALUE_{}", n + 1), header);
        }
    }
    finish(cmd.args(args), args)
}

/// Whether `url` is an HTTPS remote on bitbucket.org (SSH remotes use keys, not a helper).
pub(crate) fn is_bitbucket_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    host.eq_ignore_ascii_case("bitbucket.org")
}

/// A POSIX `sh` for tests' fake reviewers: `sh` on unix, and on Windows the one Git for Windows
/// ships (found from `git --exec-path`, since its folder is often not on PATH). Its folder, which
/// holds `cat`, `sleep` and the other tools the fake reviewers use, is put on this process's PATH
/// once, so the commands they run are found too.
#[cfg(test)]
pub(crate) fn posix_sh() -> String {
    if !cfg!(windows) {
        return "sh".into();
    }
    static SH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SH.get_or_init(|| {
        let exec = git(Path::new("."), &["--exec-path"]).unwrap_or_default();
        // <git>/mingw64/libexec/git-core
        let git_root = Path::new(exec.trim()).ancestors().nth(3).map(Path::to_path_buf).unwrap_or_default();
        let Some(sh) = ["usr/bin/sh.exe", "bin/sh.exe"].iter().map(|p| git_root.join(p)).find(|p| p.is_file()) else {
            return "sh".into();
        };
        let tools = git_root.join("usr").join("bin");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<PathBuf> = std::iter::once(tools).chain(std::env::split_paths(&path)).collect();
        if let Ok(joined) = std::env::join_paths(dirs) {
            std::env::set_var("PATH", joined);
        }
        sh.to_string_lossy().replace('\\', "/")
    })
    .clone()
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

/// The main checkout an issue's work happens in: the repo linked to the issue's project, so an
/// agent working across projects starts, gates and lands in the right one. When several repos name
/// the project, the one `dir` is in; when none does, the repo `dir` is in, unless that repo holds
/// another project's work.
pub fn repo_for_issue(config: &crate::Config, project: Option<&str>, dir: &str) -> Result<PathBuf> {
    let here = config.repo_for_dir(dir);
    let Some(project) = project else {
        return repo_root(config, dir).with_context(|| format!("{dir} is not inside a git repo"));
    };
    let is_project = |r: &crate::config::RepoConfig| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project));
    let linked: Vec<&crate::config::RepoConfig> = config.repos.iter().filter(|r| is_project(r)).collect();
    match linked.as_slice() {
        [one] => return Ok(PathBuf::from(&one.root)),
        [] => {}
        many => {
            if let Some(here) = here.filter(|h| is_project(h)) {
                return Ok(PathBuf::from(&here.root));
            }
            let names: Vec<&str> = many.iter().map(|r| r.name.as_str()).collect();
            bail!("several repos are linked to {project} ({}); call this from inside the one the issue belongs to", names.join(", "));
        }
    }
    if let Some(here) = here {
        if let Some(other) = here.project.as_deref().filter(|p| !p.eq_ignore_ascii_case(project)) {
            bail!(
                "{dir} is in {}, which holds {other}'s work, and no repo is linked to {project}; link one with `nitely repo add <path> --project {project}`",
                here.name
            );
        }
    }
    repo_root(config, dir).with_context(|| format!("{dir} is not inside a git repo, and no repo is linked to {project}"))
}

/// The worktree of `root` whose branch names the issue; None when there is none.
pub fn issue_worktree(root: &Path, key: &str) -> Result<Option<PathBuf>> {
    let named: Vec<PathBuf> = crate::git::list_worktrees(&root.to_string_lossy())?
        .into_iter()
        .filter(|w| !w.is_main && w.branch.as_deref().is_some_and(|b| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(key))))
        .map(|w| PathBuf::from(w.path))
        .collect();
    match named.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => bail!("several worktrees of {} have a branch that names {key}; pass the worktree", root.display()),
    }
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
    /// Playbook keys this nitely ignored.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Creates the issue's branch and worktree from the integration branch and runs the setup
/// commands. Calling it again for the same issue returns the existing worktree.
pub fn start_task(db: &Db, repo_root: &Path, issue: &IssueRecord, vendor: &str, actor: &Actor) -> Result<Started> {
    if issue.is_humans() && *actor != Actor::Human {
        bail!("{} is the human's own work (owner: human), so agents do not start it; `ask` the human if an agent should take it", issue.key);
    }
    let playbook = Playbook::load(repo_root)?;
    let branch = playbook.branch_for(vendor, issue);
    let worktree = playbook.worktree_for(repo_root, vendor, issue);
    let worktree_str = worktree.to_string_lossy().into_owned();

    if worktree.join(".git").exists() {
        return Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: false, setup: vec![], warnings: playbook.unknown_warnings() });
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
    Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: true, setup, warnings: playbook.unknown_warnings() })
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
    Ok(GateResult { passed, commit, results, warnings: playbook.unknown_warnings() })
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
    fn only_https_bitbucket_remotes_get_the_token_helper() {
        assert!(is_bitbucket_https("https://bitbucket.org/acme/my-app.git"));
        assert!(is_bitbucket_https("https://someone@bitbucket.org/acme/my-app.git"));
        assert!(!is_bitbucket_https("https://github.com/frontiermaxxing/Nitely.git"));
        assert!(!is_bitbucket_https("git@bitbucket.org:acme/my-app.git"));
        assert!(!is_bitbucket_https("https://bitbucket.org.evil.com/x.git"));
    }

    #[test]
    fn partial_sections_keep_defaults() {
        let p = Playbook::parse("[worktree]\nsetup = [\"npm ci\"]\n[branch]\nintegration = \"win-dev\"\n").unwrap();
        assert_eq!(p.worktree.dir, WorktreeRules::default().dir);
        assert_eq!(p.branch.pattern, BranchRules::default().pattern);
        assert_eq!(p.branch.integration, "win-dev");
        assert_eq!(Playbook::parse("[gate]\ncomand = []\n").unwrap().unknown, ["gate.comand"]);
    }

    #[test]
    fn reads_who_lands() {
        let by = |toml: &str| Playbook::parse(toml).unwrap().land.by;
        assert_eq!(by(""), LandBy::Author);
        assert_eq!(by("[land]\nby = \"auto\"\n"), LandBy::Auto);
        assert_eq!(by("[land]\nby = \"reviewer\"\n"), LandBy::Auto, "a reviewer that merges is auto: Nitely runs the review");
        assert_eq!(by("[land]\nby = \"human\"\n"), LandBy::Human);
        assert!(Playbook::parse("[land]\nby = \"robot\"\n").is_err());
        let human = Playbook::parse("[land]\nby = \"human\"\n").unwrap();
        assert_eq!(human.human_reasons(&["a.txt".into()]).unwrap(), ["this playbook lands only what a human approved"]);
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
        let wt = pb.worktree_for(Path::new("/dev/my-app"), "claude", &issue());
        assert_eq!(wt, PathBuf::from("/dev/my-app-worktrees/nit-3-playbook-engine-start-a-task"));
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
        let api = step(&["^api/"], &[]);
        assert!(api.skip_reason(&files(&["app/x.ts"])).unwrap().is_some());
        assert!(api.skip_reason(&files(&["app/x.ts", "api/y.ts"])).unwrap().is_none());
        let app = step(&[], &["^docs/", "^site/"]);
        assert!(app.skip_reason(&files(&["docs/a.md", "site/b.tsx"])).unwrap().is_some());
        assert!(app.skip_reason(&files(&["docs/a.md", "app/c.ts"])).unwrap().is_none());
        assert!(app.skip_reason(&[]).unwrap().is_none(), "an empty diff runs everything");

        let rules = ApprovalRules {
            blocked_paths: vec![PathRule { pattern: "^src$".into(), reason: "src lands through the C++ lane".into() }],
            ..Default::default()
        };
        assert!(rules.check_blocked(&files(&["src/x.cc", "app/y.ts"])).is_ok());
        assert!(rules.check_blocked(&files(&["src"])).unwrap_err().to_string().contains("C++ lane"));
    }

    /// The worked example in docs must keep parsing, and must route paths the way it says.
    #[test]
    fn example_playbook_parses_and_routes() {
        let pb = Playbook::parse(include_str!("../../../docs/playbooks/example.toml")).unwrap();
        assert!(pb.unknown.is_empty(), "{:?}", pb.unknown);
        assert!(pb.worktree.setup.iter().any(|c| c.contains("link-sdk")));
        assert!(pb.worktree.teardown.iter().any(|c| c.contains("unlink-sdk")));
        let files = |f: &[&str]| f.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let human = |f: &str| !pb.approval.human_reasons(&files(&[f])).unwrap().is_empty();
        for f in [
            "app/src/billing/Checkout.tsx", "api/src/invoices.ts", "site/src/pricing/page.tsx",
            "deploy/release.ps1", "installer/acme.nsi", "scripts/deploy_site.sh", "api/Dockerfile",
        ] {
            assert!(human(f), "{f} should wait for a human");
        }
        for f in ["app/src/login.ts", "api/src/users.ts", "site/messages/en.json", "AGENTS.md", "docs/setup.md", "tools/new.py"] {
            assert!(!human(f), "{f} should not wait for a human");
        }
        assert!(!pb.approval.human_reasons(&[]).unwrap().is_empty(), "an empty branch is not approved by anyone");
        assert!(pb.approval.check_blocked(&files(&["vendor/sdk/lib.h"])).is_err());
        assert!(pb.approval.check_blocked(&files(&["vendor/other.h"])).is_ok());
        assert_eq!(pb.review.reviewer_for("claude"), Some("codex"));
        assert_eq!(pb.review.flags.len(), 2);
        let docs_only = files(&["docs/a.md", "site/app/page.tsx"]);
        let skipped: Vec<bool> = pb.gate.steps.iter().map(|s| s.skip_reason(&docs_only).unwrap().is_some()).collect();
        assert_eq!(skipped, vec![true, true, true, true, false]);
        if !cfg!(target_os = "macos") {
            assert_eq!(pb.integration_branch(), "develop");
        }
    }

    #[test]
    fn reports_unknown_keys_instead_of_refusing_them() {
        // A typo, a section a newer nitely added, and a key inside a list entry.
        let pb = Playbook::parse("[gate]\ncomand = [\"x\"]\n[future]\nx = 1\n[[gate.steps]]\nrun = \"y\"\nnew_rule = true\n").unwrap();
        let mut unknown = pb.unknown.clone();
        unknown.sort();
        assert_eq!(unknown, ["future", "gate.comand", "gate.steps.0.new_rule"]);
        assert_eq!(pb.gate.steps[0].run, "y", "the known keys around them still count");
        assert!(pb.unknown_warnings().iter().any(|w| w.contains("`gate.comand`") && w.contains("ignored")));
        assert_eq!(Playbook::parse("").unwrap().branch.integration, "main");
        assert!(Playbook::parse("[gate]\ncommands = 3").is_err(), "a known key of the wrong type is still an error");
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
        let humans = IssueRecord { owner: "human".into(), ..issue() };
        assert!(start_task(&db, &repo, &humans, "claude", &actor).unwrap_err().to_string().contains("human's own work"));

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
    fn finds_the_repo_of_the_issues_project_not_the_callers() {
        let base = std::env::temp_dir().join(format!("nitely-repos-{}-{}", std::process::id(), now()));
        let init = |name: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let sh = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success(), "{args:?}");
            sh(&["init", "-q", "-b", "main"]);
            sh(&["commit", "-q", "--allow-empty", "-m", "init"]);
            dir.to_string_lossy().into_owned()
        };
        let (app, site, loose) = (init("app"), init("site"), init("loose"));
        let repo = |name: &str, root: &str, project: &str| crate::RepoConfig {
            name: name.into(),
            root: root.into(),
            integration_branch: "main".into(),
            project: Some(project.into()),
        };
        let mut config = crate::Config { repos: vec![repo("app", &app, "APP"), repo("site", &site, "WEB")], ..Default::default() };
        let same = |got: PathBuf, want: &str| assert_eq!(crate::paths::normalize(&got.to_string_lossy()), crate::paths::normalize(want));

        same(repo_for_issue(&config, Some("WEB"), &app).unwrap(), &site);
        same(repo_for_issue(&config, Some("web"), &loose).unwrap(), &site);
        same(repo_for_issue(&config, None, &app).unwrap(), &app);
        let refused = repo_for_issue(&config, Some("OPS"), &app).unwrap_err().to_string();
        assert!(refused.contains("holds APP's work") && refused.contains("--project OPS"), "{refused}");
        same(repo_for_issue(&config, Some("OPS"), &loose).unwrap(), &loose);

        config.repos.push(repo("app-2", &loose, "APP"));
        same(repo_for_issue(&config, Some("APP"), &loose).unwrap(), &loose);
        assert!(repo_for_issue(&config, Some("APP"), &site).unwrap_err().to_string().contains("several repos"));

        let worktree = base.join("app-nit-3");
        let out = Command::new("git").arg("-C").arg(&app).args(["worktree", "add", "-q", "-b", "claude/nit-3-x"]).arg(&worktree).arg("main").output().unwrap();
        assert!(out.status.success(), "{out:?}");
        same(issue_worktree(Path::new(&app), "NIT-3").unwrap().unwrap(), &worktree.to_string_lossy());
        assert!(issue_worktree(Path::new(&app), "NIT-4").unwrap().is_none());
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

    /// CI on GitHub runs Nitely's own gate: every gate command is a `- run:` step of the workflow,
    /// spelled the same, and the workflow runs no other cargo or npm command.
    #[test]
    fn ci_runs_the_same_gate_as_the_playbook() {
        let pb = Playbook::parse(include_str!("../../../.nitely/playbook.toml")).unwrap();
        let ci = include_str!("../../../.github/workflows/ci.yml");
        let runs: Vec<&str> = ci.lines().filter_map(|l| l.trim().strip_prefix("- run: ")).map(str::trim).collect();
        for command in pb.gate.commands.iter().chain(pb.gate.steps.iter().map(|s| &s.run)) {
            assert!(runs.contains(&command.as_str()), "the CI workflow does not run the gate command {command:?}");
        }
        for run in runs.iter().filter(|r| r.starts_with("cargo ") || r.starts_with("npm ")) {
            assert!(*run == "npm ci" || pb.gate.commands.iter().any(|c| c == run), "the CI workflow runs {run:?}, which the gate does not");
        }
    }
}
