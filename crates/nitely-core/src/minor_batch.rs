//! The day's minor review findings, batched (NIT-718). A review's P2 findings hold no change
//! (NIT-695), and filing one follow-up per change made the minor work arrive faster than it was
//! worked. So every review of a project on one local day appends its P2s to one rolling issue, and
//! one agent clears the batch in one change: one gate and one review for the day's minor fixes.
//! Once that agent starts it, the day's later findings go to a second one. When the batch is
//! started, a finding whose cited code has changed since it was found is dropped.

use anyhow::Result;

use crate::db::Db;
use crate::issues::{Actor, IssueRecord};
use crate::review::Review;

/// How the evidence of a day's batch starts; then the project and the day.
pub const EVIDENCE_START: &str = "The P2 review findings of ";

/// The evidence of `project`'s batch for `day` (YYYY-MM-DD, local): how every review that day
/// finds it again instead of filing another.
pub fn evidence(project: &str, day: &str) -> String {
    format!("{EVIDENCE_START}{project} on {day}, which held no change.")
}

/// Whether `issue` is a day's batch of minor findings.
pub fn is_batch(issue: &IssueRecord) -> bool {
    issue.evidence.starts_with(EVIDENCE_START) && issue.evidence.ends_with(", which held no change.")
}

/// The local day now, as the batch names it.
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// One finding as the batch lists it: its words, then the change whose review found it.
pub fn line(key: &str, finding: &str) -> String {
    format!("- {finding} (from [{key}]({}))", crate::links::issue_url(key))
}

/// The words of a finding the batch lists, without the change it came from; None for any other line.
fn finding_of(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("- ")?;
    if !rest.starts_with("[P") {
        return None;
    }
    Some(rest.rfind(" (from [").map_or(rest, |i| &rest[..i]))
}

fn body(day: &str, lines: &[String]) -> String {
    format!(
        "The reviews of {day} found these. They are real but minor (P2), so they held no change. Fix the ones that still apply, all in one change. A finding whose cited code changed after it was found was dropped when this issue was started.\n\n{}",
        lines.join("\n")
    )
}

/// Files `review`'s new P2 findings of change `key` in its project's batch for today: the open one
/// no agent has started yet, with the new lines added, else a new one at priority 3, outside any
/// goal, so minor work holds no checkpoint. Returns the batch's key, or None with nothing to file.
pub(crate) fn file(db: &mut Db, key: &str, review: &Review, actor: &Actor) -> Result<Option<String>> {
    file_on(db, key, review, &today(), actor)
}

pub(crate) fn file_on(db: &mut Db, key: &str, review: &Review, day: &str, actor: &Actor) -> Result<Option<String>> {
    let minor = crate::submit_rules::minor(&review.findings);
    let Some(issue) = db.issue(key)?.filter(|_| !minor.is_empty()) else { return Ok(None) };
    let project = issue.project.clone().unwrap_or_default();
    let lines: Vec<String> = minor.iter().map(|f| line(&issue.key, f)).collect();
    let evidence = evidence(&project, day);
    let listed = db.list_issues(Some(&project), false)?;
    let open = listed.iter().find(|i| i.evidence == evidence && matches!(i.state.as_str(), "Todo" | "Backlog")).cloned();
    let batch = match open {
        Some(existing) => {
            let new: Vec<&String> = lines.iter().filter(|l| !existing.body.lines().any(|b| b.trim() == l.as_str())).collect();
            if !new.is_empty() {
                let more: Vec<String> = new.into_iter().cloned().collect();
                db.edit_issue(&existing.key, None, Some(&format!("{}\n{}", existing.body.trim_end(), more.join("\n"))), actor)?;
            }
            existing.key
        }
        None => {
            let new = crate::issues::NewIssue {
                project: project.clone(),
                title: format!("Minor review findings of {day} in {project}"),
                body: body(day, &lines),
                kind: "bug".into(),
                // Who filed it: the vendor of the change whose review started the batch.
                source: issue.source.clone().unwrap_or_else(|| "human".into()),
                evidence,
                priority: 3,
                allow_similar: true,
                ..Default::default()
            };
            let batch = db.create_issue(&new, actor)?.key;
            fold_per_change(db, &batch, &listed, actor)?;
            batch
        }
    };
    Ok(Some(batch))
}

/// How the follow-up an earlier build filed for one change's P2s (NIT-695) said where it came from:
/// this, the change's key, then `PER_CHANGE_END`.
const PER_CHANGE_END: &str = ", which do not hold the change.";

/// The change an earlier build's per-change follow-up was filed for, from its evidence.
fn per_change_of(issue: &IssueRecord) -> Option<&str> {
    issue.evidence.strip_prefix(EVIDENCE_START)?.strip_suffix(PER_CHANGE_END)
}

/// A new batch takes in the project's open per-change follow-ups an earlier build filed, one each
/// per change (NIT-718): their findings join it, each under the change it came from, and each
/// closes as a duplicate that names it. One no agent has started, that is: one being worked stays.
/// So the backlog folds into the project's first batch, and the stale findings among it are dropped
/// with the rest when the batch is started.
fn fold_per_change(db: &mut Db, batch: &str, listed: &[IssueRecord], actor: &Actor) -> Result<()> {
    let old: Vec<&IssueRecord> = listed.iter().filter(|i| per_change_of(i).is_some() && matches!(i.state.as_str(), "Todo" | "Backlog")).collect();
    if old.is_empty() {
        return Ok(());
    }
    let mut body = db.issue(batch)?.map(|i| i.body).unwrap_or_default();
    for issue in &old {
        let from = per_change_of(issue).unwrap_or(&issue.key);
        for l in issue.body.lines().filter_map(|l| l.trim().strip_prefix("- ").filter(|f| f.starts_with("[P"))) {
            let folded = line(from, l);
            if !body.lines().any(|b| b.trim() == folded) {
                body = format!("{}\n{folded}", body.trim_end());
            }
        }
    }
    db.edit_issue(batch, None, Some(&body), actor)?;
    let url = crate::links::issue_url(batch);
    for issue in old {
        db.move_issue(&issue.key, "Duplicate", None, Some(&format!("Folded into the day's batch of minor findings, [{batch}]({url}) (NIT-718)")), actor)?;
    }
    Ok(())
}

/// When an agent starts a day's batch in `worktree`: drops each finding whose cited code is no
/// longer there at the worktree's start (fixed since, or in a change that never landed), says
/// which on the issue, and returns them.
pub(crate) fn drop_stale(db: &Db, issue: &IssueRecord, worktree: &std::path::Path, actor: &Actor) -> Result<Vec<String>> {
    if !is_batch(issue) {
        return Ok(vec![]);
    }
    let stale: Vec<String> = issue
        .body
        .lines()
        .filter(|l| finding_of(l).is_some_and(|f| !crate::review::cited_code_holds_at(worktree, "HEAD", f)))
        .map(|l| l.trim().to_string())
        .collect();
    if stale.is_empty() {
        return Ok(vec![]);
    }
    let kept: Vec<&str> = issue.body.lines().filter(|l| !stale.iter().any(|s| s == l.trim())).collect();
    let left = kept.iter().filter(|l| finding_of(l).is_some()).count();
    db.edit_issue(&issue.key, None, Some(&kept.join("\n")), actor)?;
    db.add_note(
        &issue.key,
        &format!(
            "Dropped {} finding(s) whose cited code has changed since they were found; {left} left to fix:\n{}",
            stale.len(),
            stale.join("\n")
        ),
        "note",
        actor,
    )?;
    Ok(stale)
}
