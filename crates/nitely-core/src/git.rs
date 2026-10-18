//! Reads worktree and branch state by running the user's own `git`, so sparse checkouts,
//! `core.autocrlf` and credential config behave exactly as they do in their terminal.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::process::Command;

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub path: String,
    pub head: String,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
    pub is_main: bool,
    pub locked: bool,
    pub prunable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchState {
    pub ahead: u32,
    pub behind: u32,
    pub dirty_files: u32,
    pub last_commit_subject: Option<String>,
    pub last_commit_at: Option<i64>,
    /// A rebase is stopped in this worktree (land hit a conflict).
    pub rebase_in_progress: bool,
    /// When that rebase began (unix seconds), from its state folder in the git dir: how long the
    /// conflict has waited, which the branch's last commit does not say.
    pub rebase_started_at: Option<i64>,
}

/// Whether a rebase is stopped in the worktree whose git dir is `git_dir`, and since when: git
/// keeps its state in `rebase-merge` (or `rebase-apply`) for as long as the rebase lasts.
fn rebase_in(git_dir: &std::path::Path) -> (bool, Option<i64>) {
    for name in ["rebase-merge", "rebase-apply"] {
        if let Ok(meta) = std::fs::metadata(git_dir.join(name)) {
            let at = meta.created().or_else(|_| meta.modified()).ok();
            let secs = at.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
            return (true, secs);
        }
    }
    (false, None)
}

fn git(dir: &str, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = crate::playbook::run_git(&mut cmd).with_context(|| format!("running git in {dir}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {dir}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repo's worktrees, without what Nitely keeps beside them for itself (the reference tree and
/// build lanes under `.nitely-lanes`), which are nobody's work.
pub fn list_worktrees(repo_root: &str) -> Result<Vec<Worktree>> {
    let all = parse_worktrees(&git(repo_root, &["worktree", "list", "--porcelain"])?);
    Ok(all.into_iter().filter(|w| !crate::light::is_lane_path(&w.path)).collect())
}

/// Parses `git worktree list --porcelain`. The first entry is the main worktree.
pub fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    for block in text.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let mut wt = Worktree {
            path: String::new(),
            head: String::new(),
            branch: None,
            is_main: out.is_empty(),
            locked: false,
            prunable: false,
        };
        for line in block.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => wt.path = value.to_string(),
                "HEAD" => wt.head = value.to_string(),
                "branch" => wt.branch = Some(value.trim_start_matches("refs/heads/").to_string()),
                "locked" => wt.locked = true,
                "prunable" => wt.prunable = true,
                _ => {}
            }
        }
        if !wt.path.is_empty() {
            out.push(wt);
        }
    }
    out
}

pub fn branch_state(worktree: &str, integration_branch: &str) -> Result<BranchState> {
    let (ahead, behind) = git(
        worktree,
        &["rev-list", "--left-right", "--count", &format!("HEAD...{integration_branch}")],
    )
    .ok()
    .and_then(|s| {
        let mut it = s.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(0));
        Some((it.next()?, it.next()?))
    })
    .unwrap_or((0, 0));

    let dirty_files = dirty_files(worktree)?;

    let log = git(worktree, &["log", "-1", "--format=%s%x00%ct"]).unwrap_or_default();
    let mut parts = log.trim_end().splitn(2, '\0');
    let last_commit_subject = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
    let last_commit_at = parts.next().and_then(|t| t.parse().ok());

    let git_dir = git(worktree, &["rev-parse", "--absolute-git-dir"]).unwrap_or_default();
    let git_dir = std::path::Path::new(git_dir.trim());
    let (rebase_in_progress, rebase_started_at) = rebase_in(git_dir);

    Ok(BranchState {
        ahead,
        behind,
        dirty_files,
        last_commit_subject,
        last_commit_at,
        rebase_in_progress,
        rebase_started_at,
    })
}

/// The worktree's git dir without running git: `.git` is the dir itself in a main checkout, and a
/// file holding `gitdir: <path>` in a linked worktree.
/// What moves when a worktree's HEAD or index does (a checkout, a commit, a `git add`, a rebase):
/// the times of its gitdir, its HEAD and its index. Edits not yet added move none of them. None
/// when it has no gitdir to read.
pub fn worktree_fingerprint(worktree: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let dir = git_dir_of(worktree)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for path in [dir.clone(), dir.join("HEAD"), dir.join("index")] {
        let meta = std::fs::metadata(&path).ok();
        meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).hash(&mut h);
        meta.map(|m| m.len()).hash(&mut h);
    }
    Some(h.finish())
}

/// What moves when `branch` of the repo at `root` does: the times of its loose ref and of
/// packed-refs (where git keeps refs it packed).
pub fn ref_fingerprint(root: &str, branch: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let git = std::path::Path::new(root).join(".git");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for path in [git.join("refs").join("heads").join(branch), git.join("packed-refs")] {
        let meta = std::fs::metadata(&path).ok();
        meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).hash(&mut h);
        meta.map(|m| m.len()).hash(&mut h);
    }
    h.finish()
}

pub fn git_dir_of(worktree: &str) -> Option<std::path::PathBuf> {
    let dot_git = std::path::Path::new(worktree).join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let path = std::path::Path::new(path);
    Some(if path.is_absolute() { path.to_path_buf() } else { std::path::Path::new(worktree).join(path) })
}

/// A worktree's state at the cost of one git process: ahead/behind and the last commit come from
/// `branch`, the branch's line from the repo's one `for-each-ref` call (`list_branches`), and the
/// rebase check reads the git dir. Without counts for the branch (a detached HEAD, an older git),
/// it asks git for them as `branch_state` does.
pub fn worktree_state(worktree: &str, integration_branch: &str, branch: Option<&BranchRef>) -> Result<BranchState> {
    worktree_state_with(worktree, integration_branch, branch, None)
}

/// `worktree_state`, with its count of uncommitted files given when an earlier read is still
/// current (`worktree_fingerprint` did not move): then it runs no git at all (NIT-339).
pub fn worktree_state_with(worktree: &str, integration_branch: &str, branch: Option<&BranchRef>, dirty: Option<u32>) -> Result<BranchState> {
    let counted = branch.and_then(|b| Some((b.ahead?, b.behind?, b)));
    let Some((ahead, behind, b)) = counted else { return branch_state(worktree, integration_branch) };
    let dirty_files = match dirty {
        Some(n) => n,
        None => dirty_files(worktree)?,
    };
    let (rebase_in_progress, rebase_started_at) = match git_dir_of(worktree) {
        Some(dir) => rebase_in(&dir),
        None => {
            let asked = branch_state(worktree, integration_branch)?;
            (asked.rebase_in_progress, asked.rebase_started_at)
        }
    };
    Ok(BranchState {
        ahead,
        behind,
        dirty_files,
        last_commit_subject: b.last_commit_subject.clone(),
        last_commit_at: b.last_commit_at,
        rebase_in_progress,
        rebase_started_at,
    })
}

fn dirty_files(worktree: &str) -> Result<u32> {
    Ok(git(worktree, &["status", "--porcelain", "--ignore-submodules=dirty"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count() as u32)
}

/// A local branch: where it stands against the integration branch and when it last moved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BranchRef {
    pub name: String,
    /// `None` when this git cannot count them (older than 2.41) or the integration branch is missing.
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub last_commit_subject: Option<String>,
    pub last_commit_at: Option<i64>,
}

/// Every local branch, most recently committed first, with ahead/behind counts against
/// `integration_branch` from one `for-each-ref` call.
pub fn list_branches(repo_root: &str, integration_branch: &str) -> Result<Vec<BranchRef>> {
    let counted = format!(
        "--format=%(refname:short)%00%(committerdate:unix)%00%(ahead-behind:{integration_branch})%00%(subject)"
    );
    let text = match git(repo_root, &["for-each-ref", "--sort=-committerdate", &counted, "refs/heads"]) {
        Ok(text) => text,
        // An older git, or no integration branch to count against: list them without counts.
        Err(_) => git(
            repo_root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)%00%(committerdate:unix)%00%00%(subject)",
                "refs/heads",
            ],
        )?,
    };
    Ok(parse_branch_refs(&text))
}

/// Parses lines of `name NUL unix-time NUL "ahead behind" (or empty) NUL subject`.
pub fn parse_branch_refs(text: &str) -> Vec<BranchRef> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(4, ' ');
            let name = f.next()?.trim();
            if name.is_empty() {
                return None;
            }
            let at = f.next().and_then(|t| t.trim().parse().ok());
            let mut counts = f.next().unwrap_or("").split_whitespace().map(|n| n.parse::<u32>().ok());
            let (ahead, behind) = (counts.next().flatten(), counts.next().flatten());
            let subject = f.next().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
            Some(BranchRef {
                name: name.to_string(),
                ahead,
                behind,
                last_commit_subject: subject,
                last_commit_at: at,
            })
        })
        .collect()
}

/// The main checkout of the repo `dir` belongs to, also from inside a linked worktree.
pub fn main_root(dir: &str) -> Option<String> {
    let common = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).ok()?;
    let common = std::path::Path::new(common.trim());
    let root = if common.file_name().is_some_and(|n| n == ".git") { common.parent()? } else { common };
    Some(root.to_string_lossy().into_owned())
}

/// `workspace/repo` when the repo's `origin` is on Bitbucket.
pub fn bitbucket_slug(repo_root: &str) -> Option<String> {
    parse_bitbucket_url(git(repo_root, &["remote", "get-url", "origin"]).ok()?.trim())
}

/// `owner/repo` when the repo's `origin` is on GitHub.
pub fn github_slug(repo_root: &str) -> Option<String> {
    parse_host_url(git(repo_root, &["remote", "get-url", "origin"]).ok()?.trim(), "github.com")
}

/// Accepts `https://[user@]bitbucket.org/ws/repo[.git]` and `git@bitbucket.org:ws/repo[.git]`.
pub fn parse_bitbucket_url(url: &str) -> Option<String> {
    parse_host_url(url, "bitbucket.org")
}

fn parse_host_url(url: &str, host: &str) -> Option<String> {
    let rest = url.split_once(host)?.1.trim_start_matches([':', '/']);
    let mut parts = rest.trim_end_matches('/').trim_end_matches(".git").splitn(3, '/');
    let (ws, repo) = (parts.next()?, parts.next()?);
    (!ws.is_empty() && !repo.is_empty()).then(|| format!("{ws}/{repo}"))
}

/// Splits `claude/app-123-fix-sidebar` into vendor `claude` and issue key `APP-123`.
pub fn parse_branch(branch: &str) -> (Option<String>, Option<String>) {
    let (vendor, rest) = match branch.split_once('/') {
        Some((v, r)) => (Some(v.to_string()), r),
        None => (None, branch),
    };
    let mut parts = rest.split('-');
    let key = match (parts.next(), parts.next()) {
        (Some(prefix), Some(num))
            if !prefix.is_empty()
                && prefix.chars().all(|c| c.is_ascii_alphabetic())
                && !num.is_empty()
                && num.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(format!("{}-{}", prefix.to_uppercase(), num))
        }
        _ => None,
    };
    (vendor, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_state_from_the_branch_line_matches_asking_git() {
        let dir = crate::paths::unique_temp("nitely-wtstate", "");
        let main = dir.join("main");
        let wt = dir.join("wt");
        std::fs::create_dir_all(&main).unwrap();
        let sh = |at: &std::path::Path, args: &[&str]| {
            let o = crate::playbook::run_git(Command::new("git").arg("-C").arg(at).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&main, &["init", "-q", "-b", "main"]);
        sh(&main, &["commit", "-q", "--allow-empty", "-m", "one"]);
        sh(&main, &["worktree", "add", "-q", "-b", "claude/nit-5-x", wt.to_str().unwrap()]);
        sh(&wt, &["commit", "-q", "--allow-empty", "-m", "on the branch"]);
        sh(&main, &["commit", "-q", "--allow-empty", "-m", "on main"]);
        std::fs::write(wt.join("dirty.txt"), "x").unwrap();

        let root = main.to_str().unwrap();
        let path = wt.to_str().unwrap();
        let branches = list_branches(root, "main").unwrap();
        let line = branches.iter().find(|b| b.name == "claude/nit-5-x");
        let asked = branch_state(path, "main").unwrap();
        let read = worktree_state(path, "main", line).unwrap();
        assert_eq!((read.ahead, read.behind, read.dirty_files), (asked.ahead, asked.behind, asked.dirty_files));
        assert_eq!((read.ahead, read.behind, read.dirty_files), (1, 1, 1));
        assert_eq!(read.last_commit_subject, asked.last_commit_subject);
        assert_eq!(read.last_commit_at, asked.last_commit_at);
        assert!(!read.rebase_in_progress);

        // A stopped rebase, found from the worktree's .git file without asking git.
        let git_dir = git_dir_of(path).unwrap();
        assert!(git_dir.join("HEAD").exists(), "{}", git_dir.display());
        assert_eq!(read.rebase_started_at, None);
        std::fs::create_dir_all(git_dir.join("rebase-merge")).unwrap();
        let stopped = worktree_state(path, "main", line).unwrap();
        assert!(stopped.rebase_in_progress);
        assert!(branch_state(path, "main").unwrap().rebase_in_progress);
        // Since when: the rebase's folder, just made, not the branch's last commit.
        let since = stopped.rebase_started_at.expect("when the rebase began");
        assert!((crate::db::now() - since).abs() < 60, "{since}");
        assert_eq!(branch_state(path, "main").unwrap().rebase_started_at, Some(since));
        // Without the branch's line (a detached HEAD) it asks git, with the same answer.
        assert_eq!(worktree_state(path, "main", None).unwrap().ahead, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_porcelain() {
        let text = "worktree C:/Code/my-app\nHEAD abc\nbranch refs/heads/develop\n\n\
                    worktree C:/Code/my-app-worktrees/app-1-x\nHEAD def\nbranch refs/heads/claude/app-1-x\nlocked\n\n\
                    worktree C:/tmp/detached\nHEAD 123\ndetached\n";
        let w = parse_worktrees(text);
        assert_eq!(w.len(), 3);
        assert!(w[0].is_main && !w[1].is_main);
        assert_eq!(w[1].branch.as_deref(), Some("claude/app-1-x"));
        assert!(w[1].locked);
        assert_eq!(w[2].branch, None);
    }

    #[test]
    fn parses_bitbucket_remotes() {
        let want = Some("acme/my-app".to_string());
        assert_eq!(parse_bitbucket_url("https://sam@bitbucket.org/acme/my-app.git"), want);
        assert_eq!(parse_bitbucket_url("git@bitbucket.org:acme/my-app.git"), want);
        assert_eq!(parse_bitbucket_url("https://github.com/x/y.git"), None);
        assert_eq!(parse_host_url("https://github.com/frontiermaxxing/Nitely", "github.com"), Some("frontiermaxxing/Nitely".into()));
    }

    #[test]
    fn parses_branch_refs() {
        let text = "claude/nit-3-x 1792000000 2 5 Fix the thing
                    main 1792000100 0 0 Land it
                    old   
";
        let b = parse_branch_refs(text);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].name, "claude/nit-3-x");
        assert_eq!((b[0].ahead, b[0].behind), (Some(2), Some(5)));
        assert_eq!(b[0].last_commit_subject.as_deref(), Some("Fix the thing"));
        assert_eq!(b[0].last_commit_at, Some(1792000000));
        assert_eq!((b[2].ahead, b[2].behind, b[2].last_commit_at), (None, None, None));
        assert_eq!(b[2].last_commit_subject, None);
    }

    #[test]
    fn lists_branches_with_counts() {
        let dir = crate::paths::unique_temp("nitely-branches", "");
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| {
            let o = crate::playbook::run_git(Command::new("git").arg("-C").arg(&dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).unwrap();
            assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "one"]);
        sh(&["branch", "claude/nit-9-side"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "two"]);
        sh(&["checkout", "-q", "claude/nit-9-side"]);
        sh(&["commit", "-q", "--allow-empty", "-m", "side work"]);
        let root = dir.to_string_lossy().into_owned();
        let branches = list_branches(&root, "main").unwrap();
        let side = branches.iter().find(|b| b.name == "claude/nit-9-side").unwrap();
        assert_eq!(side.last_commit_subject.as_deref(), Some("side work"));
        // A git older than 2.41 lists the branch without counts; a newer one counts 1 ahead, 1 behind.
        assert!(side.ahead.is_none() || (side.ahead, side.behind) == (Some(1), Some(1)));
        assert!(branches.iter().any(|b| b.name == "main"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_branch_names() {
        assert_eq!(
            parse_branch("claude/app-123-fix-sidebar"),
            (Some("claude".into()), Some("APP-123".into()))
        );
        assert_eq!(parse_branch("develop"), (None, None));
        assert_eq!(parse_branch("codex/refactor"), (Some("codex".into()), None));
    }
}
