//! The quick path (NIT-624): the agent already there makes a small change itself, in two calls.
//! `start` files the issue (or takes the one named) and makes a light worktree from the
//! integration branch. `land` commits what the agent changed there, and when the change is small
//! (verification::small_change) checks it with its own checks and lands it through the land
//! queue, with no pull request and no review before landing: quick changes are reviewed after
//! landing, together. A change that is not small, or one a person must approve, stops there and
//! goes on through the usual submit from the same worktree. The person's own checkout is never
//! touched.
use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::{
    issues::{Actor, IssueRecord, NewIssue},
    playbook::{self, git, Playbook, Started},
    Db,
};

/// The review row's verdict for a change that took the quick path: nobody reviewed it before it
/// landed, and the review after landing looks for these.
pub const VERDICT: &str = "quick";
pub const NOTE: &str = "Quick path: a small change, checked by its own checks, with no pull request; it is reviewed after landing, together with other quick changes";

/// The size limits the issue's review mode gives the quick path, or None where it is off
/// (Release, and a project that never chose a mode).
pub fn limits_for_issue(db: &Db, key: &str) -> Result<Option<crate::project_modes::QuickLimits>> {
    Ok(crate::project_modes::for_issue(db, key)?.and_then(|m| m.options().quick_limits))
}

/// What `start` makes the issue from when no key is given.
pub fn new_issue(project: &str, title: &str, source: &str) -> NewIssue {
    NewIssue {
        project: project.into(),
        title: title.into(),
        kind: "improvement".into(),
        body: format!("{title}\n\nA small change made through the quick path by the agent already there."),
        evidence: "Made through the quick path.".into(),
        source: source.into(),
        review_level: Some("contained".into()),
        allow_similar: true,
        ..Default::default()
    }
}

/// Makes the quick change's light worktree from the integration branch, for `issue`. An item of a
/// goal that has a goal branch lands through it, so it takes the usual path instead.
pub fn start(db: &Db, repo_root: &std::path::Path, issue: &IssueRecord, vendor: &str, actor: &Actor) -> Result<Started> {
    if let Some(gb) = crate::goal_branch::route_of_issue(db, repo_root, &issue.key, None)? {
        bail!("{} is an item of a goal that lands through {}: use start_task and submit for it", issue.key, gb.branch);
    }
    playbook::start_task_light(db, repo_root, issue, vendor, actor)
}

#[derive(Debug, Clone, Serialize)]
pub struct QuickLanded {
    pub issue: String,
    pub worktree: String,
    /// The commit `land` made of what was in the worktree, if anything was.
    pub committed: Option<String>,
    pub small: bool,
    pub reason: String,
    pub landed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub state: String,
    pub next: String,
}

/// Commits what is in the quick change's worktree (`dir`) and lands it when it is small. `message`
/// is the commit's, else the issue's title and key.
pub fn land(db: &mut Db, config: &crate::Config, dir: &str, message: Option<&str>, actor: &Actor) -> Result<QuickLanded> {
    let (worktree, branch) = playbook::worktree_and_branch(dir)?;
    let key = crate::git::parse_branch(&branch).1.context("the worktree's branch names no issue; run quick_land in the worktree quick_start made")?;
    let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
    let root = playbook::repo_root(config, dir).with_context(|| format!("{dir} is not inside a git repo"))?;
    if let Some(gb) = crate::goal_branch::route_of_issue(db, &root, &key, Some(&branch))? {
        bail!("{key} is an item of a goal that lands through {}: submit it", gb.branch);
    }
    let wt = worktree.to_string_lossy().into_owned();
    git(&worktree, &["add", "-A"])?;
    let committed = if git(&worktree, &["diff", "--cached", "--quiet"]).is_err() {
        let message = message.map(str::to_string).unwrap_or_else(|| format!("{} ({key})", issue.title));
        git(&worktree, &["commit", "-q", "-m", &message])?;
        Some(git(&worktree, &["rev-parse", "HEAD"])?.trim().to_string())
    } else {
        None
    };
    let mut pb = Playbook::load(&root)?;
    crate::project_modes::apply_issue(db, &key, &mut pb)?;
    let base = pb.integration_branch().to_string();
    let since = playbook::diff_base(&worktree, &base);
    let files: Vec<String> = git(&worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")])?.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    if files.is_empty() {
        bail!("{wt} has no change against {base}: make the change there first");
    }
    let mut verdict = crate::verification::small_change(&worktree, &base, "HEAD", limits_for_issue(db, &key)?)?;
    // A path the person approves, or one blocked, takes the usual path, where they see it.
    if verdict.small {
        let mut needs = pb.human_reasons(&files)?;
        needs.extend(pb.approval.check_blocked(&files).err().map(|e| format!("{e:#}")));
        if !needs.is_empty() {
            verdict = crate::verification::SmallVerdict { small: false, reason: format!("A person must see it: {}.", needs.join("; ")) };
        }
    }
    let mut out = QuickLanded { issue: key.clone(), worktree: wt.clone(), committed, small: verdict.small, reason: verdict.reason.clone(), landed: false, head: None, state: issue.state.clone(), next: String::new() };
    if !verdict.small {
        db.add_note(&key, &format!("Not a small change, so not the quick path: {} It goes on through the usual submit from {wt}.", verdict.reason), "quick", actor)?;
        out.next = format!("Not small: {} Submit it from {wt} as usual (submit), with a review before it lands.", verdict.reason);
        return Ok(out);
    }
    // The diff on the issue, for whoever reads it before the review after landing does.
    let diff = git(&worktree, &["diff", "--no-color", "--no-ext-diff", &format!("{since}...HEAD")])?;
    db.add_note(&key, &format!("Quick change ({}):\n```diff\n{}\n```", verdict.reason, diff.trim_end()), "quick", actor)?;
    let opts = crate::submit::SubmitOptions { dir: &wt, issue_key: Some(&key), skip_review: false, local_only: false, batch: false, solution: None, solution_commit: None, solution_patch_id: None };
    let submitted = crate::submit::submit_quick(db, config, &opts, actor)?;
    out.state = submitted.state.clone();
    if !submitted.gate.passed && submitted.gate.skipped.is_none() {
        out.next = format!("Its own checks failed: fix them in {wt}, then quick_land again. {}", submitted.next);
        return Ok(out);
    }
    let landed = match submitted.land {
        Some(landed) => landed,
        None if out.state == "In Review" => {
            let root_dir = root.to_string_lossy().into_owned();
            crate::land::land(db, config, &crate::land::LandOptions { dir: &root_dir, issue_key: &key, push: true, keep: false, break_lock: false }, actor)?
        }
        None => {
            out.next = submitted.next;
            return Ok(out);
        }
    };
    out.landed = landed.landed;
    out.head = landed.head.clone();
    out.state = db.issue(&key)?.map(|i| i.state).unwrap_or_default();
    out.next = if landed.landed {
        format!("Landed on {} at {}, with no pull request; it is reviewed after landing, with other quick changes. Nothing is left to do.", landed.base, landed.head.as_deref().unwrap_or("?"))
    } else {
        format!("The land stopped: {}. Fix it in {wt}, then quick_land again.", landed.error.as_deref().unwrap_or("see the issue's timeline"))
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_modes::{Mode, Options};
    use crate::test_repo::git as g;

    // The gate passes and the reviewer would fail: a quick change never reaches it.
    const PLAYBOOK: &str = "[worktree]\ndir = \"../wt/{key}\"\n[gate]\ncommands = [\"exit 0\"]\n[review]\nreviewer = { \"*\" = \"fake\" }\ncommands = { fake = [\"exit 1\"] }\n[[approval.human_paths]]\npattern = \"billing\"\nreason = \"billing\"\n";

    /// One whole quick change, and one too big for it (NIT-624).
    #[test]
    fn a_small_change_lands_in_one_call_and_a_big_one_is_refused_with_its_reason() {
        let base = crate::test_repo::copy("quick-path", |base| {
            crate::test_repo::init(&base.join("repo"), &[(playbook::PLAYBOOK_PATH, PLAYBOOK), ("scripts/build.txt", "one\n"), ("notes/a.txt", "a\n")]);
            crate::test_repo::add_remote(&base.join("repo"), &base.join("remote.git"));
        });
        let repo = base.join("repo");
        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("APP", "Example").unwrap();
        db.set_project_review_mode("APP", Mode::Fast, Options::default(), 0, &Actor::Human).unwrap();
        let actor = Actor::Agent("claude:test".into());
        let config = crate::Config::default();

        let issue = db.create_issue(&new_issue("APP", "Build script prints its version", "claude"), &actor).unwrap();
        let started = start(&db, &repo, &issue, "claude", &actor).unwrap();
        assert!(started.light, "a light worktree");
        let wt = std::path::PathBuf::from(&started.worktree);
        std::fs::write(wt.join("scripts/build.txt"), "one\ntwo\n").unwrap();
        let quick = land(&mut db, &config, &started.worktree, None, &actor).unwrap();
        assert!(quick.small && quick.landed, "{quick:?}");
        assert_eq!(db.issue(&issue.key).unwrap().unwrap().state, "Done");
        assert_eq!(db.reviews(&issue.key).unwrap()[0].verdict, VERDICT, "no review ran before landing");
        assert_eq!(g(&base.join("remote.git"), &["show", "main:scripts/build.txt"]), "one\ntwo", "pushed");
        assert!(db.timeline(&issue.key).unwrap().iter().any(|e| e.summary.as_deref().is_some_and(|s| s.contains("+two"))), "the diff is on the issue");

        let big = db.create_issue(&new_issue("APP", "Rewrite the notes", "claude"), &actor).unwrap();
        let started = start(&db, &repo, &big, "claude", &actor).unwrap();
        std::fs::write(std::path::Path::new(&started.worktree).join("notes/a.txt"), "x\n".repeat(40)).unwrap();
        let refused = land(&mut db, &config, &started.worktree, None, &actor).unwrap();
        assert!(!refused.small && !refused.landed && refused.reason.contains("over the limit"), "{refused:?}");
        assert_ne!(db.issue(&big.key).unwrap().unwrap().state, "Done");
        assert!(refused.next.contains("submit"), "it goes on through the usual submit: {}", refused.next);
    }
}
