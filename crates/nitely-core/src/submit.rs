//! `submit`: a finished branch becomes a reviewed pull request. It runs the gate, has the other
//! vendor review the branch, decides whether a human must approve it, pushes, opens or updates the
//! pull request, records the review for the exact patch so landing can check it, and moves the
//! issue, as the repo's playbook says.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::db::{now, Db};
use crate::issues::{Actor, IssueRecord};
use crate::playbook::{self, git, git_remote, GateResult, Playbook};
use crate::review::{PreviousRound, Review};

#[derive(Debug, Clone, Serialize)]
pub struct Submitted {
    pub issue: String,
    pub branch: String,
    pub base: String,
    /// `git patch-id` of the branch's diff: the review applies to this patch and no other.
    pub patch_id: String,
    pub commit: String,
    pub files: usize,
    pub gate: GateResult,
    /// None when the gate failed (nothing else ran) or the review was skipped.
    pub review: Option<Review>,
    /// `agent` when the review is enough, `human` when it waits for approval.
    pub approver: String,
    pub reasons: Vec<String>,
    pub pushed: bool,
    pub pr_url: Option<String>,
    pub pr_error: Option<String>,
    /// The issue's state afterwards.
    pub state: String,
    pub next: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewRecord {
    pub issue_key: String,
    pub patch_id: String,
    pub commit_sha: String,
    pub reviewer: String,
    pub verdict: String,
    pub approver: String,
    pub reasons: Vec<String>,
    pub findings: Vec<crate::review::Finding>,
    pub ts: i64,
    /// As `Review::note`: how independent the review was, e.g. `same vendor: codex not installed`.
    pub note: String,
}

pub struct SubmitOptions<'a> {
    /// The worktree, or any directory inside it.
    pub dir: &'a str,
    /// Defaults to the issue the branch names.
    pub issue_key: Option<&'a str>,
    /// Skip the review; the change then waits for a human.
    pub skip_review: bool,
    /// Skip the push and pull request (for a dry run or a repo without a remote).
    pub local_only: bool,
}

pub fn submit(db: &mut Db, config: &crate::Config, opts: &SubmitOptions, actor: &Actor) -> Result<Submitted> {
    let (worktree, branch_key) = playbook::current_worktree(opts.dir)?;
    let root = playbook::repo_root(config, opts.dir).with_context(|| format!("{} is not inside a git repo", opts.dir))?;
    let pb = Playbook::load(&root)?;
    let base = pb.integration_branch().to_string();
    let branch = git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
    if branch == base || branch == "HEAD" {
        bail!("{branch} is not a task branch; submit from the worktree start_task made");
    }
    let key = opts.issue_key.map(str::to_string).or(branch_key).context("the branch names no issue; pass the issue key")?;
    let issue = db.issue(&key)?.with_context(|| format!("no issue {key}"))?;
    if rebase_in_progress(&worktree) {
        bail!("a rebase is in progress in {}", worktree.display());
    }
    let dirty = git(&worktree, &["status", "--porcelain", "--untracked-files=no", "--ignore-submodules=all"])?;
    if !dirty.trim().is_empty() {
        bail!("{} has uncommitted changes; commit them first:\n{}", worktree.display(), dirty.trim_end());
    }
    let wt = worktree.to_string_lossy().into_owned();
    if !opts.local_only {
        // Best effort: measure against the remote's base, so work merged since the last pull isn't counted.
        git_remote(&worktree, &["fetch", "--quiet", "origin", &base]).ok();
    }
    let since = playbook::diff_base(&worktree, &base);
    if git(&worktree, &["rev-list", "--count", &format!("{since}..HEAD")])?.trim() == "0" {
        bail!("{branch} has no changes against {base}; commit your work first");
    }

    let files: Vec<String> = git(&worktree, &["diff", "--name-only", "--no-renames", &format!("{since}...HEAD")])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    pb.approval.check_blocked(&files)?;
    let gate = playbook::run_gate(db, &root, &worktree, Some(&issue.key), actor)?;
    let mut out = Submitted {
        issue: issue.key.clone(),
        branch: branch.clone(),
        base: base.clone(),
        patch_id: patch_id(&worktree, &since)?,
        commit: gate.commit.clone(),
        files: files.len(),
        gate,
        review: None,
        approver: "human".into(),
        reasons: vec![],
        pushed: false,
        pr_url: None,
        pr_error: None,
        state: issue.state.clone(),
        next: String::new(),
    };
    if !out.gate.passed {
        let failed = out.gate.results.last().map(|r| r.command.as_str()).unwrap_or("");
        out.next = format!("The gate failed at `{failed}`. Fix it, commit, and submit again.");
        return Ok(out);
    }

    out.reasons = pb.approval.human_reasons(&files)?;
    if opts.skip_review {
        out.reasons.push("no review ran".into());
    } else {
        let (vendor, _) = crate::git::parse_branch(&branch);
        let vendor = vendor.unwrap_or_default();
        let choice = pb.review.choose(&vendor, &|r| pb.review.is_installed(r))?;
        let reviewer = choice.reviewer.clone();
        let previous = previous_round(db, &issue.key, &worktree)?;
        let started = format!("Review by {reviewer}{} started", noted(&choice.note));
        db.log_event(actor, Some(&issue.key), Some(&wt), "review_started", &started)?;
        let mut review = match pb.review.run(&reviewer, &worktree, &since, previous.as_ref()) {
            Ok(r) => r,
            Err(e) => {
                db.log_event(actor, Some(&issue.key), Some(&wt), "review_failed", &format!("Review by {reviewer} did not finish"))?;
                return Err(e);
            }
        };
        review.note = choice.note.clone();
        out.reasons.extend(pb.review.independence_reason(&choice));
        out.reasons.extend(pb.review.flag_reasons(&review));
        out.review = Some(review);
    }
    out.approver = if out.reasons.is_empty() { "agent" } else { "human" }.into();
    let verdict = out.review.as_ref().map_or("none", |r| r.verdict.as_str()).to_string();
    db.record_review(&out)?;

    let title = format!("{}: {}", issue.key, issue.title);
    let body = describe(&issue, &out);
    if !opts.local_only {
        match push(&worktree, &branch) {
            Ok(()) => out.pushed = true,
            Err(e) => out.pr_error = Some(format!("push failed: {e:#}")),
        }
        if out.pushed {
            match crate::remote::open_pr(&root.to_string_lossy(), &branch, &base, &title, &body) {
                Ok(url) => out.pr_url = Some(url),
                Err(e) => out.pr_error = Some(format!("{e:#}")),
            }
        }
    }

    let pr_line = match (&out.pr_url, &out.pr_error) {
        (Some(url), _) => format!("Pull request: {url}"),
        (None, Some(e)) => format!("No pull request: {e}"),
        _ => "Not pushed (local only).".into(),
    };
    let outcome = match (&out.review, out.approver.as_str()) {
        (Some(r), _) if r.verdict == "changes" => format!("Review by {}{} requested changes", r.reviewer, noted(&r.note)),
        (Some(r), "human") => format!("Review by {}{} passed; waits for a human", r.reviewer, noted(&r.note)),
        (Some(r), _) => format!("Review by {}{} passed; ready to land", r.reviewer, noted(&r.note)),
        (None, _) => "Submitted without a review; waits for a human".to_string(),
    };
    let outcome = match out.review.as_ref().map(|r| rechecked(&r.previous)) {
        Some(Some(counts)) => format!("{outcome} (previous round: {counts})"),
        _ => outcome,
    };
    // The full description is on the pull request and in the app's review section.
    db.add_note(&issue.key, &format!("{outcome}. {pr_line}"), "submit", actor)?;

    let push_failed = !opts.local_only && !out.pushed;
    let (state, next) = if verdict == "changes" {
        ("In Progress", "The review found P0 or P1 problems. Fix them, commit, and submit again.")
    } else if push_failed {
        // Not handed in: nobody can see or merge the branch, so it must not look ready to land.
        ("In Progress", "The push failed, so the change is not handed in. Fix the remote or its access (ask the human if it is a permission error), then submit again.")
    } else if out.approver == "human" {
        ("Needs You", "It waits for the human's approval in the Nitely inbox. Pick up other work meanwhile; once it is Approved, call land.")
    } else {
        ("In Review", "The review passed and no human approval is needed. Call land to put it on the integration branch.")
    };
    if issue.state != state && issue.origin == "nitely" {
        out.state = db.move_issue(&issue.key, state, None, None, actor)?.state;
    }
    out.next = next.into();
    Ok(out)
}

/// The latest review that ran on the issue, for the next one to start from. Its commit counts as
/// in the branch's history only when it is an ancestor of HEAD; after a rebase it is not.
fn previous_round(db: &Db, key: &str, worktree: &Path) -> Result<Option<PreviousRound>> {
    let Some(last) = db.reviews(key)?.into_iter().find(|r| !r.reviewer.is_empty()) else {
        return Ok(None);
    };
    let in_history = git(worktree, &["merge-base", "--is-ancestor", &last.commit_sha, "HEAD"]).is_ok();
    Ok(Some(PreviousRound { commit: last.commit_sha, findings: last.findings, in_history }))
}

/// " (same vendor: codex not installed)" after a reviewer's name, or nothing.
fn noted(note: &str) -> String {
    if note.is_empty() { String::new() } else { format!(" ({note})") }
}

/// "2 fixed, 1 still open" for a re-review's checks of the previous round; None when it had none.
fn rechecked(previous: &[crate::review::Recheck]) -> Option<String> {
    if previous.is_empty() {
        return None;
    }
    let count = |status: &str| previous.iter().filter(|r| r.status == status).count();
    let parts: Vec<String> = [("fixed", "fixed"), ("open", "still open"), ("unchecked", "not checked")]
        .iter()
        .filter(|(status, _)| count(status) > 0)
        .map(|(status, label)| format!("{} {label}", count(status)))
        .collect();
    Some(parts.join(", "))
}

/// The pull request description: approval, review, findings, and a marker line with the patch.
pub fn describe(issue: &IssueRecord, s: &Submitted) -> String {
    // A link with the app's own scheme; hosts that drop such links still show the key, and the
    // second line keeps the link copyable.
    let named = if issue.url.is_empty() { issue.key.clone() } else { format!("[{}]({})", issue.key, issue.url) };
    let mut lines = vec![format!("Nitely: **{named}** {}", issue.title)];
    if issue.url.starts_with(&format!("{}://", crate::issues::LINK_SCHEME)) {
        lines.push(format!("Open it in Nitely: `{}`", issue.url));
    }
    lines.push(String::new());
    lines.push(if s.approver == "human" {
        "**Approval:** waits for a human. Approve it in Nitely's inbox.".to_string()
    } else {
        "**Approval:** the review is enough under this repo's playbook.".to_string()
    });
    lines.extend(s.reasons.iter().map(|r| format!("- {r}")));
    lines.push(String::new());
    match &s.review {
        Some(r) => {
            let result = if r.verdict == "pass" { "no P0 or P1 findings" } else { "changes requested" };
            lines.push(format!("**Review** by {}{} ({}s): {result}.", r.reviewer, noted(&r.note), r.seconds));
            if !r.summary.is_empty() {
                lines.push(format!("> {}", r.summary));
            }
            if let Some(counts) = rechecked(&r.previous) {
                lines.push(format!("Previous round's findings: {counts}."));
            }
            lines.extend(r.findings.iter().map(|f| {
                let carried = if f.carried { " (open since an earlier round)" } else { "" };
                format!("- [P{}] {}{carried}", f.priority, f.text)
            }));
        }
        None => lines.push("**Review:** none ran.".into()),
    }
    lines.push(String::new());
    let gate: Vec<String> = s.gate.results.iter().map(|r| format!("`{}`", r.command)).collect();
    lines.push(format!("**Gate** passed at `{}`: {}.", s.commit, gate.join(", ")));
    lines.push(format!("{} files changed against `{}`.", s.files, s.base));
    lines.push(String::new());
    lines.push(format!(
        "`nitely review patch={} verdict={} approver={} reviewer={}`",
        s.patch_id,
        s.review.as_ref().map_or("none", |r| r.verdict.as_str()),
        s.approver,
        s.review.as_ref().map_or("none", |r| r.reviewer.as_str())
    ));
    lines.join("\n")
}

/// The diff's stable id: the same change rebased onto a newer base keeps its id.
pub fn patch_id(worktree: &Path, base: &str) -> Result<String> {
    let diff = git(worktree, &["diff", "-U0", &format!("{base}...HEAD")])?;
    if diff.is_empty() {
        return Ok("empty".into());
    }
    use std::io::Write;
    let mut child = playbook::shell_free_command("git")
        .arg("-C")
        .arg(worktree)
        .args(["patch-id", "--verbatim"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(diff.as_bytes()));
    let out = child.wait_with_output()?;
    writer.join().ok();
    Ok(String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("unknown").to_string())
}

fn rebase_in_progress(worktree: &Path) -> bool {
    ["rebase-merge", "rebase-apply"].iter().any(|d| {
        git(worktree, &["rev-parse", "--git-path", d]).is_ok_and(|p| worktree.join(p.trim()).exists())
    })
}

/// Pushes the branch. `--force-with-lease` lets a rebased branch replace its own old head but
/// refuses when the remote moved to something this checkout has not seen.
fn push(worktree: &Path, branch: &str) -> Result<()> {
    git_remote(worktree, &["push", "--force-with-lease", "-u", "origin", &format!("HEAD:refs/heads/{branch}")]).map(|_| ())
}

impl Db {
    pub fn record_review(&self, s: &Submitted) -> Result<()> {
        let (reviewer, verdict, findings, flagged, note) = match &s.review {
            Some(r) => (r.reviewer.as_str(), r.verdict.as_str(), to_json(&r.findings), to_json(&r.flagged), r.note.as_str()),
            None => ("", "none", "[]".to_string(), "[]".to_string(), ""),
        };
        self.conn.execute(
            "INSERT INTO review (issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, flagged, note, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![s.issue, s.patch_id, s.commit, reviewer, verdict, s.approver, to_json(&s.reasons), findings, flagged, note, now()],
        )?;
        Ok(())
    }

    /// Every review of an issue's branch, newest first.
    pub fn reviews(&self, issue_key: &str) -> Result<Vec<ReviewRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_key, patch_id, commit_sha, reviewer, verdict, approver, reasons, findings, ts, note
             FROM review WHERE issue_key = ?1 COLLATE NOCASE ORDER BY ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([issue_key], |r| {
            Ok(ReviewRecord {
                issue_key: r.get(0)?,
                patch_id: r.get(1)?,
                commit_sha: r.get(2)?,
                reviewer: r.get(3)?,
                verdict: r.get(4)?,
                approver: r.get(5)?,
                reasons: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                findings: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default(),
                ts: r.get(8)?,
                note: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

fn to_json<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::NewIssue;
    use std::process::Command;

    #[test]
    fn submits_with_a_review_and_routes_flags_to_a_human() {
        let base = std::env::temp_dir().join(format!("nitely-submit-{}-{}", std::process::id(), now()));
        let repo = base.join("repo");
        let remote = base.join("remote.git");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            let o = Command::new("git").arg("-C").arg(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).output().unwrap();
            assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        };
        Command::new("git").args(["init", "-q", "--bare"]).arg(&remote).output().unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        sh(&repo, &["config", "user.name", "t"]);
        sh(&repo, &["config", "user.email", "t@t"]);
        // Only the fake `claude` reviewer is installed, so Claude's branches get a same-vendor
        // review. It flags money whenever the diff touches a billing file, finds an edge case
        // in a first look at an edge file, and on a re-review says that finding is still open.
        std::fs::write(
            repo.join("review.sh"),
            r#"prompt=$(cat)
files=$(git diff main...HEAD --name-only)
money=no; case "$files" in *billing*) money=yes;; esac
case "$prompt" in
  *"PREVIOUS <n>"*) echo "PREVIOUS 1: open"; echo "VERDICT: clean";;
  *) case "$files" in
       *edge*) echo "- [P2] Edge case: edge.txt, the empty line"; echo "VERDICT: findings";;
       *) echo "Fine."; echo "VERDICT: clean";;
     esac;;
esac
echo "MONEY: $money"
"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(playbook::PLAYBOOK_PATH),
            r#"
[worktree]
dir = "../wt/{key}"
[gate]
commands = ["exit 0"]
[review]
reviewer = { "*" = "codex" }
commands = { claude = ["{sh}", "review.sh"], codex = ["nitely-test-no-such-codex"] }
[[review.flags]]
name = "MONEY"
question = "the change alters billing"
reason = "money logic"
"#
            .replace("{sh}", &playbook::posix_sh()),
        )
        .unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        sh(&repo, &["remote", "add", "origin", &remote.to_string_lossy()]);
        sh(&repo, &["push", "-q", "origin", "main"]);

        let mut db = Db::open(&base.join("t.db")).unwrap();
        db.create_project("NIT", "Nitely").unwrap();
        let actor = Actor::Agent("claude:test".into());
        let new = |title: &str| NewIssue {
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
        };
        let config = crate::Config::default();

        for (title, file, approver, state) in
            [("Sidebar polish", "sidebar.txt", "agent", "In Review"), ("Billing fix", "billing.txt", "human", "Needs You")]
        {
            let issue = db.create_issue(&new(title), &actor).unwrap();
            db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
            let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
            let wt = Path::new(&started.worktree);
            std::fs::write(wt.join(file), "x").unwrap();
            let dir = started.worktree.as_str();
            let opts = SubmitOptions { dir, issue_key: None, skip_review: false, local_only: false };
            let empty = submit(&mut db, &config, &opts, &actor).unwrap_err().to_string();
            assert!(empty.contains("no changes"), "{empty}");
            sh(wt, &["add", "."]);
            sh(wt, &["commit", "-q", "-m", title]);

            let s = submit(&mut db, &config, &opts, &actor).unwrap();
            assert!(s.gate.passed && s.pushed, "{s:?}");
            let review = s.review.as_ref().unwrap();
            assert_eq!((review.verdict.as_str(), review.reviewer.as_str()), ("pass", "claude"));
            assert_eq!(review.note, "same vendor: codex not installed");
            assert_eq!((s.approver.as_str(), s.state.as_str()), (approver, state), "{s:?}");
            assert_eq!(db.reviews(&issue.key).unwrap()[0].note, "same vendor: codex not installed");
            assert!(describe(&issue, &s).contains("**Review** by claude (same vendor: codex not installed)"));
            // A local bare remote is not a code host, so there is no PR, and it says why.
            assert!(s.pr_url.is_none() && s.pr_error.as_deref().unwrap().contains("not on GitHub"));
            assert_eq!(db.reviews(&issue.key).unwrap()[0].patch_id, s.patch_id);
            assert!(describe(&issue, &s).contains(&format!("patch={}", s.patch_id)));
        }

        // A second round checks the first round's finding and carries it while it is open.
        let issue = db.create_issue(&new("Edge handling"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        std::fs::write(wt.join("edge.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "edge"]);
        let first = submit(&mut db, &config, &opts, &actor).unwrap();
        let review = first.review.as_ref().unwrap();
        assert!(review.previous.is_empty() && !review.findings[0].carried, "{review:?}");
        std::fs::write(wt.join("edge.txt"), "y").unwrap();
        sh(wt, &["commit", "-q", "-am", "edge again"]);
        let second = submit(&mut db, &config, &opts, &actor).unwrap();
        let review = second.review.as_ref().unwrap();
        assert_eq!(review.previous.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(), ["open"], "{review:?}");
        assert_eq!((review.findings.len(), review.findings[0].carried, review.verdict.as_str()), (1, true, "pass"));
        assert!(describe(&issue, &second).contains("Previous round's findings: 1 still open."));
        assert!(db.reviews(&issue.key).unwrap()[0].findings[0].carried);

        // A playbook that requires another vendor's review sends a same-vendor one to a human, and
        // with no reviewer installed at all, submit says what to install.
        let pb_path = repo.join(playbook::PLAYBOOK_PATH);
        let pb = std::fs::read_to_string(&pb_path).unwrap();
        std::fs::write(&pb_path, pb.replace("[review]
", "[review]
require_cross_vendor = true
")).unwrap();
        let issue = db.create_issue(&new("Footer tweak"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("footer.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "footer"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: true };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        assert_eq!((s.approver.as_str(), s.state.as_str()), ("human", "Needs You"), "{s:?}");
        assert!(s.reasons[0].contains("requires a review by another vendor"), "{:?}", s.reasons);
        std::fs::write(&pb_path, pb.replace("\"{sh}\", \"review.sh\"]".replace("{sh}", &playbook::posix_sh()).as_str(), "\"nitely-test-no-such-claude\"]")).unwrap();
        let none = submit(&mut db, &config, &opts, &actor).unwrap_err().to_string();
        assert!(none.contains("no reviewer can run") && none.contains("nitely-test-no-such"), "{none}");
        std::fs::write(&pb_path, pb).unwrap();

        // A push that fails hands nothing in, so the issue stays In Progress.
        sh(&repo, &["remote", "set-url", "origin", &base.join("missing.git").to_string_lossy()]);
        let issue = db.create_issue(&new("Header tweak"), &actor).unwrap();
        db.move_issue(&issue.key, "In Progress", None, None, &actor).unwrap();
        let started = playbook::start_task(&db, &repo, &issue, "claude", &actor).unwrap();
        let wt = Path::new(&started.worktree);
        std::fs::write(wt.join("header.txt"), "x").unwrap();
        sh(wt, &["add", "."]);
        sh(wt, &["commit", "-q", "-m", "header"]);
        let opts = SubmitOptions { dir: started.worktree.as_str(), issue_key: None, skip_review: false, local_only: false };
        let s = submit(&mut db, &config, &opts, &actor).unwrap();
        assert!(!s.pushed && s.pr_error.as_deref().unwrap().contains("push failed"), "{s:?}");
        assert_eq!(s.state, "In Progress");
        assert!(s.next.contains("push failed"), "{}", s.next);
        std::fs::remove_dir_all(base).ok();
    }
}
