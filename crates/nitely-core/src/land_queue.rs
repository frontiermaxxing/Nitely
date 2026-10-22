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
//! Each run holds the repo's land lock, so a land of one change alone and a queue run never
//! overlap. Human approvals are verified separately for every change before and after the gate.
//! A change lands alone, as
//! before, when the playbook batches nothing, the repo has no build lanes, it is in large-repo mode,
//! or its managed submodule baseline changes.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::land::{self, Landed, LandOptions};
use crate::land_rules::{self, AfterGate, Bisect, Found, Next, Probe};
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

// Explicit project batching replaces a reduced submit gate with a Full combined gate.
fn review_requires_individual(db: &Db, key: &str, review: Option<&crate::submit::ReviewRecord>) -> Result<bool> {
    let Some(review) = review else { return Ok(false) };
    Ok(review.gate.as_ref().is_some_and(|g| g.plan.is_some())
        && crate::project_modes::for_issue(db, key)?.is_none_or(|s| s.options().land_batch != Some(true)))
}

/// Why `key` lands alone even though it could wait in a queue, or None when it goes in the queue.
pub fn lands_alone(db: &Db, root: &Path, pb: &Playbook, key: &str) -> Result<Option<String>> {
    let mut current = pb.clone();
    crate::project_modes::apply_issue(db, key, &mut current)?;
    let pb = &current;
    let batching = land_rules::Batching {
        batch_off: pb.land.batch == LandBatch::Off,
        checkpoint: pb.land.batch != LandBatch::Off && db.checkpoint_of_issue(key)?.is_some(),
        lanes: pb.lanes.is_some(),
        large_repo: pb.lanes.as_ref().is_some_and(|l| l.large_repo),
    };
    let submodules_unchanged = || -> Result<Option<bool>> {
        if pb.submodules.is_empty() {
            return Ok(None);
        }
        let (branch, _) = land::branch_of(root, key)?;
        Ok(Some(unchanged_submodules(root, pb, pb.integration_branch(), &branch)?))
    };
    let reviewed = || -> Result<Option<land_rules::Reviewed>> {
        let Some(review) = db.reviews(key)?.into_iter().next() else { return Ok(None) };
        Ok(Some(land_rules::Reviewed {
            individual: review_requires_individual(db, key, Some(&review))?,
            needs_signature: review.approver == "human" || !review.reasons.is_empty() || db.issue(key)?.is_some_and(|i| i.state == "Approved"),
        }))
    };
    let signed = || -> Result<bool> {
        let patch = db.reviews(key)?.into_iter().next().map(|r| r.patch_id).unwrap_or_default();
        Ok(db.standing_approval(key)?.is_some_and(|at| db.check_signed_approval(key, &patch, at).is_ok()))
    };
    Ok(land_rules::alone_reason(batching, submodules_unchanged, reviewed, signed)?.map(str::to_string))
}

/// A batch never updates a managed submodule or adopts a new baseline during rebase. Its original
/// fork, handed-in commit and integration candidate must all name the same gitlink objects.
fn unchanged_submodules(root: &Path, pb: &Playbook, onto: &str, head: &str) -> Result<bool> {
    if pb.submodules.is_empty() {
        return Ok(true);
    }
    let fork = git(root, &["merge-base", onto, head])?;
    let fork = fork.trim();
    for s in &pb.submodules {
        let pointer = |at: &str| git(root, &["ls-tree", at, "--", s.path()]);
        let original = pointer(fork)?;
        if !original.starts_with("160000 ") || original != pointer(onto)? || original != pointer(head)? {
            return Ok(false);
        }
    }
    // A proposed URL or management-policy change belongs on the ordinary single-land path too.
    Ok(git(root, &["diff", "--name-only", fork, head, "--", ".gitmodules", playbook::PLAYBOOK_PATH])?.trim().is_empty())
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
            // What it removed is said on those issues; a land that cannot look is no reason to stop.
            if let Ok(pb) = Playbook::load(root) {
                land::tidy(db, root, &pb, actor, &mut Vec::new());
            }
            drop(lock);
            // The after steps that need no lock, once the next land may go (NIT-334). With no one
            // land's change to judge by, the ones that what changed since they last ran calls for.
            let pb = Playbook::load(root)?;
            for r in land::after_land_soon(root, &pb, Some(key)).iter().filter(|r| !r.ok) {
                eprintln!("the after-land step `{}` failed; the land stands", r.command);
            }
            // Main moved: it flows into each open goal branch (goal_branch.rs).
            for said in crate::goal_branch::flow_main_in(db, root, actor) {
                db.log_event(actor, Some(key), None, "goal_branch", &said)?;
            }
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
        let waiting = db.land_queue(root)?;
        let Some(oldest) = waiting.first() else { return Ok(()) };
        let mut pb = Playbook::load(root)?;
        crate::project_modes::apply_issue(db, &oldest.issue, &mut pb)?;
        // A project can switch to individual land while this row waits. The queue already
        // holds the repository lock, so run the single-change path without reacquiring it.
        if lands_alone(db, root, &pb, &oldest.issue)?.is_some() {
            db.conn.execute("UPDATE land_queue SET state = 'running' WHERE id = ?1", [oldest.id])?;
            let dir = root.to_string_lossy();
            let opts = LandOptions { dir: &dir, issue_key: &oldest.issue, push: oldest.push, keep: oldest.keep, break_lock: false };
            let report = land::queued_alone(db, root, &oldest.issue, &opts, &oldest.actor())?;
            finish(db, oldest, if report.landed { "landed" } else { "sent_back" }, &report)?;
            continue;
        }
        // With a window, the run waits for more; the lock is held meanwhile, so nothing lands
        // around it.
        if land_rules::waits_until(oldest.queued_at, pb.land.batch_window().as_secs(), now()).is_some() {
            std::thread::sleep(Duration::from_millis(500));
            continue; // Reload human settings while the batching window is open.
        }
        // What each waiting change is, read until the first that cannot join: another policy
        // gets its own run on the next iteration.
        let mut batch = db.land_queue(root)?.into_iter().take(pb.land.batch_max()).collect::<Vec<_>>();
        let mut seen = Vec::new();
        for entry in &batch {
            let authority = crate::project_modes::authority_for_issue(db, &entry.issue)?;
            let alone = authority == pb.mode_authority && lands_alone(db, root, &pb, &entry.issue)?.is_some();
            seen.push(land_rules::Candidate { authority, alone });
            if land_rules::batch_len(&seen, &pb.mode_authority) < seen.len() {
                break;
            }
        }
        batch.truncate(land_rules::batch_len(&seen, &pb.mode_authority));
        if batch.is_empty() { continue; }
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
    // The batch's review (review_batch.rs, NIT-509): every change in it whose review waits for its
    // land batch is reviewed now, in one reviewer call, before the gate. One the review sends back
    // or hands to a person leaves the run.
    let mut batch = batch;
    if batch.iter().any(|e| db.reviews(&e.issue).ok().and_then(|r| r.into_iter().next()).is_some_and(|r| r.verdict == crate::review_batch::PENDING)) {
        step(db, run, "reviewing the batch")?;
        let actor = batch.first().map(Entry::actor).unwrap_or(Actor::Agent(land::AUTO_LANDER.into()));
        let reviewed = crate::review_batch::review_pending(db, root, &issues, &actor)?;
        let mut stays = Vec::new();
        for e in batch {
            match reviewed.iter().find(|(k, _)| *k == e.issue).map(|(_, o)| o.clone()) {
                Some(Err(why)) => send_back(db, run, &e, &why, None, pb)?,
                _ => stays.push(e),
            }
        }
        batch = stays;
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
    let full = gate_prefix(db, root, stack, &stacked, k, gates, true)?;
    if full.passed || full.skipped.is_some() {
        return land_prefix(db, root, pb, run, stack, stacked, before, &full, *gates);
    }
    // The search for the change that broke it (land_rules::Bisect): one cheap guess first, the
    // only change touching a file the failing output names, gated without it; then halving.
    let mut search = Bisect::new(k, suspect(&stacked, &full));
    let mut failing = full;
    let mut passing: Option<GateResult> = None;
    let mut without: Option<(Vec<Stacked>, GateResult)> = None;
    let mut next = search.first();
    let found = loop {
        let probe = match next {
            Next::Done(found) => break found,
            Next::Gate(probe) => probe,
        };
        next = match probe {
            Probe::Without(i) => {
                // The others, stacked again without it.
                let mut others: Vec<Stacked> = Vec::new();
                let mut restacked = true;
                for s in stacked.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, s)| s) {
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
                    step(db, run, &format!("bisecting: gating without {}", stacked[i].entry.issue))?;
                    let g = gate_prefix(db, root, stack, &others, others.len(), gates, false)?;
                    let passed = g.passed;
                    without = Some((others, g));
                    search.next(Some(passed))
                } else {
                    search.next(None)
                }
            }
            Probe::Prefix(mid) => {
                step(db, run, &format!("bisecting: gating {mid} of {k}"))?;
                let g = gate_prefix(db, root, stack, &stacked, mid, gates, false)?;
                let passed = g.passed;
                if passed {
                    passing = Some(g);
                } else {
                    failing = g;
                }
                search.next(Some(passed))
            }
        };
    };
    match found {
        Found::Only => {
            let only = stacked.into_iter().next().unwrap();
            send_back(db, run, &only.entry, &gate_failed(&failing, "its gate failed"), Some(&failing), pb)
        }
        Found::Suspect(i) => {
            let (others, gate) = without.expect("the suspect was gated without");
            let bad = &stacked[i];
            let why = gate_failed(&failing, &format!("the stacked gate failed on a file only {} changes, and passed without it", bad.entry.issue));
            send_back(db, run, &bad.entry, &why, Some(&failing), pb)?;
            land_prefix(db, root, pb, run, stack, others, before, &gate, *gates)
        }
        Found::Bad(at) => {
            let mut stacked = stacked;
            let rest: Vec<Stacked> = stacked.split_off(at + 1);
            let bad = stacked.pop().expect("the prefix ends with the bad change");
            let why = gate_failed(&failing, "bisecting the stack found that its change is the one that fails the gate");
            send_back(db, run, &bad.entry, &why, Some(&failing), pb)?;
            // The changes after it wait for the next run, stacked again without it.
            for s in &rest {
                db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE id = ?1", [s.entry.id])?;
                db.log_event(&s.entry.actor(), Some(&s.entry.issue), None, "status", &format!("Back in the land queue: {} broke the stacked gate before it", bad.entry.issue))?;
            }
            match passing {
                Some(g) if at > 0 => land_prefix(db, root, pb, run, stack, stacked, before, &g, *gates),
                _ => Ok(()),
            }
        }
    }
}

/// Gates the first `n` stacked changes: the stack worktree at the n-th tip, as a land request.
#[allow(clippy::too_many_arguments)]
fn gate_prefix(db: &Db, root: &Path, stack: &Path, stacked: &[Stacked], n: usize, gates: &mut u32, allow_yolo: bool) -> Result<GateResult> {
    let last = &stacked[n - 1];
    checkout(stack, &last.tip)?;
    let keys: Vec<&str> = stacked[..n].iter().map(|s| s.entry.issue.as_str()).collect();
    let authority = crate::project_modes::authority_for_issue(db, keys[0])?;
    for key in &keys {
        anyhow::ensure!(crate::project_modes::authority_for_issue(db, key)? == authority, "project modes changed during the land batch; submit again");
    }
    // A skipped prefix proves nothing about which change caused the failed full stack.
    let mut gate = if allow_yolo {
        playbook::run_automatic_gate(db, root, stack, &keys, &last.entry.actor(), crate::lanes::Kind::Land)?
    } else {
        playbook::run_gate_as(db, root, stack, Some(&last.entry.issue), &last.entry.actor(), crate::lanes::Kind::Land)?
    };
    if !allow_yolo {
        // Diagnostic prefixes deliberately run the Full gate; bind that stronger evidence to
        // the same mode authority so it cannot be reused across a settings change.
        gate.mode_authority = authority;
    }
    crate::project_modes::check_gate_authority(db, &keys, &gate)?;
    if gate.skipped.is_none() { *gates += 1; }
    Ok(gate)
}

/// Why a change goes back after a failed gate, with the failing step's output.
fn gate_failed(gate: &GateResult, why: &str) -> String {
    let failed = gate.results.iter().find(|r| !r.ok);
    let step = failed.map(|r| format!(" at `{}`", r.command)).unwrap_or_default();
    let output = failed.map(|r| r.output.trim().to_string()).filter(|o| !o.is_empty()).map(|o| format!("\n{o}")).unwrap_or_default();
    if let Some(r) = gate.timed_out() {
        return format!("the gate did not finish: {}, which says nothing about the change. Land again.{output}", r.stopped());
    }
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
            crate::unlink::remove_tree(&path).with_context(|| format!("clearing {}", path.display()))?;
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

/// Where each stacked change's branch is now, in one git (NIT-508); a branch that is gone is missing.
fn heads(root: &Path, stacked: &[Stacked]) -> Result<std::collections::HashMap<String, String>> {
    let mut args = vec!["for-each-ref".to_string(), "--format=%(refname) %(objectname)".to_string()];
    args.extend(stacked.iter().map(|s| format!("refs/heads/{}", s.branch)));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // A pattern also lists the refs under it (refs/heads/a/b for a): only the exact names count.
    Ok(git(root, &args)?
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter_map(|(name, id)| name.strip_prefix("refs/heads/").map(|b| (b.to_string(), id.trim().to_string())))
        .filter(|(b, _)| stacked.iter().any(|s| &s.branch == b))
        .collect())
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
        crate::project_modes::check_playbook_authority(db, &[key], pb)?;
        anyhow::ensure!(!review_requires_individual(db, key, db.reviews(key)?.first())?,
            "reduced verification is bound to this individual patch; it must land alone");
        let (branch, worktree) = land::branch_of(root, key)?;
        if let Some(wt) = &worktree {
            land::check_worktree(wt, &branch)?;
        }
        let head = land::rev(root, &branch)?;
        anyhow::ensure!(unchanged_submodules(root, pb, onto, &head)?,
            "the managed submodule baseline changed; this change must land alone");
        checkout(stack, &head)?;
        // Already on top of what is below it, or rebased there as land would.
        let on_top = git(root, &["merge-base", "--is-ancestor", onto, &head]).is_ok();
        if !on_top && git(stack, &["rebase", "--quiet", onto]).is_err()
            && !crate::test_command_merge::finish_rebase(stack).unwrap_or(false) {
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
fn land_prefix(db: &mut Db, root: &Path, pb: &Playbook, run: i64, stack: &Path, stacked: Vec<Stacked>, before: &str, gate: &GateResult, gates: u32) -> Result<()> {
    let base = pb.integration_branch().to_string();
    step(db, run, "landing")?;
    let tip = stacked.last().expect("something to land").tip.clone();
    // Main moving sends all back to the queue; a branch that gained commits during the gate holds
    // what was not reviewed or gated, so it goes back and the others wait for the next run.
    let base_moved = land::rev(root, &base)? != before;
    let now_at = if base_moved { Default::default() } else { heads(root, &stacked)? };
    let gained: Vec<bool> = stacked.iter().map(|s| now_at.get(&s.branch) != Some(&s.head)).collect();
    match land_rules::after_gate(base_moved, &gained) {
        AfterGate::RequeueAll => {
            for s in &stacked {
                db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE id = ?1", [s.entry.id])?;
            }
            bail!("{base} moved during the gate");
        }
        AfterGate::SendBack(gained) => {
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
        AfterGate::Authorize => {}
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
    let keys: Vec<&str> = stacked.iter().map(|s| s.entry.issue.as_str()).collect();
    crate::project_modes::check_gate_authority(db, &keys, gate)?;
    crate::project_modes::check_playbook_authority(db, &keys, pb)?;
    let current = Playbook::load(root)?;
    let now_at = heads(root, &stacked)?;
    let mut refused = false;
    for s in &stacked {
        // Each authorization covers only its own delta, not the combined stack. A revoked
        // signature, changed review, or newly required human approval cannot ride a green gate.
        // Checked at its own tip, which the stack worktree's repository has (NIT-508).
        let authorization = (|| -> Result<()> {
            anyhow::ensure!(!review_requires_individual(db, &s.entry.issue, db.reviews(&s.entry.issue)?.first())?,
                "reduced verification is bound to this individual patch; it must land alone");
            anyhow::ensure!(unchanged_submodules(root, &current, &s.from, &s.tip)?,
                "the managed submodule baseline changed; this change must land alone");
            anyhow::ensure!(now_at.get(&s.branch) == Some(&s.head), "the branch changed after the gate; submit again");
            // Report the exact revoked/missing approval before the batching eligibility check,
            // which also rejects unsigned changes but cannot explain their authorization failure.
            land::check_approval_at(db, &current, &s.entry.issue, stack, &s.from, &s.tip, &s.branch)?;
            anyhow::ensure!(lands_alone(db, root, &current, &s.entry.issue)?.is_none(),
                "the current project policy requires this change to land individually");
            Ok(())
        })();
        if let Err(e) = authorization {
            send_back(db, run, &s.entry, &format!("{e:#}"), None, &current)?;
            refused = true;
        }
    }
    if refused {
        db.conn.execute("UPDATE land_queue SET state = 'queued', run_id = NULL WHERE run_id = ?1 AND state = 'running'", [run])?;
        return Ok(());
    }
    crate::project_modes::check_playbook_authority(db, &keys, pb)?;
    anyhow::ensure!(land::rev(root, &base)? == before, "{base} moved after the gate; land again");
    land::fast_forward_main(root, &tip).with_context(|| land::refused_fast_forward(root))?;
    let head = git(root, &["rev-parse", "--short", "HEAD"])?.trim().to_string();
    let pushed = push.then(|| git_remote(root, &["push", "origin", &base]));
    // The after steps once for the whole stack, still under the lock.
    let mut after = Landed { base: base.clone(), landed_from: Some(before.to_string()), ..Default::default() };
    {
        // This install serves the actual landed prefix, which may differ from the queued batch.
        // A shared install belongs to its common project, never an arbitrary member issue or
        // the issue whose caller happened to drain the queue.
        let keys: Vec<&str> = stacked.iter().map(|s| s.entry.issue.as_str()).collect();
        let _telemetry = install_scope(db, &keys);
        land::after_land(root, pb, &mut after);
    }

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
            // Where this change started, so the post-merge backstop records the landing (NIT-457).
            landed_from: Some(s.from.clone()),
            own_tip: Some(s.tip.clone()),
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
                // Said on the issue, so neither a crew supervisor once its worker exits nor a later
                // land (leftovers.rs) removes it.
                db.log_event(&actor, Some(&s.entry.issue), Some(&wt.to_string_lossy()), "land_kept", &format!("Kept the worktree and branch ({why})"))?;
                (format!("kept {} and {} ({why})", wt.display(), s.branch), vec![])
            }
            Some(wt) if db.crew_worker_in(wt)? => (format!("kept {} whole: the crew worker running in it removes it once it exits", wt.display()), vec![]),
            Some(wt) => {
                let (message, held) = land::cleanup(root, pb, wt, &s.branch, false, s.entry.keep);
                if !s.entry.keep && land::made_by_layout(root, pb, wt) && wt.exists() {
                    crate::leftovers::note_held(db, &actor, &s.entry.issue, wt, &message)?;
                }
                (message, held)
            }
            None if !moved => (format!("kept {}", s.branch), vec![]),
            None => match git(root, &["branch", "-d", &s.branch]) {
                Ok(_) => ("removed the merged branch".to_string(), vec![]),
                Err(e) => (format!("kept the branch ({})", land::first_line(&e)), vec![]),
            },
        };
        report.cleanup = Some(cleanup);
        report.held_by = held_by;
        let issue = db.issue(&s.entry.issue)?.with_context(|| format!("no issue {}", s.entry.issue))?;
        land::record_landed(db, root, &issue, &mut report, &actor)?;
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

fn install_scope(db: &Db, issues: &[&str]) -> crate::performance_attribution::Scope {
    let projects: Vec<_> = issues.iter().map(|key| db.issue(key).ok().flatten().and_then(|issue| issue.project)).collect();
    let project = projects.first().cloned().flatten().filter(|p| projects.iter().all(|value| value.as_ref() == Some(p)));
    let issue = (issues.len() == 1).then(|| issues[0].to_string());
    crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project, issue, ..Default::default() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_stacked_gate_that_ran_past_the_lane_limit_sends_nobody_to_fix_code() {
        let step = |timed_out: bool| crate::playbook::CommandResult { step: None, command: "npm test".into(), ok: false, seconds: 1800, output: "out".into(), skipped: false, timed_out };
        let gate = |timed_out: bool| super::GateResult { mode_authority: None, plan: None, skipped: None, passed: false, commit: "c".into(), results: vec![step(timed_out)], warnings: vec![], flaked: vec![], lane_run: None };
        let stopped = super::gate_failed(&gate(true), "its gate failed");
        assert!(stopped.starts_with("the gate did not finish: `npm test` ran past the lane's time limit") && !stopped.contains("Fix it"), "{stopped}");
        assert!(super::gate_failed(&gate(false), "its gate failed").starts_with("the gate failed at `npm test`: its gate failed. Fix it"));
    }

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

    /// A repo with build lanes, a Custom mode that lands in batches, a gate that passes, a fake reviewer
    /// that passes everything, and `billing` paths that need a person.
    fn fixture() -> Fixture {
        let base = crate::test_repo::copy("land-queue", |base| {
            let text = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("{gate}", "\"exit 0\"");
            crate::test_repo::init(&base.join("repo"), &[(playbook::PLAYBOOK_PATH, &text), ("shared.txt", "one\n")]);
        });
        let repo = base.join("repo");
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        // Landing in batches is the review mode's to decide, not the playbook's (NIT-499): a migrated
        // project's mode, not yet saved, so the reviews the tests seed carry no authority to check.
        db.conn.execute(r#"INSERT INTO project_review_mode(project,mode,overrides,explicit) VALUES('NIT','custom','{"land_batch":true}',0)"#, []).unwrap();
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
"#;

    impl Fixture {
        fn committed(&mut self, title: &str, files: &[(&str, &str)]) -> (String, PathBuf) {
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
                if let Some(dir) = wt.join(file).parent() {
                    std::fs::create_dir_all(dir).unwrap();
                }
                std::fs::write(wt.join(file), content).unwrap();
            }
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);
            (issue.key, wt)
        }

        /// Files an issue, commits `files` on its branch, and submits it (gate and review pass).
        fn submitted(&mut self, title: &str, files: &[(&str, &str)]) -> (String, PathBuf) {
            let (key, wt) = self.committed(title, files);
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
            let s = submit(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap();
            assert_eq!(s.state, "In Review", "{s:?}");
            (key, wt)
        }

        /// As `submitted`, with a passing review of the real patch standing in for the submit:
        /// what the queue starts from, without a gate and a reviewer per member.
        fn reviewed(&mut self, title: &str, files: &[(&str, &str)]) -> (String, PathBuf) {
            let (key, wt) = self.committed(title, files);
            crate::test_repo::reviewed(&mut self.db, &wt, &key, &self.actor);
            (key, wt)
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

        fn require_and_sign(&mut self, keys: &[&str]) {
            let path = self.repo.join(playbook::PLAYBOOK_PATH);
            std::fs::write(&path, std::fs::read_to_string(&path).unwrap().replace("pattern = \"billing\"", "pattern = \"txt$\"")).unwrap();
            sh(&self.repo, &["commit", "-qam", "require approval"]);
            let secret = [42; 32];
            crate::approval::trust_in_tests(Some(crate::approval::public_of(&secret)));
            for key in keys {
                self.db.move_issue(key, "Approved", None, None, &Actor::Human).unwrap();
                self.db.sign_approval(key, &secret).unwrap();
            }
        }
    }

    #[test]
    fn approved_changes_with_an_unchanged_managed_submodule_share_one_gate() {
        let mut f = fixture();
        let upstream = f.base.join("engine-upstream");
        crate::test_repo::init(&upstream, &[("engine.txt", "baseline")]);
        sh(&f.repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &upstream.to_string_lossy(), "engine"]);
        let path = f.repo.join(playbook::PLAYBOOK_PATH);
        let text = format!("{}\n[[submodules]]\npath = \"engine\"\n", std::fs::read_to_string(&path).unwrap());
        std::fs::write(&path, text).unwrap();
        sh(&f.repo, &["add", "."]);
        sh(&f.repo, &["commit", "-qm", "managed engine"]);
        let pointer = sh(&f.repo, &["ls-tree", "HEAD", "--", "engine"]);
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
        let (b, _) = f.reviewed("B", &[("b.txt", "b")]);
        f.require_and_sign(&[&a, &b]);
        let pb = Playbook::load(&f.repo).unwrap();
        assert!(lands_alone(&f.db, &f.repo, &pb, &a).unwrap().is_none());
        assert!(lands_alone(&f.db, &f.repo, &pb, &b).unwrap().is_none());
        f.queue_and_run(&[&a, &b]);
        assert_eq!(f.entry(&a).state, "landed", "{}", f.error(&a));
        assert_eq!(f.entry(&b).state, "landed", "{}", f.error(&b));
        assert_eq!(f.runs().len(), 1);
        assert_eq!(f.runs()[0].gates, 1);
        assert_eq!(sh(&f.repo, &["ls-tree", "HEAD", "--", "engine"]), pointer);
        // A real pointer change cannot enter the same path.
        let before = sh(&f.repo, &["rev-parse", "HEAD"]);
        std::fs::write(f.repo.join("engine/engine.txt"), "changed").unwrap();
        sh(&f.repo.join("engine"), &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qam", "engine change"]);
        sh(&f.repo, &["add", "engine"]);
        sh(&f.repo, &["commit", "-qm", "new engine"]);
        assert!(!unchanged_submodules(&f.repo, &pb, &before, "HEAD").unwrap());
        assert!(!unchanged_submodules(&f.repo, &pb, "HEAD", &before).unwrap(), "a stale baseline also stays separate");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn approval_revoked_after_the_combined_gate_prevents_any_fast_forward() {
        let mut f = fixture();
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
        let (b, _) = f.reviewed("B", &[("b.txt", "b")]);
        f.require_and_sign(&[&a, &b]);
        let pb = Playbook::load(&f.repo).unwrap();
        for key in [&a, &b] {assert!(lands_alone(&f.db, &f.repo, &pb, key).unwrap().is_none(), "both signed changes start eligible for the batch");}
        let before = sh(&f.repo, &["rev-parse", "main"]);
        let stack = stack_worktree(&f.repo, &pb, &before).unwrap();
        let mut entries = Vec::new();
        for key in [&a, &b] {
            enqueue(&f.db, &f.repo, key, &f.opts(), &f.actor).unwrap();
            entries.push(f.entry(key));
        }
        f.db.conn.execute("INSERT INTO land_run (repo, started_at, issues) VALUES (?1, ?2, ?3)", params![repo_key(&f.repo), now(), format!("{a},{b}")]).unwrap();
        let run = f.db.conn.last_insert_rowid();
        let first = stack_one(&f.db, &f.repo, &pb, &stack, &entries[0], &before, "main", &[]).unwrap();
        let second = stack_one(&f.db, &f.repo, &pb, &stack, &entries[1], &first.tip, "main", std::slice::from_ref(&a)).unwrap();
        let stacked = vec![first, second];
        let mut gates = 0;
        let gate = gate_prefix(&f.db, &f.repo, &stack, &stacked, 2, &mut gates, false).unwrap();
        assert!(gate.passed);
        f.db.conn.execute("DELETE FROM approval WHERE issue_key = ?1", [&b]).unwrap();
        land_prefix(&mut f.db, &f.repo, &pb, run, &stack, stacked, &before, &gate, gates).unwrap();
        assert_eq!(sh(&f.repo, &["rev-parse", "main"]), before);
        assert_eq!(f.entry(&b).state, "sent_back");
        assert!(f.error(&b).contains("not signed"), "{}", f.error(&b));
        assert_eq!(f.entry(&a).state, "queued");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn explicit_basic_submissions_can_batch_only_with_a_full_combined_gate() {
        use crate::project_modes::{Mode, Options};
        let mut f = fixture();
        let path = f.repo.join(playbook::PLAYBOOK_PATH);
        let policy = std::fs::read_to_string(&path).unwrap().replace("commands = [\"exit 0\"]", "commands = [\"echo full-batch\"]");
        std::fs::write(&path, format!("{policy}\n[verification]\nversion=1\nbasic=['echo basic-submit']\n")).unwrap();
        sh(&f.repo, &["commit", "-qam", "separate Basic and Full checks"]);
        // Fast deliberately lands individually; this scenario explicitly opts into batching.
        let options = Options { test_depth: Some(crate::project_modes::TestDepth::Basic), pre_review: Some(crate::project_modes::PreReview::Off), land_batch: Some(true), shared_lanes: Some(true), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, options);
        let (a, _) = f.submitted("First Basic change", &[("first.txt", "one")]);
        let (b, _) = f.submitted("Second Basic change", &[("second.txt", "two")]);
        for key in [&a, &b] {
            let review = f.db.reviews(key).unwrap().remove(0);
            assert_eq!(review.verdict, "disabled");
            assert_eq!(review.gate.as_ref().unwrap().plan.as_ref().unwrap().effective, "basic");
            assert!(lands_alone(&f.db, &f.repo, &Playbook::load(&f.repo).unwrap(), key).unwrap().is_none());
        }
        f.queue_and_run(&[&a, &b]);
        assert!(f.on_main("first.txt") && f.on_main("second.txt"));
        assert_eq!(f.runs().last().unwrap().gates, 1);
        let report: Landed = serde_json::from_value(f.entry(&a).result.unwrap()).unwrap();
        let gate = report.gate.unwrap();
        assert!(gate.passed && gate.plan.is_none());
        assert!(gate.results.iter().any(|r| r.command == "echo full-batch"));
    }

    #[test]
    fn queued_mode_change_routes_individually_and_refuses_stale_submission() {
        use crate::project_modes::{Mode, Options};
        let mut f = fixture();
        let (a, _) = f.submitted("Queued change", &[("queued.txt", "queued")]);
        enqueue(&f.db, &f.repo, &a, &f.opts(), &f.actor).unwrap();
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, Options { land_batch: Some(false), ..Default::default() });
        run_queue(&mut f.db, &f.repo).unwrap();
        assert_eq!(f.entry(&a).state, "sent_back");
        assert!(!f.on_main("queued.txt"));
        assert!(f.runs().is_empty(), "a queued row cannot retain an obsolete batched-land policy");
        assert!(f.error(&a).contains("mode changed"), "{}", f.error(&a));
    }

    #[test]
    fn yolo_batch_lands_without_a_gate_pass_or_cached_success() {
        let mut f = fixture();
        f.strict_gate();
        let off = crate::project_modes::Options { test_depth: Some(crate::project_modes::TestDepth::Off), pre_review: Some(crate::project_modes::PreReview::Off), land_batch: Some(true), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Custom, off);
        let (a, _) = f.submitted("Unchecked first", &[("bad.txt", "fails the configured gate")]);
        let (b, _) = f.submitted("Unchecked second", &[("second.txt", "second")]);
        f.queue_and_run(&[&a, &b]);
        assert!(f.on_main("bad.txt") && f.on_main("second.txt"));
        assert_eq!(f.runs().last().unwrap().gates, 0);
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Done");
        assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "Done");
        assert!(f.db.timeline(&a).unwrap().iter().any(|e| e.kind == "gate_skipped"));
        assert!(!f.db.timeline(&a).unwrap().iter().any(|e| e.kind == "gate_passed"));
    }

    #[test]
    #[cfg(windows)]
    fn attribution_queued_installs_use_actual_landed_prefix() {
        let mut f = fixture();
        let path = f.repo.join(playbook::PLAYBOOK_PATH);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}\n[[land.after]]\nrun = \"echo installed fixture\"\n")).unwrap();
        sh(&f.repo, &["commit", "-qam", "add fixture install"]);
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
        let (b, _) = f.reviewed("B", &[("b.txt", "b")]);
        let (c, _) = f.reviewed("C", &[("c.txt", "c")]);
        let store = crate::performance::Store::open(&f.base.join("performance")).unwrap();
        // Simulate A's caller draining another issue, then a shared batch. Each after-land
        // command must get context from what actually landed, not this ambient caller.
        let caller = crate::performance_attribution::Scope::new(&f.db, crate::performance_attribution::Context {
            project: Some("NIT".into()), issue: Some(a.clone()), job_id: Some(123), ..Default::default()
        });
        f.queue_and_run(&[&b]);
        f.queue_and_run(&[&a, &c]);
        drop(caller);
        assert_eq!(f.entry(&b).state, "landed");
        assert_eq!(f.entry(&a).state, "landed");
        assert_eq!(f.entry(&c).state, "landed");
        let installs: Vec<_> = store.recent_workloads(0, i64::MAX, 100, None).unwrap().into_iter().filter(|w| w.kind == "install").collect();
        assert_eq!(installs.len(), 2);
        assert!(installs.iter().any(|w| w.context.issue.as_deref() == Some(b.as_str())));
        assert!(installs.iter().any(|w| w.context.issue.is_none()), "shared install must not select a member issue");
        for install in installs {
            assert_eq!(install.context.project.as_deref(), Some("NIT"));
            assert!(install.context.job_id.is_none(), "caller A's submit job must not leak");
            assert!(install.end_ms.is_some());
            assert!(install.job_accounting.as_ref().unwrap().processes >= 1);
        }
        drop(store);
        drop(f.db);
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// One queue run over what waited: the changes that stack land together on one gate; one that
    /// conflicts with the change landing just before it goes back with its branch and worktree as
    /// handed in; one that moved since its review goes back for a new one. Each change it landed
    /// records its verification as a single land does, and the `[[land.after]]` steps run once for
    /// the stack, said on every change it landed (NIT-469).
    #[test]
    fn what_waits_lands_together_on_one_gate_and_what_cannot_goes_back() {
        use crate::project_modes::{Backstop, Mode, Options};
        let mut f = fixture();
        let installs = f.base.join("installs.txt");
        let path = f.repo.join(playbook::PLAYBOOK_PATH);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}\n[[land.after]]\nrun = \"echo installed>> {}\"\n", installs.to_string_lossy().replace('\\', "/"))).unwrap();
        sh(&f.repo, &["commit", "-qam", "an install after landing"]);
        let backstop = Options { full_backstop: Some(Backstop { enabled: true, landings: 5, delay_seconds: 1800 }), land_batch: Some(true), ..Default::default() };
        crate::project_modes::set_for_test(&mut f.db, "NIT", Mode::Custom, backstop);
        let (a, wa) = f.submitted("A", &[("shared.txt", "from A\n")]);
        let (b, wb) = f.reviewed("B", &[("shared.txt", "from B\n")]);
        let (m, wm) = f.reviewed("M", &[("m.txt", "m")]);
        let (c, _) = f.submitted("C", &[("c.txt", "c")]);
        let b_head = sh(&wb, &["rev-parse", "HEAD"]);
        std::fs::write(wm.join("later.txt"), "after the review").unwrap();
        sh(&wm, &["add", "."]);
        sh(&wm, &["commit", "-q", "-m", "more"]);
        f.queue_and_run(&[&a, &b, &m, &c]);
        for k in [&a, &c] {
            assert_eq!(f.entry(k).state, "landed", "{k}: {}", f.error(k));
            assert_eq!(f.db.issue(k).unwrap().unwrap().state, "Done");
        }
        let runs = f.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].gates, runs[0].landed.clone()), (1, vec![a.clone(), c.clone()]));
        assert!(f.on_main("c.txt") && !f.on_main("m.txt"));
        assert!(!wa.exists(), "its worktree is cleaned up once landed");
        let landed: Landed = serde_json::from_value(f.entry(&a).result.unwrap()).unwrap();
        let note = landed.batch.unwrap();
        assert_eq!((note.with, note.gates), (vec![c.clone()], 1));
        assert!(landed.approval.unwrap().contains("no human approval is needed"));
        for k in [&a, &c] {
            let timeline = f.db.timeline(k).unwrap();
            assert!(!timeline.iter().any(|e| e.kind == "verification_record_failed"), "{k}: {timeline:?}");
            assert!(timeline.iter().any(|e| e.kind == "land_after" && e.summary.as_deref().is_some_and(|s| s.contains("ran `echo installed"))), "{k}: {timeline:?}");
        }
        let recorded: i64 = f.db.conn.query_row("SELECT COUNT(*) FROM verification_landing WHERE issue_key IN (?1, ?2)", rusqlite::params![a, c], |r| r.get(0)).unwrap();
        assert_eq!(recorded, 2, "both lands' verification records");
        let ran = std::fs::read_to_string(&installs).unwrap();
        assert_eq!(ran.lines().filter(|l| l.trim() == "installed").count(), 1, "one install for the run: {ran:?}");
        assert!(f.db.after_land_failed().unwrap().is_none());

        assert_eq!(f.entry(&b).state, "sent_back");
        let why = f.error(&b);
        assert!(why.contains(&format!("conflicts with main or with {a}, landing just before it")) && why.contains("shared.txt"), "{why}");
        assert_eq!(sh(&wb, &["rev-parse", "HEAD"]), b_head, "its branch is as it was handed in");
        assert!(sh(&wb, &["status", "--porcelain"]).is_empty(), "and its worktree untouched");
        assert!(f.db.timeline(&b).unwrap().iter().any(|t| t.kind == "land_failed"));

        assert_eq!(f.entry(&m).state, "sent_back");
        assert!(f.error(&m).contains("changed since its review"), "{}", f.error(&m));
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
            keys.push(f.reviewed(&format!("Change {i}"), &files).0);
        }
        f.strict_gate();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        f.queue_and_run(&refs);
        for (i, k) in keys.iter().enumerate() {
            let want = if i == bad { "sent_back" } else { "landed" };
            assert_eq!(f.entry(k).state, want, "change {i} ({k}): {}", f.error(k));
            assert_eq!(f.on_main(&format!("f{i}.txt")), i != bad, "only good members reach main");
            if i != bad {
                assert_eq!(f.db.issue(k).unwrap().unwrap().state, "Done");
            }
        }
        assert!(f.error(&keys[bad]).contains("bisecting the stack found"), "{}", f.error(&keys[bad]));
        let gates: u32 = f.runs().iter().map(|r| r.gates).sum();
        let bound = 1 + 2 * (n as f64).log2().ceil() as u32;
        assert!(gates <= bound, "{gates} gates for one bad change in {n}, over 1 + 2⌈log2 n⌉ = {bound}");
        assert!(!f.on_main("quiet-bad.txt"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// Three changes, the middle one bad: one prefix passes and one fails, so both halving outcomes
    /// run on real gates. land_rules::bisection_finds_the_change... proves the search for stacks of
    /// 1 to 9 (NIT-528).
    #[test]
    fn a_bad_change_is_bisected_out_of_three() {
        bisects(3, 1);
    }

    #[test]
    fn a_failure_that_names_one_changes_file_costs_one_more_gate() {
        let mut f = fixture();
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
        let (b, _) = f.reviewed("B", &[("bad.txt", "b")]);
        let (c, _) = f.reviewed("C", &[("c.txt", "c")]);
        f.strict_gate();
        f.queue_and_run(&[&a, &b, &c]);
        assert_eq!((f.entry(&a).state.as_str(), f.entry(&b).state.as_str(), f.entry(&c).state.as_str()), ("landed", "sent_back", "landed"));
        assert!(f.error(&b).contains("bad.txt is in the tree"), "the gate's output goes back with it: {}", f.error(&b));
        assert_eq!(f.runs().iter().map(|r| r.gates).sum::<u32>(), 2, "the stack, then the stack without the one it names");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_land_alone_lands_what_queued_behind_it_and_a_local_land_never_goes_in_a_stack() {
        let mut f = fixture();
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
        let (b, _) = f.reviewed("B", &[("b.txt", "b")]);
        // B queued while another land held the lock, and its own land stopped waiting.
        enqueue(&f.db, &f.repo, &b, &f.opts(), &f.actor).unwrap();
        // A lands alone (not to be pushed: a stack is pushed as one), and then lands B under its lock.
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: false, keep: false, break_lock: false };
        let r = land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap();
        assert!(r.landed && r.batch.is_none(), "{r:?}");
        assert_eq!(f.entry(&b).state, "landed", "{}", f.error(&b));
        assert!(f.db.latest_land_queue_entry(&a).unwrap().is_none(), "A never went in the queue");
        // A is recorded before the queue behind it runs, not after (NIT-600): its issue is Done
        // while B is still to land, whatever B's run costs.
        let landed_at = |key: &str| f.db.conn.query_row("SELECT MIN(id) FROM event WHERE issue_key = ?1 AND type = 'landed'", [key], |r| r.get::<_, i64>(0)).unwrap();
        assert!(landed_at(&a) < landed_at(&b), "A's land recorded first");

        // With a remote, a land kept local leaves the queue alone: landing it would push it.
        crate::test_repo::add_remote(&f.repo, &f.base.join("remote.git"));
        let (c, _) = f.reviewed("C", &[("c.txt", "c")]);
        let (d, _) = f.reviewed("D", &[("d.txt", "d")]);
        enqueue(&f.db, &f.repo, &d, &f.opts(), &f.actor).unwrap();
        let opts = LandOptions { dir: &dir, issue_key: &c, push: false, keep: false, break_lock: false };
        assert!(land::land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap().landed);
        assert_eq!(f.entry(&d).state, "queued");
        assert!(!sh(&f.repo, &["ls-remote", "origin", "main"]).contains(&sh(&f.repo, &["rev-parse", "HEAD"])), "C stayed local");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_land_waits_its_turn_and_lands_once_the_lock_is_free() {
        let mut f = fixture();
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
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
        let (a, _) = f.reviewed("A", &[("a.txt", "a")]);
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

    /// Under a mode that reviews each land batch (Fast, NIT-509): submit only gates; the queue run
    /// reviews what waits in it in one reviewer call before its gate, sends back the change the
    /// review asks to fix, and lands the others on one gate. A change landing by itself is reviewed
    /// at its land, and land refuses a change whose review has not run.
    #[test]
    fn a_land_batch_is_reviewed_once_before_its_gate_and_nothing_lands_unreviewed() {
        let mut f = fixture();
        // A reviewer that logs each call and, for a batch of three, finds a P1 in the second.
        let calls = f.base.join("calls.txt");
        let script = format!(
            "prompt=$(cat)\necho call >> '{}'\ncase \"$prompt\" in *\"CHANGE 3:\"*)\n  echo '- [P1] CHANGE 2: Wrong total: shared.txt, the second line'\n  printf 'VERDICT 1: clean\\nVERDICT 2: findings\\nVERDICT 3: clean\\n'\n  exit 0;;\nesac\necho 'VERDICT: clean'\n",
            calls.to_string_lossy().replace('\\', "/")
        );
        std::fs::write(f.repo.join("review.sh"), script).unwrap();
        let text = PLAYBOOK
            .replace(r#"["{sh}", "-c", "cat > /dev/null; echo 'Fine.'; echo 'VERDICT: clean'"]"#, r#"["{sh}", "review.sh"]"#)
            .replace("{sh}", &playbook::posix_sh())
            .replace("{gate}", "\"exit 0\"");
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), text).unwrap();
        sh(&f.repo, &["add", "."]);
        sh(&f.repo, &["commit", "-q", "-m", "a reviewer that reviews batches"]);
        f.db.conn.execute(r#"UPDATE project_review_mode SET overrides = '{"land_batch":true,"pre_review":"batched"}' WHERE project = 'NIT'"#, []).unwrap();
        let called = || std::fs::read_to_string(&calls).unwrap_or_default().lines().count();

        let (a, _) = f.submitted("Totals", &[("a.txt", "a\n")]);
        let (b, _) = f.submitted("Totals again", &[("b.txt", "b\n")]);
        let (c, _) = f.submitted("Labels", &[("c.txt", "c\n")]);
        assert_eq!(called(), 0, "submit reviews nothing");
        for k in [&a, &b, &c] {
            assert_eq!(f.db.reviews(k).unwrap()[0].verdict, crate::review_batch::PENDING);
        }
        // Land refuses a review that has not run, whatever path reaches it.
        let (branch, wt) = land::branch_of(&f.repo, &a).unwrap();
        let refused = land::check_approval(&f.db, &Playbook::load(&f.repo).unwrap(), &a, &wt.unwrap(), "main", &branch).unwrap_err();
        assert!(format!("{refused:#}").contains("waits for its land batch"), "{refused:#}");

        f.queue_and_run(&[&a, &b, &c]);
        assert_eq!(called(), 1, "one reviewer call for the batch");
        assert_eq!((f.entry(&a).state.as_str(), f.entry(&b).state.as_str(), f.entry(&c).state.as_str()), ("landed", "sent_back", "landed"), "{}", f.error(&b));
        let (ra, rb, rc) = (f.db.reviews(&a).unwrap().remove(0), f.db.reviews(&b).unwrap().remove(0), f.db.reviews(&c).unwrap().remove(0));
        assert!(ra.batch_id.is_some() && ra.batch_id == rb.batch_id && rb.batch_id == rc.batch_id, "{ra:?} {rb:?}");
        assert_eq!((ra.verdict.as_str(), rb.verdict.as_str(), rc.verdict.as_str()), ("pass", "changes", "pass"));
        assert_eq!(rb.findings[0].text, "Wrong total: shared.txt, the second line");
        assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "In Progress", "back to its agent");
        assert!(f.error(&b).contains("asked for changes"), "{}", f.error(&b));
        let run = f.runs().into_iter().find(|r| r.landed.contains(&a)).unwrap();
        assert_eq!((run.gates, run.landed.len()), (1, 2), "the others land on one gate");

        // Alone, a change is reviewed at its land, by itself.
        let (d, _) = f.submitted("Lone", &[("d.txt", "d\n")]);
        f.queue_and_run(&[&d]);
        assert_eq!((called(), f.entry(&d).state.as_str()), (2, "landed"), "{}", f.error(&d));
        assert!(f.db.reviews(&d).unwrap()[0].batch_id.is_none());
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
