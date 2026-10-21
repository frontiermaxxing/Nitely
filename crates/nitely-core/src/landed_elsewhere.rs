//! Done for an issue whose change reached the integration branch without landing by itself
//! (NIT-361): merged by hand, or rebased into another issue's change that landed. Done is refused
//! while a submitted change has not landed since (`Db::check_move`); this records the landing the
//! integration branch shows, so Done goes through and the goal counts the work as shipped.

use anyhow::Result;
use rusqlite::OptionalExtension;
use std::path::Path;

use crate::db::Db;
use crate::issues::Actor;
use crate::playbook::{self, Playbook};

/// Whether the work is on the integration branch: `git cherry <base> <tip>` says every commit is
/// there, the tip itself (no lines) or each one as an equivalent patch (`-`, as a rebase or a fold
/// into another change keeps them), and `merges`, `git rev-list --merges <base>..<tip>`, is empty:
/// cherry passes over merge commits, whose own resolution changes it cannot see.
pub fn all_on_base(cherry: &str, merges: &str) -> bool {
    merges.trim().is_empty() && cherry.lines().filter(|l| !l.trim().is_empty()).all(|l| l.starts_with('-'))
}

/// Before Done: when `key`'s last submitted change has not landed by itself, but every commit of
/// the head it was last submitted at is on the integration branch of the repo at `root`, records
/// the landing as a `landed` event, which like a land's ends with the commit it landed at, and says
/// what it recorded. None: nothing to record, and Done stays refused.
pub fn record(db: &Db, root: &Path, key: &str, actor: &Actor) -> Result<Option<String>> {
    let Some(issue) = db.issue(key)? else { return Ok(None) };
    let (submitted, landed): (Option<i64>, Option<i64>) = db.conn.query_row(
        "SELECT (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'submit'),
                (SELECT MAX(id) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'landed')",
        [&issue.key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    // Nothing submitted, or landed since it was: not the case Done refuses.
    if submitted.is_none_or(|s| landed.is_some_and(|l| l >= s)) {
        return Ok(None);
    }
    // Only the last submission counts: an earlier one on main says nothing of what came after.
    let last: Option<(String, String)> = db
        .conn
        .query_row("SELECT branch, head FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1", [&issue.key], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((branch, head)) = last else { return Ok(None) };
    let base = Playbook::load(root)?.integration_branch().to_string();
    // The branch as it is now, which holds anything committed or submitted after that job (a
    // `submit --wait` leaves no job of its own); the job's head when the branch is gone.
    let tip = playbook::git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).map(|t| t.trim().to_string()).unwrap_or(head);
    let (Ok(cherry), Ok(merges)) = (playbook::git(root, &["cherry", &base, &tip]), playbook::git(root, &["rev-list", "--merges", &format!("{base}..{tip}")])) else { return Ok(None) };
    if !all_on_base(&cherry, &merges) {
        return Ok(None);
    }
    let at = playbook::git(root, &["rev-parse", &base])?.trim().to_string();
    let said = format!("Landed {branch} on {base} without a land of its own: its commits are there at {at}");
    db.log_event(actor, Some(&issue.key), None, "landed", &said)?;
    // Its own submit is superseded, and its pull request closed with why (GitHub).
    db.conn.execute("UPDATE submit_job SET state = 'superseded', ended_at = ?2 WHERE issue_key = ?1 COLLATE NOCASE AND state = 'queued'", rusqlite::params![issue.key, crate::db::now()])?;
    let mut told = said.clone();
    if let Some(slug) = crate::git::github_slug(&root.to_string_lossy()) {
        let why = format!("{} reached {base} without a land of its own: its commits are there at {at}. Closed by Nitely.", issue.key);
        match crate::github::close_pr(&slug, &branch, &why) {
            Ok(Some(url)) => told.push_str(&format!("; closed {url}")),
            Ok(None) => {}
            Err(e) => told.push_str(&format!("; its pull request is still open: {e:#}")),
        }
    }
    Ok(Some(told))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_counts_as_landed_only_when_every_submitted_commit_is_on_the_branch() {
        assert!(all_on_base("", ""), "the branch is on the integration branch: merged by hand");
        assert!(all_on_base("- 1f3a9c\n- 7be210\n", ""), "rebased or folded into another change: equivalent patches");
        assert!(!all_on_base("- 1f3a9c\n+ 7be210\n", ""), "one commit is not there: Done stays refused");
        assert!(!all_on_base("+ 7be210", ""));
        assert!(!all_on_base("- 1f3a9c\n", "9c01de\n"), "a merge on the branch may hold changes cherry cannot see");
    }
}
