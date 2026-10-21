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
    /// The files it will change, so Crew keeps parallel workers out of each other's files before
    /// either has a branch (docs/coordinator.md, decision 3). Written into the filed issue's body,
    /// where the overlap check reads paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touches: Vec<String>,
    /// Why this agent work changes no file in the repo (a design made in an outside tool,
    /// research), in place of `touches`. Shown on the filed issue; Crew counts it as colliding with
    /// nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_repo_changes: Option<String>,
    /// S, M or L: an hour or two, a day, more than a day of one agent's work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// inert, contained, standard or sensitive. Omitted new issues default to standard when filed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_level: Option<String>,
    /// Set when a path rule raised `review_level`. Recorded when the issue is filed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
}

/// The label of a "Plan a goal" issue: `next_task` and Crew take it ahead of other work of the same
/// urgency, since a whole goal waits on it, and Crew gives its worker the planner's brief.
pub const PLANNING_LABEL: &str = "planning";

/// Whether `issue` asks for a goal plan.
pub fn is_planning(issue: &IssueRecord) -> bool {
    issue.labels.iter().any(|l| l.eq_ignore_ascii_case(PLANNING_LABEL))
}

/// Where a plan comes from, as Nitely can show it rather than as the drafting agent says: its
/// `sentence` is the agent's own claim (NIT-550).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// It answers a "Plan a goal" issue the person created in the app.
    PersonAsked,
    /// Anything Nitely cannot show the person asked for: an agent's own proposal.
    AgentInitiative,
}

/// Whether a plan from `origin` starts at once under the review mode's `goal_plans`, rather than
/// waiting in the Inbox for the person's approval.
pub fn starts_by_policy(mode: crate::project_modes::GoalPlans, origin: Origin) -> bool {
    use crate::project_modes::GoalPlans;
    match mode {
        GoalPlans::Approve => false,
        GoalPlans::Asked => origin == Origin::PersonAsked,
        GoalPlans::All => true,
    }
}

/// Sizes a planned issue may have.
pub const SIZES: &[&str] = &["S", "M", "L"];

impl PlanItem {
    /// A live check or the person's own task changes no files; every other issue names them, or
    /// says why it changes none.
    fn changes_files(&self) -> bool {
        self.kind != "live-check" && self.owner.as_deref() != Some("human")
    }

    /// Why this item cannot be planned as it is, if it cannot.
    fn check(&self) -> Option<String> {
        let r = &self.local_ref;
        if let Some(s) = self.size.as_deref().filter(|s| !SIZES.contains(s)) {
            return Some(format!("{r}: size is S, M or L, not {s}"));
        }
        if let Some(reason) = &self.no_repo_changes {
            if !self.touches.is_empty() {
                return Some(format!("{r}: name the files it will change in touches, or say in no_repo_changes why it changes none, not both"));
            }
            if reason.trim().is_empty() {
                return Some(format!("{r}: no_repo_changes says why the work changes no file in the repo, e.g. \"designs the screens in the design tool\""));
            }
        } else if self.changes_files() && self.touches.is_empty() {
            return Some(format!(
                "{r}: name the files it will change in touches (paths like src/App.tsx), so Crew keeps parallel workers apart. Work that changes no file in the repo (a design in an outside tool, research) says why in no_repo_changes instead; a live check or the person's own task needs neither"
            ));
        }
        // Any file the repo has (Dockerfile, .gitignore, docs/Release Notes.md), but a path, not a
        // description: words with neither a folder nor an extension.
        let prose = |t: &str| t.is_empty() || t.contains('`') || (t.chars().any(char::is_whitespace) && !t.contains('/') && !t.contains('.'));
        let not_files: Vec<&str> = self.touches.iter().map(|t| t.trim()).filter(|t| prose(t)).collect();
        if !not_files.is_empty() {
            return Some(format!("{r}: touches names files by their path in the repo (src/App.tsx, Dockerfile), not {}", not_files.join(", ")));
        }
        if let Some(level) = self.review_level.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if crate::issues::review_rank(level).is_none() {
                return Some(format!("{r}: review_level is inert, contained, standard or sensitive, not {level}"));
            }
        }
        None
    }

    /// The body the filed issue gets: the drafted body, then the files and size Crew reads.
    fn filed_body(&self) -> String {
        let mut body = self.body.trim_end().to_string();
        if !self.touches.is_empty() {
            let files: Vec<String> = self.touches.iter().map(|t| format!("`{}`", t.trim())).collect();
            body.push_str(&format!("\n\nTouches: {}", files.join(", ")));
        }
        if let Some(reason) = &self.no_repo_changes {
            body.push_str(&format!("\n\n{} {}", crate::overlap::NO_REPO_CHANGES, reason.trim()));
        }
        if let Some(s) = &self.size {
            body.push_str(&format!("\nSize: {s}"));
        }
        body
    }
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
    /// Human edits to the action lists, keyed by local ref.
    #[serde(default)]
    pub bodies: std::collections::BTreeMap<String, String>,
}

/// A plan as the Inbox and the issue panel show it.
#[derive(Debug, Clone, Serialize)]
pub struct GoalPlanView {
    pub id: i64,
    pub issue_key: String,
    pub project: String,
    pub plan: GoalPlan,
    /// pending, approved, started (by the review mode, NIT-550) or rejected.
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
                body: i.filed_body(),
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
                review_level: i.review_level.clone(),
                review_source: i.review_level.as_ref().map(|_| "planner".to_string()),
                review_note: i.review_note.clone(),
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
        let size = i.size.as_deref().map(|s| format!(", {s}")).unwrap_or_default();
        let level = i.review_level.as_deref().map(|l| format!(", {} ({l})", crate::issues::review_depth(l))).unwrap_or_default();
        let touches = if i.touches.is_empty() { String::new() } else { format!("; touches {}", i.touches.join(", ")) };
        out.push_str(&format!("- `{}` {} [{}{p}{size}{level}]{blockers}{touches}\n", i.local_ref, i.title.trim(), i.kind));
    }
    if !plan.existing.is_empty() {
        out.push_str(&format!("\nExisting issues it would gather: {}\n", plan.existing.join(", ")));
    }
    out.push_str("\nApprove to create the goal and file these (you can edit first in the Inbox); cancel it with a reason to file nothing.");
    out
}

/// The notice a plan that started by itself leaves the person (NIT-550).
fn describe_started(plan: &GoalPlan, who: &str) -> String {
    let drafted = describe(plan, who);
    let body = drafted.rsplit_once("\n\nApprove to create").map(|(b, _)| b).unwrap_or(&drafted);
    format!(
        "Started: {}, from your words \"{}\". The review mode starts plans you asked for, so its goal and issues are filed already.\n\n{body}\n\nCancel this with a reason to drop the goal and cancel its issues not yet started; mark it Done to keep it.",
        plan.title.trim(),
        plan.sentence.trim()
    )
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
            if let Some(body) = edit.bodies.get(&i.local_ref) { i.body = body.trim().to_string(); }
        }
        plan
    }
}

/// How long an approval holds its plan. One that has not finished by then was interrupted (the
/// app quit midway), and the plan is pending again: approving it resumes with what was created.
const APPROVAL_LEASE_SECS: i64 = 120;

/// What a planning issue asks of the agent that takes it (a crew worker, or any agent).
pub const PLANNER: &str = "Draft it with `propose_goal`, passing this issue as planning_issue. Nothing else: no code changes, no commit, no submit.

1. Read the project's brief (`get_project`), then the code the goal is about, enough to name the files each step changes.
2. The goal: a short title, and a one-paragraph outcome that says what is true once it is reached and how anyone can tell. A target date only if the person gave one.
3. The issues, in the order to work them (Crew starts them in this order, as blockers and free workers allow). Each one is a change one agent can land on its own, with evidence and blockers, a size (S: an hour or two, M: a day, L: more; split an L when you can), and `touches`: the files it will change (work that changes no file in the repo, such as a design in an outside tool, says why in `no_repo_changes` instead). Two issues that touch the same files cannot run at once, so split by file where the work allows, and say so with blockers where it does not. Each one also declares a `review_level`: inert (prose, comments or display copy), contained (one component), standard (the usual change) or sensitive (approval, auth, migrations, landing, release). Leave it off only when you cannot tell. Path rules can only raise it.
4. Existing open issues of the project that belong in the goal (`list_issues`): pass them as existing rather than filing them again.
5. Keep the person's part to what only they can do (a decision, an account, a live check on their desktop), and say it plainly in that issue.

Do not file the issues yourself: the person approves the plan in the Inbox, and that files them. If the sentence is too unclear to plan, ask (the person answers in the Inbox) instead of guessing.";

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
        if let Some(why) = plan.issues.iter().find_map(PlanItem::check) {
            bail!("{why}");
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
        let mode = self.project_review_mode(&p.key)?;
        let policy = mode.options().goal_plans.unwrap_or(crate::project_modes::GoalPlans::Approve);
        let origin = self.plan_origin(planning.as_ref())?;
        let starts = starts_by_policy(policy, origin);

        let issue = self.create_issue(
            &NewIssue {
                project: p.key.clone(),
                title: if starts { format!("Started: {}", plan.title.trim()) } else { format!("Plan: {}", plan.title.trim()) },
                body: if starts { describe_started(plan, actor.id()) } else { describe(plan, actor.id()) },
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
                ..Default::default()
            },
            actor,
        )?;
        let mut stored = plan.clone();
        stored.planning_issue = planning.as_ref().map(|i| i.key.clone());
        self.conn.execute(
            "INSERT INTO goal_plan (issue_key, project, plan, status, created_by, created_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?5)",
            params![issue.key, p.key, serde_json::to_string(&stored)?, actor.id(), now()],
        )?;
        if let Some(planning) = planning.as_ref().filter(|i| i.is_open()) {
            let said = if starts {
                format!("Drafted the plan as {}; it started by itself, as the review mode says.", issue.key)
            } else {
                format!("Drafted the plan as {}; the person decides it in the Inbox.", issue.key)
            };
            self.move_issue(&planning.key, "Done", Some(&said), None, actor)?;
        }
        if starts {
            return self.start_by_policy(&issue.key, &mode, planning.as_ref().map(|i| i.key.as_str()), actor);
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
        let result = self.approve_claimed(key, &v, edit, actor, false);
        if result.is_err() {
            self.conn
                .execute("UPDATE goal_plan SET status = 'pending', decided_at = NULL WHERE id = ?1 AND status = 'approving'", [v.id])
                .ok();
        }
        result
    }

    /// `by_policy`: started by the review mode, not the person (NIT-550). The plan is `started`
    /// and its issue stays in Needs You as the person's notice, with Cancel.
    fn approve_claimed(&mut self, key: &str, v: &GoalPlanView, edit: &PlanEdit, actor: &Actor, by_policy: bool) -> Result<PlanApproved> {
        for (local_ref, body) in &edit.bodies {
            if !v.plan.issues.iter().any(|i| &i.local_ref == local_ref) || body.trim().is_empty() || body.len() > 50000 { bail!("invalid action-list edit: unknown ref, empty body or more than 50,000 bytes"); }
        }
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
            ..Default::default()
        };
        let goal = match resumed {
            Some(g) => self.save_goal(&v.project, Some(g.id), &change, actor)?,
            None => {
                // The goal and the plan's note of it commit together, so a retry always finds it;
                // immediate, so its reads cannot leave it unable to write (NIT-346).
                let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
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
        let (status, decided_by) = if by_policy { ("started", "policy".to_string()) } else { ("approved", actor.id().to_string()) };
        self.conn.execute(
            "UPDATE goal_plan SET status = ?7, outcome = ?2, goal_id = ?3, decided_by = ?4, decided_at = ?5, plan = ?6 WHERE id = ?1",
            params![v.id, outcome, goal.id, decided_by, now(), serde_json::to_string(&plan)?, status],
        )?;
        let goal = self.goal(goal.id)?.context("goal vanished")?;
        let issue = if by_policy { self.issue(key)?.context("the plan's issue is gone")? } else { self.finish_plan_issue(key, &outcome, actor)? };
        Ok(PlanApproved { goal, filed, attached, not_attached, issue })
    }

    /// What proves a plan was asked for: it answers a "Plan a goal" request the person made in the
    /// app, recorded by `start_goal` at that moment (goal_request). A label or title an agent adds
    /// later proves nothing, nor does who created the issue; anything else is an agent's initiative
    /// (NIT-550).
    pub(crate) fn plan_origin(&self, planning: Option<&IssueRecord>) -> Result<Origin> {
        let Some(planning) = planning else { return Ok(Origin::AgentInitiative) };
        let asked: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM goal_request WHERE issue_key = ?1 COLLATE NOCASE)", [&planning.key], |r| r.get(0))?;
        Ok(if asked { Origin::PersonAsked } else { Origin::AgentInitiative })
    }

    /// Starts a plan the review mode lets start: the goal and its issues are filed at once, as the
    /// drafting agent, and the plan's issue stays with the person as a notice they can cancel. Its
    /// timeline says which mode, at which revision, started it.
    fn start_by_policy(&mut self, key: &str, mode: &crate::project_modes::ProjectReviewMode, asked_in: Option<&str>, actor: &Actor) -> Result<PlanProposed> {
        let v = self.goal_plan_for_issue(key)?.context("the plan just filed is gone")?;
        self.conn.execute("UPDATE goal_plan SET status = 'approving', decided_at = ?2 WHERE id = ?1", params![v.id, now()])?;
        let started = match self.approve_claimed(key, &v, &PlanEdit::default(), actor, true) {
            Ok(s) => s,
            Err(e) => {
                // It could not start (a duplicate filed meanwhile, say): it waits for the person.
                self.conn.execute("UPDATE goal_plan SET status = 'pending', decided_at = NULL WHERE id = ?1", [v.id])?;
                self.log_event(actor, Some(key), None, "goal_plan_started", &format!("Could not start by itself ({e:#}); it waits for your approval"))?;
                return Err(e);
            }
        };
        let label = serde_json::to_value(mode.mode)?.as_str().unwrap_or_default().to_string();
        let asked = asked_in.map(|k| format!("; you asked for it in {k}")).unwrap_or_default();
        self.log_event(
            actor,
            Some(key),
            None,
            "goal_plan_started",
            &format!(
                "Started by policy: the {label} review mode (revision {}) starts plans of this kind{asked}. Goal {} \"{}\"",
                mode.revision, started.goal.id, started.goal.title
            ),
        )?;
        Ok(PlanProposed {
            url: started.issue.url.clone(),
            next: format!(
                "Started at once, as the review mode says: goal {} with its issues filed. {key} stays in the person's Inbox as a notice; canceling it drops the goal and its issues not yet started.",
                started.goal.id
            ),
            issue: started.issue,
        })
    }

    /// The person cancels a plan that started by itself: its goal is dropped and its issues not yet
    /// started are canceled, as rejecting the plan would have filed nothing. Work already under way
    /// is left as it is.
    fn cancel_started(&mut self, v: &GoalPlanView, reason: &str, actor: &Actor) -> Result<()> {
        if let Some(goal) = v.goal_id {
            for i in self.list_issues(Some(&v.project), false)?.into_iter().filter(|i| i.goal == Some(goal)) {
                if matches!(i.state_type.as_str(), "unstarted" | "backlog") && i.key != v.issue_key {
                    self.move_issue(&i.key, "Canceled", None, Some(&format!("Its goal's plan was canceled: {reason}")), actor)?;
                }
            }
            self.save_goal(&v.project, Some(goal), &GoalChange { state: Some("dropped".into()), ..Default::default() }, actor)?;
        }
        self.conn.execute(
            "UPDATE goal_plan SET status = 'rejected', outcome = ?2, decided_by = ?3, decided_at = ?4 WHERE id = ?1",
            params![v.id, format!("Canceled after it started: {reason}"), actor.id(), now()],
        )?;
        Ok(())
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
        // A plan that started by itself: canceling its notice undoes it; any other move keeps it.
        if let Some(v) = self.goal_plan_for_issue(key)?.filter(|v| v.status == "started") {
            if matches!(state, "Canceled" | "Duplicate") {
                let reason = reason.map(str::trim).filter(|r| !r.is_empty()).unwrap_or("no reason given");
                self.cancel_started(&v, reason, actor)?;
            }
            return Ok(None);
        }
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
        let issue = self.create_issue(
            &NewIssue {
                project: project.to_string(),
                title: format!("Plan a goal: {short}"),
                body: format!(
                    "The person wants: \"{sentence}\"\n\n{PLANNER}"
                ),
                kind: "feature".into(),
                source: if *actor == Actor::Human { "human".into() } else { source_of(actor) },
                evidence: "Asked for from the command palette.".into(),
                priority: 2,
                allow_similar: true,
                labels: vec!["goal".into(), PLANNING_LABEL.into()],
                ..Default::default()
            },
            actor,
        )?;
        // The person's own request is what lets the review mode start its plan (NIT-550).
        if *actor == Actor::Human {
            self.conn.execute("INSERT OR IGNORE INTO goal_request (issue_key, asked_at) VALUES (?1, ?2)", params![issue.key, now()])?;
        }
        Ok(issue)
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
            touches: vec![format!("src/{r}.rs")],
            size: Some("M".into()),
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

    /// What starts by itself under each goal_plans setting (NIT-550).
    #[test]
    fn a_plan_starts_by_policy_only_when_the_mode_allows_its_origin() {
        use crate::project_modes::GoalPlans::*;
        for (mode, asked, initiative) in [(Approve, false, false), (Asked, true, false), (All, true, true)] {
            assert_eq!(starts_by_policy(mode, Origin::PersonAsked), asked, "{mode:?}, asked for");
            assert_eq!(starts_by_policy(mode, Origin::AgentInitiative), initiative, "{mode:?}, an agent's own");
        }
    }

    /// Fast starts a plan answering the person's own "Plan a goal" issue, leaves them a notice, and
    /// canceling the notice drops the goal and its issues not yet started. An agent's own plan, or
    /// one answering a planning issue an agent filed, waits as before.
    #[test]
    fn under_fast_a_plan_the_person_asked_for_starts_and_canceling_it_undoes_it() {
        use crate::project_modes::{set_for_test, Mode, Options};
        let mut db = db();
        set_for_test(&mut db, "NIT", Mode::Fast, Options::default());

        let asked = db.start_goal("NIT", "make Nitely installable by strangers", &Actor::Human).unwrap();
        let p = db.propose_goal("NIT", &plan(), Some(&asked.key), &agent()).unwrap();
        assert_eq!((p.issue.state.as_str(), p.issue.owner.as_str()), ("Needs You", "human"), "the notice is the person's");
        assert!(p.issue.title.starts_with("Started: "), "{}", p.issue.title);
        let v = db.goal_plan_for_issue(&p.issue.key).unwrap().unwrap();
        assert_eq!(v.status, "started");
        let goal = v.goal_id.expect("the goal was created");
        let filed: Vec<_> = db.list_issues(Some("NIT"), false).unwrap().into_iter().filter(|i| i.goal == Some(goal)).collect();
        assert_eq!(filed.len(), 3, "its issues were filed at once");
        let said = db.timeline(&p.issue.key).unwrap();
        assert!(said.iter().any(|e| e.kind == "goal_plan_started" && e.summary.as_deref().unwrap_or("").contains("fast review mode (revision")), "{said:?}");

        // One item is under way; canceling the notice leaves it and cancels the rest.
        let started = filed[0].key.clone();
        db.move_issue(&started, "In Progress", None, None, &agent()).unwrap();
        db.move_issue(&p.issue.key, "Canceled", None, Some("not now"), &Actor::Human).unwrap();
        assert_eq!(db.goal(goal).unwrap().unwrap().state, "dropped");
        for i in &filed[1..] {
            assert_eq!(db.issue(&i.key).unwrap().unwrap().state, "Canceled", "{} was not started", i.key);
        }
        assert_eq!(db.issue(&started).unwrap().unwrap().state, "In Progress", "work under way stays");
        assert_eq!(db.goal_plan_for_issue(&p.issue.key).unwrap().unwrap().status, "rejected");

        // An agent's own plan, and one answering a planning issue an agent filed, wait.
        let mine = db.propose_goal("NIT", &GoalPlan { title: "Agent's idea".into(), ..plan() }, None, &agent()).unwrap();
        assert_eq!(db.goal_plan_for_issue(&mine.issue.key).unwrap().unwrap().status, "pending");
        let filed_by_agent = db.start_goal("NIT", "make Nitely installable by strangers", &agent()).unwrap();
        let theirs = db.propose_goal("NIT", &GoalPlan { title: "Their idea".into(), ..plan() }, Some(&filed_by_agent.key), &agent()).unwrap();
        assert_eq!(db.goal_plan_for_issue(&theirs.issue.key).unwrap().unwrap().status, "pending");
        // An issue the person filed for something else, labeled planning afterwards, is no request.
        let other = NewIssue { project: "NIT".into(), title: "Fix the export".into(), kind: "bug".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, labels: vec![PLANNING_LABEL.into()], ..Default::default() };
        let other = db.create_issue(&other, &Actor::Human).unwrap();
        let relabeled = db.propose_goal("NIT", &GoalPlan { title: "Borrowed request".into(), ..plan() }, Some(&other.key), &agent()).unwrap();
        assert_eq!(db.goal_plan_for_issue(&relabeled.issue.key).unwrap().unwrap().status, "pending", "only start_goal's own record proves a request");
    }

    #[test]
    fn a_plan_files_nothing_until_approved_then_files_everything_into_its_goal() {
        let mut db = db();
        let p = db.propose_goal("NIT", &plan(), None, &agent()).unwrap();
        assert_eq!((p.issue.state.as_str(), p.issue.owner.as_str()), ("Needs You", "human"));
        assert!(p.issue.body.contains("`sign` Sign the Windows installer [feature, P2, M] (after ci); touches src/sign.rs"), "{}", p.issue.body);
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

    #[test]
    fn every_planned_change_names_its_files_and_the_filed_issue_carries_them() {
        let mut db = db();
        let mut p = plan();
        p.issues[1].touches.clear();
        let e = db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string();
        assert!(e.contains("sign: name the files it will change") && e.contains("no_repo_changes"), "code work still names its files, and the refusal names the way out: {e}");
        p.issues[1].touches = vec!["the installer".into()];
        let e = db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string();
        assert!(e.contains("not the installer"), "{e}");
        p.issues[1].touches = vec!["src-tauri/tauri.conf.json".into(), "scripts/sign.ps1".into(), "Dockerfile".into(), "docs/Release Notes.md".into()];
        p.issues[1].size = Some("XL".into());
        assert!(db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string().contains("S, M or L"));
        p.issues[1].size = Some("L".into());
        p.issues[1].review_level = Some("nope".into());
        assert!(db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string().contains("review_level"));
        p.issues[1].review_level = Some("sensitive".into());
        // A live check and the person's own task change no files.
        let mut check = item("live", "Install it on a clean PC", &["sign"]);
        check.kind = "live-check".into();
        check.touches.clear();
        p.issues.push(check);
        let proposed = db.propose_goal("NIT", &p, None, &agent()).unwrap();
        db.move_issue(&proposed.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        let sign = db.list_issues(Some("NIT"), true).unwrap().into_iter().find(|i| i.title == "Sign the Windows installer").unwrap();
        assert!(sign.body.ends_with("Touches: `src-tauri/tauri.conf.json`, `scripts/sign.ps1`, `Dockerfile`, `docs/Release Notes.md`\nSize: L"), "{}", sign.body);
        assert_eq!((sign.review_level.as_deref(), sign.review_level_source.as_deref()), (Some("sensitive"), Some("planner")));
        assert_eq!(
            crate::overlap::touches_in(&sign.body),
            ["src-tauri/tauri.conf.json", "scripts/sign.ps1", "Dockerfile", "docs/Release Notes.md"],
            "the overlap check reads every one, extension or not"
        );
        // Declared files are held as they are, before any branch: another issue naming one waits.
        db.move_issue(&sign.key, "In Progress", None, None, &agent()).unwrap();
        let held = crate::overlap::held(&crate::Config::default(), &db, "NIT").unwrap();
        let files = &held.iter().find(|h| h.key == sign.key).unwrap().files;
        assert!(files.contains(&"Dockerfile".to_string()) && files.contains(&"docs/Release Notes.md".to_string()), "{files:?}");
    }

    /// Agent work done outside the repo (screens in a design tool) is planned by saying so, is
    /// shown with its reason, and holds no files for Crew (NIT-486).
    #[test]
    fn agent_work_that_changes_no_repo_files_says_why_and_collides_with_nothing() {
        let mut db = db();
        let mut p = plan();
        let mut design = item("design", "Design the install screens in the design tool", &[]);
        design.touches.clear();
        design.body = "Lay out src/Install.tsx's three steps as screens.".into();
        design.no_repo_changes = Some("  ".into());
        p.issues.insert(0, design);
        let e = db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string();
        assert!(e.contains("design: no_repo_changes says why"), "{e}");
        p.issues[0].touches = vec!["src/Install.tsx".into()];
        p.issues[0].no_repo_changes = Some("the screens are drawn in the design tool".into());
        assert!(db.propose_goal("NIT", &p, None, &agent()).unwrap_err().to_string().contains("not both"));
        p.issues[0].touches.clear();
        let proposed = db.propose_goal("NIT", &p, None, &agent()).unwrap();
        db.move_issue(&proposed.issue.key, "Approved", None, None, &Actor::Human).unwrap();
        let issues = db.list_issues(Some("NIT"), true).unwrap();
        let design = issues.iter().find(|i| i.title.starts_with("Design the install screens")).unwrap();
        assert!(design.body.ends_with("Changes no repo files: the screens are drawn in the design tool\nSize: M"), "{}", design.body);
        // In flight, it holds nothing, though its body names a file another issue changes.
        db.move_issue(&design.key, "In Progress", None, None, &agent()).unwrap();
        let held = crate::overlap::held(&crate::Config::default(), &db, "NIT").unwrap();
        assert!(held.iter().find(|h| h.key == design.key).unwrap().files.is_empty(), "{held:?}");
    }

    #[test]
    fn a_planning_issue_asks_for_a_plan_and_nothing_else() {
        let mut db = db();
        let ask = db.start_goal("NIT", "make Nitely installable by strangers", &Actor::Human).unwrap();
        assert!(ask.body.starts_with("The person wants: \"make Nitely installable by strangers\""));
        assert!(ask.body.contains("no code changes, no commit, no submit") && ask.body.contains("`touches`"), "{}", ask.body);
        assert!(is_planning(&ask));
    }

    #[test]
    fn a_planning_issue_goes_ahead_of_other_work_of_its_urgency_but_not_of_more_urgent_work() {
        let mut db = db();
        let filed = |db: &mut Db, title: &str, priority: i64| {
            db.create_issue(&NewIssue { project: "NIT".into(), title: title.into(), body: "b".into(), kind: "bug".into(), source: "human".into(), evidence: "e".into(), priority, ..Default::default() }, &Actor::Human)
                .unwrap()
        };
        let older = filed(&mut db, "An older P2 bug", 2);
        let urgent = filed(&mut db, "A data-loss bug", 1);
        let ask = db.start_goal("NIT", "make Nitely installable by strangers", &Actor::Human).unwrap();
        let first = db.next_task("NIT", "claude:a").unwrap().unwrap();
        assert_eq!(first.key, urgent.key, "priority 1 still goes first");
        let second = db.next_task("NIT", "claude:b").unwrap().unwrap();
        assert_eq!(second.key, ask.key, "a whole goal waits on the plan");
        assert_eq!(db.next_task("NIT", "claude:c").unwrap().unwrap().key, older.key);
    }
}
