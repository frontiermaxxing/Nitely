//! The "Now" view's data: every worktree of every configured repo, joined with the latest agent
//! session working inside it.

use anyhow::Result;
use serde::Serialize;

use std::collections::HashSet;

use crate::config::{Config, RepoConfig};
use crate::db::{now, Db, IssueRecord, PrRecord, Session, SyncStatus};
use crate::issues::Ask;
use crate::git::{self, BranchState};
use crate::paths::is_within;

/// Sessions not seen for this long are left off the board.
const SESSION_WINDOW_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub worktree: String,
    pub branch: Option<String>,
    pub is_main: bool,
    pub vendor: Option<String>,
    pub issue_key: Option<String>,
    pub state: Option<BranchState>,
    pub error: Option<String>,
    pub session: Option<Session>,
    pub issue: Option<IssueRecord>,
    /// The open PR for this branch, else its most recent merged or declined one.
    pub pr: Option<PrRecord>,
    /// A rebase is stopped, the agent is waiting on a human, or the issue waits on the human.
    pub needs_me: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub repos: Vec<RepoView>,
    /// Issues waiting on the human (Needs You, or Needs Andrew in Linear) with no worktree on the board.
    pub needs_me_issues: Vec<IssueRecord>,
    /// Agents' questions still waiting for an answer.
    pub asks: Vec<Ask>,
    pub sync: Vec<SyncStatus>,
    /// Agents on other machines reaching this one through `nitely serve` (cwd `remote:<token>`),
    /// which no repo here can place.
    pub remote_sessions: Vec<Session>,
}

/// A local branch with no worktree: left over after a land, parked, or made by hand.
#[derive(Debug, Clone, Serialize)]
pub struct BranchRow {
    pub name: String,
    pub vendor: Option<String>,
    pub issue_key: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub last_commit_subject: Option<String>,
    pub last_commit_at: Option<i64>,
    pub issue: Option<IssueRecord>,
    pub pr: Option<PrRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoView {
    pub name: String,
    pub root: String,
    pub integration_branch: String,
    /// The project (issue prefix) this repo's work belongs to, when the config names one.
    pub project: Option<String>,
    pub rows: Vec<TaskRow>,
    /// Local branches checked out in no worktree (the integration branch aside), newest first.
    pub branches: Vec<BranchRow>,
    pub error: Option<String>,
    /// Sessions under this repo's root that match no worktree (e.g. a scratch folder).
    pub unmatched_sessions: Vec<Session>,
}

pub fn snapshot(config: &Config, db: &Db) -> Result<Board> {
    let sessions = db.sessions_since(now() - SESSION_WINDOW_SECS)?;
    let issues = db.issues()?;
    let prs = db.prs()?;
    let mut views = Vec::new();

    for repo in &config.repos {
        let worktrees = match git::list_worktrees(&repo.root) {
            Ok(w) => w,
            Err(e) => {
                views.push(RepoView {
                    name: repo.name.clone(),
                    root: repo.root.clone(),
                    integration_branch: repo.integration_branch.clone(),
                    project: repo.project.clone(),
                    rows: vec![],
                    branches: vec![],
                    error: Some(e.to_string()),
                    unmatched_sessions: vec![],
                });
                continue;
            }
        };

        // Longest path first, so a session in a nested worktree is not claimed by its parent.
        let mut order: Vec<usize> = (0..worktrees.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(worktrees[i].path.len()));
        let mut claimed = vec![false; sessions.len()];
        let mut session_for = vec![None; worktrees.len()];
        for &i in &order {
            for (j, s) in sessions.iter().enumerate() {
                if !claimed[j] && is_within(&s.cwd, &worktrees[i].path) {
                    claimed[j] = true;
                    // `sessions` is newest first, so the first match is the latest.
                    if session_for[i].is_none() {
                        session_for[i] = Some(s.clone());
                    }
                }
            }
        }

        let slug = git::bitbucket_slug(&repo.root).or_else(|| git::github_slug(&repo.root));
        let rows = worktrees
            .iter()
            .zip(session_for)
            .map(|(wt, session)| {
                let (vendor, issue_key) = wt
                    .branch
                    .as_deref()
                    .map(git::parse_branch)
                    .unwrap_or((None, None));
                let (state, error) = match git::branch_state(&wt.path, &repo.integration_branch) {
                    Ok(s) => (Some(s), None),
                    Err(e) => (None, Some(e.to_string())),
                };
                let issue = issue_key
                    .as_ref()
                    .and_then(|k| issues.iter().find(|i| &i.key == k).cloned());
                let pr = wt.branch.as_deref().and_then(|b| pr_for(&prs, slug.as_deref(), b));
                let needs_me = state.as_ref().is_some_and(|s| s.rebase_in_progress)
                    || session.as_ref().is_some_and(|s| s.state == "waiting")
                    || issue.as_ref().is_some_and(|i| i.needs_human());
                TaskRow {
                    worktree: wt.path.clone(),
                    branch: wt.branch.clone(),
                    is_main: wt.is_main,
                    vendor: session.as_ref().map(|s| s.vendor.clone()).or(vendor),
                    issue_key,
                    state,
                    error,
                    session,
                    issue,
                    pr,
                    needs_me,
                }
            })
            .collect();

        let branches = loose_branches(repo, &worktrees, &issues, &prs, slug.as_deref());

        let unmatched_sessions = sessions
            .iter()
            .zip(&claimed)
            .filter(|(s, c)| !**c && is_within(&s.cwd, &repo.root))
            .map(|(s, _)| s.clone())
            .collect();

        views.push(RepoView {
            name: repo.name.clone(),
            root: repo.root.clone(),
            integration_branch: repo.integration_branch.clone(),
            project: repo.project.clone(),
            rows,
            branches,
            error: None,
            unmatched_sessions,
        });
    }
    let on_board: HashSet<&str> = views
        .iter()
        .flat_map(|v| v.rows.iter())
        .filter_map(|r| r.issue_key.as_deref())
        .collect();
    let needs_me_issues = issues
        .iter()
        .filter(|i| i.needs_human() && !on_board.contains(i.key.as_str()))
        .cloned()
        .collect();

    let remote_sessions = sessions.iter().filter(|s| s.cwd.starts_with("remote:")).cloned().collect();
    Ok(Board { repos: views, needs_me_issues, asks: db.open_asks()?, sync: db.sync_statuses()?, remote_sessions })
}

/// Branches no worktree has checked out. A repo whose branches cannot be listed shows none rather
/// than failing the whole board.
fn loose_branches(
    repo: &RepoConfig,
    worktrees: &[git::Worktree],
    issues: &[IssueRecord],
    prs: &[PrRecord],
    slug: Option<&str>,
) -> Vec<BranchRow> {
    let checked_out: HashSet<&str> = worktrees.iter().filter_map(|w| w.branch.as_deref()).collect();
    git::list_branches(&repo.root, &repo.integration_branch)
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.name != repo.integration_branch && !checked_out.contains(b.name.as_str()))
        .map(|b| {
            let (vendor, issue_key) = git::parse_branch(&b.name);
            let issue = issue_key.as_ref().and_then(|k| issues.iter().find(|i| &i.key == k).cloned());
            let pr = pr_for(prs, slug, &b.name);
            BranchRow {
                name: b.name,
                vendor,
                issue_key,
                ahead: b.ahead,
                behind: b.behind,
                last_commit_subject: b.last_commit_subject,
                last_commit_at: b.last_commit_at,
                issue,
                pr,
            }
        })
        .collect()
}

/// `prs` is newest first. Without a known remote, the branch name alone decides.
fn pr_for(prs: &[PrRecord], slug: Option<&str>, branch: &str) -> Option<PrRecord> {
    let matching: Vec<&PrRecord> = prs
        .iter()
        .filter(|p| p.branch == branch && slug.is_none_or(|s| p.repo.eq_ignore_ascii_case(s)))
        .collect();
    matching
        .iter()
        .find(|p| p.state == "OPEN")
        .or(matching.first())
        .map(|p| (*p).clone())
}
