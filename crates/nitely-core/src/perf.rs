//! Performance budgets on a large board (NIT-255): what the app reads on every refresh, timed on a
//! seeded board of 4 projects, 800 issues, 30 worktrees and 5,000 events. `cargo test --release -p
//! nitely-core perf -- --ignored --nocapture` runs them and prints each time; CI fails a change that
//! breaks a budget. Debug builds print only, since their times say little about a release. The app's
//! own budgets (start-up, memory over live updates) are in scripts/harness/perf.e2e.mjs, on the same
//! board, seeded by `nitely perf-seed`.

use crate::db::{now, NewEvent};
use crate::issues::{Actor, NewIssue};
use crate::{Config, Db, RepoConfig};
use anyhow::{bail, Result};
use std::path::Path;

pub const PROJECTS: [&str; 4] = ["APP", "WEB", "API", "OPS"];
pub const ISSUES: usize = 800;
pub const WORKTREES: usize = 30;
pub const EVENTS: usize = 5_000;

/// git in `dir` as a made-up author, for the seeded repos.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let all: Vec<&str> = ["-c", "user.name=Seed", "-c", "user.email=seed@example.com", "-c", "maintenance.auto=false"].into_iter().chain(args.iter().copied()).collect();
    crate::playbook::git(dir, &all)
}

/// Seeds an empty database at `db_path` with the large board, its repos made under `repos_in`: 4
/// projects with a repo each, 800 issues in every state, 30 worktrees on issue branches with an
/// agent session in each, 5,000 events over two weeks, open asks, reviews and blockers. Returns the
/// config naming the repos.
pub fn seed(db_path: &Path, repos_in: &Path) -> Result<Config> {
    let mut db = Db::open(db_path)?;
    if db.conn.query_row("SELECT COUNT(*) FROM issue", [], |r| r.get::<_, i64>(0))? > 0 {
        bail!("{} already has issues; the large board is seeded only into an empty database", db_path.display());
    }
    std::fs::create_dir_all(repos_in)?;
    let agent = Actor::Agent("claude:seed".into());
    let mut repos = vec![];
    for (p, key) in PROJECTS.iter().enumerate() {
        db.create_project(key, &format!("Project {}", p + 1))?;
        let root = repos_in.join(format!("repo-{}", key.to_lowercase()));
        std::fs::create_dir_all(&root)?;
        git(&root, &["init", "-q", "-b", "main"])?;
        std::fs::write(root.join("README.md"), "a repo\n")?;
        git(&root, &["add", "."])?;
        git(&root, &["commit", "-q", "-m", "init"])?;
        repos.push(RepoConfig { name: format!("repo-{}", key.to_lowercase()), root: root.to_string_lossy().into_owned(), integration_branch: "main".into(), project: Some(key.to_string()) });
    }

    let states = ["Todo", "Todo", "Backlog", "In Progress", "In Review", "Needs You", "Done", "Done", "Done", "Canceled"];
    let words = ["sidebar", "billing", "export", "login", "search", "sync", "upload", "theme", "cache", "report", "invite", "audit"];
    let mut keys: Vec<String> = vec![];
    for n in 0..ISSUES {
        let project = PROJECTS[n % PROJECTS.len()];
        let title = format!("{} {} fails when the {} list is long ({n})", words[n % words.len()], ["view", "job", "panel", "check"][n % 4], words[(n * 7) % words.len()]);
        let new = NewIssue {
            project: project.into(),
            title,
            body: "What happens, what was expected, and where.\n".repeat(1 + n % 5),
            kind: ["bug", "feature", "improvement", "idea"][n % 4].into(),
            source: "claude".into(),
            evidence: "seen in a seeded run".into(),
            priority: (n % 5) as i64,
            allow_similar: true,
            labels: if n % 3 == 0 { vec![["ios", "web", "api"][n % 3].into()] } else { vec![] },
            ..Default::default()
        };
        let issue = db.create_issue(&new, &agent)?;
        match states[n % states.len()] {
            "Todo" => {}
            "Done" => drop(db.move_issue(&issue.key, "Done", Some("landed in a seeded run"), None, &agent)?),
            "Canceled" => drop(db.move_issue(&issue.key, "Canceled", None, Some("not needed"), &agent)?),
            s => drop(db.move_issue(&issue.key, s, None, None, &agent)?),
        }
        keys.push(issue.key);
    }
    // Blockers between open issues of a project.
    for n in (0..ISSUES - PROJECTS.len()).step_by(17) {
        db.add_blocker(&keys[n + PROJECTS.len()], &keys[n], &agent).ok();
    }

    // 30 worktrees on issue branches, spread over the repos, each with an agent session working in it.
    let open: Vec<&String> = keys.iter().enumerate().filter(|(n, _)| matches!(states[n % states.len()], "In Progress" | "In Review" | "Needs You")).map(|(_, k)| k).collect();
    let worktree = |key: &str| repos_in.join(format!("wt-{}", key.to_lowercase()));
    for (w, key) in open.iter().take(WORKTREES).enumerate() {
        let p = PROJECTS.iter().position(|p| key.starts_with(&format!("{p}-"))).unwrap_or(0);
        let root = Path::new(&repos[p].root);
        let wt = worktree(key);
        let branch = format!("{}/{}-work", ["claude", "codex"][w % 2], key.to_lowercase());
        git(root, &["worktree", "add", "-q", "-b", &branch, &wt.to_string_lossy(), "main"])?;
        std::fs::write(wt.join(format!("change-{w}.txt")), "work in progress\n")?;
        if w % 2 == 0 {
            git(&wt, &["add", "."])?;
            git(&wt, &["commit", "-q", "-m", &format!("{key}: part of it")])?;
        }
        db.record(&event(w, &wt, "Bash cargo test".into()))?;
    }

    // 5,000 events: hook events in the worktrees' sessions and notes on issues, then spread over
    // two weeks the way a real board's history is.
    for n in 0..EVENTS {
        let w = n % WORKTREES.min(open.len());
        if n % 3 == 0 {
            db.add_note(&keys[(n * 13) % keys.len()], &format!("running the tests ({n})"), "status", &agent)?;
        } else {
            db.record(&event(w, &worktree(open[w]), format!("Edit src/file-{}.rs", n % 40)))?;
        }
    }
    let two_weeks = 14 * 24 * 3600;
    db.conn.execute("UPDATE event SET ts = ?1 - (id * 2654435761 % ?2)", rusqlite::params![now(), two_weeks])?;

    // Open asks, and a passing review on what waits for approval.
    for (n, key) in keys.iter().enumerate().filter(|(n, _)| states[n % states.len()] == "In Progress").take(20) {
        db.create_ask(&format!("Should {key} keep the old export format? ({n})"), &["yes".into(), "no".into()], Some(key), None, &agent)?;
    }
    for key in keys.iter().enumerate().filter(|(n, _)| states[n % states.len()] == "Needs You").map(|(_, k)| k) {
        db.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, ts) VALUES (?1, 'p', 'c', 'codex', 'pass', 'human', '[\"billing\"]', '[]', ?2)",
            rusqlite::params![key, now() - 600],
        )?;
    }
    Ok(Config { repos, ..Default::default() })
}

/// An agent's hook event in the `w`th worktree's session.
fn event(w: usize, worktree: &Path, summary: String) -> NewEvent {
    NewEvent { session_id: format!("s-{w}"), vendor: ["claude", "codex"][w % 2].into(), cwd: worktree.to_string_lossy().into_owned(), kind: "tool".into(), summary: Some(summary), state: "working".into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Budgets for a release build on an ordinary machine (CI's runners included), with room for a
    /// busy one. What each reads is what the app reads on every refresh, or on a click.
    const BUDGETS: [(&str, u64); 6] = [
        ("open the database", 50),
        ("snapshot, git read again", 1500),
        ("snapshot, git cached", 300),
        ("pulse", 150),
        ("inbox decisions", 300),
        ("issue detail", 50),
    ];

    /// A seeded board in its own folder, removed when it goes.
    struct LargeBoard {
        base: PathBuf,
        db_path: PathBuf,
        config: Config,
    }

    impl Drop for LargeBoard {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    fn large_board() -> LargeBoard {
        let base = crate::paths::unique_temp("nitely-perf", "");
        std::fs::create_dir_all(&base).unwrap();
        let db_path = base.join("nitely.db");
        let config = seed(&db_path, &base.join("repos")).unwrap();
        LargeBoard { base, db_path, config }
    }

    /// The median of `runs` timings of `f`.
    fn median(runs: usize, mut f: impl FnMut()) -> Duration {
        let mut times: Vec<Duration> = (0..runs)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed()
            })
            .collect();
        times.sort();
        times[times.len() / 2]
    }

    #[test]
    fn seeding_refuses_a_database_with_issues() {
        let base = crate::paths::unique_temp("nitely-perf-refuse", "");
        std::fs::create_dir_all(&base).unwrap();
        let db_path = base.join("nitely.db");
        let mut db = Db::open(&db_path).unwrap();
        db.create_project("APP", "App").unwrap();
        let new = NewIssue { project: "APP".into(), title: "Mine".into(), kind: "bug".into(), body: "b".into(), source: "human".into(), evidence: "e".into(), ..Default::default() };
        db.create_issue(&new, &Actor::Human).unwrap();
        let err = seed(&db_path, &base.join("repos")).unwrap_err().to_string();
        assert!(err.contains("already has issues"), "{err}");
        assert!(!base.join("repos").exists(), "nothing made");
        drop(db);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    #[ignore = "seeds a large board; CI runs it in release (cargo test --release -p nitely-core perf -- --ignored)"]
    fn a_large_board_stays_within_its_budgets() {
        let seeded = Instant::now();
        let board = large_board();
        eprintln!("seeded in {:.1} s", seeded.elapsed().as_secs_f64());
        let db = Db::open(&board.db_path).unwrap();
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM issue", [], |r| r.get::<_, i64>(0)).unwrap(), ISSUES as i64);

        let mut measured = vec![];
        measured.push(("open the database", median(9, || drop(Db::open(&board.db_path).unwrap()))));
        measured.push(("snapshot, git read again", median(5, || drop(crate::snapshot::snapshot(&board.config, &db).unwrap()))));
        let cache = crate::snapshot::GitCache::new();
        crate::snapshot::snapshot_cached(&board.config, &db, &cache).unwrap();
        measured.push(("snapshot, git cached", median(9, || drop(crate::snapshot::snapshot_cached(&board.config, &db, &cache).unwrap()))));
        let day_start = now() - 6 * 3600;
        measured.push(("pulse", median(9, || drop(crate::pulse(&db, now() - 24 * 3600, day_start).unwrap()))));
        // What the Inbox reads on each refresh: the detail of every issue a decision could name
        // (decisions.ts, detailKeys), each through its own call and connection, as the app does.
        let snap = crate::snapshot::snapshot(&board.config, &db).unwrap();
        assert_eq!(snap.repos.iter().flat_map(|r| &r.rows).filter(|r| !r.is_main).count(), WORKTREES, "every seeded worktree is on the board");
        let keys = inbox_keys(&snap);
        assert!(keys.len() >= 30, "the board has decisions to read: {}", keys.len());
        measured.push((
            "inbox decisions",
            median(5, || {
                for key in &keys {
                    let db = Db::open(&board.db_path).unwrap();
                    db.issue_detail(key).unwrap().unwrap();
                }
            }),
        ));
        let one = keys.first().unwrap().clone();
        measured.push(("issue detail", median(9, || drop(Db::open(&board.db_path).unwrap().issue_detail(&one).unwrap().unwrap()))));

        let mut over = vec![];
        for (what, took) in &measured {
            let budget = BUDGETS.iter().find(|(w, _)| w == what).unwrap().1;
            eprintln!("{what:<26} {:>7.1} ms  (budget {budget} ms)", took.as_secs_f64() * 1000.0);
            if took.as_millis() as u64 > budget {
                over.push(format!("{what}: {} ms, budget {budget} ms", took.as_millis()));
            }
        }
        eprintln!("inbox decisions read {} issues", keys.len());
        if !cfg!(debug_assertions) {
            assert!(over.is_empty(), "over budget:\n{}", over.join("\n"));
        }
    }

    /// The issues the Inbox reads details of, as decisions.ts's `detailKeys` picks them.
    fn inbox_keys(board: &crate::Board) -> Vec<String> {
        let mut keys: Vec<String> = board.asks.iter().filter_map(|a| a.issue_key.clone()).collect();
        keys.extend(board.needs_me_issues.iter().map(|i| i.key.clone()));
        for row in board.repos.iter().flat_map(|r| &r.rows) {
            if let Some(i) = row.issue.as_ref().filter(|i| i.origin == "nitely") {
                if row.needs_me || i.state_type == "started" || i.state == "Needs You" || i.state == "Approved" {
                    keys.push(i.key.clone());
                }
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }
}
