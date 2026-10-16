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

const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

struct Server {
    vendor: String,
    cwd: String,
}

impl Server {
    /// One id per agent per directory, e.g. `claude:c:/development/nitely-worktrees/nit-3`.
    fn agent(&self) -> String {
        format!("{}:{}", self.vendor, paths::normalize(&self.cwd))
    }

    fn actor(&self) -> Actor {
        Actor::Agent(self.agent())
    }
}

pub fn serve() -> Result<()> {
    let mut server = Server { vendor: "agent".into(), cwd: std::env::current_dir()?.to_string_lossy().into_owned() };
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

fn handle(server: &mut Server, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    // Notifications (no id) get no response.
    let id = id?;
    let result = match method {
        "initialize" => Ok(initialize(server, &params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call_tool(server, &params)),
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
    let instructions = Config::load()
        .ok()
        .and_then(|c| nitely_core::instructions::for_dir(&c, &server.cwd))
        .unwrap_or_else(|| nitely_core::instructions::WORKFLOW.to_string());
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nitely", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions
    })
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

fn run_tool(server: &Server, name: &str, args: &Value) -> Result<Value> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let actor = server.actor();
    let project = |db: &Db| resolve_project(&config, db, str_arg(args, "project"), &server.cwd);

    Ok(match name {
        "next_task" => {
            let project = project(&db)?;
            match db.next_task(&project, &server.agent())? {
                Some(issue) => {
                    let branch = match playbook::repo_root(&config, &server.cwd) {
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
            let root = playbook::repo_root(&config, &server.cwd)
                .with_context(|| format!("{} is not inside a git repo", server.cwd))?;
            json!(playbook::start_task(&db, &root, &issue, &server.vendor, &actor)?)
        }
        "run_gate" => {
            let dir = str_arg(args, "worktree").unwrap_or(&server.cwd);
            let (worktree, branch_key) = playbook::current_worktree(dir)?;
            let key = str_arg(args, "key").map(str::to_string).or(branch_key);
            let root = playbook::repo_root(&config, dir).with_context(|| format!("{dir} is not inside a git repo"))?;
            json!(playbook::run_gate(&db, &root, &worktree, key.as_deref(), &actor)?)
        }
        "submit" => {
            let dir = str_arg(args, "worktree").unwrap_or(&server.cwd);
            let opts = nitely_core::submit::SubmitOptions { dir, issue_key: str_arg(args, "key"), skip_review: false, local_only: false };
            json!(nitely_core::submit::submit(&mut db, &config, &opts, &actor)?)
        }
        "land" => {
            let opts = nitely_core::land::LandOptions { dir: &server.cwd, issue_key: req(args, "key")?, push: true, keep: false, break_lock: false };
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
                cwd: server.cwd.clone(),
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
            let ask = db.create_ask(req(args, "question")?, &options, str_arg(args, "key"), Some(&server.cwd), &actor)?;
            // Show the agent as waiting on the board until it moves on.
            db.record(&NewEvent {
                session_id: server.agent(),
                vendor: server.vendor.clone(),
                cwd: server.cwd.clone(),
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
        let mut s = Server { vendor: "agent".into(), cwd: "/w".into() };
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
