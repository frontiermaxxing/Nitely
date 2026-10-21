//! Release verification is independent of task verification levels and the PR review switch.
//! Gate and review are separate invocations so CI never gives build commands reviewer credentials.
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{db::now, playbook::{self, git, Playbook}, Actor, Db};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    /// Tags marking release baselines, e.g. `v*`. The candidate's own tag is excluded.
    pub tag: String,
    /// Baseline tag patterns to exclude, e.g. `*-*` to include every beta since the last stable.
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    /// Extra release checks, in addition to EVERY Full gate command and conditional step.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Exact tracked paths the release job may stamp after the original source commit.
    #[serde(default)]
    pub stamp_paths: Vec<String>,
}

pub struct Request<'a> {
    pub worktree: &'a Path,
    /// A trusted integration ref supplied by the release orchestrator, never candidate config.
    pub integration_ref: &'a str,
    /// Original tagged source commit; HEAD may be its one-child, version-stamped candidate.
    pub source: &'a str,
    pub author_vendor: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub repo: String,
    pub candidate: String,
    pub tree: String,
    pub source: String,
    pub policy: String,
    pub base: String,
    pub author_vendor: String,
    pub commands: Vec<String>,
    pub platform: String,
}

struct Candidate { identity: Identity, playbook: Playbook, root: PathBuf }

fn commit(repo: &Path, reference: &str) -> Result<String> {
    Ok(git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{reference}^{{commit}}")])?.trim().into())
}

fn clean(repo: &Path) -> Result<()> {
    if !git(repo, &["status", "--porcelain", "--untracked-files=all"])?.trim().is_empty() {
        bail!("release verification requires a clean committed candidate, including version stamping");
    }
    Ok(())
}

fn prepare(db: &Db, req: &Request<'_>) -> Result<Candidate> {
    clean(req.worktree)?;
    let root = PathBuf::from(crate::git::main_root(&req.worktree.to_string_lossy()).context("release candidate has no main repository")?);
    // One git for what prepare reads by name (NIT-508); it fails when any of them names no
    // commit, as --verify did for each.
    let names = git(req.worktree, &[
        "rev-parse", "--is-shallow-repository", "--end-of-options",
        &format!("{}^{{commit}}", req.source), "HEAD^{commit}", &format!("{}^{{commit}}", req.integration_ref), "HEAD^{tree}",
    ])?;
    let [shallow, source, candidate, policy, tree]: [String; 5] = names.lines().map(|l| l.trim().to_string()).collect::<Vec<_>>().try_into()
        .map_err(|_| anyhow::anyhow!("git rev-parse gave {names:?}"))?;
    git(req.worktree, &["merge-base", "--is-ancestor", &source, &policy]).context("release source must already belong to the trusted integration history")?;
    let text = git(req.worktree, &["show", &format!("{policy}:{}", playbook::PLAYBOOK_PATH)])?;
    let pb = Playbook::parse(&text)?;
    if !pb.unknown.is_empty() { bail!("release policy has unknown fields: {}", pb.unknown.join(", ")); }
    let rules = pb.release.as_ref().context("integration policy has no [release] configuration; release verification is not ready")?;
    if rules.tag.trim().is_empty() { bail!("release.tag must name the release baseline tag pattern"); }
    // Each commit with its parents, the candidate's and the source's in one git.
    let listed = git(req.worktree, &["rev-list", "--no-walk=unsorted", "--parents", &candidate, &source])?;
    let parents_of = |of: &str| listed.lines().map(|l| l.split_whitespace().collect::<Vec<_>>()).find(|l| l.first() == Some(&of)).map(|l| l[1..].iter().map(|p| p.to_string()).collect::<Vec<_>>()).unwrap_or_default();
    if candidate != source {
        let parents = parents_of(&candidate);
        if parents != [source.clone()] { bail!("stamped release candidate must be a single commit directly on its original source"); }
        let changed = git(req.worktree, &["diff", "--name-only", "-z", "--no-renames", &source, &candidate])?;
        for path in changed.split('\0').filter(|s| !s.is_empty()) {
            if !rules.stamp_paths.iter().any(|allowed| allowed == path) {
                bail!("release candidate changes {path} after its source; integration policy does not allow stamping this path");
            }
        }
    }
    // Start at the parent to exclude the release tag already placed on SOURCE. Missing history
    // is an error, not an apparent first release: shallow clones must fetch their full history.
    if shallow != "false" {
        bail!("release verification requires complete history and release tags");
    }
    let parents = parents_of(&source);
    let base = if let Some(parent) = parents.first().map(String::as_str) {
        let mut args = vec!["describe", "--tags", "--abbrev=0", "--match", rules.tag.as_str()];
        for pattern in &rules.exclude_tags {
            args.extend(["--exclude", pattern.as_str()]);
        }
        args.push(parent);
        let tag = git(req.worktree, &args);
        match tag {
            Ok(tag) => commit(req.worktree, tag.trim())?,
            Err(_) => playbook::git_with_input(req.worktree, &["hash-object", "-t", "tree", "--stdin", "-w"], "")?.trim().into(),
        }
    } else {
        playbook::git_with_input(req.worktree, &["hash-object", "-t", "tree", "--stdin", "-w"], "")?.trim().into()
    };
    if pb.gate.commands.is_empty() && pb.gate.steps.is_empty() {
        bail!("release requires a nonempty Full gate; release-only commands cannot replace it");
    }
    let commands: Vec<String> = pb.gate.commands.iter().cloned()
        .chain(pb.gate.steps.iter().map(|s| s.run.clone())).chain(rules.commands.iter().cloned()).collect();
    if commands.is_empty() || commands.iter().any(|c| c.trim().is_empty()) {
        bail!("release requires a nonempty Full gate; no lighter level or empty command is accepted");
    }
    crate::verification_backstop::release_ready(db, &root, &source)?;
    let identity = Identity {
        repo: root.to_string_lossy().into_owned(), candidate,
        tree, source, policy, base,
        author_vendor: req.author_vendor.into(), commands, platform: std::env::consts::OS.into(),
    };
    Ok(Candidate { identity, playbook: pb, root })
}

fn unchanged(db: &Db, req: &Request<'_>, before: &Identity) -> Result<()> {
    if prepare(db, req)?.identity != *before { bail!("release candidate, source, baseline or integration policy changed during verification; start again"); }
    Ok(())
}

fn start(db: &Db, identity: &Identity, phase: &str, gate_id: Option<i64>) -> Result<i64> {
    db.conn.execute("INSERT INTO release_verification(identity, phase, gate_id, started_at) VALUES (?1, ?2, ?3, ?4)",
        params![serde_json::to_string(identity)?, phase, gate_id, now()])?;
    Ok(db.conn.last_insert_rowid())
}

fn finish(db: &Db, id: i64, passed: bool, evidence: &impl Serialize) -> Result<()> {
    db.conn.execute("UPDATE release_verification SET passed=?2, evidence=?3, finished_at=?4 WHERE id=?1",
        params![id, passed, serde_json::to_string(evidence)?, now()])?;
    Ok(())
}

fn latest(db: &Db, identity: &Identity, phase: &str) -> Result<(i64, Option<i64>)> {
    let row: Option<(i64, Option<i64>, bool)> = db.conn.query_row(
        "SELECT id, gate_id, passed FROM release_verification WHERE identity=?1 AND phase=?2 ORDER BY id DESC LIMIT 1",
        params![serde_json::to_string(identity)?, phase], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    match row {
        Some((id, gate, true)) => Ok((id, gate)),
        _ => bail!("the exact release candidate has no completed passing {phase}; run release-verify {phase}"),
    }
}

pub fn gate(db: &Db, req: &Request<'_>) -> Result<Identity> {
    let c = prepare(db, req)?;
    let id = start(db, &c.identity, "gate", None)?;
    let result = execute_gate(db, req, &c).and_then(|evidence| {
        unchanged(db, req, &c.identity)?;
        finish(db, id, true, &evidence)?;
        Ok(c.identity.clone())
    });
    if let Err(error) = &result { finish(db, id, false, &error.to_string())?; }
    result
}

fn execute_gate(db: &Db, req: &Request<'_>, c: &Candidate) -> Result<Vec<playbook::CommandResult>> {
    let actor = Actor::Agent("release".into());
    let steps = c.identity.commands.iter().map(|s| (s.clone(), None)).collect::<Vec<_>>();
    if c.playbook.lanes.is_some() {
        let result = crate::lanes::gate_fresh(db, &c.root, &c.playbook, req.worktree, None, crate::lanes::Kind::Gate, &steps, &actor)?;
        if !result.passed || !result.warnings.is_empty() || !c.identity.candidate.starts_with(&result.commit) {
            bail!("release Full gate did not pass cleanly at the exact candidate: {}", serde_json::to_string(&result.results)?);
        }
        return Ok(result.results);
    }
    let turn = crate::lanes::hold_machine(db, &c.root, req.worktree, "", None, crate::lanes::Kind::Gate, &c.identity.commands.join("\n"), &actor)?;
    crate::light::make_whole(req.worktree)?;
    let (mut results, setup_ok) = playbook::refresh_setup(&c.playbook.worktree, req.worktree, &c.root, &c.identity.policy, 0, &mut Vec::new())?;
    if !setup_ok { turn.finish(false); bail!("release setup failed: {}", serde_json::to_string(&results)?); }
    for command in &c.identity.commands {
        let command = playbook::expand(command, &c.root, req.worktree, &c.identity.policy);
        let (ok, output, seconds) = playbook::run_shell(&command, req.worktree)?;
        results.push(playbook::CommandResult { command, ok, output, seconds, skipped: false, timed_out: false });
        if !ok { turn.finish(false); bail!("release Full gate failed: {}", serde_json::to_string(&results)?); }
    }
    turn.finish(true);
    Ok(results)
}

pub fn review(db: &Db, req: &Request<'_>) -> Result<Identity> {
    let c = prepare(db, req)?;
    let (gate_id, _) = latest(db, &c.identity, "gate")?;
    let id = start(db, &c.identity, "review", Some(gate_id))?;
    let result = execute_review(req, &c).and_then(|evidence| {
        unchanged(db, req, &c.identity)?;
        if latest(db, &c.identity, "gate")?.0 != gate_id { bail!("Full gate was rerun while the release review was running"); }
        finish(db, id, true, &evidence)?;
        Ok(c.identity.clone())
    });
    if let Err(error) = &result { finish(db, id, false, &error.to_string())?; }
    result
}

fn execute_review(req: &Request<'_>, c: &Candidate) -> Result<crate::review::Review> {
    let files = git(req.worktree, &["diff", "--name-only", "-z", "--no-renames", &c.identity.base, &c.identity.candidate])?.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect::<Vec<_>>();
    c.playbook.approval.check_blocked(&files)?;
    let reasons = c.playbook.human_reasons(&files)?;
    if !reasons.is_empty() { bail!("release needs human approval; automatic release verification cannot grant it: {}", reasons.join("; ")); }
    let mut rules = c.playbook.review.clone();
    // The project PR review toggle does not exempt releases, nor does a lighter task level.
    rules.enabled = true;
    rules.policy_root = None;
    let choice = rules.choose(req.author_vendor, &|name| rules.is_installed(name, req.worktree))?;
    if let Some(reason) = rules.independence_reason(&choice.reviewer, req.author_vendor, &choice.note) { bail!("release review needs human approval: {reason}"); }
    // Direct tree comparison also covers a first release (empty tree) and the stamped files.
    let prompt = rules.prompt(&c.identity.base, None).replace(
        &format!("git diff {}...HEAD", c.identity.base), &format!("git diff {} {}", c.identity.base, c.identity.candidate));
    let prompt = format!("Review the WHOLE release, not just the latest task. Original source: {}. Exact stamped candidate: {}.\n{prompt}", c.identity.source, c.identity.candidate);
    let reply = rules.run_prompt(&choice.reviewer, req.worktree, &prompt)?;
    let mut review = rules.parse(&choice.reviewer, &reply.text, reply.seconds, None, Some(req.worktree))?;
    review.tokens = reply.tokens;
    review.note = choice.note;
    if review.verdict != "pass" || !review.flagged.is_empty() { bail!("release peer review did not pass without human decisions: {}", serde_json::to_string(&review)?); }
    Ok(review)
}

/// Call immediately before publishing/promoting, using the same private Nitely database as the
/// gate/review phases. This is an attestation check, never a command that itself publishes.
pub fn check(db: &Db, req: &Request<'_>) -> Result<Identity> {
    let c = prepare(db, req)?;
    let (gate, _) = latest(db, &c.identity, "gate")?;
    if latest(db, &c.identity, "review")?.1 != Some(gate) { bail!("release review does not cover the latest Full gate attempt"); }
    Ok(c.identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_repo::{git as run_git, init};

    const POLICY: &str = r#"
[release]
tag = "v*"
stamp_paths = ["version.txt"]
commands = ["echo release-check"]
[gate]
commands = ["echo full-check"]
[[gate.steps]]
run = "echo conditional-check"
when_changed = ["never-changed/**"]
[review]
enabled = false
[review.reviewer]
codex = "claude"
[review.commands]
claude = ["sh", "-c", "cat >.git/release-prompt; printf 'VERDICT: clean\\n'"]
"#;

    fn fixture() -> (PathBuf, Db) {
        let repo = crate::test_repo::copy("release-verification", |repo| {
            // Git for Windows supplies sh outside PATH. Resolve it before the fixture is
            // committed instead of depending on another parallel test to initialize PATH.
            let mut pb = Playbook::parse(POLICY).unwrap();
            pb.review.commands.get_mut("claude").unwrap()[0] = playbook::posix_sh();
            let policy = toml::to_string(&pb).unwrap();
            init(repo, &[(playbook::PLAYBOOK_PATH, &policy), ("version.txt", "1"), ("app.txt", "old")]);
            run_git(repo, &["tag", "v1"]);
            std::fs::write(repo.join("app.txt"), "new").unwrap();
            run_git(repo, &["commit", "-qam", "new application"]);
        });
        let db = Db::open(&repo.join(".git/release-test.db")).unwrap();
        (repo, db)
    }

    fn request(repo: &Path) -> Request<'_> {
        Request { worktree: repo, integration_ref: "main", source: "main", author_vendor: "codex" }
    }

    fn stamp(repo: &Path) {
        run_git(repo, &["checkout", "-qb", "candidate"]);
        std::fs::write(repo.join("version.txt"), "2").unwrap();
        run_git(repo, &["commit", "-qam", "stamp"]);
    }

    #[test]
    fn stamped_candidate_full_gate_and_whole_release_review_ignore_pr_switch() {
        let (repo, db) = fixture();
        stamp(&repo);
        let req = request(&repo);
        assert!(check(&db, &req).is_err());
        let identity = gate(&db, &req).unwrap();
        assert_ne!(identity.candidate, identity.source);
        assert_eq!(identity.base, commit(&repo, "v1").unwrap());
        assert_eq!(identity.commands.len(), 3);
        let evidence: String = db.conn.query_row("SELECT evidence FROM release_verification WHERE phase='gate'", [], |r| r.get(0)).unwrap();
        assert!(evidence.contains("conditional-check"), "Full runs even a step whose changed-path condition does not match");
        assert!(review(&db, &req).is_ok(), "release review runs despite enabled=false for PRs");
        let prompt = std::fs::read_to_string(repo.join(".git/release-prompt")).unwrap();
        assert!(prompt.contains(&format!("git diff {} {}", identity.base, identity.candidate)));
        assert!(prompt.contains(&identity.source));
        assert_eq!(check(&db, &req).unwrap(), identity);
        // An exact-tree rerun still needs its own review after the new gate attempt.
        gate(&db, &req).unwrap();
        assert!(check(&db, &req).unwrap_err().to_string().contains("latest Full gate"));
        review(&db, &req).unwrap();
        assert!(check(&db, &req).is_ok());
    }

    #[test]
    fn failed_attempt_supersedes_previous_pass_and_cannot_release() {
        let (repo, db) = fixture();
        let req = request(&repo);
        let identity = gate(&db, &req).unwrap();
        review(&db, &req).unwrap();
        let attempt = start(&db, &identity, "gate", None).unwrap();
        assert!(check(&db, &req).is_err(), "incomplete attempt supersedes previous success");
        finish(&db, attempt, false, &"test failure").unwrap();
        assert!(review(&db, &req).is_err());
        assert!(check(&db, &req).is_err());
    }

    #[test]
    fn candidate_cannot_replace_release_policy_or_add_unapproved_stamping_paths() {
        let (repo, db) = fixture();
        stamp(&repo);
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), "[release]\ntag='*'\nstamp_paths=['.nitely/playbook.toml']\n[gate]\ncommands=['true']").unwrap();
        run_git(&repo, &["commit", "-qam", "weaken candidate policy", "--amend"]);
        assert!(gate(&db, &request(&repo)).unwrap_err().to_string().contains("does not allow stamping"));
    }

    #[test]
    fn dirty_candidate_and_new_policy_invalidate_release_evidence() {
        let (repo, db) = fixture();
        stamp(&repo);
        let req = request(&repo);
        gate(&db, &req).unwrap();
        review(&db, &req).unwrap();
        std::fs::write(repo.join("version.txt"), "3").unwrap();
        assert!(check(&db, &req).unwrap_err().to_string().contains("clean committed"));
        run_git(&repo, &["restore", "version.txt"]);
        // A policy-only integration advance changes the identity, even with the same candidate.
        run_git(&repo, &["checkout", "main"]);
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), format!("{POLICY}\n# new policy revision\n")).unwrap();
        run_git(&repo, &["commit", "-qam", "policy revision"]);
        run_git(&repo, &["checkout", "candidate"]);
        let original = commit(&repo, "candidate^").unwrap();
        let req = Request { source: &original, ..request(&repo) };
        assert!(check(&db, &req).is_err());
    }

    #[test]
    fn release_without_review_auth_or_with_human_policy_fails_closed() {
        let (repo, db) = fixture();
        let mut pb = Playbook::parse(POLICY).unwrap();
        pb.review.commands.insert("claude".into(), vec!["nitely-missing-release-reviewer-xyz".into()]);
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), toml::to_string(&pb).unwrap()).unwrap();
        run_git(&repo, &["commit", "-qam", "reviewer unavailable"]);
        gate(&db, &request(&repo)).unwrap();
        assert!(review(&db, &request(&repo)).is_err());
        assert!(check(&db, &request(&repo)).is_err());
        pb.approval.always_human = true;
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), toml::to_string(&pb).unwrap()).unwrap();
        run_git(&repo, &["commit", "-qam", "human release policy"]);
        gate(&db, &request(&repo)).unwrap();
        assert!(review(&db, &request(&repo)).unwrap_err().to_string().contains("needs human approval"));
    }

    #[test]
    fn failed_full_command_never_gets_a_release_receipt() {
        let (repo, db) = fixture();
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), POLICY.replace("echo conditional-check", "exit 7")).unwrap();
        run_git(&repo, &["commit", "-qam", "failing conditional check"]);
        let failure = gate(&db, &request(&repo)).unwrap_err().to_string();
        assert!(failure.contains("Full gate failed") && failure.contains("exit 7"), "{failure}");
        assert!(review(&db, &request(&repo)).is_err());
        let passed: bool = db.conn.query_row("SELECT passed FROM release_verification ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert!(!passed);
    }

    #[test]
    fn no_release_tag_reviews_from_empty_tree_instead_of_only_last_commit() {
        let (repo, db) = fixture();
        run_git(&repo, &["tag", "-d", "v1"]);
        let identity = prepare(&db, &request(&repo)).unwrap().identity;
        assert_eq!(git(&repo, &["cat-file", "-t", &identity.base]).unwrap().trim(), "tree");
        let diff = git(&repo, &["diff", "--name-only", &identity.base, &identity.candidate]).unwrap();
        assert!(diff.lines().any(|line| line == playbook::PLAYBOOK_PATH), "first release includes unchanged foundational files");
    }

    #[test]
    fn stable_baseline_keeps_earlier_beta_changes_in_the_release_review() {
        let (repo, db) = fixture();
        let stable = commit(&repo, "v1").unwrap();
        run_git(&repo, &["tag", "v2-beta.1"]);
        std::fs::write(repo.join("last-beta.txt"), "last beta change").unwrap();
        run_git(&repo, &["add", "last-beta.txt"]);
        run_git(&repo, &["commit", "-m", "another beta"]);
        run_git(&repo, &["tag", "v2-beta.2"]);
        let mut pb = Playbook::parse(&std::fs::read_to_string(repo.join(playbook::PLAYBOOK_PATH)).unwrap()).unwrap();
        pb.release.as_mut().unwrap().exclude_tags = vec!["*-*".into()];
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), toml::to_string(&pb).unwrap()).unwrap();
        run_git(&repo, &["commit", "-qam", "stable release policy"]);
        stamp(&repo);
        let identity = prepare(&db, &request(&repo)).unwrap().identity;
        assert_eq!(identity.base, stable);
        let diff = git(&repo, &["diff", "--name-only", &identity.base, &identity.candidate]).unwrap();
        assert!(diff.lines().any(|p| p == "app.txt"), "the first beta change must remain in the review");
        assert!(diff.lines().any(|p| p == "last-beta.txt"), "the later beta change must remain in the review");
        // Older policies retain their prior baseline semantics; candidate policy cannot opt out.
        run_git(&repo, &["checkout", "main"]);
        pb.release.as_mut().unwrap().exclude_tags.clear();
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), toml::to_string(&pb).unwrap()).unwrap();
        run_git(&repo, &["commit", "-qam", "original baseline semantics"]);
        let identity = prepare(&db, &request(&repo)).unwrap().identity;
        assert_eq!(identity.base, commit(&repo, "v2-beta.2").unwrap());
    }

    #[test]
    fn release_only_checks_do_not_replace_missing_full_gate() {
        let (repo, db) = fixture();
        let mut pb = Playbook::parse(POLICY).unwrap();
        pb.gate.commands.clear();
        pb.gate.steps.clear();
        std::fs::write(repo.join(playbook::PLAYBOOK_PATH), toml::to_string(&pb).unwrap()).unwrap();
        run_git(&repo, &["commit", "-qam", "missing full gate"]);
        assert!(gate(&db, &request(&repo)).unwrap_err().to_string().contains("release-only commands cannot replace"));
    }
}
