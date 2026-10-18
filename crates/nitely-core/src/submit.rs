//! `submit`: a finished branch becomes a reviewed pull request. It runs the gate, has the other
//! vendor review the branch, decides whether a human must approve it, pushes, opens or updates the
//! pull request, records the review for the exact patch so landing can check it, and moves the
//! issue, as the repo's playbook says.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::playbook::{self, git, git_remote, GateResult, Playbook};
use crate::review::{Finding, PreviousRound, Review};

#[derive(Debug, Clone, Serialize)]
pub struct Submitted {
    pub issue: String,
    pub branch: String,
    pub base: String,
    /// `git patch-id` of the branch's diff: the review applies to this patch and no other.
    pub patch_id: String,
    pub commit: String,
    pub files: usize,
    pub gate: GateResult,
    /// None when the gate failed (nothing else ran) or the review was skipped.
    pub review: Option<Review>,
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
    /// With `[review] batch = "shadow"`: the change to review again in a shadow batch once this
    /// submit has handed its result back (NIT-217).
    #[serde(skip)]
    pub shadow: Option<crate::review_batch::Shadow>,
    /// The issue's state afterwards.
    pub state: String,
    pub next: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewRecord {
    pub issue_key: String,
    pub patch_id: String,
    pub commit_sha: String,
    pub reviewer: String,
    pub verdict: String,
    pub approver: String,
    pub reasons: Vec<String>,
    pub findings: Vec<crate::review::Finding>,
    pub ts: i64,
    /// As `Review::note`: how independent the review was, e.g. `same vendor: codex not installed`.
    pub note: String,
    /// The review batch it was reviewed in, with other changes in one reviewer call (NIT-216).
    pub batch_id: Option<i64>,
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
}

pub fn submit(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor) -> Result<Submitted> {
    let (worktree, branch) = playbook::worktree_and_branch(opts.dir)?;
    let (_, branch_key) = crate::git::parse_branch(&branch);
    let root = playbook::repo_root(config, opts.dir).with_context(|| format!("{} is not inside a git repo", opts.dir))?;
    let pb = Playbook::load(&root)?;
    let base = pb.integration_branch().to_string();
    if branch.is_empty() {
        bail!("{} has no commit on its branch yet; submit from the worktree start_task made", worktree.display());
    }
    if branch == base || branch == "HEAD" {
        bail!("{branch} is not a task branch; submit from the worktree start_task made");
    }
    let key = opts.issue_key.map(str::to_string).or(branch_key).context("the branch names no issue; pass the issue key")?;
    let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
    if rebase_in_progress(&worktree) {
        bail!("a rebase is in progress in {}", worktree.display());
    }
    let dirty = git(&worktree, &["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{} has uncommitted changes; commit them first:\n{}", worktree.display(), dirty.trim_end());
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

    let files: Vec<String> = git(&worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    pb.approval.check_blocked(&files)?;
    crate::private_terms::check(issue.project.as_deref(), &worktree, &since)?;
    let gate = playbook::run_gate(db, &root, &worktree, Some(&issue.key), actor)?;
    let mut out = Submitted {
        issue: issue.key.clone(),
        branch: branch.clone(),
        base: base.clone(),
        patch_id: patch_id(&worktree, &since)?,
        commit: gate.commit.clone(),
        files: files.len(),
        gate,
        review: None,
        approver: "human".into(),
        reasons: vec![],
        approval_stands: false,
        pushed: false,
        pr_url: None,
        pr_error: None,
        land: None,
        shadow: None,
        state: issue.state.clone(),
        next: String::new(),
    };
    if !out.gate.passed {
        let failed = out.gate.results.last().map(|r| r.command.as_str()).unwrap_or("");
        out.next = format!("The gate failed at `{failed}`. Fix it, commit, and submit again.");
        return Ok(out);
    }

    out.reasons = pb.human_reasons(&files)?;
    if opts.skip_review {
        out.reasons.push("no review ran".into());
    } else {
        let (vendor, _) = crate::git::parse_branch(&branch);
        let vendor = vendor.unwrap_or_default();
        let previous = previous_round(db, &issue.key, &worktree)?;
        // From a batch: the review, or what its review found for this change that its single review
        // must carry (an interaction with another change).
        let (batched, extra) = if opts.batch && matches!(pb.review.batch, crate::review::BatchMode::Small | crate::review::BatchMode::All) {
            in_a_batch(db, &pb, &issue, &root, &worktree, &branch, &since, &files, &vendor, &out.reasons, previous.as_ref(), actor)?
        } else {
            (None, vec![])
        };
        // A reviewer that cannot run (not signed in, say) is passed over for the next candidate;
        // one that ran and did not finish its review stops the submit.
        let mut failed: Vec<crate::review::CouldNotRun> = vec![];
        let (choice, mut review) = if let Some(done) = batched { done } else { loop {
            let choice = pb.review.choose_after(&vendor, &|r| pb.review.is_installed(r, &worktree), &failed)?;
            let reviewer = choice.reviewer.clone();
            let started = format!("Review by {reviewer}{} started", noted(&choice.note));
            db.log_event(actor, Some(&issue.key), Some(&wt), "review_started", &started)?;
            match pb.review.run(&reviewer, &worktree, &since, previous.as_ref()) {
                Ok(r) => break (choice, r),
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
        review.findings.extend(extra);
        crate::review_batch::hold_interactions(db, &worktree, &mut review)?;
        let reviewer = choice.reviewer.clone();
        review.note = choice.note.clone();
        if opts.batch && pb.review.batch == crate::review::BatchMode::Shadow {
            out.shadow = shadow_of(db, &pb, &issue, &root, &worktree, &branch, &since, &files, &vendor, &choice, &out.reasons, previous.as_ref())?;
        }
        out.reasons.extend(pb.review.independence_reason(&reviewer, &vendor, &choice.note));
        out.reasons.extend(pb.review.flag_reasons(&review));
        out.review = Some(review);
    }
    out.approver = if out.reasons.is_empty() { "agent" } else { "human" }.into();
    let mut verdict = out.review.as_ref().map_or("none", |r| r.verdict.as_str()).to_string();
    // Findings the person accepted as they are, signed for this very patch, block nothing: the
    // review is recorded as it asked, and goes on as a passing one (land checks the same).
    let accepted_only = match &out.review {
        Some(r) if verdict == "changes" => {
            let accepted = db.accepted_for(&issue.key, &out.patch_id)?;
            r.findings.iter().filter(|f| f.priority <= 1).all(|f| accepted.iter().any(|a| a.finding == f.text))
        }
        _ => false,
    };
    if accepted_only {
        verdict = "pass".into();
    }
    // Read before this round is recorded: the rounds before it, since the human last decided on the
    // issue's findings or handed it back (their decision to go on starts a new count).
    let stalled = match &out.review {
        Some(r) if verdict == "changes" && issue.origin == "nitely" => {
            let since = db.handed_back_at(&issue.key)?.max(db.findings_decided_at(&issue.key)?);
            let earlier: Vec<ReviewRecord> = db.reviews(&issue.key)?.into_iter().filter(|r| r.ts > since).collect();
            not_converging(&earlier, r)
        }
        _ => None,
    };
    db.record_review(&out)?;
    if let Some(shadow) = out.shadow.as_mut() {
        shadow.review_id = db.conn.last_insert_rowid();
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
    if !local_only {
        match push(&worktree, &branch) {
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
    let by = |r: &Review| match r.batched_with() {
        Some(with) => format!("{}{}, in a batch with {with},", r.reviewer, noted(&r.note)),
        None => format!("{}{}", r.reviewer, noted(&r.note)),
    };
    let outcome = match (&out.review, out.approver.as_str()) {
        (Some(r), _) if accepted_only => format!("Review by {} asked for changes only in findings you accepted, so it passes", by(r)),
        (Some(r), _) if r.verdict == "changes" => format!("Review by {} requested changes", by(r)),
        (Some(r), "human") if out.approval_stands => format!("Review by {} passed; the human's approval of this patch stands", by(r)),
        (Some(r), "human") => format!("Review by {} passed; waits for a human", by(r)),
        (Some(r), _) => format!("Review by {} passed; ready to land", by(r)),
        (None, _) => "Submitted without a review; waits for a human".to_string(),
    };
    let outcome = match out.review.as_ref().map(|r| rechecked(&r.previous)) {
        Some(Some(counts)) => format!("{outcome} (previous round: {counts})"),
        _ => outcome,
    };
    let outcome = if pb.land.by.by_nitely() && verdict != "changes" { format!("{outcome}; it {}", pb.land.by.describe()) } else { outcome };
    let outcome = match &stalled {
        Some(why) => format!("{outcome}. The review rounds are not converging, so it is handed to you: {why}"),
        None => outcome,
    };
    // The full description is on the pull request and in the app's review section.
    db.add_note(&issue.key, &format!("{outcome}. {pr_line}"), "submit", actor)?;

    let push_failed = !local_only && !out.pushed;
    let by_nitely = pb.land.by.by_nitely();
    let (state, next) = if stalled.is_some() {
        // Another round would cost as much again and likely end the same: a person decides each open
        // finding (accept it as it is, signed, or send it back). The issue stays the agents' work: a
        // change that failed review is never the person's own task to mark done.
        ("Needs You", "The review rounds are not converging, so Nitely asked the human to decide each open P0/P1 finding in the Inbox: accept it as it is, or send it back to be fixed. Stop working on it and pick up other work. If you have reasoning for a finding (a trade-off they decided, say), add it in a `note` on the issue: they read it beside the finding.")
    } else if verdict == "changes" {
        ("In Progress", "The review found P0 or P1 problems. Fix them, commit, and submit again. If one is a trade-off the human decided or should decide, not a defect (they ruled out what it asks for), call request_acceptance with it and your reasoning instead of looping on it.")
    } else if push_failed {
        // Not handed in: nobody can see or merge the branch, so it must not look ready to land.
        ("In Progress", "The push failed, so the change is not handed in. Fix the remote or its access (ask the human if it is a permission error), then submit again.")
    } else if out.approval_stands {
        ("Approved", "The human approved this exact patch before and the review raised nothing new, so the approval stands. Call land.")
    } else if out.approver == "human" && by_nitely {
        ("Needs You", "It waits for the human's approval in the Nitely inbox, and Nitely lands it as soon as they approve it; you do not call land. Nitely already asked them, so do not `ask` for this approval yourself. Pick up other work meanwhile.")
    } else if out.approver == "human" {
        ("Needs You", "It waits for the human's approval in the Nitely inbox. Nitely already asked them, so do not `ask` for this approval yourself. Pick up other work meanwhile; once it is Approved, call land.")
    } else {
        ("In Review", "The review passed and no human approval is needed. Call land to put it on the integration branch.")
    };
    if let Some(why) = &stalled {
        out.state = db.ask_to_decide_findings(&issue.key, why, actor)?;
    } else if issue.state != state && issue.origin == "nitely" {
        out.state = db.move_issue(&issue.key, state, None, None, actor)?.state;
    }
    out.next = next.into();

    // In a project that lands by Nitely, what is ready to land lands now, in this submit. An
    // approved change waits for the app where only the app can check approvals (macOS).
    let app_lands = state == "Approved" && !crate::approval::agents_can_verify();
    if by_nitely && matches!(state, "In Review" | "Approved") && !app_lands {
        let root_dir = root.to_string_lossy().into_owned();
        let landed = crate::land::land_now(db, config, &issue.key, &root_dir, !local_only, actor);
        let (state, next) = crate::land::after_auto_land(db, &issue.key, &landed, actor)?;
        out.state = state;
        out.next = next;
        out.land = landed.ok();
    }
    Ok(out)
}

/// A review from a batch with the reviewer that ran it, or None, and what a batch found that a
/// single review must carry.
type FromBatch = (Option<(crate::review::Choice, Review)>, Vec<Finding>);

/// The change's review from a review batch, when its project batches reviews and it may join one;
/// otherwise None (the timeline says why), with what a batch found for it that its single review
/// must carry.
#[allow(clippy::too_many_arguments)]
fn in_a_batch(
    db: &Db,
    pb: &Playbook,
    issue: &IssueRecord,
    root: &Path,
    worktree: &Path,
    branch: &str,
    since: &str,
    files: &[String],
    vendor: &str,
    reasons: &[String],
    previous: Option<&PreviousRound>,
    actor: &Actor,
) -> Result<FromBatch> {
    let wt = worktree.to_string_lossy().into_owned();
    let choice = pb.review.choose(vendor, &|r| pb.review.is_installed(r, worktree))?;
    let lines = crate::review_batch::changed_lines(worktree, since)?;
    let large_repo = pb.lanes.as_ref().is_some_and(|l| l.large_repo);
    let moves_submodule = pb.submodules.iter().find(|s| files.iter().any(|f| f == s.path())).map(|s| format!("it moves the submodule {}, whose code is reviewed with it", s.path()));
    let why_alone = match moves_submodule {
        Some(why) => Some(why),
        None => crate::review_batch::why_alone(db, &pb.review, vendor, &choice, reasons, files, lines, previous, large_repo, &issue.key)?,
    };
    if let Some(why) = why_alone {
        db.log_event(actor, Some(&issue.key), Some(&wt), "status", &format!("Reviewed alone, not in a batch: {why}"))?;
        return Ok((None, vec![]));
    }
    let candidate = crate::review_batch::Candidate { issue, repo: root, worktree, branch, base: since, files, lines, choice: &choice };
    match crate::review_batch::review_in_batch(db, &pb.review, &candidate, actor)? {
        crate::review_batch::Outcome::Reviewed(review) => Ok((Some((choice, review)), vec![])),
        crate::review_batch::Outcome::Alone(why, carried) => {
            db.log_event(actor, Some(&issue.key), Some(&wt), "status", &format!("Reviewed alone after all: {why}"))?;
            Ok((None, carried))
        }
    }
}

/// The change to review again in a shadow batch, when it could have joined a real one.
#[allow(clippy::too_many_arguments)]
fn shadow_of(
    db: &Db,
    pb: &Playbook,
    issue: &IssueRecord,
    root: &Path,
    worktree: &Path,
    branch: &str,
    since: &str,
    files: &[String],
    vendor: &str,
    choice: &crate::review::Choice,
    reasons: &[String],
    previous: Option<&PreviousRound>,
) -> Result<Option<crate::review_batch::Shadow>> {
    let lines = crate::review_batch::changed_lines(worktree, since)?;
    let large_repo = pb.lanes.as_ref().is_some_and(|l| l.large_repo);
    if crate::review_batch::why_alone(db, &pb.review, vendor, choice, reasons, files, lines, previous, large_repo, &issue.key)?.is_some() {
        return Ok(None);
    }
    let rev = |r: &str| git(worktree, &["rev-parse", r]).map(|o| o.trim().to_string());
    Ok(Some(crate::review_batch::Shadow {
        issue: issue.key.clone(),
        title: issue.title.clone(),
        repo: crate::paths::normalize(&root.to_string_lossy()),
        worktree: worktree.to_string_lossy().into_owned(),
        branch: branch.to_string(),
        base: rev(since)?,
        head: rev("HEAD")?,
        files: files.to_vec(),
        lines,
        reviewer: choice.reviewer.clone(),
        note: choice.note.clone(),
        review_id: 0,
        rules: pb.review.clone(),
    }))
}

/// How many review rounds in a row may ask for changes before the issue goes to a person.
pub const MAX_ROUNDS: usize = 5;

/// Why an issue's review rounds have stopped converging, given the reviews before this one
/// (newest first) and this one, which asks for changes: a P0 or P1 finding still open after two
/// rounds that tried to fix it (carried a second time), or the `MAX_ROUNDS`th round in a row that
/// asks for changes. A passing review ends a run: rounds after it (a rebase, a land that stopped)
/// are not a loop. The text is the short summary the person reads.
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
    let open: Vec<String> = review
        .findings
        .iter()
        .filter(|f| f.priority <= 1)
        .map(|f| match &f.now {
            Some(now) => format!("[P{}] {} (the reviewer says it is still open: {now})", f.priority, f.text),
            None => format!("[P{}] {}", f.priority, f.text),
        })
        .collect();
    let why = if twice.is_empty() {
        format!("{round} review rounds in a row asked for changes")
    } else {
        format!("{} still open after two rounds that tried to fix {}", plural(twice.len(), "P0/P1 finding"), if twice.len() == 1 { "it" } else { "them" })
    };
    Some(format!("{why}. Open: {}", open.join("; ")))
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
    // Only acceptances that verify count: a row an agent wrote itself must not silence a finding.
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
    let findings = last.findings.into_iter().filter(|f| !told.iter().any(|(t, _)| t.text == f.text)).collect();
    Ok(Some(PreviousRound { commit: last.commit_sha, findings, in_history, accepted: told }))
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
    let mut lines = vec![format!("Nitely: **{named}** {}", issue.title)];
    if issue.url.starts_with(&format!("{}://", crate::issues::LINK_SCHEME)) {
        lines.push(format!("Open it in Nitely: `{}`", issue.url));
    }
    lines.push(String::new());
    lines.push(if s.approval_stands {
        "**Approval:** a human approved this exact patch in Nitely, and this review raised nothing new, so it stands.".to_string()
    } else if s.approver == "human" {
        "**Approval:** waits for a human. Approve it in Nitely's inbox.".to_string()
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
            lines.push(format!("**Review** by {}{} ({}s{batched}): {result}.", r.reviewer, noted(&r.note), r.seconds));
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
        None => lines.push("**Review:** none ran.".into()),
    }
    lines.push(String::new());
    let gate: Vec<String> = s.gate.results.iter().map(|r| format!("`{}`", r.command)).collect();
    lines.push(format!("**Gate** passed at `{}`: {}.", s.commit, gate.join(", ")));
    lines.push(format!("{} files changed against `{}`.", s.files, s.base));
    lines.push(String::new());
    lines.push(format!(
        "`nitely review patch={} verdict={} approver={} reviewer={}`",
        s.patch_id,
        s.review.as_ref().map_or("none", |r| r.verdict.as_str()),
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
/// `Submodule <path> <a>..<b>:` header lines go: patch-id would start a new patch at each.
pub fn patch_id_range(worktree: &Path, base: &str, head: &str) -> Result<String> {
    let diff = git(worktree, &["diff", "-U0", "--submodule=diff", &format!("{base}...{head}")])?;
    let diff: String = diff.split_inclusive('\n').filter(|l| !l.starts_with("Submodule ")).collect();
    if diff.is_empty() {
        return Ok("empty".into());
    }
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
fn push(worktree: &Path, branch: &str) -> Result<()> {
    git_remote(worktree, &["push", "--force-with-lease", "-u", "origin", &format!("HEAD:refs/heads/{branch}")]).map(|_| ())
}

impl Db {
    /// When the human last handed the issue back to agents (0 when never).
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
            None => ("", "none", "[]".to_string(), "[]".to_string(), ""),
        };
        let batch = s.review.as_ref().and_then(|r| r.batch.as_ref()).map(|b| b.id);
        let (seconds, tokens) = (s.review.as_ref().map(|r| r.seconds as i64), s.review.as_ref().and_then(|r| r.tokens).map(|t| t as i64));
        self.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, note, ts, batch_id, seconds, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![s.issue, s.patch_id, s.commit, reviewer, verdict, s.approver, to_json(&s.reasons), findings, flagged, note, now(), batch, seconds, tokens],
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
            "SELECT issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, ts, note, batch_id
             FROM review WHERE issue_key = ?1 COLLATE NOCASE AND (?2 IS NULL OR id < ?2) ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![issue_key, before], |r| {
            Ok(ReviewRecord {
                issue_key: r.get(0)?,
                patch_id: r.get(1)?,
                commit_sha: r.get(2)?,
                reviewer: r.get(3)?,
                verdict: r.get(4)?,
                approver: r.get(5)?,
                reasons: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                findings: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                ts: r.get(8)?,
                note: r.get(9)?,
                batch_id: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
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

    #[test]
    fn rounds_that_do_not_converge_go_to_a_person() {
        use crate::review::Finding;
        let f = |priority, text: &str, carried| Finding { priority, text: text.into(), carried, raised_as: None, now: None };
        let record = |findings: Vec<Finding>| ReviewRecord {
            issue_key: "NIT-1".into(),
            patch_id: "p".into(),
            commit_sha: "c".into(),
            reviewer: "codex".into(),
            verdict: "changes".into(),
            approver: "agent".into(),
            reasons: vec![],
            findings,
            ts: 0,
            note: String::new(),
            batch_id: None,
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
        };
        // Round 2 carrying a P1 once is still a round that may converge.
        let first = record(vec![f(1, "Drops the last row: a.rs", false)]);
        assert_eq!(not_converging(&[first.clone()], &review(vec![f(1, "Drops the last row: a.rs", true)])), None);
        // Round 3 carrying it a second time goes to a person, and says what is open.
        let second = record(vec![f(1, "Drops the last row: a.rs", true)]);
        let why = not_converging(&[second.clone(), first.clone()], &review(vec![f(1, "Drops the last row: a.rs", true), f(0, "Leaks the token: b.rs", false)])).unwrap();
        assert!(why.starts_with("1 P0/P1 finding still open after two rounds that tried to fix it. Open:"), "{why}");
        assert!(why.contains("[P1] Drops the last row: a.rs; [P0] Leaks the token: b.rs"), "{why}");
        // A finding carried at P2 (its code gone) or a new one does not count as carried twice.
        assert_eq!(not_converging(&[second.clone(), first.clone()], &review(vec![f(2, "Drops the last row: a.rs", true), f(1, "New: c.rs", false)])), None);
        // The fifth round in a row asking for changes goes to a person whatever was carried; a
        // review that did not run (no reviewer) is not a round, and a passing one ends the run.
        let none = ReviewRecord { reviewer: String::new(), ..first.clone() };
        let four = vec![record(vec![]), record(vec![]), none.clone(), record(vec![]), record(vec![])];
        let new = || review(vec![f(1, "Off by one: d.rs", false)]);
        let why = not_converging(&four, &new()).unwrap();
        assert!(why.starts_with("5 review rounds in a row asked for changes. Open: [P1] Off by one: d.rs"), "{why}");
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
       *stuck*) echo "PREVIOUS 1: open stuck.txt:1 \`let stuck = 1;\` is still never read"; echo "VERDICT: clean";;
       *) echo "PREVIOUS 1: open edge.txt:1 \`let edge = 2;\` still skips the empty line"; echo "VERDICT: clean";;
     esac;;
  *) case "$files" in
       *edge*) echo "- [P2] Edge case: edge.txt, the empty line"; echo "VERDICT: findings";;
       *stuck*) echo "- [P1] Stuck: stuck.txt, the x is never read"; echo "VERDICT: findings";;
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
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only, batch: false };
            submit(&mut self.db, &self.config, &opts, &self.actor)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    #[test]
    fn submits_with_a_review_and_routes_flags_to_a_human() {
        let mut f = fixture();
        for (title, file, approver, state) in
            [("Sidebar polish", "sidebar.txt", "agent", "In Review"), ("Billing fix", "billing.txt", "human", "Needs You")]
        {
            let (issue, wt) = f.started(title);
            std::fs::write(wt.join(file), "x").unwrap();
            let empty = f.submit(&wt, false).unwrap_err().to_string();
            assert!(empty.contains("no changes"), "{empty}");
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);

            let s = f.submit(&wt, false).unwrap();
            assert!(s.gate.passed && s.pushed, "{s:?}");
            let review = s.review.as_ref().unwrap();
            assert_eq!((review.verdict.as_str(), review.reviewer.as_str()), ("pass", "claude"));
            assert_eq!(review.note, "same vendor: codex not installed");
            assert_eq!((s.approver.as_str(), s.state.as_str()), (approver, state), "{s:?}");
            assert_eq!(f.db.reviews(&issue.key).unwrap()[0].note, "same vendor: codex not installed");
            assert!(describe(&issue, &s, Default::default()).contains("**Review** by claude (same vendor: codex not installed)"));
            // A local bare remote is not a code host, so there is no PR, and it says why.
            assert!(s.pr_url.is_none() && s.pr_error.as_deref().unwrap().contains("not on GitHub"));
            assert_eq!(f.db.reviews(&issue.key).unwrap()[0].patch_id, s.patch_id);
            assert!(describe(&issue, &s, Default::default()).contains(&format!("patch={}", s.patch_id)));
        }
    }

    #[test]
    fn a_second_round_checks_the_first_rounds_finding() {
        let mut f = fixture();
        // A second round checks the first round's finding and carries it while it is open.
        let (issue, wt) = f.committed("Edge handling", "edge.txt");
        let first = f.submit(&wt, true).unwrap();
        let review = first.review.as_ref().unwrap();
        assert!(review.previous.is_empty() && !review.findings[0].carried, "{review:?}");
        std::fs::write(wt.join("edge.txt"), "let edge = 2;").unwrap();
        sh(&wt, &["commit", "-q", "-am", "edge again"]);
        let second = f.submit(&wt, true).unwrap();
        let review = second.review.as_ref().unwrap();
        assert_eq!(review.previous.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(), ["open"], "{review:?}");
        assert_eq!((review.findings.len(), review.findings[0].carried, review.verdict.as_str()), (1, true, "pass"));
        assert!(describe(&issue, &second, Default::default()).contains("Previous round's findings: 1 still open."));
        assert!(f.db.reviews(&issue.key).unwrap()[0].findings[0].carried);
    }

    #[test]
    fn a_p1_that_two_rounds_leave_open_goes_to_a_person() {
        let mut f = fixture();
        // A P1 that two rounds of fixes leave open sends the issue to a person instead of a
        // fourth round.
        let (issue, wt) = f.started("Stuck fix");
        std::fs::write(wt.join("stuck.txt"), "let stuck = 1;").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "stuck"]);
        let states: Vec<String> = (0..3).map(|_| f.submit(&wt, true).unwrap().state).collect();
        assert_eq!(states, ["In Progress", "In Progress", "Needs You"]);
        let last: String = f
            .db
            .conn
            .query_row("SELECT summary FROM event WHERE issue_key = ?1 AND type = 'submit' ORDER BY id DESC LIMIT 1", [&issue.key], |r| r.get(0))
            .unwrap();
        assert!(last.contains("not converging") && last.contains("[P1] Stuck: stuck.txt"), "{last}");
        // A decision about the change, not the person's own work (NIT-220): the owner stays the agent,
        // and the open finding waits in the Inbox as a request to accept it, or send it back.
        let stuck = f.db.issue(&issue.key).unwrap().unwrap();
        assert_eq!((stuck.owner.as_str(), stuck.state.as_str()), ("agent", "Needs You"));
        let requests: Vec<crate::issues::Ask> = f.db.open_asks().unwrap().into_iter().filter(|a| a.issue_key.as_deref() == Some(issue.key.as_str())).collect();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert!(requests[0].finding.as_ref().is_some_and(|f| f.priority == 1 && f.text.starts_with("Stuck: stuck.txt")), "{requests:?}");
        assert!(requests[0].question.contains("not converging"), "{}", requests[0].question);
        // Done is refused while the change is not landed: it is not on the integration branch.
        let done = f.db.move_issue(&issue.key, "Done", Some("Done by the human, outside Nitely."), None, &Actor::Human).unwrap_err().to_string();
        assert!(done.contains("never landed"), "{done}");
        // Sent back, the count starts over: the next round that carries it is not escalated.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        f.db.decline_finding(requests[0].id, "fix it properly", "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(f.submit(&wt, true).unwrap().state, "In Progress");
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

        // A repo with no origin at all is local: submit hands the change in without a push, and
        // land lands it without one.
        sh(&f.repo, &["remote", "remove", "origin"]);
        let (issue, wt) = f.committed("Local tweak", "local.txt");
        let s = f.submit(&wt, false).unwrap();
        assert!(!s.pushed && s.pr_error.is_none(), "{s:?}");
        assert_eq!(s.state, "In Review", "{s:?}");
        let dir = f.repo.to_string_lossy().into_owned();
        let land = crate::land::LandOptions { dir: &dir, issue_key: &issue.key, push: true, keep: false, break_lock: false };
        let landed = crate::land::land(&mut f.db, &f.config, &land, &f.actor).unwrap();
        assert!(landed.landed && landed.pushed.is_none(), "{landed:?}");
    }

    /// Turns on batched review in the fixture's playbook.
    fn batching(f: &Fixture, window: &str) {
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        let on = format!("[review]\nbatch = \"small\"\nbatch_window = \"{window}\"\nbatch_max = 2\n");
        std::fs::write(&pb_path, pb.replacen("[review]\n", &on, 1)).unwrap();
    }

    /// A background submit, as a submit job runs it: it may wait for a review batch.
    fn submit_in_background(db_path: &Path, config: &crate::Config, actor: &Actor, wt: &Path) -> Result<Submitted> {
        let mut db = Db::open(db_path)?;
        let dir = wt.to_string_lossy().into_owned();
        let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true, batch: true };
        submit(&mut db, config, &opts, actor)
    }

    #[test]
    fn small_changes_handed_in_together_are_reviewed_in_one_batch() {
        let mut f = fixture();
        batching(&f, "60s");
        // Two small changes to one file, handed in at once: one reviewer call, a verdict each.
        let (a, wa) = f.committed("Totals", "shared.txt");
        let (b, wb) = f.committed("Totals again", "shared.txt");
        let db_path = f.base.join("t.db");
        let started = std::time::Instant::now();
        let threads: Vec<_> = [wa, wb]
            .into_iter()
            .map(|wt| {
                let (db_path, config, actor) = (db_path.clone(), f.config.clone(), f.actor.clone());
                std::thread::spawn(move || submit_in_background(&db_path, &config, &actor, &wt).map_err(|e| format!("{e:#}")))
            })
            .collect();
        let done: Vec<Submitted> = threads.into_iter().map(|t| t.join().unwrap().unwrap()).collect();
        assert!(started.elapsed() < std::time::Duration::from_secs(50), "the batch closed when full, not at its window");

        // The reviewer saw both; the P1 it named for change 2 stays on change 2.
        let order: String = f.db.conn.query_row("SELECT issues FROM review_batch", [], |r| r.get(0)).unwrap();
        let order: Vec<String> = serde_json::from_str(&order).unwrap();
        assert_eq!(order.iter().filter(|k| [&a.key, &b.key].contains(k)).count(), 2, "{order:?}");
        let by_key = |k: &str| done.iter().find(|s| s.issue == k).unwrap();
        let (first, second) = (by_key(&order[0]), by_key(&order[1]));
        let (r1, r2) = (first.review.as_ref().unwrap(), second.review.as_ref().unwrap());
        assert_eq!((r1.verdict.as_str(), first.state.as_str()), ("pass", "In Review"), "{first:?}");
        assert_eq!((r2.verdict.as_str(), second.state.as_str()), ("changes", "In Progress"), "{second:?}");
        assert_eq!(r2.findings[0].text, "Wrong total: shared.txt, the second line");
        assert_eq!(r1.batch.as_ref().unwrap().with, [order[1].clone()]);
        assert_eq!(r1.note, "same vendor: codex not installed");
        // One row per change, as a single review writes, with the batch's id on both.
        let batches: Vec<Option<i64>> = order.iter().map(|k| f.db.conn.query_row("SELECT batch_id FROM review WHERE issue_key = ?1", [k], |r| r.get(0)).unwrap()).collect();
        assert!(batches[0].is_some() && batches[0] == batches[1], "{batches:?}");
        assert_eq!(f.db.reviews(&order[0]).unwrap()[0].patch_id, first.patch_id);
        let said: Vec<String> = f.db.timeline(&order[1]).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s.contains(&format!("in a batch with {}, requested changes", order[0]))), "{said:?}");
        assert!(said.iter().any(|s| s.starts_with("Review by claude (same vendor: codex not installed) started in a batch with")), "{said:?}");
    }

    #[test]
    fn a_change_that_cannot_join_a_batch_is_reviewed_alone() {
        let mut f = fixture();
        batching(&f, "0");
        let db_path = f.base.join("t.db");
        // Nobody else in the queue when its window ends: reviewed alone, and the timeline says why.
        let (lone, wt) = f.committed("Lone tweak", "lone.txt");
        let s = submit_in_background(&db_path, &f.config, &f.actor, &wt).unwrap();
        assert!(s.review.as_ref().is_some_and(|r| r.batch.is_none() && r.verdict == "pass"), "{s:?}");
        let said: Vec<String> = f.db.timeline(&lone.key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s == "Reviewed alone after all: no other change joined it in time"), "{said:?}");

        // A change that is not small is reviewed alone at once, without waiting.
        let (big, wt) = f.started("Big change");
        std::fs::write(wt.join("big.txt"), "line\n".repeat(150)).unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "big"]);
        submit_in_background(&db_path, &f.config, &f.actor, &wt).unwrap();
        let said: Vec<String> = f.db.timeline(&big.key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(said.iter().any(|s| s.starts_with("Reviewed alone, not in a batch: it is not small (150 lines in 1 files")), "{said:?}");
        // A foreground submit never waits for a batch.
        let (fg, wt) = f.committed("Foreground", "fg.txt");
        f.submit(&wt, true).unwrap();
        let said: Vec<String> = f.db.timeline(&fg.key).unwrap().into_iter().filter_map(|t| t.summary).collect();
        assert!(!said.iter().any(|s| s.contains("batch")), "{said:?}");
    }

    #[test]
    fn a_shadow_batch_is_recorded_beside_the_single_reviews_and_routes_nothing() {
        let mut f = fixture();
        batching(&f, "60s");
        let pb_path = f.repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("batch = \"small\"", "batch = \"shadow\"")).unwrap();
        let db_path = f.base.join("t.db");
        // Each change is reviewed alone, which decides: both pass and are ready to land.
        let (a, wa) = f.committed("Totals", "shared.txt");
        let (b, wb) = f.committed("Totals again", "shared.txt");
        let decided: Vec<Submitted> = [&wa, &wb].iter().map(|wt| submit_in_background(&db_path, &f.config, &f.actor, wt).unwrap()).collect();
        for s in &decided {
            let review = s.review.as_ref().unwrap();
            assert!(review.batch.is_none() && review.verdict == "pass" && s.state == "In Review", "{s:?}");
            assert!(s.shadow.as_ref().is_some_and(|sh| sh.review_id > 0), "{s:?}");
        }
        // A foreground submit is never measured.
        let (_, wc) = f.committed("Foreground", "fg.txt");
        assert!(f.submit(&wc, true).unwrap().shadow.is_none());

        // Their shadow batch, once both jobs have handed their results back.
        let threads: Vec<_> = decided
            .into_iter()
            .map(|s| {
                let db_path = db_path.clone();
                std::thread::spawn(move || crate::review_batch::shadow(&Db::open(&db_path).unwrap(), s.shadow.as_ref().unwrap()).map_err(|e| format!("{e:#}")))
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        let pairs = crate::review_compare::shadow_pairs(&f.db).unwrap();
        assert_eq!(pairs.len(), 2, "{pairs:?}");
        // The batch found a P1 on its second change that its single review did not: recorded, never routed.
        let differ: Vec<&crate::review_compare::Pair> = pairs.iter().filter(|p| p.single.verdict != p.batched.verdict).collect();
        assert_eq!(differ.len(), 1, "{pairs:?}");
        assert_eq!((differ[0].batched.verdict.as_str(), differ[0].batch_size), ("changes", 2));
        for key in [&a.key, &b.key] {
            assert_eq!(f.db.issue(key).unwrap().unwrap().state, "In Review");
            let said: Vec<String> = f.db.timeline(key).unwrap().into_iter().filter_map(|t| t.summary).collect();
            assert!(!said.iter().any(|s| s.contains("batch")), "a shadow batch says nothing on the issue: {said:?}");
        }
        let report = crate::review_compare::Report::of(&pairs);
        assert_eq!((report.changes, report.batches, report.same_verdict, report.blocking_batched), (2, 1, 1, 1));
    }

    #[test]
    fn a_shadow_batch_starts_from_the_round_before_the_review_that_decided() {
        let mut f = fixture();
        let (issue, wt) = f.committed("Edge again", "edge.txt");
        let first = f.submit(&wt, true).unwrap();
        std::fs::write(wt.join("edge.txt"), "let edge = 2;").unwrap();
        sh(&wt, &["commit", "-q", "-am", "edge again"]);
        f.submit(&wt, true).unwrap();
        let deciding: i64 = f.db.conn.query_row("SELECT MAX(id) FROM review WHERE issue_key = ?1", [&issue.key], |r| r.get(0)).unwrap();
        // The next round would start from the deciding review; its shadow batch starts where that
        // review started, from the first round, so it does not see the answers it is measured against.
        let next = previous_round(&f.db, &issue.key, &wt).unwrap().unwrap();
        let shadow = previous_round_at(&f.db, &issue.key, &wt, "HEAD", Some(deciding)).unwrap().unwrap();
        assert_ne!(next.commit, shadow.commit);
        assert_eq!(shadow.commit, first.commit);
        assert!(shadow.in_history && shadow.findings.len() == 1 && !shadow.findings[0].carried, "{shadow:?}");
    }
}
