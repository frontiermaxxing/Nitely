//! The crew: Nitely starting and keeping up to N headless agent workers on a project's backlog,
//! one issue each (docs/crew.md). The supervisor claims the issue and makes its worktree itself,
//! then starts the agent there with the standard brief; everything the agent does after that goes
//! through the same MCP tools a person's agent uses. Starting a process is behind [`Agents`], so
//! tests run the supervisor with a fake agent.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// `HH:MM-HH:MM` local time when no new worker starts; it may wrap midnight.
    pub quiet_hours: Option<String>,
    /// A worker running longer is stopped, never while it lands.
    pub timeout_minutes: u64,
    /// Failed workers on one issue before it goes to a human.
    pub attempts: u32,
    /// Replaces the default command for an agent. `{worktree}`, `{repo}`, `{git_dir}`,
    /// `{mcp_config}` and `{nitely}` are filled in; the brief goes on stdin.
    pub commands: BTreeMap<String, Vec<String>>,
}

impl Default for CrewRules {
    fn default() -> Self {
        CrewRules {
            workers: 0,
            agents: vec!["claude".into(), "codex".into()],
            max_per_day: None,
            quiet_hours: None,
            timeout_minutes: 180,
            attempts: 3,
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
            // never. A worktree's commits are written to the main checkout's git folder.
            "codex" => args(&[
                "codex", "exec", "--sandbox", "workspace-write", "--add-dir", "{git_dir}",
                "-c", "sandbox_workspace_write.network_access=true",
                "-c", "approval_policy=\"never\"",
                "-c", "mcp_servers.nitely.command={nitely}",
                "-c", "mcp_servers.nitely.args=[\"mcp\"]",
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
}

enum Start {
    Running(WorkerLine),
    /// It cannot start on this issue now; others may.
    Wait(String),
    SetupFailed(String),
    LaunchFailed(String),
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
}

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
    running: Vec<Running>,
    /// Agents that would not start in this run, and why.
    disabled: BTreeMap<String, String>,
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
            running: Vec::new(),
            disabled: BTreeMap::new(),
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

    pub fn with_interrupt(mut self, interrupted: impl Fn() -> bool + 'static) -> Supervisor {
        self.interrupted = Box::new(interrupted);
        self
    }

    fn actor(&self) -> Actor {
        Actor::Agent(format!("crew:{}", self.project))
    }

    pub fn running(&self) -> usize {
        self.running.len()
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
        self.retry_answered(db)?;
        if self.self_paused && !db.crew_control(&self.project)?.paused {
            // A person resumed the crew: give every agent another chance.
            self.self_paused = false;
            self.disabled.clear();
            self.launch_failures.clear();
            self.waiting.clear();
        }
        tick.cap = rules.workers;
        let status = match &playbook {
            Err(e) => Some(format!("the playbook does not read: {e:#}")),
            Ok(p) => self.why_not_starting(db, p, &moment, draining)?,
        };
        tick.status = match status {
            Some(why) => why,
            None => self.fill(db, playbook.as_ref().unwrap(), &mut tick)?,
        };
        tick.running = self.running.len();
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
        if self.running.len() >= rules.workers as usize {
            return Ok(Some("working".into()));
        }
        Ok(None)
    }

    /// Starts workers until the slots are full, the daily cap is reached or nothing is ready.
    fn fill(&mut self, db: &mut Db, playbook: &Playbook, tick: &mut Tick) -> Result<String> {
        let rules = &playbook.crew;
        let moment = (self.clock)();
        while self.running.len() < rules.workers as usize {
            if (self.interrupted)() {
                return Ok("stopping: waiting for running workers".into());
            }
            if db.crew_started_since(&self.project, moment.day_start)? >= rules.max_per_day() {
                return Ok(format!("{} of {} started today", rules.max_per_day(), rules.max_per_day()));
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
            let Some(issue) = db.next_task_except(&self.project, self.actor().id(), &skipped)? else {
                return Ok(self.nothing_ready());
            };
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
            let why = match self.start_worker(db, playbook, &issue, &agent) {
                Ok(Start::Running(line)) => {
                    tick.started.push(line);
                    continue;
                }
                Ok(Start::Wait(why)) => {
                    self.wait(db, &issue.key, why)?;
                    continue;
                }
                Ok(Start::SetupFailed(why)) => {
                    // A person has to look: starting it again would fail the same way every tick.
                    db.add_note(&issue.key, &format!("The crew could not set up the worktree: {why}"), "handoff", &self.actor())?;
                    db.move_issue(&issue.key, "Needs You", None, Some("the crew could not set up its worktree"), &self.actor())?;
                    continue;
                }
                Ok(Start::LaunchFailed(why)) => why,
                Err(e) => format!("{e:#}"),
            };
            // The agent would not start: the issue goes back, and this run stops using that agent
            // instead of failing the same way on every tick.
            self.disabled.insert(agent.clone(), first_line(&why).to_string());
            db.add_note(&issue.key, &format!("The crew could not start a {agent} worker: {why}"), "note", &self.actor())?;
            db.move_issue(&issue.key, "Todo", None, Some("the crew could not start a worker"), &self.actor())?;
        }
        Ok("working".into())
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

    fn start_worker(&mut self, db: &mut Db, playbook: &Playbook, issue: &IssueRecord, agent: &str) -> Result<Start> {
        let worktree = playbook.worktree_for(&self.repo_root, agent, issue);
        // The id the worker's own MCP server will use, so its reports keep the claim fresh.
        let agent_id = format!("{agent}:{}", crate::paths::normalize(&worktree.to_string_lossy()));
        db.reassign_claim(&issue.key, &agent_id)?;
        let actor = Actor::Agent(agent_id.clone());
        let started = playbook::start_task(db, &self.repo_root, issue, agent, &actor)?;
        // The worktree may already exist on someone else's branch (the folder pattern need not name
        // the vendor): never put this agent on it.
        // The same agent's branch for the issue under an older title is fine.
        let on = playbook::git(Path::new(&started.worktree), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        let on = on.trim().to_string();
        let same = on == started.branch || crate::git::parse_branch(&on) == (Some(agent.to_string()), Some(issue.key.to_uppercase()));
        if !same {
            return Ok(Start::Wait(format!("its worktree {} is on {on}, not {}", started.worktree, started.branch)));
        }
        let started = playbook::Started { branch: on, ..started };
        if let Some(failed) = started.setup.iter().find(|r| !r.ok) {
            return Ok(Start::SetupFailed(format!("{} failed:\n{}", failed.command, failed.output)));
        }
        let worktree = PathBuf::from(&started.worktree);

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
        };
        let Some(program) = self.agents.resolve(&command[0]) else { return Ok(Start::LaunchFailed(format!("{} is not installed", command[0]))) };
        let id = db.crew_worker_started(&self.project, &issue.key, agent, &agent_id, &worktree, &log, ts)?;
        let launch = Launch {
            agent: agent.into(),
            program,
            args: command[1..].iter().map(|a| fill(a)).collect(),
            cwd: worktree.clone(),
            brief: brief(issue, &self.project, &worktree, &started.branch, &self.repo_root),
            log: log.clone(),
            env: vec![("NITELY_CREW".into(), id.to_string()), ("NITELY_PROJECT".into(), self.project.clone())],
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
        self.running.push(Running { id, issue: issue.key.clone(), agent: agent.into(), agent_id, worktree, started_at: ts, log, process });
        Ok(Start::Running(line))
    }

    /// `mcp.json` for Claude Code: the nitely server and nothing else.
    fn write_mcp_config(&self) -> Result<PathBuf> {
        let path = self.dir.join("mcp.json");
        let config = serde_json::json!({ "mcpServers": { "nitely": { "command": self.nitely, "args": ["mcp"] } } });
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&path, serde_json::to_string_pretty(&config)?).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Takes over the workers an earlier supervisor left running (it crashed, or its terminal was
    /// closed): they count toward the cap and are settled when they exit. Those that are gone are
    /// settled now.
    fn adopt(&mut self, db: &mut Db, rules: &CrewRules, tick: &mut Tick) -> Result<()> {
        for w in db.crew_running(&self.project)? {
            let running = Running {
                id: w.id,
                issue: w.issue_key,
                agent: w.agent,
                agent_id: w.agent_id,
                worktree: PathBuf::from(w.worktree),
                started_at: w.started_at,
                log: PathBuf::from(w.log),
                process: Box::new(Gone),
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
        let ts = (self.clock)().ts;
        let mut i = 0;
        while i < self.running.len() {
            let end = match self.running[i].process.exited() {
                Ok(Some(code)) => Some(End::Exited(code)),
                Ok(None) if ts - self.running[i].started_at > deadline && !self.landing(&self.running[i].issue) => {
                    self.running[i].process.stop();
                    Some(End::TimedOut)
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

    /// Records how a worker ended, read from its issue's state, and deals with a worker that ended
    /// without handing its issue on (docs/crew.md, Guardrails): a handoff note when it left none,
    /// and the issue back to Todo, or to a person after `attempts` failures.
    fn settle(&mut self, db: &mut Db, rules: &CrewRules, w: &Running, end: End) -> Result<String> {
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
        let mut outcome = match state.as_str() {
            "Done" => "landed",
            "In Review" | "Approved" => "submitted",
            "Needs You" => "asked",
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
        if outcome != "failed" {
            return Ok(outcome.into());
        }

        // Only this worker's own handoff counts: the supervisor's note on an earlier attempt can
        // fall in the same second this one started.
        if !timeline.iter().any(|t| t.kind == "handoff" && t.actor == w.agent_id) {
            let last = timeline.iter().rev().find(|t| t.actor == w.agent_id && t.kind == "status").and_then(|t| t.summary.clone());
            let last = last.map_or("it made no report".to_string(), |s| format!("its last report: \"{s}\""));
            let note = format!(
                "A crew worker ({}) stopped without handing this issue on ({how}); {last}. Whatever it committed is on its branch in {}. Its log: {}",
                w.agent,
                w.worktree.display(),
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
pub fn brief(issue: &IssueRecord, project: &str, worktree: &Path, branch: &str, repo: &Path) -> String {
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
5. Commit, then call submit. If the review sends it back (P0 or P1 findings), fix them, commit and submit again, up to three rounds; then file what is left and stop.
6. When submit says it passed and no human approval is needed, call land. On a rebase conflict, resolve it in the worktree, then submit and land again.
7. Before you stop, for any reason, leave a note on {key} with handoff: true: what is done, what is left, what you were unsure about.

Then end. Do not take another issue: Nitely starts a new worker for it.
"
    )
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
}

#[derive(Debug, Clone, Serialize)]
pub struct CrewStatus {
    pub project: String,
    pub cap: u32,
    pub running: Vec<CrewWorker>,
    pub started_today: u32,
    pub max_per_day: u32,
    pub paused: bool,
    /// A supervisor's heartbeat is fresh.
    pub supervised: bool,
    /// What the supervisor last said, e.g. `working` or `quiet hours until 07:00`.
    pub status: Option<String>,
    pub recent: Vec<CrewWorker>,
}

/// The crew of `project`, whose repo playbook is `playbook`.
pub fn status(db: &Db, project: &str, playbook: &Playbook, moment: &Moment) -> Result<CrewStatus> {
    let control = db.crew_control(project)?;
    let workers = db.crew_workers(project, 20)?;
    Ok(CrewStatus {
        project: project.to_uppercase(),
        cap: playbook.crew.workers,
        running: db.crew_running(project)?,
        started_today: db.crew_started_since(project, moment.day_start)?,
        max_per_day: playbook.crew.max_per_day(),
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
        if Playbook::load(Path::new(&repo.root)).is_ok_and(|p| p.crew.workers > 0) {
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
/// builds, so a supervisor that leaves for a new build is replaced by that build.
pub fn nitely_on_path() -> Option<PathBuf> {
    on_path("nitely")
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
    let playbooks: Vec<Playbook> = config
        .repos
        .iter()
        .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
        .filter_map(|r| Playbook::load(Path::new(&r.root)).ok())
        .collect();
    let playbook = playbooks.iter().find(|p| p.crew.workers > 0).or(playbooks.first()).cloned().unwrap_or_default();
    let status = status(db, project, &playbook, moment)?;
    Ok((status.cap > 0 || !status.running.is_empty() || status.supervised).then_some(status))
}

const WORKER_COLUMNS: &str = "id, project, issue_key, agent, pid, worktree, log_path, started_at, ended_at, exit_code, outcome, reason, agent_id";

impl Db {
    /// Marks the sessions a crew worker runs: in its worktree, started once the worker had. A
    /// database without the crew tables (an older one, read-only) marks none.
    pub(crate) fn mark_crew(&self, sessions: &mut [crate::db::Session]) {
        let Some(oldest) = sessions.iter().map(|s| s.started_at).min() else { return };
        let workers: Vec<(String, i64, Option<i64>)> = (|| -> rusqlite::Result<_> {
            let mut stmt = self.conn.prepare("SELECT worktree, started_at, ended_at FROM crew_worker WHERE ended_at IS NULL OR ended_at >= ?1")?;
            let rows = stmt.query_map([oldest - 60], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect()
        })()
        .unwrap_or_default();
        if workers.is_empty() {
            return;
        }
        let workers: Vec<(String, i64, Option<i64>)> = workers.into_iter().map(|(w, s, e)| (crate::paths::normalize(&w), s, e)).collect();
        for s in sessions.iter_mut() {
            let cwd = crate::paths::normalize(&s.cwd);
            s.crew = workers.iter().any(|(wt, started, ended)| {
                let inside = cwd == *wt || cwd.strip_prefix(wt.as_str()).is_some_and(|rest| rest.starts_with('/'));
                // A minute's slack: the worker's row is written just before its process starts.
                inside && s.started_at >= started - 60 && ended.is_none_or(|e| s.started_at <= e)
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
                "SELECT paused, changed_by, changed_at, supervisor_pid, heartbeat_at, status FROM crew_control WHERE project = ?1 COLLATE NOCASE",
                [project],
                |r| {
                    Ok(CrewControl {
                        paused: r.get::<_, i64>(0)? != 0,
                        changed_by: r.get(1)?,
                        changed_at: r.get(2)?,
                        supervisor_pid: r.get(3)?,
                        heartbeat_at: r.get(4)?,
                        status: r.get(5)?,
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
    fn crew_worker_started(&self, project: &str, issue: &str, agent: &str, agent_id: &str, worktree: &Path, log: &Path, ts: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO crew_worker (project, issue_key, agent, agent_id, worktree, log_path, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![project, issue, agent, agent_id, worktree.to_string_lossy(), log.to_string_lossy(), ts],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn crew_worker_pid(&self, id: i64, pid: u32) -> Result<()> {
        self.conn.execute("UPDATE crew_worker SET pid = ?2 WHERE id = ?1", rusqlite::params![id, pid])?;
        Ok(())
    }

    fn crew_worker_ended(&self, id: i64, code: Option<i32>, outcome: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE crew_worker SET ended_at = ?2, exit_code = ?3, outcome = ?4, reason = ?5 WHERE id = ?1",
            rusqlite::params![id, now(), code, outcome, reason],
        )?;
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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window, and its own process group: Ctrl+C in the supervisor's console
            // must not reach a worker that may be landing.
            cmd.creation_flags(0x0800_0000 | 0x0000_0200); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().with_context(|| format!("starting {}", launch.program))?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let brief = launch.brief.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            stdin.write_all(brief.as_bytes()).ok();
        });
        Ok(Box::new(SystemProcess(child)))
    }

    fn adopt(&self, pid: u32) -> Option<Box<dyn Process>> {
        pid_alive(pid).then(|| Box::new(Adopted(pid)) as Box<dyn Process>)
    }
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

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = playbook::shell_free_command("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output();
        out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).output().is_ok_and(|o| o.status.success())
    }
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
    fn the_brief_names_the_issue_the_worktree_and_the_rules() {
        let issue = IssueRecord { key: "NIT-7".into(), title: "Fix the thing".into(), ..Default::default() };
        let b = brief(&issue, "NIT", Path::new("/w/nit-7"), "codex/nit-7-fix", Path::new("/r"));
        for part in ["NIT-7, \"Fix the thing\"", "/w/nit-7 (branch codex/nit-7-fix)", "never edit or commit in /r", "call land", "handoff: true", "Do not take another issue"] {
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
        let base = std::env::temp_dir().join(format!("nitely-crew-{}-{}-{}", std::process::id(), now(), rand_suffix()));
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap().status.success(), "{args:?}");
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), format!("[worktree]\ndir = \"../wt/{{key}}\"\n{crew}")).unwrap();
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        for (title, priority) in [("First", 1), ("Second", 2), ("Third", 3)] {
            let new = NewIssue { project: "NIT".into(), title: title.into(), body: "wanted".into(), kind: "bug".into(), source: "human".into(), evidence: "seen".into(), priority, ..Default::default() };
            db.create_issue(&new, &Actor::Human).unwrap();
        }
        Fixture { base, repo, db }
    }

    fn rand_suffix() -> u64 {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        u64::from_le_bytes(b)
    }

    const NOON: Moment = Moment { ts: 1_000_000, minute_of_day: 12 * 60, day_start: 1_000_000 - 12 * 3600 };

    fn supervisor(f: &Fixture, fake: &Fake) -> Supervisor {
        Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(fake.clone())).with_clock(|| NOON)
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
        let status = status(&f.db, "NIT", &Playbook::load(&f.repo).unwrap(), &NOON).unwrap();
        assert_eq!((status.running.len(), status.started_today, status.cap), (2, 4, 2));
        assert!(status.supervised);
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
    fn a_second_attempt_keeps_the_first_attempts_agent() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 1\n"));
        // NIT-1 already has a codex branch from an earlier attempt.
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&f.repo).args(args).output().unwrap().status.success());
        git(&["branch", "codex/nit-1-first"]);
        let fake = Fake { installed: vec!["claude", "codex"], ..Default::default() };
        let tick = supervisor(&f, &fake).tick(&mut f.db, false).unwrap();
        assert_eq!(tick.started[0].agent, "codex");
        assert_eq!(fake.launches.borrow()[0].args.last().unwrap(), "-");
    }

    #[test]
    fn never_puts_an_agent_on_another_vendors_branch() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 3\n"));
        let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&f.repo).args(args).output().unwrap().status.success(), "{args:?}");
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
        let mut sup = Supervisor::new("NIT", &f.repo, &f.base.join("crew"), "nitely", Box::new(fake.clone())).with_clock(move || c.get());
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
        let json = serde_json::to_value(f.db.session("claude:person").unwrap().unwrap()).unwrap();
        assert!(json.get("crew").is_none(), "only crew sessions carry the field");
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
        let dir = std::env::temp_dir().join(format!("nitely-crew-proc-{}-{}", std::process::id(), rand_suffix()));
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
