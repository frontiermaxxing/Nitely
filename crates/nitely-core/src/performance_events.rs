//! Small durable workload observations. No command text, paths, output or free-form errors.
use crate::performance::{self, Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Started, Ended, WaitingForMemory, MemoryWaitEnded, OutOfMemory }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub wall_ms: i64,
    pub kind: Kind,
    pub project: String,
    pub issue: Option<String>,
    pub run: i64,
    pub memory_cap_bytes: Option<u64>,
}
pub(crate) fn schema(db: &rusqlite::Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS performance_event(id INTEGER PRIMARY KEY, wall_ms INTEGER NOT NULL, project TEXT NOT NULL, run INTEGER NOT NULL, kind TEXT NOT NULL, json TEXT NOT NULL CHECK(length(json)<=1024));
        CREATE INDEX IF NOT EXISTS performance_event_time ON performance_event(wall_ms);
        CREATE INDEX IF NOT EXISTS performance_event_run ON performance_event(run,id);")?;
    Ok(())
}
pub fn record(dir: &Path, event: &Event) -> Result<()> {
    let valid = |text: &str| !text.is_empty() && text.len() <= 80 && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid(&event.project) || event.issue.as_ref().is_some_and(|s| !valid(s)) || event.run < 1 { bail!("invalid performance event identity"); }
    let mut store = Store::open(dir)?;
    if !store.current_version()? || !store.settings()?.enabled { return Ok(()); }
    let days = store.settings()?.summary_days;
    let kind = serde_json::to_string(&event.kind)?;
    let tx = store.db.transaction()?;
    let prior: Option<String> = tx.query_row("SELECT kind FROM performance_event WHERE run=?1 ORDER BY id DESC LIMIT 1", [event.run], |r| r.get(0)).optional()?;
    if prior.as_deref() == Some(&kind) { return Ok(()); }
    tx.execute("DELETE FROM performance_event WHERE wall_ms<?1 OR id<=(SELECT max(id) FROM performance_event)-19999", [event.wall_ms.saturating_sub(i64::from(days)*86_400_000)])?;
    tx.execute("INSERT INTO performance_event(wall_ms,project,run,kind,json) VALUES(?1,?2,?3,?4,?5)", params![event.wall_ms,event.project,event.run,kind,serde_json::to_string(event)?])?;
    tx.commit()?;
    Ok(())
}
/// Recorder failures never stop a workload. The event stays with its coordination database,
/// including test/harness homes and lane callers whose child changes NITELY_HOME.
pub fn lane(db: &crate::Db, run: i64, kind: Kind) {
    let Some(dir) = db.conn.path().filter(|p| !p.is_empty()).and_then(|p| Path::new(p).parent()) else { return };
    let row = db.conn.query_row("SELECT project,issue_key,memory_cap FROM lane_run WHERE id=?1", [run], |r| Ok((r.get::<_, String>(0)?,r.get::<_, Option<String>>(1)?,r.get::<_, Option<u64>>(2)?)));
    if let Ok((project, issue, memory_cap_bytes)) = row { let _ = record(&dir.join("performance"), &Event { wall_ms: performance::wall_ms(), kind, project, issue, run, memory_cap_bytes }); }
}
pub fn read(store: &Store, from: i64, to: i64, project: Option<&str>, limit: usize) -> Result<(Vec<Event>, bool)> {
    if limit == 0 || limit > 2000 { bail!("event limit must be 1–2000"); }
    let exists: bool = store.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='performance_event')", [], |r| r.get(0))?;
    if !exists { return Ok((vec![], false)); }
    let mut stmt = store.db.prepare("SELECT json FROM performance_event WHERE wall_ms>=?1 AND wall_ms<=?2 AND (?3 IS NULL OR project=?3 COLLATE NOCASE) ORDER BY id DESC LIMIT ?4")?;
    let mut events = Vec::new();
    let mut rows = 0;
    for row in stmt.query_map(params![from,to,project,(limit+1) as i64], |r| r.get::<_, String>(0))? { rows += 1; if let Ok(event) = serde_json::from_str(&row?) { events.push(event); } }
    events.truncate(limit);
    Ok((events, rows > limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn events_follow_the_coordination_home_and_survive_workload_exit_without_commands() {
        let home = crate::paths::unique_temp("performance-events", "");
        std::fs::create_dir_all(&home).unwrap();
        let db = crate::Db::open(&home.join("nitely.db")).unwrap();
        db.create_project("APP", "App").unwrap();
        db.conn.execute("INSERT INTO lane_run(project,repo,requested_by,kind,snapshot,tree,base,command,state,executor,queued_at,pid,priority) VALUES('APP','C:/Code/private','test','run','a','b','a','secret arguments','queued','local',0,1,3)", []).unwrap();
        let id = db.conn.last_insert_rowid();
        lane(&db, id, Kind::WaitingForMemory);
        lane(&db, id, Kind::WaitingForMemory);
        lane(&db, id, Kind::MemoryWaitEnded);
        lane(&db, id, Kind::Started);
        lane(&db, id, Kind::OutOfMemory);
        lane(&db, id, Kind::Ended);
        drop(db);
        let store = Store::read(&home.join("performance")).unwrap().unwrap();
        let (events, truncated) = read(&store, 0, i64::MAX, Some("app"), 2000).unwrap();
        assert!(!truncated);
        assert_eq!(events.len(), 5, "one event per transition, no wait-loop duplicates");
        assert_eq!(events[0].kind, Kind::Ended);
        assert!(events.iter().any(|e| e.kind == Kind::OutOfMemory));
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains("secret") && !json.contains("private"));
        assert!(read(&store, 0, i64::MAX, Some("DEMO"), 100).unwrap().0.is_empty());
        assert!(read(&store, 0, i64::MAX, None, 2).unwrap().1);
        drop(store);
        std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn pause_clear_and_retention_apply_to_durable_events() {
        let dir = crate::paths::unique_temp("performance-event-retention", "");
        let event = |at, run| Event { wall_ms: at, kind: Kind::Started, project: "APP".into(), issue: None, run, memory_cap_bytes: None };
        record(&dir, &event(1, 1)).unwrap();
        record(&dir, &event(31*86_400_000, 2)).unwrap();
        let store = Store::open(&dir).unwrap();
        assert_eq!(read(&store, 0, i64::MAX, None, 100).unwrap().0.len(), 1);
        store.configure(&performance::Settings { enabled: false, ..Default::default() }).unwrap();
        record(&dir, &event(31*86_400_000+1, 3)).unwrap();
        assert_eq!(read(&store, 0, i64::MAX, None, 100).unwrap().0.len(), 1);
        drop(store);
        performance::clear_history(&dir).unwrap();
        let store = Store::read(&dir).unwrap().unwrap();
        assert!(read(&store, 0, i64::MAX, None, 100).unwrap().0.is_empty());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
