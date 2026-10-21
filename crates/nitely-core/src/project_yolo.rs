//! Human-controlled project bypass for automatic verification. Manual checks still run.
use crate::{Db, issues::Actor, playbook::{self, GateResult}};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const NOTE: &str = "YOLO mode: automatic gate and PR review skipped";

#[derive(Debug, Clone, Serialize)]
pub struct ProjectYolo {
    pub project: String,
    pub enabled: bool,
    pub revision: i64,
}

fn read(conn: &Connection, project: &str) -> Result<ProjectYolo> {
    let s=crate::project_modes::read(conn,project)?;
    let o=s.options();
    Ok(ProjectYolo{project:s.project,enabled:o.test_depth==Some(crate::project_modes::TestDepth::Off)&&o.pre_review==Some(crate::project_modes::PreReview::Off),revision:s.revision})
}
impl Db {
    pub fn project_yolo(&self,project:&str)->Result<ProjectYolo> {read(&self.conn,project)}
    pub fn set_project_yolo(&mut self,project:&str,enabled:bool,expected_revision:i64,actor:&Actor)->Result<ProjectYolo> {
        let before=self.project_yolo(project)?;
        // YOLO is retired (NIT-491): the switch sets Custom with tests and review off.
        let overrides=if enabled {crate::project_modes::Options{test_depth:Some(crate::project_modes::TestDepth::Off),pre_review:Some(crate::project_modes::PreReview::Off),..Default::default()}}else{Default::default()};
        self.set_project_review_mode(project,crate::project_modes::Mode::Custom,overrides,expected_revision,actor)?;
        if before.enabled!=enabled {self.log_event(actor,None,None,"project_yolo_changed",&format!("{project} YOLO mode {}",if enabled {"enabled"}else{"disabled"}))?;}
        self.project_yolo(project)
    }
}

pub(crate) fn enabled_for_issue(db: &Db, key: &str) -> Result<bool> {
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    issue.project.as_deref().map(|p| db.project_yolo(p).map(|s| s.enabled)).unwrap_or(Ok(false))
}

/// Every member of a stack must opt in; another project's settings never skip its checks.
pub(crate) fn all_enabled(db: &Db, keys: &[&str]) -> Result<bool> {
    if keys.is_empty() { return Ok(false); }
    for key in keys { if !crate::project_modes::tests_off(db, key)? { return Ok(false); } }
    Ok(true)
}

pub(crate) fn skipped_gate(db: &Db, worktree: &Path, keys: &[&str], actor: &Actor) -> Result<GateResult> {
    let commit = playbook::git(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    let note=if keys.iter().all(|k|enabled_for_issue(db,k).unwrap_or(false)) {NOTE}else{"Automatic tests disabled by project review mode"};
    for key in keys { db.log_event(actor, Some(key), Some(&worktree.to_string_lossy()), "gate_skipped", note)?; }
    let mode_authority=keys.first().map(|key|crate::project_modes::authority_for_issue(db,key)).transpose()?.flatten();
    Ok(GateResult { mode_authority, passed: false, plan: None, skipped: Some(note.into()), commit, results: vec![], warnings: vec![], flaked: vec![], lane_run: None })
}


#[derive(Debug, Clone)]
pub struct ReviewGuard { pub database: PathBuf, pub project: String, pub authority: Option<String> }
impl ReviewGuard {
    pub(crate) fn for_issue(db: &Db, key: &str) -> Result<Option<Self>> {
        let Some(project) = db.issue(key)?.and_then(|i| i.project) else { return Ok(None) };
        let database = db.conn.path().context("review needs the project's persistent settings database")?;
        Ok(Some(Self { database: database.into(), authority: db.project_review_mode(&project)?.authority()?, project }))
    }
    pub(crate) fn enabled_override(&self)->Result<Option<bool>> {
        let conn=Connection::open_with_flags(&self.database,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(crate::project_modes::read(&conn,&self.project)?.options().pre_review.map(|r|r!=crate::project_modes::PreReview::Off))
    }
    pub(crate) fn require_review(&self) -> Result<()> {
        let conn = Connection::open_with_flags(&self.database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mode=crate::project_modes::read(&conn,&self.project)?;
        if mode.options().pre_review==Some(crate::project_modes::PreReview::Off) {
            if mode.options().test_depth==Some(crate::project_modes::TestDepth::Off) {bail!(SkipReview);} else {bail!(crate::review::Disabled);}
        }
        if !crate::project_modes::covers_authority(&conn,&self.project,&self.authority)? {bail!("project review mode changed before reviewer launch; submit again");}
        Ok(())
    }
}

/// Checked while queued, before taking memory or a lane. Its row cannot become a cache hit.
pub(crate) fn check_queued_gate(db: &Db, id: i64, keys: &[&str]) -> Result<()> {
    if all_enabled(db, keys)? {
        db.conn.execute("UPDATE lane_run SET state = 'abandoned', ended_at = ?2, error = ?3, waiting_memory = NULL, waiting_for = NULL WHERE id = ?1 AND state = 'queued'", params![id, crate::db::now(), NOTE])?;
        bail!(SkipGate);
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct SkipGate;
impl std::fmt::Display for SkipGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(NOTE) }
}
impl std::error::Error for SkipGate {}

#[derive(Debug)]
pub struct SkipReview;
impl std::fmt::Display for SkipReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(NOTE) }
}
impl std::error::Error for SkipReview {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn yolo_is_default_off_human_only_revision_checked_and_project_scoped() {
        let dir = crate::paths::unique_temp("nitely-yolo", "");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Db::open(&dir.join("test.db")).unwrap();
        db.create_project("APP", "App").unwrap();
        db.create_project("WEB", "Web").unwrap();
        assert!(!db.project_yolo("APP").unwrap().enabled);
        assert!(db.set_project_yolo("APP", true, 0, &Actor::Agent("cli".into())).is_err());
        let enabled = db.set_project_yolo("app", true, 0, &Actor::Human).unwrap();
        assert!(enabled.enabled);
        assert_eq!(enabled.revision, 1);
        assert!(!db.project_yolo("WEB").unwrap().enabled);
        assert!(db.set_project_yolo("APP", false, 0, &Actor::Human).is_err());
        assert!(!db.set_project_yolo("APP", false, 1, &Actor::Human).unwrap().enabled);
        let events: i64 = db.conn.query_row("SELECT COUNT(*) FROM event WHERE type = 'project_yolo_changed' AND actor = 'human'", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 2);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn yolo_review_guard_reloads_authority_before_each_launch() {
        let dir = crate::paths::unique_temp("nitely-yolo-review", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let mut db = Db::open(&path).unwrap();
        db.create_project("APP", "App").unwrap();
        let guard = ReviewGuard { database: path, project: "APP".into(), authority: None };
        guard.require_review().unwrap();
        db.set_project_yolo("APP", true, 0, &Actor::Human).unwrap();
        assert!(guard.require_review().unwrap_err().is::<SkipReview>());
        db.set_project_yolo("APP", false, 1, &Actor::Human).unwrap();
        assert!(guard.require_review().is_err(), "a queued reviewer cannot retain stale mode authority");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
