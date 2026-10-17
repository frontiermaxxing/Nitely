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
        .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| plain(s)).collect::<Vec<_>>());
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
        let dirs: Vec<(String, u64)> = keep.iter().filter(|d| path.join(d).is_dir() && ignored(&path, d)).map(|d| (d.clone(), size(&path.join(d)))).collect();
        if dirs.is_empty() {
            continue;
        }
        let bytes = dirs.iter().map(|(_, b)| b).sum();
        let at = crate::paths::normalize(&wt.path);
        let in_it = recent.folders.iter().any(|r| r == &at || r.starts_with(&format!("{at}/")));
        let key = wt.branch.as_deref().and_then(|b| crate::git::parse_branch(b).1).map(|k| k.to_uppercase());
        let skip = if in_it {
            Some("an agent worked in it within the hour".into())
        } else if let Some(k) = key.filter(|k| recent.issues.contains(k)) {
            Some(format!("{k} had activity within the hour"))
        } else {
            why_kept(&path, wt.branch.as_deref(), &wt.head, &base)
        };
        out.push(Candidate { repo: name.to_string(), worktree: path, branch: wt.branch.clone(), dirs, bytes, skip });
    }
    Ok(out)
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
    match git(worktree, &["status", "--porcelain", "--untracked-files=normal"]) {
        Ok(s) if !s.trim().is_empty() => return Some("it has uncommitted changes".into()),
        Err(e) => return Some(format!("git could not read it: {}", e.to_string().lines().next().unwrap_or(""))),
        Ok(_) => {}
    }
    let Some(branch) = branch else { return Some("it is not on a branch".into()) };
    let within = |rev: &str| git(worktree, &["merge-base", "--is-ancestor", head, rev]).is_ok();
    let exists = |rev: &str| git(worktree, &["rev-parse", "--verify", "--quiet", rev]).is_ok();
    let landed = [base.to_string(), format!("origin/{base}")].iter().any(|b| exists(b) && within(b));
    let pushed = exists(&format!("refs/remotes/origin/{branch}")) && within(&format!("refs/remotes/origin/{branch}"));
    if !landed && !pushed {
        return Some(format!("{branch} is neither pushed nor landed"));
    }
    let held = crate::procs::holders(worktree);
    if !held.is_empty() {
        let names: Vec<String> = held.iter().map(|h| format!("{} ({})", h.name, h.pid)).collect();
        return Some(format!("in use by {}", names.join(", ")));
    }
    None
}

/// Removes the build directories of the candidates slim may reclaim. Pass a plan made just now
/// (`plan` again after asking), so what changed while the person decided counts.
pub fn reclaim(candidates: &[Candidate]) -> Reclaimed {
    let mut out = Reclaimed::default();
    for c in candidates.iter().filter(|c| c.skip.is_none()) {
        let mut any = false;
        for (dir, bytes) in &c.dirs {
            let path = c.worktree.join(dir);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    out.bytes += bytes;
                    any = true;
                }
                Err(e) => {
                    // Part of it may be gone: count what is.
                    out.bytes += bytes.saturating_sub(size(&path));
                    out.failed.push(format!("{}: {e}", path.display()));
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

        // An agent at work in the pushed worktree (its session's folder, or one inside it), or on
        // its issue, keeps it.
        let working = Recent { folders: vec![crate::paths::normalize(&pushed.join("src").to_string_lossy())], issues: vec![] };
        let plan = plan_repo("repo", &repo, &working).unwrap();
        let skip_of = |plan: &[Candidate]| plan.iter().find(|c| c.branch.as_deref() == Some("claude/nit-2-pushed")).unwrap().skip.clone();
        assert_eq!(skip_of(&plan).as_deref(), Some("an agent worked in it within the hour"));
        let plan = plan_repo("repo", &repo, &Recent { folders: vec![], issues: vec!["NIT-2".into()] }).unwrap();
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
        assert_eq!(keep_dirs(&repo), ["build"]);
        built(&landed, "build");
        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        assert_eq!(plan.iter().map(|c| c.dirs[0].0.as_str()).collect::<Vec<_>>(), ["build"], "target is not a build directory now");
        // Only plain names inside the worktree, and only what git ignores, are ever build output.
        std::fs::write(repo.join(crate::playbook::PLAYBOOK_PATH), "[lanes]\nkeep = [\".\", \"..\", \"/tmp\", \"C:/Code\", \"a\\\\b\", \"src\", \"build\"]\n").unwrap();
        assert_eq!(keep_dirs(&repo), ["src", "build"]);
        std::fs::create_dir_all(landed.join("src")).unwrap();
        std::fs::write(landed.join("src/kept.rs"), "work").unwrap();
        sh(&landed, &["add", "-A"]);
        sh(&landed, &["commit", "-q", "-m", "src"]);
        sh(&repo, &["merge", "-q", "--ff-only", "claude/nit-1-landed"]);
        let plan = plan_repo("repo", &repo, &Recent::default()).unwrap();
        assert!(plan.iter().all(|c| c.dirs.iter().all(|(d, _)| d == "build")), "src is tracked, so never build output: {plan:#?}");
        assert_eq!(human(3 * 4096), "12 KB");
        assert_eq!(human(7_000_000_000), "6.5 GB");
        std::fs::remove_dir_all(base).ok();
    }
}
