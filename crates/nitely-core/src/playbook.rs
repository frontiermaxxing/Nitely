//! A project's playbook: how a task becomes a branch and a worktree, how the worktree is set up,
//! and which commands gate a change. It lives in the repo at `.nitely/playbook.toml`, versioned
//! with the code it governs, so every agent and every machine follows the same rules. `review.rs`
//! and `submit.rs` build on it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};

pub const PLAYBOOK_PATH: &str = ".nitely/playbook.toml";

/// Output kept per gate command: the end of it, where failures are.
const OUTPUT_TAIL: usize = 4000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Playbook {
    #[serde(skip)]
    pub mode_authority: Option<String>,
    #[serde(skip)]
    pub mode_project: Option<String>,
    /// Mechanical verification recommendations. Shadow never replaces the normal gate/review.
    #[serde(default)]
    pub verification: crate::verification::Rules,
    #[serde(default)]
    pub release: Option<crate::release_verification::Rules>,
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
    pub crew: crate::crew_rules::CrewRules,
    #[serde(default)]
    pub schedule: crate::test_sweep::Schedule,
    /// Build lanes (docs/build-lanes.md); none unless the playbook has a `[lanes]` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lanes: Option<crate::lanes::LaneRules>,
    /// Submodules whose branch lands with the repo's (`[[submodules]]`, docs/playbooks.md).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub submodules: Vec<crate::submodules::SubmoduleRules>,
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
    /// Landing changes together (docs/batching.md, "The land queue"): `off` lands each alone;
    /// `waiting` queues a change while a land runs and lands what waited together, gated once;
    /// `all` also waits `batch_window` for more.
    #[serde(default)]
    pub batch: LandBatch,
    /// With `batch = "all"`: how long a queue run waits for more changes, e.g. "5m" ("0": none).
    #[serde(default)]
    pub batch_window: Option<String>,
    /// Changes per queue run (default 5).
    #[serde(default)]
    pub batch_max: Option<u32>,
}

/// How a project lands reviewed changes: one at a time, or together in a queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LandBatch {
    /// Each change lands on its own, as it always has.
    #[default]
    Off,
    /// A change that arrives while a land runs waits, and what waited lands together.
    Waiting,
    /// As `Waiting`, and a queue run also waits `batch_window` for more.
    All,
}

impl LandRules {
    /// Changes per queue run.
    pub fn batch_max(&self) -> usize {
        self.batch_max.filter(|n| *n > 0).unwrap_or(5) as usize
    }

    /// How long a queue run waits for more changes: only with `batch = "all"`.
    pub fn batch_window(&self) -> std::time::Duration {
        let secs = match (self.batch, self.batch_window.as_deref()) {
            (LandBatch::All, Some(w)) => duration_secs(w).unwrap_or(0),
            _ => 0,
        };
        std::time::Duration::from_secs(secs)
    }
}

/// "90s", "5m", "2h" or a plain number of seconds, as seconds.
fn duration_secs(text: &str) -> Option<u64> {
    let t = text.trim();
    let (n, unit) = match t.char_indices().last()? {
        (i, 's') => (&t[..i], 1),
        (i, 'm') => (&t[..i], 60),
        (i, 'h') => (&t[..i], 3600),
        _ => (t, 1),
    };
    n.trim().parse::<u64>().ok().map(|n| n * unit)
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
    /// "full" (the default): a worktree with its setup run, built in place. "light": source only,
    /// no setup, sharing the repo's objects (docs/build-lanes.md, `light.rs`).
    pub mode: WorktreeMode,
    /// Light worktrees only: cone-mode directories every worktree starts with, plus those of the
    /// paths the issue names; empty means whole. Widened on demand (`widen`).
    #[serde(default)]
    pub sparse: Vec<String>,
    /// Run in the worktree, in order, before land removes it: undo what setup made that
    /// `git worktree remove` cannot take down (a sparse checkout of a submodule, a nested worktree
    /// git refuses to remove with). A failure is reported and the removal still tried.
    #[serde(default)]
    pub teardown: Vec<String>,
}

/// How `start_task` makes a worktree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeMode {
    #[default]
    Full,
    Light,
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

/// The dependency files of each kind of tool a setup command starts with.
const JS_FILES: &[&str] = &["package.json", "package-lock.json", "npm-shrinkwrap.json", "pnpm-lock.yaml", "yarn.lock", "bun.lockb"];
const PYTHON_FILES: &[&str] = &["requirements.txt", "poetry.lock", "uv.lock", "Pipfile.lock"];

/// The dependency files `setup`'s commands read (NIT-287): `npm ci` reads package.json and the JS
/// lockfiles, not Cargo.lock, so a lane serving branches that differ only in Cargo.lock does not
/// install node_modules again for each. A command whose tool it does not know (`make`, a script)
/// may read any of them, as does a setup with no tool at all; `cd` and `set` count for nothing.
pub(crate) fn setup_files_for(setup: &[String]) -> Vec<&'static str> {
    let mut files: Vec<&'static str> = Vec::new();
    for part in setup.iter().flat_map(|c| c.split(['&', '|', ';', '\n'])) {
        // The program, past any `NAME=value` the command sets first.
        let Some(first) = part.split_whitespace().find(|w| !w.contains('=')) else { continue };
        let name = first.rsplit(['/', '\\']).next().unwrap_or(first).to_ascii_lowercase();
        let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".cmd")).unwrap_or(&name);
        let these: &[&'static str] = match name {
            "cd" | "set" => continue,
            "npm" | "npx" | "pnpm" | "yarn" | "bun" | "node" | "corepack" => JS_FILES,
            "cargo" | "rustup" => &["Cargo.lock"],
            "pip" | "pip3" | "python" | "python3" | "py" | "poetry" | "uv" | "pipenv" => PYTHON_FILES,
            "go" => &["go.sum"],
            "bundle" | "bundler" | "gem" => &["Gemfile.lock"],
            _ => SETUP_FILES,
        };
        for f in these {
            if !files.contains(f) {
                files.push(f);
            }
        }
    }
    if files.is_empty() {
        return SETUP_FILES.to_vec();
    }
    files.sort_by_key(|f| SETUP_FILES.iter().position(|s| s == f));
    files
}

/// The dependency files among `files` (paths relative to the repo root): what makes a checkout's
/// setup stale when it changes.
pub fn setup_files_in(rules: &WorktreeRules, files: &[String]) -> Vec<String> {
    let read = setup_files_for(&rules.setup);
    files
        .iter()
        .filter(|f| read.contains(&f.as_str()) || rules.setup_when_changed.iter().any(|w| w.trim_start_matches("./") == f.as_str()))
        .cloned()
        .collect()
}

/// Where a worktree keeps the fingerprint of the dependency files its setup last ran against:
/// in its own git folder, so it is never committed and goes away with the worktree.
pub(crate) fn setup_record(worktree: &Path) -> Option<PathBuf> {
    let path = git(worktree, &["rev-parse", "--git-path", "nitely-setup"]).ok()?;
    let path = PathBuf::from(path.trim());
    Some(if path.is_absolute() { path } else { worktree.join(path) })
}

/// Whether `worktree` exists and its setup passed: `start_task` records that only when every setup
/// command passed, so a worktree whose setup failed or was cut off has no record.
pub fn setup_passed(worktree: &Path) -> bool {
    worktree.join(".git").exists() && setup_record(worktree).is_some_and(|r| r.exists())
}

/// FNV-1a over the names and contents of the dependency files present that its setup reads
/// (`setup_files_for`), stable across builds.
pub(crate) fn setup_fingerprint(rules: &WorktreeRules, worktree: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for name in setup_files_for(&rules.setup).into_iter().chain(rules.setup_when_changed.iter().map(String::as_str)) {
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
/// and false when one failed; a command that failed and then passed goes in `flakes`.
pub(crate) fn refresh_setup(rules: &WorktreeRules, worktree: &Path, main: &Path, base: &str, retries: u32, flakes: &mut Vec<Flake>) -> Result<(Vec<CommandResult>, bool)> {
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
        let (ok, output, seconds, flake) = run_retrying(&command, worktree, retries)?;
        let command = format!("{command} (setup again: a dependency file changed)");
        flakes.extend(flake.map(|f| Flake { command: command.clone(), ..f }));
        results.push(CommandResult { command, ok, seconds, output, skipped: false, timed_out: false });
        if !ok {
            return Ok((results, false));
        }
    }
    std::fs::write(&record, &now).ok();
    Ok((results, true))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateRules {
    /// Must all pass before a change can land. Run in the task's worktree, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Commands that run only for some changes, after `commands`.
    #[serde(default)]
    pub steps: Vec<GateStep>,
    /// How many times a failing command (or a setup run again before the gate) runs again at the
    /// same commit before the gate fails. One that then passes is a flake: the gate passes and
    /// says so, with the tests that failed (a `gate_flaked` event). 0 turns this off.
    #[serde(default = "one")]
    pub retries: u32,
}

impl Default for GateRules {
    fn default() -> Self {
        GateRules { commands: vec![], steps: vec![], retries: 1 }
    }
}

fn one() -> u32 {
    1
}

/// A gate command that failed and then passed at the same commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flake {
    pub command: String,
    /// The tests the failed runs named (cargo's `test x ... FAILED` and closing `failures:` list,
    /// node:test's `not ok`), across every failed run.
    pub tests: Vec<String>,
    /// The end of the first failed run's output.
    pub output: String,
}

/// Runs `command`, and again up to `retries` times while it fails. Returns the last run, with
/// its time counting every run, and the failures it got past when a later run passed.
fn run_retrying(command: &str, dir: &Path, retries: u32) -> Result<(bool, String, u64, Option<Flake>)> {
    let mut runs = vec![run_shell(command, dir)?];
    while !runs.last().expect("one run").0 && runs.len() <= retries as usize {
        runs.push(run_shell(command, dir)?);
    }
    Ok(classify_runs(command, runs))
}

/// A command's runs, in order (each passed, its output and seconds), as one result (NIT-537): the
/// last run's outcome and output, the time of them all, and, when a later run passed, a flake with
/// the tests the failed runs named, each once, and the first failure's output.
fn classify_runs(command: &str, mut runs: Vec<(bool, String, u64)>) -> (bool, String, u64, Option<Flake>) {
    let seconds = runs.iter().map(|r| r.2).sum();
    let (ok, output, _) = runs.pop().unwrap_or((false, String::new(), 0));
    let mut failed: Vec<String> = runs.into_iter().map(|r| r.1).collect();
    let flake = (ok && !failed.is_empty()).then(|| {
        let mut tests: Vec<String> = Vec::new();
        for name in failed.iter().flat_map(|out| failed_tests(out)) {
            if !tests.contains(&name) {
                tests.push(name);
            }
        }
        Flake { command: command.to_string(), tests, output: failed.swap_remove(0) }
    });
    (ok, output, seconds, flake)
}

/// The tests a run's output says failed, each once, in order: cargo's `test x ... FAILED` lines
/// and the `failures:` list it closes with (the end of a long run, which is what is kept of it),
/// and node:test's `not ok N - name`.
pub fn failed_tests(output: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in output.lines() {
        let line = line.trim_end();
        // With per-test times on (test_budget.rs), the line ends in the time: `... FAILED <1.234s>`.
        let untimed = match line.rsplit_once(" <") {
            Some((rest, time)) if time.ends_with("s>") => rest,
            _ => line,
        };
        let name = if let Some(rest) = untimed.strip_prefix("test ").and_then(|r| r.strip_suffix(" ... FAILED")) {
            rest.trim()
        } else if let Some(rest) = line.strip_prefix("not ok ") {
            rest.split_once(" - ").map(|(_, n)| n.trim()).unwrap_or("")
        } else if line == "failures:" {
            in_list = true;
            ""
        } else if in_list && line.starts_with("    ") && !line.trim().contains(' ') {
            line.trim()
        } else {
            // The list is names only; the first `failures:` block, of each test's output, is not.
            in_list = in_list && (line.is_empty() || line.starts_with("    ")) && !line.starts_with("----");
            ""
        };
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
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
    /// A `[[land.after]]` step only: run once the land lock is released, in a checkout of the
    /// integration branch kept for these steps beside the lanes (`{worktree}` names it, `{main}` the
    /// main checkout), so the next land does not wait for it; lands that finish meanwhile run it
    /// once more at the newest commit rather than once each (NIT-334).
    pub outside_lock: bool,
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

    /// Why a changed path raises the review level to sensitive, when one matches a human or
    /// blocked path. None when none do. Path rules only raise.
    pub fn protected_hit(&self, paths: &[String]) -> Result<Option<String>> {
        let mut hits = Vec::new();
        for rule in self.human_paths.iter().chain(&self.blocked_paths) {
            let re = regex::Regex::new(&rule.pattern).with_context(|| format!("approval pattern {}", rule.pattern))?;
            for path in paths {
                let path = path.replace('\\', "/");
                if re.is_match(&path) {
                    hits.push(format!("{} ({path})", rule.reason));
                }
            }
        }
        Ok((!hits.is_empty()).then(|| hits.join("; ")))
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
        WorktreeRules {
            dir: "../{repo}-worktrees/{key}-{slug}".into(),
            setup: vec![],
            setup_when_changed: vec![],
            mode: WorktreeMode::Full,
            sparse: vec![],
            teardown: vec![],
        }
    }
}

impl Playbook {
    /// The repo's playbook: its own `.nitely/playbook.toml` when it has one, else the one Nitely
    /// keeps for it (written when the repo was connected), else the defaults.
    /// What it leaves unset of the review and landing rules comes from the app-wide defaults
    /// (`rules::RuleDefaults`).
    pub fn load(repo_root: &Path) -> Result<Playbook> {
        Playbook::load_from(repo_root, crate::Config::load().ok().as_ref(), false)
    }

    /// `load` with a config the caller already read, so nothing more is read and nothing written
    /// (`Config::load` may tidy files): for read-only callers like the live preview.
    pub fn load_with(repo_root: &Path, config: &crate::Config) -> Result<Playbook> {
        Playbook::load_from(repo_root, Some(config), false)
    }

    fn load_from(repo_root: &Path, config: Option<&crate::Config>, require_policy: bool) -> Result<Playbook> {
        // Tests build their own playbooks; the defaults of whoever runs them must not leak in.
        let none = crate::rules::RuleDefaults::default();
        let defaults = if cfg!(test) { &none } else { config.map(|c| &c.rule_defaults).unwrap_or(&none) };
        // Large-repo mode: while a build request has the main checkout at an agent's snapshot, its
        // playbook there is the agent's; the rules are the ones on the branch it goes back to.
        let from_branch = |branch: &str| -> Option<Result<Playbook>> {
            let text = git(repo_root, &["show", &format!("{branch}:{PLAYBOOK_PATH}")]).ok()?;
            Some(Playbook::parse(&text).with_context(|| format!("in {PLAYBOOK_PATH} on {branch}")).map(|mut pb| {
                crate::rules::apply_defaults(&mut pb, &text, defaults);
                pb
            }))
        };
        let read = |path: &Option<PathBuf>| -> Result<Option<String>> {
            match path {
                Some(path) => Ok(Some(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)),
                None => Ok(None),
            }
        };
        // Read until the checkout's state is the same before and after the read (each lend and
        // each return moves the count), so what was read is what that state allows: the file on
        // disk, unless a request's commit with a playbook of its own may be there. A lend lasts a
        // whole build, so a read settles at once in practice; one that never does is an error,
        // never a guess at the rules.
        let mut settled = None;
        for attempt in 0..300 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let before = crate::lanes::lends(repo_root);
            if let Some(pb) = before.lent.as_deref().and_then(from_branch) {
                return pb;
            }
            let path = if crate::lanes::playbook_on_disk_is_theirs(repo_root, &before) {
                Playbook::path_in(repo_root, config)
            } else {
                Playbook::stored_path_for(repo_root, config)
            };
            let text = read(&path)?;
            let after = crate::lanes::lends(repo_root);
            if after == before {
                settled = Some((path, text));
                break;
            }
        }
        let Some((path, text)) = settled else {
            bail!("{}: build requests kept taking and giving back the main checkout while its playbook was read; try again", repo_root.display());
        };
        let (Some(path), Some(text)) = (path, text) else {
            if require_policy {
                bail!("target project policy is unavailable in {}", repo_root.display());
            }
            let mut pb = Playbook::default();
            crate::rules::apply_defaults(&mut pb, "", defaults);
            return Ok(pb);
        };
        let mut pb = Playbook::parse(&text).with_context(|| format!("in {}", path.display()))?;
        crate::rules::apply_defaults(&mut pb, &text, defaults);
        Ok(pb)
    }

    /// Where the repo's playbook is read from, if it has one.
    pub fn path_for(repo_root: &Path) -> Option<PathBuf> {
        let in_repo = repo_root.join(PLAYBOOK_PATH);
        if in_repo.exists() {
            return Some(in_repo);
        }
        Playbook::path_in(repo_root, crate::Config::load().ok().as_ref())
    }

    fn path_in(repo_root: &Path, config: Option<&crate::Config>) -> Option<PathBuf> {
        let in_repo = repo_root.join(PLAYBOOK_PATH);
        if in_repo.exists() {
            return Some(in_repo);
        }
        Playbook::stored_path_for(repo_root, config)
    }

    /// The playbook Nitely keeps for `repo_root`, when it is connected and has one there.
    fn stored_path_for(repo_root: &Path, config: Option<&crate::Config>) -> Option<PathBuf> {
        let config = config?;
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

    /// The folder this playbook puts a repo's task worktrees in (the parent of each one).
    pub fn worktrees_folder(&self, repo_root: &Path) -> Option<PathBuf> {
        let probe = IssueRecord { key: "X-1".into(), title: "x".into(), ..Default::default() };
        self.worktree_for(repo_root, "x", &probe).parent().map(Path::to_path_buf)
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
pub(crate) fn normalize_lexically(p: &Path) -> PathBuf {
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

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let key = if settled_query(args) {
        Some(format!("{}\0{}", dir.to_string_lossy(), args.join("\0")))
    } else {
        where_query(dir, args)
    };
    let Some(key) = key else {
        return ask(dir, args);
    };
    if let Some(hit) = SETTLED.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(hit.clone());
    }
    let out = ask(dir, args)?;
    let mut memo = SETTLED.lock().unwrap_or_else(|e| e.into_inner());
    if memo.len() >= SETTLED_MAX {
        memo.clear();
    }
    memo.insert(key, out.clone());
    Ok(out)
}

/// git's answer to `args` in `dir`: read in this process when git_read knows the question
/// (NIT-518), from the git CLI otherwise.
fn ask(dir: &Path, args: &[&str]) -> Result<String> {
    if let Some(answer) = crate::git_read::answer(dir, args) {
        return answer;
    }
    finish(shell_free_command("git").arg("-C").arg(dir).args(args), args)
}

/// Answers to `settled_query` and `where_query` questions, by folder and arguments. A land or a submit asks the same
/// ones several times (the verification plan reads both trees in the submit, again in its gate and
/// again in the land), and on Windows each git costs 50 to 400 ms (NIT-458).
static SETTLED: std::sync::Mutex<std::collections::BTreeMap<String, String>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
const SETTLED_MAX: usize = 2048;

/// The memo's key for a question about where `dir`'s repository is (its git folder, its common
/// folder, a path inside it, its top level), asked with `dir` at the top of a checkout: the answer
/// depends only on `dir` and its `.git`, so the key holds both. A `.git` folder is a main checkout,
/// whose answers follow from its path; a `.git` file names a worktree's git folder, and a worktree
/// removed and added again elsewhere writes another. None for any other question, for a folder
/// below the top, or when the environment points git elsewhere.
fn where_query(dir: &Path, args: &[&str]) -> Option<String> {
    let asks_where = match args {
        ["rev-parse", "--path-format=absolute", "--git-common-dir" | "--git-dir"] | ["rev-parse", "--git-common-dir" | "--absolute-git-dir" | "--show-toplevel"] => true,
        ["rev-parse", rest @ ..] => {
            let rest = rest.strip_prefix(&["--path-format=absolute"]).unwrap_or(rest);
            !rest.is_empty() && rest.chunks(2).all(|pair| matches!(pair, ["--git-path", p] if !p.is_empty()))
        }
        _ => false,
    };
    if !asks_where || ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE"].iter().any(|v| std::env::var_os(v).is_some()) {
        return None;
    }
    let dot_git = dir.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    let which = if meta.is_dir() {
        "folder".to_string()
    } else if meta.is_file() && meta.len() < 4096 {
        std::fs::read_to_string(&dot_git).ok()?
    } else {
        return None;
    };
    Some(format!("{}\0{}\0{}", dir.to_string_lossy(), which.trim(), args.join("\0")))
}

/// Whether git's answer to `args` depends only on objects named by their full id, so no commit,
/// ref move, checkout or fetch can change it: the commit or tree an id names, a tree's listing,
/// a file's size at a commit, the merge base of two commits. Only answers git gave are kept, so an
/// object that is missing now is asked about again.
fn settled_query(args: &[&str]) -> bool {
    fn id(s: &str) -> bool {
        matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
    }
    let peeled = |s: &str| s.strip_suffix("^{commit}").or_else(|| s.strip_suffix("^{tree}")).is_some_and(id);
    match args {
        ["rev-parse", "--verify", r] => peeled(r),
        ["rev-parse", rest @ ..] => !rest.is_empty() && rest.iter().all(|r| peeled(r)),
        ["ls-tree", "-r", "-z", r] | ["ls-tree", "-r", "--name-only", r] => id(r),
        ["cat-file", "-s", r] => r.split_once(':').is_some_and(|(c, _)| id(c)),
        ["merge-base", a, b] => id(a) && id(b),
        _ => false,
    }
}

fn finish(cmd: &mut Command, args: &[&str]) -> Result<String> {
    let out = run_git(cmd).with_context(|| format!("starting git {}", args.join(" ")))?;
    checked(out, args)
}

/// `git` in `dir` with `input` on its stdin, for the commands that read their list of commits
/// there (`diff-tree --stdin`); started again like run_git when git never ran.
pub(crate) fn git_with_input(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    use std::io::Write;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let last = attempt == 3;
        let mut cmd = shell_free_command("git");
        cmd.arg("-C").arg(dir).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match cmd.spawn() {
            Err(e) if !last && e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(e) => return Err(e).with_context(|| format!("starting git {}", args.join(" "))),
            Ok(mut child) => {
                let mut stdin = child.stdin.take().context("git's stdin")?;
                let input = input.to_string();
                // Written while git runs, so a large output cannot stall it; git's exit status says
                // whether it read what it needed.
                let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
                let out = child.wait_with_output().with_context(|| format!("running git {}", args.join(" ")))?;
                writer.join().ok();
                if last || !never_ran(&out) {
                    return checked(out, args);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
    }
}

/// git's output, or an error with what it said (or its exit code when it said nothing).
fn checked(out: std::process::Output, args: &[&str]) -> Result<String> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = if stderr.trim().is_empty() { format!("exited with {} and said nothing", exit_code(&out.status)) } else { stderr.trim().to_string() };
        bail!("git {}: {why}", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs a git command, again up to twice when git never ran. On a Windows machine starting many
/// processes at once (parallel gates), starting git can fail with "Access is denied", or Git for
/// Windows' `git.exe` launcher can fail to start the real git and exit with -1 having said and done
/// nothing (NIT-116). Nothing ran either way, so running it again is safe; any other failure is
/// git's own answer and is returned as it is.
pub fn run_git(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    let mut real;
    let cmd = match real_git(cmd) {
        Some(c) => {
            real = c;
            &mut real
        }
        None => cmd,
    };
    no_optional_locks(cmd);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let last = attempt == 3;
        match cmd.output() {
            Err(e) if !last && e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Ok(out) if !last && never_ran(&out) => {}
            done => return done,
        }
        std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
    }
}

/// A failed git that printed nothing at all with the launcher's -1: it never got to run.
fn never_ran(out: &std::process::Output) -> bool {
    cfg!(windows) && out.status.code() == Some(-1) && out.stdout.is_empty() && out.stderr.is_empty()
}

/// An exit status as a number, in hex on Windows, where a process that never ran reports an
/// NTSTATUS like 0xc0000142.
fn exit_code(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(c) if cfg!(windows) => format!("{c} (0x{:08x})", c as u32),
        Some(c) => c.to_string(),
        None => status.to_string(),
    }
}

/// Whether the repo has an `origin` remote. A repo without one is local: submit and land keep the
/// change in it instead of failing on a push nobody can receive. Only a listing that ran and names
/// no origin counts as none: a git that fails to run keeps the push, which then says why.
pub(crate) fn has_origin(dir: &Path) -> bool {
    git(dir, &["remote"]).map_or(true, |out| out.lines().any(|l| l.trim() == "origin"))
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
#[cfg(any(test, feature = "test-support"))]
pub fn posix_sh() -> String {
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

pub fn shell_free_command(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = if program == "git" { Command::new(git_program()) } else { Command::new(program) };
    if program == "git" {
        no_optional_locks(&mut cmd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// The git to start. On Windows, PATH's git is usually Git for Windows' `cmd\git.exe`, a launcher
/// that only starts `mingw64\bin\git.exe`: two process starts for every git call, where starting a
/// process can cost hundreds of milliseconds (NIT-517). The real binary sets up its own environment
/// (its tools on PATH, MSYSTEM), so it is started directly: found once per process beside the
/// launcher PATH names, or from `git --exec-path` when PATH's git is some other launcher. PATH's
/// `git` stays the fallback, and is what every other platform runs.
pub fn git_program() -> &'static std::ffi::OsStr {
    static GIT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    GIT.get_or_init(|| if cfg!(windows) { resolve_real_git().unwrap_or_else(|| "git".into()) } else { "git".into() }).as_os_str()
}

fn resolve_real_git() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path).map(|d| d.join("git.exe")).find(|p| p.is_file())?;
    if let Some(real) = real_git_beside(&found, |p| p.is_file()) {
        return Some(real);
    }
    // Some other launcher (a package manager's shim): ask it where git keeps its programs,
    // <prefix>/libexec/git-core, whose <prefix>/bin holds the real git. One start, once.
    let mut cmd = Command::new(&found);
    cmd.arg("--exec-path");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    let exec = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let real = exec.parent()?.parent()?.join("bin").join("git.exe");
    real.is_file().then_some(real).or(Some(found))
}

/// The real git for the `git.exe` found on PATH, when that is one of Git for Windows' launchers
/// (`<root>\cmd\git.exe` or `<root>\bin\git.exe`) and the real one is beside it; the found one
/// itself when it already is the real one (`<root>\mingw64\bin\git.exe`); None otherwise.
fn real_git_beside(found: &Path, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let dir = found.parent()?;
    let named = |d: &Path, n: &str| d.file_name().is_some_and(|f| f.eq_ignore_ascii_case(n));
    const BUILDS: [&str; 3] = ["mingw64", "clangarm64", "mingw32"];
    if named(dir, "bin") && dir.parent().is_some_and(|p| BUILDS.iter().any(|b| named(p, b))) {
        return Some(found.to_path_buf());
    }
    if !(named(dir, "cmd") || named(dir, "bin")) {
        return None;
    }
    let root = dir.parent()?;
    BUILDS.iter().map(|b| root.join(b).join("bin").join("git.exe")).find(|p| exists(p))
}

/// `cmd` again with the real git as its program, when it names PATH's `git` and that is not what
/// git_program resolved to: its arguments, environment and folder copied, without a window.
fn real_git(cmd: &Command) -> Option<Command> {
    if cmd.get_program() != "git" || git_program() == "git" {
        return None;
    }
    let mut real = shell_free_command("git");
    real.args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => real.env(k, v),
            None => real.env_remove(k),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        real.current_dir(dir);
    }
    Some(real)
}

/// No lock git takes only to save work for later: `git status` and `git diff` otherwise refresh a
/// worktree's own index under index.lock, and a Nitely process killed mid-way (a stopped `run` or
/// `submit`, a board refresh) would leave that lock for the agent's next commit to fail on
/// (NIT-341). Commands that must write the index still lock it.
pub(crate) fn no_optional_locks(cmd: &mut Command) {
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
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
        cmd.args(["-c", &sh_script(command)]);
    }
    cmd
}

/// A command of several lines for `sh -c`, which runs every line but exits with the last one's
/// code: `set -e` first, so the first line that fails fails the whole (NIT-392). One shell keeps
/// what a line sets (`cd`, a variable) and quoted line breaks. A one-line command is as written.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn sh_script(command: &str) -> String {
    let trimmed = command.trim_end_matches(['\r', '\n']);
    if trimmed.contains('\n') { format!("set -e\n{trimmed}") } else { command.to_string() }
}

/// Why this platform's shell cannot run `command` faithfully, if it cannot: `cmd /C` runs only up to
/// the first line break, and joining the lines would change what `rem` and `%VAR%` mean, so on
/// Windows a command of several lines is refused instead of passing on its first line (NIT-392).
pub(crate) fn unsupported_command(command: &str, windows: bool) -> Option<String> {
    // Any break before the end counts: cmd stops at a leading one too, running nothing.
    (windows && command.trim_end_matches(['\r', '\n']).contains(['\r', '\n'])).then(|| {
        "on Windows a command runs through cmd, which stops at the first line break: send each line as its own run, or join them with && on one line".to_string()
    })
}

/// Runs one playbook command through the platform shell, held to the machine's share for one
/// build as a lane command is: its cores, its memory cap, below-normal priority (NIT-266). What it
/// leaves running (a setup's daemon) keeps running, still held to them. A command over its cap is
/// stopped, and that is an error, not the command's failure.
pub(crate) fn run_shell(command: &str, dir: &Path) -> Result<(bool, String, u64)> { run_shell_kind(command,dir,"build") }

pub(crate) fn run_shell_kind(command: &str, dir: &Path, kind: &str) -> Result<(bool, String, u64)> {
    use std::io::Read;
    let share = crate::lanes::share();
    let command = crate::lanes::tests_share(command, share.jobs);
    let mut cmd = shell_command(&command);
    cmd.current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::lanes::share_env(&mut cmd, share.jobs);
    if kind == "install" {
        crate::install::inherit_install_lock(&mut cmd, dir);
    }
    let started = Instant::now();
    let guard = crate::procs::TreeGuard::with(crate::procs::Contain { memory_cap: share.cap, low_priority: true, end_with_guard: false });
    let spawned = match &guard {
        Some(g) => g.spawn(&mut cmd),
        None => cmd.spawn(),
    };
    let mut child = spawned.with_context(|| format!("running {command}"))?;
    let tracked = crate::performance_attribution::track_child(&child,kind);
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut p) = pipe {
                p.read_to_end(&mut bytes).ok();
            }
            bytes
        })
    };
    let out = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let over = || guard.as_ref().is_some_and(|g| g.over_memory());
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if over() {
            if let Some(g) = &guard {
                g.stop();
            }
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if let Some(t) = tracked { t.finish(guard.as_ref().and_then(|g|g.accounting())); }
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&err.join().unwrap_or_default()));
    let Some(status) = status.filter(|_| !over()) else {
        let cap = share.cap.map(crate::machine::gb).unwrap_or_default();
        bail!("out of memory: `{command}` and what it started went over its {cap} memory cap and were stopped (Settings > Build lanes sets the cap)\n{}", tail(&text));
    };
    Ok((status.success(), tail(&text), started.elapsed().as_secs()))
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
    let main = crate::git::main_root(dir)?;
    Some(config.repo_for_main(&main).map_or_else(|| PathBuf::from(&main), |r| PathBuf::from(&r.root)))
}

/// The main checkout an issue's work happens in: the repo linked to the issue's project, so an
/// agent working across projects starts, gates and lands in the right one. When several repos name
/// the project, the one `dir` is in; when none does, the repo `dir` is in, unless that repo holds
/// another project's work.
pub fn repo_for_issue(config: &crate::Config, project: Option<&str>, dir: &str) -> Result<PathBuf> {
    // Asked of git once: the main checkout `dir` is in, and the repo it is.
    let main = crate::git::main_root(dir);
    let here = main.as_deref().and_then(|m| config.repo_for_main(m));
    choose_repo(&config.repos, project, main.as_deref(), here, dir)
}

/// The repo an issue of `project` belongs to (NIT-537), from the linked repos, the main checkout
/// the caller is in (`main`) and the linked repo that is (`here`): the one linked to the project;
/// of several, the caller's; with none linked, the caller's own checkout, unless it holds another
/// project's work.
pub(crate) fn choose_repo(repos: &[crate::config::RepoConfig], project: Option<&str>, main: Option<&str>, here: Option<&crate::config::RepoConfig>, dir: &str) -> Result<PathBuf> {
    let root_here = || main.map(|m| here.map_or_else(|| PathBuf::from(m), |r| PathBuf::from(&r.root)));
    let Some(project) = project else {
        return root_here().with_context(|| format!("{dir} is not inside a git repo"));
    };
    let is_project = |r: &crate::config::RepoConfig| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project));
    let linked: Vec<&crate::config::RepoConfig> = repos.iter().filter(|r| is_project(r)).collect();
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
    root_here().with_context(|| format!("{dir} is not inside a git repo, and no repo is linked to {project}"))
}

/// The review level to store for a declaration, raised to sensitive when `paths` hit a human or
/// blocked path in `repo`'s playbook. An empty path list leaves the declaration as given.
/// `undeclared` and an empty declaration are no declaration.
pub fn review_declaration(repo: &Path, declared: Option<&str>, paths: &[String]) -> Result<(Option<String>, Option<String>)> {
    if paths.is_empty() {
        return crate::issues::resolve_review_level(declared, None);
    }
    let hit = Playbook::load(repo)?.approval.protected_hit(paths)?;
    crate::issues::resolve_review_level(declared, hit.as_deref())
}

/// Resolve path protection using the target project's authoritative repository, never a remote
/// session label or an unrelated caller's checkout. A remote caller supplies no local directory.
/// Unknown, unavailable or ambiguous policy cannot silently turn protected work into a low level.
pub fn project_review_declaration(
    config: &crate::Config,
    project: Option<&str>,
    dir: Option<&str>,
    declared: Option<&str>,
    paths: &[String],
) -> Result<(Option<String>, Option<String>)> {
    if paths.is_empty() {
        return crate::issues::resolve_review_level(declared, None);
    }
    let project = project.context("path-based review needs a target project")?;
    let linked: Vec<_> = config.repos.iter().filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project))).collect();
    let root = match linked.as_slice() {
        [] => bail!("no repository policy is connected to {project}; connect its repository before declaring paths"),
        [one] => PathBuf::from(&one.root),
        _ => repo_for_issue(config, Some(project), dir.context("several repositories are linked to the target project; remote path policy is ambiguous")?)?,
    };
    if !root.is_dir() {
        bail!("target project repository is unavailable: {}", root.display());
    }
    let pb = Playbook::load_from(&root, Some(config), true)?;
    if !pb.unknown.is_empty() {
        bail!("target project policy has unknown fields: {}", pb.unknown.join(", "));
    }
    let hit = pb.approval.protected_hit(paths)?;
    crate::issues::resolve_review_level(declared, hit.as_deref())
}

/// The worktree of `root` whose branch names the issue; None when there is none.
pub fn issue_worktree(root: &Path, key: &str) -> Result<Option<PathBuf>> {
    issue_worktree_in(&crate::git::list_worktrees(&root.to_string_lossy())?, root, key)
}

/// Of `worktrees`, the one whose branch names the issue `key` (NIT-537): never the main checkout,
/// and an error when several do.
pub(crate) fn issue_worktree_in(worktrees: &[crate::git::Worktree], root: &Path, key: &str) -> Result<Option<PathBuf>> {
    let named: Vec<PathBuf> = worktrees
        .iter()
        .filter(|w| !w.is_main && w.branch.as_deref().is_some_and(|b| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(key))))
        .map(|w| PathBuf::from(&w.path))
        .collect();
    match named.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => bail!("several worktrees of {} have a branch that names {key}; pass the worktree", root.display()),
    }
}

/// The worktree `dir` is in, and the issue its branch names (`claude/nit-3-...` is NIT-3).
pub fn current_worktree(dir: &str) -> Result<(PathBuf, Option<String>)> {
    let (top, branch) = worktree_and_branch(dir)?;
    let (_, key) = crate::git::parse_branch(&branch);
    Ok((top, key))
}

/// The worktree `dir` is in, and the branch checked out there: `HEAD` when detached, empty when
/// git cannot say (a branch with no commits yet).
pub fn worktree_and_branch(dir: &str) -> Result<(PathBuf, String)> {
    let dir = Path::new(dir);
    // Both from one git call; it fails on a branch with no commits, which the two calls below
    // tell apart from a folder outside any worktree.
    if let Ok(out) = git(dir, &["rev-parse", "--show-toplevel", "--abbrev-ref", "HEAD"]) {
        if let [top, branch] = out.lines().map(str::trim).collect::<Vec<_>>()[..] {
            return Ok((PathBuf::from(top), branch.to_string()));
        }
    }
    let top = git(dir, &["rev-parse", "--show-toplevel"]).context("not inside a git worktree")?;
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    Ok((PathBuf::from(top.trim()), branch.trim().to_string()))
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
    /// A light worktree: source only, its setup not run (docs/build-lanes.md).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub light: bool,
    /// The directories a sparse worktree holds; empty when it is whole.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
    /// The read-only reference tree at the project's base commit, for reading code outside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub command: String,
    pub ok: bool,
    pub seconds: u64,
    /// The end of stdout and stderr, or why the step was skipped.
    pub output: String,
    #[serde(default)]
    pub skipped: bool,
    /// It ran past the lane's time limit and was stopped (NIT-310): that says nothing about the
    /// change, so the gate is not passed, but nobody is told to fix the code.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

impl GateResult {
    /// The step that stopped the gate when it ran past the lane's time limit, rather than failed.
    pub fn timed_out(&self) -> Option<&CommandResult> {
        self.results.iter().find(|r| r.timed_out)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_authority: Option<String>,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<crate::verification::Plan>,
    /// An intentional bypass is unverified, never a passing gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub commit: String,
    pub results: Vec<CommandResult>,
    /// Playbook keys this nitely ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Commands that failed and then passed on a later run (`[gate] retries`): the gate passed,
    /// but these tests are flaky.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaked: Vec<Flake>,
    /// The build lane run it was (or the earlier run that answered it), when it ran in a lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_run: Option<i64>,
}

/// Creates the issue's branch and worktree from the integration branch and runs the setup
/// commands. Calling it again for the same issue returns the existing worktree.
pub fn start_task(db: &Db, repo_root: &Path, issue: &IssueRecord, vendor: &str, actor: &Actor) -> Result<Started> {
    let _telemetry = crate::performance_attribution::Scope::new(db,crate::performance_attribution::Context {project:issue.project.clone(),issue:Some(issue.key.clone()),..Default::default()});
    if issue.is_humans() && *actor != Actor::Human {
        bail!("{} is the human's own work (owner: human), so agents do not start it; `ask` the human if an agent should take it", issue.key);
    }
    let playbook = Playbook::load(repo_root)?;
    let branch = playbook.branch_for(vendor, issue);
    let worktree = playbook.worktree_for(repo_root, vendor, issue);
    let worktree_str = worktree.to_string_lossy().into_owned();

    if worktree.join(".git").exists() {
        // A start that failed at the submodules (one not checked out yet) finishes them now.
        for line in crate::submodules::start(repo_root, &playbook, &worktree, &branch, &format!("{}\n{}", issue.title, issue.body))? {
            db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started the submodule branch {line}"))?;
        }
        let light = crate::light::is_light(&worktree);
        let reference = Some(crate::light::reference_path(&playbook, repo_root)).filter(|r| light && r.join(".git").exists());
        return Ok(Started {
            issue: issue.key.clone(),
            branch,
            worktree: worktree_str,
            created: false,
            setup: vec![],
            warnings: playbook.unknown_warnings(),
            light,
            sparse: crate::light::sparse_set(&worktree).unwrap_or_default(),
            reference: reference.map(|r| r.to_string_lossy().into_owned()),
        });
    }

    let base = playbook.integration_branch();
    let branch_exists = git(repo_root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    // A new branch starts from the base commit when the playbook moves it on a schedule
    // (`[lanes] base = "every 4h"`), so agents share one starting point between moves; otherwise
    // from the integration branch's tip, as always.
    // An item of a goal with a goal branch starts from that branch, and the goal's checkpoint from
    // the branch's head it lands (goal_branch.rs).
    let goal_start = if branch_exists { None } else { crate::goal_branch::start_point(db, repo_root, &playbook, issue, actor)? };
    let from = match (&goal_start, playbook.lanes.as_ref().and_then(|l| l.base_every_hours())) {
        (Some((at, _)), _) => at.clone(),
        (None, Some(_)) if !branch_exists => crate::light::base_commit(&playbook, repo_root)?,
        _ => base.to_string(),
    };
    let from_words = match &goal_start {
        Some((_, words)) => words.clone(),
        None if from == base => base.to_string(),
        None => format!("{base} at {}", &from[..from.len().min(9)]),
    };
    if crate::light::wanted(&playbook) {
        return start_light(db, repo_root, &playbook, issue, branch, worktree, &from_words, &from, branch_exists, actor);
    }
    if branch_exists {
        git(repo_root, &["worktree", "add", &worktree_str, &branch])?;
    } else {
        git(repo_root, &["worktree", "add", "-b", &branch, &worktree_str, &from])?;
    }
    db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started {branch} from {from_words}"))?;
    for line in crate::submodules::start(repo_root, &playbook, &worktree, &branch, &format!("{}\n{}", issue.title, issue.body))? {
        db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started the submodule branch {line}"))?;
    }

    let mut setup = Vec::new();
    for command in &playbook.worktree.setup {
        let command = &expand(command, repo_root, &worktree, base);
        let (ok, output, seconds) = run_shell_kind(command, &worktree, "setup")?;
        setup.push(CommandResult { command: command.clone(), ok, seconds, output, skipped: false, timed_out: false });
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
    Ok(Started {
        issue: issue.key.clone(),
        branch,
        worktree: worktree_str,
        created: true,
        setup,
        warnings: playbook.unknown_warnings(),
        light: false,
        sparse: vec![],
        reference: None,
    })
}

/// `start_task` for a playbook that asks for light worktrees: source only, no setup, sparse when the
/// playbook names directories, and the reference tree made or moved to the base.
#[allow(clippy::too_many_arguments)]
fn start_light(db: &Db, repo_root: &Path, playbook: &Playbook, issue: &IssueRecord, branch: String, worktree: PathBuf, from_words: &str, start: &str, exists: bool, actor: &Actor) -> Result<Started> {
    let worktree_str = worktree.to_string_lossy().into_owned();
    let from = if exists { format!("refs/heads/{branch}") } else { start.to_string() };
    let mut sparse = crate::light::sparse_dirs(playbook, repo_root, &from, &format!("{}\n{}", issue.title, issue.body));
    // A sparse worktree still has the submodules that land with it.
    if !sparse.is_empty() {
        for s in &playbook.submodules {
            if !sparse.iter().any(|d| d == s.path()) {
                sparse.push(s.path().to_string());
            }
        }
    }
    crate::light::add(repo_root, &worktree, &branch, start, exists, &sparse)?;
    for line in crate::submodules::start(repo_root, playbook, &worktree, &branch, &format!("{}\n{}", issue.title, issue.body))? {
        db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started the submodule branch {line}"))?;
    }
    let shape = if sparse.is_empty() { "light".to_string() } else { format!("light, sparse: {}", sparse.join(", ")) };
    db.log_event(actor, Some(&issue.key), Some(&worktree_str), "worktree", &format!("Started {branch} from {from_words} ({shape})"))?;
    let mut warnings = playbook.unknown_warnings();
    // Reading code outside the worktree needs it, but a worktree without it still works.
    let reference = match crate::light::reference_tree(playbook, repo_root) {
        Ok(r) => Some(r.to_string_lossy().into_owned()),
        Err(e) => {
            warnings.push(format!("the reference tree could not be made: {e:#}"));
            None
        }
    };
    Ok(Started { issue: issue.key.clone(), branch, worktree: worktree_str, created: true, setup: vec![], warnings, light: true, sparse, reference })
}

/// The ref a branch's changes are measured against: `origin/<base>` when the local `<base>` is
/// behind it (or missing), so a stale local branch doesn't count already-merged work as this change.
pub fn diff_base(worktree: &Path, base: &str) -> String {
    let remote = format!("origin/{base}");
    let remote_exists = git(worktree, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{remote}")]).is_ok();
    let local_exists = remote_exists && git(worktree, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{base}")]).is_ok();
    pick_base(base, remote_exists, local_exists, &mut || git(worktree, &["merge-base", "--is-ancestor", base, &remote]).is_ok())
}

/// The base a branch is measured against (NIT-537): `origin/<base>` when it exists and local
/// `<base>` is missing or behind it (`local_behind`, asked only then), else local `<base>`.
pub(crate) fn pick_base(base: &str, remote_exists: bool, local_exists: bool, local_behind: &mut dyn FnMut() -> bool) -> String {
    if remote_exists && (!local_exists || local_behind()) {
        format!("origin/{base}")
    } else {
        base.to_string()
    }
}

/// Runs the gate commands in `worktree` and records the outcome on the issue's timeline.
/// Stops at the first failure, so the agent sees the error that matters. A failing command runs
/// again first (`[gate] retries`); one that then passes is recorded as a flake.
pub fn run_gate(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor) -> Result<GateResult> {
    run_gate_as(db, repo_root, worktree, issue_key, actor, crate::lanes::Kind::Gate)
}

/// `run_gate`, saying what the gate is for: in a repo with build lanes (`[lanes]`), the gate runs
/// there as a request of that kind, land's first, and never builds in the worktree.
pub fn run_gate_as(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor, kind: crate::lanes::Kind) -> Result<GateResult> {
    run_gate_inner(db, repo_root, worktree, issue_key, actor, kind, &[], None)
}

/// The gate with `plan`'s commands in place of the playbook's, whatever the project's mode says:
/// a goal item's checks (goal_branch.rs), checked against the plan's affected-test evidence.
pub(crate) fn run_planned_gate(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor, kind: crate::lanes::Kind, plan: &crate::verification::Plan) -> Result<GateResult> {
    run_gate_inner(db, repo_root, worktree, issue_key, actor, kind, &[], Some(plan))
}

/// Marks the gate's full suites (`ci_commands`) skipped for a submit (`Kind::Gate`) that runs the
/// whole gate (no reduced plan) in a project whose lands batch: the land gates them once, in CI
/// when CI can take them, so they never run twice, nor on this machine at submit.
pub(crate) fn leave_full_suites_to_land(steps: &mut [(String, Option<String>)], ci_commands: &[String], kind: crate::lanes::Kind, planned: bool, batched: bool) {
    if kind != crate::lanes::Kind::Gate || planned || !batched {
        return;
    }
    for (_, skip) in steps.iter_mut().filter(|(c, skip)| skip.is_none() && ci_commands.contains(c)) {
        *skip = Some("the full suite runs once, when this lands (in CI when CI can take it)".into());
    }
}

pub(crate) fn run_automatic_gate(db: &Db, repo_root: &Path, worktree: &Path, keys: &[&str], actor: &Actor, kind: crate::lanes::Kind) -> Result<GateResult> {
    let authority=keys.first().map(|k|crate::project_modes::authority_for_issue(db,k)).transpose()?.flatten();
    for key in keys {if crate::project_modes::authority_for_issue(db,key)?!=authority {bail!("batch members have incompatible project review modes; land individually");}}
    let plan=if !crate::project_yolo::all_enabled(db,keys)? && keys.len()==1 {crate::verification::execution_plan(db,repo_root,worktree,keys[0])?}else{None};
    let mut gate=run_gate_inner(db,repo_root,worktree,keys.last().copied(),actor,kind,keys,plan.as_ref())?;
    gate.mode_authority=authority;
    gate.plan=plan;
    crate::project_modes::check_gate_authority(db,keys,&gate)?;
    if keys.len()==1 {crate::verification::check_gate_authority(db,repo_root,worktree,keys[0],&gate)?;}
    Ok(gate)
}

#[allow(clippy::too_many_arguments)]
fn run_gate_inner(db: &Db, repo_root: &Path, worktree: &Path, issue_key: Option<&str>, actor: &Actor, kind: crate::lanes::Kind, yolo_keys: &[&str], plan:Option<&crate::verification::Plan>) -> Result<GateResult> {
    if crate::project_yolo::all_enabled(db, yolo_keys)? { return crate::project_yolo::skipped_gate(db, worktree, yolo_keys, actor); }
    let mut playbook = Playbook::load(repo_root)?;
    if let Some(key)=yolo_keys.first() {crate::project_modes::apply_issue(db,key,&mut playbook)?;}
    if let Some(plan)=plan {playbook.gate.commands=plan.commands.clone();playbook.gate.steps.clear();}
    if playbook.gate.commands.is_empty() && playbook.gate.steps.is_empty() {
        bail!("this repo's playbook has no gate commands; add them under [gate] in {PLAYBOOK_PATH}");
    }
    let base = playbook.integration_branch();
    let commit = git(worktree, &["rev-parse", "--short", "HEAD"]).unwrap_or_default().trim().to_string();
    // What changed, for the steps that skip by it; four git calls, so only when there are some.
    // --no-renames: a moved file counts as a change at both paths.
    let files: Vec<String> = if playbook.gate.steps.is_empty() {
        vec![]
    } else {
        let since = diff_base(worktree, base);
        git(worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")]).unwrap_or_default().lines().map(str::to_string).collect()
    };
    let mut steps: Vec<(String, Option<String>)> = playbook.gate.commands.iter().map(|c| (c.clone(), None)).collect();
    for step in &playbook.gate.steps {
        steps.push((step.run.clone(), step.skip_reason(&files)?));
    }
    // A submit's full gate under a mode that lands in batches: the full suites run once, at the
    // land, which always gates in full (NIT-514). Only the issue's own submit, never `run_gate`.
    let batched = !yolo_keys.is_empty() && playbook.land.batch != LandBatch::Off;
    let ci = playbook.lanes.as_ref().map(|l| l.ci_commands.as_slice()).unwrap_or_default();
    leave_full_suites_to_land(&mut steps, ci, kind, plan.is_some(), batched);

    if playbook.lanes.is_some() {
        // In a lane: a full checkout of the worktree's snapshot, set up there. The worktree itself
        // is not built, made whole or set up.
        // A reduced gate already validated on this exact tree is not run again (NIT-494); a lane's
        // own record cannot answer it, since its selected tests must be seen to run.
        let tree = if plan.is_some() {Some(git(worktree,&["rev-parse","HEAD^{tree}"])?.trim().to_string())} else {None};
        if let (Some(plan),Some(tree)) = (plan,&tree) {
            if let Some(run)=crate::verification::validated_pass(db,repo_root,tree,&plan.commands)? {
                let results=plan.commands.iter().map(|c|CommandResult{command:c.clone(),ok:true,seconds:0,output:format!("passed on this same tree in lane run {run}, every selected test seen to run; not run again"),skipped:true,timed_out:false}).collect();
                let mut gate=record_gate(db,issue_key,worktree,actor,&commit,results,true,vec![],playbook.unknown_warnings())?;
                gate.lane_run=Some(run);
                return Ok(gate);
            }
        }
        let lane = if plan.is_some() {crate::lanes::gate_fresh_checked(db,repo_root,&playbook,worktree,issue_key,kind,&steps,actor,yolo_keys)?}
            else {crate::lanes::gate(db, repo_root, &playbook, worktree, issue_key, kind, &steps, actor, yolo_keys)?};
        if lane.yolo_skipped { return crate::project_yolo::skipped_gate(db, worktree, yolo_keys, actor); }
        let mut warnings = playbook.unknown_warnings();
        warnings.extend(lane.warnings);
        let mut passed=lane.passed;
        validate_gate_plan(plan,&lane.results,&mut passed,&mut warnings);
        if let (true,Some(plan),Some(tree),Some(run))=(passed,plan,&tree,lane.run) {crate::verification::remember_pass(db,repo_root,tree,&plan.commands,run)?;}
        let mut gate = record_gate(db, issue_key, worktree, actor, &lane.commit, lane.results, passed, lane.flaked, warnings)?;
        gate.lane_run = lane.run;
        return Ok(gate);
    }

    // Outside lanes too, the gate takes one of the machine's build slots and waits for its memory,
    // so a project without lanes cannot take the whole machine from the rest (NIT-266).
    let project = issue_key.and_then(|k| db.issue(k).ok().flatten()).and_then(|i| i.project).unwrap_or_default();
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context {
        project: (!project.is_empty()).then(|| project.clone()), issue: issue_key.map(str::to_string), ..Default::default()
    });
    let key: Vec<&str> = steps.iter().filter(|(_, skip)| skip.is_none()).map(|(c, _)| c.as_str()).collect();
    let turn = match crate::lanes::hold_machine_checked(db, repo_root, worktree, &project, issue_key, kind, &key.join("\n"), actor, yolo_keys) {
        Ok(turn) => turn,
        Err(e) if e.is::<crate::project_yolo::SkipGate>() => return crate::project_yolo::skipped_gate(db, worktree, yolo_keys, actor),
        Err(e) => return Err(e),
    };
    if crate::project_yolo::all_enabled(db, yolo_keys)? { return crate::project_yolo::skipped_gate(db, worktree, yolo_keys, actor); }
    crate::project_modes::check_playbook_authority(db,yolo_keys,&playbook)?;
    // A sparse worktree lacks files the gate builds (a workspace's other members): without build
    // lanes, it becomes whole first, so the gate sees what land will.
    let whole = crate::light::make_whole(worktree)?;
    // A stale setup fails the gate for reasons that are not the change's; set up again first.
    let retries = playbook.gate.retries;
    let mut flaked = Vec::new();
    let (mut results, mut passed) = refresh_setup(&playbook.worktree, worktree, repo_root, base, retries, &mut flaked)?;
    results.splice(0..0, whole);
    if !passed {
        steps.clear();
    }
    for (command, skip) in steps {
        crate::project_modes::check_playbook_authority(db,yolo_keys,&playbook)?;
        if crate::project_yolo::all_enabled(db, yolo_keys)? { return crate::project_yolo::skipped_gate(db, worktree, yolo_keys, actor); }
        let command = expand(&command, repo_root, worktree, base);
        if let Some(why) = skip {
            results.push(CommandResult { command, ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true, timed_out: false });
            continue;
        }
        let (ok, output, seconds, flake) = run_retrying(&command, worktree, retries)?;
        flaked.extend(flake);
        results.push(CommandResult { command, ok, seconds, output, skipped: false, timed_out: false });
        if !ok {
            passed = false;
            break;
        }
    }
    let mut warnings=playbook.unknown_warnings();
    validate_gate_plan(plan,&results,&mut passed,&mut warnings);
    turn.finish(passed);
    record_gate(db, issue_key, worktree, actor, &commit, results, passed, flaked, warnings)
}

fn validate_gate_plan(plan:Option<&crate::verification::Plan>,results:&[CommandResult],passed:&mut bool,warnings:&mut Vec<String>) {
    if *passed {if let Some(plan)=plan {if let Err(e)=crate::verification::validate_selected(plan,results) {*passed=false;warnings.push(format!("Selected test evidence incomplete: {e:#}"));}}}
}

/// Puts a gate's outcome on the issue's timeline (its flakes, then passed or failed) and returns it.
#[allow(clippy::too_many_arguments)]
fn record_gate(db: &Db, issue_key: Option<&str>, worktree: &Path, actor: &Actor, commit: &str, results: Vec<CommandResult>, passed: bool, flaked: Vec<Flake>, warnings: Vec<String>) -> Result<GateResult> {
    let at = Some(worktree.to_string_lossy().into_owned());
    for f in &flaked {
        let tests = if f.tests.is_empty() { String::new() } else { format!(": {}", f.tests.join(", ")) };
        db.log_event(actor, issue_key, at.as_deref(), "gate_flaked", &format!("{} failed at {commit}, then passed when run again{tests}", f.command))?;
    }
    let summary = if passed {
        format!("Gate passed at {commit}")
    } else {
        format!("Gate failed at {commit}: {}", results.last().map(|r| r.command.as_str()).unwrap_or(""))
    };
    db.log_event(actor, issue_key, at.as_deref(), if passed { "gate_passed" } else { "gate_failed" }, &summary)?;
    Ok(GateResult { mode_authority: None, passed, plan: None, skipped: None, commit: commit.to_string(), results, warnings, flaked, lane_run: None })
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

    /// NIT-287: a lane serving branches that differ only in Cargo.lock ran `npm ci` again for each,
    /// because the setup fingerprint read every lockfile. Setup reads only its tools' files now.
    #[test]
    fn setup_goes_stale_only_on_the_files_its_tools_read() {
        let s = |c: &[&str]| setup_files_for(&c.iter().map(|c| c.to_string()).collect::<Vec<_>>());
        assert_eq!(s(&["npm ci"]), JS_FILES);
        assert!(!s(&["npm ci"]).contains(&"Cargo.lock"));
        assert_eq!(s(&["cd app && CI=1 pnpm install", "cargo fetch"]).len(), JS_FILES.len() + 1);
        assert_eq!(s(&["C:\\Tools\\uv.exe sync"]), PYTHON_FILES);
        assert_eq!(s(&["./setup.sh"]), SETUP_FILES, "a tool it does not know may read any");
        assert_eq!(s(&["echo {base} > setup.txt"]), SETUP_FILES, "echo writes what it is given: any file may matter");
        assert_eq!(s(&["cd app"]), SETUP_FILES, "no tool at all reads as unknown");
        let rules = WorktreeRules { setup: vec!["npm ci".into()], setup_when_changed: vec!["./tools/versions.txt".into()], ..Default::default() };
        let changed = ["Cargo.lock", "package-lock.json", "tools/versions.txt"].map(String::from);
        assert_eq!(setup_files_in(&rules, &changed), ["package-lock.json", "tools/versions.txt"]);
    }

    #[test]
    fn git_for_windows_launchers_resolve_to_the_real_git_beside_them() {
        let installed = |p: &Path| p == Path::new("C:/Git/mingw64/bin/git.exe");
        let real = Some(PathBuf::from("C:/Git/mingw64/bin/git.exe"));
        assert_eq!(real_git_beside(Path::new("C:/Git/cmd/git.exe"), installed), real);
        assert_eq!(real_git_beside(Path::new("C:/Git/bin/git.exe"), installed), real);
        assert_eq!(real_git_beside(Path::new("C:/Git/mingw64/bin/git.exe"), |_| false), real);
        // A launcher with no real git beside it, or some other git: left to --exec-path.
        assert_eq!(real_git_beside(Path::new("D:/Git/cmd/git.exe"), installed), None);
        assert_eq!(real_git_beside(Path::new("C:/scoop/shims/git.exe"), installed), None);
    }

    /// A multi-line command whose first line fails must never pass (NIT-392).
    #[test]
    fn a_multi_line_command_stops_at_its_first_failure_or_is_refused() {
        assert_eq!(sh_script("cd app\nnpm test\n"), "set -e\ncd app\nnpm test");
        assert_eq!(sh_script("npm test\n"), "npm test\n");
        assert!(unsupported_command("rem check\r\ncargo test\nnpm test", true).is_some());
        assert!(unsupported_command("\nexit /b 7", true).is_some(), "a leading break runs nothing in cmd");
        assert_eq!(unsupported_command("cargo test\r\n", true), None);
        assert_eq!(unsupported_command("cargo test\nnpm test", false), None);
    }

    #[test]
    fn a_batched_submit_leaves_its_full_suites_to_the_land_and_nothing_else_does() {
        use crate::lanes::Kind;
        let ci = vec!["cargo test --workspace".to_string()];
        let gate = || vec![("npm test".to_string(), None), ("cargo test --workspace".to_string(), None)];
        let mut submit = gate();
        leave_full_suites_to_land(&mut submit, &ci, Kind::Gate, false, true);
        assert_eq!(submit[0].1, None, "the quick checks still run at submit");
        assert!(submit[1].1.as_deref().is_some_and(|w| w.contains("when this lands")));
        // The land itself, a reduced plan, a project that lands one at a time: all run it.
        for (kind, planned, batched) in [(Kind::Land, false, true), (Kind::Gate, true, true), (Kind::Gate, false, false)] {
            let mut steps = gate();
            leave_full_suites_to_land(&mut steps, &ci, kind, planned, batched);
            assert_eq!(steps, gate(), "{kind:?} planned={planned} batched={batched}");
        }
    }

    #[test]
    fn git_keeps_answers_about_object_ids_but_asks_again_about_a_moved_branch() {
        let repo = crate::paths::unique_temp("nitely-settled", "");
        crate::test_repo::init(&repo, &[("a.txt", "one\n")]);
        let first = git(&repo, &["rev-parse", "main^{commit}"]).unwrap();
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        crate::test_repo::git(&repo, &["commit", "-q", "-am", "two"]);
        let second = git(&repo, &["rev-parse", "main^{commit}"]).unwrap();
        assert_ne!(first, second, "a branch is read again each time");
        // The id's answer is kept: asked again, it is the same without a new git.
        let tree = |id: &str| git(&repo, &["rev-parse", &format!("{}^{{tree}}", id.trim())]).unwrap();
        assert_eq!(tree(&first), tree(&first));
        assert!(settled_query(&["rev-parse", &format!("{}^{{tree}}", first.trim())]));
        assert!(!settled_query(&["rev-parse", "main^{commit}"]) && !settled_query(&["rev-parse", "HEAD", "HEAD^{tree}"]));
        assert!(!settled_query(&["rev-parse", &format!("{}^{{tree}}", &first.trim()[..7])]), "an abbreviated id can grow ambiguous");

        // Where a worktree's repository is: kept while its .git names the same git folder, read
        // again once the worktree at that path is another.
        let wt = repo.with_extension("wt");
        crate::test_repo::git(&repo, &["worktree", "add", "-q", "-b", "one", &wt.to_string_lossy()]);
        let git_dir = |dir: &Path| git(dir, &["rev-parse", "--absolute-git-dir"]).unwrap();
        let first_dir = git_dir(&wt);
        assert_eq!(git_dir(&wt), first_dir);
        crate::test_repo::git(&repo, &["worktree", "remove", &wt.to_string_lossy()]);
        // Another worktree of the same name takes its git folder's name first.
        let other = repo.with_extension("x").join(wt.file_name().unwrap());
        crate::test_repo::git(&repo, &["worktree", "add", "-q", "-b", "two", &other.to_string_lossy()]);
        crate::test_repo::git(&repo, &["worktree", "add", "-q", "-b", "three", &wt.to_string_lossy()]);
        assert_ne!(git_dir(&wt), first_dir, "the new worktree's own git folder, not the old one's");
        assert!(where_query(&wt.join("sub"), &["rev-parse", "--show-toplevel"]).is_none(), "below the top: asked each time");
        std::fs::remove_dir_all(&wt).ok();
        std::fs::remove_dir_all(repo.with_extension("x")).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

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
            outside_lock: false,
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
        let base = crate::paths::unique_temp("nitely-pb", "");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(args)).unwrap().status.success());
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
    fn a_command_that_fails_then_passes_is_a_flake_and_one_that_keeps_failing_fails() {
        let base = crate::paths::unique_temp("nitely-flake", "");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(args)).unwrap().status.success());
        sh(&["init", "-q", "-b", "main"]);
        sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        // Fails the first time it runs in a folder, naming a test, and passes after.
        let flaky = if cfg!(windows) {
            "if exist ran.txt (exit 0) else (echo test crew::tests::slow ... FAILED& type nul > ran.txt& exit 1)"
        } else {
            "if [ -f ran.txt ]; then exit 0; else echo 'test crew::tests::slow ... FAILED'; touch ran.txt; exit 1; fi"
        };
        let playbook = |extra: &str| std::fs::write(repo.join(PLAYBOOK_PATH), format!("[gate]\ncommands = [{flaky:?}]\n{extra}")).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        let actor = Actor::Agent("claude:test".into());

        playbook("");
        let gate = run_gate(&db, &repo, &repo, Some("NIT-3"), &actor).unwrap();
        assert!(gate.passed, "{gate:?}");
        assert_eq!(gate.flaked.len(), 1);
        assert_eq!(gate.flaked[0].tests, ["crew::tests::slow"]);
        let timeline = db.timeline("NIT-3").unwrap();
        let kinds: Vec<&str> = timeline.iter().map(|t| t.kind.as_str()).collect();
        assert_eq!(kinds, ["gate_flaked", "gate_passed"]);
        assert!(timeline[0].summary.as_deref().unwrap_or("").ends_with("then passed when run again: crew::tests::slow"), "{:?}", timeline[0].summary);

        // With retries off the same first failure fails the gate.
        std::fs::remove_file(repo.join("ran.txt")).unwrap();
        playbook("retries = 0\n");
        let gate = run_gate(&db, &repo, &repo, Some("NIT-4"), &actor).unwrap();
        assert!(!gate.passed && gate.flaked.is_empty(), "{gate:?}");

        // A command that fails every time fails the gate, and is no flake.
        std::fs::write(repo.join(PLAYBOOK_PATH), "[gate]\ncommands = [\"exit 1\"]\n").unwrap();
        let gate = run_gate(&db, &repo, &repo, Some("NIT-5"), &actor).unwrap();
        assert!(!gate.passed && gate.flaked.is_empty(), "{gate:?}");
        let kinds: Vec<String> = db.timeline("NIT-5").unwrap().into_iter().map(|t| t.kind).collect();
        assert_eq!(kinds, ["gate_failed"]);
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn names_the_failed_tests_of_cargo_and_node() {
        let cargo = "test a::ok ... ok\ntest a::bad ... FAILED\n\nfailures:\n    a::bad\n\ntest result: FAILED.";
        assert_eq!(failed_tests(cargo), ["a::bad"]);
        assert_eq!(failed_tests("test a::ok ... ok <0.010s>\ntest a::timed ... FAILED <3.250s>\n"), ["a::timed"]);
        // A long run's kept tail may start after the `... FAILED` lines; its closing list names them.
        let tail = "…oo\n\nfailures:\n\n---- b::slow stdout ----\nthread panicked at x.rs:3\n    left: 1\n\nfailures:\n    b::slow\n    c::late\n\ntest result: FAILED. 1 passed; 2 failed";
        assert_eq!(failed_tests(tail), ["b::slow", "c::late"]);
        let node = "ok 1 - fine\nnot ok 2 - since: counts from the move\n  ---\nnot ok 2 - since: counts from the move\n";
        assert_eq!(failed_tests(node), ["since: counts from the move"]);
        assert!(failed_tests("error[E0425]: cannot find value").is_empty());
    }

    #[test]
    fn finds_the_repo_of_the_issues_project_not_the_callers() {
        let base = crate::paths::unique_temp("nitely-repos", "");
        let init = |name: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
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
        let out = crate::playbook::run_git(Command::new("git").arg("-C").arg(&app).args(["worktree", "add", "-q", "-b", "claude/nit-3-x"]).arg(&worktree).arg("main")).unwrap();
        assert!(out.status.success(), "{out:?}");
        same(issue_worktree(Path::new(&app), "NIT-3").unwrap().unwrap(), &worktree.to_string_lossy());
        assert!(issue_worktree(Path::new(&app), "NIT-4").unwrap().is_none());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn project_review_declarations_use_target_policy_and_fail_closed_for_remote_ambiguity() {
        let base = crate::paths::unique_temp("nitely-review-project", "");
        let app = base.join("app");
        let web = base.join("web");
        for root in [&app, &web] {
            std::fs::create_dir_all(root.join(".nitely")).unwrap();
        }
        std::fs::write(app.join(PLAYBOOK_PATH), "[approval]\nhuman_paths = []\n").unwrap();
        std::fs::write(web.join(PLAYBOOK_PATH), "[[approval.human_paths]]\npattern = '^src/billing/'\nreason = 'Money'\n").unwrap();
        let repo = |name: &str, root: &Path, project: &str| crate::RepoConfig {
            name: name.into(), root: root.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some(project.into()),
        };
        let mut config = crate::Config { repos: vec![repo("app", &app, "APP"), repo("web", &web, "WEB")], ..Default::default() };
        let paths = vec!["src/billing/pay.ts".into()];
        // Local cross-project calls and remote sessions both use WEB's policy.
        for dir in [Some(app.to_str().unwrap()), None] {
            let (level, note) = project_review_declaration(&config, Some("WEB"), dir, Some("inert"), &paths).unwrap();
            assert_eq!(level.as_deref(), Some("sensitive"));
            assert!(note.unwrap().contains("Money"));
        }
        assert!(project_review_declaration(&config, Some("MISSING"), Some(app.to_str().unwrap()), Some("inert"), &paths).is_err());
        std::fs::write(web.join(PLAYBOOK_PATH), "[approval\n").unwrap();
        assert!(project_review_declaration(&config, Some("WEB"), None, Some("inert"), &paths).is_err());
        std::fs::remove_file(web.join(PLAYBOOK_PATH)).unwrap();
        assert!(project_review_declaration(&config, Some("WEB"), None, Some("inert"), &paths).is_err());
        config.repos.push(repo("web-other", &app, "WEB"));
        assert!(project_review_declaration(&config, Some("WEB"), None, Some("inert"), &paths).unwrap_err().to_string().contains("ambiguous"));
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_playbooks_decisions_over_plain_data() {
        // A command that failed then passed is a flake; one that kept failing fails (NIT-537).
        let flaky = classify_runs("cargo test", vec![(false, "test a::b ... FAILED\n".into(), 3), (true, "ok".into(), 2)]);
        assert_eq!((flaky.0, flaky.1.as_str(), flaky.2), (true, "ok", 5));
        let flake = flaky.3.unwrap();
        assert_eq!((flake.tests, flake.output.as_str()), (vec!["a::b".to_string()], "test a::b ... FAILED\n"));
        let failed = classify_runs("cargo test", vec![(false, "one".into(), 1), (false, "two".into(), 1)]);
        assert_eq!((failed.0, failed.1.as_str(), failed.2, failed.3.is_none()), (false, "two", 2, true));
        assert!(classify_runs("x", vec![(true, "ok".into(), 1)]).3.is_none(), "a first pass is no flake");

        // The base: the remote's when local is missing or behind, else local.
        let never = &mut || -> bool { panic!("asked only when both exist") };
        assert_eq!(pick_base("main", false, false, never), "main", "no remote");
        assert_eq!(pick_base("main", true, false, never), "origin/main", "no local branch");
        assert_eq!(pick_base("main", true, true, &mut || true), "origin/main", "local main is behind");
        assert_eq!(pick_base("main", true, true, &mut || false), "main", "local main has diverged");

        // The repo of the issue's project, not the caller's.
        let repo = |name: &str, project: Option<&str>| crate::config::RepoConfig { name: name.into(), root: format!("C:/Code/{name}"), project: project.map(str::to_string), integration_branch: "main".into() };
        let (app, web, other) = (repo("app", Some("APP")), repo("web", Some("APP")), repo("other", Some("OTH")));
        assert_eq!(choose_repo(std::slice::from_ref(&app), Some("app"), Some("C:/Code/other"), Some(&other), "C:/Code/other").unwrap(), PathBuf::from("C:/Code/app"));
        let both = [app.clone(), web.clone(), other.clone()];
        assert_eq!(choose_repo(&both, Some("APP"), Some("C:/Code/web"), Some(&web), "C:/Code/web").unwrap(), PathBuf::from("C:/Code/web"), "of several, the caller's");
        assert!(choose_repo(&both, Some("APP"), Some("C:/Code/other"), Some(&other), "x").unwrap_err().to_string().contains("several repos are linked to APP (app, web)"));
        assert!(choose_repo(std::slice::from_ref(&other), Some("NEW"), Some("C:/Code/other"), Some(&other), "C:/Code/other").unwrap_err().to_string().contains("holds OTH's work"));
        assert_eq!(choose_repo(&[], Some("NEW"), Some("C:/Code/loose"), None, "C:/Code/loose").unwrap(), PathBuf::from("C:/Code/loose"), "nothing linked: the caller's checkout");
        assert_eq!(choose_repo(&[], None, Some("C:/Code/loose"), None, "x").unwrap(), PathBuf::from("C:/Code/loose"));
        assert!(choose_repo(&[], None, None, None, "C:/tmp").unwrap_err().to_string().contains("not inside a git repo"));

        // The worktree whose branch names the issue.
        let wt = |path: &str, branch: Option<&str>, is_main| crate::git::Worktree { path: path.into(), head: "a".into(), branch: branch.map(str::to_string), is_main, locked: false, prunable: false };
        let root = Path::new("C:/Code/app");
        let list = [wt("C:/Code/app", Some("claude/app-3-main-named"), true), wt("C:/w/a", Some("claude/app-3-fix"), false), wt("C:/w/b", None, false)];
        assert_eq!(issue_worktree_in(&list, root, "APP-3").unwrap(), Some(PathBuf::from("C:/w/a")), "never the main checkout");
        assert_eq!(issue_worktree_in(&list, root, "APP-4").unwrap(), None);
        let twice = [wt("C:/w/a", Some("claude/app-3-fix"), false), wt("C:/w/c", Some("codex/app-3-again"), false)];
        assert!(issue_worktree_in(&twice, root, "APP-3").is_err());

        // What git answers once and for all: questions about full ids only.
        let id = "a".repeat(40);
        assert!(settled_query(&["rev-parse", &format!("{id}^{{tree}}")]) && settled_query(&["merge-base", &id, &id]) && settled_query(&["cat-file", "-s", &format!("{id}:a.txt")]));
        assert!(!settled_query(&["rev-parse", "main^{commit}"]) && !settled_query(&["rev-parse", &format!("{}^{{tree}}", &id[..7])]) && !settled_query(&["merge-base", "main", &id]));
    }

    #[test]
    fn measures_against_the_newer_of_local_and_remote_base() {
        let base = crate::paths::unique_temp("nitely-db", "");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let sh = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap().status.success(), "{args:?}");
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
        ci_agrees_with_the_gate(&pb, include_str!("../../../.github/workflows/ci.yml")).unwrap();
    }

    /// Every gate command or step runs in CI, and every cargo or npm command CI runs is in the gate:
    /// a step the gate skips for some changes (`unless_only`, `when_changed`) still runs in CI on
    /// every pull request, which stays the backstop (NIT-464).
    fn ci_agrees_with_the_gate(pb: &Playbook, ci: &str) -> std::result::Result<(), String> {
        let runs: Vec<String> = ci.lines().filter_map(|l| l.trim().strip_prefix("- run: ")).map(|r| same_tests(r.trim())).collect();
        let gate: Vec<String> = pb.gate.commands.iter().chain(pb.gate.steps.iter().map(|s| &s.run)).map(|c| same_tests(c)).collect();
        for command in &gate {
            if !runs.contains(command) {
                return Err(format!("the CI workflow does not run the gate command {command:?}"));
            }
        }
        for run in runs.iter().filter(|r| r.starts_with("cargo ") || r.starts_with("npm ")) {
            if run != "npm ci" && !gate.contains(run) {
                return Err(format!("the CI workflow runs {run:?}, which the gate does not"));
            }
        }
        Ok(())
    }

    /// A `cargo test` command without a trailing `--test-threads=N` for the test binaries: the
    /// thread count only schedules the run on the machine it runs on (the gate's lanes measured 12,
    /// NIT-458; a CI runner picks its own), and the same tests run either way (NIT-474). Anything
    /// else, another package, filter or flag, stays part of the command.
    fn same_tests(command: &str) -> String {
        let words: Vec<&str> = command.split_whitespace().collect();
        if !command.starts_with("cargo test") {
            return words.join(" ");
        }
        let mut kept: Vec<&str> = words.iter().copied().filter(|w| !(w.starts_with("--test-threads=") && w[15..].chars().all(|c| c.is_ascii_digit()) && w.len() > 15)).collect();
        if kept.last() == Some(&"--") {
            kept.pop();
        }
        kept.join(" ")
    }

    #[test]
    fn a_cargo_test_thread_count_is_the_same_command_and_nothing_else_is() {
        let gate = |test: &str| {
            Playbook::parse(&format!("[gate]\ncommands = [\"npm test\"]\n\n[[gate.steps]]\nrun = {test:?}\nunless_only = ['^src/']\n")).unwrap()
        };
        let ci = "      - run: npm ci\n      - run: npm test\n      - run: cargo test --workspace\n";
        // The lanes' thread count: the same tests as CI's plain run (NIT-458's 12 threads).
        ci_agrees_with_the_gate(&gate("cargo test --workspace -- --test-threads=12"), ci).unwrap();
        ci_agrees_with_the_gate(&gate("cargo test --workspace"), ci).unwrap();
        // Anything else is another command: another package, a filter, ignored tests, other flags.
        for other in [
            "cargo test -p nitely-core -- --test-threads=12",
            "cargo test --workspace lanes -- --test-threads=12",
            "cargo test --workspace -- --ignored",
            "cargo test --workspace -- --test-threads=12 --ignored",
            "cargo test --workspace --release -- --test-threads=12",
            "cargo test --workspace -- --test-threads=abc",
        ] {
            assert!(ci_agrees_with_the_gate(&gate(other), ci).is_err(), "{other} is not CI's cargo test");
        }
    }

    #[test]
    fn frontend_only_changes_skip_the_cargo_steps_and_ci_still_runs_them() {
        // The playbook NIT-464 proposes: Cargo as steps that frontend and prose changes skip.
        let pb = Playbook::parse(
            r#"
[gate]
commands = ["npm test", "npm run build"]

[[gate.steps]]
run = "cargo test --workspace"
unless_only = ['^src/', '^scripts/', '^public/', '^index\.html$', '^docs/.*\.md$']

[[gate.steps]]
run = "cargo clippy --workspace -- -D warnings"
unless_only = ['^src/', '^scripts/', '^public/', '^index\.html$', '^docs/.*\.md$']
"#,
        )
        .unwrap();
        let skip = |files: &[&str]| -> Vec<Option<String>> {
            let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
            pb.gate.steps.iter().map(|s| s.skip_reason(&files).unwrap()).collect()
        };
        // A src/ and docs change skips both, with the reason shown on the gate.
        let frontend = skip(&["src/Rules.tsx", "src/rules/Paths.tsx", "docs/visualization.md"]);
        assert!(frontend.iter().all(|r| r.as_deref().is_some_and(|r| r.starts_with("only paths matching ^src/"))), "{frontend:?}");
        // Anything else runs them: Rust, a manifest, a lockfile, the playbook, CI, a file Rust embeds
        // from docs, or a path nobody listed.
        for other in ["crates/nitely-core/src/lanes.rs", "Cargo.lock", "package.json", ".nitely/playbook.toml", ".github/workflows/ci.yml", "docs/claude-hooks.json", "phone.html", "src-tauri/src/main.rs"] {
            assert!(skip(&["src/App.tsx", other]).iter().all(Option::is_none), "{other} runs Cargo");
        }
        // CI runs every step on every pull request, and the agreement check accepts steps.
        let ci = "      - run: npm ci\n      - run: cargo test --workspace\n      - run: cargo clippy --workspace -- -D warnings\n      - run: npm test\n      - run: npm run build\n";
        ci_agrees_with_the_gate(&pb, ci).unwrap();
        // A CI that dropped a step the gate skips locally is refused: CI is the backstop.
        let dropped = ci.replace("      - run: cargo clippy --workspace -- -D warnings\n", "");
        assert!(ci_agrees_with_the_gate(&pb, &dropped).unwrap_err().contains("cargo clippy"));
    }

    #[test]
    fn a_protected_path_is_named_and_any_other_path_is_not() {
        let rules = ApprovalRules {
            human_paths: vec![PathRule { pattern: "^src/billing/".into(), reason: "money".into() }],
            blocked_paths: vec![PathRule { pattern: "^migrations/".into(), reason: "schema".into() }],
            ..Default::default()
        };
        assert_eq!(rules.protected_hit(&["src/billing/pay.ts".into()]).unwrap().as_deref(), Some("money (src/billing/pay.ts)"));
        assert_eq!(rules.protected_hit(&["docs/readme.md".into()]).unwrap(), None);
        assert!(rules.protected_hit(&["migrations/001.sql".into()]).unwrap().unwrap().contains("schema"));
    }
}
