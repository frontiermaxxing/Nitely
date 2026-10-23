//! The review after landing of quick changes (NIT-625): they landed with only their own checks
//! (quick.rs), so they are reviewed once they are on the integration branch, together, in one
//! reviewer call, once `EVERY` of them wait or the oldest has waited `MAX_WAIT`. A P0 or P1 finding
//! reopens the quick change it names, or files a follow-up linked to them, for an agent to fix,
//! and the person hears it as a notice in the Inbox. Nothing is reverted: only the backstop's full
//! suite on main reverts. Each change's result goes on its issue, so how often quick changes
//! escape is there to count. Where the project's periodic review is on, it reviews them instead.
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::path::Path;

use crate::{db::now, playbook::git, Actor, Config, Db, NewIssue};

/// How many quick changes make a review due, and how long the oldest may wait before one is.
pub const EVERY: usize = 5;
pub const MAX_WAIT: i64 = 3 * 3600;
/// The event that tells the person a quick change's review after landing found a problem.
pub const QUICK_FINDING: &str = "quick_finding";
const ACTOR: &str = "nitely:quick-review";

/// Whether the quick changes waiting make a review due: enough of them, or the oldest waited long.
pub fn due(waiting: usize, oldest_landed_at: Option<i64>, now: i64) -> bool {
    waiting >= EVERY || (waiting > 0 && oldest_landed_at.is_some_and(|t| now.saturating_sub(t) >= MAX_WAIT))
}

/// What a finding of the review after landing does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// P2 or lower: a note on the change it names, or on each change reviewed.
    Note(Option<String>),
    /// P0 or P1 naming one change that may be reopened: back to Todo for an agent to fix.
    Reopen(String),
    /// P0 or P1 otherwise: a follow-up issue linked to the changes reviewed.
    FollowUp(Option<String>),
}

/// The action for a finding of `priority` that names `named` (one of the reviewed changes, or
/// none). `reopenable` says whether that change may be reopened: Done, Nitely's own, and nothing a
/// person decided on it since.
pub fn action(priority: u8, named: Option<&str>, reopenable: impl Fn(&str) -> bool) -> Action {
    match named {
        _ if priority > 1 => Action::Note(named.map(str::to_string)),
        Some(key) if reopenable(key) => Action::Reopen(key.to_string()),
        _ => Action::FollowUp(named.map(str::to_string)),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Waiting {
    pub issue_key: String,
    pub base: String,
    pub head: String,
    pub landed_at: i64,
}

/// Records a quick change that just landed, for the review after landing. Called where every
/// land records itself; a change that did not take the quick path is left alone.
pub(crate) fn on_landed(db: &Db, root: &Path, issue: &crate::IssueRecord, land: &crate::land::Landed) -> Result<()> {
    let (Some(project), Some((base, head))) = (issue.project.as_deref(), landed_range(land)) else { return Ok(()) };
    if db.reviews(&issue.key)?.first().is_none_or(|r| r.verdict != crate::quick::VERDICT) {
        return Ok(());
    }
    // The periodic review of main, where it is on, covers what landed quickly too.
    if periodic_on(db, project)? {
        return Ok(());
    }
    let commit = |r: &str| -> Result<String> { Ok(git(root, &["rev-parse", &format!("{r}^{{commit}}")])?.trim().to_string()) };
    db.conn.execute(
        "INSERT OR REPLACE INTO quick_review (issue_key, project, repo, base, head, landed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![issue.key, project, repo_id(root)?, commit(base)?, commit(head)?, now()],
    )?;
    Ok(())
}

/// What a land put on the branch for its change alone: from where it started to its own tip, not
/// the tip of a batch it landed with (NIT-636 review).
fn landed_range(land: &crate::land::Landed) -> Option<(&str, &str)> {
    Some((land.landed_from.as_deref()?, land.own_tip.as_deref().or(land.head.as_deref())?))
}

fn periodic_on(db: &Db, project: &str) -> Result<bool> {
    Ok(db.project_review_mode(project)?.options().periodic.is_some_and(|p| p.enabled))
}

fn repo_id(root: &Path) -> Result<String> {
    crate::verification_audit::repository_id(root)
}

/// The quick changes of `project` in `root` waiting for their review after landing, oldest first.
pub fn waiting(db: &Db, project: &str, root: &Path) -> Result<Vec<Waiting>> {
    let mut stmt = db.conn.prepare("SELECT issue_key, base, head, landed_at FROM quick_review WHERE project = ?1 AND repo = ?2 AND reviewed_at IS NULL ORDER BY landed_at, issue_key")?;
    let rows = stmt.query_map(params![project, repo_id(root)?], |r| Ok(Waiting { issue_key: r.get(0)?, base: r.get(1)?, head: r.get(2)?, landed_at: r.get(3)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Reviews what waits in each connected repo where a review is due; one repo per call, as the
/// periodic review does. Run by the app every minute.
pub fn tick_all(db: &mut Db, config: &Config) -> Result<()> {
    for r in &config.repos {
        let Some(project) = &r.project else { continue };
        if db.project_archived(project)? { continue; }
        let root = Path::new(&r.root);
        let waiting = waiting(db, project, root)?;
        if due(waiting.len(), waiting.first().map(|w| w.landed_at), now()) {
            review(db, root, project, &r.integration_branch)?;
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Reviewed {
    pub reviewer: String,
    pub changes: Vec<String>,
    pub findings: usize,
    /// The issues reopened or filed for P0 and P1 findings.
    pub filed: Vec<String>,
}

/// Reviews every quick change of `project` waiting in `root`, now, in one reviewer call, and acts
/// on what it finds. None when nothing waits, or another review holds the repo.
pub fn review(db: &mut Db, root: &Path, project: &str, branch: &str) -> Result<Option<Reviewed>> {
    let waiting = waiting(db, project, root)?;
    if waiting.is_empty() {
        return Ok(None);
    }
    let Some(_lock) = crate::land::Lock::try_take(&Path::new(&repo_id(root)?).join("nitely-quick-review.lock"), "quick review", false)? else { return Ok(None) };
    let head = git(root, &["rev-parse", &format!("refs/heads/{branch}")])?.trim().to_string();
    // A change rewritten out of the branch's history has nothing left to review.
    let (present, gone): (Vec<Waiting>, Vec<Waiting>) = waiting.into_iter().partition(|w| git(root, &["merge-base", "--is-ancestor", &w.head, &head]).is_ok());
    for w in &gone {
        db.conn.execute("UPDATE quick_review SET reviewed_at = ?2, outcome = 'gone' WHERE issue_key = ?1", params![w.issue_key, now()])?;
    }
    if present.is_empty() {
        return Ok(None);
    }
    let mut rules = crate::periodic_review::integration_review_rules(root, project, branch)?;
    rules.timeout_minutes = rules.timeout_minutes.clamp(1, 60);
    rules.policy_root = None;
    rules.yolo_guards.clear();
    let mut changes = String::new();
    for w in &present {
        let title = db.issue(&w.issue_key)?.map(|i| i.title).unwrap_or_default();
        changes.push_str(&format!("- {} \"{title}\": `git diff {} {}`\n", w.issue_key, w.base, w.head));
    }
    let review = crate::verification_backstop::with_checkout(root, &head, |checkout| {
        let reviewer = rules.choose("quick", &|r| rules.is_installed(r, checkout))?.reviewer;
        let reply = rules.run_prompt_checked(&reviewer, checkout, &rules.landed_prompt(&changes), &mut || Ok(()))?;
        let mut review = rules.parse(&reviewer, &reply.text, reply.seconds, None, Some(checkout))?;
        review.tokens = reply.tokens;
        Ok(review)
    })?;
    apply(db, project, &present, &review)
}

/// Acts on the review of `reviewed`: each finding as `action` says, then each change's result on
/// its issue.
fn apply(db: &mut Db, project: &str, reviewed: &[Waiting], review: &crate::review::Review) -> Result<Option<Reviewed>> {
    let actor = Actor::Agent(ACTOR.into());
    let keys: Vec<String> = reviewed.iter().map(|w| w.issue_key.clone()).collect();
    let mut filed = Vec::new();
    let mut notes: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for f in &review.findings {
        let named = crate::periodic_review::named_source(&f.text, &keys).cloned();
        let reopenable = |key: &str| -> bool { reopenable(db, key).unwrap_or(false) };
        let line = format!("[P{}] {}", f.priority, f.text);
        match action(f.priority, named.as_deref(), reopenable) {
            Action::Note(Some(key)) => notes.entry(key).or_default().push(line),
            Action::Note(None) => keys.iter().for_each(|k| notes.entry(k.clone()).or_default().push(line.clone())),
            Action::Reopen(key) => {
                db.move_issue(&key, "Todo", None, Some(&format!("Its review after landing found: {line}")), &actor)?;
                db.log_event(&actor, Some(&key), None, QUICK_FINDING, &format!("{key} landed through the quick path, and its review after landing found a problem, so it is open again for an agent to fix: {line}"))?;
                notes.entry(key.clone()).or_default().push(format!("{line} (reopened)"));
                filed.push(key);
            }
            Action::FollowUp(named) => {
                let sources = named.iter().cloned().collect::<Vec<_>>();
                let links = (if sources.is_empty() { &keys } else { &sources }).iter().map(|k| crate::issues::issue_link(k)).collect::<Vec<_>>().join(", ");
                let issue = db.create_issue(&NewIssue {
                    project: project.into(),
                    title: crate::issues::headline(&f.text, 120),
                    body: format!("The review after landing of quick changes found:\n\n{}\n\nReviewed: {links}. Confirm it still applies on the integration branch before you fix it.", f.text),
                    kind: "bug".into(),
                    source: match review.reviewer.as_str() { "claude" => "claude", "cursor" => "cursor", _ => "codex" }.into(),
                    evidence: format!("{} review after landing of {}", review.reviewer, keys.join(", ")),
                    priority: i64::from(f.priority.saturating_add(1).clamp(1, 4)),
                    allow_similar: true,
                    ..Default::default()
                }, &actor)?;
                db.log_event(&actor, Some(&issue.key), None, QUICK_FINDING, &format!("The review after landing of quick changes ({links}) found a problem, filed as {} for an agent to fix: {line}", issue.key))?;
                for k in if sources.is_empty() { &keys } else { &sources } {
                    notes.entry(k.clone()).or_default().push(format!("{line} (filed as {})", issue.key));
                }
                filed.push(issue.key);
            }
        }
    }
    for w in reviewed {
        let found = notes.get(&w.issue_key);
        let outcome = if found.is_some() { "findings" } else { "passed" };
        let said = match found {
            None => format!("Reviewed after landing by {}, with {} other quick change{}: nothing found.", review.reviewer, reviewed.len() - 1, if reviewed.len() == 2 { "" } else { "s" }),
            Some(lines) => format!("Reviewed after landing by {}: {}", review.reviewer, lines.join("; ")),
        };
        db.add_note(&w.issue_key, &said, "quick_review", &actor)?;
        db.conn.execute("UPDATE quick_review SET reviewed_at = ?2, reviewer = ?3, outcome = ?4 WHERE issue_key = ?1", params![w.issue_key, now(), review.reviewer, outcome])?;
    }
    Ok(Some(Reviewed { reviewer: review.reviewer.clone(), changes: keys, findings: review.findings.len(), filed }))
}

/// Whether the quick change `key` may be reopened: Done, Nitely's own, with no finding accepted
/// and nothing a person decided on it since it landed.
fn reopenable(db: &Db, key: &str) -> Result<bool> {
    let Some(issue) = db.issue(key)? else { return Ok(false) };
    let landed_at: Option<i64> = db.conn.query_row("SELECT landed_at FROM quick_review WHERE issue_key = ?1", [key], |r| r.get(0)).optional()?;
    Ok(issue.state == "Done"
        && issue.origin == "nitely"
        && db.acceptances(key)?.is_empty()
        && !db.timeline(key)?.iter().any(|e| e.actor == "human" && landed_at.is_none_or(|t| e.ts >= t)))
}

/// How the quick changes of `project` fared in their reviews after landing: reviewed, and how many
/// of those had findings, so how often quick changes escape is there to read.
pub fn escape_rate(db: &Db, project: &str) -> Result<(usize, usize)> {
    db.conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(outcome = 'findings'), 0) FROM quick_review WHERE project = ?1 AND outcome IN ('passed', 'findings')",
            [project],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )
        .context("reading quick review outcomes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_modes::{Mode, Options};

    #[test]
    fn a_change_in_a_batch_is_reviewed_by_its_own_range() {
        let alone = crate::land::Landed { landed_from: Some("a".into()), head: Some("b".into()), ..Default::default() };
        assert_eq!(landed_range(&alone), Some(("a", "b")));
        let batched = crate::land::Landed { own_tip: Some("b".into()), head: Some("tip".into()), ..alone };
        assert_eq!(landed_range(&batched), Some(("a", "b")), "not the batch's tip");
    }

    /// NIT-625: when the review after landing is due, and what each finding does.
    #[test]
    fn quick_changes_are_reviewed_in_batches_and_only_p0_p1_reopen_or_file() {
        let now = 100_000;
        assert!(!due(0, None, now), "nothing waits");
        assert!(!due(4, Some(now - 60), now), "four, and fresh");
        assert!(due(EVERY, Some(now - 60), now), "enough of them");
        assert!(due(1, Some(now - MAX_WAIT), now), "one that waited long enough");
        let open = |k: &str| k == "APP-1";
        assert_eq!(action(2, Some("APP-1"), open), Action::Note(Some("APP-1".into())));
        assert_eq!(action(1, Some("APP-1"), open), Action::Reopen("APP-1".into()));
        assert_eq!(action(0, Some("APP-2"), open), Action::FollowUp(Some("APP-2".into())), "a person decided on it");
        assert_eq!(action(1, None, open), Action::FollowUp(None), "names no change");
    }

    /// One quick change landed, reviewed after landing with a P1 that names it: it is open again,
    /// the person has a notice, and the result is on the issue.
    #[test]
    fn a_p1_found_after_landing_reopens_the_quick_change_and_tells_the_person() {
        let base = crate::test_repo::copy("quick-review", |base| {
            let sh = crate::playbook::posix_sh();
            let playbook = format!(
                "[worktree]\ndir = \"../wt/{{key}}\"\n[gate]\ncommands = [\"exit 0\"]\n[review]\nreviewer = {{ \"*\" = \"fake\" }}\ncommands = {{ fake = [\"{sh}\", \"-c\", \"cat > /dev/null; echo '- [P1] APP-1 scripts/build.txt: the second line breaks the build'; echo 'VERDICT: findings'\"] }}\n"
            );
            crate::test_repo::init(&base.join("repo"), &[(crate::playbook::PLAYBOOK_PATH, &playbook), ("scripts/build.txt", "one\n")]);
            crate::test_repo::add_remote(&base.join("repo"), &base.join("remote.git"));
        });
        let repo = base.join("repo");
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        db.set_project_review_mode("APP", Mode::Fast, Options::default(), 0, &Actor::Human).unwrap();
        let actor = Actor::Agent("claude:test".into());
        let issue = db.create_issue(&crate::quick::new_issue("APP", "Build script prints its version", "claude"), &actor).unwrap();
        assert_eq!(issue.key, "APP-1");
        let started = crate::quick::start(&db, &repo, &issue, "claude", &actor).unwrap();
        std::fs::write(Path::new(&started.worktree).join("scripts/build.txt"), "one\ntwo\n").unwrap();
        assert!(crate::quick::land(&mut db, &Config::default(), &started.worktree, None, &actor).unwrap().landed);
        assert_eq!(waiting(&db, "APP", &repo).unwrap().len(), 1, "it waits for its review after landing");

        let reviewed = review(&mut db, &repo, "APP", "main").unwrap().unwrap();
        assert_eq!(reviewed.filed, ["APP-1"]);
        assert_eq!(db.issue("APP-1").unwrap().unwrap().state, "Todo", "open again for an agent");
        assert!(db.notices().unwrap().iter().any(|n| n.issue_key.as_deref() == Some("APP-1") && n.summary.contains("review after landing")));
        assert!(waiting(&db, "APP", &repo).unwrap().is_empty());
        assert_eq!(escape_rate(&db, "APP").unwrap(), (1, 1));
    }
}
