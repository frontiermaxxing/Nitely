//! `land`: the only writer of the integration branch. One land at a time, under a lock in the
//! repo's common git dir. The branch is rebased onto the integration branch in its worktree, its
//! review and approval are checked against exactly what it now changes, the gate runs there, and
//! the main checkout is fast-forwarded to the checked commit and pushed. A rebase conflict stops
//! the land with the rebase left in progress for the branch's owner. This is Aera's `land.mjs`.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::db::{now, Db};
use crate::issues::Actor;
use crate::playbook::{self, git, GateResult, Playbook};
use crate::submit::patch_id;

/// A lock older than this probably belongs to a land that died.
const LOCK_STALE_SECS: u64 = 60 * 60;
const LOCK_FILE: &str = "nitely-land.lock";

#[derive(Debug, Clone, Default, Serialize)]
pub struct Landed {
    pub issue: String,
    pub branch: String,
    pub base: String,
    pub landed: bool,
    /// The integration branch's new head, short.
    pub head: Option<String>,
    pub commits: Vec<String>,
    /// Why no human approval was needed, or which approval let it land.
    pub approval: Option<String>,
    pub gate: Option<GateResult>,
    pub pushed: Option<bool>,
    pub warnings: Vec<String>,
    /// Files left conflicted by a rebase that stopped.
    pub conflicts: Vec<String>,
    pub cleanup: Option<String>,
    /// Processes that kept the worktree from being removed (a dev server, a build, a shell).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held_by: Vec<crate::procs::Holder>,
    pub error: Option<String>,
    /// Issues that waited on this one and have no open blocker left.
    pub unblocked: Vec<String>,
}

pub struct LandOptions<'a> {
    /// Any directory in the repo.
    pub dir: &'a str,
    pub issue_key: &'a str,
    /// Push the integration branch to origin (and delete the landed remote branch).
    pub push: bool,
    /// Keep the task's worktree and branch after landing.
    pub keep: bool,
    /// Take over a lock left by a land that died.
    pub break_lock: bool,
}

pub fn land(db: &mut Db, config: &crate::Config, opts: &LandOptions, actor: &Actor) -> Result<Landed> {
    let root = playbook::repo_root(config, opts.dir).with_context(|| format!("{} is not inside a git repo", opts.dir))?;
    let pb = Playbook::load(&root)?;
    let base = pb.integration_branch().to_string();
    let issue = db.issue(opts.issue_key)?.with_context(|| format!("no issue {}", opts.issue_key))?;
    let mut report = Landed { issue: issue.key.clone(), base: base.clone(), ..Default::default() };

    let common = PathBuf::from(git(&root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?.trim());
    let lock = Lock::take(&common.join(LOCK_FILE), &issue.key, opts.break_lock)?;
    let result = run(db, &root, &pb, &issue.key, opts, actor, &mut report);
    drop(lock);

    match result {
        Ok(()) => {
            let evidence = format!(
                "Landed on {base} at {}; gate passed ({}).",
                report.head.as_deref().unwrap_or("?"),
                report.gate.as_ref().map(|g| g.results.iter().map(|r| r.command.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
            );
            db.log_event(actor, Some(&issue.key), None, "landed", &format!("Landed {} on {base} at {}", report.branch, report.head.as_deref().unwrap_or("?")))?;
            if issue.origin == "nitely" && issue.state != "Done" {
                db.move_issue(&issue.key, "Done", Some(&evidence), None, actor)?;
            }
            for waiting in &issue.blocks {
                if db.issue(waiting)?.is_some_and(|i| i.blocked_by.is_empty()) {
                    report.unblocked.push(waiting.clone());
                }
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            db.log_event(actor, Some(&issue.key), None, "land_failed", &format!("Land stopped: {}", msg.lines().next().unwrap_or("")))?;
            report.error = Some(msg);
        }
    }
    Ok(report)
}

fn run(db: &Db, root: &Path, pb: &Playbook, key: &str, opts: &LandOptions, actor: &Actor, report: &mut Landed) -> Result<()> {
    let base = report.base.clone();
    let current = git(root, &["branch", "--show-current"])?.trim().to_string();
    if current != base {
        bail!("the main checkout {} is on {current}, not {base}; land only writes {base}", root.display());
    }

    // The branch: the one the issue's worktree has, else the only local branch that names the issue.
    let worktrees = list_worktrees(root)?;
    let named = |b: &str| crate::git::parse_branch(b).1.is_some_and(|k| k.eq_ignore_ascii_case(key));
    let branch = match worktrees.iter().filter(|(_, b)| b.as_deref().is_some_and(named)).count() {
        1 => worktrees.iter().find_map(|(_, b)| b.clone().filter(|b| named(b))).unwrap(),
        0 => {
            let branches: Vec<String> = git(root, &["branch", "--format=%(refname:short)"])?.lines().filter(|b| named(b)).map(str::to_string).collect();
            match branches.as_slice() {
                [one] => one.clone(),
                [] => bail!("no local branch names {key}"),
                many => bail!("several branches name {key}: {}", many.join(", ")),
            }
        }
        _ => bail!("several worktrees have a branch that names {key}"),
    };
    report.branch = branch.clone();

    let (worktree, temporary) = match worktrees.iter().find(|(_, b)| b.as_deref() == Some(branch.as_str())) {
        Some((path, _)) if same_path(path, root) => bail!("{branch} is checked out in the main checkout"),
        Some((path, _)) => (path.clone(), false),
        None => {
            let path = root.parent().unwrap_or(root).join(format!("{}-land-{}", file_name(root), key.to_lowercase()));
            git(root, &["worktree", "add", &path.to_string_lossy(), &branch])?;
            (path, true)
        }
    };
    let wt = worktree.to_string_lossy().into_owned();
    if rebase_in_progress(&worktree) {
        bail!("a rebase is in progress in {wt}; finish it (git rebase --continue) or abort it, then land again");
    }
    let dirty = git(&worktree, &["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{wt} has uncommitted changes to tracked files; commit them on {branch} first:\n{}", dirty.trim_end());
    }

    let before = rev(root, &base)?;
    if git(root, &["merge-base", "--is-ancestor", &base, &branch]).is_err() {
        db.log_event(actor, Some(key), Some(&wt), "status", &format!("Rebasing {branch} onto {base}"))?;
        if let Err(e) = git(&worktree, &["rebase", &base]) {
            report.conflicts = git(&worktree, &["diff", "--name-only", "--diff-filter=U"])?.lines().map(str::to_string).collect();
            if report.conflicts.is_empty() {
                git(&worktree, &["rebase", "--abort"]).ok();
                return Err(e).context(format!("rebasing {branch} onto {base}"));
            }
            bail!(
                "the rebase of {branch} onto {base} stopped on conflicts in {}; it is left in progress in {wt}. \
                 Resolve each file, git add it, git rebase --continue, then submit and land again",
                report.conflicts.join(", ")
            );
        }
    }

    let head = rev(root, &branch)?;
    report.approval = Some(check_approval(db, pb, key, &worktree, &base)?);

    let gate = playbook::run_gate(db, root, &worktree, Some(key), actor)?;
    let passed = gate.passed;
    report.gate = Some(gate);
    if !passed {
        bail!("the gate failed; fix it on {branch}, submit, and land again");
    }
    if rev(root, &base)? != before {
        bail!("{base} moved during the gate; land again");
    }
    // What lands is exactly what was checked.
    if rev(root, &branch)? != head {
        bail!("{branch} gained commits during the gate, after its review was checked; submit again");
    }

    if opts.push {
        // The pull request should show the commits that land, not the ones from before the rebase.
        if let Err(e) = git(root, &["push", "--force-with-lease", "origin", &format!("{head}:refs/heads/{branch}")]) {
            report.warnings.push(format!("the pull request may still show the pre-rebase commits ({})", first_line(&e)));
        }
    }

    report.commits = git(root, &["log", "--oneline", &format!("{base}..{head}")])?.lines().map(str::to_string).collect();
    git(root, &["merge", "--ff-only", &head]).context("the main checkout refused the fast-forward (do uncommitted edits there overlap the change?)")?;
    report.landed = true;
    report.head = Some(git(root, &["rev-parse", "--short", "HEAD"])?.trim().to_string());

    if opts.push {
        match git(root, &["push", "origin", &base]) {
            Ok(_) => {
                report.pushed = Some(true);
                // The code host marks the pull request merged once its commits are on the base.
                // Deleted only while it still points at what landed.
                git(root, &["push", &format!("--force-with-lease=refs/heads/{branch}:{head}"), "origin", &format!(":refs/heads/{branch}")]).ok();
            }
            Err(e) => {
                report.pushed = Some(false);
                report.warnings.push(format!("{base} landed locally but the push failed: {}", first_line(&e)));
            }
        }
    }

    let (cleanup, held_by) = cleanup(root, pb, &worktree, &branch, temporary, opts.keep);
    report.cleanup = Some(cleanup);
    report.held_by = held_by;
    Ok(())
}

/// A branch lands with a passing review of exactly what it changes, and, when a human must
/// approve, with that approval given in Nitely after the review.
fn check_approval(db: &Db, pb: &Playbook, key: &str, worktree: &Path, base: &str) -> Result<String> {
    let patch = patch_id(worktree, base)?;
    let review = db.reviews(key)?.into_iter().next().with_context(|| format!("{key} has no review; submit it first"))?;
    if review.patch_id != patch {
        bail!(
            "the branch changed since its review (reviewed {}, now {}); submit again",
            short(&review.patch_id),
            short(&patch)
        );
    }
    match review.verdict.as_str() {
        "pass" | "none" => {}
        _ => bail!("the {} review asked for changes; fix them, commit, and submit again", review.reviewer),
    }
    let files: Vec<String> = git(worktree, &["diff", "--name-only", "--no-renames", &format!("{base}...HEAD")])?.lines().map(str::to_string).collect();
    pb.approval.check_blocked(&files)?;
    let mut reasons = pb.approval.human_reasons(&files)?;
    if review.approver == "human" {
        reasons.extend(review.reasons.iter().cloned());
    }
    reasons.dedup();
    if reasons.is_empty() {
        return Ok(format!("the {} review passed and no human approval is needed", review.reviewer));
    }
    let approved_at = db.human_approval_at(key)?;
    match approved_at {
        Some(at) if at >= review.ts => Ok(format!("approved by a human in Nitely after the review ({})", reasons.join("; "))),
        _ => bail!(
            "this change waits for a human ({}); it lands once they approve it in Nitely, after the review",
            reasons.join("; ")
        ),
    }
}

impl Db {
    /// When a human last moved the issue to Approved, if it is still Approved.
    pub fn human_approval_at(&self, key: &str) -> Result<Option<i64>> {
        let Some(issue) = self.issue(key)? else { return Ok(None) };
        if issue.state != "Approved" {
            return Ok(None);
        }
        Ok(self.conn.query_row(
            "SELECT MAX(ts) FROM event WHERE issue_key = ?1 COLLATE NOCASE AND type = 'issue_moved' AND actor = 'human'
               AND summary LIKE '%→ Approved%'",
            [&issue.key],
            |r| r.get(0),
        )?)
    }
}

/// Removes the task's worktree and branch once landed: always for a worktree land added itself,
/// and for one the playbook's layout made (under its worktree folder) unless `keep`.
///
/// A process using the worktree (a dev server, a build) makes Windows refuse to delete files in
/// it, and git then stops halfway: the worktree unregistered, part of it deleted. So a worktree in
/// use is kept whole, with the processes named; and when git still stops halfway, what is left is
/// retried and then said plainly, with how to delete it.
fn cleanup(root: &Path, pb: &Playbook, worktree: &Path, branch: &str, temporary: bool, keep: bool) -> (String, Vec<crate::procs::Holder>) {
    let issue = crate::IssueRecord { key: "x".into(), title: "x".into(), ..Default::default() };
    let layout_dir = pb.worktree_for(root, "x", &issue);
    let ours = layout_dir.parent().is_some_and(|d| worktree.starts_with(d));
    if !(temporary || (ours && !keep)) {
        return (format!("kept {} (not made by start_task, or --keep)", worktree.display()), vec![]);
    }
    let path = worktree.to_string_lossy().into_owned();
    let held = crate::procs::holders(worktree);
    if !held.is_empty() {
        let names: Vec<String> = held.iter().map(|h| format!("{} (pid {})", h.name, h.pid)).collect();
        let message = format!(
            "kept {path} whole because {} still use{} it; stop {}, then run `git worktree remove \"{path}\"` and `git branch -d {branch}` in {}",
            names.join(", "),
            if held.len() == 1 { "s" } else { "" },
            if held.len() == 1 { "it" } else { "them" },
            root.display()
        );
        return (message, held);
    }
    let removed = match git(root, &["worktree", "remove", &path]) {
        Ok(_) => Ok(()),
        Err(e) => finish_removal(root, worktree, &e),
    };
    let message = match removed {
        Ok(()) if temporary => "removed the temporary worktree".into(),
        Ok(()) => match git(root, &["branch", "-d", branch]) {
            Ok(_) => "removed the worktree and the merged branch".into(),
            Err(e) => format!("removed the worktree; kept the branch ({})", first_line(&e)),
        },
        Err(message) => message,
    };
    (message, vec![])
}

/// After `git worktree remove` failed: Ok when the folder could be deleted after all, else what is
/// left and how to delete it. A worktree git still lists was not touched and is kept as it is.
fn finish_removal(root: &Path, worktree: &Path, error: &anyhow::Error) -> std::result::Result<(), String> {
    let path = worktree.to_string_lossy().into_owned();
    let same = |p: &Path| crate::paths::normalize(&p.to_string_lossy()) == crate::paths::normalize(&path);
    if list_worktrees(root).map(|w| w.iter().any(|(p, _)| same(p))).unwrap_or(true) {
        return Err(format!("kept the worktree ({})", first_line(error)));
    }
    // Unregistered and partly deleted: files just closed are often free a moment later.
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
        if !worktree.exists() || std::fs::remove_dir_all(worktree).is_ok() {
            git(root, &["worktree", "prune"]).ok();
            return Ok(());
        }
    }
    git(root, &["worktree", "prune"]).ok();
    Err(format!(
        "git unregistered the worktree but could not delete all of {path} ({}); nothing in it is needed any more. \
Close whatever still uses it, then delete the folder (PowerShell: `Remove-Item -Recurse -Force \"{path}\"`; sh: `rm -rf \"{path}\"`). \
The merged branch was kept; delete it with `git branch -d` once the folder is gone",
        first_line(error)
    ))
}

struct Lock(PathBuf);

impl Lock {
    fn take(path: &Path, holder: &str, break_lock: bool) -> Result<Lock> {
        if path.exists() {
            let held = std::fs::read_to_string(path).unwrap_or_default();
            if !break_lock {
                let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).map_or(0, |d| d.as_secs());
                let stale = if age > LOCK_STALE_SECS { " It is over an hour old; if no land is running, land again with break_lock." } else { "" };
                bail!("another land holds {}: {}.{stale}", path.display(), held.trim());
            }
            std::fs::remove_file(path).ok();
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("another land took {} just now", path.display()))?;
        writeln!(f, "{holder} pid {} since {}", std::process::id(), crate::issues::iso(now()))?;
        Ok(Lock(path.to_path_buf()))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

/// (path, branch) for every worktree, the main checkout first.
fn list_worktrees(root: &Path) -> Result<Vec<(PathBuf, Option<String>)>> {
    let mut out = Vec::new();
    let mut cur: Option<(PathBuf, Option<String>)> = None;
    for line in git(root, &["worktree", "list", "--porcelain"])?.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            out.extend(cur.take());
            cur = Some((PathBuf::from(p), None));
        } else if let (Some(b), Some(c)) = (line.strip_prefix("branch refs/heads/"), cur.as_mut()) {
            c.1 = Some(b.to_string());
        }
    }
    out.extend(cur);
    Ok(out)
}

fn rebase_in_progress(worktree: &Path) -> bool {
    ["rebase-merge", "rebase-apply"]
        .iter()
        .any(|d| git(worktree, &["rev-parse", "--path-format=absolute", "--git-path", d]).is_ok_and(|p| Path::new(p.trim()).exists()))
}

fn rev(dir: &Path, name: &str) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", &format!("{name}^{{commit}}")])?.trim().to_string())
}

fn same_path(a: &Path, b: &Path) -> bool {
    crate::paths::normalize(&a.to_string_lossy()) == crate::paths::normalize(&b.to_string_lossy())
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into())
}

fn short(s: &str) -> &str {
    &s[..s.len().min(12)]
}

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or("").to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::issues::NewIssue;
    use crate::submit::{submit, SubmitOptions};
    use std::process::Command;

    struct Fixture {
        base: PathBuf,
        repo: PathBuf,
        remote: PathBuf,
        db: Db,
        actor: Actor,
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn fixture(name: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!("nitely-land-{name}-{}-{}", std::process::id(), now()));
        let repo = base.join("repo");
        let remote = base.join("remote.git");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        Command::new("git").args(["init", "-q", "--bare", "-b", "main"]).arg(&remote).output().unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join(playbook::PLAYBOOK_PATH),
            r#"
[worktree]
dir = "../wt/{key}"
[gate]
commands = ["true"]
[review]
reviewer = { "*" = "fake" }
commands = { fake = ["sh", "-c", "cat > /dev/null; echo 'Fine.'; echo 'VERDICT: clean'"] }
[[approval.human_paths]]
pattern = "billing"
reason = "billing"
"#,
        )
        .unwrap();
        std::fs::write(repo.join("shared.txt"), "one\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        sh(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        sh(&repo, &["push", "-q", "-u", "origin", "main"]);
        let db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        Fixture { base, repo, remote, db, actor: Actor::Agent("claude:test".into()) }
    }

    impl Fixture {
        /// Files an issue, starts its worktree, commits `file`, and submits it.
        fn submitted(&mut self, title: &str, file: &str, content: &str) -> (String, PathBuf) {
            let issue = self
                .db
                .create_issue(
                    &NewIssue {
                        project: "NIT".into(),
                        title: title.into(),
                        kind: "feature".into(),
                        body: "b".into(),
                        evidence: "none yet".into(),
                        source: "human".into(),
                        priority: 0,
                        allow_similar: true,
                        state: None,
                        blocked_by: vec![],
                    },
                    &self.actor,
                )
                .unwrap();
            let started = playbook::start_task(&self.db, &self.repo, &issue, "claude", &self.actor).unwrap();
            let wt = PathBuf::from(&started.worktree);
            std::fs::write(wt.join(file), content).unwrap();
            sh(&wt, &["add", "."]);
            sh(&wt, &["commit", "-q", "-m", title]);
            self.submit(&wt);
            (issue.key, wt)
        }

        fn submit(&mut self, wt: &Path) {
            let dir = wt.to_string_lossy().into_owned();
            let opts = SubmitOptions { dir: &dir, issue_key: None, skip_review: false, local_only: true };
            submit(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap();
        }

        fn land(&mut self, key: &str) -> Landed {
            let dir = self.repo.to_string_lossy().into_owned();
            let opts = LandOptions { dir: &dir, issue_key: key, push: true, keep: false, break_lock: false };
            land(&mut self.db, &crate::Config::default(), &opts, &self.actor).unwrap()
        }
    }

    #[test]
    fn keeps_a_worktree_a_process_still_uses() {
        let mut f = fixture("held");
        let (a, wt) = f.submitted("Pulse view", "pulse.txt", "x");
        // A dev server started in the worktree, as the browser preview asks agents to do.
        let mut server = std::process::Command::new("sh").arg("-c").arg(format!("sleep 30; : {}", wt.display())).spawn().unwrap();
        let r = f.land(&a);
        server.kill().ok();
        server.wait().ok();
        assert!(r.landed, "{r:?}");
        assert!(r.held_by.iter().any(|h| h.pid == server.id()), "{r:?}");
        assert!(r.cleanup.as_deref().unwrap().contains("kept"), "{r:?}");
        assert!(wt.exists() && sh(&f.repo, &["worktree", "list"]).contains(&*wt.to_string_lossy()), "kept whole and still registered");
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn lands_reviewed_work_and_waits_for_human_approval() {
        let mut f = fixture("approve");
        let (a, wt_a) = f.submitted("Sidebar polish", "sidebar.txt", "x");
        let (b, _) = f.submitted("Billing fix", "billing.txt", "x");

        let r = f.land(&a);
        assert!(r.landed && r.pushed == Some(true), "{r:?}");
        assert_eq!(sh(&f.remote, &["rev-parse", "main"]), sh(&f.repo, &["rev-parse", "main"]));
        assert!(!wt_a.exists(), "the worktree is cleaned up: {:?}", r.cleanup);
        assert_eq!(f.db.issue(&a).unwrap().unwrap().state, "Done");

        // b needs a human; it was reviewed before a landed, so it also rebases.
        let r = f.land(&b);
        assert!(!r.landed && r.error.as_deref().unwrap().contains("waits for a human"), "{r:?}");
        // An agent cannot approve; only a human move counts.
        assert!(f.db.move_issue(&b, "Approved", None, None, &f.actor).is_err());
        f.db.move_issue(&b, "Approved", None, None, &Actor::Human).unwrap();
        let r = f.land(&b);
        assert!(r.landed, "{r:?}");
        assert!(r.approval.unwrap().contains("approved by a human"));
        assert!(Path::new(&f.repo.join("billing.txt")).exists() && Path::new(&f.repo.join("sidebar.txt")).exists());
        std::fs::remove_dir_all(&f.base).ok();
    }

    #[test]
    fn refuses_changes_after_review_conflicts_and_a_held_lock() {
        let mut f = fixture("refuse");
        let (a, wt) = f.submitted("Edit shared", "shared.txt", "two\n");

        // A commit after the review is not what was reviewed.
        std::fs::write(wt.join("extra.txt"), "x").unwrap();
        sh(&wt, &["add", "."]);
        sh(&wt, &["commit", "-q", "-m", "extra"]);
        let r = f.land(&a);
        assert!(r.error.as_deref().unwrap().contains("changed since its review"), "{r:?}");
        f.submit(&wt);

        // Someone else lands a conflicting edit first.
        std::fs::write(f.repo.join("shared.txt"), "three\n").unwrap();
        sh(&f.repo, &["commit", "-q", "-am", "conflicting"]);
        let r = f.land(&a);
        assert_eq!(r.conflicts, vec!["shared.txt"], "{r:?}");
        assert!(rebase_in_progress(&wt));
        sh(&wt, &["rebase", "--abort"]);

        let lock = f.repo.join(".git").join(LOCK_FILE);
        std::fs::write(&lock, "NIT-9 pid 1").unwrap();
        let dir = f.repo.to_string_lossy().into_owned();
        let opts = LandOptions { dir: &dir, issue_key: &a, push: false, keep: true, break_lock: false };
        let err = land(&mut f.db, &crate::Config::default(), &opts, &f.actor).unwrap_err().to_string();
        assert!(err.contains("another land holds"), "{err}");
        std::fs::remove_dir_all(&f.base).ok();
    }
}
