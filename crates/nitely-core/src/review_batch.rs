//! Batched review (docs/batching.md; NIT-216, NIT-509). Under a mode that reviews in batches
//! (Fast), a change with no goal is not reviewed when it is submitted: its review row says
//! `batched`, and the land queue reviews every change of a run that waits for one in one reviewer
//! call, before the run's gate (`review_pending`). The batch is the land queue's own: whatever
//! arrived while the last land ran, up to `[land] batch_max`. A change that lands by itself is
//! reviewed by itself at its land the same way. Nothing lands unreviewed: land refuses a `batched`
//! row (`land::check_approval`).
//!
//! Each change still gets a review of its own, as if reviewed alone: its own row, findings, verdict
//! and next step, so rounds (NIT-161), accepted findings (NIT-191) and land's check of the
//! reviewed patch work as they do for a single review. A change the batch's reply leaves
//! unattributed, or whose batch's reviewer did not finish, is reviewed by itself.

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::playbook::{git, Playbook};
use crate::review::{BatchChange, BatchInfo, BatchMode, Choice, Finding, PreviousRound, Review, ReviewRules};
use crate::submit::ReviewRecord;

/// The verdict of a review row whose change waits for its land batch's review.
pub const PENDING: &str = "batched";

/// What a submit says of a change whose review waits for its land batch.
pub const PENDING_NOTE: &str = "Reviewed with its land queue batch, once, before the batch's gate";

/// Why a change is reviewed at submit, alone, even though its project reviews in batches, or None
/// when its review may wait for its land batch. `reasons` are what already sends it to a person.
#[allow(clippy::too_many_arguments)]
pub fn why_alone(
    db: &Db,
    rules: &ReviewRules,
    vendor: &str,
    choice: &Choice,
    reasons: &[String],
    files: &[String],
    previous: Option<&PreviousRound>,
    large_repo: bool,
    issue: &str,
) -> Result<Option<String>> {
    let why = if rules.batch == BatchMode::Off {
        "its project reviews each change alone"
    } else if !reasons.is_empty() || rules.independence_reason(&choice.reviewer, vendor, &choice.note).is_some() {
        "it waits for a person's approval anyway, which comes after its review"
    } else if files.iter().any(|f| f == crate::playbook::PLAYBOOK_PATH) {
        "it changes the playbook"
    } else if previous.is_some_and(|p| p.findings.iter().any(|f| f.priority <= 1)) {
        "its previous review left P0 or P1 findings open, so it is checked against them alone"
    } else if last_flagged(db, issue)? {
        "its previous review raised a flag"
    } else if large_repo {
        "the project is in large-repo mode, which lands each change alone"
    } else if db.checkpoint_of_issue(issue)?.is_some() {
        "it is a goal's checkpoint, whose slice is reviewed as one change"
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

/// A change whose review waits: its handed-in row and what the reviewer reads.
struct Waiting {
    pending: ReviewRecord,
    change: BatchChange,
    vendor: String,
    choice: Choice,
}

/// Reviews the changes among `keys` whose latest review row waits for its batch (`PENDING`): one
/// reviewer call for those that share a reviewer, each change's review recorded as its own. For
/// each key, Ok when it may go on to its gate (reviewed and passing, or not waiting), or why it
/// may not: the review asked for changes (the issue goes back to In Progress), it now waits for a
/// person's approval (Needs You), or it could not be reviewed.
pub fn review_pending(db: &mut Db, root: &Path, keys: &[String], actor: &Actor) -> Result<Vec<(String, std::result::Result<(), String>)>> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    for key in keys {
        match db.reviews(key)?.into_iter().next() {
            Some(r) if r.verdict == PENDING => pending.push((key.clone(), r)),
            _ => out.push((key.clone(), Ok(()))),
        }
    }
    if pending.is_empty() {
        return Ok(out);
    }
    let mut pb = Playbook::load(root)?;
    crate::project_modes::apply_issue(db, &pending[0].0, &mut pb)?;
    let mut rules = pb.review.clone();
    rules.policy_root = Some(root.to_path_buf());
    let integration = pb.integration_branch().to_string();
    // A worktree already cleaned up: a scratch clone at the change's head stands in for it.
    let scratch = Scratch(std::cell::RefCell::new(vec![]));
    let mut waiting: Vec<Waiting> = Vec::new();
    for (key, record) in pending {
        let prepared = (|| -> Result<Waiting> {
            rules.require_enabled()?;
            let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
            let (branch, worktree) = crate::land::branch_of(root, &key)?;
            let head = crate::land::rev(root, &branch)?;
            anyhow::ensure!(
                !record.commit_sha.is_empty() && head.starts_with(&record.commit_sha),
                "{branch} gained commits since it was handed in, which were never checked; submit again"
            );
            let base = git(root, &["merge-base", &integration, &head])?.trim().to_string();
            let dir = match worktree.filter(|w| w.is_dir() && crate::land::rev(w, "HEAD").ok().as_deref() == Some(head.as_str())) {
                Some(w) => w,
                None => {
                    let dir = checkout_at(root, &head)?;
                    scratch.0.borrow_mut().push(dir.clone());
                    dir
                }
            };
            let files = git(root, &["diff", "--name-only", "--no-renames", &base, &head])?.lines().map(str::to_string).collect();
            let previous = crate::submit::previous_round_at(db, &key, &dir, &head, None)?;
            let vendor = crate::git::parse_branch(&branch).0.unwrap_or_default();
            let choice = rules.choose(&vendor, &|r| rules.is_installed(r, &dir))?;
            let change = BatchChange { key: key.clone(), title: issue.title, branch, base, head, worktree: dir, files, previous };
            Ok(Waiting { pending: record, change, vendor, choice })
        })();
        match prepared {
            Ok(w) => waiting.push(w),
            Err(e) => out.push((key, Err(format!("its review could not start: {e:#}")))),
        }
    }
    // One call per reviewer: a batch has one.
    let mut reviewers: Vec<String> = waiting.iter().map(|w| w.choice.reviewer.clone()).collect();
    reviewers.dedup();
    for reviewer in reviewers {
        let group: Vec<&Waiting> = waiting.iter().filter(|w| w.choice.reviewer == reviewer).collect();
        let reviews = review_group(db, &rules, root, &reviewer, &group, actor)?;
        for (w, review) in group.into_iter().zip(reviews) {
            let decided = match review {
                Ok(review) => decide(db, &rules, w, review, actor)?,
                Err(why) => Err(format!("its review did not finish: {why}; land again")),
            };
            out.push((w.change.key.clone(), decided));
        }
    }
    Ok(out)
}

/// The reviews of one reviewer's changes: alone for one, else one batch call, and alone for each
/// change the batch could not review.
fn review_group(db: &Db, rules: &ReviewRules, root: &Path, reviewer: &str, group: &[&Waiting], actor: &Actor) -> Result<Vec<std::result::Result<Review, String>>> {
    let alone = |w: &Waiting| -> std::result::Result<Review, String> {
        db.log_event(actor, Some(&w.change.key), None, "review_started", &format!("Review by {reviewer}{} started", noted(&w.choice.note))).ok();
        rules.run(reviewer, &w.change.worktree, &w.change.base, w.change.previous.as_ref()).map_err(|e| first_line(&format!("{e:#}")))
    };
    if group.len() == 1 {
        return Ok(vec![alone(group[0])]);
    }
    let keys: Vec<String> = group.iter().map(|w| w.change.key.clone()).collect();
    db.conn.execute(
        "INSERT INTO review_batch (repo, reviewer, issues, state, pid, started_at, shadow) VALUES (?1, ?2, ?3, 'reviewing', ?4, ?5, 0)",
        params![crate::paths::normalize(&root.to_string_lossy()), reviewer, serde_json::to_string(&keys)?, std::process::id(), now()],
    )?;
    let batch = db.conn.last_insert_rowid();
    let projects: std::collections::HashSet<_> = keys.iter().filter_map(|k| db.issue(k).ok().flatten().and_then(|i| i.project)).collect();
    let project = (projects.len() == 1).then(|| projects.into_iter().next()).flatten();
    let _performance = crate::performance_attribution::Scope::new(db, crate::performance_attribution::Context { project, review_batch_id: Some(batch), ..Default::default() });
    for (i, w) in group.iter().enumerate() {
        let with: Vec<&str> = keys.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, k)| k.as_str()).collect();
        db.log_event(actor, Some(&w.change.key), None, "review_started", &format!("Review by {reviewer}{} started in a batch with {}", noted(&w.choice.note), with.join(", ")))?;
    }
    let changes: Vec<BatchChange> = group.iter().map(|w| w.change.clone()).collect();
    let reply = match rules.run_review_prompt(reviewer, &changes[0].worktree, &rules.batch_prompt(&changes)) {
        Ok(reply) if reply.only_a_login_message => Err(reply.signed_out(rules)),
        other => other,
    };
    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => {
            let why = format!("its batch's reviewer did not finish: {}", first_line(&format!("{e:#}")));
            db.conn.execute("UPDATE review_batch SET state = 'failed', ended_at = ?2, error = ?3 WHERE id = ?1", params![batch, now(), why])?;
            // Each by itself, as if it had come alone.
            return Ok(group.iter().map(|w| alone(w)).collect());
        }
    };
    let parsed = rules.parse_batch(reviewer, &reply.text, reply.seconds, &changes);
    let mut reviews = parsed.reviews;
    // What a change reviewed alone after all must still carry from the batch.
    let mut carried: Vec<Vec<Finding>> = vec![vec![]; group.len()];
    for (j, k, what) in &parsed.interactions {
        let (first, second) = (&keys[*j], &keys[*k]);
        // It cites the files both touch, so it keeps pointing at code on the branch.
        let shared: Vec<&String> = changes[*k].files.iter().filter(|f| changes[*j].files.contains(f)).collect();
        let cited: Vec<&str> = if shared.is_empty() { changes[*k].files.iter().map(String::as_str).collect() } else { shared.iter().map(|f| f.as_str()).collect() };
        let finding = Finding {
            priority: 1,
            text: format!(
                "{INTERACTS}{first}{SAME_BATCH} {what} ({}). Once {first} has landed, rebase onto it and submit again: it is then reviewed alone on top of it.",
                cited.join(", ")
            ),
            carried: false,
            raised_as: None,
            now: None,
        };
        match &mut reviews[*k] {
            Ok(r) => {
                r.findings.push(finding);
                r.verdict = "changes".into();
            }
            Err(_) => carried[*k].push(finding),
        }
        db.add_note(first, &format!("Reviewed in a batch with {second}, which interacts with it: {what}. {second} is reviewed again on top of this change once it lands."), "review", actor)?;
        db.add_note(second, &format!("Reviewed in a batch with {first}, which it interacts with: {what}. Land {first} first, then rebase this onto it and submit again."), "review", actor)?;
    }
    db.conn.execute(
        "UPDATE review_batch SET state = 'done', ended_at = ?2, seconds = ?3, tokens = ?4 WHERE id = ?1",
        params![batch, now(), reply.seconds as i64, reply.tokens.map(|t| t as i64)],
    )?;
    // Each change's share of what the call cost.
    let share = reply.tokens.map(|t| t / group.len() as u64);
    Ok(reviews
        .into_iter()
        .enumerate()
        .map(|(i, review)| match review {
            Ok(mut review) => {
                review.tokens = share;
                review.batch = Some(BatchInfo { id: batch, with: keys.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, k)| k.clone()).collect() });
                Ok(review)
            }
            Err(why) => {
                db.log_event(actor, Some(&keys[i]), None, "status", &format!("Reviewed alone after all: {why}")).ok();
                alone(group[i]).map(|mut r| {
                    r.findings.extend(carried[i].clone());
                    if r.findings.iter().any(|f| f.priority <= 1) {
                        r.verdict = "changes".into();
                    }
                    r
                })
            }
        })
        .collect())
}

/// Records a change's review as its own row, bound to the patch it reviewed, and says whether the
/// change may go on to its gate.
fn decide(db: &mut Db, rules: &ReviewRules, w: &Waiting, mut review: Review, actor: &Actor) -> Result<std::result::Result<(), String>> {
    let key = &w.change.key;
    hold_interactions(db, &w.change.worktree, &mut review)?;
    review.note = w.choice.note.clone();
    let mut reasons = w.pending.reasons.clone();
    reasons.extend(rules.independence_reason(&review.reviewer, &w.vendor, &review.note));
    reasons.extend(rules.flag_reasons(&review));
    reasons.dedup();
    let patch = crate::submit::patch_id_range(&w.change.worktree, &w.change.base, &w.change.head)?;
    // Findings the person accepted as they are, signed for this very patch, block nothing.
    let accepted = db.accepted_for(key, &patch)?;
    let accepted_only = review.verdict == "changes" && review.findings.iter().filter(|f| f.priority <= 1).all(|f| accepted.iter().any(|a| a.finding == f.text));
    let verdict = if accepted_only { "pass".to_string() } else { review.verdict.clone() };
    let approver = if reasons.is_empty() { "agent" } else { "human" };
    db.conn.execute(
        "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, note, ts, batch_id, seconds, tokens, solution, gate, branch, after_event)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, (SELECT COALESCE(MAX(id), 0) FROM event))",
        params![
            key,
            patch,
            w.pending.commit_sha,
            review.reviewer,
            verdict,
            approver,
            to_json(&reasons),
            to_json(&review.findings),
            to_json(&review.flagged),
            review.note,
            now(),
            review.batch.as_ref().map(|b| b.id),
            review.seconds as i64,
            review.tokens.map(|t| t as i64),
            w.pending.solution.as_ref().map(to_json),
            w.pending.gate.as_ref().map(to_json),
            w.change.branch,
        ],
    )?;
    db.close_superseded_requests(key, &review.findings)?;
    let by = match &review.batch {
        Some(b) => format!("{}{}, in a batch with {},", review.reviewer, noted(&review.note), b.with.join(", ")),
        None => format!("{}{}", review.reviewer, noted(&review.note)),
    };
    let state = db.issue(key)?.map(|i| (i.state, i.origin == "nitely")).unwrap_or_default();
    if verdict == "changes" {
        let open: Vec<String> = review.findings.iter().filter(|f| f.priority <= 1).map(|f| format!("[P{}] {}", f.priority, f.text)).collect();
        db.add_note(key, &format!("Review by {by} at its land requested changes: {}", open.join("; ")), "submit", actor)?;
        if state.1 && state.0 != "In Progress" {
            db.move_issue(key, "In Progress", None, None, actor)?;
        }
        return Ok(Err(format!("the {} review asked for changes ({}); fix them, commit, and submit again", review.reviewer, open.join("; "))));
    }
    if approver == "human" {
        db.add_note(key, &format!("Review by {by} at its land passed; it waits for a human ({})", reasons.join("; ")), "submit", actor)?;
        if state.1 && state.0 != "Needs You" {
            db.move_issue(key, "Needs You", None, None, actor)?;
        }
        return Ok(Err(format!("this change waits for a human ({}); it lands once they approve it in Nitely, after the review", reasons.join("; "))));
    }
    db.add_note(key, &format!("Review by {by} at its land passed"), "submit", actor)?;
    Ok(Ok(()))
}

fn to_json<T: serde::Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn noted(note: &str) -> String {
    if note.is_empty() { String::new() } else { format!(" ({note})") }
}

/// A clone of `repo` checked out at `head`, sharing the repo's objects (nothing is copied but the
/// files): a change's own files after its worktree is gone.
fn checkout_at(repo: &Path, head: &str) -> Result<PathBuf> {
    let dir = crate::paths::unique_temp("nitely-batch-change", "");
    let clone = crate::playbook::run_git(crate::playbook::shell_free_command("git").args(["clone", "--quiet", "--shared", "--no-checkout"]).arg(repo).arg(&dir))?;
    if !clone.status.success() {
        anyhow::bail!("could not clone {}: {}", repo.display(), String::from_utf8_lossy(&clone.stderr).trim());
    }
    if let Err(e) = git(&dir, &["checkout", "--quiet", "--detach", head]) {
        std::fs::remove_dir_all(&dir).ok();
        return Err(e);
    }
    Ok(dir)
}

/// Scratch clones a batch made, removed when it is done, however it ends.
struct Scratch(std::cell::RefCell<Vec<PathBuf>>);

impl Drop for Scratch {
    fn drop(&mut self) {
        for dir in self.0.borrow().iter() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(300).collect()
}

/// How the finding an interaction gives the second change starts: `Interacts with NIT-4, reviewed
/// in the same batch:`.
const INTERACTS: &str = "Interacts with ";

/// Whether a finding is the one an interaction in a review batch gave a change: it stays until the
/// other change has landed under it, whatever a later review says (`hold_interactions`).
pub(crate) fn is_interaction(text: &str) -> bool {
    text.starts_with(INTERACTS) && text.contains(SAME_BATCH)
}
const SAME_BATCH: &str = ", reviewed in the same batch:";

/// The change an interaction finding names (`Interacts with NIT-4, reviewed in the same batch: ...`).
pub fn interaction_with(text: &str) -> Option<&str> {
    text.strip_prefix(INTERACTS).and_then(|rest| rest.split_once(SAME_BATCH)).map(|(key, _)| key.trim())
}

/// Keeps an interaction finding from a batch open until the change it interacts with has landed
/// and this branch holds what landed, whatever a later review says of it: the second change must be
/// reviewed on top of the first. Recomputes the verdict.
pub fn hold_interactions(db: &Db, worktree: &Path, review: &mut Review) -> Result<()> {
    let mut held = vec![];
    for r in review.previous.iter_mut() {
        let Some(first) = interaction_with(&r.text).map(str::to_string) else { continue };
        if let Some(why) = not_on_top_of(db, worktree, &first)? {
            r.status = "open".into();
            r.now = Some(why.clone());
            held.push(Finding { priority: r.priority.min(1), text: r.text.clone(), carried: true, raised_as: None, now: Some(why) });
        }
    }
    for f in held {
        review.findings.retain(|x| !x.text.starts_with(&f.text));
        review.findings.push(f);
    }
    review.verdict = if review.findings.iter().any(|f| f.priority <= 1) { "changes" } else { "pass" }.into();
    Ok(())
}

/// Why this branch is not yet on top of `key`'s landed change, or None when it is.
fn not_on_top_of(db: &Db, worktree: &Path, key: &str) -> Result<Option<String>> {
    let Some(issue) = db.issue(key)? else { return Ok(None) };
    // A first change that will never land holds nothing back: the review's own answer counts.
    if issue.state_type == "canceled" {
        return Ok(None);
    }
    if issue.state != "Done" {
        return Ok(Some(format!("{key} has not landed yet: land it first, then rebase this onto it")));
    }
    let landed: Option<String> = db
        .conn
        .query_row("SELECT summary FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'landed' ORDER BY id DESC LIMIT 1", [key], |r| r.get(0))
        .optional()?;
    let Some(at) = landed.and_then(|s| s.split_whitespace().last().map(|w| w.trim_end_matches('.').to_string())) else { return Ok(None) };
    if crate::playbook::git(worktree, &["merge-base", "--is-ancestor", &at, "HEAD"]).is_ok() {
        return Ok(None);
    }
    Ok(Some(format!("{key} landed at {at}, and this branch is not rebased onto it yet")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;

    fn db() -> (PathBuf, Db, Actor) {
        let base = crate::paths::unique_temp("nitely-review-batch", "");
        std::fs::create_dir_all(&base).unwrap();
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        (base, db, Actor::Agent("claude:test".into()))
    }

    #[test]
    fn an_interaction_stays_open_until_the_first_change_landed_under_the_second() {
        let (base, mut db, actor) = db();
        let first = db.create_issue(&NewIssue { project: "NIT".into(), title: "Lands first".into(), kind: "feature".into(), body: "b".into(), evidence: "e".into(), source: "human".into(), allow_similar: true, ..Default::default() }, &actor).unwrap().key;
        // The second change's branch: one commit on top of a base.
        let wt = base.join("second");
        crate::test_repo::init(&wt, &[("a.txt", "1")]);
        let git = |args: &[&str]| crate::test_repo::git(&wt, args);
        let at_base = git(&["rev-parse", "HEAD"]);
        git(&["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
        let elsewhere = git(&["rev-parse", "HEAD"]);
        git(&["reset", "-q", "--hard", &at_base]);

        let text = format!("Interacts with {first}, reviewed in the same batch: both bump the version (a.txt). Once {first} has landed, rebase onto it and submit again: it is then reviewed alone on top of it.");
        // The next review calls it fixed, but nothing has landed: it stays open, at P1.
        let review = |status: &str| Review {
            reviewer: "codex".into(),
            verdict: "pass".into(),
            findings: vec![],
            flagged: vec![],
            summary: String::new(),
            seconds: 1,
            previous: vec![crate::review::Recheck { priority: 1, text: text.clone(), status: status.into(), now: None }],
            note: String::new(),
            batch: None,
            tokens: None,
            reused: None,
        };
        let mut r = review("fixed");
        hold_interactions(&db, &wt, &mut r).unwrap();
        assert_eq!((r.verdict.as_str(), r.previous[0].status.as_str()), ("changes", "open"));
        assert!(r.findings[0].now.as_deref().unwrap().contains("has not landed yet"), "{r:?}");
        // Landed, but not under this branch: still open.
        db.conn.execute("UPDATE issue SET state = 'Done', state_type = 'completed' WHERE key = ?1", [&first]).unwrap();
        db.log_event(&actor, Some(&first), None, "landed", &format!("Landed claude/x on main at {elsewhere}")).unwrap();
        let mut r = review("fixed");
        hold_interactions(&db, &wt, &mut r).unwrap();
        assert!(r.verdict == "changes" && r.findings[0].now.as_deref().unwrap().contains("not rebased onto it"), "{r:?}");
        // Canceled, it holds nothing back either.
        db.conn.execute("UPDATE issue SET state = 'Canceled', state_type = 'canceled' WHERE key = ?1", [&first]).unwrap();
        let mut r = review("fixed");
        hold_interactions(&db, &wt, &mut r).unwrap();
        assert_eq!(r.verdict, "pass");
        db.conn.execute("UPDATE issue SET state = 'Done', state_type = 'completed' WHERE key = ?1", [&first]).unwrap();
        // On top of what landed, the review's own answer counts.
        git(&["reset", "-q", "--hard", &elsewhere]);
        let mut r = review("fixed");
        hold_interactions(&db, &wt, &mut r).unwrap();
        assert_eq!((r.verdict.as_str(), r.findings.len()), ("pass", 0));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_change_whose_worktree_is_gone_is_read_at_its_own_commit() {
        let (base, _db, _) = db();
        let repo = base.join("repo");
        crate::test_repo::init(&repo, &[("a.txt", "the change's line\n")]);
        let head = crate::test_repo::git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("a.txt"), "moved on since\n").unwrap();
        crate::test_repo::git(&repo, &["commit", "-q", "-am", "later"]);
        let dir = checkout_at(&repo, &head).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap().lines().next(), Some("the change's line"));
        drop(Scratch(std::cell::RefCell::new(vec![dir.clone()])));
        assert!(!dir.exists(), "removed when the batch is done");
        assert!(checkout_at(&repo, "0000000000000000000000000000000000000000").is_err());
        std::fs::remove_dir_all(&base).ok();
    }
}

