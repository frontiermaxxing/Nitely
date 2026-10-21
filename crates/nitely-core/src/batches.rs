//! What the app shows of review batches and the land queue (docs/batching.md, "What the person
//! sees"; NIT-218): the batches running now for the board, and each issue's own batches for its
//! timeline. It only reads, so the read-only live preview can show it too.
//!
//! Shadow batches (NIT-217) are shown as what they are: recorded beside a single review that
//! decided, never deciding anything themselves.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::db::Db;
use crate::land::Landed;
use crate::land_queue::Run;
use crate::review::Review;

/// A review batch: changes of one repository reviewed in one reviewer call.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewBatch {
    pub id: i64,
    pub repo: String,
    /// The repo's name in Nitely's config, when it is connected.
    pub repo_name: Option<String>,
    pub reviewer: String,
    /// Its changes, in the order the reviewer got them.
    pub issues: Vec<String>,
    /// reviewing | done | failed
    pub state: String,
    /// Recorded only: each change's single review decided it.
    pub shadow: bool,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub seconds: Option<i64>,
    pub tokens: Option<i64>,
    pub error: Option<String>,
}

/// A change waiting in a review queue for others to join it.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewWait {
    pub issue: String,
    pub repo_name: Option<String>,
    pub reviewer: String,
    pub shadow: bool,
    pub queued_at: i64,
    pub lines: i64,
}

/// A change in a land queue.
#[derive(Debug, Clone, Serialize)]
pub struct LandWait {
    pub id: i64,
    pub issue: String,
    /// queued | running
    pub state: String,
    pub queued_at: i64,
    pub run: Option<i64>,
}

/// One repository's land queue: what waits or lands now, and the run landing it.
#[derive(Debug, Clone, Serialize)]
pub struct LandQueue {
    pub repo: String,
    pub repo_name: Option<String>,
    /// Oldest first: its place in the queue.
    pub entries: Vec<LandWait>,
    /// The queue runs going now (one at a time, under the land lock).
    pub runs: Vec<Run>,
}

/// The batches going now, for the board.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Overview {
    /// Review batches being reviewed, real and shadow, oldest first.
    pub reviewing: Vec<ReviewBatch>,
    /// Changes waiting for a review batch to form.
    pub review_waiting: Vec<ReviewWait>,
    /// Each repository's land queue that has something in it.
    pub land: Vec<LandQueue>,
}

/// Why a change went back from a batch, in the words the timeline uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SentBack {
    /// `own` (its own P0/P1 in the batch's review), `interacts` (a change of the same batch it
    /// interacts with lands first), `bisected` (the stack's gate failed and bisection found it),
    /// `gate` (its gate failed on its own), `conflict` (it conflicts with what lands before it),
    /// `moved` (its branch changed since its review), `stopped` (the queue run itself stopped),
    /// or `other`.
    pub why: String,
    /// The changes it names: the one it interacts or conflicts with.
    pub with: Vec<String>,
}

/// One of the issue's review batches, and what it said of this change.
#[derive(Debug, Clone, Serialize)]
pub struct IssueReview {
    pub batch: ReviewBatch,
    /// Its row in the queue: reviewing | done | alone (it was reviewed by itself after all).
    pub state: String,
    /// pass | changes, once the batch reviewed it.
    pub verdict: Option<String>,
    /// Its P0 and P1 findings in the batch, and its P2 and lower.
    pub blocking: usize,
    pub minor: usize,
    /// Why it went alone, when it did.
    pub why: Option<String>,
    /// Why the batch sent it back, when its verdict asked for changes.
    pub sent_back: Option<SentBack>,
}

/// One of the issue's trips through a land queue.
#[derive(Debug, Clone, Serialize)]
pub struct IssueLand {
    pub id: i64,
    /// queued | running | landed | sent_back
    pub state: String,
    pub queued_at: i64,
    pub ended_at: Option<i64>,
    /// While it waits: its place in the queue, 1 for the next to land.
    pub place: Option<usize>,
    pub run: Option<Run>,
    /// The build lane run of the gate that decided it, for its log.
    pub lane_run: Option<i64>,
    /// The integration branch's head it landed at.
    pub head: Option<String>,
    /// What land said when it did not land.
    pub error: Option<String>,
    pub sent_back: Option<SentBack>,
}

/// An issue's review batches and land queue trips, newest first.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IssueBatches {
    pub reviews: Vec<IssueReview>,
    pub lands: Vec<IssueLand>,
}

/// A land_queue row: id, repo, state, queued_at, ended_at, run_id, result.
type LandRow = (i64, String, String, i64, Option<i64>, Option<i64>, Option<String>);

const BATCH_COLUMNS: &str = "id, repo, reviewer, issues, state, shadow, started_at, ended_at, seconds, tokens, error";

fn batch_row(r: &rusqlite::Row) -> rusqlite::Result<ReviewBatch> {
    let issues: String = r.get(3)?;
    Ok(ReviewBatch {
        id: r.get(0)?,
        repo: r.get(1)?,
        repo_name: None,
        reviewer: r.get(2)?,
        issues: serde_json::from_str(&issues).unwrap_or_default(),
        state: r.get(4)?,
        shadow: r.get(5)?,
        started_at: r.get(6)?,
        ended_at: r.get(7)?,
        seconds: r.get(8)?,
        tokens: r.get(9)?,
        error: r.get(10)?,
    })
}

/// The name a repository has in Nitely's config, by its path as the queues record it.
fn name_of(config: &crate::Config, repo: &str) -> Option<String> {
    let want = crate::paths::normalize(repo);
    config.repos.iter().find(|r| crate::paths::normalize(&r.root) == want).map(|r| r.name.clone())
}

/// Whether the process `pid` runs (this one does).
fn runs(pid: i64) -> bool {
    let pid = pid as u32;
    pid == std::process::id() || crate::submit_job::process_runs(pid)
}

/// Whether a land queue run of `repo` can be going: a land holds the repo's lock, and the process
/// that took it still runs. A run whose process died stays unended in its row until the next run
/// puts its changes back in the queue, so its row alone does not say.
pub fn land_running(repo: &str) -> bool {
    let Ok(lock) = crate::land::lock_path(std::path::Path::new(repo)) else { return false };
    let Ok(held) = std::fs::read_to_string(lock) else { return false };
    // "<holder> pid <n> since <time>", as land.rs writes it.
    let pid = held.split_whitespace().skip_while(|w| *w != "pid").nth(1).and_then(|p| p.parse::<u32>().ok());
    pid.is_some_and(|p| runs(p as i64))
}

/// The batches going now: review batches under review, changes waiting for one, and every land
/// queue with something in it. A batch or run whose process is gone is not shown as going.
pub fn overview(config: &crate::Config, db: &Db) -> Result<Overview> {
    let reviewing: Vec<(ReviewBatch, i64)> = db
        .conn
        .prepare(&format!("SELECT {BATCH_COLUMNS}, pid FROM review_batch WHERE state = 'reviewing' ORDER BY id"))?
        .query_map([], |r| Ok((batch_row(r)?, r.get(11)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut reviewing: Vec<ReviewBatch> = reviewing
        .into_iter()
        .filter(|(_, pid)| runs(*pid))
        .map(|(b, _)| b)
        .collect();
    for b in &mut reviewing {
        b.repo_name = name_of(config, &b.repo);
    }
    // A change whose submit stopped while it waited is no longer waiting (the next batch to form
    // marks it abandoned).
    let review_waiting: Vec<(ReviewWait, i64)> = db
        .conn
        .prepare("SELECT issue_key, repo, reviewer, shadow, queued_at, lines, pid FROM review_queue WHERE state = 'waiting' ORDER BY id")?
        .query_map([], |r| {
            let repo: String = r.get(1)?;
            let wait = ReviewWait { issue: r.get(0)?, repo_name: name_of(config, &repo), reviewer: r.get(2)?, shadow: r.get(3)?, queued_at: r.get(4)?, lines: r.get(5)? };
            Ok((wait, r.get(6)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let review_waiting = review_waiting.into_iter().filter(|(_, pid)| runs(*pid)).map(|(w, _)| w).collect();
    let rows: Vec<(String, LandWait)> = db
        .conn
        .prepare("SELECT repo, id, issue_key, state, queued_at, run_id FROM land_queue WHERE state IN ('queued', 'running') ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, LandWait { id: r.get(1)?, issue: r.get(2)?, state: r.get(3)?, queued_at: r.get(4)?, run: r.get(5)? })))?
        .collect::<rusqlite::Result<_>>()?;
    let mut land: Vec<LandQueue> = Vec::new();
    let mut live: Vec<(String, bool)> = Vec::new();
    for (repo, mut entry) in rows {
        let running = match live.iter().find(|(r, _)| *r == repo) {
            Some((_, running)) => *running,
            None => {
                let running = land_running(&repo);
                live.push((repo.clone(), running));
                running
            }
        };
        // A change its dead run held waits again: the next run takes it.
        if entry.state == "running" && !running {
            entry.state = "queued".into();
            entry.run = None;
        }
        let at = match land.iter().position(|q| q.repo == repo) {
            Some(i) => i,
            None => {
                land.push(LandQueue { repo_name: name_of(config, &repo), repo, entries: vec![], runs: vec![] });
                land.len() - 1
            }
        };
        let q = &mut land[at];
        if let Some(run) = entry.run.filter(|id| !q.runs.iter().any(|r| r.id == *id)) {
            if let Some(run) = db.land_run(run)?.filter(|r| r.ended_at.is_none()) {
                q.runs.push(run);
            }
        }
        q.entries.push(entry);
    }
    Ok(Overview { reviewing, review_waiting, land })
}

/// Why a land queue run sent a change back, from what it said (land_queue.rs words it).
pub fn land_sent_back(error: &str) -> SentBack {
    let first = error.lines().next().unwrap_or("");
    let back = |why: &str, with: Vec<String>| SentBack { why: why.into(), with };
    if first.contains("bisecting the stack found") || first.contains("and passed without it") {
        return back("bisected", vec![]);
    }
    if let Some((_, rest)) = first.split_once(" conflicts with ") {
        // "<branch> conflicts with main or with NIT-1, NIT-2, landing just before it in a.txt; ..."
        let with = rest
            .split_once(" or with ")
            .and_then(|(_, keys)| keys.split_once(", landing just before it"))
            .map(|(keys, _)| keys.split(", ").map(str::to_string).collect())
            .unwrap_or_default();
        return back("conflict", with);
    }
    if first.contains("its gate failed") {
        return back("gate", vec![]);
    }
    if first.contains("gained commits during the gate") || first.contains("changed since its review") {
        return back("moved", vec![]);
    }
    if first.contains("the land queue stopped") {
        return back("stopped", vec![]);
    }
    back("other", vec![])
}

/// Why a review batch asked for changes on a change: its own P0/P1, or only that a change of the
/// same batch it interacts with lands first.
pub fn review_sent_back(review: &Review) -> Option<SentBack> {
    if review.verdict != "changes" {
        return None;
    }
    let blocking: Vec<&str> = review.findings.iter().filter(|f| f.priority <= 1).map(|f| f.text.as_str()).collect();
    let with: Vec<String> = blocking.iter().filter_map(|t| crate::review_batch::interaction_with(t)).map(str::to_string).collect();
    if !blocking.is_empty() && with.len() == blocking.len() {
        return Some(SentBack { why: "interacts".into(), with });
    }
    Some(SentBack { why: "own".into(), with: vec![] })
}

impl Db {
    /// The issue's review batches (real and shadow) and land queue trips, newest first.
    pub fn issue_batches(&self, key: &str) -> Result<IssueBatches> {
        let mut reviews = Vec::new();
        let rows: Vec<(i64, String, Option<String>, Option<String>)> = self
            .conn
            .prepare("SELECT batch_id, state, result, why FROM review_queue WHERE issue_key = ?1 COLLATE NOCASE AND batch_id IS NOT NULL ORDER BY id DESC LIMIT 20")?
            .query_map([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (batch, state, result, why) in rows {
            let Some(b) = self.conn.query_row(&format!("SELECT {BATCH_COLUMNS} FROM review_batch WHERE id = ?1"), [batch], batch_row).optional()? else { continue };
            let review: Option<Review> = (state == "done").then(|| result.as_deref().and_then(|t| serde_json::from_str(t).ok())).flatten();
            let blocking = review.as_ref().map(|r| r.findings.iter().filter(|f| f.priority <= 1).count()).unwrap_or(0);
            let minor = review.as_ref().map(|r| r.findings.iter().filter(|f| f.priority > 1).count()).unwrap_or(0);
            reviews.push(IssueReview {
                sent_back: review.as_ref().and_then(review_sent_back),
                verdict: review.map(|r| r.verdict),
                blocking,
                minor,
                why: if state == "alone" { why } else { None },
                state,
                batch: b,
            });
        }
        let mut lands = Vec::new();
        let rows: Vec<LandRow> = self
            .conn
            .prepare("SELECT id, repo, state, queued_at, ended_at, run_id, result FROM land_queue WHERE issue_key = ?1 COLLATE NOCASE ORDER BY id DESC LIMIT 20")?
            .query_map([key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, repo, mut state, queued_at, ended_at, mut run, result) in rows {
            // A change its dead run held waits again, as the board shows it.
            if state == "running" && !land_running(&repo) {
                state = "queued".into();
                run = None;
            }
            let landed: Option<Landed> = result.as_deref().and_then(|t| serde_json::from_str(t).ok());
            let place = if state == "queued" {
                let ahead: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM land_queue WHERE repo = ?1 AND state IN ('queued', 'running') AND id < ?2",
                    params![repo, id],
                    |r| r.get(0),
                )?;
                Some(ahead as usize + 1)
            } else {
                None
            };
            let error = landed.as_ref().and_then(|l| l.error.clone());
            lands.push(IssueLand {
                id,
                sent_back: (state == "sent_back").then(|| land_sent_back(error.as_deref().unwrap_or(""))),
                state,
                queued_at,
                ended_at,
                place,
                run: run.map(|r| self.land_run(r)).transpose()?.flatten(),
                lane_run: landed.as_ref().and_then(|l| l.gate.as_ref()).and_then(|g| g.lane_run),
                head: landed.as_ref().and_then(|l| l.head.clone()),
                error,
            });
        }
        Ok(IssueBatches { reviews, lands })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Finding;

    fn finding(priority: u8, text: &str) -> Finding {
        Finding { priority, text: text.into(), carried: false, raised_as: None, now: None }
    }

    #[test]
    fn a_land_sent_back_says_why_in_the_queues_own_words() {
        let why = |e: &str| land_sent_back(e);
        let bisected = "the gate failed at `cargo test`: bisecting the stack found that its change is the one that fails the gate. Fix it on the branch, submit, and land again.\nerror[E0425]";
        assert_eq!(why(bisected).why, "bisected");
        assert_eq!(why("the gate failed at `npm test`: the stacked gate failed on a file only NIT-4 changes, and passed without it. Fix it").why, "bisected");
        let conflict = "claude/nit-5-x conflicts with main or with NIT-3, NIT-4, landing just before it in shared.txt; nothing was changed on your branch. Rebase it";
        assert_eq!(why(conflict), SentBack { why: "conflict".into(), with: vec!["NIT-3".into(), "NIT-4".into()] });
        assert_eq!(why("claude/nit-5-x conflicts with main in a.txt; nothing was changed").with, Vec::<String>::new());
        assert_eq!(why("the gate failed at `cargo test`: its gate failed. Fix it on the branch").why, "gate");
        assert_eq!(why("claude/nit-5-x gained commits during the gate, after its review was checked; submit again").why, "moved");
        assert_eq!(why("the land queue stopped: main moved during the gate").why, "stopped");
        assert_eq!(why("something else").why, "other");
    }

    #[test]
    fn a_review_batch_sends_back_for_its_own_p1_or_for_an_interaction() {
        let mut review = Review {
            reviewer: "codex".into(),
            verdict: "pass".into(),
            findings: vec![],
            flagged: vec![],
            summary: String::new(),
            seconds: 0,
            previous: vec![],
            note: String::new(),
            batch: None,
            tokens: None,
            reused: None,
        };
        assert_eq!(review_sent_back(&review), None);
        review.verdict = "changes".into();
        review.findings = vec![finding(1, "Interacts with NIT-4, reviewed in the same batch: both edit the parser (a.rs). Once NIT-4 has landed, rebase")];
        assert_eq!(review_sent_back(&review), Some(SentBack { why: "interacts".into(), with: vec!["NIT-4".into()] }));
        review.findings.push(finding(1, "The parser drops the last line."));
        assert_eq!(review_sent_back(&review).unwrap().why, "own");
        review.findings = vec![finding(0, "Leaks the token."), finding(2, "A nit.")];
        assert_eq!(review_sent_back(&review).unwrap().why, "own");
    }

    /// The queues' rows as the review batcher and the land queue write them.
    #[test]
    fn the_board_and_an_issue_read_the_queues_rows() {
        let dir = crate::paths::unique_temp("nitely-batches", "");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        // A repo whose land lock a live process (this test) holds: its queue run is going.
        let root = dir.join("my-app");
        crate::test_repo::init(&root, &[("a.txt", "a")]);
        let lock = crate::land::lock_path(&root).unwrap();
        std::fs::write(&lock, format!("APP-5 (land queue) pid {} since now\n", std::process::id())).unwrap();
        let repo = crate::paths::normalize(&root.to_string_lossy());
        let pid = std::process::id();
        let config = crate::Config { repos: vec![crate::RepoConfig { name: "my-app".into(), root: repo.clone(), integration_branch: "main".into(), project: Some("APP".into()) }], ..Default::default() };
        let exec = |sql: &str| db.conn.execute_batch(sql).unwrap();
        let shadow_review = r#"{"reviewer":"codex","verdict":"changes","findings":[{"priority":1,"text":"Drops a line."},{"priority":3,"text":"A nit."}],"flagged":[],"summary":"","seconds":9}"#;
        let conflict = r#"{"issue":"APP-6","error":"b conflicts with main or with APP-5, landing just before it in a.txt"}"#;
        // A real batch under review, a shadow one done, and a change waiting for one.
        exec(&format!(
            r#"INSERT INTO review_batch (id, repo, reviewer, issues, state, pid, started_at, shadow) VALUES
                 (1, '{repo}', 'codex', '["APP-1","APP-2"]', 'reviewing', {pid}, 100, 0),
                 (2, '{repo}', 'codex', '["APP-1","APP-3"]', 'done', {pid}, 50, 1),
                 (3, '{repo}', 'codex', '["APP-8","APP-9"]', 'reviewing', 4000000000, 100, 0);
               INSERT INTO review_queue (issue_key, repo, reviewer, note, title, branch, worktree, base, head, lines, files, pid, actor, state, batch_id, queued_at, result, shadow) VALUES
                 ('APP-1', '{repo}', 'codex', '', 'One', 'b1', 'w1', 'x', 'y', 10, '[]', 1, 'claude:s', 'reviewing', 1, 90, NULL, 0),
                 ('APP-1', '{repo}', 'codex', '', 'One', 'b1', 'w1', 'x', 'y', 10, '[]', 1, '', 'done', 2, 40, '{shadow_review}', 1),
                 ('APP-4', '{repo}', 'codex', '', 'Four', 'b4', 'w4', 'x', 'y', 5, '[]', {pid}, 'claude:s', 'waiting', NULL, 95, NULL, 0),
                 ('APP-11', '{repo}', 'codex', '', 'Eleven', 'b11', 'w11', 'x', 'y', 5, '[]', 4000000000, 'claude:s', 'waiting', NULL, 96, NULL, 0);"#
        ));
        // A queue run landing two changes, with a third behind it, and one sent back earlier.
        exec(&format!(
            r#"INSERT INTO land_run (id, repo, started_at, issues, step) VALUES (7, '{repo}', 200, 'APP-5,APP-6', 'bisecting: gating 1 of 2');
               INSERT INTO land_queue (id, repo, issue_key, requested_by, state, queued_at, run_id, result) VALUES
                 (1, '{repo}', 'APP-6', 'claude:s', 'sent_back', 10, NULL, '{conflict}'),
                 (2, '{repo}', 'APP-5', 'claude:s', 'running', 190, 7, NULL),
                 (3, '{repo}', 'APP-6', 'claude:s', 'running', 191, 7, NULL),
                 (4, '{repo}', 'APP-7', 'claude:s', 'queued', 195, NULL, NULL);"#
        ));
        let o = overview(&config, &db).unwrap();
        assert_eq!(o.reviewing.len(), 1, "only what is under review now, by a process that still runs");
        assert_eq!((o.reviewing[0].issues.clone(), o.reviewing[0].repo_name.as_deref()), (vec!["APP-1".to_string(), "APP-2".to_string()], Some("my-app")));
        assert_eq!(o.review_waiting.iter().map(|w| w.issue.as_str()).collect::<Vec<_>>(), ["APP-4"], "not APP-11, whose submit stopped");
        assert_eq!(o.land.len(), 1);
        assert_eq!(o.land[0].entries.iter().map(|e| e.issue.as_str()).collect::<Vec<_>>(), ["APP-5", "APP-6", "APP-7"]);
        assert_eq!(o.land[0].runs.iter().map(|r| r.step.as_deref()).collect::<Vec<_>>(), [Some("bisecting: gating 1 of 2")]);

        let one = db.issue_batches("app-1").unwrap();
        assert_eq!(one.reviews.iter().map(|r| (r.batch.id, r.batch.shadow, r.state.as_str())).collect::<Vec<_>>(), [(2, true, "done"), (1, false, "reviewing")]);
        let shadow = &one.reviews[0];
        assert_eq!((shadow.verdict.as_deref(), shadow.blocking, shadow.minor), (Some("changes"), 1, 1));
        assert_eq!(shadow.sent_back.as_ref().unwrap().why, "own");
        let six = db.issue_batches("APP-6").unwrap();
        assert_eq!(six.lands.iter().map(|l| l.state.as_str()).collect::<Vec<_>>(), ["running", "sent_back"]);
        assert_eq!(six.lands[0].run.as_ref().unwrap().issues, ["APP-5", "APP-6"]);
        assert_eq!(six.lands[1].sent_back, Some(SentBack { why: "conflict".into(), with: vec!["APP-5".into()] }));
        assert_eq!(db.issue_batches("APP-7").unwrap().lands[0].place, Some(3), "two land ahead of it");

        // The run's process died: its changes wait again, and no run shows as going.
        std::fs::write(&lock, "APP-5 (land queue) pid 4000000000 since then\n").unwrap();
        let o = overview(&config, &db).unwrap();
        assert!(o.land[0].runs.is_empty());
        assert!(o.land[0].entries.iter().all(|e| e.state == "queued" && e.run.is_none()));
        let five = db.issue_batches("APP-5").unwrap();
        assert_eq!((five.lands[0].state.as_str(), five.lands[0].place, five.lands[0].run.is_none()), ("queued", Some(1), true), "and so does its panel");
        std::fs::remove_file(&lock).unwrap();
        assert!(overview(&config, &db).unwrap().land[0].runs.is_empty(), "nor without a lock");
        std::fs::remove_dir_all(&dir).ok();
    }
}
