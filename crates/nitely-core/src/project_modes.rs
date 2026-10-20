//! Human-owned execution presets. Repository commands and approval rules remain authoritative.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::{Db, Actor, playbook::{Playbook, GateResult, LandBatch}, review::BatchMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum Mode { Yolo, Fast, Balanced, Thorough, Custom }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum TestDepth { Off, Basic, Risk, Full }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum PreReview { Off, Individual, Batched }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backstop { pub enabled:bool, pub landings:u32, pub delay_seconds:i64 }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Periodic { pub enabled:bool, pub every_landings:u32, pub max_delay_seconds:i64, pub reviewer:String, pub findings_strategy:String }
impl Default for Periodic {
 fn default()->Self {Self {enabled:false,every_landings:5,max_delay_seconds:1800,reviewer:"configured".into(),findings_strategy:"reopen_or_create".into()}}
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default,deny_unknown_fields)]
pub struct Options {
 pub test_depth:Option<TestDepth>, pub pre_review:Option<PreReview>, pub land_batch:Option<bool>, pub shared_lanes:Option<bool>,
 pub full_backstop:Option<Backstop>, pub audit_percent:Option<u8>, pub periodic:Option<Periodic>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Preset {pub mode:Mode,pub options:Options}
#[derive(Debug, Clone, Serialize)]
pub struct ProjectReviewMode {
 pub project:String,pub mode:Mode,pub revision:i64,pub overrides:Options,pub presets:Vec<Preset>,
 /// False until a human explicitly saves modes; migration does not invalidate existing reviews.
 pub explicit:bool,
}
fn presets()->Vec<Preset> {
 [Mode::Yolo,Mode::Fast,Mode::Balanced,Mode::Thorough].into_iter().map(|mode|Preset {mode,options:preset(mode)}).collect()
}
fn preset(mode:Mode)->Options {
 if mode==Mode::Custom {return Options::default();}
 let balanced=mode==Mode::Balanced;
 let periodic=matches!(mode,Mode::Fast|Mode::Balanced);
 Options {test_depth:Some(match mode {Mode::Yolo=>TestDepth::Off,Mode::Fast=>TestDepth::Basic,Mode::Balanced=>TestDepth::Risk,_=>TestDepth::Full}),
 pre_review:Some(match mode {Mode::Yolo|Mode::Fast=>PreReview::Off,Mode::Balanced=>PreReview::Batched,_=>PreReview::Individual}),
 land_batch:Some(mode!=Mode::Thorough),shared_lanes:Some(true),full_backstop:Some(Backstop{enabled:balanced,landings:5,delay_seconds:1800}),
 audit_percent:Some(if balanced {10}else{0}),periodic:Some(Periodic{enabled:periodic,..Default::default()})}
}
impl ProjectReviewMode {
 pub fn options(&self)->Options {if self.mode==Mode::Custom {self.overrides.clone()}else{preset(self.mode)}}
 pub fn effective_options(&self,pb:&Playbook)->Options {
  let o=self.options();
  Options {test_depth:o.test_depth.or(Some(if pb.verification.mode=="active" {TestDepth::Risk}else{TestDepth::Full})),
   pre_review:o.pre_review.or(Some(if !pb.review.enabled {PreReview::Off}else if matches!(pb.review.batch,BatchMode::All|BatchMode::Small) {PreReview::Batched}else{PreReview::Individual})),
   land_batch:o.land_batch.or(Some(pb.land.batch!=LandBatch::Off)),shared_lanes:o.shared_lanes.or(Some(pb.lanes.is_some())),
   full_backstop:o.full_backstop.or(Some(Backstop{enabled:pb.verification.backstop.enabled,landings:pb.verification.backstop.landings,delay_seconds:pb.verification.backstop.delay_seconds})),
   audit_percent:o.audit_percent.or(Some(pb.verification.audit_percent)),periodic:Some(o.periodic.unwrap_or_default())}
 }
 pub fn authority(&self)->Result<Option<String>> {
  if !self.explicit {return Ok(None);}
  Ok(Some(format!("{:x}",Sha256::digest(serde_json::to_vec(&(self.mode,self.revision,self.options()))?))))
 }
}
pub(crate) fn read(conn:&Connection,project:&str)->Result<ProjectReviewMode> {
 let row=conn.query_row("SELECT p.key,COALESCE(m.mode,CASE WHEN y.enabled=1 THEN 'yolo' ELSE 'custom' END),COALESCE(m.overrides,'{}'),COALESCE(m.revision,y.revision,0),COALESCE(m.explicit,0) FROM project p LEFT JOIN project_review_mode m ON m.project=p.key LEFT JOIN project_yolo y ON y.project=p.key WHERE p.key=?1 COLLATE NOCASE",[project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,bool>(4)?))).optional()?.with_context(||format!("no project {project}"))?;
 Ok(ProjectReviewMode{project:row.0,mode:serde_json::from_value(serde_json::Value::String(row.1))?,overrides:serde_json::from_str(&row.2)?,revision:row.3,explicit:row.4,presets:presets()})
}
fn validate(o:&Options)->Result<()> {
 if o.audit_percent.is_some_and(|n|n!=0 && !(10..=100).contains(&n)) {bail!("audit percent must be zero (off), or between 10 and 100");}
 if let Some(b)=&o.full_backstop {if !(1..=1000).contains(&b.landings)||!(1..=86400).contains(&b.delay_seconds) {bail!("Full backstop needs 1–1000 landings and 1–86400 seconds");}}
 if let Some(p)=&o.periodic {
  if !(1..=1000).contains(&p.every_landings)||!(0..=604800).contains(&p.max_delay_seconds) {bail!("periodic review needs 1–1000 landings and 1–604800 seconds");}
  if !matches!(p.reviewer.as_str(),"configured"|"codex"|"claude"|"cursor") {bail!("unsupported periodic reviewer");}
  if !matches!(p.findings_strategy.as_str(),"reopen_or_create"|"create") {bail!("unsupported findings strategy");}
 }
 Ok(())
}
impl Db {
 pub fn project_review_mode(&self,project:&str)->Result<ProjectReviewMode> {read(&self.conn,project)}
 pub fn set_project_review_mode(&mut self,project:&str,mode:Mode,overrides:Options,expected_revision:i64,actor:&Actor)->Result<ProjectReviewMode> {
  if *actor!=Actor::Human {bail!("only the person changes review modes, in project settings");}
  validate(&overrides)?;
  if mode!=Mode::Custom && overrides!=Options::default() {bail!("preset controls are fixed; select Custom to override them");}
  let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
  let before=read(&tx,project)?;
  if before.revision!=expected_revision {bail!("review mode settings changed; reload before saving");}
  if !before.explicit||before.mode!=mode||before.overrides!=overrides {
   let at=crate::db::now();let mode_text=serde_json::to_value(mode)?.as_str().context("mode name")?.to_owned();
   tx.execute("INSERT INTO project_review_mode(project,mode,overrides,revision,explicit,updated_at,updated_by) VALUES(?1,?2,?3,?4,1,?5,?6) ON CONFLICT(project) DO UPDATE SET mode=excluded.mode,overrides=excluded.overrides,revision=excluded.revision,explicit=1,updated_at=excluded.updated_at,updated_by=excluded.updated_by",params![before.project,mode_text,serde_json::to_string(&overrides)?,before.revision+1,at,actor.id()])?;
   tx.execute("INSERT INTO project_yolo(project,enabled,revision,updated_at,updated_by) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(project) DO UPDATE SET enabled=excluded.enabled,revision=excluded.revision,updated_at=excluded.updated_at,updated_by=excluded.updated_by",params![before.project,mode==Mode::Yolo,before.revision+1,at,actor.id()])?;
   tx.execute("INSERT INTO event(ts,actor,type,summary,payload) VALUES(?1,?2,'project_review_mode_changed',?3,?4)",params![at,actor.id(),format!("{} review mode: {mode_text}",before.project),serde_json::json!({"project":before.project,"before":before,"mode":mode,"overrides":overrides}).to_string()])?;
  }
  let after=read(&tx,project)?;tx.commit()?;Ok(after)
 }
}
pub fn for_issue(db:&Db,key:&str)->Result<Option<ProjectReviewMode>> {
 db.issue(key)?.context("mode issue disappeared")?.project.as_deref().map(|p|db.project_review_mode(p)).transpose()
}
pub fn authority_for_issue(db:&Db,key:&str)->Result<Option<String>> {for_issue(db,key)?.map(|s|s.authority()).transpose().map(Option::flatten)}
pub fn tests_off(db:&Db,key:&str)->Result<bool> {Ok(for_issue(db,key)?.is_some_and(|s|s.options().test_depth==Some(TestDepth::Off)))}
pub fn review_off(db:&Db,key:&str)->Result<bool> {Ok(for_issue(db,key)?.is_some_and(|s|s.options().pre_review==Some(PreReview::Off)))}
pub fn apply_project(db:&Db,project:&str,pb:&mut Playbook)->Result<()> {
 let settings=db.project_review_mode(project)?;let o=settings.options();pb.mode_authority=settings.authority()?;pb.mode_project=Some(settings.project.clone());
 if let Some(r)=o.pre_review {pb.review.enabled=r!=PreReview::Off;pb.review.batch=if r==PreReview::Batched {BatchMode::All}else{BatchMode::Off};}
 if let Some(batch)=o.land_batch {pb.land.batch=if batch {LandBatch::Waiting}else{LandBatch::Off};}
 if let Some(shared)=o.shared_lanes {if shared {if pb.lanes.is_none() {pb.lanes=Some(Default::default());}}else{pb.lanes=None;}}
 if let Some(b)=o.full_backstop {pb.verification.backstop.enabled=b.enabled;pb.verification.backstop.landings=b.landings;pb.verification.backstop.delay_seconds=b.delay_seconds;}
 if let Some(n)=o.audit_percent {pb.verification.audit_percent=n;}
 Ok(())
}
pub fn apply_issue(db:&Db,key:&str,pb:&mut Playbook)->Result<()> {
 if let Some(s)=for_issue(db,key)? {apply_project(db,&s.project,pb)?;} Ok(())
}
pub fn check_gate_authority(db:&Db,keys:&[&str],gate:&GateResult)->Result<()> {
 for key in keys {
  if authority_for_issue(db,key)?!=gate.mode_authority {bail!("project review mode changed; submit again with current settings");}
  if gate.skipped.is_some()&&!tests_off(db,key)? {bail!("automatic tests are now required; submit again");}
 }Ok(())
}
pub fn check_playbook_authority(db:&Db,keys:&[&str],pb:&Playbook)->Result<()> {
 if let Some(project)=&pb.mode_project {if db.project_review_mode(project)?.authority()?!=pb.mode_authority {bail!("project review mode changed while queued; retry with current settings");}}
 for key in keys {if authority_for_issue(db,key)?!=pb.mode_authority {bail!("project review mode changed while queued; submit again");}}Ok(())
}

#[derive(Debug,Clone,Serialize)]
pub struct EffectiveRepository {pub repo:String,pub options:Options,pub warnings:Vec<String>}
pub fn effective_repositories(db:&Db,config:&crate::Config,project:&str)->Result<Vec<EffectiveRepository>> {
 let settings=db.project_review_mode(project)?;
 config.repos.iter().filter(|r|r.project.as_deref().is_some_and(|p|p.eq_ignore_ascii_case(project))).map(|repo| {
  let root=std::path::Path::new(&repo.root);
  let policy=crate::playbook::git(root,&["show",&format!("refs/heads/{}:{}",repo.integration_branch,crate::playbook::PLAYBOOK_PATH)]);
  let (pb,mut warnings)=match policy.and_then(|s|Playbook::parse(&s)) {Ok(pb)=>{let w=pb.unknown_warnings();(pb,w)},Err(e)=>(Playbook::default(),vec![format!("Committed repository settings unavailable: {e:#}")])};
  let options=settings.effective_options(&pb);
  if matches!(options.test_depth,Some(TestDepth::Basic|TestDepth::Risk)) && pb.verification.basic.is_empty() {warnings.push("Basic commands are not configured; automatic verification falls back to Full.".into());}
  if options.test_depth==Some(TestDepth::Risk) {warnings.push("Risk-based verification falls back to Full for missing mappings, protected changes or an unhealthy enabled backstop.".into());}
  Ok(EffectiveRepository{repo:repo.name.clone(),options,warnings})
 }).collect()
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn presets_preserve_legacy_projects_and_require_human_revision_authority() {
  let dir=crate::paths::unique_temp("nitely-modes","");std::fs::create_dir_all(&dir).unwrap();
  let db=Db::open(&dir.join("state.db")).unwrap();
  db.create_project("APP","Example").unwrap();db.create_project("WEB","Website").unwrap();
  db.conn.execute("INSERT INTO project_yolo(project,enabled,revision,updated_at,updated_by) VALUES('APP',1,3,0,'human')",[]).unwrap();
  // Reopen an actual pre-mode schema: upgrade materializes legacy settings without opting in.
  db.conn.execute("DROP TABLE project_review_mode",[]).unwrap();
  db.conn.execute("DELETE FROM schema_migration WHERE name='20261021-project-review-modes'",[]).unwrap();
  drop(db);let mut db=Db::open(&dir.join("state.db")).unwrap();
  let legacy=db.project_review_mode("APP").unwrap();assert_eq!(legacy.mode,Mode::Yolo);assert_eq!(legacy.revision,3);assert_eq!(legacy.authority().unwrap(),None);
  let untouched=db.project_review_mode("WEB").unwrap();assert_eq!(untouched.mode,Mode::Custom);assert_eq!(untouched.options(),Options::default());
  assert!(db.set_project_review_mode("APP",Mode::Thorough,Options::default(),3,&Actor::Agent("cli".into())).is_err());
  let changed=db.set_project_review_mode("app",Mode::Balanced,Options::default(),3,&Actor::Human).unwrap();
  assert_eq!(changed.revision,4);assert!(changed.authority().unwrap().is_some());assert!(!db.project_yolo("APP").unwrap().enabled);
  assert!(db.set_project_review_mode("APP",Mode::Yolo,Options::default(),3,&Actor::Human).is_err());
  assert_eq!(db.project_review_mode("WEB").unwrap().options(),Options::default());
  let mut pb=Playbook::default();pb.approval.always_human=true;
  apply_project(&db,"APP",&mut pb).unwrap();assert!(pb.approval.always_human);assert_eq!(pb.review.batch,BatchMode::All);assert!(pb.verification.backstop.enabled);
  db.set_project_review_mode("APP",Mode::Yolo,Options::default(),4,&Actor::Human).unwrap();
  apply_project(&db,"APP",&mut pb).unwrap();assert!(!pb.review.enabled);assert!(!pb.verification.backstop.enabled);assert_eq!(pb.verification.audit_percent,0);assert!(pb.approval.always_human);
  drop(db);std::fs::remove_dir_all(dir).ok();
 }
 #[test]
 fn independent_depth_controls_execute_real_commands_and_manual_gate_stays_full() {
  use crate::test_repo::git as g;
  let root=crate::test_repo::copy("mode-gate",|p|crate::test_repo::init(p,&[(crate::playbook::PLAYBOOK_PATH,"[gate]\ncommands=['exit 1']\nretries=0\n[verification]\nbasic=['echo basic-checked']\n"),("readme.txt","old")]));
  let file=crate::paths::unique_temp("mode-gate-db",".sqlite");let mut db=Db::open(&file).unwrap();db.create_project("APP","Example").unwrap();
  let issue=db.create_issue(&crate::NewIssue{project:"APP".into(),title:"A real change".into(),body:"Verify configured tests execute and independent review remains enabled".into(),evidence:"Real committed fixture".into(),source:"codex".into(),kind:"feature".into(),..Default::default()},&Actor::Human).unwrap();
  g(&root,&["checkout","-b","codex/app-1-example"]);std::fs::write(root.join("readme.txt"),"new").unwrap();g(&root,&["add","readme.txt"]);g(&root,&["commit","-m","change"]);
  let opts=Options{test_depth:Some(TestDepth::Basic),pre_review:Some(PreReview::Individual),shared_lanes:Some(false),..Default::default()};
  db.set_project_review_mode("APP",Mode::Custom,opts,0,&Actor::Human).unwrap();
  let gate=crate::playbook::run_automatic_gate(&db,&root,&root,&[&issue.key],&Actor::Human,crate::lanes::Kind::Gate).unwrap();
  assert!(gate.passed,"{gate:?}");assert_eq!(gate.plan.as_ref().unwrap().depth,"Basic");assert!(gate.results.iter().any(|r|r.output.contains("basic-checked")&&!r.skipped));assert!(!review_off(&db,&issue.key).unwrap());
  let manual=crate::playbook::run_gate(&db,&root,&root,Some(&issue.key),&Actor::Human).unwrap();assert!(!manual.passed);assert!(manual.mode_authority.is_none());
  db.set_project_review_mode("APP",Mode::Custom,Options{test_depth:Some(TestDepth::Off),pre_review:Some(PreReview::Individual),..Default::default()},1,&Actor::Human).unwrap();
  assert!(check_gate_authority(&db,&[&issue.key],&gate).is_err());
  let skipped=crate::playbook::run_automatic_gate(&db,&root,&root,&[&issue.key],&Actor::Human,crate::lanes::Kind::Gate).unwrap();
  assert!(!skipped.passed);assert!(skipped.results.is_empty());assert!(!skipped.skipped.as_ref().unwrap().contains("YOLO"));assert!(!review_off(&db,&issue.key).unwrap());
  db.set_project_review_mode("APP",Mode::Thorough,Options::default(),2,&Actor::Human).unwrap();assert!(check_gate_authority(&db,&[&issue.key],&skipped).is_err());
  g(&root,&["checkout","main"]);std::fs::write(root.join(crate::playbook::PLAYBOOK_PATH),"[gate]\ncommands=['exit 1']\nretries=0\n").unwrap();g(&root,&["add",crate::playbook::PLAYBOOK_PATH]);g(&root,&["commit","-m","remove basic commands"]);
  db.set_project_review_mode("APP",Mode::Custom,Options{test_depth:Some(TestDepth::Basic),shared_lanes:Some(false),..Default::default()},3,&Actor::Human).unwrap();
  let fallback=crate::playbook::run_automatic_gate(&db,&root,&root,&[&issue.key],&Actor::Human,crate::lanes::Kind::Gate).unwrap();
  assert!(!fallback.passed && fallback.plan.is_none(),"unconfigured Basic must execute the failing Full gate");
  drop(db);std::fs::remove_dir_all(root).ok();std::fs::remove_file(file).ok();
 }
 #[test]
 fn full_backstop_runs_without_shared_lanes_and_yolo_stops_future_automatic_work() {
  use crate::test_repo::git as g;
  let root=crate::test_repo::copy("mode-backstop",|p|crate::test_repo::init(p,&[(crate::playbook::PLAYBOOK_PATH,"[gate]\ncommands=['echo full-checked']\n"),("readme.txt","base")]));
  let file=crate::paths::unique_temp("mode-backstop-db",".sqlite");let mut db=Db::open(&file).unwrap();db.create_project("APP","Example").unwrap();
  db.set_project_review_mode("APP",Mode::Custom,Options{shared_lanes:Some(false),full_backstop:Some(Backstop{enabled:true,landings:1,delay_seconds:1}),audit_percent:Some(0),..Default::default()},0,&Actor::Human).unwrap();
  crate::verification_backstop::tick(&mut db,&crate::Config::default(),&root,"APP","main").unwrap();
  let before=crate::verification_backstop::health(&db,&root).unwrap().unwrap();assert_eq!(before.status,"green");
  let count:i64=db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get(0)).unwrap();assert_eq!(count,1,"direct backstop still takes memory-admitted machine slot");
  db.set_project_review_mode("APP",Mode::Yolo,Options::default(),1,&Actor::Human).unwrap();
  std::fs::write(root.join("readme.txt"),"changed").unwrap();g(&root,&["add","readme.txt"]);g(&root,&["commit","-m","next"]);
  crate::verification_backstop::tick(&mut db,&crate::Config::default(),&root,"APP","main").unwrap();
  assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get::<_,i64>(0)).unwrap(),count);
  assert_eq!(crate::verification_backstop::health(&db,&root).unwrap().unwrap().verified_head,before.verified_head,"turning checks off does not claim the new head passed");
  assert!(crate::verification_backstop::release_ready(&db,&root,"HEAD").is_err(),"YOLO never exempts release verification");
  drop(db);std::fs::remove_dir_all(root).ok();std::fs::remove_file(file).ok();
 }
}
