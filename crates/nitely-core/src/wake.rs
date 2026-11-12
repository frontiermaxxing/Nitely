//! Waking an agent session when its background work ends (NIT-765), so it ends its turn after
//! `submit` instead of polling `submit_status`: each poll is a model call over its whole context.
//!
//! Claude Code runs `nitely wake claude` after each of Nitely's job tools (a PostToolUse hook with
//! `asyncRewake`, written by `connect`). The hook runs in the background and watches, on the
//! database, the session's own work: submit jobs, land jobs and lane runs it asked for, the job or
//! run the tool's own reply names, and a land or quick land that went on in a process of its own.
//! As soon as one of them ends, it exits 2 with what each ended one came to and what still runs;
//! Claude Code shows that and wakes the session, even when idle. With nothing to watch it exits at
//! once and wakes nobody. Work another session runs on an issue this one claims is NIT-774.
//!
//! One waiter per session: a newer one takes over (the state file's token) and inherits what the
//! older one still watched, so nothing is dropped and nothing is told twice. No other agent host
//! lets a process wake an idle session: Codex and the rest are told to make one long
//! `wait_seconds` call instead (instructions.rs).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::db::{now, Db};

/// The tools after which the hook looks for work to wait on: the ones that start or report a job.
pub const TOOLS: &[&str] = &["submit", "submit_status", "land", "quick_land", "run", "run_status"];

/// The Claude Code hook matcher for `TOOLS` (a regular expression over the tool name).
pub fn matcher() -> String {
    format!("mcp__nitely__({})$", TOOLS.join("|"))
}

/// How long Claude Code lets the hook run, in seconds, and how long it watches before it wakes the
/// session anyway to say what is still running (a little less, so it is never cut off).
pub const HOOK_TIMEOUT: u64 = 3600;
pub const WAIT: u64 = HOOK_TIMEOUT - 120;

/// A finished job not yet told to its session counts as news this long after it ended: one that
/// ended between the tool's reply and the hook's first look.
const RECENT_SECS: i64 = 600;

/// What a session's waiter watches. Kept in the state file, so a newer waiter inherits it.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watched {
    #[serde(default)]
    pub submits: Vec<i64>,
    #[serde(default)]
    pub lands: Vec<i64>,
    #[serde(default)]
    pub runs: Vec<i64>,
    /// The answer files of calls that went on in a process of their own (`run_apart`), with the
    /// tool that started each.
    #[serde(default)]
    pub apart: Vec<(String, PathBuf)>,
}

impl Watched {
    pub fn is_empty(&self) -> bool {
        self.submits.is_empty() && self.lands.is_empty() && self.runs.is_empty() && self.apart.is_empty()
    }

    fn add(&mut self, other: Watched) {
        fn merge<T: PartialEq>(into: &mut Vec<T>, from: Vec<T>) {
            for x in from {
                if !into.contains(&x) {
                    into.push(x);
                }
            }
        }
        merge(&mut self.submits, other.submits);
        merge(&mut self.lands, other.lands);
        merge(&mut self.runs, other.runs);
        merge(&mut self.apart, other.apart);
    }

    fn names(&self) -> Vec<String> {
        let mut what = vec![];
        what.extend(self.submits.iter().map(|id| format!("submit job {id}")));
        what.extend(self.lands.iter().map(|id| format!("land job {id}")));
        what.extend(self.runs.iter().map(|id| format!("lane run {id}")));
        what.extend(self.apart.iter().map(|(tool, _)| tool.clone()));
        what
    }
}

/// The session's waiter state on disk: whose turn it is to watch, and what is watched.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    token: String,
    #[serde(default)]
    watched: Watched,
    /// What a waiter already told the session (`submit:12`, `land:3`, `run:40`, the newest last), so
    /// a finished job still unread in the database is not told again.
    #[serde(default)]
    told: Vec<String>,
}

/// How many told pieces the state remembers.
const TOLD: usize = 100;

/// Runs `f` on the state file's contents with the session's lock held, so two waiters that start
/// together never both read the old state and drop each other's work. A lock left by a waiter that
/// died is broken after a minute.
fn locked<T>(file: &Path, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
    let lock = file.with_extension("lock");
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                let stale = std::fs::metadata(&lock).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 60);
                if stale {
                    std::fs::remove_file(&lock).ok();
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(anyhow::anyhow!("the wake state of this session is locked: {e}")),
        }
    }
    let mut state = read_state(file).unwrap_or_default();
    let out = f(&mut state);
    let written = out.is_ok().then(|| {
        let n = state.told.len();
        state.told.drain(..n.saturating_sub(TOLD));
        write_state(file, &state)
    });
    std::fs::remove_file(&lock).ok();
    if let Some(w) = written {
        w?;
    }
    out
}

/// The state file of `actor`'s waiter.
pub fn state_path(dir: &Path, actor: &str) -> PathBuf {
    let safe: String = actor.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    dir.join("wake").join(format!("{safe}.json"))
}

fn read_state(file: &Path) -> Option<State> {
    std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_state(file: &Path, state: &State) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_vec(state)?)?;
    Ok(())
}

/// What the tool call the hook follows names as still going: the submit job its reply names, a lane
/// run it says is queued or running, the answer file of a call that went on apart.
pub fn named(tool: &str, response: &Value) -> Watched {
    let mut out = Watched::default();
    for answer in answers(response) {
        match &answer {
            Value::String(s) => {
                // "Handed in as submit job 12: …", "Submit job 12 of APP-3 is running …".
                let lower = s.to_ascii_lowercase();
                if let Some(id) = lower.split_once("submit job ").and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next()).and_then(|n| n.parse().ok()) {
                    out.submits.push(id);
                }
            }
            Value::Object(o) => {
                if o.get("still_running").and_then(Value::as_bool) == Some(true) {
                    if let Some(log) = o.get("log").and_then(Value::as_str) {
                        out.apart.push((tool.to_string(), Path::new(log).with_extension("json")));
                    }
                }
                let open = o.get("state").and_then(Value::as_str).is_some_and(|s| matches!(s, "queued" | "running"));
                if let (true, true, Some(id)) = (matches!(tool, "run" | "run_status"), open, o.get("id").and_then(Value::as_i64)) {
                    out.runs.push(id);
                }
            }
            _ => {}
        }
    }
    out
}

/// The answers inside an MCP tool response as Claude Code hands it to the hook: the result itself,
/// or its `content` text blocks, each parsed as JSON where it is JSON.
fn answers(response: &Value) -> Vec<Value> {
    let mut out = vec![];
    let mut take = |v: &Value| match v {
        Value::String(s) => out.push(serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone()))),
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

/// The session's own work in the database now: open submit jobs, land jobs and lane runs it asked
/// for, and its jobs that ended lately without being told to it.
pub fn scan(db: &Db, actor: &str) -> Result<Watched> {
    let ids = |sql: &str| -> Result<Vec<i64>> {
        let mut s = db.conn.prepare(sql)?;
        let rows = s.query_map(rusqlite::params![actor, now() - RECENT_SECS], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    };
    Ok(Watched {
        submits: ids("SELECT id FROM submit_job WHERE requested_by = ?1 AND (state IN ('queued', 'running') OR (delivered_at IS NULL AND ended_at >= ?2)) ORDER BY id")?,
        lands: ids("SELECT id FROM land_job WHERE requested_by = ?1 AND (state IN ('waiting', 'landing') OR (delivered_at IS NULL AND ended_at >= ?2)) ORDER BY id")?,
        runs: ids("SELECT id FROM lane_run WHERE requested_by = ?1 AND state IN ('queued', 'running') AND ?2 = ?2 ORDER BY id")?,
        apart: vec![],
    })
}

/// What one watched piece came to, or None while it still runs.
fn outcome_submit(db: &Db, id: i64) -> Result<Option<String>> {
    Ok(match db.submit_job(id)? {
        Some(j) if !j.finished() => None,
        Some(j) => Some(j.outcome()),
        None => Some(format!("submit job {id}: gone")),
    })
}

fn outcome_land(db: &Db, id: i64) -> Result<Option<String>> {
    Ok(match db.land_job(id)? {
        Some(j) if !j.finished() => None,
        Some(j) => Some(match (&j.result, &j.error) {
            (_, Some(e)) => format!("{}: the land stopped: {e}", j.issue),
            (Some(r), _) => format!("{}: {}", j.issue, said(r)),
            _ => format!("{}: the land is {}.", j.issue, j.state),
        }),
        None => Some(format!("land job {id}: gone")),
    })
}

fn outcome_run(db: &Db, id: i64) -> Result<Option<String>> {
    Ok(match db.lane_run(id)? {
        Some(r) if matches!(r.state.as_str(), "queued" | "running") => None,
        Some(r) => Some(format!("Run {} (`{}`): {}{}", r.id, r.command, r.state, r.exit_code.map(|c| format!(", exit {c}")).unwrap_or_default())),
        None => Some(format!("lane run {id}: gone")),
    })
}

/// A call that went on apart: its answer once it is written, or the lane run it queued (a `run`
/// past the call's wait writes `{"queued": id}` first).
enum Apart {
    Answered(String),
    Queued(i64),
}

/// None while neither is there.
fn outcome_apart(tool: &str, file: &Path) -> Option<Apart> {
    let answer = std::fs::read(file).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())?;
    match (answer.get("ok"), answer.get("error"), answer.get("queued").and_then(Value::as_i64)) {
        (_, Some(e), _) => Some(Apart::Answered(format!("{tool}: {}", e.as_str().unwrap_or("stopped")))),
        (Some(ok), _, _) => Some(Apart::Answered(format!("{tool}: {}", said(ok)))),
        (None, None, Some(run)) => Some(Apart::Queued(run)),
        _ => None,
    }
}

/// Takes the pieces of `watched` that have ended out of it, with a line each.
/// `told` gets the database pieces it takes.
pub fn take_ended(db: &Db, watched: &mut Watched, told: &mut Vec<String>) -> Result<Vec<String>> {
    let mut lines = vec![];
    let mut keep = Watched::default();
    for id in std::mem::take(&mut watched.submits) {
        match outcome_submit(db, id)? {
            Some(l) => {
                lines.push(l);
                told.push(format!("submit:{id}"));
            }
            None => keep.submits.push(id),
        }
    }
    for id in std::mem::take(&mut watched.lands) {
        match outcome_land(db, id)? {
            Some(l) => {
                lines.push(l);
                told.push(format!("land:{id}"));
            }
            None => keep.lands.push(id),
        }
    }
    for id in std::mem::take(&mut watched.runs) {
        match outcome_run(db, id)? {
            Some(l) => {
                lines.push(l);
                told.push(format!("run:{id}"));
            }
            None => keep.runs.push(id),
        }
    }
    for (tool, file) in std::mem::take(&mut watched.apart) {
        match outcome_apart(&tool, &file) {
            Some(Apart::Answered(l)) => lines.push(l),
            // Its run is watched from here, until it ends.
            Some(Apart::Queued(run)) if !told.contains(&format!("run:{run}")) => match outcome_run(db, run)? {
                Some(l) => {
                    lines.push(l);
                    told.push(format!("run:{run}"));
                }
                None => keep.runs.push(run),
            },
            Some(Apart::Queued(_)) => {}
            None => keep.apart.push((tool, file)),
        }
    }
    *watched = keep;
    Ok(lines)
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

/// What the hook tells the session it wakes: what ended, and what still runs.
pub fn message(lines: &[String], still: &Watched) -> String {
    let mut text = String::from("Nitely: work you were waiting on has ended.");
    for l in lines {
        text.push_str("\n- ");
        text.push_str(l);
    }
    if !still.is_empty() {
        text.push_str(&format!(
            "\nStill running: {}. Nitely keeps watching it after your next submit, land, run or status call; with nothing else to do, make one status call without wait_seconds and end your turn.",
            still.names().join(", ")
        ));
    }
    text.push_str("\nCarry on from here: `next_task` or the issue's own status call has the detail.");
    text
}

/// What it tells the session when its work still runs as the hook's time runs out.
pub fn still_running(still: &Watched) -> String {
    format!(
        "Nitely: still running after {} minutes: {}. To be woken when it ends, make one status call without wait_seconds (submit_status, run_status) and end your turn.",
        WAIT / 60,
        still.names().join(", ")
    )
}

/// What the waiter did.
#[derive(Debug, PartialEq)]
pub enum Woke {
    /// Nothing to watch: wake nobody.
    Nothing,
    /// A newer waiter took over the session, with what this one watched.
    Replaced,
    /// Wake the session with this.
    Wake(String),
}

/// Watches `actor`'s work, starting from what the tool call named (`named`) and what the session's
/// previous waiter still watched, until a piece ends (wake), a newer waiter takes over, or `wait`
/// runs out. Looks every `every`; `open` opens the database for
/// each look, so no connection stays open for an hour.
pub fn wait(open: &dyn Fn() -> Result<Db>, state_file: &Path, actor: &str, token: &str, named: Watched, wait: std::time::Duration, every: std::time::Duration) -> Result<Woke> {
    let deadline = std::time::Instant::now() + wait;
    let mut first = true;
    loop {
        let db = open()?;
        let step = locked(state_file, |state| {
            if first {
                state.token = token.to_string();
                // A reply that names a job already told (two calls about the same job) adds nothing.
                let mut named = named.clone();
                named.submits.retain(|id| !state.told.contains(&format!("submit:{id}")));
                named.runs.retain(|id| !state.told.contains(&format!("run:{id}")));
                state.watched.add(named);
            } else if state.token != token {
                return Ok(Some(Woke::Replaced));
            }
            let mut found = scan(&db, actor)?;
            found.submits.retain(|id| !state.told.contains(&format!("submit:{id}")));
            found.lands.retain(|id| !state.told.contains(&format!("land:{id}")));
            found.runs.retain(|id| !state.told.contains(&format!("run:{id}")));
            state.watched.add(found);
            let lines = take_ended(&db, &mut state.watched, &mut state.told)?;
            if !lines.is_empty() {
                // What still runs goes to the next waiter, whoever starts it.
                state.token.clear();
                return Ok(Some(Woke::Wake(message(&lines, &state.watched))));
            }
            if state.watched.is_empty() {
                state.token.clear();
                return Ok(Some(Woke::Nothing));
            }
            if std::time::Instant::now() >= deadline {
                state.token.clear();
                return Ok(Some(Woke::Wake(still_running(&state.watched))));
            }
            Ok(None)
        })?;
        if let Some(woke) = step {
            return Ok(woke);
        }
        first = false;
        drop(db);
        std::thread::sleep(every);
    }
}
