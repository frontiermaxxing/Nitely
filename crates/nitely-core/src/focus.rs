//! The person's current project focus, shared by every automatic task allocator. Priority stays
//! unchanged; focus restricts new allocations to a goal and the open prerequisites it needs.

use crate::db::{now, Db};
use crate::issues::Actor;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct ProjectFocus {
    pub project: String,
    pub goal: Option<i64>,
    pub goal_title: Option<String>,
    pub allow_urgent: bool,
    /// False when focus is cleared or its goal is no longer active. An inactive goal still
    /// restricts allocation until the person chooses another goal or clears focus.
    pub active: bool,
}

fn read(conn: &Connection, project: &str) -> Result<ProjectFocus> {
    conn.query_row(
        "SELECT p.key, f.goal, g.title, COALESCE(f.allow_urgent, 1), COALESCE(g.state = 'active', 0)
         FROM project p LEFT JOIN project_focus f ON f.project = p.key
         LEFT JOIN goal g ON g.id = f.goal AND g.project = p.key
         WHERE p.key = ?1 COLLATE NOCASE",
        [project],
        |r| Ok(ProjectFocus { project: r.get(0)?, goal: r.get(1)?, goal_title: r.get(2)?, allow_urgent: r.get(3)?, active: r.get(4)? }),
    ).optional()?.with_context(|| format!("no project {project}"))
}

impl Db {
    pub fn project_focus(&self, project: &str) -> Result<ProjectFocus> {
        let p = self.project(project)?.with_context(|| format!("no project {project}"))?;
        read(&self.conn, &p.key)
    }

    /// Changes focus only at the person's request, atomically with its audit event.
    pub fn set_project_focus(&mut self, project: &str, goal: Option<i64>, allow_urgent: bool, actor: &Actor) -> Result<ProjectFocus> {
        if *actor != Actor::Human {
            bail!("only the person sets project focus, in the app: that decides what agents work on");
        }
        let project = self.project(project)?.with_context(|| format!("no project {project}"))?.key;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before = read(&tx, &project)?;
        if let Some(id) = goal {
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM goal WHERE id = ?1 AND project = ?2 AND state = 'active')", params![id, project], |r| r.get(0))?;
            if !valid {
                bail!("goal {id} is not an active goal of {project}");
            }
        }
        if before.goal != goal || before.allow_urgent != allow_urgent {
            tx.execute(
                "INSERT INTO project_focus (project, goal, allow_urgent, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(project) DO UPDATE SET goal = excluded.goal, allow_urgent = excluded.allow_urgent,
                   updated_at = excluded.updated_at, updated_by = excluded.updated_by",
                params![project, goal, allow_urgent, now(), actor.id()],
            )?;
            let after = read(&tx, &project)?;
            let summary = match after.goal_title.as_deref() {
                Some(title) => format!("{project} focus: {title}; urgent interruption {}", if allow_urgent { "allowed" } else { "off" }),
                None => format!("{project} focus cleared"),
            };
            tx.execute(
                "INSERT INTO event (ts, actor, type, summary, payload) VALUES (?1, ?2, 'project_focus_changed', ?3, ?4)",
                params![now(), actor.id(), summary, serde_json::json!({ "project": project, "before": before, "after": after }).to_string()],
            )?;
        }
        let focus = read(&tx, &project)?;
        tx.commit()?;
        Ok(focus)
    }

    /// Explanation after an allocator found nothing. Focus never silently spills into unrelated
    /// work when its issues are blocked, in flight, or at their worker limits.
    pub fn project_focus_wait_reason(&self, project: &str) -> Result<Option<String>> {
        let focus = self.project_focus(project)?;
        if focus.goal.is_none() {
            return Ok(None);
        }
        let title = focus.goal_title.as_deref().unwrap_or("the selected goal");
        Ok(Some(if !focus.active {
            format!("Focus on {title} is paused because the goal is no longer active. Choose another goal or clear focus in the app.")
        } else {
            format!("Waiting on focus: {title}. No focused work or allowed urgent work is ready; issues may be blocked, in flight, or at their worker limit.")
        }))
    }
}

/// Filter under the same immediate transaction that claims a task, so a focus change or another
/// allocator cannot race the scope and capacity check. No-focus behavior is untouched.
pub(crate) fn filter_candidates(
    conn: &Connection,
    project: &str,
    candidates: Vec<(String, i64, i64)>,
    default_goal_workers: u32,
) -> Result<Vec<(String, i64, i64)>> {
    let focused: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_focus WHERE project = ?1 COLLATE NOCASE AND goal IS NOT NULL)",
        [project], |r| r.get(0),
    )?;
    if !focused { return Ok(candidates); }
    let focus = read(conn, project)?;
    let Some(goal) = focus.goal else { return Ok(candidates) };
    if !focus.active {
        return Ok(vec![]);
    }
    // UNION (not UNION ALL) also bounds traversal if imported data ever contains a cycle.
    let mut stmt = conn.prepare(
        "WITH RECURSIVE scope(key) AS (
           SELECT key FROM issue WHERE goal = ?1 AND state_type NOT IN ('completed', 'canceled')
           UNION
           SELECT b.key FROM scope s JOIN issue_blocker r ON r.issue_key = s.key
             JOIN issue b ON b.key = r.blocker_key WHERE b.state_type NOT IN ('completed', 'canceled')
         ) SELECT key FROM scope",
    )?;
    let scope: HashSet<String> = stmt.query_map([goal], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut stmt = conn.prepare("SELECT issue_key, pid FROM crew_worker WHERE project = ?1 COLLATE NOCASE AND ended_at IS NULL AND pid IS NOT NULL")?;
    let workers = stmt.query_map([project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))?;
    let mut live_crew = HashSet::new();
    for worker in workers {
        let (key, pid) = worker?;
        if crate::procs::alive(pid) { live_crew.insert(key); }
    }
    let mut stmt = conn.prepare(
        "SELECT i.key, i.goal,
           (i.state = 'In Progress' AND i.origin = 'nitely' AND i.owner != 'human')
         FROM issue i WHERE i.project = ?1 COLLATE NOCASE AND i.state_type NOT IN ('completed', 'canceled')",
    )?;
    let rows = stmt.query_map([project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, bool>(2)?)))?;
    let ts = now();
    let sessions = crate::issues::recent_sessions(conn, ts)?;
    let mut goal_of = HashMap::new();
    let mut working: HashMap<i64, i64> = HashMap::new();
    let mut focused_working = 0;
    for row in rows {
        let (key, own_goal, in_progress) = row?;
        // A crashed supervisor can leave unclosed worker rows indefinitely. A live worker process
        // still holds its slot; otherwise use the picker's claim/activity/session lifetime so
        // abandoned Crew work can be reclaimed without first restarting its supervisor.
        let busy = live_crew.contains(&key) || (in_progress && crate::issues::idle_since(conn, &key, &sessions, ts)?.is_none());
        if let Some(id) = own_goal {
            goal_of.insert(key.clone(), id);
            if busy { *working.entry(id).or_default() += 1; }
        }
        if busy && scope.contains(&key) { focused_working += 1; }
    }
    let mut stmt = conn.prepare("SELECT id, COALESCE(workers, ?2) FROM goal WHERE project = ?1 COLLATE NOCASE AND state = 'active'")?;
    let budgets: HashMap<i64, i64> = stmt.query_map(params![project, default_goal_workers], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let full = |id: i64, count: i64| budgets.get(&id).is_some_and(|budget| *budget > 0 && count >= *budget);
    let focus_full = full(goal, focused_working);
    Ok(candidates.into_iter().filter(|(key, rank, _)| {
        // An old claim can make a live Crew issue look idle to the generic picker. It cannot be
        // reclaimed merely because the goal has room for an additional worker.
        if live_crew.contains(key) { return false; }
        // A prerequisite consumes a focus slot as well as its own goal's slot. P1 outside focus
        // may interrupt when allowed, but never bypasses its own goal's capacity.
        let in_scope = scope.contains(key);
        let allowed = if in_scope { !focus_full } else { focus.allow_urgent && *rank == 1 };
        allowed && !goal_of.get(key).is_some_and(|id| full(*id, working.get(id).copied().unwrap_or(0)))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::GoalChange;
    use crate::issues::NewIssue;

    fn db() -> Db {
        let dir = crate::paths::unique_temp("nitely-focus", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("APP", "Acme").unwrap();
        db
    }

    fn goal(db: &Db, title: &str, workers: Option<i64>) -> i64 {
        db.save_goal("APP", None, &GoalChange { title: Some(title.into()), outcome: Some("A useful outcome".into()), workers, ..Default::default() }, &Actor::Human).unwrap().id
    }

    fn issue(db: &mut Db, title: &str, priority: i64, goal: Option<i64>) -> String {
        db.create_issue(&NewIssue {
            project: "APP".into(), title: title.into(), body: "Do the work".into(), kind: "feature".into(),
            source: "human".into(), evidence: "Requested".into(), priority, goal, allow_similar: true,
            ..Default::default()
        }, &Actor::Human).unwrap().key
    }

    fn take(db: &mut Db) -> Option<String> {
        db.next_task("APP", "codex:test").unwrap().map(|i| i.key)
    }

    #[test]
    fn focus_is_human_only_validated_audited_and_persistent() {
        let mut db = db();
        let id = goal(&db, "Feature", None);
        let path = db.conn.path().unwrap().to_string();
        assert!(db.project_focus("app").unwrap().goal.is_none());
        for actor in [Actor::Agent("codex:test".into()), Actor::Agent("human".into())] {
            assert!(db.set_project_focus("APP", Some(id), false, &actor).is_err());
            assert!(db.set_project_focus("APP", None, true, &actor).is_err());
        }
        db.create_project("OPS", "Operations").unwrap();
        assert!(db.set_project_focus("OPS", Some(id), true, &Actor::Human).is_err());
        assert!(db.set_project_focus("APP", Some(999), true, &Actor::Human).is_err());
        db.set_project_focus("app", Some(id), false, &Actor::Human).unwrap();
        drop(db);
        let mut db = Db::open(std::path::Path::new(&path)).unwrap();
        let focus = db.project_focus("APP").unwrap();
        assert_eq!(focus.goal, Some(id));
        assert!(focus.active && !focus.allow_urgent);
        assert_eq!(focus.goal_title.as_deref(), Some("Feature"));
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        let (count, actor, payload): (i64, String, String) = db.conn.query_row(
            "SELECT COUNT(*), actor, payload FROM event WHERE type = 'project_focus_changed'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        ).unwrap();
        assert_eq!((count, actor.as_str()), (1, "human"), "saving the same focus is not another change");
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["after"]["goal"], id);
        assert!(payload["before"]["goal"].is_null());
        db.save_goal("APP", Some(id), &GoalChange { state: Some("done".into()), ..Default::default() }, &Actor::Human).unwrap();
        assert!(db.set_project_focus("APP", Some(id), true, &Actor::Human).is_err());
        db.set_project_focus("APP", None, true, &Actor::Human).unwrap();
        assert!(db.project_focus("APP").unwrap().goal.is_none());
    }

    #[test]
    fn direct_and_overlap_selection_follow_focus_and_urgent_toggle() {
        let mut db = db();
        let id = goal(&db, "Feature", None);
        let urgent = issue(&mut db, "Security patch", 1, None);
        let high = issue(&mut db, "Unrelated high priority", 2, None);
        let focused = issue(&mut db, "Feature work", 4, Some(id));
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        let config = crate::Config::default();
        let picked = crate::overlap::next_task(&config, &mut db, "APP", "codex:overlap", &[]).unwrap().unwrap();
        assert_eq!(picked.issue.key, focused);
        assert_eq!(take(&mut db), None, "no spill when focused work is in flight");
        assert!(db.project_focus_wait_reason("APP").unwrap().unwrap().contains("Waiting on focus"));
        db.set_project_focus("APP", Some(id), true, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(urgent));
        assert_eq!(take(&mut db), None, "high priority outside focus still waits");
        db.set_project_focus("APP", None, true, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(high));
        assert!(db.project_focus_wait_reason("APP").unwrap().is_none());
    }

    #[test]
    fn prerequisite_closure_unblocks_focus_without_including_unrelated_work() {
        let mut db = db();
        let id = goal(&db, "Feature", None);
        let other = goal(&db, "Infrastructure", None);
        let unrelated = issue(&mut db, "Unrelated urgent", 1, None);
        let root = issue(&mut db, "Root prerequisite", 4, Some(other));
        let middle = issue(&mut db, "Intermediate prerequisite", 4, None);
        let focused = issue(&mut db, "Feature finish", 3, Some(id));
        db.add_blocker(&middle, &root, &Actor::Human).unwrap();
        db.add_blocker(&focused, &middle, &Actor::Human).unwrap();
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(root.clone()));
        assert_eq!(take(&mut db), None);
        db.move_issue(&root, "Done", Some("complete"), None, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(middle.clone()));
        db.move_issue(&middle, "Done", Some("complete"), None, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(focused));
        assert_eq!(db.issue(&unrelated).unwrap().unwrap().state, "Todo");
    }

    #[test]
    fn focus_and_prerequisite_goal_budgets_apply_to_external_claims() {
        let mut db = db();
        let id = goal(&db, "Feature", Some(1));
        let other = goal(&db, "Infrastructure", Some(1));
        let first = issue(&mut db, "First prerequisite", 3, Some(other));
        let second = issue(&mut db, "Second prerequisite", 3, Some(other));
        let focused = issue(&mut db, "Feature finish", 4, Some(id));
        let ready = issue(&mut db, "Independent feature part", 4, Some(id));
        db.add_blocker(&focused, &first, &Actor::Human).unwrap();
        db.add_blocker(&focused, &second, &Actor::Human).unwrap();
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(first.clone()));
        assert_eq!(take(&mut db), None, "prerequisite uses the focus budget");
        db.save_goal("APP", Some(id), &GoalChange { workers: Some(2), ..Default::default() }, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(ready), "prerequisite's own goal budget prevents the second prerequisite");
        assert_eq!(take(&mut db), None);
        db.move_issue(&first, "Done", Some("complete"), None, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(second));
    }

    #[test]
    fn inactive_and_empty_focus_wait_until_human_changes_it() {
        let mut db = db();
        let id = goal(&db, "Empty feature", None);
        let urgent = issue(&mut db, "Urgent outside feature", 1, None);
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), None);
        db.set_project_focus("APP", Some(id), true, &Actor::Human).unwrap();
        for state in ["done", "dropped"] {
            db.save_goal("APP", Some(id), &GoalChange { state: Some(state.into()), ..Default::default() }, &Actor::Human).unwrap();
            assert_eq!(take(&mut db), None, "an inactive focus pauses even urgent work");
            let focus = db.project_focus("APP").unwrap();
            assert_eq!(focus.goal, Some(id));
            assert!(!focus.active);
            assert!(db.project_focus_wait_reason("APP").unwrap().unwrap().contains("no longer active"));
        }
        db.set_project_focus("APP", None, true, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(urgent));
    }

    #[test]
    fn external_work_keeps_its_slot_until_claim_activity_and_session_are_all_idle() {
        let mut db = db();
        let id = goal(&db, "Feature", Some(1));
        let running = issue(&mut db, "First feature part", 3, Some(id));
        issue(&mut db, "Second feature part", 4, Some(id));
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(running.clone()));
        let old = now() - 7 * 3600;
        db.conn.execute("UPDATE issue SET claimed_at = ?2 WHERE key = ?1", params![running, old]).unwrap();
        assert_eq!(take(&mut db), None, "recent activity keeps the slot after a claim expires");
        db.conn.execute("UPDATE event SET ts = ?2 WHERE issue_key = ?1", params![running, old]).unwrap();
        db.conn.execute(
            "INSERT INTO event (ts, actor, cwd, issue_key, type) VALUES (?1, 'codex:test', 'C:/Code/worktrees/feature', ?2, 'worktree')",
            params![old, running],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO session (id, vendor, cwd, started_at, last_seen_at, state) VALUES ('codex:live', 'codex', 'C:/Code/worktrees/feature/src', ?1, ?2, 'working')",
            params![old, now()],
        ).unwrap();
        assert_eq!(take(&mut db), None, "a live session keeps the slot after claim and events expire");
        db.conn.execute("UPDATE issue SET claimed_by = NULL, claimed_at = NULL WHERE key = ?1", [&running]).unwrap();
        assert_eq!(take(&mut db), None, "work started without a claim also consumes capacity");
        db.conn.execute("UPDATE session SET last_seen_at = ?1", [old]).unwrap();
        assert_eq!(take(&mut db), Some(running), "fully idle work releases capacity and can be reclaimed");
        assert_eq!(take(&mut db), None, "the reclaimed work occupies the slot again");
    }

    #[test]
    fn abandoned_crew_rows_release_capacity_but_live_workers_keep_their_slots() {
        let mut db = db();
        let id = goal(&db, "Feature", Some(1));
        let running = issue(&mut db, "First feature part", 3, Some(id));
        let second = issue(&mut db, "Second feature part", 4, Some(id));
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(running.clone()));
        let old = now() - 7 * 3600;
        db.conn.execute("UPDATE issue SET claimed_at = ?2 WHERE key = ?1", params![running, old]).unwrap();
        db.conn.execute("UPDATE event SET ts = ?2 WHERE issue_key = ?1", params![running, old]).unwrap();
        // No supervisor is running and the claim/activity have expired, but this worker process
        // is still alive. It must hold its slot even before a replacement supervisor adopts it.
        db.conn.execute(
            "INSERT INTO crew_worker (project, issue_key, agent, agent_id, pid, worktree, log_path, started_at)
             VALUES ('APP', ?1, 'codex', 'codex:worker', ?2, 'C:/Code/worktrees/feature', 'worker.log', ?3)",
            params![running, std::process::id(), old],
        ).unwrap();
        assert_eq!(take(&mut db), None, "a live worker keeps the focus slot despite old activity");
        db.save_goal("APP", Some(id), &GoalChange { workers: Some(2), ..Default::default() }, &Actor::Human).unwrap();
        assert_eq!(take(&mut db), Some(second.clone()), "spare capacity starts other work instead of reclaiming a live worker's issue");
        db.move_issue(&second, "Done", Some("complete"), None, &Actor::Human).unwrap();
        db.save_goal("APP", Some(id), &GoalChange { workers: Some(1), ..Default::default() }, &Actor::Human).unwrap();
        // The worker crashes too. Its unclosed row must not permanently fill the budget; a
        // direct allocator can recover the issue without waiting for the supervisor to restart.
        db.conn.execute("UPDATE crew_worker SET pid = 0 WHERE issue_key = ?1", [&running]).unwrap();
        assert_eq!(take(&mut db), Some(running.clone()), "an abandoned row no longer blocks crash recovery");
        assert_eq!(take(&mut db), None, "the new claim occupies the recovered slot");
        // The same recovery applies if the supervisor crashed before recording a setup pid.
        db.conn.execute("UPDATE issue SET claimed_at = ?2 WHERE key = ?1", params![running, old]).unwrap();
        db.conn.execute("UPDATE event SET ts = ?2 WHERE issue_key = ?1", params![running, old]).unwrap();
        db.conn.execute("UPDATE crew_worker SET pid = NULL, phase = 'setup', ended_at = NULL WHERE issue_key = ?1", [&running]).unwrap();
        assert_eq!(take(&mut db), Some(running));
    }

    #[test]
    fn focus_survives_rekey_and_is_removed_with_its_project() {
        let mut db = db();
        let id = goal(&db, "Feature", None);
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        db.rekey_project("APP", "NEW").unwrap();
        for key in ["NEW", "APP"] {
            let focus = db.project_focus(key).unwrap();
            assert_eq!((focus.project.as_str(), focus.goal, focus.allow_urgent), ("NEW", Some(id), false));
        }
        db.remove_project("NEW", true).unwrap();
        db.create_project("NEW", "Fresh project").unwrap();
        assert!(db.project_focus("NEW").unwrap().goal.is_none());
    }

    #[test]
    fn human_or_backlog_prerequisites_do_not_make_focus_spill() {
        let mut db = db();
        let id = goal(&db, "Feature", None);
        let prerequisite = issue(&mut db, "Human prerequisite", 3, None);
        let focused = issue(&mut db, "Feature finish", 4, Some(id));
        issue(&mut db, "Unrelated high priority", 2, None);
        db.add_blocker(&focused, &prerequisite, &Actor::Human).unwrap();
        db.set_project_focus("APP", Some(id), false, &Actor::Human).unwrap();
        db.conn.execute("UPDATE issue SET owner = 'human' WHERE key = ?1", [&prerequisite]).unwrap();
        assert_eq!(take(&mut db), None, "agents cannot do the human prerequisite");
        db.conn.execute("UPDATE issue SET owner = 'agent', state = 'Backlog', state_type = 'backlog' WHERE key = ?1", [&prerequisite]).unwrap();
        assert_eq!(take(&mut db), None, "focus does not promote backlog work");
    }
}
