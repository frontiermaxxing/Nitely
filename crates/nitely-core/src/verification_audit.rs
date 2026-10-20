//! Durable post-land sampling. Classification and sampling use the authoritative policy;
//! neither a declaration edit nor another candidate revision can reroll an existing sample.
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::{db::now, issues::Actor, review::{Review, ReviewRules}, Db};

pub const DEFAULT_PERCENT: u8 = 10;
const MAX_ROWS: usize = 200;
// Review commands have their own execution timeout. This lease recovers a killed worker;
// its attempt number fences a late result if another worker has already recovered it.
const LEASE_SECONDS: i64 = 3 * 60 * 60;

pub fn repository_id(repo:&Path)->Result<String> {
    let path=crate::playbook::git(repo,&["rev-parse","--path-format=absolute","--git-common-dir"])?;
    Ok(crate::paths::normalize(path.trim()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub repo: String,
    pub project: String,
    pub issue_key: String,
    /// Hash of authoritative policy contents + classifier, NOT its changing base commit.
    pub policy: String,
    pub author: String,
    pub declarer: Option<String>,
    pub declaration_source: Option<String>,
    pub declared: Option<String>,
    pub effective: String,
    /// Stable lowering-rule identities; a confirmed escape disables these across policies.
    pub rules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub id: i64,
    pub percent: u8,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditJob {
    #[serde(default)]
    pub rules: Vec<String>,
    pub repo: String,
    pub id: i64,
    pub project: String,
    pub issue_key: String,
    pub base: String,
    pub head: String,
    pub author: String,
    pub selected: bool,
    pub state: String,
    pub attempt: i64,
    pub lease_until: Option<i64>,
    pub result: Option<Review>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rate {
    pub name: String,
    pub landed: i64,
    pub audited: i64,
    pub confirmed_escape_lands: i64,
    pub declared_lands: i64,
    pub matching_declarations: i64,
    /// Missing before any completed audit; never infer safety from unaudited lands.
    pub audited_escape_rate: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub jobs: Vec<AuditJob>,
    pub rules: Vec<Rate>,
    pub declarers: Vec<Rate>,
    pub disabled_rules: Vec<String>,
    pub escapes: Vec<Escape>,
    pub jobs_truncated: bool,
    pub rules_truncated: bool,
    pub declarers_truncated: bool,
    pub disabled_rules_truncated: bool,
    pub escapes_truncated: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Escape {
    pub id:i64,
    pub audit_id:i64,
    pub issue_key:String,
    pub head:String,
    pub rule:String,
    pub source:String,
    pub evidence:String,
    pub confirmed_by:String,
    pub confirmed_at:i64,
}

pub fn policy_fingerprint(authoritative_policy: &[u8], classifier: &str) -> String {
    let mut hash = Sha256::new();
    hash.update((authoritative_policy.len() as u64).to_be_bytes());
    hash.update(authoritative_policy);
    hash.update(classifier.as_bytes());
    format!("{:x}", hash.finalize())
}

fn validate(candidate: &Candidate, percent: u8) -> Result<()> {
    if candidate.repo.is_empty() || candidate.repo.len()>4096 { bail!("audit needs a bounded repository identity"); }
    if !(DEFAULT_PERCENT..=100).contains(&percent) { bail!("audit sample must be between 10 and 100 percent"); }
    if candidate.policy.len() != 64 || !candidate.policy.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("audit policy must be an authoritative SHA-256 fingerprint");
    }
    if !matches!(candidate.effective.as_str(), "inert" | "contained" | "basic") { bail!("only reduced candidates enter audit sampling"); }
    if candidate.rules.is_empty() || candidate.rules.len() > 100 || candidate.rules.iter().any(|r| r.is_empty() || r.len() > 256) {
        bail!("audit candidate needs 1–100 bounded lowering-rule identities");
    }
    Ok(())
}

fn sample_in(conn: &rusqlite::Connection, c: &Candidate, percent: u8) -> Result<Sample> {
    let project: Option<String> = conn.query_row("SELECT project FROM issue WHERE key=?1", [&c.issue_key], |r| r.get(0)).optional()?;
    if project.as_deref() != Some(&c.project) { bail!("audit issue does not belong to project"); }
    if let Some(sample) = conn.query_row("SELECT id,percent,selected FROM verification_sample WHERE issue_key=?1 AND policy=?2", params![c.issue_key,c.policy], |r| Ok(Sample{id:r.get(0)?,percent:r.get(1)?,selected:r.get(2)?})).optional()? {
        return Ok(sample);
    }
    let mut entropy = [0u8; 32];
    getrandom::fill(&mut entropy).map_err(|e| anyhow::anyhow!("audit entropy unavailable: {e}"))?;
    conn.execute("INSERT OR IGNORE INTO verification_audit_salt(project,salt) VALUES(?1,?2)", params![c.project,entropy.as_slice()])?;
    let salt: Vec<u8> = conn.query_row("SELECT salt FROM verification_audit_salt WHERE project=?1", [&c.project], |r| r.get(0))?;
    let mut hash = Sha256::new();
    hash.update(salt);
    hash.update((c.issue_key.len() as u64).to_be_bytes());
    hash.update(c.issue_key.as_bytes());
    hash.update(c.policy.as_bytes());
    let digest = hash.finalize();
    let draw = u64::from_be_bytes(digest[..8].try_into().expect("SHA256 has eight bytes"));
    let selected = u128::from(draw) * 100 < (u128::from(u64::MAX) + 1) * u128::from(percent);
    conn.execute("INSERT INTO verification_sample(project,issue_key,policy,percent,selected,created_at) VALUES(?1,?2,?3,?4,?5,?6)", params![c.project,c.issue_key,c.policy,percent,selected,now()])?;
    Ok(Sample { id:conn.last_insert_rowid(), percent, selected })
}

/// Call at candidate assessment, then again at landing with the same policy fingerprint.
/// The transaction serializes first selection across concurrent submit/land workers.
pub fn record_candidate(db: &mut Db, candidate: &Candidate, percent: Option<u8>) -> Result<Sample> {
    let percent = percent.unwrap_or(DEFAULT_PERCENT);
    validate(candidate, percent)?;
    let tx = db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sample = sample_in(&tx, candidate, percent)?;
    tx.commit()?;
    Ok(sample)
}

/// Record every reduced landing (including unsampled ones, for denominators), idempotently.
/// Call only after the integration ref update is confirmed; base/head are that exact diff.
pub fn landed(db: &mut Db, candidate: &Candidate, base: &str, head: &str, percent: Option<u8>) -> Result<i64> {
    let percent = percent.unwrap_or(DEFAULT_PERCENT);
    validate(candidate, percent)?;
    for commit in [base,head] {
        if !matches!(commit.len(),40|64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) { bail!("audit requires full immutable commit IDs"); }
    }
    let tx = db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(id) = tx.query_row("SELECT id FROM verification_audit WHERE issue_key=?1 AND head=?2 AND repo=?3", params![candidate.issue_key,head,candidate.repo], |r| r.get(0)).optional()? {
        return Ok(id);
    }
    let s = sample_in(&tx,candidate,percent)?;
    tx.execute("INSERT INTO verification_audit(sample_id,project,issue_key,base,head,author,declarer,declaration_source,declared,effective,selected,landed_at,state,repo) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", params![s.id,candidate.project,candidate.issue_key,base,head,candidate.author,candidate.declarer,candidate.declaration_source,candidate.declared,candidate.effective,s.selected,now(),if s.selected {"pending"} else {"not_sampled"},candidate.repo])?;
    let id=tx.last_insert_rowid();
    for rule in &candidate.rules { tx.execute("INSERT OR IGNORE INTO verification_audit_rule(audit_id,rule) VALUES(?1,?2)",params![id,rule])?; }
    tx.commit()?;
    Ok(id)
}

fn job(conn: &rusqlite::Connection, id:i64) -> Result<AuditJob> {
    let (mut j,json): (AuditJob,Option<String>) = conn.query_row("SELECT id,project,issue_key,base,head,author,selected,state,attempt,lease_until,result,error,repo FROM verification_audit WHERE id=?1",[id],|r| Ok((AuditJob{rules:vec![],repo:r.get(12)?,id:r.get(0)?,project:r.get(1)?,issue_key:r.get(2)?,base:r.get(3)?,head:r.get(4)?,author:r.get(5)?,selected:r.get(6)?,state:r.get(7)?,attempt:r.get(8)?,lease_until:r.get(9)?,result:None,error:r.get(11)?},r.get(10)?)))?;
    j.result=json.map(|s|serde_json::from_str(&s)).transpose()?;
    j.rules=conn.prepare("SELECT rule FROM verification_audit_rule WHERE audit_id=?1 ORDER BY rule")?
        .query_map([id],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(j)
}

pub fn claim_next(db: &mut Db, project: &str, repo: &str) -> Result<Option<AuditJob>> {
    let tx=db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id:Option<i64>=tx.query_row("SELECT id FROM verification_audit WHERE project=?1 AND repo=?3 AND (state='pending' OR (state='running' AND lease_until<?2)) ORDER BY id LIMIT 1",params![project,now(),repo],|r|r.get(0)).optional()?;
    let Some(id)=id else { return Ok(None); };
    tx.execute("UPDATE verification_audit SET state='running',attempt=attempt+1,lease_until=?2,error=NULL WHERE id=?1",params![id,now()+LEASE_SECONDS])?;
    let j=job(&tx,id)?;
    tx.commit()?;
    Ok(Some(j))
}

/// Fenced completion: a killed/slow old worker cannot replace its successor's result.
pub fn finish(db: &Db, claimed: &AuditJob, outcome: &Result<Review>) -> Result<()> {
    let (state,result,error)=match outcome {
        Ok(r) => {
            if !matches!(r.verdict.as_str(),"pass"|"changes") || r.reused.is_some() { bail!("audit must be a completed fresh full review"); }
            (if r.findings.is_empty() && r.verdict=="pass" {"passed"} else {"findings"},Some(serde_json::to_string(r)?),None)
        }
        Err(e)=>("failed",None,Some(format!("{e:#}"))),
    };
    let changed=db.conn.execute("UPDATE verification_audit SET state=?3,result=?4,error=?5,completed_at=?6,lease_until=NULL WHERE id=?1 AND attempt=?2 AND state='running'",params![claimed.id,claimed.attempt,state,result,error,now()])?;
    if changed!=1 { bail!("audit lease was replaced; ignoring stale completion"); }
    Ok(())
}

/// Retry an infrastructure failure explicitly; no busy-loop retries or fictional passes.
pub fn retry(db: &Db, id:i64) -> Result<()> {
    if db.conn.execute("UPDATE verification_audit SET state='pending',completed_at=NULL WHERE id=?1 AND state='failed'",[id])?!=1 { bail!("only a failed audit can be retried"); }
    Ok(())
}

/// Executable queue worker. The callback prepares an isolated checkout and calls full_review.
/// No database transaction stays open while a reviewer process runs.
pub fn run_next(db:&mut Db, project:&str, repo:&str, execute:impl FnOnce(&AuditJob)->Result<Review>) -> Result<Option<AuditJob>> {
    let Some(claimed)=claim_next(db,project,repo)? else { return Ok(None); };
    let outcome=execute(&claimed);
    finish(db,&claimed,&outcome)?;
    Ok(Some(job(&db.conn,claimed.id)?))
}

/// Use a dedicated clean checkout at the landed commit, never a moving integration checkout.
/// Existing review launch checks and policy settings still apply; unavailable review is failure.
pub fn full_review(job:&AuditJob, checkout:&Path, rules:&ReviewRules) -> Result<Review> {
    full_review_checked(job,checkout,rules,&mut ||rules.require_enabled())
}
pub(crate) fn full_review_checked(job:&AuditJob,checkout:&Path,rules:&ReviewRules,authority:&mut dyn FnMut()->Result<()>)->Result<Review> {
    use crate::playbook::git;
    if repository_id(checkout)?!=job.repo { bail!("audit checkout belongs to another repository"); }
    let check=||->Result<()> {
        if git(checkout,&["rev-parse","HEAD"])?.trim()!=job.head || !git(checkout,&["status","--porcelain"])?.trim().is_empty() { bail!("audit checkout must be clean at the exact landed head"); }
        Ok(())
    };
    check()?;
    git(checkout,&["merge-base","--is-ancestor",&job.base,&job.head])?;
    let prefix=job.author.split(':').next().unwrap_or(&job.author);
    let vendor=if matches!(prefix,"codex"|"claude"|"cursor") {prefix} else {&job.author};
    let choice=rules.choose(vendor,&|r|rules.is_installed(r,checkout))?;
    if !choice.independent { bail!("audit needs an independent peer reviewer"); }
    let mut rules=rules.clone();
    rules.context=format!("Post-landing full audit of {}. Review the entire {}..{} diff; no earlier review or reduced checks replace this audit.\n{}",job.issue_key,job.base,job.head,rules.context);
    let result=rules.run_checked(&choice.reviewer,checkout,&job.base,None,&mut ||{authority()?;check()})?;
    authority()?;
    check()?;
    Ok(result)
}

/// Review findings are allegations until a human confirms they escaped reduced verification.
pub fn confirm_escape(db:&mut Db, audit_id:i64, rule:&str, evidence:&str, actor:&Actor) -> Result<()> {
    if !matches!(actor,Actor::Human) { bail!("only a human confirms an audit finding as an escape"); }
    escape(db,audit_id,rule,"audit",evidence,actor.id())
}

/// Human desktop decisions return a fresh bounded report for immediate UI feedback.
pub fn confirm_report(db:&mut Db,id:i64,rule:&str,evidence:&str,actor:&Actor)->Result<Report> {
    let audit=job(&db.conn,id)?;
    confirm_escape(db,id,rule,evidence,actor)?;
    db.add_note(&audit.issue_key,&format!("Confirmed post-merge escape at {} for rule {rule}: {evidence}",audit.head),"verification_escape_confirmed",actor)?;
    report(db,&audit.project,100)
}
pub fn retry_report(db:&Db,id:i64,actor:&Actor)->Result<Report> {
    if !matches!(actor,Actor::Human) {bail!("only the human retries audits through the desktop decision");}
    let audit=job(&db.conn,id)?;
    retry(db,id)?;
    db.add_note(&audit.issue_key,"Retrying the failed post-merge audit; no verification result is assumed","verification_audit_retry",actor)?;
    report(db,&audit.project,100)
}

/// Internal backstop hook, only after reproducible failure + isolated culprit attribution.
pub(crate) fn confirmed_culprit(db:&mut Db, audit_id:i64, rule:&str, evidence:&str) -> Result<()> {
    escape(db,audit_id,rule,"post_merge",evidence,"verification-backstop")
}

/// Backstop convenience: an isolated commit can carry several issues/rules. Attribute
/// only to recorded reduced landings, never to whichever issue happens to be latest.
pub(crate) fn confirmed_landing_culprit(db:&mut Db,project:&str,repo:&str,head:&str,evidence:&str)->Result<usize> {
    let affected:Vec<(i64,String)>={
        let mut stmt=db.conn.prepare("SELECT a.id,r.rule FROM verification_audit a JOIN verification_audit_rule r ON r.audit_id=a.id WHERE a.project=?1 AND a.head=?2 AND a.repo=?3")?;
        let rows=stmt.query_map(params![project,head,repo],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (id,rule) in &affected { confirmed_culprit(db,*id,rule,evidence)?; }
    Ok(affected.len())
}

fn escape(db:&mut Db,id:i64,rule:&str,source:&str,evidence:&str,actor:&str)->Result<()> {
    if evidence.trim().is_empty() || evidence.len()>16_384 { bail!("escape needs bounded evidence of the confirmed defect"); }
    let tx=db.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let project:String=tx.query_row("SELECT a.project FROM verification_audit a JOIN verification_audit_rule r ON r.audit_id=a.id WHERE a.id=?1 AND r.rule=?2",params![id,rule],|r|r.get(0)).context("escape rule must have lowered this landed change")?;
    if source=="audit" {
        let state:String=tx.query_row("SELECT state FROM verification_audit WHERE id=?1",[id],|r|r.get(0))?;
        if state!="findings" { bail!("audit confirmation requires a completed review with findings"); }
    }
    tx.execute("INSERT OR IGNORE INTO verification_escape(audit_id,rule,source,evidence,confirmed_by,confirmed_at) VALUES(?1,?2,?3,?4,?5,?6)",params![id,rule,source,evidence,actor,now()])?;
    let escape_id:i64=tx.query_row("SELECT id FROM verification_escape WHERE audit_id=?1 AND rule=?2 AND source=?3 AND evidence=?4",params![id,rule,source,evidence],|r|r.get(0))?;
    tx.execute("INSERT OR IGNORE INTO verification_disabled_rule(project,rule,escape_id,disabled_at) VALUES(?1,?2,?3,?4)",params![project,rule,escape_id,now()])?;
    tx.commit()?;
    Ok(())
}

/// Enforcement reads are deliberately untruncated: a display cap must not hide a disabled rule.
pub fn disabled_rules(db:&Db,project:&str)->Result<Vec<String>> {
    let mut stmt=db.conn.prepare("SELECT rule FROM verification_disabled_rule WHERE project=?1 ORDER BY rule")?;
    let rows=stmt.query_map([project],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn report(db:&Db,project:&str,limit:usize)->Result<Report> {
    let limit=limit.clamp(1,MAX_ROWS);
    let mut stmt=db.conn.prepare("SELECT id FROM verification_audit WHERE project=?1 ORDER BY id DESC LIMIT ?2")?;
    let ids:Vec<i64>=stmt.query_map(params![project,(limit+1) as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let jobs_truncated=ids.len()>limit;
    let jobs=ids.into_iter().take(limit).map(|id|job(&db.conn,id)).collect::<Result<_>>()?;
    let rates=|by_rule:bool|->Result<Vec<Rate>> {
        let (name,join)=if by_rule {("r.rule","JOIN verification_audit_rule r ON r.audit_id=a.id")} else {("coalesce(a.declarer,'undeclared') || ' / ' || coalesce(a.declaration_source,'unknown')","")};
        let sql=format!("SELECT {name},count(*),sum(a.state IN ('passed','findings')),sum(EXISTS(SELECT 1 FROM verification_escape e WHERE e.audit_id=a.id {})),sum(a.declared IS NOT NULL),sum(a.declared=a.effective),sum(a.state IN ('passed','findings') AND EXISTS(SELECT 1 FROM verification_escape e WHERE e.audit_id=a.id {})) FROM verification_audit a {join} WHERE a.project=?1 GROUP BY {name} ORDER BY {name} LIMIT ?2",if by_rule {"AND e.rule=r.rule"} else {""},if by_rule {"AND e.rule=r.rule"} else {""});
        let mut stmt=db.conn.prepare(&sql)?;
        let rows=stmt.query_map(params![project,(limit+1) as i64],|r| {let audited:i64=r.get(2)?;let audited_escapes:i64=r.get(6)?; Ok(Rate{name:r.get(0)?,landed:r.get(1)?,audited,confirmed_escape_lands:r.get(3)?,declared_lands:r.get(4)?,matching_declarations:r.get::<_,Option<i64>>(5)?.unwrap_or(0),audited_escape_rate:(audited>0).then_some(audited_escapes as f64/audited as f64)})})?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    };
    let mut rules=rates(true)?; let rules_truncated=rules.len()>limit; rules.truncate(limit);
    let mut declarers=rates(false)?; let declarers_truncated=declarers.len()>limit; declarers.truncate(limit);
    let mut stmt=db.conn.prepare("SELECT rule FROM verification_disabled_rule WHERE project=?1 ORDER BY rule LIMIT ?2")?;
    let mut disabled_rules:Vec<String>=stmt.query_map(params![project,(limit+1) as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let disabled_rules_truncated=disabled_rules.len()>limit; disabled_rules.truncate(limit);
    let mut stmt=db.conn.prepare("SELECT e.id,e.audit_id,a.issue_key,a.head,e.rule,e.source,e.evidence,e.confirmed_by,e.confirmed_at FROM verification_escape e JOIN verification_audit a ON a.id=e.audit_id WHERE a.project=?1 ORDER BY e.id DESC LIMIT ?2")?;
    let mut escapes:Vec<Escape>=stmt.query_map(params![project,(limit+1) as i64],|r|Ok(Escape{id:r.get(0)?,audit_id:r.get(1)?,issue_key:r.get(2)?,head:r.get(3)?,rule:r.get(4)?,source:r.get(5)?,evidence:r.get(6)?,confirmed_by:r.get(7)?,confirmed_at:r.get(8)?}))?.collect::<rusqlite::Result<_>>()?;
    let escapes_truncated=escapes.len()>limit;escapes.truncate(limit);
    Ok(Report{jobs,rules,declarers,disabled_rules,escapes,jobs_truncated,rules_truncated,declarers_truncated,disabled_rules_truncated,escapes_truncated,note:"Rates describe observed landed changes, not proof of safety. Findings await confirmation; declaration matches compare declared and mechanically effective levels. Missing or failed audits are not passes.".into()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewIssue;

    fn setup() -> (Db,Candidate) {
        let path=crate::paths::unique_temp("nitely-audit-test", ".sqlite");
        let mut db=Db::open(&path).unwrap();
        db.create_project("APP","Example").unwrap();
        let issue=db.create_issue(&NewIssue{project:"APP".into(),title:"Clarify help".into(),body:"Improve explanatory text".into(),kind:"feature".into(),source:"codex".into(),evidence:"test".into(),..Default::default()},&Actor::Human).unwrap();
        let c=Candidate{repo:"repo-one".into(),project:"APP".into(),issue_key:issue.key,policy:policy_fingerprint(b"policy", "classifier-v1"),author:"codex".into(),declarer:Some("codex:writer".into()),declaration_source:Some("implementer".into()),declared:Some("inert".into()),effective:"inert".into(),rules:vec!["prose".into()]};
        (db,c)
    }
    fn review(findings:bool)->Review {
        Review{reviewer:"claude".into(),verdict:if findings {"changes"} else {"pass"}.into(),findings:if findings {vec![crate::review::Finding{priority:1,text:"Executable documentation changed".into(),carried:false,raised_as:None,now:None}]} else {vec![]},flagged:vec![],summary:"Full landed diff inspected".into(),seconds:1,previous:vec![],note:String::new(),batch:None,tokens:None,reused:None}
    }
    fn land(db:&mut Db,c:&Candidate,n:u8)->i64 {landed(db,c,&"a".repeat(40),&format!("{n:040x}"),Some(100)).unwrap()}

    #[test]
    fn sample_survives_revisions_declarations_restart_and_rekey() {
        let (mut db,mut c)=setup();
        let first=record_candidate(&mut db,&c,None).unwrap();
        let salt:Vec<u8>=db.conn.query_row("SELECT salt FROM verification_audit_salt",[],|r|r.get(0)).unwrap();
        assert_eq!(salt.len(),32);
        c.declared=Some("contained".into()); c.effective="contained".into(); c.rules=vec!["component-a".into()];
        assert_eq!(record_candidate(&mut db,&c,Some(100)).unwrap(),first,"changing declaration or sample request cannot reroll this policy");
        let path=db.conn.path().unwrap().to_owned(); drop(db); let mut db=Db::open(Path::new(&path)).unwrap();
        assert_eq!(record_candidate(&mut db,&c,None).unwrap(),first);
        db.rekey_project("APP","NEW").unwrap(); c.project="NEW".into();c.issue_key="NEW-1".into();
        assert_eq!(record_candidate(&mut db,&c,None).unwrap(),first);
        c.policy=policy_fingerprint(b"revised policy","classifier-v1");
        assert_ne!(record_candidate(&mut db,&c,None).unwrap().id,first.id);
        assert!(record_candidate(&mut db,&c,Some(9)).is_err());
    }

    #[test]
    fn concurrent_first_selection_and_claim_are_single_owner() {
        let (mut db,c)=setup();
        let path=db.conn.path().unwrap().to_owned();
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
        let other_c=c.clone();let other_barrier=barrier.clone();
        let thread=std::thread::spawn(move || {let mut db=Db::open(Path::new(&path)).unwrap();other_barrier.wait();record_candidate(&mut db,&other_c,Some(100)).unwrap()});
        barrier.wait();let sample=record_candidate(&mut db,&c,Some(100)).unwrap();
        assert_eq!(sample,thread.join().unwrap());
        land(&mut db,&c,1);
        let mut other=Db::open(Path::new(db.conn.path().unwrap())).unwrap();
        assert!(claim_next(&mut db,"APP",&c.repo).unwrap().is_some());
        assert!(claim_next(&mut other,"APP",&c.repo).unwrap().is_none());
    }

    #[test]
    fn workers_cannot_claim_another_repository_in_the_same_project() {
        let (mut db,mut c)=setup();let first=land(&mut db,&c,1);
        c.repo="repo-two".into();let second=land(&mut db,&c,1);
        assert_ne!(first,second,"shared commit history does not merge repository jobs");
        assert_eq!(claim_next(&mut db,"APP","repo-two").unwrap().unwrap().id,second);
        assert_eq!(claim_next(&mut db,"APP","repo-one").unwrap().unwrap().id,first);
        assert!(claim_next(&mut db,"APP","unknown-repo").unwrap().is_none());
    }

    #[test]
    fn expired_worker_recovery_fences_completion_and_failed_jobs_retry_explicitly() {
        let (mut db,c)=setup();let id=land(&mut db,&c,1);
        assert_eq!(id,land(&mut db,&c,1));
        let old=claim_next(&mut db,"APP",&c.repo).unwrap().unwrap();
        db.conn.execute("UPDATE verification_audit SET lease_until=0 WHERE id=?1",[id]).unwrap();
        let mut other=Db::open(Path::new(db.conn.path().unwrap())).unwrap();
        let current=claim_next(&mut other,"APP",&c.repo).unwrap().unwrap();
        assert_eq!(current.attempt,old.attempt+1);
        assert!(finish(&db,&old,&Ok(review(false))).is_err());
        finish(&other,&current,&Err(anyhow::anyhow!("reviewer unavailable"))).unwrap();
        assert!(claim_next(&mut db,"APP",&c.repo).unwrap().is_none());
        assert!(retry_report(&db,id,&Actor::Agent("human".into())).is_err());
        let pending=retry_report(&db,id,&Actor::Human).unwrap();
        assert_eq!(pending.jobs[0].state,"pending");
        let done=run_next(&mut db,"APP",&c.repo,|j| {assert_eq!(j.head,format!("{:040x}",1));Ok(review(false))}).unwrap().unwrap();
        assert_eq!(done.state,"passed");
        assert!(retry(&db,id).is_err());
    }

    #[test]
    fn findings_need_human_confirmation_and_disable_only_attributed_rules() {
        let (mut db,c)=setup();let id=land(&mut db,&c,1);
        run_next(&mut db,"APP",&c.repo,|_|Ok(review(true))).unwrap();
        assert!(disabled_rules(&db,"APP").unwrap().is_empty());
        assert_eq!(report(&db,"APP",10).unwrap().jobs[0].rules,vec!["prose"]);
        assert!(confirm_report(&mut db,id,"prose","reproduced",&Actor::Agent("human".into())).is_err());
        assert!(confirm_escape(&mut db,id,"unrelated","reproduced",&Actor::Human).is_err());
        let confirmed=confirm_report(&mut db,id,"prose","reproduced executable example",&Actor::Human).unwrap();
        assert_eq!(confirmed.disabled_rules,vec!["prose"]);
        confirm_escape(&mut db,id,"prose","reproduced executable example",&Actor::Human).unwrap();
        assert_eq!(disabled_rules(&db,"APP").unwrap(),vec!["prose"]);
        let r=report(&db,"APP",10).unwrap();
        assert_eq!(r.rules[0].confirmed_escape_lands,1);
        assert_eq!(r.rules[0].audited_escape_rate,Some(1.0));
        assert_eq!(r.declarers[0].matching_declarations,1);
    }

    #[test]
    fn unsampled_culprits_count_without_inventing_audit_passes_and_reports_are_bounded() {
        let (mut db,c)=setup();
        record_candidate(&mut db,&c,None).unwrap();
        // Deterministic fixture for the non-selected side of the random decision.
        db.conn.execute("UPDATE verification_sample SET selected=0",[]).unwrap();
        let id=land(&mut db,&c,1);
        assert!(claim_next(&mut db,"APP",&c.repo).unwrap().is_none());
        confirmed_culprit(&mut db,id,"prose","bisect isolated exact landed diff; failing check reproduces").unwrap();
        let r=report(&db,"APP",10).unwrap();
        assert_eq!(r.rules[0].landed,1);assert_eq!(r.rules[0].audited,0);
        assert_eq!(r.rules[0].confirmed_escape_lands,1);assert_eq!(r.rules[0].audited_escape_rate,None);
        land(&mut db,&c,2);
        let r=report(&db,"APP",1).unwrap();assert_eq!(r.jobs.len(),1);assert!(r.jobs_truncated);
        db.rekey_project("APP","NEW").unwrap();
        assert_eq!(disabled_rules(&db,"NEW").unwrap(),vec!["prose"]);
        db.remove_project("NEW",true).unwrap();
        for table in ["verification_audit","verification_sample","verification_audit_salt","verification_audit_rule","verification_escape","verification_disabled_rule"] {
            assert_eq!(db.conn.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,i64>(0)).unwrap(),0, "{table}");
        }
    }

    #[test]
    fn audit_requires_immutable_clean_checkout_and_fresh_result() {
        use crate::test_repo::git;
        let repo=crate::test_repo::copy("audit-exact-head",|p|crate::test_repo::init(p,&[("help.txt","old")]));
        let base=git(&repo,&["rev-parse","HEAD"]);
        std::fs::write(repo.join("help.txt"),"new").unwrap();git(&repo,&["commit","-am","clarify"]);
        let head=git(&repo,&["rev-parse","HEAD"]);
        let (mut db,mut c)=setup();c.repo=repository_id(&repo).unwrap();c.author="codex:session".into();landed(&mut db,&c,&base,&head,Some(100)).unwrap();
        let j=claim_next(&mut db,"APP",&c.repo).unwrap().unwrap();
        git(&repo,&["checkout","--detach",&base]);
        assert!(full_review(&j,&repo,&ReviewRules::default()).unwrap_err().to_string().contains("exact landed head"));
        git(&repo,&["checkout","--detach",&head]);std::fs::write(repo.join("help.txt"),"uncommitted").unwrap();
        assert!(full_review(&j,&repo,&ReviewRules::default()).unwrap_err().to_string().contains("clean"));
        let mut reused=review(false);reused.reused=Some(base);
        assert!(finish(&db,&j,&Ok(reused)).is_err());
    }

    #[test]
    fn executable_worker_reviews_entire_landed_diff_with_independent_command() {
        use crate::test_repo::git;
        let repo=crate::test_repo::copy("audit-executable",|p|crate::test_repo::init(p,&[("help.txt","old")]));
        let base=git(&repo,&["rev-parse","HEAD"]);
        std::fs::write(repo.join("help.txt"),"new").unwrap();git(&repo,&["commit","-am","clarify"]);
        let head=git(&repo,&["rev-parse","HEAD"]);
        let (mut db,mut c)=setup();c.repo=repository_id(&repo).unwrap();c.author="codex:session".into();landed(&mut db,&c,&base,&head,Some(100)).unwrap();
        let mut rules=ReviewRules::default();
        rules.reviewer.insert("codex".into(),"fake".into());rules.commands.clear();
        rules.commands.insert("fake".into(),vec![crate::playbook::posix_sh(),"-c".into(),"prompt=$(cat); case \"$prompt\" in *'Post-landing full audit'*'Review the entire'*) printf 'Complete diff inspected.\\nVERDICT: clean\\n' ;; *) exit 4 ;; esac".into()]);
        let done=run_next(&mut db,"APP",&c.repo,|j|full_review(j,&repo,&rules)).unwrap().unwrap();
        assert_eq!(done.state,"passed","{:?}",done.error);
        assert_eq!(done.result.as_ref().unwrap().reviewer,"fake");
        rules.enabled=false;
        assert!(full_review(&done,&repo,&rules).is_err(),"legacy PR switch remains enforced for legacy audit callers");
        let independent=full_review_checked(&done,&repo,&rules,&mut ||Ok(())).unwrap();
        assert_eq!(independent.verdict,"pass","an independently enabled audit does not inherit the pre-merge review switch");
        assert!(full_review_checked(&done,&repo,&rules,&mut ||bail!("audit settings changed")).unwrap_err().to_string().contains("audit settings changed"));
    }
}
