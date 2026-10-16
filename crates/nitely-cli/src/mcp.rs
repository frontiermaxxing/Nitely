//! `nitely mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC 2.0).
//! This is how agents work with Nitely: take the next issue, report progress, leave notes, file
//! what they find, and ask the human. Every call acts as an agent, never as the human, so it can
//! not approve or answer.

use anyhow::{anyhow, bail, Context, Result};
use nitely_core::db::NewEvent;
use nitely_core::issues::{branch_name, KINDS, SOURCES, STATES};
use nitely_core::playbook::{self, Playbook};
use nitely_core::{paths, resolve_project, Actor, Config, Db, NewIssue};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::SystemTime;

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

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
pub(crate) struct Remote {
    pub name: String,
    pub project: Option<String>,
}

/// Tools that work in a repo on this machine, so not for an agent elsewhere.
const LOCAL_ONLY: &[&str] = &["start_task", "run_gate", "submit", "land"];

impl Server {
    pub(crate) fn remote(name: &str, project: Option<String>) -> Server {
        Server { vendor: "agent".into(), cwd: String::new(), exe: None, remote: Some(Remote { name: name.into(), project }) }
    }

    /// One id per agent per directory, e.g. `claude:c:/development/nitely-worktrees/nit-3`; an
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

    fn actor(&self) -> Actor {
        Actor::Agent(self.agent())
    }
}

pub fn serve() -> Result<()> {
    let exe = std::env::current_exe().ok().and_then(|p| nitely_core::install::stamp(&p).map(|s| (p, s)));
    let mut server = Server { vendor: "agent".into(), cwd: std::env::current_dir()?.to_string_lossy().into_owned(), exe, remote: None };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&mut server, &msg),
            Err(e) => Some(error_response(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = response {
            writeln!(stdout, "{}", serde_json::to_string(&r)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
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
            "description": "Claim the most urgent Todo issue in this repo's project and get it with a suggested branch name. Two agents never get the same issue. Issues with an open blocker are skipped, and a blocker is as urgent as what it blocks. Returns null when nothing is ready.",
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
            "name": "run_gate",
            "description": "Run the playbook's gate commands (tests, build) in your worktree and record the result on the issue. Stops at the first failure and returns the end of its output. Pass before you open a pull request.",
            "inputSchema": { "type": "object", "properties": {
                "key": { "type": "string", "description": "The issue; defaults to the one your branch names." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." }
            } }
        },
        {
            "name": "submit",
            "description": "Hand in a finished branch: runs the gate, has another vendor review it (this takes minutes), decides whether the human must approve, pushes, opens or updates the pull request, and moves the issue (In Review, Needs You, or back to In Progress when the review finds P0/P1 problems). Commit everything first. Read `next` in the result for what to do after.",
            "inputSchema": { "type": "object", "properties": {
                "key": { "type": "string", "description": "The issue; defaults to the one your branch names." },
                "worktree": { "type": "string", "description": "Defaults to the worktree you are in." }
            } }
        },
        {
            "name": "land",
            "description": "Land a submitted issue on the integration branch: one land at a time, rebase onto the integration branch, check that the review covers exactly this change (and that the human approved it when the playbook says so), run the gate, fast-forward, push, clean up the worktree, and move the issue to Done. A rebase conflict is left in progress for you to resolve.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string" }
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
                "include_closed": { "type": "boolean" }
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
            "name": "file_issue",
            "description": "File a finding, follow-up or idea you will not handle now. Refused when an open issue has a similar title (it lists them); set allow_similar only if it is really different.",
            "inputSchema": { "type": "object", "required": ["title", "kind", "body", "evidence"], "properties": {
                "title": { "type": "string" },
                "kind": { "type": "string", "enum": KINDS },
                "body": { "type": "string", "description": "What is wrong or wanted, and why. Include file paths and the branch or commit you saw it on." },
                "evidence": { "type": "string", "description": "What you saw or ran; \"none yet\" for an idea." },
                "priority": { "type": "integer", "minimum": 0, "maximum": 4, "description": "1 urgent, 2 high, 3 medium, 4 low, 0 none" },
                "state": { "type": "string", "enum": ["Todo", "Backlog"] },
                "project": { "type": "string" },
                "allow_similar": { "type": "boolean" },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Issues that must close before this one can start, e.g. [\"NIT-9\"]." }
            } }
        },
        {
            "name": "update_issue",
            "description": "Move an issue, or change what blocks it. Done needs evidence (what ran and what it showed); Canceled and Duplicate need a reason. Approved is set only by the human; move to Needs You and ask instead. An issue with an open blocker waits: next_task skips it until every blocker is Done or Canceled.",
            "inputSchema": { "type": "object", "required": ["key"], "properties": {
                "key": { "type": "string" },
                "state": { "type": "string", "enum": states },
                "evidence": { "type": "string" },
                "reason": { "type": "string" },
                "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Issues to add as blockers of this one." },
                "remove_blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Blockers to remove." }
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
            "description": "Ask the human something only they can decide. It goes to their inbox. Returns an id; check it with get_answer. Keep working on whatever does not depend on the answer.",
            "inputSchema": { "type": "object", "required": ["question"], "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" } },
                "key": { "type": "string", "description": "The issue it is about." }
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
    match run_tool(server, name, &args) {
        Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }] }),
        Err(e) => json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true }),
    }
}

/// When the nitely this server started from has been replaced by a new build since (`nitely
/// install`, which land runs for Nitely itself), runs the call in that build: an agent that has
/// been running for hours gets the landed fixes without restarting. None when the build is the
/// same, or the new one cannot take the call; then it runs here.
fn forward(server: &Server, params: &Value) -> Option<Value> {
    let (exe, stamp) = server.exe.as_ref()?;
    if nitely_core::install::stamp(exe).as_ref() == Some(stamp) {
        return None;
    }
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
    let mut result: Value = serde_json::from_slice(&out.stdout).ok()?;
    // Says which build answered, for whoever debugs an agent's session.
    result["_meta"] = json!({ "nitely_forwarded": exe.to_string_lossy() });
    Some(result)
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

fn run_tool(server: &Server, name: &str, args: &Value) -> Result<Value> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let args = &current_args(&db, args)?;
    let actor = server.actor();
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

    Ok(match name {
        "next_task" => {
            let project = project(&db)?;
            match db.next_task(&project, &server.agent())? {
                Some(issue) => {
                    let root = if server.remote.is_some() { None } else { playbook::repo_for_issue(&config, issue.project.as_deref(), &server.cwd).ok() };
                    let branch = match root {
                        Some(root) => Playbook::load(&root)?.branch_for(&server.vendor, &issue),
                        None => branch_name(&server.vendor, &issue.key, &issue.title),
                    };
                    json!({
                        "issue": issue,
                        "branch": branch,
                        "next": "Call start_task with this key for the branch and worktree, work there, report as you go, call submit when it is committed, and leave a handoff note before you stop."
                    })
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
        "run_gate" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            let (worktree, branch_key) = playbook::current_worktree(&dir)?;
            let key = str_arg(args, "key").map(str::to_string).or(branch_key);
            let root = playbook::repo_root(&config, &dir).with_context(|| format!("{dir} is not inside a git repo"))?;
            json!(playbook::run_gate(&db, &root, &worktree, key.as_deref(), &actor)?)
        }
        "submit" => {
            let dir = task_dir(&config, &db, args, &server.cwd)?;
            nitely_core::install::check_current(&config, &dir, env!("NITELY_COMMIT"))?;
            let opts = nitely_core::submit::SubmitOptions { dir: &dir, issue_key: str_arg(args, "key"), skip_review: false, local_only: false };
            json!(nitely_core::submit::submit(&mut db, &config, &opts, &actor)?)
        }
        "land" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            let root = playbook::repo_for_issue(&config, issue.project.as_deref(), &server.cwd)?;
            nitely_core::install::check_current(&config, &root.to_string_lossy(), env!("NITELY_COMMIT"))?;
            let opts = nitely_core::land::LandOptions { dir: &server.cwd, issue_key: key, push: true, keep: false, break_lock: false };
            json!(nitely_core::land::land(&mut db, &config, &opts, &actor)?)
        }
        "get_issue" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            json!({
                "issue": issue,
                "relations": db.relations(&issue.key)?,
                "timeline": db.timeline(&issue.key)?,
                "pull_requests": db.prs_for_issue(&issue.key)?
            })
        }
        "list_issues" => {
            let project = project(&db)?;
            let include_closed = args.get("include_closed").and_then(Value::as_bool).unwrap_or(false);
            json!(db.list_issues(Some(&project), include_closed)?)
        }
        "get_project" => json!(db.project_brief(&project(&db)?)?),
        "set_project_brief" => {
            let revision = args.get("revision").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing revision: pass the one get_project returned"))?;
            let brief = args.get("brief").and_then(Value::as_str).ok_or_else(|| anyhow!("missing brief"))?;
            json!(db.set_project_brief(&project(&db)?, brief, revision, &actor)?)
        }
        "mark_setup_complete" => json!(db.mark_setup_complete(&project(&db)?, req(args, "sha")?, &actor)?),
        "file_issue" => {
            let project = project(&db)?;
            let source = if SOURCES.contains(&server.vendor.as_str()) { server.vendor.clone() } else { "claude".into() };
            let issue = db.create_issue(
                &NewIssue {
                    project,
                    title: req(args, "title")?.into(),
                    kind: req(args, "kind")?.into(),
                    body: req(args, "body")?.into(),
                    evidence: req(args, "evidence")?.into(),
                    source,
                    priority: args.get("priority").and_then(Value::as_i64).unwrap_or(0),
                    allow_similar: args.get("allow_similar").and_then(Value::as_bool).unwrap_or(false),
                    state: str_arg(args, "state").map(str::to_string),
                    blocked_by: keys_arg(args, "blocked_by"),
                },
                &actor,
            )?;
            json!(issue)
        }
        "update_issue" => {
            let key = req(args, "key")?;
            let (add, remove) = (keys_arg(args, "blocked_by"), keys_arg(args, "remove_blocked_by"));
            if str_arg(args, "state").is_none() && add.is_empty() && remove.is_empty() {
                bail!("give a state, blocked_by or remove_blocked_by");
            }
            // Check the move first, so a refused move leaves the blockers as they were.
            if let Some(state) = str_arg(args, "state") {
                db.check_move(key, state, str_arg(args, "evidence"), str_arg(args, "reason"), &actor)?;
            }
            db.change_blockers(key, &add, &remove, &actor)?;
            match str_arg(args, "state") {
                Some(state) => json!(db.move_issue(key, state, str_arg(args, "evidence"), str_arg(args, "reason"), &actor)?),
                None => json!(db.issue(key)?.with_context(|| format!("no issue {key}"))?),
            }
        }
        "report" => {
            let text = req(args, "text")?;
            db.record(&NewEvent {
                session_id: server.agent(),
                vendor: server.vendor.clone(),
                cwd: server.place(),
                kind: "status".into(),
                summary: Some(text.into()),
                state: "working".into(),
            })?;
            if let Some(key) = str_arg(args, "key") {
                db.add_note(key, text, "status", &actor)?;
            }
            json!({ "ok": true })
        }
        "note" => {
            let handoff = args.get("handoff").and_then(Value::as_bool).unwrap_or(false);
            db.add_note(req(args, "key")?, req(args, "text")?, if handoff { "handoff" } else { "note" }, &actor)?;
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
                session_id: server.agent(),
                vendor: server.vendor.clone(),
                cwd: server.place(),
                kind: "ask".into(),
                summary: Some(format!("Asked: {}", ask.question)),
                state: "waiting".into(),
            })?;
            json!(ask)
        }
        "get_answer" => {
            let id = args.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow!("missing id"))?;
            let ask = db.ask(id)?.with_context(|| format!("no ask {id}"))?;
            json!({ "id": ask.id, "question": ask.question, "answer": ask.answer })
        }
        other => bail!("unknown tool {other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
