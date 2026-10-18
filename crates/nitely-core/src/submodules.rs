//! Submodules that land with their repo (docs/playbooks.md, "Submodules that land with the repo").
//!
//! A playbook's `[[submodules]]` names a submodule whose own branch moves with the repo's: a task
//! gets a branch in it too, the review reads its code, and land moves its integration branch and
//! the repo's together, or neither.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::playbook::{git, git_remote, Playbook};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubmoduleRules {
    /// The submodule's folder in the repo, e.g. "engine".
    pub path: String,
    /// The submodule's integration branch; the repo's own name when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The remote the submodule's integration branch is pushed to; left out, it stays on this
    /// machine (a submodule whose only remote is an upstream nobody pushes to).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

impl SubmoduleRules {
    pub fn path(&self) -> &str {
        self.path.trim_matches('/')
    }

    /// Its integration branch.
    pub fn branch<'a>(&'a self, pb: &'a Playbook) -> &'a str {
        self.branch.as_deref().filter(|b| !b.trim().is_empty()).unwrap_or_else(|| pb.integration_branch())
    }

    /// The repo's own copy of the submodule, which holds its branches.
    pub fn repo(&self, root: &Path) -> PathBuf {
        root.join(self.path())
    }
}

/// Whether `dir` holds a checkout of its own (a submodule's, or a worktree of one).
fn checked_out(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Cuts the task branch in each declared submodule, as a worktree of the repo's copy at
/// `<worktree>/<path>`: from the commit the task's repo commit points it at (so a task that leaves
/// the submodule alone matches its pointer), or the branch as it is when it exists. Returns what it
/// did, a line each.
pub fn start(root: &Path, pb: &Playbook, worktree: &Path, branch: &str) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for s in &pb.submodules {
        let repo = s.repo(root);
        let at = worktree.join(s.path());
        if checked_out(&at) {
            continue;
        }
        if !checked_out(&repo) {
            bail!(
                "the submodule {} is not checked out in {}; run `git submodule update --init {}` there first",
                s.path(),
                root.display(),
                s.path()
            );
        }
        let at_s = at.to_string_lossy().into_owned();
        let exists = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        let from = match pointer(worktree, "HEAD", s.path())? {
            Some(p) if git(&repo, &["cat-file", "-e", &format!("{p}^{{commit}}")]).is_ok() => p,
            _ => s.branch(pb).to_string(),
        };
        if exists {
            git(&repo, &["worktree", "add", &at_s, branch])?;
        } else {
            git(&repo, &["worktree", "add", "-b", branch, &at_s, &from])?;
        }
        let tip = crate::land::rev(&repo, s.branch(pb)).unwrap_or_default();
        let words = if exists {
            format!("{}: {branch} as it was", s.path())
        } else if tip == from {
            format!("{}: {branch} from {}", s.path(), s.branch(pb))
        } else {
            format!("{}: {branch} from {}, where the repo points it ({} has moved past it; land rebases onto it)", s.path(), &from[..from.len().min(12)], s.branch(pb))
        };
        done.push(words);
    }
    Ok(done)
}

/// The commit `path` points at in `commit`, when it is a submodule there.
fn pointer(dir: &Path, commit: &str, path: &str) -> Result<Option<String>> {
    let line = git(dir, &["ls-tree", commit, "--", path])?;
    Ok(line.split_whitespace().nth(2).filter(|_| line.starts_with("160000")).map(str::to_string))
}

/// Refuses a task's submodule whose work the repo's commit does not name: uncommitted changes, or
/// a checkout at another commit than the pointer at HEAD (the review would not see it).
pub fn check_pointers(pb: &Playbook, worktree: &Path) -> Result<()> {
    for s in &pb.submodules {
        let at = worktree.join(s.path());
        if !checked_out(&at) {
            continue;
        }
        let dirty = git(&at, &["status", "--porcelain", "--untracked-files=no"])?;
        if !dirty.trim().is_empty() {
            bail!("the submodule {} has uncommitted changes; commit them there, then `git add {}` and commit here:\n{}", s.path(), s.path(), dirty.trim_end());
        }
        let head = git(&at, &["rev-parse", "HEAD"])?.trim().to_string();
        if pointer(worktree, "HEAD", s.path())?.as_deref() != Some(head.as_str()) {
            bail!(
                "the submodule {} is at {}, which this branch's last commit does not point at; `git add {}` and commit, so the change (and its review) includes it",
                s.path(),
                &head[..head.len().min(12)],
                s.path()
            );
        }
    }
    Ok(())
}

/// A submodule a land moves: its integration branch from `before` to `head`.
#[derive(Debug, Clone)]
pub struct Moving {
    pub path: String,
    pub repo: PathBuf,
    pub branch: String,
    pub before: String,
    pub head: String,
}

/// For a land: the declared submodules the branch moves (its pointer at HEAD differs from
/// `base`'s), each rebased onto its integration branch in the task's submodule worktree. Stops on
/// a conflict, left in progress there, like the repo's own rebase.
pub fn rebase(root: &Path, pb: &Playbook, worktree: &Path, base: &str) -> Result<Vec<Moving>> {
    let mut moving = Vec::new();
    let fork = git(worktree, &["merge-base", base, "HEAD"])?.trim().to_string();
    for s in &pb.submodules {
        let path = s.path();
        // Moved by this branch: since it left the base, not compared with where the base is now
        // (another land may have moved the pointer since).
        let ours = pointer(worktree, "HEAD", path)?;
        if ours.is_none() || ours == pointer(worktree, &fork, path)? {
            continue;
        }
        let at = worktree.join(path);
        if !checked_out(&at) {
            bail!("this branch moves the submodule {path}, but {} has no checkout of it; run start_task again to make one", worktree.display());
        }
        let repo = s.repo(root);
        let branch = s.branch(pb).to_string();
        let before = crate::land::rev(&repo, &branch).with_context(|| format!("the submodule {path} has no branch {branch}"))?;
        if git(&at, &["merge-base", "--is-ancestor", &branch, "HEAD"]).is_err() {
            if let Err(e) = git(&at, &["rebase", &branch]) {
                let conflicts: Vec<String> = git(&at, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect();
                if conflicts.is_empty() {
                    git(&at, &["rebase", "--abort"]).ok();
                    return Err(e).context(format!("rebasing the submodule {path} onto {branch}"));
                }
                bail!(
                    "the rebase of the submodule {path} onto {branch} stopped on conflicts in {}; it is left in progress in {}. \
                     Resolve each file, git add it, git rebase --continue, then `git add {path}` and commit here, and submit and land again",
                    conflicts.join(", "),
                    at.display()
                );
            }
        }
        let head = git(&at, &["rev-parse", "HEAD"])?.trim().to_string();
        moving.push(Moving { path: path.to_string(), repo, branch, before, head });
    }
    Ok(moving)
}

/// Rebases the task branch in `worktree` onto `base`. A stop whose only conflicts are pointers of
/// submodules being moved is resolved by pointing them at their rebased commit; any other conflict
/// is the caller's, as from a plain `git rebase`.
pub fn rebase_repo(worktree: &Path, base: &str, moving: &[Moving]) -> Result<()> {
    let mut result = git(worktree, &["rebase", base]).map(|_| ());
    // Each stop is one commit; a branch has far fewer than this.
    for _ in 0..1000 {
        let Err(e) = result else { return Ok(()) };
        let conflicts: Vec<String> = git(worktree, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect();
        let pins: Vec<&Moving> = conflicts.iter().filter_map(|c| moving.iter().find(|m| m.path == *c)).collect();
        if conflicts.is_empty() || pins.len() != conflicts.len() {
            return Err(e);
        }
        for m in pins {
            git(worktree, &["update-index", "--cacheinfo", &format!("160000,{},{}", m.head, m.path)])?;
        }
        result = git(worktree, &["-c", "core.editor=true", "rebase", "--continue"]).map(|_| ());
        // A commit that only moved the pointer, to where it points now, is empty: drop it.
        if result.is_err() && git(worktree, &["diff", "--cached", "--quiet"]).is_ok() && git(worktree, &["diff", "--name-only", "--diff-filter=U"])?.trim().is_empty() {
            result = git(worktree, &["rebase", "--skip"]).map(|_| ());
        }
    }
    result
}

/// After the rebase: the task branch's last commit points each moving submodule at its rebased
/// commit, with a commit of its own when it does not yet.
pub fn point(worktree: &Path, moving: &[Moving]) -> Result<()> {
    let mut moved = Vec::new();
    for m in moving {
        if pointer(worktree, "HEAD", &m.path)?.as_deref() != Some(m.head.as_str()) {
            git(worktree, &["update-index", "--cacheinfo", &format!("160000,{},{}", m.head, m.path)])?;
            moved.push(format!("{} at {}", m.path, &m.head[..m.head.len().min(12)]));
        }
    }
    if !moved.is_empty() {
        git(worktree, &["commit", "-q", "-m", &format!("Point {}", moved.join(", "))])?;
    }
    Ok(())
}

/// Whether each moving submodule is still as the rebase left it: its integration branch where it
/// was, its task head unchanged.
pub fn unchanged(worktree: &Path, moving: &[Moving]) -> Result<Option<String>> {
    for m in moving {
        if crate::land::rev(&m.repo, &m.branch)? != m.before {
            return Ok(Some(format!("the submodule {}'s {} moved during the gate; land again", m.path, m.branch)));
        }
        if git(&worktree.join(&m.path), &["rev-parse", "HEAD"])?.trim() != m.head {
            return Ok(Some(format!("the submodule {} gained commits during the gate; submit again", m.path)));
        }
    }
    Ok(None)
}

/// How to put a fast-forwarded submodule branch back.
pub struct Undo {
    repo: PathBuf,
    branch: String,
    before: String,
    head: String,
    /// The checkout that has the branch, when one does: put back with `reset --keep` there.
    checkout: Option<PathBuf>,
    /// The repo's own copy, detached at the old tip and moved along with the branch.
    followed: bool,
}

/// The checkout of `repo` that has `branch`, if any.
fn checkout_of(repo: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let list = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut path = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if line == format!("branch refs/heads/{branch}") {
            // The repo's own copy of a submodule lists its git dir as its worktree; the files are
            // where `repo` is.
            return Ok(path.map(|p| if is_git_dir(&p) { repo.to_path_buf() } else { p }));
        }
    }
    Ok(None)
}

fn is_git_dir(p: &Path) -> bool {
    p.join("HEAD").is_file() && p.join("objects").is_dir()
}

/// Fast-forwards each moving submodule's integration branch to its rebased head: in the checkout
/// that has it, else the branch alone. Returns how to undo each, or puts back what moved and fails.
pub fn fast_forward(moving: &[Moving]) -> Result<Vec<Undo>> {
    let mut done: Vec<Undo> = Vec::new();
    for m in moving {
        let checkout = checkout_of(&m.repo, &m.branch)?;
        let moved = match &checkout {
            Some(dir) => git(dir, &["merge", "--ff-only", &m.head])
                .map(|_| ())
                .with_context(|| format!("the submodule {}'s checkout refused the fast-forward of {} (do uncommitted edits there overlap the change?)", m.path, m.branch)),
            None => git(&m.repo, &["update-ref", &format!("refs/heads/{}", m.branch), &m.head, &m.before]).map(|_| ()).with_context(|| format!("moving the submodule {}'s {}", m.path, m.branch)),
        };
        if let Err(e) = moved {
            undo(&done);
            return Err(e);
        }
        // The usual state after `git submodule update`: the repo's copy detached at the commit the
        // repo pointed at. It follows, or the main checkout would build the old code.
        let detached = checkout.is_none() && git(&m.repo, &["symbolic-ref", "-q", "HEAD"]).is_err();
        let followed = detached && git(&m.repo, &["rev-parse", "HEAD"]).is_ok_and(|h| h.trim() == m.before);
        let mut u = Undo { repo: m.repo.clone(), branch: m.branch.clone(), before: m.before.clone(), head: m.head.clone(), checkout, followed: false };
        if followed {
            if let Err(e) = git(&m.repo, &["checkout", "-q", "--detach", &m.head]) {
                done.push(u);
                undo(&done);
                return Err(e).with_context(|| format!("moving the submodule {}'s checkout to the new {}", m.path, m.branch));
            }
            u.followed = true;
        }
        done.push(u);
    }
    Ok(done)
}

/// Puts fast-forwarded submodule branches back where they were. Returns what it could not.
pub fn undo(done: &[Undo]) -> Vec<String> {
    let mut failed = Vec::new();
    for u in done.iter().rev() {
        if u.followed {
            if let Err(e) = git(&u.repo, &["checkout", "-q", "--detach", &u.before]) {
                failed.push(format!("the submodule checkout {} could not be put back ({})", u.repo.display(), crate::land::first_line(&e)));
            }
        }
        let back = match &u.checkout {
            Some(dir) => git(dir, &["reset", "-q", "--keep", &u.before]).map(|_| ()),
            None => git(&u.repo, &["update-ref", &format!("refs/heads/{}", u.branch), &u.before, &u.head]).map(|_| ()),
        };
        if let Err(e) = back {
            failed.push(format!("the submodule branch {} could not be put back to {} ({})", u.branch, &u.before[..u.before.len().min(12)], crate::land::first_line(&e)));
        }
    }
    failed
}

/// Pushes each declared submodule's integration branch that has a remote: every one, not only those
/// this land moved, so one a failed push left behind goes out before any repo commit pointing
/// into it. Err names the first that failed.
pub fn push(root: &Path, pb: &Playbook) -> Result<()> {
    for s in &pb.submodules {
        let Some(remote) = s.remote.as_deref().filter(|r| !r.trim().is_empty()) else { continue };
        let repo = s.repo(root);
        if checked_out(&repo) {
            git_remote(&repo, &["push", remote, s.branch(pb)]).with_context(|| format!("pushing the submodule {}'s {} to {remote}", s.path(), s.branch(pb)))?;
        }
    }
    Ok(())
}

/// Pushes the task branch of each declared submodule that has a remote, from the task's worktree,
/// so a pull request's pointer resolves on the remote.
pub fn push_task(pb: &Playbook, worktree: &Path, branch: &str) -> Result<()> {
    for s in &pb.submodules {
        let Some(remote) = s.remote.as_deref().filter(|r| !r.trim().is_empty()) else { continue };
        let at = worktree.join(s.path());
        if checked_out(&at) {
            git_remote(&at, &["push", "--force", remote, &format!("HEAD:refs/heads/{branch}")]).with_context(|| format!("pushing the submodule {}'s {branch} to {remote}", s.path()))?;
        }
    }
    Ok(())
}

/// After the rebase: each declared submodule the branch leaves alone, checked out in the task's
/// worktree where the rebased branch points it (another land may have moved it), so the gate
/// builds what lands.
pub fn follow(pb: &Playbook, worktree: &Path, moving: &[Moving]) -> Result<()> {
    for s in pb.submodules.iter().filter(|s| !moving.iter().any(|m| m.path == s.path())) {
        let at = worktree.join(s.path());
        let Some(to) = pointer(worktree, "HEAD", s.path())? else { continue };
        if !checked_out(&at) || git(&at, &["rev-parse", "HEAD"])?.trim() == to {
            continue;
        }
        // The task's branch there has nothing of its own (the branch does not move it), so it
        // can follow.
        git(&at, &["reset", "-q", "--keep", &to]).with_context(|| format!("moving the submodule {} to where the rebased branch points it", s.path()))?;
    }
    Ok(())
}

/// Before the task's worktree is removed: each submodule worktree in it goes first (git will not
/// remove a worktree that holds one), leaving the empty folder an unchecked-out submodule has.
/// Err says what was kept and why.
pub fn remove_worktrees(root: &Path, pb: &Playbook, worktree: &Path) -> std::result::Result<(), String> {
    for s in &pb.submodules {
        let at = worktree.join(s.path());
        if !at.join(".git").is_file() {
            continue;
        }
        if let Err(e) = git(&s.repo(root), &["worktree", "remove", &at.to_string_lossy()]) {
            return Err(format!("kept the worktree: its submodule {} could not be removed ({})", s.path(), crate::land::first_line(&e)));
        }
        std::fs::create_dir_all(&at).ok();
    }
    Ok(())
}

/// After the task's worktree is gone: the task branch in each submodule, when it is merged.
pub fn delete_branches(root: &Path, pb: &Playbook, branch: &str) {
    for s in &pb.submodules {
        let repo = s.repo(root);
        if checked_out(&repo) {
            git(&repo, &["branch", "-d", branch]).ok();
        }
    }
}

/// Large-repo mode builds in the person's own checkout, whose submodules stay as they are: a
/// snapshot that points one elsewhere would be built against the wrong code, so it is refused.
pub fn check_large(root: &Path, pb: &Playbook, commit: &str) -> Result<()> {
    for s in &pb.submodules {
        let Some(want) = pointer(root, commit, s.path())? else { continue };
        let repo = s.repo(root);
        let have = git(&repo, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()).unwrap_or_default();
        if have != want {
            bail!(
                "this snapshot points the submodule {} at {}, but your checkout has it at {}; large-repo mode builds in your checkout and leaves its submodules alone, so it cannot build this one (land it to move both, or run it without large_repo)",
                s.path(),
                &want[..want.len().min(12)],
                if have.is_empty() { "nothing".to_string() } else { have[..have.len().min(12)].to_string() }
            );
        }
    }
    Ok(())
}

/// Puts each declared submodule of a build lane at the commit `commit` points it at: a detached
/// worktree of the repo's copy, made the first time, then checked out and cleaned, keeping the
/// `keep` folders under it.
pub fn sync_lane(root: &Path, pb: &Playbook, lane: &Path, commit: &str, keep: &[String]) -> Result<()> {
    for s in &pb.submodules {
        let path = s.path();
        let Some(at_commit) = pointer(lane, commit, path)? else { continue };
        let repo = s.repo(root);
        if !checked_out(&repo) {
            continue;
        }
        let at = lane.join(path);
        if !checked_out(&at) {
            std::fs::remove_dir(&at).ok();
            git(&repo, &["worktree", "add", "--detach", &at.to_string_lossy(), &at_commit])?;
            continue;
        }
        git(&at, &["checkout", "--quiet", "--force", "--detach", &at_commit])?;
        let mut args: Vec<String> = vec!["clean".into(), "-ffdxq".into()];
        let prefix = format!("{path}/");
        for k in keep.iter().filter_map(|k| k.trim_matches('/').strip_prefix(&prefix)) {
            args.push("-e".into());
            args.push(format!("/{k}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        git(&at, &args)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_repo::git as sh;

    #[test]
    fn a_lane_has_the_submodule_at_its_snapshot_and_keeps_its_build_output() {
        let base = crate::paths::unique_temp("nitely-sub-lane", "");
        let upstream = base.join("engine-upstream");
        crate::test_repo::init(&upstream, &[("core.c", "int core;\n")]);
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("app.txt", "app\n")]);
        sh(&repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &upstream.to_string_lossy(), "engine"]);
        sh(&repo, &["commit", "-q", "-m", "the engine"]);
        let pb: Playbook = toml::from_str("[[submodules]]\npath = \"engine\"\n").unwrap();
        let keep = vec!["target".to_string(), "engine/out".to_string()];

        let lane = base.join("lane");
        sh(&repo, &["worktree", "add", "-q", "--detach", &lane.to_string_lossy(), "HEAD"]);
        let first = sh(&repo, &["rev-parse", "HEAD"]);
        sync_lane(&repo, &pb, &lane, &first, &keep).unwrap();
        assert_eq!(std::fs::read_to_string(lane.join("engine/core.c")).unwrap().replace('\r', ""), "int core;\n");
        std::fs::create_dir_all(lane.join("engine/out")).unwrap();
        std::fs::write(lane.join("engine/out/core.o"), "built").unwrap();
        std::fs::write(lane.join("engine/stray.tmp"), "x").unwrap();

        // The engine moves; the lane switches as lanes::switch does, then syncs its engine.
        std::fs::write(repo.join("engine/core.c"), "int core;\nint faster;\n").unwrap();
        sh(&repo.join("engine"), &["commit", "-q", "-am", "faster"]);
        sh(&repo, &["commit", "-q", "-am", "point at faster"]);
        let second = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&lane, &["checkout", "--quiet", "--force", "--detach", &second]);
        sh(&lane, &["clean", "-ffdxq", "-e", "/target", "-e", "/engine/out"]);
        sync_lane(&repo, &pb, &lane, &second, &keep).unwrap();
        assert_eq!(std::fs::read_to_string(lane.join("engine/core.c")).unwrap().replace('\r', ""), "int core;\nint faster;\n");
        assert!(lane.join("engine/out/core.o").exists(), "its build output stays");
        assert!(!lane.join("engine/stray.tmp").exists(), "what else it made goes");
        // Where the commit points (its kept build output is untracked content, which status counts).
        assert!(sh(&lane, &["ls-tree", &second, "engine"]).contains(&sh(&lane.join("engine"), &["rev-parse", "HEAD"])));
        assert!(sh(&lane, &["status", "--porcelain", "--ignore-submodules=untracked"]).is_empty(), "the lane is clean");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_pointer_whose_code_cannot_be_shown_still_changes_the_patch() {
        let base = crate::paths::unique_temp("nitely-sub-patch", "");
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("app.txt", "app\n")]);
        let id = |sha: &str, branch: &str| {
            sh(&repo, &["checkout", "-q", "-B", branch, "main"]);
            sh(&repo, &["update-index", "--add", "--cacheinfo", &format!("160000,{sha},engine")]);
            sh(&repo, &["commit", "-q", "-m", "point"]);
            crate::submit::patch_id_range(&repo, "main", "HEAD").unwrap()
        };
        // Commits nobody here has: git can only say the pointer moved.
        let (a, b) = (id(&"a".repeat(40), "one"), id(&"b".repeat(40), "two"));
        assert_ne!(a, "empty");
        assert_ne!(a, b, "another pointer is another patch");
        assert_eq!(a, id(&"a".repeat(40), "three"), "the same pointer, the same patch");
        std::fs::remove_dir_all(&base).ok();
    }
}
