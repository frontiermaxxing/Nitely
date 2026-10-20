//! Human-controlled project bypass for automatic verification. Manual checks still run.
use crate::{Db, issues::Actor, playbook::{self, GateResult}};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
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
    conn.query_row("SELECT p.key, COALESCE(y.enabled, 0), COALESCE(y.revision, 0) FROM project p LEFT JOIN project_yolo y ON y.project = p.key WHERE p.key = ?1 COLLATE NOCASE", [project], |r| Ok(ProjectYolo { project: r.get(0)?, enabled: r.get(1)?, revision: r.get(2)? }))
        .optional()?.with_context(|| format!("no project {project}"))
}

impl Db {
    pub fn project_yolo(&self, project: &str) -> Result<ProjectYolo> {
        let project = self.project(project)?.with_context(|| format!("no project {project}"))?.key;
        read(&self.conn, &project)
    }

    pub fn set_project_yolo(&mut self, project: &str, enabled: bool, expected_revision: i64, actor: &Actor) -> Result<ProjectYolo> {
        if *actor != Actor::Human { bail!("only the person changes YOLO mode, in project settings"); }
        let project = self.project(project)?.with_context(|| format!("no project {project}"))?.key;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before = read(&tx, &project)?;
        if before.revision != expected_revision { bail!("YOLO settings changed; reload them before saving"); }
        if before.enabled != enabled {
            let at = crate::db::now();
            tx.execute("INSERT INTO project_yolo (project, enabled, revision, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(project) DO UPDATE SET enabled = excluded.enabled, revision = excluded.revision, updated_at = excluded.updated_at, updated_by = excluded.updated_by", params![before.project, enabled, before.revision + 1, at, actor.id()])?;
            tx.execute("INSERT INTO event (ts, actor, type, summary, payload) VALUES (?1, ?2, 'project_yolo_changed', ?3, ?4)", params![at, actor.id(), format!("{} YOLO mode {}", before.project, if enabled { "enabled: automatic gates and PR review skipped" } else { "disabled: normal verification required" }), serde_json::json!({"project": before.project, "before": before, "enabled": enabled}).to_string()])?;
        }
        let after = read(&tx, &project)?;
        tx.commit()?;
        Ok(after)
    }
}

pub(crate) fn enabled_for_issue(db: &Db, key: &str) -> Result<bool> {
    let issue = db.issue(key)?.with_context(|| format!("no issue {key}"))?;
    issue.project.as_deref().map(|p| db.project_yolo(p).map(|s| s.enabled)).unwrap_or(Ok(false))
}

/// Every member of a stack must opt in; another project's settings never skip its checks.
pub(crate) fn all_enabled(db: &Db, keys: &[&str]) -> Result<bool> {
    if keys.is_empty() { return Ok(false); }
    for key in keys { if !enabled_for_issue(db, key)? { return Ok(false); } }
    Ok(true)
}

pub(crate) fn skipped_gate(db: &Db, worktree: &Path, keys: &[&str], actor: &Actor) -> Result<GateResult> {
    let commit = playbook::git(worktree, &["rev-parse", "HEAD"])?.trim().to_string();
    for key in keys { db.log_event(actor, Some(key), Some(&worktree.to_string_lossy()), "gate_skipped", NOTE)?; }
    Ok(GateResult { passed: false, skipped: Some(NOTE.into()), commit, results: vec![], warnings: vec![], flaked: vec![], lane_run: None })
}

pub(crate) fn check_gate_authority(db: &Db, keys: &[&str], gate: &GateResult) -> Result<()> {
    if gate.skipped.is_some() && !all_enabled(db, keys)? {
        bail!("YOLO mode was disabled; submit again with the normal gate and PR review before landing");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ReviewGuard { pub database: PathBuf, pub project: String }
impl ReviewGuard {
    pub(crate) fn for_issue(db: &Db, key: &str) -> Result<Option<Self>> {
        let Some(project) = db.issue(key)?.and_then(|i| i.project) else { return Ok(None) };
        let database = db.conn.path().context("review needs the project's persistent settings database")?;
        Ok(Some(Self { database: database.into(), project }))
    }
    pub(crate) fn require_review(&self) -> Result<()> {
        let conn = Connection::open_with_flags(&self.database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if read(&conn, &self.project)?.enabled { bail!(SkipReview); }
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
        let guard = ReviewGuard { database: path, project: "APP".into() };
        guard.require_review().unwrap();
        db.set_project_yolo("APP", true, 0, &Actor::Human).unwrap();
        assert!(guard.require_review().unwrap_err().is::<SkipReview>());
        db.set_project_yolo("APP", false, 1, &Actor::Human).unwrap();
        guard.require_review().unwrap();
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
