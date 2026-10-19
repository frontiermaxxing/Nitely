//! The crew: Nitely starting and keeping up to N headless agent workers on a project's backlog,
//! one issue each (docs/crew.md). The supervisor claims the issue and makes its worktree itself,
//! then starts the agent there with the standard brief; everything the agent does after that goes
//! through the same MCP tools a person's agent uses. Starting a process is behind [`Agents`], so
//! tests run the supervisor with a fake agent.

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::playbook::{self, Playbook};

/// Workers a crew may start per project per local day when the playbook does not say.
pub const DEFAULT_MAX_PER_DAY: u32 = 20;

/// A supervisor whose heartbeat is older than this is gone.
pub const HEARTBEAT_STALE_SECS: i64 = 60;

/// Kept per project; older worker logs are deleted.
const LOGS_KEPT: usize = 200;

/// The playbook's `[crew]` section. `workers = 0`, the default, is off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrewRules {
    /// How many workers run at once.
    pub workers: u32,
    /// Agents in preference order; the ones not installed are skipped.
    pub agents: Vec<String>,
    /// Workers started per local day; `DEFAULT_MAX_PER_DAY` when unset.
    pub max_per_day: Option<u32>,
    /// Tokens the project's agents may use per local day before no new worker starts (crew
    /// workers and everyone else, as `usage` reads them); unset, the default, is no budget.
    pub max_tokens_per_day: Option<u64>,
    /// `HH:MM-HH:MM` local time when no new worker starts; it may wrap midnight.
    pub quiet_hours: Option<String>,
    /// A worker running longer is stopped, never while it lands.
    pub timeout_minutes: u64,
    /// A worktree setup running longer is stopped, and its issue goes to a person.
    pub setup_timeout_minutes: u64,
    /// Failed workers on one issue before it goes to a human.
    pub attempts: u32,
    /// Files nearly every change touches (registries, shared types): holding one counts for a
    /// quarter when `next_task` keeps new work out of files in flight (overlap.rs).
    pub shared_files: Vec<String>,
    /// A worker with no output in its log and no report for this long is stopped as hung, and its
    /// issue handed on; 0 is off.
    pub idle_minutes: u64,
    /// No new worker starts while less than this is free where worktrees go (GB); 0 is off. Memory
    /// is the machine's own admission (NIT-266, machine.rs).
    pub min_free_disk_gb: u64,
    /// How many workers may work on one goal at once (a goal's own `workers` wins); 0, the
    /// default, is no cap below `workers`.
    pub goal_workers: u32,
    /// Replaces the default command for an agent. `{worktree}`, `{repo}`, `{git_dir}`,
    /// `{mcp_config}`, `{nitely}` and `{nitely_env}` are filled in; the brief goes on stdin.
    pub commands: BTreeMap<String, Vec<String>>,
}

impl Default for CrewRules {
    fn default() -> Self {
        CrewRules {
            workers: 0,
            agents: vec!["claude".into(), "codex".into()],
            max_per_day: None,
            max_tokens_per_day: None,
            quiet_hours: None,
            timeout_minutes: 180,
            setup_timeout_minutes: 30,
            attempts: 3,
            shared_files: vec![],
            idle_minutes: 30,
            min_free_disk_gb: 10,
            goal_workers: 0,
            commands: BTreeMap::new(),
        }
    }
}

impl CrewRules {
    pub fn max_per_day(&self) -> u32 {
        self.max_per_day.unwrap_or(DEFAULT_MAX_PER_DAY)
    }

    /// The command that starts `agent`: the playbook's, else the default, else None.
    pub fn command_for(&self, agent: &str) -> Option<Vec<String>> {
        if let Some(c) = self.commands.get(agent).filter(|c| !c.is_empty()) {
            return Some(c.clone());
        }
        let args = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect());
        match agent {
            // auto: edits, builds and commits go ahead without a person; prompts none: anything that
            // would still ask is refused instead of waiting for an answer nobody gives.
            "claude" => args(&[
                "claude", "-p", "--mcp-config", "{mcp_config}", "--permission-mode", "auto", "--permission-prompts", "none",
                "--output-format", "stream-json", "--verbose",
            ]),
            // Without default_tools_approval_mode, exec refuses every MCP call under approval_policy
            // never. No sandbox: workspace-write never lets git write under a `.git` folder (not with
            // --add-dir, not with writable_roots), so a worker could not commit in its worktree; this
            // is the trust Claude's auto mode gets. Seen in the live check of NIT-118.
            "codex" => args(&[
                "codex", "exec", "--sandbox", "danger-full-access",
                "-c", "approval_policy=\"never\"",
                "-c", "mcp_servers.nitely.command={nitely}",
                "-c", "mcp_servers.nitely.args=[\"mcp\"]",
                // Codex starts MCP servers with a cleared environment: without this, a crew on
                // its own data folder (NITELY_HOME) would have workers on the real one.
                "-c", "mcp_servers.nitely.env={nitely_env}",
                "-c", "mcp_servers.nitely.default_tools_approval_mode=\"approve\"",
                "-",
            ]),
            _ => None,
        }
    }
}

/// A point in local time, as the daily cap and quiet hours read it.
#[derive(Debug, Clone, Copy)]
pub struct Moment {
    pub ts: i64,
    /// Minutes since local midnight.
    pub minute_of_day: u32,
    /// Unix time of the last local midnight.
    pub day_start: i64,
}

impl Moment {
    pub fn now() -> Moment {
        use chrono::Timelike;
        let t = chrono::Local::now();
        let since_midnight = i64::from(t.num_seconds_from_midnight());
        let ts = t.timestamp();
        Moment { ts, minute_of_day: t.hour() * 60 + t.minute(), day_start: ts - since_midnight }
    }
}

/// When `spec` (`HH:MM-HH:MM`) covers `minute`, the time it ends, e.g. `07:00`.
pub fn quiet_until(spec: &str, minute: u32) -> Result<Option<String>> {
    let parse = |s: &str| -> Option<u32> {
        let (h, m) = s.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let Some((from, to)) = spec.split_once('-').and_then(|(a, b)| Some((parse(a)?, parse(b)?))) else {
        bail!("the playbook's crew.quiet_hours `{spec}` is not HH:MM-HH:MM");
    };
    let inside = if from <= to { minute >= from && minute < to } else { minute >= from || minute < to };
    Ok(inside.then(|| format!("{:02}:{:02}", to / 60, to % 60)))
}

/// What the supervisor hands an agent to start it.
#[derive(Debug, Clone)]
pub struct Launch {
    pub agent: String,
    /// The program, resolved to a path when it was found on PATH.
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Goes on the agent's stdin.
    pub brief: String,
    /// Where its stdout and stderr go.
    pub log: PathBuf,
    pub env: Vec<(String, String)>,
}

/// A running worker process.
pub trait Process {
    fn pid(&self) -> u32;
    /// The exit code once it has exited (-1 when it has none, e.g. killed).
    fn exited(&mut self) -> Result<Option<i32>>;
    /// Stops it and everything it started.
    fn stop(&mut self);
}

/// Starts agent processes. The real one spawns them; tests use a fake.
pub trait Agents {
    /// The program to run for `program` when it is installed.
    fn resolve(&self, program: &str) -> Option<String>;
    fn start(&self, launch: &Launch) -> Result<Box<dyn Process>>;
    /// A worker an earlier supervisor started, when its process `pid` is still running.
    fn adopt(&self, pid: u32) -> Option<Box<dyn Process>>;
    /// Makes an issue's branch and worktree and runs the playbook's setup, in a process of its own,
    /// so a slow or hung setup never holds the supervisor up. It exits 0 when setup passed.
    fn prepare(&self, prep: &Prepare) -> Result<Box<dyn Process>>;
    /// Removes a landed worker's worktree and branch the way land would (`clean_up_worktree`), in a
    /// process of its own, so a playbook teardown that hangs never holds the supervisor up.
    fn clean_up(&self, job: &CleanUp) -> Result<Box<dyn Process>>;
    /// Lands an issue a worker handed in and left (`nitely land <key>`), in a process of its own:
    /// a land runs the gate and may wait in the land queue, which must not hold the supervisor up.
    fn land(&self, job: &LandJob) -> Result<Box<dyn Process>>;
    /// Why no new worker should be begun (its worktree set up) because the machine is short of
    /// memory (NIT-266), or None when one may. Only looks.
    fn memory_wait(&self) -> Option<String> {
        None
    }
    /// The same, right before a worker launches: None lets it go and counts as a start, so the next
    /// request to ask lets it grow first.
    fn memory_turn(&self) -> Option<String> {
        None
    }
    /// Runs a submit job again (`nitely submit-job <id>`) whose process died before it finished, as
    /// submit starts one; returns its process id.
    fn run_submit_job(&self, nitely: &str, job: &crate::submit_job::Job) -> Result<Option<u32>>;
}

/// What `Agents::land` needs: `nitely land <issue>` in the repo, output to `log`.
#[derive(Debug, Clone)]
pub struct LandJob {
    pub nitely: String,
    pub repo: PathBuf,
    pub issue: String,
    pub log: PathBuf,
}

/// What `Agents::clean_up` needs: `nitely crew clean-up` with these, output to `log`.
#[derive(Debug, Clone)]
pub struct CleanUp {
    pub nitely: String,
    pub repo: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub log: PathBuf,
    /// The issue and the crew's actor the outcome is recorded under, and the worker's agent.
    pub issue: String,
    pub actor: String,
    pub agent: String,
    pub timeout_minutes: u64,
}

/// Land's cleanup of a worktree it kept for a crew worker: the playbook's teardown, `git worktree
/// remove` without force (a dirty worktree stays), `git branch -d` (an unmerged branch stays).
/// Only a worktree git lists for `repo`, on `branch`, is touched: anything else is refused, since
/// land's last resort deletes what git would not. Returns what it did, in words.
pub fn clean_up_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<String> {
    let listed = playbook::git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = crate::paths::normalize(&worktree.to_string_lossy());
    let mut on: Option<String> = None;
    let mut path_matches = false;
    for line in listed.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            path_matches = crate::paths::normalize(p) == wanted;
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if path_matches {
                on = Some(b.trim().to_string());
            }
        } else if line.is_empty() {
            path_matches = false;
        }
    }
    if on.as_deref() != Some(branch) {
        bail!("{} is not a worktree of {} on {branch}, so it is left alone", worktree.display(), repo.display());
    }
    let pb = Playbook::load(repo)?;
    Ok(crate::land::cleanup(repo, &pb, worktree, branch, false, false).0)
}

/// Runs `clean_up_worktree` for the crew, stopping waiting after `timeout_minutes` (a teardown that
/// hangs), and records how it went on the issue itself, so the outcome is kept whether or not a
/// supervisor is still there to see it. Returns the words recorded, and whether it timed out (the
/// teardown may still run: `nitely crew clean-up` then ends everything it started).
pub fn clean_up_and_report(db: &Db, job: &CleanUp) -> Result<(String, bool)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (repo, worktree, branch) = (job.repo.clone(), job.worktree.clone(), job.branch.clone());
    std::thread::spawn(move || tx.send(clean_up_worktree(&repo, &worktree, &branch).map_err(|e| format!("{e:#}"))).ok());
    let (said, timed_out) = match rx.recv_timeout(std::time::Duration::from_secs(job.timeout_minutes.max(1) * 60)) {
        Ok(Ok(said)) => (said, false),
        Ok(Err(why)) => (why, false),
        Err(_) => (format!("its removal ran past {} minutes (crew.setup_timeout_minutes) and was stopped; {} is left as it is", job.timeout_minutes.max(1), job.worktree.display()), true),
    };
    db.log_event(&Actor::Agent(job.actor.clone()), Some(&job.issue), None, "crew_cleaned", &format!("After the {} worker left: {said}", job.agent))?;
    Ok((said, timed_out))
}

/// Ends this process and everything it started (a teardown shell and what that runs). On Windows
/// the whole tree; on Unix the process group, when this process leads its own (the supervisor
/// starts `crew clean-up` that way), else its direct children.
pub fn end_own_tree() -> ! {
    let pid = std::process::id().to_string();
    #[cfg(windows)]
    {
        playbook::shell_free_command("taskkill").args(["/T", "/F", "/PID", &pid]).output().ok();
    }
    #[cfg(unix)]
    {
        let group = std::process::Command::new("ps").args(["-o", "pgid=", "-p", &pid]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        if group.as_deref() == Some(pid.as_str()) {
            std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).output().ok();
        } else {
            std::process::Command::new("pkill").args(["-KILL", "-P", &pid]).output().ok();
        }
    }
    std::process::exit(1)
}

/// What `Agents::prepare` needs: `nitely start <key> --agent <vendor>` in the repo, output to `log`.
#[derive(Debug, Clone)]
pub struct Prepare {
    pub nitely: String,
    pub repo: PathBuf,
    pub key: String,
    pub vendor: String,
    pub log: PathBuf,
}

/// What one tick did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Tick {
    pub started: Vec<WorkerLine>,
    pub ended: Vec<WorkerLine>,
    pub running: usize,
    pub cap: u32,
    /// In words: `working`, `paused`, `no gate`, `nothing ready`, `quiet hours until 07:00`...
    pub status: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WorkerLine {
    pub issue: String,
    pub agent: String,
    /// Set for an ended worker: landed, submitted, asked, failed, stopped, launch_failed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub log: String,
}

/// How a worker ended.
#[derive(Debug, Clone, Copy)]
enum End {
    Exited(i32),
    /// Stopped by the supervisor at `timeout_minutes`.
    TimedOut,
    /// Its process was gone when a new supervisor looked (no exit code).
    Gone,
    /// Stopped by the supervisor after `idle_minutes` with no output and no report: hung.
    Silent,
}

enum Start {
    Running(WorkerLine),
    /// Its worktree is being set up in a process of its own.
    Preparing,
    /// It cannot start on this issue now; others may.
    Wait(String),
    SetupFailed(String),
    LaunchFailed(String),
}

/// A land the supervisor started for an issue whose worker handed it in and left.
struct Landing {
    issue: String,
    since: i64,
    log: PathBuf,
    process: Box<dyn Process>,
}

/// How long a land that could not start (another land held the lock) waits before the next try.
const LAND_RETRY_SECS: i64 = 60;

/// A landed worker's worktree being removed, in a process of its own.
struct Cleaning {
    issue: String,
    agent: String,
    worktree: PathBuf,
    since: i64,
    process: Box<dyn Process>,
}

/// An issue whose worktree setup runs, before its worker starts.
struct Preparing {
    id: i64,
    issue: IssueRecord,
    agent: String,
    agent_id: String,
    since: i64,
    log: PathBuf,
    process: Box<dyn Process>,
}

struct Running {
    id: i64,
    issue: String,
    agent: String,
    agent_id: String,
    worktree: PathBuf,
    started_at: i64,
    log: PathBuf,
    process: Box<dyn Process>,
    /// The log's length when last looked at, and when it last grew: a sign the worker is alive.
    seen: std::cell::Cell<(u64, i64)>,
    /// Its browser preview's port, when it was given one (not to a worker taken over).
    port: Option<u16>,
}

/// The ports workers are given for their browser preview, one each.
const PREVIEW_PORTS: std::ops::Range<u16> = 5190..5290;

/// Tokens a project used since a time (`usage::project_tokens_since` over a scan).
type TokensSince = Box<dyn Fn(&str, i64) -> u64>;

pub struct Supervisor {
    pub project: String,
    pub repo_root: PathBuf,
    /// The crew's files: `mcp.json` and one log folder per project.
    pub dir: PathBuf,
    /// How agents start the nitely MCP server.
    pub nitely: String,
    agents: Box<dyn Agents>,
    clock: Box<dyn Fn() -> Moment>,
    /// True once the person asked to stop: checked before each start, since a worktree's setup
    /// can take minutes.
    interrupted: Box<dyn Fn() -> bool>,
    /// Tokens the project used since a time, for the daily token budget.
    tokens: TokensSince,
    running: Vec<Running>,
    preparing: Vec<Preparing>,
    cleaning: Vec<Cleaning>,
    /// Lands of handed-in issues (NIT-222), and when an issue whose land could not start may try
    /// again.
    landings: Vec<Landing>,
    land_after: BTreeMap<String, i64>,
    /// Agents that would not start in this run, and why.
    disabled: BTreeMap<String, String>,
    /// Pick issues clear of the files work in flight holds (overlap.rs); reads the config.
    avoid_overlap: bool,
    /// Workers left by an earlier supervisor have been looked for.
    adopted: bool,
    /// Issues passed over for the rest of this run, and why.
    waiting: BTreeMap<String, String>,
    /// Launch failures in a row, per agent.
    launch_failures: BTreeMap<String, u32>,
    /// The crew paused itself because no agent would start; a resume starts over.
    self_paused: bool,
}

/// A worker that exits this soon, unsuccessfully and without one report, never got going: the
/// agent itself is not working (signed out, a bad command).
const LAUNCH_FAILURE_SECS: i64 = 120;

/// Launch failures in a row before the crew stops using an agent.
const LAUNCH_FAILURES_TO_DROP: u32 = 2;

/// The answer to a crew ask that gives the issue to a new worker.
pub const TRY_AGAIN: &str = "Try again";

impl Supervisor {
    pub fn new(project: &str, repo_root: &Path, dir: &Path, nitely: &str, agents: Box<dyn Agents>) -> Supervisor {
        Supervisor {
            project: project.to_uppercase(),
            repo_root: repo_root.to_path_buf(),
            dir: dir.to_path_buf(),
            nitely: nitely.into(),
            agents,
            clock: Box::new(Moment::now),
            interrupted: Box::new(|| false),
            tokens: Box::new(|project, since| {
                let config = crate::Config::load().unwrap_or_default();
                let Ok(db) = Db::open_default() else { return 0 };
                crate::usage::project_tokens_since(&crate::usage::scan(&config, &db, since), project, since)
            }),
            running: Vec::new(),
            preparing: Vec::new(),
            cleaning: Vec::new(),
            landings: Vec::new(),
            land_after: BTreeMap::new(),
            disabled: BTreeMap::new(),
            avoid_overlap: false,
            adopted: false,
            waiting: BTreeMap::new(),
            launch_failures: BTreeMap::new(),
            self_paused: false,
        }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> Moment + 'static) -> Supervisor {
        self.clock = Box::new(clock);
        self
    }

    /// Looks for workers again on the next pass, as a new supervisor does: after a newer build held
    /// the crew for a moment, it may have started some.
    pub fn rescan(&mut self) {
        self.adopted = false;
    }

    /// Picks issues clear of the files work in flight holds, reading the repos from the config.
    pub fn avoiding_overlap(mut self) -> Supervisor {
        self.avoid_overlap = true;
        self
    }

    pub fn with_interrupt(mut self, interrupted: impl Fn() -> bool + 'static) -> Supervisor {
        self.interrupted = Box::new(interrupted);
        self
    }

    pub fn with_tokens(mut self, tokens: impl Fn(&str, i64) -> u64 + 'static) -> Supervisor {
        self.tokens = Box::new(tokens);
        self
    }

    /// Why the daily token budget stops new workers, or None while there is budget (or none set).
    fn over_budget(&self, rules: &CrewRules, moment: &Moment) -> Option<String> {
        let max = rules.max_tokens_per_day?;
        let used = (self.tokens)(&self.project, moment.day_start);
        (used >= max).then(|| format!("token budget used: {} of {} today", crate::usage::short(used), crate::usage::short(max)))
    }

    fn actor(&self) -> Actor {
        Actor::Agent(format!("crew:{}", self.project))
    }

    /// Workers running, and issues whose worktree is being set up for one.
    pub fn running(&self) -> usize {
        self.running.len() + self.preparing.len()
    }

    /// Issues whose worktree is being set up.
    pub fn setting_up(&self) -> usize {
        self.preparing.len()
    }

    /// Settles the setups that finished, starting their workers, and nothing else: for a one-pass
    /// run, which must not leave a setup it began without anyone to see it through.
    pub fn finish_setups(&mut self, db: &mut Db) -> Result<Tick> {
        let mut tick = Tick::default();
        match Playbook::load(&self.repo_root) {
            Ok(p) => self.advance(db, &p, !(self.interrupted)(), &mut tick)?,
            Err(_) => self.advance(db, &Playbook::default(), false, &mut tick)?,
        }
        tick.running = self.running();
        Ok(tick)
    }

    /// The issues of workers still running, and which of them are landing.
    pub fn running_issues(&self) -> Vec<(String, bool)> {
        self.running.iter().map(|r| (r.issue.clone(), self.landing(&r.issue))).collect()
    }

    /// One pass: settle workers that exited, then start workers into free slots. `draining` starts
    /// none (Ctrl+C was pressed once).
    pub fn tick(&mut self, db: &mut Db, draining: bool) -> Result<Tick> {
        let moment = (self.clock)();
        let mut tick = Tick::default();
        let playbook = Playbook::load(&self.repo_root);
        let rules = playbook.as_ref().map(|p| p.crew.clone()).unwrap_or_default();
        if !self.adopted {
            self.adopt(db, &rules, &mut tick)?;
            self.adopted = true;
        }
        self.reap(db, &rules, &mut tick)?;
        // A land that died with the machine left its lock: nothing would land again until a person
        // broke it.
        if let Some(holder) = crate::land::clear_dead_lock(&self.repo_root) {
            // The holder is the issue the land was for, when it names one.
            let key = db.issue(&holder)?.map(|i| i.key);
            db.log_event(&self.actor(), key.as_deref(), None, "status", &format!("Removed the land lock {holder} left: the land that took it is gone"))?;
        }
        self.finish_cleanups(db, &rules)?;
        self.finish_landings(db)?;
        if playbook.is_ok() {
            self.after_hand_in(db)?;
        }
        self.retry_answered(db)?;
        if self.self_paused && !db.crew_control(&self.project)?.paused {
            // A person resumed the crew: give every agent another chance.
            self.self_paused = false;
            self.disabled.clear();
            self.launch_failures.clear();
            self.waiting.clear();
        }
        tick.cap = rules.workers;
        match &playbook {
            Ok(p) => self.advance(db, p, !draining && !(self.interrupted)(), &mut tick)?,
            // Setups are still watched (and time out); finished ones go back, since no worker can
            // start without a playbook.
            Err(_) => self.advance(db, &Playbook::default(), false, &mut tick)?,
        }
        let status = match &playbook {
            Err(e) => Some(format!("the playbook does not read: {e:#}")),
            Ok(p) => self.why_not_starting(db, p, &moment, draining)?,
        };
        tick.status = match status {
            Some(why) => why,
            None => self.fill(db, playbook.as_ref().unwrap(), &mut tick)?,
        };
        tick.running = self.running();
        db.crew_heartbeat(&self.project, std::process::id(), &tick.status)?;
        Ok(tick)
    }

    /// Why no worker may start now, or None when slots may be filled.
    fn why_not_starting(&self, db: &Db, playbook: &Playbook, moment: &Moment, draining: bool) -> Result<Option<String>> {
        let rules = &playbook.crew;
        if draining {
            return Ok(Some("stopping: waiting for running workers".into()));
        }
        if rules.workers == 0 {
            return Ok(Some("off: the playbook's crew.workers is 0".into()));
        }
        if db.crew_control(&self.project)?.paused {
            return Ok(Some("paused".into()));
        }
        if playbook.gate.commands.is_empty() && playbook.gate.steps.is_empty() {
            return Ok(Some("no gate: the playbook has no [gate] commands, so the crew does not run".into()));
        }
        if let Some(spec) = &rules.quiet_hours {
            match quiet_until(spec, moment.minute_of_day) {
                Ok(Some(until)) => return Ok(Some(format!("quiet hours until {until}"))),
                Ok(None) => {}
                Err(e) => return Ok(Some(format!("{e:#}"))),
            }
        }
        let today = db.crew_started_since(&self.project, moment.day_start)?;
        if today >= rules.max_per_day() {
            return Ok(Some(format!("{today} of {} started today", rules.max_per_day())));
        }
        if let Some(why) = self.over_budget(rules, moment) {
            return Ok(Some(why));
        }
        if self.running() >= rules.workers as usize {
            return Ok(Some("working".into()));
        }
        // A disk filling up is worse than an idle slot (memory waits in the machine's admission).
        let worktrees = playbook.worktrees_folder(&self.repo_root).unwrap_or_else(|| self.repo_root.clone());
        if let Some(why) = crate::machine::disk_short(&worktrees, rules.min_free_disk_gb) {
            return Ok(Some(format!("{why}; no new worker starts")));
        }
        Ok(None)
    }

    /// Starts workers until the slots are full, the daily cap is reached or nothing is ready.
    fn fill(&mut self, db: &mut Db, playbook: &Playbook, tick: &mut Tick) -> Result<String> {
        let rules = &playbook.crew;
        let moment = (self.clock)();
        loop {
            // A setup that finished meanwhile (or at once) starts its worker before more are begun.
            self.advance(db, playbook, true, tick)?;
            if self.running() >= rules.workers as usize {
                break;
            }
            if (self.interrupted)() {
                return Ok("stopping: waiting for running workers".into());
            }
            if db.crew_started_since(&self.project, moment.day_start)? >= rules.max_per_day() {
                return Ok(format!("{} of {} started today", rules.max_per_day(), rules.max_per_day()));
            }
            if let Some(why) = self.over_budget(rules, &moment) {
                return Ok(why);
            }
            if let Some(why) = self.agents.memory_wait() {
                return Ok(why);
            }
            let installed: Vec<String> = rules
                .agents
                .iter()
                .filter(|a| rules.command_for(a).and_then(|c| self.agents.resolve(&c[0])).is_some())
                .cloned()
                .collect();
            if installed.is_empty() {
                return Ok(format!("no agent to start: none of {} is installed", rules.agents.join(", ")));
            }
            let installed: Vec<String> = installed.into_iter().filter(|a| !self.disabled.contains_key(a)).collect();
            if installed.is_empty() {
                // Nothing would start until someone fixes the agents: pause, so it shows as paused
                // with the reason, and a resume tries every agent again.
                let why: Vec<String> = self.disabled.iter().map(|(a, why)| format!("{a}: {why}")).collect();
                if !self.self_paused {
                    db.crew_pause(&self.project, true, &self.actor())?;
                    self.self_paused = true;
                }
                return Ok(format!("paused: no agent will start ({})", why.join("; ")));
            }
            let skipped: Vec<String> = self.waiting.keys().cloned().collect();
            // Approved goals first, in the person's order and within each one's worker budget.
            let order = self.goal_order(db, rules)?;
            let picked = if self.avoid_overlap {
                // Read fresh: repos and their playbooks change while a crew runs.
                let config = crate::Config::load()?;
                crate::overlap::next_task_ordered(&config, db, &self.project, self.actor().id(), &skipped, Some(&order))?
            } else {
                db.next_task_choosing_with_goal_budget(&self.project, self.actor().id(), &skipped, rules.goal_workers, |c| order.arrange(c).first().map(|(k, _, _)| k.clone()))?
                    .map(|issue| crate::overlap::Picked { issue, why: None })
            };
            let Some(crate::overlap::Picked { issue, why }) = picked else {
                return Ok(db.project_focus_wait_reason(&self.project)?.unwrap_or_else(|| self.nothing_ready()));
            };
            if let Some(why) = why {
                db.log_event(&self.actor(), Some(&issue.key), None, "picked", &format!("The crew picked {}: {why}", issue.key))?;
            }
            // A second attempt continues the first one's branch, with the first one's agent: the
            // branch's prefix picks the reviewer, so another agent on it would review itself.
            let vendors = branch_vendors(&self.repo_root, &issue.key);
            let earlier = vendors.iter().find(|v| installed.contains(v)).or(vendors.first()).cloned();
            let agent = match earlier {
                Some(v) if installed.contains(&v) => v,
                Some(v) => {
                    self.wait(db, &issue.key, format!("its branch is {v}'s, and {v} is not starting"))?;
                    continue;
                }
                None => installed[0].clone(),
            };
            let started = self.begin(db, playbook, &issue, &agent);
            self.handle_start(db, &issue.key, &agent, started, tick)?;
        }
        Ok(if self.preparing.is_empty() { "working".into() } else { format!("working; setting up {}", self.preparing.iter().map(|p| p.issue.key.as_str()).collect::<Vec<_>>().join(", ")) })
    }

    /// The project's active goals in the person's order, and which have room for another worker:
    /// a goal's own `workers`, else the playbook's `goal_workers`, counts the workers running or
    /// setting up on its issues.
    fn goal_order(&self, db: &Db, rules: &CrewRules) -> Result<crate::overlap::GoalOrder> {
        let goals = db.goals(&self.project, false)?;
        let active: std::collections::HashSet<i64> = goals.iter().map(|g| g.id).collect();
        let open = db.list_issues(Some(&self.project), false)?;
        let first = open.iter().filter(|i| crate::goal_plans::is_planning(i)).map(|i| i.key.clone()).collect();
        let goal_of: HashMap<String, i64> = open.into_iter().filter_map(|i| i.goal.filter(|g| active.contains(g)).map(|g| (i.key, g))).collect();
        let busy = self.running.iter().map(|r| r.issue.as_str()).chain(self.preparing.iter().map(|p| p.issue.key.as_str()));
        let mut working: HashMap<i64, i64> = HashMap::new();
        for key in busy {
            if let Some(g) = goal_of.get(key) {
                *working.entry(*g).or_default() += 1;
            }
        }
        let mut order = crate::overlap::GoalOrder { goal_of, first, ..Default::default() };
        for g in goals {
            let budget = g.workers.or((rules.goal_workers > 0).then_some(rules.goal_workers as i64));
            if budget.is_some_and(|b| working.get(&g.id).copied().unwrap_or(0) >= b) {
                order.full.insert(g.id);
            } else {
                order.open.push(g.id);
            }
        }
        Ok(order)
    }

    /// A port for a new worker's browser preview: the first of `PREVIEW_PORTS` that no running
    /// worker of any project has (their rows say, so a restarted supervisor knows too) and that is
    /// free on this machine.
    fn preview_port(&self, db: &Db) -> Option<u16> {
        let mut taken: Vec<u16> = self.running.iter().filter_map(|r| r.port).collect();
        if let Ok(mut stmt) = db.conn.prepare("SELECT port FROM crew_worker WHERE ended_at IS NULL AND port IS NOT NULL") {
            taken.extend(stmt.query_map([], |r| r.get::<_, i64>(0)).into_iter().flatten().flatten().map(|p| p as u16));
        }
        PREVIEW_PORTS.clone().find(|p| !taken.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
    }

    /// What follows an attempt to start a worker on `key`.
    fn handle_start(&mut self, db: &mut Db, key: &str, agent: &str, started: Result<Start>, tick: &mut Tick) -> Result<()> {
        let why = match started {
            Ok(Start::Running(line)) => {
                tick.started.push(line);
                return Ok(());
            }
            Ok(Start::Preparing) => return Ok(()),
            Ok(Start::Wait(why)) => return self.wait(db, key, why),
            Ok(Start::SetupFailed(why)) => {
                // A person has to look: starting it again would fail the same way every tick.
                db.add_note(key, &format!("The crew could not set up the worktree: {why}"), "handoff", &self.actor())?;
                db.move_issue(key, "Needs You", None, Some("the crew could not set up its worktree"), &self.actor())?;
                return Ok(());
            }
            Ok(Start::LaunchFailed(why)) => why,
            Err(e) => format!("{e:#}"),
        };
        // The agent would not start: the issue goes back, and this run stops using that agent
        // instead of failing the same way on every tick.
        self.disabled.insert(agent.to_string(), first_line(&why).to_string());
        db.add_note(key, &format!("The crew could not start a {agent} worker: {why}"), "note", &self.actor())?;
        db.move_issue(key, "Todo", None, Some("the crew could not start a worker"), &self.actor())?;
        Ok(())
    }

    /// Claims `issue` for `agent` and starts setting up its worktree in a process of its own.
    fn begin(&mut self, db: &mut Db, playbook: &Playbook, issue: &IssueRecord, agent: &str) -> Result<Start> {
        let worktree = playbook.worktree_for(&self.repo_root, agent, issue);
        // The id the worker's own MCP server will use, so its reports keep the claim fresh.
        let agent_id = format!("{agent}:{}", crate::paths::normalize(&worktree.to_string_lossy()));
        db.reassign_claim(&issue.key, &agent_id)?;
        if worktree.join(".git").exists() {
            if let Some(why) = other_branch(&worktree, &playbook.branch_for(agent, issue), agent, &issue.key) {
                return Ok(Start::Wait(why));
            }
        }
        let ts = (self.clock)().ts;
        let logs = self.dir.join(&self.project);
        std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
        let log = logs.join(format!("{ts}-{}-setup.log", issue.key.to_lowercase()));
        let id = db.crew_worker_started(&self.project, &issue.key, agent, &agent_id, &worktree, &log, ts, "setup")?;
        match self.prepare(db, id, issue, agent, &log) {
            Ok(process) => {
                self.preparing.push(Preparing { id, issue: issue.clone(), agent: agent.into(), agent_id, since: ts, log, process });
                Ok(Start::Preparing)
            }
            Err(e) => {
                db.crew_worker_ended(id, None, "launch_failed", &format!("{e:#}"))?;
                Err(e)
            }
        }
    }

    /// Starts `nitely start` for `issue` and records its pid on the worker's row.
    fn prepare(&self, db: &Db, id: i64, issue: &IssueRecord, agent: &str, log: &Path) -> Result<Box<dyn Process>> {
        let prep = Prepare { nitely: self.nitely.clone(), repo: self.repo_root.clone(), key: issue.key.clone(), vendor: agent.into(), log: log.to_path_buf() };
        let process = self.agents.prepare(&prep).with_context(|| format!("starting `{} start {}`", self.nitely, issue.key))?;
        db.crew_worker_pid(id, process.pid())?;
        Ok(process)
    }

    /// Starts the workers whose worktree setup finished, sends a failed or too slow one to a
    /// person, and, when `start` is false (the crew is stopping), hands finished ones back.
    fn advance(&mut self, db: &mut Db, playbook: &Playbook, start: bool, tick: &mut Tick) -> Result<()> {
        let worktrees = playbook.worktrees_folder(&self.repo_root).unwrap_or_else(|| self.repo_root.clone());
        let disk_short = start && !self.preparing.is_empty() && crate::machine::disk_short(&worktrees, playbook.crew.min_free_disk_gb).is_some();
        let deadline = playbook.crew.setup_timeout_minutes.max(1) as i64 * 60;
        let now = (self.clock)().ts;
        let mut i = 0;
        while i < self.preparing.len() {
            let p = &self.preparing[i];
            if !db.crew_resume_setup(p.id, &p.issue.key, &p.agent_id)? {
                // Another allocator owns it now. Do not launch, move its issue, or end sessions
                // in the shared worktree on behalf of this obsolete setup.
                self.preparing.remove(i).process.stop();
                continue;
            }
            let exited = match self.preparing[i].process.exited() {
                // A setup taken over from an earlier supervisor has no exit code: its record says.
                Ok(Some(code)) if code < 0 => {
                    let worktree = Path::new(self.preparing[i].agent_id.split_once(':').map_or("", |(_, w)| w));
                    Some(if playbook::setup_passed(worktree) { Ok(0) } else { Err("its setup ended without passing".to_string()) })
                }
                Ok(Some(code)) => Some(Ok(code)),
                Ok(None) if now - self.preparing[i].since > deadline => {
                    self.preparing[i].process.stop();
                    Some(Err(format!("it ran past {} minutes (crew.setup_timeout_minutes) and was stopped", deadline / 60)))
                }
                Ok(None) => None,
                Err(e) => Some(Err(format!("{e:#}"))),
            };
            let Some(exited) = exited else {
                i += 1;
                continue;
            };
            // Set up and ready, a worker still launches only while the machine has memory for it
            // (NIT-266) and the disk room (NIT-246); until then it waits here, its setup done, and is
            // looked at again next tick.
            if start && matches!(exited, Ok(0)) && (disk_short || self.agents.memory_turn().is_some()) {
                i += 1;
                continue;
            }
            let p = self.preparing.remove(i);
            let started = match exited {
                Ok(0) if !start => {
                    db.crew_worker_ended(p.id, Some(0), "stopped", "the crew stopped once its worktree was ready")?;
                    db.move_issue(&p.issue.key, "Todo", None, Some("the crew is stopping; its worktree is ready"), &self.actor())?;
                    continue;
                }
                Ok(0) => self.launch(db, playbook, p.id, &p.issue, &p.agent, &p.agent_id),
                Ok(code) => Ok(Start::SetupFailed(format!("`nitely start` exited with code {code}:\n{}", log_tail(&p.log)))),
                Err(why) => Ok(Start::SetupFailed(format!("{why}\n{}", log_tail(&p.log)))),
            };
            match &started {
                Ok(Start::SetupFailed(why)) => db.crew_worker_ended(p.id, None, "setup_failed", first_line(why))?,
                Ok(Start::Wait(why)) => db.crew_worker_ended(p.id, None, "stopped", why)?,
                Err(e) => db.crew_worker_ended(p.id, None, "launch_failed", &format!("{e:#}"))?,
                _ => {}
            }
            self.handle_start(db, &p.issue.key, &p.agent, started, tick)?;
        }
        Ok(())
    }

    /// Hands `key` back and passes over it for the rest of this run.
    fn wait(&mut self, db: &mut Db, key: &str, why: String) -> Result<()> {
        db.move_issue(key, "Todo", None, Some(&format!("the crew cannot start it: {why}")), &self.actor())?;
        self.waiting.insert(key.to_string(), why);
        Ok(())
    }

    fn nothing_ready(&self) -> String {
        let mut s = if self.running.is_empty() { "nothing ready".to_string() } else { "working; nothing else ready".to_string() };
        if !self.waiting.is_empty() {
            let waits: Vec<String> = self.waiting.iter().map(|(k, why)| format!("{k}: {why}")).collect();
            s.push_str(&format!(" (passed over {})", waits.join("; ")));
        }
        s
    }

    /// Starts the agent in `issue`'s worktree, which its setup has made.
    fn launch(&mut self, db: &mut Db, playbook: &Playbook, id: i64, issue: &IssueRecord, agent: &str, agent_id: &str) -> Result<Start> {
        let worktree = playbook.worktree_for(&self.repo_root, agent, issue);
        let wanted = playbook.branch_for(agent, issue);
        if let Some(why) = other_branch(&worktree, &wanted, agent, &issue.key) {
            return Ok(Start::Wait(why));
        }
        let branch = current_branch(&worktree).unwrap_or(wanted);
        let agent_id = agent_id.to_string();

        let ts = (self.clock)().ts;
        let logs = self.dir.join(&self.project);
        std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
        prune_logs(&logs, LOGS_KEPT);
        let log = logs.join(format!("{ts}-{}-{agent}.log", issue.key.to_lowercase()));
        let mcp_config = self.write_mcp_config()?;
        let git_dir = playbook::git(&self.repo_root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| self.repo_root.join(".git").to_string_lossy().into_owned());
        let command = playbook.crew.command_for(agent).with_context(|| format!("the playbook has no command for {agent} (add crew.commands.{agent})"))?;
        let fill = |a: &str| {
            a.replace("{worktree}", &worktree.to_string_lossy())
                .replace("{repo}", &self.repo_root.to_string_lossy())
                .replace("{git_dir}", &git_dir)
                .replace("{mcp_config}", &mcp_config.to_string_lossy())
                .replace("{nitely}", &toml_string(&self.nitely))
                .replace("{nitely_env}", &toml_inline_table(&nitely_env()))
        };
        let Some(program) = self.agents.resolve(&command[0]) else {
            let why = format!("{} is not installed", command[0]);
            db.crew_worker_ended(id, None, "launch_failed", &why)?;
            return Ok(Start::LaunchFailed(why));
        };
        db.crew_worker_working(id, &log, ts)?;
        let port = self.preview_port(db);
        if let Some(p) = port {
            db.conn.execute("UPDATE crew_worker SET port = ?2 WHERE id = ?1", params![id, p])?;
        }
        let mut env = vec![("NITELY_CREW".into(), id.to_string()), ("NITELY_PROJECT".into(), self.project.clone())];
        if let Some(p) = port {
            env.push(("NITELY_PREVIEW_PORT".into(), p.to_string()));
        }
        let launch = Launch {
            agent: agent.into(),
            program,
            args: command[1..].iter().map(|a| fill(a)).collect(),
            cwd: worktree.clone(),
            brief: brief(issue, &self.project, &worktree, &branch, &self.repo_root, port),
            log: log.clone(),
            env,
        };
        let process = match self.agents.start(&launch) {
            Ok(p) => p,
            Err(e) => {
                let why = format!("{e:#}");
                db.crew_worker_ended(id, None, "launch_failed", &why)?;
                return Ok(Start::LaunchFailed(why));
            }
        };
        db.crew_worker_pid(id, process.pid())?;
        db.log_event(&self.actor(), Some(&issue.key), Some(&worktree.to_string_lossy()), "crew_started", &format!("Crew worker started ({agent}), log: {}", log.display()))?;
        let line = WorkerLine { issue: issue.key.clone(), agent: agent.into(), outcome: String::new(), log: log.to_string_lossy().into_owned() };
        self.running.push(Running { id, issue: issue.key.clone(), agent: agent.into(), agent_id, worktree, started_at: ts, log, process, seen: std::cell::Cell::new((0, ts)), port });
        Ok(Start::Running(line))
    }

    /// `mcp.json` for Claude Code: the nitely server and nothing else, on this supervisor's data
    /// folder (`nitely_env`).
    fn write_mcp_config(&self) -> Result<PathBuf> {
        let path = self.dir.join("mcp.json");
        let env: serde_json::Map<String, serde_json::Value> = nitely_env().into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
        let config = serde_json::json!({ "mcpServers": { "nitely": { "command": self.nitely, "args": ["mcp"], "env": env } } });
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&path, serde_json::to_string_pretty(&config)?).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Takes over the workers an earlier supervisor left running (it crashed, or its terminal was
    /// closed): they count toward the cap and are settled when they exit. Those that are gone are
    /// settled now.
    fn adopt(&mut self, db: &mut Db, rules: &CrewRules, tick: &mut Tick) -> Result<()> {
        for w in db.crew_running(&self.project)? {
            // Ones this supervisor already watches (a rescan).
            if self.running.iter().any(|r| r.id == w.id) || self.preparing.iter().any(|p| p.id == w.id) {
                continue;
            }
            if w.phase == "setup" {
                // A setup an earlier supervisor started: watch it if it still runs. If it is gone,
                // its worktree says how it went (`nitely start` would not set up an existing worktree
                // again); with no worktree yet, start it again.
                let Some(issue) = db.issue(&w.issue_key)? else { continue };
                if !db.crew_resume_setup(w.id, &w.issue_key, &w.agent_id)? {
                    continue;
                }
                let worktree = Path::new(&w.worktree);
                let process = match w.pid.and_then(|pid| self.agents.adopt(pid)) {
                    Some(p) => p,
                    None if playbook::setup_passed(worktree) => Box::new(Done(0)),
                    None if worktree.join(".git").exists() => Box::new(Done(1)),
                    None => match self.prepare(db, w.id, &issue, &w.agent, Path::new(&w.log)) {
                        Ok(p) => p,
                        Err(e) => {
                            db.crew_worker_ended(w.id, None, "launch_failed", &format!("{e:#}"))?;
                            db.move_issue(&issue.key, "Todo", None, Some("the crew could not set up its worktree again"), &self.actor())?;
                            continue;
                        }
                    },
                };
                // Its deadline counts from when the setup began, not from this supervisor.
                self.preparing.push(Preparing { id: w.id, issue, agent: w.agent, agent_id: w.agent_id, since: w.started_at, log: PathBuf::from(w.log), process });
                continue;
            }
            let running = Running {
                id: w.id,
                issue: w.issue_key,
                agent: w.agent,
                agent_id: w.agent_id,
                worktree: PathBuf::from(w.worktree),
                started_at: w.started_at,
                log: PathBuf::from(w.log),
                process: Box::new(Gone),
                seen: std::cell::Cell::new((0, w.started_at)),
                port: w.port,
            };
            match w.pid.and_then(|pid| self.agents.adopt(pid)) {
                Some(process) => self.running.push(Running { process, ..running }),
                None => {
                    let outcome = self.settle(db, rules, &running, End::Gone)?;
                    tick.ended.push(WorkerLine { issue: running.issue, agent: running.agent, outcome, log: running.log.to_string_lossy().into_owned() });
                }
            }
        }
        Ok(())
    }

    /// Settles every worker that exited, and stops (then settles) those past the timeout unless
    /// they are landing.
    fn reap(&mut self, db: &mut Db, rules: &CrewRules, tick: &mut Tick) -> Result<()> {
        let deadline = rules.timeout_minutes.max(1) as i64 * 60;
        let idle = rules.idle_minutes as i64 * 60;
        let ts = (self.clock)().ts;
        let mut i = 0;
        while i < self.running.len() {
            let end = match self.running[i].process.exited() {
                Ok(Some(code)) => Some(End::Exited(code)),
                Ok(None) if ts - self.running[i].started_at > deadline && !self.landing(&self.running[i].issue) => {
                    self.running[i].process.stop();
                    Some(End::TimedOut)
                }
                Ok(None) if idle > 0 && ts - self.last_sign(db, &self.running[i])? > idle && !self.landing(&self.running[i].issue) => {
                    self.running[i].process.stop();
                    Some(End::Silent)
                }
                Ok(None) => None,
                Err(_) => Some(End::Exited(-1)),
            };
            match end {
                None => i += 1,
                Some(end) => {
                    let w = self.running.remove(i);
                    let outcome = self.settle(db, rules, &w, end)?;
                    tick.ended.push(WorkerLine { issue: w.issue, agent: w.agent, outcome, log: w.log.to_string_lossy().into_owned() });
                }
            }
        }
        Ok(())
    }

    /// When a running worker last showed it is alive: its log last grew, or it last called a nitely
    /// tool (a report, a note, a submit), or it started.
    fn last_sign(&self, db: &Db, w: &Running) -> Result<i64> {
        // The log's length, read through an open handle: Windows can keep a file's directory entry
        // (its size and modified time) stale while the worker still holds the file open writing.
        let len = std::fs::File::open(&w.log).and_then(|f| f.metadata()).map(|m| m.len()).ok();
        let (seen, grown) = w.seen.get();
        let grown = match len {
            Some(l) if l != seen => {
                let now = (self.clock)().ts;
                w.seen.set((l, now));
                now
            }
            _ => grown,
        };
        let called: Option<i64> = db.conn.query_row("SELECT MAX(ts) FROM event WHERE actor = ?1", [&w.agent_id], |r| r.get(0))?;
        Ok([w.started_at, grown, called.unwrap_or(0)].into_iter().max().unwrap_or(w.started_at))
    }

    /// Records how a worker ended, read from its issue's state, and deals with a worker that ended
    /// without handing its issue on (docs/crew.md, Guardrails): a handoff note when it left none,
    /// and the issue back to Todo, or to a person after `attempts` failures.
    fn settle(&mut self, db: &mut Db, rules: &CrewRules, w: &Running, end: End) -> Result<String> {
        use rusqlite::OptionalExtension;
        let settled: Option<String> = db.conn.query_row(
            "SELECT COALESCE(outcome, 'stopped') FROM crew_worker WHERE id = ?1 AND ended_at IS NOT NULL",
            [w.id], |r| r.get(0),
        ).optional()?;
        if let Some(outcome) = settled {
            // Reclamation can retire a crashed worker before its original supervisor observes
            // the exit. Do not then end the replacement's sessions or change its issue.
            return Ok(outcome);
        }
        let issue = db.issue(&w.issue)?;
        let state = issue.as_ref().map(|i| i.state.clone()).unwrap_or_default();
        let still_mine = issue.as_ref().is_some_and(|i| i.claimed_by.as_deref() == Some(w.agent_id.as_str()));
        let since = db.crew_worker_logged_at(w.id)?;
        let timeline: Vec<_> = db.timeline(&w.issue)?.into_iter().filter(|t| t.ts >= since).collect();
        let reported = timeline.iter().any(|t| t.actor == w.agent_id && t.kind == "status");
        let elapsed = (self.clock)().ts - w.started_at;
        let code = match end {
            End::Exited(c) => Some(c),
            _ => None,
        };
        // A worker hands its change in and leaves (NIT-212): a submit it made is still running, or
        // came back after it left, and the claim stays until the supervisor acts on the result.
        let handed_in = state == "In Progress" && still_mine && self.handed_in(db, &w.issue, &w.agent_id, w.started_at)?.is_some();
        let mut outcome = match state.as_str() {
            "Done" => "landed",
            "In Review" | "Approved" => "submitted",
            "Needs You" => "asked",
            "In Progress" if handed_in => "submitted",
            "In Progress" if still_mine => "failed",
            "Todo" => "failed",
            _ => "stopped",
        };
        if outcome == "failed" && matches!(end, End::Exited(c) if c != 0) && elapsed < LAUNCH_FAILURE_SECS && !reported {
            outcome = "launch_failed";
        }
        let how = match end {
            End::Exited(c) => format!("exit code {c} after {} min", elapsed / 60),
            End::TimedOut => format!("stopped after {} min (crew.timeout_minutes)", elapsed / 60),
            End::Gone => format!("gone while no supervisor watched, after at most {} min", elapsed / 60),
            End::Silent => format!("stopped after {} min with no output and no report (crew.idle_minutes): hung", rules.idle_minutes),
        };
        let reason = format!("{how}, issue {state}");
        db.crew_worker_ended(w.id, code, outcome, &reason)?;
        db.log_event(&self.actor(), Some(&w.issue), None, "crew_ended", &format!("Crew worker ({}) ended: {outcome}, {reason}. Log: {}", w.agent, w.log.display()))?;

        if outcome == "launch_failed" {
            // The agent never got going: not the issue's fault, so no attempt is counted.
            let n = self.launch_failures.entry(w.agent.clone()).or_insert(0);
            *n += 1;
            if *n >= LAUNCH_FAILURES_TO_DROP {
                self.disabled.insert(w.agent.clone(), format!("exited {n} times right after starting; see {}", w.log.display()));
            }
            if state == "In Progress" {
                db.move_issue(&w.issue, "Todo", None, Some(&format!("the {} worker exited right after starting", w.agent)), &self.actor())?;
            }
            return Ok(outcome.into());
        }
        self.launch_failures.remove(&w.agent);
        if outcome == "landed" {
            self.clean_up_landed(db, w)?;
        }
        if outcome != "failed" {
            return Ok(outcome.into());
        }

        // Only this worker's own handoff counts: the supervisor's note on an earlier attempt can
        // fall in the same second this one started.
        if !timeline.iter().any(|t| t.kind == "handoff" && t.actor == w.agent_id) {
            let last = timeline.iter().rev().find(|t| t.actor == w.agent_id && t.kind == "status").and_then(|t| t.summary.clone());
            let last = last.map_or("it made no report".to_string(), |s| format!("its last report: \"{s}\""));
            let note = format!(
                "A crew worker ({}) stopped without handing this issue on ({how}); {last}. Whatever it committed is on its branch in {}.{} Its log: {}",
                w.agent,
                w.worktree.display(),
                worktree_state(&w.worktree),
                w.log.display()
            );
            db.add_note(&w.issue, &note, "handoff", &self.actor())?;
        }
        let failures = db.crew_failures_since(&w.issue, self.reset_since(db, &w.issue)?)?;
        if failures >= rules.attempts.max(1) {
            let logs: Vec<String> = db.crew_workers_for(&w.issue)?.into_iter().filter(|x| x.outcome.as_deref() == Some("failed")).take(failures as usize).map(|x| x.log).collect();
            let question = format!(
                "The crew tried {} {failures} times without submitting it (last: {how}). Logs: {}. Answer \"{TRY_AGAIN}\" to give it to a new worker, or take it yourself.",
                w.issue,
                logs.join(", ")
            );
            db.move_issue(&w.issue, "Needs You", None, Some(&format!("the crew tried it {failures} times")), &self.actor())?;
            db.create_ask(&question, &[TRY_AGAIN.to_string(), "I'll take it".to_string()], Some(&w.issue), None, &self.actor())?;
        } else if state == "In Progress" {
            db.move_issue(&w.issue, "Todo", None, Some(&format!("the {} worker stopped without submitting", w.agent)), &self.actor())?;
        }
        Ok(outcome.into())
    }

    /// Removes the worktree land kept whole while the worker ran in it, as land itself would have:
    /// the playbook's teardown, `git worktree remove` without force (a dirty worktree stays), and
    /// `git branch -d` (an unmerged branch stays). Only a worktree git still tracks is touched.
    fn clean_up_landed(&mut self, db: &Db, w: &Running) -> Result<()> {
        let listed = playbook::git(&self.repo_root, &["worktree", "list", "--porcelain"]);
        let tracked = listed.as_deref().is_ok_and(|l| {
            l.lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .any(|p| crate::paths::normalize(p) == crate::paths::normalize(&w.worktree.to_string_lossy()))
        });
        if !tracked {
            return Ok(());
        }
        // A person who landed it with --keep wants the worktree and branch kept.
        let since = db.crew_worker_logged_at(w.id)?;
        if db.timeline(&w.issue)?.iter().any(|t| t.kind == "land_kept" && t.ts >= since) {
            return Ok(());
        }
        let Ok(branch) = playbook::git(&w.worktree, &["rev-parse", "--abbrev-ref", "HEAD"]) else { return Ok(()) };
        let ts = (self.clock)().ts;
        let log = self.dir.join(&self.project).join(format!("{ts}-{}-cleanup.log", w.issue.to_lowercase()));
        let timeout_minutes = Playbook::load(&self.repo_root).map(|p| p.crew.setup_timeout_minutes).unwrap_or(30);
        let job = CleanUp {
            nitely: self.nitely.clone(),
            repo: self.repo_root.clone(),
            worktree: w.worktree.clone(),
            branch: branch.trim().to_string(),
            log: log.clone(),
            issue: w.issue.clone(),
            actor: self.actor().id().to_string(),
            agent: w.agent.clone(),
            timeout_minutes,
        };
        match self.agents.clean_up(&job) {
            Ok(process) => self.cleaning.push(Cleaning { issue: w.issue.clone(), agent: w.agent.clone(), worktree: w.worktree.clone(), since: ts, process }),
            Err(e) => db.log_event(&self.actor(), Some(&w.issue), None, "crew_cleaned", &format!("Could not start removing {}: {e:#}", w.worktree.display()))?,
        }
        Ok(())
    }

    /// Records the worktree removals that finished, and stops one past `setup_timeout_minutes`
    /// (a teardown that hangs), leaving its worktree for a person.
    /// Forgets the removals that ended (each records its own outcome on the issue), and stops one
    /// a minute past `setup_timeout_minutes`, after its own limit should have ended it, with
    /// everything it started.
    fn finish_cleanups(&mut self, db: &mut Db, rules: &CrewRules) -> Result<()> {
        let deadline = (rules.setup_timeout_minutes.max(1) as i64 + 1) * 60;
        let now = (self.clock)().ts;
        let mut i = 0;
        while i < self.cleaning.len() {
            match self.cleaning[i].process.exited() {
                Ok(Some(_)) | Err(_) => {
                    self.cleaning.remove(i);
                }
                Ok(None) if now - self.cleaning[i].since > deadline => {
                    let mut c = self.cleaning.remove(i);
                    c.process.stop();
                    let why = format!("its removal ran past {} minutes and was stopped; {} is left as it is", deadline / 60, c.worktree.display());
                    db.log_event(&self.actor(), Some(&c.issue), None, "crew_cleaned", &format!("After the {} worker left: {why}", c.agent))?;
                }
                Ok(None) => i += 1,
            }
        }
        Ok(())
    }

    /// Worktree removals still running: a stopping supervisor waits for them.
    pub fn cleaning(&self) -> usize {
        self.cleaning.len()
    }

    /// The submit `agent_id` handed `key` in with since `since`: the issue's latest submit job, when
    /// that worker queued it.
    fn handed_in(&self, db: &Db, key: &str, agent_id: &str, since: i64) -> Result<Option<crate::submit_job::Job>> {
        Ok(db.latest_submit_job(key)?.filter(|j| j.requested_by == agent_id && j.queued_at >= since))
    }

    /// Issues a worker handed in and then left (NIT-222). One its submit sent back (findings, a
    /// failed gate or push, a job that failed) goes to a new worker, with the outcome in a handoff
    /// note. One that passed and still waits to land (In Review, or Approved) is landed here: a
    /// worker's whole session for one mechanical call would cost more, and the land queue may keep
    /// it waiting.
    fn after_hand_in(&mut self, db: &mut Db) -> Result<()> {
        let now = (self.clock)().ts;
        for issue in db.list_issues(Some(&self.project), false)? {
            if !matches!(issue.state.as_str(), "In Progress" | "In Review" | "Approved") {
                continue;
            }
            let busy = self.running.iter().any(|r| r.issue == issue.key)
                || self.preparing.iter().any(|p| p.issue.key == issue.key)
                || self.landings.iter().any(|l| l.issue == issue.key);
            if busy {
                continue;
            }
            // Its last crew worker ended having handed it in (its submit may have reached Needs You,
            // for a person's approval, before the worker was seen to end).
            let Some(worker) = db.crew_workers_for(&issue.key)?.into_iter().next() else { continue };
            if !matches!(worker.outcome.as_deref(), Some("submitted" | "asked")) {
                continue;
            }
            let Some(job) = self.handed_in(db, &issue.key, &worker.agent_id, worker.started_at)? else { continue };
            // Nobody else has taken it: still the worker's claim, or none (an approval clears it).
            let unclaimed = issue.claimed_by.as_deref().is_none_or(|c| c == worker.agent_id);
            // A land after the submit that stopped and sent it back to In Progress (the app's land of
            // an approved change, say).
            let land_stopped = job.ended_at.is_some_and(|end| {
                db.timeline(&issue.key).map(|t| t.iter().any(|e| e.kind == "land_failed" && e.ts >= end)).unwrap_or(false)
            });
            // A submit whose process died with the machine (its gate or review cut short) runs again
            // as it was handed in, once, while the branch is still at what was handed in.
            let interrupted = job.state == "failed" && job.error.as_deref().is_some_and(|e| e.contains(crate::submit_job::ABANDONED));
            if issue.state == "In Progress" && interrupted && unclaimed && self.resubmit(db, &issue.key, &worker, &job)? {
                continue;
            }
            match issue.state.as_str() {
                "In Progress" if job.finished() && (job.needs_the_submitter() || land_stopped) && unclaimed => {
                    db.mark_submit_jobs_delivered(&[job.id])?;
                    let what = if job.needs_the_submitter() { job.outcome() } else { "it passed, and then its land stopped (the land's reason is on this issue).".to_string() };
                    let note = format!(
                        "Its submit (job {}) came back after the {} worker had left: {what} The review's findings and the gate's output are on this issue. \
                         A new crew worker takes it, in {}.",
                        job.id,
                        worker.agent,
                        worker.worktree
                    );
                    db.add_note(&issue.key, &note, "handoff", &self.actor())?;
                    db.move_issue(&issue.key, "Todo", None, Some("its submit came back; a new crew worker takes it"), &self.actor())?;
                }
                // Whoever lands: the author's land is this worker's, gone; Nitely's own land in the
                // submit may have found the lock held, and the app retries only approvals. Where
                // only the app can verify an approval (macOS), an approved one is the app's to land.
                "Approved" if !crate::approval::agents_can_verify() => {}
                "In Review" | "Approved" if self.land_after.get(&issue.key).is_none_or(|t| now >= *t) => {
                    self.start_land(db, &issue.key)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Hands `job`'s change in again for the worker that handed it in, when its branch is still at
    /// the commit handed in and it was not handed in again already for that commit. Returns
    /// whether it did.
    fn resubmit(&mut self, db: &mut Db, key: &str, worker: &CrewWorker, job: &crate::submit_job::Job) -> Result<bool> {
        let head = playbook::git(Path::new(&job.worktree), &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()).unwrap_or_default();
        if head != job.head {
            return Ok(false);
        }
        let tries: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE AND head = ?2 AND state = 'failed' AND error LIKE '%' || ?3 || '%'",
            params![key, job.head, crate::submit_job::ABANDONED],
            |r| r.get(0),
        )?;
        if tries > 1 {
            return Ok(false);
        }
        let opts = crate::submit::SubmitOptions { dir: &job.worktree, issue_key: Some(key), skip_review: job.skip_review, local_only: job.local_only, batch: true, solution: job.solution.as_ref(), solution_commit: Some(&job.head), solution_patch_id: job.solution_patch_id.as_deref() };
        // One that cannot be handed in as it is (uncommitted changes since) goes to a new worker
        // instead, with its handoff; it never holds up the rest of the pass.
        let Ok(again) = crate::submit_job::enqueue(db, &opts, &Actor::Agent(worker.agent_id.clone())) else {
            return Ok(false);
        };
        db.mark_submit_jobs_delivered(&[job.id])?;
        match self.agents.run_submit_job(&self.nitely, &again) {
            Ok(Some(pid)) => {
                db.conn.execute("UPDATE submit_job SET pid = ?2 WHERE id = ?1 AND state = 'queued' AND pid IS NULL", params![again.id, pid])?;
            }
            Ok(None) => {}
            Err(e) => {
                db.conn.execute("UPDATE submit_job SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1", params![again.id, crate::db::now(), format!("could not start it again: {e:#}")])?;
                return Ok(false);
            }
        }
        let short = &job.head[..job.head.len().min(7)];
        db.add_note(key, &format!("Its submit (job {}) stopped when its process died (the machine restarted, or it was stopped). Handed {short} in again as job {}, for the {} worker that handed it in.", job.id, again.id, worker.agent), "note", &self.actor())?;
        Ok(true)
    }

    /// Starts `nitely land` for `key` in a process of its own.
    fn start_land(&mut self, db: &Db, key: &str) -> Result<()> {
        let ts = (self.clock)().ts;
        let logs = self.dir.join(&self.project);
        std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
        let log = logs.join(format!("{ts}-{}-land.log", key.to_lowercase()));
        let job = LandJob { nitely: self.nitely.clone(), repo: self.repo_root.clone(), issue: key.to_string(), log: log.clone() };
        match self.agents.land(&job) {
            Ok(process) => {
                db.log_event(&self.actor(), Some(key), None, "status", &format!("Landing it for the worker that handed it in, log: {}", log.display()))?;
                self.landings.push(Landing { issue: key.to_string(), since: ts, log, process });
            }
            Err(e) => {
                db.log_event(&self.actor(), Some(key), None, "status", &format!("Could not start its land: {e:#}"))?;
                self.land_after.insert(key.to_string(), ts + LAND_RETRY_SECS);
            }
        }
        Ok(())
    }

    /// Settles the lands that ended: landed, nothing more to do (land removed the worktree); one
    /// that could not start, or is still waiting in the land queue, tries again later; one that
    /// stopped on the change itself (a conflict, a failed gate, a change since its review) goes to
    /// a new worker with why.
    fn finish_landings(&mut self, db: &mut Db) -> Result<()> {
        let now = (self.clock)().ts;
        let mut i = 0;
        while i < self.landings.len() {
            let ended = matches!(self.landings[i].process.exited(), Ok(Some(_)) | Err(_));
            if !ended {
                i += 1;
                continue;
            }
            let l = self.landings.remove(i);
            let Some(issue) = db.issue(&l.issue)? else { continue };
            if issue.state == "Done" || !matches!(issue.state.as_str(), "In Review" | "Approved") {
                self.land_after.remove(&l.issue);
                continue;
            }
            let stopped = db.timeline(&l.issue)?.into_iter().rev().find(|t| t.kind == "land_failed" && t.ts >= l.since).and_then(|t| t.summary);
            // Not the change's fault: the lock was held, or the approval is for the app to sign
            // (land itself sends that one back to the person). Only a land that stopped on the change
            // itself goes to a new worker; one that now needs a person's approval goes to them.
            let not_the_change = |why: &str| why.contains("another land holds") || why.contains(crate::approval::UNSIGNED);
            match stopped {
                Some(why) if why.contains("waits for a human") => {
                    db.move_issue(&l.issue, "Needs You", None, Some("its land waits for a person's approval"), &self.actor())?;
                    self.land_after.remove(&l.issue);
                }
                Some(why) if !not_the_change(&why) => {
                    let note = format!("The land stopped: {}. A new crew worker takes it from here (its land log: {}).", why.trim_start_matches("Land stopped: "), l.log.display());
                    db.add_note(&l.issue, &note, "handoff", &self.actor())?;
                    db.move_issue(&l.issue, "Todo", None, Some("its land stopped; a new crew worker takes it"), &self.actor())?;
                    self.land_after.remove(&l.issue);
                }
                // Another land held the lock, it is still waiting in the land queue, or the land
                // process ended without a word: later.
                _ => {
                    self.land_after.insert(l.issue.clone(), now + LAND_RETRY_SECS);
                }
            }
        }
        Ok(())
    }

    /// Failures count from the last time a person moved the issue or answered the crew about it.
    fn reset_since(&self, db: &Db, key: &str) -> Result<i64> {
        let moved = db.timeline(key)?.into_iter().filter(|t| t.actor == "human" && t.kind == "issue_moved").map(|t| t.ts).max().unwrap_or(0);
        let answered = db.crew_ask_answers(key, self.actor().id())?.into_iter().flatten().max().unwrap_or(0);
        Ok(moved.max(answered))
    }

    /// Issues the crew sent to a person whose latest crew ask was answered "Try again" go back to
    /// Todo for a new worker.
    fn retry_answered(&self, db: &mut Db) -> Result<()> {
        for key in db.crew_retries(&self.project, self.actor().id())? {
            db.move_issue(&key, "Todo", None, Some(&format!("the person answered \"{TRY_AGAIN}\"")), &self.actor())?;
        }
        Ok(())
    }

    /// Whether the repo's land lock names `issue`: that worker must not be stopped now.
    fn landing(&self, issue: &str) -> bool {
        let Ok(common) = playbook::git(&self.repo_root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]) else { return false };
        let lock = Path::new(common.trim()).join(crate::land::LOCK_FILE);
        std::fs::read_to_string(lock).is_ok_and(|held| held.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case(issue)))
    }

    /// Stops every worker that is not landing: a note on its issue, and the issue back to Todo.
    /// Returns the issues still landing, which keep running.
    pub fn stop_all(&mut self, db: &mut Db) -> Result<Vec<String>> {
        for mut p in std::mem::take(&mut self.preparing) {
            p.process.stop();
            db.crew_worker_ended(p.id, None, "stopped", "stopped while setting up its worktree")?;
            if db.issue(&p.issue.key)?.is_some_and(|i| i.state == "In Progress" && i.claimed_by.as_deref() == Some(p.agent_id.as_str())) {
                db.move_issue(&p.issue.key, "Todo", None, Some("the crew was stopped while setting up its worktree"), &self.actor())?;
            }
        }
        let mut landing = Vec::new();
        let mut i = 0;
        while i < self.running.len() {
            if self.landing(&self.running[i].issue) {
                landing.push(self.running[i].issue.clone());
                i += 1;
                continue;
            }
            let mut w = self.running.remove(i);
            w.process.stop();
            db.crew_worker_ended(w.id, None, "stopped", "stopped by the person running the crew")?;
            let still_working = db.issue(&w.issue)?.is_some_and(|i| i.state == "In Progress" && i.claimed_by.as_deref() == Some(w.agent_id.as_str()));
            if still_working {
                db.add_note(&w.issue, &format!("The crew was stopped while a {} worker had this issue. Its log: {}", w.agent, w.log.display()), "handoff", &self.actor())?;
                db.move_issue(&w.issue, "Todo", None, Some("the crew was stopped"), &self.actor())?;
            }
            db.log_event(&self.actor(), Some(&w.issue), None, "crew_ended", &format!("Crew worker ({}) stopped. Log: {}", w.agent, w.log.display()))?;
        }
        Ok(landing)
    }

    /// The supervisor is leaving: no heartbeat, so the app says the crew is not running.
    pub fn leave(&self, db: &Db) -> Result<()> {
        db.crew_leave(&self.project, std::process::id())
    }
}

/// The brief every worker gets on stdin.
pub fn brief(issue: &IssueRecord, project: &str, worktree: &Path, branch: &str, repo: &Path, port: Option<u16>) -> String {
    if crate::goal_plans::is_planning(issue) {
        return planner_brief(issue, project, worktree, repo);
    }
    // A light worktree with lanes builds through `run` (docs/build-lanes.md).
    let lanes = Playbook::load(repo).ok().is_some_and(|pb| pb.lanes.is_some());
    let mut light = if lanes && crate::light::is_light(worktree) { format!("\n{}\n", crate::light::RUN_LINE) } else { String::new() };
    if let Some(p) = port {
        light.push_str(&format!("\nFor a browser preview (`npm run dev`), use port {p} (also in NITELY_PREVIEW_PORT): other workers preview at the same time on theirs.\n"));
    }
    let (key, title, wt, repo) = (&issue.key, &issue.title, worktree.display(), repo.display());
    format!(
        "You are a Nitely crew worker: Nitely started you to work on one issue, {key}, \"{title}\", in project {project}. \
Nobody is watching this session; nobody will answer a question here.

The issue is claimed for you and its worktree is ready: {wt} (branch {branch}). It is your working directory. \
Work only there; never edit or commit in {repo}.

1. Read the issue with get_issue {key}, and the repo's AGENTS.md.
2. Do the work. Call report with key {key} at each step, one line.
3. File anything you find but will not fix with file_issue.
4. If only the human can decide something, ask it, move {key} to Needs You, leave a handoff note and stop: do not wait for the answer.
5. Commit, then call submit (without wait) with solution_title and solution_summary describing the implemented outcome, behavior, validation and any remaining limits for this revision (not the original problem): it hands the change in and returns at once. Then leave the handoff note (step 7) and stop. Nitely runs the gate and the review, and lands it when it passes; if it comes back (findings, a failed gate, a land that stopped), a new worker gets it with why.
6. If the issue's latest handoff says its submit or land came back, start from that: fix what it names (on a rebase conflict, resolve it in the worktree and `git rebase --continue`), commit, and submit again.
7. Before you stop, for any reason, leave a note on {key} with handoff: true: what is done, what is left, what you were unsure about.

Then end. Do not take another issue: Nitely starts a new worker for it.
{light}"
    )
}

/// The brief of a worker that plans a goal from the person's sentence: it reads and drafts, and
/// changes nothing. `propose_goal` closes its issue, which the supervisor reads as the work done.
fn planner_brief(issue: &IssueRecord, project: &str, worktree: &Path, repo: &Path) -> String {
    let (key, title, wt, repo) = (&issue.key, &issue.title, worktree.display(), repo.display());
    format!(
        "You are a Nitely crew worker: Nitely started you to plan a goal, {key}, \"{title}\", in project {project}. \
Nobody is watching this session; nobody will answer a question here.

The issue is claimed for you. Its worktree, {wt}, is there for you to read the code: change nothing in it, and never edit or commit in {repo}.

1. Read the issue with get_issue {key}: the person's sentence, and how to plan it. Then the repo's AGENTS.md and the project's brief (get_project).
2. Draft the plan with propose_goal, passing planning_issue {key}: the goal, its issues in the order to work them, each with the files it touches and its size, and the existing issues that belong in it. Call report with key {key} at each step, one line.
3. If the sentence is too unclear to plan, ask it, move {key} to Needs You, leave a handoff note and stop: do not wait for the answer.
4. Before you stop, leave a note on {key} with handoff: true: the plan's issue, or why there is none.

Do not commit, submit or file issues yourself: the person approves the plan in the Inbox, and that files them. Then end."
    )
}

/// Why `agent` may not work in `worktree`: it is on another branch than `wanted` (the folder pattern
/// need not name the vendor). The same agent's branch for the issue under an older title is fine.
fn other_branch(worktree: &Path, wanted: &str, agent: &str, key: &str) -> Option<String> {
    let on = current_branch(worktree)?;
    let on = on.as_str();
    let same = on == wanted || crate::git::parse_branch(on) == (Some(agent.to_string()), Some(key.to_uppercase()));
    (!same).then(|| format!("its worktree {} is on {on}, not {wanted}", worktree.display()))
}

/// The branch a worktree is on, or, while a rebase of a branch is in progress there (a land that
/// stopped on a conflict), the branch being rebased.
fn current_branch(worktree: &Path) -> Option<String> {
    let on = playbook::git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?.trim().to_string();
    if on != "HEAD" {
        return Some(on);
    }
    let paths = playbook::git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", "rebase-merge/head-name", "--git-path", "rebase-apply/head-name"]).ok()?;
    let rebasing = paths.lines().find_map(|p| std::fs::read_to_string(p.trim()).ok()).map(|h| h.trim().trim_start_matches("refs/heads/").to_string());
    Some(rebasing.filter(|b| !b.is_empty()).unwrap_or(on))
}

/// What a worker left in its worktree that the next one must see first: a rebase in progress, or
/// uncommitted changes. Empty when neither (or the worktree is gone).
fn worktree_state(worktree: &Path) -> String {
    if !worktree.join(".git").exists() {
        return String::new();
    }
    let mut said = String::new();
    let rebasing = playbook::git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", "rebase-merge", "--git-path", "rebase-apply"])
        .is_ok_and(|paths| paths.lines().any(|p| !p.trim().is_empty() && Path::new(p.trim()).exists()));
    if rebasing {
        said.push_str(" A rebase is in progress there: finish it (git rebase --continue) or abort it first.");
    }
    let changed = playbook::git(worktree, &["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0);
    if changed > 0 {
        said.push_str(&format!(" It has {changed} uncommitted file{} there, kept as they are: look at them before anything else.", if changed == 1 { "" } else { "s" }));
    }
    said
}

/// The end of a setup log, for the note on the issue.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
    let tail = tail.join("\n");
    let start = tail.char_indices().rev().nth(1500).map_or(0, |(i, _)| i);
    format!("{}\n(full log: {})", &tail[start..], log.display())
}

/// The vendors of the local branches that name `key` (`claude/nit-3-...` is claude's NIT-3).
fn branch_vendors(repo: &Path, key: &str) -> Vec<String> {
    let refs = playbook::git(repo, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"]).unwrap_or_default();
    let mut vendors: Vec<String> = Vec::new();
    for branch in refs.lines() {
        if let (Some(vendor), Some(k)) = crate::git::parse_branch(branch.trim()) {
            if k.eq_ignore_ascii_case(key) && !vendors.contains(&vendor) {
                vendors.push(vendor);
            }
        }
    }
    vendors
}

/// `s` as a TOML string, for a `-c key=value` override.
fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The variables a worker's nitely MCP server gets, since agents don't pass theirs on by
/// themselves: the ones that pick Nitely's data and agent folders, as this supervisor has them,
/// and `NITELY_CREW`, so its calls act as the worker's id, the one its claim is under (mcp.rs).
fn nitely_env() -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["NITELY_HOME", "NITELY_AGENT_HOME"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| (k.to_string(), v)))
        .collect();
    env.push(("NITELY_CREW".into(), "1".into()));
    env
}

/// `{KEY = "value", ...}`, a TOML inline table for a `-c` override.
fn toml_inline_table(pairs: &[(String, String)]) -> String {
    let body: Vec<String> = pairs.iter().map(|(k, v)| format!("{k} = {}", toml_string(v))).collect();
    format!("{{{}}}", body.join(", "))
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Deletes the oldest `.log` files in `dir` beyond `keep`, making room for one more.
fn prune_logs(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut logs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "log")).collect();
    if logs.len() < keep {
        return;
    }
    // Names start with the start time, so name order is age order.
    logs.sort();
    for old in &logs[..logs.len() + 1 - keep] {
        std::fs::remove_file(old).ok();
    }
}

/// The crew's state for a project, for `crew status` and the app.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CrewControl {
    pub paused: bool,
    pub changed_by: Option<String>,
    pub changed_at: Option<i64>,
    pub supervisor_pid: Option<u32>,
    pub heartbeat_at: Option<i64>,
    pub status: Option<String>,
    /// The repo whose playbook the supervisor follows.
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrewWorker {
    pub id: i64,
    pub project: String,
    pub issue_key: String,
    pub agent: String,
    #[serde(skip)]
    pub agent_id: String,
    pub pid: Option<u32>,
    pub worktree: String,
    pub log: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    /// `setup` while its worktree is being set up, then `work`.
    pub phase: String,
    /// Its browser preview's port, when it was given one.
    #[serde(skip)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CrewStatus {
    pub project: String,
    pub cap: u32,
    pub running: Vec<CrewWorker>,
    pub started_today: u32,
    pub max_per_day: u32,
    /// Tokens the project's agents used today, and the playbook's daily budget if it sets one.
    pub tokens_today: u64,
    pub max_tokens_per_day: Option<u64>,
    pub paused: bool,
    /// A supervisor's heartbeat is fresh.
    pub supervised: bool,
    /// What the supervisor last said, e.g. `working` or `quiet hours until 07:00`.
    pub status: Option<String>,
    pub recent: Vec<CrewWorker>,
}

/// The crew of `project`, whose repo playbook is `playbook`; `tokens_today` is what its agents
/// used since `moment`'s day started.
pub fn status(db: &Db, project: &str, playbook: &Playbook, moment: &Moment, tokens_today: u64) -> Result<CrewStatus> {
    let control = db.crew_control(project)?;
    let workers = db.crew_workers(project, 20)?;
    Ok(CrewStatus {
        project: project.to_uppercase(),
        cap: playbook.crew.workers,
        running: db.crew_running(project)?,
        started_today: db.crew_started_since(project, moment.day_start)?,
        max_per_day: playbook.crew.max_per_day(),
        tokens_today,
        max_tokens_per_day: playbook.crew.max_tokens_per_day,
        paused: control.paused,
        supervised: control.heartbeat_at.is_some_and(|h| moment.ts - h < HEARTBEAT_STALE_SECS),
        status: control.status,
        recent: workers.into_iter().filter(|w| w.ended_at.is_some()).take(10).collect(),
    })
}

fn worker_row(r: &rusqlite::Row) -> rusqlite::Result<CrewWorker> {
    Ok(CrewWorker {
        id: r.get(0)?,
        project: r.get(1)?,
        issue_key: r.get(2)?,
        agent: r.get(3)?,
        pid: r.get(4)?,
        worktree: r.get(5)?,
        log: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        exit_code: r.get(9)?,
        outcome: r.get(10)?,
        reason: r.get(11)?,
        agent_id: r.get(12)?,
        phase: r.get(13)?,
        port: r.get::<_, Option<i64>>(14)?.map(|p| p as u16),
    })
}

/// The projects whose crew should be running, each with the repo whose playbook it follows: the
/// first repo of the project whose playbook has `workers > 0`.
pub fn supervisors_wanted(config: &crate::Config) -> Vec<(String, PathBuf)> {
    let mut wanted: Vec<(String, PathBuf)> = Vec::new();
    for repo in &config.repos {
        let Some(project) = repo.project.as_deref().map(str::to_uppercase) else { continue };
        if wanted.iter().any(|(p, _)| *p == project) {
            continue;
        }
        if Playbook::load_with(Path::new(&repo.root), config).is_ok_and(|p| p.crew.workers > 0) {
            wanted.push((project, PathBuf::from(&repo.root)));
        }
    }
    wanted
}

/// Whether no supervisor is running `project`'s crew: none ever did, it left, or its heartbeat is
/// stale.
pub fn unsupervised(db: &Db, project: &str) -> Result<bool> {
    let c = db.crew_control(project)?;
    Ok(!(c.supervisor_pid.is_some() && c.heartbeat_at.is_some_and(|h| now() - h < HEARTBEAT_STALE_SECS)))
}

/// The `nitely` the app starts supervisors with: the one on PATH, where `nitely install` puts new
/// builds, so a supervisor that leaves for a new build is replaced by that build. Never one in
/// Cargo's build folders, which `cargo run` puts first on PATH (NIT-193).
pub fn nitely_on_path() -> Option<PathBuf> {
    crate::install::installed_on_path()
}

/// Starts `nitely crew run --managed` for `project` in the background, apart from the caller: no
/// window, its own process group, and nothing that ends it when the caller ends, so a crew outlives
/// the app. Its output goes to `supervisor.log` in the project's crew folder. The caller should
/// `try_wait` the child now and then, so an exited one does not linger.
pub fn start_supervisor(nitely: &Path, project: &str, repo: &Path, dir: &Path, take_over: Option<u32>) -> Result<std::process::Child> {
    use std::process::Stdio;
    let logs = dir.join(project.to_uppercase());
    std::fs::create_dir_all(&logs).with_context(|| format!("creating {}", logs.display()))?;
    let log_path = logs.join("supervisor.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).with_context(|| format!("opening {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(nitely);
    cmd.args(["crew", "run", "--managed", "--project", project, "--repo"]).arg(repo).current_dir(repo);
    if let Some(from) = take_over {
        cmd.args(["--take-over", &from.to_string()]);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
    for (name, _) in std::env::vars() {
        if crate::review::is_session_marker(&name) {
            cmd.env_remove(name);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A hidden console of its own (so its Ctrl+C handling works) and its own process group.
        cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().with_context(|| format!("starting {} crew run", nitely.display()))
}

/// The crew of a project as the app shows it: the crew of the first of its repos whose playbook
/// has one. None when no repo has a crew, none is running and no supervisor is there, so a project
/// that never used a crew shows nothing.
pub fn project_status(db: &Db, config: &crate::Config, project: &str, moment: &Moment) -> Result<Option<CrewStatus>> {
    let roots: Vec<&str> = config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .map(|r| r.root.as_str())
        .collect();
    // The repo a running supervisor says it follows, when it is one of the project's; else the
    // first whose playbook has a crew (a supervisor that left says nothing about what runs next).
    let live = !unsupervised(db, project)?;
    let followed = db
        .crew_control(project)?
        .repo
        .filter(|r| live && roots.iter().any(|root| crate::paths::normalize(root) == crate::paths::normalize(r)));
    let playbooks: Vec<Playbook> = followed
        .iter()
        .map(String::as_str)
        .chain(roots.iter().copied())
        .filter_map(|root| Playbook::load_with(Path::new(root), config).ok())
        .collect();
    let first_with_crew = if followed.is_some() { playbooks.first() } else { playbooks.iter().find(|p| p.crew.workers > 0) };
    let playbook = first_with_crew.or(playbooks.first()).cloned().unwrap_or_default();
    let tokens = crate::usage::project_tokens_since(&crate::usage::scan(config, db, moment.day_start), project, moment.day_start);
    let status = status(db, project, &playbook, moment, tokens)?;
    Ok((status.cap > 0 || !status.running.is_empty() || status.supervised).then_some(status))
}

const WORKER_COLUMNS: &str = "id, project, issue_key, agent, pid, worktree, log_path, started_at, ended_at, exit_code, outcome, reason, agent_id, phase, port";

impl Db {
    /// Marks the sessions a crew worker runs: in its worktree, started once the worker had. A
    /// database without the crew tables (an older one, read-only) marks none.
    pub(crate) fn mark_crew(&self, sessions: &mut [crate::db::Session]) {
        let Some(oldest) = sessions.iter().map(|s| s.started_at).min() else { return };
        let workers: Vec<(String, i64, Option<i64>, String)> = (|| -> rusqlite::Result<_> {
            let mut stmt = self.conn.prepare("SELECT worktree, started_at, ended_at, agent_id FROM crew_worker WHERE ended_at IS NULL OR ended_at >= ?1")?;
            let rows = stmt.query_map([oldest - 60], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect()
        })()
        .unwrap_or_default();
        if workers.is_empty() {
            return;
        }
        let workers: Vec<(String, i64, Option<i64>, String)> = workers.into_iter().map(|(w, s, e, a)| (crate::paths::normalize(&w), s, e, a)).collect();
        for s in sessions.iter_mut() {
            let cwd = crate::paths::normalize(&s.cwd);
            s.crew = workers.iter().any(|(wt, started, ended, agent_id)| {
                let inside = cwd == *wt || cwd.strip_prefix(wt.as_str()).is_some_and(|rest| rest.starts_with('/'));
                // A minute's slack: the worker's row is written just before its process starts.
                let began_in_it = s.started_at >= started - 60 && ended.is_none_or(|e| s.started_at <= e);
                // A worker's reports go to a session named after its agent id, which an earlier
                // session in the same worktree may already have started: it counts while the worker
                // is there.
                let reports_in_it = s.id == *agent_id && s.last_seen_at >= started - 60 && ended.is_none_or(|e| s.last_seen_at <= e + 60);
                inside && (began_in_it || reports_in_it)
            });
        }
    }

    /// Moves a claim to another agent id: the supervisor claims an issue, then hands it to the
    /// worker's own id once it knows where the worker will run.
    pub fn reassign_claim(&self, key: &str, agent: &str) -> Result<()> {
        self.conn.execute("UPDATE issue SET claimed_by = ?2, claimed_at = ?3 WHERE key = ?1", rusqlite::params![key, agent, now()])?;
        Ok(())
    }

    pub fn crew_control(&self, project: &str) -> Result<CrewControl> {
        use rusqlite::OptionalExtension;
        let row = self
            .conn
            .query_row(
                "SELECT paused, changed_by, changed_at, supervisor_pid, heartbeat_at, status, repo FROM crew_control WHERE project = ?1 COLLATE NOCASE",
                [project],
                |r| {
                    Ok(CrewControl {
                        paused: r.get::<_, i64>(0)? != 0,
                        changed_by: r.get(1)?,
                        changed_at: r.get(2)?,
                        supervisor_pid: r.get(3)?,
                        heartbeat_at: r.get(4)?,
                        status: r.get(5)?,
                        repo: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or_default())
    }

    /// Pauses (no new workers; running ones finish) or resumes a project's crew.
    pub fn crew_pause(&self, project: &str, paused: bool, actor: &Actor) -> Result<()> {
        let project = project.to_uppercase();
        self.conn.execute(
            "INSERT INTO crew_control (project, paused, changed_by, changed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET paused = ?2, changed_by = ?3, changed_at = ?4",
            rusqlite::params![project, paused as i64, actor.id(), now()],
        )?;
        self.log_event(actor, None, None, if paused { "crew_paused" } else { "crew_resumed" }, &format!("{} the {project} crew", if paused { "Paused" } else { "Resumed" }))
    }

    /// Fails when another supervisor's heartbeat for `project` is fresh.
    /// In one statement, so two supervisors starting at once (the app's and one handing over to a
    /// new build) cannot both win.
    pub fn crew_claim_supervisor(&self, project: &str, pid: u32) -> Result<()> {
        self.crew_take_over(project, pid, None)
    }

    /// `crew_claim_supervisor`, also taking the crew from the live supervisor `from`: the one
    /// handing over to this newer build.
    pub fn crew_take_over(&self, project: &str, pid: u32, from: Option<u32>) -> Result<()> {
        let ts = now();
        let claimed = self.conn.execute(
            "INSERT INTO crew_control (project, supervisor_pid, heartbeat_at, status) VALUES (?1, ?2, ?3, 'starting')
             ON CONFLICT (project) DO UPDATE SET supervisor_pid = ?2, heartbeat_at = ?3, status = 'starting'
             WHERE crew_control.supervisor_pid IS NULL OR crew_control.heartbeat_at IS NULL
                OR crew_control.heartbeat_at <= ?3 - ?4 OR crew_control.supervisor_pid = ?2
                OR crew_control.supervisor_pid = ?5",
            rusqlite::params![project.to_uppercase(), pid, ts, HEARTBEAT_STALE_SECS, from],
        )?;
        if claimed == 0 {
            let other = self.crew_control(project)?.supervisor_pid.unwrap_or(0);
            bail!("another `nitely crew run` (pid {other}) is running the {project} crew; stop it first");
        }
        Ok(())
    }

    pub fn crew_heartbeat(&self, project: &str, pid: u32, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO crew_control (project, supervisor_pid, heartbeat_at, status) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET supervisor_pid = ?2, heartbeat_at = ?3, status = ?4",
            rusqlite::params![project.to_uppercase(), pid, now(), status],
        )?;
        Ok(())
    }

    /// Moves only the heartbeat, from a thread that keeps it fresh while a pass runs long (a
    /// worktree's setup can take minutes).
    pub fn crew_heartbeat_touch(&self, project: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE crew_control SET heartbeat_at = ?3 WHERE project = ?1 COLLATE NOCASE AND supervisor_pid = ?2",
            rusqlite::params![project, pid, now()],
        )?;
        Ok(())
    }

    /// What the crew says when no supervisor is there to say it, e.g. why the app could not start
    /// one. A running supervisor overwrites it on its next pass.
    pub fn crew_set_status(&self, project: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO crew_control (project, status) VALUES (?1, ?2) ON CONFLICT (project) DO UPDATE SET status = ?2",
            rusqlite::params![project.to_uppercase(), status],
        )?;
        Ok(())
    }

    /// Records the repo whose playbook `project`'s supervisor follows, for the app's crew status.
    pub fn crew_follows(&self, project: &str, repo: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE crew_control SET repo = ?2 WHERE project = ?1 COLLATE NOCASE",
            rusqlite::params![project, repo.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Whether a crew worker is running in `worktree` now.
    pub fn crew_worker_in(&self, worktree: &Path) -> Result<bool> {
        let mut stmt = self.conn.prepare("SELECT worktree FROM crew_worker WHERE ended_at IS NULL")?;
        let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let wanted = crate::paths::normalize(&worktree.to_string_lossy());
        Ok(rows.iter().any(|w| crate::paths::normalize(w) == wanted))
    }

    pub fn crew_leave(&self, project: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE crew_control SET supervisor_pid = NULL, heartbeat_at = NULL, status = 'not running'
             WHERE project = ?1 COLLATE NOCASE AND supervisor_pid = ?2",
            rusqlite::params![project, pid],
        )?;
        Ok(())
    }

    /// When a worker's row was written, in real time (its `started_at` follows the supervisor's
    /// clock, which tests move).
    fn crew_worker_logged_at(&self, id: i64) -> Result<i64> {
        use rusqlite::OptionalExtension;
        let key: Option<String> = self.conn.query_row("SELECT issue_key FROM crew_worker WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let Some(key) = key else { return Ok(0) };
        let ts: Option<i64> = self.conn.query_row(
            "SELECT MAX(ts) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'crew_started'",
            [key],
            |r| r.get(0),
        )?;
        Ok(ts.unwrap_or(0))
    }

    /// Crew workers on `key` that failed and ended after `since`.
    pub fn crew_failures_since(&self, key: &str, since: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM crew_worker WHERE issue_key = ?1 COLLATE NOCASE AND outcome = 'failed' AND ended_at > ?2",
            rusqlite::params![key, since],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// When each of `asked_by`'s asks about `key` was answered (None while open).
    pub fn crew_ask_answers(&self, key: &str, asked_by: &str) -> Result<Vec<Option<i64>>> {
        let mut stmt = self.conn.prepare("SELECT answered_at FROM ask WHERE issue_key = ?1 COLLATE NOCASE AND asked_by = ?2")?;
        let rows = stmt.query_map(rusqlite::params![key, asked_by], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Issues of `project` in Needs You whose newest ask is the crew's, answered "Try again" since
    /// the issue last went to Needs You.
    pub fn crew_retries(&self, project: &str, asked_by: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.key, a.answered_at FROM issue i JOIN ask a ON a.issue_key = i.key
             WHERE i.project = ?1 COLLATE NOCASE AND i.state = 'Needs You' AND a.asked_by = ?2 AND a.answer = ?3
               AND a.id = (SELECT MAX(id) FROM ask WHERE issue_key = i.key)",
        )?;
        let rows = stmt.query_map(rusqlite::params![project, asked_by, TRY_AGAIN], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut keys = Vec::new();
        for row in rows {
            let (key, answered) = row?;
            let into_needs_you = self
                .timeline(&key)?
                .into_iter()
                .filter(|t| t.kind == "issue_moved" && t.summary.as_deref().is_some_and(|s| s.contains("→ Needs You")))
                .map(|t| t.ts)
                .max()
                .unwrap_or(0);
            if answered >= into_needs_you {
                keys.push(key);
            }
        }
        Ok(keys)
    }

    /// The crew's workers on one issue, newest first.
    pub fn crew_workers_for(&self, key: &str) -> Result<Vec<CrewWorker>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {WORKER_COLUMNS} FROM crew_worker WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC"))?;
        let rows = stmt.query_map([key], worker_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn crew_started_since(&self, project: &str, since: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM crew_worker WHERE project = ?1 COLLATE NOCASE AND started_at >= ?2",
            rusqlite::params![project, since],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    #[allow(clippy::too_many_arguments)]
    fn crew_worker_started(&self, project: &str, issue: &str, agent: &str, agent_id: &str, worktree: &Path, log: &Path, ts: i64, phase: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO crew_worker (project, issue_key, agent, agent_id, worktree, log_path, started_at, phase) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![project, issue, agent, agent_id, worktree.to_string_lossy(), log.to_string_lossy(), ts, phase],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A worker's setup passed and its agent starts: its log is the agent's from now on.
    /// Its time limit counts from here, not from the setup.
    fn crew_worker_working(&self, id: i64, log: &Path, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE crew_worker SET phase = 'work', log_path = ?2, started_at = ?3 WHERE id = ?1",
            rusqlite::params![id, log.to_string_lossy(), ts],
        )?;
        Ok(())
    }

    fn crew_worker_pid(&self, id: i64, pid: u32) -> Result<()> {
        self.conn.execute("UPDATE crew_worker SET pid = ?2 WHERE id = ?1", rusqlite::params![id, pid])?;
        Ok(())
    }

    /// Resuming setup and automatic reclamation compete for the same issue claim. Renew only
    /// while this unclosed setup still owns it; a new claimant closes the row atomically too.
    /// This also protects a supervisor that read the setup before another agent reclaimed it.
    fn crew_resume_setup(&self, id: i64, key: &str, agent_id: &str) -> Result<bool> {
        let renewed = self.conn.execute(
            "UPDATE issue SET claimed_at = ?4 WHERE key = ?2 AND state = 'In Progress' AND claimed_by = ?3
             AND EXISTS (SELECT 1 FROM crew_worker WHERE id = ?1 AND issue_key = ?2 AND agent_id = ?3 AND phase = 'setup' AND ended_at IS NULL)",
            params![id, key, agent_id, now()],
        )?;
        if renewed == 0 {
            self.conn.execute(
                "UPDATE crew_worker SET ended_at = ?2, outcome = 'stopped', reason = 'the setup no longer owns the issue' WHERE id = ?1 AND ended_at IS NULL",
                params![id, now()],
            )?;
        }
        Ok(renewed > 0)
    }

    fn crew_worker_ended(&self, id: i64, code: Option<i32>, outcome: &str, reason: &str) -> Result<()> {
        let at = now();
        self.conn.execute(
            "UPDATE crew_worker SET ended_at = ?2, exit_code = ?3, outcome = ?4, reason = ?5 WHERE id = ?1",
            rusqlite::params![id, at, code, outcome, reason],
        )?;
        // Its agent sends no session end when it exits (a `codex exec` never does), so its sessions
        // end here: the ones in its worktree since it started, and the one named after its agent id.
        let (worktree, started, agent_id): (String, i64, String) =
            self.conn.query_row("SELECT worktree, started_at, agent_id FROM crew_worker WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let named = Some(agent_id.as_str()).filter(|a| !a.is_empty());
        self.end_sessions_in(&worktree, started - 60, named, at, &format!("Ended: its crew worker exited ({outcome})"))?;
        Ok(())
    }

    /// A project's workers, newest first.
    pub fn crew_workers(&self, project: &str, limit: usize) -> Result<Vec<CrewWorker>> {
        self.crew_worker_rows("", project, limit)
    }

    /// A project's workers that have not ended, however old.
    pub fn crew_running(&self, project: &str) -> Result<Vec<CrewWorker>> {
        self.crew_worker_rows("AND ended_at IS NULL", project, 1 << 30)
    }

    fn crew_worker_rows(&self, filter: &str, project: &str, limit: usize) -> Result<Vec<CrewWorker>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WORKER_COLUMNS} FROM crew_worker WHERE project = ?1 COLLATE NOCASE {filter} ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(rusqlite::params![project, limit as i64], worker_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// The agents installed on this machine, started as real processes.
pub struct SystemAgents;

impl Agents for SystemAgents {
    fn resolve(&self, program: &str) -> Option<String> {
        if Path::new(program).is_absolute() {
            return Path::new(program).is_file().then(|| program.to_string());
        }
        if let Some(found) = on_path(program) {
            return Some(found.to_string_lossy().into_owned());
        }
        // On Windows the Codex CLI ships inside the Codex Store app and is not on PATH.
        let resolved = crate::review::resolve_program(program);
        (resolved != program && Path::new(&resolved).is_file()).then_some(resolved)
    }

    fn start(&self, launch: &Launch) -> Result<Box<dyn Process>> {
        use std::process::Stdio;
        let log = std::fs::File::create(&launch.log).with_context(|| format!("creating {}", launch.log.display()))?;
        let mut cmd = std::process::Command::new(&launch.program);
        cmd.args(&launch.args).current_dir(&launch.cwd);
        // Files, not pipes: a worker must not die because the supervisor did.
        cmd.stdin(Stdio::piped()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        // A worker started from inside an agent session must not think it is that session.
        for (name, _) in std::env::vars() {
            if crate::review::is_session_marker(&name) {
                cmd.env_remove(name);
            }
        }
        for (k, v) in &launch.env {
            cmd.env(k, v);
        }
        // Held as a build is (NIT-266): below-normal priority, and it and what it starts to a run's
        // memory cap; it outlives this supervisor all the same.
        let share = crate::lanes::share();
        crate::lanes::share_env(&mut cmd, share.jobs);
        let guard = crate::procs::TreeGuard::with(crate::procs::Contain { memory_cap: share.cap, low_priority: true, end_with_guard: false });
        // No console window, and its own process group: Ctrl+C in the supervisor's console must not
        // reach a worker that may be landing.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        match &guard {
            Some(g) => g.prepare_with(&mut cmd, CREATE_NEW_PROCESS_GROUP),
            None => {
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000 | CREATE_NEW_PROCESS_GROUP); // CREATE_NO_WINDOW
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().with_context(|| format!("starting {}", launch.program))?;
        if let Some(g) = &guard {
            g.adopt(&child);
        }
        let cap = share.cap;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let brief = launch.brief.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(brief.as_bytes()).ok();
        });
        Ok(Box::new(Capped { process: SystemProcess(child), guard, log: launch.log.clone(), cap }))
    }

    fn adopt(&self, pid: u32) -> Option<Box<dyn Process>> {
        pid_alive(pid).then(|| Box::new(Adopted(pid)) as Box<dyn Process>)
    }

    fn memory_wait(&self) -> Option<String> {
        crate::machine::would_wait()
    }

    fn memory_turn(&self) -> Option<String> {
        crate::machine::admit_here_once()
    }

    fn prepare(&self, prep: &Prepare) -> Result<Box<dyn Process>> {
        use std::process::Stdio;
        let log = std::fs::File::create(&prep.log).with_context(|| format!("creating {}", prep.log.display()))?;
        let program = self.resolve(&prep.nitely).unwrap_or_else(|| prep.nitely.clone());
        let mut cmd = std::process::Command::new(program);
        cmd.args(["start", &prep.key, "--agent", &prep.vendor]).current_dir(&prep.repo);
        cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        Ok(Box::new(SystemProcess(cmd.spawn().with_context(|| format!("starting {} start", prep.nitely))?)))
    }

    fn clean_up(&self, job: &CleanUp) -> Result<Box<dyn Process>> {
        use std::process::Stdio;
        if let Some(dir) = job.log.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let log = std::fs::File::create(&job.log).with_context(|| format!("creating {}", job.log.display()))?;
        let program = self.resolve(&job.nitely).unwrap_or_else(|| job.nitely.clone());
        let mut cmd = std::process::Command::new(program);
        cmd.args(["crew", "clean-up", "--repo"]).arg(&job.repo).arg("--worktree").arg(&job.worktree).args(["--branch", &job.branch]);
        cmd.args(["--issue", &job.issue, "--actor", &job.actor, "--agent", &job.agent, "--timeout-minutes", &job.timeout_minutes.to_string()]);
        cmd.current_dir(&job.repo).stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        Ok(Box::new(SystemProcess(cmd.spawn().with_context(|| format!("starting {} crew clean-up", job.nitely))?)))
    }

    fn land(&self, job: &LandJob) -> Result<Box<dyn Process>> {
        use std::process::Stdio;
        if let Some(dir) = job.log.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let log = std::fs::File::create(&job.log).with_context(|| format!("creating {}", job.log.display()))?;
        let program = self.resolve(&job.nitely).unwrap_or_else(|| job.nitely.clone());
        let mut cmd = std::process::Command::new(program);
        cmd.args(["land", &job.issue]).current_dir(&job.repo);
        cmd.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        Ok(Box::new(SystemProcess(cmd.spawn().with_context(|| format!("starting {} land {}", job.nitely, job.issue))?)))
    }

    fn run_submit_job(&self, nitely: &str, job: &crate::submit_job::Job) -> Result<Option<u32>> {
        use crate::submit_job::Runner;
        let program = self.resolve(nitely).unwrap_or_else(|| nitely.to_string());
        crate::submit_job::DetachedProcess { nitely: PathBuf::from(program) }.start(job)
    }
}

/// A setup known to have ended, with this exit code.
struct Done(i32);

impl Process for Done {
    fn pid(&self) -> u32 {
        0
    }
    fn exited(&mut self) -> Result<Option<i32>> {
        Ok(Some(self.0))
    }
    fn stop(&mut self) {}
}

/// A worker whose process is no longer there.
struct Gone;

impl Process for Gone {
    fn pid(&self) -> u32 {
        0
    }
    fn exited(&mut self) -> Result<Option<i32>> {
        Ok(Some(-1))
    }
    fn stop(&mut self) {}
}

/// A worker an earlier supervisor started: not this process's child, so it is watched by pid and
/// its exit code is not known.
struct Adopted(u32);

impl Process for Adopted {
    fn pid(&self) -> u32 {
        self.0
    }
    fn exited(&mut self) -> Result<Option<i32>> {
        Ok((!pid_alive(self.0)).then_some(-1))
    }
    fn stop(&mut self) {
        let pid = self.0.to_string();
        #[cfg(windows)]
        playbook::shell_free_command("taskkill").args(["/T", "/F", "/PID", &pid]).output().ok();
        #[cfg(unix)]
        {
            // As for a child: SIGTERM to the group, then SIGKILL, so it is gone before its issue
            // goes to another worker in the same worktree.
            std::process::Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).output().ok();
            for _ in 0..40 {
                if !pid_alive(self.0) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).output().ok();
        }
    }
}

/// Whether process `pid` is running (`procs::alive`: the system's own answer, no process started).
pub(crate) fn pid_alive(pid: u32) -> bool {
    crate::procs::alive(pid)
}

struct SystemProcess(std::process::Child);

impl Process for SystemProcess {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn exited(&mut self) -> Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(|s| s.code().unwrap_or(-1)))
    }

    fn stop(&mut self) {
        let pid = self.0.id().to_string();
        #[cfg(windows)]
        {
            // The whole tree: agents start shells, builds and MCP servers under them.
            playbook::shell_free_command("taskkill").args(["/T", "/F", "/PID", &pid]).output().ok();
        }
        #[cfg(unix)]
        {
            std::process::Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).output().ok();
            for _ in 0..40 {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            std::process::Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).output().ok();
        }
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

/// A crew worker held to a run's memory cap (NIT-266). On Windows its job holds the cap whatever
/// happens to this supervisor; looking at it here (each `exited`) is what stops one that went over,
/// and off Windows it is the only thing that holds it.
struct Capped {
    process: SystemProcess,
    guard: Option<crate::procs::TreeGuard>,
    log: PathBuf,
    cap: Option<u64>,
}

impl Process for Capped {
    fn pid(&self) -> u32 {
        self.process.pid()
    }

    fn exited(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.process.exited()? {
            return Ok(Some(code));
        }
        if self.guard.as_ref().is_some_and(|g| g.over_memory()) {
            let cap = self.cap.map(crate::machine::gb).unwrap_or_default();
            if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(&self.log) {
                use std::io::Write;
                writeln!(log, "
out of memory: the worker and what it started went over the {cap} memory cap a run has (Settings > Build lanes) and were stopped").ok();
            }
            self.stop();
            return Ok(Some(-1));
        }
        Ok(None)
    }

    fn stop(&mut self) {
        if let Some(g) = &self.guard {
            g.stop();
        }
        self.process.stop();
    }
}

/// `program` on PATH (with the Windows executable extensions).
fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat"] } else { &[""] };
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|ext| if ext.is_empty() { dir.join(program) } else { dir.join(format!("{program}.{ext}")) })
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewIssue;
    use std::cell::RefCell;
    use std::process::Command;
    use std::rc::Rc;

    #[test]
    fn codex_workers_get_the_supervisors_data_folder() {
        let codex = CrewRules::default().command_for("codex").unwrap();
        assert!(codex.iter().any(|a| a == "mcp_servers.nitely.env={nitely_env}"), "{codex:?}");
        assert_eq!(toml_inline_table(&[]), "{}");
        let env = [("NITELY_HOME".to_string(), r"C:\Temp\n h".to_string()), ("NITELY_AGENT_HOME".to_string(), "/tmp/a\"b".to_string())];
        let table = toml_inline_table(&env);
        assert_eq!(table, r#"{NITELY_HOME = 'C:\Temp\n h', NITELY_AGENT_HOME = '/tmp/a"b'}"#);
        let parsed: toml::Table = format!("env = {table}").parse().unwrap();
        assert_eq!(parsed["env"]["NITELY_HOME"].as_str(), Some(r"C:\Temp\n h"), "the value survives TOML quoting");
        assert!(nitely_env().contains(&("NITELY_CREW".into(), "1".into())), "a worker's MCP server acts as the worker's id");
    }

    #[test]
    fn crew_is_off_by_default_and_reads_its_section() {
        assert_eq!(Playbook::parse("").unwrap().crew.workers, 0);
        let pb = Playbook::parse("[crew]\nworkers = 2\nagents = [\"codex\"]\nquiet_hours = \"23:00-07:00\"\n[crew.commands]\ncodex = [\"x\", \"{worktree}\"]\n").unwrap();
        assert!(pb.unknown.is_empty(), "{:?}", pb.unknown);
        assert_eq!((pb.crew.workers, pb.crew.max_per_day(), pb.crew.attempts), (2, DEFAULT_MAX_PER_DAY, 3));
        assert_eq!(pb.crew.command_for("codex").unwrap(), ["x", "{worktree}"]);
        assert_eq!(pb.crew.command_for("claude").unwrap()[0], "claude", "unreplaced agents keep the default");
        assert!(pb.crew.command_for("cursor").is_none());
        let codex = CrewRules::default().command_for("codex").unwrap();
        assert!(codex.iter().any(|a| a.contains("default_tools_approval_mode=\"approve\"")), "{codex:?}");
        assert!(!codex.iter().any(|a| a == "workspace-write"), "a codex worker must be able to commit: {codex:?}");
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let at = |h: u32, m: u32| h * 60 + m;
        assert_eq!(quiet_until("23:00-07:00", at(23, 30)).unwrap().as_deref(), Some("07:00"));
        assert_eq!(quiet_until("23:00-07:00", at(3, 0)).unwrap().as_deref(), Some("07:00"));
        assert_eq!(quiet_until("23:00-07:00", at(7, 0)).unwrap(), None);
        assert_eq!(quiet_until("09:00-17:30", at(17, 29)).unwrap().as_deref(), Some("17:30"));
        assert_eq!(quiet_until("09:00-17:30", at(8, 59)).unwrap(), None);
        assert!(quiet_until("late", 0).is_err());
        assert!(quiet_until("25:00-07:00", 0).is_err());
    }

    #[test]
    fn a_planning_issue_gets_the_planners_brief() {
        let issue = IssueRecord {
            key: "NIT-9".into(),
            title: "Plan a goal: make Nitely installable".into(),
            labels: vec!["goal".into(), "planning".into()],
            ..Default::default()
        };
        let b = brief(&issue, "NIT", Path::new("/w/nit-9"), "claude/nit-9-plan", Path::new("/r"), None);
        for part in ["plan a goal, NIT-9", "propose_goal, passing planning_issue NIT-9", "files it touches", "change nothing in it", "Do not commit, submit"] {
            assert!(b.contains(part), "missing {part:?} in {b}");
        }
        assert!(!b.contains("submit (without wait)"));
    }

    #[test]
    fn the_brief_names_the_issue_the_worktree_and_the_rules() {
        let issue = IssueRecord { key: "NIT-7".into(), title: "Fix the thing".into(), ..Default::default() };
        let b = brief(&issue, "NIT", Path::new("/w/nit-7"), "codex/nit-7-fix", Path::new("/r"), Some(5191));
        for part in ["NIT-7, \"Fix the thing\"", "/w/nit-7 (branch codex/nit-7-fix)", "never edit or commit in /r", "submit (without wait)", "lands it when it passes", "handoff: true", "Do not take another issue", "use port 5191"] {
            assert!(b.contains(part), "{part} missing from:\n{b}");
        }
    }

    /// A fake agent: records launches; each process exits when the test says so.
    #[derive(Clone, Default)]
    struct Fake {
        installed: Vec<&'static str>,
        launches: Rc<RefCell<Vec<Launch>>>,
        /// Exit codes by pid; a pid not here is still running.
        exits: Rc<RefCell<BTreeMap<u32, i32>>>,
        stopped: Rc<RefCell<Vec<u32>>>,
        /// Starting fails with this.
        fail_start: Option<&'static str>,
        /// Where `prepare` finds the database.
        db: Option<PathBuf>,
        /// Issues whose setup was started, in order.
        prepares: Rc<RefCell<Vec<String>>>,
        /// Setups never finish.
        hang_setup: bool,
        /// Worktrees whose removal was started.
        cleanups: Rc<RefCell<Vec<PathBuf>>>,
        /// Removals never finish.
        hang_cleanup: bool,
        /// Issues whose land was started, in order; what each land does to the issue.
        lands: Rc<RefCell<Vec<String>>>,
        land_does: Option<&'static str>,
        /// The machine is short of memory: for beginning a worker, and for launching one.
        short_to_begin: Rc<std::cell::Cell<bool>>,
        short_to_launch: Rc<std::cell::Cell<bool>>,
        /// Submit jobs started again.
        resubmits: Rc<RefCell<Vec<i64>>>,
    }

    struct FakeProcess {
        pid: u32,
        exits: Rc<RefCell<BTreeMap<u32, i32>>>,
        stopped: Rc<RefCell<Vec<u32>>>,
    }

    impl Process for FakeProcess {
        fn pid(&self) -> u32 {
            self.pid
        }
        fn exited(&mut self) -> Result<Option<i32>> {
            Ok(self.exits.borrow().get(&self.pid).copied())
        }
        fn stop(&mut self) {
            self.stopped.borrow_mut().push(self.pid);
        }
    }

    impl Agents for Fake {
        fn resolve(&self, program: &str) -> Option<String> {
            self.installed.contains(&program).then(|| format!("/bin/{program}"))
        }
        fn memory_wait(&self) -> Option<String> {
            self.short_to_begin.get().then(|| "waiting for memory: 5.0 GB free, new work starts above 12.8 GB".into())
        }
        fn memory_turn(&self) -> Option<String> {
            self.short_to_launch.get().then(|| "waiting for memory: the last run to start is still growing".into())
        }
        fn start(&self, launch: &Launch) -> Result<Box<dyn Process>> {
            if let Some(why) = self.fail_start {
                bail!("{why}");
            }
            self.launches.borrow_mut().push(launch.clone());
            let pid = 1000 + self.launches.borrow().len() as u32;
            Ok(Box::new(FakeProcess { pid, exits: self.exits.clone(), stopped: self.stopped.clone() }))
        }
        fn adopt(&self, pid: u32) -> Option<Box<dyn Process>> {
            let alive = (1001..=1000 + self.launches.borrow().len() as u32).contains(&pid) && !self.exits.borrow().contains_key(&pid);
            alive.then(|| Box::new(FakeProcess { pid, exits: self.exits.clone(), stopped: self.stopped.clone() }) as Box<dyn Process>)
        }
        /// Removes the worktree here and now, as `nitely crew clean-up` would, unless removals hang.
        fn clean_up(&self, job: &CleanUp) -> Result<Box<dyn Process>> {
            let pid = 7000 + self.cleanups.borrow().len() as u32;
            self.cleanups.borrow_mut().push(job.worktree.clone());
            if !self.hang_cleanup {
                let db = Db::open(self.db.as_ref().expect("the fixture sets the database")).unwrap();
                assert!(!clean_up_and_report(&db, job)?.1);
                self.exits.borrow_mut().insert(pid, 0);
            }
            Ok(Box::new(FakeProcess { pid, exits: self.exits.clone(), stopped: self.stopped.clone() }))
        }
        /// Lands as the test says: `landed` (Done), `conflict` (a land that stopped on the change),
        /// `busy` (another land held the lock).
        fn land(&self, job: &LandJob) -> Result<Box<dyn Process>> {
            let pid = 9000 + self.lands.borrow().len() as u32;
            self.lands.borrow_mut().push(job.issue.clone());
            let mut db = Db::open(self.db.as_ref().expect("the fixture sets the database")).unwrap();
            let cli = Actor::Agent("cli".into());
            match self.land_does.unwrap_or("landed") {
                "landed" => {
                    db.move_issue(&job.issue, "Done", Some("landed"), None, &cli)?;
                }
                "conflict" => db.log_event(&cli, Some(&job.issue), None, "land_failed", "Land stopped: the rebase stopped on conflicts in a.txt")?,
                "unsigned" => db.log_event(&cli, Some(&job.issue), None, "land_failed", &format!("Land stopped: the approval {}", crate::approval::UNSIGNED))?,
                "human" => db.log_event(&cli, Some(&job.issue), None, "land_failed", "Land stopped: this change waits for a human (billing); it lands once they approve it in Nitely, after the review")?,
                _ => db.log_event(&cli, Some(&job.issue), None, "land_failed", "Land stopped: another land holds the lock")?,
            }
            self.exits.borrow_mut().insert(pid, 0);
            Ok(Box::new(FakeProcess { pid, exits: self.exits.clone(), stopped: self.stopped.clone() }))
        }
        fn run_submit_job(&self, _nitely: &str, job: &crate::submit_job::Job) -> Result<Option<u32>> {
            self.resubmits.borrow_mut().push(job.id);
            // No process to record: a made-up pid would read as a job whose process died.
            Ok(None)
        }
        /// Runs start_task here and now, as `nitely start` would, unless setups hang.
        fn prepare(&self, prep: &Prepare) -> Result<Box<dyn Process>> {
            let pid = 5000 + self.prepares.borrow().len() as u32;
            self.prepares.borrow_mut().push(prep.key.clone());
            if !self.hang_setup {
                let db = Db::open(self.db.as_ref().expect("the fixture sets the database")).unwrap();
                let issue = db.issue(&prep.key).unwrap().unwrap();
                let started = playbook::start_task(&db, &prep.repo, &issue, &prep.vendor, &Actor::Agent("cli".into()))?;
                self.exits.borrow_mut().insert(pid, if started.setup.iter().all(|r| r.ok) { 0 } else { 1 });
            }
            Ok(Box::new(FakeProcess { pid, exits: self.exits.clone(), stopped: self.stopped.clone() }))
        }
    }

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        db: Db,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    fn fixture(crew: &str) -> Fixture {
        let base = crate::test_repo::copy("crew", |base| crate::test_repo::init(&base.join("repo"), &[]));
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), format!("[worktree]\ndir = \"../wt/{{key}}\"\n{crew}")).unwrap();
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        for (title, priority) in [("First", 1), ("Second", 2), ("Third", 3)] {
            let new = NewIssue { project: "NIT".into(), title: title.into(), body: "wanted".into(), kind: "bug".into(), source: "human".into(), evidence: "seen".into(), priority, ..Default::default() };
            db.create_issue(&new, &Actor::Human).unwrap();
        }
        Fixture { base, repo, db }
    }

    const NOON: Moment = Moment { ts: 1_000_000, minute_of_day: 12 * 60, day_start: 1_000_000 - 12 * 3600 };

    fn supervisor(f: &Fixture, fake: &Fake) -> Supervisor {
        Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(Fake { db: Some(f.base.join("t.db")), ..fake.clone() })).with_clock(|| NOON).with_tokens(|_, _| 0)
    }

    const GATE: &str = "[gate]\ncommands = [\"echo ok\"]\n";

    #[test]
    fn keeps_the_cap_and_starts_the_next_when_a_slot_frees() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\n"));
        let fake = Fake { installed: vec!["claude", "codex"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);

        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1", "NIT-2"], "{tick:?}");
        assert_eq!((tick.running, tick.cap, tick.status.as_str()), (2, 2, "working"));
        {
            let launches = fake.launches.borrow();
            let first = &launches[0];
            assert_eq!(first.agent, "claude", "the first installed agent in preference order");
            assert_eq!(first.program, "/bin/claude");
            assert!(first.cwd.join(".git").exists() && first.cwd.ends_with("nit-1"), "start_task made the worktree: {:?}", first.cwd);
            assert!(first.brief.contains("NIT-1") && first.brief.contains(&first.cwd.display().to_string()));
            let mcp = first.args.iter().skip_while(|a| *a != "--mcp-config").nth(1).unwrap();
            assert!(std::fs::read_to_string(mcp).unwrap().contains("\"nitely\""), "the MCP config names the nitely server");
            assert!(first.env.contains(&("NITELY_PROJECT".into(), "NIT".into())));
        }
        // The claim is under the worker's own agent id, the one its MCP server will use.
        let claimed = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        assert!(claimed.starts_with("claude:") && claimed.ends_with("/nit-1"), "{claimed}");
        let timeline: Vec<String> = f.db.timeline("NIT-1").unwrap().into_iter().map(|t| t.kind).collect();
        assert!(timeline.contains(&"crew_started".to_string()), "{timeline:?}");

        // Full: nothing more starts.
        assert!(sup.tick(&mut f.db, false).unwrap().started.is_empty());

        // The first worker submits and exits: its slot goes to the next issue.
        f.db.move_issue("NIT-1", "In Review", None, None, &Actor::Agent(claimed)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended, [WorkerLine { issue: "NIT-1".into(), agent: "claude".into(), outcome: "submitted".into(), log: tick.ended[0].log.clone() }]);
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-3"]);

        // The second worker reports, then exits with NIT-2 still In Progress: a failed attempt. The
        // supervisor writes the handoff note it did not leave, and NIT-2 goes to the next worker.
        let two = f.db.issue("NIT-2").unwrap().unwrap().claimed_by.unwrap();
        f.db.add_note("NIT-2", "running the tests", "status", &Actor::Agent(two)).unwrap();
        fake.exits.borrow_mut().insert(1002, 1);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "failed", "exited with its issue still In Progress and claimed");
        let handoff = f.db.timeline("NIT-2").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap();
        assert!(handoff.summary.as_deref().unwrap().contains("its last report: \"running the tests\""), "{handoff:?}");
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-2"], "a second attempt");
        let status = status(&f.db, "NIT", &Playbook::load(&f.repo).unwrap(), &NOON, 0).unwrap();
        assert_eq!((status.running.len(), status.started_today, status.cap), (2, 4, 2));
        assert!(status.supervised);
    }

    /// No worker is begun while the machine is short of memory, and one whose setup finished
    /// launches only once the machine has memory for it (NIT-266).
    #[test]
    fn workers_wait_for_memory_to_begin_and_to_launch() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 1
"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        fake.short_to_begin.set(true);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.status.starts_with("waiting for memory"), "{tick:?}");
        assert!(fake.prepares.borrow().is_empty(), "no worktree set up");

        // Memory to begin, but not yet to launch: the setup runs, the worker waits ready.
        fake.short_to_begin.set(false);
        fake.short_to_launch.set(true);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.prepares.borrow(), ["NIT-1"]);
        assert!(tick.started.is_empty() && fake.launches.borrow().is_empty(), "{tick:?}");
        assert!(sup.tick(&mut f.db, false).unwrap().started.is_empty(), "still waiting");

        fake.short_to_launch.set(false);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1"], "{tick:?}");
        assert_eq!(fake.prepares.borrow().len(), 1, "set up once");
    }

    /// A submit job for `key` that its worker queued, as `submit` (without wait) records one.
    fn handed_in(f: &Fixture, key: &str, state: &str, result: Option<&str>) -> i64 {
        let by = f.db.issue(key).unwrap().unwrap().claimed_by.unwrap();
        f.db.conn
            .execute(
                "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at, result)
                 VALUES (?1, 'w', 'b', 'h', ?2, 0, 0, ?3, ?4, ?5)",
                rusqlite::params![key, by, state, NOON.ts + 1, result],
            )
            .unwrap();
        f.db.conn.last_insert_rowid()
    }

    #[test]
    fn a_worker_that_handed_in_and_left_is_no_failure_and_what_comes_back_goes_to_a_new_worker() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let claimed = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();

        // It submits without waiting and leaves while the gate and review run.
        let job = handed_in(&f, "NIT-1", "queued", None);
        fake.exits.borrow_mut().insert(1001, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "submitted", "{tick:?}");
        let one = f.db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((one.state.as_str(), one.claimed_by.as_deref()), ("In Progress", Some(claimed.as_str())), "the claim stays");
        assert!(!f.db.timeline("NIT-1").unwrap().iter().any(|t| t.kind == "handoff" && t.actor.starts_with("crew:")), "no failure note");
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-2"], "its slot went to the next issue");

        // The review sends it back while NIT-2's worker runs: it waits for a free slot as Todo,
        // with the outcome in a handoff note.
        f.db.conn
            .execute("UPDATE submit_job SET state = 'done', ended_at = ?2, result = ?3 WHERE id = ?1", rusqlite::params![job, NOON.ts + 5, r#"{"state":"In Progress","next":"Fix the P1."}"#])
            .unwrap();
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Todo");
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap().summary.unwrap_or_default();
        assert!(note.contains(&format!("job {job}")) && note.contains("came back") && note.contains("Fix the P1."), "{note}");
        assert!(f.db.submit_job(job).unwrap().unwrap().delivered_at.is_some(), "told, so no notice goes to the worker that left");

        // NIT-2's worker hands in too; NIT-1 goes to a new worker, in the same worktree.
        let two = f.db.issue("NIT-2").unwrap().unwrap().claimed_by.unwrap();
        f.db.move_issue("NIT-2", "In Review", None, None, &Actor::Agent(two)).unwrap();
        fake.exits.borrow_mut().insert(1002, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1"], "{tick:?}");
        let launches = fake.launches.borrow();
        assert_eq!(launches[2].cwd, launches[0].cwd);
        assert!(launches[2].brief.contains("submit (without wait)"));
    }

    #[test]
    fn a_handed_in_change_that_passed_is_landed_by_the_supervisor() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let claimed = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        handed_in(&f, "NIT-1", "done", Some(r#"{"state":"In Review","next":"Call land."}"#));
        f.db.move_issue("NIT-1", "In Review", None, None, &Actor::Agent(claimed)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.lands.borrow(), ["NIT-1"]);
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Done");
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(fake.lands.borrow().len(), 1, "landed once");

        // Where Nitely lands, an issue still In Review is one whose own land found the lock held:
        // landed here too.
        let mut g = fixture(&format!("{GATE}[crew]\nworkers = 1\n[land]\nby = \"auto\"\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&g, &fake);
        sup.tick(&mut g.db, false).unwrap();
        let claimed = g.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        handed_in(&g, "NIT-1", "done", Some(r#"{"state":"In Review","next":"x"}"#));
        g.db.move_issue("NIT-1", "In Review", None, None, &Actor::Agent(claimed)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        sup.tick(&mut g.db, false).unwrap();
        assert_eq!(*fake.lands.borrow(), ["NIT-1"]);

        // A submit that waits for a person when the worker is seen to end: the worker `asked`;
        // once they approve, it is landed.
        let mut h = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&h, &fake);
        sup.tick(&mut h.db, false).unwrap();
        let claimed = h.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        handed_in(&h, "NIT-1", "done", Some(r#"{"state":"Needs You","next":"x"}"#));
        h.db.move_issue("NIT-1", "Needs You", None, None, &Actor::Agent(claimed)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        assert_eq!(sup.tick(&mut h.db, false).unwrap().ended[0].outcome, "asked");
        assert!(fake.lands.borrow().is_empty(), "nothing to land while it waits for the person");
        h.db.move_issue("NIT-1", "Approved", None, None, &Actor::Human).unwrap();
        sup.tick(&mut h.db, false).unwrap();
        // Where only the app can verify an approval (macOS), landing it is the app's.
        let want: &[&str] = if crate::approval::agents_can_verify() { &["NIT-1"] } else { &[] };
        assert_eq!(*fake.lands.borrow(), want);
    }

    #[test]
    fn an_approved_change_whose_land_stopped_elsewhere_goes_to_a_new_worker() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let claimed = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        let job = handed_in(&f, "NIT-1", "done", Some(r#"{"state":"Needs You","next":"x"}"#));
        f.db.conn.execute("UPDATE submit_job SET ended_at = ?2 WHERE id = ?1", rusqlite::params![job, NOON.ts + 2]).unwrap();
        f.db.move_issue("NIT-1", "Needs You", None, None, &Actor::Agent(claimed)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        sup.tick(&mut f.db, false).unwrap();
        // The app lands it once approved (its own lander, not the supervisor's), and that land stops
        // on a conflict: the issue is back In Progress with nobody's claim.
        let app = Actor::Agent(crate::land::AUTO_LANDER.into());
        f.db.log_event(&app, Some("NIT-1"), None, "land_failed", "Land stopped: the rebase stopped on conflicts in a.txt").unwrap();
        f.db.move_issue("NIT-1", "In Progress", None, None, &app).unwrap();
        f.db.conn.execute("UPDATE issue SET claimed_by = NULL WHERE key = 'NIT-1'", []).unwrap();
        sup.tick(&mut f.db, false).unwrap();
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap().summary.unwrap_or_default();
        assert!(note.contains("its land stopped"), "{note}");
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Todo", "for the next free slot");
    }

    #[test]
    fn a_worktree_left_mid_rebase_is_still_its_branchs() {
        let f = fixture("");
        let wt = f.base.join("wt").join("nit-1");
        crate::test_repo::git(&f.repo, &["worktree", "add", "-q", "-b", "claude/nit-1-first", &wt.to_string_lossy()]);
        std::fs::write(wt.join("a.txt"), "the branch's\n").unwrap();
        crate::test_repo::git(&wt, &["add", "."]);
        crate::test_repo::git(&wt, &["commit", "-q", "-m", "branch"]);
        std::fs::write(f.repo.join("a.txt"), "main's\n").unwrap();
        crate::test_repo::git(&f.repo, &["add", "a.txt"]);
        crate::test_repo::git(&f.repo, &["commit", "-q", "-m", "main"]);
        // A land's rebase that stopped on the conflict, left for the next worker to resolve.
        assert!(playbook::git(&wt, &["rebase", "main"]).is_err());
        assert_eq!(current_branch(&wt).as_deref(), Some("claude/nit-1-first"));
        assert_eq!(other_branch(&wt, "claude/nit-1-first", "claude", "NIT-1"), None);
        assert!(other_branch(&wt, "codex/nit-1-first", "codex", "NIT-1").is_some());
    }

    #[test]
    fn a_land_that_stops_on_the_change_goes_to_a_new_worker_and_a_held_lock_waits() {
        for (does, state) in [("conflict", "Todo"), ("busy", "In Review"), ("unsigned", "In Review"), ("human", "Needs You")] {
            let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
            let fake = Fake { installed: vec!["claude"], land_does: Some(does), ..Default::default() };
            let mut sup = supervisor(&f, &fake);
            sup.tick(&mut f.db, false).unwrap();
            let claimed = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
            handed_in(&f, "NIT-1", "done", Some(r#"{"state":"In Review","next":"Call land."}"#));
            f.db.move_issue("NIT-1", "In Review", None, None, &Actor::Agent(claimed)).unwrap();
            fake.exits.borrow_mut().insert(1001, 0);
            sup.tick(&mut f.db, false).unwrap();
            sup.tick(&mut f.db, false).unwrap();
            assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, state, "{does}");
            assert_eq!(fake.lands.borrow().len(), 1, "{does}: tried again a minute later, not every tick");
            if does == "conflict" {
                let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap().summary.unwrap_or_default();
                assert!(note.contains("The land stopped: the rebase stopped on conflicts in a.txt"), "{note}");
            }
        }
    }

    /// A clock the test moves: real time ahead, so the events a test writes (at real now) are older.
    fn moving_clock(sup: Supervisor) -> (Supervisor, Rc<std::cell::Cell<i64>>) {
        let at = Rc::new(std::cell::Cell::new(crate::db::now() + 10_000));
        let clock = at.clone();
        let sup = sup.with_clock(move || Moment { ts: clock.get(), minute_of_day: 12 * 60, day_start: clock.get() - 12 * 3600 });
        (sup, at)
    }

    #[test]
    fn a_worker_silent_past_idle_minutes_is_stopped_as_hung_and_its_issue_handed_on() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\nidle_minutes = 20\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let (mut sup, at) = moving_clock(supervisor(&f, &fake));
        sup.tick(&mut f.db, false).unwrap();
        // Quiet for 19 minutes: still working.
        at.set(at.get() + 19 * 60);
        assert!(sup.tick(&mut f.db, false).unwrap().ended.is_empty());
        // Output in its log (read through a handle, not the directory entry) keeps it alive.
        let log = fake.launches.borrow()[0].log.clone();
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "still building").unwrap();
        at.set(at.get() + 19 * 60);
        assert!(sup.tick(&mut f.db, false).unwrap().ended.is_empty(), "its log grew since the last look");
        // Silent for 21 minutes after the supervisor saw it last grow: hung.
        at.set(at.get() + 21 * 60);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "failed", "{tick:?}");
        assert!(fake.stopped.borrow().contains(&1001), "stopped with what it started");
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap().summary.unwrap_or_default();
        assert!(note.contains("no output and no report") && note.contains("hung"), "{note}");
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1"], "handed to a new worker at once");
    }

    #[test]
    fn after_a_restart_the_handoff_says_what_the_worktree_holds() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let wt = fake.launches.borrow()[0].cwd.clone();
        std::fs::write(wt.join("half-done.txt"), "x").unwrap();
        drop(sup);
        // The machine restarts: the worker is gone, and a new supervisor looks.
        fake.exits.borrow_mut().insert(1001, -1);
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "failed", "{tick:?}");
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap().summary.unwrap_or_default();
        assert!(note.contains("1 uncommitted file there"), "{note}");
        assert!(wt.join("half-done.txt").exists(), "kept as it is");
    }

    #[test]
    fn a_submit_cut_short_by_a_restart_runs_again_once_while_the_branch_is_unchanged() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let wt = fake.launches.borrow()[0].cwd.clone();
        let by = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        let head = playbook::git(&wt, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let branch = playbook::git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap().trim().to_string();
        let died = |f: &Fixture| {
            f.db.conn
                .execute(
                    "INSERT INTO submit_job (issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at, ended_at, error)
                     VALUES ('NIT-1', ?1, ?2, ?3, ?4, 0, 0, 'failed', ?5, ?5, ?6)",
                    rusqlite::params![wt.to_string_lossy(), branch, head, by, NOON.ts + 1, format!("{} (the machine restarted, or it was stopped); submit again", crate::submit_job::ABANDONED)],
                )
                .unwrap();
        };
        died(&f);
        fake.exits.borrow_mut().insert(1001, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "submitted", "it had handed in: {tick:?}");
        let again = f.db.latest_submit_job("NIT-1").unwrap().unwrap();
        assert_eq!((again.state.as_str(), again.requested_by.as_str(), again.head.as_str()), ("queued", by.as_str(), head.as_str()));
        assert_eq!(*fake.resubmits.borrow(), [again.id]);
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "In Progress", "still the worker's, waiting on the submit");

        // It dies again at the same commit: no third try, a new worker takes it.
        f.db.conn.execute("DELETE FROM submit_job WHERE id = ?1", [again.id]).unwrap();
        died(&f);
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(fake.resubmits.borrow().len(), 1);
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Todo");
    }

    #[test]
    fn a_worker_whose_setup_finished_waits_while_the_disk_is_short() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 1
"));
        let fake = Fake { installed: vec!["claude"], hang_setup: true, ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(sup.setting_up(), 1);
        // Its setup ends while the disk has filled up: it does not launch, and waits.
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), format!("[worktree]
dir = \"../wt/{{key}}\"
{GATE}[crew]
workers = 1
min_free_disk_gb = 100000000
")).unwrap();
        fake.exits.borrow_mut().insert(5000, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.started.is_empty() && fake.launches.borrow().is_empty(), "{tick:?}");
        assert_eq!(sup.setting_up(), 1, "still waiting, its setup done");
    }

    #[test]
    fn a_land_lock_left_by_a_land_that_died_is_removed_and_the_machine_guard_holds_new_workers() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\nmin_free_disk_gb = 100000000\n"));
        let lock = crate::land::lock_path(&f.repo).unwrap();
        std::fs::write(&lock, "NIT-2 pid 999999 since 2026-10-16T23:27:00Z\n").unwrap();
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(!lock.exists(), "nothing would land again until a person broke it");
        assert!(f.db.timeline("NIT-2").unwrap().iter().any(|t| t.summary.as_deref().is_some_and(|s| s.contains("Removed the land lock"))));
        assert!(tick.started.is_empty() && tick.status.starts_with("disk low:"), "{tick:?}");
        // A live land's lock stays.
        std::fs::write(&lock, format!("NIT-2 pid {} since now\n", std::process::id())).unwrap();
        assert_eq!(crate::land::clear_dead_lock(&f.repo), None);
        assert!(lock.exists());
    }

    #[test]
    fn approved_goals_are_worked_in_the_persons_order_within_each_ones_budget() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 4\ngoal_workers = 1\n"));
        let human = Actor::Human;
        // Two more issues, less urgent than NIT-2 and NIT-3.
        for title in ["Goal work, first", "Goal work, second"] {
            let new = NewIssue { project: "NIT".into(), title: title.into(), body: "wanted".into(), kind: "feature".into(), source: "human".into(), evidence: "seen".into(), priority: 4, ..Default::default() };
            f.db.create_issue(&new, &human).unwrap();
        }
        let goal = |title: &str, rank: i64| {
            let change = crate::goals::GoalChange { title: Some(title.into()), outcome: Some("o".into()), rank: Some(rank), ..Default::default() };
            f.db.save_goal("NIT", None, &change, &human).unwrap().id
        };
        let (first, second) = (goal("Ranked first", 1), goal("Ranked second", 2));
        // The plan's order is the order its issues were filed: NIT-4, then NIT-5.
        f.db.set_issue_goal("NIT-5", Some(first), &human).unwrap();
        f.db.set_issue_goal("NIT-4", Some(first), &human).unwrap();
        f.db.set_issue_goal("NIT-3", Some(second), &human).unwrap();

        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        let started: Vec<&str> = tick.started.iter().map(|w| w.issue.as_str()).collect();
        // Priority 1 first wherever it is; then the first goal's next issue though it is the least
        // urgent; then, that goal at its budget of one, the second goal; then work in no goal.
        assert_eq!(started, ["NIT-1", "NIT-4", "NIT-3", "NIT-2"], "{tick:?}");
        let goals = f.db.goals("NIT", false).unwrap();
        assert_eq!(goals.iter().map(|g| (g.id, g.working)).collect::<Vec<_>>(), [(first, 1), (second, 1)], "the rows say who works on each");

        // NIT-4's worker hands it in and leaves: its goal has room again, and NIT-5 is next.
        let four = f.db.issue("NIT-4").unwrap().unwrap().claimed_by.unwrap();
        f.db.move_issue("NIT-4", "In Review", None, None, &Actor::Agent(four)).unwrap();
        fake.exits.borrow_mut().insert(1002, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-5"], "{tick:?}");

        // Each worker previews on its own port.
        let ports: Vec<String> = fake.launches.borrow().iter().filter_map(|l| l.env.iter().find(|(k, _)| k == "NITELY_PREVIEW_PORT").map(|(_, v)| v.clone())).collect();
        let distinct: std::collections::BTreeSet<&String> = ports.iter().collect();
        assert!(ports.len() >= 4 && distinct.len() == ports.len() - 1, "one port per worker running at once, one freed and given again: {ports:?}");
    }

    #[test]
    fn a_goal_with_nothing_ready_yields_its_slots_and_a_goals_own_budget_wins() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\ngoal_workers = 1\n"));
        let human = Actor::Human;
        let change = |title: &str, rank: i64, workers: Option<i64>| crate::goals::GoalChange { title: Some(title.into()), outcome: Some("o".into()), rank: Some(rank), workers, ..Default::default() };
        // The first goal's only issue is blocked; the second may take two workers at once.
        let blocked = f.db.save_goal("NIT", None, &change("Blocked", 1, None), &human).unwrap().id;
        let wide = f.db.save_goal("NIT", None, &change("Wide", 2, Some(2)), &human).unwrap().id;
        f.db.set_issue_goal("NIT-1", Some(blocked), &human).unwrap();
        f.db.add_blocker("NIT-1", "NIT-3", &human).unwrap();
        f.db.set_issue_goal("NIT-2", Some(wide), &human).unwrap();
        f.db.set_issue_goal("NIT-3", Some(wide), &human).unwrap();
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        let mut started: Vec<&str> = tick.started.iter().map(|w| w.issue.as_str()).collect();
        started.sort();
        assert_eq!(started, ["NIT-2", "NIT-3"], "{tick:?}");
    }

    #[test]
    fn project_focus_limits_crew_and_external_claims_without_interrupting_workers() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 4\ngoal_workers = 1\n"));
        let human = Actor::Human;
        let goal = f.db.save_goal("NIT", None, &crate::goals::GoalChange {
            title: Some("Focused feature".into()), outcome: Some("Ship the feature".into()), ..Default::default()
        }, &human).unwrap().id;
        f.db.set_issue_goal("NIT-3", Some(goal), &human).unwrap();
        let extra = f.db.create_issue(&NewIssue {
            project: "NIT".into(), title: "More feature work".into(), body: "wanted".into(), kind: "feature".into(),
            source: "human".into(), evidence: "seen".into(), priority: 4, goal: Some(goal), ..Default::default()
        }, &human).unwrap();
        f.db.set_project_focus("NIT", Some(goal), false, &human).unwrap();
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-3"]);
        assert!(tick.status.contains("Waiting on focus"), "{tick:?}");
        assert_eq!(f.db.issue(&extra.key).unwrap().unwrap().state, "Todo", "default goal budget still applies");
        // An external allocator using the same budget also waits on the Crew worker's slot.
        assert!(f.db.next_task_choosing_with_goal_budget("NIT", "codex:external", &[], 1, |c| c.first().map(|x| x.0.clone())).unwrap().is_none());

        f.db.set_project_focus("NIT", Some(goal), true, &human).unwrap();
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1"]);
        assert_eq!(sup.running(), 2, "changing focus leaves existing workers running");
        f.db.save_goal("NIT", Some(goal), &crate::goals::GoalChange { state: Some("done".into()), ..Default::default() }, &human).unwrap();
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.started.is_empty());
        assert!(tick.status.contains("no longer active"));
        assert_eq!(sup.running(), 2);
        f.db.set_project_focus("NIT", None, true, &human).unwrap();
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.started.iter().any(|w| w.issue == "NIT-2"), "clearing focus restores normal allocation");
    }

    #[test]
    fn starts_nothing_while_off_paused_ungated_quiet_or_capped() {
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let check = |crew: &str, setup: &dyn Fn(&Db), want: &str| {
            let mut f = fixture(crew);
            setup(&f.db);
            let mut sup = supervisor(&f, &fake);
            let tick = sup.tick(&mut f.db, false).unwrap();
            assert!(tick.started.is_empty(), "{crew}: {tick:?}");
            assert!(tick.status.starts_with(want), "{crew}: {}", tick.status);
            assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Todo", "{crew}: nothing claimed");
        };
        let none = |_: &Db| {};
        check(GATE, &none, "off");
        check("[crew]\nworkers = 1\n", &none, "no gate");
        check(&format!("{GATE}[crew]\nworkers = 1\n"), &|db: &Db| db.crew_pause("nit", true, &Actor::Human).unwrap(), "paused");
        check(&format!("{GATE}[crew]\nworkers = 1\nquiet_hours = \"11:00-13:00\"\n"), &none, "quiet hours until 13:00");
        check(&format!("{GATE}[crew]\nworkers = 1\nquiet_hours = \"noon\"\n"), &none, "the playbook's crew.quiet_hours");
        check(&format!("{GATE}[crew]\nworkers = 1\nmax_per_day = 0\n"), &none, "0 of 0 started today");
        check(&format!("{GATE}[crew]\nworkers = 1\nagents = [\"cursor\", \"codex\"]\n"), &none, "no agent to start");

        // Draining starts nothing either.
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let tick = supervisor(&f, &fake).tick(&mut f.db, true).unwrap();
        assert!(tick.started.is_empty() && tick.status.starts_with("stopping"));
    }

    #[test]
    fn the_daily_cap_counts_every_worker_started_today() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 3\nmax_per_day = 2\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.len(), 2);
        assert_eq!(tick.status, "2 of 2 started today");
    }

    #[test]
    fn the_daily_token_budget_stops_new_workers() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 2
max_tokens_per_day = 1000000
"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let used = std::rc::Rc::new(std::cell::Cell::new(999_999u64));
        let seen = used.clone();
        let mut sup = supervisor(&f, &fake).with_tokens(move |project, since| {
            assert_eq!((project, since), ("NIT", NOON.day_start), "today's tokens of this project");
            seen.get()
        });
        assert_eq!(sup.tick(&mut f.db, false).unwrap().started.len(), 2, "under budget, workers start");
        used.set(1_200_000);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.started.is_empty());
        assert_eq!(tick.status, "token budget used: 1.2M of 1.0M today");
    }

    #[test]
    fn a_second_attempt_keeps_the_first_attempts_agent() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        // NIT-1 already has a codex branch from an earlier attempt.
        let git = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&f.repo).args(args)).unwrap().status.success());
        git(&["branch", "codex/nit-1-first"]);
        let fake = Fake { installed: vec!["claude", "codex"], ..Default::default() };
        let tick = supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started[0].agent, "codex");
        assert_eq!(fake.launches.borrow()[0].args.last().unwrap(), "-");
    }

    #[test]
    fn never_puts_an_agent_on_another_vendors_branch() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 3\n"));
        let git = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&f.repo).args(args)).unwrap().status.success(), "{args:?}");
        // NIT-1 has a claude branch, but only codex is installed.
        git(&["branch", "claude/nit-1-first"]);
        // NIT-2's worktree folder is on a branch a person started by hand.
        git(&["worktree", "add", "-q", "-b", "cli/nit-2-second", "../wt/nit-2"]);
        let fake = Fake { installed: vec!["codex"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-3"], "{tick:?}");
        assert!(tick.status.contains("NIT-1: its branch is claude's") && tick.status.contains("NIT-2: its branch is cli's"), "{}", tick.status);
        for key in ["NIT-1", "NIT-2"] {
            let i = f.db.issue(key).unwrap().unwrap();
            assert_eq!((i.state.as_str(), i.claimed_by), ("Todo", None), "{key} is handed back");
        }
        assert!(sup.tick(&mut f.db, false).unwrap().started.is_empty(), "and passed over for the rest of the run");
    }

    #[test]
    fn an_interrupt_stops_starting_between_workers() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 3\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let launches = fake.launches.clone();
        let mut sup = supervisor(&f, &fake).with_interrupt(move || !launches.borrow().is_empty());
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started.len(), 1);
        assert!(tick.status.starts_with("stopping"));
    }

    #[test]
    fn stopping_spares_a_landing_worker_and_returns_the_others_to_todo() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 3\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        // NIT-2's worker holds the land lock.
        std::fs::write(f.repo.join(".git").join(crate::land::LOCK_FILE), "NIT-2 pid 1 since now").unwrap();
        // NIT-3's worker had submitted: stopping it must not requeue reviewed work.
        let three = f.db.issue("NIT-3").unwrap().unwrap().claimed_by.unwrap();
        f.db.move_issue("NIT-3", "In Review", None, None, &Actor::Agent(three)).unwrap();
        let landing = sup.stop_all(&mut f.db).unwrap();
        assert_eq!(landing, ["NIT-2"]);
        assert_eq!(*fake.stopped.borrow(), [1001, 1003]);
        assert_eq!(f.db.issue("NIT-3").unwrap().unwrap().state, "In Review");
        let one = f.db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((one.state.as_str(), one.claimed_by), ("Todo", None));
        let kinds: Vec<String> = f.db.timeline("NIT-1").unwrap().into_iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&"handoff".to_string()) && kinds.contains(&"crew_ended".to_string()), "{kinds:?}");
        assert_eq!(sup.running(), 1);
    }

    #[test]
    fn a_failed_setup_goes_to_a_human_and_an_agent_that_will_not_start_is_dropped() {
        let mut f = fixture(&format!("setup = [\"exit 1\"]
{GATE}[crew]
workers = 1
"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let tick = supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        assert!(tick.started.is_empty() && fake.launches.borrow().is_empty(), "{tick:?}");
        // Every issue's setup fails the same way: each goes to Needs You once, nothing loops.
        let states: Vec<String> = (1..=3).map(|n| f.db.issue(&format!("NIT-{n}")).unwrap().unwrap().state).collect();
        assert_eq!(states, ["Needs You", "Needs You", "Needs You"]);
        assert_eq!(tick.status, "nothing ready");

        let mut f = fixture(&format!("{GATE}[crew]
workers = 2
"));
        let fake = Fake { installed: vec!["claude"], fail_start: Some("Failed to authenticate"), ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.status, "paused: no agent will start (claude: Failed to authenticate)");
        assert!(f.db.crew_control("NIT").unwrap().paused, "the crew paused itself");
        let one = f.db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((one.state.as_str(), one.claimed_by), ("Todo", None), "the issue goes back");
        assert_eq!(f.db.issue("NIT-2").unwrap().unwrap().state, "Todo", "and no other issue was taken");
        assert_eq!(f.db.crew_workers("NIT", 5).unwrap()[0].outcome.as_deref(), Some("launch_failed"));
        assert_eq!(sup.tick(&mut f.db, false).unwrap().status, "paused");
    }

    #[test]
    fn a_new_supervisor_adopts_live_workers_and_settles_gone_ones() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 2
"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        // The first supervisor is gone; NIT-2's worker died with it, NIT-1's still runs.
        fake.exits.borrow_mut().insert(1002, 0);
        let mut next = supervisor(&f, &fake);
        let tick = next.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended.iter().map(|w| (w.issue.as_str(), w.outcome.as_str())).collect::<Vec<_>>(), [("NIT-2", "failed")]);
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-2"], "NIT-2 is back in Todo; NIT-1's live worker holds the other slot");
        assert_eq!(next.running(), 2);
        fake.exits.borrow_mut().insert(1001, 0);
        assert_eq!(next.tick(&mut f.db, false).unwrap().ended[0].issue, "NIT-1");
    }

    #[test]
    fn an_issue_that_keeps_failing_goes_to_a_person_and_try_again_brings_it_back() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\nattempts = 2\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        let fail = |f: &mut Fixture, sup: &mut Supervisor, pid: u32| {
            let agent = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
            f.db.add_note("NIT-1", "trying", "status", &Actor::Agent(agent)).unwrap();
            fake.exits.borrow_mut().insert(pid, 1);
            sup.tick(&mut f.db, false).unwrap()
        };
        sup.tick(&mut f.db, false).unwrap();
        let tick = fail(&mut f, &mut sup, 1001);
        assert_eq!(tick.started[0].issue, "NIT-1", "the first failure gives it to a new worker");
        let tick = fail(&mut f, &mut sup, 1002);
        assert_eq!(tick.ended[0].outcome, "failed");
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Needs You", "two failures: a person decides");
        assert_eq!(tick.started[0].issue, "NIT-2", "and the crew moves on");
        let ask = f.db.open_asks().unwrap().into_iter().find(|a| a.issue_key.as_deref() == Some("NIT-1")).unwrap();
        assert!(ask.question.contains("tried NIT-1 2 times") && ask.options[0] == TRY_AGAIN, "{ask:?}");

        // The person answers Try again: NIT-1 is back in Todo, its failures counted from now.
        f.db.answer_ask(ask.id, TRY_AGAIN, &Actor::Human).unwrap();
        fake.exits.borrow_mut().insert(1003, 0);
        f.db.move_issue("NIT-2", "In Review", None, None, &Actor::Agent(f.db.issue("NIT-2").unwrap().unwrap().claimed_by.unwrap())).unwrap();
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started[0].issue, "NIT-1", "{tick:?}");
        let tick = fail(&mut f, &mut sup, 1004);
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "In Progress", "one failure since the answer: tried again: {tick:?}");
    }

    #[test]
    fn an_agent_that_exits_at_once_twice_is_dropped_until_a_resume() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        fake.exits.borrow_mut().insert(1001, 1);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "launch_failed");
        assert_eq!(tick.started[0].issue, "NIT-1", "no attempt counted, and it is tried again");
        assert!(!f.db.timeline("NIT-1").unwrap().iter().any(|t| t.kind == "handoff"), "nothing to hand off");
        fake.exits.borrow_mut().insert(1002, 1);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.status.starts_with("paused: no agent will start (claude: exited 2 times right after starting"), "{}", tick.status);
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Todo");
        assert!(sup.tick(&mut f.db, false).unwrap().started.is_empty());
        // A person resumes: claude gets another chance, on the issues passed over too.
        sup.waiting.insert("NIT-2".into(), "passed over while claude would not start".into());
        f.db.crew_pause("NIT", false, &Actor::Human).unwrap();
        assert_eq!(sup.tick(&mut f.db, false).unwrap().started[0].issue, "NIT-1");
        assert!(sup.waiting.is_empty());
    }

    #[test]
    fn a_worker_past_the_timeout_is_stopped_unless_it_is_landing() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\ntimeout_minutes = 30\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let clock = Rc::new(std::cell::Cell::new(NOON));
        let c = clock.clone();
        let mut sup = Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(Fake { db: Some(f.base.join("t.db")), ..fake.clone() })).with_clock(move || c.get());
        sup.tick(&mut f.db, false).unwrap();
        std::fs::write(f.repo.join(".git").join(crate::land::LOCK_FILE), "NIT-2 pid 1 since now").unwrap();
        clock.set(Moment { ts: NOON.ts + 31 * 60, ..NOON });
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.stopped.borrow(), [1001], "NIT-2 is landing");
        assert_eq!(tick.ended.iter().map(|w| (w.issue.as_str(), w.outcome.as_str())).collect::<Vec<_>>(), [("NIT-1", "failed")]);
        let w = f.db.crew_workers_for("NIT-1").unwrap().into_iter().last().unwrap();
        assert!(w.reason.unwrap().contains("crew.timeout_minutes"));
    }

    #[test]
    fn sessions_in_a_crew_worktree_are_marked() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        let wt = fake.launches.borrow()[0].cwd.to_string_lossy().replace('/', "\\");
        let repo = f.repo.to_string_lossy().into_owned();
        let mut record = |id: &str, cwd: &str| {
            let e = crate::db::NewEvent { session_id: id.into(), vendor: "claude".into(), cwd: cwd.into(), kind: "session_start".into(), summary: None, state: "working".into() };
            f.db.record(&e).unwrap();
        };
        record("claude:worker", &format!("{wt}\\src"));
        record("claude:person", &repo);
        let marks: BTreeMap<String, bool> = f.db.sessions_since(0).unwrap().into_iter().map(|s| (s.id, s.crew)).collect();
        assert_eq!(marks, BTreeMap::from([("claude:person".to_string(), false), ("claude:worker".to_string(), true)]));
        assert!(f.db.session("claude:worker").unwrap().unwrap().crew);

        // A worker's reports go to the session named after its agent id, which may have started
        // before it, in an earlier attempt or a person's session: it counts while the worker runs.
        let agent_id = f.db.crew_running("NIT").unwrap()[0].agent_id.clone();
        let reused = crate::db::NewEvent { session_id: agent_id.clone(), vendor: "claude".into(), cwd: wt.clone(), kind: "status".into(), summary: None, state: "working".into() };
        f.db.record(&reused).unwrap();
        f.db.conn.execute("UPDATE session SET started_at = started_at - 86400 WHERE id = ?1", [&agent_id]).unwrap();
        assert!(f.db.session(&agent_id).unwrap().unwrap().crew);
        let json = serde_json::to_value(f.db.session("claude:person").unwrap().unwrap()).unwrap();
        assert!(json.get("crew").is_none(), "only crew sessions carry the field");

        // The worker exits without a session end of its own (as `codex exec` does): its sessions
        // end with it, the person's does not (NIT-189).
        f.db.record(&crate::db::NewEvent { session_id: "claude:worker".into(), vendor: "claude".into(), cwd: format!("{wt}\\src"), kind: "ask".into(), summary: None, state: "waiting".into() }).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        let state = |id: &str| f.db.session(id).unwrap().unwrap().state;
        assert_eq!((state("claude:worker"), state(&agent_id), state("claude:person")), ("ended".into(), "ended".into(), "working".into()));
    }

    #[test]
    fn the_app_wants_a_supervisor_for_each_project_with_a_crew() {
        let off = fixture(GATE);
        let on = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let other = fixture(&format!("{GATE}[crew]\nworkers = 2\n"));
        let repo = |name: &str, root: &Path, project: Option<&str>| crate::RepoConfig {
            name: name.into(),
            root: root.to_string_lossy().into_owned(),
            integration_branch: "main".into(),
            project: project.map(str::to_string),
        };
        let config = crate::Config {
            repos: vec![repo("off", &off.repo, Some("OFF")), repo("on", &on.repo, Some("nit")), repo("second", &other.repo, Some("NIT")), repo("loose", &other.repo, None)],
            ..Default::default()
        };
        assert_eq!(supervisors_wanted(&config), vec![("NIT".to_string(), on.repo.clone())], "one per project, the first repo with a crew");

        assert!(unsupervised(&on.db, "NIT").unwrap());
        on.db.crew_heartbeat("NIT", 42, "working").unwrap();
        assert!(!unsupervised(&on.db, "NIT").unwrap());
        on.db.crew_leave("NIT", 42).unwrap();
        assert!(unsupervised(&on.db, "NIT").unwrap());
        on.db.crew_set_status("NIT", "the app could not start the supervisor: no nitely on PATH").unwrap();
        assert!(on.db.crew_control("NIT").unwrap().status.unwrap().starts_with("the app could not"));
    }

    #[test]
    fn a_hung_setup_holds_its_slot_but_never_the_supervisor() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 2
setup_timeout_minutes = 30
"));
        let fake = Fake { installed: vec!["claude"], hang_setup: true, db: Some(f.base.join("t.db")), ..Default::default() };
        let clock = Rc::new(std::cell::Cell::new(NOON));
        let c = clock.clone();
        let mut sup = Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(fake.clone())).with_clock(move || c.get());

        // Both setups hang: the tick returns, and their slots stay taken.
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert!(tick.started.is_empty());
        assert_eq!((sup.running(), tick.status.as_str()), (2, "working; setting up NIT-1, NIT-2"));
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.prepares.borrow(), ["NIT-1", "NIT-2"], "nothing more starts meanwhile");

        // Past the setup timeout they are stopped and go to a person; the last issue gets a slot.
        clock.set(Moment { ts: NOON.ts + 31 * 60, ..NOON });
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.stopped.borrow(), [5000, 5001]);
        let one = f.db.issue("NIT-1").unwrap().unwrap();
        assert_eq!(one.state, "Needs You");
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "handoff").unwrap();
        assert!(note.summary.unwrap().contains("crew.setup_timeout_minutes"));
        assert_eq!(*fake.prepares.borrow(), ["NIT-1", "NIT-2", "NIT-3"]);

        // A second Ctrl+C stops a setup too, and its issue goes back.
        assert!(sup.stop_all(&mut f.db).unwrap().is_empty());
        assert_eq!(*fake.stopped.borrow(), [5000, 5001, 5002]);
        assert_eq!(f.db.issue("NIT-3").unwrap().unwrap().state, "Todo");
        assert_eq!(sup.running(), 0);
    }

    #[test]
    fn a_new_supervisor_picks_up_a_setup_the_old_one_left() {
        let mut f = fixture(&format!("{GATE}[crew]
workers = 1
"));
        let hung = Fake { installed: vec!["claude"], hang_setup: true, db: Some(f.base.join("t.db")), ..Default::default() };
        Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(hung)).with_clock(|| NOON).tick(&mut f.db, false).unwrap();
        let row = &f.db.crew_running("NIT").unwrap()[0];
        assert_eq!((row.issue_key.as_str(), row.phase.as_str()), ("NIT-1", "setup"));

        // That supervisor is gone and so is its setup: the next one runs the setup again and starts NIT-1.
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let tick = supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.prepares.borrow(), ["NIT-1"]);
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-1"]);
        let rows = f.db.crew_running("NIT").unwrap();
        assert_eq!((rows.len(), rows[0].phase.as_str(), rows[0].id), (1, "work", row.id), "the same row, now working");
        assert!(rows[0].log.ends_with("-claude.log"));

        // A setup that ended without passing while no supervisor watched goes to a person.
        let mut f = fixture(&format!("setup = [\"exit 1\"]\n{GATE}[crew]\nworkers = 1\n"));
        let hung = Fake { installed: vec!["claude"], hang_setup: true, db: Some(f.base.join("t.db")), ..Default::default() };
        Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(hung)).with_clock(|| NOON).tick(&mut f.db, false).unwrap();
        let issue = f.db.issue("NIT-1").unwrap().unwrap();
        let worktree = PathBuf::from(&f.db.crew_running("NIT").unwrap()[0].worktree);
        playbook::start_task(&f.db, &f.repo, &issue, "claude", &Actor::Agent("cli".into())).unwrap();
        assert!(worktree.join(".git").exists() && !playbook::setup_passed(&worktree));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let tick = supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        assert!(!fake.prepares.borrow().contains(&"NIT-1".to_string()) && !tick.started.iter().any(|w| w.issue == "NIT-1"), "not set up again: {tick:?}");
        assert_eq!(f.db.issue("NIT-1").unwrap().unwrap().state, "Needs You");
    }

    #[test]
    fn focus_reclamation_retires_crashed_workers_before_a_supervisor_can_resume_them() {
        // An interrupted setup and an interrupted worker must both be retired by reclamation.
        for phase in ["setup", "work"] {
            let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
            let goal = f.db.save_goal("NIT", None, &crate::goals::GoalChange {
                title: Some("Focused feature".into()), outcome: Some("Ship it".into()), workers: Some(1), ..Default::default()
            }, &Actor::Human).unwrap().id;
            f.db.set_issue_goal("NIT-1", Some(goal), &Actor::Human).unwrap();
            f.db.set_project_focus("NIT", Some(goal), false, &Actor::Human).unwrap();
            let fake = Fake { installed: vec!["claude"], ..Default::default() };
            // Setup succeeds, but the old supervisor waits for memory before launching the agent.
            fake.short_to_launch.set(phase == "setup");
            let mut old = supervisor(&f, &fake);
            old.tick(&mut f.db, false).unwrap();
            let worker = f.db.crew_running("NIT").unwrap().remove(0);
            assert!(playbook::setup_passed(Path::new(&worker.worktree)));
            let launches_before = fake.launches.borrow().len();
            assert_eq!(launches_before, usize::from(phase == "work"));
            if phase == "work" {
                fake.exits.borrow_mut().insert(worker.pid.unwrap(), -1);
            }
            let stale = now() - 7 * 3600;
            f.db.conn.execute("UPDATE crew_worker SET phase = ?2, pid = 0 WHERE id = ?1", params![worker.id, phase]).unwrap();
            f.db.conn.execute("UPDATE issue SET claimed_at = ?1 WHERE key = 'NIT-1'", [stale]).unwrap();
            f.db.conn.execute("UPDATE event SET ts = ?1 WHERE issue_key = 'NIT-1'", [stale]).unwrap();

            let external = "codex:external";
            let claimed = f.db.next_task("NIT", external).unwrap().unwrap();
            assert_eq!(claimed.key, "NIT-1");
            assert!(f.db.crew_running("NIT").unwrap().is_empty(), "reclaim retires the {phase} row atomically");
            // A replacement session in that same folder must not be ended by the old worker's
            // cleanup when either the original or a restarted supervisor sees the stale setup.
            f.db.conn.execute(
                "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES (?1, 'codex', ?2, ?3, ?3, 'working')",
                params![external, worker.worktree, now()],
            ).unwrap();
            fake.short_to_launch.set(false);
            old.tick(&mut f.db, false).unwrap();
            let mut restarted = supervisor(&f, &fake);
            restarted.tick(&mut f.db, false).unwrap();
            assert_eq!(fake.launches.borrow().len(), launches_before, "neither supervisor launches a duplicate from the {phase} row");
            assert_eq!(fake.prepares.borrow().len(), 1, "the reclaimed worktree is not set up again");
            let issue = f.db.issue("NIT-1").unwrap().unwrap();
            assert_eq!((issue.state.as_str(), issue.claimed_by.as_deref()), ("In Progress", Some(external)));
            assert_eq!(f.db.session(external).unwrap().unwrap().state, "working");
        }
    }

    #[test]
    fn the_status_reads_the_repo_the_supervisor_follows() {
        let first = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let second = fixture(&format!("{GATE}[crew]\nworkers = 3\nmax_per_day = 7\n"));
        let repo = |name: &str, root: &Path| crate::RepoConfig {
            name: name.into(),
            root: root.to_string_lossy().into_owned(),
            integration_branch: "main".into(),
            project: Some("NIT".into()),
        };
        let config = crate::Config { repos: vec![repo("first", &first.repo), repo("second", &second.repo)], ..Default::default() };
        let db = &first.db;
        db.crew_heartbeat("NIT", 9, "working").unwrap();
        assert_eq!(project_status(db, &config, "NIT", &NOON).unwrap().unwrap().cap, 1, "the first repo with a crew");
        db.crew_follows("NIT", &second.repo).unwrap();
        let s = project_status(db, &config, "NIT", &NOON).unwrap().unwrap();
        assert_eq!((s.cap, s.max_per_day), (3, 7), "the repo `crew run --repo` follows");
        db.crew_follows("NIT", Path::new("/elsewhere")).unwrap();
        assert_eq!(project_status(db, &config, "NIT", &NOON).unwrap().unwrap().cap, 1, "a repo no longer the project's is ignored");
        db.crew_follows("NIT", &second.repo).unwrap();
        db.crew_leave("NIT", 9).unwrap();
        assert_eq!(project_status(db, &config, "NIT", &NOON).unwrap().unwrap().cap, 1, "once the supervisor left, its repo says nothing");
    }

    #[test]
    fn a_landed_workers_worktree_and_branch_are_removed_once_it_exits() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let worktree = fake.launches.borrow()[0].cwd.clone();
        // Land kept the worktree whole (the worker was in it) after merging the branch.
        assert!(f.db.crew_worker_in(&worktree).unwrap(), "land sees the worker in it");
        let agent = f.db.issue("NIT-1").unwrap().unwrap().claimed_by.unwrap();
        f.db.move_issue("NIT-1", "Done", Some("landed"), None, &Actor::Agent(agent)).unwrap();
        fake.exits.borrow_mut().insert(1001, 0);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "landed");
        assert!(!worktree.exists(), "the worktree is gone");
        assert!(branch_vendors(&f.repo, "NIT-1").is_empty(), "and the merged branch");
        let note = f.db.timeline("NIT-1").unwrap().into_iter().find(|t| t.kind == "crew_cleaned").unwrap();
        assert!(note.summary.unwrap().contains("removed the worktree and the merged branch"));

        // A dirty worktree stays, as land would leave it; one git does not track is never touched.
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let (dirty, loose) = (fake.launches.borrow()[0].cwd.clone(), fake.launches.borrow()[1].cwd.clone());
        std::fs::write(dirty.join("notes.txt"), "not committed").unwrap();
        std::fs::remove_dir_all(f.repo.join(".git").join("worktrees").join(loose.file_name().unwrap())).unwrap();
        for (key, pid) in [("NIT-1", 1001), ("NIT-2", 1002)] {
            let agent = f.db.issue(key).unwrap().unwrap().claimed_by.unwrap();
            f.db.move_issue(key, "Done", Some("landed"), None, &Actor::Agent(agent)).unwrap();
            fake.exits.borrow_mut().insert(pid, 0);
        }
        sup.tick(&mut f.db, false).unwrap();
        assert!(dirty.join("notes.txt").exists(), "uncommitted work is kept");
        assert!(loose.exists(), "an untracked folder is left alone");
    }

    #[test]
    fn a_hung_removal_is_stopped_and_land_keep_is_respected() {
        let land_and_exit = |f: &mut Fixture, key: &str, pid: u32, fake: &Fake| {
            let agent = f.db.issue(key).unwrap().unwrap().claimed_by.unwrap();
            f.db.move_issue(key, "Done", Some("landed"), None, &Actor::Agent(agent)).unwrap();
            fake.exits.borrow_mut().insert(pid, 0);
        };
        // A removal that hangs holds nothing up and is stopped at the setup timeout.
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\nsetup_timeout_minutes = 5\n"));
        let fake = Fake { installed: vec!["claude"], hang_cleanup: true, db: Some(f.base.join("t.db")), ..Default::default() };
        let clock = Rc::new(std::cell::Cell::new(NOON));
        let c = clock.clone();
        let mut sup = Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(fake.clone())).with_clock(move || c.get());
        sup.tick(&mut f.db, false).unwrap();
        let worktree = fake.launches.borrow()[0].cwd.clone();
        land_and_exit(&mut f, "NIT-1", 1001, &fake);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(*fake.cleanups.borrow(), [worktree.clone()]);
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-2"], "the crew goes on meanwhile");
        clock.set(Moment { ts: NOON.ts + 7 * 60, ..NOON });
        sup.tick(&mut f.db, false).unwrap();
        assert!(fake.stopped.borrow().contains(&7000));
        let note = f.db.timeline("NIT-1").unwrap().into_iter().rev().find(|t| t.kind == "crew_cleaned").unwrap();
        assert!(note.summary.unwrap().contains("ran past 6 minutes"), "its own limit plus a minute");
        assert!(worktree.exists(), "left for a person");

        // Landed with --keep: nothing is removed.
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        let worktree = fake.launches.borrow()[0].cwd.clone();
        f.db.log_event(&Actor::Human, Some("NIT-1"), None, "land_kept", "Kept the worktree and branch (--keep)").unwrap();
        land_and_exit(&mut f, "NIT-1", 1001, &fake);
        sup.tick(&mut f.db, false).unwrap();
        assert!(fake.cleanups.borrow().is_empty() && worktree.exists());
    }

    #[test]
    fn clean_up_touches_only_a_listed_worktree_on_its_branch() {
        let f = fixture(GATE);
        let git = |args: &[&str]| assert!(crate::playbook::run_git(Command::new("git").arg("-C").arg(&f.repo).args(args)).unwrap().status.success(), "{args:?}");
        let wt = f.base.join("wt").join("nit-1");
        git(&["worktree", "add", "-q", "-b", "claude/nit-1-first", &wt.to_string_lossy()]);
        let stray = f.base.join("wt").join("not-a-worktree");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("keep.txt"), "mine").unwrap();
        assert!(clean_up_worktree(&f.repo, &stray, "claude/nit-1-first").is_err());
        assert!(stray.join("keep.txt").exists(), "an ordinary folder is never deleted");
        assert!(clean_up_worktree(&f.repo, &wt, "main").is_err(), "nor a worktree on another branch");
        assert_eq!(clean_up_worktree(&f.repo, &wt, "claude/nit-1-first").unwrap(), "removed the worktree and the merged branch");
        assert!(!wt.exists());
    }

    #[test]
    fn a_rescan_adopts_only_workers_it_does_not_watch() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(sup.running(), 2);
        sup.rescan();
        sup.tick(&mut f.db, false).unwrap();
        assert_eq!(sup.running(), 2, "the two it watches are not taken twice");
    }

    #[test]
    fn one_supervisor_per_project() {
        let f = fixture(GATE);
        f.db.crew_claim_supervisor("NIT", 1).unwrap();
        assert!(f.db.crew_claim_supervisor("NIT", 2).unwrap_err().to_string().contains("pid 1"));
        assert!(f.db.crew_take_over("NIT", 2, Some(7)).is_err(), "only from the one holding it");
        f.db.crew_take_over("NIT", 2, Some(1)).unwrap();
        assert_eq!(f.db.crew_control("NIT").unwrap().supervisor_pid, Some(2), "the new build holds it");
        f.db.crew_leave("NIT", 1).unwrap();
        assert_eq!(f.db.crew_control("NIT").unwrap().supervisor_pid, Some(2), "the old one leaving does not clear it");
        f.db.crew_leave("NIT", 2).unwrap();
        f.db.crew_claim_supervisor("NIT", 3).unwrap();
    }

    #[test]
    fn a_real_agent_gets_the_brief_on_stdin_and_writes_its_log() {
        let dir = crate::paths::unique_temp("nitely-crew-proc", "");
        std::fs::create_dir_all(&dir).unwrap();
        let sh = playbook::posix_sh();
        let launch = Launch {
            agent: "fake".into(),
            program: sh.clone(),
            args: vec!["-c".into(), "read line; echo \"got: $line $NITELY_CREW\"; exit 3".into()],
            cwd: dir.clone(),
            brief: "the brief\n".into(),
            log: dir.join("w.log"),
            env: vec![("NITELY_CREW".into(), "7".into())],
        };
        let agents = SystemAgents;
        assert!(agents.resolve(&sh).is_some() || !Path::new(&sh).is_absolute());
        let mut p = agents.start(&launch).unwrap();
        let mut code = None;
        for _ in 0..100 {
            code = p.exited().unwrap();
            if code.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(code, Some(3));
        assert_eq!(std::fs::read_to_string(dir.join("w.log")).unwrap().trim(), "got: the brief 7");
        std::fs::remove_dir_all(dir).ok();
    }
}
