//! Goal plans: an agent drafts a goal and its issues (with blockers) from a sentence the person
//! wrote, and the person decides it once in the Inbox. The plan rides on a human-owned issue in
//! Needs You, like a playbook change (`proposals.rs`). Approving creates the goal and files every
//! issue into it in one batch, after any edits (a new title, outcome or date, items left out);
//! canceling the issue rejects the plan and files nothing.

use crate::db::{now, Db};
use crate::goals::GoalChange;
use crate::issues::{Actor, BatchFiled, BatchItem, IssueRecord, NewIssue};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

/// One issue of a plan, as the agent drafted it: the fields of a `file_issues` item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanItem {
    #[serde(rename = "ref")]
    pub local_ref: String,
    pub title: String,
    pub kind: String,
    pub body: String,
    pub evidence: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Refs of this plan or existing issue keys.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// What an agent proposes: the goal, the new issues, and existing issues that belong in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalPlan {
    /// What the person asked for, in their words.
    pub sentence: String,
    pub title: String,
    pub outcome: String,
    #[serde(default)]
    pub target_date: Option<String>,
    #[serde(default)]
    pub issues: Vec<PlanItem>,
    #[serde(default)]
    pub existing: Vec<String>,
    /// The "Plan a goal" issue it answers, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_issue: Option<String>,
}

/// What the person changes before approving.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlanEdit {
    pub title: Option<String>,
    pub outcome: Option<String>,
    /// `Some("")` clears the date.
    pub target_date: Option<String>,
    /// Refs of drafted issues to leave out; blockers naming them are dropped with them.
    #[serde(default)]
    pub drop: Vec<String>,
}

/// A plan as the Inbox and the issue panel show it.
#[derive(Debug, Clone, Serialize)]
pub struct GoalPlanView {
    pub id: i64,
    pub issue_key: String,
    pub project: String,
    pub plan: GoalPlan,
    /// pending, approved or rejected.
    pub status: String,
    pub created_by: String,
    pub created_at: i64,
    /// What approving did (the goal and the issues filed), or why it was rejected.
    pub outcome: Option<String>,
    /// The goal approving created.
    pub goal_id: Option<i64>,
}

/// What `propose_goal` returns to the agent.
#[derive(Debug, Clone, Serialize)]
pub struct PlanProposed {
    pub issue: IssueRecord,
    pub url: String,
    pub next: String,
}

/// What approving did.
#[derive(Debug, Clone, Serialize)]
pub struct PlanApproved {
    pub goal: crate::goals::Goal,
    pub filed: BatchFiled,
    /// Existing issues put in the goal, and those that could not be.
    pub attached: Vec<String>,
    pub not_attached: Vec<String>,
    pub issue: IssueRecord,
}

fn source_of(actor: &Actor) -> String {
    match actor.id().split(':').next() {
        Some(v) if crate::issues::SOURCES.contains(&v) => v.to_string(),
        _ => "claude".into(),
    }
}

/// The plan's issues as a batch for `project`, filed into `goal` (None while only checking). The
/// plan's own issues (`own`: the one that carries it and the one that asked for it) describe the
/// same work, so the duplicate check passes over them.
fn batch(plan: &GoalPlan, project: &str, goal: Option<i64>, source: &str, own: &[String]) -> Vec<BatchItem> {
    plan.issues
        .iter()
        .map(|i| BatchItem {
            not_duplicates: own.to_vec(),
            local_ref: i.local_ref.clone(),
            issue: NewIssue {
                project: project.to_string(),
                title: i.title.clone(),
                body: i.body.clone(),
                kind: i.kind.clone(),
                source: source.to_string(),
                evidence: i.evidence.clone(),
                priority: i.priority,
                allow_similar: false,
                state: i.state.clone(),
                owner: i.owner.clone(),
                blocked_by: i.blocked_by.clone(),
                labels: i.labels.clone(),
                goal,
            },
        })
        .collect()
}

/// The issue body the person reads: the sentence, the goal, and each issue with its blockers.
fn describe(plan: &GoalPlan, who: &str) -> String {
    let who = crate::issues::agent_name(who);
    let mut out = format!(
        "{who} drafted this goal from what you asked for: \"{}\"\n\n**{}**: {}",
        plan.sentence.trim(),
        plan.title.trim(),
        plan.outcome.trim()
    );
    if let Some(d) = plan.target_date.as_deref().filter(|d| !d.is_empty()) {
        out.push_str(&format!(" Target: {d}."));
    }
    out.push_str("\n\nIssues it would file:\n");
    for i in &plan.issues {
        let blockers = if i.blocked_by.is_empty() { String::new() } else { format!(" (after {})", i.blocked_by.join(", ")) };
        let p = if i.priority > 0 { format!(", P{}", i.priority) } else { String::new() };
        out.push_str(&format!("- `{}` {} [{}{p}]{blockers}\n", i.local_ref, i.title.trim(), i.kind));
    }
    if !plan.existing.is_empty() {
        out.push_str(&format!("\nExisting issues it would gather: {}\n", plan.existing.join(", ")));
    }
    out.push_str("\nApprove to create the goal and file these (you can edit first in the Inbox); cancel it with a reason to file nothing.");
    out
}

impl GoalPlan {
    /// Applies the person's edit: new goal fields, and drafted issues left out with any blockers
    /// that named them.
    fn edited(&self, edit: &PlanEdit) -> GoalPlan {
        let mut plan = self.clone();
        let clean = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(t) = clean(&edit.title) {
            plan.title = t;
        }
        if let Some(o) = clean(&edit.outcome) {
            plan.outcome = o;
        }
        if let Some(d) = &edit.target_date {
            plan.target_date = Some(d.trim().to_string()).filter(|d| !d.is_empty());
        }
        let dropped = |r: &str| edit.drop.iter().any(|d| d.trim() == r.trim());
        plan.issues.retain(|i| !dropped(&i.local_ref));
        for i in &mut plan.issues {
            i.blocked_by.retain(|b| !dropped(b));
        }
        plan
    }
}

/// How long an approval holds its plan. One that has not finished by then was interrupted (the
/// app quit midway), and the plan is pending again: approving it resumes with what was created.
const APPROVAL_LEASE_SECS: i64 = 120;

/// A plan's columns, an interrupted approval (`?2` is the moment its lease began) read as pending.
const COLUMNS: &str = "id, issue_key, project, plan,
    CASE WHEN status = 'approving' AND IFNULL(decided_at, 0) < ?2 THEN 'pending' ELSE status END,
    created_by, created_at, outcome, goal_id";

fn view(r: &rusqlite::Row) -> rusqlite::Result<GoalPlanView> {
    let plan: String = r.get(3)?;
    Ok(GoalPlanView {
        id: r.get(0)?,
        issue_key: r.get(1)?,
        project: r.get(2)?,
        plan: serde_json::from_str(&plan).unwrap_or_default(),
        status: r.get(4)?,
        created_by: r.get(5)?,
        created_at: r.get(6)?,
        outcome: r.get(7)?,
        goal_id: r.get(8)?,
    })
}

impl Db {
    /// The plan an issue carries, if it carries one.
    pub fn goal_plan_for_issue(&self, key: &str) -> Result<Option<GoalPlanView>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM goal_plan WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1"),
                params![key, now() - APPROVAL_LEASE_SECS],
                view,
            )
            .optional()?)
    }

    /// Checks a plan as approving it would (the goal's fields, every issue, blockers and cycles,
    /// the existing issues), files it as a human-owned issue in Needs You, and, when it answers a
    /// planning issue, closes that one. Nothing is filed when a check fails.
    pub fn propose_goal(&mut self, project: &str, plan: &GoalPlan, planning_issue: Option<&str>, actor: &Actor) -> Result<PlanProposed> {
        if *actor == Actor::Human {
            bail!("a person creates a goal directly; plans are drafted by agents");
        }
        let p = self.project(project)?.with_context(|| format!("no project {project}"))?;
        if plan.sentence.trim().is_empty() {
            bail!("say what the person asked for, in their words (sentence)");
        }
        if plan.title.trim().is_empty() || plan.outcome.trim().is_empty() {
            bail!("a goal needs a title and an outcome: one paragraph on what is true once it is reached");
        }
        crate::goals::check_date(plan.target_date.as_deref().unwrap_or(""))?;
        if plan.issues.is_empty() && plan.existing.is_empty() {
            bail!("a plan needs issues: new ones, existing ones, or both");
        }
        if !plan.issues.is_empty() {
            self.check_batch(&batch(plan, &p.key, None, &source_of(actor), &[]))?;
        }
        for key in &plan.existing {
            let i = self.issue(key)?.with_context(|| format!("no issue {key}"))?;
            if i.project.as_deref() != Some(p.key.as_str()) {
                bail!("{} belongs to {}, not {}", i.key, i.project.unwrap_or_default(), p.key);
            }
        }
        let planning = match planning_issue {
            Some(k) => Some(self.issue(k)?.with_context(|| format!("no issue {k}"))?),
            None => None,
        };

        let issue = self.create_issue(
            &NewIssue {
                project: p.key.clone(),
                title: format!("Plan: {}", plan.title.trim()),
                body: describe(plan, actor.id()),
                kind: "feature".into(),
                source: source_of(actor),
                evidence: format!("Drafted with propose_goal from \"{}\".", plan.sentence.trim()),
                priority: 2,
                allow_similar: true,
                state: Some("Needs You".into()),
                owner: Some("human".into()),
                labels: vec!["goal".into()],
                blocked_by: vec![],
                goal: None,
            },
            actor,
        )?;
        let mut stored = plan.clone();
        stored.planning_issue = planning.as_ref().map(|i| i.key.clone());
        self.conn.execute(
            "INSERT INTO goal_plan (issue_key, project, plan, status, created_by, created_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?5)",
            params![issue.key, p.key, serde_json::to_string(&stored)?, actor.id(), now()],
        )?;
        if let Some(planning) = planning.filter(|i| i.is_open()) {
            self.move_issue(&planning.key, "Done", Some(&format!("Drafted the plan as {}; the person decides it in the Inbox.", issue.key)), None, actor)?;
        }
        Ok(PlanProposed {
            url: issue.url.clone(),
            next: format!("Filed as {} in Needs You. The person approves (maybe after editing) or rejects it; nothing is filed until then.", issue.key),
            issue,
        })
    }

    /// The person approves a pending plan, with their edits: the goal is created and every issue
    /// filed into it in one batch (as the agent who drafted them), existing issues are put in it,
    /// and the plan's issue is closed.
    pub fn approve_goal_plan(&mut self, key: &str, edit: &PlanEdit, actor: &Actor) -> Result<PlanApproved> {
        if *actor != Actor::Human {
            bail!("only the person approves a goal plan");
        }
        let v = self.goal_plan_for_issue(key)?.filter(|v| v.status == "pending").with_context(|| format!("{key} carries no pending goal plan"))?;
        // Claim the decision first, so two approvals at once cannot both create a goal. A claim
        // older than the lease was interrupted and is taken over.
        let ts = now();
        let claimed = self.conn.execute(
            "UPDATE goal_plan SET status = 'approving', decided_at = ?2
             WHERE id = ?1 AND (status = 'pending' OR (status = 'approving' AND IFNULL(decided_at, 0) < ?3))",
            params![v.id, ts, ts - APPROVAL_LEASE_SECS],
        )?;
        if claimed == 0 {
            bail!("{key}'s goal plan was decided just now");
        }
        let result = self.approve_claimed(key, &v, edit, actor);
        if result.is_err() {
            self.conn
                .execute("UPDATE goal_plan SET status = 'pending', decided_at = NULL WHERE id = ?1 AND status = 'approving'", [v.id])
                .ok();
        }
        result
    }

    fn approve_claimed(&mut self, key: &str, v: &GoalPlanView, edit: &PlanEdit, actor: &Actor) -> Result<PlanApproved> {
        let mut plan = v.plan.edited(edit);
        // Keys stored under a project's old prefix name the same issues under its new one.
        plan.planning_issue = plan.planning_issue.as_deref().map(|k| self.current_key(k)).transpose()?;
        plan.existing = plan.existing.iter().map(|k| self.current_key(k)).collect::<Result<_>>()?;
        let author = Actor::Agent(v.created_by.clone());
        let own: Vec<String> = std::iter::once(v.issue_key.clone()).chain(plan.planning_issue.clone()).collect();
        // An interrupted approval left its goal, and maybe the issues it filed (create_issues files
        // all or none): carry on with them. Its issues are the ones in the goal that the plan's
        // author filed under the plan's titles; one attached to the goal since does not count.
        let resumed = match v.goal_id {
            Some(id) => self.goal(id)?,
            None => None,
        };
        let titles: Vec<&str> = v.plan.issues.iter().map(|i| i.title.trim()).collect();
        let already_filed = match &resumed {
            Some(g) => self
                .list_issues(Some(&v.project), true)?
                .into_iter()
                .filter(|i| i.goal == Some(g.id) && i.source.as_deref() == Some(source_of(&author).as_str()) && titles.contains(&i.title.trim()))
                .count(),
            None => 0,
        };
        if !plan.issues.is_empty() && already_filed == 0 {
            self.check_batch(&batch(&plan, &v.project, None, &source_of(&author), &own))?;
        }
        // The person's edits stand on a resumed approval too (a cleared date clears it).
        let change = GoalChange {
            title: Some(plan.title.clone()),
            outcome: Some(plan.outcome.clone()),
            target_date: Some(plan.target_date.clone().unwrap_or_default()),
            state: None,
        };
        let goal = match resumed {
            Some(g) => self.save_goal(&v.project, Some(g.id), &change, actor)?,
            None => {
                // The goal and the plan's note of it commit together, so a retry always finds it.
                let tx = self.conn.unchecked_transaction()?;
                let goal = self.save_goal(&v.project, None, &change, actor)?;
                self.conn.execute("UPDATE goal_plan SET goal_id = ?2 WHERE id = ?1", params![v.id, goal.id])?;
                tx.commit()?;
                goal
            }
        };
        let filed = if plan.issues.is_empty() || already_filed > 0 {
            BatchFiled::default()
        } else {
            match self.create_issues(&batch(&plan, &v.project, Some(goal.id), &source_of(&author), &own), &author) {
                Ok(f) => f,
                Err(e) => {
                    // Nothing was filed (create_issues is one transaction): the goal goes too.
                    self.conn.execute("UPDATE goal_plan SET goal_id = NULL WHERE id = ?1", [v.id])?;
                    self.conn.execute("DELETE FROM goal WHERE id = ?1", [goal.id])?;
                    return Err(e);
                }
            }
        };
        let (mut attached, mut not_attached) = (Vec::new(), Vec::new());
        for k in &plan.existing {
            match self.set_issue_goal(k, Some(goal.id), actor) {
                Ok(_) => attached.push(k.clone()),
                Err(e) => not_attached.push(format!("{k}: {e:#}")),
            }
        }
        let new = filed.filed.len().max(already_filed);
        let outcome = format!(
            "Goal {} \"{}\" with {} new issue{}{}{}",
            goal.id,
            goal.title,
            new,
            if new == 1 { "" } else { "s" },
            if attached.is_empty() { String::new() } else { format!(" and {} existing", attached.len()) },
            if filed.skipped.is_empty() { String::new() } else { format!("; {} skipped as duplicates", filed.skipped.len()) },
        );
        self.conn.execute(
            "UPDATE goal_plan SET status = 'approved', outcome = ?2, goal_id = ?3, decided_by = ?4, decided_at = ?5 WHERE id = ?1",
            params![v.id, outcome, goal.id, actor.id(), now()],
        )?;
        let goal = self.goal(goal.id)?.context("goal vanished")?;
        let issue = self.finish_plan_issue(key, &outcome, actor)?;
        Ok(PlanApproved { goal, filed, attached, not_attached, issue })
    }

    /// Moves the plan's issue to Done once its plan is decided.
    fn finish_plan_issue(&mut self, key: &str, outcome: &str, actor: &Actor) -> Result<IssueRecord> {
        let issue = self.issue(key)?.context("the plan's issue is gone")?;
        if !issue.is_open() {
            return Ok(issue);
        }
        self.move_issue(&issue.key, "Done", Some(&format!("Approved: {outcome}.")), None, actor)
    }

    /// Runs before an issue moves (see `proposals::before_move`): approving the issue of a pending
    /// plan approves the plan as drafted; canceling it rejects the plan; marking it Done is refused.
    pub(crate) fn before_plan_move(&mut self, key: &str, state: &str, reason: Option<&str>, actor: &Actor) -> Result<Option<IssueRecord>> {
        let Some(v) = self.goal_plan_for_issue(key)?.filter(|v| v.status == "pending") else { return Ok(None) };
        match state {
            "Approved" if *actor == Actor::Human => Ok(Some(self.approve_goal_plan(key, &PlanEdit::default(), actor)?.issue)),
            "Done" => bail!("{key} carries a goal plan: approve it (files its issues) or cancel it with a reason (files nothing)"),
            "Canceled" | "Duplicate" => {
                let reason = reason.map(str::trim).filter(|r| !r.is_empty()).unwrap_or("no reason given");
                self.conn.execute(
                    "UPDATE goal_plan SET status = 'rejected', outcome = ?2, decided_by = ?3, decided_at = ?4 WHERE id = ?1",
                    params![v.id, reason, actor.id(), now()],
                )?;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// A pending plan stays the human's to decide.
    pub(crate) fn check_plan_owner_change(&self, key: &str, owner: &str) -> Result<()> {
        if owner != "human" && self.goal_plan_for_issue(key)?.is_some_and(|v| v.status == "pending") {
            bail!("{key} carries a goal plan only you can decide: approve it or cancel it with a reason");
        }
        Ok(())
    }

    /// Files the issue an agent picks up to draft a goal from the person's sentence.
    pub fn start_goal(&mut self, project: &str, sentence: &str, actor: &Actor) -> Result<IssueRecord> {
        let sentence = sentence.trim();
        if sentence.is_empty() {
            bail!("say in a sentence what the goal is");
        }
        let short: String = sentence.chars().take(80).collect();
        self.create_issue(
            &NewIssue {
                project: project.to_string(),
                title: format!("Plan a goal: {short}"),
                body: format!(
                    "The person wants: \"{sentence}\"\n\nDraft it with `propose_goal`, passing this issue as planning_issue: a goal (a short title and a one-paragraph outcome, a target date only if they gave one) and the issues that get there, each with evidence and blockers, plus existing issues of the project that belong in it (`list_issues`). Read the project's brief first (`get_project`). Do not file the issues yourself: the person approves the plan in the Inbox, and that files them."
                ),
                kind: "feature".into(),
                source: if *actor == Actor::Human { "human".into() } else { source_of(actor) },
                evidence: "Asked for from the command palette.".into(),
                priority: 2,
                allow_similar: true,
                labels: vec!["goal".into()],
                ..Default::default()
            },
            actor,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open(&crate::paths::unique_temp("nitely-plans", "").join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        db
    }

    fn agent() -> Actor {
        Actor::Agent("claude:/w".into())
    }

    fn item(r: &str, title: &str, blocked_by: &[&str]) -> PlanItem {
        PlanItem {
            local_ref: r.into(),
            title: title.into(),
            kind: "feature".into(),
            body: format!("{title}, because the goal needs it"),
            evidence: "none yet".into(),
            priority: 2,
            blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn plan() -> GoalPlan {
        GoalPlan {
            sentence: "make Nitely installable by strangers".into(),
            title: "Strangers can install Nitely".into(),
            outcome: "A signed installer anyone can run.".into(),
            target_date: Some("2026-12-01".into()),
            issues: vec![item("ci", "Build installers in CI", &[]), item("sign", "Sign the Windows installer", &["ci"]), item("docs", "Install guide in the README", &["ci"])],
            existing: vec![],
            planning_issue: None,
        }
    }

    #[test]
    fn a_plan_files_nothing_until_approved_then_files_everything_into_its_goal() {
        let mut db = db();
        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        assert_eq!((p.issue.state.as_str(), p.issue.owner.as_str()), ("Needs You", "human"));
        assert!(p.issue.body.contains("`sign` Sign the Windows installer [feature, P2] (after ci)"), "{}", p.issue.body);
        assert_eq!(db.list_issues(Some("NIT"), true).unwrap().len(), 1, "only the plan's issue");
        assert!(db.goals("NIT", true).unwrap().is_empty());

        // An agent cannot approve it, and it cannot be marked done or handed to agents.
        assert!(db.approve_goal_plan(&p.issue.key, &PlanEdit::default(), &agent()).is_err());
        assert!(db.move_issue(&p.issue.key, "Done", Some("x"), None, &Actor::Human).is_err());
        assert!(db.set_owner(&p.issue.key, "agent", &Actor::Human).is_err());

        // The person edits the title and leaves the docs item out, then approves.
        let edit = PlanEdit { title: Some("Anyone can install Nitely".into()), drop: vec!["docs".into()], ..Default::default() };
        let a = db.approve_goal_plan(&p.issue.key, &edit, &Actor::Human).unwrap();
        assert_eq!((a.goal.title.as_str(), a.goal.progress.total, a.filed.filed.len()), ("Anyone can install Nitely", 2, 2));
        let sign = db.issue(&a.filed.keys["sign"]).unwrap().unwrap();
        assert_eq!((sign.goal, sign.blocked_by.clone(), sign.source.as_deref()), (Some(a.goal.id), vec![a.filed.keys["ci"].clone()], Some("claude")));
        assert_eq!(a.issue.state, "Done");
        let v = db.goal_plan_for_issue(&p.issue.key).unwrap().unwrap();
        assert_eq!((v.status.as_str(), v.goal_id), ("approved", Some(a.goal.id)));
        assert!(db.approve_goal_plan(&p.issue.key, &PlanEdit::default(), &Actor::Human).is_err(), "decided once");
    }

    #[test]
    fn a_plans_own_issues_never_count_as_duplicates_of_its_work() {
        let mut db = db();
        let ask = db.start_goal("NIT", "sign the Windows installer", &Actor::Human).unwrap();
        let mut p = plan();
        p.title = "Sign the Windows installer".into();
        p.issues = vec![item("sign", "Sign the Windows installer", &[])];
        let proposed = db.propose_goal("NIT", &p, Some(&ask.key), &agent()).unwrap();
        let a = db.approve_goal_plan(&proposed.issue.key, &PlanEdit::default(), &Actor::Human).unwrap();
        assert_eq!((a.filed.filed.len(), a.filed.skipped.len()), (1, 0), "{:?}", a.filed.skipped);
        assert_eq!(a.goal.progress.total, 1);

        // The same after the project took a new key: the plan still knows its planning issue.
        let mut db = self::db();
        let ask = db.start_goal("NIT", "sign the Windows installer", &Actor::Human).unwrap();
        let proposed = db.propose_goal("NIT", &p, Some(&ask.key), &agent()).unwrap();
        db.rekey_project("NIT", "NX").unwrap();
        let a = db.approve_goal_plan(&proposed.issue.key.replace("NIT-", "NX-"), &PlanEdit::default(), &Actor::Human).unwrap();
        assert_eq!((a.filed.filed.len(), a.filed.skipped.len()), (1, 0), "{:?}", a.filed.skipped);
    }

    #[test]
    fn an_interrupted_approval_is_taken_up_again_and_finishes_what_it_started() {
        let mut db = db();
        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        let key = p.issue.key.clone();
        db.approve_goal_plan(&key, &PlanEdit::default(), &Actor::Human).unwrap();
        let goal = db.goal_plan_for_issue(&key).unwrap().unwrap().goal_id.unwrap();
        let filed = |db: &Db| db.list_issues(Some("NIT"), true).unwrap().into_iter().filter(|i| i.goal == Some(goal)).count();
        let n = filed(&db);
        // Wind the plan back to how the app left it when it quit after filing, before finishing.
        db.conn.execute("UPDATE goal_plan SET status = 'approving', decided_at = ?1, outcome = NULL", [now()]).unwrap();
        db.conn.execute("UPDATE issue SET state = 'Needs You', state_type = 'unstarted' WHERE key = ?1", [&key]).unwrap();
        assert_eq!(db.goal_plan_for_issue(&key).unwrap().unwrap().status, "approving");
        assert!(db.approve_goal_plan(&key, &PlanEdit::default(), &Actor::Human).is_err(), "a running approval holds the plan");
        db.conn.execute("UPDATE goal_plan SET decided_at = ?1", [now() - APPROVAL_LEASE_SECS - 1]).unwrap();
        assert_eq!(db.goal_plan_for_issue(&key).unwrap().unwrap().status, "pending", "an interrupted one lets go");
        let edit = PlanEdit { title: Some("Strangers install Nitely".into()), target_date: Some(String::new()), ..Default::default() };
        let a = db.approve_goal_plan(&key, &edit, &Actor::Human).unwrap();
        assert_eq!(a.goal.id, goal, "the goal it created is the one it finishes");
        assert_eq!((a.goal.title.as_str(), a.goal.target_date.as_deref()), ("Strangers install Nitely", None), "the edits made this time stand");
        assert_eq!((filed(&db), a.filed.filed.len(), db.goals("NIT", true).unwrap().len()), (n, 0, 1), "nothing filed twice");
        assert_eq!(a.issue.state, "Done");
        assert!(a.goal.progress.total >= 3);
    }

    #[test]
    fn an_approval_stopped_before_filing_files_its_issues_when_taken_up() {
        let mut db = db();
        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        let key = p.issue.key.clone();
        // The app quit after creating the goal, before filing; meanwhile an agent put another issue in it.
        let change = GoalChange { title: Some("Strangers can install Nitely".into()), outcome: Some("o".into()), ..Default::default() };
        let goal = db.save_goal("NIT", None, &change, &Actor::Human).unwrap().id;
        db.conn.execute("UPDATE goal_plan SET status = 'approving', decided_at = ?1, goal_id = ?2", params![now() - APPROVAL_LEASE_SECS - 1, goal]).unwrap();
        let other = db
            .create_issue(&NewIssue { project: "NIT".into(), title: "Pick a code-signing vendor".into(), body: "b".into(), kind: "idea".into(), source: "claude".into(), evidence: "e".into(), goal: Some(goal), ..Default::default() }, &agent())
            .unwrap();
        let a = db.approve_goal_plan(&key, &PlanEdit::default(), &Actor::Human).unwrap();
        assert_eq!((a.goal.id, a.filed.filed.len()), (goal, 3), "an unrelated issue in the goal is not the plan's filing");
        assert_eq!(db.issue(&other.key).unwrap().unwrap().goal, Some(goal));
        assert_eq!(db.goals("NIT", true).unwrap().len(), 1);
    }

    #[test]
    fn plans_follow_a_new_key_and_go_with_a_removed_project() {
        let mut db = db();
        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        db.rekey_project("NIT", "NX").unwrap();
        let key = p.issue.key.replace("NIT-", "NX-");
        assert_eq!(db.goal_plan_for_issue(&key).unwrap().unwrap().project, "NX");
        let a = db.approve_goal_plan(&key, &PlanEdit::default(), &Actor::Human).unwrap();
        assert_eq!(a.goal.project, "NX");
        db.remove_project("NX", true).unwrap();
        db.create_project("NX", "Again").unwrap();
        let fresh = db
            .create_issue(&NewIssue { project: "NX".into(), title: "New".into(), body: "b".into(), kind: "bug".into(), source: "human".into(), evidence: "e".into(), ..Default::default() }, &Actor::Human)
            .unwrap();
        assert!(db.goal_plan_for_issue(&fresh.key).unwrap().is_none(), "an old plan never attaches to a new issue");
    }

    #[test]
    fn bad_plans_are_refused_and_rejecting_files_nothing() {
        let mut db = db();
        let mut cycle = plan();
        cycle.issues[0].blocked_by = vec!["sign".into()];
        assert!(db.propose_goal("NIT", &cycle, None, &agent()).unwrap_err().to_string().contains("cycle"));
        let mut unknown = plan();
        unknown.issues[1].blocked_by = vec!["nope".into()];
        assert!(db.propose_goal("NIT", &unknown, None, &agent()).is_err());
        let mut no_outcome = plan();
        no_outcome.outcome = " ".into();
        assert!(db.propose_goal("NIT", &no_outcome, None, &agent()).is_err());
        assert!(db.propose_goal("NIT", &plan(), None, &Actor::Human).is_err());
        assert!(db.list_issues(Some("NIT"), true).unwrap().is_empty(), "nothing filed by a refused plan");

        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        db.move_issue(&p.issue.key, "Canceled", None, Some("not now"), &Actor::Human).unwrap();
        let v = db.goal_plan_for_issue(&p.issue.key).unwrap().unwrap();
        assert_eq!((v.status.as_str(), v.outcome.as_deref()), ("rejected", Some("not now")));
        assert_eq!(db.list_issues(Some("NIT"), true).unwrap().len(), 1);
        assert!(db.goals("NIT", true).unwrap().is_empty());
    }

    #[test]
    fn the_palette_files_a_planning_issue_that_the_plan_closes_and_existing_issues_join() {
        let mut db = db();
        let ask = db.start_goal("NIT", "  make Nitely installable by strangers ", &Actor::Human).unwrap();
        assert_eq!((ask.state.as_str(), ask.title.as_str()), ("Todo", "Plan a goal: make Nitely installable by strangers"));
        let old = db
            .create_issue(&NewIssue { project: "NIT".into(), title: "Old README".into(), body: "b".into(), kind: "bug".into(), source: "human".into(), evidence: "e".into(), ..Default::default() }, &Actor::Human)
            .unwrap();
        let mut p = plan();
        p.existing = vec![old.key.clone()];
        let proposed = db.propose_goal("NIT", &p, Some(&ask.key), &agent()).unwrap();
        assert_eq!(db.issue(&ask.key).unwrap().unwrap().state, "Done", "the planning issue's work is the draft");
        // Approving from the issue panel (Approve) approves the plan as drafted.
        db.move_issue(&proposed.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        let goal = db.goals("NIT", false).unwrap().remove(0);
        assert_eq!(goal.progress.total, 4);
        assert_eq!(db.issue(&old.key).unwrap().unwrap().goal, Some(goal.id));
    }
}
