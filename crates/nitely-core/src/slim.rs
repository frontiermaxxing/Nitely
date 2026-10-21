//! `nitely lanes slim` (docs/build-lanes.md, "Moving over"): reclaims the build directories of
//! task worktrees that hold none of the agent's unfinished work. Build output is never anyone's
//! work, but a worktree an agent still uses would have to build again, so only finished ones go:
//! nothing uncommitted, the branch pushed or landed, and no process using the folder.

use crate::playbook::{git, Playbook};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// How long after the last sign of an agent at work slim still counts it as working: Windows does
/// not tell which folder a process works in, so its session's folder and its issue's timeline say
/// it.
pub const RECENT_SECS: i64 = 3600;

/// Signs of agents at work lately: the folders their sessions ran in, normalized, and the issues
/// with a timeline event (a report, a note, a gate, a submit), upper-case.
#[derive(Debug, Clone, Default)]
pub struct Recent {
    pub folders: Vec<String>,
    pub issues: Vec<String>,
}

/// The build directories a lane keeps, and slim reclaims, when the playbook names none.
pub const DEFAULT_KEEP: &[&str] = &["target", "node_modules"];

/// One task worktree with build output.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub repo: String,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    /// Its build directories, with their size in bytes.
    pub dirs: Vec<(String, u64)>,
    pub bytes: u64,
    /// Why it is left alone; None when slim may reclaim it.
    pub skip: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Reclaimed {
    pub bytes: u64,
    pub worktrees: usize,
    /// Directories that could not be removed (a file held open), with why.
    pub failed: Vec<String>,
}

/// The build directories of a repo's worktrees: the playbook's `[lanes] keep` when it sets one,
/// else `target` and `node_modules`. Read from the playbook's own TOML, so it holds whichever
/// nitely wrote the `[lanes]` table.
pub fn keep_dirs(repo_root: &Path) -> Vec<String> {
    let set = Playbook::path_for(repo_root)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|t| t.get("lanes")?.get("keep")?.as_array().cloned())
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().trim_end_matches('/').to_string())
            // A leading slash anchors the entry at the root (`/node_modules`, NIT-268); what follows it
            // must be plain all the same.
            .filter(|s| plain(s.strip_prefix('/').unwrap_or(s)))
            .collect::<Vec<_>>());
    match set {
        Some(dirs) if !dirs.is_empty() => dirs,
        _ => DEFAULT_KEEP.iter().map(|s| s.to_string()).collect(),
    }
}

/// A directory name inside a worktree and nothing else: relative, of plain names only, so no
/// `.`, `..`, root, drive or absolute path can make slim delete the worktree or a folder outside it.
fn plain(dir: &str) -> bool {
    let p = Path::new(dir);
    // A drive or a backslash path is never a build directory, on any OS: the same playbook must
    // mean the same thing on Windows, macOS and Linux.
    !dir.is_empty() && !dir.contains([':', '\\']) && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Whether git ignores `dir` in `worktree`: build output is ignored, and slim never deletes a
/// directory that holds anything git tracks or would show as new.
fn ignored(worktree: &Path, dir: &str) -> bool {
    plain(dir) && git(worktree, &["check-ignore", "-q", &format!("{dir}/")]).is_ok() && git(worktree, &["ls-files", "--", dir]).is_ok_and(|t| t.trim().is_empty())
}

/// The repo's task worktrees with build output, each with whether slim may reclaim it.
pub fn plan_repo(name: &str, repo_root: &Path, recent: &Recent) -> Result<Vec<Candidate>> {
    let keep = keep_dirs(repo_root);
    let pb = Playbook::load(repo_root).unwrap_or_default();
    let base = pb.integration_branch().to_string();
    let mut out = Vec::new();
    // list_worktrees leaves out the reference tree and the lanes.
    for wt in crate::git::list_worktrees(&repo_root.to_string_lossy())? {
        if wt.is_main || wt.prunable {
            continue;
        }
        let path = PathBuf::from(&wt.path);
        // A name keeps at any depth (every package's node_modules), a path from the root (NIT-268).
        let dirs: Vec<(String, u64)> = crate::lanes::kept_dirs(&path, &keep).into_iter().filter(|d| ignored(&path, d)).map(|d| (d.clone(), size(&path.join(&d)))).collect();
        if dirs.is_empty() {
            continue;
        }
        let bytes = dirs.iter().map(|(_, b)| b).sum();
        let skip = at_work(recent, &wt.path, wt.branch.as_deref()).or_else(|| why_kept(&path, wt.branch.as_deref(), &wt.head, &base));
        out.push(Candidate { repo: name.to_string(), worktree: path, branch: wt.branch.clone(), dirs, bytes, skip });
    }
    Ok(out)
}

/// Why the worktree at `path` on `branch` keeps its build output while an agent is at work: a
/// session in it or in a folder inside it, or activity on the issue its branch names.
fn at_work(recent: &Recent, path: &str, branch: Option<&str>) -> Option<String> {
    let at = crate::paths::normalize(path);
    if recent.folders.iter().any(|r| r == &at || r.starts_with(&format!("{at}/"))) {
        return Some("an agent worked in it within the hour".into());
    }
    let key = branch.and_then(|b| crate::git::parse_branch(b).1).map(|k| k.to_uppercase());
    key.filter(|k| recent.issues.contains(k)).map(|k| format!("{k} had activity within the hour"))
}

/// The signs of agents at work within `RECENT_SECS`; none when the database cannot be read.
pub fn recent(db: Option<&crate::Db>) -> Recent {
    let Some(db) = db else { return Recent::default() };
    let since = crate::db::now() - RECENT_SECS;
    let folders = db.sessions_since(since).unwrap_or_default().into_iter().filter(|s| s.vendor != "human").map(|s| crate::paths::normalize(&s.cwd)).collect();
    let issues = db
        .conn
        .prepare("SELECT DISTINCT upper(issue_key) FROM event WHERE ts >= ?1 AND issue_key IS NOT NULL")
        .and_then(|mut stmt| stmt.query_map([since], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default();
    Recent { folders, issues }
}

/// Every configured repo's candidates, or only those of `project`'s repos.
pub fn plan(config: &crate::Config, db: Option<&crate::Db>, project: Option<&str>) -> Result<Vec<Candidate>> {
    let recent = recent(db);
    let mut out = Vec::new();
    for repo in &config.repos {
        if project.is_some_and(|p| !repo.project.as_deref().is_some_and(|rp| rp.eq_ignore_ascii_case(p))) {
            continue;
        }
        out.extend(plan_repo(&repo.name, Path::new(&repo.root), &recent)?);
    }
    Ok(out)
}

/// Why a worktree keeps its build output: its agent's work is not finished or something uses it.
fn why_kept(worktree: &Path, branch: Option<&str>, head: &str, base: &str) -> Option<String> {
    let status = git(worktree, &["status", "--porcelain", "--untracked-files=normal"])
        .map(|s| !s.trim().is_empty())
        .map_err(|e| e.to_string().lines().next().unwrap_or("").to_string());
    let within = |rev: &str| git(worktree, &["merge-base", "--is-ancestor", head, rev]).is_ok();
    let exists = |rev: &str| git(worktree, &["rev-parse", "--verify", "--quiet", rev]).is_ok();
    let finished = |branch: &str| {
        let landed = [base.to_string(), format!("origin/{base}")].iter().any(|b| exists(b) && within(b));
        landed || (exists(&format!("refs/remotes/origin/{branch}")) && within(&format!("refs/remotes/origin/{branch}")))
    };
    let users = || crate::procs::holders(worktree).iter().map(|h| format!("{} ({})", h.name, h.pid)).collect();
    kept_because(status, branch, finished, users)
}

/// Why slim keeps a worktree's build output, or None to reclaim it: its status (`Ok(dirty)`, or
/// why git could not read it), its branch, whether that branch is landed or pushed (read only for
/// a clean worktree on a branch), and what uses it (read last).
fn kept_because(status: Result<bool, String>, branch: Option<&str>, finished: impl FnOnce(&str) -> bool, users: impl FnOnce() -> Vec<String>) -> Option<String> {
    match status {
        Ok(true) => return Some("it has uncommitted changes".into()),
        Err(e) => return Some(format!("git could not read it: {e}")),
        Ok(false) => {}
    }
    let Some(branch) = branch else { return Some("it is not on a branch".into()) };
    if !finished(branch) {
        return Some(format!("{branch} is neither pushed nor landed"));
    }
    let users = users();
    (!users.is_empty()).then(|| format!("in use by {}", users.join(", ")))
}

/// Removes the build directories of the candidates slim may reclaim. Pass a plan made just now
/// (`plan` again after asking), so what changed while the person decided counts.
pub fn reclaim(candidates: &[Candidate]) -> Reclaimed {
    let mut out = Reclaimed::default();
    for c in candidates.iter().filter(|c| c.skip.is_none()) {
        let mut any = false;
        for (dir, bytes) in &c.dirs {
            let path = c.worktree.join(dir);
            // A link among the build output goes as a link, never through it (NIT-460).
            match crate::unlink::remove_tree(&path) {
                Ok(()) => {
                    out.bytes += bytes;
                    any = true;
                }
                Err(e) => {
                    // Part of it may be gone: count what is.
                    out.bytes += bytes.saturating_sub(size(&path));
                    out.failed.push(format!("{}: {e:#}", path.display()));
                }
            }
        }
        out.worktrees += usize::from(any);
    }
    out
}

/// Bytes under `path`, not following links (a junction in node_modules points elsewhere).
pub fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|entries| entries.flatten().map(|e| size(&e.path())).sum()).unwrap_or(0)
}

/// A size as people read it: "6.5 GB", "312 MB", "18 KB".
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 10.0 || unit == 0 { format!("{v:.0} {}", UNITS[unit]) } else { format!("{v:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let out = crate::playbook::run_git(Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn built(wt: &Path, dir: &str) {
        std::fs::create_dir_all(wt.join(dir).join("debug")).unwrap();
        std::fs::write(wt.join(dir).join("debug/app.exe"), vec![0u8; 4096]).unwrap();
    }

    #[test]
    fn reclaims_only_finished_worktrees_and_says_why_it_keeps_the_rest() {
        let base = crate::paths::unique_temp("nitely-slim", "");
        let (repo, origin) = (base.join("repo"), base.join("origin.git"));
        std::fs::create_dir_all(&repo).unwrap();
        sh(&base, &["init", "-q", "--bare", &origin.to_string_lossy()]);
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(".gitignore"), "target/\nnode_modules/\nbuild/\n").unwrap();
        sh(&repo, &["add", "-A"]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        sh(&repo, &["remote", "add", "origin", &origin.to_string_lossy()]);
        sh(&repo, &["push", "-q", "origin", "main"]);
        let wt = |name: &str| {
            let p = base.join("wt").join(name);
            sh(&repo, &["worktree", "add", "-q", "-b", &format!("claude/{name}"), &p.to_string_lossy(), "main"]);
            std::fs::write(p.join(format!("{name}.txt")), name).unwrap();
            sh(&p, &["add", "-A"]);
            sh(&p, &["commit", "-q", "-m", name]);
            built(&p, "target");
            p
        };
        let landed = wt("nit-1-landed");
        sh(&repo, &["merge", "-q", "--ff-only", "claude/nit-1-landed"]);
        let pushed = wt("nit-2-pushed");
        built(&pushed, "node_modules");
        sh(&pushed, &["push", "-q", "origin", "claude/nit-2-pushed"]);
        let unpushed = wt("nit-3-unpushed");
        let dirty = wt("nit-4-dirty");
        sh(&dirty, &["push", "-q", "origin", "claude/nit-4-dirty"]);
        std::fs::write(dirty.join("half.rs"), "fn half() {}").unwrap();
        let clean = base.join("wt").join("nit-5-none");
        sh(&repo, &["worktree", "add", "-q", "-b", "claude/nit-5-none", &clean.to_string_lossy(), "main"]);
        // The reference tree is not a task worktree, whatever it holds.
        let reference = base.join("wt").join(crate::light::LANES_DIR).join("repo").join("ref");
        sh(&repo, &["worktree", "add", "-q", "--detach", &reference.to_string_lossy(), "main"]);
        built(&reference, "target");

        // An agent at work on its issue keeps it (an_agent_at_work_keeps_a_worktree has the rest).
        let plan = plan_repo("repo", &repo, &Recent { folders: vec![], issues: vec!["NIT-2".into()] }).unwrap();
        let skip_of = |plan: &[Candidate]| plan.iter().find(|c| c.branch.as_deref() == Some("claude/nit-2-pushed")).unwrap().skip.clone();
        assert_eq!(skip_of(&plan).as_deref(), Some("NIT-2 had activity within the hour"));

        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        let by = |p: &Path| plan.iter().find(|c| crate::paths::normalize(&c.worktree.to_string_lossy()) == crate::paths::normalize(&p.to_string_lossy()));
        assert_eq!(plan.len(), 4, "only worktrees with build output: {plan:#?}");
        assert_eq!(by(&landed).unwrap().skip, None);
        assert_eq!((by(&pushed).unwrap().skip.clone(), by(&pushed).unwrap().dirs.len()), (None, 2));
        assert_eq!(by(&unpushed).unwrap().skip.as_deref(), Some("claude/nit-3-unpushed is neither pushed nor landed"));
        assert_eq!(by(&dirty).unwrap().skip.as_deref(), Some("it has uncommitted changes"));
        assert_eq!(by(&pushed).unwrap().bytes, 8192);

        let r = reclaim(&plan);
        assert_eq!((r.worktrees, r.bytes, r.failed.len()), (2, 3 * 4096, 0));
        assert!(!landed.join("target").exists() && !pushed.join("node_modules").exists() && pushed.join("nit-2-pushed.txt").exists());
        assert!(unpushed.join("target").exists() && dirty.join("target").exists() && reference.join("target").exists());

        // A playbook that names its build directories is read, whichever nitely wrote it.
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        std::fs::write(repo.join(crate::playbook::PLAYBOOK_PATH), "[lanes]\nkeep = [\"build\"]\n").unwrap();
        built(&landed, "build");
        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        assert_eq!(plan.iter().map(|c| c.dirs[0].0.as_str()).collect::<Vec<_>>(), ["build"], "target is not a build directory now");
        // Only what git ignores is ever build output: src is named here, but tracked.
        std::fs::write(repo.join(crate::playbook::PLAYBOOK_PATH), "[lanes]\nkeep = [\".\", \"..\", \"/tmp\", \"C:/Code\", \"a\\\\b\", \"src\", \"build\"]\n").unwrap();
        std::fs::create_dir_all(landed.join("src")).unwrap();
        std::fs::write(landed.join("src/kept.rs"), "work").unwrap();
        sh(&landed, &["add", "-A"]);
        sh(&landed, &["commit", "-q", "-m", "src"]);
        sh(&repo, &["merge", "-q", "--ff-only", "claude/nit-1-landed"]);
        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        assert!(plan.iter().all(|c| c.dirs.iter().all(|(d, _)| d == "build")), "src is tracked, so never build output: {plan:#?}");

        // A name reaches every package's build directory; an anchored one only the root's (NIT-268).
        built(&landed, "pkg/build");
        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        let landed_dirs = |plan: &[Candidate]| plan.iter().find(|c| crate::paths::normalize(&c.worktree.to_string_lossy()) == crate::paths::normalize(&landed.to_string_lossy())).map(|c| c.dirs.iter().map(|(d, _)| d.clone()).collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(landed_dirs(&plan), ["build", "pkg/build"], "{plan:#?}");
        std::fs::write(repo.join(crate::playbook::PLAYBOOK_PATH), "[lanes]\nkeep = [\"/build\"]\n").unwrap();
        assert_eq!(landed_dirs(&plan_repo("repo", &repo, &Recent::default()).unwrap()), ["build"]);
        std::fs::remove_dir_all(base).ok();
    }

    /// What a playbook names as build directories: plain names inside the worktree only, a
    /// leading slash anchoring one at the root, the defaults when it names none. No git involved.
    #[test]
    fn keep_dirs_are_plain_names_inside_the_worktree() {
        let repo = crate::paths::unique_temp("nitely-slim-keep", "");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let keep = |text: &str| {
            std::fs::write(repo.join(crate::playbook::PLAYBOOK_PATH), text).unwrap();
            keep_dirs(&repo)
        };
        assert_eq!(keep(""), DEFAULT_KEEP);
        assert_eq!(keep("[lanes]\nkeep = [\"build\"]\n"), ["build"], "read whichever nitely wrote it");
        assert_eq!(keep("[lanes]\nkeep = [\".\", \"..\", \"/tmp\", \"C:/Code\", \"a\\\\b\", \"src\", \"build\"]\n"), ["/tmp", "src", "build"]);
        assert_eq!(keep("[lanes]\nkeep = [\"/build\"]\n"), ["/build"], "kept, not dropped for the defaults");
        assert_eq!(human(3 * 4096), "12 KB");
        assert_eq!(human(7_000_000_000), "6.5 GB");
        std::fs::remove_dir_all(repo).ok();
    }

    /// The keep rules `reclaims_only_finished_worktrees_and_says_why_it_keeps_the_rest` checks with
    /// six real worktrees, and the order their facts are read in.
    #[test]
    fn a_worktree_keeps_its_build_output_until_it_is_clean_finished_and_unused() {
        let unread = |_: &str| -> bool { panic!("read only for a clean worktree on a branch") };
        let nobody = || -> Vec<String> { panic!("read last") };
        assert_eq!(kept_because(Ok(true), Some("b"), unread, nobody).as_deref(), Some("it has uncommitted changes"));
        assert_eq!(kept_because(Err("locked".into()), Some("b"), unread, nobody).as_deref(), Some("git could not read it: locked"));
        assert_eq!(kept_because(Ok(false), None, unread, nobody).as_deref(), Some("it is not on a branch"));
        assert_eq!(kept_because(Ok(false), Some("b"), |_| false, nobody).as_deref(), Some("b is neither pushed nor landed"));
        assert_eq!(kept_because(Ok(false), Some("b"), |_| true, || vec!["node (12)".into()]).as_deref(), Some("in use by node (12)"));
        assert_eq!(kept_because(Ok(false), Some("b"), |_| true, Vec::new), None);
    }

    /// An agent at work in a worktree (its session's folder, or one inside it), or on its issue,
    /// keeps its build output; a folder that only shares the name's start does not.
    #[test]
    fn an_agent_at_work_keeps_a_worktree() {
        let wt = if cfg!(windows) { "C:/wt/nit-2-pushed" } else { "/wt/nit-2-pushed" };
        let folder = |p: &str| Recent { folders: vec![crate::paths::normalize(p)], issues: vec![] };
        let branch = Some("claude/nit-2-pushed");
        assert_eq!(at_work(&folder(&format!("{wt}/src")), wt, branch).as_deref(), Some("an agent worked in it within the hour"));
        assert_eq!(at_work(&folder(wt), wt, branch).as_deref(), Some("an agent worked in it within the hour"));
        assert_eq!(at_work(&folder(&format!("{wt}-other")), wt, branch), None);
        let on_issue = Recent { folders: vec![], issues: vec!["NIT-2".into()] };
        assert_eq!(at_work(&on_issue, wt, branch).as_deref(), Some("NIT-2 had activity within the hour"));
        assert_eq!(at_work(&on_issue, wt, None), None);
        assert_eq!(at_work(&on_issue, wt, Some("claude/nit-3-other")), None);
    }
}
