//! Worktrees of finished issues still on disk (NIT-340): most often ones a land meant to remove but
//! had to keep, because a dev server, a build or the landing agent's own shell still used them.
//! Each such keep is said on its issue (`worktree_held`), and the next land, or `nitely lanes
//! leftovers --remove`, removes the ones nothing keeps any more.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::issues::{Actor, TimelineEntry};
use crate::playbook::{self, Playbook};

/// A closed issue's worktree still on disk, and whether it goes.
#[derive(Debug, Clone, Serialize)]
pub struct Leftover {
    pub issue: String,
    pub worktree: PathBuf,
    /// Empty for a folder whose branch is gone.
    pub branch: String,
    /// Why it stays; None when nothing keeps it.
    pub stays: Option<String>,
    /// What removing it did, when it was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<String>,
    /// False for a folder git no longer lists: a land unregistered it but could not delete all of
    /// it (`land::finish_removal`).
    #[serde(skip)]
    registered: bool,
}

/// Said on the issue when land kept a worktree it meant to remove, with why (`cleanup`, the first
/// clause of what land reported), so a later land knows to try again.
pub(crate) fn note_held(db: &Db, actor: &Actor, key: &str, worktree: &Path, cleanup: &str) -> Result<()> {
    let why = cleanup.split("; stop ").next().unwrap_or(cleanup);
    db.log_event(actor, Some(key), Some(&worktree.to_string_lossy()), "worktree_held", &format!("Worktree kept after landing: {why}. The next land removes it once nothing uses it"))
}

/// The worktrees, under the playbook's worktree folder, of closed issues (Done, Duplicate,
/// Canceled), and the folders lands could not finish deleting, each with why it stays. One goes
/// when no land kept it on purpose (`--keep`, or commits made after its gate), its branch is in the
/// integration branch, it has nothing uncommitted or untracked, and no crew worker and no process
/// uses it. With `remove`, the ones that go are removed with their merged branch, and each removal
/// is said on its issue.
pub fn leftovers(db: &Db, root: &Path, pb: &Playbook, remove: bool, actor: &Actor) -> Result<Vec<Leftover>> {
    let issue = crate::IssueRecord { key: "x".into(), title: "x".into(), ..Default::default() };
    let Some(folder) = pb.worktree_for(root, "x", &issue).parent().map(|d| d.to_string_lossy().into_owned()) else { return Ok(vec![]) };
    let base = pb.integration_branch();
    let closed = |key: &str| -> Result<bool> { Ok(db.issue(key)?.is_some_and(|i| matches!(i.state_type.as_str(), "completed" | "canceled"))) };
    let registered = crate::git::list_worktrees(&root.to_string_lossy())?;
    let mut found = Vec::new();
    for wt in &registered {
        if wt.is_main || wt.prunable || !crate::paths::is_within(&wt.path, &folder) {
            continue;
        }
        let Some(branch) = wt.branch.clone() else { continue };
        let Some(key) = crate::git::parse_branch(&branch).1.map(|k| k.to_uppercase()) else { continue };
        if !closed(&key)? {
            continue;
        }
        let path = PathBuf::from(&wt.path);
        let stays = match kept_on_purpose(&db.timeline(&key)?) {
            Some(why) => Some(why),
            None if db.crew_worker_in(&path)? => Some("a crew worker still runs in it".to_string()),
            None => why_it_stays(&path, &branch, base),
        };
        found.push(Leftover { issue: key, worktree: path, branch, stays, removed: None, registered: true });
    }
    // Folders git no longer lists, but a land that kept them said where they are.
    let listed = |p: &str| registered.iter().any(|w| crate::paths::normalize(&w.path) == crate::paths::normalize(p));
    for (key, place) in held_places(db)? {
        let path = PathBuf::from(&place);
        if !crate::paths::is_within(&place, &folder) || listed(&place) || !path.exists() || !closed(&key)? {
            continue;
        }
        if found.iter().any(|l| crate::paths::normalize(&l.worktree.to_string_lossy()) == crate::paths::normalize(&place)) {
            continue;
        }
        let branch = crate::land::branch_of(root, &key).map(|(b, _)| b).unwrap_or_default();
        let stays = kept_on_purpose(&db.timeline(&key)?);
        found.push(Leftover { issue: key, worktree: path, branch, stays, removed: None, registered: false });
    }
    // Listing processes is the costly part (NIT-289): once, for the ones nothing else keeps.
    let free: Vec<usize> = (0..found.len()).filter(|&i| found[i].stays.is_none()).collect();
    if !free.is_empty() {
        let dirs: Vec<&Path> = free.iter().map(|&i| found[i].worktree.as_path()).collect();
        let (held, whole) = crate::procs::holders_of(&dirs);
        let said: Vec<Option<String>> = held
            .iter()
            .map(|h| match (whole, h.is_empty()) {
                (false, _) => Some("the running processes could not be listed".to_string()),
                (true, true) => None,
                (true, false) => Some(format!("in use by {}", h.iter().map(|h| format!("{} (pid {})", h.name, h.pid)).collect::<Vec<_>>().join(", "))),
            })
            .collect();
        for (&i, why) in free.iter().zip(said) {
            found[i].stays = why;
        }
    }
    if remove {
        for l in found.iter_mut().filter(|l| l.stays.is_none()) {
            let message = if l.registered {
                crate::land::cleanup(root, pb, &l.worktree, &l.branch, false, false).0
            } else {
                remove_folder(root, &l.worktree, &l.branch)
            };
            if !l.worktree.exists() {
                db.log_event(actor, Some(&l.issue), Some(&l.worktree.to_string_lossy()), "worktree_removed", &format!("Removed the worktree a land left: {message}"))?;
            }
            l.removed = Some(message);
        }
    }
    Ok(found)
}

/// Why a land kept the worktree on purpose: a `land_kept` since the land before the last one.
/// Lands write it before `landed`, so the last land's own keep counts.
fn kept_on_purpose(timeline: &[TimelineEntry]) -> Option<String> {
    let landed: Vec<usize> = timeline.iter().enumerate().filter(|(_, t)| t.kind == "landed").map(|(i, _)| i).collect();
    let from = if landed.len() >= 2 { landed[landed.len() - 2] + 1 } else { 0 };
    timeline[from..].iter().rev().find(|t| t.kind == "land_kept").map(|t| format!("a land kept it on purpose: {}", t.summary.as_deref().unwrap_or("--keep")))
}

/// Where lands said they kept a worktree, per issue.
fn held_places(db: &Db) -> Result<Vec<(String, String)>> {
    let mut stmt = db.conn.prepare("SELECT DISTINCT upper(issue_key), cwd FROM event WHERE type = 'worktree_held' AND issue_key IS NOT NULL AND cwd IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Deletes a folder git no longer lists, then its branch when that is merged.
fn remove_folder(root: &Path, folder: &Path, branch: &str) -> String {
    if let Err(e) = std::fs::remove_dir_all(folder) {
        return format!("could not delete the rest of the folder: {e}");
    }
    playbook::git(root, &["worktree", "prune"]).ok();
    if branch.is_empty() {
        return "deleted the rest of the folder".into();
    }
    match playbook::git(root, &["branch", "-d", branch]) {
        Ok(_) => "deleted the rest of the folder and the merged branch".into(),
        Err(e) => format!("deleted the rest of the folder; kept the branch ({})", e.to_string().lines().next().unwrap_or("")),
    }
}

/// Why a closed issue's worktree stays, before asking which processes use it.
fn why_it_stays(worktree: &Path, branch: &str, base: &str) -> Option<String> {
    let git = |args: &[&str]| playbook::git(worktree, args);
    match git(&["status", "--porcelain", "--untracked-files=normal"]) {
        Ok(s) if !s.trim().is_empty() => return Some("it has uncommitted changes".into()),
        Err(e) => return Some(format!("git could not read it: {}", e.to_string().lines().next().unwrap_or(""))),
        Ok(_) => {}
    }
    if git(&["merge-base", "--is-ancestor", "HEAD", base]).is_err() {
        return Some(format!("{branch} has commits {base} does not"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str) -> TimelineEntry {
        TimelineEntry { ts: 0, actor: "human".into(), kind: kind.into(), summary: Some(kind.into()), place: None }
    }

    #[test]
    fn a_keep_counts_from_the_land_before_the_last() {
        let of = |kinds: &[&str]| kept_on_purpose(&kinds.iter().map(|k| entry(k)).collect::<Vec<_>>()).is_some();
        // Written just before `landed`, in the same land.
        assert!(of(&["issue_created", "land_kept", "landed"]));
        assert!(!of(&["issue_created", "landed"]));
        // Kept at an earlier land, landed again since without --keep: no longer kept.
        assert!(!of(&["land_kept", "landed", "submitted", "landed"]));
        assert!(of(&["landed", "land_kept", "landed"]));
    }
}
