//! Nonblocking reviews of immutable integration ranges. This is not a delivery gate.
use crate::{
    db::now,
    playbook::{git, Playbook},
    review::Review,
    Actor, Config, Db, NewIssue,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

const MAX_ATTEMPTS: u32 = 3;
const ACTOR: &str = "nitely:periodic-review";

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub priority: u8,
    pub text: String,
    pub issue_key: Option<String>,
    pub action: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub base: String,
    pub head: String,
    pub state: String,
    pub attempt: u32,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub reviewer: Option<String>,
    pub error: Option<String>,
    pub findings: Vec<Finding>,
}
#[derive(Debug, Clone, Serialize)]
pub struct RepositoryStatus {
    pub repo: String,
    pub project: String,
    pub baseline: String,
    pub cursor: Option<String>,
    pub observed_head: String,
    pub pending_landings: u32,
    pub pending_since: Option<i64>,
    pub last_success_at: Option<i64>,
    pub error: Option<String>,
    pub jobs: Vec<Job>,
}

fn identity(repo: &Path) -> Result<String> {
    crate::verification_audit::repository_id(repo)
}
fn job(db: &Db, id: i64) -> Result<Job> {
    let mut j = db.conn.query_row("SELECT id,base,head,state,attempt,created_at,completed_at,reviewer,error FROM periodic_review_job WHERE id=?1", [id], |r| Ok(Job {
        id:r.get(0)?,base:r.get(1)?,head:r.get(2)?,state:r.get(3)?,attempt:r.get(4)?,created_at:r.get(5)?,completed_at:r.get(6)?,reviewer:r.get(7)?,error:r.get(8)?,findings:vec![],
    }))?;
    j.findings = db.conn.prepare("SELECT priority,text,issue_key,action FROM periodic_review_finding WHERE job=?1 ORDER BY priority,fingerprint")?
        .query_map([id], |r| Ok(Finding{priority:r.get(0)?,text:r.get(1)?,issue_key:r.get(2)?,action:r.get(3)?}))?.collect::<rusqlite::Result<_>>()?;
    Ok(j)
}
/// Read-only, bounded history; repo is the fixed common Git directory identity, not a display name.
pub fn status(db: &Db, project: &str) -> Result<Vec<RepositoryStatus>> {
    let mut rows: Vec<RepositoryStatus> = db.conn.prepare("SELECT repo,project,baseline,cursor,observed_head,last_success_at,error,pending_since FROM periodic_review_state WHERE project=?1 ORDER BY repo")?
        .query_map([project], |r| Ok(RepositoryStatus {repo:r.get(0)?,project:r.get(1)?,baseline:r.get(2)?,cursor:r.get(3)?,observed_head:r.get(4)?,last_success_at:r.get(5)?,error:r.get(6)?,pending_since:r.get(7)?,pending_landings:0,jobs:vec![]}))?
        .collect::<rusqlite::Result<_>>()?;
    for row in &mut rows {
        row.pending_landings = db.conn.query_row(
            "SELECT count(*) FROM periodic_review_landing WHERE repo=?1",
            [&row.repo],
            |r| r.get(0),
        )?;
        let ids = db
            .conn
            .prepare("SELECT id FROM periodic_review_job WHERE repo=?1 ORDER BY id DESC LIMIT 10")?
            .query_map([&row.repo], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        row.jobs = ids
            .into_iter()
            .map(|id| job(db, id))
            .collect::<Result<_>>()?;
    }
    Ok(rows)
}
fn rules(db: &Db, project: &str) -> Result<Option<crate::project_modes::Periodic>> {
    let mode = db.project_review_mode(project)?;
    if mode.mode == crate::project_modes::Mode::Yolo || db.project_yolo(project)?.enabled {
        return Ok(None);
    }
    Ok(mode.options().periodic.filter(|p| p.enabled))
}
fn initial(db: &Db, repo: &str, project: &str, head: &str) -> Result<()> {
    db.conn.execute("INSERT OR IGNORE INTO periodic_review_state(repo,project,baseline,observed_head) VALUES(?1,?2,?3,?3)",params![repo,project,head])?;
    let owner: String = db.conn.query_row(
        "SELECT project FROM periodic_review_state WHERE repo=?1",
        [repo],
        |r| r.get(0),
    )?;
    if owner != project {
        bail!("repository periodic history belongs to project {owner}; explicit project reassignment is required");
    }
    Ok(())
}
/// Runs in the shared single/batched landing completion path. Only durable bookkeeping.
pub(crate) fn on_landed(
    db: &Db,
    root: &Path,
    issue: &crate::IssueRecord,
    land: &crate::land::Landed,
) -> Result<()> {
    let (Some(project), Some(head), Some(base)) = (
        issue.project.as_deref(),
        land.head.as_deref(),
        land.landed_from.as_deref(),
    ) else {
        return Ok(());
    };
    let repo = identity(root)?;
    let exists: bool = db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM periodic_review_state WHERE repo=?1)",
        [&repo],
        |r| r.get(0),
    )?;
    if !exists && rules(db, project)?.is_none() {
        return Ok(());
    }
    let base = git(root, &["rev-parse", &format!("{base}^{{commit}}")])?
        .trim()
        .to_owned();
    let head = git(root, &["rev-parse", &format!("{head}^{{commit}}")])?
        .trim()
        .to_owned();
    initial(db, &repo, project, &base)?;
    db.conn.execute("INSERT OR IGNORE INTO periodic_review_landing(repo,project,issue_key,base,head,landed_at) VALUES(?1,?2,?3,?4,?5,?6)",params![repo,project,issue.key,base,head,now()])?;
    db.conn.execute(
        "UPDATE periodic_review_state SET pending_since=coalesce(pending_since,?2) WHERE repo=?1",
        params![repo, now()],
    )?;
    // Only the default service database may wake the installed service; fixture and imported
    // databases never accidentally launch against the person's backlog.
    if db
        .conn
        .path()
        .is_some_and(|p| Path::new(p) == crate::paths::db_path())
    {
        if let Some(policy) = rules(db, project)? {
            let count: u32 = db.conn.query_row(
                "SELECT count(*) FROM periodic_review_landing WHERE repo=?1",
                [&repo],
                |r| r.get(0),
            )?;
            if count >= policy.every_landings {
                if let Some(binary) = crate::install::installed_on_path() {
                    crate::submit_job::spawn_nitely(
                        &binary,
                        &[
                            "periodic-review".into(),
                            "--project".into(),
                            project.into(),
                            "--tick".into(),
                            "--quiet".into(),
                        ],
                        root,
                        &Path::new(&repo).join("nitely-periodic-review.log"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// One due repository per invocation. Independent of pre-merge review and Full-test workers.
pub fn tick_all(db: &mut Db, config: &Config) -> Result<()> {
    let mut errors = Vec::new();
    for r in &config.repos {
        let Some(project) = &r.project else { continue };
        match tick(db, Path::new(&r.root), project, &r.integration_branch) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => errors.push(format!("{}: {e:#}", r.name)),
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(())
}
/// Explicit retry resets only the failed job. Its cursor and newest mode checks remain intact.
pub fn retry(db: &Db, project: &str, repo: &str, id: i64) -> Result<()> {
    if db.conn.execute("UPDATE periodic_review_job SET state='pending',attempt=0,retry_at=0,error=NULL WHERE id=?1 AND project=?2 AND repo=?3 AND state='failed'",params![id,project,repo])?!=1 {
        bail!("only a failed periodic review in this project and repository can be retried");
    }
    Ok(())
}
fn tick(db: &mut Db, root: &Path, project: &str, branch: &str) -> Result<bool> {
    let Some(policy) = rules(db, project)? else {
        return Ok(false);
    };
    let repo = identity(root)?;
    let Some(_lock) = crate::land::Lock::try_take(
        &Path::new(&repo).join("nitely-periodic-review.lock"),
        "periodic review",
        false,
    )?
    else {
        return Ok(false);
    };
    let outcome = (|| -> Result<bool> {
        let head = git(root, &["rev-parse", &format!("refs/heads/{branch}")])?
            .trim()
            .to_owned();
        initial(db, &repo, project, &head)?;
        db.conn.execute("UPDATE periodic_review_state SET pending_since=CASE WHEN observed_head!=?2 THEN coalesce(pending_since,?3) ELSE pending_since END,observed_head=?2 WHERE repo=?1",params![repo,head,now()])?;
        let base: String = db.conn.query_row(
            "SELECT coalesce(cursor,baseline) FROM periodic_review_state WHERE repo=?1",
            [&repo],
            |r| r.get(0),
        )?;
        git(root, &["merge-base", "--is-ancestor", &base, &head])
            .context("integration history changed; retained periodic cursor is not an ancestor")?;
        let pending_commits = range_commits(root, &base, &head)?;
        let old_heads = db
            .conn
            .prepare("SELECT DISTINCT head FROM periodic_review_landing WHERE repo=?1")?
            .query_map([&repo], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for old in old_heads {
            if !pending_commits.contains(&old)
                && git(root, &["merge-base", "--is-ancestor", &old, &base]).is_ok()
            {
                db.conn.execute(
                    "DELETE FROM periodic_review_landing WHERE repo=?1 AND head=?2",
                    params![repo, old],
                )?;
            }
        }
        let existing:Option<i64>=db.conn.query_row("SELECT id FROM periodic_review_job WHERE repo=?1 AND state IN ('pending','running','applying','failed')",[&repo],|r|r.get(0)).optional()?;
        let id = if let Some(id) = existing {
            id
        } else {
            let count: u32 = db.conn.query_row(
                "SELECT count(*) FROM periodic_review_landing WHERE repo=?1",
                [&repo],
                |r| r.get(0),
            )?;
            let since: Option<i64> = db.conn.query_row(
                "SELECT pending_since FROM periodic_review_state WHERE repo=?1",
                [&repo],
                |r| r.get(0),
            )?;
            // An external integration update or crash between Git and the landing hook still
            // becomes due by time, without inventing issue/landing counts.
            if head == base
                || !(count >= policy.every_landings.max(1)
                    || (policy.max_delay_seconds > 0
                        && since
                            .is_some_and(|s| now().saturating_sub(s) >= policy.max_delay_seconds)))
            {
                return Ok(false);
            }
            db.conn.execute("INSERT OR IGNORE INTO periodic_review_job(repo,project,base,head,created_at) VALUES(?1,?2,?3,?4,?5)",params![repo,project,base,head,now()])?;
            db.conn.query_row(
                "SELECT id FROM periodic_review_job WHERE repo=?1 AND base=?2 AND head=?3",
                params![repo, base, head],
                |r| r.get(0),
            )?
        };
        let mut current = job(db, id)?;
        if current.state == "running" {
            // Owning the process lock proves the prior worker is gone. Even an exhausted
            // attempt must become explicitly failed so the supported retry can recover it.
            db.conn.execute("UPDATE periodic_review_job SET state='failed',lease_until=NULL,error='Previous periodic reviewer stopped before retaining its result' WHERE id=?1 AND state='running'",[id])?;
            current = job(db, id)?;
        }
        if current.state != "applying" {
            let retry_at: i64 = db.conn.query_row(
                "SELECT retry_at FROM periodic_review_job WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            // The process lock proves a previous owner is gone even if its clock lease remains.
            if current.attempt >= MAX_ATTEMPTS || retry_at > now() {
                return Ok(false);
            }
            db.conn.execute("UPDATE periodic_review_job SET state='running',attempt=attempt+1,lease_until=?2,error=NULL WHERE id=?1",params![id,now()+7200])?;
            current = job(db, id)?;
            let result =
                crate::verification_backstop::with_checkout(root, &current.head, |checkout| {
                    full_review(db, root, project, branch, &current, checkout)
                });
            match result {
                Ok(review) => record_review(db, &repo, &current, &review)?,
                Err(e) => {
                    let error = format!("{e:#}");
                    db.conn.execute("UPDATE periodic_review_job SET state='failed',error=?3,lease_until=NULL,retry_at=?4 WHERE id=?1 AND attempt=?2",params![id,current.attempt,error,now()+300*i64::from(current.attempt)])?;
                    bail!("{error}");
                }
            }
        }
        // Settings may have changed during the independent review. Retain its evidence but defer actions.
        let Some(_) = rules(db, project)? else {
            return Ok(true);
        };
        apply_findings(db, root, &repo, project, branch, id)?;
        db.conn.execute(
            "UPDATE periodic_review_state SET error=NULL WHERE repo=?1",
            [&repo],
        )?;
        Ok(true)
    })();
    if let Err(e) = &outcome {
        db.conn.execute(
            "UPDATE periodic_review_state SET error=?2 WHERE repo=?1",
            params![repo, format!("{e:#}")],
        )?;
    }
    outcome
}
fn full_review(
    db: &Db,
    root: &Path,
    project: &str,
    branch: &str,
    j: &Job,
    checkout: &Path,
) -> Result<Review> {
    let _performance = crate::performance_attribution::Scope::new(
        db,
        crate::performance_attribution::Context {
            project: Some(project.into()),
            ..Default::default()
        },
    );
    let mode = db.project_review_mode(project)?;
    let policy = rules(db, project)?.context("periodic review was disabled")?;
    let policy_head = git(root, &["rev-parse", &format!("refs/heads/{branch}")])?;
    let policy_blob = git(
        root,
        &[
            "rev-parse",
            &format!("{}:{}", policy_head.trim(), crate::playbook::PLAYBOOK_PATH),
        ],
    )?;
    let mut review = Playbook::parse(&git(
        root,
        &[
            "show",
            &format!("{}:{}", policy_head.trim(), crate::playbook::PLAYBOOK_PATH),
        ],
    )?)?
    .review;
    review.timeout_minutes = review.timeout_minutes.clamp(1, 60);
    review.policy_root = None;
    review.yolo_guards.clear();
    let reviewer = if policy.reviewer == "configured" {
        review
            .choose("periodic", &|r| review.is_installed(r, checkout))?
            .reviewer
    } else {
        policy.reviewer.clone()
    };
    let sources = source_issues(db, &identity(root)?, j)?;
    review.context=format!("Independent periodic integration review of exact {}..{}. This cannot approve, revert or block delivery. Review the whole diff. Cite the file, trigger and wrong outcome. Attribute a finding to a source issue only when unambiguous by including its exact key; otherwise omit attribution. Source issues: {}.\n{}",j.base,j.head,sources.join(", "),review.context);
    for key in &sources {
        for accepted in db.acceptances(key)? {
            review.context.push_str(&format!("\nExisting human decision on {}: {} — {}. Preserve this decision; only raise a materially new regression.",crate::issues::issue_link(key),accepted.finding,accepted.reason));
        }
    }
    let mut check = || -> Result<()> {
        let latest = db.project_review_mode(project)?;
        if latest.revision != mode.revision || rules(db, project)?.is_none() {
            bail!("periodic settings changed before launch; retry against latest human authority");
        }
        if git(
            root,
            &[
                "rev-parse",
                &format!("refs/heads/{branch}:{}", crate::playbook::PLAYBOOK_PATH),
            ],
        )?
        .trim()
            != policy_blob.trim()
        {
            bail!("integration review policy changed before reviewer launch; retry");
        }
        if git(checkout, &["rev-parse", "HEAD"])?.trim() != j.head
            || !git(checkout, &["status", "--porcelain"])?.trim().is_empty()
        {
            bail!("periodic checkout must remain clean at the exact reviewed revision");
        }
        Ok(())
    };
    check()?;
    let reply = review.run_prompt_checked(
        &reviewer,
        checkout,
        &review.prompt(&j.base, None),
        &mut check,
    )?;
    let mut result = review.parse(&reviewer, &reply.text, reply.seconds, None, Some(checkout))?;
    result.tokens = reply.tokens;
    if git(checkout, &["rev-parse", "HEAD"])?.trim() != j.head
        || !git(checkout, &["status", "--porcelain"])?.trim().is_empty()
    {
        bail!("reviewer modified the immutable review checkout");
    }
    Ok(result)
}
fn record_review(db: &mut Db, repo: &str, j: &Job, review: &Review) -> Result<()> {
    if !matches!(review.verdict.as_str(), "pass" | "changes")
        || review.reused.is_some()
        || review.findings.len() > 50
    {
        bail!("periodic review requires a fresh completed result with at most 50 findings");
    }
    let serialized = serde_json::to_string(review)?;
    if serialized.len() > 1024 * 1024 {
        bail!("periodic review result exceeds the 1 MiB retention bound");
    }
    let tx = db
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if tx.execute("UPDATE periodic_review_job SET state='applying',result=?3,reviewer=?4,lease_until=NULL WHERE id=?1 AND attempt=?2 AND state='running'",params![j.id,j.attempt,serialized,review.reviewer])?!=1 {bail!("periodic review completion lost its lease");}
    for finding in &review.findings {
        if finding.text.len() > 16_384 {
            bail!("periodic finding is too large");
        }
        let fingerprint = fingerprint(repo, &finding.text);
        tx.execute("INSERT OR IGNORE INTO periodic_review_finding(job,fingerprint,priority,text) VALUES(?1,?2,?3,?4)",params![j.id,fingerprint,finding.priority,finding.text])?;
    }
    tx.commit()?;
    Ok(())
}
fn fingerprint(repo: &str, text: &str) -> String {
    let normal = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!(
        "{:x}",
        Sha256::digest(format!("{repo}\0{normal}").as_bytes())
    )
}
fn range_commits(root: &Path, base: &str, head: &str) -> Result<std::collections::HashSet<String>> {
    let range = git(
        root,
        &["rev-list", "--max-count=20001", &format!("{base}..{head}")],
    )?;
    let commits: std::collections::HashSet<String> = range.lines().map(str::to_owned).collect();
    if commits.len() > 20_000 {
        bail!(
            "periodic review range exceeds 20000 commits; explicit baseline recovery is required"
        );
    }
    Ok(commits)
}
fn source_issues(db: &Db, repo: &str, j: &Job) -> Result<Vec<String>> {
    let rows=db.conn.prepare("SELECT DISTINCT issue_key,head FROM periodic_review_landing WHERE repo=?1 ORDER BY issue_key")?
        .query_map([repo],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let commits = range_commits(Path::new(repo), &j.base, &j.head)?;
    let mut keys = Vec::new();
    for (key, head) in rows {
        if commits.contains(head.as_str()) && !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}
fn apply_findings(
    db: &mut Db,
    root: &Path,
    repo: &str,
    project: &str,
    branch: &str,
    id: i64,
) -> Result<()> {
    let j = job(db, id)?;
    let sources = source_issues(db, repo, &j)?;
    let actor = Actor::Agent(ACTOR.into());
    for f in j.findings.iter().filter(|f| f.action == "pending") {
        let Some(policy) = rules(db, project)? else {
            return Ok(());
        };
        let fp = fingerprint(repo, &f.text);
        let marker = format!("[periodic-finding:{fp}:job:{id}]");
        let previous:Option<String>=db.conn.query_row("SELECT issue_key FROM periodic_review_finding WHERE fingerprint=?1 AND issue_key IS NOT NULL ORDER BY job DESC LIMIT 1",[&fp],|r|r.get(0)).optional()?;
        let mut accepted = None;
        for key in sources.iter().chain(previous.iter()) {
            if db.acceptances(key)?.iter().any(|a| a.finding == f.text) {
                accepted = Some(key.clone());
                break;
            }
        }
        if let Some(key) = accepted {
            db.conn.execute("UPDATE periodic_review_finding SET issue_key=?3,action='accepted' WHERE job=?1 AND fingerprint=?2 AND action='pending'",params![id,fp,key])?;
            continue;
        }
        // A marker belongs to this exact job, so replay after a crash cannot redo its action.
        let existing: Option<String> = db
            .conn
            .query_row(
                "SELECT key FROM issue WHERE project=?1 AND instr(body,?2)>0 ORDER BY key LIMIT 1",
                params![project, marker],
                |r| r.get(0),
            )
            .optional()?;
        let existing=existing.or(db.conn.query_row("SELECT e.issue_key FROM event e JOIN issue i ON i.key=e.issue_key WHERE i.project=?1 AND e.actor=?2 AND instr(e.summary,?3)>0 ORDER BY e.id LIMIT 1",params![project,ACTOR,marker],|r|r.get(0)).optional()?);
        let previous_open = match &previous {
            Some(key) => db.issue(key)?.is_some_and(|i| i.is_open()),
            None => false,
        };
        let existing = if existing.is_some() {
            existing
        } else {
            previous.as_ref().filter(|_| previous_open).cloned()
        };
        let (key, action) = if let Some(key) = existing {
            (key, "existing")
        } else {
            // Attribution must explicitly name exactly one source card. Never undo a human decision.
            let named: Vec<_> = sources
                .iter()
                .filter(|key| {
                    f.text
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                        .any(|word| word == key.as_str())
                })
                .collect();
            let candidate = if named.len() == 1 {
                db.issue(named[0])?
            } else {
                None
            };
            let can_reopen = if let Some(i) = &candidate {
                policy.findings_strategy == "reopen_or_create"
                    && i.state == "Done"
                    && i.origin == "nitely"
                    && db.acceptances(&i.key)?.is_empty()
                    && !db.timeline(&i.key)?.iter().any(|e| {
                        e.actor == "human"
                            && matches!(e.kind.as_str(), "issue_moved" | "ask_answered")
                    })
                    && git(root, &["rev-parse", &format!("refs/heads/{branch}")])?.trim() == j.head
            } else {
                false
            };
            if can_reopen {
                if rules(db, project)?.as_ref() != Some(&policy) {
                    return Ok(());
                }
                let key = candidate.unwrap().key;
                db.move_issue(
                    &key,
                    "Todo",
                    None,
                    Some(&format!(
                        "Periodic review {}..{}: {}\n{}",
                        j.base, j.head, f.text, marker
                    )),
                    &actor,
                )?;
                (key, "reopened")
            } else {
                if rules(db, project)?.as_ref() != Some(&policy) {
                    return Ok(());
                }
                let mut links = sources
                    .iter()
                    .map(|s| crate::issues::issue_link(s))
                    .collect::<Vec<_>>();
                if let Some(previous) = &previous {
                    links.push(format!(
                        "Previous occurrence: {}",
                        crate::issues::issue_link(previous)
                    ));
                }
                let links = links.join(", ");
                let issue=db.create_issue(&NewIssue {
                    project:project.into(),title:crate::issues::headline(&f.text,120),
                    body:format!("Periodic integration review found:\n\n{}\n\nReviewed exact {}..{} in job {}. Source changes: {}.\nConfirm the finding remains applicable on current main before editing; later changes or prior human decisions may affect it.\n\n{}",f.text,j.base,j.head,id,links,marker),
                    kind:"bug".into(),source:match j.reviewer.as_deref() { Some("claude")=>"claude",Some("cursor")=>"cursor",_=>"codex" }.into(),evidence:format!("Fresh {} review of {}..{}",j.reviewer.as_deref().unwrap_or("independent"),j.base,j.head),
                    priority:i64::from(f.priority.saturating_add(1).clamp(1,4)),allow_similar:true,..Default::default()
                },&actor)?;
                (issue.key, "filed")
            }
        };
        db.conn.execute("UPDATE periodic_review_finding SET issue_key=?3,action=?4 WHERE job=?1 AND fingerprint=?2 AND action='pending'",params![id,fp,key,action])?;
    }
    // Preserve landings that arrived while this review was running.
    let covered = db
        .conn
        .prepare("SELECT issue_key,head FROM periodic_review_landing WHERE repo=?1")?
        .query_map([repo], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let reviewed_commits = range_commits(root, &j.base, &j.head)?;
    let covered: Vec<_> = covered
        .into_iter()
        .filter(|(_, h)| h == &j.base || reviewed_commits.contains(h))
        .collect();
    let tx = db
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mode = crate::project_modes::read(&tx, project)?;
    if mode.mode == crate::project_modes::Mode::Yolo
        || !mode.options().periodic.is_some_and(|p| p.enabled)
    {
        return Ok(());
    }
    if tx.execute("UPDATE periodic_review_state SET cursor=?3,last_success_at=?4,error=NULL,pending_since=CASE WHEN observed_head=?3 THEN NULL ELSE pending_since END WHERE repo=?1 AND coalesce(cursor,baseline)=?2",params![repo,j.base,j.head,now()])?!=1 {bail!("periodic cursor changed; refusing stale completion");}
    for (key, head) in covered {
        tx.execute(
            "DELETE FROM periodic_review_landing WHERE repo=?1 AND issue_key=?2 AND head=?3",
            params![repo, key, head],
        )?;
    }
    tx.execute("UPDATE periodic_review_job SET state=?2,completed_at=?3,error=NULL WHERE id=?1 AND state='applying'",params![id,if j.findings.is_empty() {"passed"} else {"findings"},now()])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_modes::{Mode, Options, Periodic};
    use std::path::PathBuf;

    struct Fixture {
        db: Db,
        root: PathBuf,
        response: PathBuf,
    }
    impl Fixture {
        fn new(every: u32) -> Self {
            let root = crate::test_repo::copy("periodic-review", |p| {
                crate::test_repo::init(p, &[("value.txt", "base")])
            });
            let response = root.parent().unwrap().join(format!(
                "{}.response",
                root.file_name().unwrap().to_string_lossy()
            ));
            std::fs::write(&response, "VERDICT: clean\n").unwrap();
            let mut review = crate::review::ReviewRules {
                enabled: false,
                ..Default::default()
            };
            review.commands.clear();
            review.reviewer.clear();
            review.reviewer.insert("*".into(), "fake".into());
            review.commands.insert(
                "fake".into(),
                vec![
                    crate::playbook::posix_sh(),
                    "-c".into(),
                    "cat \"$1\"".into(),
                    "--".into(),
                    response.to_string_lossy().into_owned(),
                ],
            );
            std::fs::create_dir_all(root.join(".nitely")).unwrap();
            let playbook = Playbook { review, ..Default::default() };
            let text = toml::to_string(&playbook).unwrap();
            let parsed = Playbook::parse(&text).unwrap();
            assert_eq!(parsed.review.reviewer_for("periodic"), Some("fake"));
            assert_eq!(parsed.review.commands.len(), 1);
            std::fs::write(root.join(".nitely/playbook.toml"), text).unwrap();
            crate::test_repo::git(&root, &["add", "."]);
            crate::test_repo::git(&root, &["commit", "-qm", "review policy"]);
            let mut db = Db::open(&root.parent().unwrap().join(format!(
                "{}.sqlite",
                root.file_name().unwrap().to_string_lossy()
            )))
            .unwrap();
            db.create_project("APP", "Example").unwrap();
            db.set_project_review_mode(
                "APP",
                Mode::Custom,
                Options {
                    periodic: Some(Periodic {
                        enabled: true,
                        every_landings: every,
                        max_delay_seconds: 1800,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                0,
                &Actor::Human,
            )
            .unwrap();
            Self { db, root, response }
        }
        fn land(&mut self, title: &str) -> (String, String) {
            let base = crate::test_repo::git(&self.root, &["rev-parse", "HEAD"]);
            std::fs::write(self.root.join("value.txt"), title).unwrap();
            crate::test_repo::git(&self.root, &["commit", "-qam", title]);
            let head = crate::test_repo::git(&self.root, &["rev-parse", "HEAD"]);
            let actor = Actor::Agent("codex:test".into());
            let issue = self
                .db
                .create_issue(
                    &NewIssue {
                        project: "APP".into(),
                        title: title.into(),
                        body: "Concrete source change".into(),
                        kind: "feature".into(),
                        source: "codex".into(),
                        evidence: "fixture".into(),
                        allow_similar: true,
                        ..Default::default()
                    },
                    &actor,
                )
                .unwrap();
            let landed = crate::land::Landed {
                head: Some(head.clone()),
                landed_from: Some(base),
                ..Default::default()
            };
            on_landed(&self.db, &self.root, &issue, &landed).unwrap();
            on_landed(&self.db, &self.root, &issue, &landed).unwrap();
            self.db
                .move_issue(&issue.key, "Done", Some("landed"), None, &actor)
                .unwrap();
            (issue.key, head)
        }
        fn status(&self) -> RepositoryStatus {
            status(&self.db, "APP").unwrap().remove(0)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.response).ok();
            // The database is outside the disposable Git checkout and may still be open here.
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    #[test]
    fn periodic_scenario_threshold_failure_retry_reopen_and_duplicate_finding() {
        let mut f = Fixture::new(2);
        let (source, _) = f.land("First meaningful change");
        assert!(!tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(
            f.status().pending_landings,
            1,
            "duplicate completion hooks count once"
        );
        let (_, head) = f.land("Second meaningful change");
        std::fs::write(&f.response, "unfinished reviewer output").unwrap();
        assert!(tick(&mut f.db, &f.root, "APP", "main").is_err());
        let failed = f.status();
        assert_eq!(failed.cursor, None);
        assert_eq!(failed.jobs[0].state, "failed");
        assert_eq!(failed.pending_landings, 2);
        let finding=format!("- [P1] {source} value.txt: an empty input is persisted and breaks the next read.\nVERDICT: findings\n");
        std::fs::write(&f.response, &finding).unwrap();
        f.db.conn
            .execute("UPDATE periodic_review_job SET retry_at=0", [])
            .unwrap();
        assert!(
            tick(&mut f.db, &f.root, "APP", "main").unwrap(),
            "periodic review runs although pre-merge review.enabled=false"
        );
        let done = f.status();
        assert_eq!(done.cursor.as_deref(), Some(head.as_str()));
        assert_eq!(done.jobs[0].state, "findings");
        assert_eq!(done.jobs[0].findings[0].action, "reopened");
        assert_eq!(f.db.issue(&source).unwrap().unwrap().state, "Todo");
        f.land("Third meaningful change");
        f.land("Fourth meaningful change");
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        let latest = f.status();
        assert_eq!(
            latest.jobs[0].findings[0].issue_key.as_deref(),
            Some(source.as_str())
        );
        assert_eq!(latest.jobs[0].findings[0].action, "existing");
        assert_eq!(latest.pending_landings, 0);
        assert_eq!(f.db.list_issues(Some("APP"), true).unwrap().len(), 4);
        f.db.move_issue(
            &source,
            "Done",
            Some("fixed and verified"),
            None,
            &Actor::Agent("codex:test".into()),
        )
        .unwrap();
        f.land("Fifth meaningful change");
        f.land("Sixth meaningful change");
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        let recurrence = f.status();
        assert_eq!(
            recurrence.jobs[0].findings[0].action, "filed",
            "a closed previous occurrence does not swallow a fresh finding"
        );
        assert_ne!(
            recurrence.jobs[0].findings[0].issue_key.as_deref(),
            Some(source.as_str())
        );
        let mode = f.db.project_review_mode("APP").unwrap();
        f.db.set_project_review_mode(
            "APP",
            Mode::Yolo,
            Options::default(),
            mode.revision,
            &Actor::Human,
        )
        .unwrap();
        f.land("Seventh meaningful change");
        assert!(!tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(
            f.status().jobs.len(),
            3,
            "YOLO does not enqueue automatic reviews"
        );
    }

    #[test]
    fn periodic_durable_completion_preserves_new_landings_and_replays_card_actions_once() {
        let mut f = Fixture::new(1);
        let (source, head) = f.land("Original change");
        let repo = identity(&f.root).unwrap();
        let base = f.status().baseline;
        f.db.conn.execute("INSERT INTO periodic_review_job(repo,project,base,head,state,attempt,created_at) VALUES(?1,'APP',?2,?3,'running',1,?4)",params![repo,base,head,now()]).unwrap();
        let id = f.db.conn.last_insert_rowid();
        let claimed = job(&f.db, id).unwrap();
        std::fs::write(&f.response,format!("- [P1] {source} value.txt: missing validation allows empty data.\nVERDICT: findings\n")).unwrap();
        let review = crate::verification_backstop::with_checkout(&f.root, &head, |wt| {
            full_review(&f.db, &f.root, "APP", "main", &claimed, wt)
        })
        .unwrap();
        record_review(&mut f.db, &repo, &claimed, &review).unwrap();
        assert!(
            record_review(&mut f.db, &repo, &claimed, &review).is_err(),
            "stale completion cannot overwrite retained review"
        );
        let settings = f.db.project_review_mode("APP").unwrap();
        f.db.set_project_review_mode(
            "APP",
            Mode::Yolo,
            Options::default(),
            settings.revision,
            &Actor::Human,
        )
        .unwrap();
        apply_findings(&mut f.db, &f.root, &repo, "APP", "main", id).unwrap();
        assert_eq!(
            f.status().jobs[0].findings[0].action,
            "pending",
            "mode off defers even a retained review's mutations"
        );
        assert_eq!(f.status().cursor, None);
        let revision = f.db.project_review_mode("APP").unwrap().revision;
        f.db.set_project_review_mode(
            "APP",
            settings.mode,
            settings.overrides,
            revision,
            &Actor::Human,
        )
        .unwrap();
        let (_, next) = f.land("Concurrent later change");
        let dbpath = PathBuf::from(f.db.conn.path().unwrap());
        f.db = Db::open(&dbpath).unwrap();
        assert!(
            tick(&mut f.db, &f.root, "APP", "main").unwrap(),
            "restart resumes retained review without launching reviewer"
        );
        let s = f.status();
        assert_eq!(s.cursor.as_deref(), Some(head.as_str()));
        assert_eq!(s.pending_landings, 1);
        assert_eq!(
            s.jobs[0].findings[0].action, "filed",
            "a moving main prevents reopening the old card from stale evidence"
        );
        let followup = s.jobs[0].findings[0].issue_key.clone().unwrap();
        assert!(f
            .db
            .issue(&followup)
            .unwrap()
            .unwrap()
            .body
            .contains(&crate::issues::issue_link(&source)));
        // Simulate interruption after creating the issue but before recording its action.
        f.db.conn
            .execute(
                "UPDATE periodic_review_finding SET action='pending',issue_key=NULL WHERE job=?1",
                [id],
            )
            .unwrap();
        f.db.conn
            .execute(
                "UPDATE periodic_review_job SET state='applying' WHERE id=?1",
                [id],
            )
            .unwrap();
        f.db.conn
            .execute(
                "UPDATE periodic_review_state SET cursor=NULL WHERE repo=?1",
                [&repo],
            )
            .unwrap();
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(
            f.status().jobs[0].findings[0].issue_key.as_deref(),
            Some(followup.as_str())
        );
        assert_eq!(f.db.list_issues(Some("APP"), true).unwrap().len(), 3);
        std::fs::write(&f.response, "VERDICT: clean\n").unwrap();
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(f.status().cursor.as_deref(), Some(next.as_str()));
    }

    #[test]
    fn periodic_last_attempt_crash_becomes_retryable_without_advancing_cursor() {
        let mut f = Fixture::new(1);
        let (_, head) = f.land("Crash recovery source");
        let repo = identity(&f.root).unwrap();
        let base = f.status().baseline;
        f.db.conn.execute("INSERT INTO periodic_review_job(repo,project,base,head,state,attempt,created_at,lease_until) VALUES(?1,'APP',?2,?3,'running',3,?4,?5)",params![repo,base,head,now(),now()+3600]).unwrap();
        let id = f.db.conn.last_insert_rowid();
        assert!(!tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(f.status().jobs[0].state, "failed");
        assert_eq!(f.status().cursor, None);
        assert!(retry(&f.db, "APP", "different-repository", id).is_err());
        retry(&f.db, "APP", &repo, id).unwrap();
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(f.status().cursor.as_deref(), Some(head.as_str()));
        f.db.create_project("WEB", "Another project").unwrap();
        f.db.set_project_review_mode("WEB", Mode::Fast, Options::default(), 0, &Actor::Human)
            .unwrap();
        assert!(tick(&mut f.db, &f.root, "WEB", "main")
            .unwrap_err()
            .to_string()
            .contains("belongs to project APP"));
        assert!(status(&f.db, "WEB").unwrap().is_empty());
    }

    #[test]
    fn periodic_exact_accepted_finding_records_decision_without_reopening_or_filing() {
        let mut f = Fixture::new(1);
        let (source, _) = f.land("Accepted limitation");
        let text = format!("{source} value.txt: empty strings are intentionally retained");
        f.db.conn.execute("INSERT INTO accepted_finding(issue_key,patch_id,priority,finding,reason,ts,signature) VALUES(?1,'fixture',2,?2,'intentional compatibility tradeoff',?3,'fixture')",params![source,text,now()]).unwrap();
        std::fs::write(&f.response, format!("- [P2] {text}\nVERDICT: findings\n")).unwrap();
        assert!(tick(&mut f.db, &f.root, "APP", "main").unwrap());
        assert_eq!(f.status().jobs[0].findings[0].action, "accepted");
        assert_eq!(f.db.issue(&source).unwrap().unwrap().state, "Done");
        assert_eq!(f.db.list_issues(Some("APP"), true).unwrap().len(), 1);
    }
}
