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

/// One repo's lanes as `lane_waits` reads them: its project and its queue (running and queued
/// requests).
pub struct RepoQueue<'a> {
    pub project: &'a str,
    pub queue: &'a [Run],
}

/// Why each queued lane run waits, from the lanes overview.
pub fn lane_waits(o: &lanes::Overview) -> Vec<Waiting> {
    let repos: Vec<RepoQueue> = o.projects.iter().map(|p| RepoQueue { project: &p.lanes.project, queue: &p.lanes.queue }).collect();
    lane_waits_of(&repos)
}

/// Why each queued lane run waits: the scheduler's own reason (NIT-291's `waiting_why`), with what
/// it waits behind. One the scheduler gave no reason for says only its place in its queue: the
/// reason is the scheduler's alone, never worked out a second time here.
pub fn lane_waits_of(repos: &[RepoQueue]) -> Vec<Waiting> {
    let running_anywhere: Vec<&Run> = repos.iter().flat_map(|p| p.queue.iter()).filter(|r| r.state == "running").collect();
    let mut out = Vec::new();
    for p in repos {
        let running: Vec<&Run> = p.queue.iter().filter(|r| r.state == "running").collect();
        let queued: Vec<&Run> = p.queue.iter().filter(|r| r.state == "queued").collect();
        for (i, r) in queued.iter().enumerate() {
            let (why, behind): (String, Vec<Behind>) = match &r.waiting_why {
                Some(w) => scheduler_says(w, &running, &running_anywhere, repos),
                None => (format!("{} in {}'s queue", ordinal(i + 1), p.project), vec![]),
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

/// The land holding the lock of `repo`, if one does and its process runs: a land of one change
/// (a queue run has its own row on the board), what it lands and since when, with its gate's run.
fn landing_now(db: &Db, repo: &str) -> Result<Option<Waiting>> {
    let Some((holder, since)) = land_lock(repo) else { return Ok(None) };
    if holder.ends_with("(land queue)") || !crate::batches::land_running(repo) {
        return Ok(None);
    }
    let key = holder.split_whitespace().next().map(str::to_string);
    let gate: Option<(i64, Option<u32>, i64)> = match &key {
        Some(k) => db
            .conn
            .query_row(
                "SELECT id, lane, COALESCE(started_at, queued_at) FROM lane_run WHERE issue_key = ?1 COLLATE NOCASE AND kind = 'land' AND state IN ('queued', 'running') ORDER BY id DESC LIMIT 1",
                [k],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok(),
        None => None,
    };
    let behind = gate
        .map(|(id, lane, at)| {
            let lane = lane.map(|l| format!(" in lane {l}")).unwrap_or_default();
            vec![Behind { what: format!("run {id}: its gate{lane}"), issue: key.clone(), since: Some(at) }]
        })
        .unwrap_or_default();
    Ok(Some(Waiting {
        kind: "landing".into(),
        project: key.as_deref().and_then(project_of),
        issue: key,
        since: since.unwrap_or_else(crate::db::now),
        why: "landing now: it holds the land lock, so other lands of this repo wait".into(),
        place: None,
        behind,
    }))
}

/// Lands refused because another land held the lock (a `land_refused` event, NIT-335), whose issue
/// has done nothing since (no new submit, gate or land) and still waits to land. Each waits behind
/// the land holding the lock now, behind a lock whose process is gone (it stays until a land
/// breaks it), or says the lock is free and its agent can land it.
fn refused_waits(db: &Db) -> Result<Vec<Waiting>> {
    let rows: Vec<(String, i64, Option<String>)> = db
        .conn
        .prepare(
            "SELECT e.issue_key, e.ts, e.cwd FROM event e
             WHERE e.type = 'land_refused'
               AND e.id = (SELECT MAX(id) FROM event x WHERE x.issue_key = e.issue_key COLLATE NOCASE
                           AND x.type IN ('land_refused', 'landed', 'land_failed', 'submit', 'submit_queued', 'gate_failed', 'gate_passed'))
             ORDER BY e.id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (key, at, repo) in rows {
        if !db.issue(&key)?.is_some_and(|i| i.state == "In Review" || i.state == "Approved") {
            continue;
        }
        let lock = repo.as_deref().and_then(land_lock);
        let live = repo.as_deref().is_some_and(crate::batches::land_running);
        let holds = |holder: &str| holder.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case(&key));
        let (why, behind) = match lock {
            Some((holder, _)) if holds(&holder) => continue,
            Some((holder, since)) if live => (
                "its land was refused: another land holds the lock; its agent lands again once it is free".to_string(),
                vec![Behind { what: format!("{holder}, holding the land lock"), issue: holder.split_whitespace().next().map(str::to_string), since }],
            ),
            // The lock file outlived its land: every land is refused until one breaks it.
            Some((holder, since)) => (
                "its land was refused, and the land lock's process is gone: land again with break_lock".to_string(),
                vec![Behind { what: format!("{holder}, a land lock whose process is gone"), issue: holder.split_whitespace().next().map(str::to_string), since }],
            ),
            // Free a long while: its agent was told to land again, and the row has said so.
            None if crate::db::now() - at > REFUSED_FOR_SECS => continue,
            None => ("its land was refused while another land held the lock; the lock is free now: land it again".to_string(), vec![]),
        };
        out.push(Waiting { kind: "land_refused".into(), project: project_of(&key), issue: Some(key), since: at, why, place: None, behind });
    }
    Ok(out)
}

/// How long a refused land counts as waiting once the lock is free: its agent was told to land again.
const REFUSED_FOR_SECS: i64 = 2 * 3600;

/// Submit jobs not started: each waits for its issue's previous submit to finish.
fn submit_waits(db: &Db) -> Result<Vec<Waiting>> {
    // A job whose process is gone waits for nothing: the next submit settles it.
    let rows: Vec<(i64, String, String, i64)> = db
        .conn
        .prepare("SELECT id, issue_key, state, queued_at, pid FROM submit_job WHERE state IN ('queued', 'running') ORDER BY id")?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?), r.get::<_, Option<i64>>(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, pid)| pid.is_none_or(alive))
        .map(|(row, _)| row)
        .collect();
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

/// Whether the process `pid` runs (this one does).
fn alive(pid: i64) -> bool {
    pid as u32 == std::process::id() || crate::submit_job::process_runs(pid as u32)
}

/// Changes waiting in a review queue for a batch to form, each with the others of its own queue (a
/// repository's changes for one reviewer, as the batcher keeps them) and the batch of that queue
/// under review. Shadow changes decide nothing, and a change whose submit stopped waits for
/// nothing, so neither is listed; nor is a batch whose reviewing process is gone.
fn review_waits(db: &Db) -> Result<Vec<Waiting>> {
    let rows: Vec<(String, String, String, i64, i64)> = db
        .conn
        .prepare("SELECT issue_key, repo, reviewer, queued_at, pid FROM review_queue WHERE state = 'waiting' AND shadow = 0 ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|row: &(String, String, String, i64, i64)| alive(row.4))
        .collect();
    let reviewing: Vec<(i64, String, String, String, i64)> = db
        .conn
        .prepare("SELECT id, repo, reviewer, issues, started_at, pid FROM review_batch WHERE state = 'reviewing' AND shadow = 0 ORDER BY id")?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?), r.get::<_, i64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, pid)| alive(*pid))
        .map(|(b, _)| b)
        .collect();
    let mut out = Vec::new();
    for (issue, repo, reviewer, at, _) in &rows {
        let queue: Vec<&String> = rows.iter().filter(|(_, r, v, _, _)| r == repo && v == reviewer).map(|(k, ..)| k).collect();
        let place = queue.iter().position(|k| *k == issue).unwrap_or(0) + 1;
        let others = queue.len() - 1;
        let why = format!("waits for a review batch by {reviewer} ({others} other change{} waiting)", if others == 1 { "" } else { "s" });
        let behind = reviewing
            .iter()
            .filter(|(_, r, v, _, _)| r == repo && v == reviewer)
            .map(|(id, _, _, issues, since)| {
                let keys: Vec<String> = serde_json::from_str(issues).unwrap_or_default();
                Behind { what: format!("review batch {id}: {}", keys.join(", ")), issue: None, since: Some(*since) }
            })
            .collect();
        out.push(Waiting { kind: "review".into(), issue: Some(issue.clone()), project: project_of(issue), since: *at, why, place: Some(place), behind });
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
        // A run whose process died stays unended in its row until the next run: a run is landing
        // now only while the queue itself holds the lock and its process runs.
        let lock = land_lock(repo);
        let lock_alive = crate::batches::land_running(repo);
        let queue_holds = lock_alive && lock.as_ref().is_some_and(|(holder, _)| holder.ends_with("(land queue)"));
        let mut behind = Vec::new();
        for run in runs.into_iter().filter(|_| queue_holds) {
            if let Some(r) = db.land_run(run)? {
                behind.push(Behind { what: format!("land queue run {}: {}", r.id, r.issues.join(", ")), issue: None, since: Some(r.started_at) });
            }
        }
        let window = || format!("{} in the land queue, waiting for more changes to land with", ordinal(place));
        let why = if !behind.is_empty() {
            format!("{} in the land queue, behind the run landing now", ordinal(place))
        } else {
            match lock {
                // Left by a land whose process is gone: no land takes it until one breaks it.
                Some((holder, since)) if !lock_alive => {
                    behind.push(Behind { what: format!("{holder}, a land lock whose process is gone"), issue: holder.split_whitespace().next().map(str::to_string), since });
                    format!("{} in the land queue, behind a stale land lock: land again with break_lock", ordinal(place))
                }
                // The queue holds the lock itself while it waits for its window: that is the wait.
                Some((holder, _)) if holder.ends_with("(land queue)") => window(),
                Some((holder, since)) => {
                    behind.push(Behind { what: format!("{holder}, holding the land lock"), issue: holder.split_whitespace().next().map(str::to_string), since });
                    format!("{} in the land queue, behind a land holding the lock", ordinal(place))
                }
                None => window(),
            }
        };
        out.push(Waiting { kind: "land_queue".into(), issue: Some(issue.clone()), project: project_of(issue), since: *at, why, place: Some(place), behind });
    }
    Ok(out)
}

/// Every request waiting now: the lane runs (`lane_waits`), submits, review batches and land
/// queues; longest waiting first.
pub fn waits(db: &Db, lane_runs: Vec<Waiting>, repos: &[String]) -> Result<Waits> {
    let mut waiting = lane_runs;
    waiting.extend(submit_waits(db)?);
    waiting.extend(review_waits(db)?);
    waiting.extend(land_waits(db)?);
    waiting.extend(refused_waits(db)?);
    for repo in repos {
        waiting.extend(landing_now(db, repo)?);
    }
    waiting.sort_by_key(|w| w.since);
    Ok(Waits { waiting })
}

/// `waits` for the app: the lanes overview read here too.
pub fn overview(config: &crate::Config, db: &Db) -> Result<Waits> {
    let lanes = lanes::overview(config, db)?;
    let repos: Vec<String> = config.repos.iter().map(|r| r.root.clone()).collect();
    waits(db, lane_waits(&lanes), &repos)
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
        let w = lane_waits_of(&[RepoQueue { project: "APP", queue: &queue }]);
        assert_eq!(w[0].why, "2 of 2 machine slots in use", "the runs move to behind, where each opens");
        assert_eq!(w[0].behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["run 1: APP-1 gate", "run 2: APP-2 land"]);
        assert_eq!((w[1].why.as_str(), w[1].behind[0].what.as_str()), ("behind request 4 of the same repo", "run 4: APP-4 gate"));
        assert_eq!((w[2].why.as_str(), w[2].behind.len()), ("the free machine slots go to requests ahead", 2));
    }

    #[test]
    fn a_queued_lane_run_says_the_schedulers_reason_and_its_place() {
        let mut busy = run(3, "queued", "APP-3", "run", 20);
        busy.waiting_why = Some("waiting: all 2 of its repo's lanes are busy".into());
        let mut memory = run(5, "queued", "APP-5", "run", 40);
        memory.waiting_why = Some("waiting for memory: 3.1 GB free, needs 6".into());
        let queue = [run(1, "running", "APP-1", "gate", 10), run(2, "running", "APP-2", "land", 12), busy, run(4, "queued", "APP-4", "gate", 30), memory];
        let w = lane_waits_of(&[RepoQueue { project: "APP", queue: &queue }]);
        assert_eq!(w.len(), 3);
        assert_eq!((w[0].kind.as_str(), w[0].why.as_str(), w[0].place), ("run", "all 2 of its repo's lanes are busy", Some(1)));
        assert_eq!(w[0].behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["run 1: APP-1 gate", "run 2: APP-2 land"]);
        assert_eq!(w[0].behind[0].since, Some(15), "held since it started");
        assert_eq!((w[1].why.as_str(), w[1].behind.len()), ("2nd in APP's queue", 0), "no reason from the scheduler: only its place, nothing guessed");
        assert_eq!(w[2].why, "waiting for memory: 3.1 GB free, needs 6");
    }

    #[test]
    fn a_refused_land_waits_behind_the_land_holding_the_lock_and_that_land_has_its_row() {
        use crate::issues::{Actor, NewIssue};
        let dir = crate::paths::unique_temp("nitely-waits-refused", "");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("t.db")).unwrap();
        db.create_project("APP", "Acme").unwrap();
        let actor = Actor::Agent("claude:s".into());
        let new = |title: &str| NewIssue { project: "APP".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() };
        let landing = db.create_issue(&new("Landing"), &actor).unwrap().key;
        let refused = db.create_issue(&new("Refused"), &actor).unwrap().key;
        db.conn.execute("UPDATE issue SET state = 'In Review', state_type = 'started' WHERE key = ?1", [&refused]).unwrap();
        let root = dir.join("my-app");
        crate::test_repo::init(&root, &[("a.txt", "a")]);
        let repo = root.to_string_lossy().into_owned();
        let lock = crate::land::lock_path(&root).unwrap();
        std::fs::write(&lock, format!("{landing} pid {} since now\n", std::process::id())).unwrap();
        db.conn
            .execute(
                "INSERT INTO lane_run (project, repo, issue_key, requested_by, kind, snapshot, tree, base, command, state, executor, queued_at, started_at, pid, priority, lane, lane_count) VALUES ('APP', ?1, ?2, 'claude:s', 'land', 's', 't', 'b', 'gate', 'running', 'local', 100, 101, ?3, 0, 1, 1)",
                rusqlite::params![repo, landing, std::process::id()],
            )
            .unwrap();
        db.log_event(&actor, Some(&refused), Some(&repo), "land_refused", "Land refused: another land holds the lock").unwrap();
        let w = waits(&db, vec![], &[repo.clone()]).unwrap();
        let by = |key: &str| w.waiting.iter().find(|x| x.issue.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} in {w:?}")).clone();
        let row = by(&landing);
        assert_eq!(row.kind, "landing");
        assert!(row.behind[0].what.starts_with("run ") && row.behind[0].what.ends_with("its gate in lane 1"), "{:?}", row.behind);
        let waits_for = by(&refused);
        assert_eq!(waits_for.kind, "land_refused");
        assert_eq!(waits_for.behind[0].issue.as_deref(), Some(landing.as_str()), "behind the land holding the lock");
        // The lock is free: it says the agent can land again.
        std::fs::remove_file(&lock).unwrap();
        let w = waits(&db, vec![], &[repo.clone()]).unwrap();
        assert!(w.waiting.iter().any(|x| x.issue.as_deref() == Some(refused.as_str()) && x.why.ends_with("the lock is free now: land it again")), "{w:?}");
        assert!(w.waiting.iter().all(|x| x.kind != "landing"), "nobody lands now");
        // The lock file outlived its land: refused until a land breaks it, and it says so.
        std::fs::write(&lock, format!("{landing} pid 4000000000 since then\n")).unwrap();
        let w = waits(&db, vec![], &[repo.clone()]).unwrap();
        assert!(w.waiting.iter().any(|x| x.issue.as_deref() == Some(refused.as_str()) && x.why.ends_with("land again with break_lock")), "{w:?}");
        std::fs::remove_file(&lock).unwrap();
        // Handed in again since (even only queued): nothing to wait for from that refusal.
        db.log_event(&actor, Some(&refused), Some(&repo), "submit_queued", "Submitted for the gate and review").unwrap();
        assert!(waits(&db, vec![], &[repo]).unwrap().waiting.is_empty());
        std::fs::remove_dir_all(&dir).ok();
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
        // A repo whose land lock a live process (this test) holds for a queue run.
        let root = dir.join("my-app");
        crate::test_repo::init(&root, &[("a.txt", "a")]);
        let lock = crate::land::lock_path(&root).unwrap();
        let hold = |holder: &str| std::fs::write(&lock, format!("{holder} pid {} since now\n", std::process::id())).unwrap();
        hold("APP-5 (land queue)");
        let repo = crate::paths::normalize(&root.to_string_lossy());
        let pid = std::process::id();
        exec(&format!(
            r#"INSERT INTO submit_job (id, issue_key, worktree, branch, head, requested_by, skip_review, local_only, state, queued_at, started_at) VALUES
                 (30, 'APP-1', 'w', 'b', 'h', 'claude:s', 0, 0, 'running', 100, 101),
                 (31, 'APP-1', 'w', 'b', 'h', 'claude:s', 0, 0, 'queued', 120, NULL),
                 (32, 'APP-2', 'w', 'b', 'h', 'claude:s', 0, 0, 'queued', 130, NULL);
               INSERT INTO review_queue (issue_key, repo, reviewer, note, title, branch, worktree, base, head, lines, files, pid, actor, state, queued_at, shadow) VALUES
                 ('APP-3', '{repo}', 'codex', '', 'Three', 'b', 'w', 'x', 'y', 5, '[]', {pid}, 'claude:s', 'waiting', 90, 0),
                 ('APP-4', '{repo}', 'codex', '', 'Four', 'b', 'w', 'x', 'y', 5, '[]', {pid}, 'claude:s', 'waiting', 95, 1),
                 ('APP-8', 'C:/Code/other', 'codex', '', 'Other repo', 'b', 'w', 'x', 'y', 5, '[]', {pid}, 'claude:s', 'waiting', 96, 0),
                 ('APP-9', '{repo}', 'codex', '', 'Its submit stopped', 'b', 'w', 'x', 'y', 5, '[]', 4000000000, 'claude:s', 'waiting', 97, 0);
               INSERT INTO review_batch (id, repo, reviewer, issues, state, pid, started_at, shadow) VALUES
                 (12, '{repo}', 'codex', '["APP-10","APP-11"]', 'reviewing', {pid}, 80, 0),
                 (13, 'C:/Code/other', 'codex', '["APP-12","APP-13"]', 'reviewing', {pid}, 80, 0),
                 (14, '{repo}', 'codex', '["APP-14","APP-15"]', 'reviewing', 4000000000, 80, 0);
               INSERT INTO land_run (id, repo, started_at, issues) VALUES (7, '{repo}', 140, 'APP-5,APP-6');
               INSERT INTO land_queue (id, repo, issue_key, requested_by, state, queued_at, run_id) VALUES
                 (1, '{repo}', 'APP-5', 'claude:s', 'running', 135, 7),
                 (2, '{repo}', 'APP-7', 'claude:s', 'queued', 150, NULL);"#
        ));
        let w = waits(&db, vec![], &[]).unwrap();
        let by = |key: &str| w.waiting.iter().find(|x| x.issue.as_deref() == Some(key)).unwrap_or_else(|| panic!("{key} in {w:?}"));
        assert_eq!(w.waiting.iter().map(|x| x.since).collect::<Vec<_>>(), [90, 96, 120, 130, 150], "longest waiting first");
        let second = by("APP-1");
        assert_eq!((second.kind.as_str(), second.why.as_str(), second.behind[0].what.as_str()), ("submit", "waits for the issue's previous submit", "submit job 30"));
        assert_eq!(by("APP-2").why, "starting");
        // Each review queue is one repository's for one reviewer: another repo's change and batch are
        // not in it, and neither a change whose submit stopped nor a batch whose reviewer died.
        let three = by("APP-3");
        assert_eq!(three.why, "waits for a review batch by codex (0 other changes waiting)");
        assert_eq!(three.behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["review batch 12: APP-10, APP-11"]);
        assert_eq!(by("APP-8").behind[0].what, "review batch 13: APP-12, APP-13");
        assert!(w.waiting.iter().all(|x| x.issue.as_deref() != Some("APP-4")), "a shadow change decides nothing, so it waits for nothing");
        assert!(w.waiting.iter().all(|x| x.issue.as_deref() != Some("APP-9")), "nor does one whose submit stopped");
        let land = by("APP-7");
        assert_eq!((land.kind.as_str(), land.place), ("land_queue", Some(2)));
        assert_eq!(land.why, "2nd in the land queue, behind the run landing now");
        assert_eq!(land.behind[0].what, "land queue run 7: APP-5, APP-6");

        // The run is over (nothing unended): the queue holding the lock is its own window.
        exec("UPDATE land_run SET ended_at = 160 WHERE id = 7");
        let why = |w: &Waits| w.waiting.iter().find(|x| x.issue.as_deref() == Some("APP-7")).unwrap().why.clone();
        assert_eq!(why(&waits(&db, vec![], &[]).unwrap()), "2nd in the land queue, waiting for more changes to land with");
        // A land of one change holds it: behind that land.
        hold("APP-20");
        let w = waits(&db, vec![], &[]).unwrap();
        let land = w.waiting.iter().find(|x| x.issue.as_deref() == Some("APP-7")).unwrap();
        assert_eq!((land.why.as_str(), land.behind[0].what.as_str()), ("2nd in the land queue, behind a land holding the lock", "APP-20, holding the land lock"));
        // A crashed run left unended, and another land holds the lock now: the run is not landing.
        exec("UPDATE land_run SET ended_at = NULL WHERE id = 7");
        assert_eq!(why(&waits(&db, vec![], &[]).unwrap()), "2nd in the land queue, behind a land holding the lock");
        // The crash left its lock too, its process gone: stuck until a land breaks it, and it says so.
        std::fs::write(&lock, "APP-5 (land queue) pid 4000000000 since then\n").unwrap();
        let w = waits(&db, vec![], &[]).unwrap();
        let land = w.waiting.iter().find(|x| x.issue.as_deref() == Some("APP-7")).unwrap();
        assert_eq!(land.why, "2nd in the land queue, behind a stale land lock: land again with break_lock");
        assert_eq!(land.behind.iter().map(|b| b.what.as_str()).collect::<Vec<_>>(), ["APP-5 (land queue), a land lock whose process is gone"], "not the dead run");
        std::fs::remove_dir_all(&dir).ok();
    }
}
