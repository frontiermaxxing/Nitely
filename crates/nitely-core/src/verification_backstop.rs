//! Durable post-merge checks. Nothing enables reduced verification by itself.
//! Commands and recovery permissions come from the committed integration policy.
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use crate::{Db, Config, Actor, NewIssue, db::now, playbook::{self, git, Playbook}};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    pub enabled: bool,
    pub landings: u32,
    pub delay_seconds: i64,
    pub nightly_seconds: i64,
    pub auto_revert: bool,
    /// The paths an automatic revert may touch; unset, every path (NIT-496), so auto-revert is on
    /// unless the playbook narrows it. The built-in list (migrations, schemas, deploys, releases,
    /// installs, credentials, secrets, the playbook, `.nitely/`, `.github/`) and
    /// `irreversible_paths` always pause instead. `[]` reverts nothing.
    pub reversible_paths: Option<Vec<String>>,
    pub irreversible_paths: Vec<String>,
}
impl Default for Rules {
    fn default() -> Self { Self { enabled: false, landings: 5, delay_seconds: 1800, nightly_seconds: 86400, auto_revert: true, reversible_paths: None, irreversible_paths: vec![] } }
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub repo: String, pub project: String, pub observed_head: String,
    pub verified_head: Option<String>, pub pending_since: i64, pub reduced_count: u32,
    pub checked_at: i64, pub status: String, pub message: String, pub recovery_issue: Option<String>,
}
const COLUMNS: &str = "repo,project,observed_head,verified_head,pending_since,reduced_count,checked_at,status,message,recovery_issue";
fn row(r: &rusqlite::Row) -> rusqlite::Result<Health> {
    Ok(Health { repo:r.get(0)?,project:r.get(1)?,observed_head:r.get(2)?,verified_head:r.get(3)?,pending_since:r.get(4)?,reduced_count:r.get(5)?,checked_at:r.get(6)?,status:r.get(7)?,message:r.get(8)?,recovery_issue:r.get(9)? })
}
fn repo_id(repo: &Path) -> Result<String> {
    crate::verification_audit::repository_id(repo)
}
pub fn health_all(db: &Db) -> Result<Vec<Health>> {
    Ok(db.conn.prepare(&format!("SELECT {COLUMNS} FROM verification_health ORDER BY project,repo"))?.query_map([],row)?.collect::<rusqlite::Result<_>>()?)
}
pub fn health(db: &Db, repo: &Path) -> Result<Option<Health>> {
    Ok(db.conn.query_row(&format!("SELECT {COLUMNS} FROM verification_health WHERE repo=?1"),[repo_id(repo)?],row).optional()?)
}
pub fn project_health(db: &Db, project: &str) -> Result<Vec<Health>> {
    Ok(db.conn.prepare(&format!("SELECT {COLUMNS} FROM verification_health WHERE project=?1 ORDER BY repo"))?.query_map([project],row)?.collect::<rusqlite::Result<_>>()?)
}

/// Absence is not a Full certificate: release verification must independently run its gate.
pub fn release_ready(db: &Db, repo: &Path, _head: &str) -> Result<()> {
    if let Some(h)=health(db,repo)? {
        let branch=Playbook::load(repo)?.integration_branch().to_string();
        let current=git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?;
        optional_policy(db,repo,current.trim())?.context("recorded backstop no longer has its committed policy")?;
        if current.trim()!=h.observed_head || h.status != "green" || h.verified_head.as_deref()!=Some(&h.observed_head) {
            bail!("integration backstop is {}: {}; resolve the unverified range before release",h.status,h.message);
        }
    }
    Ok(())
}
pub fn reduced_ready(db: &Db, repo: &Path, rules: &Rules, at: i64) -> Result<()> {
    if !rules.enabled { bail!("the post-merge backstop is not enabled"); }
    reconcile(db,repo)?;
    let h=health(db,repo)?.context("the backstop has not established a green baseline")?;
    let branch=Playbook::load(repo)?.integration_branch().to_string();
    let current=git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?;
    if current.trim()!=h.observed_head { bail!("reduced verification paused: integration moved before its landing was accounted for"); }
    if let Some(verified)=&h.verified_head {
        if git(repo,&["merge-base","--is-ancestor",verified,current.trim()]).is_err() { bail!("reduced verification paused: integration no longer descends from the verified baseline"); }
    }
    if h.verified_head.is_none() || matches!(h.status.as_str(),"failed"|"paused"|"recovering") || due(&h,rules,at) {
        bail!("reduced verification paused: backstop {} or due",h.status);
    }
    Ok(())
}
fn due(h:&Health,r:&Rules,at:i64)->bool {
    if h.observed_head==h.verified_head.as_deref().unwrap_or("") { return false; }
    h.verified_head.is_none() || h.reduced_count>=r.landings.max(1)
        || (h.reduced_count>0 && at.saturating_sub(h.pending_since)>=r.delay_seconds.clamp(1,86400))
        || at.saturating_sub(h.checked_at)>=r.nightly_seconds.clamp(1,86400)
}

/// Record the exact landed range once. Full gates do not increment the reduced counter.
pub fn record_landing(db:&Db,repo:&Path,project:&str,issue:&str,base:&str,head:&str,reduced:bool)->Result<()> {
    let id=repo_id(repo)?;
    let head=git(repo,&["rev-parse",&format!("{head}^{{commit}}")])?.trim().to_string();
    let base=git(repo,&["rev-parse",&format!("{base}^{{commit}}")])?.trim().to_string();
    db.conn.execute("INSERT OR IGNORE INTO verification_landing(repo,head,base,issue_key,reduced,landed_at) VALUES(?1,?2,?3,?4,?5,?6)",params![id,head,base,issue,reduced,now()])?;
    // Reconcile from durable land rows, so a crash between inserts cannot lose a reduced landing.
    let pb=Playbook::load(repo)?;
    let current=git(repo,&["rev-parse",pb.integration_branch()])?;
    observe(db,repo,project,current.trim())?;
    reconcile(db,repo)?;
    Ok(())
}
fn reconcile(db:&Db,repo:&Path)->Result<()> {
    let id=repo_id(repo)?;
    if let Some(verified)=health(db,repo)?.and_then(|h|h.verified_head) {
        let rows=db.conn.prepare("SELECT DISTINCT head FROM verification_landing WHERE repo=?1 AND covered=0")?.query_map([&id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for head in rows {
            if git(repo,&["merge-base","--is-ancestor",&head,&verified]).is_ok() {
                db.conn.execute("UPDATE verification_landing SET covered=1 WHERE repo=?1 AND head=?2",params![id,head])?;
            }
        }
    }
    db.conn.execute("UPDATE verification_health SET reduced_count=(SELECT COUNT(*) FROM verification_landing l WHERE l.repo=?1 AND l.reduced=1 AND l.covered=0),pending_since=min(pending_since,coalesce((SELECT min(landed_at) FROM verification_landing l WHERE l.repo=?1 AND l.reduced=1 AND l.covered=0),pending_since)) WHERE repo=?1",[id])?;
    Ok(())
}
fn observe(db:&Db,repo:&Path,project:&str,head:&str)->Result<()> {
    let id=repo_id(repo)?;
    db.conn.execute("INSERT OR IGNORE INTO verification_health(repo,project,observed_head,pending_since) VALUES(?1,?2,?3,?4)",params![id,project,head,now()])?;
    // A landing can race a checker. Never replace a newer observation with its ancestor.
    // CAS retries close the read/check/write window without holding SQLite across Git.
    for _ in 0..8 {
        let old=health(db,repo)?.context("branch health disappeared")?;
        let behind=|| git(repo,&["merge-base","--is-ancestor",head,&old.observed_head]).is_ok();
        let branch_at_head=|| -> Result<bool> {
            let branch=Playbook::load(repo)?.integration_branch().to_string();
            Ok(git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?.trim()==head)
        };
        let rewritten=match observation(&old.observed_head,head,behind,branch_at_head)? {
            Observation::Keep => return Ok(()),
            Observation::Record => git(repo,&["merge-base","--is-ancestor",&old.observed_head,head]).is_err(),
        };
        let changed=db.conn.execute("UPDATE verification_health SET pending_since=CASE WHEN verified_head=observed_head THEN ?4 ELSE pending_since END,observed_head=?2,status=CASE WHEN ?5 THEN 'paused' WHEN status='green' THEN 'pending' ELSE status END,message=CASE WHEN ?5 THEN 'Integration history changed; a new Full baseline is required' ELSE message END WHERE repo=?1 AND observed_head=?3",params![id,head,old.observed_head,now(),rewritten])?;
        if changed==1 { return Ok(()); }
    }
    bail!("integration observations kept changing; retry verification")
}
/// What an observation of the integration branch at `head` does to the recorded one.
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(crate) enum Observation { Keep, Record }

/// Never replace a newer observation with its ancestor (`head_is_behind`), unless the branch
/// really was reset there (`branch_at_head`: an authoritative backward move). Each fact is read
/// only when the one before leaves it open (NIT-539).
pub(crate) fn observation(old:&str,head:&str,head_is_behind:impl FnOnce()->bool,branch_at_head:impl FnOnce()->Result<bool>)->Result<Observation> {
    if old==head { return Ok(Observation::Keep); }
    if head_is_behind() && !branch_at_head()? { return Ok(Observation::Keep); }
    Ok(Observation::Record)
}
fn committed_policy(repo:&Path,head:&str)->Result<(Playbook,String)> {
    let object=format!("{head}:{}",playbook::PLAYBOOK_PATH);
    let hash=git(repo,&["rev-parse",&object])?.trim().to_string();
    let pb=Playbook::parse(&git(repo,&["show",&object])?)?;
    if pb.unknown.iter().any(|s|s.starts_with("verification")) { bail!("unknown verification policy fields"); }
    Ok((pb,hash))
}
/// Unconfigured projects are quiet. Removing policy after opting in does not erase
/// pending evidence or turn previously blocked health into permission to proceed.
fn optional_policy(db:&Db,repo:&Path,head:&str)->Result<Option<(Playbook,String)>> {
    let id=repo_id(repo)?;
    let contents=git(repo,&["show",&format!("{head}:{}",playbook::PLAYBOOK_PATH)]).ok();
    if let Some(contents)=&contents {
        let document:toml::Value=toml::from_str(contents)?;
        if document.get("verification").is_some() || health(db,repo)?.is_some_and(|h|db.project_review_mode(&h.project).map(|m|m.explicit).unwrap_or(false)) {return committed_policy(repo,head).map(Some);}
    }
    let jobs:bool=db.conn.query_row("SELECT EXISTS(SELECT 1 FROM verification_audit WHERE repo=?1)",[&id],|r|r.get(0))?;
    if health(db,repo)?.is_some() || jobs {
        db.conn.execute("UPDATE verification_health SET status='paused',message='Committed verification policy was removed; restore or explicitly resolve recorded verification' WHERE repo=?1",[id])?;
        bail!("committed verification policy is missing for recorded backstop/audit work");
    }
    Ok(None)
}
fn automatic_policy(db:&Db,repo:&Path,head:&str,project:&str)->Result<Option<(Playbook,String)>> {
    let settings=db.project_review_mode(project)?;
    let chosen=settings.mode!=crate::project_modes::Mode::Custom || settings.overrides.full_backstop.is_some();
    let policy=if chosen {Some(committed_policy(repo,head)?)}else{optional_policy(db,repo,head)?};
    let Some((mut pb,mut identity))=policy else{return Ok(None);};
    crate::project_modes::apply_project(db,project,&mut pb)?;
    if let Some(authority)=settings.authority()? {identity.push_str(&format!(":{authority}"));}
    Ok(Some((pb,identity)))
}
pub(crate) fn on_landed(db:&mut Db,repo:&Path,issue:&crate::issues::IssueRecord,land:&crate::land::Landed)->Result<()> {
    let Some(head)=&land.head else { return Ok(()); };
    let Some((pb,_))=automatic_policy(db,repo,head,issue.project.as_deref().unwrap_or(""))? else {return Ok(());};
    if !pb.verification.backstop.enabled { return Ok(()); }
    let commands:Vec<_>=pb.gate.commands.iter().chain(pb.gate.steps.iter().map(|s|&s.run)).collect();
    let full=!commands.is_empty() && land.gate.as_ref().is_some_and(|g|g.passed && commands.iter().all(|c|g.results.iter().any(|r|r.command==c.as_str() && r.ok && (!r.skipped || r.output.starts_with("skipped: this tree passed it in lane run ")))));
    let base=land.landed_from.as_deref().context("land has no immutable starting revision")?;
    record_landing(db,repo,issue.project.as_deref().unwrap_or(""),&issue.key,base,head,!full)?;
    // An after-land step that failed (an install that cannot build) already says this head is
    // broken: record it, and spend no Full run on it (NIT-457).
    if let Some(step)=land.after.iter().find(|r|!r.ok && !r.skipped && !r.timed_out) {
        known_broken(db,repo,head,&format!("the after-land step `{}` failed",step.command),&step.output)?;
    }
    Ok(())
}

/// Records that integration at `head` is known broken, without a Full run: a failed run row (so
/// the next tick neither runs nor bisects it again) and failed health naming the head. The next
/// commit that lands is checked at once.
pub(crate) fn known_broken(db:&Db,repo:&Path,head:&str,why:&str,output:&str)->Result<()> {
    let id=repo_id(repo)?;
    let head=git(repo,&["rev-parse",&format!("{head}^{{commit}}")])?.trim().to_string();
    let Some(h)=health(db,repo)? else {return Ok(());};
    let already:bool=db.conn.query_row("SELECT EXISTS(SELECT 1 FROM verification_backstop_run WHERE repo=?1 AND head=?2 AND state='failed')",params![id,head],|r|r.get(0))?;
    if already {return Ok(());}
    let tail:String={let lines:Vec<&str>=output.lines().collect();lines[lines.len().saturating_sub(30)..].join("\n")};
    let message=format!("Known broken at {}: {why}. No Full check runs on this head; the next commit that lands is checked at once.\n\nFailing step output (end):\n{tail}\n\n{}",short(&head),range_text(db,repo,h.verified_head.as_deref(),&head));
    db.conn.execute("INSERT INTO verification_backstop_run(repo,head,base,policy,commands,started_at,ended_at,state,evidence,isolate_failure,isolation_complete) VALUES(?1,?2,?3,'known-broken','[]',?4,?4,'failed',?5,0,1)",params![id,head,h.verified_head,now(),message])?;
    db.conn.execute("UPDATE verification_health SET status='failed',message=?2 WHERE repo=?1",params![id,format!("Known broken at {}: {why}",short(&head))])?;
    Ok(())
}

fn short(rev:&str)->&str {&rev[..rev.len().min(12)]}

/// The commits a failure could have come from, for a recovery issue that cannot name one culprit:
/// from the last passing Full check, or, with none, from the earliest landing not yet covered.
fn range_text(db:&Db,repo:&Path,verified:Option<&str>,head:&str)->String {
    let id=repo_id(repo).unwrap_or_default();
    let earliest:Option<String>=db.conn.query_row("SELECT base FROM verification_landing WHERE repo=?1 AND covered=0 ORDER BY landed_at,rowid LIMIT 1",[&id],|r|r.get(0)).optional().ok().flatten();
    let (base,why)=match (verified,earliest) {
        (Some(v),_)=>(v.to_string(),"since the last passing Full check".to_string()),
        (None,Some(b))=>(b,"since the first landing recorded; no Full check has passed yet, so there is no passing base to bisect from".to_string()),
        (None,None)=>return format!("Range: up to {} (no Full check has passed yet and no landing is recorded, so there is no passing base to bisect from).",short(head)),
    };
    let log=git(repo,&["log","--oneline","--first-parent",&format!("{base}..{head}")]).unwrap_or_default();
    let commits:Vec<&str>=log.lines().collect();
    let shown=commits.iter().take(20).map(|c|format!("- {c}")).collect::<Vec<_>>().join("\n");
    let more=if commits.len()>20 {format!("\n- … and {} more",commits.len()-20)} else {String::new()};
    format!("Range: {}..{} ({} commit{}, {why}):\n{shown}{more}",short(&base),short(head),commits.len(),if commits.len()==1 {""} else {"s"})
}

/// One worker per repository, shared by CLI and desktop; dead process locks recover normally.
pub fn tick_all(db:&mut Db,config:&Config)->Result<()> {
    tick_all_inner(db,config,false)
}
/// Explicit Full check now. Overrides cadence, not enablement, resource admission,
/// unresolved product failures, normal recovery gates, or human approvals.
pub fn tick_all_now(db:&mut Db,config:&Config)->Result<()> {
    tick_all_inner(db,config,true)
}
fn tick_all_inner(db:&mut Db,config:&Config,force:bool)->Result<()> {
    let mut errors=Vec::new();
    for repo in &config.repos {
        if let Some(project)=&repo.project {
            if let Err(e)=tick_inner(db,config,Path::new(&repo.root),project,&repo.integration_branch,force) { errors.push(format!("{}: {e:#}",repo.name)); }
            if let Err(e)=run_audit(db,Path::new(&repo.root),project,&repo.integration_branch) { errors.push(format!("{} audit: {e:#}",repo.name)); }
        }
    }
    if !errors.is_empty() { bail!("{}",errors.join("; ")); }
    Ok(())
}
fn run_audit(db:&mut Db,repo:&Path,project:&str,branch:&str)->Result<()> {
    let head=git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?;
    let Some((mut pb,_))=automatic_policy(db,repo,head.trim(),project)? else {return Ok(());};
    if pb.verification.audit_percent==0 {return Ok(());}
    pb.review.policy_root=Some(repo.to_path_buf());
    let settings=db.project_review_mode(project)?;
    let expected=settings.authority()?;
    let connection=rusqlite::Connection::open_with_flags(db.conn.path().context("audit needs a persistent settings database")?,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    crate::verification_audit::run_next(db,project,&repo_id(repo)?,|job|with_checkout(repo,&job.head,|wt|crate::verification_audit::full_review_checked(job,wt,&pb.review,&mut || {
        let current=crate::project_modes::read(&connection,project)?;
        if current.authority()?!=expected || current.options().audit_percent==Some(0) {bail!("project audit settings changed before launch; retry under current policy");}
        if current.options().audit_percent.is_none() {pb.review.require_enabled()?;}
        Ok(())
    })))?;
    Ok(())
}
pub fn tick(db:&mut Db,config:&Config,repo:&Path,project:&str,branch:&str)->Result<()> {
    tick_inner(db,config,repo,project,branch,false)
}
fn tick_inner(db:&mut Db,config:&Config,repo:&Path,project:&str,branch:&str,force:bool)->Result<()> {
    let lock_path=PathBuf::from(repo_id(repo)?).join("nitely-verification.lock");
    let Some(_lock)=crate::land::Lock::try_take(&lock_path,"verification",false)? else { return Ok(()); };
    let head=git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?.trim().to_string();
    let Some((pb,policy))=automatic_policy(db,repo,&head,project)? else {return Ok(());};
    let rules=&pb.verification.backstop;
    if !rules.enabled { return Ok(()); }
    observe(db,repo,project,&head)?;
    reconcile(db,repo)?;
    let h=health(db,repo)?.context("missing branch health")?;
    let last_failed:Option<String>=db.conn.query_row("SELECT head FROM verification_backstop_run WHERE repo=?1 AND state='failed' ORDER BY id DESC LIMIT 1",[repo_id(repo)?],|r|r.get(0)).optional()?;
    if last_failed.as_deref()==Some(&head) && matches!(h.status.as_str(),"failed"|"paused"|"recovering") {
        let (run,mut culprit,evidence,complete,isolate_failure,base):(i64,Option<String>,String,bool,bool,Option<String>)=db.conn.query_row("SELECT id,culprit,evidence,isolation_complete,isolate_failure,base FROM verification_backstop_run WHERE repo=?1 AND head=?2 AND state='failed' ORDER BY id DESC LIMIT 1",params![repo_id(repo)?,head],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        if !force || isolate_failure {
          if h.status=="recovering" || h.recovery_issue.is_none() {
            if !complete {
                let commands:Vec<_>=pb.gate.commands.iter().chain(pb.gate.steps.iter().map(|s|&s.run)).cloned().collect();
                culprit=if isolate_failure {base.as_deref().and_then(|base|isolate(db,repo,&pb,base,&head,&commands,&Actor::Agent("verification-backstop".into())).ok())} else {None};
                db.conn.execute("UPDATE verification_backstop_run SET culprit=?2,isolation_complete=1 WHERE id=?1",params![run,culprit])?;
            }
            recover(db,config,repo,project,&pb,&head,culprit.as_deref(),&evidence,&Actor::Agent("verification-backstop".into()))?;
          }
          return Ok(());
        }
    }
    // A commit landed on a head known to fail (its Full check, or an after-land build): check it
    // at once, whatever the cadence, so the fix clears the pause as soon as it lands (NIT-457).
    let past_failure=last_failed.as_deref().is_some_and(|f|f!=head) && matches!(h.status.as_str(),"failed"|"paused"|"recovering");
    if !force && !past_failure && !due(&h,rules,now()) { return Ok(()); }
    let commands:Vec<_>=pb.gate.commands.iter().chain(pb.gate.steps.iter().map(|s|&s.run)).cloned().collect();
    if commands.is_empty() { bail!("the post-merge Full gate is empty"); }
    let id=repo_id(repo)?;
    db.conn.execute("UPDATE verification_backstop_run SET state='interrupted',ended_at=?2 WHERE repo=?1 AND state='running'",params![id,now()])?;
    db.conn.execute("INSERT INTO verification_backstop_run(repo,head,base,policy,commands,started_at,state) VALUES(?1,?2,?3,?4,?5,?6,'running')",params![id,head,h.verified_head,policy,serde_json::to_string(&commands)?,now()])?;
    let run=db.conn.last_insert_rowid();
    db.conn.execute("UPDATE verification_health SET status='running',message='Full check queued in CI or a build lane',recovery_issue=NULL WHERE repo=?1",[&id])?;
    let actor=Actor::Agent("verification-backstop".into());
    let result=execute(db,repo,&pb,&head,&commands,&actor);
    match result {
        Ok(gate) if gate.passed && gate.flaked.is_empty() => {
            let current=git(repo,&["rev-parse",&format!("refs/heads/{branch}")])?;
            observe(db,repo,project,current.trim())?;
            db.conn.execute("UPDATE verification_backstop_run SET state='passed',ended_at=?2,lane_run=?3,evidence=?4 WHERE id=?1",params![run,now(),gate.run,serde_json::to_string(&gate.results)?])?;
            db.conn.execute("UPDATE verification_health SET verified_head=?2,checked_at=?3,status=CASE WHEN observed_head=?2 THEN 'green' ELSE 'pending' END,message='Full gate passed',recovery_issue=NULL WHERE repo=?1",params![id,head,now()])?;
            reconcile(db,repo)?;
            close_recovered(db,&id,&head,&actor)?;
        }
        result => {
            let message=match &result { Ok(g)=>format!("Full gate failed or flaked: {}",serde_json::to_string(&g.results)?),Err(e)=>format!("Full gate could not complete: {e:#}") };
            let isolate_failure=result.as_ref().is_ok_and(|g|!g.passed && g.flaked.is_empty());
            db.conn.execute("UPDATE verification_backstop_run SET state='failed',ended_at=?2,evidence=?3,isolate_failure=?4 WHERE id=?1",params![run,now(),message,isolate_failure])?;
            db.conn.execute("UPDATE verification_health SET status='failed',message=?2 WHERE repo=?1",params![id,message])?;
            // Infrastructure/flake failures never justify reverting product changes.
            let isolated=if !isolate_failure {Err("the failure was infrastructure or a flake, not a product change".to_string())} else {
                match h.verified_head.as_deref() {
                    Some(base)=>isolate(db,repo,&pb,base,&head,&commands,&actor).map_err(|e|format!("{e:#}")),
                    None=>Err("no Full check has passed yet, so there is no passing base to bisect from".to_string()),
                }
            };
            let culprit=isolated.as_ref().ok().cloned();
            db.conn.execute("UPDATE verification_backstop_run SET culprit=?2,isolation_complete=1 WHERE id=?1",params![run,culprit])?;
            // Without one culprit, the recovery issue names the failing step and the range instead.
            let evidence=match &isolated {
                Ok(_)=>message.clone(),
                Err(why)=>{
                    let step=result.as_ref().ok().and_then(|g|g.results.iter().find(|r|!r.ok)).map(|r|format!("Failing step: `{}`.\n",r.command)).unwrap_or_default();
                    format!("{step}No single culprit: {why}.\n{}\n\n{message}",range_text(db,repo,h.verified_head.as_deref(),&head))
                }
            };
            db.conn.execute("UPDATE verification_backstop_run SET evidence=?2 WHERE id=?1",params![run,evidence])?;
            recover(db,config,repo,project,&pb,&head,culprit.as_deref(),&evidence,&actor)?;
        }
    }
    Ok(())
}

/// A Full check passed at `head`: recovery issues filed for earlier failing heads are done, since
/// whatever fixed them has landed. One left open would keep asking for a fix already made.
fn close_recovered(db:&mut Db,repo_id:&str,head:&str,actor:&Actor)->Result<()> {
    let open:Vec<(String,String)>=db.conn.prepare("SELECT head,issue_key FROM verification_recovery WHERE repo=?1 AND issue_key IS NOT NULL AND head!=?2")?
        .query_map(params![repo_id,head],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (failed,key) in open {
        if !db.issue(&key)?.is_some_and(|i|i.is_open()) {continue;}
        db.add_note(&key,&format!("The Full check passed at {}, which is after the failing {}: integration is healthy again, so this recovery is done.",short(head),short(&failed)),"note",actor)?;
        db.move_issue(&key,"Done",Some(&format!("Full verification passed at {}",short(head))),None,actor)?;
    }
    Ok(())
}

/// Disposable detached source only. Builds remain in warm, admitted lanes.
pub(crate) fn with_checkout<T>(repo:&Path,head:&str,f:impl FnOnce(&Path)->Result<T>)->Result<T> {
    let parent=PathBuf::from(repo_id(repo)?).join("nitely-verification-worktrees");
    std::fs::create_dir_all(&parent)?;
    let mut random=[0u8;16]; getrandom::fill(&mut random).map_err(|e|anyhow::anyhow!("verification checkout randomness: {e}"))?;
    let suffix=random.iter().map(|b|format!("{b:02x}")).collect::<String>();
    let path=parent.join(format!("{}-{suffix}",std::process::id()));
    git(repo,&["worktree","add","--detach",&path.to_string_lossy(),head])?;
    let result=f(&path);
    // Only this newly-created checkout is removed; never reset a user's worktree.
    let cleanup=git(repo,&["worktree","remove","--force",&path.to_string_lossy()]);
    match (result,cleanup) { (Ok(v),Ok(_))=>Ok(v),(Err(e),_)=>Err(e),(Ok(_),Err(e))=>Err(e.context("removing verification checkout")) }
}
fn execute(db:&Db,repo:&Path,pb:&Playbook,head:&str,commands:&[String],actor:&Actor)->Result<crate::lanes::GateRun> {
    if let Some(gate)=in_ci(db,repo,pb,head,commands,actor)? {return Ok(gate);}
    with_checkout(repo,head,|wt| {
        let mut pb=pb.clone();
        pb.gate.retries=0;
        let steps=commands.iter().map(|c|(c.clone(),None)).collect::<Vec<_>>();
        if pb.lanes.is_some() {return crate::lanes::gate_fresh(db,repo,&pb,wt,None,crate::lanes::Kind::Run,&steps,actor);}
        // Custom may disable shared checkouts, never machine memory/slot admission.
        let turn=crate::lanes::hold_machine(db,repo,wt,pb.mode_project.as_deref().unwrap_or(""),None,crate::lanes::Kind::Run,&commands.join("\n"),actor)?;
        crate::project_modes::check_playbook_authority(db,&[],&pb)?;
        let run=turn.id();
        let mut flaked=vec![];
        let (mut results,mut passed)=playbook::refresh_setup(&pb.worktree,wt,repo,pb.integration_branch(),0,&mut flaked)?;
        if passed {for command in commands {
            crate::project_modes::check_playbook_authority(db,&[],&pb)?;
            let command=playbook::expand(command,repo,wt,pb.integration_branch());
            let (ok,output,seconds)=playbook::run_shell_kind(&command,wt,"build")?;
            results.push(playbook::CommandResult{step:None,command,ok,output,seconds,skipped:false,timed_out:false});
            if !ok {passed=false;break;}
        }}
        turn.finish(passed);
        Ok(crate::lanes::GateRun{yolo_skipped:false,commit:head.into(),results,passed,flaked,run:Some(run),warnings:vec![]})
    })
}

/// The Full gate in GitHub Actions through the CI executor (NIT-493, NIT-496), when every command
/// is one of `[lanes] ci_commands`. None when CI cannot take it (a machine's `no_ci_projects`,
/// no GitHub origin or gh, the dispatch refused, a run that never
/// reported) or a command names a local path: the gate then runs in a lane as before. Failed tests
/// in CI are the result, and bisection runs in CI the same way.
fn in_ci(db:&Db,repo:&Path,pb:&Playbook,head:&str,commands:&[String],actor:&Actor)->Result<Option<crate::lanes::GateRun>> {
    // Only the playbook's full suites go to CI (NIT-515). A gate with other commands goes to its
    // lane, where `gate_fresh` sends the full suites to CI and runs the rest beside them.
    let steps:Vec<(String,Option<String>)>=commands.iter().map(|c|(c.clone(),None)).collect();
    if crate::executor::ci_commands(pb,&steps).len()!=commands.len() {return Ok(None);}
    crate::project_modes::check_playbook_authority(db,&[],pb)?;
    let tree=git(repo,&["rev-parse",&format!("{head}^{{tree}}")])?.trim().to_string();
    let snapshot=crate::lanes::Snapshot{commit:head.into(),tree,base:head.into(),made:false};
    let req=crate::executor::SuiteRequest{project:pb.mode_project.as_deref().unwrap_or(""),issue:None,kind:crate::lanes::Kind::Run,snapshot:&snapshot,commands,workflow_ref:None};
    match crate::executor::suite(db,repo,pb,&req,actor) {
        Ok(crate::executor::Suite::Ran(e))=>Ok(Some(crate::lanes::GateRun{yolo_skipped:false,commit:head.into(),results:e.results,passed:e.passed,flaked:vec![],run:Some(e.run),warnings:vec![format!("Full gate ran in CI: {} (queued {} s, ran {} s)",e.url,e.queued_seconds,e.run_seconds)]})),
        Ok(crate::executor::Suite::Unavailable(_))=>Ok(None),
        // A run that never reported: its CI row records why, and the lane runs the gate instead.
        Err(_)=>Ok(None),
    }
}

/// Bisect first-parent commits, then independently verify parent, culprit and inverse at tip.
/// Non-monotonic/interacting changes cannot pass the final isolation checks.
fn isolate(db:&Db,repo:&Path,pb:&Playbook,base:&str,head:&str,commands:&[String],actor:&Actor)->Result<String> {
    isolate_with(repo,base,head,&mut |rev|execute(db,repo,pb,rev,commands,actor).map(|g|g.passed && g.flaked.is_empty()))
}
fn isolate_with(repo:&Path,base:&str,head:&str,pass:&mut dyn FnMut(&str)->Result<bool>)->Result<String> {
    git(repo,&["merge-base","--is-ancestor",base,head])?;
    if !pass(base)? || pass(head)? { bail!("failure is not reproducible from the last green baseline"); }
    let revs=git(repo,&["rev-list","--first-parent","--reverse",&format!("{base}..{head}")])?;
    let revs:Vec<_>=revs.lines().collect();
    if revs.is_empty() || revs.len()>128 { bail!("range is empty or exceeds bounded bisection"); }
    let (mut lo,mut hi)=(0,revs.len()-1);
    while lo<hi { let mid=(lo+hi)/2; if pass(revs[mid])? {lo=mid+1;} else {hi=mid;} }
    let culprit=revs[lo];
    let parents=git(repo,&["rev-list","--parents","-n","1",culprit])?;
    let parents:Vec<_>=parents.split_whitespace().collect();
    if parents.len()!=2 || !pass(parents[1])? || pass(culprit)? { bail!("culprit is not an isolated ordinary commit"); }
    // Also apply the culprit alone to last green: a dependency on other bad changes pauses.
    let alone=with_checkout(repo,base,|wt| {
        git(wt,&["cherry-pick","--no-commit",culprit])?;
        commit_snapshot(wt,"Verify isolated post-merge culprit")
    })?;
    if pass(&alone)? { bail!("failure requires interacting commits"); }
    let inverse=with_checkout(repo,head,|wt| {
        git(wt,&["revert","--no-commit",culprit])?;
        commit_snapshot(wt,"Verify post-merge recovery")
    })?;
    if !pass(&inverse)? { bail!("reverting the candidate does not restore Full health"); }
    Ok(culprit.into())
}
fn commit_snapshot(repo:&Path,message:&str)->Result<String> {
    git(repo,&["-c","user.name=Nitely verification","-c","user.email=verification@localhost","commit","--no-gpg-sign","-m",message])?;
    Ok(git(repo,&["rev-parse","HEAD"])?.trim().to_string())
}
fn reversible(repo:&Path,rules:&Rules,commit:&str)->Result<bool> {
    let paths=git(repo,&["diff-tree","--no-commit-id","--name-only","-r",commit])?;
    if paths.trim().is_empty() { return Ok(false); }
    let every=vec![".*".to_string()];
    let allowed=rules.reversible_paths.as_ref().unwrap_or(&every).iter().map(|s|regex::Regex::new(&format!("^(?:{s})$"))).collect::<std::result::Result<Vec<_>,_>>()?;
    let denied=rules.irreversible_paths.iter().map(|s|regex::Regex::new(&format!("^(?:{s})$"))).collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(paths.lines().all(|p| {
        let lower=p.to_ascii_lowercase();
        !["migration","schema","deploy","release","install","credential","secret","playbook",".nitely/",".github/"].iter().any(|s|lower.contains(s))
            && allowed.iter().any(|r|r.is_match(p)) && !denied.iter().any(|r|r.is_match(p))
    }))
}
struct RecoveryIntent<'a> {
    repo:&'a str, project:&'a str, head:&'a str, culprit:Option<&'a str>,
    can_revert:bool, evidence:&'a str,
}
fn recovery_issue(db:&mut Db,intent:&RecoveryIntent<'_>,actor:&Actor)->Result<String> {
    let mut random=[0u8;32]; getrandom::fill(&mut random).map_err(|e|anyhow::anyhow!("recovery identity randomness: {e}"))?;
    let token=random.iter().map(|b|format!("{b:02x}")).collect::<String>();
    db.conn.execute("INSERT OR IGNORE INTO verification_recovery(repo,head,project,token) VALUES(?1,?2,?3,?4)",params![intent.repo,intent.head,intent.project,token])?;
    let (token,existing):(String,Option<String>)=db.conn.query_row("SELECT token,issue_key FROM verification_recovery WHERE repo=?1 AND head=?2",params![intent.repo,intent.head],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let key=match existing {
        Some(key)=>{db.issue(&key)?.context("recorded recovery issue was removed; inspect recovery before retrying")?;key}
        None=>{
            let issue=db.create_issue(&NewIssue {project:intent.project.into(),title:format!("Restore Full verification after integration {} failed",&intent.head[..intent.head.len().min(12)]),body:format!("Post-merge Full verification failed. Reduced landings are paused.\n\nIsolated culprit: {}. Automatic inverse permitted: {}.\n\n{}\n\n[verification-recovery:{token}]",intent.culprit.unwrap_or("not proven"),intent.can_revert,intent.evidence),kind:"bug".into(),source:"codex".into(),evidence:intent.evidence.into(),priority:2,review_level:Some("sensitive".into()),allow_similar:true,..Default::default()},actor)?;
            // The insertion trigger records this key before create_issue commits.
            let linked:Option<String>=db.conn.query_row("SELECT issue_key FROM verification_recovery WHERE repo=?1 AND head=?2",params![intent.repo,intent.head],|r|r.get(0))?;
            if linked.as_deref()!=Some(&issue.key) { bail!("recovery issue was not atomically linked"); }
            issue.key
        }
    };
    db.conn.execute("UPDATE verification_health SET recovery_issue=?2,status=?3 WHERE repo=?1",params![intent.repo,key,if intent.can_revert {"recovering"} else {"paused"}])?;
    Ok(key)
}

/// Recover only the exact inverse: a stale light-worktree base can fast-forward,
/// a fully staged inverse can be committed, and an already committed inverse reused.
/// Any unrelated edits or extra history stop automatic recovery without discarding work.
fn prepare_inverse(repo:&Path,wt:&Path,head:&str,culprit:&str,message:&str)->Result<String> {
    let inverse=with_checkout(repo,head,|temp| {git(temp,&["revert","--no-commit",culprit])?; Ok(git(temp,&["write-tree"])?.trim().to_string())})?;
    let mut current=git(wt,&["rev-parse","HEAD"])?.trim().to_string();
    let clean=git(wt,&["status","--porcelain"])?.trim().is_empty();
    if clean && current!=head && git(wt,&["merge-base","--is-ancestor",&current,head]).is_ok() {
        git(wt,&["merge","--ff-only",head])?;
        current=head.into();
    }
    if current==head {
        if clean { git(wt,&["revert","--no-commit",culprit])?; }
        if !git(wt,&["diff","--name-only"])?.trim().is_empty()
            || !git(wt,&["ls-files","--others","--exclude-standard"])?.trim().is_empty()
            || git(wt,&["write-tree"])?.trim()!=inverse { bail!("recovery checkout contains changes beyond the proven staged inverse"); }
        return commit_snapshot(wt,message);
    }
    if !clean || git(wt,&["rev-parse","HEAD^{tree}"])?.trim()!=inverse
        || git(wt,&["rev-parse","HEAD^"])?.trim()!=head { bail!("recovery checkout is not the single proven inverse on the failed head"); }
    Ok(current)
}

#[allow(clippy::too_many_arguments)]
fn recover(db:&mut Db,config:&Config,repo:&Path,project:&str,pb:&Playbook,head:&str,culprit:Option<&str>,evidence:&str,actor:&Actor)->Result<()> {
    let id=repo_id(repo)?;
    let can_revert=pb.verification.backstop.auto_revert && culprit.map(|c|reversible(repo,&pb.verification.backstop,c)).transpose()?.unwrap_or(false);
    if let Some(culprit)=culprit { crate::verification_audit::confirmed_landing_culprit(db,project,&id,culprit,evidence)?; }
    let key=recovery_issue(db,&RecoveryIntent{repo:&id,project,head,culprit,can_revert,evidence},actor)?;
    if !can_revert { return Ok(()); }
    let culprit=culprit.context("no isolated culprit")?;
    // If integration moved, a new worker must verify that exact inverse first.
    if git(repo,&["rev-parse",pb.integration_branch()])?.trim()!=head { return Ok(()); }
    let issue=db.issue(&key)?.context("recovery issue disappeared")?;
    if matches!(issue.state.as_str(),"In Review"|"Approved") {
        crate::land::land(db,config,&crate::land::LandOptions { dir:&repo.to_string_lossy(),issue_key:&key,push:true,keep:false,break_lock:false },actor)?;
        return Ok(());
    }
    if matches!(issue.state.as_str(),"Needs You"|"Done") { return Ok(()); }
    let started=playbook::start_task(db,repo,&issue,"codex",actor)?;
    let wt=Path::new(&started.worktree);
    if let Err(e)=prepare_inverse(repo,wt,head,culprit,&format!("Restore Full verification: [{key}](http://127.0.0.1:7423/i/{key})")) {
        db.add_note(&key,&format!("Recovery checkout needs inspection before submitting: {e:#}"),"handoff",actor)?;
        db.conn.execute("UPDATE verification_health SET status='paused',message='Recovery worktree needs inspection' WHERE repo=?1",[&id])?;
        return Ok(());
    }
    db.conn.execute("UPDATE verification_health SET status='recovering',message='Isolated inverse submitted through normal review and approval' WHERE repo=?1",[id])?;
    let original:Option<String>=db.conn.query_row("SELECT issue_key FROM verification_landing WHERE repo=?1 AND head=?2 ORDER BY landed_at DESC LIMIT 1",params![repo_id(repo)?,culprit],|r|r.get(0)).optional()?;
    if let Some(original)=original {
        if db.issue(&original)?.is_some_and(|i|i.state=="Done") { db.move_issue(&original,"Todo",Some("Post-merge Full check isolated this change as a regression"),None,actor)?; }
        db.add_note(&original,&format!("An isolated regression is being reverted through [{key}](http://127.0.0.1:7423/i/{key})."),"note",actor)?;
    }
    let solution=crate::submit::SolutionSummary { title:"Restore the last proven integration behavior".into(),summary:format!("Reverts isolated culprit {culprit}; Full checks failed on it alone and passed with its inverse at {head}. Normal gate, independent review and human approval rules still apply.") };
    let current=git(wt,&["rev-parse","HEAD"])?.trim().to_string();
    let submitted:Option<String>=db.conn.query_row("SELECT submitted_head FROM verification_recovery WHERE repo=?1 AND head=?2",params![repo_id(repo)?,head],|r|r.get(0))?;
    if submitted.as_deref()==Some(&current) {
        db.conn.execute("UPDATE verification_health SET status='paused',message='Recovery revision needs fixes before another submit' WHERE repo=?1",[repo_id(repo)?])?;
        return Ok(());
    }
    let submitted=crate::submit::submit(db,config,&crate::submit::SubmitOptions { dir:&started.worktree,issue_key:Some(&key),skip_review:false,local_only:false,batch:false,solution:Some(&solution),solution_commit:None,solution_patch_id:None },actor)?;
    db.conn.execute("UPDATE verification_recovery SET submitted_head=?3 WHERE repo=?1 AND head=?2",params![repo_id(repo)?,head,current])?;
    if submitted.state=="In Progress" {
        db.conn.execute("UPDATE verification_health SET status='paused',message='Recovery gate or review needs fixes' WHERE repo=?1",[repo_id(repo)?])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule `observations_never_regress_and_unrecorded_tip_blocks_reduced_landing` and
    /// `authoritative_backward_reset_invalidates_the_old_green_baseline` reach with real repos.
    #[test]
    fn an_older_observation_never_replaces_a_newer_one_unless_the_branch_was_reset() {
        let unread=|| -> bool { panic!("not read for the same head") };
        let unread_branch=|| -> Result<bool> { panic!("read only when the head is behind") };
        assert_eq!(observation("a","a",unread,unread_branch).unwrap(),Observation::Keep);
        assert_eq!(observation("a","b",|| false,unread_branch).unwrap(),Observation::Record,"ahead or diverged: recorded");
        assert_eq!(observation("b","a",|| true,|| Ok(false)).unwrap(),Observation::Keep,"a late, older observation");
        assert_eq!(observation("b","a",|| true,|| Ok(true)).unwrap(),Observation::Record,"the branch was reset there");
    }
    fn fixture()->(Db,PathBuf,String) {
        let repo=crate::test_repo::copy("verification-backstop-durability",|p|crate::test_repo::init(p,&[("value.txt","good"),("a.txt","0"),("b.txt","0"),(".nitely/playbook.toml","[verification.backstop]\nenabled=true\n")]));
        let db=Db::open(&crate::paths::unique_temp("nitely-backstop-test",".sqlite")).unwrap();
        db.create_project("APP","Example").unwrap();
        let base=git(&repo,&["rev-parse","HEAD"]).unwrap().trim().to_string();
        observe(&db,&repo,"APP",&base).unwrap();
        db.conn.execute("UPDATE verification_health SET verified_head=?1,status='green',checked_at=?2",params![base,now()]).unwrap();
        (db,repo,base)
    }
    fn change(repo:&Path,path:&str,value:&str)->String {
        std::fs::write(repo.join(path),value).unwrap();
        crate::test_repo::git(repo,&["add",path]);crate::test_repo::git(repo,&["commit","-m","change"]);
        git(repo,&["rev-parse","HEAD"]).unwrap().trim().to_string()
    }
    fn state()->Health { Health { repo:"repo".into(),project:"APP".into(),observed_head:"b".into(),verified_head:Some("a".into()),pending_since:100,reduced_count:1,checked_at:50,status:"pending".into(),message:String::new(),recovery_issue:None } }
    #[test]
    fn due_by_count_age_and_changed_nightly_but_never_unchanged() {
        let r=Rules::default(); let mut h=state();
        assert!(!due(&h,&r,1899)); assert!(due(&h,&r,1900));
        h.reduced_count=5; assert!(due(&h,&r,101));
        h.reduced_count=0; assert!(!due(&h,&r,1900)); assert!(due(&h,&r,86450));
        h.verified_head=Some("b".into()); assert!(!due(&h,&r,999999));
        h.verified_head=None; assert!(due(&h,&r,101));
    }

    #[test]
    fn observations_never_regress_and_unrecorded_tip_blocks_reduced_landing() {
        let (db,repo,base)=fixture();
        let head=change(&repo,"value.txt","bad");
        let rules=Rules{enabled:true,..Default::default()};
        assert!(reduced_ready(&db,&repo,&rules,now()).unwrap_err().to_string().contains("integration moved"));
        assert!(release_ready(&db,&repo,&head).is_err(),"old green health cannot authorize the unrecorded tip");
        observe(&db,&repo,"APP",&head).unwrap();
        let other=Db::open(Path::new(db.conn.path().unwrap())).unwrap();
        observe(&other,&repo,"APP",&base).unwrap();
        let h=health(&db,&repo).unwrap().unwrap();
        assert_eq!(h.observed_head,head);assert_eq!(h.status,"pending");
        assert!(reduced_ready(&db,&repo,&rules,now()).is_ok());
    }

    #[test]
    fn authoritative_backward_reset_invalidates_the_old_green_baseline() {
        let (db,repo,base)=fixture();
        let head=change(&repo,"value.txt","changed");
        observe(&db,&repo,"APP",&head).unwrap();
        db.conn.execute("UPDATE verification_health SET verified_head=?1,status='green'",[&head]).unwrap();
        crate::test_repo::git(&repo,&["reset","--hard",&base]);
        observe(&db,&repo,"APP",&base).unwrap();
        let h=health(&db,&repo).unwrap().unwrap();
        assert_eq!(h.observed_head,base);
        assert_eq!(h.status,"paused");
        assert!(reduced_ready(&db,&repo,&Rules{enabled:true,..Default::default()},now()).is_err());
    }

    #[test]
    fn ordinary_projects_are_quiet_but_removed_recorded_policy_fails_closed() {
        let mut db=Db::open(&crate::paths::unique_temp("nitely-backstop-opt-in",".sqlite")).unwrap();
        db.create_project("APP","Example").unwrap();
        let repo=crate::test_repo::copy("verification-no-policy",|p|crate::test_repo::init(p,&[("help.txt","help")]));
        tick(&mut db,&Config::default(),&repo,"APP","main").unwrap();
        run_audit(&mut db,&repo,"APP","main").unwrap();
        assert!(health(&db,&repo).unwrap().is_none());
        let head=git(&repo,&["rev-parse","HEAD"]).unwrap();
        let issue=db.create_issue(&NewIssue{project:"APP".into(),title:"Ordinary change".into(),body:"No verification policy configured".into(),kind:"feature".into(),source:"codex".into(),evidence:"test".into(),..Default::default()},&Actor::Agent("codex:test".into())).unwrap();
        let landed=crate::land::Landed{head:Some(head.trim().into()),..Default::default()};
        on_landed(&mut db,&repo,&issue,&landed).unwrap();
        assert!(health(&db,&repo).unwrap().is_none(),"ordinary landing does not opt the repo into backstop checks");
        observe(&db,&repo,"APP",head.trim()).unwrap();
        assert!(tick(&mut db,&Config::default(),&repo,"APP","main").unwrap_err().to_string().contains("missing"));
        assert_eq!(health(&db,&repo).unwrap().unwrap().status,"paused");
        std::fs::create_dir_all(repo.join(".nitely")).unwrap();
        let head=change(&repo,".nitely/playbook.toml","[gate]\ncommands=['echo checked']\n");
        assert!(optional_policy(&db,&repo,&head).is_err(),"removing only the verification section also fails closed");
    }

    #[test]
    fn reconciliation_recovers_counter_and_original_deadline_after_interrupted_landing() {
        let (db,repo,base)=fixture();let head=change(&repo,"value.txt","bad");
        db.conn.execute("INSERT INTO verification_landing(repo,head,base,issue_key,reduced,landed_at) VALUES(?1,?2,?3,'APP-1',1,?4)",params![repo_id(&repo).unwrap(),head,base,now()-1900]).unwrap();
        observe(&db,&repo,"APP",&head).unwrap();
        let rules=Rules{enabled:true,..Default::default()};
        assert!(reduced_ready(&db,&repo,&rules,now()).is_err());
        let h=health(&db,&repo).unwrap().unwrap();assert_eq!(h.reduced_count,1);assert!(due(&h,&rules,now()));
        db.conn.execute("UPDATE verification_health SET verified_head=?1",[head]).unwrap();
        reconcile(&db,&repo).unwrap();assert_eq!(health(&db,&repo).unwrap().unwrap().reduced_count,0);
    }

    #[test]
    fn issue_creation_links_ticket_atomically_and_reuses_it_after_restart() {
        let (mut db,repo,head)=fixture();let id=repo_id(&repo).unwrap();
        let actor=Actor::Agent("verification-backstop".into());
        let intent=RecoveryIntent{repo:&id,project:"APP",head:&head,culprit:Some(&head),can_revert:true,evidence:"reproduced failure"};
        let key=recovery_issue(&mut db,&intent,&actor).unwrap();
        db.conn.execute("UPDATE verification_health SET recovery_issue=NULL,status='failed'",[]).unwrap();
        let path=db.conn.path().unwrap().to_owned();drop(db);let mut db=Db::open(Path::new(&path)).unwrap();
        assert_eq!(recovery_issue(&mut db,&intent,&actor).unwrap(),key);
        assert_eq!(db.list_issues(Some("APP"),true).unwrap().len(),1);
        assert_eq!(health(&db,&repo).unwrap().unwrap().status,"recovering");
        db.rekey_project("APP","NEW").unwrap();
        let linked:String=db.conn.query_row("SELECT issue_key FROM verification_recovery",[],|r|r.get(0)).unwrap();assert_eq!(linked,"NEW-1");
        db.remove_project("NEW",true).unwrap();
        assert!(health_all(&db).unwrap().is_empty());
    }

    #[test]
    fn inverse_preparation_resumes_stale_base_staged_changes_and_committed_inverse() {
        let (_db,repo,base)=fixture();let culprit=change(&repo,"value.txt","bad");
        with_checkout(&repo,&base,|wt| {
            let inverse=prepare_inverse(&repo,wt,&culprit,&culprit,"Restore verified behavior")?;
            assert_eq!(std::fs::read_to_string(wt.join("value.txt"))?,"good");
            assert_eq!(git(wt,&["rev-parse","HEAD^"])?.trim(),culprit);
            assert_eq!(prepare_inverse(&repo,wt,&culprit,&culprit,"Resume")?,inverse);
            Ok(())
        }).unwrap();
        with_checkout(&repo,&culprit,|wt| {
            git(wt,&["revert","--no-commit",&culprit])?;
            prepare_inverse(&repo,wt,&culprit,&culprit,"Resume staged inverse")?;
            assert!(git(wt,&["status","--porcelain"])?.trim().is_empty());Ok(())
        }).unwrap();
        with_checkout(&repo,&culprit,|wt| {
            std::fs::write(wt.join("value.txt"),"human work")?;
            assert!(prepare_inverse(&repo,wt,&culprit,&culprit,"Must not overwrite").is_err());
            assert_eq!(std::fs::read_to_string(wt.join("value.txt"))?,"human work");Ok(())
        }).unwrap();
    }

    #[test]
    fn a_head_known_broken_is_not_run_again_and_the_next_landing_is_checked_at_once() {
        use crate::project_modes::{Backstop, Mode, Options};
        // The 2026-10-20 case: an after-land install failed to build main, the first Full check
        // ran anyway, found no culprit, and the fix's landing sat behind the cadence.
        let root=crate::test_repo::copy("backstop-known-broken",|p|crate::test_repo::init(p,&[(playbook::PLAYBOOK_PATH,"[gate]\ncommands=['echo full-checked']\n"),("value.txt","good")]));
        let file=crate::paths::unique_temp("backstop-known-broken-db",".sqlite");let mut db=Db::open(&file).unwrap();db.create_project("APP","Example").unwrap();
        db.set_project_review_mode("APP",Mode::Custom,Options{shared_lanes:Some(false),full_backstop:Some(Backstop{enabled:true,landings:100,delay_seconds:86400}),audit_percent:Some(0),..Default::default()},0,&Actor::Human).unwrap();
        let config=Config::default();
        tick(&mut db,&config,&root,"APP","main").unwrap();
        assert_eq!(health(&db,&root).unwrap().unwrap().status,"green");
        let before=db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get::<_,i64>(0)).unwrap();

        let base=git(&root,&["rev-parse","HEAD"]).unwrap().trim().to_string();
        let broken=change(&root,"value.txt","does not build");
        let issue=db.create_issue(&NewIssue{project:"APP".into(),title:"A change".into(),body:"b".into(),kind:"feature".into(),source:"codex".into(),evidence:"e".into(),..Default::default()},&Actor::Agent("codex:test".into())).unwrap();
        let failed=playbook::CommandResult{step:None,command:"cargo run --release -p app -- install".into(),ok:false,seconds:3,output:"error[E0063]: missing field".into(),skipped:false,timed_out:false};
        on_landed(&mut db,&root,&issue,&crate::land::Landed{head:Some(broken.clone()),landed_from:Some(base.clone()),after:vec![failed],..Default::default()}).unwrap();
        let h=health(&db,&root).unwrap().unwrap();
        assert!(h.status=="failed" && h.message.starts_with(&format!("Known broken at {}",short(&broken))),"{h:?}");

        tick(&mut db,&config,&root,"APP","main").unwrap();
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get::<_,i64>(0)).unwrap(),before,"no Full run on a head already known broken");
        let recovery=db.issue(&health(&db,&root).unwrap().unwrap().recovery_issue.expect("a recovery issue")).unwrap().unwrap();
        assert!(recovery.body.contains("after-land step `cargo run --release -p app -- install` failed") && recovery.body.contains(&format!("Range: {}..{} (1 commit",short(&base),short(&broken))),"{}",recovery.body);

        // The fix lands: checked at once though neither count nor delay is due, and the recovery closes.
        change(&root,"value.txt","fixed");
        tick(&mut db,&config,&root,"APP","main").unwrap();
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get::<_,i64>(0)).unwrap(),before+1);
        assert_eq!(health(&db,&root).unwrap().unwrap().status,"green");
        assert_eq!(db.issue(&recovery.key).unwrap().unwrap().state,"Done");
        drop(db);std::fs::remove_dir_all(root).ok();std::fs::remove_file(file).ok();
    }

    #[test]
    fn isolation_proves_single_culprit_and_rejects_interactions_and_irreversible_paths() {
        let (_db,repo,base)=fixture();let culprit=change(&repo,"value.txt","bad");
        let tip=change(&repo,"a.txt","1");
        let isolated=isolate_with(&repo,&base,&tip,&mut |rev|Ok(git(&repo,&["show",&format!("{rev}:value.txt")])?.trim()=="good")).unwrap();
        assert_eq!(isolated,culprit);
        // Unset paths revert anything but the built-in list; an explicit [] reverts nothing.
        let r=Rules::default();assert!(reversible(&repo,&r,&culprit).unwrap());
        let migration=change(&repo,"schema.sql","change schema");assert!(!reversible(&repo,&r,&migration).unwrap());
        assert!(!reversible(&repo,&Rules{reversible_paths:Some(vec![]),..Default::default()},&culprit).unwrap());
        let (_db,repo,base)=fixture();change(&repo,"a.txt","1");let tip=change(&repo,"b.txt","1");
        let result=isolate_with(&repo,&base,&tip,&mut |rev| {
            let a=git(&repo,&["show",&format!("{rev}:a.txt")])?;let b=git(&repo,&["show",&format!("{rev}:b.txt")])?;
            Ok(!(a.trim()=="1" && b.trim()=="1"))
        });
        assert!(result.unwrap_err().to_string().contains("interacting"));
    }
}
