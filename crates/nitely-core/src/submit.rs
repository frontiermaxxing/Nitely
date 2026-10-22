//! `submit`: a finished branch becomes a reviewed pull request. It runs the gate, has the other
//! vendor review the branch, decides whether a human must approve it, pushes, opens or updates the
//! pull request, records the review for the exact patch so landing can check it, and moves the
//! issue, as the repo's playbook says.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use rusqlite::OptionalExtension;
use std::path::Path;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::playbook::{self, git, git_remote, GateResult, Playbook};
use crate::project_modes::ItemReview;
use crate::review::{Finding, PreviousRound, Review};

/// The submitter's explanation, captured with the revision handed in, never guessed from an issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SolutionSummary {
    pub title: String,
    pub summary: String,
}

impl SolutionSummary {
    pub fn from_fields(title: Option<&str>, summary: Option<&str>) -> Result<Option<Self>> {
        match (title, summary) {
            (None, None) => Ok(None),
            (Some(t), Some(s)) if !t.trim().is_empty() && !s.trim().is_empty() && t.len() <= 200 && s.len() <= 8000 =>
                Ok(Some(Self { title: t.trim().into(), summary: s.trim().into() })),
            _ => bail!("provide both solution_title (1–200 bytes) and solution_summary (1–8000 bytes)"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Submitted {
    /// Shadow advice only; never replaces the gate/review result below.
    pub verification: Option<crate::verification::Plan>,
    pub issue: String,
    pub branch: String,
    pub base: String,
    /// `git patch-id` of the branch's diff: the review applies to this patch and no other.
    pub patch_id: String,
    pub commit: String,
    pub files: usize,
    pub gate: GateResult,
    pub solution: Option<SolutionSummary>,
    /// None when the gate failed (nothing else ran) or the review was skipped.
    pub review: Option<Review>,
    /// Deliberately disabled by project policy, distinct from an ad-hoc skip.
    pub review_disabled: bool,
    /// Its review waits for its land queue batch (review_batch.rs, NIT-509).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub review_batched: bool,
    /// Project YOLO bypass; neither a gate nor review pass.
    pub yolo: bool,
    /// The quick path (quick.rs): a small change, checked by its own checks, reviewed after landing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quick: bool,
    /// `agent` when the review is enough, `human` when it waits for approval.
    pub approver: String,
    pub reasons: Vec<String>,
    /// The human approved this exact patch before, and this review raised nothing new, so the
    /// approval stands and the issue stays Approved.
    pub approval_stands: bool,
    pub pushed: bool,
    pub pr_url: Option<String>,
    pub pr_error: Option<String>,
    /// The land Nitely ran itself, when the playbook's `[land] by` lands without the author.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub land: Option<crate::land::Landed>,
    /// The issue's state afterwards.
    pub state: String,
    pub next: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewRecord {
    pub issue_key: String,
    pub patch_id: String,
    pub commit_sha: String,
    /// The branch handed in. Legacy reviews cannot reliably identify their pull request.
    pub branch: Option<String>,
    pub reviewer: String,
    pub verdict: String,
    pub approver: String,
    pub reasons: Vec<String>,
    pub findings: Vec<crate::review::Finding>,
    pub ts: i64,
    /// The newest event when it was recorded (NIT-544): what orders it against a decision's event.
    /// None for reviews recorded before that.
    pub after_event: Option<i64>,
    /// As `Review::note`: how independent the review was, e.g. `same vendor: codex not installed`.
    pub note: String,
    /// The review batch it was reviewed in, with other changes in one reviewer call (NIT-216).
    pub batch_id: Option<i64>,
    /// These fields describe this record’s commit only. Legacy records have neither.
    pub solution: Option<SolutionSummary>,
    pub gate: Option<GateResult>,
}

/// A decision about an issue (an approval, a hand-back, an answer to a finding): its event, when it
/// has one, and its time (NIT-544).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub id: Option<i64>,
    pub ts: i64,
}

impl Decision {
    /// Whether review `r` came after this decision: it saw the decision's event when both are known
    /// (so two in the same second are told apart), else by whole seconds as before.
    pub fn came_before(&self, r: &ReviewRecord) -> bool {
        match (self.id, r.after_event) {
            (Some(id), Some(seen)) => seen >= id,
            _ => r.ts > self.ts,
        }
    }
}

pub struct SubmitOptions<'a> {
    /// The worktree, or any directory inside it.
    pub dir: &'a str,
    /// Defaults to the issue the branch names.
    pub issue_key: Option<&'a str>,
    /// Skip the review; the change then waits for a human.
    pub skip_review: bool,
    /// Skip the push and pull request (for a dry run or a repo without a remote).
    pub local_only: bool,
    /// May wait for a review batch when the playbook batches reviews: only a submit that runs in
    /// the background, where nobody waits on it.
    pub batch: bool,
    pub solution: Option<&'a SolutionSummary>,
    /// Async jobs bind the supplied text to the head captured at enqueue.
    pub solution_commit: Option<&'a str>,
    pub solution_patch_id: Option<&'a str>,
}

/// Refuses to submit work the issue marks as outside the repo while it has no commits of its own
/// (NIT-551): there is no change to gate or review.
pub fn refuse_outside_repo(issue: &IssueRecord, has_commits: bool) -> Result<()> {
    if crate::overlap::outside_repo(&issue.body, has_commits) {
        bail!("{} changes no repo files, so there is nothing to submit. {}", issue.key, crate::overlap::OUTSIDE_REPO);
    }
    Ok(())
}

pub fn submit(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor) -> Result<Submitted> {
    submit_as(db, config, opts, false, actor)
}

/// `submit` for the quick path (quick.rs), once the change is found small: its own checks, then
/// no review, push or pull request; the review comes after landing.
pub fn submit_quick(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor) -> Result<Submitted> {
    submit_as(db, config, opts, true, actor)
}

fn submit_as(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, quick: bool, actor: &Actor) -> Result<Submitted> {
    let (worktree, branch) = playbook::worktree_and_branch(opts.dir)?;
    let (_, branch_key) = crate::git::parse_branch(&branch);
    let root = playbook::repo_root(config, opts.dir).with_context(|| format!("{} is not inside a git repo", opts.dir))?;
    let mut pb = Playbook::load(&root)?;
    let handed_in = opts.solution_commit.map(str::to_string).unwrap_or(git(&worktree, &["rev-parse", "HEAD"])?.trim().to_string());
    let base = pb.integration_branch().to_string();
    let explained_patch = if opts.solution.is_some() {
        Some(match opts.solution_patch_id {
            Some(patch) => patch.to_string(),
            None => patch_id_range(&worktree, &playbook::diff_base(&worktree, &base), &handed_in)?,
        })
    } else { None };
    if branch.is_empty() {
        bail!("{} has no commit on its branch yet; submit from the worktree start_task made", worktree.display());
    }
    if branch == base || branch == "HEAD" {
        bail!("{branch} is not a task branch; submit from the worktree start_task made");
    }
    let key = opts.issue_key.map(str::to_string).or(branch_key.clone()).context("the branch names no issue; pass the issue key")?;
    let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
    if crate::overlap::changes_no_repo_files(&issue.body) {
        // Only the issue's own branch counts: another task's worktree has commits of its own. A
        // branch whose pattern names no key is taken at the key given.
        let own = branch_key.as_deref().is_none_or(|k| k.eq_ignore_ascii_case(&issue.key));
        let ahead = git(&worktree, &["rev-list", "--count", &format!("{}..HEAD", playbook::diff_base(&worktree, &base))])?;
        refuse_outside_repo(&issue, own && ahead.trim() != "0")?;
    }
    crate::project_modes::apply_issue(db, &key, &mut pb)?;
    let _telemetry = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project: issue.project.clone(), issue: Some(key.clone()), ..Default::default() });
    if rebase_in_progress(&worktree) {
        bail!("a rebase is in progress in {}", worktree.display());
    }
    let dirty = git(&worktree, &["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{} has uncommitted changes; commit them first:\n{}", worktree.display(), dirty.trim_end());
    }
    // An item of a goal with a goal branch has no pull request of its own: its hop is the land into
    // the goal branch, with item checks and the item review the mode asks for (goal_branch.rs).
    if let Some(gb) = crate::goal_branch::route_of_issue(db, &root, &key, Some(&branch))? {
        if quick {
            bail!("{key} is an item of a goal that lands through {}: submit it", gb.branch);
        }
        return submit_into_goal(db, config, &issue, &root, &worktree, &branch, &gb.branch, opts.local_only, actor);
    }
    crate::submodules::check_pointers(&pb, &worktree)?;
    let wt = worktree.to_string_lossy().into_owned();
    let no_origin = !opts.local_only && !playbook::has_origin(&worktree);
    let local_only = opts.local_only || no_origin;
    if !local_only {
        // Best effort: measure against the remote's base, so work merged since the last pull isn't counted.
        git_remote(&worktree, &["fetch", "--quiet", "origin", &base]).ok();
    }
    let since = playbook::diff_base(&worktree, &base);
    if git(&worktree, &["rev-list", "--count", &format!("{since}..HEAD")])?.trim() == "0" {
        bail!("{branch} has no changes against {base}; commit your work first");
    }

    let checked_head = git(&worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    let files: Vec<String> = git(&worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    pb.approval.check_blocked(&files)?;
    crate::private_terms::check(issue.project.as_deref(), &worktree, &since)?;
    let mut verification = crate::verification::assess_and_record(db, &worktree, &base, &checked_head, &issue,actor)?;
    let gate = playbook::run_automatic_gate(db, &root, &worktree, &[&issue.key], actor, crate::lanes::Kind::Gate)?;
    if gate.skipped.is_some() {
        verification.enforcement="Project mode: automatic tests did not run; classification is advice only.".into();
        verification.commands.clear();
    } else if !verification.shadow && gate.plan.is_none() {
        verification.enforcement="Full configured gate used: reduced checks were unavailable or the post-merge baseline was missing, overdue or unhealthy.".into();
        verification.commands.clear();
    }
    crate::verification::record(db,&issue,&verification,actor)?;
    if let Some(note)=crate::verification::checks_note(&gate) {db.add_note(&issue.key,&note,"verification_checks",actor)?;}
    let submitted_patch = patch_id_range(&worktree, &since, &gate.commit)?;
    let gate_ran_on_it = git(&worktree, &["rev-parse", "--verify", &format!("{}^{{commit}}", gate.commit)])?.trim() == handed_in;
    let mut out = Submitted {
        verification: Some(verification),
        issue: issue.key.clone(),
        branch: branch.clone(),
        base: base.clone(),
        // Of the commit the gate ran on, as the review and its reuse are: one made meanwhile is not covered.
        patch_id: submitted_patch.clone(),
        commit: gate.commit.clone(),
        files: files.len(),
        solution: opts.solution.filter(|_| crate::submit_rules::solution_applies(gate_ran_on_it, explained_patch.as_deref(), &submitted_patch)).cloned(),
        gate,
        review: None,
        review_disabled: false,
        review_batched: false,
        yolo: false,
        quick: false,
        approver: "human".into(),
        reasons: vec![],
        approval_stands: false,
        pushed: false,
        pr_url: None,
        pr_error: None,
        land: None,
        state: issue.state.clone(),
        next: String::new(),
    };
    if !out.gate.passed && out.gate.skipped.is_none() {
        out.next = after_failed_gate(&out.gate);
        // A goal's checkpoint: the item at fault is found, reverted on the goal branch and sent
        // back, and the next checkpoint opened (goal_branch.rs).
        if let Some(said) = crate::goal_branch::after_failed_gate(db, &root, &issue.key, &out.gate, actor) {
            db.add_note(&issue.key, &said, "goal_branch", actor)?;
            out.next = format!("{said}. {}", out.next);
            out.state = db.issue(&issue.key)?.map(|i| i.state).unwrap_or_default();
        }
        return Ok(out);
    }

    // A queued gate may outlive a human settings change. Read the authoritative policy again.
    let mut pb = Playbook::load(&root)?;
    crate::project_modes::apply_issue(db, &issue.key, &mut pb)?;
    // A goal item on main still gets the blocking item review its mode asks for, though changes
    // with no goal land unreviewed (NIT-563).
    let item_blocking = issue.goal.is_some() && item_review_of(db, &issue.key)? == ItemReview::Blocking;
    if item_blocking {
        pb.review.enabled = true;
    }
    pb.review.policy_root = Some(root.clone());
    // Guarded by that item review then, not by the review of changes with no goal (NIT-597 review).
    pb.review.yolo_guards = crate::project_yolo::ReviewGuard::for_issue(db, &issue.key)?.map(|g| crate::project_yolo::ReviewGuard { item: item_blocking, ..g }).into_iter().collect();
    crate::project_modes::check_gate_authority(db, &[&issue.key], &out.gate)?;
    pb.approval.check_blocked(&files)?;
    out.reasons = pb.human_reasons(&files)?;
    // A blocking item review is never skipped, YOLO or not (NIT-597 review, round 2).
    if quick {
        if crate::quick::limits_for_issue(db, &issue.key)?.is_none() {
            bail!("the quick path is off under the project's review mode; submit it for review");
        }
        out.quick = true;
    } else if !item_blocking && crate::project_yolo::enabled_for_issue(db, &issue.key)? {
        out.yolo = true;
    } else if !pb.review.enabled {
        out.review_disabled = true;
    } else if opts.skip_review {
        out.reasons.push("no review ran".into());
    } else {
        let reviewed = (|| -> Result<()> {
            pb.review.require_enabled()?;
            let (vendor, _) = crate::git::parse_branch(&branch);
            let vendor = vendor.unwrap_or_default();
            let previous = previous_round(db, &issue.key, &worktree)?;
            // The same patch passed review last time: that pass carries over (NIT-185).
            let reused = reused_pass(db, &pb.review, &issue.key, &diff_id(&worktree, &since, &out.commit)?, &vendor, &|r| pb.review.is_installed(r, &worktree))?;
            if let Some((_, r)) = &reused {
                let at = r.reused.as_deref().unwrap_or_default();
                db.log_event(actor, Some(&issue.key), Some(&wt), "status", &format!("The same patch passed review at {}: that pass carries over, so it is not reviewed again", &at[..at.len().min(7)]))?;
            }
            // The single review knows what the change is for (NIT-282).
            let mut rules = pb.review.clone();
            rules.context = review_context(db, &issue, &out.gate)?;
            // Under a mode that reviews in batches, its review waits for its land queue batch
            // (review_batch.rs): nothing is reviewed now, and land refuses it until it is.
            if reused.is_none() && waits_for_land_batch(db, &pb, &issue, &worktree, &files, &vendor, &out.reasons, previous.as_ref(), actor)? {
                out.review_batched = true;
                return Ok(());
            }
            // A reviewer that cannot run (not signed in, say) is passed over for the next candidate;
            // one that ran and did not finish its review stops the submit.
            let mut failed: Vec<crate::review::CouldNotRun> = vec![];
            let (choice, mut review) = if let Some(done) = reused { done } else { loop {
                rules.require_enabled()?;
                let choice = pb.review.choose_after(&vendor, &|r| pb.review.is_installed(r, &worktree), &failed)?;
                let reviewer = choice.reviewer.clone();
                let started = format!("Review by {reviewer}{} started", noted(&choice.note));
                db.log_event(actor, Some(&issue.key), Some(&wt), "review_started", &started)?;
                match rules.run(&reviewer, &worktree, &since, previous.as_ref()) {
                    Ok(r) => break (choice, r),
                    Err(e) if e.is::<crate::review::Disabled>() || e.is::<crate::project_yolo::SkipReview>() => return Err(e),
                    Err(e) => match e.downcast::<crate::review::CouldNotRun>() {
                        Ok(out) => {
                            db.log_event(actor, Some(&issue.key), Some(&wt), "review_failed", &format!("Review by {reviewer} could not run ({}); trying another reviewer", out.why))?;
                            failed.push(out);
                        }
                        Err(e) => {
                            db.log_event(actor, Some(&issue.key), Some(&wt), "review_failed", &format!("Review by {reviewer} did not finish"))?;
                            return Err(e);
                        }
                    },
                }
            } };
            crate::review_batch::hold_interactions(db, &worktree, &mut review)?;
            let reviewer = choice.reviewer.clone();
            review.note = choice.note.clone();
            out.reasons.extend(pb.review.independence_reason(&reviewer, &vendor, &choice.note));
            out.reasons.extend(pb.review.flag_reasons(&review));
            out.review = Some(review);
            Ok(())
        })();
        if let Err(e) = reviewed {
            if e.is::<crate::project_yolo::SkipReview>() {
                out.yolo = crate::project_yolo::enabled_for_issue(db, &issue.key)?;
                out.review_disabled = !out.yolo;
            }
            else if e.is::<crate::review::Disabled>() { out.review_disabled = true; } else { return Err(e); }
        }
    }
    if out.review_disabled || out.yolo || out.quick {
        let mut latest = Playbook::load(&root)?;
        crate::project_modes::apply_issue(db, &issue.key, &mut latest)?;
        out.reasons.extend(latest.review.unevaluated_reasons());
    }
    out.approver = if out.reasons.is_empty() { "agent" } else { "human" }.into();
    let mut verdict = out.review.as_ref().map_or(if out.quick { crate::quick::VERDICT } else if out.yolo { "yolo" } else if out.review_batched { crate::review_batch::PENDING } else if out.review_disabled { "disabled" } else { "none" }, |r| r.verdict.as_str()).to_string();
    // Findings the person accepted as they are, signed for this very patch, block nothing: the
    // review is recorded as it asked, and goes on as a passing one (land checks the same).
    let accepted_only = match &out.review {
        Some(r) if verdict == "changes" => {
            let accepted: Vec<String> = db.accepted_for(&issue.key, &out.patch_id)?.into_iter().map(|a| a.finding).collect();
            crate::submit_rules::accepted_only(&r.findings, &accepted)
        }
        _ => false,
    };
    if accepted_only {
        verdict = "pass".into();
    }
    // Read before this round is recorded: the rounds before it, since the human last decided on the
    // issue's findings or handed it back (their decision to go on starts a new count).
    let mut stalled = match &out.review {
        Some(r) if verdict == "changes" && issue.origin == "nitely" => stalled_rounds(db, &issue.key, r)?,
        _ => None,
    };
    // Before stalled rounds go to the person, a fresh review that has not seen them looks (NIT-245):
    // what it no longer finds was carried stale, and what it still finds is what the person decides.
    if let (Some(why), Some(r)) = (stalled.clone(), out.review.clone()) {
        db.log_event(actor, Some(&issue.key), Some(&wt), "status", "The review rounds are not converging: a fresh review, without the earlier rounds, looks before it goes to the person")?;
        // Fresh of the rounds, not of what the change is for (NIT-282) or of what the person
        // accepted on it: told those, it does not raise them again in other words (NIT-331).
        let mut rules = pb.review.clone();
        rules.context = review_context(db, &issue, &out.gate)?;
        let accepted = told_accepted(db, &issue.key)?;
        if !accepted.is_empty() {
            rules.context.push_str(&format!("\n{}", crate::review::accepted_told(&accepted)));
        }
        if let Ok(mut fresh) = rules.run(&r.reviewer, &worktree, &since, None) {
            fresh.note = if r.note.is_empty() { FRESH.to_string() } else { format!("{}; {FRESH}", r.note) };
            // What the person accepted as it is blocks nothing, whichever review says it again.
            let accepted: Vec<String> = db.accepted_for(&issue.key, &out.patch_id)?.into_iter().map(|a| a.finding).collect();
            stalled = crate::submit_rules::settle_fresh(&r, &mut fresh, &accepted, &why);
            if stalled.is_none() {
                verdict = "pass".into();
            }
            for reason in pb.review.flag_reasons(&fresh) {
                if !out.reasons.contains(&reason) {
                    out.reasons.push(reason);
                }
            }
            out.review = Some(fresh);
            out.approver = if out.reasons.is_empty() { "agent" } else { "human" }.into();
        }
    }
    if out.yolo || out.gate.skipped.is_some() {
        let current_head = git(&worktree, &["rev-parse", "HEAD"])?.trim().to_string();
        if current_head != checked_head || git(&worktree, &["rev-parse", &out.commit])?.trim() != checked_head {
            bail!("the branch changed while automatic checks were skipped; submit again so path and approval checks cover this revision");
        }
    }
    // Settings can change while queued reviewers or commands run. Unchecked evidence is usable
    // only while the person still authorizes that bypass.
    crate::project_modes::check_gate_authority(db, &[&issue.key], &out.gate)?;
    if out.yolo && !crate::project_yolo::enabled_for_issue(db, &issue.key)? {
        bail!("YOLO mode was disabled during submit; submit again for normal verification");
    }
    crate::verification::check_gate_authority(db,&root,&worktree,&issue.key,&out.gate)?;
    db.record_review(&out)?;
    let review_id = db.conn.last_insert_rowid();
    if out.review.is_some() {
        // The commit the gate and review ran on, not what HEAD is now: a commit since was not reviewed.
        let diff = diff_id(&worktree, &since, &out.commit).ok();
        db.conn.execute("UPDATE review SET rules = ?2, diff_id = ?3 WHERE id = ?1", rusqlite::params![review_id, rules_of(&pb.review), diff])?;
    }
    // A request to accept a finding this round no longer raised has nothing left to decide.
    if let Some(r) = &out.review {
        db.close_superseded_requests(&issue.key, &r.findings)?;
    }
    out.approval_stands =
        out.approver == "human" && verdict != "changes" && issue.state == "Approved" && db.standing_approval(&issue.key)?.is_some();

    let title = format!("{}: {}", issue.key, issue.title);
    let mut body = describe(&issue, &out, pb.land.by);
    // The host links the pull request to the issue's mirror there (mirror.rs).
    if let Some(closes) = crate::mirror::closes_line(db, &issue.key, &root) {
        body = format!("{closes}\n\n{body}");
    }
    // A quick change has no pull request: land pushes the integration branch.
    if !local_only && !out.quick {
        match push(&pb, &worktree, &branch) {
            Ok(()) => out.pushed = true,
            Err(e) => out.pr_error = Some(format!("push failed: {e:#}")),
        }
        if out.pushed {
            match crate::remote::open_pr(&root.to_string_lossy(), &branch, &base, &title, &body) {
                Ok(url) => out.pr_url = Some(url),
                Err(e) => out.pr_error = Some(format!("{e:#}")),
            }
        }
    }

    let pr_line = match (&out.pr_url, &out.pr_error) {
        (Some(url), _) => format!("Pull request: {url}"),
        (None, Some(e)) => format!("No pull request: {e}"),
        _ if no_origin => "Not pushed: the repo has no origin remote, so the change stays in it.".into(),
        _ => "Not pushed (local only).".into(),
    };
    let by = |r: &Review| match (r.batched_with(), &r.reused) {
        (Some(with), _) => format!("{}{}, in a batch with {with},", r.reviewer, noted(&r.note)),
        (None, Some(at)) => format!("{}{} of this same patch at {}", r.reviewer, noted(&r.note), &at[..at.len().min(7)]),
        (None, None) => format!("{}{}", r.reviewer, noted(&r.note)),
    };
    let outcome = match (&out.review, out.approver.as_str()) {
        (Some(r), _) if accepted_only => format!("Review by {} asked for changes only in findings you accepted, so it passes", by(r)),
        (Some(r), _) if r.verdict == "changes" => format!("Review by {} requested changes", by(r)),
        (Some(r), "human") if out.approval_stands => format!("Review by {} passed; the human's approval of this patch stands", by(r)),
        (Some(r), "human") => format!("Review by {} passed; waits for a human", by(r)),
        (Some(r), _) => format!("Review by {} passed; ready to land", by(r)),
        (None, _) if out.review_batched => "Gate passed; its review waits for its land queue batch, which reviews what waits together, once, before the batch's gate".into(),
        (None, "human") if out.quick => format!("{}; waits for a human under the remaining approval rules", crate::quick::NOTE),
        (None, _) if out.quick => format!("{}; ready to land", crate::quick::NOTE),
        (None, "human") if out.yolo => format!("{}; waits for a human under the remaining approval rules", crate::project_yolo::NOTE),
        (None, _) if out.yolo => format!("{}; unverified and ready to land", crate::project_yolo::NOTE),
        (None, "human") if out.review_disabled => format!("{}; waits for a human under the remaining approval rules", crate::review::DISABLED_NOTE),
        (None, _) if out.review_disabled && out.gate.skipped.is_some() => "Automatic tests and pre-merge review skipped by project settings; unverified and ready to land".into(),
        (None, _) if out.review_disabled => format!("{}; gate passed and ready to land", crate::review::DISABLED_NOTE),
        (None, _) => "Submitted without a review; waits for a human".to_string(),
    };
    let outcome = match out.review.as_ref().map(|r| rechecked(&r.previous)) {
        Some(Some(counts)) => format!("{outcome} (previous round: {counts})"),
        _ => outcome,
    };
    let outcome = if pb.land.by.by_nitely() && verdict != "changes" { format!("{outcome}; it {}", pb.land.by.describe()) } else { outcome };
    let outcome = match &stalled {
        Some(why) => format!("{outcome}. The review rounds are not converging: {why}"),
        None => outcome,
    };
    // The full description is on the pull request and in the app's review section.
    db.add_note(&issue.key, &format!("{outcome}. {pr_line}"), "submit", actor)?;

    let push_failed = !local_only && !out.quick && !out.pushed;
    let by_nitely = pb.land.by.by_nitely();
    // An open P0 covers a part that must not land without the person, whatever the mode says.
    let open_p0 = match &out.review {
        Some(r) => {
            let accepted: Vec<String> = db.accepted_for(&issue.key, &out.patch_id)?.into_iter().map(|a| a.finding).collect();
            r.findings.iter().any(|f| f.priority == 0 && !accepted.contains(&f.text))
        }
        None => false,
    };
    let mode_stalled = crate::project_modes::for_issue(db, &issue.key)?.map(|m| m.options()).unwrap_or_else(crate::project_modes::builtin).stalled_reviews;
    let route = crate::submit_rules::stalled_route(mode_stalled, open_p0);
    let stalled_reviews = Some(route);
    let (state, next) = crate::submit_rules::next_state(&crate::submit_rules::Outcome {
        stalled: stalled.is_some(),
        stalled_reviews,
        changes: verdict == "changes",
        push_failed,
        approval_stands: out.approval_stands,
        human: out.approver == "human",
        by_nitely,
        batched: out.review_batched,
        yolo: out.yolo || out.quick,
        review_disabled: out.review_disabled,
        gate_skipped: out.gate.skipped.is_some(),
    });
    // Stalled rounds never hold the agent (NIT-568): under `ask` the person decides each open
    // finding while it goes on fixing; under `fix` they are told it splits the change.
    let mut split_follow_up: Option<String> = None;
    if let Some(why) = &stalled {
        match route {
            crate::project_modes::StalledReviews::Ask => {
                db.ask_to_decide_findings(&issue.key, why, actor)?;
            }
            crate::project_modes::StalledReviews::Fix => {
                // The findings go to a follow-up filed now, so the notice can name it (NIT-634 review).
                let follow_up = match &out.review {
                    Some(r) => {
                        let accepted: Vec<String> = db.accepted_for(&issue.key, &out.patch_id)?.into_iter().map(|a| a.finding).collect();
                        Some(file_follow_up(db, &issue, r, &accepted, actor)?)
                    }
                    None => None,
                };
                let notice = match &follow_up {
                    Some(f) => crate::submit_rules::stalled_notice_for(&issue.key, why, f),
                    None => crate::submit_rules::stalled_notice(&issue.key, why),
                };
                db.add_note(&issue.key, &notice, "note", actor)?;
                split_follow_up = follow_up;
            }
        }
    }
    let now_in = db.issue(&issue.key)?.map(|i| i.state).unwrap_or_else(|| issue.state.clone());
    // The person may have accepted its blocking findings while it was pushed (NIT-575): that
    // acceptance moved the issue itself, from the review as it now stands.
    // A machine that cannot read the approval key (macOS) cannot verify the acceptance, and keeps
    // the person's decision rather than undoing it (NIT-580 review).
    use crate::submit_rules::Meanwhile;
    let meanwhile = match db.reviews(&issue.key)?.into_iter().find(|r| !r.reviewer.is_empty()) {
        Some(r) if verdict == "changes" => {
            let key_readable = crate::approval::trusted_key().is_ok();
            let unaccepted = if key_readable { db.unaccepted_blockers(&issue.key, &r)?.len() } else { usize::MAX };
            crate::submit_rules::meanwhile(true, key_readable, unaccepted, db.recorded_acceptances_cover(&issue.key, &r)?)
        }
        _ => Meanwhile::No,
    };
    out.state = now_in.clone();
    out.next = match &split_follow_up {
        Some(f) => crate::submit_rules::split_into(f),
        None if out.quick && state == "In Review" => "A quick change: its own checks passed. Call land; it is reviewed after landing.".into(),
        None => next.into(),
    };
    match crate::submit_rules::final_move(state, &now_in, meanwhile) {
        Some(to) if issue.origin == "nitely" => out.state = db.move_issue(&issue.key, to, None, None, actor)?.state,
        _ if meanwhile == Meanwhile::Accepted => out.next = format!("The person accepted the review's blocking findings while this submit ran, so it goes on as their acceptance moved it ({now_in})."),
        _ if meanwhile == Meanwhile::Unverified => {
            out.next = format!("The person accepted the review's blocking findings while this submit ran. This machine cannot read the approval key to verify that, so it keeps their decision ({now_in}) and leaves the check and the land to the app.")
        }
        _ => {}
    }

    // In a project that lands by Nitely, what is ready to land lands now, in this submit, from the
    // state it ended in (one the person's acceptance kept counts). An approved change waits for the
    // app where only the app can check approvals (macOS).
    let state = crate::submit_rules::land_state(state, &now_in, meanwhile).unwrap_or("");
    let app_lands = state == "Approved" && !crate::approval::agents_can_verify();
    if by_nitely && matches!(state, "In Review" | "Approved") && !app_lands {
        let root_dir = root.to_string_lossy().into_owned();
        // Nitely lands it, as it does an approved change: what the land and its queue log is
        // Nitely's, not the agent's, who may be on another issue by the time it runs (NIT-273).
        let lander = Actor::Agent(crate::land::AUTO_LANDER.into());
        let landed = crate::land::land_now(db, config, &issue.key, &root_dir, !local_only, &lander);
        let (state, next) = crate::land::after_auto_land(db, &issue.key, &landed, &lander)?;
        out.state = state;
        out.next = next;
        out.land = landed.ok();
    }
    Ok(out)
}

/// Why `key`'s review rounds stopped converging, with `review` the newest, which asks for changes,
/// read before it is recorded: the rounds since the person last decided on its findings or handed
/// it back (their decision to go on starts a new count). Change reviews and item reviews alike.
pub(crate) fn stalled_rounds(db: &Db, key: &str, review: &Review) -> Result<Option<String>> {
    let decided = [db.handed_back_event(key)?, db.findings_decided_event(key)?].into_iter().flatten().max_by_key(|d| (d.ts, d.id));
    let earlier: Vec<ReviewRecord> = db.reviews(key)?.into_iter().filter(|r| decided.is_none_or(|d| d.came_before(r))).collect();
    Ok(not_converging(&earlier, review))
}

/// The item review the issue's mode asks for (Off with no mode).
pub(crate) fn item_review_of(db: &Db, key: &str) -> Result<ItemReview> {
    Ok(crate::project_modes::for_issue(db, key)?.and_then(|m| m.options().item_review).unwrap_or(ItemReview::Off))
}

/// The item review of a goal item, against its goal branch `goal_branch` (NIT-563), recorded as
/// any review is, so the next round, acceptances and land read it. Returns the review, the patch
/// it reviewed, and why the rounds stopped converging, when they did (NIT-568).
fn review_item(db: &mut Db, root: &Path, worktree: &Path, issue: &IssueRecord, branch: &str, goal_branch: &str, actor: &Actor) -> Result<(Review, String, Option<String>)> {
    let mut pb = Playbook::load(root)?;
    crate::project_modes::apply_issue(db, &issue.key, &mut pb)?;
    pb.review.policy_root = Some(root.to_path_buf());
    // Guarded by the item review the mode asks for, not the review of changes with no goal.
    pb.review.yolo_guards = crate::project_yolo::ReviewGuard::for_issue(db, &issue.key)?.map(|g| crate::project_yolo::ReviewGuard { item: true, ..g }).into_iter().collect();
    // The mode asked for it, whether or not changes with no goal are reviewed.
    pb.review.enabled = true;
    let since = playbook::diff_base(worktree, goal_branch);
    let head = git(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    let gate = GateResult { mode_authority: None, passed: true, plan: None, skipped: None, commit: head[..head.len().min(7)].to_string(), results: vec![], warnings: vec![], flaked: vec![], lane_run: None };
    let (vendor, _) = crate::git::parse_branch(branch);
    let vendor = vendor.unwrap_or_default();
    let previous = previous_round(db, &issue.key, worktree)?;
    let mut rules = pb.review.clone();
    rules.context = review_context(db, issue, &gate)?;
    let wt = worktree.to_string_lossy().into_owned();
    // A reviewer that cannot run (not signed in, say) is passed over for the next one, as for a
    // change to main; one that ran and did not finish stops the submit.
    let mut failed: Vec<crate::review::CouldNotRun> = vec![];
    let (choice, mut review) = loop {
        let choice = pb.review.choose_after(&vendor, &|r| pb.review.is_installed(r, worktree), &failed)?;
        db.log_event(actor, Some(&issue.key), Some(&wt), "review_started", &format!("Item review by {}{} started", choice.reviewer, noted(&choice.note)))?;
        match rules.run(&choice.reviewer, worktree, &since, previous.as_ref()) {
            Ok(r) => break (choice, r),
            Err(e) => match e.downcast::<crate::review::CouldNotRun>() {
                Ok(out) => {
                    db.log_event(actor, Some(&issue.key), Some(&wt), "review_failed", &format!("Item review by {} could not run ({}); trying another reviewer", choice.reviewer, out.why))?;
                    failed.push(out);
                }
                Err(e) => return Err(e),
            },
        }
    };
    review.note = choice.note.clone();
    let patch = patch_id_range(worktree, &since, &head)?;
    let stalled = if review.verdict == "changes" && issue.origin == "nitely" { stalled_rounds(db, &issue.key, &review)? } else { None };
    let record = Submitted {
        verification: None,
        issue: issue.key.clone(),
        branch: branch.to_string(),
        base: goal_branch.to_string(),
        patch_id: patch.clone(),
        commit: gate.commit.clone(),
        files: 0,
        gate,
        solution: None,
        review: Some(review.clone()),
        review_disabled: false,
        review_batched: false,
        yolo: false,
        quick: false,
        approver: "agent".into(),
        reasons: vec![],
        approval_stands: false,
        pushed: false,
        pr_url: None,
        pr_error: None,
        land: None,
        state: issue.state.clone(),
        next: String::new(),
    };
    db.record_review(&record)?;
    db.close_superseded_requests(&issue.key, &review.findings)?;
    Ok((review, patch, stalled))
}

/// `submit` for an item of a goal with a goal branch: the item review its mode asks for, then the
/// land into that branch, as `land` would, and says so. The goal's checkpoint gets the blocking
/// review of the whole slice and goes into main.
#[allow(clippy::too_many_arguments)]
fn submit_into_goal(db: &mut Db, config: &crate::Config, issue: &IssueRecord, root: &Path, worktree: &Path, branch: &str, goal_branch: &str, local_only: bool, actor: &Actor) -> Result<Submitted> {
    let mode = item_review_of(db, &issue.key)?;
    let mut reviewed: Option<Review> = None;
    if mode != ItemReview::Off {
        let (review, patch, stalled) = review_item(db, root, worktree, issue, branch, goal_branch, actor)?;
        let accepted: Vec<String> = db.accepted_for(&issue.key, &patch)?.into_iter().map(|a| a.finding).collect();
        let open = crate::submit_rules::blocking(&review.findings, &accepted);
        let by = format!("{}{}", review.reviewer, noted(&review.note));
        if !crate::submit_rules::item_lands(mode, &review.findings, &accepted) {
            // Rounds that stop converging go as a change's do (NIT-568): never held, and the mode's
            // `stalled_reviews` says whether the person decides each finding or the agent splits.
            let mode_stalled = crate::project_modes::for_issue(db, &issue.key)?.map(|m| m.options()).unwrap_or_else(crate::project_modes::builtin).stalled_reviews;
            let open_p0 = review.findings.iter().any(|f| f.priority == 0 && !accepted.contains(&f.text));
            let route = crate::submit_rules::stalled_route(mode_stalled, open_p0);
            let stalled_reviews = Some(route);
            let mut split_follow_up: Option<String> = None;
            if let Some(why) = &stalled {
                match route {
                    crate::project_modes::StalledReviews::Ask => {
                        db.ask_to_decide_findings(&issue.key, why, actor)?;
                    }
                    crate::project_modes::StalledReviews::Fix => {
                        let f = file_follow_up(db, issue, &review, &accepted, actor)?;
                        db.add_note(&issue.key, &crate::submit_rules::stalled_notice_for(&issue.key, why, &f), "note", actor)?;
                        split_follow_up = Some(f);
                    }
                }
            }
            let stalled_line = stalled.as_ref().map(|why| format!(". The review rounds are not converging: {why}")).unwrap_or_default();
            db.add_note(&issue.key, &format!("Item review by {by} requested changes: {}{stalled_line}", open.join("; ")), "submit", actor)?;
            let state = if issue.state != "In Progress" && issue.origin == "nitely" { db.move_issue(&issue.key, "In Progress", None, None, actor)?.state } else { issue.state.clone() };
            return Ok(Submitted {
                verification: None,
                issue: issue.key.clone(),
                branch: branch.to_string(),
                base: goal_branch.to_string(),
                patch_id: patch,
                commit: String::new(),
                files: 0,
                gate: GateResult { mode_authority: None, passed: true, plan: None, skipped: Some("not run: the item review asked for changes".into()), commit: String::new(), results: vec![], warnings: vec![], flaked: vec![], lane_run: None },
                solution: None,
                review: Some(review),
                review_disabled: false,
                review_batched: false,
                yolo: false,
                quick: false,
                approver: "agent".into(),
                reasons: vec![],
                approval_stands: false,
                pushed: false,
                pr_url: None,
                pr_error: None,
                land: None,
                state,
                next: match &split_follow_up {
                    Some(f) => crate::submit_rules::split_into(f),
                    None => crate::submit_rules::item_sent_back(stalled.is_some(), stalled_reviews).to_string(),
                },
            });
        }
        if !open.is_empty() {
            db.add_note(&issue.key, &format!("Item review by {by}, advisory, found: {}. It lands into {goal_branch} as it is; the checkpoint review sees the whole slice.", open.join("; ")), "note", actor)?;
        }
        reviewed = Some(review);
    }
    let dir = root.to_string_lossy().into_owned();
    let landed = crate::land::land(db, config, &crate::land::LandOptions { dir: &dir, issue_key: &issue.key, push: !local_only, keep: false, break_lock: false }, actor)?;
    let target = landed.goal_branch.clone().unwrap_or_else(|| landed.base.clone());
    let gate = landed.gate.clone().unwrap_or(GateResult { mode_authority: None, passed: false, plan: None, skipped: None, commit: String::new(), results: vec![], warnings: vec![], flaked: vec![], lane_run: None });
    let next = if landed.landed {
        let mut next = format!(
            "An item of a goal lands into {target} with item checks{}, with no pull request of its own: it landed there at {}. The goal's checkpoint reviews it and lands it into main; nothing is left to do.",
            if reviewed.is_some() { " and its item review" } else { " and no review of its own" },
            landed.head.as_deref().unwrap_or("?")
        );
        for said in &landed.goals {
            next.push_str(&format!(" {said}."));
        }
        next
    } else {
        format!("The land into {target} stopped: {}. Fix it, commit, and submit again.", landed.error.as_deref().unwrap_or("see the issue's timeline"))
    };
    Ok(Submitted {
        verification: gate.plan.clone(),
        issue: issue.key.clone(),
        branch: branch.to_string(),
        base: target,
        patch_id: String::new(),
        commit: gate.commit.clone(),
        files: 0,
        gate,
        solution: None,
        review: reviewed,
        review_disabled: false,
        review_batched: false,
        yolo: false,
        quick: false,
        approver: "agent".into(),
        reasons: vec![],
        approval_stands: false,
        pushed: landed.pushed.unwrap_or(false),
        pr_url: None,
        pr_error: None,
        state: db.issue(&issue.key)?.map(|i| i.state).unwrap_or_default(),
        land: Some(landed),
        next,
    })
}

/// Whether the change's review waits for its land queue batch (review_batch.rs): its project
/// reviews in batches and nothing makes it a change reviewed alone. The timeline says which.
#[allow(clippy::too_many_arguments)]
fn waits_for_land_batch(
    db: &Db,
    pb: &Playbook,
    issue: &IssueRecord,
    worktree: &Path,
    files: &[String],
    vendor: &str,
    reasons: &[String],
    previous: Option<&PreviousRound>,
    actor: &Actor,
) -> Result<bool> {
    if pb.review.batch == crate::review::BatchMode::Off {
        return Ok(false);
    }
    let wt = worktree.to_string_lossy().into_owned();
    pb.review.require_enabled()?;
    let choice = pb.review.choose(vendor, &|r| pb.review.is_installed(r, worktree))?;
    let large_repo = pb.lanes.as_ref().is_some_and(|l| l.large_repo);
    let moves_submodule = pb.submodules.iter().find(|s| files.iter().any(|f| f == s.path())).map(|s| format!("it moves the submodule {}, whose code is reviewed with it", s.path()));
    let why_alone = match moves_submodule {
        Some(why) => Some(why),
        None => crate::review_batch::why_alone(db, &pb.review, vendor, &choice, reasons, files, previous, large_repo, &issue.key)?,
    };
    match why_alone {
        Some(why) => {
            db.log_event(actor, Some(&issue.key), Some(&wt), "status", &format!("Reviewed now, alone, not with its land batch: {why}"))?;
            Ok(false)
        }
        None => Ok(true),
    }
}

/// What a single review is told about the change beyond its diff (NIT-282): the issue it is for,
/// so it checks the change does what was asked first; the gate's result; the author's notes since
/// the last round, which may answer its findings; and findings the person accepted as trade-offs on
/// other changes, as examples of what is not a P0/P1 here.
pub(crate) fn review_context(db: &Db, issue: &IssueRecord, gate: &GateResult) -> Result<String> {
    let mut c = String::new();
    let body: String = issue.body.chars().take(2500).collect();
    c.push_str(&format!(
        "What the change is for, its issue {} \"{}\":\n{}\n\nFirst check that the change does what the issue asks (its \"done when\", \
when it has one): a gap there is a finding like any other, and comes before line-level problems.\n",
        issue.key,
        issue.title,
        body.trim()
    ));
    // A step skipped here passed on this same tree before (lanes keep that), so it counts as run.
    let gate: Vec<String> = gate.results.iter().filter(|r| r.ok).map(|r| format!("`{}`", r.command)).collect();
    if !gate.is_empty() {
        c.push_str(&format!("\nThe gate passed on this tree: {}. Do not report what it already checks.\n", gate.join(", ")));
    }
    // Notes since the last review: after the event it saw (NIT-544), or, for an older review, from
    // its second on.
    let last = db.reviews(&issue.key)?.into_iter().find(|r| !r.reviewer.is_empty()).map(|r| (r.ts, r.after_event));
    if let Some((since, seen)) = last {
        let notes: Vec<String> = db
            .conn
            .prepare(
                "SELECT summary FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type IN ('note', 'handoff') AND (CASE WHEN ?3 IS NULL THEN ts >= ?2 ELSE id > ?3 END) AND actor != 'human'
                 AND summary IS NOT NULL ORDER BY id DESC LIMIT 5",
            )?
            .query_map(rusqlite::params![issue.key, since, seen], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !notes.is_empty() {
            c.push_str("\nThe author's notes since that review, which may answer its findings:\n");
            for n in notes.iter().rev() {
                c.push_str(&format!("- {}\n", n.chars().take(600).collect::<String>().replace('\n', " ")));
            }
        }
    }
    // Findings the person is deciding now: a round that still sees one says it in the same words,
    // so the request stays attached to it (NIT-353). Only a finding's words, which a review wrote,
    // never the asking agent's reasoning, and nothing about its weight.
    let deciding: Vec<crate::accept::AskedFinding> = db.open_asks()?.into_iter().filter(|a| a.issue_key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&issue.key))).filter_map(|a| a.finding).collect();
    if !deciding.is_empty() {
        c.push_str("\nThe person is deciding these findings of an earlier round. If one still holds, report it in exactly these words, so their decision stays attached to it; report it only if it still holds:\n");
        for f in &deciding {
            // Whole and as recorded: the round is to repeat these very words, which are matched whole.
            c.push_str(&format!("- [P{}] {}\n", f.priority, f.text));
        }
    }
    // Only what the person signed: a row an agent wrote itself must never steer the reviewer.
    let prefix = issue.key.split('-').next().unwrap_or("");
    let mut accepted: Vec<crate::accept::Accepted> = vec![];
    if let Ok(public) = crate::approval::trusted_key() {
        let keys: Vec<String> = db
            .conn
            .prepare("SELECT DISTINCT issue_key FROM accepted_finding WHERE issue_key LIKE ?1 || '-%' AND issue_key != ?2 COLLATE NOCASE AND priority <= 1")?
            .query_map(rusqlite::params![prefix, issue.key], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for k in keys {
            accepted.extend(db.signed_acceptances(&k, &public)?.into_iter().filter(|a| a.priority <= 1));
        }
        accepted.sort_by_key(|a| std::cmp::Reverse(a.ts));
        accepted.truncate(3);
    }
    if !accepted.is_empty() {
        // Examples to calibrate severity by, never exemptions: each acceptance was the person's call for
        // that change, for its reason, and a like finding whose reason does not hold here is still a P1.
        c.push_str(
            "\nFor calibration: on other changes, the person accepted these findings as trade-offs, for the reasons given. They are \
not exemptions. Raise a similar finding at P0 or P1 only when its concrete failure applies here and the person's reason \
does not.\n",
        );
        for a in accepted {
            c.push_str(&format!("- [P{}] {} (accepted: {})\n", a.priority, a.finding.chars().take(300).collect::<String>(), a.reason));
        }
    }
    Ok(c)
}

/// What to do after a gate that did not pass: fix the step that failed, or, when a step ran past
/// the lane's time limit, nothing in the change (NIT-310).
fn after_failed_gate(gate: &GateResult) -> String {
    if let Some(r) = gate.timed_out() {
        return format!(
            "The gate did not finish: {}, which says nothing about this change. Read its log for where it stopped, then submit again; a gate that needs longer needs a larger `[lanes] timeout_minutes`.",
            r.stopped()
        );
    }
    // The step that failed: CI runs its steps side by side, so it need not be the last one reported.
    let failed = gate.results.iter().find(|r| !r.ok && !r.skipped).or(gate.results.last()).map(|r| r.command.as_str()).unwrap_or("");
    format!("The gate failed at `{failed}`. Fix it, commit, and submit again.")
}

/// The follow-up issue that takes `issue`'s open P0/P1 findings when its review rounds stop
/// converging under `fix`, in the same goal: the one filed at an earlier stall while it is still
/// open, with this stall's new findings added to it, else a new one. Filed before the notice, so
/// the notice names it (NIT-634 review).
fn file_follow_up(db: &mut Db, issue: &IssueRecord, review: &Review, accepted: &[String], actor: &Actor) -> Result<String> {
    let evidence = crate::submit_rules::follow_up_evidence(&issue.key);
    let open = crate::submit_rules::blocking(&review.findings, accepted);
    if let Some(existing) = db.list_issues(issue.project.as_deref(), false)?.into_iter().find(|i| i.evidence == evidence) {
        if let Some(body) = crate::submit_rules::follow_up_with(&existing.body, &open) {
            db.edit_issue(&existing.key, None, Some(&body), actor)?;
        }
        return Ok(existing.key);
    }
    let p0 = review.findings.iter().any(|f| f.priority == 0 && !accepted.contains(&f.text));
    let new = crate::issues::NewIssue {
        project: issue.project.clone().unwrap_or_default(),
        title: format!("Open review findings of {}: {}", issue.key, issue.title),
        body: crate::submit_rules::follow_up_body(&issue.key, &open),
        kind: "bug".into(),
        source: issue.source.clone().unwrap_or_else(|| "nitely".into()),
        evidence,
        priority: if p0 { 1 } else { 2 },
        allow_similar: true,
        goal: issue.goal,
        ..Default::default()
    };
    Ok(db.create_issue(&new, actor)?.key)
}

/// The note on a review run fresh, without the rounds before it, before stalled rounds escalate.
const FRESH: &str = "fresh, without the earlier rounds";

/// How many review rounds in a row may ask for changes before the issue goes to a person.
pub const MAX_ROUNDS: usize = 5;

/// Why an issue's review rounds have stopped converging, given the reviews before this one
/// (newest first) and this one, which asks for changes: a P0 or P1 finding still open after two
/// rounds that tried to fix it (carried a second time), or the `MAX_ROUNDS`th round in a row that
/// asks for changes. A passing review ends a run: rounds after it (a rebase, a land that stopped)
/// are not a loop. The text is the one line the person reads: what happened and how many findings
/// are open, by priority; the findings themselves are on the review and its follow-up (NIT-633).
pub fn not_converging(earlier: &[ReviewRecord], review: &Review) -> Option<String> {
    let earlier: Vec<&ReviewRecord> = earlier.iter().filter(|r| !r.reviewer.is_empty()).collect();
    let round = 1 + earlier.iter().take_while(|r| r.verdict == "changes").count();
    // Carried now and already carried by the round before: two fixes did not close it.
    let twice: Vec<&crate::review::Finding> = review
        .findings
        .iter()
        .filter(|f| f.carried && f.priority <= 1)
        .filter(|f| earlier.first().is_some_and(|last| last.findings.iter().any(|p| p.carried && p.priority <= 1 && p.text == f.text)))
        .collect();
    if twice.is_empty() && round < MAX_ROUNDS {
        return None;
    }
    let why = if twice.is_empty() {
        format!("{round} review rounds in a row asked for changes")
    } else {
        format!("{} still open after two rounds that tried to fix {}", plural(twice.len(), "P0/P1 finding"), if twice.len() == 1 { "it" } else { "them" })
    };
    Some(format!("{why}; {}", open_count(&review.findings)))
}

/// How many P0/P1 findings are open, and of which priority: `3 open findings (1 P0, 2 P1)`.
pub(crate) fn open_count(findings: &[crate::review::Finding]) -> String {
    let p0 = findings.iter().filter(|f| f.priority == 0).count();
    let p1 = findings.iter().filter(|f| f.priority == 1).count();
    let by: Vec<String> = [(p0, "P0"), (p1, "P1")].iter().filter(|(n, _)| *n > 0).map(|(n, p)| format!("{n} {p}")).collect();
    format!("{} ({})", plural(p0 + p1, "open finding"), by.join(", "))
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// The latest review that ran on the issue, for the next one to start from. Its commit counts as
/// in the branch's history only when it is an ancestor of HEAD; after a rebase it is not.
pub(crate) fn previous_round(db: &Db, key: &str, worktree: &Path) -> Result<Option<PreviousRound>> {
    previous_round_at(db, key, worktree, "HEAD", None)
}

/// `previous_round` for the commit `head` (read in `dir`), from the reviews recorded before the
/// review `before`: for a shadow batch, the round before the single review that decided the change,
/// so the batch reviews it as that single review did, not knowing its answers (NIT-217).
pub(crate) fn previous_round_at(db: &Db, key: &str, dir: &Path, head: &str, before: Option<i64>) -> Result<Option<PreviousRound>> {
    let Some(last) = db.reviews_before(key, before)?.into_iter().find(|r| !r.reviewer.is_empty()) else {
        return Ok(None);
    };
    let in_history = git(dir, &["merge-base", "--is-ancestor", &last.commit_sha, head]).is_ok();
    let told = told_accepted(db, key)?;
    let findings = last.findings.into_iter().filter(|f| !told.iter().any(|(t, _)| t.text == f.text)).collect();
    // A pass whose branch was rebased since: where its change began, for a range-diff against it.
    let passed_on = if !in_history && last.verdict == "pass" { git(dir, &["merge-base", &last.commit_sha, head]).ok().map(|o| o.trim().to_string()) } else { None };
    Ok(Some(PreviousRound { commit: last.commit_sha, findings, in_history, accepted: told, passed_on }))
}

/// The findings the person accepted on issue `key`, each once, with its reason, for a review to be
/// told. Only acceptances that verify count: a row an agent wrote itself must not silence a finding.
fn told_accepted(db: &Db, key: &str) -> Result<Vec<(Finding, String)>> {
    let accepted: Vec<crate::accept::Accepted> = match crate::approval::trusted_key() {
        Ok(public) => db.signed_acceptances(key, &public)?,
        Err(_) => vec![],
    };
    let mut told: Vec<(Finding, String)> = vec![];
    for a in &accepted {
        if !told.iter().any(|(f, _)| f.text == a.finding) {
            told.push((Finding { priority: a.priority, text: a.finding.clone(), carried: false, raised_as: None, now: None }, a.reason.clone()));
        }
    }
    Ok(told)
}

/// The last review's pass, carried over when it reviewed this very patch: nothing in the change is
/// new to review. It keeps that review's reviewer, note, findings and flags, and says where it is
/// from. Only the latest review counts, so a pass followed by a review that asked for changes is not
/// carried over to a later return to the same patch. It must have seen this very diff (`diff_id`),
/// run under the same review rules (a flag added since was never asked), and been by the reviewer
/// that would be chosen now.
fn reused_pass(
    db: &Db,
    rules: &crate::review::ReviewRules,
    key: &str,
    diff: &str,
    vendor: &str,
    installed: &dyn Fn(&str) -> bool,
) -> Result<Option<(crate::review::Choice, Review)>> {
    if diff.is_empty() {
        return Ok(None);
    }
    let last = db
        .conn
        .query_row(
            "SELECT reviewer, verdict, COALESCE(diff_id, ''), commit_sha, findings, flagged, note, rules FROM review
             WHERE issue_key = ?1 COLLATE NOCASE AND reviewer != '' ORDER BY ts DESC, id DESC LIMIT 1",
            [key],
            |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?),
                    (r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?),
                ))
            },
        )
        .optional()?;
    let Some(((reviewer, verdict, last_diff, commit), (findings, flagged, note, ran_under))) = last else { return Ok(None) };
    let previous = crate::submit_rules::LastReview { reviewer: &reviewer, verdict: &verdict, diff_id: &last_diff, rules: ran_under.as_deref() };
    let chosen = rules.choose(vendor, installed).ok().map(|c| c.reviewer);
    if !crate::submit_rules::reusable(&previous, diff, &rules_of(rules), chosen.as_deref()) {
        return Ok(None);
    }
    let review = Review {
        reviewer: reviewer.clone(),
        verdict,
        findings: serde_json::from_str(&findings).unwrap_or_default(),
        flagged: serde_json::from_str(&flagged).unwrap_or_default(),
        summary: String::new(),
        seconds: 0,
        previous: vec![],
        note: note.clone(),
        batch: None,
        tokens: None,
        reused: Some(commit),
    };
    let choice = crate::review::Choice { independent: rules.is_independent(&reviewer, vendor), reviewer, note };
    Ok(Some((choice, review)))
}

/// The change's diff exactly, as git's hash of it: what each hunk changes, where, and in what
/// context. Two changes with one patch-id can differ here (the same edit in another function); a
/// rebase that touched none of the change's files leaves it as it was. Empty for no diff.
fn diff_id(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let diff = git(worktree, &["diff", "--no-color", "--no-ext-diff", &format!("{base}...{head}")])?;
    if diff.is_empty() {
        return Ok(String::new());
    }
    use std::io::Write;
    let mut child = playbook::shell_free_command("git")
        .arg("-C")
        .arg(worktree)
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(diff.as_bytes()));
    let out = child.wait_with_output()?;
    writer.join().ok();
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The review rules as recorded with a review, to tell whether they changed since.
fn rules_of(rules: &crate::review::ReviewRules) -> String {
    serde_json::to_string(rules).unwrap_or_default()
}

/// " (same vendor: codex not installed)" after a reviewer's name, or nothing.
fn noted(note: &str) -> String {
    if note.is_empty() { String::new() } else { format!(" ({note})") }
}

/// "2 fixed, 1 still open" for a re-review's checks of the previous round; None when it had none.
fn rechecked(previous: &[crate::review::Recheck]) -> Option<String> {
    if previous.is_empty() {
        return None;
    }
    let count = |status: &str| previous.iter().filter(|r| r.status == status).count();
    let parts: Vec<String> = [("fixed", "fixed"), ("open", "still open"), ("unchecked", "not checked"), ("unconfirmed", "unconfirmed (its code is gone)")]
        .iter()
        .filter(|(status, _)| count(status) > 0)
        .map(|(status, label)| format!("{} {label}", count(status)))
        .collect();
    Some(parts.join(", "))
}

/// The pull request description: approval, review, findings, and a marker line with the patch.
pub fn describe(issue: &IssueRecord, s: &Submitted, land_by: crate::playbook::LandBy) -> String {
    // A link with the app's own scheme; hosts that drop such links still show the key, and the
    // second line keeps the link copyable.
    let named = if issue.url.is_empty() { issue.key.clone() } else { format!("[{}]({})", issue.key, issue.url) };
    let mut lines = match &s.solution {
        Some(solution) => vec![format!("## {}", solution.title), solution.summary.clone(), String::new(), format!("Original issue: **{named}** {}", issue.title)],
        None => vec![format!("Nitely: **{named}** {}", issue.title), "Solution summary unavailable.".into()],
    };
    if issue.url.starts_with(&format!("{}://", crate::issues::LINK_SCHEME)) {
        lines.push(format!("Open it in Nitely: `{}`", issue.url));
    }
    lines.push(String::new());
    lines.push(if s.approval_stands {
        "**Approval:** a human approved this exact patch in Nitely, and this review raised nothing new, so it stands.".to_string()
    } else if s.approver == "human" {
        "**Approval:** waits for a human. Approve it in Nitely's inbox.".to_string()
    } else if s.review_disabled || s.yolo {
        "**Approval:** no human approval is required under this repo's remaining rules.".to_string()
    } else {
        "**Approval:** the review is enough under this repo's playbook.".to_string()
    });
    lines.extend(s.reasons.iter().map(|r| format!("- {r}")));
    lines.push(format!("**Landing:** {}.", land_by.describe()));
    lines.push(String::new());
    match &s.review {
        Some(r) => {
            let result = if r.verdict == "pass" { "no P0 or P1 findings" } else { "changes requested" };
            let batched = r.batched_with().map(|with| format!(", in one review call with {with}")).unwrap_or_default();
            match &r.reused {
                Some(at) => lines.push(format!("**Review** by {}{}: {result}, carried over: this same patch passed review at `{at}`.", r.reviewer, noted(&r.note))),
                None => lines.push(format!("**Review** by {}{} ({}s{batched}): {result}.", r.reviewer, noted(&r.note), r.seconds)),
            }
            if !r.summary.is_empty() {
                lines.push(format!("> {}", r.summary));
            }
            if let Some(counts) = rechecked(&r.previous) {
                lines.push(format!("Previous round's findings: {counts}."));
            }
            lines.extend(r.findings.iter().map(|f| {
                let carried = match (&f.now, f.carried) {
                    (Some(now), _) => format!(" (open since an earlier round; now: {now})"),
                    (None, true) => " (open since an earlier round)".to_string(),
                    _ => String::new(),
                };
                format!("- [P{}] {}{carried}", f.priority, f.text)
            }));
        }
        None if s.review_batched => lines.push(format!("**Review:** {}.", crate::review_batch::PENDING_NOTE)),
        None if s.yolo => lines.push(format!("**Review:** {}. This revision is unverified.", crate::project_yolo::NOTE)),
        None if s.review_disabled => lines.push(format!("**Review:** {}.", crate::review::DISABLED_NOTE)),
        None => lines.push("**Review:** none ran.".into()),
    }
    lines.push(String::new());
    let gate: Vec<String> = s.gate.results.iter().map(|r| format!("`{}`", r.command)).collect();
    if let Some(why) = &s.gate.skipped {
        lines.push(format!("**Gate** skipped at `{}`: {why}. No passing evidence.", s.commit));
    } else {
        lines.push(format!("**{} checks** {} at `{}`: {}.", s.gate.plan.as_ref().map_or("Configured gate",|p|p.depth.as_str()), if s.gate.passed { "passed" } else { "failed" }, s.commit, gate.join(", ")));
        if s.gate.plan.is_some() { lines.push("The full configured gate did not run for this submission; post-merge verification remains separate.".into()); }
    }
    lines.push(format!("{} files changed against `{}`.", s.files, s.base));
    lines.push(String::new());
    lines.push(format!(
        "`nitely review patch={} verdict={} approver={} reviewer={}`",
        s.patch_id,
        s.review.as_ref().map_or(if s.quick { crate::quick::VERDICT } else if s.yolo { "yolo" } else if s.review_disabled { "disabled" } else { "none" }, |r| r.verdict.as_str()),
        s.approver,
        s.review.as_ref().map_or("none", |r| r.reviewer.as_str())
    ));
    lines.join("\n")
}

/// The diff's stable id: the same change rebased onto a newer base keeps its id.
pub fn patch_id(worktree: &Path, base: &str) -> Result<String> {
    patch_id_range(worktree, base, "HEAD")
}

/// The patch-id of what `head` changes since it left `base` (`base...head`), as `patch_id`.
///
/// A submodule's move counts by the code it changes (`--submodule=diff`), not by the commit ids it
/// points at, so a rebase of the submodule that leaves its code the same keeps the patch. The
/// `Submodule <path> <a>..<b>:` header lines that its diff follows go (patch-id would start a new
/// patch at each). When git cannot show a move's code (`(commits not present)`, a new submodule),
/// every pointer the change moves counts by its full commit ids, so a pointer never changes unseen.
pub fn patch_id_range(worktree: &Path, base: &str, head: &str) -> Result<String> {
    // `-U0` does not reach the submodule's own diff, a git process of its own; config does.
    let diff = git(worktree, &["-c", "diff.context=0", "diff", "-U0", "--submodule=diff", &format!("{base}...{head}")])?;
    let mut unseen = false;
    let diff: String = diff
        .split_inclusive('\n')
        .filter(|l| {
            if !l.starts_with("Submodule ") {
                return true;
            }
            unseen |= !l.trim_end().ends_with(':');
            false
        })
        .collect();
    // The header abbreviates the commit ids; the raw diff has them whole.
    let pointers: String = if unseen {
        git(worktree, &["diff", "--raw", "--no-abbrev", "--no-renames", &format!("{base}...{head}")])?
            .lines()
            .filter(|l| l.split_whitespace().take(2).any(|m| m.trim_start_matches(':') == "160000"))
            .map(|l| format!("{l}\n"))
            .collect()
    } else {
        String::new()
    };
    if diff.is_empty() && pointers.is_empty() {
        return Ok("empty".into());
    }
    let id = if diff.is_empty() { String::new() } else { patch_id_of(worktree, diff)? };
    if pointers.is_empty() {
        return Ok(id);
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("{id}\n{pointers}").as_bytes());
    Ok(digest.iter().take(20).map(|b| format!("{b:02x}")).collect())
}

/// `git patch-id --verbatim` of `diff`: its first id.
pub(crate) fn patch_id_of(worktree: &Path, diff: String) -> Result<String> {
    use std::io::Write;
    let mut child = playbook::shell_free_command("git")
        .arg("-C")
        .arg(worktree)
        .args(["patch-id", "--verbatim"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(diff.as_bytes()));
    let out = child.wait_with_output()?;
    writer.join().ok();
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("unknown").to_string())
}

fn rebase_in_progress(worktree: &Path) -> bool {
    // Both paths from one git call, a line each.
    git(worktree, &["rev-parse", "--git-path", "rebase-merge", "--git-path", "rebase-apply"])
        .is_ok_and(|paths| paths.lines().any(|p| !p.trim().is_empty() && worktree.join(p.trim()).exists()))
}

/// Pushes the branch. `--force-with-lease` lets a rebased branch replace its own old head but
/// refuses when the remote moved to something this checkout has not seen.
fn push(pb: &Playbook, worktree: &Path, branch: &str) -> Result<()> {
    // A submodule with a remote first: the pull request's commits point into it.
    crate::submodules::push_task(pb, worktree, branch)?;
    git_remote(worktree, &["push", "--force-with-lease", "-u", "origin", &format!("HEAD:refs/heads/{branch}")]).map(|_| ())
}

impl Db {
    /// When the human last handed the issue back to agents (0 when never).
    /// The latest time the person handed the issue back to agents, as an event.
    pub fn handed_back_event(&self, issue_key: &str) -> Result<Option<Decision>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, ts FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'owner_changed' AND summary = 'Handed back to agents' ORDER BY id DESC LIMIT 1",
                [issue_key],
                |r| Ok(Decision { id: Some(r.get(0)?), ts: r.get(1)? }),
            )
            .optional()?)
    }

    pub fn handed_back_at(&self, issue_key: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(ts), 0) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'owner_changed' AND summary = 'Handed back to agents'",
            [issue_key],
            |r| r.get(0),
        )?)
    }

    pub fn record_review(&self, s: &Submitted) -> Result<()> {
        let (reviewer, verdict, findings, flagged, note) = match &s.review {
            Some(r) => (r.reviewer.as_str(), r.verdict.as_str(), to_json(&r.findings), to_json(&r.flagged), r.note.as_str()),
            None if s.review_batched => ("", crate::review_batch::PENDING, "[]".to_string(), "[]".to_string(), crate::review_batch::PENDING_NOTE),
            None if s.quick => ("", crate::quick::VERDICT, "[]".to_string(), "[]".to_string(), crate::quick::NOTE),
            None if s.yolo => ("", "yolo", "[]".to_string(), "[]".to_string(), crate::project_yolo::NOTE),
            None if s.review_disabled => ("", "disabled", "[]".to_string(), "[]".to_string(), crate::review::DISABLED_NOTE),
            None => ("", "none", "[]".to_string(), "[]".to_string(), ""),
        };
        // Approval cards need the commands and their outcomes, not potentially large console logs.
        let mut evidence = s.gate.clone();
        for result in &mut evidence.results {
            result.output.clear();
        }
        let batch = s.review.as_ref().and_then(|r| r.batch.as_ref()).map(|b| b.id);
        let (seconds, tokens) = (s.review.as_ref().map(|r| r.seconds as i64), s.review.as_ref().and_then(|r| r.tokens).map(|t| t as i64));
        self.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, note, ts, batch_id, seconds, tokens, solution, gate, branch, after_event)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, (SELECT COALESCE(MAX(id), 0) FROM event))",
            rusqlite::params![s.issue, s.patch_id, s.commit, reviewer, verdict, s.approver, to_json(&s.reasons), findings, flagged, note, now(), batch, seconds, tokens, s.solution.as_ref().map(to_json), to_json(&evidence), s.branch],
        )?;
        Ok(())
    }

    /// Every review of an issue's branch, newest first.
    pub fn reviews(&self, issue_key: &str) -> Result<Vec<ReviewRecord>> {
        self.reviews_before(issue_key, None)
    }

    /// `reviews`, only those recorded before the review `before` when given.
    pub fn reviews_before(&self, issue_key: &str, before: Option<i64>) -> Result<Vec<ReviewRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, ts, note, batch_id, solution, gate, branch, after_event
             FROM review WHERE issue_key = ?1 COLLATE NOCASE AND (?2 IS NULL OR id < ?2) ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![issue_key, before], |r| {
            Ok(ReviewRecord {
                issue_key: r.get(0)?,
                patch_id: r.get(1)?,
                commit_sha: r.get(2)?,
                branch: r.get(13)?,
                reviewer: r.get(3)?,
                verdict: r.get(4)?,
                approver: r.get(5)?,
                reasons: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                findings: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                ts: r.get(8)?,
                after_event: r.get(14)?,
                note: r.get(9)?,
                batch_id: r.get(10)?,
                solution: r.get::<_, Option<String>>(11)?.and_then(|s| serde_json::from_str(&s).ok()),
                gate: r.get::<_, Option<String>>(12)?.and_then(|s| serde_json::from_str(&s).ok()),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

/// Only a uniquely matching branch and revision is a route to this submitted change. A comment
/// on an older PR must not turn that PR into the approval's diff link.
pub fn review_pr<'a>(review: &ReviewRecord, prs: &'a [crate::db::PrRecord]) -> Option<&'a crate::db::PrRecord> {
    let branch = review.branch.as_deref().filter(|b| !b.is_empty())?;
    let commit = &review.commit_sha;
    if commit.len() < 7 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
    let mut matches = prs.iter().filter(|p| p.branch == branch && p.head_sha.starts_with(commit));
    let first = matches.next()?;
    if matches.next().is_some() { None } else { Some(first) }
}

fn to_json<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;
    use crate::test_repo::git as sh;
    use std::path::PathBuf;

    /// A review and a decision in the same second are told apart by the event the review saw
    /// (NIT-544); an older review, recorded before that was kept, is ordered by whole seconds.
    #[test]
    fn a_review_is_after_a_decision_when_it_saw_its_event() {
        let review = |ts: i64, after_event: Option<i64>| ReviewRecord {
            issue_key: "APP-7".into(), patch_id: "p".into(), commit_sha: "c".into(), branch: None,
            reviewer: "codex".into(), verdict: "pass".into(), approver: "agent".into(), reasons: vec![], findings: vec![], ts, after_event,
            note: String::new(), batch_id: None, solution: None, gate: None,
        };
        let decided = Decision { id: Some(40), ts: 100 };
        assert!(decided.came_before(&review(100, Some(40))), "same second, it saw the decision");
        assert!(!decided.came_before(&review(100, Some(39))), "same second, before the decision");
        assert!(!decided.came_before(&review(100, None)), "an older review in the same second: before, as it always was");
        assert!(decided.came_before(&review(101, None)));
        let unlinked = Decision { id: None, ts: 100 };
        assert!(!unlinked.came_before(&review(100, Some(99))), "a decision with no event: seconds");
    }

    #[test]
    fn approval_pr_matches_the_submission_not_recent_comments() {
        let pr = |number: i64, branch: &str, head: &str, updated: &str| crate::db::PrRecord {
            provider: "github".into(), repo: "example/acme".into(), number, title: "Change".into(),
            url: format!("https://github.com/example/acme/pull/{number}"), state: "OPEN".into(),
            branch: branch.into(), head_sha: head.into(), updated_at: updated.into(), comment_count: 0,
            approved_by: vec![], approved_by_me: false, checks: "success".into(),
        };
        let review = ReviewRecord {
            issue_key: "APP-7".into(), patch_id: "patch".into(), commit_sha: "bbbbbbb".into(), branch: Some("codex/app-7-current".into()),
            reviewer: "codex".into(), verdict: "pass".into(), approver: "human".into(), reasons: vec![], findings: vec![], ts: 0, after_event: None,
            note: String::new(), batch_id: None, solution: None, gate: None,
        };
        // Same issue, newest update first: the older PR just received a comment.
        let old = pr(1, "claude/app-7-old", "aaaaaaa111", "2026-10-20");
        let current = pr(2, "codex/app-7-current", "bbbbbbb222", "2026-10-19");
        assert_eq!(review_pr(&review, &[old.clone(), current.clone()]).unwrap().number, 2);
        assert!(review_pr(&review, &[old]).is_none());
        let advanced = pr(2, "codex/app-7-current", "ccccccc333", "2026-10-21");
        assert!(review_pr(&review, &[advanced]).is_none(), "the same PR now contains an unreviewed revision");
        assert!(review_pr(&ReviewRecord { branch: None, ..review.clone() }, std::slice::from_ref(&current)).is_none());
        assert!(review_pr(&ReviewRecord { commit_sha: String::new(), ..review.clone() }, std::slice::from_ref(&current)).is_none());
        assert!(review_pr(&review, &[current.clone(), current]).is_none(), "ambiguous identity is unavailable");
    }

    #[test]
    fn rounds_that_do_not_converge_go_to_a_person() {
        use crate::review::Finding;
        let f = |priority, text: &str, carried| Finding { priority, text: text.into(), carried, raised_as: None, now: None };
        let record = |findings: Vec<Finding>| ReviewRecord {
            issue_key: "NIT-1".into(),
            patch_id: "p".into(),
            commit_sha: "c".into(),
            branch: None,
            reviewer: "codex".into(),
            verdict: "changes".into(),
            approver: "agent".into(),
            reasons: vec![],
            findings,
            ts: 0, after_event: None,
            note: String::new(),
            batch_id: None,
            solution: None,
            gate: None,
        };
        let review = |findings: Vec<Finding>| Review {
            reviewer: "codex".into(),
            verdict: "changes".into(),
            findings,
            flagged: vec![],
            summary: String::new(),
            seconds: 1,
            previous: vec![],
            note: String::new(),
            batch: None,
            tokens: None,
            reused: None,
        };
        // Round 2 carrying a P1 once is still a round that may converge.
        let first = record(vec![f(1, "Drops the last row: a.rs", false)]);
        assert_eq!(not_converging(&[first.clone()], &review(vec![f(1, "Drops the last row: a.rs", true)])), None);
        // Round 3 carrying it a second time goes to a person, and says what is open.
        let second = record(vec![f(1, "Drops the last row: a.rs", true)]);
        let why = not_converging(&[second.clone(), first.clone()], &review(vec![f(1, "Drops the last row: a.rs", true), f(0, "Leaks the token: b.rs", false)])).unwrap();
        // One line: what happened and how many are open, never the findings themselves (NIT-633).
        assert_eq!(why, "1 P0/P1 finding still open after two rounds that tried to fix it; 2 open findings (1 P0, 1 P1)");
        // A finding carried at P2 (its code gone) or a new one does not count as carried twice.
        assert_eq!(not_converging(&[second.clone(), first.clone()], &review(vec![f(2, "Drops the last row: a.rs", true), f(1, "New: c.rs", false)])), None);
        // The fifth round in a row asking for changes goes to a person whatever was carried; a
        // review that did not run (no reviewer) is not a round, and a passing one ends the run.
        let none = ReviewRecord { reviewer: String::new(), ..first.clone() };
        let four = vec![record(vec![]), record(vec![]), none.clone(), record(vec![]), record(vec![])];
        let new = || review(vec![f(1, "Off by one: d.rs", false)]);
        let why = not_converging(&four, &new()).unwrap();
        assert_eq!(why, "5 review rounds in a row asked for changes; 1 open finding (1 P1)");
        assert_eq!(not_converging(&four[1..], &new()), None, "the fourth");
        let passed = ReviewRecord { verdict: "pass".into(), ..record(vec![]) };
        let after_a_pass = vec![record(vec![]), record(vec![]), passed, record(vec![]), record(vec![]), record(vec![])];
        assert_eq!(not_converging(&after_a_pass, &new()), None, "three in a row since the pass");
    }

    /// A repo whose playbook gates with `exit 0`, pushed to a bare remote. Only the fake `claude`
    /// reviewer is installed, so Claude's branches get a same-vendor review. It flags money
    /// whenever the diff touches a billing file, finds an edge case in a first look at an edge
    /// file, and on a re-review says that finding is still open.
    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        db: Db,
        actor: Actor,
        config: crate::Config,
    }

    const REVIEW_SH: &str = r#"prompt=$(cat)
case "$prompt" in *"CHANGE 2: NIT"*)
  echo "Two small changes."
  echo "- [P1] CHANGE 2: Wrong total: shared.txt, the second line"
  printf 'VERDICT 1: clean
MONEY 1: no
VERDICT 2: findings
MONEY 2: no
'
  exit 0;;
esac
files=$(git diff main...HEAD --name-only)
money=no; case "$files" in *billing*) money=yes;; esac
case "$prompt" in
  *"PREVIOUS <n>"*) case "$files" in
       *unstuck*) echo "PREVIOUS 1: fixed"; echo "VERDICT: clean";;
       *stuck*) echo "PREVIOUS 1: open stuck.txt:1 \`let stuck = 1;\` is still never read"; echo "VERDICT: clean";;
       *stale*) echo "PREVIOUS 1: open stale.txt:1 \`let stale = 1;\` is still never read"; echo "VERDICT: clean";;
       *worded*) echo "PREVIOUS 1: open worded.txt:1 \`let worded = 1;\` is still never read"; echo "VERDICT: clean";;
       *) echo "PREVIOUS 1: open edge.txt:1 \`let edge = 2;\` still skips the empty line"; echo "VERDICT: clean";;
     esac;;
  *) case "$files" in
       *edge*) echo "- [P2] Edge case: edge.txt, the empty line"; echo "VERDICT: findings";;
       *stuck*) echo "- [P1] Stuck: stuck.txt, the x is never read"; echo "VERDICT: findings";;
       *stale*) if [ -f ../stale-seen ]; then echo "Fine."; echo "VERDICT: clean"; else touch ../stale-seen; echo "- [P1] Stale: stale.txt, the x is never read"; echo "VERDICT: findings"; fi;;
       *worded*) if [ ! -f ../worded-seen ]; then touch ../worded-seen; echo "- [P1] No migration: worded.txt, load"; echo "- [P1] Worded: worded.txt, the x is never read"; echo "VERDICT: findings"
                 else case "$prompt" in *"(accepted: no outside users yet)"*) echo "Fine."; echo "VERDICT: clean";;
                   *) echo "- [P1] Old keys are dropped: worded.txt, load reads only the new format"; echo "VERDICT: findings";; esac; fi;;
       *) echo "Fine."; echo "VERDICT: clean";;
     esac;;
esac
echo "MONEY: $money"
"#;

    const PLAYBOOK: &str = r#"
[worktree]
dir = "../wt/{key}"
[gate]
commands = ["exit 0"]
[review]
reviewer = { "*" = "codex" }
commands = { claude = ["{sh}", "review.sh"], codex = ["nitely-test-no-such-codex"] }
[[review.flags]]
name = "MONEY"
question = "the change alters billing"
reason = "money logic"
"#;

    fn fixture() -> Fixture {
        let base = crate::test_repo::copy("submit", |base| {
            let repo = base.join("repo");
            let playbook = PLAYBOOK.replace("{sh}", &playbook::posix_sh());
            crate::test_repo::init(&repo, &[("review.sh", REVIEW_SH), (playbook::PLAYBOOK_PATH, &playbook)]);
            crate::test_repo::add_remote(&repo, &base.join("remote.git"));
        });
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        Fixture { repo: base.join("repo"), base, db, actor: Actor::Agent("claude:test".into()), config: crate::Config::default() }
    }

    fn new(title: &str) -> NewIssue {
        NewIssue {
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
        }
    }

    impl Fixture {
        /// An issue In Progress with its worktree started; nothing committed there yet.
        fn started(&mut self, title: &str) -> (IssueRecord, PathBuf) {
            let issue = self.db.create_issue(&new(title), &self.actor).unwrap();
            self.db.move_issue(&issue.key, "In Progress", None, None, &self.actor).unwrap();
            let started = playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap();
            (issue, PathBuf::from(started.worktree))
        }

        /// An issue whose worktree has `file` committed.
        fn committed(&mut self, title: &str, file: &str) -> (IssueRecord, PathBuf) {
            let (issue, wt) = self.started(title);
            std::fs::write(wt.join(file), "x").unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);
            (issue, wt)
        }

        fn submit(&mut self, wt: &Path, local_only: bool) -> Result<Submitted> {
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
            submit(&mut self.db, &self.config, &opts, &self.actor)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    /// A review round as a submit records it, of the worktree's head, `ago` seconds back: for tests
    /// about a later round, without running the earlier ones. Each finding is (priority, text, carried).
    fn round(f: &Fixture, key: &str, wt: &Path, verdict: &str, findings: &[(u8, &str, bool)], ago: i64) {
        let findings: Vec<crate::review::Finding> =
            findings.iter().map(|(priority, text, carried)| crate::review::Finding { priority: *priority, text: (*text).into(), carried: *carried, raised_as: None, now: None }).collect();
        f.db.conn
            .execute(
                "INSERT INTO review (issue_key, patch_id, commit_sha, branch, reviewer, verdict, approver, findings, note, ts) VALUES (?1, ?2, ?3, ?4, 'claude', ?5, 'agent', ?6, 'same vendor: codex not installed', ?7)",
                rusqlite::params![key, patch_id(wt, "main").unwrap(), sh(wt, &["rev-parse", "HEAD"]), sh(wt, &["branch", "--show-current"]), verdict, serde_json::to_string(&findings).unwrap(), crate::db::now() - ago],
            )
            .unwrap();
    }


    #[test]
    fn basic_submission_and_land_run_only_basic_checks_and_enqueue_an_audit() {
        let mut f=fixture();
        let policy="[worktree]\ndir='../wt/{key}'\n[lanes]\ncount=1\n[gate]\ncommands=['exit 37']\n[review]\nreviewer={ '*'='missing' }\ncommands={ missing=['nitely-test-do-not-launch'] }\n[verification]\nversion=1\nmode='active'\ncomments=['math[.]rs']\nbasic=['echo basic-checked']\naudit_percent=100\n[verification.backstop]\nenabled=true\n";
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH),policy).unwrap();
        std::fs::write(f.repo.join("math.rs"),"// old\nfn value()->i32 { 1 }").unwrap();
        sh(&f.repo,&["add","."]);sh(&f.repo,&["commit","-m","approved policy"]);
        let head=sh(&f.repo,&["rev-parse","HEAD"]).trim().to_string();
        crate::project_modes::set_for_test(&mut f.db,"NIT",crate::project_modes::Mode::Custom,crate::project_modes::Options{test_depth:Some(crate::project_modes::TestDepth::Risk),pre_review:Some(crate::project_modes::PreReview::Off),audit_percent:Some(100),full_backstop:Some(crate::project_modes::Backstop{enabled:true,landings:5,delay_seconds:1800}),..Default::default()});
        let (issue,wt)=f.started("Clarify an ordinary source comment");
        f.db.set_review_level(&issue.key,Some("inert"),Some("Comment-only candidate"),&Actor::Human).unwrap();
        crate::verification_backstop::record_landing(&f.db,&f.repo,"NIT",&issue.key,&head,&head,false).unwrap();
        f.db.conn.execute("UPDATE verification_health SET verified_head=?1,status='green',checked_at=?2",rusqlite::params![head,crate::db::now()]).unwrap();
        std::fs::write(wt.join("math.rs"),"// explained\nfn value()->i32 { 1 }").unwrap();
        sh(&wt,&["add","math.rs"]);sh(&wt,&["commit","-m","comment"]);
        let s=f.submit(&wt,true).unwrap();
        assert!(s.gate.passed && s.review_disabled && s.review.is_none(),"{s:?}");
        assert_eq!(s.gate.plan.as_ref().unwrap().depth,"Basic");
        assert!(s.gate.results.iter().all(|r|r.command!="exit 37"));
        assert_eq!(f.db.reviews(&issue.key).unwrap()[0].verdict,"disabled");
        let report=crate::land::land(&mut f.db,&f.config,&crate::land::LandOptions{dir:&f.repo.to_string_lossy(),issue_key:&issue.key,push:false,keep:true,break_lock:false},&f.actor).unwrap();
        assert!(report.landed,"{report:?}");
        assert!(report.warnings.is_empty(),"{report:?}");
        let (state,base,audited_head):(String,String,String)=f.db.conn.query_row("SELECT state,base,head FROM verification_audit WHERE issue_key=?1",[&issue.key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(state,"pending");
        assert_eq!(base,head);
        assert_eq!(audited_head,sh(&f.repo,&["rev-parse","main"]).trim());
    }

    #[test]
    fn tests_and_review_off_skip_automatic_checks_but_preserve_manual_checks_and_restored_policy() {
        let mut f = fixture();
        let policy = "[worktree]\ndir = \"../wt/{key}\"\n[gate]\ncommands = [\"exit 37\"]\n[review]\nreviewer = { \"*\" = \"missing\" }\ncommands = { missing = [\"nitely-test-do-not-launch\"] }\n";
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), policy).unwrap();
        sh(&f.repo, &["commit", "-qam", "checks must not launch"]);
        use crate::project_modes::{Mode, Options, PreReview, TestDepth};
        let off = || Options { test_depth: Some(TestDepth::Off), pre_review: Some(PreReview::Off), ..Default::default() };
        f.db.set_project_review_mode("NIT", Mode::Custom, off(), 0, &Actor::Human).unwrap();
        let (issue, wt) = f.committed("Unchecked feature", "feature.txt");
        let s = f.submit(&wt, true).unwrap();
        assert!(s.yolo && !s.gate.passed && s.gate.skipped.is_some() && s.review.is_none());
        assert_eq!(s.state, "In Review");
        assert_eq!(f.db.reviews(&issue.key).unwrap()[0].verdict, "yolo");
        assert!(!describe(&issue, &s, Default::default()).contains("**Gate** passed"));
        assert!(!playbook::run_gate(&f.db, &f.repo, &wt, Some(&issue.key), &f.actor).unwrap().passed, "manual checks still execute");
        let pb = Playbook::load(&f.repo).unwrap();
        // A review recorded under the retired Basic exemption never lands unreviewed (NIT-510).
        f.db.conn.execute("UPDATE review SET verdict = 'light' WHERE issue_key = ?1", [&issue.key]).unwrap();
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &s.branch).unwrap_err().to_string().contains("exemption is retired"));
        f.db.conn.execute("UPDATE review SET verdict = 'yolo' WHERE issue_key = ?1", [&issue.key]).unwrap();
        f.db.set_project_review_mode("NIT", Mode::Release, Options::default(), 1, &Actor::Human).unwrap();
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &s.branch).unwrap_err().to_string().contains("project review mode changed"));
        f.db.set_project_review_mode("NIT", Mode::Custom, off(), 2, &Actor::Human).unwrap();
        crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &s.branch).expect("the same policy restored covers the work checked under it (NIT-487)");
        f.submit(&wt, true).unwrap();
        let mut protected = pb.clone();
        protected.approval.blocked_paths.push(playbook::PathRule { pattern: "feature".into(), reason: "Protected".into() });
        assert!(crate::land::check_approval(&f.db, &protected, &issue.key, &wt, "main", &s.branch).is_err());
        let landed = crate::land::land(&mut f.db, &f.config, &crate::land::LandOptions { dir: &wt.to_string_lossy(), issue_key: &issue.key, push: false, keep: true, break_lock: false }, &f.actor).unwrap();
        assert!(landed.landed, "{landed:?}");
        assert!(landed.gate.as_ref().is_some_and(|g| !g.passed && g.skipped.is_some()));
        assert!(f.db.issue(&issue.key).unwrap().unwrap().evidence.contains(crate::project_yolo::NOTE));
    }

    #[test]
    fn custom_mode_controls_tests_and_review_independently_and_invalidates_old_evidence() {
        use crate::project_modes::{Mode, Options, PreReview, TestDepth};
        let mut f = fixture();
        let policy = format!("[worktree]\ndir='../wt/{{key}}'\n[gate]\ncommands=['exit 0']\n[review]\nreviewer={{ '*'='claude' }}\ncommands={{ claude=[{:?},'review.sh'] }}\n", playbook::posix_sh());
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), policy).unwrap();
        sh(&f.repo, &["commit", "-qam", "independent checks"]);
        f.db.set_project_review_mode("NIT", Mode::Custom, Options { test_depth: Some(TestDepth::Full), pre_review: Some(PreReview::Off), ..Default::default() }, 0, &Actor::Human).unwrap();
        let (issue, wt) = f.committed("Independent checks", "feature.txt");
        let full = f.submit(&wt, true).unwrap();
        assert!(full.gate.passed && full.gate.skipped.is_none() && full.review_disabled && full.review.is_none() && !full.yolo);
        assert_eq!(full.gate.results.len(), 1, "turning review off still executes tests");
        f.db.set_project_review_mode("NIT", Mode::Custom, Options { test_depth: Some(TestDepth::Off), pre_review: Some(PreReview::Individual), ..Default::default() }, 1, &Actor::Human).unwrap();
        let pb = Playbook::load(&f.repo).unwrap();
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &full.branch).unwrap_err().to_string().contains("mode changed"));
        let reviewed = f.submit(&wt, true).unwrap();
        assert!(!reviewed.gate.passed && reviewed.gate.skipped.is_some() && reviewed.gate.results.is_empty());
        assert!(!reviewed.review_disabled && !reviewed.yolo && reviewed.review.as_ref().is_some_and(|r| r.verdict == "pass"), "{reviewed:?}");
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &reviewed.branch).is_ok());
    }

    #[test]
    fn disabled_review_keeps_gate_and_landing_without_synthetic_review_or_fallback() {
        // All modes operate on one real change. Each submit still runs the gate and checks
        // authoritative policy; rebuilding the repository adds no coverage to this matrix.
        let mut f = fixture();
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Custom, crate::project_modes::Options { pre_review: Some(crate::project_modes::PreReview::Off), ..Default::default() });
        let (issue, wt) = f.committed("Unreviewed change", "feature.txt");
        let mut branch = String::new();
        for mode in ["off", "small", "all", "shadow"] {
            let policy = format!("[worktree]\ndir = \"../wt/{{key}}\"\n[gate]\ncommands = [\"exit 0\"]\n[review]\nenabled = false\nbatch = \"{mode}\"\nreviewer = {{ \"*\" = \"missing\" }}\ncommands = {{ missing = [\"nitely-test-do-not-launch\"] }}\n");
            std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), &policy).unwrap();
            sh(&f.repo, &["commit", "-qam", "disable review"]);
            let s = submit(&mut f.db, &f.config, &SubmitOptions { dir: &wt.to_string_lossy(), issue_key: None, skip_review: false, local_only: mode != "shadow", batch: true, solution: None, solution_commit: None, solution_patch_id: None }, &f.actor).unwrap();
            assert!(s.gate.passed && s.review_disabled && s.review.is_none(), "{s:?}");
            assert_eq!((s.approver.as_str(), s.state.as_str()), ("agent", "In Review"));
            assert!(s.reasons.is_empty());
            assert_eq!(s.pushed, mode == "shadow", "one mode exercises the remote boundary");
            let r = &f.db.reviews(&issue.key).unwrap()[0];
            assert_eq!((r.verdict.as_str(), r.note.as_str(), r.reviewer.as_str()), ("disabled", crate::review::DISABLED_NOTE, ""));
            assert!(describe(&issue, &s, Default::default()).contains(crate::review::DISABLED_NOTE));
            assert_eq!(f.db.conn.query_row("SELECT count(*) FROM review_queue", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(s.gate.results.len(), 1, "the configured gate actually ran for {mode}");
            branch = s.branch;
        }
        // Approval/revision enforcement depends on the recorded disabled verdict, not the
        // batching mode. Exercise it once, followed by a complete real land.
        let pb = Playbook::load(&f.repo).unwrap();
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &branch).unwrap().contains(crate::review::DISABLED_NOTE));
        // Turning review back on (the mode's, now) leaves the unreviewed change unable to land.
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Release, Default::default());
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &branch).is_err());
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Custom, crate::project_modes::Options { pre_review: Some(crate::project_modes::PreReview::Off), ..Default::default() });
        std::fs::write(wt.join("feature.txt"), "changed").unwrap();
        sh(&wt, &["commit", "-qam", "change after submit"]);
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &branch).unwrap_err().to_string().contains("branch changed"));
        sh(&wt, &["reset", "--hard", "HEAD~1"]);
        let landed = crate::land::land(&mut f.db, &f.config, &crate::land::LandOptions {
            dir: &wt.to_string_lossy(), issue_key: &issue.key, push: false, keep: true, break_lock: false,
        }, &f.actor).unwrap();
        assert!(landed.landed, "{landed:?}");
        assert_eq!(f.db.issue(&issue.key).unwrap().unwrap().state, "Done");
        assert!(f.repo.join("feature.txt").exists());
    }

    #[test]
    fn disabled_review_leaves_reviewer_conditions_unevaluated_and_other_approvals_intact() {
        let mut f = fixture();
        crate::project_modes::set_for_test(&mut f.db, "NIT", crate::project_modes::Mode::Custom, crate::project_modes::Options { pre_review: Some(crate::project_modes::PreReview::Off), ..Default::default() });
        let policy = PLAYBOOK.replace("{sh}", &playbook::posix_sh()).replace("[review]\n", "[review]\nenabled = false\nrequire_cross_vendor = true\n");
        std::fs::write(f.repo.join(playbook::PLAYBOOK_PATH), format!("{policy}\n[approval]\nalways_human = true\n")).unwrap();
        sh(&f.repo, &["commit", "-qam", "disable review"]);
        let (issue, wt) = f.committed("Approval conditions", "feature.txt");
        let s = f.submit(&wt, true).unwrap();
        assert!(s.review_disabled && s.review.is_none());
        assert_eq!(s.state, "Needs You");
        assert!(s.reasons.iter().any(|r| r.contains("Unevaluated reviewer condition MONEY")), "{:?}", s.reasons);
        assert!(s.reasons.iter().any(|r| r.contains("Unevaluated cross-vendor")));
        assert!(s.reasons.len() >= 3, "independent always-human condition remains");
        let pb = Playbook::load(&f.repo).unwrap();
        assert!(crate::land::check_approval(&f.db, &pb, &issue.key, &wt, "main", &s.branch).is_err());
    }

    #[test]
    fn submits_with_a_review_and_routes_flags_to_a_human() {
        let mut f = fixture();
        // The first change also goes through the push to the remote; the second stays local.
        for (title, file, approver, state, push) in
            [("Sidebar polish", "sidebar.txt", "agent", "In Review", true), ("Billing fix", "billing.txt", "human", "Needs You", false)]
        {
            let (issue, wt) = f.started(title);
            std::fs::write(wt.join(file), "x").unwrap();
            let empty = f.submit(&wt, !push).unwrap_err().to_string();
            assert!(empty.contains("no changes"), "{empty}");
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);

            let s = f.submit(&wt, !push).unwrap();
            assert!(s.gate.passed && s.pushed == push, "{s:?}");
            let review = s.review.as_ref().unwrap();
            assert_eq!((review.verdict.as_str(), review.reviewer.as_str()), ("pass", "claude"));
            assert_eq!(review.note, "same vendor: codex not installed");
            assert_eq!((s.approver.as_str(), s.state.as_str()), (approver, state), "{s:?}");
            assert_eq!(f.db.reviews(&issue.key).unwrap()[0].note, "same vendor: codex not installed");
            assert!(describe(&issue, &s, Default::default()).contains("**Review** by claude (same vendor: codex not installed)"));
            // A local bare remote is not a code host, so there is no PR, and it says why.
            assert!(s.pr_url.is_none() && (!push || s.pr_error.as_deref().unwrap().contains("not on GitHub")), "{s:?}");
            assert_eq!(f.db.reviews(&issue.key).unwrap()[0].patch_id, s.patch_id);
            assert!(describe(&issue, &s, Default::default()).contains(&format!("patch={}", s.patch_id)));
        }
    }

    #[test]
    fn a_p1_that_two_rounds_leave_open_goes_to_a_person() {
        let mut f = fixture();
        // A P1 that two rounds of fixes leave open goes to a person instead of a fourth round, and
        // its agent goes on (NIT-568: a project with no mode asks, as Release does).
        let (issue, wt) = f.started("Stuck fix");
        std::fs::write(wt.join("stuck.txt"), "let stuck = 1;").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "stuck"]);
        // Rounds one and two as submit records them: the P1 raised, then carried as open.
        let stuck = "Stuck: stuck.txt, the x is never read";
        round(&f, &issue.key, &wt, "changes", &[(1, stuck, false)], 2);
        round(&f, &issue.key, &wt, "changes", &[(1, stuck, true)], 1);
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Progress");
        let last: String = f
            .db
            .conn
            .query_row("SELECT summary FROM event WHERE issue_key = ?1 AND type = 'submit' ORDER BY id DESC LIMIT 1", [&issue.key], |r| r.get(0))
            .unwrap();
        // The note says how many are open, not the findings themselves (NIT-633).
        assert!(last.contains("not converging") && last.contains("1 open finding (1 P1)") && !last.contains("Stuck: stuck.txt"), "{last}");
        // A decision about the change, not the person's own work (NIT-220): the owner stays the agent,
        // and the open finding waits in the Inbox as a request to accept it, or send it back.
        let stuck = f.db.issue(&issue.key).unwrap().unwrap();
        assert_eq!((stuck.owner.as_str(), stuck.state.as_str()), ("agent", "In Progress"));
        let requests: Vec<crate::issues::Ask> = f.db.open_asks().unwrap().into_iter().filter(|a| a.issue_key.as_deref() == Some(issue.key.as_str())).collect();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].finding.as_ref().is_some_and(|f| f.priority == 1 && f.text.starts_with("Stuck: stuck.txt")), "{requests:?}");
        assert!(requests[0].question.contains("not converging"), "{}", requests[0].question);
        // Done is refused while the change is not landed: it is not on the integration branch.
        let done = f.db.move_issue(&issue.key, "Done", Some("Done by the human, outside Nitely."), None, &Actor::Human).unwrap_err().to_string();
        assert!(done.contains("never landed"), "{done}");
        // Sent back, the count starts over: the next round that carries it is not escalated.
        f.db.decline_finding(requests[0].id, "fix it properly", "").unwrap();
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Progress");
    }

    /// NIT-568: the question about a finding holds the land, never the agent, and a fix closes it.
    #[test]
    fn a_fix_closes_the_question_about_its_finding_without_an_answer() {
        let mut f = fixture();
        let (issue, wt) = f.started("Stuck once");
        std::fs::write(wt.join("stuck.txt"), "let stuck = 1;").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "stuck"]);
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Progress");
        // The agent asks to have it accepted; the issue stays with it, and the review still blocks.
        let ask = f.db.request_acceptance(&issue.key, "1", "The x is read by the next change.", None, &f.actor).unwrap();
        assert_eq!(f.db.issue(&issue.key).unwrap().unwrap().state, "In Progress", "the question holds the land, not the agent");
        let latest = f.db.reviews(&issue.key).unwrap().remove(0);
        assert!(!f.db.review_passes(&issue.key, &latest).unwrap(), "land waits for a fix or an acceptance");
        // It fixes it meanwhile; the next round no longer raises it, and the question closes unanswered.
        std::fs::write(wt.join("unstuck.txt"), "read(stuck);").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "read it"]);
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Review");
        let closed = f.db.ask(ask.id).unwrap().unwrap();
        assert_eq!((closed.answer, closed.closed.as_deref()), (None, Some(crate::accept::FIXED)));
        assert!(f.db.open_asks().unwrap().iter().all(|a| a.issue_key.as_deref() != Some(issue.key.as_str())), "it left the Inbox");
    }

    #[test]
    fn a_signed_out_reviewer_is_passed_over() {
        let mut f = fixture();
        // A preferred reviewer that is installed but signed out is passed over: the next one
        // reviews, and the note says why.
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("codex = [\"nitely-test-no-such-codex\"]", &format!("codex = [\"{}\", \"-c\", \"echo 'Error: not logged in' >&2; exit 1\"]", playbook::posix_sh()))).unwrap();
        let (issue, wt) = f.committed("Signed out", "signed.txt");
        let s = f.submit(&wt, true).unwrap();
        let review = s.review.as_ref().unwrap();
        assert_eq!((review.reviewer.as_str(), review.note.as_str()), ("claude", "same vendor: codex could not run: not signed in"), "{s:?}");
        let failed: String = f
            .db
            .conn
            .query_row("SELECT summary FROM event WHERE issue_key = ?1 AND type = 'review_failed'", [&issue.key], |r| r.get(0))
            .unwrap();
        assert!(failed.contains("codex could not run (not signed in)"), "{failed}");
    }

    #[test]
    fn a_cross_vendor_rule_sends_a_same_vendor_review_to_a_human() {
        let mut f = fixture();
        // A playbook that requires another vendor's review sends a same-vendor one to a human, and
        // with no reviewer installed at all, submit says what to install.
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("[review]\n", "[review]\nrequire_cross_vendor = true\n")).unwrap();
        let (_, wt) = f.committed("Footer tweak", "footer.txt");
        let s = f.submit(&wt, true).unwrap();
        assert_eq!((s.approver.as_str(), s.state.as_str()), ("human", "Needs You"), "{s:?}");
        assert!(s.reasons[0].contains("requires a review by another vendor"), "{:?}", s.reasons);
        std::fs::write(&pb_path, pb.replace("\"{sh}\", \"review.sh\"]".replace("{sh}", &playbook::posix_sh()).as_str(), "\"nitely-test-no-such-claude\"]")).unwrap();
        let none = f.submit(&wt, true).unwrap_err().to_string();
        assert!(none.contains("no reviewer can run") && none.contains("nitely-test-no-such"), "{none}");
    }

    #[test]
    fn a_failed_push_hands_nothing_in_and_a_repo_without_origin_is_local() {
        let mut f = fixture();
        // A push that fails hands nothing in, so the issue stays In Progress.
        sh(&f.repo, &["remote", "set-url", "origin", &f.base.join("missing.git").to_string_lossy()]);
        let (_, wt) = f.committed("Header tweak", "header.txt");
        let s = f.submit(&wt, false).unwrap();
        assert!(!s.pushed && s.pr_error.as_deref().unwrap().contains("push failed"), "{s:?}");
        assert_eq!(s.state, "In Progress");
        assert!(s.next.contains("push failed"), "{}", s.next);

        // A repo with no origin at all is local: submit hands the change in without a push (land
        // without one is land.rs's fixture, which has no remote).
        sh(&f.repo, &["remote", "remove", "origin"]);
        let (_, wt) = f.committed("Local tweak", "local.txt");
        let s = f.submit(&wt, false).unwrap();
        assert!(!s.pushed && s.pr_error.is_none(), "{s:?}");
        assert_eq!(s.state, "In Review", "{s:?}");
    }

    /// Under a mode that reviews each land batch (NIT-509), submit gates a change and leaves its
    /// review to its land batch, unless something makes it a change reviewed alone, at once.
    #[test]
    fn a_batched_projects_submit_leaves_the_review_to_the_land_batch_unless_it_must_be_reviewed_alone() {
        let mut f = fixture();
        // A migrated project's mode, not yet saved, so queued work has no authority to lose.
        f.db.conn.execute("INSERT OR REPLACE INTO project_review_mode(project,mode,overrides,explicit) VALUES('NIT','custom','{\"pre_review\":\"batched\"}',0)", []).unwrap();
        let (waits, wt) = f.committed("Small tweak", "tweak.txt");
        let s = f.submit(&wt, true).unwrap();
        assert!(s.review_batched && s.review.is_none() && s.state == "In Review", "{s:?}");
        assert!(s.next.contains("Call land"), "{}", s.next);
        let row = &f.db.reviews(&waits.key).unwrap()[0];
        assert_eq!((row.verdict.as_str(), row.approver.as_str(), row.patch_id.as_str()), (crate::review_batch::PENDING, "agent", s.patch_id.as_str()));
        assert!(!f.db.timeline(&waits.key).unwrap().iter().any(|t| t.kind == "review_started"), "no reviewer ran");

        // A change to the playbook is reviewed now, alone, and the timeline says why.
        let (alone, wt) = f.started("Playbook tweak");
        let pb = std::fs::read_to_string(wt.join(playbook::PLAYBOOK_PATH)).unwrap();
        std::fs::write(wt.join(playbook::PLAYBOOK_PATH), format!("{pb}\n# a note\n")).unwrap();
        sh(&wt, &["commit", "-q", "-am", "playbook"]);
        let s = f.submit(&wt, true).unwrap();
        assert!(s.review.is_some() && !s.review_batched, "{s:?}");
        let said: Vec<String> = f.db.timeline(&alone.key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s == "Reviewed now, alone, not with its land batch: it changes the playbook"), "{said:?}");
    }

    #[test]
    fn the_same_edit_somewhere_else_is_reviewed_though_its_patch_id_is_the_same() {
        let mut f = fixture();
        let two = |a: u8, b: u8| format!("fn a() {{\n    let x = {a};\n}}\n\n// one\n// two\n// three\n// four\n\nfn b() {{\n    let x = {b};\n}}\n");
        std::fs::write(f.repo.join("two.rs"), two(1, 1)).unwrap();
        sh(&f.repo, &["add", "."]);
        sh(&f.repo, &["commit", "-q", "-m", "two functions"]);
        sh(&f.repo, &["push", "-q", "origin", "main"]);
        let (_, wt) = f.started("Same edit");
        std::fs::write(wt.join("two.rs"), two(2, 1)).unwrap();
        sh(&wt, &["commit", "-q", "-am", "x in a"]);
        let in_a = f.submit(&wt, true).unwrap();
        assert!(in_a.review.as_ref().is_some_and(|r| r.verdict == "pass"));
        // The same line change, made in b instead: git's patch-id cannot tell them apart.
        sh(&wt, &["reset", "-q", "--hard", "HEAD~1"]);
        std::fs::write(wt.join("two.rs"), two(1, 2)).unwrap();
        sh(&wt, &["commit", "-q", "-am", "x in b"]);
        let in_b = f.submit(&wt, true).unwrap();
        assert_eq!(in_b.patch_id, in_a.patch_id);
        assert!(in_b.review.as_ref().is_some_and(|r| r.reused.is_none()), "another change gets its own review: {in_b:?}");
    }

    #[test]
    fn a_review_is_told_what_the_change_is_for_what_the_author_said_and_what_was_accepted_elsewhere() {
        let mut f = fixture();
        // The context reads only the database and the gate: a round recorded, and the gate it passed.
        let issue = f.db.create_issue(&new("Footer links"), &f.actor).unwrap();
        f.db.conn
            .execute("INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, ts) VALUES (?1, 'p', 'c', 'claude', 'pass', 'agent', ?2)", rusqlite::params![issue.key, crate::db::now()])
            .unwrap();
        let ok = playbook::CommandResult { step: None, command: "exit 0".into(), ok: true, seconds: 0, output: String::new(), skipped: false, timed_out: false };
        let gate = GateResult { mode_authority: None, plan: None, skipped: None, passed: true, commit: "c".into(), results: vec![ok], warnings: vec![], flaked: vec![], lane_run: None };
        // Since that round: the author's note, and a finding the person accepted on another issue.
        f.db.add_note(&issue.key, "The empty line is kept on purpose: the footer needs it.", "note", &f.actor).unwrap();
        // One acceptance the person signed, and one an agent wrote itself, which must not count.
        let secret = [9u8; 32];
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&secret)));
        let accept = |finding: &str, reason: &str, signature: String| {
            f.db.conn
                .execute(
                    "INSERT INTO accepted_finding (issue_key, patch_id, priority, finding, reason, ts, signature) VALUES ('NIT-99', 'p', 1, ?1, ?2, 5, ?3)",
                    rusqlite::params![finding, reason, signature],
                )
                .unwrap();
        };
        let signed = crate::approval::sign(&secret, &crate::accept::message("NIT-99", "p", "No migration: keys.txt, load", "no outside users yet", 5));
        accept("No migration: keys.txt, load", "no outside users yet", signed);
        accept("Never log in: auth.rs, login", "forged", "zz".into());
        let issue = f.db.issue(&issue.key).unwrap().unwrap();
        let c = review_context(&f.db, &issue, &gate).unwrap();
        assert!(c.contains(&format!("its issue {} \"Footer links\"", issue.key)) && c.contains("does what the issue asks"), "{c}");
        assert!(c.contains("The gate passed on this tree: `exit 0`."), "{c}");
        assert!(c.contains("- The empty line is kept on purpose: the footer needs it."), "{c}");
        assert!(c.contains("- [P1] No migration: keys.txt, load (accepted: no outside users yet)"), "{c}");
        assert!(c.contains("They are not exemptions"), "{c}");
        assert!(!c.contains("forged") && !c.contains("Never log in"), "an unsigned row is no acceptance: {c}");
        // A finding the person is deciding: the round is told its words, to say it the same way (NIT-353).
        let asked = f.db.create_ask("The agent's reasoning.", &[], Some(&issue.key), None, &f.actor).unwrap();
        // A long one too: the round can repeat only the words it was given, so it gets them all.
        let words = format!("Drops a row: rows.txt, export: the last row{}", ", and then the next".repeat(60));
        let asked_finding = serde_json::json!({ "priority": 1, "text": words });
        f.db.conn.execute("UPDATE ask SET finding = ?2 WHERE id = ?1", rusqlite::params![asked.id, asked_finding.to_string()]).unwrap();
        let c = review_context(&f.db, &issue, &gate).unwrap();
        assert!(c.contains("report it in exactly these words") && c.contains(&format!("- [P1] {words}\n")), "{c}");
        assert!(!c.contains("The agent's reasoning."), "{c}");
    }

    #[test]
    fn a_gate_step_that_ran_past_the_lane_limit_is_not_a_failure_to_fix() {
        let step = |command: &str, timed_out: bool| playbook::CommandResult { step: None, command: command.into(), ok: false, seconds: 2700, output: String::new(), skipped: false, timed_out };
        let gate = |r: playbook::CommandResult| GateResult { mode_authority: None, plan: None, skipped: None, passed: false, commit: "abc1234".into(), results: vec![r], warnings: vec![], flaked: vec![], lane_run: Some(250) };
        let stopped = after_failed_gate(&gate(step("npm run test:gate", true)));
        assert!(stopped.contains("did not finish") && stopped.contains("`npm run test:gate` ran past the lane's time limit") && !stopped.contains("Fix it"), "{stopped}");
        assert_eq!(after_failed_gate(&gate(step("cargo test", false))), "The gate failed at `cargo test`. Fix it, commit, and submit again.");
        // Stored gates from before the field read as not timed out.
        let old: playbook::CommandResult = serde_json::from_str(r#"{"command":"x","ok":false,"seconds":1,"output":""}"#).unwrap();
        assert!(!old.timed_out);
    }

    #[test]
    fn the_fresh_review_is_told_what_the_person_accepted_so_a_reworded_repeat_does_not_reach_them() {
        let mut f = fixture();
        // Round one raises two P1s; the person accepts one, and the rounds after carry the other as
        // open. The fresh review no longer finds the open one, and would say the accepted one again
        // in other words unless it is told the person accepted it (NIT-331).
        let (issue, wt) = f.started("Worded again");
        std::fs::write(wt.join("worded.txt"), "let worded = 1;").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "worded"]);
        let first = f.submit(&wt, true).unwrap();
        assert_eq!(first.state, "In Progress");
        let secret = [7u8; 32];
        crate::approval::trust_in_tests(Some(crate::approval::public_of(&secret)));
        let (finding, reason) = ("No migration: worded.txt, load", "no outside users yet");
        let signature = crate::approval::sign(&secret, &crate::accept::message(&issue.key, &first.patch_id, finding, reason, 5));
        f.db.conn
            .execute(
                "INSERT INTO accepted_finding (issue_key, patch_id, priority, finding, reason, ts, signature) VALUES (?1, ?2, 1, ?3, ?4, 5, ?5)",
                rusqlite::params![issue.key, first.patch_id, finding, reason, signature],
            )
            .unwrap();
        // Round two as submit records it: the accepted finding no longer told, the other carried open.
        round(&f, &issue.key, &wt, "changes", &[(1, "Worded: worded.txt, the x is never read", true)], 0);
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Review");
        let last = &f.db.reviews(&issue.key).unwrap()[0];
        assert!(last.note.ends_with("fresh, without the earlier rounds") && last.verdict == "pass", "{last:?}");
        assert!(last.findings.is_empty(), "the accepted finding is not raised again: {:?}", last.findings);
        assert!(f.db.open_asks().unwrap().iter().all(|a| a.issue_key.as_deref() != Some(issue.key.as_str())), "nothing for the person");
    }
}
