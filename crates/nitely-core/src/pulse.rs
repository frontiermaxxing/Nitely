//! The Pulse view's data in one read: every project's open issues on the lifecycle track with
//! their agent, latest review, gate and pull request; what needs the human; what happened since a
//! given time; and landed and submitted per hour over the last day. Only the database is read, so
//! it is cheap to refresh; git state (a stopped rebase) comes from the snapshot the app already
//! holds.

use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;

use crate::db::{now, Db, PrRecord, Session};
use crate::git;
use crate::issues::{iso, Ask, IssueRecord};
use crate::paths::is_within;
use crate::submit::ReviewRecord;

/// An issue at one of the agent's stations with no event and no session activity for this long
/// counts as quiet, which Pulse shows with the stuck ones.
pub const QUIET_SECS: i64 = 3600;
/// "Since you were last here" never reaches further back than this.
const SINCE_LIMIT_SECS: i64 = 7 * 24 * 3600;
const HAPPENED_LIMIT: i64 = 300;
const HOURS: i64 = 24;

#[derive(Debug, Clone, Serialize)]
pub struct Pulse {
    pub now: i64,
    /// The start of "since you were last here", after the 7-day limit.
    pub since: i64,
    pub projects: Vec<ProjectFlow>,
    /// Agents' questions waiting for an answer.
    pub asks: Vec<Ask>,
    /// Issues waiting on the human's approval.
    pub approvals: Vec<IssueRecord>,
    /// What happened since `since`, newest first.
    pub happened: Vec<Happening>,
    /// Landed and submitted per hour, oldest first, the last ending with the current hour.
    pub throughput: Vec<Hour>,
    pub landed_today: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectFlow {
    pub key: String,
    pub name: String,
    /// Open issues, most urgent first.
    pub issues: Vec<FlowIssue>,
    /// Issues that reached Done since the start of the day.
    pub done_today: Vec<FlowIssue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowIssue {
    /// Carries its `track`.
    pub issue: IssueRecord,
    /// Who works on it: the claim, else the session in its worktree, else who started it.
    pub agent: Option<String>,
    /// The latest agent session inside its worktree.
    pub session: Option<Session>,
    pub pr: Option<PrRecord>,
    pub review: Option<ReviewRecord>,
    pub gate: Option<GateRun>,
    pub worktree: Option<String>,
    /// The latest event on the issue or sign of life from its session.
    pub last_activity: i64,
    /// At an agent's station, not stuck, and nothing has happened for `QUIET_SECS`.
    pub quiet: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateRun {
    pub ts: i64,
    pub passed: bool,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Happening {
    pub ts: i64,
    /// landed, filed, stuck, review or asked.
    pub kind: &'static str,
    pub issue_key: Option<String>,
    pub actor: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hour {
    pub start: i64,
    pub landed: i64,
    pub submitted: i64,
}

/// Per open issue: its latest gate run, worktree, first agent and latest event.
#[derive(Default)]
struct Facts {
    gate: Option<GateRun>,
    worktree: Option<String>,
    starter: Option<String>,
    last: i64,
}

/// `since` is when the human last looked; `day_start` is the start of their local day.
pub fn pulse(db: &Db, since: i64, day_start: i64) -> Result<Pulse> {
    let now = now();
    let since = since.max(now - SINCE_LIMIT_SECS);
    let issues = db.list_issues(None, true)?;
    let facts = db.open_issue_facts()?;
    let sessions = db.sessions_since(now - 24 * 3600)?;
    let mut prs_by_key: HashMap<String, Vec<PrRecord>> = HashMap::new();
    for pr in db.prs()? {
        if let (_, Some(key)) = git::parse_branch(&pr.branch) {
            prs_by_key.entry(key).or_default().push(pr);
        }
    }
    let today = iso(day_start);

    let flow = |issue: &IssueRecord| -> Result<FlowIssue> {
        let key = issue.key.to_uppercase();
        let f = facts.get(&key);
        let worktree = f.and_then(|f| f.worktree.clone());
        // Newest first, so the first match is the latest.
        let session = worktree.as_deref().and_then(|w| sessions.iter().find(|s| is_within(&s.cwd, w)).cloned());
        let pr = prs_by_key.get(&key).and_then(|prs| prs.iter().find(|p| p.state == "OPEN").or(prs.first()).cloned());
        let last_activity = f.map_or(0, |f| f.last).max(session.as_ref().map_or(0, |s| s.last_seen_at));
        let at_agent = matches!(issue.track.station, "started" | "working" | "gate" | "review" | "landing");
        let quiet = issue.is_open() && at_agent && issue.track.stuck.is_none() && last_activity > 0 && now - last_activity > QUIET_SECS;
        Ok(FlowIssue {
            agent: issue
                .claimed_by
                .clone()
                .or_else(|| session.as_ref().map(|s| s.vendor.clone()))
                .or_else(|| f.and_then(|f| f.starter.clone())),
            session,
            pr,
            review: db.reviews(&issue.key)?.into_iter().next(),
            gate: f.and_then(|f| f.gate.clone()),
            worktree,
            last_activity,
            quiet,
            issue: issue.clone(),
        })
    };

    let mut projects = Vec::new();
    for p in db.projects()? {
        let mine = issues.iter().filter(|i| i.origin == "nitely" && i.project.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&p.key)));
        let mut open = Vec::new();
        let mut done_today = Vec::new();
        for i in mine {
            if i.is_open() {
                open.push(flow(i)?);
            } else if i.state == "Done" && i.updated_at >= today {
                done_today.push(flow(i)?);
            }
        }
        projects.push(ProjectFlow { key: p.key, name: p.name, issues: open, done_today });
    }

    let approvals = issues.iter().filter(|i| i.is_open() && i.needs_human()).cloned().collect();
    let end = (now / 3600 + 1) * 3600;
    let start = end - HOURS * 3600;
    let mut throughput: Vec<Hour> = (0..HOURS).map(|h| Hour { start: start + h * 3600, landed: 0, submitted: 0 }).collect();
    for (ts, kind) in db.events_of(&["landed", "submit"], start)? {
        let h = &mut throughput[((ts - start) / 3600).clamp(0, HOURS - 1) as usize];
        if kind == "landed" { h.landed += 1 } else { h.submitted += 1 }
    }
    let landed_today = db.events_of(&["landed"], day_start)?.len() as i64;

    Ok(Pulse {
        now,
        since,
        projects,
        asks: db.open_asks()?,
        approvals,
        happened: db.happened_since(since)?,
        throughput,
        landed_today,
    })
}

impl Db {
    /// One pass over the events of open issues.
    fn open_issue_facts(&self) -> Result<HashMap<String, Facts>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.issue_key, e.ts, e.actor, e.type, e.summary, e.cwd FROM event e
             JOIN issue i ON i.key = e.issue_key COLLATE NOCASE
             WHERE i.state_type NOT IN ('completed', 'canceled')
             ORDER BY e.ts, e.id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut facts: HashMap<String, Facts> = HashMap::new();
        for row in rows {
            let (key, ts, actor, kind, summary, cwd) = row?;
            let f = facts.entry(key.to_uppercase()).or_default();
            f.last = f.last.max(ts);
            match kind.as_str() {
                "gate_passed" | "gate_failed" => f.gate = Some(GateRun { ts, passed: kind == "gate_passed", summary }),
                "worktree" => {
                    f.worktree = cwd;
                    f.starter.get_or_insert(actor);
                }
                "claimed" if actor != "human" => {
                    f.starter.get_or_insert(actor);
                }
                _ => {}
            }
        }
        Ok(facts)
    }

    /// (ts, type) of the events of these types since `since`.
    fn events_of(&self, kinds: &[&str], since: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT ts, type FROM event WHERE ts >= ?1 ORDER BY ts")?;
        let rows = stmt.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (ts, kind) = row?;
            if kinds.contains(&kind.as_str()) {
                out.push((ts, kind));
            }
        }
        Ok(out)
    }

    /// Landings, new issues, failures, review rounds and questions since `since`, newest first.
    fn happened_since(&self, since: i64) -> Result<Vec<Happening>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, type, issue_key, actor, summary FROM event
             WHERE ts >= ?1 AND type IN ('landed', 'issue_created', 'gate_failed', 'land_failed', 'review_failed', 'submit', 'ask')
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![since, HAPPENED_LIMIT], |r| {
            let kind: String = r.get(1)?;
            Ok(Happening {
                ts: r.get(0)?,
                kind: match kind.as_str() {
                    "landed" => "landed",
                    "issue_created" => "filed",
                    "submit" => "review",
                    "ask" => "asked",
                    _ => "stuck",
                },
                issue_key: r.get(2)?,
                actor: r.get(3)?,
                summary: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::{Actor, NewIssue};

    fn db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "nitely-pulse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        (db, dir)
    }

    fn file(db: &mut Db, title: &str) -> IssueRecord {
        let new = NewIssue {
            project: "NIT".into(),
            title: title.into(),
            body: "So Andrew sees the flow.".into(),
            kind: "feature".into(),
            source: "claude".into(),
            evidence: "none yet".into(),
            priority: 2,
            ..Default::default()
        };
        db.create_issue(&new, &Actor::Agent("claude:c:/x".into())).unwrap()
    }

    #[test]
    fn flows_carry_tracks_and_happenings() {
        let (mut db, _dir) = db();
        let a = file(&mut db, "Pulse home screen");
        let b = file(&mut db, "Board and branch map");
        let agent = Actor::Agent("claude:c:/wt/nit-1".into());
        db.next_task("NIT", "claude:c:/wt/nit-1").unwrap();
        db.log_event(&agent, Some(&a.key), Some("C:/wt/nit-1"), "worktree", "Started claude/nit-1 from main").unwrap();
        db.log_event(&agent, Some(&a.key), Some("C:/wt/nit-1"), "gate_failed", "Gate failed at `npm run build`").unwrap();

        let p = pulse(&db, 0, 0).unwrap();
        let flow = &p.projects[0];
        assert_eq!(flow.issues.len(), 2);
        let pulse_a = flow.issues.iter().find(|f| f.issue.key == a.key).unwrap();
        assert_eq!(pulse_a.issue.track.station, "gate");
        assert_eq!(pulse_a.issue.track.stuck.as_deref(), Some("Gate failed"));
        assert_eq!(pulse_a.gate.as_ref().map(|g| g.passed), Some(false));
        assert_eq!(pulse_a.worktree.as_deref(), Some("C:/wt/nit-1"));
        assert!(pulse_a.agent.as_deref().is_some_and(|a| a.starts_with("claude")));
        assert!(!pulse_a.quiet);
        let pulse_b = flow.issues.iter().find(|f| f.issue.key == b.key).unwrap();
        assert_eq!(pulse_b.issue.track.station, "todo");

        let kinds: Vec<&str> = p.happened.iter().map(|h| h.kind).collect();
        assert!(kinds.contains(&"filed") && kinds.contains(&"stuck"));
        assert_eq!(p.throughput.len(), HOURS as usize);
        assert_eq!(p.landed_today, 0);
    }

    #[test]
    fn rows_and_detail_get_the_same_track() {
        let (mut db, _dir) = db();
        let a = file(&mut db, "Pulse home screen");
        let agent = Actor::Agent("claude:c:/wt/nit-1".into());
        db.next_task("NIT", "claude:c:/wt/nit-1").unwrap();
        db.log_event(&agent, Some(&a.key), None, "gate_failed", "Gate failed").unwrap();
        let listed = db.list_issues(Some("NIT"), false).unwrap();
        let single = db.issue(&a.key).unwrap().unwrap();
        assert_eq!(listed[0].track, single.track);
        assert_eq!(single.track.stuck.as_deref(), Some("Gate failed"));
    }
}
