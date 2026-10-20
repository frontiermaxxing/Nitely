//! A land that waits for other issues to land first (NIT-456). An agent whose change is ready but
//! sits on another issue's change hands its land in with `after` and moves on: a job in a process
//! of its own (`nitely land-job <id>`, as an async submit's) waits until every named issue is Done,
//! then lands the change as `land` does, as that agent. A rebase conflict or a failed gate stops it
//! the way it stops `land`, on the issue, and the agent hears the outcome on its next nitely call.
//!
//! One row per job in `land_job`; the process running it recorded by pid, so a job whose process is
//! gone reads as failed and is told as such. A newer land of the same issue replaces a waiting one.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

use crate::db::{now, Db};
use crate::issues::Actor;

/// A land handed in to wait for other issues, and what became of it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Job {
    pub id: i64,
    pub issue: String,
    /// The issues it waits to see Done.
    pub after: Vec<String>,
    /// Where it lands from, as `land`'s own `dir`.
    pub dir: String,
    pub requested_by: String,
    /// `waiting`, `landing`, `done` (land ran: see `result`), `failed` (a blocker was closed
    /// without landing, the land could not run, or its process ended), or `replaced`.
    pub state: String,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub pid: Option<i64>,
    /// What `land` returned.
    pub result: Option<Value>,
    pub error: Option<String>,
    pub delivered_at: Option<i64>,
}

impl Job {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "replaced")
    }

    /// One line for the agent that handed it in.
    pub fn outcome(&self) -> String {
        let landed = self.result.as_ref().is_some_and(|r| r["landed"] == true);
        let stopped = self.result.as_ref().and_then(|r| r["error"].as_str()).map(str::to_string);
        match self.state.as_str() {
            "done" if landed => format!("{}: landed after {}.", self.issue, self.after.join(", ")),
            "done" => format!("{}: the land after {} stopped: {}", self.issue, self.after.join(", "), stopped.unwrap_or_default()),
            "failed" => format!("{}: the land after {} did not run: {}", self.issue, self.after.join(", "), self.error.clone().unwrap_or_default()),
            "replaced" => format!("{}: a newer land replaced the one waiting for {}.", self.issue, self.after.join(", ")),
            "landing" => format!("{}: {} landed; landing it now.", self.issue, self.after.join(", ")),
            _ => format!("{}: waits for {} to land, then lands by itself.", self.issue, self.after.join(", ")),
        }
    }
}

const COLUMNS: &str = "id, issue_key, after, dir, requested_by, state, queued_at, started_at, ended_at, pid, result, error, delivered_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Job> {
    let after: String = r.get(2)?;
    let result: Option<String> = r.get(10)?;
    Ok(Job {
        id: r.get(0)?,
        issue: r.get(1)?,
        after: serde_json::from_str(&after).unwrap_or_default(),
        dir: r.get(3)?,
        requested_by: r.get(4)?,
        state: r.get(5)?,
        queued_at: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        pid: r.get(9)?,
        result: result.and_then(|t| serde_json::from_str(&t).ok()),
        error: r.get(11)?,
        delivered_at: r.get(12)?,
    })
}

impl Db {
    /// A land job, with an unfinished one whose process is gone marked failed.
    pub fn land_job(&self, id: i64) -> Result<Option<Job>> {
        let job = self.conn.query_row(&format!("SELECT {COLUMNS} FROM land_job WHERE id = ?1"), [id], row).optional()?;
        match job {
            Some(j) => Ok(Some(self.settle_abandoned_land(j)?)),
            None => Ok(None),
        }
    }

    /// Land jobs not finished (their processes alive), oldest first: for the waits list.
    pub fn waiting_land_jobs(&self) -> Result<Vec<Job>> {
        let jobs: Vec<Job> = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM land_job WHERE state IN ('waiting', 'landing') ORDER BY id"))?
            .query_map([], row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for j in jobs {
            let j = self.settle_abandoned_land(j)?;
            if !j.finished() {
                out.push(j);
            }
        }
        Ok(out)
    }

    /// Finished land jobs this requester has not been told about, oldest first.
    pub fn undelivered_land_jobs(&self, requested_by: &str) -> Result<Vec<Job>> {
        let jobs: Vec<Job> = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM land_job WHERE requested_by = ?1 AND delivered_at IS NULL ORDER BY id"))?
            .query_map([requested_by], row)?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::new();
        for j in jobs {
            let j = self.settle_abandoned_land(j)?;
            // A replaced one is told by the land that replaced it.
            if j.finished() && j.state != "replaced" {
                out.push(j);
            }
        }
        Ok(out)
    }

    pub fn mark_land_jobs_delivered(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn.execute("UPDATE land_job SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL", params![id, now()])?;
        }
        Ok(())
    }

    fn settle_abandoned_land(&self, mut j: Job) -> Result<Job> {
        let Some(pid) = j.pid.map(|p| p as u32).filter(|p| *p != std::process::id()) else { return Ok(j) };
        if j.finished() || crate::submit_job::process_runs(pid) {
            return Ok(j);
        }
        let why = format!("{} (the machine restarted, or it was stopped); land again", crate::submit_job::ABANDONED);
        let failed = self
            .conn
            .execute("UPDATE land_job SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1 AND state IN ('waiting', 'landing')", params![j.id, now(), why])?;
        if failed == 0 {
            return self.conn.query_row(&format!("SELECT {COLUMNS} FROM land_job WHERE id = ?1"), [j.id], row).map_err(Into::into);
        }
        j.state = "failed".into();
        j.error = Some(why);
        j.ended_at = Some(now());
        Ok(j)
    }
}

/// The issues a land waits for: those named (as their current keys), else the issue's own open
/// blockers. Empty: nothing to wait for, so it lands now.
pub fn waits_for(db: &Db, key: &str, named: &[String]) -> Result<Vec<String>> {
    if named.is_empty() {
        let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
        return Ok(issue.blocked_by);
    }
    let mut out = Vec::new();
    for k in named {
        let other = db.issue(k)?.with_context(|| format!("no issue {k}"))?;
        if other.key.eq_ignore_ascii_case(key) {
            bail!("{key} cannot wait for itself");
        }
        if !out.contains(&other.key) {
            out.push(other.key);
        }
    }
    Ok(out)
}

/// Records a land that waits for `after`, replacing a waiting one of the same issue, and logs it
/// on the issue's timeline. The caller starts its process (`nitely land-job <id>`).
pub fn enqueue(db: &Db, key: &str, after: &[String], dir: &str, actor: &Actor) -> Result<Job> {
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    if after.is_empty() {
        bail!("name the issues {} lands after", issue.key);
    }
    db.conn.execute(
        "UPDATE land_job SET state = 'replaced', ended_at = ?2 WHERE issue_key = ?1 COLLATE NOCASE AND state = 'waiting'",
        params![issue.key, now()],
    )?;
    db.conn.execute(
        "INSERT INTO land_job (issue_key, after, dir, requested_by, state, queued_at) VALUES (?1, ?2, ?3, ?4, 'waiting', ?5)",
        params![issue.key, serde_json::to_string(after)?, dir, actor.id(), now()],
    )?;
    let id = db.conn.last_insert_rowid();
    db.log_event(actor, Some(&issue.key), None, "status", &format!("Lands by itself once {} {} landed (land job {id})", after.join(", "), if after.len() == 1 { "has" } else { "have" }))?;
    db.land_job(id)?.context("the land job's record went missing")
}

/// Records the process that runs job `id`.
pub fn set_pid(db: &Db, id: i64, pid: u32) -> Result<()> {
    db.conn.execute("UPDATE land_job SET pid = ?2 WHERE id = ?1", params![id, pid])?;
    Ok(())
}

/// How often a waiting job looks at its blockers. `NITELY_LAND_JOB_POLL_MS` sets it, for tests.
fn poll() -> Duration {
    Duration::from_millis(std::env::var("NITELY_LAND_JOB_POLL_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(15_000))
}

/// Where a waiting job stands now.
pub enum Ready {
    /// Every issue it waits for is Done: land.
    Go(Job),
    /// It is over (replaced, failed, or a blocker closed without landing, recorded so).
    Over(Job),
}

/// Waits, in the job's own process, until every issue job `id` waits for is Done. A blocker closed
/// without landing (Canceled, Duplicate) ends the job: what it waited for will not come.
pub fn wait_for_blockers(db: &Db, id: i64) -> Result<Ready> {
    db.conn.execute("UPDATE land_job SET pid = ?2 WHERE id = ?1 AND state IN ('waiting', 'landing')", params![id, std::process::id()])?;
    loop {
        let job = db.land_job(id)?.with_context(|| format!("no land job {id}"))?;
        if job.state != "waiting" {
            return Ok(if job.state == "landing" { Ready::Go(job) } else { Ready::Over(job) });
        }
        let mut open = false;
        for key in &job.after {
            let other = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            match other.state.as_str() {
                "Done" => {}
                "Canceled" | "Duplicate" => {
                    let why = format!("{} was closed as {} without landing; land again without waiting for it", other.key, other.state);
                    fail(db, id, &why)?;
                    return Ok(Ready::Over(db.land_job(id)?.context("the land job's record went missing")?));
                }
                _ => open = true,
            }
        }
        if !open {
            db.conn.execute("UPDATE land_job SET state = 'landing', started_at = ?2 WHERE id = ?1 AND state = 'waiting'", params![id, now()])?;
            continue;
        }
        std::thread::sleep(poll());
    }
}

/// Lands job `id`'s issue as `land` does, as the agent that handed it in, and records the result.
pub fn land_now(db: &mut Db, config: &crate::Config, id: i64) -> Result<Job> {
    let job = db.land_job(id)?.with_context(|| format!("no land job {id}"))?;
    let actor = Actor::Agent(job.requested_by.clone());
    let opts = crate::land::LandOptions { dir: &job.dir, issue_key: &job.issue, push: true, keep: false, break_lock: false };
    match crate::land::land(db, config, &opts, &actor) {
        Ok(landed) => {
            db.conn.execute(
                "UPDATE land_job SET state = 'done', ended_at = ?2, result = ?3 WHERE id = ?1",
                params![id, now(), serde_json::to_string(&landed)?],
            )?;
        }
        Err(e) => fail(db, id, &format!("{e:#}"))?,
    }
    db.land_job(id)?.context("the land job's record went missing")
}

/// Ends job `id` as failed with `why`, said on its issue too (as a status line: no land ran, so
/// nothing stopped there).
pub fn fail(db: &Db, id: i64, why: &str) -> Result<()> {
    db.conn.execute("UPDATE land_job SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1 AND state IN ('waiting', 'landing')", params![id, now(), why])?;
    if let Some(job) = db.land_job(id)? {
        db.log_event(&Actor::Agent(job.requested_by.clone()), Some(&job.issue), None, "status", &format!("The land after {} did not run: {why}", job.after.join(", ")))?;
    }
    Ok(())
}
