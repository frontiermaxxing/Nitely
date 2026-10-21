//! `land`: the only writer of the integration branch. One land at a time, under a lock in the
//! repo's common git dir. The branch is rebased onto the integration branch in its worktree, its
//! review and approval are checked against exactly what it now changes, the gate runs there, and
//! the main checkout is fast-forwarded to the checked commit and pushed. A rebase conflict stops
//! the land with the rebase left in progress for the branch's owner.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::playbook::{self, git, git_remote, GateResult, Playbook};

/// A lock older than this probably belongs to a land that died.
const LOCK_STALE_SECS: u64 = 60 * 60;
pub const LOCK_FILE: &str = "nitely-land.lock";
/// A git `index.lock` made this long before a land began may still be that git's, running.
pub(crate) const INDEX_LOCK_GRACE_SECS: u64 = 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Landed {
    pub issue: String,
    pub branch: String,
    pub base: String,
    pub landed: bool,
    /// The integration branch's new head, short.
    pub head: Option<String>,
    pub commits: Vec<String>,
    /// Why no human approval was needed, or which approval let it land.
    pub approval: Option<String>,
    pub gate: Option<GateResult>,
    pub pushed: Option<bool>,
    pub warnings: Vec<String>,
    /// Files left conflicted by a rebase that stopped.
    pub conflicts: Vec<String>,
    pub cleanup: Option<String>,
    /// Processes that kept the worktree from being removed (a dev server, a build, a shell).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held_by: Vec<crate::procs::Holder>,
    /// Worktrees earlier lands had to keep, removed by this one now that nothing uses them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tidied: Vec<String>,
    /// The playbook's `[land] after` steps, run in the main checkout once it landed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<playbook::CommandResult>,
    pub error: Option<String>,
    /// The integration branch's head before this land, to tell what landed.
    #[serde(skip)]
    pub(crate) landed_from: Option<String>,
    /// Issues that waited on this one and have no open blocker left.
    pub unblocked: Vec<String>,
    /// Landed (or sent back) through the land queue: the run and what it went with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<crate::land_queue::BatchNote>,
    /// The goal branch it landed into, for an item of a goal with one (goal_branch.rs); None for a
    /// land on the integration branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_branch: Option<String>,
    /// What the land did to goal branches: main merged into them, one turned red, a checkpoint
    /// opened or landed, a bisect.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub goals: Vec<String>,
    /// The new head in full, for the goal branch's record of its items.
    #[serde(skip)]
    pub(crate) landed_to: Option<String>,
}

pub struct LandOptions<'a> {
    /// Where the land was asked from. The repo linked to the issue's project wins; this directory's
    /// repo counts when none is (see `playbook::repo_for_issue`).
    pub dir: &'a str,
    pub issue_key: &'a str,
    /// Push the integration branch to origin (and delete the landed remote branch).
    pub push: bool,
    /// Keep the task's worktree and branch after landing.
    pub keep: bool,
    /// Take over a lock left by a land that died.
    pub break_lock: bool,
}

pub fn land(db: &mut Db, config: &crate::Config, opts: &LandOptions, actor: &Actor) -> Result<Landed> {
    let issue = db.issue(opts.issue_key)?.with_context(|| format!("no issue {}", opts.issue_key))?;
    let root = playbook::repo_for_issue(config, issue.project.as_deref(), opts.dir)?;
    let mut pb = Playbook::load(&root)?;
    crate::project_modes::apply_issue(db, &issue.key, &mut pb)?;
    // This land is the issue's land now: a `land --after` job still waiting for it ends (NIT-513).
    // A job's own land runs with its row `landing`, so it is not one of them.
    crate::land_job::end_waiting(db, &issue.key, "landed directly instead")?;
    // An item of a goal with a goal branch lands into that branch, with item checks (goal_branch.rs).
    if let Some(gb) = db.goal_branch_of_issue(&issue.key)?.filter(|g| g.repo == crate::paths::normalize(&root.to_string_lossy())) {
        return land_into_goal(db, &root, &issue, opts, actor, gb);
    }
    // With `[land] batch`, a change that may land with others waits in the queue (land_queue.rs).
    // One that is not to be pushed lands alone: a stack is pushed as one.
    if opts.push && crate::land_queue::lands_alone(db, &root, &pb, &issue.key)?.is_none() {
        return crate::land_queue::land(db, &root, &issue.key, opts, actor);
    }
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project: issue.project.clone(), issue: Some(issue.key.clone()), ..Default::default() });
    let base = pb.integration_branch().to_string();
    let mut report = Landed { issue: issue.key.clone(), base: base.clone(), ..Default::default() };

    let lock = match Lock::take(&lock_path(&root)?, &issue.key, opts.break_lock) {
        Ok(lock) => lock,
        Err(e) => {
            // Refused because another land holds the lock: said on the issue, so the app can show
            // what this land waits behind until its agent lands again (NIT-335).
            let why = format!("{e:#}");
            // Held, or taken by another land between the look and the take: refused all the same.
            if why.starts_with("another land holds") || why.starts_with("another land took") {
                // The holder as the lock names it, so a lost race names the land that won it too.
                let holder = lock_path(&root).ok().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| t.lines().next().map(|l| l.trim().to_string()));
                let said = match holder {
                    Some(h) if !why.contains(&h) => format!("Land refused: {} (held by {h})", why.lines().next().unwrap_or("")),
                    _ => format!("Land refused: {}", why.lines().next().unwrap_or("")),
                };
                db.log_event(actor, Some(&issue.key), Some(&root.to_string_lossy()), "land_refused", &said)?;
            }
            return Err(e);
        }
    };
    if let Some(before) = &lock.took_over {
        report.warnings.push(if opts.break_lock {
            format!("broke the land lock {before} held")
        } else {
            format!("took over the land lock {before} left: the land that took it is gone")
        });
    }
    // Settings may have changed between the initial routing decision and taking the land lock.
    let mut pb = Playbook::load(&root)?;
    crate::project_modes::apply_issue(db, &issue.key, &mut pb)?;
    let result = reviewed_first(db, &root, &issue.key, actor).and_then(|()| run(db, &root, &pb, &issue.key, opts, actor, &mut report, None));
    // A checkpoint whose full gate failed: the item at fault is found, reverted on the goal branch
    // and sent back, still under the lock (as the land queue bisects its stack).
    if let (Err(_), Some(gate)) = (&result, report.gate.clone()) {
        report.goals.extend(crate::goal_branch::after_failed_gate(db, &root, &issue.key, &gate, actor));
    }
    // Still under the lock, so the next land cannot change the main checkout mid-build; and, in
    // large-repo mode, with the checkout held from build requests.
    if result.is_ok() && report.landed {
        match crate::lanes::hold_checkout(&root, &pb) {
            Ok(_held) => after_land(&root, &pb, &mut report),
            Err(e) => report.warnings.push(format!("the after-land steps did not run: {e:#}")),
        }
    }
    // Changes that queued while this land held the lock land now, under it: their own lands may
    // have stopped waiting. Not after a land kept local (--no-push): their push would publish it.
    drop(_telemetry);
    let kept_local = !opts.push && playbook::has_origin(&root);
    let mut queue_ran = false;
    if !kept_local && crate::land_queue::has_waiting(db, &root).unwrap_or(false) {
        queue_ran = true;
        if let Err(e) = crate::land_queue::run_queue(db, &root) {
            report.warnings.push(format!("the land queue that waited behind this land stopped: {e:#}"));
        }
    }
    report.tidied = tidy(db, &root, &pb, actor, &mut report.warnings);
    drop(lock);
    // The steps that need no lock, now that the next land may go (NIT-334): whenever the
    // integration branch may have moved, by this land or the queue it ran.
    if (result.is_ok() && report.landed) || queue_ran {
        let ran = after_land_outside_lock(&root, &pb);
        report.warnings.extend(ran.iter().filter(|r| !r.ok).map(|r| format!("the after-land step `{}` failed; the land stands", r.command)));
        report.after.extend(ran);
        // Main moved: it flows into each open goal branch (goal_branch.rs).
        report.goals.extend(crate::goal_branch::flow_main_in(db, &root, actor));
    }

    match result {
        Ok(()) => record_landed(db, &root, &issue, &mut report, actor)?,
        Err(e) => record_stopped(db, &issue.key, &format!("{e:#}"), &mut report, actor)?,
    }
    Ok(report)
}

/// The queue already owns the repository lock; a changed mode can require an individual land.
pub(crate) fn queued_alone(db: &mut Db, root: &Path, key: &str, opts: &LandOptions, actor: &Actor) -> Result<Landed> {
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    let mut pb = Playbook::load(root)?;
    crate::project_modes::apply_issue(db, key, &mut pb)?;
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project: issue.project.clone(), issue: Some(key.into()), ..Default::default() });
    let mut report = Landed { issue: key.into(), base: pb.integration_branch().into(), ..Default::default() };
    match reviewed_first(db, root, key, actor).and_then(|()| run(db, root, &pb, key, opts, actor, &mut report, None)) {
        Ok(()) => {
            if report.landed {
                match crate::lanes::hold_checkout(root, &pb) {
                    Ok(_held) => after_land(root, &pb, &mut report),
                    Err(e) => report.warnings.push(format!("the after-land steps did not run: {e:#}")),
                }
            }
            record_landed(db, root, &issue, &mut report, actor)?;
        }
        Err(e) => record_stopped(db, key, &format!("{e:#}"), &mut report, actor)?,
    }
    Ok(report)
}

/// A change whose review waits for its land batch, landing by itself: reviewed now, as a batch of
/// one (review_batch.rs). Stops the land when the review asks for changes or a person must approve.
fn reviewed_first(db: &mut Db, root: &Path, key: &str, actor: &Actor) -> Result<()> {
    for (_, outcome) in crate::review_batch::review_pending(db, root, &[key.to_string()], actor)? {
        outcome.map_err(|why| anyhow::anyhow!(why))?;
    }
    Ok(())
}

/// `land` for an item of a goal with a goal branch: the same `run`, into the goal branch, with
/// item checks, under the goal's own lock. Main's lands and its queue never wait on it, nor it on
/// them: it moves only the goal branch, never the main checkout.
fn land_into_goal(db: &mut Db, root: &Path, issue: &IssueRecord, opts: &LandOptions, actor: &Actor, gb: crate::goal_branch::GoalBranch) -> Result<Landed> {
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project: issue.project.clone(), issue: Some(issue.key.clone()), ..Default::default() });
    let mut report = Landed { issue: issue.key.clone(), base: gb.branch.clone(), goal_branch: Some(gb.branch.clone()), ..Default::default() };
    let lock = Lock::take(&crate::goal_branch::lock_path(root, gb.goal)?, &issue.key, opts.break_lock)?;
    if let Some(before) = &lock.took_over {
        report.warnings.push(format!("took over the lock of {} that {before} left", gb.branch));
    }
    let pb = Playbook::load(root)?;
    let result = item_hop(db, root, &pb, issue, opts, actor, &gb, &mut report);
    drop(lock);
    match result {
        Ok(()) => record_landed(db, root, issue, &mut report, actor)?,
        Err(e) => record_stopped(db, &issue.key, &format!("{e:#}"), &mut report, actor)?,
    }
    Ok(report)
}

/// The item hop under the goal's lock: main merged into the goal branch first when it moved (or,
/// on a red branch, only the item that brings that main in), then `run` into it.
#[allow(clippy::too_many_arguments)]
fn item_hop(db: &mut Db, root: &Path, pb: &Playbook, issue: &IssueRecord, opts: &LandOptions, actor: &Actor, gb: &crate::goal_branch::GoalBranch, report: &mut Landed) -> Result<()> {
    if !pb.submodules.is_empty() {
        bail!("a repo with managed submodules lands goal items on {} for now; turn the goal's branch off", pb.integration_branch());
    }
    let (branch, _) = branch_of(root, &issue.key)?;
    let item_head = rev(root, &branch)?;
    let gb = crate::goal_branch::before_item_land(db, root, pb, gb, &issue.key, &item_head, actor)?;
    run(db, root, pb, &issue.key, opts, actor, report, Some(&gb))
}

/// The `[[land.after]]` steps' outcome on the issue, as a single land's or a land queue run's
/// (one run for the whole stack, said on each change it landed): one line for what ran and what was
/// skipped and why, and a `land_after_failed` event with the end of a failed step's output, which
/// Overview shows until a later land's steps pass (NIT-469). Nothing when no step is configured.
pub(crate) fn log_after_steps(db: &Db, actor: &Actor, key: &str, after: &[playbook::CommandResult]) -> Result<()> {
    if after.is_empty() {
        return Ok(());
    }
    let said: Vec<String> = after
        .iter()
        .map(|r| match (r.skipped, r.ok) {
            (true, _) => format!("skipped `{}` ({})", r.command, r.output.trim_start_matches("skipped: ")),
            (false, true) => format!("ran `{}` ({} s)", r.command, r.seconds),
            (false, false) => format!("`{}` failed after {} s", r.command, r.seconds),
        })
        .collect();
    let ran = after.iter().any(|r| !r.skipped);
    let kind = if after.iter().any(|r| !r.ok) { "land_after_failed" } else if ran { "land_after" } else { "status" };
    let mut summary = format!("After landing: {}", said.join("; "));
    for r in after.iter().filter(|r| !r.ok) {
        let tail: Vec<&str> = r.output.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(3)..].join(" / ");
        summary.push_str(&format!(". `{}`: {}", r.command, tail));
    }
    db.log_event(actor, Some(key), None, kind, &summary)
}

/// What a land that went through does to its issue: the landed event, Done with the gate as
/// evidence, and the issues it unblocked.
pub(crate) fn record_landed(db: &mut Db, root: &Path, issue: &IssueRecord, report: &mut Landed, actor: &Actor) -> Result<()> {
    if report.goal_branch.is_some() {
        return record_item_landed(db, issue, report, actor);
    }
    match crate::goal_branch::checkpoint_landed(db, root, &issue.key, actor) {
        Ok(said) => report.goals.extend(said),
        Err(e) => report.warnings.push(format!("the goal branch's checkpoint record needs recovery: {e:#}")),
    }
    if let Err(e)=crate::verification_backstop::on_landed(db,root,issue,report) {
        report.warnings.push(format!("post-merge verification record needs recovery: {e:#}"));
        db.log_event(actor,Some(&issue.key),None,"verification_record_failed",&format!("{e:#}"))?;
    }
    if let Some(plan)=report.gate.as_ref().and_then(|g|g.plan.as_ref()).filter(|p|matches!(p.effective.as_str(),"inert"|"basic"|"contained")) {
        let audit=(|| -> Result<()> {
            let (vendor,_)=crate::git::parse_branch(&report.branch);
            if let Some(candidate)=crate::verification::audit_candidate(root,issue,plan,vendor.as_deref().unwrap_or(actor.id()))? {
                let head=rev(root,report.head.as_deref().context("landed head missing")?)?;
                crate::verification_audit::landed(db,&candidate,&plan.base,&head,Some(plan.audit_percent))?;
            }
            Ok(())
        })();
        if let Err(e)=audit {
            report.warnings.push(format!("post-merge audit record needs recovery: {e:#}"));
            db.log_event(actor,Some(&issue.key),None,"verification_record_failed",&format!("{e:#}"))?;
        }
    }
    let base = report.base.clone();
    let evidence = format!(
        "Landed on {base} at {}; {} ({}).",
        report.head.as_deref().unwrap_or("?"),
        report.gate.as_ref().and_then(|g| g.skipped.as_deref()).unwrap_or("gate passed"),
        report.gate.as_ref().map(|g| g.results.iter().map(|r| r.command.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
    );
    db.log_event(actor, Some(&issue.key), None, "landed", &format!("Landed {} on {base} at {}", report.branch, report.head.as_deref().unwrap_or("?")))?;
    log_after_steps(db, actor, &issue.key, &report.after)?;
    if issue.origin == "nitely" && issue.state != "Done" {
        db.move_issue(&issue.key, "Done", Some(&evidence), None, actor)?;
    }
    for waiting in &issue.blocks {
        if db.issue(waiting)?.is_some_and(|i| i.blocked_by.is_empty()) {
            report.unblocked.push(waiting.clone());
        }
    }
    if let Err(e)=crate::periodic_review::on_landed(db,root,issue,report) {
        report.warnings.push(format!("periodic review record needs recovery: {e:#}"));
        db.log_event(actor,Some(&issue.key),None,"periodic_review_record_failed",&format!("{e:#}"))?;
    }
    Ok(())
}

/// `record_landed` for an item hop: its item checks as its verification evidence, the goal
/// branch's record of what it added (which may open the goal's checkpoint), the landed event and
/// Done. Main's backstop, audits and periodic review wait for the checkpoint, which is what lands
/// on main.
fn record_item_landed(db: &mut Db, issue: &IssueRecord, report: &mut Landed, actor: &Actor) -> Result<()> {
    let branch = report.goal_branch.clone().unwrap_or_default();
    if let Some(plan) = report.gate.as_ref().and_then(|g| g.plan.as_ref()) {
        crate::verification::record(db, issue, plan, actor)?;
    }
    if let (Some(gb), Some(from), Some(to)) = (db.goal_branch_of_issue(&issue.key)?, report.landed_from.clone(), report.landed_to.clone()) {
        match crate::goal_branch::item_landed(db, &gb, &issue.key, &from, &to, actor) {
            Ok(said) => report.goals.extend(said),
            Err(e) => report.warnings.push(format!("the goal branch's record of this item needs recovery: {e:#}")),
        }
    }
    let checks = report.gate.as_ref().map(|g| g.results.iter().filter(|r| !r.skipped).map(|r| r.command.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let evidence = format!("Landed on {branch} at {} with item checks ({checks}); it reaches main with the goal's next checkpoint.", report.head.as_deref().unwrap_or("?"));
    db.log_event(actor, Some(&issue.key), None, "landed", &format!("Landed {} on {branch} at {}", report.branch, report.head.as_deref().unwrap_or("?")))?;
    if issue.origin == "nitely" && issue.state != "Done" {
        db.move_issue(&issue.key, "Done", Some(&evidence), None, actor)?;
    }
    for waiting in &issue.blocks {
        if db.issue(waiting)?.is_some_and(|i| i.blocked_by.is_empty()) {
            report.unblocked.push(waiting.clone());
        }
    }
    Ok(())
}

/// What a land that stopped does to its issue: the event, and an unsigned approval back to the
/// person.
pub(crate) fn record_stopped(db: &mut Db, key: &str, msg: &str, report: &mut Landed, actor: &Actor) -> Result<()> {
    db.log_event(actor, Some(key), None, "land_failed", &format!("Land stopped: {}", msg.lines().next().unwrap_or("")))?;
    // An approval the app did not sign goes back to the person, so they can approve it again.
    if msg.contains(crate::approval::UNSIGNED) && db.issue(key)?.is_some_and(|i| i.state == "Approved") {
        db.move_issue(key, "Needs You", None, None, actor)?;
    }
    report.error = Some(msg.to_string());
    Ok(())
}

/// Whether a live land holds `root`'s land lock: its file names the process that took it. The
/// lanes keep that land a lane while it does (NIT-332). Read often, so a main checkout's lock is
/// found without asking git.
pub(crate) fn held_by_live_land(root: &Path) -> bool {
    let dot_git = root.join(".git");
    let path = if dot_git.is_dir() { dot_git.join(LOCK_FILE) } else { match lock_path(root) { Ok(p) => p, Err(_) => return false } };
    std::fs::read_to_string(&path).is_ok_and(|held| holder_alive(&held))
}

/// The repo's land lock file, in its common git dir.
pub fn lock_path(root: &Path) -> Result<PathBuf> {
    let common = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?.trim());
    Ok(common.join(LOCK_FILE))
}

/// The main checkout's git `index.lock`, when there is one.
fn index_lock(root: &Path) -> Option<PathBuf> {
    let dir = git(root, &["rev-parse", "--path-format=absolute", "--git-dir"]).ok()?;
    Some(PathBuf::from(dir.trim()).join("index.lock")).filter(|p| p.exists())
}

/// Why git cannot change the main checkout, when an `index.lock` there is older than this land's
/// lock. Git holds that file for one command only, so one from before the land began was left by a
/// git that ended without removing it (the machine went down mid-command), and git refuses every
/// change to the checkout while it is there (NIT-280). Before the gate, one from the minute before
/// the land began may be a git still running; once the fast-forward was refused, it is not.
pub(crate) fn left_index_lock(root: &Path, grace_secs: u64) -> Option<String> {
    // A killed land's own lock is cleared, not judged (NIT-341).
    clear_killed_fast_forward(root);
    let path = index_lock(root)?;
    let made = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    let began = std::fs::metadata(lock_path(root).ok()?).and_then(|m| m.modified()).ok()?;
    if !crate::land_rules::index_lock_left(made, began, std::time::Duration::from_secs(grace_secs)) {
        return None;
    }
    let at = made.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(format!(
        "{} is from {}, before this land began: a git command that ended without removing it left it, and git changes nothing in {} while it is there. If no git command is running there, delete that file and land again",
        path.display(),
        crate::issues::iso(at),
        root.display()
    ))
}

/// What a refused fast-forward of the main checkout says: an `index.lock` there for what it is,
/// else the likeliest cause.
pub(crate) fn refused_fast_forward(root: &Path) -> String {
    if let Some(why) = left_index_lock(root, 0) {
        return format!("the main checkout refused the fast-forward: {why}");
    }
    match index_lock(root) {
        Some(path) => format!("the main checkout refused the fast-forward: a git command is running there ({} exists); land again once it ends", path.display()),
        None => "the main checkout refused the fast-forward (do uncommitted edits there overlap the change?)".to_string(),
    }
}

/// Who Nitely lands as when it lands a change itself, not the agent that wrote it.
pub const AUTO_LANDER: &str = "nitely:auto-land";

/// Lands `key` for a playbook whose `[land] by` lands without the author: in submit, once the
/// review passes, and after a human approves it (`land_approved`).
pub fn land_now(db: &mut Db, config: &crate::Config, key: &str, dir: &str, push: bool, actor: &Actor) -> Result<Landed> {
    let opts = LandOptions { dir, issue_key: key, push, keep: false, break_lock: false };
    // Nitely's line, not the agent's: the agent may be on another issue by now, and a status line
    // under its id would say it is back on this one (NIT-273).
    db.log_event(&Actor::Agent(AUTO_LANDER.into()), Some(key), None, "status", "Landing it automatically, as the playbook says")?;
    land(db, config, &opts, actor)
}

/// Where an automatic land leaves the issue, and what to do next. A land that stopped (a rebase
/// conflict, a failed gate, a change since the review) sends the issue back to In Progress with
/// the reason; one that could not start (another land holds the lock) leaves it ready to land.
pub fn after_auto_land(db: &mut Db, key: &str, landed: &Result<Landed>, actor: &Actor) -> Result<(String, String)> {
    let state = |db: &Db| -> Result<String> { Ok(db.issue(key)?.map(|i| i.state).unwrap_or_default()) };
    match landed {
        Ok(r) if r.landed && !r.warnings.is_empty() => {
            // Nobody is watching an automatic land, so what went wrong after it goes on the issue.
            db.add_note(key, &format!("Landed automatically, with warnings: {}", r.warnings.join("; ")), "land", actor)?;
            Ok((state(db)?, format!("Nitely landed it on {}, with warnings: {}", r.base, r.warnings.join("; "))))
        }
        Ok(r) if r.landed => Ok((state(db)?, format!("Nitely landed it on {} as the playbook says; nothing is left to do.", r.base))),
        Ok(r) => {
            let why = r.error.clone().unwrap_or_else(|| "it did not land".into());
            db.add_note(key, &format!("The automatic land stopped: {}", why.lines().next().unwrap_or("")), "land", actor)?;
            // A gate that ran past the lane's time limit says nothing about the change: the issue
            // stays where it is, and nobody is sent to fix code that did not fail (NIT-310).
            if r.gate.as_ref().is_some_and(|g| g.timed_out().is_some()) {
                return Ok((state(db)?, format!("Nitely tried to land it and stopped: {why}\nNothing in the change is to fix: land it again.")));
            }
            let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
            // Sent back to Needs You for an unsigned approval: the person approves it again.
            if issue.origin == "nitely" && issue.state != "In Progress" && !why.contains(crate::approval::UNSIGNED) {
                db.move_issue(key, "In Progress", None, None, actor)?;
            }
            Ok((state(db)?, format!("Nitely tried to land it and stopped: {why}\nFix it, commit, and submit again.")))
        }
        Err(e) => Ok((state(db)?, format!("Nitely could not start the land ({e:#}); call land to land it."))),
    }
}

/// Whether the app lands a change once the person approves it: when the playbook says Nitely lands,
/// and always where only the app can check approvals (macOS, docs/approvals.md), since an agent's
/// land cannot there.
fn nitely_lands_approved(pb: &Playbook) -> bool {
    pb.land.by.by_nitely() || !crate::approval::agents_can_verify()
}

/// After a human approves `key`: when its project's playbook lands by Nitely, Nitely lands it
/// now. None when it is not Approved or the playbook leaves the land to the author. `dir` helps
/// find the repo when no single repo is linked to the issue's project.
pub fn land_on_approval(db: &mut Db, config: &crate::Config, key: &str, dir: &str) -> Result<Option<Landed>> {
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    if issue.state != "Approved" {
        return Ok(None);
    }
    let root = approval_repo(config, &issue, dir)?;
    if !nitely_lands_approved(&Playbook::load(&root)?) {
        return Ok(None);
    }
    let actor = Actor::Agent(AUTO_LANDER.into());
    let landed = land_now(db, config, key, &root.to_string_lossy(), true, &actor);
    after_auto_land(db, key, &landed, &actor)?;
    landed.map(Some)
}

/// Lands each Approved issue whose project's playbook lands by Nitely and whose approval covers
/// its latest review. The app runs this every few seconds, so an approval lands even when another
/// land held the lock at the time, or the app was closed since: a held lock only waits for the
/// next round. `reported` keeps the last error noted per issue, so a lasting one (no repo found,
/// a playbook that does not parse) goes on the issue once, not every round.
pub fn land_approved(db: &mut Db, config: &crate::Config, reported: &mut HashMap<String, String>) -> Result<Vec<Landed>> {
    let mut landed = Vec::new();
    let approved: Vec<IssueRecord> =
        db.list_issues(None, false)?.into_iter().filter(|i| i.state == "Approved" && i.origin == "nitely").collect();
    for issue in approved {
        let Some(at) = db.standing_approval(&issue.key)? else { continue };
        // Approved a moment ago and not signed yet: the app signs right after the move.
        if crate::db::now() - at < 10 && db.approval_unsigned_yet(&issue.key, at)? {
            continue;
        }
        let ready = approval_repo(config, &issue, "").and_then(|root| Ok((nitely_lands_approved(&Playbook::load(&root)?), root)));
        let result = match ready {
            Ok((false, _)) => continue,
            // Another land is running: a later round lands it. A lock that old is probably left by a
            // land that died, which only a person can tell, so it is reported (once).
            // A lock whose land is gone is taken over by this one.
            Ok((true, root)) => match lock_age(&root).filter(|_| !lock_path(&root).is_ok_and(|p| held_by_dead(&p))) {
                Some(age) if age > LOCK_STALE_SECS => Err(anyhow::anyhow!(
                    "the land lock in {} is over an hour old; if no land is running, land it with break_lock",
                    root.display()
                )),
                Some(_) => continue,
                None => land_on_approval(db, config, &issue.key, &root.to_string_lossy()),
            },
            Err(e) => Err(e),
        };
        match result {
            Ok(Some(r)) => {
                reported.remove(&issue.key);
                landed.push(r);
            }
            Ok(None) => {}
            Err(e) if format!("{e:#}").contains("another land holds") => {}
            Err(e) => {
                let msg = format!("Nitely could not land it after the approval: {e:#}");
                if reported.get(&issue.key) != Some(&msg) {
                    db.add_note(&issue.key, &msg, "land", &Actor::Agent(AUTO_LANDER.into()))?;
                    reported.insert(issue.key.clone(), msg);
                }
            }
        }
    }
    Ok(landed)
}

/// The repo an approved issue lands in: the one linked to its project, or the one `dir` is in.
/// When several repos hold the project's work, the only one with a branch that names the issue.
fn approval_repo(config: &crate::Config, issue: &IssueRecord, dir: &str) -> Result<PathBuf> {
    playbook::repo_for_issue(config, issue.project.as_deref(), dir).or_else(|e| {
        let project = issue.project.as_deref().ok_or_else(|| anyhow::anyhow!("{e:#}"))?;
        let names = |root: &Path| {
            git(root, &["branch", "--format=%(refname:short)"])
                .is_ok_and(|out| out.lines().any(|b| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(&issue.key))))
        };
        let with_branch: Vec<PathBuf> = config
            .repos
            .iter()
            .filter(|r| r.project.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(project)))
            .map(|r| PathBuf::from(&r.root))
            .filter(|root| names(root))
            .collect();
        match with_branch.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(e),
            _ => bail!("several repos linked to {project} have a branch naming {}; land it from the right one", issue.key),
        }
    })
}

/// Removes the repo's land lock when the process that took it is gone (the machine restarted
/// mid-land): its file says `<holder> pid <pid> since <time>`. Returns the holder it named. A lock
/// whose process still runs, or whose pid cannot be read, is left alone.
pub fn clear_dead_lock(root: &Path) -> Option<String> {
    clear_dead(&lock_path(root).ok()?)
}

/// `clear_dead_lock` for the lock file at `path`.
fn clear_dead(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let holder = crate::land_rules::dead_holder(&text, std::process::id(), crate::procs::alive)?;
    // Moved aside first, then checked: when the dead land's lock was released and a new land took
    // the lock in between, what was moved is the new land's, and it goes straight back.
    let aside = path.with_extension(format!("dead-{}", std::process::id()));
    std::fs::rename(path, &aside).ok()?;
    if std::fs::read_to_string(&aside).ok().as_deref() != Some(text.as_str()) {
        std::fs::rename(&aside, path).ok();
        return None;
    }
    std::fs::remove_file(&aside).ok();
    Some(holder)
}

/// How long the repo's land lock has been held, in seconds; None when no land holds it.
fn lock_age(root: &Path) -> Option<u64> {
    let common = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).ok()?;
    let modified = std::fs::metadata(PathBuf::from(common.trim()).join(LOCK_FILE)).ok()?.modified().ok()?;
    Some(modified.elapsed().map_or(0, |d| d.as_secs()))
}

/// The playbook's `[land] after` steps that match what landed. A failure is a warning. Then, when
/// a dependency file landed that no after step took care of, a warning that the main checkout's
/// setup is stale: land fast-forwards it but never runs setup there (NIT-81).
pub(crate) fn after_land(root: &Path, pb: &Playbook, report: &mut Landed) {
    // What landed, from the head before the fast-forward. When that cannot be told, every step
    // runs: skipping a needed one silently is worse than running one more.
    let files: Option<Vec<String>> = report
        .landed_from
        .as_deref()
        .and_then(|from| git(root, &["diff", "--name-only", "--no-renames", from, "HEAD"]).ok())
        .map(|out| out.lines().map(str::to_string).collect());
    let mut covered: Vec<&playbook::GateStep> = Vec::new();
    for step in &pb.land.after {
        let command = playbook::expand(&step.run, root, root, &report.base);
        match crate::land_rules::after_step(step, files.as_deref()) {
            crate::land_rules::AfterStep::Skip(why) => {
                report.after.push(playbook::CommandResult { command, ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true, timed_out: false });
                continue;
            }
            // Run once the lock is released (`after_land_outside_lock`); it covers what it names.
            crate::land_rules::AfterStep::Outside => {
                covered.push(step);
                continue;
            }
            crate::land_rules::AfterStep::Run => {}
        }
        let result = match playbook::run_shell_kind(&command, root, "install") {
            Ok((ok, output, seconds)) => playbook::CommandResult { command, ok, seconds, output, skipped: false, timed_out: false },
            Err(e) => playbook::CommandResult { command, ok: false, seconds: 0, output: format!("{e:#}"), skipped: false, timed_out: false },
        };
        if result.ok {
            covered.push(step);
        } else {
            report.warnings.push(format!("the after-land step `{}` failed; the land stands", result.command));
        }
        report.after.push(result);
    }

    if pb.worktree.setup.is_empty() {
        return;
    }
    let Some(files) = files else { return };
    // A dependency file counts as handled when an after step that names it by pattern ran and passed.
    let stale = crate::land_rules::stale_setup(playbook::setup_files_in(&pb.worktree, &files), &covered);
    if !stale.is_empty() {
        report.warnings.push(format!(
            "{} changed, and the main checkout {} has not run setup since; run `{}` there before using it (a [[land.after]] step can do it for you)",
            stale.join(", "),
            root.display(),
            pb.worktree.setup.iter().map(|c| playbook::expand(c, root, root, &report.base)).collect::<Vec<_>>().join(" && ")
        ));
    }
}

/// Removes the index.lock a land killed mid fast-forward left, with its mark: only when the mark's
/// process is gone and the lock was taken during that fast-forward (one taken long after is
/// someone else's). Run before anything judges a lock there, so a land after the killed one goes on.
pub(crate) fn clear_killed_fast_forward(root: &Path) {
    let Ok(git_dir) = git(root, &["rev-parse", "--absolute-git-dir"]) else { return };
    let git_dir = PathBuf::from(git_dir.trim());
    let (index_lock, mark) = (git_dir.join("index.lock"), git_dir.join("nitely-fast-forward"));
    let Ok(text) = std::fs::read_to_string(&mark) else { return };
    let pid = text.split_whitespace().skip_while(|w| *w != "pid").nth(1).and_then(|p| p.parse::<u32>().ok());
    if pid.is_some_and(crate::procs::alive) {
        return;
    }
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(since), Some(t)) = (modified(&mark), modified(&index_lock)) {
        if crate::land_rules::killed_fast_forwards_lock(since, t, FF_SLACK, FF_TAKES) {
            std::fs::remove_file(&index_lock).ok();
        }
    }
    std::fs::remove_file(&mark).ok();
}

/// The main checkout's fast-forward to `to`. It writes that checkout's index, so a land killed
/// mid-way would leave index.lock and every git command there would fail. A file naming this
/// process goes beside the index first; a later land that finds index.lock with such a file whose
/// process is gone knows the lock is a killed land's, and removes both (NIT-341). A lock with no
/// such file (the person's own git, another tool) is left alone.
pub(crate) fn fast_forward_main(root: &Path, to: &str) -> Result<()> {
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"])?.trim());
    let (index_lock, mark) = (git_dir.join("index.lock"), git_dir.join("nitely-fast-forward"));
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    clear_killed_fast_forward(root);
    let started = std::time::SystemTime::now();
    std::fs::write(&mark, format!("fast-forward pid {} since {}", std::process::id(), crate::issues::iso(now()))).ok();
    let done = git(root, &["merge", "--ff-only", to]);
    // Our git has ended: a lock it took and did not let go (it was killed) is stale now. Not one
    // it could not take because another process held it ("File exists"): that one is theirs.
    let refused_by_lock = done.as_ref().err().is_some_and(|e| format!("{e:#}").contains("index.lock") && format!("{e:#}").contains("File exists"));
    if done.is_err() && !refused_by_lock && modified(&index_lock).is_some_and(|t| t >= started && t <= started + FF_TAKES) {
        std::fs::remove_file(&index_lock).ok();
    }
    std::fs::remove_file(&mark).ok();
    // What a refusal means for the person, a lock left there included: `refused_fast_forward`.
    done.map(|_| ())
}

/// How long a fast-forward of the main checkout may take, for telling its lock from a later one.
const FF_TAKES: std::time::Duration = std::time::Duration::from_secs(120);
/// File times are coarse on some file systems: a lock may read as a little older than the mark.
const FF_SLACK: std::time::Duration = std::time::Duration::from_secs(2);

/// The `[[land.after]]` steps marked `outside_lock`, run once the land lock is released
/// (NIT-334). They run in a checkout of the integration branch kept for them beside the lanes, so
/// the next land's fast-forward of the main checkout never changes what they build, and one
/// land's at a time, under a lock of their own. A land that finishes while another's run leaves a
/// mark and returns at once; the running one goes round again at the newest commit, so lands
/// close together build once, not once each. What each round runs follows from what changed since
/// the steps last ran, whoever landed it (a land queue's changes too).
pub(crate) fn after_land_outside_lock(root: &Path, pb: &Playbook) -> Vec<playbook::CommandResult> {
    let steps: Vec<&playbook::GateStep> = pb.land.after.iter().filter(|s| s.outside_lock).collect();
    if steps.is_empty() {
        return vec![];
    }
    let (Ok(lock), Ok(pending)) = (after_lock_path(root), after_pending_path(root)) else { return vec![] };
    let mut out = Vec::new();
    loop {
        let held = match take_after_lock(&lock) {
            Some(held) => held,
            None => {
                // The mark first, then one more try: the holder looks for marks after it lets go,
                // so either it sees this one or this land finds the lock free.
                std::fs::write(&pending, "").ok();
                match take_after_lock(&lock) {
                    Some(held) => held,
                    None => {
                        out.push(playbook::CommandResult {
                            command: steps.iter().map(|s| s.run.as_str()).collect::<Vec<_>>().join("; "),
                            ok: true,
                            seconds: 0,
                            output: "another land is running these steps; it runs them again at the newest commit once it is done".into(),
                            skipped: true,
                            timed_out: false,
                        });
                        return out;
                    }
                }
            }
        };
        // Until no land has left a mark: each round builds whatever landed before it started.
        loop {
            std::fs::remove_file(&pending).ok();
            match after_round(root, pb, &steps) {
                Ok(ran) => out.extend(ran),
                Err(e) => out.push(playbook::CommandResult { command: "preparing the after-land checkout".into(), ok: false, seconds: 0, output: format!("{e:#}"), skipped: false, timed_out: false }),
            }
            if !pending.exists() {
                break;
            }
        }
        drop(held);
        // A land that left its mark between the last look and the release.
        if !pending.exists() {
            return out;
        }
    }
}

/// One round of the outside-the-lock steps: their checkout moved to the integration branch's
/// newest commit, and each step that what changed since the last round calls for.
fn after_round(root: &Path, pb: &Playbook, steps: &[&playbook::GateStep]) -> Result<Vec<playbook::CommandResult>> {
    let base = pb.integration_branch();
    let at = git(root, &["rev-parse", base])?.trim().to_string();
    let tree = after_tree_path(pb, root);
    let before = if tree.join(".git").exists() { git(&tree, &["rev-parse", "HEAD"]).ok().map(|h| h.trim().to_string()) } else { None };
    if before.is_none() {
        git(root, &["worktree", "prune"]).ok();
        if tree.exists() {
            std::fs::remove_dir_all(&tree).with_context(|| format!("clearing {}", tree.display()))?;
        }
        git(root, &["worktree", "add", "--detach", &tree.to_string_lossy(), &at]).context("creating the after-land checkout")?;
    } else if before.as_deref() != Some(at.as_str()) {
        git(&tree, &["checkout", "--force", "--detach", &at]).context("moving the after-land checkout")?;
        git(&tree, &["clean", "-ffdq"]).ok();
    }
    // What changed since the steps last ran; unknown (a new checkout): every step runs.
    // Every commit's files since then, not the two ends' difference: a change and its revert in
    // between still count, as the stale-build check counts them; a merge, by what it brought to
    // the branch (against its first parent).
    let files: Option<Vec<String>> = before
        .filter(|b| b != &at)
        .and_then(|b| git(&tree, &["log", "--name-only", "--no-renames", "-m", "--first-parent", "--format=", &format!("{b}..{at}")]).ok())
        .map(|o| o.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect());
    let mut out = Vec::new();
    for step in steps {
        let command = playbook::expand(&step.run, root, &tree, base);
        let skip = files.as_deref().and_then(|f| step.skip_reason(f).ok().flatten());
        if let Some(why) = skip {
            out.push(playbook::CommandResult { command, ok: true, seconds: 0, output: format!("skipped: {why}"), skipped: true, timed_out: false });
            continue;
        }
        out.push(match playbook::run_shell_kind(&command, &tree, "install") {
            Ok((ok, output, seconds)) => playbook::CommandResult { command, ok, seconds, output, skipped: false, timed_out: false },
            Err(e) => playbook::CommandResult { command, ok: false, seconds: 0, output: format!("{e:#}"), skipped: false, timed_out: false },
        });
    }
    Ok(out)
}

/// The checkout the outside-the-lock steps run in.
pub(crate) fn after_tree_path(pb: &Playbook, root: &Path) -> PathBuf {
    crate::light::lanes_dir(pb, root).join("after")
}

fn after_lock_path(root: &Path) -> Result<PathBuf> {
    Ok(lock_path(root)?.with_file_name("nitely-after.lock"))
}

fn after_pending_path(root: &Path) -> Result<PathBuf> {
    Ok(lock_path(root)?.with_file_name("nitely-after.pending"))
}

/// Whether a lock file's text ("<holder> pid <pid> since <time>") names a process that still runs.
fn holder_alive(text: &str) -> bool {
    let mut words = text.split_whitespace().skip_while(|w| *w != "pid");
    words.nth(1).and_then(|p| p.parse::<u32>().ok()).is_some_and(crate::procs::alive)
}

/// The outside-the-lock steps' lock, held: an exclusive lock on a file that stays, which the system
/// lets go when its holder's process ends however it ends, so a land that died mid-build (the
/// machine restarted) holds nothing and nothing has to break it. Beside it, while held, a file
/// naming the holder, which `after_steps_running` reads without touching the lock itself.
struct AfterHold {
    _file: std::fs::File,
    running: PathBuf,
}

impl Drop for AfterHold {
    fn drop(&mut self) {
        std::fs::remove_file(&self.running).ok();
    }
}

/// The lock of the outside-the-lock steps, or None while another land holds it.
fn take_after_lock(path: &Path) -> Option<AfterHold> {
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    file.try_lock().ok()?;
    let running = path.with_extension("running");
    std::fs::write(&running, format!("after-land-steps pid {}", std::process::id())).ok();
    Some(AfterHold { _file: file, running })
}

/// Whether the outside-the-lock steps of a land are running now for `root` (an install of the
/// landed build under way): submit and land say so when they find the installed build behind.
pub fn after_steps_running(root: &Path) -> bool {
    after_lock_path(root).ok().and_then(|p| std::fs::read_to_string(p.with_extension("running")).ok()).is_some_and(|t| holder_alive(&t))
}

/// One land of `key` into `report.base`: the integration branch, or, with `goal`, the goal branch
/// an item of that goal lands into (goal_branch.rs). The two hops differ only in their checks and
/// how the target moves: an item gets item checks and no review, and moves the goal branch's ref;
/// a land on main checks its review and approval, runs the gate, and fast-forwards the main
/// checkout.
#[allow(clippy::too_many_arguments)]
fn run(db: &Db, root: &Path, pb: &Playbook, key: &str, opts: &LandOptions, actor: &Actor, report: &mut Landed, goal: Option<&crate::goal_branch::GoalBranch>) -> Result<()> {
    let base = report.base.clone();
    // A repo with no origin is local: what lands stays in it.
    let push = opts.push && playbook::has_origin(root);
    if goal.is_none() {
        // Large-repo mode: a build request may have the main checkout detached for a moment.
        let held = crate::lanes::hold_checkout(root, pb)?;
        let current = git(root, &["branch", "--show-current"])?.trim().to_string();
        drop(held);
        if current != base {
            bail!("the main checkout {} is on {current}, not {base}; land only writes {base}", root.display());
        }
        // Said before the gate, not after it, when the fast-forward would fail.
        if let Some(why) = left_index_lock(root, INDEX_LOCK_GRACE_SECS) {
            bail!("{why}");
        }
    }
    // A goal's checkpoint holds the shared goal branch, merges of main included: main is merged
    // into it, never rebased under it.
    let checkpoint = goal.is_none() && db.checkpoint_of_issue(key)?.is_some();

    let (branch, found) = branch_of(root, key)?;
    report.branch = branch.clone();

    let (worktree, temporary) = match found {
        Some(path) => (path, false),
        None => {
            let path = root.parent().unwrap_or(root).join(format!("{}-land-{}", file_name(root), key.to_lowercase()));
            git(root, &["worktree", "add", &path.to_string_lossy(), &branch])?;
            (path, true)
        }
    };
    let wt = worktree.to_string_lossy().into_owned();
    check_worktree(&worktree, &branch)?;

    let before = rev(root, &base)?;
    // Submodules that land with the repo: their work must be what the branch points at, then each
    // is rebased onto its own integration branch first (docs/playbooks.md).
    crate::submodules::check_pointers(pb, &worktree)?;
    let moving = crate::submodules::rebase(root, pb, &worktree, &base)?;
    if checkpoint && git(root, &["merge-base", "--is-ancestor", &base, &branch]).is_err() {
        db.log_event(actor, Some(key), Some(&wt), "status", &format!("Merging {base} into the checkpoint {branch}"))?;
        if git(&worktree, &["merge", "--no-ff", "--no-edit", "-m", &format!("Merge {base} into {branch}"), &base]).is_err() {
            report.conflicts = git(&worktree, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect();
            git(&worktree, &["merge", "--abort"]).ok();
            bail!(
                "{base} conflicts with the checkpoint {branch} in {}; main flows into the goal branch after each land on main, which files the fix on the goal. Land the checkpoint once the goal branch is green",
                report.conflicts.join(", ")
            );
        }
    } else if git(root, &["merge-base", "--is-ancestor", &base, &branch]).is_err() {
        db.log_event(actor, Some(key), Some(&wt), "status", &format!("Rebasing {branch} onto {base}"))?;
        let rebased = crate::submodules::rebase_repo(&worktree, &base, &moving);
        if let Err(e) = rebased {
            if !crate::test_command_merge::finish_rebase(&worktree).unwrap_or(false) {
                report.conflicts = git(&worktree, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect();
                if report.conflicts.is_empty() {
                    git(&worktree, &["rebase", "--abort"]).ok();
                    return Err(e).context(format!("rebasing {branch} onto {base}"));
                }
                bail!(
                    "the rebase of {branch} onto {base} stopped on conflicts in {}; it is left in progress in {wt}. \
                     Resolve each file, git add it, git rebase --continue, then submit and land again",
                    report.conflicts.join(", ")
                );
            }
        }
    }

    crate::submodules::point(&worktree, &moving)?;
    crate::submodules::follow(pb, &worktree, &moving)?;
    let head = rev(root, &branch)?;
    report.approval = Some(match goal {
        Some(gb) => check_item(pb, &worktree, &base, gb)?,
        None => check_approval(db, pb, key, &worktree, &base, &branch)?,
    });
    let project = db.issue(key)?.and_then(|i| i.project);
    crate::private_terms::check(project.as_deref(), &worktree, &base)?;

    // In a repo with build lanes, a land request: first in line, and answered from the record when
    // this same tree passed its gate already (a rebase that changed nothing). An item hop runs the
    // item checks instead.
    let gate = match goal {
        Some(gb) => crate::goal_branch::item_gate(db, root, &worktree, Some(key), &before, &gb.branch, actor)?,
        None => playbook::run_automatic_gate(db, root, &worktree, &[key], actor, crate::lanes::Kind::Land)?,
    };
    let passed = gate.passed || gate.skipped.is_some();
    report.gate = Some(gate);
    if !passed {
        if let Some(r) = report.gate.as_ref().and_then(|g| g.timed_out()) {
            bail!("the gate did not finish: `{}` ran past the lane's time limit and was stopped, which says nothing about {branch}; land again", r.command);
        }
        bail!("the gate failed; fix it on {branch}, submit, and land again");
    }
    if rev(root, &base)? != before {
        bail!("{base} moved during the gate; land again");
    }
    // What lands is exactly what was checked.
    if rev(root, &branch)? != head {
        bail!("{branch} gained commits during the gate, after its review was checked; submit again");
    }
    if let Some(why) = crate::submodules::unchanged(&worktree, &moving)? {
        bail!("{why}");
    }
    if let Some(gb) = goal {
        return land_item(db, root, pb, key, opts, actor, gb, &worktree, &branch, temporary, &before, &head, report);
    }
    crate::project_modes::check_gate_authority(db, &[key], report.gate.as_ref().expect("gate recorded"))?;
    crate::verification::check_gate_authority(db,root,&worktree,key,report.gate.as_ref().expect("gate recorded"))?;
    report.approval = Some(check_approval(db, &Playbook::load(root)?, key, &worktree, &base, &branch)?);

    if push {
        // The pull request should show the commits that land, not the ones from before the rebase:
        // the submodules' rebased task branches first, so its pointers resolve.
        let pushed = crate::submodules::push_task(pb, &worktree, &branch)
            .and_then(|_| git_remote(root, &["push", "--force-with-lease", "origin", &format!("{head}:refs/heads/{branch}")]).map(|_| ()));
        if let Err(e) = pushed {
            report.warnings.push(format!("the pull request may still show the pre-rebase commits ({})", first_line(&e)));
        }
    }

    report.commits = git(root, &["log", "--oneline", &format!("{base}..{head}")])?.lines().map(str::to_string).collect();
    let held = crate::lanes::hold_checkout(root, pb)?;
    if held.is_some() && git(root, &["branch", "--show-current"])?.trim() != base {
        bail!("the main checkout left {base} during the gate; land again once it is back on {base}");
    }
    // The submodules' integration branches first, then the repo's; when the repo's refuses, theirs
    // go back, so neither moves.
    crate::project_modes::check_gate_authority(db, &[key], report.gate.as_ref().expect("gate recorded"))?;
    crate::verification::check_gate_authority(db,root,&worktree,key,report.gate.as_ref().expect("gate recorded"))?;
    check_yolo_policy(db, key, &db.reviews(key)?.into_iter().next().context("submission evidence disappeared")?)?;
    let undo = crate::submodules::fast_forward(&moving)?;
    if let Err(e) = fast_forward_main(root, &head) {
        let stuck = crate::submodules::undo(&undo);
        let back = if moving.is_empty() {
            String::new()
        } else if stuck.is_empty() {
            "; the submodules' branches were put back, so nothing moved".to_string()
        } else {
            format!("; {}", stuck.join("; "))
        };
        return Err(e).context(format!("{}{back}", refused_fast_forward(root)));
    }
    drop(held);
    report.landed = true;
    report.landed_from = Some(before.clone());
    report.head = Some(git(root, &["rev-parse", "--short", "HEAD"])?.trim().to_string());

    // A submodule's branch is pushed before the repo that points into it; when it cannot be, the
    // repo stays unpushed too, so its remote never points at a commit nobody can fetch.
    let push = if push && !pb.submodules.is_empty() {
        match crate::submodules::push(root, pb) {
            Ok(()) => true,
            Err(e) => {
                report.pushed = Some(false);
                report.warnings.push(format!("{base} landed locally but was not pushed: {}", first_line(&e)));
                false
            }
        }
    } else {
        push
    };
    if push {
        match git_remote(root, &["push", "origin", &base]) {
            Ok(_) => {
                report.pushed = Some(true);
                // The code host marks the pull request merged once its commits are on the base.
                // Deleted only while it still points at what landed.
                git_remote(root, &["push", &format!("--force-with-lease=refs/heads/{branch}:{head}"), "origin", &format!(":refs/heads/{branch}")]).ok();
            }
            Err(e) => {
                report.pushed = Some(false);
                report.warnings.push(format!("{base} landed locally but the push failed: {}", first_line(&e)));
            }
        }
    }

    clean_up_landed(db, root, pb, key, opts, actor, &worktree, &branch, temporary, report)?;
    // The base moved: so does the reference tree light worktrees read code from, when there is one.
    if crate::light::reference_path(pb, root).join(".git").exists() {
        if let Err(e) = crate::light::reference_tree(pb, root) {
            report.warnings.push(format!("the reference tree stayed at the old base: {e:#}"));
        }
    }
    Ok(())
}

/// The item hop's end, once its checks passed: the goal branch moved to what was checked and
/// pushed, and the item's worktree cleaned up as a land on main cleans up.
#[allow(clippy::too_many_arguments)]
fn land_item(db: &Db, root: &Path, pb: &Playbook, key: &str, opts: &LandOptions, actor: &Actor, gb: &crate::goal_branch::GoalBranch, worktree: &Path, branch: &str, temporary: bool, before: &str, head: &str, report: &mut Landed) -> Result<()> {
    report.approval = Some(check_item(&Playbook::load(root)?, worktree, &report.base, gb)?);
    report.commits = git(root, &["log", "--oneline", &format!("{before}..{head}")])?.lines().map(str::to_string).collect();
    let warning = crate::goal_branch::advance(root, gb, before, head)?;
    report.landed = true;
    report.landed_from = Some(before.to_string());
    report.landed_to = Some(head.to_string());
    report.head = Some(git(root, &["rev-parse", "--short", head])?.trim().to_string());
    if playbook::has_origin(root) {
        report.pushed = Some(warning.is_none());
        if opts.push {
            // A task branch the agent pushed itself goes, while it still points at what landed.
            git_remote(root, &["push", &format!("--force-with-lease=refs/heads/{branch}:{head}"), "origin", &format!(":refs/heads/{branch}")]).ok();
        }
    }
    report.warnings.extend(warning);
    clean_up_landed(db, root, pb, key, opts, actor, worktree, branch, temporary, report)
}

/// What a land does with the task's worktree once it landed: kept for a crew worker in it or with
/// `--keep`, else removed with its branch.
#[allow(clippy::too_many_arguments)]
fn clean_up_landed(db: &Db, root: &Path, pb: &Playbook, key: &str, opts: &LandOptions, actor: &Actor, worktree: &Path, branch: &str, temporary: bool, report: &mut Landed) -> Result<()> {
    // A crew worker runs in the worktree until land returns to it; the supervisor removes the
    // worktree once the worker has exited (crew::Supervisor::clean_up_landed).
    let crew_in_it = !temporary && db.crew_worker_in(worktree)?;
    if !temporary && opts.keep {
        // Said on the issue, so neither a crew supervisor once its worker exits nor a later land
        // (leftovers.rs) removes it.
        db.log_event(actor, Some(key), Some(&worktree.to_string_lossy()), "land_kept", "Kept the worktree and branch (--keep)")?;
    }
    let (cleanup, held_by) = if crew_in_it && opts.keep {
        (format!("kept {} (--keep)", worktree.display()), vec![])
    } else if crew_in_it {
        (format!("kept {} whole: the crew worker running in it removes it once it exits", worktree.display()), vec![])
    } else {
        let (message, held) = cleanup(root, pb, worktree, branch, temporary, opts.keep);
        if !temporary && !opts.keep && made_by_layout(root, pb, worktree) && worktree.exists() {
            crate::leftovers::note_held(db, actor, key, worktree, &message)?;
        }
        (message, held)
    };
    report.cleanup = Some(cleanup);
    report.held_by = held_by;
    Ok(())
}

/// An item's land into its goal branch: no review and no human approval (both come at the
/// checkpoint into main), but a blocked path is still refused.
fn check_item(pb: &Playbook, worktree: &Path, base: &str, gb: &crate::goal_branch::GoalBranch) -> Result<String> {
    let files: Vec<String> = git(worktree, &["diff", "--name-only", "--no-renames", &format!("{base}...HEAD")])?.lines().map(str::to_string).collect();
    pb.approval.check_blocked(&files)?;
    Ok(format!("an item of goal {}: it is reviewed, and approved where a person must, with the goal's checkpoint into {}", gb.goal, pb.integration_branch()))
}

/// The issue's branch, and the worktree it is checked out in: the one the issue's worktree has,
/// else the only local branch that names the issue.
pub(crate) fn branch_of(root: &Path, key: &str) -> Result<(String, Option<PathBuf>)> {
    let worktrees = list_worktrees(root)?;
    let named = |b: &str| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(key));
    let branch = match worktrees.iter().filter(|(_, b)| b.as_deref().is_some_and(named)).count() {
        1 => worktrees.iter().find_map(|(_, b)| b.clone().filter(|b| named(b))).unwrap(),
        0 => {
            let branches: Vec<String> = git(root, &["branch", "--format=%(refname:short)"])?.lines().filter(|b| named(b)).map(str::to_string).collect();
            match branches.as_slice() {
                [one] => one.clone(),
                [] => bail!("no local branch names {key}"),
                many => bail!("several branches name {key}: {}", many.join(", ")),
            }
        }
        _ => bail!("several worktrees have a branch that names {key}"),
    };
    match worktrees.into_iter().find(|(_, b)| b.as_deref() == Some(branch.as_str())) {
        Some((path, _)) if same_path(&path, root) => bail!("{branch} is checked out in the main checkout"),
        Some((path, _)) => Ok((branch, Some(path))),
        None => Ok((branch, None)),
    }
}

/// A worktree's state that stops a land: a rebase in progress, or uncommitted changes to tracked files.
pub(crate) fn check_worktree(worktree: &Path, branch: &str) -> Result<()> {
    let wt = worktree.to_string_lossy();
    if rebase_in_progress(worktree) {
        bail!("a rebase is in progress in {wt}; finish it (git rebase --continue) or abort it, then land again");
    }
    let dirty = git(worktree, &["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{wt} has uncommitted changes to tracked files; commit them on {branch} first:\n{}", dirty.trim_end());
    }
    Ok(())
}

/// A branch lands with a passing review of exactly what it changes, and, when a human must
/// approve, with that approval given in Nitely after the review. `worktree` holds the change at
/// HEAD on top of `base`; `branch` is the issue's branch, whose name says its vendor.
pub(crate) fn check_approval(db: &Db, pb: &Playbook, key: &str, worktree: &Path, base: &str, branch: &str) -> Result<String> {
    check_approval_at(db, pb, key, worktree, base, "HEAD", branch)
}

/// `check_approval` for the change at `head` in `worktree`'s repository, wherever `worktree` is
/// checked out: the land queue checks each change of a stack at its own tip without moving the
/// stack worktree to it (NIT-508).
pub(crate) fn check_approval_at(db: &Db, pb: &Playbook, key: &str, worktree: &Path, base: &str, head: &str, branch: &str) -> Result<String> {
    let mut current = pb.clone();
    crate::project_modes::apply_issue(db, key, &mut current)?;
    let pb = &current;
    let patch = crate::submit::patch_id_range(worktree, base, head)?;
    let review = db.reviews(key)?.into_iter().next().with_context(|| format!("{key} has no review; submit it first"))?;
    if review.patch_id != patch && !crate::test_command_merge::equivalent(worktree, base, head, &review).unwrap_or(false) {
        bail!(
            "the branch changed since its review (reviewed {}, now {}); submit again",
            short(&review.patch_id),
            short(&patch)
        );
    }
    check_yolo_policy(db, key, &review)?;
    check_disabled_policy(pb, &review)?;
    let accepted = crate::land_rules::verdict_lets_land(&review.verdict, &review.reviewer, || {
        Ok((db.unaccepted_blockers(key, &review)?.len(), review.findings.iter().filter(|f| f.priority <= 1).count()))
    })?;
    let files: Vec<String> = git(worktree, &["diff", "--name-only", "--no-renames", &format!("{base}...{head}")])?.lines().map(str::to_string).collect();
    pb.approval.check_blocked(&files)?;
    let mut reasons = pb.human_reasons(&files)?;
    if matches!(review.verdict.as_str(), "disabled" | "yolo") {
        let unanswered = pb.review.unevaluated_reasons();
        reasons.extend(unanswered);
    }
    if review.approver == "human" {
        reasons.extend(review.reasons.iter().cloned());
    }
    // A cross-vendor requirement added after the review still applies, judged from the branch.
    if !review.reviewer.is_empty() {
        let vendor = crate::git::parse_branch(branch.trim()).0.unwrap_or_default();
        if let Some(reason) = pb.review.independence_reason(&review.reviewer, &vendor, &review.note).filter(|r| !reasons.contains(r)) {
            reasons.push(reason);
        }
    }
    reasons.dedup();
    if let Some(note) = crate::land_rules::lands_without_a_person(&reasons, &review.verdict, &review.reviewer, &accepted) {
        return Ok(note);
    }
    match db.standing_approval(key)? {
        Some(at) => {
            // Only the app can sign, so a database write does not make an approval.
            // A structured test-list reconciliation above proves the same reviewed operation;
            // its existing signature still names the original patch, never a new unsigned id.
            db.check_signed_approval(key, &review.patch_id, at)?;
            Ok(crate::land_rules::approved_by_a_person(&reasons))
        }
        _ => bail!("{}", crate::land_rules::waits_for_a_person(&reasons)),
    }
}

pub(crate) fn check_yolo_policy(db: &Db, key: &str, review: &crate::submit::ReviewRecord) -> Result<()> {
    if let Some(gate) = &review.gate {
        crate::project_modes::check_gate_authority(db, &[key], gate)?;
    } else if crate::project_modes::authority_for_issue(db, key)?.is_some() {
        bail!("the submission has no project-mode evidence; submit again under the current settings");
    }
    if review.verdict == "yolo" && !crate::project_yolo::enabled_for_issue(db, key)? {
        bail!("YOLO mode is disabled; submit this unchecked revision for the normal gate and PR review before landing");
    }
    Ok(())
}

/// Disabled evidence never satisfies a newly required review, including a queued batch's final
/// check after its gate. New unanswered conditions must first be shown in a fresh submission.
pub(crate) fn check_disabled_policy(pb: &Playbook, review: &crate::submit::ReviewRecord) -> Result<()> {
    if review.verdict != "disabled" { return Ok(()) }
    if pb.review.enabled { bail!("automated review is enabled again; submit this patch for review before landing"); }
    if pb.review.unevaluated_reasons().iter().any(|r| !review.reasons.contains(r)) {
        bail!("review-dependent approval conditions changed since submit; submit again so the human sees the unevaluated conditions");
    }
    Ok(())
}

impl Db {
    /// When the human approved the issue, if that approval covers its latest review: the review
    /// the human saw, and every review since of the same patch, without asking for changes or
    /// raising a reason the approved one did not have. A changed patch or a new reason needs a new
    /// approval. Times are whole seconds, so the review the human saw is the newest one from before
    /// the approval's second, and reviews from that second count as later ones; only when none is
    /// older is it the first review of that second.
    pub fn standing_approval(&self, key: &str) -> Result<Option<i64>> {
        let Some(at) = self.human_approval_at(key)? else { return Ok(None) };
        let reviews = self.reviews(key)?; // newest first
        let approved = reviews.iter().position(|r| r.ts < at).or_else(|| reviews.iter().rposition(|r| r.ts == at));
        let Some(approved) = approved else {
            return Ok(reviews.is_empty().then_some(at));
        };
        let base = &reviews[approved];
        let mut unchanged = true;
        for r in &reviews[..approved] {
            unchanged &= r.patch_id == base.patch_id && self.review_passes(key, r)? && r.reasons.iter().all(|x| base.reasons.contains(x));
        }
        Ok((self.review_passes(key, base)? && unchanged).then_some(at))
    }

    /// When a human last moved the issue to Approved, if it is still Approved.
    pub fn human_approval_at(&self, key: &str) -> Result<Option<i64>> {
        let Some(issue) = self.issue(key)? else { return Ok(None) };
        if issue.state != "Approved" {
            return Ok(None);
        }
        Ok(self.conn.query_row(
            "SELECT MAX(ts) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'issue_moved' AND actor = 'human'
               AND summary LIKE '%→ Approved%'",
            [&issue.key],
            |r| r.get(0),
        )?)
    }
}

/// Removes the task's worktree and branch once landed: always for a worktree land added itself,
/// and for one the playbook's layout made (under its worktree folder) unless `keep`.
///
/// A process using the worktree (a dev server, a build) makes Windows refuse to delete files in
/// it, and git then stops halfway: the worktree unregistered, part of it deleted. So a worktree in
/// use is kept whole, with the processes named; and when git still stops halfway, what is left is
/// retried and then said plainly, with how to delete it.
pub fn cleanup(root: &Path, pb: &Playbook, worktree: &Path, branch: &str, temporary: bool, keep: bool) -> (String, Vec<crate::procs::Holder>) {
    if !(temporary || (made_by_layout(root, pb, worktree) && !keep)) {
        return (format!("kept {} (not made by start_task, or --keep)", worktree.display()), vec![]);
    }
    let path = worktree.to_string_lossy().into_owned();
    let held = crate::procs::holders(worktree);
    if !held.is_empty() {
        let names: Vec<String> = held.iter().map(|h| format!("{} (pid {})", h.name, h.pid)).collect();
        let message = format!(
            "kept {path} whole because {} still use{} it; stop {}, then run `git worktree remove \"{path}\"` and `git branch -d {branch}` in {}",
            names.join(", "),
            if held.len() == 1 { "s" } else { "" },
            if held.len() == 1 { "it" } else { "them" },
            root.display()
        );
        return (message, held);
    }
    let teardown = teardown(root, pb, worktree);
    // git worktree remove follows a junction in it and deletes what it points at (NIT-460): every
    // link goes as a link first, or the worktree is kept.
    let unlinked = crate::unlink::unlink_all(worktree).map_err(|e| format!("kept the worktree: {e:#}"));
    let removed = match unlinked.and_then(|_| crate::submodules::remove_worktrees(root, pb, worktree)) {
        Err(kept) => Err(kept),
        Ok(()) => match git(root, &["worktree", "remove", &path]) {
            Ok(_) => Ok(()),
            Err(e) => finish_removal(root, worktree, &e),
        },
    };
    if removed.is_ok() && !temporary {
        crate::submodules::delete_branches(root, pb, branch);
    }
    let message = match removed {
        Ok(()) if temporary => "removed the temporary worktree".into(),
        Ok(()) => match git(root, &["branch", "-d", branch]) {
            Ok(_) => "removed the worktree and the merged branch".into(),
            Err(e) => format!("removed the worktree; kept the branch ({})", first_line(&e)),
        },
        Err(message) => message,
    };
    (if teardown.is_empty() { message } else { format!("{message}; {}", teardown.join("; ")) }, vec![])
}

/// Whether `worktree` is under the playbook's worktree folder, where start_task makes them.
pub(crate) fn made_by_layout(root: &Path, pb: &Playbook, worktree: &Path) -> bool {
    let issue = crate::IssueRecord { key: "x".into(), title: "x".into(), ..Default::default() };
    let layout_dir = pb.worktree_for(root, "x", &issue);
    // Compared as paths::normalize spells them: git names a worktree by its real path, which may
    // differ in spelling from the repo's configured root (`/private/var` for `/var` on a Mac, a
    // long name for an 8.3 one like `RUNNER~1` on Windows), and the worktree is ours all the same.
    layout_dir.parent().is_some_and(|d| crate::paths::is_within(&worktree.to_string_lossy(), &d.to_string_lossy()))
}

/// Removes the worktrees earlier lands had to keep and nothing uses any more (NIT-340), under the
/// land lock: what it removed, for the report; what failed, as warnings.
pub(crate) fn tidy(db: &Db, root: &Path, pb: &Playbook, actor: &Actor, warnings: &mut Vec<String>) -> Vec<String> {
    match crate::leftovers::leftovers(db, root, pb, true, actor) {
        Ok(found) => found
            .into_iter()
            .filter_map(|l| {
                let message = l.removed?;
                if l.worktree.exists() {
                    warnings.push(format!("{} of {} is still there: {message}", l.worktree.display(), l.issue));
                    return None;
                }
                Some(format!("{} ({}): {message}", l.worktree.display(), l.issue))
            })
            .collect(),
        Err(e) => {
            warnings.push(format!("worktrees earlier lands kept were not looked at: {e:#}"));
            vec![]
        }
    }
}

/// Runs the playbook's worktree teardown in the worktree, returning what failed.
fn teardown(root: &Path, pb: &Playbook, worktree: &Path) -> Vec<String> {
    let mut failed = Vec::new();
    for command in &pb.worktree.teardown {
        let command = playbook::expand(command, root, worktree, pb.integration_branch());
        match playbook::run_shell_kind(&command, worktree, "setup") {
            Ok((true, _, _)) => {}
            Ok((false, output, _)) => failed.push(format!("teardown `{command}` failed: {}", output.trim().lines().last().unwrap_or(""))),
            Err(e) => failed.push(format!("teardown `{command}` did not run: {}", first_line(&e))),
        }
    }
    failed
}

/// After `git worktree remove` failed: Ok when the folder could be deleted after all, else what is
/// left and how to delete it. A worktree git still lists was not touched and is kept as it is.
fn finish_removal(root: &Path, worktree: &Path, error: &anyhow::Error) -> std::result::Result<(), String> {
    let path = worktree.to_string_lossy().into_owned();
    let same = |p: &Path| crate::paths::normalize(&p.to_string_lossy()) == crate::paths::normalize(&path);
    if list_worktrees(root).map(|w| w.iter().any(|(p, _)| same(p))).unwrap_or(true) {
        return Err(format!("kept the worktree ({})", first_line(error)));
    }
    // Unregistered and partly deleted: files just closed are often free a moment later.
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        if !worktree.exists() || crate::unlink::remove_tree(worktree).is_ok() {
            git(root, &["worktree", "prune"]).ok();
            return Ok(());
        }
    }
    git(root, &["worktree", "prune"]).ok();
    Err(format!(
        "git unregistered the worktree but could not delete all of {path} ({}); nothing in it is needed any more. \
Close whatever still uses it, then delete the folder (PowerShell: `Remove-Item -Recurse -Force \"{path}\"`; sh: `rm -rf \"{path}\"`). \
The merged branch was kept; delete it with `git branch -d` once the folder is gone",
        first_line(error)
    ))
}

pub(crate) struct Lock {
    path: PathBuf,
    /// The holder named by a lock this one replaced, left by a land whose process is gone.
    pub(crate) took_over: Option<String>,
}

impl Lock {
    /// The lock, or None when another land holds it. A lock whose land is gone (the machine went
    /// down mid-land) is taken over; one whose process still runs is broken only with `break_lock`.
    pub(crate) fn try_take(path: &Path, holder: &str, break_lock: bool) -> Result<Option<Lock>> {
        if path.exists() && !break_lock && !held_by_dead(path) {
            return Ok(None);
        }
        match Lock::take(path, holder, break_lock) {
            Ok(lock) => Ok(Some(lock)),
            // Taken by another between the look and the take.
            Err(_) if path.exists() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn take(path: &Path, holder: &str, break_lock: bool) -> Result<Lock> {
        // A land that died with the machine left its lock, and nothing would land again until a
        // person broke it (NIT-280).
        let mut took_over = if break_lock { None } else { clear_dead(path) };
        if path.exists() {
            let held = std::fs::read_to_string(path).unwrap_or_default();
            if !break_lock {
                let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(0, |d| d.as_secs());
                // Its process still runs, or it would have been taken over: a land that hangs, or
                // another program that got the number since.
                let stale = if age > LOCK_STALE_SECS {
                    " It is over an hour old and its process still runs; if that is no land, land again with break_lock."
                } else {
                    ""
                };
                bail!("another land holds {}: {}.{stale}", path.display(), held.trim());
            }
            took_over = held.split_whitespace().next().map(str::to_string);
            std::fs::remove_file(path).ok();
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("another land took {} just now", path.display()))?;
        writeln!(f, "{holder} pid {} since {}", std::process::id(), crate::issues::iso(now()))?;
        Ok(Lock { path: path.to_path_buf(), took_over })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

/// Whether the lock at `path` names a process that is gone. A lock that cannot be read, or names
/// no pid, is not.
fn held_by_dead(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    crate::land_rules::dead_holder(&text, std::process::id(), crate::procs::alive).is_some()
}

/// (path, branch) for every worktree, the main checkout first.
fn list_worktrees(root: &Path) -> Result<Vec<(PathBuf, Option<String>)>> {
    let mut out = Vec::new();
    let mut cur: Option<(PathBuf, Option<String>)> = None;
    for line in git(root, &["worktree", "list", "--porcelain"])?.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            out.extend(cur.take());
            cur = Some((PathBuf::from(p), None));
        } else if let (Some(b), Some(c)) = (line.strip_prefix("branch refs/heads/"), cur.as_mut()) {
            c.1 = Some(b.to_string());
        }
    }
    out.extend(cur);
    Ok(out)
}

fn rebase_in_progress(worktree: &Path) -> bool {
    // Both paths from one git call, a line each.
    git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", "rebase-merge", "--git-path", "rebase-apply"])
        .is_ok_and(|paths| paths.lines().any(|p| !p.trim().is_empty() && Path::new(p.trim()).exists()))
}

pub(crate) fn rev(dir: &Path, name: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", &format!("{name}^{{commit}}")])?.trim().to_string())
}

fn same_path(a: &Path, b: &Path) -> bool {
    crate::paths::normalize(&a.to_string_lossy()) == crate::paths::normalize(&b.to_string_lossy())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into())
}

fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

pub(crate) fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The main checkout's fast-forward clears an index.lock a killed land left (its mark names a
    /// process that is gone), and leaves anyone else's, saying where it is (NIT-341).
    #[test]
    fn a_killed_lands_index_lock_is_cleared_and_anyone_elses_named() {
        let base = crate::paths::unique_temp("nitely-ff-lock", "");
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("a.txt", "one\n")]);
        git(&repo, &["checkout", "-q", "-b", "next"]).unwrap();
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        git(&repo, &["commit", "-qam", "two"]).unwrap();
        let next = git(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git(&repo, &["checkout", "-q", "main"]).unwrap();
        let git_dir = repo.join(".git");
        let (lock, mark) = (git_dir.join("index.lock"), git_dir.join("nitely-fast-forward"));
        // Someone else's lock, from before this land: left alone, and the error names it.
        std::fs::write(&lock, "").unwrap();
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(600)).unwrap();
        let err = format!("{:#}", fast_forward_main(&repo, &next).unwrap_err());
        assert!(err.contains("index.lock"), "{err}");
        assert!(lock.exists() && !mark.exists());
        // A dead land's mark, but the lock was taken long after it: someone else's, left alone.
        std::fs::write(&mark, "fast-forward pid 4000000000 since then").unwrap();
        std::fs::File::options().write(true).open(&mark).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
        assert!(fast_forward_main(&repo, &next).is_err());
        assert!(lock.exists() && !mark.exists(), "the mark goes, the lock stays");
        // A killed land's: its mark names a process that is gone and the lock came with it, so
        // both go and the land goes on.
        std::fs::write(&mark, "fast-forward pid 4000000000 since then").unwrap();
        // Made anew: rewriting an empty file with nothing leaves its time as it was.
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, "").unwrap();
        fast_forward_main(&repo, &next).unwrap();
        assert!(!lock.exists() && !mark.exists());
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]).unwrap().trim(), next);
        std::fs::remove_dir_all(&base).ok();
    }

    /// The after steps marked `outside_lock` run once the land lock is released, in a checkout of
    /// the integration branch's newest commit, one land's at a time; a land that finishes while
    /// they run leaves a mark and the next round builds its commit too (NIT-334).
    #[test]
    fn steps_outside_the_lock_build_the_newest_commit_and_fold_lands_that_finish_meanwhile() {
        let base = crate::paths::unique_temp("nitely-after-outside", "");
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("a.txt", "one\n")]);
        let pb = Playbook::parse(concat!(
            "[worktree]\n",
            "dir = \"../wt/{key}\"\n",
            "[[land.after]]\n",
            "run = \"git rev-parse HEAD >> {main}/../built.txt\"\n",
            "when_changed = ['^src/', '^a\\.txt$']\n",
            "outside_lock = true\n",
        ))
        .unwrap();
        let head = |dir: &Path| git(dir, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let built = || std::fs::read_to_string(base.join("built.txt")).unwrap_or_default().lines().map(str::to_string).collect::<Vec<_>>();
        let commit = |file: &str, text: &str, message: &str| {
            std::fs::write(repo.join(file), text).unwrap();
            git(&repo, &["add", file]).unwrap();
            git(&repo, &["commit", "-qm", message]).unwrap();
        };
        // Under the lock, the step waits for later.
        let mut report = Landed::default();
        after_land(&repo, &pb, &mut report);
        assert!(report.after.is_empty(), "{:?}", report.after);
        // Then it runs in its own checkout, at the integration branch's commit.
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.len() == 1 && ran[0].ok && !ran[0].skipped, "{ran:?}");
        assert_eq!(built(), [head(&repo)]);
        assert!(after_tree_path(&pb, &repo).join("a.txt").exists());
        // Another land's steps are running: this one leaves its mark and returns at once.
        let lock = after_lock_path(&repo).unwrap();
        let other = take_after_lock(&lock).expect("free");
        assert!(after_steps_running(&repo));
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.len() == 1 && ran[0].skipped && ran[0].output.contains("another land"), "{ran:?}");
        assert!(after_pending_path(&repo).unwrap().exists());
        drop(other);
        assert!(!after_steps_running(&repo), "let go with its holder");
        // A change lands: the next run builds that commit, and no mark is left.
        commit("a.txt", "two\n", "two");
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.iter().all(|r| r.ok && !r.skipped), "{ran:?}");
        assert_eq!(built().last(), Some(&head(&repo)));
        assert!(!after_pending_path(&repo).unwrap().exists() && !after_steps_running(&repo));
        // A land that changed nothing the step is for (here: docs) skips it.
        commit("docs.md", "x\n", "docs");
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.len() == 1 && ran[0].skipped && ran[0].output.starts_with("skipped: nothing matching"), "{ran:?}");
        assert_eq!(built().len(), 2, "not run again");
        // But one after a change it is for, landed by anyone since it last ran (a land queue's
        // change, then a docs land that drained it), runs it.
        commit("a.txt", "three\n", "three");
        commit("docs.md", "y\n", "docs again");
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.iter().all(|r| r.ok && !r.skipped), "{ran:?}");
        assert_eq!(built().last(), Some(&head(&repo)));
        // A change and its revert land between two runs: the step still runs, since each commit
        // counts (the stale-build check counts them too).
        commit("a.txt", "four\n", "four");
        commit("a.txt", "three\n", "back to three");
        let ran = after_land_outside_lock(&repo, &pb);
        assert!(ran.iter().all(|r| r.ok && !r.skipped), "{ran:?}");
        assert_eq!(built().last(), Some(&head(&repo)));
        std::fs::remove_dir_all(&base).ok();
    }

    /// The lanes keep a lane for a land only while the process that took its lock lives (NIT-332).
    #[test]
    fn a_land_lock_counts_only_while_its_process_lives() {
        let root = crate::paths::unique_temp("nitely-land-held", "");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert!(!held_by_live_land(&root), "no lock");
        let lock = root.join(".git").join(LOCK_FILE);
        std::fs::write(&lock, format!("NIT-9 pid {} since 2026-10-18T12:00:00Z
", std::process::id())).unwrap();
        assert!(held_by_live_land(&root), "held by this process");
        std::fs::write(&lock, "NIT-9 pid 4000000000 since 2026-10-18T12:00:00Z
").unwrap();
        assert!(!held_by_live_land(&root), "its process is gone");
        std::fs::remove_dir_all(root).ok();
    }
    use crate::issues::NewIssue;
    use crate::submit::{submit, SubmitOptions};

    #[test]
    fn an_automatic_land_whose_gate_ran_past_the_lane_limit_leaves_the_issue_where_it_is() {
        let mut f = fixture();
        let new = NewIssue { project: "NIT".into(), title: "Timed out".into(), kind: "bug".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() };
        let issue = f.db.create_issue(&new, &f.actor).unwrap();
        f.db.move_issue(&issue.key, "In Review", None, None, &f.actor).unwrap();
        let step = |timed_out: bool| playbook::CommandResult { command: "cargo test".into(), ok: false, seconds: 1800, output: String::new(), skipped: false, timed_out };
        let stopped = |timed_out: bool| -> Result<Landed> {
            let gate = GateResult { mode_authority: None, plan: None, skipped: None, passed: false, commit: "abc1234".into(), results: vec![step(timed_out)], warnings: vec![], flaked: vec![], lane_run: None };
            Ok(Landed { issue: issue.key.clone(), error: Some("the gate stopped".into()), gate: Some(gate), ..Default::default() })
        };
        let (state, next) = after_auto_land(&mut f.db, &issue.key, &stopped(true), &f.actor).unwrap();
        assert_eq!(state, "In Review");
        assert!(next.contains("Nothing in the change is to fix") && !next.contains("Fix it"), "{next}");
        // A gate that failed sends it back, as before.
        let (state, next) = after_auto_land(&mut f.db, &issue.key, &stopped(false), &f.actor).unwrap();
        assert_eq!(state, "In Progress");
        assert!(next.contains("Fix it, commit, and submit again"), "{next}");
        drop(f.db);
        std::fs::remove_dir_all(&f.base).ok();
    }

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        remote: PathBuf,
        db: Db,
        actor: Actor,
    }

    use crate::test_repo::git as sh;

    /// A repo whose playbook gates with `exit 0` and has a fake reviewer. It has no remote, so land
    /// pushes nothing: three pushes a land, of several git processes each, in tests about something
    /// else. `pushed_fixture` has one.
    fn fixture() -> Fixture {
        fixture_from(crate::test_repo::copy("land", |base| land_repo(&base.join("repo"))))
    }

    /// As `fixture`, with a bare remote that land pushes to.
    fn pushed_fixture() -> Fixture {
        fixture_from(crate::test_repo::copy("land-pushed", |base| {
            land_repo(&base.join("repo"));
            crate::test_repo::add_remote(&base.join("repo"), &base.join("remote.git"));
        }))
    }

    fn land_repo(repo: &Path) {
        // Land rebases and commits in its own git calls, which need an identity: init sets one.
        let playbook = PLAYBOOK.replace("{sh}", &playbook::posix_sh());
        crate::test_repo::init(repo, &[(playbook::PLAYBOOK_PATH, &playbook), ("shared.txt", "one\n")]);
    }

    fn fixture_from(base: PathBuf) -> Fixture {
        let repo = base.join("repo");
        let remote = base.join("remote.git");
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        Fixture { base, repo, remote, db, actor: Actor::Agent("claude:test".into()) }
    }

    // `exit 0` and the fake reviewer run the same under cmd (Windows) and sh.
    const PLAYBOOK: &str = r#"
[worktree]
dir = "../wt/{key}"
teardown = ["echo torn >> {main}/../teardown.txt"]
[gate]
commands = ["exit 0"]
[review]
reviewer = { "*" = "fake" }
commands = { fake = ["{sh}", "-c", "cat > /dev/null; echo 'Fine.'; echo 'VERDICT: clean'"] }
[[approval.human_paths]]
pattern = "billing"
reason = "billing"
[[land.after]]
run = "echo landed >> {main}/../after-sidebar.txt"
when_changed = ["sidebar"]
[[land.after]]
run = "exit 3"
when_changed = ["billing"]
"#;

    impl Fixture {
        /// Files an issue, starts its worktree, commits `file`, and submits it.
        fn submitted(&mut self, title: &str, file: &str, content: &str) -> (String, PathBuf) {
            let (key, wt) = self.committed(title, file, content);
            self.submit(&wt);
            (key, wt)
        }

        /// As `submitted`, with a passing review standing in for the submit: for tests about
        /// what land does with a reviewed change that needs no person.
        fn reviewed(&mut self, title: &str, file: &str, content: &str) -> (String, PathBuf) {
            let (key, wt) = self.committed(title, file, content);
            crate::test_repo::reviewed(&mut self.db, &wt, &key, &self.actor);
            (key, wt)
        }

        /// Files an issue, starts its worktree, and commits `file` there.
        fn committed(&mut self, title: &str, file: &str, content: &str) -> (String, PathBuf) {
            let issue = self
                .db
                .create_issue(
                    &NewIssue {
                        project: "NIT".into(),
                        title: title.into(),
                        kind: "feature".into(),
                        body: "b".into(),
                        evidence: "none yet".into(),
                        source: "human".into(),
                        priority: 0,
                        allow_similar: true,
                        state: None,
                        blocked_by: vec![],
                        ..Default::default()
                    },
                    &self.actor,
                )
                .unwrap();
            let started = playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap();
            let wt = PathBuf::from(&started.worktree);
            std::fs::write(wt.join(file), content).unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);
            (issue.key, wt)
        }

        fn submit(&mut self, wt: &Path) {
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
            submit(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap();
        }

        fn land(&mut self, key: &str) -> Landed {
            let dir = self.repo.to_string_lossy().into_owned();
            let opts = LandOptions { dir: &dir, issue_key: key, push: true, keep: false, break_lock: false };
            land(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap()
        }
    }

    #[test]
    fn a_finding_the_person_accepted_does_not_block_the_land() {
        let mut f = fixture();
        // A reviewer that keeps one P1 a person decided to live with.
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap().replace(
            "echo 'Fine.'; echo 'VERDICT: clean'",
            "echo 'One thing.'; echo '- [P1] No migration: keys.txt, load: old profiles lose their keys'; echo 'VERDICT: findings'",
        );
        std::fs::write(&pb_path, pb).unwrap();
        sh(&f.repo, &["commit", "-q", "-am", "a reviewer with a P1"]);
        let (a, wt) = f.submitted("Keys per folder", "keys.txt", "x");
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: true, keep: false, break_lock: false };
        let refused = land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap().error.unwrap_or_default();
        assert!(refused.contains("asked for changes"), "{refused}");

        // The agent asks; a forged acceptance written into the database counts for nothing.
        let ask = f.db.request_acceptance(&a, "1", "You ruled out a migration: there are no outside users yet.", None, &f.actor).unwrap();
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Needs You");
        let patch = f.db.reviews(&a).unwrap()[0].patch_id.clone();
        f.db.conn
            .execute(
                "INSERT INTO accepted_finding (issue_key, patch_id, priority, finding, reason, ts, signature) VALUES (?1, ?2, 1, ?3, 'forged', 1, 'zz')",
                rusqlite::params![a, patch, "No migration: keys.txt, load: old profiles lose their keys"],
            )
            .unwrap();
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
        assert_eq!(land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap().error.unwrap_or_default(), refused, "an unsigned row is no acceptance");

        // The person accepts it in the app: signed, the issue goes on as a passing review would,
        // also when the rounds had handed it to them as their own work.
        f.db.set_owner(&a, "human", &f.actor).unwrap();
        let decided = f.db.accept_finding(ask.id, "no outside users yet", &SECRET).unwrap();
        // Where only the app can check acceptances (macOS), the change waits for its approval there.
        let goes_on = if crate::approval::agents_can_verify() { "In Review" } else { "Needs You" };
        assert_eq!((decided.state.as_str(), decided.ask.answer.as_deref()), (goes_on, Some("Accepted: no outside users yet")));
        assert!(f.db.accept_finding(ask.id, "again", &SECRET).is_err(), "decided once");
        // Waiting for an approval is said as such, for the goal report (NIT-354).
        let awaits = f.db.timeline(&a).unwrap().iter().filter(|e| e.kind == crate::accept::AWAITS_APPROVAL).count();
        assert_eq!(awaits, usize::from(goes_on == "Needs You"));
        // Submitted again unchanged: the reviewer still says it, and it passes on the acceptance.
        f.submit(&wt);
        let note = f.db.timeline(&a).unwrap().into_iter().rev().find(|e| e.kind == "submit").unwrap().summary.unwrap_or_default();
        assert!(note.contains("only in findings you accepted"), "{note}");
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "In Review");
        let r = f.land(&a);
        assert!(r.landed && r.approval.as_deref().is_some_and(|a| a.contains("accepted by a human")), "{r:?}");
        assert!(!wt.exists());
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_stalled_review_asks_about_each_finding_and_waits_for_all_of_them() {
        // Rounds that do not converge (NIT-220): the person decides each open finding; the issue
        // stays the agents' and waits in Needs You until every one is decided.
        let mut f = fixture();
        let (a, _) = f.reviewed("Rows", "rows.txt", "x");
        asked_for(&f, &a, &[(1, "Drops a row: rows.txt, f: the last row"), (0, "Leaks a key: keys.txt, g: logs it")], 1);
        assert_eq!(f.db.ask_to_decide_findings(&a, "1 P0/P1 finding still open after two rounds", &f.actor).unwrap(), "Needs You");
        // Asked again (another stalled round), the requests already open stand.
        f.db.ask_to_decide_findings(&a, "again", &f.actor).unwrap();
        let requests: Vec<crate::issues::Ask> = f.db.open_asks().unwrap().into_iter().filter(|x| x.issue_key.as_deref() == Some(a.as_str())).collect();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert_eq!(f.db.issue(&a).unwrap().unwrap().owner, "agent");
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
        let first = f.db.accept_finding(requests[0].id, "no outside users yet", &SECRET).unwrap();
        assert_eq!(first.state, "Needs You", "the other finding still waits for the person");
        let second = f.db.accept_finding(requests[1].id, "the key is a test key", &SECRET).unwrap();
        let goes_on = if crate::approval::agents_can_verify() { "In Review" } else { "Needs You" };
        assert_eq!(second.state, goes_on);
        assert!(f.db.findings_decided_at(&a).unwrap() > 0);
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A round of review on `key` that asks for changes with one P1 `finding`, `after` seconds on.
    fn reviewed_with(f: &Fixture, key: &str, finding: &str, after: i64) {
        asked_for(f, key, &[(1, finding)], after);
    }

    /// A round of review on `key` that asks for changes with `(priority, text)` findings, `after`
    /// seconds on, as the reviewer's findings are recorded.
    fn asked_for(f: &Fixture, key: &str, findings: &[(u8, &str)], after: i64) {
        let findings: Vec<_> = findings.iter().map(|(priority, text)| crate::review::Finding { priority: *priority, text: (*text).into(), carried: false, raised_as: None, now: None }).collect();
        f.db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, findings, ts) VALUES (?1, 'p1', 'c', 'fake', 'changes', 'agent', ?2, ?3)",
                rusqlite::params![key, serde_json::to_string(&findings).unwrap(), crate::db::now() + after],
            )
            .unwrap();
    }

    #[test]
    fn a_finding_the_rounds_keep_raising_keeps_one_open_request() {
        let mut f = fixture();
        let (a, _) = f.reviewed("Lock", "lock.txt", "x");
        let blocker = "Install holds the land lock: `crates/app/src/land.rs:416`, `after_land_outside_lock`: the install step never sets `outside_lock = true`.";
        let other = "Ignores a failed push: `crates/app/src/land.rs:500`, `after_land_outside_lock`: the push result is dropped.";
        reviewed_with(&f, &a, blocker, 1);
        let ask = f.db.request_acceptance(&a, "1", "Only the person changes the playbook.", None, &f.actor).unwrap();
        // Later rounds are told its words and say it the same way: it stays one open request.
        let no_gate = crate::playbook::GateResult { mode_authority: None, passed: true, plan: None, skipped: None, commit: "c".into(), results: vec![], warnings: vec![], flaked: vec![], lane_run: None };
        for round in 2..4 {
            reviewed_with(&f, &a, blocker, round);
            let latest = f.db.reviews(&a).unwrap().remove(0);
            assert_eq!(f.db.close_superseded_requests(&a, &latest.findings).unwrap(), 0, "round {round}");
            assert!(f.db.ask(ask.id).unwrap().unwrap().closed.is_none());
            assert!(crate::submit::review_context(&f.db, &f.db.issue(&a).unwrap().unwrap(), &no_gate).unwrap().contains(blocker), "each round is told the words being decided");
        }
        // Asking again is that same request, and the issue waits for the person again, as asking does.
        f.db.move_issue(&a, "In Progress", None, None, &f.actor).unwrap();
        assert_eq!(f.db.request_acceptance(&a, "1", "Still only the person.", None, &f.actor).unwrap().id, ask.id);
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Needs You");
        assert_eq!(f.db.open_asks().unwrap().iter().filter(|q| q.issue_key.as_deref() == Some(a.as_str())).count(), 1, "one decision in the Inbox");
        // Another blocker in the same code is another decision, and the first one's request is not its.
        reviewed_with(&f, &a, other, 4);
        let latest = f.db.reviews(&a).unwrap().remove(0);
        assert_eq!(f.db.close_superseded_requests(&a, &latest.findings).unwrap(), 1, "the round no longer raises the first");
        assert_eq!(f.db.ask(ask.id).unwrap().unwrap().finding.map(|x| x.text).as_deref(), Some(blocker));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn two_blockers_in_one_function_are_two_requests() {
        let mut f = fixture();
        let (a, _) = f.reviewed("Rows", "rows.txt", "x");
        let two = vec![
            crate::review::Finding { priority: 1, text: "Drops the last row: src/rows.rs:12, export: the loop stops early.".into(), carried: false, raised_as: None, now: None },
            crate::review::Finding { priority: 1, text: "Ignores write failures: src/rows.rs:30, export: an error is dropped.".into(), carried: false, raised_as: None, now: None },
        ];
        f.db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, findings, ts) VALUES (?1, 'p1', 'c', 'fake', 'changes', 'agent', ?2, ?3)",
                rusqlite::params![a, serde_json::to_string(&two).unwrap(), crate::db::now() + 1],
            )
            .unwrap();
        let first = f.db.request_acceptance(&a, "1", "The old export did the same.", None, &f.actor).unwrap();
        // Escalation asks about the blocker the person has no request for.
        f.db.ask_to_decide_findings(&a, "rounds did not converge", &f.actor).unwrap();
        let open: Vec<_> = f.db.open_asks().unwrap().into_iter().filter(|q| q.issue_key.as_deref() == Some(a.as_str())).collect();
        assert_eq!(open.len(), 2, "two blockers in one function are two decisions: {open:?}");
        assert_eq!(f.db.close_superseded_requests(&a, &two).unwrap(), 0);
        // Deciding one leaves the other blocker's request open.
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
        f.db.accept_finding(first.id, "the old export did the same", &SECRET).unwrap();
        let second = open.iter().find(|q| q.id != first.id).unwrap();
        assert!(f.db.ask(second.id).unwrap().unwrap().closed.is_none());
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn two_requests_for_one_finding_become_one_and_the_newest_stands() {
        let mut f = fixture();
        let (a, _) = f.reviewed("Twice", "twice.txt", "x");
        let finding = "Drops a row: `src/rows.rs:12`, `export`: the last row is never written.";
        reviewed_with(&f, &a, finding, 1);
        // Two requests for it, as an earlier build filed them.
        let asks: Vec<i64> = (0..2)
            .map(|i| {
                let q = f.db.create_ask(&format!("Request {i}"), &[], Some(&a), None, &f.actor).unwrap();
                let asked = serde_json::json!({ "priority": 1, "text": finding });
                f.db.conn.execute("UPDATE ask SET finding = ?2 WHERE id = ?1", rusqlite::params![q.id, asked.to_string()]).unwrap();
                q.id
            })
            .collect();
        let latest = f.db.reviews(&a).unwrap().remove(0);
        assert_eq!(f.db.close_superseded_requests(&a, &latest.findings).unwrap(), 1);
        let older = f.db.ask(asks[0]).unwrap().unwrap();
        assert_eq!(older.closed, Some(format!("closed: ask #{} asks for the same finding", asks[1])));
        assert!(f.db.ask(asks[1]).unwrap().unwrap().closed.is_none(), "the newest stands");
        // A round that no longer raises it closes the one left: it was fixed.
        assert_eq!(f.db.close_superseded_requests(&a, &[]).unwrap(), 1);
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_finding_sent_back_stays_a_defect_to_fix() {
        let mut f = fixture();
        let (a, _) = f.reviewed("Rows", "rows.txt", "x");
        asked_for(&f, &a, &[(1, "Drops a row: rows.txt, f: the last row")], 1);
        // A person's own issue (handed over after rounds that did not converge) goes back to the
        // agents when they decide, and a request a later review made moot can still be sent back.
        f.db.set_owner(&a, "human", &f.actor).ok();
        let wrong = f.db.request_acceptance(&a, "Nothing like it", "why", None, &f.actor).unwrap_err().to_string();
        assert!(wrong.contains("1. [P1] Drops a row"), "lists what can be accepted: {wrong}");
        let ask = f.db.request_acceptance(&a, "Drops a row: rows.txt, f: the last row", "It is how the export always worked.", None, &f.actor).unwrap();
        assert_eq!(ask.finding.as_ref().map(|f| f.priority), Some(1));
        let decided = f.db.decline_finding(ask.id, "it is a real bug", "").unwrap();
        assert_eq!(decided.state, "Todo");
        assert!(decided.ask.answer.unwrap().starts_with("Not accepted: it is a real bug"));
        assert!(f.db.unaccepted_blockers(&a, &f.db.reviews(&a).unwrap()[0]).unwrap().len() == 1);

        // A later round without the finding closes a request still open, and one already in the
        // person's hands can still be sent back.
        let moot = f.db.request_acceptance(&a, "1", "Still how it always worked.", None, &f.actor).unwrap();
        // Asking again for the same finding is the same request, with the newer reasoning (NIT-353).
        let again = f.db.request_acceptance(&a, "1", "Asked twice.", None, &f.actor).unwrap();
        assert_eq!((again.id, again.question.as_str()), (moot.id, "Asked twice."));
        // A request still open for another finding, as an earlier round may leave one.
        let open = f.db.create_ask("Another one.", &[], Some(&a), None, &f.actor).unwrap();
        f.db.conn.execute("UPDATE ask SET finding = ?2 WHERE id = ?1", rusqlite::params![open.id, r#"{"priority":1,"text":"Skips a row: rows.txt, g: the first row"}"#]).unwrap();
        f.db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, findings, ts) VALUES (?1, 'p9', 'c', 'fake', 'pass', 'agent', '[]', ?2)",
                rusqlite::params![a, crate::db::now() + 5],
            )
            .unwrap();
        assert!(f.db.accept_finding(moot.id, "fine", &SECRET).is_err(), "nothing to accept any more");
        assert!(f.db.decline_finding(moot.id, "moot now", "").is_ok());
        // Sending one back closes the issue's other requests: the change goes back to be fixed.
        assert!(f.db.ask(open.id).unwrap().unwrap().closed.as_deref().is_some_and(|c| c.contains("sent back")));
        assert_eq!(f.db.close_superseded_requests(&a, &[]).unwrap(), 0);
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn removes_its_own_worktree_however_its_path_is_spelled() {
        let mut f = fixture();
        let (_, wt) = f.committed("Spelled", "spelled.txt", "x");
        let branch = sh(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        // The worktree as git or the system may name it, which need not be how the repo's root is
        // spelled: through `/private` on a Mac, with 8.3 short names on Windows (NIT-261).
        let text = wt.to_string_lossy().into_owned();
        let other = if cfg!(target_os = "macos") && text.starts_with("/var/") {
            format!("/private{text}")
        } else {
            crate::paths::short_path(&text)
        };
        let pb = Playbook::load(&f.repo).unwrap();
        let (message, _) = cleanup(&f.repo, &pb, Path::new(&other), &branch, false, false);
        assert!(message.starts_with("removed the worktree"), "{message} ({other})");
        assert!(!wt.exists());
    }

    #[test]
    fn an_approval_covers_only_the_patch_the_human_saw() {
        let path = crate::paths::unique_temp("nitely-approval", ".db");
        let mut db = Db::open(&path).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let new = NewIssue {
            project: "NIT".into(),
            title: "Billing fix".into(),
            kind: "feature".into(),
            body: "b".into(),
            evidence: "none yet".into(),
            source: "human".into(),
            priority: 0,
            allow_similar: true,
            state: None,
            blocked_by: vec![],
            ..Default::default()
        };
        let key = db.create_issue(&new, &Actor::Agent("claude:test".into())).unwrap().key;
        db.move_issue(&key, "Approved", None, None, &Actor::Human).unwrap();
        let at = db.human_approval_at(&key).unwrap().unwrap();
        let reviewed = |db: &Db, patch: &str, verdict: &str, reasons: &str, ts: i64| {
            db.conn
                .execute(
                    "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, ts)
                     VALUES (?1, ?2, 'c', 'codex', ?3, 'human', ?4, ?5)",
                    rusqlite::params![key, patch, verdict, reasons, ts],
                )
                .unwrap();
        };
        let stands = |db: &Db| db.standing_approval(&key).unwrap().is_some();

        reviewed(&db, "p1", "pass", r#"["billing"]"#, at - 5);
        assert!(stands(&db), "approved after the review");
        reviewed(&db, "p1", "pass", r#"["billing"]"#, at + 5);
        assert!(stands(&db), "the same patch again, nothing new");
        // A review in the approval's own second is not the one the human saw.
        reviewed(&db, "p2", "none", r#"["billing"]"#, at);
        assert!(!stands(&db), "a changed patch in the approval's second");
        db.conn.execute("DELETE FROM review", []).unwrap();
        reviewed(&db, "p1", "pass", r#"["billing"]"#, at - 5);
        reviewed(&db, "p1", "pass", r#"["billing", "money logic"]"#, at + 5);
        assert!(!stands(&db), "a new reason");
        db.conn.execute("DELETE FROM review", []).unwrap();
        // With no earlier review, the first one of the approval's second is the one approved.
        reviewed(&db, "p1", "pass", r#"["billing"]"#, at);
        assert!(stands(&db));
        reviewed(&db, "p3", "pass", r#"["billing"]"#, at);
        assert!(!stands(&db), "a second, different patch in that second");
        drop(db);
        std::fs::remove_file(&path).ok();
    }

    /// Commits a playbook that lands by `mode`.
    fn set_mode(f: &Fixture, mode: &str) {
        let path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&path).unwrap().replacen("[[land.after]]", &format!("[land]\nby = \"{mode}\"\n[[land.after]]"), 1);
        std::fs::write(&path, pb).unwrap();
        sh(&f.repo, &["commit", "-q", "-am", mode]);
    }

    fn state(f: &Fixture, key: &str) -> String {
        f.db.issue(key).unwrap().unwrap().state
    }

    /// `root` as one of the NIT project's repos in the config.
    fn linked(root: &Path, name: &str) -> crate::RepoConfig {
        crate::RepoConfig { name: name.into(), root: root.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some("NIT".into()) }
    }

    #[test]
    fn an_automatic_land_that_stops_on_a_conflict_sends_the_issue_back() {
        let mut f = fixture();
        set_mode(&f, "auto");
        let (c, _) = f.submitted("Billing again", "billing.txt", "y");
        assert_eq!(state(&f, &c), "Needs You");
        // Another change to the same file lands first.
        std::fs::write(f.repo.join("billing.txt"), "x").unwrap();
        sh(&f.repo, &["add", "."]);
        sh(&f.repo, &["commit", "-q", "-m", "Billing fix"]);
        // An automatic land that stops on a conflict sends the issue back with the reason.
        approve(&mut f.db, &c);
        let dir = f.repo.to_string_lossy().into_owned();
        let r = land_on_approval(&mut f.db, &crate::Config::default(), &c, &dir).unwrap().unwrap();
        assert!(!r.landed && r.conflicts == ["billing.txt"], "{r:?}");
        assert_eq!(state(&f, &c), "In Progress");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// The app's approval key in these tests.
    const SECRET: [u8; 32] = [7; 32];

    /// The person approves in the app: the move, and the app's signature.
    fn approve(db: &mut Db, key: &str) {
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
        db.move_issue(key, "Approved", None, None, &Actor::Human).unwrap();
        db.sign_approval(key, &SECRET).unwrap();
    }

    /// A reviewed change lands, pushed, with teardown, cleanup and the after-land steps for what
    /// landed; one that needs a human waits. Then, with `[land] by = "auto"`, a change that needs
    /// no human lands in its own submit as Nitely, and the waiting one lands as soon as the person
    /// approves it, from the repo that has its branch (NIT-528 merged lands_by_nitely in here).
    #[test]
    fn lands_reviewed_work_and_waits_for_human_approval() {
        let mut f = pushed_fixture();
        let (a, wt_a) = f.submitted("Sidebar polish", "sidebar.txt", "x");
        let (b, _) = f.submitted("Billing fix", "billing.txt", "x");

        let r = f.land(&a);
        assert!(r.landed && r.pushed == Some(true), "{r:?}");
        assert_eq!(sh(&f.remote, &["rev-parse", "main"]), sh(&f.repo, &["rev-parse", "main"]));
        assert!(!wt_a.exists(), "the worktree is cleaned up: {:?}", r.cleanup);
        assert!(f.base.join("teardown.txt").exists(), "the playbook's teardown ran first: {:?}", r.cleanup);
        // The after-land step for what landed ran in the main checkout; the other was skipped.
        assert!(f.base.join("after-sidebar.txt").exists(), "{:?}", r.after);
        assert_eq!(r.after.iter().map(|a| a.skipped).collect::<Vec<_>>(), [false, true]);
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Done");

        // b needs a human; it was reviewed before a landed, so it also rebases.
        let r = f.land(&b);
        assert!(!r.landed && r.error.as_deref().unwrap().contains("waits for a human"), "{r:?}");

        // Nitely lands: a passing review that needs no human lands in the same submit.
        set_mode(&f, "auto");
        let (c, wt_c) = f.submitted("Sidebar spacing", "sidebar-spacing.txt", "x");
        assert_eq!(state(&f, &c), "Done");
        assert!(!wt_c.exists() && f.repo.join("sidebar-spacing.txt").exists());
        let said = f.db.timeline(&c).unwrap().into_iter().find(|e| e.summary.as_deref() == Some("Landing it automatically, as the playbook says")).unwrap();
        assert_eq!(said.actor, AUTO_LANDER, "Nitely's line, not the agent's: it does not put the agent back on the issue");
        let landed = f.db.timeline(&c).unwrap().into_iter().find(|e| e.kind == "landed").unwrap();
        assert_eq!(landed.actor, AUTO_LANDER, "submit's automatic land is Nitely's, as an approved one is");

        // The one that needs a human lands as soon as they approve it.
        let dir = f.repo.to_string_lossy().into_owned();
        assert!(land_on_approval(&mut f.db, &crate::Config::default(), &b, &dir).unwrap().is_none(), "not approved yet");
        approve(&mut f.db, &b);
        // The app approves with no directory: with two repos linked to the project, the one with
        // the issue's branch lands it. Its after-land step fails, which the issue is told.
        let other = f.base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["init", "-q", "-b", "main"]);
        let two = crate::Config { repos: vec![linked(&other, "other"), linked(&f.repo, "repo")], ..Default::default() };
        let mut reported = HashMap::new();
        let r = land_approved(&mut f.db, &two, &mut reported).unwrap();
        assert!(r.len() == 1 && r[0].landed && r[0].issue == b, "{r:?}");
        let notes = f.db.timeline(&b).unwrap();
        assert!(notes.iter().any(|e| e.summary.as_deref().is_some_and(|s| s.contains("with warnings") && s.contains("exit 3"))), "{notes:?}");
        assert_eq!(state(&f, &b), "Done");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn lands_only_with_the_apps_signed_approval_of_the_patch_it_saw() {
        let mut f = fixture();
        let (b, wt_b) = f.submitted("Billing fix", "billing.txt", "x");
        let r = f.land(&b);
        assert!(!r.landed && r.error.as_deref().unwrap().contains("waits for a human"), "{r:?}");
        // An agent cannot approve; only a human move counts.
        assert!(f.db.move_issue(&b, "Approved", None, None, &f.actor).is_err());
        // An approval with no valid signature goes back to the person when the app starts.
        {
            crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
            f.db.conn.execute("UPDATE issue SET state = 'Approved', state_type = 'started' WHERE key = ?1", [&b]).unwrap();
            f.db.conn
                .execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', ?2, 'issue_moved', 'Needs You → Approved')", rusqlite::params![now() - 60, b])
                .unwrap();
            let moved = f.db.unsigned_approvals_back_to_needs_you(&crate::approval::public_of(&SECRET)).unwrap();
            assert_eq!(moved, [b.clone()]);
            assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "Needs You");
        }
        // Nor can it by writing the database as if the person had: the app did not sign that.
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&SECRET)));
        f.db.conn.execute("UPDATE issue SET state = 'Approved', state_type = 'started' WHERE key = ?1", [&b]).unwrap();
        f.db.conn
            .execute("INSERT INTO event (ts, actor, issue_key, type, summary) VALUES (?1, 'human', ?2, 'issue_moved', 'Needs You → Approved')", rusqlite::params![now(), b])
            .unwrap();
        assert!(f.db.standing_approval(&b).unwrap().is_some(), "it looks approved");
        let r = f.land(&b);
        assert!(!r.landed && r.error.as_deref().unwrap().contains("not signed by this machine's Nitely app"), "{r:?}");
        assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "Needs You", "back to the person, to approve again");
        f.db.conn.execute("UPDATE issue SET state = 'Approved' WHERE key = ?1", [&b]).unwrap();
        // A signature from another key does not verify either.
        let at = f.db.human_approval_at(&b).unwrap().unwrap();
        let patch = f.db.reviews(&b).unwrap()[0].patch_id.clone();
        let forged = crate::approval::sign(&[9; 32], &crate::approval::message(&b, &patch, at));
        f.db.conn.execute("INSERT INTO approval (issue_key, patch_id, by, ts, signature) VALUES (?1, ?2, 'human', ?3, ?4)", rusqlite::params![b, patch, at, forged]).unwrap();
        assert!(!f.land(&b).landed);
        f.db.conn.execute("UPDATE issue SET state = 'Needs You' WHERE key = ?1", [&b]).unwrap();
        approve(&mut f.db, &b);
        // Submitting the same patch again (after a clean rebase, say) keeps the approval.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        f.submit(&wt_b);
        assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "Approved");
        assert!(f.db.standing_approval(&b).unwrap().is_some());
        // A changed patch needs a new approval.
        std::fs::write(wt_b.join("billing.txt"), "y").unwrap();
        sh(&wt_b, &["commit", "-q", "-am", "round again"]);
        f.submit(&wt_b);
        assert_eq!(f.db.issue(&b).unwrap().unwrap().state, "Needs You");
        assert!(f.db.standing_approval(&b).unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        approve(&mut f.db, &b);
        let r = f.land(&b);
        assert!(r.landed, "{r:?}");
        // A failing after-land step is a warning; the land stands.
        assert!(r.warnings.iter().any(|w| w.contains("exit 3")), "{r:?}");
        assert!(r.approval.unwrap().contains("approved by a human"));
        assert!(f.repo.join("billing.txt").exists());
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn appended_test_files_keep_only_the_original_signed_approval() {
        let mut f = fixture();
        let package = |files: &str| serde_json::json!({"name":"example","scripts":{"test": format!("node --test {files}")}}).to_string();
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        std::fs::write(&pb_path, std::fs::read_to_string(&pb_path).unwrap().replace("pattern = \"billing\"", "pattern = \"package\"")).unwrap();
        std::fs::write(f.repo.join("package.json"), package("out/a.js")).unwrap();
        sh(&f.repo, &["add", "."]);
        sh(&f.repo, &["commit", "-qm", "test command"]);
        let (key, wt) = f.submitted("Additional test", "package.json", &package("out/a.js out/own.js"));
        approve(&mut f.db, &key);
        let reviewed_patch = f.db.reviews(&key).unwrap()[0].patch_id.clone();
        // A preceding land appended a different file to exactly the same line.
        std::fs::write(f.repo.join("package.json"), package("out/a.js out/landed.js")).unwrap();
        sh(&f.repo, &["commit", "-qam", "preceding test"]);
        f.db.conn.execute("DELETE FROM approval WHERE issue_key = ?1", [&key]).unwrap();
        let refused = f.land(&key);
        assert!(!refused.landed && refused.error.as_deref().unwrap().contains("not signed"), "{refused:?}");
        assert_ne!(crate::submit::patch_id(&wt, "main").unwrap(), reviewed_patch, "the textual patch changed");
        let pb = Playbook::load(&f.repo).unwrap();
        let branch = branch_of(&f.repo, &key).unwrap().0;
        assert!(crate::test_command_merge::equivalent(&wt, "main", "HEAD", &f.db.reviews(&key).unwrap()[0]).unwrap());
        // Equivalence does not forgive any additional implementation change.
        std::fs::write(wt.join("unreviewed.txt"), "not reviewed").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-qm", "unreviewed change"]);
        assert!(check_approval(&f.db, &pb, &key, &wt, "main", &branch).unwrap_err().to_string().contains("branch changed"));
        sh(&wt, &["revert", "--no-edit", "HEAD"]);
        approve(&mut f.db, &key);
        let landed = f.land(&key);
        assert!(landed.landed, "{landed:?}");
        let result: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(f.repo.join("package.json")).unwrap()).unwrap();
        assert_eq!(result["scripts"]["test"], "node --test out/a.js out/landed.js out/own.js");
        assert_eq!(f.db.reviews(&key).unwrap().len(), 1, "no new review or synthetic approval");
        assert!(landed.gate.unwrap().passed, "the combined integration candidate still runs its gate");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A land handed in after another issue waits until that issue is Done, then lands as `land`
    /// does; one whose rebase conflicts stops with the conflict said and its issue still open; and
    /// a blocker closed without landing ends the wait (NIT-456).
    /// A land run directly (`--now`, or `land` by hand) ends the issue's waiting `land --after`
    /// jobs, and a waiting job whose own issue is closed meanwhile ends too (NIT-513).
    #[test]
    fn a_direct_land_or_a_closed_issue_ends_its_waiting_land_jobs() {
        let mut f = fixture();
        let dir = f.repo.to_string_lossy().into_owned();
        let (blocker, _) = f.submitted("Blocker", "blocker.txt", "x");
        let (a, _) = f.submitted("A", "a.txt", "x");
        let job = crate::land_job::enqueue(&f.db, &a, std::slice::from_ref(&blocker), &dir, &f.actor).unwrap();
        assert!(f.land(&a).landed, "landed directly, its blocker still open");
        let ended = f.db.land_job(job.id).unwrap().unwrap();
        assert_eq!(ended.state, "replaced");
        assert!(ended.outcome().contains("landed directly instead"), "{}", ended.outcome());

        let (b, _) = f.submitted("B", "b.txt", "x");
        let job = crate::land_job::enqueue(&f.db, &b, std::slice::from_ref(&blocker), &dir, &f.actor).unwrap();
        f.db.move_issue(&b, "Canceled", None, Some("not needed"), &f.actor).unwrap();
        match crate::land_job::wait_for_blockers(&f.db, job.id).unwrap() {
            crate::land_job::Ready::Over(j) => assert_eq!(j.state, "replaced", "{j:?}"),
            crate::land_job::Ready::Go(_) => panic!("a closed issue does not land"),
        }
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_land_after_another_waits_for_it_then_lands_or_stops_cleanly() {
        std::env::set_var("NITELY_LAND_JOB_POLL_MS", "50");
        let mut f = fixture();
        let dir = f.repo.to_string_lossy().into_owned();
        let path = f.base.join("t.db");
        let (first, _) = f.reviewed("Header", "header.txt", "x");
        let (second, _) = f.reviewed("Footer", "footer.txt", "x");
        assert_eq!(crate::land_job::waits_for(&f.db, &second, std::slice::from_ref(&first)).unwrap(), [first.clone()]);
        let job = crate::land_job::enqueue(&f.db, &second, std::slice::from_ref(&first), &dir, &f.actor).unwrap();
        // Its process: waits, then lands.
        let id = job.id;
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut db = Db::open(&path).unwrap();
                match crate::land_job::wait_for_blockers(&db, id).unwrap() {
                    crate::land_job::Ready::Go(_) => crate::land_job::land_now(&mut db, &crate::Config::default(), id).unwrap(),
                    crate::land_job::Ready::Over(j) => j,
                }
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(f.db.land_job(id).unwrap().unwrap().state, "waiting");
        assert_eq!(state(&f, &second), "In Review", "nothing lands before what it waits for");
        assert!(f.land(&first).landed);
        let done = waiter.join().unwrap();
        assert_eq!(done.state, "done", "{done:?}");
        assert!(done.outcome().contains("landed after"), "{}", done.outcome());
        assert_eq!(state(&f, &second), "Done");
        assert_eq!(f.db.undelivered_land_jobs(f.actor.id()).unwrap().len(), 1, "told once, on the agent's next call");

        // Behind a change to the same file: the rebase conflicts, and it stops cleanly.
        let (third, _) = f.reviewed("Menu", "menu.txt", "a");
        let (fourth, _) = f.reviewed("Menu too", "menu.txt", "b");
        let job = crate::land_job::enqueue(&f.db, &fourth, std::slice::from_ref(&third), &dir, &f.actor).unwrap();
        assert!(f.land(&third).landed);
        let mut db = Db::open(&path).unwrap();
        assert!(matches!(crate::land_job::wait_for_blockers(&db, job.id).unwrap(), crate::land_job::Ready::Go(_)));
        let stopped = crate::land_job::land_now(&mut db, &crate::Config::default(), job.id).unwrap();
        assert_eq!(stopped.state, "done");
        assert!(stopped.outcome().contains("stopped"), "{}", stopped.outcome());
        assert_ne!(state(&f, &fourth), "Done");

        // What it waits for is closed without landing: the wait ends, and says why.
        let (fifth, _) = f.reviewed("Fifth", "fifth.txt", "x");
        let (sixth, _) = f.reviewed("Sixth", "sixth.txt", "x");
        let job = crate::land_job::enqueue(&f.db, &sixth, std::slice::from_ref(&fifth), &dir, &f.actor).unwrap();
        f.db.move_issue(&fifth, "Canceled", None, Some("not needed"), &f.actor).unwrap();
        match crate::land_job::wait_for_blockers(&db, job.id).unwrap() {
            crate::land_job::Ready::Over(j) => assert!(j.state == "failed" && j.error.as_deref().unwrap_or("").contains("without landing"), "{j:?}"),
            crate::land_job::Ready::Go(_) => panic!("it does not land after a canceled issue"),
        }
        assert_eq!(state(&f, &sixth), "In Review");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// What lands leave and what a later land tidies (NIT-309, NIT-340, NIT-370, NIT-460): a
    /// worktree landed with --keep stays; one a process still uses is kept whole and said on the
    /// issue; a link in a worktree goes, never what it points at; and the next land removes the
    /// leftovers of closed issues nothing keeps, leaving what is a person's to look at.
    #[test]
    fn later_lands_tidy_what_earlier_ones_left_and_spare_what_is_kept_used_or_linked() {
        let mut f = fixture();
        // node_modules is ignored, as in a real repo: git removes a worktree that holds ignored files.
        std::fs::create_dir_all(f.repo.join(".git").join("info")).unwrap();
        std::fs::write(f.repo.join(".git").join("info").join("exclude"), "node_modules\n").unwrap();
        let dir = f.repo.to_string_lossy().into_owned();

        // Landed with --keep: kept, and said so for later lands.
        let (a, kept) = f.reviewed("Kept", "kept.txt", "x");
        let opts = LandOptions { dir: &dir, issue_key: &a, push: true, keep: true, break_lock: false };
        assert!(land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap().landed);

        // The person's own node_modules, which an agent linked its worktree's to (NIT-460).
        let outside = crate::unlink::tests::outside(&f.base);
        let (l, linked) = f.reviewed("Linked deps", "linked.txt", "x");
        crate::unlink::tests::link_folder(&linked.join("node_modules"), &outside);
        let r = f.land(&l);
        assert!(r.landed && !linked.exists(), "{r:?}");
        assert!(crate::unlink::tests::intact(&outside), "the land removed the link, not what it points at: {r:?}");

        let (p, pulse) = f.reviewed("Pulse view", "pulse.txt", "x");
        // A dev server started in the worktree, as the browser preview asks agents to do. It runs
        // until the test stops it (at most ten minutes): on a busy machine the land below can take minutes, and a server
        // that ended first would rightly leave the worktree free (NIT-309).
        let mut server = std::process::Command::new(playbook::posix_sh()).arg("-c").arg(format!("sleep 600; : {}", pulse.display())).spawn().unwrap();
        // Land sees it the way this does; wait until it shows (NIT-122).
        assert!(crate::procs::tests::held_by(&pulse, server.id()).iter().any(|h| h.pid == server.id()), "the server is listed");
        let r = f.land(&p);
        server.kill().ok();
        server.wait().ok();
        assert!(r.landed, "{r:?}");
        assert!(r.held_by.iter().any(|h| h.pid == server.id()), "{r:?}");
        assert!(r.cleanup.as_deref().unwrap().contains("kept"), "{r:?}");
        let listed = crate::paths::normalize(&sh(&f.repo, &["worktree", "list"]));
        assert!(pulse.exists() && listed.contains(&crate::paths::normalize(&pulse.to_string_lossy())), "kept whole and still registered");
        // Said on the issue, so a later land tries again (NIT-340).
        assert!(f.db.timeline(&p).unwrap().iter().any(|t| t.kind == "worktree_held"), "the keep is on the issue");

        // Closed without a land: its branch merged by hand, the issue canceled.
        let (c, merged) = f.committed("Merged by hand", "hand.txt", "y");
        let branch = sh(&merged, &["rev-parse", "--abbrev-ref", "HEAD"]).trim().to_string();
        sh(&f.repo, &["merge", "-q", "--ff-only", &branch]);
        f.db.move_issue(&c, "Canceled", None, Some("merged by hand"), &f.actor).unwrap();
        let held = |f: &Fixture, key: &str, path: &Path| {
            f.db.log_event(&f.actor, Some(key), Some(&path.to_string_lossy()), "worktree_held", "Worktree kept after landing").unwrap();
        };
        // What a land that git stopped halfway leaves: a folder git no longer lists, its .git file
        // naming this repo's record of it, now gone; one of them with a link in it.
        let left = |name: &str| {
            let stray = merged.parent().unwrap().join(name);
            std::fs::create_dir_all(stray.join("target")).unwrap();
            std::fs::write(stray.join(".git"), format!("gitdir: {}\n", f.repo.join(".git").join("worktrees").join(name).display())).unwrap();
            stray
        };
        let stray = left("nit-stray-left-behind");
        held(&f, &c, &stray);
        let stray_linked = left("nit-stray-linked");
        crate::unlink::tests::link_folder(&stray_linked.join("target").join("deps"), &outside);
        held(&f, &l, &stray_linked);
        // One that says nothing of where it came from, and a clone made where one was removed:
        // both are a person's to look at.
        let unknown = merged.parent().unwrap().join("nit-stray-unknown");
        std::fs::create_dir_all(unknown.join("target")).unwrap();
        held(&f, &c, &unknown);
        // An empty one is nobody's work, wherever it came from (NIT-370).
        let empty = merged.parent().unwrap().join("nit-stray-empty");
        std::fs::create_dir_all(&empty).unwrap();
        held(&f, &c, &empty);
        let clone = merged.parent().unwrap().join("nit-stray-reused");
        held(&f, &c, &clone);
        f.db.log_event(&f.actor, Some(&c), Some(&clone.to_string_lossy()), "worktree_removed", "Removed").unwrap();
        crate::test_repo::init(&clone, &[("work.txt", "mine")]);

        let pb = Playbook::load(&f.repo).unwrap();
        let found = crate::leftovers::leftovers(&f.db, &f.repo, &pb, false, &Actor::Human).unwrap();
        let of = |p: &Path| found.iter().find(|l| crate::paths::normalize(&l.worktree.to_string_lossy()) == crate::paths::normalize(&p.to_string_lossy())).cloned();
        assert!(of(&kept).and_then(|l| l.stays).is_some_and(|s| s.contains("on purpose")), "{found:?}");
        // The server is gone now: listed as going.
        assert!(found.iter().any(|l| l.issue == p && l.stays.is_none()), "{found:?}");
        assert!(of(&merged).is_some_and(|l| l.stays.is_none()), "{found:?}");
        assert!(of(&stray).is_some_and(|l| l.stays.is_none()), "{found:?}");
        assert!(of(&unknown).and_then(|l| l.stays).is_some_and(|s| s.contains("by hand")), "{found:?}");
        assert!(of(&empty).is_some_and(|l| l.stays.is_none()), "{found:?}");
        assert!(of(&clone).is_none(), "removed since it was kept: not a leftover {found:?}");

        let (b, _) = f.reviewed("Next", "next.txt", "z");
        let r = f.land(&b);
        assert!(r.landed, "{r:?}");
        assert!(kept.exists(), "--keep holds at the next land too");
        assert!(!merged.exists() && !stray.exists() && !empty.exists() && !stray_linked.exists() && !pulse.exists(), "{r:?}");
        assert!(unknown.exists() && clone.join("work.txt").exists(), "{r:?}");
        assert!(crate::unlink::tests::intact(&outside), "the cleanup removed the link, not what it points at: {r:?}");
        assert!(r.tidied.iter().any(|t| t.contains(&p)), "{r:?}");
        assert_eq!(r.tidied.len(), 5, "{r:?}");
        assert!(f.db.timeline(&p).unwrap().iter().any(|t| t.kind == "worktree_removed"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn refuses_changes_after_review_conflicts_and_a_held_lock() {
        let mut f = fixture();
        let (a, wt) = f.reviewed("Edit shared", "shared.txt", "two\n");

        // A commit after the review is not what was reviewed.
        std::fs::write(wt.join("extra.txt"), "x").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "extra"]);
        let r = f.land(&a);
        assert!(r.error.as_deref().unwrap().contains("changed since its review"), "{r:?}");
        crate::test_repo::reviewed(&mut f.db, &wt, &a, &f.actor);

        // Someone else lands a conflicting edit first.
        std::fs::write(f.repo.join("shared.txt"), "three\n").unwrap();
        sh(&f.repo, &["commit", "-q", "-am", "conflicting"]);
        let r = f.land(&a);
        assert_eq!(r.conflicts, vec!["shared.txt"], "{r:?}");
        assert!(rebase_in_progress(&wt));
        sh(&wt, &["rebase", "--abort"]);

        // Held by a live process (this one).
        let lock = f.repo.join(".git").join(LOCK_FILE);
        let held = format!("NIT-9 pid {}", std::process::id());
        std::fs::write(&lock, &held).unwrap();
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: false, keep: true, break_lock: false };
        let err = land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        assert!(err.contains("another land holds"), "{err}");
        // Said on the issue, with the repo, so the app shows what the land waits behind (NIT-335).
        let refused = f.db.timeline(&a).unwrap().into_iter().rev().find(|t| t.kind == "land_refused").expect("a land_refused event");
        assert!(refused.summary.as_deref().is_some_and(|s| s.contains(&held)), "{refused:?}");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A land that died with the machine leaves its lock, and a git that died mid-command leaves
    /// the main checkout's index.lock. The next land takes the lock over by itself, and says what
    /// the index.lock is instead of a refused fast-forward (NIT-280).
    #[test]
    fn a_dead_lands_lock_is_taken_over_and_a_left_index_lock_is_named() {
        let mut f = fixture();
        let lock = f.repo.join(".git").join(LOCK_FILE);
        std::fs::write(&lock, "NIT-9 pid 4000000000 since 2026-10-18T05:06:53Z\n").unwrap();
        let (a, _) = f.reviewed("Dead lock", "dead.txt", "x");
        let r = f.land(&a);
        // With no origin, the land is local: nothing is pushed.
        assert!(r.landed && r.pushed.is_none(), "{r:?}");
        assert!(r.warnings.iter().any(|w| w.contains("took over the land lock NIT-9")), "{r:?}");
        assert!(!lock.exists(), "released after the land");

        let (b, _) = f.reviewed("Left index lock", "left.txt", "x");
        let index_lock = f.repo.join(".git").join("index.lock");
        let left = std::fs::File::create(&index_lock).unwrap();
        left.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600)).unwrap();
        drop(left);
        let r = f.land(&b);
        let err = r.error.as_deref().unwrap_or_default();
        assert!(!r.landed && err.contains("index.lock") && err.contains("before this land began") && err.contains("delete that file"), "{r:?}");
        assert!(index_lock.exists(), "never deleted by land: a git may still be running there");

        // One a git made a moment ago is that git's, still running.
        std::fs::write(&lock, format!("NIT-9 pid {}", std::process::id())).unwrap();
        std::fs::remove_file(&index_lock).unwrap();
        std::fs::write(&index_lock, "").unwrap();
        assert_eq!(left_index_lock(&f.repo, INDEX_LOCK_GRACE_SECS), None);
        assert!(refused_fast_forward(&f.repo).contains("a git command is running there"));
        std::fs::remove_file(&index_lock).unwrap();
        assert!(refused_fast_forward(&f.repo).contains("uncommitted edits"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A repo whose `engine` folder is a submodule with no remote of its own (its only remote, an
    /// upstream, is gone), declared to land with the repo.
    fn submodule_fixture() -> Fixture {
        let base = crate::paths::unique_temp("nitely-land-sub", "");
        let upstream = base.join("engine-upstream");
        crate::test_repo::init(&upstream, &[("core.c", "int core;\n")]);
        let repo = base.join("repo");
        let playbook = PLAYBOOK.replace("{sh}", &playbook::posix_sh()) + "[[submodules]]\npath = \"engine\"\ncheckout = \"every_task\"\n";
        crate::test_repo::init(&repo, &[(playbook::PLAYBOOK_PATH, &playbook), ("shared.txt", "one\n")]);
        sh(&repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &upstream.to_string_lossy(), "engine"]);
        sh(&repo, &["commit", "-q", "-m", "the engine"]);
        sh(&repo.join("engine"), &["remote", "remove", "origin"]);
        fixture_from(base)
    }

    impl Fixture {
        fn engine(&self) -> PathBuf {
            self.repo.join("engine")
        }

        /// Starts an issue whose change edits both the repo and its engine, committed in both, not
        /// submitted.
        fn spanning(&mut self, title: &str) -> (String, PathBuf) {
            let issue = self
                .db
                .create_issue(
                    &NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "none yet".into(), source: "human".into(), allow_similar: true, ..Default::default() },
                    &self.actor,
                )
                .unwrap();
            let started = playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap();
            let wt = PathBuf::from(&started.worktree);
            assert!(wt.join("engine/.git").is_file(), "the engine is a worktree of the repo's copy");
            assert_eq!(sh(&wt.join("engine"), &["branch", "--show-current"]), started.branch, "the same task branch in the engine");
            std::fs::write(wt.join("engine/core.c"), "int core;\nint faster;\n").unwrap();
            sh(&wt.join("engine"), &["commit", "-q", "-am", "engine: faster"]);
            std::fs::write(wt.join("shared.txt"), "one\nuses faster\n").unwrap();
            sh(&wt, &["add", "shared.txt", "engine"]);
            sh(&wt, &["commit", "-q", "-m", title]);
            (issue.key, wt)
        }

        /// Another change lands meanwhile, moving the engine's main and the repo's pointer in step.
        fn land_elsewhere(&self) {
            std::fs::write(self.engine().join("other.c"), "int other;\n").unwrap();
            sh(&self.engine(), &["add", "other.c"]);
            sh(&self.engine(), &["commit", "-q", "-m", "engine: other"]);
            std::fs::write(self.repo.join("other.txt"), "x\n").unwrap();
            sh(&self.repo, &["add", "other.txt", "engine"]);
            sh(&self.repo, &["commit", "-q", "-m", "other"]);
        }
    }

    #[test]
    fn a_change_that_spans_a_submodule_lands_both_branches_together() {
        let mut f = submodule_fixture();
        let (key, wt) = f.spanning("Faster engine");
        crate::test_repo::reviewed(&mut f.db, &wt, &key, &f.actor);
        f.land_elsewhere();

        // First the repo cannot move (NIT-528 merged when_the_repo_cannot_move... in here). The
        // engine's copy detached at main, as `git submodule update` leaves it: main moves by itself
        // there, and the checkout follows it. An edit in the main checkout that the change overlaps
        // makes the repo's fast-forward fail after the engine's went through: both are put back.
        sh(&f.engine(), &["checkout", "-q", "--detach", "main"]);
        let (repo_before, engine_before) = (sh(&f.repo, &["rev-parse", "main"]), sh(&f.engine(), &["rev-parse", "main"]));
        let shared = std::fs::read_to_string(f.repo.join("shared.txt")).unwrap();
        std::fs::write(f.repo.join("shared.txt"), format!("{shared}mine, not committed\n")).unwrap();
        let stopped = f.land(&key);
        assert!(!stopped.landed);
        let error = stopped.error.unwrap_or_default();
        assert!(error.contains("refused the fast-forward") && error.contains("put back, so nothing moved"), "{error}");
        assert_eq!(sh(&f.repo, &["rev-parse", "main"]), repo_before);
        assert_eq!(sh(&f.engine(), &["rev-parse", "main"]), engine_before, "the engine's main went back");
        assert_eq!(sh(&f.engine(), &["rev-parse", "HEAD"]), engine_before, "its detached checkout too");
        assert!(wt.join("engine/.git").exists(), "the task's worktrees stay for another try");
        std::fs::write(f.repo.join("shared.txt"), shared).unwrap();

        // With the edit gone, the same land goes through.
        let landed = f.land(&key);
        assert!(landed.landed, "{landed:?}");
        assert_eq!(sh(&f.engine(), &["rev-parse", "HEAD"]), sh(&f.engine(), &["rev-parse", "main"]), "the detached checkout followed main");
        // The engine's main has both changes, its checkout too; the repo points at its tip.
        let engine_main = sh(&f.engine(), &["rev-parse", "main"]);
        assert_eq!(sh(&f.engine(), &["log", "--format=%s", "-2", "main"]), "engine: faster\nengine: other");
        assert_eq!(std::fs::read_to_string(f.engine().join("core.c")).unwrap().replace('\r', ""), "int core;\nint faster;\n");
        assert!(sh(&f.repo, &["ls-tree", "HEAD", "engine"]).contains(&engine_main), "the repo points at the engine's new main");
        assert_eq!(std::fs::read_to_string(f.repo.join("shared.txt")).unwrap().replace('\r', ""), "one\nuses faster\n");
        assert!(sh(&f.repo, &["status", "--porcelain"]).is_empty(), "the main checkout is clean, its engine where it points");
        // The rebase kept the review: the engine's code is the same, only its commit ids moved.
        assert!(landed.approval.unwrap_or_default().contains("review passed"));
        // Both task worktrees and branches are gone.
        assert!(!wt.exists(), "{:?}", landed.cleanup);
        assert!(sh(&f.engine(), &["branch", "--list", &landed.branch]).is_empty());
        assert!(!sh(&f.engine(), &["worktree", "list"]).contains("wt"));
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn a_change_that_leaves_the_submodule_alone_lands_as_any_other() {
        let mut f = submodule_fixture();
        let (key, wt) = f.reviewed("Words only", "words.txt", "w
");
        assert!(wt.join("engine/.git").is_file());
        // Another land moves the pointer meanwhile: this change still does not move the engine.
        f.land_elsewhere();
        let engine_main = sh(&f.engine(), &["rev-parse", "main"]);
        let landed = f.land(&key);
        assert!(landed.landed, "{landed:?}");
        assert_eq!(sh(&f.engine(), &["rev-parse", "main"]), engine_main, "the engine's main stays where the other land put it");
        assert!(sh(&f.repo, &["ls-tree", "HEAD", "engine"]).contains(&engine_main));
        assert!(!wt.exists(), "{:?}", landed.cleanup);
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn submit_refuses_submodule_work_the_branch_does_not_point_at() {
        let mut f = submodule_fixture();
        let (_, wt) = f.spanning("Faster engine");
        std::fs::write(wt.join("engine/core.c"), "int core;\nint faster;\nint more;\n").unwrap();
        sh(&wt.join("engine"), &["commit", "-q", "-am", "engine: more"]);
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
        let err = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        assert!(err.contains("does not point at") && err.contains("git add engine"), "{err}");
        std::fs::write(wt.join("engine/core.c"), "int core;\n").unwrap();
        let err = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"), "{err}");
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// The integration branch rolls the submodule back to an older commit: a rebased task points at
    /// it while its checkout stays at the newer one, which is on the submodule's integration branch,
    /// so no work of the task. Submit says to move the checkout back, not to `git add` it (NIT-357).
    #[test]
    fn submit_says_to_move_a_submodule_checkout_back_after_a_rollback() {
        let mut f = submodule_fixture();
        f.land_elsewhere();
        let (_, wt) = f.committed("Words only", "words.txt", "w
");
        let older = sh(&f.engine(), &["rev-parse", "HEAD~1"]);
        sh(&f.engine(), &["checkout", "-q", &older]);
        sh(&f.repo, &["add", "engine"]);
        sh(&f.repo, &["commit", "-q", "-m", "roll the engine back"]);
        sh(&wt, &["rebase", "-q", "main"]);
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
        let err = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        let fix = format!("git -C engine reset --keep {}", &older[..12]);
        assert!(err.contains("the branch did not change it") && err.contains(&fix) && err.contains("Do not `git add engine`"), "{err}");
        sh(&wt.join("engine"), &["reset", "-q", "--keep", &older]);
        if let Err(e) = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor) {
            assert!(!e.to_string().contains("submodule engine"), "{e:#}");
        }
        std::fs::remove_dir_all(&f.base).ok();
    }

    /// A rebase onto an integration branch that moved the submodule leaves the task's checkout
    /// behind the pointer it brought in: submit says to bring the checkout up to it, not to
    /// `git add` the older commit back (NIT-343).
    #[test]
    fn submit_says_to_update_a_submodule_checkout_a_rebase_left_behind() {
        let mut f = submodule_fixture();
        let (_, wt) = f.committed("Words only", "words.txt", "w\n");
        f.land_elsewhere();
        sh(&wt, &["rebase", "-q", "main"]);
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
        let err = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        let main = sh(&f.engine(), &["rev-parse", "main"]);
        let fix = format!("git -C engine merge --ff-only {}", &main[..12]);
        assert!(err.contains("the branch did not change it") && err.contains(&fix) && err.contains("Do not `git add engine`"), "{err}");
        sh(&wt.join("engine"), &["merge", "-q", "--ff-only", &main]);
        if let Err(e) = submit(&mut f.db, &crate::Config::default(), &opts, &f.actor) {
            assert!(!e.to_string().contains("submodule engine"), "{e:#}");
        }
        std::fs::remove_dir_all(&f.base).ok();
    }
}
