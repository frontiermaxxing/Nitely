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

/// The commits that must be on the integration branch for an issue to count as landed: each named
/// branch's tip, and the head it was last submitted at (a branch reset after its submit must not
/// hide unshipped commits). From the last submit job when there is one, else (a `submit --wait`
/// records no job) from the local branches that name the issue.
pub fn must_be_on_base(job: Option<(&str, &str)>, tips: &[(String, String)]) -> Vec<String> {
    let mut revs: Vec<String> = Vec::new();
    match job {
        Some((branch, head)) => {
            revs.push(head.to_string());
            revs.extend(tips.iter().filter(|(b, _)| b == branch).map(|(_, t)| t.clone()));
        }
        None => revs.extend(tips.iter().map(|(_, t)| t.clone())),
    }
    revs.dedup();
    revs
}

/// Before Done: when `key`'s last submitted change has not landed by itself, but the integration
/// branch of the repo at `root` holds every commit of it (`must_be_on_base`, `all_on_base`),
/// records the landing as a `landed` event, which like a land's ends with the commit it landed at;
/// retires the issue's queued submit and its pull request (GitHub or Bitbucket), saying on the
/// issue when the pull request could not be closed; and says what it recorded. None: nothing to
/// record, and Done stays refused. Refused while a submit of the issue runs: it could reopen it.
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
    let running: bool = db.conn.query_row("SELECT EXISTS(SELECT 1 FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE AND state = 'running')", [&issue.key], |r| r.get(0))?;
    if running {
        anyhow::bail!("a submit of {} is running; wait for it to finish (submit_status) before closing the issue", issue.key);
    }
    let job: Option<(String, String)> = db
        .conn
        .query_row("SELECT branch, head FROM submit_job WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 1", [&issue.key], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    // The local branches that name the issue, as they are now.
    let refs = playbook::git(root, &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads/"]).unwrap_or_default();
    let tips: Vec<(String, String)> = refs
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(b, _)| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(&issue.key)) || job.as_ref().is_some_and(|(jb, _)| jb == b))
        .map(|(b, t)| (b.to_string(), t.trim().to_string()))
        .collect();
    let revs = must_be_on_base(job.as_ref().map(|(b, h)| (b.as_str(), h.as_str())), &tips);
    let Some(branch) = job.as_ref().map(|(b, _)| b.clone()).or_else(|| tips.first().map(|(b, _)| b.clone())) else { return Ok(None) };
    if revs.is_empty() {
        return Ok(None);
    }
    let base = Playbook::load(root)?.integration_branch().to_string();
    for rev in &revs {
        let (Ok(cherry), Ok(merges)) = (playbook::git(root, &["cherry", &base, rev]), playbook::git(root, &["rev-list", "--merges", &format!("{base}..{rev}")])) else { return Ok(None) };
        if !all_on_base(&cherry, &merges) {
            return Ok(None);
        }
    }
    let at = playbook::git(root, &["rev-parse", &base])?.trim().to_string();
    let said = format!("Landed {branch} on {base} without a land of its own: its commits are there at {at}");
    db.log_event(actor, Some(&issue.key), None, "landed", &said)?;
    // Its own submit is superseded, and its pull request closed with why.
    db.conn.execute("UPDATE submit_job SET state = 'superseded', ended_at = ?2 WHERE issue_key = ?1 COLLATE NOCASE AND state = 'queued'", rusqlite::params![issue.key, crate::db::now()])?;
    let why = format!("{} reached {base} without a land of its own: its commits are there at {at}. Closed by Nitely.", issue.key);
    let mut told = said.clone();
    match crate::remote::close_pr(&root.to_string_lossy(), &branch, &why) {
        Ok(Some(url)) => told.push_str(&format!("; closed {url}")),
        Ok(None) => {}
        Err(e) => {
            let left = format!("Its pull request for {branch} could not be closed, so it is still open: {e:#}. Close it by hand.");
            db.log_event(actor, Some(&issue.key), None, "note", &left)?;
            told.push_str(&format!("; {left}"));
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

        let tips = [("claude/app-3-x".to_string(), "aaa".to_string()), ("codex/app-3-again".to_string(), "bbb".to_string())];
        // The last submit job's head and its branch's tip, whatever other branches there are.
        assert_eq!(must_be_on_base(Some(("claude/app-3-x", "000")), &tips), ["000", "aaa"]);
        assert_eq!(must_be_on_base(Some(("claude/app-3-gone", "000")), &tips), ["000"], "the branch is gone: its head alone");
        // No job (submit --wait): every branch that names the issue.
        assert_eq!(must_be_on_base(None, &tips), ["aaa", "bbb"]);
        assert!(must_be_on_base(None, &[]).is_empty(), "nothing to check: Done stays refused");
    }
}
