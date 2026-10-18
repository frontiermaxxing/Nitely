//! The land queue (docs/batching.md, "The land queue"; NIT-215). With `[land] batch`, a reviewed
//! change that arrives while another land runs waits, and what waited lands together: stacked onto
//! the integration branch one after another, checked against its review as land checks one change,
//! gated once at the stack's tip in a build lane, fast-forwarded and pushed once.
//!
//! The stack is built in a worktree of its own in the lanes folder (`<lanes dir>/stack`), with git
//! only: an agent's branch moves only once its change lands, so one that is sent back keeps the
//! branch it handed in. When the tip's gate fails, the change that broke it is found, sent back to
//! its agent with the gate's output, and the others land: first one cheap guess (the only change
//! touching a file the failing output names), then halving the stack, so one bad change in k costs
//! at most 1 + 2⌈log2 k⌉ gates rather than k.
//!
//! Each run holds the repo's land lock, so a land of one change alone (one that needs a person's
//! approval, which is given per change) and a queue run never overlap. A change lands alone, as
//! before, when the playbook batches nothing, the repo has no build lanes, it is in large-repo mode,
//! or the change needs a person's approval.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::land::{self, Landed, LandOptions};
use crate::playbook::{self, git, git_remote, GateResult, LandBatch, Playbook};

/// How long `land` waits for its change before it says the change is still in the queue.
const WAIT: Duration = Duration::from_secs(20 * 60);

/// What a change's land says about the queue it went through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BatchNote {
    /// Its row in the queue.
    pub entry: i64,
    /// The queue run that landed it or sent it back.
    pub run: Option<i64>,
    /// The other changes of that run, in stack order.
    pub with: Vec<String>,
    /// Gates that run ran.
    pub gates: u32,
    /// Still waiting in the queue (land returned before it was its turn).
    pub waiting: bool,
}

/// One change handed to the queue.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    pub repo: String,
    pub issue: String,
    pub requested_by: String,
    pub push: bool,
    pub keep: bool,
    /// queued | running | landed | sent_back
    pub state: String,
    pub queued_at: i64,
    pub ended_at: Option<i64>,
    pub run: Option<i64>,
    /// What land returned for it.
    pub result: Option<Value>,
    pub delivered_at: Option<i64>,
}

impl Entry {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "landed" | "sent_back")
    }

    fn actor(&self) -> Actor {
        if self.requested_by == "human" {
            Actor::Human
        } else {
            Actor::Agent(self.requested_by.clone())
        }
    }
}

const COLUMNS: &str = "id, repo, issue_key, requested_by, push, keep, state, queued_at, ended_at, run_id, result, delivered_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
    let result: Option<String> = r.get(10)?;
    Ok(Entry {
        id: r.get(0)?,
        repo: r.get(1)?,
        issue: r.get(2)?,
        requested_by: r.get(3)?,
        push: r.get(4)?,
        keep: r.get(5)?,
        state: r.get(6)?,
        queued_at: r.get(7)?,
        ended_at: r.get(8)?,
        run: r.get(9)?,
        result: result.and_then(|t| serde_json::from_str(&t).ok()),
        delivered_at: r.get(11)?,
    })
}

/// A queue run: its changes in stack order, what became of them, and the gates it ran.
#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: i64,
    pub repo: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub issues: Vec<String>,
    pub landed: Vec<String>,
    pub sent_back: Vec<String>,
    pub gates: u32,
    pub head: Option<String>,
    /// What it is doing while it runs: "stacking", "gating the stack", "bisecting: gating 2 of
    /// 3", "landing" (NIT-218). None once it ended.
    pub step: Option<String>,
}

fn keys(text: String) -> Vec<String> {
    text.split(',').filter(|k| !k.is_empty()).map(str::to_string).collect()
}

impl Db {
    pub fn land_queue_entry(&self, id: i64) -> Result<Option<Entry>> {
        Ok(self.conn.query_row(&format!("SELECT {COLUMNS} FROM land_queue WHERE id = ?1"), [id], row).optional()?)
    }

    /// The repo's changes waiting or landing now, oldest first.
    pub fn land_queue(&self, repo: &Path) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare(&format!("SELECT {COLUMNS} FROM land_queue WHERE repo = ?1 AND state IN ('queued', 'running') ORDER BY id"))?;
        let rows = stmt.query_map([repo_key(repo)], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The issue's latest entry in a queue.
    pub fn latest_land_queue_entry(&self, key: &str) -> Result<Option<Entry>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {COLUMNS} FROM land_queue WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1"), [key], row)
            .optional()?)
    }

    pub fn land_run(&self, id: i64) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row("SELECT id, repo, started_at, ended_at, issues, landed, sent_back, gates, head, step FROM land_run WHERE id = ?1", [id], |r| {
                Ok(Run {
                    id: r.get(0)?,
                    repo: r.get(1)?,
                    started_at: r.get(2)?,
                    ended_at: r.get(3)?,
                    issues: keys(r.get(4)?),
                    landed: keys(r.get(5)?),
                    sent_back: keys(r.get(6)?),
                    gates: r.get(7)?,
                    head: r.get(8)?,
                    step: r.get(9)?,
                })
            })
            .optional()?)
    }
}

/// How a repo is named in the queue: one form for every spelling of its path.
fn repo_key(root: &Path) -> String {
    crate::paths::normalize(&root.to_string_lossy())
}

/// Why `key` lands alone even though it could wait in a queue, or None when it goes in the queue.
pub fn lands_alone(db: &Db, pb: &Playbook, key: &str) -> Result<Option<String>> {
    if pb.land.batch == LandBatch::Off {
        return Ok(Some("the playbook lands each change alone".into()));
    }
    let Some(lanes) = &pb.lanes else {
        return Ok(Some("a batch is gated in a build lane, and this repo has no [lanes]".into()));
    };
    if lanes.large_repo {
        return Ok(Some("large-repo mode lands each change alone".into()));
    }
    if !pb.submodules.is_empty() {
        return Ok(Some("a repo whose submodules land with it lands each change alone".into()));
    }
    let Some(review) = db.reviews(key)?.into_iter().next() else {
        return Ok(Some("it has no review yet".into()));
    };
    if review.approver == "human" || !review.reasons.is_empty() || db.issue(key)?.is_some_and(|i| i.state == "Approved") {
        return Ok(Some("it needs a person's approval, which is given and checked per change".into()));
    }
    Ok(None)
}

/// `land` for a change that waits in the queue: puts it there (or finds it there), runs the queue
/// whenever no other land holds the lock, and returns once its change landed or was sent back, or
/// after `WAIT` with it still waiting.
pub fn land(db: &mut Db, root: &Path, key: &str, opts: &LandOptions, actor: &Actor) -> Result<Landed> {
    land_waiting(db, root, key, opts, actor, WAIT)
}

fn land_waiting(db: &mut Db, root: &Path, key: &str, opts: &LandOptions, actor: &Actor, wait: Duration) -> Result<Landed> {
    let id = match db.latest_land_queue_entry(key)? {
        Some(e) if !e.finished() => e.id,
        // Landed, or sent back with nothing new since, and not told yet: that is the answer.
        Some(e) if e.delivered_at.is_none() && (e.state == "landed" || !reviewed_since(db, key, e.ended_at.unwrap_or(0))?) => e.id,
        _ => enqueue(db, root, key, opts, actor)?,
    };
    let lock_path = land::lock_path(root)?;
    let deadline = Instant::now() + wait;
    let mut break_lock = opts.break_lock;
    loop {
        let entry = db.land_queue_entry(id)?.context("the land queue's record went missing")?;
        if entry.finished() {
            db.conn.execute("UPDATE land_queue SET delivered_at = ?2 WHERE id = ?1", params![id, now()])?;
            return result_of(db, &entry);
        }
        if let Some(lock) = land::Lock::try_take(&lock_path, &format!("{key} (land queue)"), break_lock)? {
            if let Some(before) = &lock.took_over {
                let said = if break_lock { format!("Broke the land lock {before} held") } else { format!("Took over the land lock {before} left: the land that took it is gone") };
                db.log_event(actor, Some(key), Some(&root.to_string_lossy()), "status", &said)?;
            }
            break_lock = false;
            run_queue(db, root)?;
            drop(lock);
            continue;
        }
        if Instant::now() >= deadline {
            let pb = Playbook::load(root)?;
            let ahead = db.land_queue(root)?.iter().take_while(|e| e.id != id).count();
            return Ok(Landed {
                issue: key.to_string(),
                base: pb.integration_branch().to_string(),
                error: Some(format!(
                    "still waiting in the land queue ({ahead} ahead of it, behind a land that is running); it lands without you: land again later to see how it went"
                )),
                batch: Some(BatchNote { entry: id, waiting: true, ..Default::default() }),
                ..Default::default()
            });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Whether the issue was reviewed again after `since`: a newer submit, so a land that was sent
/// back before it is no longer the answer.
fn reviewed_since(db: &Db, key: &str, since: i64) -> Result<bool> {
    Ok(db.reviews(key)?.first().is_some_and(|r| r.ts > since))
}

/// Puts `key` in its repo's queue.
pub fn enqueue(db: &Db, root: &Path, key: &str, opts: &LandOptions, actor: &Actor) -> Result<i64> {
    db.conn.execute(
        "INSERT INTO land_queue (repo, issue_key, requested_by, push, keep, state, queued_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
        params![repo_key(root), key, actor.id(), opts.push, opts.keep, now()],
    )?;
    let id = db.conn.last_insert_rowid();
    let ahead = db.land_queue(root)?.iter().take_while(|e| e.id != id).count();
    let place = if ahead == 0 { "first in line".to_string() } else { format!("{ahead} ahead of it") };
    db.log_event(actor, Some(key), None, "status", &format!("In the land queue ({place}); it lands with whatever else is waiting"))?;
    Ok(id)
}

fn result_of(db: &Db, entry: &Entry) -> Result<Landed> {
    let mut landed: Landed = match &entry.result {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
        None => Landed::default(),
    };
    if landed.issue.is_empty() {
        landed.issue = entry.issue.clone();
    }
    if landed.batch.is_none() {
        let run = entry.run.map(|r| db.land_run(r)).transpose()?.flatten();
        landed.batch = Some(BatchNote {
            entry: entry.id,
            run: entry.run,
            with: run.as_ref().map(|r| r.issues.iter().filter(|k| **k != entry.issue).cloned().collect()).unwrap_or_default(),
            gates: run.map(|r| r.gates).unwrap_or(0),
            waiting: false,
        });
    }
    Ok(landed)
}

/// Whether changes wait in the repo's queue.
pub fn has_waiting(db: &Db, root: &Path) -> Result<bool> {
    Ok(!db.land_queue(root)?.is_empty())
}

/// Runs the repo's queue until nothing waits: the caller holds the land lock.
pub fn run_queue(db: &mut Db, root: &Path) -> Result<()> {
    // A run that died (its process ended) left its changes running; with the lock held, no run is
    // live, so they wait again.
    db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE repo = ?1 AND state = 'running'", [repo_key(root)])?;
    loop {
        let pb = Playbook::load(root)?;
        let waiting = db.land_queue(root)?;
        let Some(oldest) = waiting.first() else { return Ok(()) };
        // With a window, the run waits for more; the lock is held meanwhile, so nothing lands
        // around it.
        let window = pb.land.batch_window();
        let ready_at = oldest.queued_at + window.as_secs() as i64;
        if now() < ready_at {
            std::thread::sleep(Duration::from_secs((ready_at - now()) as u64).min(window));
        }
        let batch: Vec<Entry> = db.land_queue(root)?.into_iter().take(pb.land.batch_max()).collect();
        let ids: Vec<i64> = batch.iter().map(|e| e.id).collect();
        if let Err(e) = run_batch(db, root, &pb, batch) {
            // An error of the run itself (not of one change): its changes are told, not retried
            // forever.
            let why = format!("the land queue stopped: {e:#}");
            for id in ids {
                // Only what the run still held: a change it put back in the queue waits for the next.
                if let Some(entry) = db.land_queue_entry(id)?.filter(|e| e.state == "running") {
                    let mut report = Landed { issue: entry.issue.clone(), base: pb.integration_branch().to_string(), ..Default::default() };
                    land::record_stopped(db, &entry.issue, &why, &mut report, &entry.actor())?;
                    finish(db, &entry, "sent_back", &report)?;
                }
            }
        }
    }
}

/// A change stacked onto the ones before it.
struct Stacked {
    entry: Entry,
    branch: String,
    worktree: Option<PathBuf>,
    /// The branch's head when it was stacked: what its review covers.
    head: String,
    /// The commit it was stacked onto, and its own tip.
    from: String,
    tip: String,
    approval: String,
    files: Vec<String>,
}

/// One queue run over `batch`, oldest first.
fn run_batch(db: &mut Db, root: &Path, pb: &Playbook, batch: Vec<Entry>) -> Result<()> {
    let base = pb.integration_branch().to_string();
    let issues: Vec<String> = batch.iter().map(|e| e.issue.clone()).collect();
    db.conn.execute(
        "INSERT INTO land_run (repo, started_at, issues) VALUES (?1, ?2, ?3)",
        params![repo_key(root), now(), issues.join(",")],
    )?;
    let run = db.conn.last_insert_rowid();
    step(db, run, "stacking")?;
    for e in &batch {
        db.conn.execute("UPDATE land_queue SET state = 'running', run_id = ?2 WHERE id = ?1", params![e.id, run])?;
        let others: Vec<&str> = issues.iter().filter(|k| **k != e.issue).map(String::as_str).collect();
        let with = if others.is_empty() { String::new() } else { format!(" with {}", others.join(", ")) };
        db.log_event(&e.actor(), Some(&e.issue), None, "status", &format!("Landing in queue run {run}{with}"))?;
    }
    let mut gates = 0u32;
    let outcome = stack_and_land(db, root, pb, run, batch, &base, &mut gates);
    // Ended however it ended, so a run that stopped early is not shown landing.
    let head = git(root, &["rev-parse", "--short", &base]).ok().map(|s| s.trim().to_string());
    db.conn.execute("UPDATE land_run SET ended_at = ?2, gates = ?3, head = ?4, step = NULL WHERE id = ?1", params![run, now(), gates, head])?;
    outcome
}

/// `run_batch`'s work once its run is recorded: stack the batch onto `base`, gate it, land it.
fn stack_and_land(db: &mut Db, root: &Path, pb: &Playbook, run: i64, batch: Vec<Entry>, base: &str, gates: &mut u32) -> Result<()> {
    let base = base.to_string();
    let current = git(root, &["branch", "--show-current"])?.trim().to_string();
    if current != base {
        bail!("the main checkout {} is on {current}, not {base}; land only writes {base}", root.display());
    }
    // Said before the gate, not after it, when the fast-forward would fail.
    if let Some(why) = land::left_index_lock(root, land::INDEX_LOCK_GRACE_SECS) {
        bail!("{why}");
    }
    let before = land::rev(root, &base)?;
    let stack = stack_worktree(root, pb, &before)?;

    // Stack, oldest first; a change that cannot go on top goes back to its agent.
    let mut stacked: Vec<Stacked> = Vec::new();
    for e in batch {
        let onto = stacked.last().map(|s| s.tip.clone()).unwrap_or_else(|| before.clone());
        let below: Vec<String> = stacked.iter().map(|s| s.entry.issue.clone()).collect();
        match stack_one(db, root, pb, &stack, &e, &onto, &base, &below) {
            Ok(s) => stacked.push(s),
            Err(why) => {
                checkout(&stack, &onto)?;
                send_back(db, run, &e, &why, None, pb)?;
            }
        }
    }
    gate_and_land(db, root, pb, run, &stack, stacked, &before, gates)
}

/// Says what the run is doing now, for the app's board row.
fn step(db: &Db, run: i64, what: &str) -> Result<()> {
    db.conn.execute("UPDATE land_run SET step = ?2 WHERE id = ?1", params![run, what])?;
    Ok(())
}

/// Gates the stack's tip and lands it; on a failure, finds the change that broke it.
#[allow(clippy::too_many_arguments)]
fn gate_and_land(db: &mut Db, root: &Path, pb: &Playbook, run: i64, stack: &Path, stacked: Vec<Stacked>, before: &str, gates: &mut u32) -> Result<()> {
    if stacked.is_empty() {
        return Ok(());
    }
    let k = stacked.len();
    step(db, run, "gating the stack")?;
    let full = gate_prefix(db, root, stack, &stacked, k, gates)?;
    if full.passed {
        return land_prefix(db, root, pb, run, stacked, before, &full, *gates);
    }
    if k == 1 {
        let only = stacked.into_iter().next().unwrap();
        return send_back(db, run, &only.entry, &gate_failed(&full, "its gate failed"), Some(&full), pb);
    }

    // One cheap guess first: the only change touching a file the failing output names. The others
    // are stacked again without it and gated; if they pass, it was that one.
    if let Some(suspect) = suspect(&stacked, &full) {
        let mut others: Vec<Stacked> = Vec::new();
        let mut restacked = true;
        for s in stacked.iter().enumerate().filter(|(i, _)| *i != suspect).map(|(_, s)| s) {
            let onto = others.last().map(|o| o.tip.clone()).unwrap_or_else(|| before.to_string());
            let below: Vec<String> = others.iter().map(|o| o.entry.issue.clone()).collect();
            match stack_one(db, root, pb, stack, &s.entry, &onto, pb.integration_branch(), &below) {
                Ok(o) => others.push(o),
                Err(_) => {
                    restacked = false;
                    break;
                }
            }
        }
        if restacked {
            step(db, run, &format!("bisecting: gating without {}", stacked[suspect].entry.issue))?;
            let without = gate_prefix(db, root, stack, &others, others.len(), gates)?;
            if without.passed {
                let bad = &stacked[suspect];
                let why = gate_failed(&full, &format!("the stacked gate failed on a file only {} changes, and passed without it", bad.entry.issue));
                send_back(db, run, &bad.entry, &why, Some(&full), pb)?;
                return land_prefix(db, root, pb, run, others, before, &without, *gates);
            }
        }
    }

    // Halving: the shortest failing prefix ends with the change that broke it (one bad change).
    let (mut lo, mut hi) = (0usize, k);
    let mut failing = full;
    let mut passing: Option<GateResult> = None;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        step(db, run, &format!("bisecting: gating {mid} of {k}"))?;
        let g = gate_prefix(db, root, stack, &stacked, mid, gates)?;
        if g.passed {
            lo = mid;
            passing = Some(g);
        } else {
            hi = mid;
            failing = g;
        }
    }
    let mut stacked = stacked;
    let rest: Vec<Stacked> = stacked.split_off(hi);
    let bad = stacked.pop().expect("the prefix ends with the bad change");
    let why = gate_failed(&failing, "bisecting the stack found that its change is the one that fails the gate");
    send_back(db, run, &bad.entry, &why, Some(&failing), pb)?;
    // The changes after it wait for the next run, stacked again without it.
    for s in &rest {
        db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE id = ?1", [s.entry.id])?;
        db.log_event(&s.entry.actor(), Some(&s.entry.issue), None, "status", &format!("Back in the land queue: {} broke the stacked gate before it", bad.entry.issue))?;
    }
    match passing {
        Some(g) if lo > 0 => land_prefix(db, root, pb, run, stacked, before, &g, *gates),
        _ => Ok(()),
    }
}

/// Gates the first `n` stacked changes: the stack worktree at the n-th tip, as a land request.
fn gate_prefix(db: &Db, root: &Path, stack: &Path, stacked: &[Stacked], n: usize, gates: &mut u32) -> Result<GateResult> {
    let last = &stacked[n - 1];
    checkout(stack, &last.tip)?;
    *gates += 1;
    playbook::run_gate_as(db, root, stack, Some(&last.entry.issue), &last.entry.actor(), crate::lanes::Kind::Land)
}

/// Why a change goes back after a failed gate, with the failing step's output.
fn gate_failed(gate: &GateResult, why: &str) -> String {
    let failed = gate.results.iter().find(|r| !r.ok);
    let step = failed.map(|r| format!(" at `{}`", r.command)).unwrap_or_default();
    let output = failed.map(|r| r.output.trim().to_string()).filter(|o| !o.is_empty()).map(|o| format!("\n{o}")).unwrap_or_default();
    format!("the gate failed{step}: {why}. Fix it on the branch, submit, and land again.{output}")
}

/// The one change that alone touches a file the failing gate's output names, if there is one.
fn suspect(stacked: &[Stacked], gate: &GateResult) -> Option<usize> {
    // What the failing steps printed, without the command lines the log echoes (`$ ...`), which
    // name what the command looks at, not what failed.
    let output: String = gate
        .results
        .iter()
        .filter(|r| !r.ok)
        .flat_map(|r| r.output.lines().filter(|l| !l.starts_with("$ ")).map(|l| l.replace('\\', "/")).collect::<Vec<_>>())
        .collect::<Vec<_>>()
        .join("\n");
    let named = |s: &Stacked| s.files.iter().any(|f| output.contains(f.as_str()));
    let hits: Vec<usize> = stacked.iter().enumerate().filter(|(_, s)| named(s)).map(|(i, _)| i).collect();
    match hits.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// The worktree the queue stacks in: detached at `at`, nothing else in it.
fn stack_worktree(root: &Path, pb: &Playbook, at: &str) -> Result<PathBuf> {
    let path = crate::lanes::dir(root, pb).join("stack");
    if !path.join(".git").exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        git(root, &["worktree", "prune"]).ok();
        if path.exists() {
            std::fs::remove_dir_all(&path).with_context(|| format!("clearing {}", path.display()))?;
        }
        git(root, &["worktree", "add", "--detach", &path.to_string_lossy(), at]).context("making the land queue's stack worktree")?;
        return Ok(path);
    }
    // A rebase a run that died left in it goes first.
    git(&path, &["rebase", "--abort"]).ok();
    checkout(&path, at)?;
    git(&path, &["clean", "-ffdq"]).ok();
    Ok(path)
}

fn checkout(stack: &Path, commit: &str) -> Result<()> {
    git(stack, &["checkout", "--quiet", "--force", "--detach", commit]).map(|_| ()).context("moving the land queue's stack worktree")
}

/// Puts `entry`'s change on top of `onto` in the stack worktree, as land would rebase it onto the
/// integration branch, and checks it against its review there. The agent's branch is not moved.
#[allow(clippy::too_many_arguments)]
fn stack_one(db: &Db, root: &Path, pb: &Playbook, stack: &Path, entry: &Entry, onto: &str, base: &str, below: &[String]) -> std::result::Result<Stacked, String> {
    let key = &entry.issue;
    let inner = || -> Result<Stacked> {
        let (branch, worktree) = land::branch_of(root, key)?;
        if let Some(wt) = &worktree {
            land::check_worktree(wt, &branch)?;
        }
        let head = land::rev(root, &branch)?;
        checkout(stack, &head)?;
        // Already on top of what is below it, or rebased there as land would.
        let on_top = git(root, &["merge-base", "--is-ancestor", onto, &head]).is_ok();
        if !on_top && git(stack, &["rebase", "--quiet", onto]).is_err() {
            let conflicts: Vec<String> = git(stack, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default().lines().map(str::to_string).collect();
            git(stack, &["rebase", "--abort"]).ok();
            let with = if below.is_empty() { base.to_string() } else { format!("{base} or with {}, landing just before it", below.join(", ")) };
            let files = if conflicts.is_empty() { String::new() } else { format!(" in {}", conflicts.join(", ")) };
            bail!("{branch} conflicts with {with}{files}; nothing was changed on your branch. Rebase it onto {base}, resolve, submit, and land again");
        }
        let tip = land::rev(stack, "HEAD")?;
        let approval = land::check_approval(db, pb, key, stack, onto, &branch)?;
        let project = db.issue(key)?.and_then(|i| i.project);
        crate::private_terms::check(project.as_deref(), stack, onto)?;
        let files = git(stack, &["diff", "--name-only", "--no-renames", onto, &tip])?.lines().map(str::to_string).collect();
        Ok(Stacked { entry: entry.clone(), branch, worktree, head, from: onto.to_string(), tip, approval, files })
    };
    inner().map_err(|e| format!("{e:#}"))
}

/// Lands the stacked changes: the integration branch fast-forwarded to the last tip and pushed
/// once, then each change landed as land lands one.
#[allow(clippy::too_many_arguments)]
fn land_prefix(db: &mut Db, root: &Path, pb: &Playbook, run: i64, stacked: Vec<Stacked>, before: &str, gate: &GateResult, gates: u32) -> Result<()> {
    let base = pb.integration_branch().to_string();
    step(db, run, "landing")?;
    let tip = stacked.last().expect("something to land").tip.clone();
    if land::rev(root, &base)? != before {
        // Something else wrote the integration branch meanwhile: they wait for the next run.
        for s in &stacked {
            db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE id = ?1", [s.entry.id])?;
        }
        bail!("{base} moved during the gate");
    }
    // A branch that gained commits during the gate: what it holds now was not reviewed or gated,
    // so it goes back; the others wait for the next run, stacked again without it.
    let gained: Vec<usize> = stacked.iter().enumerate().filter(|(_, s)| land::rev(root, &s.branch).ok().as_deref() != Some(s.head.as_str())).map(|(i, _)| i).collect();
    if !gained.is_empty() {
        for (i, s) in stacked.iter().enumerate() {
            if gained.contains(&i) {
                let why = format!("{} gained commits during the gate, after its review was checked; submit again", s.branch);
                send_back(db, run, &s.entry, &why, None, pb)?;
            } else {
                db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE id = ?1", [s.entry.id])?;
            }
        }
        return Ok(());
    }
    // Every change in a queue pushes (one that does not lands alone), so the stack pushes when
    // the repo has a remote.
    let push = playbook::has_origin(root) && stacked.iter().all(|s| s.entry.push);
    let mut warnings = Vec::new();
    if push {
        // Each pull request shows the commits that land, not the ones from before the stacking.
        for s in stacked.iter().filter(|s| s.entry.push) {
            if let Err(e) = git_remote(root, &["push", "--force-with-lease", "origin", &format!("{}:refs/heads/{}", s.tip, s.branch)]) {
                warnings.push((s.entry.id, format!("the pull request may still show the pre-rebase commits ({})", land::first_line(&e))));
            }
        }
    }
    land::fast_forward_main(root, &tip).with_context(|| land::refused_fast_forward(root))?;
    let head = git(root, &["rev-parse", "--short", "HEAD"])?.trim().to_string();
    let pushed = push.then(|| git_remote(root, &["push", "origin", &base]));
    // The after steps once for the whole stack, still under the lock.
    let mut after = Landed { base: base.clone(), landed_from: Some(before.to_string()), ..Default::default() };
    land::after_land(root, pb, &mut after);

    let keys: Vec<String> = stacked.iter().map(|s| s.entry.issue.clone()).collect();
    for s in stacked {
        let actor = s.entry.actor();
        let mut report = Landed {
            issue: s.entry.issue.clone(),
            branch: s.branch.clone(),
            base: base.clone(),
            landed: true,
            head: Some(head.clone()),
            commits: git(root, &["log", "--oneline", &format!("{}..{}", s.from, s.tip)])?.lines().map(str::to_string).collect(),
            approval: Some(s.approval.clone()),
            gate: Some(gate.clone()),
            after: after.after.clone(),
            warnings: after.warnings.clone(),
            batch: Some(BatchNote {
                entry: s.entry.id,
                run: Some(run),
                with: keys.iter().filter(|k| **k != s.entry.issue).cloned().collect(),
                gates,
                waiting: false,
            }),
            ..Default::default()
        };
        report.warnings.extend(warnings.iter().filter(|(id, _)| *id == s.entry.id).map(|(_, w)| w.clone()));
        match &pushed {
            Some(Ok(_)) => {
                report.pushed = Some(true);
                if s.entry.push {
                    git_remote(root, &["push", &format!("--force-with-lease=refs/heads/{}:{}", s.branch, s.tip), "origin", &format!(":refs/heads/{}", s.branch)]).ok();
                }
            }
            Some(Err(e)) => {
                report.pushed = Some(false);
                report.warnings.push(format!("{base} landed locally but the push failed: {}", land::first_line(e)));
            }
            None => {}
        }
        // The branch moves to what landed, so it reads as merged and its worktree as clean; but a
        // commit made on it since the check above is never thrown away.
        let still = land::rev(root, &s.branch).ok().as_deref() == Some(s.head.as_str());
        let moved = match (&s.worktree, still) {
            (_, false) => Err(anyhow::anyhow!("it gained commits after the gate, so it was left as it is")),
            (Some(wt), true) => git(wt, &["reset", "--quiet", "--keep", &s.tip]),
            (None, true) => git(root, &["branch", "--force", &s.branch, &s.tip]),
        };
        let moved = match moved {
            Ok(_) => true,
            Err(e) => {
                report.warnings.push(format!("{} was not moved to what landed ({})", s.branch, land::first_line(&e)));
                false
            }
        };
        let (cleanup, held_by) = match &s.worktree {
            Some(wt) if s.entry.keep || !moved => {
                let why = if moved { "--keep" } else { "it gained commits after the gate" };
                // Said on the issue, so a crew supervisor does not remove it once its worker exits.
                if db.crew_worker_in(wt)? {
                    db.log_event(&actor, Some(&s.entry.issue), Some(&wt.to_string_lossy()), "land_kept", &format!("Kept the worktree and branch ({why})"))?;
                }
                (format!("kept {} and {} ({why})", wt.display(), s.branch), vec![])
            }
            Some(wt) if db.crew_worker_in(wt)? => (format!("kept {} whole: the crew worker running in it removes it once it exits", wt.display()), vec![]),
            Some(wt) => land::cleanup(root, pb, wt, &s.branch, false, s.entry.keep),
            None if !moved => (format!("kept {}", s.branch), vec![]),
            None => match git(root, &["branch", "-d", &s.branch]) {
                Ok(_) => ("removed the merged branch".to_string(), vec![]),
                Err(e) => (format!("kept the branch ({})", land::first_line(&e)), vec![]),
            },
        };
        report.cleanup = Some(cleanup);
        report.held_by = held_by;
        let issue = db.issue(&s.entry.issue)?.with_context(|| format!("no issue {}", s.entry.issue))?;
        land::record_landed(db, &issue, &mut report, &actor)?;
        finish(db, &s.entry, "landed", &report)?;
        db.conn.execute("UPDATE land_run SET landed = landed || ?2 || ',' WHERE id = ?1", params![run, s.entry.issue])?;
    }
    if crate::light::reference_path(pb, root).join(".git").exists() {
        crate::light::reference_tree(pb, root).ok();
    }
    Ok(())
}

/// A change that does not land in this run goes back to its agent, with why on its issue.
fn send_back(db: &mut Db, run: i64, entry: &Entry, why: &str, gate: Option<&GateResult>, pb: &Playbook) -> Result<()> {
    let actor = entry.actor();
    let mut report = Landed { issue: entry.issue.clone(), base: pb.integration_branch().to_string(), gate: gate.cloned(), ..Default::default() };
    land::record_stopped(db, &entry.issue, why, &mut report, &actor)?;
    db.add_note(&entry.issue, &format!("Sent back from land queue run {run}: {why}"), "land", &actor)?;
    finish(db, entry, "sent_back", &report)?;
    db.conn.execute("UPDATE land_run SET sent_back = sent_back || ?2 || ',' WHERE id = ?1", params![run, entry.issue])?;
    Ok(())
}

fn finish(db: &Db, entry: &Entry, state: &str, report: &Landed) -> Result<()> {
    db.conn.execute(
        "UPDATE land_queue SET state = ?2, ended_at = ?3, result = ?4 WHERE id = ?1",
        params![entry.id, state, now(), serde_json::to_string(report)?],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;
    use crate::submit::{submit, SubmitOptions};
    use crate::test_repo::git as sh;

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        db: Db,
        actor: Actor,
    }

    /// A repo with build lanes and `[land] batch = "waiting"`, a gate that passes, a fake reviewer
    /// that passes everything, and `billing` paths that need a person.
    fn fixture() -> Fixture {
        let base = crate::test_repo::copy("land-queue", |base| {
            let text = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("{gate}", "\"exit 0\"");
            crate::test_repo::init(&base.join("repo"), &[(playbook::PLAYBOOK_PATH, &text), ("shared.txt", "one\n")]);
        });
        let repo = base.join("repo");
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        Fixture { base, repo, db, actor: Actor::Agent("claude:test".into()) }
    }

    const PLAYBOOK: &str = r#"
[worktree]
dir = "../wt/{key}"
[gate]
commands = [{gate}]
[review]
reviewer = { "*" = "fake" }
commands = { fake = ["{sh}", "-c", "cat > /dev/null; echo 'Fine.'; echo 'VERDICT: clean'"] }
[[approval.human_paths]]
pattern = "billing"
reason = "billing"
[lanes]
[land]
batch = "waiting"
"#;

    impl Fixture {
        /// Files an issue, commits `files` on its branch, and submits it (gate and review pass).
        fn submitted(&mut self, title: &str, files: &[(&str, &str)]) -> (String, PathBuf) {
            let new = NewIssue {
                project: "NIT".into(),
                title: title.into(),
                kind: "feature".into(),
                body: "b".into(),
                evidence: "none yet".into(),
                source: "human".into(),
                allow_similar: true,
                ..Default::default()
            };
            let issue = self.db.create_issue(&new, &self.actor).unwrap();
            let started = playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap();
            let wt = PathBuf::from(&started.worktree);
            for (file, content) in files {
                std::fs::write(wt.join(file), content).unwrap();
            }
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false };
            let s = submit(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap();
            assert_eq!(s.state, "In Review", "{s:?}");
            (issue.key, wt)
        }

        /// The gate from now on: fails when the tree has `bad.txt` (saying so) or `quiet-bad.txt`
        /// (saying nothing), committed on main after the changes were reviewed.
        fn strict_gate(&self) {
            let (named, quiet) = if cfg!(windows) {
                ("if exist bad.txt (echo bad.txt is in the tree & exit 1)", "if exist quiet-bad.txt exit 1")
            } else {
                ("if [ -e bad.txt ]; then echo bad.txt is in the tree; exit 1; fi", "if [ -e quiet-bad.txt ]; then exit 1; fi")
            };
            let text = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("{gate}", &format!("{named:?}, {quiet:?}"));
            std::fs::write(self.repo.join(playbook::PLAYBOOK_PATH), text).unwrap();
            sh(&self.repo, &["commit", "-q", "-am", "a stricter gate"]);
        }

        fn opts(&self) -> LandOptions<'static> {
            LandOptions { dir: "", issue_key: "", push: true, keep: false, break_lock: false }
        }

        /// Queues `keys` in order and runs the queue, as a land holding the lock would.
        fn queue_and_run(&mut self, keys: &[&str]) {
            for k in keys {
                enqueue(&self.db, &self.repo, k, &self.opts(), &self.actor).unwrap();
            }
            let lock = land::Lock::try_take(&land::lock_path(&self.repo).unwrap(), "test", false).unwrap().expect("free");
            run_queue(&mut self.db, &self.repo).unwrap();
            drop(lock);
        }

        fn entry(&self, key: &str) -> Entry {
            self.db.latest_land_queue_entry(key).unwrap().unwrap()
        }

        fn error(&self, key: &str) -> String {
            self.entry(key).result.and_then(|r| r["error"].as_str().map(str::to_string)).unwrap_or_default()
        }

        fn runs(&self) -> Vec<Run> {
            let ids: Vec<i64> = self.db.conn.prepare("SELECT id FROM land_run ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
            ids.into_iter().map(|id| self.db.land_run(id).unwrap().unwrap()).collect()
        }

        fn on_main(&self, file: &str) -> bool {
            self.repo.join(file).exists()
        }
    }

    #[test]
    fn what_waits_lands_together_on_one_gate() {
        let mut f = fixture();
        let (a, wa) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("b.txt", "b")]);
        let (c, _) = f.submitted("C", &[("c.txt", "c")]);
        f.queue_and_run(&[&a, &b, &c]);
        for k in [&a, &b, &c] {
            assert_eq!(f.entry(k).state, "landed", "{k}: {}", f.error(k));
            assert_eq!(f.db.issue(k).unwrap().unwrap().state, "Done");
        }
        let runs = f.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].gates, runs[0].landed.clone()), (1, vec![a.clone(), b.clone(), c.clone()]));
        assert!(f.on_main("a.txt") && f.on_main("b.txt") && f.on_main("c.txt"));
        assert!(!wa.exists(), "its worktree is cleaned up once landed");
        let landed: Landed = serde_json::from_value(f.entry(&a).result.unwrap()).unwrap();
        let note = landed.batch.unwrap();
        assert_eq!((note.with, note.gates), (vec![b.clone(), c.clone()], 1));
        assert!(landed.approval.unwrap().contains("no human approval is needed"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_conflict_in_the_middle_goes_back_and_the_rest_land() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("shared.txt", "from A\n")]);
        let (b, wb) = f.submitted("B", &[("shared.txt", "from B\n")]);
        let (c, _) = f.submitted("C", &[("c.txt", "c")]);
        let b_head = sh(&wb, &["rev-parse", "HEAD"]);
        f.queue_and_run(&[&a, &b, &c]);
        assert_eq!((f.entry(&a).state.as_str(), f.entry(&c).state.as_str()), ("landed", "landed"));
        assert_eq!(f.entry(&b).state, "sent_back");
        let why = f.error(&b);
        assert!(why.contains(&format!("conflicts with main or with {a}, landing just before it")) && why.contains("shared.txt"), "{why}");
        assert_eq!(sh(&wb, &["rev-parse", "HEAD"]), b_head, "its branch is as it was handed in");
        assert!(sh(&wb, &["status", "--porcelain"]).is_empty(), "and its worktree untouched");
        assert_eq!(f.runs()[0].gates, 1);
        assert!(f.db.timeline(&b).unwrap().iter().any(|t| t.kind == "land_failed"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// `n` changes, the one at `bad` failing the gate without naming a file: bisection finds it
    /// within 1 + 2⌈log2 n⌉ gates, and the others land.
    fn bisects(n: usize, bad: usize) {
        let mut f = fixture();
        let mut keys = Vec::new();
        for i in 0..n {
            let file = format!("f{i}.txt");
            let mut files = vec![(file.as_str(), "x")];
            if i == bad {
                files.push(("quiet-bad.txt", "x"));
            }
            keys.push(f.submitted(&format!("Change {i}"), &files).0);
        }
        f.strict_gate();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        f.queue_and_run(&refs);
        for (i, k) in keys.iter().enumerate() {
            let want = if i == bad { "sent_back" } else { "landed" };
            assert_eq!(f.entry(k).state, want, "change {i} ({k}): {}", f.error(k));
        }
        assert!(f.error(&keys[bad]).contains("bisecting the stack found"), "{}", f.error(&keys[bad]));
        let gates: u32 = f.runs().iter().map(|r| r.gates).sum();
        let bound = 1 + 2 * (n as f64).log2().ceil() as u32;
        assert!(gates <= bound, "{gates} gates for one bad change in {n}, over 1 + 2⌈log2 n⌉ = {bound}");
        assert!(!f.on_main("quiet-bad.txt"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_bad_change_is_bisected_out_of_three() {
        bisects(3, 1);
    }

    #[test]
    fn a_bad_change_is_bisected_out_of_five() {
        bisects(5, 3);
    }

    #[test]
    fn a_failure_that_names_one_changes_file_costs_one_more_gate() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("bad.txt", "b")]);
        let (c, _) = f.submitted("C", &[("c.txt", "c")]);
        f.strict_gate();
        f.queue_and_run(&[&a, &b, &c]);
        assert_eq!((f.entry(&a).state.as_str(), f.entry(&b).state.as_str(), f.entry(&c).state.as_str()), ("landed", "sent_back", "landed"));
        assert!(f.error(&b).contains("bad.txt is in the tree"), "the gate's output goes back with it: {}", f.error(&b));
        assert_eq!(f.runs().iter().map(|r| r.gates).sum::<u32>(), 2, "the stack, then the stack without the one it names");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_change_that_moved_since_its_review_leaves_for_a_new_one() {
        let mut f = fixture();
        let (a, wa) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("b.txt", "b")]);
        std::fs::write(wa.join("later.txt"), "after the review").unwrap();
        sh(&wa, &["add", "."]);
        sh(&wa, &["commit", "-q", "-m", "more"]);
        f.queue_and_run(&[&a, &b]);
        assert_eq!(f.entry(&a).state, "sent_back");
        assert!(f.error(&a).contains("changed since its review"), "{}", f.error(&a));
        assert_eq!(f.entry(&b).state, "landed");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_branch_that_gains_commits_during_the_gate_keeps_them_and_goes_back() {
        let mut f = fixture();
        let (a, wa) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("b.txt", "b")]);
        // The gate commits on A's branch while it runs, as its agent might.
        let wt = wa.to_string_lossy().replace('\\', "/");
        let commits = if cfg!(windows) {
            format!("if not exist \"{wt}/late.txt\" (echo late> \"{wt}/late.txt\" & git -C \"{wt}\" add late.txt & git -C \"{wt}\" commit -q -m late)")
        } else {
            format!("if [ ! -e '{wt}/late.txt' ]; then echo late > '{wt}/late.txt'; git -C '{wt}' add late.txt; git -C '{wt}' commit -q -m late; fi")
        };
        let text = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("{gate}", &format!("{commits:?}"));
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), text).unwrap();
        sh(&f.repo, &["commit", "-q", "-am", "a gate that commits on A"]);
        f.queue_and_run(&[&a, &b]);
        assert_eq!(f.entry(&a).state, "sent_back");
        assert!(f.error(&a).contains("gained commits during the gate"), "{}", f.error(&a));
        assert_eq!(f.entry(&b).state, "landed", "B waited for the next run and landed there");
        assert!(sh(&wa, &["log", "--oneline", "-1"]).contains("late"), "A's new commit is still on its branch");
        assert!(!f.on_main("late.txt") && !f.on_main("a.txt"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_land_alone_lands_what_queued_behind_it_and_a_local_land_never_goes_in_a_stack() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("b.txt", "b")]);
        // B queued while another land held the lock, and its own land stopped waiting.
        enqueue(&f.db, &f.repo, &b, &f.opts(), &f.actor).unwrap();
        // A lands alone (not to be pushed: a stack is pushed as one), and then lands B under its lock.
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: false, keep: false, break_lock: false };
        let r = land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap();
        assert!(r.landed && r.batch.is_none(), "{r:?}");
        assert_eq!(f.entry(&b).state, "landed", "{}", f.error(&b));
        assert!(f.db.latest_land_queue_entry(&a).unwrap().is_none(), "A never went in the queue");

        // With a remote, a land kept local leaves the queue alone: landing it would push it.
        crate::test_repo::add_remote(&f.repo, &f.base.join("remote.git"));
        let (c, _) = f.submitted("C", &[("c.txt", "c")]);
        let (d, _) = f.submitted("D", &[("d.txt", "d")]);
        enqueue(&f.db, &f.repo, &d, &f.opts(), &f.actor).unwrap();
        let opts = LandOptions { dir: &dir, issue_key: &c, push: false, keep: false, break_lock: false };
        assert!(land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap().landed);
        assert_eq!(f.entry(&d).state, "queued");
        assert!(!sh(&f.repo, &["ls-remote", "origin", "main"]).contains(&sh(&f.repo, &["rev-parse", "HEAD"])), "C stayed local");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn approvals_large_repos_and_repos_without_lanes_land_alone() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let pb = Playbook::load(&f.repo).unwrap();
        assert_eq!(lands_alone(&f.db, &pb, &a).unwrap(), None);
        // A change a person must approve: its approval is given and checked per change.
        let new = NewIssue {
            project: "NIT".into(),
            title: "Billing".into(),
            kind: "feature".into(),
            body: "b".into(),
            evidence: "x".into(),
            source: "human".into(),
            allow_similar: true,
            ..Default::default()
        };
        let issue = f.db.create_issue(&new, &f.actor).unwrap();
        let started = playbook::start_task(&f.db, &f.repo, &issue, "claude", &f.actor).unwrap();
        let wt = PathBuf::from(&started.worktree);
        std::fs::write(wt.join("billing.txt"), "x").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "billing"]);
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false };
        submit(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap();
        assert!(lands_alone(&f.db, &pb, &issue.key).unwrap().unwrap().contains("approval"));
        // The playbook decides the rest.
        for (text, why) in [
            ("[land]\n", "alone"),
            ("[land]\nbatch = \"waiting\"\n", "[lanes]"),
            ("[lanes]\nlarge_repo = true\n[land]\nbatch = \"waiting\"\n", "large-repo"),
        ] {
            let pb = Playbook::parse(text).unwrap();
            assert!(lands_alone(&f.db, &pb, &a).unwrap().unwrap().contains(why), "{text}");
        }
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_land_waits_its_turn_and_lands_once_the_lock_is_free() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: true, keep: false, break_lock: false };
        // Another land runs: this one waits in the queue, and says so when it stops waiting.
        let held = land::Lock::try_take(&land::lock_path(&f.repo).unwrap(), "another land", false).unwrap().unwrap();
        let waited = land_waiting(&mut f.db, &f.repo, &a, &opts, &f.actor, Duration::from_millis(600)).unwrap();
        assert!(!waited.landed && waited.error.as_deref().unwrap().contains("still waiting in the land queue"), "{waited:?}");
        let entry = waited.batch.unwrap().entry;
        drop(held);
        // Landing again finds it in the queue and runs the queue itself: through `land`.
        let r = land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap();
        assert!(r.landed, "{r:?}");
        assert_eq!(r.batch.unwrap().entry, entry, "the same place in the queue, not a second one");
        assert!(f.db.land_queue_entry(entry).unwrap().unwrap().delivered_at.is_some(), "told once");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A git index.lock left in the main checkout stops the queue's run before the gate, says what
    /// it is, and ends the run, so nothing shows it landing (NIT-280).
    #[test]
    fn a_left_index_lock_stops_the_run_and_ends_it() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: true, keep: false, break_lock: false };
        let held = land::Lock::try_take(&land::lock_path(&f.repo).unwrap(), "another land", false).unwrap().unwrap();
        land_waiting(&mut f.db, &f.repo, &a, &opts, &f.actor, Duration::from_millis(300)).unwrap();
        drop(held);
        let index_lock = f.repo.join(".git").join("index.lock");
        let left = std::fs::File::create(&index_lock).unwrap();
        left.set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 3600)).unwrap();
        drop(left);
        let r = land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap();
        assert!(!r.landed && r.error.as_deref().is_some_and(|e| e.contains("index.lock") && e.contains("before this land began")), "{r:?}");
        let open: i64 = f.db.conn.query_row("SELECT COUNT(*) FROM land_run WHERE ended_at IS NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(open, 0, "the run ended");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn changes_wait_again_when_the_branch_moves_during_their_gate() {
        let mut f = fixture();
        let (a, _) = f.submitted("A", &[("a.txt", "a")]);
        let (b, _) = f.submitted("B", &[("b.txt", "b")]);
        // The first gate commits to main while it runs (as a person pushing by hand would); the
        // stack it gated is then not what main is, so nothing lands from it.
        let mark = f.base.join("moved-once").to_string_lossy().replace('\\', "/");
        let moves = if cfg!(windows) {
            format!("if not exist \"{mark}\" (type nul > \"{mark}\" & git -C {{main}} commit -q --allow-empty -m moved)")
        } else {
            format!("if [ ! -e '{mark}' ]; then touch '{mark}'; git -C {{main}} commit -q --allow-empty -m moved; fi")
        };
        let text = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("{gate}", &format!("{moves:?}"));
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), text).unwrap();
        sh(&f.repo, &["commit", "-q", "-am", "a gate that moves main once"]);
        f.queue_and_run(&[&a, &b]);
        assert_eq!((f.entry(&a).state.as_str(), f.entry(&b).state.as_str()), ("landed", "landed"), "{} {}", f.error(&a), f.error(&b));
        let runs = f.runs();
        assert_eq!((runs.len(), runs[0].landed.len(), runs[1].landed.len()), (2, 0, 2), "the second run landed both");
        assert!(sh(&f.repo, &["log", "--oneline", "-5"]).contains("moved"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn windows_parse_as_durations() {
        let rules = |batch: &str, window: &str| Playbook::parse(&format!("[land]\nbatch = {batch:?}\nbatch_window = {window:?}\n")).unwrap().land;
        assert_eq!(rules("all", "5m").batch_window(), Duration::from_secs(300));
        assert_eq!(rules("all", "90").batch_window(), Duration::from_secs(90));
        assert_eq!(rules("waiting", "5m").batch_window(), Duration::ZERO, "only \"all\" waits");
        assert_eq!((rules("all", "0").batch_max(), Playbook::default().land.batch), (5, LandBatch::Off));
    }
}
