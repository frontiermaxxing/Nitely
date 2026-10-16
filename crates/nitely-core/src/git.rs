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
    let out = cmd.output().with_context(|| format!("running git in {dir}"))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {dir}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn list_worktrees(repo_root: &str) -> Result<Vec<Worktree>> {
    Ok(parse_worktrees(&git(repo_root, &["worktree", "list", "--porcelain"])?))
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

    let dirty_files = git(worktree, &["status", "--porcelain", "--ignore-submodules=dirty"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count() as u32;

    let log = git(worktree, &["log", "-1", "--format=%s%x00%ct"]).unwrap_or_default();
    let mut parts = log.trim_end().splitn(2, '\0');
    let last_commit_subject = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
    let last_commit_at = parts.next().and_then(|t| t.parse().ok());

    let git_dir = git(worktree, &["rev-parse", "--absolute-git-dir"]).unwrap_or_default();
    let git_dir = std::path::Path::new(git_dir.trim());
    let rebase_in_progress =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();

    Ok(BranchState {
        ahead,
        behind,
        dirty_files,
        last_commit_subject,
        last_commit_at,
        rebase_in_progress,
    })
}

/// `workspace/repo` when the repo's `origin` is on Bitbucket.
pub fn bitbucket_slug(repo_root: &str) -> Option<String> {
    parse_bitbucket_url(git(repo_root, &["remote", "get-url", "origin"]).ok()?.trim())
}

/// Accepts `https://[user@]bitbucket.org/ws/repo[.git]` and `git@bitbucket.org:ws/repo[.git]`.
pub fn parse_bitbucket_url(url: &str) -> Option<String> {
    let rest = url.split_once("bitbucket.org")?.1.trim_start_matches([':', '/']);
    let mut parts = rest.trim_end_matches('/').trim_end_matches(".git").splitn(3, '/');
    let (ws, repo) = (parts.next()?, parts.next()?);
    (!ws.is_empty() && !repo.is_empty()).then(|| format!("{ws}/{repo}"))
}

/// Splits `claude/and-123-fix-sidebar` into vendor `claude` and issue key `AND-123`.
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
    fn parses_porcelain() {
        let text = "worktree C:/Development/AeraChromium\nHEAD abc\nbranch refs/heads/win-dev\n\n\
                    worktree C:/Development/AeraChromium-worktrees/and-1-x\nHEAD def\nbranch refs/heads/claude/and-1-x\nlocked\n\n\
                    worktree C:/tmp/detached\nHEAD 123\ndetached\n";
        let w = parse_worktrees(text);
        assert_eq!(w.len(), 3);
        assert!(w[0].is_main && !w[1].is_main);
        assert_eq!(w[1].branch.as_deref(), Some("claude/and-1-x"));
        assert!(w[1].locked);
        assert_eq!(w[2].branch, None);
    }

    #[test]
    fn parses_bitbucket_remotes() {
        let want = Some("quixet/aera-browser".to_string());
        assert_eq!(parse_bitbucket_url("https://andrew@bitbucket.org/quixet/aera-browser.git"), want);
        assert_eq!(parse_bitbucket_url("git@bitbucket.org:quixet/aera-browser.git"), want);
        assert_eq!(parse_bitbucket_url("https://github.com/x/y.git"), None);
    }

    #[test]
    fn parses_branch_names() {
        assert_eq!(
            parse_branch("claude/and-123-fix-sidebar"),
            (Some("claude".into()), Some("AND-123".into()))
        );
        assert_eq!(parse_branch("win-dev"), (None, None));
        assert_eq!(parse_branch("codex/refactor"), (Some("codex".into()), None));
    }
}
