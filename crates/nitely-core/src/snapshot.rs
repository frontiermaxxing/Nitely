//! The "Now" view's data: every worktree of every configured repo, joined with the latest agent
//! session working inside it.

use anyhow::Result;
use serde::Serialize;

use std::collections::HashSet;

use crate::config::Config;
use crate::db::{now, Db, IssueRecord, PrRecord, Session, SyncStatus};
use crate::git::{self, BranchState};
use crate::paths::is_within;

/// The Linear state that means the next step is Andrew's (`linear.mjs` vocabulary).
pub const NEEDS_ANDREW: &str = "Needs Andrew";

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
    /// A rebase is stopped, the agent is waiting on a human, or the issue is in Needs Andrew.
    pub needs_me: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub repos: Vec<RepoView>,
    /// Issues in Needs Andrew that have no worktree on the board.
    pub needs_me_issues: Vec<IssueRecord>,
    pub sync: Vec<SyncStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoView {
    pub name: String,
    pub root: String,
    pub integration_branch: String,
    pub rows: Vec<TaskRow>,
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
                    rows: vec![],
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

        let slug = git::bitbucket_slug(&repo.root);
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
                    || issue.as_ref().is_some_and(|i| i.state == NEEDS_ANDREW);
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
            rows,
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
        .filter(|i| i.state == NEEDS_ANDREW && !on_board.contains(i.key.as_str()))
        .cloned()
        .collect();

    Ok(Board { repos: views, needs_me_issues, sync: db.sync_statuses()? })
}

/// `prs` is newest first. Without a known remote, the branch name alone decides.
fn pr_for(prs: &[PrRecord], slug: Option<&str>, branch: &str) -> Option<PrRecord> {
    let matching: Vec<&PrRecord> = prs
        .iter()
        .filter(|p| p.branch == branch && slug.is_none_or(|s| p.repo == s))
        .collect();
    matching
        .iter()
        .find(|p| p.state == "OPEN")
        .or(matching.first())
        .map(|p| (*p).clone())
}
