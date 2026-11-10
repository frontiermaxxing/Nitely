//! Waking an agent session when its own background work ends (NIT-765), so it ends its turn after
//! `submit` instead of polling `submit_status`: each poll is a model call over its whole context.
//!
//! Claude Code runs `nitely wake claude` after each of Nitely's job tools (a PostToolUse hook with
//! `asyncRewake`, written by `connect`). The hook runs in the background, waits on the database for
//! what the session still has running (submit jobs, land jobs, lane runs, a land or quick land that
//! went on in a process of its own), and exits 2 with what each one came to. Claude Code shows that
//! to the session and wakes it, even when it is idle. With nothing running it exits at once and
//! wakes nobody. No other agent host lets a process wake an idle session: Codex and the rest are
//! told to make one long `wait_seconds` call instead (instructions.rs).
//!
//! A newer waiter for a session replaces an older one (its token file), so a session that polls
//! anyway, or starts more work, is woken once, by the waiter that knows about all of it.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::db::Db;

/// The tools after which the hook looks for work to wait on: the ones that start or report a job.
pub const TOOLS: &[&str] = &["submit", "submit_status", "land", "quick_land", "run", "run_status"];

/// The Claude Code hook matcher for `TOOLS` (a regular expression over the tool name).
pub fn matcher() -> String {
    format!("mcp__nitely__({})$", TOOLS.join("|"))
}

/// How long Claude Code lets the hook run, in seconds, and how long it waits before it wakes the
/// session anyway to say what is still running (a little less, so it is never cut off).
pub const HOOK_TIMEOUT: u64 = 3600;
pub const WAIT: u64 = HOOK_TIMEOUT - 120;

/// What a session has running, as the hook found it right after the tool call.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Pending {
    pub submits: Vec<i64>,
    pub lands: Vec<i64>,
    pub runs: Vec<i64>,
    /// The answer files of calls that went on in a process of their own (`run_apart`), with the
    /// tool that started each.
    pub apart: Vec<(String, PathBuf)>,
}

impl Pending {
    pub fn is_empty(&self) -> bool {
        self.submits.is_empty() && self.lands.is_empty() && self.runs.is_empty() && self.apart.is_empty()
    }
}

/// What `actor` has running now. `tool` and `response` are the call the hook follows: a land or
/// quick land past the call's wait names the log of the process it goes on in, and its answer
/// lands beside it.
pub fn pending(db: &Db, actor: &str, tool: &str, response: &Value) -> Result<Pending> {
    let ids = |sql: &str| -> Result<Vec<i64>> {
        let mut s = db.conn.prepare(sql)?;
        let rows = s.query_map([actor], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    };
    let mut out = Pending {
        submits: ids("SELECT id FROM submit_job WHERE requested_by = ?1 AND state IN ('queued', 'running') ORDER BY id")?,
        lands: ids("SELECT id FROM land_job WHERE requested_by = ?1 AND state IN ('waiting', 'landing') ORDER BY id")?,
        runs: ids("SELECT id FROM lane_run WHERE requested_by = ?1 AND state IN ('queued', 'running') ORDER BY id")?,
        apart: vec![],
    };
    for body in answers(response) {
        if body.get("still_running").and_then(Value::as_bool) == Some(true) {
            if let Some(log) = body.get("log").and_then(Value::as_str) {
                out.apart.push((tool.to_string(), Path::new(log).with_extension("json")));
            }
        }
    }
    Ok(out)
}

/// The JSON answers inside an MCP tool response as Claude Code hands it to the hook: the result
/// itself, or its `content` text blocks.
fn answers(response: &Value) -> Vec<Value> {
    let mut out = vec![];
    let mut take = |v: &Value| match v {
        Value::String(s) => out.extend(serde_json::from_str::<Value>(s).ok()),
        Value::Object(_) => out.push(v.clone()),
        _ => {}
    };
    match response {
        Value::Array(items) => items.iter().filter_map(|i| i.get("text")).for_each(&mut take),
        Value::Object(o) if o.contains_key("content") => {
            o["content"].as_array().into_iter().flatten().filter_map(|i| i.get("text")).for_each(&mut take)
        }
        other => take(other),
    }
    out
}

/// What each pending piece came to, one line each, once every one of them has ended; None while
/// any still runs.
pub fn ended(db: &Db, pending: &Pending) -> Result<Option<Vec<String>>> {
    let mut lines = vec![];
    for id in &pending.submits {
        match db.submit_job(*id)? {
            Some(j) if !j.finished() => return Ok(None),
            Some(j) => lines.push(j.outcome()),
            None => {}
        }
    }
    for id in &pending.lands {
        match db.land_job(*id)? {
            Some(j) if !j.finished() => return Ok(None),
            Some(j) => lines.push(match (&j.result, &j.error) {
                (_, Some(e)) => format!("{}: the land stopped: {e}", j.issue),
                (Some(r), _) => format!("{}: {}", j.issue, said(r)),
                _ => format!("{}: the land is {}.", j.issue, j.state),
            }),
            None => {}
        }
    }
    for id in &pending.runs {
        match db.lane_run(*id)? {
            Some(r) if matches!(r.state.as_str(), "queued" | "running") => return Ok(None),
            Some(r) => lines.push(format!("Run {} (`{}`): {}{}", r.id, r.command, r.state, r.exit_code.map(|c| format!(", exit {c}")).unwrap_or_default())),
            None => {}
        }
    }
    for (tool, file) in &pending.apart {
        let Some(answer) = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
            return Ok(None);
        };
        lines.push(match (answer.get("ok"), answer.get("error")) {
            (_, Some(e)) => format!("{tool}: {}", e.as_str().unwrap_or("stopped")),
            (Some(ok), _) => format!("{tool}: {}", said(ok)),
            _ => format!("{tool}: ended"),
        });
    }
    Ok(Some(lines))
}

/// A tool's answer in a line: its `next` or `error` when it has one, else the JSON, cut short.
fn said(v: &Value) -> String {
    let text = ["error", "next", "outcome"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).map(str::to_string);
    let text = text.unwrap_or_else(|| match v.get("landed").and_then(Value::as_bool) {
        Some(true) => "landed.".to_string(),
        _ => v.to_string(),
    });
    let mut s: String = text.chars().take(400).collect();
    if s.len() < text.len() {
        s.push('…');
    }
    s
}

/// What the hook tells the session it wakes.
pub fn message(lines: &[String]) -> String {
    let mut text = String::from("Nitely: what you were waiting on has ended.");
    for l in lines {
        text.push_str("\n- ");
        text.push_str(l);
    }
    text.push_str("\nCarry on from here: `next_task` or the issue's own status call has the detail.");
    text
}

/// What it tells the session when its work is still running as the hook's time runs out.
pub fn still_running(pending: &Pending) -> String {
    let mut what = vec![];
    what.extend(pending.submits.iter().map(|id| format!("submit job {id}")));
    what.extend(pending.lands.iter().map(|id| format!("land job {id}")));
    what.extend(pending.runs.iter().map(|id| format!("lane run {id}")));
    what.extend(pending.apart.iter().map(|(tool, _)| tool.clone()));
    format!(
        "Nitely: still running after {} minutes: {}. To be woken when it ends, call its status tool once without wait_seconds (submit_status, run_status) and end your turn.",
        WAIT / 60,
        what.join(", ")
    )
}

/// The token file that says which waiter is the session's current one.
pub fn token_path(dir: &Path, actor: &str) -> PathBuf {
    let safe: String = actor.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    dir.join("wake").join(format!("{safe}.token"))
}

/// What the waiter did.
#[derive(Debug, PartialEq)]
pub enum Woke {
    /// Nothing was running: wake nobody.
    Nothing,
    /// A newer waiter took over the session, or the session's work was told some other way.
    Replaced,
    /// Wake the session with this.
    Wake(String),
}

/// Waits for the session's pending work, up to `wait`: checks every `every` while it is still the
/// session's current waiter (`token`). `open` opens the database for each look, so no connection
/// stays open for an hour.
pub fn wait(open: &dyn Fn() -> Result<Db>, token_file: &Path, token: &str, pending: &Pending, wait: std::time::Duration, every: std::time::Duration) -> Result<Woke> {
    if pending.is_empty() {
        // Nothing to wait on: an older waiter has nothing left to say either.
        if std::fs::read_to_string(token_file).is_ok() {
            std::fs::write(token_file, token)?;
        }
        return Ok(Woke::Nothing);
    }
    if let Some(dir) = token_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(token_file, token)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        if std::fs::read_to_string(token_file).ok().as_deref() != Some(token) {
            return Ok(Woke::Replaced);
        }
        if let Some(lines) = ended(&open()?, pending)? {
            std::fs::remove_file(token_file).ok();
            return Ok(Woke::Wake(message(&lines)));
        }
        if std::time::Instant::now() >= deadline {
            std::fs::remove_file(token_file).ok();
            return Ok(Woke::Wake(still_running(pending)));
        }
        std::thread::sleep(every);
    }
}
