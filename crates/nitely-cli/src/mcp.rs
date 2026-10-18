//! `nitely mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC 2.0).
//! This is how agents work with Nitely: take the next issue, report progress, leave notes, file
//! what they find, and ask the human. Every call acts as an agent, never as the human, so it can
//! not approve or answer.
//!
//! Each tool call runs on a thread of its own and is answered when it ends, so a long one (a lane
//! run, a land) never holds up the agent's next calls; a call the client cancels is not answered;
//! and no call waits past `wait_cap`, below the clients' own timeouts (NIT-308). The calls that can
//! run longer (a land, a lane run, `submit` with wait) run in a process of their own, which goes on
//! when the server or its client is gone.

use anyhow::{anyhow, bail, Context, Result};
use nitely_core::db::NewEvent;
use nitely_core::issues::{branch_name, BatchItem, KINDS, OWNERS, SOURCES, STATES};
use nitely_core::playbook::{self, Playbook};
use nitely_core::{paths, resolve_project, Actor, Config, Db, NewIssue};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone)]
pub(crate) struct Server {
    vendor: String,
    cwd: String,
    /// The executable this server started from, and how that file looked then. When `nitely
    /// install` puts a new build there, tool calls go to it (see `forward`).
    exe: Option<(PathBuf, (u64, SystemTime))>,
    /// Set for an agent on another machine, reaching this one through `nitely serve`.
    remote: Option<Remote>,
}

/// An agent on another machine: the name of its token, and the project it works in by default.
#[derive(Clone)]
pub(crate) struct Remote {
    pub name: String,
    pub project: Option<String>,
}

/// Tools that work in a repo on this machine, so not for an agent elsewhere.
const LOCAL_ONLY: &[&str] = &["start_task", "run_gate", "run", "submit", "submit_status", "land", "widen", "triage", "get_triage"];

impl Server {
    pub(crate) fn remote(name: &str, project: Option<String>) -> Server {
        Server { vendor: "agent".into(), cwd: String::new(), exe: None, remote: Some(Remote { name: name.into(), project }) }
    }

    /// One id per agent per directory, e.g. `claude:c:/code/nitely-worktrees/nit-3`; an
    /// agent on another machine is `claude@mac`, after its token.
    fn agent(&self) -> String {
        match &self.remote {
            Some(r) => format!("{}@{}", self.vendor, r.name),
            None => format!("{}:{}", self.vendor, paths::normalize(&self.cwd)),
        }
    }

    /// Where the session records say the agent works.
    fn place(&self) -> String {
        match &self.remote {
            Some(r) => format!("remote:{}", r.name),
            None => self.cwd.clone(),
        }
    }

    /// Who a call acts as: its session (see `session` and `claude_session`), so a claim, a note or
    /// a move names the agent that made it, not every session in its folder (NIT-194). A crew
    /// worker keeps its folder's id: the supervisor knows it by that id, and its worktree is its own.
    fn actor_for(&self, session: &str) -> Actor {
        if crew_worker() {
            return Actor::Agent(self.agent());
        }
        Actor::Agent(session.to_string())
    }

    /// The Claude Code session making a call: the one whose PreToolUse hook noted the call's tool
    /// use id (`_meta["claudecode/toolUseId"]`); without that (hooks not set up for it yet), the
    /// session Claude Code named when it started this server (`named`, its
    /// `CLAUDE_CODE_SESSION_ID`), while that session is still open: a `/clear` starts another.
    fn claude_session(&self, params: &Value, db: &Db, named: Option<&str>) -> Option<String> {
        // A crew worker's reports go under its folder's id too, where the supervisor reads them.
        if self.vendor != "claude" || self.remote.is_some() || crew_worker() {
            return None;
        }
        let call = params.pointer("/_meta/claudecode~1toolUseId").and_then(Value::as_str).filter(|c| !c.is_empty());
        if let Some(session) = call.and_then(|c| db.session_of_tool_call(c).ok().flatten()) {
            return Some(session);
        }
        let named = format!("claude:{}", named.filter(|s| !s.is_empty())?);
        db.session(&named).ok().flatten().filter(|s| s.state != "ended").map(|s| s.id)
    }

    /// The session a tool call's report or ask counts toward. Codex names its thread on every
    /// call (`_meta.threadId`), the same id its turn-end hook reports, so each thread is a session
    /// of its own even when several work in one folder (NIT-36). Otherwise the agent id.
    fn session(&self, params: &Value) -> String {
        let meta = params.get("_meta");
        let thread = meta
            .and_then(|m| m.get("threadId"))
            .or_else(|| meta.and_then(|m| m.pointer("/x-codex-turn-metadata/thread_id")))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        match thread {
            Some(t) if self.vendor == "codex" && self.remote.is_none() => format!("codex:{t}"),
            _ => self.agent(),
        }
    }
}

/// Whether this server serves a crew worker: the supervisor sets `NITELY_CREW` in its worker's MCP
/// configuration (crew.rs), and knows the worker by its folder's id.
fn crew_worker() -> bool {
    std::env::var_os("NITELY_CREW").is_some()
}

/// The longest one call waits in this server before it answers that it is still waiting: below
/// the timeout the client gives a call, so no call outlives the client that made it. The client
/// says who it is when it connects (`initialize`), which sets this. `NITELY_MCP_WAIT_CAP`
/// (seconds) sets it, for tests.
fn wait_cap() -> Duration {
    let secs = std::env::var("NITELY_MCP_WAIT_CAP").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| CLIENT_WAIT.load(std::sync::atomic::Ordering::Relaxed));
    Duration::from_secs(secs)
}

/// The wait cap for the client this server answers, in seconds (`cap_for`).
static CLIENT_WAIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(300);

/// The wait cap for a client (NIT-314): Claude Code gives a call 30 minutes without a word; Codex
/// gives one 60 seconds by default, and other clients are not known to give more.
fn cap_for(vendor: &str) -> u64 {
    if vendor == "claude" { 300 } else { 50 }
}

/// A lock that another thread's panic does not poison for the rest.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Tool calls running in this process.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Counts as work in flight while it lives.
struct Busy;

impl Busy {
    fn start() -> Busy {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        Busy
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Waits until no call is in flight: the client closed its end, and what it asked last (a wait on
/// a run, a call forwarded to a newer build) still ends and answers before the process exits.
fn finish_in_flight() {
    while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Writes one message as a whole line: replies from several threads never interleave.
fn send(out: &Mutex<std::io::Stdout>, message: &Value) {
    let mut out = lock(out);
    writeln!(out, "{message}").ok();
    out.flush().ok();
}

pub fn serve() -> Result<()> {
    let exe = std::env::current_exe().ok().and_then(|p| nitely_core::install::stamp(&p).map(|s| (p, s)));
    let server = Mutex::new(Server { vendor: "agent".into(), cwd: std::env::current_dir()?.to_string_lossy().into_owned(), exe, remote: None });
    let out = Arc::new(Mutex::new(std::io::stdout()));
    // Tool calls still running, and those of them the client cancelled: a cancelled one is not
    // answered. Both forget a call when it ends, so an id the client uses again is a new call.
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    let cancelled: Arc<Mutex<HashSet<String>>> = Arc::default();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => msg,
            Err(e) => {
                send(&out, &error_response(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };
        match (msg.get("method").and_then(Value::as_str).unwrap_or(""), msg.get("id")) {
            ("notifications/cancelled", _) => {
                if let Some(id) = msg.pointer("/params/requestId").map(Value::to_string) {
                    // Held while it marks the call, so the call cannot end in between and leave
                    // the mark behind for a later call with its id.
                    let running = lock(&running);
                    if running.contains(&id) {
                        lock(&cancelled).insert(id);
                    }
                }
            }
            // A tool call runs on its own thread: a long one holds up nothing after it.
            ("tools/call", Some(id)) => {
                let (mut server, out, running, cancelled, id) = (lock(&server).clone(), Arc::clone(&out), Arc::clone(&running), Arc::clone(&cancelled), id.to_string());
                lock(&running).insert(id.clone());
                let busy = Busy::start();
                std::thread::spawn(move || {
                    let _busy = busy;
                    let reply = handle(&mut server, &msg);
                    let was_cancelled = {
                        let mut running = lock(&running);
                        running.remove(&id);
                        lock(&cancelled).remove(&id)
                    };
                    if !was_cancelled {
                        if let Some(reply) = reply {
                            send(&out, &reply);
                        }
                    }
                });
            }
            // The rest (initialize, tools/list, ping) is quick, and initialize sets who calls.
            _ => {
                let reply = handle(&mut lock(&server), &msg);
                if let Some(reply) = reply {
                    send(&out, &reply);
                }
            }
        }
    }
    finish_in_flight();
    Ok(())
}

/// Runs this tool call in a process of its own (`nitely mcp-detached`), acting for `session`, and
/// waits for it up to `wait_cap`. That process outlives this server and its client, so a land the
/// agent stops waiting on, or whose session ends, still finishes. Past the cap, answers with what
/// `pending` says, given the log the process writes and what it has said so far (`{queued}`, a
/// lane run's id once it is queued).
fn run_apart(server: &Server, session: &str, params: &Value, pending: impl FnOnce(&std::path::Path, Option<Value>) -> Result<Value>) -> Result<Value> {
    let jobs = paths::data_dir().join("jobs");
    std::fs::create_dir_all(&jobs)?;
    sweep_apart(&jobs);
    let name = params.get("name").and_then(Value::as_str).unwrap_or("call");
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let stem = format!("mcp-{name}-{}-{stamp}", std::process::id());
    let (out, log) = (jobs.join(format!("{stem}.json")), jobs.join(format!("{stem}.log")));
    let file = std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("mcp-detached").stdin(Stdio::piped()).stdout(Stdio::from(file.try_clone()?)).stderr(Stdio::from(file));
    if std::path::Path::new(&server.cwd).is_dir() {
        cmd.current_dir(&server.cwd);
    }
    let mut child = nitely_core::submit_job::spawn_detached(&mut cmd).context("starting nitely mcp-detached")?;
    let request = json!({ "vendor": server.vendor, "cwd": server.cwd, "params": params, "session": session, "out": out.to_string_lossy() });
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{request}")?;
    }
    let (tx, rx) = mpsc::channel();
    // Reaped when it ends, however long after this call answered.
    std::thread::spawn(move || {
        tx.send(child.wait()).ok();
    });
    let said = || std::fs::read(&out).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    match rx.recv_timeout(wait_cap()) {
        Err(mpsc::RecvTimeoutError::Timeout) => pending(&log, said()),
        _ => {
            let answer = said().filter(|a| a.get("ok").is_some() || a.get("error").is_some());
            let answer = answer.with_context(|| format!("the call's process ended without an answer; its log: {}", log.display()))?;
            std::fs::remove_file(&out).ok();
            std::fs::remove_file(&log).ok();
            match answer.get("error").and_then(Value::as_str) {
                Some(e) => bail!("{e}"),
                None => Ok(answer.get("ok").cloned().unwrap_or(Value::Null)),
            }
        }
    }
}

/// How long a past-cap call's answer and log stay in `jobs/` for whoever wants to read them.
const APART_KEPT_SECS: u64 = 24 * 60 * 60;

/// Removes the answers and logs of calls that ran past the cap, once they are a day old: nobody
/// waited for those when they ended, so nothing else removes them.
fn sweep_apart(jobs: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(jobs) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > APART_KEPT_SECS);
        if name.starts_with("mcp-") && (name.ends_with(".json") || name.ends_with(".log")) && old {
            std::fs::remove_file(e.path()).ok();
        }
    }
}

/// In the process `run_apart` started: where its answer goes, and so where a lane run's id goes
/// once it is queued, for the server's answer past the cap.
static ANSWER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// `nitely mcp-detached`: the process `run_apart` starts. Runs the call here, as the session the
/// server resolved, and writes {ok} or {error} to `out`.
pub fn run_detached() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let request: Value = serde_json::from_str(&input).context("mcp-detached takes {vendor, cwd, params, session, out} on stdin")?;
    let server = Server { vendor: str_arg(&request, "vendor").unwrap_or("agent").to_string(), cwd: req(&request, "cwd")?.to_string(), exe: None, remote: None };
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    ANSWER.set(PathBuf::from(req(&request, "out")?)).ok();
    let answer = match run_tool_as(&server, name, &args, &params, Some(req(&request, "session")?)) {
        Ok(v) => json!({ "ok": v }),
        // Older than the database: the newer build being installed answers instead (NIT-314).
        Err(e) if crate::newer::is_newer_database(&e) => {
            if crate::newer::hand_over_with_input(&input) {
                return Ok(());
            }
            json!({ "error": crate::newer::older_build(&e) })
        }
        Err(e) => json!({ "error": format!("{e:#}") }),
    };
    println!("{answer}");
    write_whole(std::path::Path::new(req(&request, "out")?), &serde_json::to_vec(&answer)?)?;
    Ok(())
}

/// Writes `path` whole: a new file renamed over it, so whoever reads it meanwhile sees the old
/// content or the new, never half of it.
fn write_whole(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let part = path.with_extension("part");
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path)
}

pub(crate) fn handle(server: &mut Server, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    // Notifications (no id) get no response.
    let id = id?;
    let result = match method {
        "initialize" => Ok(initialize(server, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools_for(server) })),
        // A remote session runs here, in this build: its server is restarted to update.
        "tools/call" => Ok(if server.remote.is_some() { None } else { forward(server, &params) }.unwrap_or_else(|| call_tool(server, &params))),
        _ => return Some(error_response(id, -32601, &format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err::<Value, anyhow::Error>(e) => error_response(id, -32603, &format!("{e:#}")),
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(server: &mut Server, params: &Value) -> Value {
    let client = params.pointer("/clientInfo/name").and_then(Value::as_str).unwrap_or("").to_lowercase();
    server.vendor = if client.contains("claude") {
        "claude"
    } else if client.contains("cursor") {
        "cursor"
    } else if client.contains("codex") {
        "codex"
    } else {
        "agent"
    }
    .into();
    CLIENT_WAIT.store(cap_for(&server.vendor), std::sync::atomic::Ordering::Relaxed);
    let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = if SUPPORTED_VERSIONS.contains(&requested) { requested } else { SUPPORTED_VERSIONS[0] };
    // The repo's own instructions when Nitely watches it, else the general workflow.
    let instructions = match &server.remote {
        Some(r) => remote_instructions(r),
        None => Config::load()
            .ok()
            .and_then(|c| nitely_core::instructions::for_dir(&c, &server.cwd))
            .unwrap_or_else(|| nitely_core::instructions::WORKFLOW.to_string()),
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nitely", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions
    })
}

/// What an agent on another machine is told: it reaches the backlog, not the repo flow.
fn remote_instructions(r: &Remote) -> String {
    let project = r.project.as_deref().map(|p| format!(" Its default project is {p}.")).unwrap_or_default();
    format!(
        "This is Nitely's backlog, reached over the local network as `{name}`.{project} Read the project's brief with \
`get_project` first, use `next_task` to claim work, `report` what you are doing, `file_issue` what you find (it refuses likely duplicates), `note` with handoff: true \
before you stop, and `ask` when only the human can decide. Starting worktrees, the gate, submit and land run on the \
machine that holds the repo, so they are not available here: do the work in your own checkout and say in your \
handoff note what branch it is on. Link every issue you name with the `url` the tools return. Never set Approved: \
only the human approves.",
        name = r.name
    )
}

/// The tools this session may call: all of them here, the backlog ones from another machine.
fn tools_for(server: &Server) -> Value {
    let all = tools();
    if server.remote.is_none() {
        return all;
    }
    let kept: Vec<Value> = all
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| !t.get("name").and_then(Value::as_str).is_some_and(|n| LOCAL_ONLY.contains(&n)))
        .cloned()
        .collect();
    json!(kept)
}

fn tools() -> Value {
    let states: Vec<&str> = STATES.iter().map(|(n, _)| *n).filter(|n| *n != "Approved").collect();
    json!([
        {
            "name": "next_task",
            "description": "Claim the most urgent Todo issue in this repo's project and get it with a suggested branch name. Two agents never get the same issue. Issues with an open blocker are skipped, and a blocker is as urgent as what it blocks. Among equally urgent issues, those of an active goal come first, the nearest target date first. Returns null when nothing is ready.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "Project key; defaults to this repo's project." }
            } }
        },
        {
            "name": "start_task",
            "description": "Create the issue's branch and worktree from the integration branch, following the repo's playbook (.nitely/playbook.toml), and run its setup commands. Safe to call again: it returns the existing worktree. Work only inside the returned worktree.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string", "description": "e.g. NIT-12" }
            } }
        },
        {
            "name": "widen",
            "description": "Add paths to a sparse light worktree: each file's directory, or the directory itself, so you can edit there. A whole worktree already has every path. A path in a submodule the playbook declares goes to that submodule: its worktree is made now when the task has none, sparse when the playbook says so.",
            "inputSchema": { "type": "object", "required": ["paths"], "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Relative to the repo root, e.g. [\"src/billing\", \"src/ui/Meter.tsx\"]." },
                "key": { "type": "string", "description": "The issue whose worktree to widen; defaults to the worktree you are in." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." }
            } }
        },
        {
            "name": "run_gate",
            "description": "Run the playbook's gate commands (tests, build) in your worktree and record the result on the issue. Stops at the first failure and returns the end of its output. Pass before you open a pull request.",
            "inputSchema": { "type": "object", "properties": {
                "key": { "type": "string", "description": "The issue; defaults to the one your branch names." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." }
            } }
        },
        {
            "name": "run",
            "description": "Run a build or test command in one of the repo's build lanes (docs/build-lanes.md) instead of your worktree: Nitely snapshots your worktree as it is now, committed or not (your files, index and branch are left as they are), runs the command in a warm checkout that keeps its build output, and returns the exit code, the end of the output and the log's path. Waits for a free lane and for the command to end. Only for a repo whose playbook has [lanes].",
            "inputSchema": { "type": "object", "required": ["command"], "properties": {
                "command": { "type": "string", "description": "As you would type it in the worktree, e.g. `cargo test -p nitely-core`." },
                "key": { "type": "string", "description": "The issue; defaults to the one your branch names." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." }
            } }
        },
        {
            "name": "run_status",
            "description": "A build lane run's record (state queued, running, done, lane_error or abandoned; exit code; output; log), by the id `run` returned. Without an id, your last runs. With wait_seconds, waits up to that long (at most 600) for a queued or running one to end.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "integer" },
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": 600 }
            } }
        },
        {
            "name": "submit",
            "description": "Hand in a finished branch and move on. It returns at once with a job id; in the background Nitely runs the gate, has another vendor review it, decides whether the human must approve, pushes, opens or updates the pull request, and moves the issue (In Review, Needs You, or back to In Progress when the review finds P0/P1 problems). The result comes back through next_task, submit_status, or a notice on your next nitely call. Commit everything first. With wait: true it blocks until the result instead (minutes), as before.",
            "inputSchema": { "type": "object", "properties": {
                "key": { "type": "string", "description": "The issue; defaults to the one your branch names." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." },
                "wait": { "type": "boolean", "description": "Wait for the gate and review (up to 5 minutes) and return the full result; past that, the job goes on and submit_status gives its result." }
            } }
        },
        {
            "name": "submit_status",
            "description": "The result of a submit: the job's state, and once it finished, what submit returned (the issue's new state and `next`). Pass the job id from submit, or the issue key for its latest submit. wait_seconds (up to 600) waits for it to finish.",
            "inputSchema": { "type": "object", "properties": {
                "job": { "type": "integer" },
                "key": { "type": "string" },
                "wait_seconds": { "type": "integer" }
            } }
        },
        {
            "name": "land",
            "description": "Land a submitted issue on the integration branch: one land at a time, rebase onto the integration branch, check that the review covers exactly this change (and that the human approved it when the playbook says so), run the gate, fast-forward, push, clean up the worktree, and move the issue to Done. A rebase conflict is left in progress for you to resolve. With `[land] batch` in the playbook, a change that arrives while another land runs waits in the repo's land queue and lands together with what else waited (stacked, gated once in a build lane, pushed once); land returns once it landed or was sent back with why. A lock left by a land whose process is gone is taken over on its own; break_lock takes over one whose process still runs, only when you know no land is running.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string" },
                "break_lock": { "type": "boolean", "description": "Take over the land lock even though the process that took it still runs (a land that hangs, or another program with its old number)." }
            } }
        },
        {
            "name": "get_issue",
            "description": "An issue with its full timeline (state changes, notes, handoffs, questions and answers), the issues it is blocked by and blocks, and the pull requests whose branch names it, with their CI checks.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string", "description": "e.g. NIT-12" }
            } }
        },
        {
            "name": "list_issues",
            "description": "Open issues of a project, most urgent first.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string" },
                "include_closed": { "type": "boolean" },
                "label": { "type": "string", "description": "Only issues with this label." }
            } }
        },
        {
            "name": "setup_guide",
            "description": "The step-by-step first-run sweep that seeds a project's board from its repo: pre-flight checks (integration branch, gate, working tree), sweeping areas, filing issues, proposing review rules, writing the brief. Call it when asked to set up or onboard a project, or when get_project shows setup is null, and follow it.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "goals",
            "description": "A project's goals: the outcomes it works toward, with how many of each one's issues are done, open or dropped. Give id for one goal with all its issues. Active goals only unless include_closed.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "integer" },
                "include_closed": { "type": "boolean" },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "propose_goal",
            "description": "Draft a goal and the issues that reach it from what the person asked for, for them to decide in the Inbox: approving creates the goal and files every issue into it in one batch (they may edit or leave items out first); rejecting files nothing. Checked as file_issues checks a batch (refs, blockers, cycles) before anything is filed, but not held to its 10 items: the person decides the plan as one. Keep it to what one goal needs; a plan of more than about 30 issues is likely two goals. List the issues in the order to work them; each names the files it touches and its size. Pass planning_issue when you were handed a \"Plan a goal\" issue; it closes once the plan is filed. Do not file the issues yourself.",
            "inputSchema": { "type": "object", "required": ["sentence", "title", "outcome"], "properties": {
                "sentence": { "type": "string", "description": "What the person asked for, in their words." },
                "title": { "type": "string" },
                "outcome": { "type": "string", "description": "One paragraph: what is true once the goal is reached." },
                "target_date": { "type": "string", "description": "YYYY-MM-DD, only if the person gave one." },
                "issues": { "type": "array", "items": { "type": "object", "required": ["ref", "title", "kind", "body", "evidence"], "properties": {
                    "ref": { "type": "string" },
                    "title": { "type": "string" },
                    "kind": { "type": "string", "enum": KINDS },
                    "body": { "type": "string" },
                    "evidence": { "type": "string" },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 4 },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Refs of this plan or existing issue keys." },
                    "owner": { "type": "string", "enum": OWNERS },
                    "state": { "type": "string", "enum": ["Todo", "Backlog", "Needs You"] },
                    "touches": { "type": "array", "items": { "type": "string" }, "description": "The files it will change (src/App.tsx), so parallel workers stay apart. Required except for a live check or the person's own task." },
                    "size": { "type": "string", "enum": ["S", "M", "L"], "description": "S: an hour or two, M: a day, L: more." }
                } } },
                "existing": { "type": "array", "items": { "type": "string" }, "description": "Open issues of the project that belong in the goal." },
                "planning_issue": { "type": "string" },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "save_goal",
            "description": "Create a goal (no id: title and outcome needed) or change one: title, outcome (one paragraph on what is true once it is reached), target_date (YYYY-MM-DD, empty clears it), state (active, done, dropped). The person orders goals and sets how many crew workers each may have, in the app. Put issues in it with goal on file_issue, file_issues or update_issue.",
            "inputSchema": { "type": "object", "properties": {
                "id": { "type": "integer" },
                "title": { "type": "string" },
                "outcome": { "type": "string" },
                "target_date": { "type": "string" },
                "state": { "type": "string", "enum": ["active", "done", "dropped"] },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "get_project",
            "description": "The project's brief (what earlier agents learned about it: vision, state of each area, target order, open questions for the owner) and its setup marker (null until the first-run sweep has run; otherwise the commit it read). Read it before exploring the repo.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string" }
            } }
        },
        {
            "name": "set_project_brief",
            "description": "Replace the project's brief (Markdown). Pass the revision get_project returned; it is refused when someone, such as the human, edited it since, so read it again and merge. Mark what you inferred rather than read as inference.",
            "inputSchema": { "type": "object", "required": ["brief", "revision"], "properties": {
                "brief": { "type": "string" },
                "revision": { "type": "integer", "description": "The revision you read; 0 when the project has no brief yet." },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "mark_setup_complete",
            "description": "Record that the first-run setup sweep of this project is done, with the commit it read (`git rev-parse` of the integration branch). A refresh calls it again with the new commit.",
            "inputSchema": { "type": "object", "required": ["sha"], "properties": {
                "sha": { "type": "string" },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "propose_playbook_change",
            "description": "Propose a change to the repo's playbook (.nitely/playbook.toml or the one Nitely keeps): the integration branch, gate commands, review focus and flags, approval paths. You never change the rules yourself: this files an issue in Needs You with the diff and your reasons, and only the human's approval puts it in effect. Refused when the result would not parse. A proposal while another is pending for the repo replaces it with a new issue that carries both changes.",
            "inputSchema": { "type": "object", "required": ["change", "why"], "properties": {
                "change": { "type": "string", "description": "TOML. With mode merge (the default), a fragment merged into the playbook, e.g. a [branch] table with integration = \"main\": tables merge key by key, other values (arrays too) are replaced. With mode replace, the whole new playbook (also how to remove a key)." },
                "why": { "type": "string", "description": "Open with one short sentence that sums up the change: it becomes the title of the person's decision. Then what is wrong now and the evidence, for the person deciding." },
                "mode": { "type": "string", "enum": ["merge", "replace"] },
                "project": { "type": "string" }
            } }
        },
        {
            "name": "file_issue",
            "description": "File a finding, follow-up or idea you will not handle now. Refused when an issue, open or closed, looks like the same work: a similar title, or bodies naming the same files (it lists them and why); set allow_similar only if it is really different.",
            "inputSchema": { "type": "object", "required": ["title", "kind", "body", "evidence"], "properties": {
                "title": { "type": "string" },
                "kind": { "type": "string", "enum": KINDS },
                "body": { "type": "string", "description": "What is wrong or wanted, and why. Include file paths and the branch or commit you saw it on." },
                "evidence": { "type": "string", "description": "What you saw or ran; \"none yet\" for an idea." },
                "priority": { "type": "integer", "minimum": 0, "maximum": 4, "description": "1 urgent, 2 high, 3 medium, 4 low, 0 none; the priority rubric in the instructions says which" },
                "state": { "type": "string", "enum": ["Todo", "Backlog", "Needs You"], "description": "Todo by default; Needs You by default for owner human." },
                "owner": { "type": "string", "enum": OWNERS, "description": "human for work only the human can do (buy hardware, sign a contract, use a console only they can reach); next_task never hands it out. Defaults to agent." },
                "project": { "type": "string" },
                "allow_similar": { "type": "boolean" },
                "goal": { "type": "integer", "description": "An active goal of the project to file it into (the goals tool lists them)." },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Issues that must close before this one can start, e.g. [\"NIT-9\"]." },
                "labels": { "type": "array", "items": { "type": "string" }, "description": "Areas and platforms, e.g. [\"ios\", \"billing\"]: reuse the project's labels (list_issues shows them); kind and priority have their own fields." }
            } }
        },
        {
            "name": "file_issues",
            "description": "File up to 10 issues in one call, e.g. to seed a board in batches of 10: more is refused and files nothing. Give each a ref; its blocked_by may name refs of the same batch as well as existing issues. Refs are local to one call, so file blockers in an earlier batch and name them by key in later ones. Nitely files the batch in one transaction, blockers first; an unknown ref, a cycle or an invalid item files nothing. An item that looks like an existing issue, open or closed (or an earlier item), is skipped, unless it sets allow_similar, and items that named it are blocked by that issue instead. Returns each ref's key, what was filed and what was skipped.",
            "inputSchema": { "type": "object", "required": ["issues"], "properties": {
                "project": { "type": "string", "description": "Project key for every item; defaults to this repo's project." },
                "issues": { "type": "array", "items": { "type": "object", "required": ["ref", "title", "kind", "body", "evidence"], "properties": {
                    "ref": { "type": "string", "description": "A name for this item that other items' blocked_by can use, e.g. \"schema\"." },
                    "title": { "type": "string" },
                    "kind": { "type": "string", "enum": KINDS },
                    "body": { "type": "string" },
                    "evidence": { "type": "string" },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 4 },
                    "state": { "type": "string", "enum": ["Todo", "Backlog", "Needs You"] },
                    "owner": { "type": "string", "enum": OWNERS },
                    "allow_similar": { "type": "boolean" },
                    "goal": { "type": "integer", "description": "An active goal of the project to file it into (the goals tool lists them)." },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Refs of this batch or existing issue keys." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "Areas and platforms; reuse the project's labels." }
                } } }
            } }
        },
        {
            "name": "update_issue",
            "description": "Move an issue, set its priority, rewrite its title or body (what it said before stays on its timeline; an issue mirrored from another tracker is changed there), change what blocks it, or make it the human's own work (owner human: next_task skips it, and an open issue goes to Needs You; only the human hands it back). Done needs evidence (what ran and what it showed), and is refused while a submitted change is not landed or a question on the issue is open; Canceled and Duplicate need a reason. Approved is set only by the human; move to Needs You and ask instead. An issue with an open blocker waits: next_task skips it until every blocker is Done or Canceled.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string" },
                "state": { "type": "string", "enum": states },
                "evidence": { "type": "string" },
                "reason": { "type": "string" },
                "title": { "type": "string", "description": "A new title; the old one stays on the timeline." },
                "priority": { "type": "integer", "minimum": 0, "maximum": 4, "description": "1 urgent, 2 high, 3 medium, 4 low, 0 none; the change goes on the timeline." },
                "body": { "type": "string", "description": "A new body, whole: it replaces the old one, which stays on the timeline." },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Issues to add as blockers of this one." },
                "remove_blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Blockers to remove." },
                "owner": { "type": "string", "enum": ["human"], "description": "human makes it the human's own work." },
                "goal": { "type": "integer", "description": "Put it in this active goal of its project; 0 takes it out of its goal." },
                "labels": { "type": "array", "items": { "type": "string" }, "description": "Labels to add (areas and platforms)." },
                "remove_labels": { "type": "array", "items": { "type": "string" }, "description": "Labels to remove." }
            } }
        },
        {
            "name": "report",
            "description": "One line on what you are doing now. Shows live on the human's board; also logged on the issue's timeline when you pass key.",
            "inputSchema": { "type": "object", "required": ["text"], "properties": {
                "text": { "type": "string", "description": "e.g. \"running the test gate\"" },
                "key": { "type": "string" }
            } }
        },
        {
            "name": "note",
            "description": "Add a note to an issue's timeline. Set handoff for the note the next agent needs to pick the work up: what is done, what is left, what you were unsure about.",
            "inputSchema": { "type": "object", "required": ["key", "text"], "properties": {
                "key": { "type": "string" },
                "text": { "type": "string" },
                "handoff": { "type": "boolean" }
            } }
        },
        {
            "name": "ask",
            "description": "Ask the human something only they can decide: a product question (scope, wording, what to build), something irreversible or outside this machine (credentials, accounts, licenses, signing, money), or a task only they can do. It goes to their inbox. Returns an id; check it with get_answer. Keep working on whatever does not depend on the answer. Never ask for an approval: when a change needs one, submit already puts it in their inbox. A question about the board or the code (who holds a file, whether an issue is done, whether a finding is stale, resubmit or wait) goes to triage instead.",
            "inputSchema": { "type": "object", "required": ["question"], "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" } },
                "key": { "type": "string", "description": "The issue it is about." }
            } }
        },
        {
            "name": "request_acceptance",
            "description": "Ask the human to accept one P0 or P1 finding of the issue's latest review as it is: a trade-off they decided or should decide, not a defect you can fix (for example a reviewer asking again for a migration they ruled out). It shows in their inbox and on their phone as one decision, the finding with your reasoning, Accept or Send back, and the issue waits in Needs You. Check the answer with get_answer: \"Accepted: ...\" means land honors it and the next review round is told; \"Not accepted: ...\" means fix it. Do not use it for a finding you can fix.",
            "inputSchema": { "type": "object", "required": ["key", "finding", "why"], "properties": {
                "key": { "type": "string", "description": "The issue." },
                "finding": { "type": "string", "description": "The finding: its number in the latest review's findings (from 1) or its exact text." },
                "why": { "type": "string", "description": "Why it should stand as it is: the decision it follows from, and what fixing it would cost. The human reads this." }
            } }
        },
        {
            "name": "triage",
            "description": "Put a question about the board or the code to triage instead of the human: who holds a file, whether an issue is already done or another worker has it, whether a carried review finding still applies, resubmit or wait, which issue a change belongs to. A triage agent answers it from the board and the repository, read-only, and records the answer as a note on the issue. Only if the question turns out to be the human's (a product question, something irreversible, accepting a finding) does it go to their inbox, with what triage found. Returns at once with an id; the answer comes as a notice on your next nitely call, or from get_triage.",
            "inputSchema": { "type": "object", "required": ["question"], "properties": {
                "question": { "type": "string", "description": "What you want to know, with what you already checked." },
                "key": { "type": "string", "description": "The issue it is about." }
            } }
        },
        {
            "name": "get_triage",
            "description": "Triage's answer to a question you put to it: its state (open, answered, escalated to the human's inbox, failed) and answer. wait_seconds (up to 600) waits for it.",
            "inputSchema": { "type": "object", "required": ["id"], "properties": {
                "id": { "type": "integer" },
                "wait_seconds": { "type": "integer" }
            } }
        },
        {
            "name": "get_answer",
            "description": "The human's answer to an ask, or null while they have not answered.",
            "inputSchema": { "type": "object", "required": ["id"], "properties": {
                "id": { "type": "integer" }
            } }
        }
    ])
}

fn call_tool(server: &Server, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match run_tool(server, name, &args, params) {
        Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
        // This server is older than the database: the newer build that upgraded it is usually
        // being installed this moment, so the call goes there once it is (NIT-327).
        Err(e) if e.chain().any(|c| c.is::<nitely_core::db::NewerDatabase>()) => match forward_once_installed(server, params) {
            Ok(forwarded) => forwarded,
            Err(why) => json!({ "content": [{ "type": "text", "text": format!("{e:#}. {why}") }], "isError": true }),
        },
        Err(e) => json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
    }
}

/// How long a server older than the database waits for a newer build to be installed at its path
/// before it refuses: an install upgrades the database a moment before it puts the new build in
/// place. `NITELY_MCP_INSTALL_WAIT` (seconds) sets it, for tests.
fn install_wait() -> Duration {
    let secs = std::env::var("NITELY_MCP_INSTALL_WAIT").ok().and_then(|s| s.parse().ok()).unwrap_or(90);
    Duration::from_secs(secs).min(wait_cap())
}

/// The installed build this server last waited for in vain: it does not wait again for the same
/// one, so with nothing newer coming each call refuses at once rather than after `install_wait`.
static WAITED_IN_VAIN: Mutex<Option<(u64, SystemTime)>> = Mutex::new(None);

/// The call forwarded to a newer build at this server's path, waiting `install_wait` for one to
/// appear; why not, when none does or it cannot take the call.
fn forward_once_installed(server: &Server, params: &Value) -> std::result::Result<Value, String> {
    let Some((exe, stamp)) = server.exe.as_ref() else {
        return Err("Install the newer version.".into());
    };
    let installed = || nitely_core::install::stamp(exe);
    let deadline = std::time::Instant::now() + install_wait();
    loop {
        let now_installed = installed();
        if now_installed.as_ref().is_some_and(|s| s != stamp) {
            return forward(server, params).ok_or_else(|| format!("The build installed at {} could not take the call either.", exe.display()));
        }
        if now_installed.is_some() && *lock(&WAITED_IN_VAIN) == now_installed {
            break;
        }
        if std::time::Instant::now() >= deadline {
            *lock(&WAITED_IN_VAIN) = now_installed;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "This MCP server runs the build installed at {}, which is older than the database, and nothing newer is installed there: install the newer version and the next call goes to it.",
        exe.display()
    ))
}

/// When the nitely this server started from has been replaced by a new build since (`nitely
/// install`, which land runs for Nitely itself), runs the call in that build: an agent that has
/// been running for hours gets the landed fixes without restarting. None when the build is the
/// same, or the new one cannot take the call; then it runs here.
fn forward(server: &Server, params: &Value) -> Option<Value> {
    let (exe, stamp) = server.exe.as_ref()?;
    let installed = nitely_core::install::stamp(exe)?;
    if &installed == stamp {
        return None;
    }
    let mut result = match forwarder(exe, installed, &server.cwd) {
        Some(f) => f.call(server, params).or_else(|| forward_once(server, params, exe))?,
        None => forward_once(server, params, exe)?,
    };
    // Says which build answered, for whoever debugs an agent's session.
    result["_meta"] = json!({ "nitely_forwarded": exe.to_string_lossy() });
    Some(result)
}

/// The newer build this server forwards to, as one child that stays: `nitely mcp-calls` takes the
/// calls on its stdin, runs each on a thread of its own and answers each by id when it ends. One
/// process for all of a server's calls, rather than one per call (NIT-308).
struct Forwarder {
    stamp: (u64, SystemTime),
    stdin: Mutex<std::process::ChildStdin>,
    waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    next: AtomicU64,
    /// Its stdout closed: it exited.
    gone: Arc<AtomicBool>,
    _child: std::process::Child,
}

static FORWARDER: Mutex<Option<Arc<Forwarder>>> = Mutex::new(None);

/// The forwarding child for the build installed now, started when there is none for it yet.
fn forwarder(exe: &std::path::Path, stamp: (u64, SystemTime), cwd: &str) -> Option<Arc<Forwarder>> {
    let mut slot = lock(&FORWARDER);
    if let Some(f) = slot.as_ref().filter(|f| f.stamp == stamp && !f.gone.load(Ordering::SeqCst)) {
        return Some(Arc::clone(f));
    }
    let mut cmd = Command::new(exe);
    cmd.arg("mcp-calls").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if std::path::Path::new(cwd).is_dir() {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().ok()?;
    let (stdin, stdout) = (child.stdin.take()?, child.stdout.take()?);
    let waiting: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> = Arc::default();
    let gone = Arc::new(AtomicBool::new(false));
    let (answers, done) = (Arc::clone(&waiting), Arc::clone(&gone));
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(reply) = serde_json::from_str::<Value>(&line) else { continue };
            let Some(id) = reply.get("id").and_then(Value::as_u64) else { continue };
            if let Some(waiter) = lock(&answers).remove(&id) {
                waiter.send(reply.get("result").cloned().unwrap_or(Value::Null)).ok();
            }
        }
        // It exited: whoever still waits runs the call here instead.
        done.store(true, Ordering::SeqCst);
        lock(&answers).clear();
    });
    let f = Arc::new(Forwarder { stamp, stdin: Mutex::new(stdin), waiting, next: AtomicU64::new(1), gone, _child: child });
    *slot = Some(Arc::clone(&f));
    Some(f)
}

impl Forwarder {
    /// The call's result from the child; None when the child cannot take it (it exited).
    fn call(&self, server: &Server, params: &Value) -> Option<Value> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.waiting).insert(id, tx);
        let request = json!({ "id": id, "vendor": server.vendor, "cwd": server.cwd, "params": params });
        if self.gone.load(Ordering::SeqCst) || writeln!(lock(&self.stdin), "{request}").is_err() {
            lock(&self.waiting).remove(&id);
            return None;
        }
        let result = rx.recv().ok()?;
        (!result.is_null()).then_some(result)
    }
}

/// `forward` through a process of its own for this one call: for a newer build without
/// `mcp-calls`, or when its child exited.
fn forward_once(server: &Server, params: &Value, exe: &std::path::Path) -> Option<Value> {
    let mut cmd = Command::new(exe);
    cmd.arg("mcp-call").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    if std::path::Path::new(&server.cwd).is_dir() {
        cmd.current_dir(&server.cwd);
    }
    let mut child = cmd.spawn().ok()?;
    let request = json!({ "vendor": server.vendor, "cwd": server.cwd, "params": params });
    child.stdin.take()?.write_all(request.to_string().as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// `nitely mcp-calls`: the calls an older `nitely mcp` forwards to this build, one JSON object
/// per line on stdin ({id, vendor, cwd, params}). Each runs on a thread of its own and is answered
/// as {id, result} when it ends. When the server that started it closes its end (it exited, or a
/// newer build replaced this one), what is still running finishes first, then the process ends.
pub fn serve_calls() -> Result<()> {
    let out = Arc::new(Mutex::new(std::io::stdout()));
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let out = Arc::clone(&out);
        let busy = Busy::start();
        std::thread::spawn(move || {
            let _busy = busy;
            let server = Server { vendor: str_arg(&request, "vendor").unwrap_or("agent").to_string(), cwd: str_arg(&request, "cwd").unwrap_or(".").to_string(), exe: None, remote: None };
            let result = call_tool(&server, request.get("params").unwrap_or(&json!({})));
            send(&out, &json!({ "id": request.get("id"), "result": result }));
        });
    }
    finish_in_flight();
    Ok(())
}

/// `nitely mcp-call`: runs one tool call given on stdin as {vendor, cwd, params} and prints its
/// result. An older `nitely mcp` forwards calls here once a newer build is installed.
pub fn call_once() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let request: Value = serde_json::from_str(&input).context("mcp-call takes {vendor, cwd, params} on stdin")?;
    let server = Server {
        vendor: str_arg(&request, "vendor").unwrap_or("agent").to_string(),
        cwd: req(&request, "cwd")?.to_string(),
        exe: None,
        remote: None,
    };
    let result = call_tool(&server, request.get("params").unwrap_or(&json!({})));
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn req<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(args, key).ok_or_else(|| anyhow!("missing {key}"))
}

/// A list of issue keys; a single string counts as a list of one.
fn keys_arg(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// The call's issue keys and project as they are now: a key or project from before a project
/// changed its key (an agent's notes, an old branch, a cached token) names the same thing.
fn current_args(db: &Db, args: &Value) -> Result<Value> {
    let aliases = db.project_aliases()?;
    let mut args = args.clone();
    if aliases.is_empty() {
        return Ok(args);
    }
    let current = |s: &mut String| *s = nitely_core::projects::current_key(&aliases, s);
    if let Some(Value::String(s)) = args.get_mut("key") {
        current(s);
    }
    for field in ["blocked_by", "remove_blocked_by"] {
        match args.get_mut(field) {
            Some(Value::Array(list)) => list.iter_mut().for_each(|v| {
                if let Value::String(s) = v {
                    current(s)
                }
            }),
            Some(Value::String(s)) => current(s),
            _ => {}
        }
    }
    if let Some(Value::String(p)) = args.get_mut("project") {
        if let Some(now) = aliases.get(&p.trim().to_uppercase()) {
            *p = now.clone();
        }
    }
    Ok(args)
}

/// The worktree a run_gate or submit call is about: the one it names, else the one this session is
/// in, unless the key names an issue whose worktree is elsewhere (another task's, or in the repo
/// linked to the issue's project).
fn task_dir(config: &Config, db: &Db, args: &Value, cwd: &str) -> Result<String> {
    if let Some(worktree) = str_arg(args, "worktree") {
        return Ok(worktree.to_string());
    }
    let Some(key) = str_arg(args, "key") else { return Ok(cwd.to_string()) };
    if playbook::current_worktree(cwd).is_ok_and(|(_, k)| k.is_some_and(|k| k.eq_ignore_ascii_case(key))) {
        return Ok(cwd.to_string());
    }
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    let root = playbook::repo_for_issue(config, issue.project.as_deref(), cwd)?;
    match playbook::issue_worktree(&root, &issue.key)? {
        Some(worktree) => Ok(worktree.to_string_lossy().into_owned()),
        // The call comes from that repo: it means this checkout, as before.
        None if playbook::repo_root(config, cwd).is_some_and(|r| paths::normalize(&r.to_string_lossy()) == paths::normalize(&root.to_string_lossy())) => Ok(cwd.to_string()),
        None => bail!("{key} has no worktree in {}; call start_task first", root.display()),
    }
}

/// A file_issue call, or one item of file_issues, as a new issue in `project`.
fn new_issue(args: &Value, project: &str, vendor: &str) -> Result<NewIssue> {
    Ok(NewIssue {
        project: project.to_string(),
        title: req(args, "title")?.into(),
        kind: req(args, "kind")?.into(),
        body: req(args, "body")?.into(),
        evidence: req(args, "evidence")?.into(),
        source: if SOURCES.contains(&vendor) { vendor.to_string() } else { "claude".into() },
        priority: args.get("priority").and_then(Value::as_i64).unwrap_or(0),
        allow_similar: args.get("allow_similar").and_then(Value::as_bool).unwrap_or(false),
        state: str_arg(args, "state").map(str::to_string),
        owner: str_arg(args, "owner").map(str::to_string),
        goal: args.get("goal").and_then(Value::as_i64).filter(|g| *g > 0),
        blocked_by: keys_arg(args, "blocked_by"),
        labels: keys_arg(args, "labels"),
    })
}

/// When a call brings labels the project has not used before, a note naming the labels it already
/// uses (and any close to the new ones), so agents keep one vocabulary.
fn label_note(before: &[(String, usize)], given: &[String]) -> Option<String> {
    let known: Vec<&str> = before.iter().map(|(l, _)| l.as_str()).collect();
    let new: Vec<String> = given.iter().filter_map(|l| nitely_core::issues::normalize_label(l)).filter(|l| !known.contains(&l.as_str())).collect();
    if new.is_empty() {
        return None;
    }
    let mut note = format!("New label{} for this project: {}.", if new.len() == 1 { "" } else { "s" }, new.join(", "));
    let close: Vec<&str> = known.iter().copied().filter(|k| new.iter().any(|n| n.contains(k) || k.contains(n.as_str()))).collect();
    if !close.is_empty() {
        note.push_str(&format!(" Close to labels it already uses: {}; use those if they mean the same.", close.join(", ")));
    }
    if !before.is_empty() {
        let used: Vec<String> = before.iter().take(20).map(|(l, n)| format!("{l} ({n})")).collect();
        note.push_str(&format!(" In use: {}.", used.join(", ")));
    }
    Some(note)
}

/// `params` are the call's, whose `_meta` says which session made it (see `Server::session`).
fn run_tool(server: &Server, name: &str, args: &Value, params: &Value) -> Result<Value> {
    run_tool_as(server, name, args, params, None)
}

/// `run_tool`; `detached` is set in the process `run_apart` started: the session the server
/// resolved for the call, and the long work (a land, a lane run) runs right here.
fn run_tool_as(server: &Server, name: &str, args: &Value, params: &Value, detached: Option<&str>) -> Result<Value> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let args = &current_args(&db, args)?;
    let named = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let session = &match detached {
        Some(s) => s.to_string(),
        None => server.claude_session(params, &db, named.as_deref()).unwrap_or_else(|| server.session(params)),
    };
    let actor = server.actor_for(session);
    if server.remote.is_some() && LOCAL_ONLY.contains(&name) {
        bail!("{name} runs on the machine that holds the repo, so it is not available over the local network; do the work in your own checkout");
    }
    let project = |db: &Db| match &server.remote {
        // Another machine has no repo here to name the project: the call or its token does.
        Some(r) => str_arg(args, "project")
            .map(str::to_uppercase)
            .or_else(|| r.project.clone())
            .with_context(|| format!("name the project: the token {} has no default project", r.name))
            // A token made before its project changed key still names the old one.
            .and_then(|p| Ok(db.project(&p)?.map(|x| x.key).unwrap_or(p))),
        None => resolve_project(&config, db, str_arg(args, "project"), &server.cwd),
    };

    // A submit that came back to this agent is told before anything else (NIT-212).
    let returned = if name == "next_task" && server.remote.is_none() {
        // Only one of the project asked for: an agent working across projects gets this one's work.
        db.returned_submit(actor.id(), &project(&db)?)?
    } else {
        None
    };
    let mut out = match name {
        "next_task" if returned.is_some() => {
            let job = returned.as_ref().unwrap();
            db.mark_submit_jobs_delivered(&[job.id])?;
            json!({
                "issue": db.issue(&job.issue)?,
                "returned": job.outcome(),
                "job": job.id,
                "worktree": job.worktree,
                "next": "Your submit came back: fix what it says in the same worktree (start_task with this key if the worktree is gone), commit, and submit again."
            })
        }
        "next_task" => {
            let project = project(&db)?;
            match nitely_core::overlap::next_task(&config, &mut db, &project, actor.id(), &[])? {
                Some(nitely_core::overlap::Picked { issue, why }) => {
                    let root = if server.remote.is_some() { None } else { playbook::repo_for_issue(&config, issue.project.as_deref(), &server.cwd).ok() };
                    let branch = match root {
                        Some(root) => Playbook::load(&root)?.branch_for(&server.vendor, &issue),
                        None => branch_name(&server.vendor, &issue.key, &issue.title),
                    };
                    let mut out = json!({
                        "issue": issue,
                        "branch": branch,
                        "next": "Call start_task with this key for the branch and worktree, work there, report as you go, call submit when it is committed, and leave a handoff note before you stop."
                    });
                    // Why this one: work in flight holds files a more urgent issue would touch.
                    if let Some(why) = why {
                        out["why"] = json!(why);
                    }
                    out
                }
                None => {
                    let waiting: Vec<String> = db
                        .list_issues(Some(&project), false)?
                        .into_iter()
                        .filter(|i| i.state == "Todo" && !i.blocked_by.is_empty())
                        .map(|i| format!("{} (blocked by {})", i.key, i.blocked_by.join(", ")))
                        .collect();
                    let message = if waiting.is_empty() {
                        format!("Nothing in Todo for {project}.")
                    } else {
                        format!("Nothing ready in {project}; waiting on blockers: {}.", waiting.join("; "))
                    };
                    json!({ "issue": null, "message": message })
                }
            }
        }
        "start_task" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            let root = playbook::repo_for_issue(&config, issue.project.as_deref(), &server.cwd)?;
            json!(playbook::start_task(&db, &root, &issue, &server.vendor, &actor)?)
        }
        "widen" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            let (worktree, _) = playbook::current_worktree(&dir)?;
            json!(nitely_core::light::widen(&worktree, &keys_arg(args, "paths"))?)
        }
        "run_gate" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            let (worktree, branch_key) = playbook::current_worktree(&dir)?;
            let key = str_arg(args, "key").map(str::to_string).or(branch_key);
            let root = playbook::repo_root(&config, &dir).with_context(|| format!("{dir} is not inside a git repo"))?;
            json!(playbook::run_gate(&db, &root, &worktree, key.as_deref(), &actor)?)
        }
        "run" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            let command = req(args, "command")?;
            if detached.is_some() {
                // Its id as soon as it is queued, where the server looks past the cap.
                let mut said = None;
                json!(nitely_core::lanes::run_in(&config, &db, &dir, str_arg(args, "key"), command, &actor, &mut |r| {
                    if said != Some(r.id) {
                        said = Some(r.id);
                        if let Some(answer) = ANSWER.get() {
                            write_whole(answer, json!({ "queued": r.id }).to_string().as_bytes()).ok();
                        }
                    }
                })?)
            } else {
                run_apart(server, session, params, |log, said| {
                    // Its id once queued, or from its answer if it ended just as the cap passed.
                    let queued = said.as_ref().and_then(|s| s.get("queued").or_else(|| s.pointer("/ok/id"))).and_then(Value::as_i64);
                    let run = match queued {
                        Some(id) => db.lane_run(id)?,
                        None => None,
                    };
                    Ok(match run {
                        Some(r) => {
                            let next = format!("Still {} after {}s: call run_status with id {} (and wait_seconds) for the result.", r.state, wait_cap().as_secs(), r.id);
                            let mut v = json!(r);
                            v["next"] = json!(next);
                            v
                        }
                        None => json!({ "still_running": true, "log": log.to_string_lossy(), "next": format!("Not queued yet after {}s; `nitely lanes` shows it once it is.", wait_cap().as_secs()) }),
                    })
                })?
            }
        }
        "run_status" => match args.get("id").and_then(Value::as_i64) {
            Some(id) => {
                let wait = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0).min(600).min(wait_cap().as_secs());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait);
                loop {
                    let run = db.lane_run(id)?.with_context(|| format!("no run {id}"))?;
                    let open = run.state == "queued" || run.state == "running";
                    if !open || std::time::Instant::now() >= deadline {
                        let mut out = json!(run);
                        if open && wait > 0 {
                            out["next"] = json!(format!("Still {} after {wait}s: call run_status with id {id} again.", run.state));
                        }
                        break out;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            None => json!(db.lane_runs_by(actor.id(), 5)?),
        },
        "submit" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            nitely_core::install::check_current(&config, &dir, env!("NITELY_COMMIT"))?;
            let opts = nitely_core::submit::SubmitOptions { dir: &dir, issue_key: str_arg(args, "key"), skip_review: false, local_only: false, batch: false };
            let runner = nitely_core::submit_job::DetachedProcess { nitely: std::env::current_exe()? };
            let job = nitely_core::submit_job::submit_async(&mut db, &config, &opts, &actor, &runner)?;
            if args.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                // The job runs in a process of its own, so it outlives this call and this server;
                // waiting on it here is only waiting.
                let job = nitely_core::submit_job::wait(&db, job.id, wait_cap())?;
                match (&job.result, job.finished()) {
                    (Some(result), true) => {
                        job_json(&db, &job, &actor)?;
                        result.clone()
                    }
                    (_, true) => job_json(&db, &job, &actor)?,
                    (_, false) => {
                        let mut v = job_json(&db, &job, &actor)?;
                        v["next"] = json!(format!("Still {} after {}s: call submit_status with id {} (and wait_seconds) for the result.", job.state, wait_cap().as_secs(), job.id));
                        v
                    }
                }
            } else {
                job_json(&db, &job, &actor)?
            }
        }
        "submit_status" => {
            let job = match (args.get("job").and_then(Value::as_i64), str_arg(args, "key")) {
                (Some(id), _) => db.submit_job(id)?.with_context(|| format!("no submit job {id}"))?,
                (None, Some(key)) => db.latest_submit_job(key)?.with_context(|| format!("{key} has no submit yet"))?,
                (None, None) => bail!("pass the job id submit returned, or the issue key"),
            };
            let seconds = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0).min(600).min(wait_cap().as_secs());
            let job = nitely_core::submit_job::wait(&db, job.id, std::time::Duration::from_secs(seconds))?;
            job_json(&db, &job, &actor)?
        }
        "land" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            let root = playbook::repo_for_issue(&config, issue.project.as_deref(), &server.cwd)?;
            nitely_core::install::check_current(&config, &root.to_string_lossy(), env!("NITELY_COMMIT"))?;
            if detached.is_some() {
                let break_lock = args.get("break_lock").and_then(Value::as_bool).unwrap_or(false);
                let opts = nitely_core::land::LandOptions { dir: &server.cwd, issue_key: key, push: true, keep: false, break_lock };
                json!(nitely_core::land::land(&mut db, &config, &opts, &actor)?)
            } else {
                run_apart(server, session, params, |log, _| {
                    Ok(json!({ "still_running": true, "log": log.to_string_lossy(), "next": format!("The land of {key} is still running after {}s, in a process of its own that goes on without this call. get_issue {key} shows when it landed or why it stopped.", wait_cap().as_secs()) }))
                })?
            }
        }
        "get_issue" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            json!({
                "issue": issue,
                "relations": db.relations(&issue.key)?,
                "timeline": db.timeline(&issue.key)?,
                "pull_requests": db.prs_for_issue(&issue.key)?,
                "proposal": db.proposal_for_issue(&issue.key)?
            })
        }
        "list_issues" => {
            let project = project(&db)?;
            let include_closed = args.get("include_closed").and_then(Value::as_bool).unwrap_or(false);
            let label = str_arg(args, "label").and_then(nitely_core::issues::normalize_label);
            let issues: Vec<_> = db.list_issues(Some(&project), include_closed)?.into_iter().filter(|i| label.as_ref().is_none_or(|l| i.labels.contains(l))).collect();
            json!(issues)
        }
        "setup_guide" => json!({ "guide": nitely_core::instructions::SETUP_GUIDE }),
        "goals" => match args.get("id").and_then(Value::as_i64) {
            Some(id) => json!(db.goal_detail(id)?),
            None => json!(db.goals(&project(&db)?, args.get("include_closed").and_then(Value::as_bool).unwrap_or(false))?),
        },
        "propose_goal" => {
            let plan = nitely_core::goal_plans::GoalPlan {
                sentence: req(args, "sentence")?.into(),
                title: req(args, "title")?.into(),
                outcome: req(args, "outcome")?.into(),
                target_date: str_arg(args, "target_date").map(str::to_string),
                issues: serde_json::from_value(args.get("issues").cloned().unwrap_or(json!([]))).context("issues: each needs ref, title, kind, body and evidence")?,
                existing: keys_arg(args, "existing"),
                planning_issue: None,
            };
            json!(db.propose_goal(&project(&db)?, &plan, str_arg(args, "planning_issue"), &actor)?)
        }
        "save_goal" => {
            let change = nitely_core::goals::GoalChange {
                title: str_arg(args, "title").map(str::to_string),
                outcome: str_arg(args, "outcome").map(str::to_string),
                // An empty string clears the date, so it is read as given.
                target_date: args.get("target_date").and_then(Value::as_str).map(str::to_string),
                state: str_arg(args, "state").map(str::to_string),
                ..Default::default()
            };
            let id = args.get("id").and_then(Value::as_i64);
            let project = match id {
                Some(_) => String::new(),
                None => project(&db)?,
            };
            json!(db.save_goal(&project, id, &change, &actor)?)
        }
        "get_project" => json!(db.project_brief(&project(&db)?)?),
        "set_project_brief" => {
            let revision = args.get("revision").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing revision: pass the one get_project returned"))?;
            let brief = args.get("brief").and_then(Value::as_str).ok_or_else(|| anyhow!("missing brief"))?;
            json!(db.set_project_brief(&project(&db)?, brief, revision, &actor)?)
        }
        "mark_setup_complete" => json!(db.mark_setup_complete(&project(&db)?, req(args, "sha")?, &actor)?),
        "propose_playbook_change" => {
            let project = project(&db)?;
            let root = playbook::repo_for_issue(&config, Some(&project), &server.cwd)?;
            let normalized = paths::normalize(&root.to_string_lossy());
            let repo = config
                .repos
                .iter()
                .find(|r| paths::normalize(&r.root) == normalized)
                .with_context(|| format!("{} is not connected to Nitely, so it has no playbook Nitely keeps; connect it first", root.display()))?;
            let target = nitely_core::proposals::Target::of(&repo.name, &root);
            let mode = nitely_core::proposals::Mode::parse(str_arg(args, "mode"))?;
            json!(db.propose_playbook_change(&project, &target, mode, req(args, "change")?, req(args, "why")?, &actor)?)
        }
        "file_issue" => {
            let project = project(&db)?;
            let new = new_issue(args, &project, &server.vendor)?;
            let before = db.project_labels(&project)?;
            let mut out = json!(db.create_issue(&new, &actor)?);
            if let Some(note) = label_note(&before, &new.labels) {
                out["labels_note"] = json!(note);
            }
            out
        }
        "file_issues" => {
            let project = project(&db)?;
            let list = args.get("issues").and_then(Value::as_array).context("issues must be a list")?;
            nitely_core::issues::check_batch_size(list.len())?;
            let items = list
                .iter()
                .enumerate()
                .map(|(n, item)| {
                    let issue = new_issue(item, &project, &server.vendor).with_context(|| format!("item {}", n + 1))?;
                    Ok(BatchItem { local_ref: str_arg(item, "ref").unwrap_or_default().to_string(), issue, not_duplicates: vec![] })
                })
                .collect::<Result<Vec<_>>>()?;
            let before = db.project_labels(&project)?;
            let mut out = json!(db.create_issues(&items, &actor)?);
            let given: Vec<String> = items.iter().flat_map(|i| i.issue.labels.clone()).collect();
            if let Some(note) = label_note(&before, &given) {
                out["labels_note"] = json!(note);
            }
            out
        }
        "update_issue" => {
            let key = req(args, "key")?;
            let (add, remove) = (keys_arg(args, "blocked_by"), keys_arg(args, "remove_blocked_by"));
            let owner = str_arg(args, "owner");
            let (add_labels, remove_labels) = (keys_arg(args, "labels"), keys_arg(args, "remove_labels"));
            let goal = args.get("goal").and_then(Value::as_i64);
            let (title, body) = (args.get("title").and_then(Value::as_str), args.get("body").and_then(Value::as_str));
            let priority = args.get("priority").and_then(Value::as_i64);
            if str_arg(args, "state").is_none()
                && add.is_empty()
                && remove.is_empty()
                && owner.is_none()
                && add_labels.is_empty()
                && remove_labels.is_empty()
                && goal.is_none()
                && title.is_none()
                && body.is_none()
                && priority.is_none()
            {
                bail!("give a state, title, body, priority, blocked_by, remove_blocked_by, labels, remove_labels, owner or goal");
            }
            if let Some(p) = priority {
                nitely_core::issues::check_priority(p)?;
            }
            // A mirrored issue, or an empty title or body, is refused before anything changes.
            if title.is_some() || body.is_some() {
                let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
                if issue.origin != "nitely" {
                    bail!("{} is mirrored from {}; change it there", issue.key, issue.origin);
                }
                if title.is_some_and(|t| t.trim().is_empty()) || body.is_some_and(|b| b.trim().is_empty()) {
                    bail!("a title or body cannot be empty");
                }
            }
            // Refuse bad labels and a goal the issue cannot join before anything changes.
            if let Some(goal) = goal {
                db.check_issue_goal(key, Some(goal).filter(|g| *g > 0))?;
            }
            nitely_core::issues::normalize_labels(&add_labels)?;
            nitely_core::issues::normalize_labels(&remove_labels)?;
            if owner.is_some_and(|o| o != "human") {
                bail!("only the human hands their own work back to agents; `ask` them if an agent should do {key}");
            }
            // Check the move first, so a refused move leaves the blockers as they were.
            if let Some(state) = str_arg(args, "state") {
                db.check_move(key, state, str_arg(args, "evidence"), str_arg(args, "reason"), &actor)?;
                db.check_open_asks(key, state)?;
            }
            db.change_blockers(key, &add, &remove, &actor)?;
            // After the blockers, which can still refuse: a refused call changes no text.
            if title.is_some() || body.is_some() {
                db.edit_issue(key, title, body, &actor)?;
            }
            if let Some(priority) = priority {
                db.set_priority(key, priority, &actor)?;
            }
            let mut note = None;
            if !add_labels.is_empty() || !remove_labels.is_empty() {
                let project = db.issue(key)?.and_then(|i| i.project).unwrap_or_default();
                note = label_note(&db.project_labels(&project)?, &add_labels);
                db.change_labels(key, &add_labels, &remove_labels, &actor)?;
            }
            if let Some(owner) = owner {
                db.set_owner(key, owner, &actor)?;
            }
            if let Some(goal) = goal {
                db.set_issue_goal(key, Some(goal).filter(|g| *g > 0), &actor)?;
            }
            let mut out = match str_arg(args, "state") {
                Some(state) => json!(db.move_issue(key, state, str_arg(args, "evidence"), str_arg(args, "reason"), &actor)?),
                None => json!(db.issue(key)?.with_context(|| format!("no issue {key}"))?),
            };
            if let Some(note) = note {
                out["labels_note"] = json!(note);
            }
            out
        }
        "report" => {
            let text = req(args, "text")?;
            db.record(&NewEvent {
                session_id: session.to_string(),
                vendor: server.vendor.clone(),
                cwd: server.place(),
                kind: "status".into(),
                summary: Some(text.into()),
                state: "working".into(),
            })?;
            if let Some(key) = str_arg(args, "key") {
                // Written by the session the report counts toward, so Activity puts the issue on
                // that Codex thread's lane rather than guessing between threads in one folder.
                db.add_note_from(key, text, "status", &actor, session)?;
                db.renew_claim(key, &server.agent())?;
            }
            json!({ "ok": true })
        }
        "note" => {
            let handoff = args.get("handoff").and_then(Value::as_bool).unwrap_or(false);
            db.add_note(req(args, "key")?, req(args, "text")?, if handoff { "handoff" } else { "note" }, &actor)?;
            // A claim it took under its folder's id, before its calls named its session.
            db.renew_claim(req(args, "key")?, &server.agent())?;
            json!({ "ok": true })
        }
        "ask" => {
            let options: Vec<String> = args
                .get("options")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let ask = db.create_ask(req(args, "question")?, &options, str_arg(args, "key"), Some(&server.place()), &actor)?;
            // Show the agent as waiting on the board until it moves on.
            db.record(&NewEvent {
                session_id: session.to_string(),
                vendor: server.vendor.clone(),
                cwd: server.place(),
                kind: "ask".into(),
                summary: Some(format!("Asked: {}", ask.question)),
                state: "waiting".into(),
            })?;
            json!(ask)
        }
        "request_acceptance" => {
            let ask = db.request_acceptance(req(args, "key")?, req(args, "finding")?, req(args, "why")?, Some(&server.place()), &actor)?;
            db.record(&NewEvent {
                session_id: session.to_string(),
                vendor: server.vendor.clone(),
                cwd: server.place(),
                kind: "ask".into(),
                summary: Some(format!("Asked to accept a finding: {}", ask.finding.as_ref().map(|f| f.text.as_str()).unwrap_or(""))),
                state: "waiting".into(),
            })?;
            json!(ask)
        }
        "get_answer" => {
            let id = args.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing id"))?;
            let ask = db.ask(id)?.with_context(|| format!("no ask {id}"))?;
            match &ask.closed {
                // Its issue closed first: nothing to wait for.
                Some(why) => json!({ "id": ask.id, "question": ask.question, "answer": null, "closed": why }),
                None => json!({ "id": ask.id, "question": ask.question, "answer": ask.answer }),
            }
        }
        "triage" => {
            let q = nitely_core::triage::ask(&db, req(args, "question")?, str_arg(args, "key"), Some(&server.place()), &actor)?;
            let q = nitely_core::triage::start(&mut db, &config, &q, &std::env::current_exe()?)?;
            triage_json(&db, &q, &actor)?
        }
        "get_triage" => {
            let id = args.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing id"))?;
            let seconds = args.get("wait_seconds").and_then(Value::as_u64).unwrap_or(0).min(600);
            let q = nitely_core::triage::wait(&db, id, std::time::Duration::from_secs(seconds))?;
            triage_json(&db, &q, &actor)?
        }
        other => bail!("unknown tool {other}"),
    };
    // Submits of this agent's that finished since its last call: one line each (NIT-212).
    if server.remote.is_none() && out.is_object() {
        let done = db.undelivered_submit_jobs(actor.id())?;
        if !done.is_empty() {
            db.mark_submit_jobs_delivered(&done.iter().map(|j| j.id).collect::<Vec<_>>())?;
            out["notices"] = json!(done.iter().map(|j| j.outcome()).collect::<Vec<_>>());
        }
        // Triage's answers to this agent's questions (NIT-245), told once.
        let answered = db.undelivered_triage(actor.id())?;
        if !answered.is_empty() {
            db.mark_triage_delivered(&answered.iter().map(|q| q.id).collect::<Vec<_>>())?;
            let mut notices = out.get("notices").and_then(Value::as_array).cloned().unwrap_or_default();
            notices.extend(answered.iter().map(|q| json!(q.outcome())));
            out["notices"] = json!(notices);
        }
    }
    Ok(out)
}

/// A triage question as triage and get_triage return it. An answered one is told to its asker
/// here, so it is not repeated as a notice.
fn triage_json(db: &Db, q: &nitely_core::triage::Question, actor: &nitely_core::issues::Actor) -> Result<Value> {
    if q.finished() && q.asked_by == actor.id() {
        db.mark_triage_delivered(&[q.id])?;
    }
    let mut v = json!(q);
    v["outcome"] = json!(q.outcome());
    if !q.finished() {
        v["next"] = json!(nitely_core::triage::ASKED_NEXT);
    }
    Ok(v)
}

/// A submit job as submit and submit_status return it. A finished one is told to its submitter
/// here, so it is not repeated as a notice.
fn job_json(db: &Db, job: &nitely_core::submit_job::Job, actor: &nitely_core::issues::Actor) -> Result<Value> {
    if job.finished() && job.requested_by == actor.id() {
        db.mark_submit_jobs_delivered(&[job.id])?;
    }
    let mut v = json!(job);
    v["outcome"] = json!(job.outcome());
    v["next"] = match (&job.result, job.state.as_str()) {
        (Some(r), _) => r["next"].clone(),
        (None, "queued" | "running") => json!(nitely_core::submit_job::QUEUED_NEXT),
        (None, _) => json!("Fix what stopped it, commit, and submit again."),
    };
    v["log"] = json!(nitely_core::submit_job::log_path(job.id).to_string_lossy());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_removes_only_day_old_answers_and_logs_of_calls_apart() {
        let jobs = paths::unique_temp("nitely-apart-sweep", "");
        std::fs::create_dir_all(&jobs).unwrap();
        let day_ago = SystemTime::now() - Duration::from_secs(APART_KEPT_SECS + 60);
        for (name, old) in [("mcp-land-1-1.json", true), ("mcp-land-1-1.log", true), ("mcp-run-1-2.log", false), ("12.log", true)] {
            let f = std::fs::File::create(jobs.join(name)).unwrap();
            if old {
                f.set_modified(day_ago).unwrap();
            }
        }
        sweep_apart(&jobs);
        let mut left: Vec<String> = std::fs::read_dir(&jobs).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["12.log", "mcp-run-1-2.log"], "a submit job's log and a recent call's stay");
        std::fs::remove_dir_all(jobs).ok();
    }

    #[test]
    fn a_new_label_names_the_ones_in_use() {
        let before = vec![("roku".to_string(), 4), ("tizen".to_string(), 2)];
        assert_eq!(label_note(&before, &["Roku".into(), "tizen".into()]), None, "known labels need no note");
        let note = label_note(&before, &["roku-tv".into()]).unwrap();
        assert!(note.contains("New label for this project: roku-tv.") && note.contains("Close to labels it already uses: roku;"), "{note}");
        assert!(note.contains("In use: roku (4), tizen (2)."), "{note}");
        assert!(label_note(&[], &["ios".into()]).unwrap().starts_with("New label for this project: ios."));
    }

    #[test]
    fn the_setup_guide_names_only_tools_that_exist() {
        let names: Vec<String> = tools().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        let guide = nitely_core::instructions::SETUP_GUIDE;
        for tool in ["get_project", "propose_playbook_change", "propose_goal", "ask", "file_issues", "list_issues", "set_project_brief", "mark_setup_complete", "note"] {
            assert!(guide.contains(&format!("`{tool}`")), "the guide uses {tool}");
            assert!(names.contains(&tool.to_string()), "{tool} is a tool");
        }
        assert!(names.contains(&"setup_guide".to_string()));
    }

    #[test]
    fn initialize_negotiates_and_detects_vendor() {
        let mut s = Server { vendor: "agent".into(), cwd: "/w".into(), exe: None, remote: None };
        let r = handle(
            &mut s,
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": { "protocolVersion": "2025-03-26", "clientInfo": { "name": "claude-code" } } }),
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(s.vendor, "claude");
        assert!(handle(&mut s, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        let tools = handle(&mut s, &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).unwrap();
        assert!(tools["result"]["tools"].as_array().unwrap().len() >= 8);
    }

    #[test]
    fn a_call_answers_within_the_timeout_its_client_gives_it() {
        // Codex's default tool timeout is 60 s; Claude Code's idle timeout is 30 minutes (NIT-314).
        assert!(cap_for("codex") < 60 && cap_for("cursor") < 60 && cap_for("agent") < 60);
        assert_eq!(cap_for("claude"), 300);
    }

    #[test]
    fn codex_calls_count_toward_their_thread() {
        // What Codex 0.163 sends with each tool call (seen with a probe server).
        let call = json!({ "name": "report", "_meta": { "threadId": "01a1-t1", "x-codex-turn-metadata": { "thread_id": "01a1-t1" } } });
        let s = Server { vendor: "codex".into(), cwd: r"C:\Code\App".into(), exe: None, remote: None };
        assert_eq!(s.session(&call), "codex:01a1-t1", "the id the turn-end hook reports");
        let older = json!({ "name": "report", "_meta": { "x-codex-turn-metadata": { "thread_id": "t2" } } });
        assert_eq!(s.session(&older), "codex:t2");
        // The agent id is the folder as paths::normalize spells it: lowercase only on Windows.
        let folder = nitely_core::paths::normalize(r"C:\Code\App");
        assert_eq!(s.session(&json!({ "name": "report" })), format!("codex:{folder}"), "no thread: the agent id");
        let claude = Server { vendor: "claude".into(), ..s };
        assert_eq!(claude.session(&call), format!("claude:{folder}"), "only Codex threads");
    }

    #[test]
    fn claude_calls_act_as_the_session_that_made_them() {
        let dir = nitely_core::paths::unique_temp("nitely-mcp-session", "");
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        let open = |db: &mut Db, id: &str, state: &str| {
            db.record(&NewEvent { session_id: id.into(), vendor: "claude".into(), cwd: r"C:\Code\App".into(), kind: "tool".into(), summary: None, state: state.into() }).unwrap();
        };
        open(&mut db, "claude:s-one", "working");
        open(&mut db, "claude:s-old", "ended");
        // The PreToolUse hook noted which session made the call; the call carries its tool use id.
        db.note_tool_call("toolu_01", "claude:s-one").unwrap();
        let s = Server { vendor: "claude".into(), cwd: r"C:\Code\App".into(), exe: None, remote: None };
        let call = |id: &str| json!({ "name": "report", "_meta": { "claudecode/toolUseId": id } });
        assert_eq!(s.claude_session(&call("toolu_01"), &db, None).as_deref(), Some("claude:s-one"));
        // Not noted (hooks older than this): the session Claude Code named, while it is open.
        assert_eq!(s.claude_session(&call("toolu_02"), &db, Some("s-one")).as_deref(), Some("claude:s-one"));
        assert_eq!(s.claude_session(&call("toolu_02"), &db, Some("s-old")), None, "a session /clear ended");
        assert_eq!(s.claude_session(&call("toolu_02"), &db, None), None, "then the folder's id, as before");
        let codex = Server { vendor: "codex".into(), cwd: r"C:\Code\App".into(), exe: None, remote: None };
        assert_eq!(codex.claude_session(&call("toolu_01"), &db, None), None);
        assert_eq!(s.actor_for("claude:s-one").id(), "claude:s-one");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
