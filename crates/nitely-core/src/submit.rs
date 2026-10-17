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
}

pub fn submit(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor) -> Result<Submitted> {
    let (worktree, branch_key) = playbook::current_worktree(opts.dir)?;
    let root = playbook::repo_root(config, opts.dir).with_context(|| format!("{} is not inside a git repo", opts.dir))?;
    let pb = Playbook::load(&root)?;
    let base = pb.integration_branch().to_string();
    let branch = git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
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
        // A reviewer that cannot run (not signed in, say) is passed over for the next candidate;
        // one that ran and did not finish its review stops the submit.
        let mut failed: Vec<crate::review::CouldNotRun> = vec![];
        let (choice, mut review) = loop {
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
        };
        let reviewer = choice.reviewer.clone();
        review.note = choice.note.clone();
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
    // Read before this round is recorded: the rounds before it, since the human last handed the
    // issue back (their decision to go on starts a new count).
    let stalled = match &out.review {
        Some(r) if verdict == "changes" && issue.origin == "nitely" => {
            let since = db.handed_back_at(&issue.key)?;
            let earlier: Vec<ReviewRecord> = db.reviews(&issue.key)?.into_iter().filter(|r| r.ts > since).collect();
            not_converging(&earlier, r)
        }
        _ => None,
    };
    db.record_review(&out)?;
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
    let outcome = match (&out.review, out.approver.as_str()) {
        (Some(r), _) if accepted_only => format!("Review by {}{} asked for changes only in findings you accepted, so it passes", r.reviewer, noted(&r.note)),
        (Some(r), _) if r.verdict == "changes" => format!("Review by {}{} requested changes", r.reviewer, noted(&r.note)),
        (Some(r), "human") if out.approval_stands => format!("Review by {}{} passed; the human's approval of this patch stands", r.reviewer, noted(&r.note)),
        (Some(r), "human") => format!("Review by {}{} passed; waits for a human", r.reviewer, noted(&r.note)),
        (Some(r), _) => format!("Review by {}{} passed; ready to land", r.reviewer, noted(&r.note)),
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
        // Another round would cost as much again and likely end the same: a person decides. It is
        // handed to them as their own work, so nothing offers to approve a change that failed review.
        ("Needs You", "The review rounds are not converging, so the issue is handed to the human: stop working on it and pick up other work. They decide whether to hand it back, narrow it, or close it. If what keeps coming back is a trade-off for them to decide, call request_acceptance with that finding and your reasoning first.")
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
    if stalled.is_some() {
        out.state = db.set_owner(&issue.key, "human", actor)?.state;
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
fn previous_round(db: &Db, key: &str, worktree: &Path) -> Result<Option<PreviousRound>> {
    let Some(last) = db.reviews(key)?.into_iter().find(|r| !r.reviewer.is_empty()) else {
        return Ok(None);
    };
    let in_history = git(worktree, &["merge-base", "--is-ancestor", &last.commit_sha, "HEAD"]).is_ok();
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
            lines.push(format!("**Review** by {}{} ({}s): {result}.", r.reviewer, noted(&r.note), r.seconds));
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
    let diff = git(worktree, &["diff", "-U0", &format!("{base}...HEAD")])?;
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
    ["rebase-merge", "rebase-apply"].iter().any(|d| {
        git(worktree, &["rev-parse", "--git-path", d]).is_ok_and(|p| worktree.join(p.trim()).exists())
    })
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
        self.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, note, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![s.issue, s.patch_id, s.commit, reviewer, verdict, s.approver, to_json(&s.reasons), findings, flagged, note, now()],
        )?;
        Ok(())
    }

    /// Every review of an issue's branch, newest first.
    pub fn reviews(&self, issue_key: &str) -> Result<Vec<ReviewRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, ts, note
             FROM review WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([issue_key], |r| {
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
    use std::process::Command;

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

    #[test]
    fn submits_with_a_review_and_routes_flags_to_a_human() {
        let base = crate::paths::unique_temp("nitely-submit", "");
        let repo = base.join("repo");
        let remote = base.join("remote.git");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let o = playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
            assert!(o.status.success(), "git {args:?}: {} {}", o.status, String::from_utf8_lossy(&o.stderr));
        };
        playbook::run_git(Command::new("git").args(["init", "-q", "--bare"]).arg(&remote)).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        // Only the fake `claude` reviewer is installed, so Claude's branches get a same-vendor
        // review. It flags money whenever the diff touches a billing file, finds an edge case
        // in a first look at an edge file, and on a re-review says that finding is still open.
        std::fs::write(
            repo.join("review.sh"),
            r#"prompt=$(cat)
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
"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(playbook::PLAYBOOK_PATH),
            r#"
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
"#
            .replace("{sh}", &playbook::posix_sh()),
        )
        .unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        sh(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        sh(&repo, &["push", "-q", "origin", "main"]);

        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let actor = Actor::Agent("claude:test".into());
        let new = |title: &str| NewIssue {
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
        };
        let config = crate::Config::default();

        for (title, file, approver, state) in
            [("Sidebar polish", "sidebar.txt", "agent", "In Review"), ("Billing fix", "billing.txt", "human", "Needs You")]
        {
            let issue = db.create_issue(&new(title), &actor).unwrap();
            db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
            let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
            let wt = Path::new(&started.worktree);
            std::fs::write(wt.join(file), "x").unwrap();
            let dir = started.worktree.as_str();
            let opts = SubmitOptions { dir, issue_key: None, skip_review: false, local_only: false };
            let empty = submit(&mut db, &config, &opts, &actor).unwrap_err().to_string();
            assert!(empty.contains("no changes"), "{empty}");
            sh(wt, &["add", "."]);
            sh(wt, &["commit", "-q", "-m", title]);

            let s = submit(&mut db, &config, &opts, &actor).unwrap();
            assert!(s.gate.passed && s.pushed, "{s:?}");
            let review = s.review.as_ref().unwrap();
            assert_eq!((review.verdict.as_str(), review.reviewer.as_str()), ("pass", "claude"));
            assert_eq!(review.note, "same vendor: codex not installed");
            assert_eq!((s.approver.as_str(), s.state.as_str()), (approver, state), "{s:?}");
            assert_eq!(db.reviews(&issue.key).unwrap()[0].note, "same vendor: codex not installed");
            assert!(describe(&issue, &s, Default::default()).contains("**Review** by claude (same vendor: codex not installed)"));
            // A local bare remote is not a code host, so there is no PR, and it says why.
            assert!(s.pr_url.is_none() && s.pr_error.as_deref().unwrap().contains("not on GitHub"));
            assert_eq!(db.reviews(&issue.key).unwrap()[0].patch_id, s.patch_id);
            assert!(describe(&issue, &s, Default::default()).contains(&format!("patch={}", s.patch_id)));
        }

        // A second round checks the first round's finding and carries it while it is open.
        let issue = db.create_issue(&new("Edge handling"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        std::fs::write(wt.join("edge.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "edge"]);
        let first = submit(&mut db, &config, &opts, &actor).unwrap();
        let review = first.review.as_ref().unwrap();
        assert!(review.previous.is_empty() && !review.findings[0].carried, "{review:?}");
        std::fs::write(wt.join("edge.txt"), "let edge = 2;").unwrap();
        sh(wt, &["commit", "-q", "-am", "edge again"]);
        let second = submit(&mut db, &config, &opts, &actor).unwrap();
        let review = second.review.as_ref().unwrap();
        assert_eq!(review.previous.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(), ["open"], "{review:?}");
        assert_eq!((review.findings.len(), review.findings[0].carried, review.verdict.as_str()), (1, true, "pass"));
        assert!(describe(&issue, &second, Default::default()).contains("Previous round's findings: 1 still open."));
        assert!(db.reviews(&issue.key).unwrap()[0].findings[0].carried);

        // A P1 that two rounds of fixes leave open sends the issue to a person instead of a
        // fourth round.
        let issue = db.create_issue(&new("Stuck fix"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        std::fs::write(wt.join("stuck.txt"), "let stuck = 1;").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "stuck"]);
        let states: Vec<String> = (0..3).map(|_| submit(&mut db, &config, &opts, &actor).unwrap().state).collect();
        assert_eq!(states, ["In Progress", "In Progress", "Needs You"]);
        let last: String = db
            .conn
            .query_row("SELECT summary FROM event WHERE issue_key = ?1 AND type = 'submit' ORDER BY id DESC LIMIT 1", [&issue.key], |r| r.get(0))
            .unwrap();
        assert!(last.contains("not converging") && last.contains("[P1] Stuck: stuck.txt"), "{last}");
        // Handed to the human as their own work: nothing offers to approve a change that failed.
        assert_eq!(db.issue(&issue.key).unwrap().unwrap().owner, "human");
        // Handed back, the count starts over: the next round that carries it is not escalated.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        db.set_owner(&issue.key, "agent", &Actor::Human).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(submit(&mut db, &config, &opts, &actor).unwrap().state, "In Progress");

        // A preferred reviewer that is installed but signed out is passed over: the next one
        // reviews, and the note says why.
        let pb_path = repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("codex = [\"nitely-test-no-such-codex\"]", &format!("codex = [\"{}\", \"-c\", \"echo 'Error: not logged in' >&2; exit 1\"]", playbook::posix_sh()))).unwrap();
        let issue = db.create_issue(&new("Signed out"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("signed.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "signed"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        let review = s.review.as_ref().unwrap();
        assert_eq!((review.reviewer.as_str(), review.note.as_str()), ("claude", "same vendor: codex could not run: not signed in"), "{s:?}");
        let failed: String = db
            .conn
            .query_row("SELECT summary FROM event WHERE issue_key = ?1 AND type = 'review_failed'", [&issue.key], |r| r.get(0))
            .unwrap();
        assert!(failed.contains("codex could not run (not signed in)"), "{failed}");
        std::fs::write(&pb_path, &pb).unwrap();

        // A playbook that requires another vendor's review sends a same-vendor one to a human, and
        // with no reviewer installed at all, submit says what to install.
        let pb_path = repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("[review]
", "[review]
require_cross_vendor = true
")).unwrap();
        let issue = db.create_issue(&new("Footer tweak"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("footer.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "footer"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        assert_eq!((s.approver.as_str(), s.state.as_str()), ("human", "Needs You"), "{s:?}");
        assert!(s.reasons[0].contains("requires a review by another vendor"), "{:?}", s.reasons);
        std::fs::write(&pb_path, pb.replace("\"{sh}\", \"review.sh\"]".replace("{sh}", &playbook::posix_sh()).as_str(), "\"nitely-test-no-such-claude\"]")).unwrap();
        let none = submit(&mut db, &config, &opts, &actor).unwrap_err().to_string();
        assert!(none.contains("no reviewer can run") && none.contains("nitely-test-no-such"), "{none}");
        std::fs::write(&pb_path, pb).unwrap();

        // A push that fails hands nothing in, so the issue stays In Progress.
        sh(&repo, &["remote", "set-url", "origin", &base.join("missing.git").to_string_lossy()]);
        let issue = db.create_issue(&new("Header tweak"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("header.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "header"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: false };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        assert!(!s.pushed && s.pr_error.as_deref().unwrap().contains("push failed"), "{s:?}");
        assert_eq!(s.state, "In Progress");
        assert!(s.next.contains("push failed"), "{}", s.next);

        // A repo with no origin at all is local: submit hands the change in without a push, and
        // land lands it without one.
        sh(&repo, &["remote", "remove", "origin"]);
        let issue = db.create_issue(&new("Local tweak"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("local.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "local"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: false };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        assert!(!s.pushed && s.pr_error.is_none(), "{s:?}");
        assert_eq!(s.state, "In Review", "{s:?}");
        let dir = repo.to_string_lossy().into_owned();
        let land = crate::land::LandOptions { dir: &dir, issue_key: &issue.key, push: true, keep: false, break_lock: false };
        let landed = crate::land::land(&mut db, &config, &land, &actor).unwrap();
        assert!(landed.landed && landed.pushed.is_none(), "{landed:?}");
        std::fs::remove_dir_all(base).ok();
    }
}
