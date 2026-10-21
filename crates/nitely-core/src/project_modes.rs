//! Human-owned verification presets: the one place policy lives (docs/verification-settings.md).
//! The playbook keeps facts about the repo (commands, the test map, protected paths); a playbook
//! policy key the mode decides is ignored, with a notice. Approval rules stay authoritative.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::{Db, Actor, playbook::{Playbook, GateResult, LandBatch}, review::BatchMode};

/// The retired names read as their closest preset, for anything stored before the migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum Mode { #[serde(alias="yolo")] Fast, #[serde(alias="balanced")] Careful, #[serde(alias="thorough")] Release, Custom }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum TestDepth { Off, Basic, Risk, Full }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum PreReview { Off, Individual, Batched }
/// Review of an item landing into its goal branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="lowercase")]
pub enum ItemReview { Off, Advisory, Blocking }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backstop { pub enabled:bool, pub landings:u32, pub delay_seconds:i64 }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Periodic { pub enabled:bool, pub every_landings:u32, pub max_delay_seconds:i64, pub reviewer:String, pub findings_strategy:String }
impl Default for Periodic {
 fn default()->Self {Self {enabled:false,every_landings:5,max_delay_seconds:1800,reviewer:"configured".into(),findings_strategy:"reopen_or_create".into()}}
}
/// When a goal branch lands into main: when the coordinator calls a coherent slice, else after
/// `items` item lands, else once a day when `daily`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint { pub items:u32, pub daily:bool }
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default,deny_unknown_fields)]
pub struct Options {
 pub test_depth:Option<TestDepth>, pub pre_review:Option<PreReview>, pub land_batch:Option<bool>, pub shared_lanes:Option<bool>,
 pub full_backstop:Option<Backstop>, pub audit_percent:Option<u8>, pub periodic:Option<Periodic>,
 pub goal_branches:Option<bool>, pub item_review:Option<ItemReview>, pub checkpoint:Option<Checkpoint>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Preset {pub mode:Mode,pub options:Options}
#[derive(Debug, Clone, Serialize)]
pub struct ProjectReviewMode {
 pub project:String,pub mode:Mode,pub revision:i64,pub overrides:Options,pub presets:Vec<Preset>,
 /// False until a human explicitly saves modes; migration does not invalidate existing reviews.
 pub explicit:bool,
 /// The retired mode the migration moved this project from, until the person saves a mode.
 pub migrated_from:Option<String>,
 /// What that move changed, in the person's words.
 pub migration_changes:Vec<String>,
}
pub const PRESETS:[Mode;3]=[Mode::Fast,Mode::Careful,Mode::Release];
fn presets()->Vec<Preset> {PRESETS.into_iter().map(|mode|Preset {mode,options:preset(mode)}).collect()}
/// Every value each preset decides. Every preset keeps the backstop on; only Custom turns it off.
pub fn preset(mode:Mode)->Options {
 if mode==Mode::Custom {return Options::default();}
 let release=mode==Mode::Release;
 Options {
  test_depth:Some(if release {TestDepth::Full}else{TestDepth::Risk}),
  // Fast reviews each no-goal arrival batch as one diff; Careful and Release review each change.
  pre_review:Some(if mode==Mode::Fast {PreReview::Batched}else{PreReview::Individual}),
  land_batch:Some(!release),shared_lanes:Some(true),
  full_backstop:Some(Backstop{enabled:true,landings:if release {1}else{5},delay_seconds:1800}),
  audit_percent:Some(if release {0}else{10}),
  periodic:Some(Periodic::default()),
  goal_branches:Some(!release),
  item_review:Some(match mode {Mode::Fast=>ItemReview::Off,Mode::Careful=>ItemReview::Advisory,_=>ItemReview::Blocking}),
  checkpoint:(!release).then(||Checkpoint{items:if mode==Mode::Fast {10}else{5},daily:true}),
 }
}
/// What moving from a retired mode to its preset changed.
fn migration_changes(from:&str)->Vec<String> {
 let lines:&[&str]=match from {
  "yolo"=>&["YOLO is now Fast.",
   "Automatic checks run again: compile, lint and the tests a change affects. Protected or unmapped changes run the full gate.",
   "Changes with no goal are reviewed before they land, one review per batch.",
   "The full suite runs on main every 5 lands or 30 minutes, with auto-revert.",
   "10% of reduced checks are audited.",
   "Goals land through a goal branch, with one full run and one review at each checkpoint."],
  "fast"=>&["Fast now keeps its safety nets on.",
   "Basic-only checks became compile, lint and the tests a change affects. Protected or unmapped changes run the full gate.",
   "Changes with no goal are reviewed before they land, one review per batch.",
   "Changes with no goal land in batches of up to 5.",
   "The full suite runs on main every 5 lands or 30 minutes, with auto-revert.",
   "10% of reduced checks are audited.",
   "Periodic review of main is off: the checkpoint review covers it."],
  "balanced"=>&["Balanced is now Careful.",
   "Changes with no goal are reviewed one by one instead of in batches.",
   "Goals land through a goal branch: each item is reviewed without blocking, and each checkpoint gets one blocking review.",
   "Periodic review of main is off: the checkpoint review covers it."],
  "thorough"=>&["Thorough is now Release.",
   "The full suite runs on main after every land, with auto-revert."],
  _=>&[],
 };
 lines.iter().map(|l|l.to_string()).collect()
}
impl ProjectReviewMode {
 pub fn options(&self)->Options {if self.mode==Mode::Custom {self.overrides.clone()}else{preset(self.mode)}}
 pub fn effective_options(&self,pb:&Playbook)->Options {
  let o=self.options();
  Options {test_depth:o.test_depth.or(Some(if pb.verification.mode=="active" {TestDepth::Risk}else{TestDepth::Full})),
   pre_review:o.pre_review.or(Some(if !pb.review.enabled {PreReview::Off}else if matches!(pb.review.batch,BatchMode::All|BatchMode::Small) {PreReview::Batched}else{PreReview::Individual})),
   land_batch:o.land_batch.or(Some(pb.land.batch!=LandBatch::Off)),shared_lanes:o.shared_lanes.or(Some(pb.lanes.is_some())),
   full_backstop:o.full_backstop.or(Some(Backstop{enabled:pb.verification.backstop.enabled,landings:pb.verification.backstop.landings,delay_seconds:pb.verification.backstop.delay_seconds})),
   audit_percent:o.audit_percent.or(Some(pb.verification.audit_percent)),periodic:Some(o.periodic.unwrap_or_default()),
   goal_branches:o.goal_branches.or(Some(false)),item_review:o.item_review.or(Some(ItemReview::Off)),checkpoint:o.checkpoint}
 }
 pub fn authority(&self)->Result<Option<String>> {
  if !self.explicit {return Ok(None);}
  Ok(Some(format!("{:x}",Sha256::digest(serde_json::to_vec(&(self.mode,self.revision,self.options()))?))))
 }
}
impl Options {
 /// Whether work checked under these options did at least what `now` asks before landing, so it
 /// may finish under `now` (NIT-487). Values that act only after landing (backstop, periodic
 /// review, batching, lanes, checkpoints) do not matter here. A value one side leaves to the
 /// playbook compares only with the same.
 pub fn covers(&self,now:&Options)->bool {
  fn at_least<T:Copy>(past:Option<T>,now:Option<T>,rank:fn(T)->u8)->bool {
   match (past,now) {(Some(p),Some(n))=>rank(p)>=rank(n),(None,None)=>true,_=>false}
  }
  at_least(self.test_depth,now.test_depth,|d|d as u8)
   && at_least(self.pre_review,now.pre_review,|r|match r {PreReview::Off=>0,PreReview::Batched=>1,PreReview::Individual=>2})
   && at_least(self.item_review,now.item_review,|r|r as u8)
   // Audits sample reduced checks; work checked in full has nothing to sample.
   && (self.test_depth==Some(TestDepth::Full) || at_least(self.audit_percent,now.audit_percent,|n|n))
 }
}
pub(crate) fn read(conn:&Connection,project:&str)->Result<ProjectReviewMode> {
 let row=conn.query_row("SELECT p.key,COALESCE(m.mode,'custom'),COALESCE(m.overrides,'{}'),COALESCE(m.revision,0),COALESCE(m.explicit,0),m.migrated_from FROM project p LEFT JOIN project_review_mode m ON m.project=p.key WHERE p.key=?1 COLLATE NOCASE",[project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,bool>(4)?,r.get::<_,Option<String>>(5)?))).optional()?.with_context(||format!("no project {project}"))?;
 let migration_changes=row.5.as_deref().map(migration_changes).unwrap_or_default();
 Ok(ProjectReviewMode{project:row.0,mode:serde_json::from_value(serde_json::Value::String(row.1))?,overrides:serde_json::from_str(&row.2)?,revision:row.3,explicit:row.4,presets:presets(),migrated_from:row.5,migration_changes})
}
/// Keeps a policy's options under its authority, for `covers_authority`.
fn remember(conn:&Connection,s:&ProjectReviewMode)->Result<()> {
 if let Some(authority)=s.authority()? {
  conn.execute("INSERT OR IGNORE INTO project_review_mode_authority(authority,project,options,recorded_at) VALUES(?1,?2,?3,?4)",params![authority,s.project,serde_json::to_string(&s.options())?,crate::db::now()])?;
 }
 Ok(())
}
/// Whether work checked under the `past` authority may still land under the project's current
/// mode: the same policy, or one no stricter than it was checked under (NIT-487).
pub(crate) fn covers_authority(conn:&Connection,project:&str,past:&Option<String>)->Result<bool> {
 let now=read(conn,project)?;
 let current=now.authority()?;
 if &current==past {return Ok(true);}
 let (Some(past),Some(_))=(past,current) else {return Ok(false)};
 let options:Option<String>=conn.query_row("SELECT options FROM project_review_mode_authority WHERE authority=?1 AND project=?2",params![past,now.project],|r|r.get(0)).optional()?;
 Ok(options.map(|o|serde_json::from_str::<Options>(&o)).transpose()?.is_some_and(|o|o.covers(&now.options())))
}
pub fn covers_issue(db:&Db,key:&str,past:&Option<String>)->Result<bool> {
 match db.issue(key)?.context("mode issue disappeared")?.project {
  Some(project)=>covers_authority(&db.conn,&project,past),
  None=>Ok(past.is_none()),
 }
}
fn validate(o:&Options)->Result<()> {
 if o.audit_percent.is_some_and(|n|n!=0 && !(10..=100).contains(&n)) {bail!("audit percent must be zero (off), or between 10 and 100");}
 if let Some(b)=&o.full_backstop {if !(1..=1000).contains(&b.landings)||!(1..=86400).contains(&b.delay_seconds) {bail!("Full backstop needs 1–1000 landings and 1–86400 seconds");}}
 if let Some(p)=&o.periodic {
  if !(1..=1000).contains(&p.every_landings)||!(0..=604800).contains(&p.max_delay_seconds) {bail!("periodic review needs 1–1000 landings and 1–604800 seconds");}
  if !matches!(p.reviewer.as_str(),"configured"|"codex"|"claude"|"cursor") {bail!("unsupported periodic reviewer");}
  if !matches!(p.findings_strategy.as_str(),"reopen_or_create"|"create") {bail!("unsupported findings strategy");}
 }
 if o.checkpoint.as_ref().is_some_and(|c|!(1..=100).contains(&c.items)) {bail!("a checkpoint needs 1–100 items");}
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
  if !before.explicit||before.mode!=mode||before.overrides!=overrides||before.migrated_from.is_some() {
   remember(&tx,&before)?;
   let at=crate::db::now();let mode_text=serde_json::to_value(mode)?.as_str().context("mode name")?.to_owned();
   tx.execute("INSERT INTO project_review_mode(project,mode,overrides,revision,explicit,updated_at,updated_by,migrated_from) VALUES(?1,?2,?3,?4,1,?5,?6,NULL) ON CONFLICT(project) DO UPDATE SET mode=excluded.mode,overrides=excluded.overrides,revision=excluded.revision,explicit=1,updated_at=excluded.updated_at,updated_by=excluded.updated_by,migrated_from=NULL",params![before.project,mode_text,serde_json::to_string(&overrides)?,before.revision+1,at,actor.id()])?;
   tx.execute("INSERT INTO event(ts,actor,type,summary,payload) VALUES(?1,?2,'project_review_mode_changed',?3,?4)",params![at,actor.id(),format!("{} review mode: {mode_text}",before.project),serde_json::json!({"project":before.project,"before":before,"mode":mode,"overrides":overrides}).to_string()])?;
  }
  let after=read(&tx,project)?;remember(&tx,&after)?;tx.commit()?;Ok(after)
 }
}
pub fn for_issue(db:&Db,key:&str)->Result<Option<ProjectReviewMode>> {
 db.issue(key)?.context("mode issue disappeared")?.project.as_deref().map(|p|db.project_review_mode(p)).transpose()
}
pub fn authority_for_issue(db:&Db,key:&str)->Result<Option<String>> {for_issue(db,key)?.map(|s|s.authority()).transpose().map(Option::flatten)}
pub fn tests_off(db:&Db,key:&str)->Result<bool> {Ok(for_issue(db,key)?.is_some_and(|s|s.options().test_depth==Some(TestDepth::Off)))}
pub fn review_off(db:&Db,key:&str)->Result<bool> {Ok(for_issue(db,key)?.is_some_and(|s|s.options().pre_review==Some(PreReview::Off)))}
/// Writes the mode's decisions over the playbook's policy keys.
fn apply_options(o:&Options,pb:&mut Playbook) {
 if let Some(r)=o.pre_review {pb.review.enabled=r!=PreReview::Off;pb.review.batch=if r==PreReview::Batched {BatchMode::All}else{BatchMode::Off};}
 if let Some(batch)=o.land_batch {pb.land.batch=if batch {LandBatch::Waiting}else{LandBatch::Off};}
 if let Some(shared)=o.shared_lanes {if shared {if pb.lanes.is_none() {pb.lanes=Some(Default::default());}}else{pb.lanes=None;}}
 if let Some(b)=&o.full_backstop {pb.verification.backstop.enabled=b.enabled;pb.verification.backstop.landings=b.landings;pb.verification.backstop.delay_seconds=b.delay_seconds;}
 if let Some(n)=o.audit_percent {pb.verification.audit_percent=n;}
}
pub fn apply_project(db:&Db,project:&str,pb:&mut Playbook)->Result<()> {
 let settings=db.project_review_mode(project)?;pb.mode_authority=settings.authority()?;pb.mode_project=Some(settings.project.clone());
 apply_options(&settings.options(),pb);
 Ok(())
}
/// The playbook's policy keys the mode overrules: each one set away from its default and
/// replaced by a different value from the mode.
pub fn ignored_policy(o:&Options,pb:&Playbook)->Vec<String> {
 let default=Playbook::default();let mut applied=pb.clone();apply_options(o,&mut applied);
 let (b,d,a)=(&pb.verification.backstop,&default.verification.backstop,&applied.verification.backstop);
 let depth_mode=o.test_depth.map(|t|if matches!(t,TestDepth::Basic|TestDepth::Risk) {"active"}else{"shadow"});
 [("[review] enabled",pb.review.enabled!=default.review.enabled,pb.review.enabled!=applied.review.enabled),
  ("[review] batch",pb.review.batch!=default.review.batch,pb.review.batch!=applied.review.batch),
  ("[land] batch",pb.land.batch!=default.land.batch,pb.land.batch!=applied.land.batch),
  ("[verification] mode",!pb.verification.mode.is_empty(),depth_mode.is_some_and(|m|m!=pb.verification.mode)),
  ("[verification] audit_percent",pb.verification.audit_percent!=default.verification.audit_percent,pb.verification.audit_percent!=applied.verification.audit_percent),
  ("[verification.backstop] enabled, landings and delay_seconds",(b.enabled,b.landings,b.delay_seconds)!=(d.enabled,d.landings,d.delay_seconds),(b.enabled,b.landings,b.delay_seconds)!=(a.enabled,a.landings,a.delay_seconds))]
  .into_iter().filter(|(_,set,overruled)|*set&&*overruled)
  .map(|(key,_,_)|format!("The playbook's {key} is ignored: the review mode decides it."))
  .collect()
}
pub fn apply_issue(db:&Db,key:&str,pb:&mut Playbook)->Result<()> {
 if let Some(s)=for_issue(db,key)? {apply_project(db,&s.project,pb)?;} Ok(())
}
/// A change of mode fails a checked gate only when the new mode asks for more than it checked.
pub fn check_gate_authority(db:&Db,keys:&[&str],gate:&GateResult)->Result<()> {
 for key in keys {
  if !covers_issue(db,key,&gate.mode_authority)? {bail!("project review mode changed; submit again with current settings");}
  if gate.skipped.is_some()&&!tests_off(db,key)? {bail!("automatic tests are now required; submit again");}
 }Ok(())
}
pub fn check_playbook_authority(db:&Db,keys:&[&str],pb:&Playbook)->Result<()> {
 if let Some(project)=&pb.mode_project {if !covers_authority(&db.conn,project,&pb.mode_authority)? {bail!("project review mode changed while queued; retry with current settings");}}
 for key in keys {if !covers_issue(db,key,&pb.mode_authority)? {bail!("project review mode changed while queued; submit again");}}Ok(())
}

#[derive(Debug,Clone,Serialize)]
pub struct EffectiveRepository {pub repo:String,pub options:Options,pub warnings:Vec<String>,
 /// Nothing runs the full suite on main: only Custom can choose this, and the app shows it in red.
 pub backstop_off:bool}
pub fn effective_repositories(db:&Db,config:&crate::Config,project:&str)->Result<Vec<EffectiveRepository>> {
 let settings=db.project_review_mode(project)?;
 config.repos.iter().filter(|r|r.project.as_deref().is_some_and(|p|p.eq_ignore_ascii_case(project))).map(|repo| {
  let root=std::path::Path::new(&repo.root);
  // Match execution's repository/stored policy and app defaults. Reduced checks separately
  // require commands from the committed integration policy, never a candidate's additions.
  let (pb,mut warnings)=match Playbook::load_with(root,config) {Ok(pb)=>{let w=pb.unknown_warnings();(pb,w)},Err(e)=>(Playbook::default(),vec![format!("Repository settings unavailable: {e:#}; displayed defaults are not verified.")])};
  warnings.extend(ignored_policy(&settings.options(),&pb));
  let options=settings.effective_options(&pb);
  if matches!(options.test_depth,Some(TestDepth::Basic|TestDepth::Risk)) {
   let committed=crate::playbook::git(root,&["show",&format!("refs/heads/{}:{}",repo.integration_branch,crate::playbook::PLAYBOOK_PATH)]).and_then(|s|Playbook::parse(&s));
   if committed.as_ref().map_or(true,|p|p.verification.basic.is_empty()) {warnings.push("Basic commands are not configured in committed integration policy; automatic verification falls back to Full.".into());}
  }
  if options.test_depth==Some(TestDepth::Risk) {warnings.push("Risk-based verification falls back to Full for missing mappings, protected changes or an unhealthy enabled backstop.".into());}
  if options.full_backstop.as_ref().is_some_and(|b|!b.enabled) {warnings.push("The full suite on main is off: nothing checks main after reduced checks land.".into());}
  let backstop_off=options.full_backstop.as_ref().is_some_and(|b|!b.enabled);
  Ok(EffectiveRepository{repo:repo.name.clone(),options,warnings,backstop_off})
 }).collect()
}

#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn retired_modes_migrate_to_the_closest_preset_and_say_what_changed() {
  let dir=crate::paths::unique_temp("nitely-modes","");std::fs::create_dir_all(&dir).unwrap();
  let db=Db::open(&dir.join("state.db")).unwrap();
  for (key,name) in [("APP","Example"),("WEB","Website"),("OPS","Operations"),("DOC","Docs")] {db.create_project(key,name).unwrap();}
  // Reopen the schema from before the presets, holding each retired mode.
  db.conn.execute_batch("DROP TABLE project_review_mode;DROP TABLE project_review_mode_authority;
   DELETE FROM schema_migration WHERE name='20261021-review-presets';
   CREATE TABLE project_review_mode(project TEXT PRIMARY KEY REFERENCES project(key),mode TEXT NOT NULL CHECK(mode IN ('yolo','fast','balanced','thorough','custom')),overrides TEXT NOT NULL DEFAULT '{}',revision INTEGER NOT NULL DEFAULT 0,explicit INTEGER NOT NULL DEFAULT 0,updated_at INTEGER NOT NULL DEFAULT 0,updated_by TEXT NOT NULL DEFAULT 'migration');
   INSERT INTO project_review_mode(project,mode,revision,explicit) VALUES('APP','yolo',3,0),('WEB','balanced',2,1),('OPS','thorough',1,1),('DOC','custom',0,0);").unwrap();
  drop(db);let mut db=Db::open(&dir.join("state.db")).unwrap();
  let app=db.project_review_mode("APP").unwrap();
  assert_eq!((app.mode,app.revision,app.migrated_from.as_deref()),(Mode::Fast,3,Some("yolo")));assert_eq!(app.authority().unwrap(),None);
  assert!(app.migration_changes.iter().any(|c|c.contains("Automatic checks run again")));
  let web=db.project_review_mode("WEB").unwrap();assert_eq!(web.mode,Mode::Careful);assert!(web.migration_changes[0].contains("Careful"));
  assert_eq!(db.project_review_mode("OPS").unwrap().mode,Mode::Release);
  let doc=db.project_review_mode("DOC").unwrap();assert_eq!((doc.mode,doc.options()),(Mode::Custom,Options::default()));assert!(doc.migration_changes.is_empty());
  assert!(db.set_project_review_mode("APP",Mode::Release,Options::default(),3,&Actor::Agent("cli".into())).is_err());
  let changed=db.set_project_review_mode("app",Mode::Release,Options::default(),3,&Actor::Human).unwrap();
  assert_eq!(changed.revision,4);assert!(changed.authority().unwrap().is_some());assert!(changed.migrated_from.is_none()&&changed.migration_changes.is_empty());
  assert!(db.set_project_review_mode("APP",Mode::Fast,Options::default(),3,&Actor::Human).is_err());
  // Every preset keeps the backstop on and the person's approval rules; Release lands alone.
  let mut pb=Playbook::default();pb.approval.always_human=true;
  apply_project(&db,"APP",&mut pb).unwrap();
  assert!(pb.approval.always_human&&pb.review.enabled);assert_eq!((pb.review.batch,pb.land.batch),(BatchMode::Off,LandBatch::Off));
  assert!(pb.verification.backstop.enabled);assert_eq!(pb.verification.backstop.landings,1);
  db.set_project_review_mode("APP",Mode::Fast,Options::default(),4,&Actor::Human).unwrap();
  apply_project(&db,"APP",&mut pb).unwrap();
  assert!(pb.review.enabled);assert_eq!((pb.review.batch,pb.land.batch),(BatchMode::All,LandBatch::Waiting));
  assert!(pb.verification.backstop.enabled);assert_eq!(pb.verification.audit_percent,10);assert!(pb.approval.always_human);
  drop(db);std::fs::remove_dir_all(dir).ok();
 }
 #[test]
 fn playbook_policy_keys_the_mode_decides_are_ignored_with_a_notice() {
  let pb=Playbook::parse("[review]\nenabled=false\n[land]\nbatch='waiting'\n[verification]\naudit_percent=0\n[verification.backstop]\nlandings=2\n").unwrap();
  let notices=ignored_policy(&preset(Mode::Fast),&pb);
  for key in ["[review] enabled","[verification] audit_percent","[verification.backstop]"] {assert!(notices.iter().any(|n|n.contains(key)),"{key}: {notices:?}");}
  assert!(!notices.iter().any(|n|n.contains("[land] batch")),"a key the mode agrees with is not overruled");
  assert!(ignored_policy(&Options::default(),&pb).is_empty(),"Custom with nothing set leaves the playbook in charge");
 }
 #[test]
 fn queued_work_finishes_under_a_looser_mode_but_not_a_stricter_one() {
  let file=crate::paths::unique_temp("mode-looser",".sqlite");let mut db=Db::open(&file).unwrap();db.create_project("APP","Example").unwrap();
  let issue=db.create_issue(&crate::NewIssue{project:"APP".into(),title:"Queued change".into(),body:"Checked under one mode, landing under another".into(),evidence:"none".into(),source:"codex".into(),kind:"feature".into(),..Default::default()},&Actor::Human).unwrap();
  let gate=|authority:Option<String>|GateResult{mode_authority:authority,passed:true,plan:None,skipped:None,commit:"abc".into(),results:vec![],warnings:vec![],flaked:vec![],lane_run:None};
  let release=db.set_project_review_mode("APP",Mode::Release,Options::default(),0,&Actor::Human).unwrap();
  let checked=gate(release.authority().unwrap());
  let fast=db.set_project_review_mode("APP",Mode::Fast,Options::default(),1,&Actor::Human).unwrap();
  check_gate_authority(&db,&[&issue.key],&checked).expect("a full gate and a review cover Fast");
  let mut pb=Playbook::default();pb.mode_project=Some("APP".into());pb.mode_authority=checked.mode_authority.clone();
  check_playbook_authority(&db,&[&issue.key],&pb).expect("work queued under Release lands under Fast");
  db.set_project_review_mode("APP",Mode::Release,Options::default(),2,&Actor::Human).unwrap();
  assert!(check_gate_authority(&db,&[&issue.key],&gate(fast.authority().unwrap())).is_err(),"Release asks for more than Fast checked");
  assert!(check_gate_authority(&db,&[&issue.key],&gate(None)).is_err(),"an unknown policy is never assumed to cover");
  drop(db);std::fs::remove_file(file).ok();
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
  check_gate_authority(&db,&[&issue.key],&gate).expect("tests off asks for less than Basic checked");
  let skipped=crate::playbook::run_automatic_gate(&db,&root,&root,&[&issue.key],&Actor::Human,crate::lanes::Kind::Gate).unwrap();
  assert!(!skipped.passed);assert!(skipped.results.is_empty());assert!(!skipped.skipped.as_ref().unwrap().contains("YOLO"));assert!(!review_off(&db,&issue.key).unwrap());
  db.set_project_review_mode("APP",Mode::Release,Options::default(),2,&Actor::Human).unwrap();assert!(check_gate_authority(&db,&[&issue.key],&skipped).is_err());
  g(&root,&["checkout","main"]);std::fs::write(root.join(crate::playbook::PLAYBOOK_PATH),"[gate]\ncommands=['exit 1']\nretries=0\n").unwrap();g(&root,&["add",crate::playbook::PLAYBOOK_PATH]);g(&root,&["commit","-m","remove basic commands"]);
  db.set_project_review_mode("APP",Mode::Custom,Options{test_depth:Some(TestDepth::Basic),shared_lanes:Some(false),..Default::default()},3,&Actor::Human).unwrap();
  let fallback=crate::playbook::run_automatic_gate(&db,&root,&root,&[&issue.key],&Actor::Human,crate::lanes::Kind::Gate).unwrap();
  assert!(!fallback.passed && fallback.plan.is_none(),"unconfigured Basic must execute the failing Full gate");
  drop(db);std::fs::remove_dir_all(root).ok();std::fs::remove_file(file).ok();
 }
 #[test]
 fn full_backstop_runs_without_shared_lanes_and_only_custom_stops_it() {
  use crate::test_repo::git as g;
  let root=crate::test_repo::copy("mode-backstop",|p|crate::test_repo::init(p,&[(crate::playbook::PLAYBOOK_PATH,"[gate]\ncommands=['echo full-checked']\n"),("readme.txt","base")]));
  let file=crate::paths::unique_temp("mode-backstop-db",".sqlite");let mut db=Db::open(&file).unwrap();db.create_project("APP","Example").unwrap();
  db.set_project_review_mode("APP",Mode::Custom,Options{shared_lanes:Some(false),full_backstop:Some(Backstop{enabled:true,landings:1,delay_seconds:1}),audit_percent:Some(0),..Default::default()},0,&Actor::Human).unwrap();
  crate::verification_backstop::tick(&mut db,&crate::Config::default(),&root,"APP","main").unwrap();
  let before=crate::verification_backstop::health(&db,&root).unwrap().unwrap();assert_eq!(before.status,"green");
  let count:i64=db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get(0)).unwrap();assert_eq!(count,1,"direct backstop still takes memory-admitted machine slot");
  db.set_project_review_mode("APP",Mode::Custom,Options{shared_lanes:Some(false),full_backstop:Some(Backstop{enabled:false,landings:1,delay_seconds:1}),..Default::default()},1,&Actor::Human).unwrap();
  std::fs::write(root.join("readme.txt"),"changed").unwrap();g(&root,&["add","readme.txt"]);g(&root,&["commit","-m","next"]);
  crate::verification_backstop::tick(&mut db,&crate::Config::default(),&root,"APP","main").unwrap();
  assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM lane_run",[],|r|r.get::<_,i64>(0)).unwrap(),count);
  assert_eq!(crate::verification_backstop::health(&db,&root).unwrap().unwrap().verified_head,before.verified_head,"turning checks off does not claim the new head passed");
  assert!(crate::verification_backstop::release_ready(&db,&root,"HEAD").is_err(),"a backstop turned off never exempts release verification");
  drop(db);std::fs::remove_dir_all(root).ok();std::fs::remove_file(file).ok();
 }
}
