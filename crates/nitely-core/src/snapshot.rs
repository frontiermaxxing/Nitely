//! The "Now" view's data: every worktree of every configured repo, joined with the latest agent
//! session working inside it, and every agent session on its own (`agents`), with the issue its own
//! work names. Several sessions often share a folder (every agent a coordinator starts runs from the
//! main checkout), so a folder is not an agent (NIT-273).

use anyhow::Result;
use serde::Serialize;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{Config, RepoConfig};
use crate::db::{now, Db, IssueRecord, PrRecord, Session, SyncStatus};
use crate::issues::Ask;
use crate::git::{self, BranchState};
use crate::paths::is_within;

/// Sessions not seen for this long are left off the board.
const SESSION_WINDOW_SECS: i64 = 24 * 3600;

/// How long a session's report or note keeps naming the issue it works on, when it has claimed,
/// started or submitted nothing since.
const REPORT_NAMES_ISSUE_SECS: i64 = 30 * 60;

/// One agent session, wherever it runs: the Overview's "Right now", Now's agents and the counts of
/// what is moving are made of these, one per session.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub session: Session,
    /// The repo it runs in, and that repo's project, when it is one of the configured repos.
    pub repo: Option<String>,
    pub project: Option<String>,
    /// The task worktree it runs in, when it runs in one rather than in a main checkout.
    pub worktree: Option<String>,
    /// The issue it works on: the one its latest claim, start_task or submit named, or its latest
    /// report or note while recent, else its worktree's issue.
    pub issue_key: Option<String>,
    pub issue: Option<IssueRecord>,
    /// Its latest report line, which says what it is doing better than its last tool call.
    pub report: Option<String>,
    /// It waits on the human, or its issue does.
    pub needs_me: bool,
}

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
    /// Issues waiting on the human (Needs You), and the human's own open work outside Backlog, with
    /// no worktree on the board.
    pub needs_me_issues: Vec<IssueRecord>,
    /// Agents' questions still waiting for an answer.
    pub asks: Vec<Ask>,
    pub sync: Vec<SyncStatus>,
    /// Agents on other machines reaching this one through `nitely serve` (cwd `remote:<token>`),
    /// which no repo here can place.
    pub remote_sessions: Vec<Session>,
    /// Every agent session seen in the window on this machine, one row each, newest first.
    pub agents: Vec<AgentRow>,
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

/// What git says about every configured repo, read once and shared by the board until the repos
/// change. Reading it is the slow part of a snapshot (each git process costs 0.1-0.8s on Windows),
/// so it starts as few processes as it can and runs them side by side.
pub struct GitState {
    repos: Vec<RepoGit>,
    /// `watch::repos_fingerprint` when it was read, and when.
    fingerprint: u64,
    read_at: Instant,
}

struct RepoGit {
    root: String,
    worktrees: Result<Vec<git::Worktree>, String>,
    /// Every local branch with its counts and last commit, from one `for-each-ref`.
    branches: Vec<git::BranchRef>,
    /// Per worktree, in the order of `worktrees`.
    states: Vec<Result<BranchState, String>>,
    slug: Option<String>,
}

/// Git processes run side by side at most; more only queue up in the OS.
const GIT_LANES: usize = 8;

/// Runs `f` over `items` on up to `GIT_LANES` threads; results keep the order of `items`.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..GIT_LANES.min(items.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                *out[i].lock().unwrap() = Some(f(item));
            });
        }
    });
    out.into_iter().map(|m| m.into_inner().unwrap().expect("every item ran")).collect()
}

/// Reads git for every repo: per repo its worktrees, branches and remote (in parallel), then one
/// `git status` per worktree (in parallel across all repos).
pub fn read_git(config: &Config) -> GitState {
    let fingerprint = crate::watch::repos_fingerprint(config);
    let heads = par_map(&config.repos, |repo: &RepoConfig| {
        let worktrees = git::list_worktrees(&repo.root).map_err(|e| e.to_string());
        let branches = git::list_branches(&repo.root, &repo.integration_branch).unwrap_or_default();
        let slug = git::bitbucket_slug(&repo.root).or_else(|| git::github_slug(&repo.root));
        (worktrees, branches, slug)
    });
    let jobs: Vec<(usize, &git::Worktree, &RepoConfig, &[git::BranchRef])> = heads
        .iter()
        .enumerate()
        .flat_map(|(r, (worktrees, branches, _))| {
            let repo = &config.repos[r];
            worktrees.iter().flatten().map(move |wt| (r, wt, repo, branches.as_slice()))
        })
        .collect();
    let mut states = par_map(&jobs, |(_, wt, repo, branches)| {
        let line = wt.branch.as_deref().and_then(|b| branches.iter().find(|x| x.name == b));
        git::worktree_state(&wt.path, &repo.integration_branch, line).map_err(|e| e.to_string())
    })
    .into_iter();
    let repos = heads
        .into_iter()
        .zip(&config.repos)
        .map(|((worktrees, branches, slug), repo)| {
            let n = worktrees.as_ref().map_or(0, Vec::len);
            RepoGit { root: repo.root.clone(), states: states.by_ref().take(n).collect(), worktrees, branches, slug }
        })
        .collect();
    GitState { repos, fingerprint, read_at: Instant::now() }
}

/// Git state reused across snapshots: while no ref, HEAD, worktree list or the config moved, and
/// for at most `GIT_FRESH`, a snapshot reads only the database. Uncommitted edits (which move
/// none of those) show within `GIT_FRESH`, as the app's fallback refresh asks for a new one.
pub struct GitCache(Mutex<Option<Arc<GitState>>>);

const GIT_FRESH: Duration = Duration::from_secs(25);

impl GitCache {
    pub const fn new() -> GitCache {
        GitCache(Mutex::new(None))
    }

    /// The cached state when it is still current for `config`, else a fresh read. The lock is
    /// held while reading, so refreshes that arrive together share one read.
    pub fn get(&self, config: &Config) -> Arc<GitState> {
        let mut cached = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let same_repos = |s: &GitState| s.repos.iter().map(|r| &r.root).eq(config.repos.iter().map(|r| &r.root));
        if let Some(state) = cached.as_ref() {
            if state.read_at.elapsed() < GIT_FRESH && same_repos(state) && state.fingerprint == crate::watch::repos_fingerprint(config) {
                return state.clone();
            }
        }
        let state = Arc::new(read_git(config));
        *cached = Some(state.clone());
        state
    }
}

impl Default for GitCache {
    fn default() -> Self {
        GitCache::new()
    }
}

pub fn snapshot(config: &Config, db: &Db) -> Result<Board> {
    build(config, db, &read_git(config))
}

/// A snapshot that reuses `cache`'s git state while the repos have not changed.
pub fn snapshot_cached(config: &Config, db: &Db, cache: &GitCache) -> Result<Board> {
    build(config, db, &cache.get(config))
}

fn build(config: &Config, db: &Db, git_state: &GitState) -> Result<Board> {
    let sessions = db.sessions_since(now() - SESSION_WINDOW_SECS)?;
    let issues = db.issues()?;
    let prs = db.prs()?;
    // Branches cut before a project changed its key still name the old one.
    let aliases = db.project_aliases()?;
    let mut views = Vec::new();

    for (repo, g) in config.repos.iter().zip(&git_state.repos) {
        let worktrees = match &g.worktrees {
            Ok(w) => w,
            Err(e) => {
                views.push(RepoView {
                    name: repo.name.clone(),
                    root: repo.root.clone(),
                    integration_branch: repo.integration_branch.clone(),
                    project: repo.project.clone(),
                    rows: vec![],
                    branches: vec![],
                    error: Some(e.clone()),
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

        let slug = g.slug.clone();
        let rows = worktrees
            .iter()
            .zip(session_for)
            .zip(&g.states)
            .map(|((wt, session), git_state)| {
                let (vendor, issue_key) = wt
                    .branch
                    .as_deref()
                    .map(git::parse_branch)
                    .unwrap_or((None, None));
                let issue_key = issue_key.map(|k| crate::projects::current_key(&aliases, &k));
                let (state, error) = match git_state {
                    Ok(s) => (Some(s.clone()), None),
                    Err(e) => (None, Some(e.clone())),
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

        let branches = loose_branches(repo, worktrees, &g.branches, &issues, &prs, &aliases, slug.as_deref());

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
        .filter(|i| (i.needs_human() || (i.is_humans() && i.is_open() && i.state != "Backlog")) && !on_board.contains(i.key.as_str()))
        .cloned()
        .collect();

    let remote_sessions = sessions.iter().filter(|s| s.cwd.starts_with("remote:")).cloned().collect();
    let agents = agents(db, config, &sessions, &views, &issues, &aliases)?;
    Ok(Board { repos: views, needs_me_issues, asks: db.open_asks()?, sync: db.sync_statuses()?, remote_sessions, agents })
}

/// One row per local session: where it runs, and the issue its own events name.
fn agents(
    db: &Db,
    config: &Config,
    sessions: &[Session],
    views: &[RepoView],
    issues: &[IssueRecord],
    aliases: &std::collections::HashMap<String, String>,
) -> Result<Vec<AgentRow>> {
    let local: Vec<&Session> = sessions.iter().filter(|s| !s.cwd.starts_with("remote:")).collect();
    let mut named = db.issues_sessions_work_on(&local.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), now())?;
    // An agent whose calls name no session acts under its folder's id (`claude:c:/code/app`), a
    // crew worker always: its hooks report the same agent under its own session id. Where such a
    // session in the folder was seen since the folder's record, it is the agent (its state is the
    // current one) and the record is not another; the record's work is its own when it is the only
    // one there. A record newer than every session there (Codex calls without a thread, after an
    // earlier thread went idle) is the agent at work, and stays.
    let folder_id = |s: &Session| s.id == format!("{}:{}", s.vendor, crate::paths::normalize(&s.cwd));
    let place = |s: &Session| (s.vendor.clone(), crate::paths::normalize(&s.cwd));
    let mut in_folder: std::collections::HashMap<(String, String), Vec<(&str, i64)>> = std::collections::HashMap::new();
    for s in local.iter().filter(|s| !folder_id(s)) {
        in_folder.entry(place(s)).or_default().push((s.id.as_str(), s.last_seen_at));
    }
    let superseded = |s: &Session| folder_id(s) && in_folder.get(&place(s)).is_some_and(|there| there.iter().any(|(_, seen)| *seen >= s.last_seen_at));
    for s in local.iter().filter(|s| superseded(s)) {
        if let (Some([(only, _)]), Some(work)) = (in_folder.get(&place(s)).map(Vec::as_slice), named.get(&s.id).cloned()) {
            let mine = named.entry(only.to_string()).or_default();
            mine.0 = mine.0.take().or(work.0);
            mine.1 = mine.1.take().or(work.1);
        }
    }
    let local: Vec<&Session> = local.iter().copied().filter(|s| !superseded(s)).collect();
    let mut out = Vec::new();
    for s in local {
        let repo = config.repos.iter().zip(views).filter(|(_, v)| is_within(&s.cwd, &v.root) || v.rows.iter().any(|r| is_within(&s.cwd, &r.worktree))).map(|(r, _)| r).next();
        // The task worktree it runs in: the deepest one holding its folder, a main checkout aside.
        let row = views.iter().flat_map(|v| v.rows.iter()).filter(|r| !r.is_main && is_within(&s.cwd, &r.worktree)).max_by_key(|r| r.worktree.len());
        let (from_events, report) = named.get(&s.id).cloned().unwrap_or_default();
        let issue_key = from_events.map(|k| crate::projects::current_key(aliases, &k)).or_else(|| row.and_then(|r| r.issue_key.clone()));
        let issue = issue_key.as_ref().and_then(|k| issues.iter().find(|i| i.key.eq_ignore_ascii_case(k)).cloned());
        let needs_me = s.state == "waiting" || issue.as_ref().is_some_and(|i| i.needs_human());
        out.push(AgentRow {
            session: s.clone(),
            repo: repo.map(|r| r.name.clone()),
            project: repo.and_then(|r| r.project.clone()),
            worktree: row.map(|r| r.worktree.clone()),
            issue_key,
            issue,
            report,
            needs_me,
        });
    }
    Ok(out)
}

/// Per session: the issue its own events name, and its latest report line.
pub type SessionWork = std::collections::HashMap<String, (Option<String>, Option<String>)>;

impl Db {
    /// For each of `sessions`, the issue its own events name and its latest report line. The issue
    /// is the one of its latest claim, start_task or hand-in, or of a report or note newer than that
    /// within `REPORT_NAMES_ISSUE_SECS` of `now`: an agent working from the main checkout names its
    /// issue only in what it does, never in its folder. What a submit or land writes as it goes (its
    /// outcome, and the status lines it logs from the worktree, with a `cwd`) is not the agent's
    /// act: its job runs on after the agent moved on, so it names nothing and is no report.
    pub fn issues_sessions_work_on(&self, sessions: &[&str], now: i64) -> Result<SessionWork> {
        let mut out = SessionWork::new();
        if sessions.is_empty() {
            return Ok(out);
        }
        let wanted: HashSet<&str> = sessions.iter().copied().collect();
        let mut stmt = self.conn.prepare(
            "SELECT actor, ts, type, issue_key, summary, cwd IS NOT NULL FROM event
             WHERE ts >= ?1 AND (type IN ('claimed', 'worktree', 'submit_queued', 'status', 'note', 'handoff')) ORDER BY ts, id",
        )?;
        let rows = stmt.query_map([now - SESSION_WINDOW_SECS], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, bool>(5)?))
        })?;
        for row in rows {
            let (actor, ts, kind, issue, summary, has_cwd) = row?;
            if !wanted.contains(actor.as_str()) {
                continue;
            }
            let entry = out.entry(actor).or_default();
            let strong = matches!(kind.as_str(), "claimed" | "worktree" | "submit_queued");
            // A pipeline step's line, logged from the worktree it works in; the agent's own
            // reports and notes name their issue without one.
            let pipeline = !strong && has_cwd && issue.is_some();
            if pipeline {
                continue;
            }
            if kind == "status" {
                entry.1 = summary.or(entry.1.take());
            }
            if let Some(key) = issue.filter(|_| strong || ts >= now - REPORT_NAMES_ISSUE_SECS) {
                entry.0 = Some(key.to_uppercase());
            }
        }
        Ok(out)
    }
}

/// Branches no worktree has checked out. A repo whose branches cannot be listed shows none rather
/// than failing the whole board.
fn loose_branches(
    repo: &RepoConfig,
    worktrees: &[git::Worktree],
    branches: &[git::BranchRef],
    issues: &[IssueRecord],
    prs: &[PrRecord],
    aliases: &std::collections::HashMap<String, String>,
    slug: Option<&str>,
) -> Vec<BranchRow> {
    let checked_out: HashSet<&str> = worktrees.iter().filter_map(|w| w.branch.as_deref()).collect();
    branches
        .iter()
        .filter(|b| b.name != repo.integration_branch && !checked_out.contains(b.name.as_str()))
        .cloned()
        .map(|b| {
            let (vendor, issue_key) = git::parse_branch(&b.name);
            let issue_key = issue_key.map(|k| crate::projects::current_key(aliases, &k));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_session_works_on_the_issue_its_own_events_name() {
        let dir = crate::paths::unique_temp("nitely-agents", "");
        let db = Db::open(&dir.join("t.db")).unwrap();
        let now = 1_800_000_000;
        let event = |ts: i64, actor: &str, kind: &str, issue: Option<&str>, summary: &str| {
            db.conn.execute("INSERT INTO event (ts, actor, type, issue_key, summary) VALUES (?1, ?2, ?3, ?4, ?5)", rusqlite::params![ts, actor, kind, issue, summary]).unwrap();
        };
        // Three sessions in one folder, each on its own issue.
        event(now - 3600, "claude:a", "claimed", Some("APP-1"), "Took the issue");
        event(now - 600, "claude:a", "status", Some("APP-1"), "Writing the export");
        event(now - 300, "claude:a", "status", None, "Running the tests");
        // Started one long ago, then reports on another now: the newer names it.
        event(now - 7200, "claude:b", "worktree", Some("APP-2"), "Started claude/app-2-x from main");
        event(now - 120, "claude:b", "note", Some("APP-3"), "Picked up APP-3 too");
        // Only a report from hours ago: it names nothing any more.
        event(now - 7200, "claude:c", "status", Some("APP-4"), "Looking around");
        // Another session's claim is not this one's.
        event(now - 60, "claude:d", "claimed", Some("APP-5"), "Took the issue");
        // Handed in APP-6, moved on to APP-7; the hand-in's outcome arrives later under its id.
        event(now - 900, "claude:e", "submit_queued", Some("APP-6"), "Handed in as job 3");
        event(now - 600, "claude:e", "claimed", Some("APP-7"), "Took the issue");
        event(now - 30, "claude:e", "submit", Some("APP-6"), "Review passed; ready to land");
        // And its job's status line, logged from the worktree as `log_event` does.
        db.conn.execute("INSERT INTO event (ts, actor, cwd, type, issue_key, summary) VALUES (?1, 'claude:e', 'C:/code/wt/app-6', 'status', 'APP-6', 'Reviewed alone, not in a batch')", [now - 20]).unwrap();
        let named = db.issues_sessions_work_on(&["claude:a", "claude:b", "claude:c", "claude:e"], now).unwrap();
        assert_eq!(named["claude:e"].0.as_deref(), Some("APP-7"), "a submit's outcome and its job's lines are not where the agent is");
        assert_eq!(named["claude:a"], (Some("APP-1".to_string()), Some("Running the tests".to_string())));
        assert_eq!(named["claude:b"].0.as_deref(), Some("APP-3"));
        assert_eq!(named["claude:c"].0, None);
        assert!(!named.contains_key("claude:d"));
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn par_map_keeps_order_and_runs_everything() {
        let items: Vec<u32> = (0..50).collect();
        let out = par_map(&items, |n| {
            std::thread::sleep(Duration::from_millis(u64::from(50 - n) % 7));
            n * 2
        });
        assert_eq!(out, items.iter().map(|n| n * 2).collect::<Vec<_>>());
        assert!(par_map(&Vec::<u32>::new(), |n| *n).is_empty());
    }
}
