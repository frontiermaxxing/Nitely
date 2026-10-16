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

enum Start {
    Running(WorkerLine),
    SetupFailed(String),
    LaunchFailed(String),
}

struct Running {
    id: i64,
    issue: String,
    agent: String,
    agent_id: String,
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
    running: Vec<Running>,
    /// Agents that would not start in this run, and why.
    disabled: BTreeMap<String, String>,
    /// Workers left by an earlier supervisor have been looked for.
    adopted: bool,
}

impl Supervisor {
    pub fn new(project: &str, repo_root: &Path, dir: &Path, nitely: &str, agents: Box<dyn Agents>) -> Supervisor {
        Supervisor {
            project: project.to_uppercase(),
            repo_root: repo_root.to_path_buf(),
            dir: dir.to_path_buf(),
            nitely: nitely.into(),
            agents,
            clock: Box::new(Moment::now),
            running: Vec::new(),
            disabled: BTreeMap::new(),
            adopted: false,
        }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> Moment + 'static) -> Supervisor {
        self.clock = Box::new(clock);
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
        if !self.adopted {
            self.adopt(db, &mut tick)?;
            self.adopted = true;
        }
        self.reap(db, &mut tick)?;

        let playbook = Playbook::load(&self.repo_root);
        let rules = playbook.as_ref().map(|p| p.crew.clone()).unwrap_or_default();
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
                let why: Vec<String> = self.disabled.iter().map(|(a, why)| format!("{a}: {why}")).collect();
                return Ok(format!("no agent will start ({})", why.join("; ")));
            }
            let Some(issue) = db.next_task(&self.project, self.actor().id())? else {
                return Ok(if self.running.is_empty() { "nothing ready".into() } else { "working; nothing else ready".into() });
            };
            // A second attempt continues the first one's branch when its agent is still on the list.
            let agent = installed
                .iter()
                .find(|a| branch_exists(&self.repo_root, &playbook.branch_for(a, &issue)))
                .unwrap_or(&installed[0])
                .clone();
            let why = match self.start_worker(db, playbook, &issue, &agent) {
                Ok(Start::Running(line)) => {
                    tick.started.push(line);
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

    fn start_worker(&mut self, db: &mut Db, playbook: &Playbook, issue: &IssueRecord, agent: &str) -> Result<Start> {
        let worktree = playbook.worktree_for(&self.repo_root, agent, issue);
        // The id the worker's own MCP server will use, so its reports keep the claim fresh.
        let agent_id = format!("{agent}:{}", crate::paths::normalize(&worktree.to_string_lossy()));
        db.reassign_claim(&issue.key, &agent_id)?;
        let actor = Actor::Agent(agent_id.clone());
        let started = playbook::start_task(db, &self.repo_root, issue, agent, &actor)?;
        if let Some(failed) = started.setup.iter().find(|r| !r.ok) {
            return Ok(Start::SetupFailed(format!("{} failed:\n{}", failed.command, failed.output)));
        }
        let worktree = PathBuf::from(&started.worktree);

        let ts = now();
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
        self.running.push(Running { id, issue: issue.key.clone(), agent: agent.into(), agent_id, started_at: ts, log, process });
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
    fn adopt(&mut self, db: &mut Db, tick: &mut Tick) -> Result<()> {
        for w in db.crew_workers(&self.project, 200)?.into_iter().filter(|w| w.ended_at.is_none()) {
            let running = Running {
                id: w.id,
                issue: w.issue_key,
                agent: w.agent,
                agent_id: w.agent_id,
                started_at: w.started_at,
                log: PathBuf::from(w.log),
                process: Box::new(Gone),
            };
            match w.pid.and_then(|pid| self.agents.adopt(pid)) {
                Some(process) => self.running.push(Running { process, ..running }),
                None => {
                    let outcome = self.settle(db, &running, None)?;
                    tick.ended.push(WorkerLine { issue: running.issue, agent: running.agent, outcome, log: running.log.to_string_lossy().into_owned() });
                }
            }
        }
        Ok(())
    }

    /// Settles every worker that exited.
    fn reap(&mut self, db: &mut Db, tick: &mut Tick) -> Result<()> {
        let mut i = 0;
        while i < self.running.len() {
            let code = match self.running[i].process.exited() {
                Ok(Some(code)) => Some(code),
                Ok(None) => None,
                Err(_) => Some(-1),
            };
            match code {
                None => i += 1,
                Some(code) => {
                    let w = self.running.remove(i);
                    let outcome = self.settle(db, &w, Some(code))?;
                    tick.ended.push(WorkerLine { issue: w.issue, agent: w.agent, outcome, log: w.log.to_string_lossy().into_owned() });
                }
            }
        }
        Ok(())
    }

    /// Records how a worker ended, read from its issue's state.
    fn settle(&self, db: &mut Db, w: &Running, code: Option<i32>) -> Result<String> {
        let issue = db.issue(&w.issue)?;
        let state = issue.as_ref().map(|i| i.state.as_str()).unwrap_or("");
        let still_mine = issue.as_ref().is_some_and(|i| i.claimed_by.as_deref() == Some(w.agent_id.as_str()));
        let outcome = match state {
            "Done" => "landed",
            "In Review" | "Approved" => "submitted",
            "Needs You" => "asked",
            "In Progress" | "Todo" if still_mine || state == "Todo" => "failed",
            _ => "stopped",
        };
        let minutes = (now() - w.started_at) / 60;
        let code_text = code.map_or("no exit code".to_string(), |c| format!("exit code {c}"));
        let reason = format!("{code_text} after {minutes} min, issue {state}");
        db.crew_worker_ended(w.id, code, outcome, &reason)?;
        db.log_event(&self.actor(), Some(&w.issue), None, "crew_ended", &format!("Crew worker ({}) ended: {outcome}, {reason}. Log: {}", w.agent, w.log.display()))?;
        Ok(outcome.into())
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
            let still_mine = db.issue(&w.issue)?.is_some_and(|i| i.claimed_by.as_deref() == Some(w.agent_id.as_str()));
            if still_mine {
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

fn branch_exists(repo: &Path, branch: &str) -> bool {
    playbook::git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
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
        running: workers.iter().filter(|w| w.ended_at.is_none()).cloned().collect(),
        started_today: db.crew_started_since(project, moment.day_start)?,
        max_per_day: playbook.crew.max_per_day(),
        paused: control.paused,
        supervised: control.heartbeat_at.is_some_and(|h| moment.ts - h < HEARTBEAT_STALE_SECS),
        status: control.status,
        recent: workers.into_iter().filter(|w| w.ended_at.is_some()).take(10).collect(),
    })
}

const WORKER_COLUMNS: &str = "id, project, issue_key, agent, pid, worktree, log_path, started_at, ended_at, exit_code, outcome, reason, agent_id";

impl Db {
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
    pub fn crew_claim_supervisor(&self, project: &str, pid: u32) -> Result<()> {
        let c = self.crew_control(project)?;
        if let (Some(other), Some(at)) = (c.supervisor_pid, c.heartbeat_at) {
            if other != pid && now() - at < HEARTBEAT_STALE_SECS {
                bail!("another `nitely crew run` (pid {other}) is running the {project} crew; stop it first");
            }
        }
        self.crew_heartbeat(project, pid, "starting")
    }

    pub fn crew_heartbeat(&self, project: &str, pid: u32, status: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO crew_control (project, supervisor_pid, heartbeat_at, status) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (project) DO UPDATE SET supervisor_pid = ?2, heartbeat_at = ?3, status = ?4",
            rusqlite::params![project.to_uppercase(), pid, now(), status],
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
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {WORKER_COLUMNS} FROM crew_worker WHERE project = ?1 COLLATE NOCASE ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(rusqlite::params![project, limit as i64], |r| {
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
        })?;
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
        std::process::Command::new("kill").args(["-TERM", "--", &format!("-{pid}")]).output().ok();
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

        // Nothing else ready once the backlog is empty.
        fake.exits.borrow_mut().insert(1002, 1);
        let tick = sup.tick(&mut f.db, false).unwrap();
        assert_eq!(tick.ended[0].outcome, "failed", "exited with its issue still In Progress and claimed");
        assert_eq!(tick.status, "working; nothing else ready");
        let status = status(&f.db, "NIT", &Playbook::load(&f.repo).unwrap(), &NOON).unwrap();
        assert_eq!((status.running.len(), status.started_today, status.cap), (1, 3, 2));
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
    fn stopping_spares_a_landing_worker_and_returns_the_others_to_todo() {
        let mut f = fixture(&format!("{GATE}[crew]\nworkers = 2\n"));
        let fake = Fake { installed: vec!["claude"], ..Default::default() };
        let mut sup = supervisor(&f, &fake);
        sup.tick(&mut f.db, false).unwrap();
        // NIT-2's worker holds the land lock.
        std::fs::write(f.repo.join(".git").join(crate::land::LOCK_FILE), "NIT-2 pid 1 since now").unwrap();
        let landing = sup.stop_all(&mut f.db).unwrap();
        assert_eq!(landing, ["NIT-2"]);
        assert_eq!(*fake.stopped.borrow(), [1001]);
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
        assert_eq!(tick.status, "no agent will start (claude: Failed to authenticate)");
        let one = f.db.issue("NIT-1").unwrap().unwrap();
        assert_eq!((one.state.as_str(), one.claimed_by), ("Todo", None), "the issue goes back");
        assert_eq!(f.db.issue("NIT-2").unwrap().unwrap().state, "Todo", "and no other issue was taken");
        assert_eq!(f.db.crew_workers("NIT", 5).unwrap()[0].outcome.as_deref(), Some("launch_failed"));
        assert!(sup.tick(&mut f.db, false).unwrap().status.starts_with("no agent will start"));
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
        assert_eq!(tick.started.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["NIT-3"], "NIT-1's live worker holds the other slot");
        assert_eq!(next.running(), 2);
        fake.exits.borrow_mut().insert(1001, 0);
        assert_eq!(next.tick(&mut f.db, false).unwrap().ended[0].issue, "NIT-1");
    }

    #[test]
    fn one_supervisor_per_project() {
        let f = fixture(GATE);
        f.db.crew_claim_supervisor("NIT", 1).unwrap();
        assert!(f.db.crew_claim_supervisor("NIT", 2).unwrap_err().to_string().contains("pid 1"));
        f.db.crew_leave("NIT", 1).unwrap();
        f.db.crew_claim_supervisor("NIT", 2).unwrap();
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
