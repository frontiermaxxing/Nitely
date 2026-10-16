//! `nitely mcp`: a Model Context Protocol server on stdio (newline-delimited JSON-RPC 2.0).
//! This is how agents work with Nitely: take the next issue, report progress, leave notes, file
//! what they find, and ask the human. Every call acts as an agent, never as the human, so it can
//! not approve or answer.

use anyhow::{anyhow, bail, Context, Result};
use nitely_core::db::NewEvent;
use nitely_core::issues::{branch_name, KINDS, SOURCES, STATES};
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
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nitely", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Nitely is the backlog and control room for this repo. Start work with next_task, \
            keep the human informed with report, leave a handoff note before you stop, file what you find \
            but will not fix now with file_issue, and use ask when only the human can decide."
    })
}

fn tools() -> Value {
    let states: Vec<&str> = STATES.iter().map(|(n, _)| *n).filter(|n| *n != "Approved").collect();
    json!([
        {
            "name": "next_task",
            "description": "Claim the most urgent Todo issue in this repo's project and get it with a suggested branch name. Two agents never get the same issue. Returns null when nothing is ready.",
            "inputSchema": { "type": "object", "properties": {
                "project": { "type": "string", "description": "Project key; defaults to this repo's project." }
            } }
        },
        {
            "name": "get_issue",
            "description": "An issue with its full timeline (state changes, notes, handoffs, questions and answers) and the pull requests whose branch names it, with their CI checks.",
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
                "allow_similar": { "type": "boolean" }
            } }
        },
        {
            "name": "update_issue",
            "description": "Move an issue. Done needs evidence (what ran and what it showed); Canceled and Duplicate need a reason. Approved is set only by the human; move to Needs You and ask instead.",
            "inputSchema": { "type": "object", "required": ["key", "state"], "properties": {
                "key": { "type": "string" },
                "state": { "type": "string", "enum": states },
                "evidence": { "type": "string" },
                "reason": { "type": "string" }
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

fn run_tool(server: &Server, name: &str, args: &Value) -> Result<Value> {
    let config = Config::load()?;
    let mut db = Db::open_default()?;
    let actor = server.actor();
    let project = |db: &Db| resolve_project(&config, db, str_arg(args, "project"), &server.cwd);

    Ok(match name {
        "next_task" => {
            let project = project(&db)?;
            match db.next_task(&project, &server.agent())? {
                Some(issue) => json!({
                    "issue": issue,
                    "branch": branch_name(&server.vendor, &issue.key, &issue.title),
                    "next": "Work on a branch with this name in its own worktree, report as you go, and leave a handoff note before you stop."
                }),
                None => json!({ "issue": null, "message": format!("Nothing in Todo for {project}.") }),
            }
        }
        "get_issue" => {
            let key = req(args, "key")?;
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            json!({ "issue": issue, "timeline": db.timeline(&issue.key)?, "pull_requests": db.prs_for_issue(&issue.key)? })
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
                },
                &actor,
            )?;
            json!(issue)
        }
        "update_issue" => json!(db.move_issue(
            req(args, "key")?,
            req(args, "state")?,
            str_arg(args, "evidence"),
            str_arg(args, "reason"),
            &actor
        )?),
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
