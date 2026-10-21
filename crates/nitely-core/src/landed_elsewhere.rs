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

/// Whether `git cherry <base> <head>` says every commit of `head` is on the integration branch:
/// `head` is an ancestor (no lines), or each commit is there as an equivalent patch (`-`), as a
/// rebase or a fold into another change keeps them. A `+` line is a commit that is not.
pub fn all_on_base(cherry: &str) -> bool {
    cherry.lines().filter(|l| !l.trim().is_empty()).all(|l| l.starts_with('-'))
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
    let Ok(cherry) = playbook::git(root, &["cherry", &base, &head]) else { return Ok(None) };
    if !all_on_base(&cherry) {
        return Ok(None);
    }
    let at = playbook::git(root, &["rev-parse", &base])?.trim().to_string();
    let said = format!("Landed {branch} on {base} without a land of its own: its submitted commits are there at {at}");
    db.log_event(actor, Some(&issue.key), None, "landed", &said)?;
    Ok(Some(said))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_counts_as_landed_only_when_every_submitted_commit_is_on_the_branch() {
        assert!(all_on_base(""), "the submitted head is on the branch: merged by hand");
        assert!(all_on_base("- 1f3a9c\n- 7be210\n"), "rebased or folded into another change: equivalent patches");
        assert!(!all_on_base("- 1f3a9c\n+ 7be210\n"), "one commit is not there: Done stays refused");
        assert!(!all_on_base("+ 7be210"));
    }
}
