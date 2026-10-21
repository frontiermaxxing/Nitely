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
    /// Cone-mode directories (relative to the submodule) that task worktrees and lanes check out;
    /// empty means the whole submodule. A task widens it with `widen` on paths under it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
    /// When a task gets the submodule's worktree: only when it needs it (the default), or always.
    #[serde(default)]
    pub checkout: Checkout,
}

/// When `start_task` makes a submodule's worktree.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Checkout {
    /// When the issue names a path in it, the task already has work there, or on the first
    /// `widen` under it; a task that never touches it never writes it.
    #[default]
    WhenNeeded,
    /// For every task (its tests read the submodule, say).
    EveryTask,
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

/// Cuts the task branch in each declared submodule the task needs (`Checkout`), as a worktree of
/// the repo's copy at `<worktree>/<path>`, sparse over the submodule's `sparse` directories and those
/// of the paths in it that `text` (the issue) names. A submodule the task does not need is left
/// unchecked out, its folder empty. Returns what it did, a line each.
pub fn start(root: &Path, pb: &Playbook, worktree: &Path, branch: &str, text: &str) -> Result<Vec<String>> {
    let mut done = Vec::new();
    for s in &pb.submodules {
        if checked_out(&worktree.join(s.path())) {
            continue;
        }
        let repo = s.repo(root);
        if !checked_out(&repo) {
            bail!(
                "the submodule {} is not checked out in {}; run `git submodule update --init {}` there first",
                s.path(),
                root.display(),
                s.path()
            );
        }
        let exists = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
        let from = start_point(pb, s, worktree, &repo, branch, exists)?;
        // The paths the issue names in it that exist there: a file at its root counts too (it needs
        // no directory of its own, cone mode always has those).
        let named: Vec<String> = named_in(text, s.path()).into_iter().filter(|p| git(&repo, &["cat-file", "-e", &format!("{from}:{p}")]).is_ok()).collect();
        let dirs: Vec<String> = named.iter().filter_map(|p| crate::light::dir_of(&repo, &from, p)).collect();
        let needed = needs_submodule(s.checkout == Checkout::EveryTask, exists, !named.is_empty(), &mut || moves(pb, worktree, s.path()))?;
        if needed {
            done.push(make(pb, s, worktree, &repo, branch, exists, &from, Some(&dirs))?);
        }
    }
    Ok(done)
}

/// The paths under the submodule at `path` that an issue's text names, relative to it (NIT-537).
pub(crate) fn named_in(text: &str, path: &str) -> Vec<String> {
    let prefix = format!("{path}/");
    crate::overlap::paths_in(text)
        .iter()
        .filter_map(|p| p.trim_start_matches("./").strip_prefix(&prefix).map(|p| p.trim_matches('/').to_string()))
        .filter(|p| !p.is_empty())
        .collect()
}

/// Whether a task gets a submodule (NIT-537): every task does when the playbook says so; else one
/// whose branch already exists there, whose issue names something in it, or whose branch already
/// moves its pointer (`moves`, asked only when nothing else decided).
pub(crate) fn needs_submodule(every_task: bool, branch_exists: bool, named: bool, moves: &mut dyn FnMut() -> Result<bool>) -> Result<bool> {
    Ok(every_task || branch_exists || named || moves()?)
}

/// Where a task's submodule branch starts: the branch itself when it exists, else the commit the
/// task's repo commit points the submodule at (so a task that leaves it alone matches its pointer),
/// else the submodule's integration branch.
fn start_point(pb: &Playbook, s: &SubmoduleRules, worktree: &Path, repo: &Path, branch: &str, exists: bool) -> Result<String> {
    if exists {
        return Ok(branch.to_string());
    }
    Ok(match pointer(worktree, "HEAD", s.path())? {
        Some(p) if git(repo, &["cat-file", "-e", &format!("{p}^{{commit}}")]).is_ok() => p,
        _ => s.branch(pb).to_string(),
    })
}

/// Whether the task's branch already moves the submodule's pointer (since it left the base).
fn moves(pb: &Playbook, worktree: &Path, path: &str) -> Result<bool> {
    let Ok(fork) = git(worktree, &["merge-base", pb.integration_branch(), "HEAD"]) else { return Ok(false) };
    Ok(pointer(worktree, "HEAD", path)? != pointer(worktree, fork.trim(), path)?)
}

/// What `keep` keeps in the submodule at `path` (NIT-537): name entries (`node_modules`) at any
/// depth, and path entries that lie under it, relative to it.
pub(crate) fn kept_in<'a>(keep: &'a [String], path: &str) -> (Vec<&'a str>, Vec<String>) {
    let prefix = format!("{path}/");
    let names: Vec<&str> = keep.iter().filter_map(|k| crate::lanes::keep_anywhere(k)).collect();
    let kept: Vec<String> = keep
        .iter()
        .filter(|k| crate::lanes::keep_anywhere(k).is_none())
        .filter_map(|k| k.trim_matches('/').strip_prefix(&prefix).map(str::to_string))
        .collect();
    (names, kept)
}

/// A sparse lane's cone in a submodule (NIT-537): its `sparse` directories, the folders of the
/// files its snapshot changes since the base (`changed`, git's NUL-separated names), the kept
/// paths under it and the kept folders there now. Whole (empty) when it is not sparse.
pub(crate) fn lane_cone(s: &SubmoduleRules, changed: &str, kept: &[String], kept_now: &[String]) -> Vec<String> {
    if s.sparse.is_empty() {
        return vec![];
    }
    let mut more: Vec<String> = changed.split('\0').filter_map(|f| f.rsplit_once('/').map(|(d, _)| d.to_string())).collect();
    more.extend(kept.iter().cloned());
    more.extend(kept_now.iter().cloned());
    cone(s, &more)
}

/// The submodule's cone: its `sparse` directories and `more`, or empty (whole) when it has none.
fn cone(s: &SubmoduleRules, more: &[String]) -> Vec<String> {
    if s.sparse.is_empty() {
        return vec![];
    }
    let dirs: Vec<String> = s.sparse.iter().chain(more).map(|d| d.trim().trim_matches('/').to_string()).filter(|d| !d.is_empty()).collect();
    crate::light::dedupe(dirs)
}

/// Makes the task's worktree of the submodule: sparse over its cone plus `named`, or whole when
/// `named` is None (a task that asked for all of it).
#[allow(clippy::too_many_arguments)]
fn make(pb: &Playbook, s: &SubmoduleRules, worktree: &Path, repo: &Path, branch: &str, exists: bool, from: &str, named: Option<&[String]>) -> Result<String> {
    let at = worktree.join(s.path());
    let at_s = at.to_string_lossy().into_owned();
    let dirs = named.map(|n| cone(s, n)).unwrap_or_default();
    let mut args = vec!["worktree", "add"];
    if !dirs.is_empty() {
        args.push("--no-checkout");
    }
    if exists {
        args.extend([at_s.as_str(), branch]);
    } else {
        args.extend(["-b", branch, at_s.as_str(), from]);
    }
    // `worktree add` wants the folder empty or absent; the repo's checkout left it empty.
    std::fs::remove_dir(&at).ok();
    git(repo, &args)?;
    if !dirs.is_empty() {
        let mut set = vec!["sparse-checkout", "set", "--cone"];
        set.extend(dirs.iter().map(String::as_str));
        git(&at, &set).with_context(|| format!("making the submodule {} sparse", s.path()))?;
        git(&at, &["checkout"]).with_context(|| format!("checking out the sparse submodule {}", s.path()))?;
    }
    let tip = crate::land::rev(repo, s.branch(pb)).unwrap_or_default();
    let shape = if dirs.is_empty() { String::new() } else { format!(" (sparse: {})", dirs.join(", ")) };
    Ok(if exists {
        format!("{}: {branch} as it was{shape}", s.path())
    } else if tip == from {
        format!("{}: {branch} from {}{shape}", s.path(), s.branch(pb))
    } else {
        format!("{}: {branch} from {}, where the repo points it ({} has moved past it; land rebases onto it){shape}", s.path(), &from[..from.len().min(12)], s.branch(pb))
    })
}

/// `widen` for paths under declared submodules: makes the task's worktree of a submodule it has not
/// got yet, sparse over its cone and these paths' directories, or adds them to the sparse set of
/// one it has. Returns the paths that are not in a submodule, and what it did, a line each.
pub fn widen(root: &Path, pb: &Playbook, worktree: &Path, paths: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut rest = Vec::new();
    let mut said = Vec::new();
    let mut wanted: Vec<(&SubmoduleRules, Vec<String>)> = Vec::new();
    for p in paths {
        let clean = p.trim().replace('\\', "/").trim_start_matches("./").trim_matches('/').to_string();
        match pb.submodules.iter().find(|s| clean == s.path() || clean.starts_with(&format!("{}/", s.path()))) {
            Some(s) => {
                let rel = clean[s.path().len()..].trim_start_matches('/').to_string();
                match wanted.iter_mut().find(|(w, _)| w.path() == s.path()) {
                    Some((_, list)) => list.push(rel),
                    None => wanted.push((s, vec![rel])),
                }
            }
            None => rest.push(p.clone()),
        }
    }
    if wanted.is_empty() {
        return Ok((rest, said));
    }
    let branch = git(worktree, &["branch", "--show-current"])?.trim().to_string();
    for (s, rels) in wanted {
        let repo = s.repo(root);
        let at = worktree.join(s.path());
        let rev = if checked_out(&at) { "HEAD".to_string() } else { start_point(pb, s, worktree, &repo, &branch, git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok())? };
        let look_in = if checked_out(&at) { at.clone() } else { repo.clone() };
        // The submodule itself (`widen engine`): all of it.
        let whole = rels.iter().any(|r| r.is_empty());
        let mut dirs = Vec::new();
        let mut missing = Vec::new();
        for rel in rels.iter().filter(|r| !r.is_empty()) {
            match crate::light::dir_of(&look_in, &rev, rel) {
                Some(d) => dirs.push(d),
                None if git(&look_in, &["cat-file", "-e", &format!("{rev}:{rel}")]).is_ok() => {} // a file at its root: always there
                None => missing.push(format!("{}/{rel}", s.path())),
            }
        }
        if !missing.is_empty() {
            bail!("not in the submodule {} at this branch's commit: {}", s.path(), missing.join(", "));
        }
        if !checked_out(&at) {
            if !checked_out(&repo) {
                bail!("the submodule {} is not checked out in {}; run `git submodule update --init {}` there first", s.path(), root.display(), s.path());
            }
            let exists = git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
            let named = if whole { None } else { Some(dirs.as_slice()) };
            said.push(format!("Made the submodule's worktree: {}.", make(pb, s, worktree, &repo, &branch, exists, &rev, named)?));
            continue;
        }
        let Some(before) = crate::light::sparse_set(&at) else {
            said.push(format!("The submodule {} is whole here: every path in it is already here.", s.path()));
            continue;
        };
        if whole {
            git(&at, &["sparse-checkout", "disable"]).with_context(|| format!("making the submodule {} whole", s.path()))?;
            said.push(format!("The submodule {} is whole now.", s.path()));
            continue;
        }
        let added: Vec<String> = crate::light::dedupe(dirs).into_iter().filter(|d| !before.iter().any(|b| d == b || d.starts_with(&format!("{b}/")))).collect();
        if added.is_empty() {
            said.push(format!("The submodule {} already has them.", s.path()));
        } else {
            let mut args = vec!["sparse-checkout", "add"];
            args.extend(added.iter().map(String::as_str));
            git(&at, &args).with_context(|| format!("widening the submodule {}", s.path()))?;
            said.push(format!("Added to the submodule {}: {}.", s.path(), added.join(", ")));
        }
    }
    Ok((rest, said))
}

/// The commit `path` points at in `commit`, when it is a submodule there.
fn pointer(dir: &Path, commit: &str, path: &str) -> Result<Option<String>> {
    let line = git(dir, &["ls-tree", commit, "--", path])?;
    Ok(line.split_whitespace().nth(2).filter(|_| line.starts_with("160000")).map(str::to_string))
}

/// Whether the branch checked out in `worktree` moved the submodule at `path` itself: its pointer
/// `pointed` is neither the integration branch's nor the one where the branch left it.
fn moved_by_branch(worktree: &Path, base: &str, path: &str, pointed: &str) -> bool {
    let fork = git(worktree, &["merge-base", "HEAD", base]).map(|s| s.trim().to_string());
    let at_base = pointer(worktree, base, path).ok().flatten();
    let at_fork = fork.ok().and_then(|f| pointer(worktree, &f, path).ok().flatten());
    at_base.as_deref() != Some(pointed) && at_fork.as_deref() != Some(pointed)
}

/// Whether `older` is `newer` or an ancestor of it, in the repository at `dir`.
fn is_ancestor(dir: &Path, older: &str, newer: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", older, newer]).is_ok()
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
        let pointed = pointer(worktree, "HEAD", s.path())?;
        if pointed.as_deref() != Some(head.as_str()) {
            // A checkout behind a pointer the branch did not move (one the integration branch
            // moved, which a rebase brought in) is stale, not new work: `git add` would commit the
            // old pointer and take the integration branch's bump back (NIT-343).
            // Not new work either when the checkout holds nothing the submodule's integration branch
            // lacks: the integration branch moved the pointer back past it (NIT-357).
            let landed = |c: &str| is_ancestor(&at, c, s.branch(pb));
            if let Some(p) = pointed.as_deref().filter(|p| !moved_by_branch(worktree, pb.integration_branch(), s.path(), p) && (is_ancestor(&at, &head, p) || landed(&head))) {
                let short = &p[..p.len().min(12)];
                let on_branch = git(&at, &["symbolic-ref", "-q", "HEAD"]).is_ok();
                let behind = is_ancestor(&at, &head, p);
                let fix = match (on_branch, behind) {
                    (true, true) => format!("git -C {} merge --ff-only {short}", s.path()),
                    // Its commits are all on the submodule's integration branch, so moving the task
                    // branch back loses nothing.
                    (true, false) => format!("git -C {} reset --keep {short}", s.path()),
                    (false, _) => format!("git -C {} checkout --detach {short}", s.path()),
                };
                let at_what = if behind { format!("behind {short}") } else { format!("not at {short}") };
                bail!(
                    "the submodule {} is checked out at {}, {at_what}, where this branch points it as {} does; the branch did not change it, so bring the checkout to it (`{fix}`) and submit again. Do not `git add {}`: that would point it at a commit {} does not",
                    s.path(),
                    &head[..head.len().min(12)],
                    pb.integration_branch(),
                    s.path(),
                    pb.integration_branch()
                );
            }
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
    /// Where the base pointed it: where the repo's own copy is checked out after `git submodule
    /// update`, which can be behind `before`.
    pub pinned: Option<String>,
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
        let pinned = pointer(worktree, base, path)?;
        moving.push(Moving { path: path.to_string(), repo, branch, before, head, pinned });
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
    /// The repo's own copy, detached where the main checkout had it and moved along with the branch.
    followed: bool,
    /// Where that copy was before it followed.
    was: String,
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
        // The repo's copy detached where the main checkout had it: at the old tip, or where the repo
        // pointed it (`git submodule update` leaves it there, which can be behind the tip). It
        // follows, or the main checkout would build the old code.
        let detached = checkout.is_none() && git(&m.repo, &["symbolic-ref", "-q", "HEAD"]).is_err();
        let was = git(&m.repo, &["rev-parse", "HEAD"]).map(|h| h.trim().to_string()).unwrap_or_default();
        let follows = detached && (was == m.before || m.pinned.as_deref() == Some(was.as_str()));
        let mut u = Undo { repo: m.repo.clone(), branch: m.branch.clone(), before: m.before.clone(), head: m.head.clone(), checkout, followed: false, was };
        if follows {
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
            if let Err(e) = git(&u.repo, &["checkout", "-q", "--detach", &u.was]) {
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
            // As the repo's own: a rebased branch replaces its old head, never commits it has not seen.
            git_remote(&at, &["push", "--force-with-lease", "-u", remote, &format!("HEAD:refs/heads/{branch}")])
                .with_context(|| format!("pushing the submodule {}'s {branch} to {remote}", s.path()))?;
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
        // A sparse submodule: its cone, the directories the snapshot changes in it since the base, and
        // the `keep` folders under it (narrowing the cone would delete what they hold).
        let (names, kept) = kept_in(keep, path);
        let dirs = if s.sparse.is_empty() {
            vec![]
        } else {
            // NUL-separated: `--name-only` quotes unusual names otherwise.
            let changed = match pointer(lane, pb.integration_branch(), path)? {
                Some(base) if base != at_commit => git(&repo, &["diff", "-z", "--name-only", "--no-renames", &base, &at_commit]).unwrap_or_default(),
                _ => String::new(),
            };
            // And the folders a name entry keeps there now (`extra/node_modules`): narrowing the
            // cone would delete them before the clean could keep them.
            let checkout = lane.join(path);
            let kept_now = if checked_out(&checkout) {
                let named: Vec<String> = names.iter().map(|n| n.to_string()).collect();
                crate::lanes::kept_dirs(&checkout, &named)
            } else {
                vec![]
            };
            lane_cone(s, &changed, &kept, &kept_now)
        };
        let at = lane.join(path);
        if !checked_out(&at) {
            std::fs::remove_dir(&at).ok();
            let at_s = at.to_string_lossy().into_owned();
            if dirs.is_empty() {
                git(&repo, &["worktree", "add", "--detach", &at_s, &at_commit])?;
            } else {
                git(&repo, &["worktree", "add", "--no-checkout", "--detach", &at_s, &at_commit])?;
                let mut set = vec!["sparse-checkout", "set", "--cone"];
                set.extend(dirs.iter().map(String::as_str));
                git(&at, &set)?;
                git(&at, &["checkout"])?;
            }
            continue;
        }
        match (crate::light::sparse_set(&at), dirs.is_empty()) {
            (Some(_), true) => {
                git(&at, &["sparse-checkout", "disable"])?;
            }
            (have, false) if have.as_deref() != Some(dirs.as_slice()) => {
                let mut set = vec!["sparse-checkout", "set", "--cone"];
                set.extend(dirs.iter().map(String::as_str));
                git(&at, &set)?;
            }
            _ => {}
        }
        git(&at, &["checkout", "--quiet", "--force", "--detach", &at_commit])?;
        let mut args: Vec<String> = vec!["clean".into(), "-ffdxq".into()];
        for k in &kept {
            args.push("-e".into());
            args.push(format!("/{k}"));
        }
        for name in &names {
            args.push("-e".into());
            args.push(format!("{name}/"));
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

    /// When a task gets the submodule, and a sparse lane's cone, over plain data (NIT-537): what
    /// the task and sparse-lane tests below decide, without their repos.
    #[test]
    fn a_task_and_a_lane_take_only_what_they_need_of_a_submodule() {
        assert_eq!(named_in("Fix `engine/core/run.rs` and engine/README.md, not engines/x.rs", "engine"), ["core/run.rs", "README.md"]);
        let never = &mut || -> Result<bool> { panic!("moves is asked only when nothing else decided") };
        assert!(needs_submodule(true, false, false, never).unwrap());
        assert!(needs_submodule(false, true, false, never).unwrap());
        assert!(needs_submodule(false, false, true, never).unwrap());
        assert!(needs_submodule(false, false, false, &mut || Ok(true)).unwrap(), "its branch moves the pointer");
        assert!(!needs_submodule(false, false, false, &mut || Ok(false)).unwrap());

        let keep = vec!["node_modules".to_string(), "engine/out".to_string(), "target".to_string(), "other/out".to_string()];
        let (names, kept) = kept_in(&keep, "engine");
        assert_eq!((names, kept), (vec!["node_modules", "target"], vec!["out".to_string()]));
        let sparse: SubmoduleRules = toml::from_str("path = 'engine'\nsparse = ['core']").unwrap();
        assert_eq!(lane_cone(&sparse, "net/http.rs\0core/a.rs\0top.txt\0", &["out".into()], &["extra/node_modules".into()]), ["core", "extra/node_modules", "net", "out"]);
        let whole: SubmoduleRules = toml::from_str("path = 'engine'").unwrap();
        assert!(lane_cone(&whole, "net/http.rs\0", &["out".into()], &[]).is_empty(), "a submodule that is not sparse is whole");
    }

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
    fn a_submodule_change_keeps_its_patch_when_the_lines_around_it_change() {
        let base = crate::paths::unique_temp("nitely-sub-context", "");
        let upstream = base.join("engine-upstream");
        crate::test_repo::init(&upstream, &[("core.c", "a\nb\nc\nd\ne\n")]);
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("app.txt", "app\n")]);
        sh(&repo, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &upstream.to_string_lossy(), "engine"]);
        sh(&repo, &["commit", "-q", "-m", "the engine"]);
        let engine = repo.join("engine");
        // The same change (c to C) on top of whatever the engine's main is.
        let change = |branch: &str| {
            sh(&engine, &["checkout", "-q", "-B", branch, "main"]);
            let text = std::fs::read_to_string(engine.join("core.c")).unwrap().replace("\nc", "\nC");
            std::fs::write(engine.join("core.c"), text).unwrap();
            sh(&engine, &["commit", "-q", "-am", "c"]);
            sh(&repo, &["checkout", "-q", "-B", branch, "main"]);
            sh(&repo, &["add", "engine"]);
            sh(&repo, &["commit", "-q", "-m", "c"]);
            crate::submit::patch_id_range(&repo, "main", "HEAD").unwrap()
        };
        let first = change("one");
        // Another land changes a line two above, in step.
        sh(&engine, &["checkout", "-q", "main"]);
        std::fs::write(engine.join("core.c"), "A\nb\nc\nd\ne\n").unwrap();
        sh(&engine, &["commit", "-q", "-am", "a"]);
        sh(&repo, &["checkout", "-q", "main"]);
        sh(&repo, &["add", "engine"]);
        sh(&repo, &["commit", "-q", "-m", "a"]);
        assert_eq!(change("two"), first, "the lines around the change are not part of it");
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
