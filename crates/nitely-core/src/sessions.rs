//! Ending sessions that will not end themselves. A session ends when its agent's hooks say so
//! (Claude Code's SessionEnd), but a `codex exec` that exits, a terminal closed under an agent, or a
//! crash sends nothing, and the session would stay working or waiting on the person forever. So:
//! the crew supervisor ends a worker's sessions when the worker ends, removing a project ends the
//! sessions in its repos, and a sweep ends sessions that have been quiet a while and whose folder
//! no process uses any more (NIT-189).

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

use crate::db::Db;
use crate::paths::normalize;

/// How long a working or waiting session may go without an event before the sweep asks whether
/// anything still runs in its folder.
pub const GONE_AFTER_SECS: i64 = 30 * 60;

impl Db {
    /// Ends a session as of `at`, as its own session end would have: its state becomes ended from
    /// then, with `why` on its timeline. False when it is unknown or already ended. `at` is when it
    /// is known to have stopped (its last event, for one found long gone), so Activity does not
    /// draw it working or waiting up to the moment it was noticed.
    pub fn end_session(&self, id: &str, at: i64, why: &str) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let found: Option<(String, String, String, i64)> = tx
            .query_row("SELECT vendor, cwd, state, last_seen_at FROM session WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?;
        let Some((_vendor, cwd, state, last_seen)) = found else { return Ok(false) };
        if state == "ended" {
            return Ok(false);
        }
        let at = at.max(last_seen);
        tx.execute("INSERT INTO session_state (session_id, ts, state) VALUES (?1, ?2, 'ended')", params![id, at])?;
        tx.execute("UPDATE session SET state = 'ended', last_seen_at = ?2, last_activity = ?3 WHERE id = ?1", params![id, at, why])?;
        tx.execute("INSERT INTO event (ts, actor, cwd, type, summary) VALUES (?1, ?2, ?3, 'session_end', ?4)", params![at, id, cwd, why])?;
        tx.commit()?;
        Ok(true)
    }

    /// Ends the sessions not ended yet in `dir` or a folder inside it that started at `since` or
    /// later, and the session `named` (a crew worker's own id) wherever it is. Returns how many.
    pub fn end_sessions_in(&self, dir: &str, since: i64, named: Option<&str>, at: i64, why: &str) -> Result<usize> {
        let dir = normalize(dir);
        let open: Vec<(String, String, i64)> = {
            let mut stmt = self.conn.prepare("SELECT id, cwd, started_at FROM session WHERE state != 'ended'")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut ended = 0;
        for (id, cwd, started) in open {
            let cwd = normalize(&cwd);
            let inside = cwd == dir || cwd.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/'));
            if (inside && started >= since) || named == Some(id.as_str()) {
                ended += usize::from(self.end_session(&id, at, why)?);
            }
        }
        Ok(ended)
    }

    /// Removes the unanswered questions asked from `dir` or a folder inside it: nothing is left to
    /// answer them for (a removed project). Returns how many.
    pub fn remove_open_asks_in(&self, dir: &str) -> Result<usize> {
        let dir = normalize(dir);
        let open: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare("SELECT id, cwd FROM ask WHERE answered_at IS NULL AND cwd IS NOT NULL")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut removed = 0;
        for (id, cwd) in open {
            let cwd = normalize(&cwd);
            if cwd == dir || cwd.strip_prefix(dir.as_str()).is_some_and(|rest| rest.starts_with('/')) {
                removed += self.conn.execute("DELETE FROM ask WHERE id = ?1", [id])?;
            }
        }
        Ok(removed)
    }

    /// Sessions working or waiting with no event since `before`: id, folder and last event.
    fn quiet_sessions(&self, before: i64) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT id, cwd, last_seen_at FROM session WHERE state IN ('working', 'waiting') AND last_seen_at < ?1")?;
        let rows = stmt.query_map([before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Ends the sessions quiet for `GONE_AFTER_SECS` whose folder nothing runs in any more, as of
/// their last event; `runs_in` says whether a process uses a folder. Returns how many it ended.
pub fn end_gone(db: &Db, now: i64, runs_in: &dyn Fn(&str) -> bool) -> Result<usize> {
    let mut ended = 0;
    for (id, cwd, last_seen) in db.quiet_sessions(now - GONE_AFTER_SECS)? {
        if !runs_in(&cwd) {
            ended += usize::from(db.end_session(&id, last_seen, "Ended: no process runs in its folder any more")?);
        }
    }
    Ok(ended)
}

/// `end_gone` with this machine's processes: a folder that is gone, or that no process uses
/// (procs.rs), has nothing running in it. The processes are listed once, and only when a session
/// is quiet long enough to ask about; when they cannot be listed, nothing is ended.
pub fn end_gone_now(db: &Db) -> Result<usize> {
    let now = crate::db::now();
    let quiet: Vec<String> = db.quiet_sessions(now - GONE_AFTER_SECS)?.into_iter().map(|(_, cwd, _)| cwd).filter(|cwd| Path::new(cwd).is_dir()).collect();
    let mut in_use: HashMap<String, bool> = HashMap::new();
    if !quiet.is_empty() {
        let dirs: Vec<&Path> = quiet.iter().map(Path::new).collect();
        let (holders, whole) = crate::procs::holders_of(&dirs);
        if !whole {
            return Ok(0);
        }
        in_use = quiet.into_iter().zip(holders).map(|(cwd, h)| (cwd, !h.is_empty())).collect();
    }
    end_gone(db, now, &|cwd| in_use.get(cwd).copied().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewEvent;

    fn db() -> (Db, std::path::PathBuf) {
        let dir = crate::paths::unique_temp("nitely-sessions", "");
        std::fs::create_dir_all(&dir).unwrap();
        (Db::open(&dir.join("t.db")).unwrap(), dir)
    }

    fn seen(db: &mut Db, id: &str, cwd: &str, kind: &str, state: &str) {
        db.record(&NewEvent { session_id: id.into(), vendor: "codex".into(), cwd: cwd.into(), kind: kind.into(), summary: Some(kind.into()), state: state.into() }).unwrap();
    }

    fn state(db: &Db, id: &str) -> (String, i64) {
        db.conn.query_row("SELECT state, last_seen_at FROM session WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    }

    #[test]
    fn ends_a_session_once_as_of_when_it_stopped() {
        let (mut db, dir) = db();
        seen(&mut db, "codex:t1", r"C:\code\app-worktrees\app-1-x", "ask", "waiting");
        let (_, last) = state(&db, "codex:t1");
        assert!(db.end_session("codex:t1", last - 100, "Worker exited").unwrap());
        assert_eq!(state(&db, "codex:t1"), ("ended".to_string(), last), "never before its last event");
        assert!(!db.end_session("codex:t1", last + 10, "again").unwrap(), "already ended");
        assert!(!db.end_session("codex:nobody", last, "x").unwrap());
        let ended: i64 = db.conn.query_row("SELECT COUNT(*) FROM event WHERE actor = 'codex:t1' AND type = 'session_end'", [], |r| r.get(0)).unwrap();
        assert_eq!(ended, 1);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ends_the_sessions_in_a_folder_since_a_time_and_a_named_one() {
        let (mut db, dir) = db();
        seen(&mut db, "codex:in", r"C:\code\app-worktrees\app-1-x\src", "ask", "waiting");
        seen(&mut db, "codex:sibling", r"C:\code\app-worktrees\app-1-xy", "tool", "working");
        seen(&mut db, "codex:mine", r"C:\code\app", "status", "working");
        let (_, t) = state(&db, "codex:in");
        assert_eq!(db.end_sessions_in(r"C:/code/app-worktrees/app-1-x", t - 60, Some("codex:mine"), t, "Worker exited").unwrap(), 2);
        assert_eq!(state(&db, "codex:in").0, "ended");
        assert_eq!(state(&db, "codex:mine").0, "ended", "its own id, wherever it reported from");
        assert_eq!(state(&db, "codex:sibling").0, "working", "a folder that only starts with the name");
        // One that started before the worker did is someone else's.
        seen(&mut db, "codex:before", r"C:\code\app-worktrees\app-1-x", "tool", "working");
        assert_eq!(db.end_sessions_in(r"C:\code\app-worktrees\app-1-x", t + 3600, None, t, "x").unwrap(), 0);
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn removes_only_the_open_questions_asked_from_a_folder() {
        let (db, dir) = db();
        let agent = crate::Actor::Agent("codex:x".into());
        let inside = db.create_ask("May I write here?", &[], None, Some(r"C:\code\app-worktrees\app-1-x"), &agent).unwrap();
        let answered = db.create_ask("Which one?", &[], None, Some(r"C:\code\app"), &agent).unwrap();
        db.answer_ask(answered.id, "the first", &crate::Actor::Human).unwrap();
        let elsewhere = db.create_ask("And here?", &[], None, Some(r"C:\code\other"), &agent).unwrap();
        assert_eq!(db.remove_open_asks_in("C:/code/app-worktrees").unwrap(), 1);
        assert_eq!(db.remove_open_asks_in(r"C:\code\app").unwrap(), 0, "an answered one stays as a record");
        assert!(db.ask(inside.id).unwrap().is_none());
        assert!(db.ask(answered.id).unwrap().is_some() && db.ask(elsewhere.id).unwrap().is_some());
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_sweep_ends_only_quiet_sessions_nothing_runs_for() {
        let (mut db, dir) = db();
        seen(&mut db, "codex:dead", r"C:\gone\wt", "ask", "waiting");
        seen(&mut db, "codex:alive", r"C:\code\app", "tool", "working");
        seen(&mut db, "claude:idle", r"C:\gone\wt", "turn_end", "idle");
        let (_, t) = state(&db, "codex:dead");
        let runs_in = |cwd: &str| cwd.contains("app");
        assert_eq!(end_gone(&db, t + 60, &runs_in).unwrap(), 0, "not quiet long enough yet");
        assert_eq!(end_gone(&db, t + GONE_AFTER_SECS + 1, &runs_in).unwrap(), 1);
        assert_eq!(state(&db, "codex:dead"), ("ended".to_string(), t), "ended as of its last event");
        assert_eq!(state(&db, "codex:alive").0, "working");
        assert_eq!(state(&db, "claude:idle").0, "idle", "idle is not left on anyone");
        drop(db);
        std::fs::remove_dir_all(dir).ok();
    }
}
