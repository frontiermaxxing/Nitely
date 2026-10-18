//! What every waiting request waits for, and behind what (NIT-322): lane runs in a queue, submits
//! waiting on the issue's previous submit or on a review batch, and changes in a land queue. The
//! app's Waiting list on Now and Overview shows it, and an issue's card and panel say the same. It
//! only reads, so the read-only live preview can show it too.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use std::path::Path;

use crate::db::Db;
use crate::lanes::{self, Run};

/// One request that waits now.
#[derive(Debug, Clone, Serialize)]
pub struct Waiting {
    /// `run` (an agent's `run`), `gate` (a submit's gate), `land` (a land's gate), `submit` (a
    /// submit job not started), `review` (a change waiting for a review batch) or `land_queue`.
    pub kind: String,
    pub issue: Option<String>,
    pub project: Option<String>,
    /// When it started waiting.
    pub since: i64,
    /// Why, in words: "all 2 of NIT's lanes are busy", "waiting for memory: ...".
    pub why: String,
    /// Its place in its queue (1: next), where it has one.
    pub place: Option<usize>,
    /// What it waits behind: the runs holding the lanes or slots, the submit before it, the land.
    pub behind: Vec<Behind>,
}

/// Something a request waits behind.
#[derive(Debug, Clone, Serialize)]
pub struct Behind {
    /// "run 412: NIT-290 gate", "submit job 31", "NIT-288's land".
    pub what: String,
    pub issue: Option<String>,
    /// Since when it holds what the request waits for, for its age.
    pub since: Option<i64>,
}

/// Every request waiting now, longest waiting first.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Waits {
    pub waiting: Vec<Waiting>,
}

fn kind_of(run: &Run) -> &'static str {
    match run.kind.as_str() {
        "gate" => "gate",
        "land" => "land",
        _ => "run",
    }
}

fn ordinal(n: usize) -> String {
    let end = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{end}")
}

fn behind_run(r: &Run) -> Behind {
    let issue = r.issue.clone();
    let what = match &issue {
        Some(k) => format!("run {}: {k} {}", r.id, r.kind),
        None => format!("run {}: {}", r.id, r.kind),
    };
    Behind { what, issue, since: r.started_at.or(Some(r.queued_at)) }
}

/// One repo's lanes as `lane_waits` reads them: its project, how many lanes, and its queue
/// (running and queued requests).
pub struct RepoQueue<'a> {
    pub project: &'a str,
    pub lanes: usize,
    pub queue: &'a [Run],
}

/// Why each queued lane run waits, from the lanes overview.
pub fn lane_waits(o: &lanes::Overview) -> Vec<Waiting> {
    let repos: Vec<RepoQueue> = o.projects.iter().map(|p| RepoQueue { project: &p.lanes.project, lanes: p.lanes.lanes.len(), queue: &p.lanes.queue }).collect();
    lane_waits_of(&repos, o.slots.as_ref().map(|s| (s.used, s.total)))
}

/// Why each queued lane run waits: the person's checkout, memory, every lane of its repo busy,
/// every build slot of the machine in use (`slots`: used, total), or the requests ahead of it.
pub fn lane_waits_of(repos: &[RepoQueue], slots: Option<(u32, u32)>) -> Vec<Waiting> {
    let running_anywhere: Vec<&Run> = repos.iter().flat_map(|p| p.queue.iter()).filter(|r| r.state == "running").collect();
    let slots_full = slots.is_some_and(|(used, total)| total > 0 && used >= total);
    let mut out = Vec::new();
    for p in repos {
        let running: Vec<&Run> = p.queue.iter().filter(|r| r.state == "running").collect();
        let queued: Vec<&Run> = p.queue.iter().filter(|r| r.state == "queued").collect();
        let lanes = p.lanes.max(1);
        for (i, r) in queued.iter().enumerate() {
            let (why, behind): (String, Vec<Behind>) = if let Some(w) = &r.waiting_why {
                scheduler_says(w, &running, &running_anywhere, repos)
            } else if let Some(w) = &r.waiting_for {
                (format!("waits for your checkout: {w}"), vec![])
            } else if let Some(w) = &r.waiting_memory {
                (w.clone(), vec![])
            } else if running.len() >= lanes {
                let n = if lanes == 1 { "its one lane is".to_string() } else { format!("all {lanes} of its lanes are") };
                (format!("{n} busy in {}", p.project), running.iter().map(|x| behind_run(x)).collect())
            } else if slots_full {
                let (used, total) = slots.expect("slots_full");
                (format!("{used} of {total} build slots in use"), running_anywhere.iter().map(|x| behind_run(x)).collect())
            } else if i > 0 {
                (format!("{} in {}'s queue", ordinal(i + 1), p.project), vec![behind_run(queued[i - 1])])
            } else {
                ("starting".to_string(), vec![])
            };
            out.push(Waiting {
                kind: kind_of(r).into(),
                issue: r.issue.clone(),
                project: Some(p.project.to_string()),
                since: r.queued_at,
                why,
                place: Some(i + 1),
                behind,
            });
        }
    }
    out
}

/// The scheduler's own reason for a queued run (NIT-291's `waiting_why`), and what it names or
/// implies the run waits behind: the request ahead of it in its repo, the runs holding its repo's
/// lanes or the machine's slots, or the requests the free slots go to. The runs in the slots'
/// parenthesis move to `behind`, where each one opens.
fn scheduler_says(text: &str, repo_running: &[&Run], running_anywhere: &[&Run], repos: &[RepoQueue]) -> (String, Vec<Behind>) {
    let why = text.strip_prefix("waiting: ").unwrap_or(text).to_string();
    let find = |id: i64| repos.iter().flat_map(|p| p.queue.iter()).find(|r| r.id == id);
    let ids = |s: &str| -> Vec<i64> { s.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect() };
    if let Some(rest) = why.split_once("behind request ").map(|(_, r)| r) {
        let behind = ids(rest).into_iter().take(1).filter_map(find).map(behind_run).collect();
        return (why, behind);
    }
    if why.contains("lanes are busy") {
        return (why, repo_running.iter().map(|r| behind_run(r)).collect());
    }
    if why.contains("machine slots in use") {
        let short = why.split_once(" (").map(|(a, _)| a.to_string()).unwrap_or(why);
        return (short, running_anywhere.iter().map(|r| behind_run(r)).collect());
    }
    if let Some((head, rest)) = why.split_once("requests ahead (") {
        let behind = ids(rest).into_iter().filter_map(find).map(behind_run).collect();
        return (format!("{head}requests ahead"), behind);
    }
    (why, vec![])
}

/// The project of an issue key: "NIT" for "NIT-12".
fn project_of(key: &str) -> Option<String> {
    key.rsplit_once('-').map(|(p, _)| p.to_uppercase())
}

/// The holder of `repo`'s land lock, as land writes it ("<holder> pid <n> since <time>"), and when
/// the lock was taken.
fn land_lock(repo: &str) -> Option<(String, Option<i64>)> {
    let path = crate::land::lock_path(Path::new(repo)).ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let holder = text.lines().next()?.split(" pid ").next()?.trim().to_string();
    let since = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
    Some((holder, since))
}

/// Submit jobs not started: each waits for its issue's previous submit to finish.
fn submit_waits(db: &Db) -> Result<Vec<Waiting>> {
    let rows: Vec<(i64, String, String, i64)> = db
        .conn
        .prepare("SELECT id, issue_key, state, queued_at FROM submit_job WHERE state IN ('queued', 'running') ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, issue, state, queued_at) in &rows {
        if state != "queued" {
            continue;
        }
        let before = rows.iter().find(|(other, k, s, _)| other < id && k.eq_ignore_ascii_case(issue) && s == "running");
        let (why, behind) = match before {
            Some((b, _, _, at)) => ("waits for the issue's previous submit".to_string(), vec![Behind { what: format!("submit job {b}"), issue: Some(issue.clone()), since: Some(*at) }]),
            None => ("starting".to_string(), vec![]),
        };
        out.push(Waiting { kind: "submit".into(), issue: Some(issue.clone()), project: project_of(issue), since: *queued_at, why, place: None, behind });
    }
    Ok(out)
}

/// Changes waiting in a review queue for a batch to form (shadow ones decide nothing, so not those).
fn review_waits(db: &Db) -> Result<Vec<Waiting>> {
    let rows: Vec<(String, String, i64)> = db
        .conn
        .prepare("SELECT issue_key, reviewer, queued_at FROM review_queue WHERE state = 'waiting' AND shadow = 0 ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let reviewing: Vec<(i64, String, i64)> = db
        .conn
        .prepare("SELECT id, issues, started_at FROM review_batch WHERE state = 'reviewing' AND shadow = 0 ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (i, (issue, reviewer, at)) in rows.iter().enumerate() {
        let others = rows.len() - 1;
        let why = format!("waits for a review batch by {reviewer} ({others} other change{} waiting)", if others == 1 { "" } else { "s" });
        let behind = reviewing
            .iter()
            .map(|(id, issues, since)| {
                let keys: Vec<String> = serde_json::from_str(issues).unwrap_or_default();
                Behind { what: format!("review batch {id}: {}", keys.join(", ")), issue: None, since: Some(*since) }
            })
            .collect();
        out.push(Waiting { kind: "review".into(), issue: Some(issue.clone()), project: project_of(issue), since: *at, why, place: Some(i + 1), behind });
    }
    Ok(out)
}

/// Changes queued to land: behind a queue run, behind a land holding the lock, or waiting for the
/// batch window.
fn land_waits(db: &Db) -> Result<Vec<Waiting>> {
    let rows: Vec<(i64, String, String, String, i64)> = db
        .conn
        .prepare("SELECT id, repo, issue_key, state, queued_at FROM land_queue WHERE state IN ('queued', 'running') ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (id, repo, issue, state, at) in &rows {
        if state != "queued" {
            continue;
        }
        let place = rows.iter().filter(|(other, r, _, _, _)| r == repo && other < id).count() + 1;
        let runs: Vec<i64> = db
            .conn
            .prepare("SELECT id FROM land_run WHERE repo = ?1 AND ended_at IS NULL ORDER BY id")?
            .query_map(params![repo], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut behind = Vec::new();
        for run in runs {
            if let Some(r) = db.land_run(run)? {
                behind.push(Behind { what: format!("land queue run {}: {}", r.id, r.issues.join(", ")), issue: None, since: Some(r.started_at) });
            }
        }
        let why = if !behind.is_empty() {
            format!("{} in the land queue, behind the run landing now", ordinal(place))
        } else if let Some((holder, since)) = land_lock(repo) {
            behind.push(Behind { what: format!("{holder}, holding the land lock"), issue: holder.split_whitespace().next().map(str::to_string), since });
            format!("{} in the land queue, behind a land holding the lock", ordinal(place))
        } else {
            format!("{} in the land queue, waiting for more changes to land with", ordinal(place))
        };
        out.push(Waiting { kind: "land_queue".into(), issue: Some(issue.clone()), project: project_of(issue), since: *at, why, place: Some(place), behind });
    }
    Ok(out)
}

/// Every request waiting now: the lane runs (`lane_waits`), submits, review batches and land
/// queues; longest waiting first.
pub fn waits(db: &Db, lane_runs: Vec<Waiting>) -> Result<Waits> {
    let mut waiting = lane_runs;
    waiting.extend(submit_waits(db)?);
    waiting.extend(review_waits(db)?);
    waiting.extend(land_waits(db)?);
    waiting.sort_by_key(|w| w.since);
    Ok(Waits { waiting })
}

/// `waits` for the app: the lanes overview read here too.
pub fn overview(config: &crate::Config, db: &Db) -> Result<Waits> {
    let lanes = lanes::overview(config, db)?;
    waits(db, lane_waits(&lanes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: i64, state: &str, issue: &str, kind: &str, queued_at: i64) -> Run {
        Run {
            id,
            project: "APP".into(),
            repo: "C:/Code/my-app".into(),
            issue: Some(issue.into()),
            by: "claude:s".into(),
            kind: kind.into(),
            snapshot: "s".into(),
            tree: "t".into(),
            base: "b".into(),
            command: "cargo test".into(),
            state: state.into(),
            lane: (state == "running").then_some(1),
            executor: "local".into(),
            queued_at,
            started_at: (state == "running").then_some(queued_at + 5),
            ended_at: None,
            exit_code: None,
            timed_out: false,
            setup_ran: false,
            files_switched: None,
            log: None,
            output: String::new(),
            error: None,
            waiting_for: None,
            waiting_memory: None,
            peak_memory: None,
            waiting_why: None,
        }
    }

    #[test]
    fn the_schedulers_own_reason_wins_and_names_what_it_waits_behind() {
        let mut slots = run(4, "queued", "APP-4", "gate", 30);
        slots.waiting_why = Some("waiting: 2 of 2 machine slots in use (APP-1 gate 2 m, APP-2 land 0 m)".into());
        let mut repo = run(5, "queued", "APP-5", "run", 40);
        repo.waiting_why = Some("waiting: behind request 4 of the same repo".into());
        let mut ahead = run(6, "queued", "APP-6", "run", 50);
        ahead.waiting_why = Some("waiting: the free machine slots go to requests ahead (4, 5)".into());
        let queue = [run(1, "running", "APP-1", "gate", 10), run(2, "running", "APP-2", "land", 12), slots, repo, ahead];
        let w = lane_waits_of(&[RepoQueue { project: "APP", lanes: 3, queue: &queue }], Some((2, 2)));
        assert_eq!(w[0].why, "2 of 2 machine slots in use", "the runs move to behind, where each opens");
        assert_eq!(w[0].behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["run 1: APP-1 gate", "run 2: APP-2 land"]);
        assert_eq!((w[1].why.as_str(), w[1].behind[0].what.as_str()), ("behind request 4 of the same repo", "run 4: APP-4 gate"));
        assert_eq!((w[2].why.as_str(), w[2].behind.len()), ("the free machine slots go to requests ahead", 2));
    }

    #[test]
    fn a_queued_lane_run_says_why_and_behind_what() {
        let mut memory = run(5, "queued", "APP-5", "run", 40);
        memory.waiting_memory = Some("waiting for memory: 3.1 GB free, needs 6".into());
        let queue = [run(1, "running", "APP-1", "gate", 10), run(2, "running", "APP-2", "land", 12), run(3, "queued", "APP-3", "run", 20), run(4, "queued", "APP-4", "gate", 30), memory];
        let w = lane_waits_of(&[RepoQueue { project: "APP", lanes: 2, queue: &queue }], Some((2, 4)));
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].kind.as_str(), w[0].why.as_str(), w[0].place), ("run", "all 2 of its lanes are busy in APP", Some(1)));
        assert_eq!(w[0].behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["run 1: APP-1 gate", "run 2: APP-2 land"]);
        assert_eq!(w[0].behind[0].since, Some(15), "held since it started");
        assert_eq!(w[2].why, "waiting for memory: 3.1 GB free, needs 6");
        // A free lane, but every build slot of the machine in use: behind every run going.
        let queue = [run(1, "running", "APP-1", "gate", 10), run(3, "queued", "APP-3", "run", 20), run(4, "queued", "APP-4", "gate", 30)];
        let w = lane_waits_of(&[RepoQueue { project: "APP", lanes: 2, queue: &queue }], Some((3, 3)));
        assert_eq!(w[0].why, "3 of 3 build slots in use");
        // Nothing full: the next starts, the one after waits behind it.
        let w = lane_waits_of(&[RepoQueue { project: "APP", lanes: 3, queue: &queue }], Some((1, 3)));
        assert_eq!((w[0].why.as_str(), w[1].why.as_str(), w[1].behind[0].what.as_str()), ("starting", "2nd in APP's queue", "run 3: APP-3 run"));
    }

    #[test]
    fn ordinals() {
        assert_eq!([1, 2, 3, 4, 11, 12, 13, 21, 22].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd"]);
    }

    #[test]
    fn submits_reviews_and_lands_say_what_they_wait_behind() {
        let dir = crate::paths::unique_temp("nitely-waits", "");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("t.db")).unwrap();
        let exec = |sql: &str| db.conn.execute_batch(sql).unwrap();
        let repo = "C:/Code/my-app";
        exec(&format!(
            r#"INSERT INTO submit_job (id, issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at, started_at) VALUES
                 (30, 'APP-1', 'w', 'b', 'h', 'claude:s', 0, 0, 'running', 100, 101),
                 (31, 'APP-1', 'w', 'b', 'h', 'claude:s', 0, 0, 'queued', 120, NULL),
                 (32, 'APP-2', 'w', 'b', 'h', 'claude:s', 0, 0, 'queued', 130, NULL);
               INSERT INTO review_queue (issue_key, repo, reviewer, note, title, branch, worktree, base, head, lines, files, pid, actor, state, queued_at, shadow) VALUES
                 ('APP-3', '{repo}', 'codex', '', 'Three', 'b', 'w', 'x', 'y', 5, '[]', 1, 'claude:s', 'waiting', 90, 0),
                 ('APP-4', '{repo}', 'codex', '', 'Four', 'b', 'w', 'x', 'y', 5, '[]', 1, 'claude:s', 'waiting', 95, 1);
               INSERT INTO land_run (id, repo, started_at, issues) VALUES (7, '{repo}', 140, 'APP-5,APP-6');
               INSERT INTO land_queue (id, repo, issue_key, requested_by, state, queued_at, run_id) VALUES
                 (1, '{repo}', 'APP-5', 'claude:s', 'running', 135, 7),
                 (2, '{repo}', 'APP-7', 'claude:s', 'queued', 150, NULL);"#
        ));
        let w = waits(&db, vec![]).unwrap();
        let by = |key: &str| w.waiting.iter().find(|x| x.issue.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} in {w:?}"));
        assert_eq!(w.waiting.iter().map(|x| x.since).collect::<Vec<_>>(), [90, 120, 130, 150], "longest waiting first");
        let second = by("APP-1");
        assert_eq!((second.kind.as_str(), second.why.as_str(), second.behind[0].what.as_str()), ("submit", "waits for the issue's previous submit", "submit job 30"));
        assert_eq!(by("APP-2").why, "starting");
        assert_eq!(by("APP-3").why, "waits for a review batch by codex (0 other changes waiting)");
        assert!(w.waiting.iter().all(|x| x.issue.as_deref() != Some("APP-4")), "a shadow change decides nothing, so it waits for nothing");
        let land = by("APP-7");
        assert_eq!((land.kind.as_str(), land.place), ("land_queue", Some(2)));
        assert_eq!(land.why, "2nd in the land queue, behind the run landing now");
        assert_eq!(land.behind[0].what, "land queue run 7: APP-5, APP-6");
        std::fs::remove_dir_all(&dir).ok();
    }
}
