//! Goals: an outcome a project works toward ("strangers can install Nitely"), with the issues that
//! get it there. An issue belongs to at most one goal; blockers still link issues across goals. A
//! goal is active until someone marks it done or dropped, and its progress is its issues' states.

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

pub const GOAL_STATES: &[&str] = &["active", "done", "dropped"];

#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    pub id: i64,
    pub project: String,
    pub title: String,
    /// One paragraph: what is true once the goal is reached.
    pub outcome: String,
    /// `YYYY-MM-DD`, or none.
    pub target_date: Option<String>,
    /// active, done or dropped.
    pub state: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub progress: Progress,
    /// Where the person put it among the project's active goals: lower first. None until they do;
    /// such goals come after ranked ones, nearest target date first, then oldest.
    pub rank: Option<i64>,
    /// How many crew workers may work on it at once; None leaves it to the playbook's
    /// `[crew] goal_workers`.
    pub workers: Option<i64>,
    /// Crew workers on its issues now.
    pub working: i64,
    /// Its issues that landed since local midnight.
    pub landed_today: i64,
    /// Its goal branch, when its items land into one (goal_branch.rs): green or red, the items
    /// since the last checkpoint and when the next is due.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<crate::goal_branch::Summary>,
}

/// How a goal's issues stand: every issue counts once.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub total: i64,
    /// Done.
    pub done: i64,
    /// Canceled or duplicate: no longer part of the work.
    pub dropped: i64,
    /// Everything else.
    pub open: i64,
}

/// A goal with its issues, open and closed.
#[derive(Debug, Clone, Serialize)]
pub struct GoalDetail {
    pub goal: Goal,
    pub issues: Vec<IssueRecord>,
    /// Its open issues that may hold it up (NIT-247): what the goal rows draw.
    pub risks: Vec<crate::goal_report::Risk>,
    /// When it will be done at its recent pace.
    pub finish: Option<crate::goal_report::Finish>,
}

/// What `save_goal` changes; `None` leaves a field as it is. A new goal needs a title and outcome.
#[derive(Debug, Clone, Default)]
pub struct GoalChange {
    pub title: Option<String>,
    pub outcome: Option<String>,
    /// `Some("")` clears the date.
    pub target_date: Option<String>,
    pub state: Option<String>,
    /// `Some(0)` clears the rank.
    pub rank: Option<i64>,
    /// `Some(0)` clears the goal's own worker budget, leaving it to the playbook.
    pub workers: Option<i64>,
}

const COLUMNS: &str = "g.id, g.project, g.title, g.outcome, g.target_date, g.state, g.created_by, g.created_at, g.updated_at,
    (SELECT COUNT(*) FROM issue i WHERE i.goal = g.id),
    (SELECT COUNT(*) FROM issue i WHERE i.goal = g.id AND i.state_type = 'completed'),
    (SELECT COUNT(*) FROM issue i WHERE i.goal = g.id AND i.state_type = 'canceled'),
    g.rank, g.workers,
    (SELECT COUNT(*) FROM crew_worker w JOIN issue i ON i.key = w.issue_key WHERE i.goal = g.id AND w.ended_at IS NULL),
    (SELECT COUNT(DISTINCT e.issue_key) FROM event e JOIN issue i ON i.key = e.issue_key
      WHERE i.goal = g.id AND e.type = 'landed' AND e.ts >= CAST(strftime('%s', 'now', 'localtime', 'start of day', 'utc') AS INTEGER))";

/// How a project's active goals are ordered: the person's rank, then the nearest target date,
/// then the oldest. What Crew works first (NIT-243) and the order the goal rows show.
const ORDER: &str = "g.state != 'active', g.rank IS NULL, g.rank, g.target_date IS NULL, g.target_date, g.id";

fn goal_from_row(r: &rusqlite::Row) -> rusqlite::Result<Goal> {
    let (total, done, dropped): (i64, i64, i64) = (r.get(9)?, r.get(10)?, r.get(11)?);
    Ok(Goal {
        id: r.get(0)?,
        project: r.get(1)?,
        title: r.get(2)?,
        outcome: r.get(3)?,
        target_date: r.get(4)?,
        state: r.get(5)?,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        progress: Progress { total, done, dropped, open: total - done - dropped },
        rank: r.get(12)?,
        workers: r.get(13)?,
        working: r.get(14)?,
        landed_today: r.get(15)?,
        branch: None,
    })
}

/// `YYYY-MM-DD` with a real month and day, or nothing.
pub(crate) fn check_date(date: &str) -> Result<Option<String>> {
    let date = date.trim();
    if date.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = date.split('-').collect();
    let digits = parts.len() == 3 && [4, 2, 2].iter().zip(&parts).all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_digit()));
    let (year, month, day): (u32, u32, u32) = if digits { (parts[0].parse()?, parts[1].parse()?, parts[2].parse()?) } else { (0, 0, 0) };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    if !digits || !(1..=days).contains(&day) {
        bail!("a target date is YYYY-MM-DD, e.g. 2026-12-01");
    }
    Ok(Some(date.to_string()))
}

impl Db {
    /// A project's goals, active ones first and nearer target dates first; closed ones only when
    /// asked for.
    pub fn goals(&self, project: &str, include_closed: bool) -> Result<Vec<Goal>> {
        let p = self.project(project)?.with_context(|| format!("no project {project}"))?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM goal g WHERE g.project = ?1 AND (?2 OR g.state = 'active') ORDER BY {ORDER}"
        ))?;
        let rows = stmt.query_map(params![p.key, include_closed], goal_from_row)?;
        let mut goals: Vec<Goal> = rows.collect::<rusqlite::Result<_>>()?;
        for g in &mut goals {
            g.branch = self.goal_branch_summary(g.id)?;
        }
        Ok(goals)
    }

    pub fn goal(&self, id: i64) -> Result<Option<Goal>> {
        let goal = self.conn.query_row(&format!("SELECT {COLUMNS} FROM goal g WHERE g.id = ?1"), [id], goal_from_row).optional()?;
        goal.map(|mut g| {
            g.branch = self.goal_branch_summary(g.id)?;
            Ok(g)
        })
        .transpose()
    }

    /// A goal and every issue in it, the open ones in the order `list_issues` gives.
    pub fn goal_detail(&self, id: i64) -> Result<GoalDetail> {
        let goal = self.goal(id)?.with_context(|| format!("no goal {id}"))?;
        let issues: Vec<IssueRecord> =
            self.list_issues(Some(&goal.project), true)?.into_iter().filter(|i| i.goal == Some(id)).collect();
        let (risks, finish) = (crate::goal_report::risks(&issues), self.expected_finish(goal.id, goal.progress.open)?);
        Ok(GoalDetail { goal, issues, risks, finish })
    }

    /// Every active goal with its issues, of one project or of all of them (projects in key order,
    /// goals as `goals` orders them): what the goal rows draw.
    pub fn active_goals(&self, project: Option<&str>) -> Result<Vec<GoalDetail>> {
        self.goal_rows(project, false)
    }

    /// `active_goals`, and with `include_closed` the completed and dropped goals too ("Show
    /// completed", NIT-611).
    pub fn goal_rows(&self, project: Option<&str>, include_closed: bool) -> Result<Vec<GoalDetail>> {
        let projects: Vec<String> = match project {
            Some(p) => vec![self.project(p)?.with_context(|| format!("no project {p}"))?.key],
            None => self.projects()?.into_iter().map(|p| p.key).collect(),
        };
        let mut out = Vec::new();
        for key in projects {
            let goals = self.goals(&key, include_closed)?;
            if goals.is_empty() {
                continue;
            }
            let issues = self.list_issues(Some(&key), true)?;
            for goal in goals {
                let mine: Vec<IssueRecord> = issues.iter().filter(|i| i.goal == Some(goal.id)).cloned().collect();
                let (risks, finish) = (crate::goal_report::risks(&mine), self.expected_finish(goal.id, goal.progress.open)?);
                out.push(GoalDetail { goal, issues: mine, risks, finish });
            }
        }
        Ok(out)
    }

    /// Creates a goal (`id` None) or changes one.
    pub fn save_goal(&self, project: &str, id: Option<i64>, change: &GoalChange, actor: &Actor) -> Result<Goal> {
        let clean = |s: &Option<String>| s.as_deref().map(str::trim).map(str::to_string);
        let (title, outcome) = (clean(&change.title), clean(&change.outcome));
        if title.as_deref() == Some("") || outcome.as_deref() == Some("") {
            bail!("a goal's title and outcome cannot be empty");
        }
        let target = change.target_date.as_deref().map(check_date).transpose()?;
        if (change.rank.is_some() || change.workers.is_some()) && *actor != Actor::Human {
            bail!("only the person orders goals and sets how many workers each may have, in the app: that decides what the crew works on");
        }
        if change.rank.is_some_and(|r| r < 0) || change.workers.is_some_and(|w| w < 0) {
            bail!("a goal's rank and worker budget are whole numbers, 0 to clear");
        }
        // 0 clears: (given, value).
        let (rank_given, rank) = (change.rank.is_some(), change.rank.filter(|r| *r > 0));
        let (workers_given, workers) = (change.workers.is_some(), change.workers.filter(|w| *w > 0));
        if let Some(state) = change.state.as_deref() {
            if !GOAL_STATES.contains(&state) {
                bail!("a goal's state is one of {}", GOAL_STATES.join(", "));
            }
        }
        let ts = now();
        let id = match id {
            None => {
                let p = self.project(project)?.with_context(|| format!("no project {project}"))?;
                let (Some(title), Some(outcome)) = (title, outcome) else {
                    bail!("a new goal needs a title and an outcome: one paragraph on what is true once it is reached");
                };
                self.conn.execute(
                    "INSERT INTO goal (project, title, outcome, target_date, state, created_by, created_at, updated_at, rank, workers)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
                    params![p.key, title, outcome, target.flatten(), change.state.as_deref().unwrap_or("active"), actor.id(), ts, rank, workers],
                )?;
                self.conn.last_insert_rowid()
            }
            Some(id) => {
                let before: Option<String> = self.conn.query_row("SELECT state FROM goal WHERE id = ?1", [id], |r| r.get(0)).optional()?;
                // Only the fields given change, in one statement: two agents editing different
                // fields at once both keep their edit.
                let changed = self.conn.execute(
                    "UPDATE goal SET title = COALESCE(?2, title), outcome = COALESCE(?3, outcome),
                       target_date = CASE WHEN ?4 THEN ?5 ELSE target_date END, state = COALESCE(?6, state), updated_at = ?7,
                       rank = CASE WHEN ?8 THEN ?9 ELSE rank END, workers = CASE WHEN ?10 THEN ?11 ELSE workers END
                     WHERE id = ?1",
                    params![id, title, outcome, target.is_some(), target.flatten(), change.state, ts, rank_given, rank, workers_given, workers],
                )?;
                if changed == 0 {
                    bail!("no goal {id}");
                }
                // A goal done or dropped is news to whoever works toward it (the updates feed,
                // NIT-562): one event, with its project, since it names no issue.
                if let Some(state) = change.state.as_deref().filter(|s| before.as_deref() != Some(*s)) {
                    let goal = self.goal(id)?.context("goal vanished")?;
                    self.conn.execute(
                        "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, NULL, 'goal_state', ?3, ?4)",
                        params![ts, actor.id(), format!("Goal {id} \"{}\" is {state}", goal.title), serde_json::json!({ "project": goal.project, "goal": id }).to_string()],
                    )?;
                }
                id
            }
        };
        self.goal(id)?.context("goal vanished")
    }

    /// The person's order of a project's active goals: `ids` first to last get ranks 1, 2, ...; the
    /// project's other active goals keep theirs after them, in their order. Only the person orders
    /// goals, since Crew works them in this order (NIT-243).
    pub fn order_goals(&self, project: &str, ids: &[i64], actor: &Actor) -> Result<Vec<Goal>> {
        if *actor != Actor::Human {
            bail!("only the person orders goals, in the app: that decides what the crew works on");
        }
        let goals = self.goals(project, false)?;
        for id in ids {
            if !goals.iter().any(|g| g.id == *id) {
                bail!("goal {id} is not an active goal of {project}");
            }
        }
        let rest = goals.iter().map(|g| g.id).filter(|g| !ids.contains(g));
        let ts = now();
        for (n, id) in ids.iter().copied().chain(rest).enumerate() {
            self.conn.execute("UPDATE goal SET rank = ?2, updated_at = ?3 WHERE id = ?1", params![id, n as i64 + 1, ts])?;
        }
        self.goals(project, false)
    }

    /// Refuses what `set_issue_goal` would refuse, without changing anything.
    pub fn check_issue_goal(&self, key: &str, goal: Option<i64>) -> Result<IssueRecord> {
        let issue = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
        if issue.origin != "nitely" {
            bail!("{} is mirrored from {}; only native issues have goals", issue.key, issue.origin);
        }
        if let Some(id) = goal.filter(|g| issue.goal != Some(*g)) {
            self.goal_for(issue.project.as_deref(), id)?;
        }
        Ok(issue)
    }

    /// Puts an issue in a goal of its project, or (`None`) takes it out of the one it is in. Only
    /// active goals take new issues. Logged on the issue's timeline.
    pub fn set_issue_goal(&self, key: &str, goal: Option<i64>, actor: &Actor) -> Result<IssueRecord> {
        let issue = self.check_issue_goal(key, goal)?;
        if issue.goal == goal {
            return Ok(issue);
        }
        let summary = match goal {
            Some(id) => format!("Goal: {}", self.goal_for(issue.project.as_deref(), id)?.title),
            None => match issue.goal.and_then(|g| self.goal(g).ok().flatten()) {
                Some(old) => format!("Left goal: {}", old.title),
                None => "Left its goal".to_string(),
            },
        };
        let ts = now();
        self.conn.execute("UPDATE issue SET goal = ?2, updated_at = ?3 WHERE key = ?1", params![issue.key, goal, crate::issues::iso(ts)])?;
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, ?2, ?3, 'goal_changed', ?4)",
            params![ts, actor.id(), issue.key, summary],
        )?;
        // The goal it left may be finished now: its last open issue went elsewhere (NIT-614).
        if let Some(left) = issue.goal {
            self.complete_goal_if_finished(left, actor)?;
        }
        self.issue(&issue.key)?.context("issue vanished")
    }

    /// The goal `id`, if an issue of `project` may join it: same project, and active.
    pub(crate) fn goal_for(&self, project: Option<&str>, id: i64) -> Result<Goal> {
        let goal = self.goal(id)?.with_context(|| format!("no goal {id}; list them with the goals tool"))?;
        if project.is_some_and(|p| !p.eq_ignore_ascii_case(&goal.project)) {
            bail!("goal {id} belongs to {}, not {}", goal.project, project.unwrap_or_default());
        }
        if goal.state != "active" {
            bail!("goal {id} ({}) is {}; reopen it (state active) before adding issues", goal.title, goal.state);
        }
        Ok(goal)
    }
}

/// Whether a goal is finished (NIT-611): it has issues, none of them is open (each is Done,
/// Canceled or Duplicate), and its goal branch holds no item still waiting for a checkpoint.
pub fn goal_finished(issues: usize, open: usize, waiting_items: usize) -> bool {
    issues > 0 && open == 0 && waiting_items == 0
}

/// The project's focus once goal `completed` is finished: cleared when it was that goal (agents
/// then work by priority), else as it was.
pub fn focus_after_completion(focused: Option<i64>, completed: i64) -> Option<i64> {
    focused.filter(|g| *g != completed)
}

/// The event kinds that tell the person, as notices in the Inbox, that a goal finished by itself
/// and that the focus it held went back to none (snapshot.rs `notices`).
pub const GOAL_COMPLETED: &str = "goal_completed";
pub const FOCUS_CLEARED: &str = "focus_cleared";

impl Db {
    /// Completes goal `id` when it is active and finished (`goal_finished`), recording why and
    /// telling the person, and clears the project's focus when it was on it (`focus_after_completion`,
    /// keeping whether urgent work may interrupt). Returns what it said, or None when nothing changed.
    pub fn complete_goal_if_finished(&self, id: i64, actor: &Actor) -> Result<Option<String>> {
        let Some(goal) = self.goal(id)? else { return Ok(None) };
        if goal.state != "active" {
            return Ok(None);
        }
        let (issues, open): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(state_type NOT IN ('completed', 'canceled')), 0) FROM issue WHERE goal = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let waiting = self.goal_items_waiting(id)?.len();
        if !goal_finished(issues as usize, open as usize, waiting) {
            return Ok(None);
        }
        let ts = now();
        let checkpoint: Option<String> = self.conn.query_row("SELECT checkpoint_head FROM goal_branch WHERE goal_id = ?1", [id], |r| r.get(0)).optional()?.flatten();
        let at = checkpoint.map(|h| format!(", last checkpoint at {}", &h[..h.len().min(8)])).unwrap_or_default();
        let said = format!("Goal {id} \"{}\" completed: {issues} issue{}, all closed{at}", goal.title, if issues == 1 { "" } else { "s" });
        let payload = serde_json::json!({ "project": goal.project, "goal": id }).to_string();
        self.conn.execute("UPDATE goal SET state = 'done', updated_at = ?2 WHERE id = ?1", params![id, ts])?;
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, NULL, 'goal_state', ?3, ?4)",
            params![ts, actor.id(), format!("Goal {id} \"{}\" is done: every issue is closed and nothing waits for a checkpoint", goal.title), payload],
        )?;
        self.conn.execute(
            "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
            params![ts, actor.id(), GOAL_COMPLETED, said, payload],
        )?;
        // `focus_after_completion`, in one statement: cleared only while it is still on this goal,
        // so a focus the person set meanwhile stands (NIT-614).
        let cleared = self.conn.execute("UPDATE project_focus SET goal = NULL, updated_at = ?2, updated_by = ?3 WHERE project = ?1 AND goal = ?4", params![goal.project, ts, actor.id(), id])?;
        if cleared > 0 {
            self.conn.execute(
                "INSERT INTO event (ts, actor, issue_key, type, summary, payload) VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
                params![ts, actor.id(), FOCUS_CLEARED, format!("{}'s focus on goal {id} \"{}\" ended with the goal: agents work by priority until you set another", goal.project, goal.title), payload],
            )?;
        }
        Ok(Some(said))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewIssue;

    /// NIT-611: when a goal is finished, and what its completion does to the project's focus.
    #[test]
    fn a_goal_finishes_when_its_issues_are_closed_and_nothing_waits_and_its_focus_goes_with_it() {
        assert!(goal_finished(3, 0, 0));
        assert!(!goal_finished(3, 1, 0), "an issue still open");
        assert!(!goal_finished(3, 0, 2), "items wait on the goal branch for a checkpoint");
        assert!(!goal_finished(0, 0, 0), "a goal with no issues yet is not done");
        assert_eq!(focus_after_completion(Some(7), 7), None, "focus goes back to none");
        assert_eq!(focus_after_completion(Some(8), 7), Some(8), "another goal's focus stays");
        assert_eq!(focus_after_completion(None, 7), None);
    }

    /// One goal through to the end: its last issue closes, the goal completes, the person is told,
    /// and the focus on it clears, keeping whether urgent work may interrupt.
    #[test]
    fn closing_a_goals_last_issue_completes_it_and_clears_its_focus() {
        let mut db = db();
        let human = Actor::Human;
        let g = db.save_goal("NIT", None, &GoalChange { title: Some("Ship it".into()), outcome: Some("It ships.".into()), ..Default::default() }, &human).unwrap();
        let new = |title: &str| crate::issues::NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, goal: Some(g.id), ..Default::default() };
        let a = db.create_issue(&new("A"), &human).unwrap().key;
        let b = db.create_issue(&new("B"), &human).unwrap().key;
        db.set_project_focus("NIT", Some(g.id), false, &human).unwrap();
        db.move_issue(&a, "Done", Some("shipped"), None, &human).unwrap();
        assert_eq!(db.goal(g.id).unwrap().unwrap().state, "active", "B is still open");
        db.move_issue(&b, "Canceled", None, Some("not needed"), &human).unwrap();
        assert_eq!(db.goal(g.id).unwrap().unwrap().state, "done");
        let focus = db.project_focus("NIT").unwrap();
        assert_eq!((focus.goal, focus.allow_urgent), (None, false), "focus cleared, the urgent choice kept");
        let notices = db.notices().unwrap();
        assert!(notices.iter().any(|n| n.summary.contains("completed: 2 issues")), "{notices:?}");
        assert!(notices.iter().any(|n| n.summary.contains("focus on goal")), "{notices:?}");
        assert!(db.goals("NIT", false).unwrap().iter().all(|x| x.id != g.id), "hidden unless closed goals are asked for");

        // NIT-614: a goal whose last open issue moves to another goal is finished too, and the
        // focus set on that other goal stands.
        let h = db.save_goal("NIT", None, &GoalChange { title: Some("Next".into()), outcome: Some("Then.".into()), ..Default::default() }, &human).unwrap();
        let other = db.save_goal("NIT", None, &GoalChange { title: Some("Other".into()), outcome: Some("Else.".into()), ..Default::default() }, &human).unwrap();
        let in_h = |title: &str| crate::issues::NewIssue { goal: Some(h.id), ..new(title) };
        let c = db.create_issue(&in_h("C"), &human).unwrap().key;
        let d = db.create_issue(&in_h("D"), &human).unwrap().key;
        db.move_issue(&c, "Done", Some("shipped"), None, &human).unwrap();
        db.set_project_focus("NIT", Some(other.id), true, &human).unwrap();
        db.set_issue_goal(&d, Some(other.id), &human).unwrap();
        assert_eq!(db.goal(h.id).unwrap().unwrap().state, "done", "its last open issue left");
        assert_eq!(db.project_focus("NIT").unwrap().goal, Some(other.id), "the person's focus on another goal stands");
    }

    fn db() -> Db {
        let dir = crate::paths::unique_temp("nitely-goals", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        db.create_project("OPS", "Ops").unwrap();
        db
    }

    fn issue(db: &mut Db, project: &str, title: &str, goal: Option<i64>) -> String {
        let new = NewIssue {
            project: project.into(),
            title: title.into(),
            body: format!("{title}, in full"),
            kind: "feature".into(),
            source: "claude".into(),
            evidence: "none yet".into(),
            allow_similar: true,
            goal,
            ..Default::default()
        };
        db.create_issue(&new, &Actor::Agent("claude:/w".into())).unwrap().key
    }

    #[test]
    fn goals_hold_issues_and_count_their_progress() {
        let mut db = db();
        let agent = Actor::Agent("claude:/w".into());
        let release = GoalChange { title: Some("Public release".into()), outcome: Some("Strangers install it.".into()), target_date: Some("2026-12-01".into()), ..Default::default() };
        let g = db.save_goal("nit", None, &release, &agent).unwrap();
        assert_eq!((g.project.as_str(), g.state.as_str(), g.target_date.as_deref()), ("NIT", "active", Some("2026-12-01")));

        let a = issue(&mut db, "NIT", "Signed installers", Some(g.id));
        let b = issue(&mut db, "NIT", "Auto-update", None);
        db.set_issue_goal(&b, Some(g.id), &agent).unwrap();
        assert_eq!(db.issue(&b).unwrap().unwrap().goal, Some(g.id));
        assert!(db.timeline(&b).unwrap().iter().any(|e| e.kind == "goal_changed" && e.summary.as_deref() == Some("Goal: Public release")));
        db.move_issue(&a, "Done", Some("shipped"), None, &agent).unwrap();

        let d = db.goal_detail(g.id).unwrap();
        assert_eq!(d.issues.len(), 2);
        assert_eq!((d.goal.progress.total, d.goal.progress.done, d.goal.progress.open), (2, 1, 1));

        // The goal rows: active goals of every project, each with its issues.
        let rows = db.active_goals(None).unwrap();
        assert_eq!((rows.len(), rows[0].issues.len()), (1, 2));
        // B leaves, and the goal's one issue left is done: it finishes by itself (NIT-614).
        db.set_issue_goal(&b, None, &agent).unwrap();
        let left = db.goal(g.id).unwrap().unwrap();
        assert_eq!((left.progress.total, left.state.as_str()), (1, "done"));
        assert!(db.active_goals(Some("nit")).unwrap().is_empty());
    }

    #[test]
    fn issues_join_only_an_active_goal_of_their_project() {
        let mut db = db();
        let agent = Actor::Agent("claude:/w".into());
        let new = |t: &str| GoalChange { title: Some(t.into()), outcome: Some("o".into()), ..Default::default() };
        let ops = db.save_goal("OPS", None, &new("Ops goal"), &agent).unwrap();
        let nit = db.save_goal("NIT", None, &new("Nit goal"), &agent).unwrap();
        let key = issue(&mut db, "NIT", "Something", None);
        assert!(db.set_issue_goal(&key, Some(ops.id), &agent).unwrap_err().to_string().contains("belongs to OPS"));
        assert!(db.set_issue_goal(&key, Some(999), &agent).is_err());
        db.save_goal("NIT", Some(nit.id), &GoalChange { state: Some("done".into()), ..Default::default() }, &agent).unwrap();
        assert!(db.set_issue_goal(&key, Some(nit.id), &agent).unwrap_err().to_string().contains("is done"));
        // Filing straight into a closed goal is refused too, and files nothing.
        let filed = db.create_issue(
            &NewIssue { project: "NIT".into(), title: "Late".into(), body: "b".into(), kind: "bug".into(), source: "claude".into(), evidence: "e".into(), goal: Some(nit.id), ..Default::default() },
            &agent,
        );
        assert!(filed.is_err());
        assert_eq!(db.list_issues(Some("NIT"), true).unwrap().len(), 1);
    }

    #[test]
    fn goals_follow_a_new_key_and_go_with_the_project() {
        let mut db = db();
        let agent = Actor::Agent("claude:/w".into());
        let g = db.save_goal("OPS", None, &GoalChange { title: Some("Calm on-call".into()), outcome: Some("o".into()), ..Default::default() }, &agent).unwrap();
        issue(&mut db, "OPS", "Page less", Some(g.id));
        db.rekey_project("OPS", "OP").unwrap();
        let moved = db.goals("op", false).unwrap();
        assert_eq!((moved.len(), moved[0].progress.total), (1, 1));
        db.remove_project("OP", true).unwrap();
        assert!(db.goal(g.id).unwrap().is_none());
    }

    #[test]
    fn goals_list_active_first_by_date_and_validate_their_fields() {
        let db = db();
        let agent = Actor::Human;
        let new = |t: &str, d: Option<&str>| GoalChange { title: Some(t.into()), outcome: Some("o".into()), target_date: d.map(str::to_string), ..Default::default() };
        let later = db.save_goal("NIT", None, &new("Later", Some("2027-03-01")), &agent).unwrap();
        db.save_goal("NIT", None, &new("Undated", None), &agent).unwrap();
        db.save_goal("NIT", None, &new("Sooner", Some("2026-11-01")), &agent).unwrap();
        db.save_goal("NIT", Some(later.id), &GoalChange { state: Some("dropped".into()), ..Default::default() }, &agent).unwrap();
        let titles = |all: bool| db.goals("NIT", all).unwrap().into_iter().map(|g| g.title).collect::<Vec<_>>();
        assert_eq!(titles(false), ["Sooner", "Undated"]);
        assert_eq!(titles(true), ["Sooner", "Undated", "Later"]);
        for bad in ["2026-13-01", "2026-02-29", "2026-04-31", "2026-1-01", "next week"] {
            assert!(db.save_goal("NIT", None, &new("Bad date", Some(bad)), &agent).is_err(), "{bad}");
        }
        assert!(db.save_goal("NIT", None, &new("Leap day", Some("2028-02-29")), &agent).is_ok());
        // A change touches only what it names.
        let a = db.save_goal("NIT", Some(later.id), &GoalChange { title: Some("Renamed".into()), ..Default::default() }, &agent).unwrap();
        let b = db.save_goal("NIT", Some(later.id), &GoalChange { outcome: Some("New outcome".into()), ..Default::default() }, &agent).unwrap();
        assert_eq!((a.title.as_str(), b.title.as_str(), b.outcome.as_str(), b.state.as_str()), ("Renamed", "Renamed", "New outcome", "dropped"));
        assert!(db.save_goal("NIT", Some(999), &GoalChange { title: Some("x".into()), ..Default::default() }, &agent).is_err());
        assert!(db.save_goal("NIT", None, &GoalChange { title: Some("No outcome".into()), ..Default::default() }, &agent).is_err());
        assert!(db.save_goal("NIT", Some(later.id), &GoalChange { state: Some("paused".into()), ..Default::default() }, &agent).is_err());
        // Clearing the date.
        let cleared = db.save_goal("NIT", Some(later.id), &GoalChange { target_date: Some(String::new()), ..Default::default() }, &agent).unwrap();
        assert_eq!(cleared.target_date, None);
    }

    #[test]
    fn the_person_ranks_goals_and_sets_their_budget_and_rows_count_the_crew() {
        let db = db();
        let human = Actor::Human;
        let new = |t: &str, d: Option<&str>| GoalChange { title: Some(t.into()), outcome: Some("o".into()), target_date: d.map(str::to_string), ..Default::default() };
        let soon = db.save_goal("NIT", None, &new("Soon", Some("2026-11-01")), &human).unwrap();
        let old = db.save_goal("NIT", None, &new("Old", None), &human).unwrap();
        let picked = db.save_goal("NIT", None, &new("Picked", None), &human).unwrap();
        let titles = |db: &Db| db.goals("NIT", false).unwrap().into_iter().map(|g| g.title).collect::<Vec<_>>();
        assert_eq!(titles(&db), ["Soon", "Old", "Picked"], "unranked: nearest date, then oldest");
        db.save_goal("NIT", Some(picked.id), &GoalChange { rank: Some(1), workers: Some(2), ..Default::default() }, &human).unwrap();
        db.save_goal("NIT", Some(old.id), &GoalChange { rank: Some(2), ..Default::default() }, &human).unwrap();
        assert_eq!(titles(&db), ["Picked", "Old", "Soon"], "ranked ones first, in rank order");
        let g = db.goal(picked.id).unwrap().unwrap();
        assert_eq!((g.rank, g.workers, g.working, g.landed_today), (Some(1), Some(2), 0, 0));
        let cleared = db.save_goal("NIT", Some(picked.id), &GoalChange { rank: Some(0), workers: Some(0), ..Default::default() }, &human).unwrap();
        assert_eq!((cleared.rank, cleared.workers), (None, None), "0 clears");
        assert!(db.save_goal("NIT", Some(soon.id), &GoalChange { rank: Some(-1), ..Default::default() }, &human).is_err());
        // Only the person orders goals: an agent's rank or budget is refused.
        let agent = Actor::Agent("claude:x".into());
        assert!(db.save_goal("NIT", Some(soon.id), &GoalChange { rank: Some(1), ..Default::default() }, &agent).unwrap_err().to_string().contains("only the person"));
        assert!(db.save_goal("NIT", Some(soon.id), &GoalChange { workers: Some(1), ..Default::default() }, &agent).is_err());
        assert!(db.order_goals("NIT", &[soon.id], &agent).is_err());
        // Ordering: the ones given first, the others after in their order.
        let ordered: Vec<String> = db.order_goals("NIT", &[soon.id], &human).unwrap().into_iter().map(|g| g.title).collect();
        assert_eq!(ordered, ["Soon", "Old", "Picked"]);
        assert!(db.order_goals("NIT", &[999], &human).is_err());
    }

}
