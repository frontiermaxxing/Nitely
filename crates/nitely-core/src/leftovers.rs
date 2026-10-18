//! Worktrees a land meant to remove but had to keep (NIT-340): a dev server, a build or the
//! landing agent's own shell still used them. Each is said on its issue (`worktree_held`), and the
//! next land, or `nitely lanes leftovers --remove`, removes the ones nothing holds any more.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::issues::Actor;
use crate::playbook::{self, Playbook};

/// A landed issue's worktree still on disk, and whether it goes.
#[derive(Debug, Clone, Serialize)]
pub struct Leftover {
    pub issue: String,
    pub worktree: PathBuf,
    pub branch: String,
    /// Why it stays; None when nothing keeps it.
    pub stays: Option<String>,
    /// What removing it did, when it was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<String>,
}

/// Said on the issue when land kept a worktree it meant to remove, with why (`cleanup`, the first
/// clause of what land reported), so a later land knows to try again.
pub(crate) fn note_held(db: &Db, actor: &Actor, key: &str, worktree: &Path, cleanup: &str) -> Result<()> {
    let why = cleanup.split("; stop ").next().unwrap_or(cleanup);
    db.log_event(actor, Some(key), Some(&worktree.to_string_lossy()), "worktree_held", &format!("Worktree kept after landing: {why}. The next land removes it once nothing uses it"))
}

/// The worktrees, under the playbook's worktree folder, of issues that landed and are closed, each
/// with why it stays. One goes when it was not kept on purpose since it landed (`--keep`), its
/// branch is in the integration branch, it has nothing uncommitted or untracked, and no crew
/// worker and no process uses it. With `remove`, the ones that go are removed with their merged
/// branch, and each removal is said on its issue.
pub fn leftovers(db: &Db, root: &Path, pb: &Playbook, remove: bool, actor: &Actor) -> Result<Vec<Leftover>> {
    let issue = crate::IssueRecord { key: "x".into(), title: "x".into(), ..Default::default() };
    let folder = pb.worktree_for(root, "x", &issue).parent().map(Path::to_path_buf);
    let Some(folder) = folder else { return Ok(vec![]) };
    let base = pb.integration_branch();
    let mut found = Vec::new();
    for wt in crate::git::list_worktrees(&root.to_string_lossy())? {
        if wt.is_main || wt.prunable || !crate::paths::is_within(&wt.path, &folder.to_string_lossy()) {
            continue;
        }
        let Some(branch) = wt.branch.clone() else { continue };
        let Some(key) = crate::git::parse_branch(&branch).1.map(|k| k.to_uppercase()) else { continue };
        let Some(record) = db.issue(&key)? else { continue };
        if !matches!(record.state_type.as_str(), "completed" | "canceled") {
            continue;
        }
        let timeline = db.timeline(&key)?;
        let Some(landed) = timeline.iter().rposition(|t| t.kind == "landed") else { continue };
        let path = PathBuf::from(&wt.path);
        let stays = if timeline[landed..].iter().any(|t| t.kind == "land_kept") {
            Some("kept on purpose when it landed (--keep)".to_string())
        } else if db.crew_worker_in(&path)? {
            Some("a crew worker still runs in it".to_string())
        } else {
            why_it_stays(&path, &branch, base)
        };
        found.push(Leftover { issue: key, worktree: path, branch, stays, removed: None });
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
            let (message, held) = crate::land::cleanup(root, pb, &l.worktree, &l.branch, false, false);
            if held.is_empty() && !l.worktree.exists() {
                db.log_event(actor, Some(&l.issue), Some(&l.worktree.to_string_lossy()), "worktree_removed", &format!("Removed the worktree land kept: {message}"))?;
            }
            l.removed = Some(message);
        }
    }
    Ok(found)
}

/// Why a landed worktree stays, before asking which processes use it.
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
