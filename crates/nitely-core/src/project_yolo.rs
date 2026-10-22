//! Custom mode with automatic tests and PR review both off: the gate and the review are skipped
//! and recorded as skipped. The legacy YOLO switch and its table are gone (NIT-499); only the
//! person chooses this, in Custom. Manual checks still run.
use crate::{Db, issues::Actor, playbook::{self, GateResult}};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub const NOTE: &str = "Tests and review off (Custom mode): automatic gate and PR review skipped";

/// Whether the issue's project skips both the gate and the review.
pub(crate) fn enabled_for_issue(db: &Db, key: &str) -> Result<bool> {
    let o = crate::project_modes::for_issue(db, key)?.map(|s| s.options()).unwrap_or_default();
    Ok(o.test_depth == Some(crate::project_modes::TestDepth::Off) && o.pre_review == Some(crate::project_modes::PreReview::Off))
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
pub struct ReviewGuard {
    pub database: PathBuf,
    pub project: String,
    pub authority: Option<String>,
    /// It guards a goal item's review (NIT-563): the mode's item review decides, not its review of
    /// changes with no goal.
    pub item: bool,
}

/// Whether the mode runs this review: a goal item's when its item review is on, any other change's
/// unless its pre-merge review is off.
pub(crate) fn review_on(pre_review: Option<crate::project_modes::PreReview>, item_review: Option<crate::project_modes::ItemReview>, item: bool) -> bool {
    if item {
        item_review.is_some_and(|r| r != crate::project_modes::ItemReview::Off)
    } else {
        pre_review != Some(crate::project_modes::PreReview::Off)
    }
}
impl ReviewGuard {
    pub(crate) fn for_issue(db: &Db, key: &str) -> Result<Option<Self>> {
        let Some(project) = db.issue(key)?.and_then(|i| i.project) else { return Ok(None) };
        let database = db.conn.path().context("review needs the project's persistent settings database")?;
        Ok(Some(Self { database: database.into(), authority: db.project_review_mode(&project)?.authority()?, project, item: false }))
    }
    pub(crate) fn enabled_override(&self)->Result<Option<bool>> {
        let conn=Connection::open_with_flags(&self.database,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let o = crate::project_modes::read(&conn,&self.project)?.options();
        if self.item { return Ok(Some(review_on(o.pre_review, o.item_review, true))); }
        Ok(o.pre_review.map(|r|r!=crate::project_modes::PreReview::Off))
    }
    pub(crate) fn require_review(&self) -> Result<()> {
        let conn = Connection::open_with_flags(&self.database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mode=crate::project_modes::read(&conn,&self.project)?;
        if self.item {
            if !review_on(None, mode.options().item_review, true) {bail!(crate::review::Disabled);}
        } else if mode.options().pre_review==Some(crate::project_modes::PreReview::Off) {
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
    use crate::project_modes::{Mode, Options, PreReview, TestDepth};

    #[test]
    fn review_guard_reloads_authority_before_each_launch() {
        let dir = crate::paths::unique_temp("nitely-checks-off-review", "");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let mut db = Db::open(&path).unwrap();
        db.create_project("APP", "App").unwrap();
        let guard = ReviewGuard { database: path, project: "APP".into(), authority: None, item: false };
        // NIT-563: an item review runs on the mode's item review, whatever its pre-merge review.
        use crate::project_modes::{ItemReview, PreReview};
        assert!(review_on(Some(PreReview::Off), Some(ItemReview::Blocking), true));
        assert!(!review_on(Some(PreReview::Individual), Some(ItemReview::Off), true));
        assert!(!review_on(Some(PreReview::Off), Some(ItemReview::Blocking), false));
        guard.require_review().unwrap();
        let off = Options { test_depth: Some(TestDepth::Off), pre_review: Some(PreReview::Off), ..Default::default() };
        db.set_project_review_mode("APP", Mode::Custom, off, 0, &Actor::Human).unwrap();
        assert!(guard.require_review().unwrap_err().is::<SkipReview>());
        db.set_project_review_mode("APP", Mode::Release, Options::default(), 1, &Actor::Human).unwrap();
        assert!(guard.require_review().is_err(), "a queued reviewer cannot retain stale mode authority");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
