//! Batched review (docs/batching.md, NIT-216): with `[review] batch`, a small change that a
//! background submit hands in waits in its repository's review queue for others, and one reviewer
//! call reviews them together. Each change still gets a review of its own, as if reviewed alone:
//! its own row, findings, verdict and next step, so rounds (NIT-161), accepted findings (NIT-191)
//! and land's check of the reviewed patch work as they do for a single review.
//!
//! There is no resident process. Each waiting submit polls its row. The first one to find the
//! queue's batch due (full, its oldest change past the window, or with `busy` no batch under
//! review) takes it under the queue's lock and runs the reviewer for all of them, then writes each
//! change's review into its row. Whatever cannot be reviewed in the batch (the reviewer failed, its
//! reply left a change unattributed or unfinished, the process running it died) goes back to its
//! own submit as "alone", which then reviews it by itself.

use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::review::{BatchChange, BatchInfo, BatchMode, Choice, Finding, PreviousRound, Review, ReviewRules};

/// How long the oldest change in a queue waits for others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    After(Duration),
    /// Until no batch of the repository is being reviewed.
    Busy,
}

impl Window {
    /// "5m", "30s", "2h", "0" (seconds when no unit is given), or "busy".
    pub fn parse(text: &str) -> Result<Window> {
        let s = text.trim().to_lowercase();
        if s == "busy" {
            return Ok(Window::Busy);
        }
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (n, unit) = (&s[..split], s[split..].trim());
        let bad = || format!("`{text}` is not a window: use a duration like \"5m\" or \"30s\", or \"busy\"");
        let n: u64 = n.parse().with_context(bad)?;
        let seconds = match unit {
            "" | "s" | "sec" | "secs" => n,
            "m" | "min" | "mins" => n * 60,
            "h" => n * 3600,
            _ => bail!(bad()),
        };
        Ok(Window::After(Duration::from_secs(seconds)))
    }

    fn describe(&self) -> String {
        match self {
            Window::Busy => "until the reviewer is free".into(),
            Window::After(d) if d.as_secs() % 60 == 0 && d.as_secs() > 0 => format!("at most {} min", d.as_secs() / 60),
            Window::After(d) => format!("at most {} s", d.as_secs()),
        }
    }
}

/// The submit's change was replaced in the queue by a newer submit of its issue.
#[derive(Debug)]
pub struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "replaced by a newer submit of the issue while it waited for a review batch")
    }
}

impl std::error::Error for Superseded {}

/// What a change that waited in the queue gets.
#[derive(Debug)]
pub enum Outcome {
    /// Its review from the batch.
    Reviewed(Review),
    /// It is to be reviewed by itself after all, and why.
    Alone(String),
}

/// A change submit wants reviewed.
pub struct Candidate<'a> {
    pub issue: &'a IssueRecord,
    /// The repository's root: a batch holds one repository's changes.
    pub repo: &'a Path,
    pub worktree: &'a Path,
    pub branch: &'a str,
    /// What its diff is measured against (`playbook::diff_base`).
    pub base: &'a str,
    pub files: &'a [String],
    pub lines: usize,
    pub choice: &'a Choice,
}

/// Lines added and removed against `base`.
pub fn changed_lines(worktree: &Path, base: &str) -> Result<usize> {
    let numstat = crate::playbook::git(worktree, &["diff", "--numstat", "--no-renames", &format!("{base}...HEAD")])?;
    Ok(numstat
        .lines()
        .map(|l| l.split_whitespace().take(2).map(|n| n.parse::<usize>().unwrap_or(0)).sum::<usize>())
        .sum())
}

/// Why a change is reviewed alone even though its project batches reviews, or None when it may
/// wait for a batch. `reasons` are what already sends it to a person.
#[allow(clippy::too_many_arguments)]
pub fn why_alone(
    db: &Db,
    rules: &ReviewRules,
    vendor: &str,
    choice: &Choice,
    reasons: &[String],
    files: &[String],
    lines: usize,
    previous: Option<&PreviousRound>,
    large_repo: bool,
    issue: &str,
) -> Result<Option<String>> {
    let why = if rules.batch == BatchMode::Off {
        "its project reviews each change alone"
    } else if !reasons.is_empty() || rules.independence_reason(&choice.reviewer, vendor, &choice.note).is_some() {
        "it waits for a person's approval anyway"
    } else if files.iter().any(|f| f == crate::playbook::PLAYBOOK_PATH) {
        "it changes the playbook"
    } else if previous.is_some_and(|p| p.findings.iter().any(|f| f.priority <= 1)) {
        "its previous review left P0 or P1 findings open, so it is checked against them alone"
    } else if last_flagged(db, issue)? {
        "its previous review raised a flag"
    } else if large_repo {
        "the project is in large-repo mode"
    } else if rules.batch == BatchMode::Small && (lines > rules.small_lines || files.len() > rules.small_files) {
        return Ok(Some(format!("it is not small ({lines} lines in {} files; small is at most {} in {})", files.len(), rules.small_lines, rules.small_files)));
    } else if rules.batch == BatchMode::All && lines > rules.max_lines {
        return Ok(Some(format!("it is larger than a batch may be ({lines} lines; at most {})", rules.max_lines)));
    } else {
        return Ok(None);
    };
    Ok(Some(why.to_string()))
}

fn last_flagged(db: &Db, issue: &str) -> Result<bool> {
    let flagged: Option<String> = db
        .conn
        .query_row("SELECT flagged FROM review WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts DESC, id DESC LIMIT 1", [issue], |r| r.get(0))
        .optional()?;
    Ok(flagged.is_some_and(|f| !matches!(f.trim(), "" | "[]")))
}

/// How often a waiting submit reads its row.
const POLL: Duration = Duration::from_millis(500);

/// Puts the change in its repository's queue and waits for its review: from a batch, or the word
/// that it is reviewed alone. Errs with `Superseded` when a newer submit of the issue replaced it.
pub fn review_in_batch(db: &Db, rules: &ReviewRules, c: &Candidate, actor: &Actor) -> Result<Outcome> {
    let window = match Window::parse(&rules.batch_window) {
        Ok(w) => w,
        Err(e) => return Ok(Outcome::Alone(format!("{e:#}"))),
    };
    let git = |args: &[&str]| crate::playbook::git(c.worktree, args).map(|o| o.trim().to_string());
    let base = git(&["rev-parse", c.base])?;
    let head = git(&["rev-parse", "HEAD"])?;
    let repo = crate::paths::normalize(&c.repo.to_string_lossy());
    let wt = c.worktree.to_string_lossy().into_owned();
    db.conn.execute(
        "INSERT INTO review_queue (issue_key, repo, reviewer, note, title, branch, worktree, base, head, lines, files, pid, actor, state, queued_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'waiting', ?14)",
        params![
            c.issue.key,
            repo,
            c.choice.reviewer,
            c.choice.note,
            c.issue.title,
            c.branch,
            wt,
            base,
            head,
            c.lines as i64,
            serde_json::to_string(c.files)?,
            std::process::id(),
            actor.id(),
            now()
        ],
    )?;
    let id = db.conn.last_insert_rowid();
    db.log_event(
        actor,
        Some(&c.issue.key),
        Some(&wt),
        "status",
        &format!("Waits to be reviewed in a batch with other small changes (up to {}, {})", rules.batch_max.max(2), window.describe()),
    )?;
    loop {
        let row = queued(db, id)?.context("the change's place in the review queue went missing")?;
        match row.state.as_str() {
            "done" => {
                let review: Review = serde_json::from_str(row.result.as_deref().unwrap_or("")).context("reading the change's review from its batch")?;
                return Ok(Outcome::Reviewed(review));
            }
            "alone" => return Ok(Outcome::Alone(row.why.unwrap_or_default())),
            "superseded" => bail!(Superseded),
            "abandoned" => return Ok(Outcome::Alone("it was taken out of the queue as abandoned".into())),
            "reviewing" => settle_dead_batch(db, row.batch_id.unwrap_or_default())?,
            _ => {
                if let Some((batch, rows)) = close_if_due(db, rules, window, &repo, &row.reviewer)? {
                    run_batch(db, rules, batch, &rows)?;
                    continue;
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// A row of the queue.
#[derive(Debug, Clone)]
struct Queued {
    id: i64,
    issue: String,
    reviewer: String,
    note: String,
    title: String,
    branch: String,
    worktree: String,
    base: String,
    head: String,
    lines: usize,
    files: Vec<String>,
    pid: u32,
    actor: String,
    state: String,
    batch_id: Option<i64>,
    queued_at: i64,
    result: Option<String>,
    why: Option<String>,
}

const COLUMNS: &str = "id, issue_key, reviewer, note, title, branch, worktree, base, head, lines, files, pid, actor, state, batch_id, queued_at, result, why";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Queued> {
    Ok(Queued {
        id: r.get(0)?,
        issue: r.get(1)?,
        reviewer: r.get(2)?,
        note: r.get(3)?,
        title: r.get(4)?,
        branch: r.get(5)?,
        worktree: r.get(6)?,
        base: r.get(7)?,
        head: r.get(8)?,
        lines: r.get::<_, i64>(9)? as usize,
        files: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
        pid: r.get::<_, i64>(11)? as u32,
        actor: r.get(12)?,
        state: r.get(13)?,
        batch_id: r.get(14)?,
        queued_at: r.get(15)?,
        result: r.get(16)?,
        why: r.get(17)?,
    })
}

fn queued(db: &Db, id: i64) -> Result<Option<Queued>> {
    Ok(db.conn.query_row(&format!("SELECT {COLUMNS} FROM review_queue WHERE id = ?1"), [id], row).optional()?)
}

/// Who submitted a queued change, as an actor for its timeline.
fn actor_of(row: &Queued) -> Actor {
    if row.actor == "human" { Actor::Human } else { Actor::Agent(row.actor.clone()) }
}

/// A batch whose reviewing process is gone sends its changes back to be reviewed alone.
fn settle_dead_batch(db: &Db, batch: i64) -> Result<()> {
    let pid: Option<i64> = db.conn.query_row("SELECT pid FROM review_batch WHERE id = ?1 AND state = 'reviewing'", [batch], |r| r.get(0)).optional()?;
    if let Some(pid) = pid.map(|p| p as u32).filter(|p| *p != std::process::id() && !crate::submit_job::process_runs(*p)) {
        let why = format!("the process reviewing its batch (pid {pid}) stopped");
        db.conn.execute("UPDATE review_batch SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1 AND state = 'reviewing'", params![batch, now(), why])?;
        db.conn.execute("UPDATE review_queue SET state = 'alone', why = ?2 WHERE batch_id = ?1 AND state = 'reviewing'", params![batch, why])?;
    }
    Ok(())
}

/// Takes the repository's next batch for `reviewer` when it is due, under the queue's lock: full,
/// its oldest change past the window, or (`busy`) no batch of the repository under review. A batch
/// of one is no batch: that change is told to go alone.
fn close_if_due(db: &Db, rules: &ReviewRules, window: Window, repo: &str, reviewer: &str) -> Result<Option<(i64, Vec<Queued>)>> {
    let dir = crate::paths::data_dir().join("review-batches");
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!("{}.lock", lock_name(repo, reviewer))))?;
    if lock.try_lock().is_err() {
        return Ok(None);
    }
    let mut stmt = db.conn.prepare(&format!("SELECT {COLUMNS} FROM review_queue WHERE repo = ?1 AND reviewer = ?2 AND state = 'waiting' ORDER BY id"))?;
    let rows: Vec<Queued> = stmt.query_map(params![repo, reviewer], row)?.collect::<rusqlite::Result<_>>()?;
    let mut waiting = vec![];
    for r in rows {
        if r.pid == std::process::id() || crate::submit_job::process_runs(r.pid) {
            waiting.push(r);
        } else {
            db.conn.execute("UPDATE review_queue SET state = 'abandoned', why = 'its submit stopped while it waited' WHERE id = ?1 AND state = 'waiting'", [r.id])?;
        }
    }
    let Some(oldest) = waiting.first() else { return Ok(None) };
    let max = rules.batch_max.max(2);
    let busy: bool = db.conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM review_batch WHERE repo = ?1 AND reviewer = ?2 AND state = 'reviewing')",
        params![repo, reviewer],
        |r| r.get(0),
    )?;
    let due = waiting.len() >= max
        || match window {
            Window::After(d) => now() - oldest.queued_at >= d.as_secs() as i64,
            Window::Busy => !busy,
        };
    if !due {
        return Ok(None);
    }
    let mut taken: Vec<Queued> = vec![];
    let mut lines = 0;
    for r in waiting {
        if taken.len() == max || (rules.batch == BatchMode::All && !taken.is_empty() && lines + r.lines > rules.max_lines) {
            break;
        }
        lines += r.lines;
        taken.push(r);
    }
    if taken.len() == 1 {
        db.conn.execute(
            "UPDATE review_queue SET state = 'alone', why = 'no other change joined it in time' WHERE id = ?1 AND state = 'waiting'",
            [taken[0].id],
        )?;
        return Ok(None);
    }
    let issues: Vec<&str> = taken.iter().map(|r| r.issue.as_str()).collect();
    db.conn.execute(
        "INSERT INTO review_batch (repo, reviewer, issues, state, pid, started_at) VALUES (?1, ?2, ?3, 'reviewing', ?4, ?5)",
        params![repo, reviewer, serde_json::to_string(&issues)?, std::process::id(), now()],
    )?;
    let batch = db.conn.last_insert_rowid();
    for r in &taken {
        db.conn.execute("UPDATE review_queue SET state = 'reviewing', batch_id = ?2 WHERE id = ?1", params![r.id, batch])?;
    }
    Ok(Some((batch, taken)))
}

/// A file name for a queue's lock.
fn lock_name(repo: &str, reviewer: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (repo, reviewer).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Reviews a batch in one call and writes each change's review, or why it goes alone, into its row.
/// Each change's timeline says so under its own submitter.
fn run_batch(db: &Db, rules: &ReviewRules, batch: i64, rows: &[Queued]) -> Result<()> {
    let keys: Vec<String> = rows.iter().map(|r| r.issue.clone()).collect();
    for (i, r) in rows.iter().enumerate() {
        let with: Vec<&str> = keys.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, k)| k.as_str()).collect();
        let note = if r.note.is_empty() { String::new() } else { format!(" ({})", r.note) };
        let started = format!("Review by {}{note} started in a batch with {}", r.reviewer, with.join(", "));
        db.log_event(&actor_of(r), Some(&r.issue), Some(&r.worktree), "review_started", &started)?;
    }
    let changes: Vec<BatchChange> = rows
        .iter()
        .map(|r| {
            let worktree = PathBuf::from(&r.worktree);
            let previous = crate::submit::previous_round(db, &r.issue, &worktree).ok().flatten();
            BatchChange {
                key: r.issue.clone(),
                title: r.title.clone(),
                branch: r.branch.clone(),
                base: r.base.clone(),
                head: r.head.clone(),
                worktree,
                files: r.files.clone(),
                previous,
            }
        })
        .collect();
    let reviewer = rows[0].reviewer.as_str();
    let fail = |why: String| -> Result<()> {
        db.conn.execute("UPDATE review_batch SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1", params![batch, now(), why])?;
        db.conn.execute("UPDATE review_queue SET state = 'alone', why = ?2 WHERE batch_id = ?1 AND state = 'reviewing'", params![batch, why])?;
        Ok(())
    };
    let Some(run_in) = changes.iter().map(|c| c.worktree.as_path()).find(|w| w.is_dir()) else {
        return fail("none of its changes' worktrees is there to run the reviewer in".into());
    };
    let reply = match rules.run_prompt(reviewer, run_in, &rules.batch_prompt(&changes)) {
        Ok(reply) if reply.only_a_login_message => Err(reply.signed_out(rules)),
        other => other,
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => return fail(format!("its batch's reviewer did not finish: {}", first_line(&format!("{e:#}")))),
    };
    let parsed = rules.parse_batch(reviewer, &reply.text, reply.seconds, &changes);
    let mut reviews = parsed.reviews;
    for (j, k, what) in &parsed.interactions {
        let (first, second) = (&keys[*j], &keys[*k]);
        if let Ok(r) = &mut reviews[*k] {
            r.findings.push(Finding {
                priority: 1,
                text: format!(
                    "Interacts with {first}, reviewed in the same batch: {what}. Once {first} has landed, rebase onto it and submit again: it is then reviewed alone on top of it."
                ),
                carried: false,
                raised_as: None,
                now: None,
            });
            r.verdict = "changes".into();
        }
        let note = format!("Reviewed in a batch with {second}, which interacts with it: {what}. {second} is reviewed again on top of this change once it lands.");
        db.add_note(first, &note, "review", &actor_of(&rows[*j]))?;
        let note = format!("Reviewed in a batch with {first}, which it interacts with: {what}. Land {first} first, then rebase this onto it and submit again.");
        db.add_note(second, &note, "review", &actor_of(&rows[*k]))?;
    }
    for (i, (r, review)) in rows.iter().zip(reviews).enumerate() {
        match review {
            Ok(mut review) => {
                review.note = r.note.clone();
                review.batch = Some(BatchInfo { id: batch, with: keys.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, k)| k.clone()).collect() });
                db.conn.execute(
                    "UPDATE review_queue SET state = 'done', result = ?2 WHERE id = ?1",
                    params![r.id, serde_json::to_string(&review)?],
                )?;
            }
            Err(why) => {
                db.conn.execute("UPDATE review_queue SET state = 'alone', why = ?2 WHERE id = ?1", params![r.id, why])?;
            }
        }
    }
    db.conn.execute("UPDATE review_batch SET state = 'done', ended_at = ?2, seconds = ?3 WHERE id = ?1", params![batch, now(), reply.seconds as i64])?;
    Ok(())
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(300).collect()
}

/// A newer submit of an issue replaces its change waiting in the queue.
pub fn supersede_waiting(db: &Db, issue: &str) -> Result<()> {
    db.conn.execute("UPDATE review_queue SET state = 'superseded' WHERE issue_key = ?1 COLLATE NOCASE AND state = 'waiting'", [issue])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;

    #[test]
    fn windows_are_durations_or_busy() {
        assert_eq!(Window::parse("5m").unwrap(), Window::After(Duration::from_secs(300)));
        assert_eq!(Window::parse(" 30s ").unwrap(), Window::After(Duration::from_secs(30)));
        assert_eq!(Window::parse("0").unwrap(), Window::After(Duration::ZERO));
        assert_eq!(Window::parse("2h").unwrap(), Window::After(Duration::from_secs(7200)));
        assert_eq!(Window::parse("Busy").unwrap(), Window::Busy);
        assert!(Window::parse("soon").unwrap_err().to_string().contains("is not a window"));
        assert!(Window::parse("5 days").is_err());
    }

    struct Queue {
        base: PathBuf,
        db: Db,
        actor: Actor,
    }

    impl Queue {
        fn new() -> Queue {
            let base = crate::paths::unique_temp("nitely-review-batch", "");
            std::fs::create_dir_all(&base).unwrap();
            let db = Db::open(&base.join("t.db")).unwrap();
            db.create_project("NIT", "Nitely").unwrap();
            Queue { base, db, actor: Actor::Agent("claude:test".into()) }
        }

        /// A small change waiting in the queue of `repo`, submitted by this process.
        fn waiting(&mut self, repo: &str, title: &str) -> (String, i64) {
            let issue = self
                .db
                .create_issue(&NewIssue { project: "NIT".into(), title: title.into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() }, &self.actor)
                .unwrap();
            let wt = self.base.join(&issue.key);
            std::fs::create_dir_all(&wt).unwrap();
            self.db
                .conn
                .execute(
                    "INSERT INTO review_queue (issue_key, repo, reviewer, title, branch, worktree, base, head, lines, files, pid, actor, state, queued_at)
                     VALUES (?1, ?2, 'claude', ?3, 'claude/x', ?4, 'b0', 'h1', 3, '[\"a.txt\"]', ?5, 'claude:test', 'waiting', ?6)",
                    params![issue.key, repo, title, wt.to_string_lossy(), std::process::id(), now()],
                )
                .unwrap();
            (issue.key, self.db.conn.last_insert_rowid())
        }

        fn row(&self, id: i64) -> Queued {
            queued(&self.db, id).unwrap().unwrap()
        }
    }

    impl Drop for Queue {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.base).ok();
        }
    }

    fn rules(reviewer: &[&str]) -> ReviewRules {
        let mut rules = ReviewRules { batch: BatchMode::Small, batch_max: 2, ..Default::default() };
        rules.commands.insert("claude".into(), reviewer.iter().map(|a| a.to_string()).collect());
        rules
    }

    #[test]
    fn a_batch_closes_when_full_and_what_it_cannot_review_goes_alone() {
        let mut q = Queue::new();
        let wait = Window::After(Duration::from_secs(300));
        // One change, inside its window: nothing closes yet. A second fills the batch.
        let (_, a) = q.waiting("repo-a", "One");
        let no_reviewer = rules(&["nitely-test-no-such-reviewer"]);
        assert!(close_if_due(&q.db, &no_reviewer, wait, "repo-a", "claude").unwrap().is_none());
        let (_, b) = q.waiting("repo-a", "Two");
        let (batch, rows) = close_if_due(&q.db, &no_reviewer, wait, "repo-a", "claude").unwrap().unwrap();
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [a, b]);
        assert_eq!(q.row(a).state, "reviewing");
        // A reviewer that cannot run sends every change back to be reviewed alone.
        run_batch(&q.db, &no_reviewer, batch, &rows).unwrap();
        for id in [a, b] {
            let r = q.row(id);
            assert_eq!(r.state, "alone");
            assert!(r.why.as_deref().unwrap().starts_with("its batch's reviewer did not finish"), "{r:?}");
        }

        // With `busy`, a change goes as soon as no batch of its repository is under review: alone.
        let (_, lone) = q.waiting("repo-b", "Lone");
        assert!(close_if_due(&q.db, &no_reviewer, Window::Busy, "repo-b", "claude").unwrap().is_none());
        assert_eq!((q.row(lone).state.as_str(), q.row(lone).why.as_deref()), ("alone", Some("no other change joined it in time")));
    }

    #[test]
    fn an_interaction_lets_the_first_land_and_sends_the_second_back() {
        let mut q = Queue::new();
        let sh = crate::playbook::posix_sh();
        let reply = "cat > /dev/null; printf 'Fine.\\nVERDICT 1: clean\\nVERDICT 2: clean\\nINTERACTION 1,2: both bump the version\\n'";
        let rules = rules(&[&sh, "-c", reply]);
        let (first, a) = q.waiting("repo-c", "Bump one");
        let (second, b) = q.waiting("repo-c", "Bump two");
        let (batch, rows) = close_if_due(&q.db, &rules, Window::After(Duration::from_secs(300)), "repo-c", "claude").unwrap().unwrap();
        run_batch(&q.db, &rules, batch, &rows).unwrap();
        let review = |id| -> Review { serde_json::from_str(q.row(id).result.as_deref().unwrap()).unwrap() };
        let (one, two) = (review(a), review(b));
        assert_eq!((one.verdict.as_str(), one.batch.as_ref().unwrap().with.clone()), ("pass", vec![second.clone()]));
        assert_eq!(two.verdict, "changes");
        assert!(two.findings[0].text.starts_with(&format!("Interacts with {first}, reviewed in the same batch: both bump the version.")), "{two:?}");
        let said = |k: &str| q.db.timeline(k).unwrap().into_iter().filter_map(|t| t.summary).collect::<Vec<_>>();
        assert!(said(&first).iter().any(|s| s.contains(&format!("{second} is reviewed again on top of this change"))), "{:?}", said(&first));
        assert!(said(&second).iter().any(|s| s == &format!("Review by claude started in a batch with {first}")), "{:?}", said(&second));
        let state: String = q.db.conn.query_row("SELECT state FROM review_batch WHERE id = ?1", [batch], |r| r.get(0)).unwrap();
        assert_eq!(state, "done");
    }

    #[test]
    fn a_batch_whose_process_died_and_a_replaced_change_are_settled() {
        let mut q = Queue::new();
        // The process reviewing a batch is gone: its changes are reviewed alone.
        let (_, a) = q.waiting("repo-d", "Orphan");
        q.db.conn.execute("INSERT INTO review_batch (repo, reviewer, state, pid, started_at) VALUES ('repo-d', 'claude', 'reviewing', 999999, ?1)", [now()]).unwrap();
        let batch = q.db.conn.last_insert_rowid();
        q.db.conn.execute("UPDATE review_queue SET state = 'reviewing', batch_id = ?2 WHERE id = ?1", params![a, batch]).unwrap();
        settle_dead_batch(&q.db, batch).unwrap();
        assert_eq!((q.row(a).state.as_str(), q.row(a).why.as_deref()), ("alone", Some("the process reviewing its batch (pid 999999) stopped")));
        // A waiting change whose own submit is gone leaves the queue.
        let (_, gone) = q.waiting("repo-e", "Gone");
        q.db.conn.execute("UPDATE review_queue SET pid = 999999 WHERE id = ?1", [gone]).unwrap();
        assert!(close_if_due(&q.db, &rules(&[]), Window::After(Duration::ZERO), "repo-e", "claude").unwrap().is_none());
        assert_eq!(q.row(gone).state, "abandoned");
        // A newer submit of the issue replaces the change waiting in the queue.
        let (key, waiting) = q.waiting("repo-f", "Replaced");
        supersede_waiting(&q.db, &key.to_lowercase()).unwrap();
        assert_eq!(q.row(waiting).state, "superseded");
    }
}
