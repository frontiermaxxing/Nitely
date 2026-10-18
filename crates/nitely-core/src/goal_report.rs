//! A goal at a glance, and what happened while the person was away (NIT-247, docs/coordinator.md
//! "Watch"): for each active goal the issues at risk and when it will be done at its recent pace,
//! and a report since a time with, per goal, what landed and what is stuck, plus every place the
//! person was needed and what triage answered so they were not. Read from the database only, like
//! Pulse.

use anyhow::Result;
use serde::Serialize;

use crate::db::{now, Db};
use crate::goals::Goal;
use crate::issues::IssueRecord;

/// Review rounds that asked for changes, from which an issue counts as at risk.
pub const MANY_ROUNDS: i64 = 3;

/// How far back the pace of a goal is read.
const PACE_SECS: i64 = 3 * 24 * 3600;

/// An open issue of a goal that may hold it up, and why, in a few words.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Risk {
    pub key: String,
    pub title: String,
    pub why: String,
}

/// When a goal will be done if it keeps its recent pace.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Finish {
    /// Unix seconds.
    pub at: i64,
    /// Issues of the goal landed per day over the last three days.
    pub per_day: f64,
}

/// The open issues of a goal that may hold it up: stuck, waiting on the person, or through many
/// review rounds. One reason each, the most pressing.
pub fn risks(issues: &[IssueRecord]) -> Vec<Risk> {
    issues
        .iter()
        .filter(|i| i.is_open())
        .filter_map(|i| {
            let why = if let Some(stuck) = &i.track.stuck {
                format!("stuck: {stuck}")
            } else if i.needs_human() {
                "waits on you".to_string()
            } else if i.track.asking {
                "asked you something".to_string()
            } else if i.track.rounds >= MANY_ROUNDS {
                format!("{} review rounds", i.track.rounds)
            } else {
                return None;
            };
            Some(Risk { key: i.key.clone(), title: i.title.clone(), why })
        })
        .collect()
}

impl Db {
    /// When goal `id` will be done at the pace its issues landed over the last three days, with
    /// `open` issues left. None when nothing of it landed in that time, or nothing is left.
    pub fn expected_finish(&self, id: i64, open: i64) -> Result<Option<Finish>> {
        if open <= 0 {
            return Ok(None);
        }
        let ts = now();
        let landed: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT e.issue_key) FROM event e JOIN issue i ON i.key = e.issue_key
             WHERE i.goal = ?1 AND e.type = 'landed' AND e.ts >= ?2",
            rusqlite::params![id, ts - PACE_SECS],
            |r| r.get(0),
        )?;
        if landed == 0 {
            return Ok(None);
        }
        let per_day = landed as f64 / (PACE_SECS as f64 / 86400.0);
        let at = ts + (open as f64 / per_day * 86400.0) as i64;
        Ok(Some(Finish { at, per_day }))
    }
}

/// One goal in the report.
#[derive(Debug, Clone, Serialize)]
pub struct GoalReport {
    pub goal: Goal,
    /// Its issues that landed since the report's start, oldest first.
    pub landed: Vec<Landed>,
    /// Its open issues at risk now.
    pub risks: Vec<Risk>,
    pub finish: Option<Finish>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Landed {
    pub key: String,
    pub title: String,
    pub ts: i64,
}

/// A place the person was needed: an agent's question, a change that waited for their approval,
/// or a finding an agent asked them to accept.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Needed {
    pub ts: i64,
    /// asked, approval
    pub kind: &'static str,
    pub issue_key: Option<String>,
    pub summary: String,
    /// Still waiting on them.
    pub open: bool,
}

/// A question an agent asked that triage answered instead of the person (NIT-245).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Triaged {
    pub ts: i64,
    pub issue_key: Option<String>,
    pub question: String,
    pub answer: String,
}

/// What happened since `since`: per active goal, and the rest.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub since: i64,
    pub now: i64,
    pub goals: Vec<GoalReport>,
    /// Issues in no active goal that landed since `since`.
    pub other_landed: Vec<Landed>,
    /// Every place the person was needed since `since`, oldest first.
    pub needed: Vec<Needed>,
    /// What triage answered since `since`, oldest first: places the person was not needed.
    pub triaged: Vec<Triaged>,
}

impl Db {
    /// The report since `since`, for every project or one.
    pub fn goal_report(&self, project: Option<&str>, since: i64) -> Result<Report> {
        let ts = now();
        let landed = self.landed_with_goals_since(since)?;
        let mut in_goal = std::collections::HashSet::new();
        let mut goals = Vec::new();
        for d in self.active_goals(project)? {
            let open = d.goal.progress.open;
            let mine: Vec<Landed> = landed.iter().filter(|(_, g, _)| *g == Some(d.goal.id)).map(|(l, _, _)| l.clone()).collect();
            in_goal.extend(mine.iter().map(|l| l.key.clone()));
            goals.push(GoalReport { risks: risks(&d.issues), finish: self.expected_finish(d.goal.id, open)?, landed: mine, goal: d.goal });
        }
        let other_landed = landed
            .into_iter()
            .filter(|(l, _, p)| !in_goal.contains(&l.key) && project.is_none_or(|k| p.eq_ignore_ascii_case(k)))
            .map(|(l, _, _)| l)
            .collect();
        let (needed, triaged) = (self.needed_since(project, since)?, self.triaged_since(project, since)?);
        Ok(Report { since, now: ts, goals, other_landed, needed, triaged })
    }

    /// Issues landed since `since`, oldest first, with their goal and project.
    fn landed_with_goals_since(&self, since: i64) -> Result<Vec<(Landed, Option<i64>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.key, i.title, MAX(e.ts), i.goal, COALESCE(i.project, '') FROM event e JOIN issue i ON i.key = e.issue_key
             WHERE e.type = 'landed' AND e.ts >= ?1 GROUP BY i.key ORDER BY MAX(e.ts)",
        )?;
        let rows = stmt.query_map([since], |r| Ok((Landed { key: r.get(0)?, title: r.get(1)?, ts: r.get(2)? }, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Questions triage answered since `since`, of one project or all.
    fn triaged_since(&self, project: Option<&str>, since: i64) -> Result<Vec<Triaged>> {
        let mut stmt = self.conn.prepare(
            "SELECT answered_at, issue_key, question, COALESCE(answer, '') FROM triage WHERE state = 'answered' AND answered_at >= ?1 ORDER BY answered_at, id",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(Triaged { ts: r.get(0)?, issue_key: r.get(1)?, question: first_line(&r.get::<_, String>(2)?), answer: first_line(&r.get::<_, String>(3)?) })
        })?;
        let all: Vec<Triaged> = rows.collect::<rusqlite::Result<_>>()?;
        Ok(all.into_iter().filter(|t| of_project(project, &t.issue_key)).collect())
    }

    /// Every place the person was needed since `since`: asks filed (questions, and findings to
    /// accept), changes that went to Needs You for their approval, and approvals filed straight
    /// into Needs You (goal plans, playbook proposals). A finding to accept moves its issue to
    /// Needs You too; that move is the same request as its ask. Accepting it can leave the change
    /// waiting for the person's approval, which `accept` records as its own event. Only an issue's
    /// latest approval can still wait.
    fn needed_since(&self, project: Option<&str>, since: i64) -> Result<Vec<Needed>> {
        let of_project = |key: &Option<String>| of_project(project, key);
        let mut out = Vec::new();
        // Findings to accept, by issue and when: their move to Needs You is the same request.
        let mut findings: Vec<(String, i64)> = Vec::new();
        let mut asks = self.conn.prepare("SELECT asked_at, issue_key, question, finding, answer IS NULL AND closed IS NULL FROM ask WHERE asked_at >= ?1")?;
        for row in asks.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, bool>(4)?)))? {
            let (ts, issue_key, question, finding, open) = row?;
            if finding.is_some() {
                findings.extend(issue_key.clone().map(|k| (k, ts)));
            }
            if of_project(&issue_key) {
                // Asks about several findings of one review share their question: the finding
                // tells them apart.
                let summary = match finding.as_deref().map(serde_json::from_str::<crate::accept::AskedFinding>) {
                    Some(Ok(f)) => format!("accept a finding? [P{}] {}", f.priority, first_line(&f.text)),
                    Some(Err(_)) => format!("accept a finding? {}", first_line(&question)),
                    None => first_line(&question),
                };
                out.push(Needed { ts, kind: "asked", issue_key, summary, open });
            }
        }
        let mut moves = self.conn.prepare(
            "SELECT e.ts, e.issue_key, e.type, COALESCE(e.summary, ''), COALESCE(i.title, ''), i.state = 'Needs You' FROM event e LEFT JOIN issue i ON i.key = e.issue_key
             WHERE e.ts >= ?1
               AND ((e.actor != 'human' AND e.type = 'issue_moved' AND e.summary LIKE '%→ Needs You%')
                 OR (e.actor != 'human' AND e.type = 'issue_created' AND e.summary LIKE 'Filed as Needs You%')
                 OR e.type = ?2
                 OR (e.type = 'note' AND e.actor = 'human' AND e.summary LIKE 'Accepted%' AND (instr(e.summary, ?3) > 0 OR instr(e.summary, ?4) > 0)))
             ORDER BY e.ts, e.id",
        )?;
        let rows = moves.query_map(rusqlite::params![since, crate::accept::AWAITS_APPROVAL, crate::accept::WAITS_FOR_APPROVAL, crate::accept::APPROVE_IN_THE_APP], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<bool>>(5)?))
        })?;
        let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
        // An acceptance that left the change waiting for approval: since NIT-354 its own event;
        // before, only its note says so, and the note stands in where no event came with it.
        let awaits: Vec<(Option<String>, i64)> = rows.iter().filter(|r| r.2 == crate::accept::AWAITS_APPROVAL).map(|r| (r.1.clone(), r.0)).collect();
        let mut approvals: Vec<Needed> = Vec::new();
        for (ts, issue_key, kind, summary, title, waits) in rows {
            if !of_project(&issue_key) || (kind == "note" && awaits.iter().any(|(k, t)| *k == issue_key && (t - ts).abs() <= 5)) {
                continue;
            }
            // An earlier approval of the same issue was settled once a later request came.
            for earlier in approvals.iter_mut().filter(|n| n.issue_key == issue_key) {
                earlier.open = false;
            }
            // request_acceptance moves with no reason, right after its ask; a submit's move says why.
            let bare = kind == "issue_moved" && !summary.contains(": ");
            if bare && findings.iter().any(|(fk, fts)| issue_key.as_ref() == Some(fk) && (fts - ts).abs() <= 10) {
                continue;
            }
            let why = if kind == crate::accept::AWAITS_APPROVAL {
                summary
            } else if kind == "note" {
                if summary.contains(crate::accept::APPROVE_IN_THE_APP) { crate::accept::APPROVE_IN_THE_APP } else { crate::accept::WAITS_FOR_APPROVAL }.to_string()
            } else if kind == "issue_created" {
                first_line(&title)
            } else {
                summary.split_once(": ").map(|(_, w)| w.to_string()).unwrap_or_else(|| "waited for your approval".to_string())
            };
            approvals.push(Needed { ts, kind: "approval", issue_key, summary: why, open: waits.unwrap_or(false) });
        }
        out.extend(approvals);
        out.sort_by(|a, b| (a.ts, &a.issue_key).cmp(&(b.ts, &b.issue_key)));
        Ok(out)
    }
}

/// Whether an issue key belongs to `project` (every key does when there is none).
fn of_project(project: Option<&str>, key: &Option<String>) -> bool {
    match (project, key) {
        (None, _) => true,
        (Some(p), Some(k)) => k.split_once('-').is_some_and(|(kp, _)| kp.eq_ignore_ascii_case(p)),
        (Some(_), None) => false,
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::GoalChange;
    use crate::issues::{Actor, NewIssue};

    fn db() -> Db {
        let dir = crate::paths::unique_temp("nitely-goal-report", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        db
    }

    fn issue(db: &mut Db, title: &str, goal: Option<i64>) -> String {
        let new = NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, goal, ..Default::default() };
        db.create_issue(&new, &Actor::Human).unwrap().key
    }

    fn landed(db: &Db, key: &str, ts: i64) {
        db.conn.execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'claude:x', ?2, 'landed', 'Landed')", rusqlite::params![ts, key]).unwrap();
        db.conn.execute("UPDATE issue SET state = 'Done', state_type = 'completed' WHERE key = ?1", [key]).unwrap();
    }

    #[test]
    fn a_goal_says_what_is_at_risk_when_it_will_finish_and_what_needed_the_person() {
        let mut db = db();
        let change = GoalChange { title: Some("Strangers can install it".into()), outcome: Some("o".into()), ..Default::default() };
        let goal = db.save_goal("NIT", None, &change, &Actor::Human).unwrap().id;
        let keys: Vec<String> = (0..5).map(|n| issue(&mut db, &format!("Part {n}"), Some(goal))).collect();
        let loose = issue(&mut db, "Outside any goal", None);
        let t = now();
        // Two landed in the last three days, one before the report's start; one outside a goal.
        landed(&db, &keys[0], t - 2 * 86400);
        landed(&db, &keys[1], t - 600);
        landed(&db, &loose, t - 300);
        // One waits on the person, one asked something.
        db.move_issue(&keys[2], "Needs You", None, Some("billing needs your approval"), &Actor::Agent("claude:x".into())).unwrap();
        db.create_ask("Which license?", &["MIT".into(), "Apache-2.0".into()], Some(&keys[3]), None, &Actor::Agent("claude:x".into())).unwrap();
        // Triage answered one question; one is still open, one was answered before the report.
        for (state, at) in [("answered", Some(t - 60)), ("open", None), ("answered", Some(t - 7200))] {
            db.conn
                .execute(
                    "INSERT INTO triage (issue_key, question, asked_by, state, answer, asked_at, answered_at) VALUES (?1, 'Is NIT-9 done?', 'claude:x', ?2, 'Yes, it landed.', ?3, ?4)",
                    rusqlite::params![keys[4], state, t - 7300, at],
                )
                .unwrap();
        }

        let r = db.goal_report(Some("NIT"), t - 3600).unwrap();
        assert_eq!(r.goals.len(), 1);
        let g = &r.goals[0];
        assert_eq!(g.landed.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), [keys[1].as_str()], "only since the report's start");
        let whys: Vec<(&str, &str)> = g.risks.iter().map(|x| (x.key.as_str(), x.why.as_str())).collect();
        assert!(whys.contains(&(keys[2].as_str(), "waits on you")), "{whys:?}");
        assert!(whys.iter().any(|(k, w)| *k == keys[3] && w.contains("asked")), "{whys:?}");
        // 2 landed in 3 days: 2/3 a day, 3 left, so about 4.5 days.
        let f = g.finish.clone().unwrap();
        assert!((f.per_day - 2.0 / 3.0).abs() < 1e-9);
        assert!((f.at - t - (4.5 * 86400.0) as i64).abs() < 120, "{}", f.at - t);
        assert_eq!(r.other_landed.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), [loose.as_str()]);
        let needed: Vec<(&str, bool)> = r.needed.iter().map(|n| (n.kind, n.open)).collect();
        assert_eq!(needed, [("approval", true), ("asked", true)], "{:?}", r.needed);
        assert!(r.needed[0].summary.contains("billing needs your approval"), "{:?}", r.needed[0]);
        assert_eq!(r.triaged.iter().map(|q| (q.issue_key.as_deref(), q.answer.as_str())).collect::<Vec<_>>(), [(Some(keys[4].as_str()), "Yes, it landed.")]);
        db.create_project("APP", "Acme").unwrap();
        assert!(db.goal_report(Some("APP"), t - 3600).unwrap().triaged.is_empty(), "another project's");

        // A goal with nothing landed lately has no finish to promise.
        assert!(db.expected_finish(goal, 3).unwrap().is_some());
        let other = db.save_goal("NIT", None, &GoalChange { title: Some("Quiet".into()), outcome: Some("o".into()), ..Default::default() }, &Actor::Human).unwrap().id;
        assert_eq!(db.expected_finish(other, 2).unwrap(), None);
        assert_eq!(db.expected_finish(goal, 0).unwrap(), None, "nothing left");
    }

    #[test]
    fn each_request_of_the_person_counts_once_and_only_the_latest_still_waits() {
        let mut db = db();
        let agent = Actor::Agent("claude:x".into());
        // Sent to the person twice: the first approval was settled when the second came.
        let twice = issue(&mut db, "Billing", None);
        db.move_issue(&twice, "Needs You", None, Some("billing needs your approval"), &agent).unwrap();
        db.move_issue(&twice, "In Progress", None, None, &agent).unwrap();
        db.move_issue(&twice, "Needs You", None, Some("billing again"), &agent).unwrap();
        // A finding to accept: its ask and its move to Needs You are one request.
        let finding = issue(&mut db, "Settings", None);
        let ask = db.create_ask("No migration is needed: nobody uses it yet.", &[], Some(&finding), None, &agent).unwrap();
        db.conn.execute("UPDATE ask SET finding = '{\"priority\":1,\"text\":\"No migration\"}' WHERE id = ?1", [ask.id]).unwrap();
        db.move_issue(&finding, "Needs You", None, None, &agent).unwrap();
        // Waited for approval, went back, then asked to accept a finding, which the person
        // accepted: the change still waits for their approval, and the first approval is settled.
        let accepted = issue(&mut db, "Webhooks", None);
        db.move_issue(&accepted, "Needs You", None, Some("release process"), &agent).unwrap();
        db.move_issue(&accepted, "In Progress", None, None, &agent).unwrap();
        let ask = db.create_ask("Retries are out of scope here.", &[], Some(&accepted), None, &agent).unwrap();
        db.conn.execute("UPDATE ask SET finding = '{\"priority\":1,\"text\":\"No retries\"}', answer = 'Accepted: fine', answered_at = ?2 WHERE id = ?1", rusqlite::params![ask.id, now()]).unwrap();
        db.move_issue(&accepted, "Needs You", None, None, &agent).unwrap();
        let waits = crate::accept::WAITS_FOR_APPROVAL;
        let note = format!("Accepted [P1] No retries: fine. Then {waits}.");
        db.add_note(&accepted, &note, "note", &Actor::Human).unwrap();
        db.add_note(&accepted, waits, crate::accept::AWAITS_APPROVAL, &Actor::Human).unwrap();
        // Accepted before the event existed: its note alone says the change waits.
        let old = issue(&mut db, "Receipts", None);
        let ask = db.create_ask("Receipts keep the old layout.", &[], Some(&old), None, &agent).unwrap();
        db.conn.execute("UPDATE ask SET finding = '{\"priority\":1,\"text\":\"Layout\"}', answer = 'Accepted: fine', answered_at = ?2 WHERE id = ?1", rusqlite::params![ask.id, now()]).unwrap();
        db.move_issue(&old, "Needs You", None, None, &agent).unwrap();
        db.add_note(&old, &format!("Accepted [P1] Layout: fine. Then {}.", crate::accept::APPROVE_IN_THE_APP), "note", &Actor::Human).unwrap();
        // The same, then sent back and resubmitted (NIT-354): the approval after the acceptance
        // stays, settled, and the resubmitted one waits.
        let resent = issue(&mut db, "Invoices", None);
        let ask = db.create_ask("Rounding is per line on purpose.", &[], Some(&resent), None, &agent).unwrap();
        db.conn.execute("UPDATE ask SET finding = '{\"priority\":1,\"text\":\"Rounding\"}', answer = 'Accepted: on purpose', answered_at = ?2 WHERE id = ?1", rusqlite::params![ask.id, now()]).unwrap();
        db.move_issue(&resent, "Needs You", None, None, &agent).unwrap();
        db.add_note(&resent, waits, crate::accept::AWAITS_APPROVAL, &Actor::Human).unwrap();
        db.move_issue(&resent, "In Progress", None, Some("the totals are off"), &Actor::Human).unwrap();
        db.move_issue(&resent, "Needs You", None, Some("approve the corrected totals"), &agent).unwrap();
        // A finding the person declined, fixed, and back for approval: one approval, the later one.
        let declined = issue(&mut db, "Exports", None);
        let ask = db.create_ask("The old format is gone.", &[], Some(&declined), None, &agent).unwrap();
        db.conn.execute("UPDATE ask SET finding = '{\"priority\":1,\"text\":\"Old format\"}', answer = 'decline', answered_at = ?2 WHERE id = ?1", rusqlite::params![ask.id, now()]).unwrap();
        db.move_issue(&declined, "Needs You", None, None, &agent).unwrap();
        db.move_issue(&declined, "In Progress", None, None, &agent).unwrap();
        db.move_issue(&declined, "Needs You", None, Some("approve the fix"), &agent).unwrap();
        // Filed straight into Needs You, as a goal plan or a playbook proposal is.
        let new = NewIssue {
            project: "NIT".into(),
            title: "Plan: ship it".into(),
            kind: "feature".into(),
            body: "b".into(),
            evidence: "e".into(),
            source: "claude".into(),
            allow_similar: true,
            state: Some("Needs You".into()),
            owner: Some("human".into()),
            ..Default::default()
        };
        let plan = db.create_issue(&new, &agent).unwrap().key;

        let r = db.goal_report(None, now() - 60).unwrap();
        let mut needed: Vec<(&str, &str, bool)> = r.needed.iter().map(|n| (n.kind, n.issue_key.as_deref().unwrap(), n.open)).collect();
        needed.sort();
        let mut want = vec![
            ("approval", twice.as_str(), false),
            ("approval", twice.as_str(), true),
            ("asked", finding.as_str(), true),
            ("approval", accepted.as_str(), false),
            ("asked", accepted.as_str(), false),
            ("approval", accepted.as_str(), true),
            ("asked", declined.as_str(), false),
            ("approval", declined.as_str(), true),
            ("asked", old.as_str(), false),
            ("approval", old.as_str(), true),
            ("asked", resent.as_str(), false),
            ("approval", resent.as_str(), false),
            ("approval", resent.as_str(), true),
            ("approval", plan.as_str(), true),
        ];
        want.sort();
        assert_eq!(needed, want, "{:?}", r.needed);
        assert!(r.needed.iter().any(|n| n.issue_key.as_deref() == Some(plan.as_str()) && n.summary == "Plan: ship it"), "{:?}", r.needed);
        let resent_why: Vec<&str> = r.needed.iter().filter(|n| n.issue_key.as_deref() == Some(resent.as_str()) && n.kind == "approval").map(|n| n.summary.as_str()).collect();
        assert_eq!(resent_why, [waits, "approve the corrected totals"]);
        let asked: Vec<&str> = r.needed.iter().filter(|n| n.issue_key.as_deref() == Some(finding.as_str()) && n.kind == "asked").map(|n| n.summary.as_str()).collect();
        assert_eq!(asked, ["accept a finding? [P1] No migration"], "the finding, not the shared question");
    }
}
